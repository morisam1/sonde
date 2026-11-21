//! The Redis driver, which also serves Valkey, KeyDB and Dragonfly: anything
//! that speaks RESP.
//!
//! The query tab takes commands one per line, as `redis-cli` does. The
//! `SCAN` family started at cursor `0` is run to the end and streamed, so
//! `SCAN 0 MATCH user:*` lists every match. The browser lists the selected
//! database's keys (the first thousand); opening one shows its contents in
//! the shape of its type.

mod command;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use redis::{
    AsyncConnectionConfig, Client, ConnectionAddr, IntoConnectionInfo as _, RedisConnectionInfo,
    Value as Reply, aio::MultiplexedConnection,
};
use tokio::sync::Mutex;

use crate::{
    driver::{
        Canceller, ConnectionConfig, DbKind, Dialect, Driver, QueryHandle, Secret, ServerConfig,
        Session, TlsMode,
    },
    edit::EditStatement,
    error::{Error, Result},
    schema::{Catalog, ColumnInfo, Relation, RelationKind, Schema, TableDetail, TableRef},
    stream::{self, CancelSignal, bridge},
    value::Value,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Split text into arguments the way the query tab reads a command line.
///
/// # Errors
///
/// A message when a quote is left open.
pub(crate) fn split_args(text: &str) -> std::result::Result<Vec<String>, String> {
    command::parse(text)
        .map(|args| {
            args.into_iter()
                .map(|arg| String::from_utf8_lossy(&arg).into_owned())
                .collect()
        })
        .map_err(|error| error.to_string())
}

/// A command argument written so the query tab reads it back unchanged.
pub(crate) fn quote_arg(arg: &str) -> String {
    command::quote(arg)
}
/// Keys the browser lists per database.
const CATALOG_KEYS: usize = 1_000;

/// How Redis commands are written. There is no SQL; the table view's
/// statements are commands.
#[derive(Debug, Clone, Copy)]
pub struct RedisDialect;

impl Dialect for RedisDialect {
    fn kind(&self) -> DbKind {
        DbKind::Redis
    }

    fn placeholder(&self, _: usize) -> String {
        "?".to_owned()
    }

    fn quote_ident(&self, ident: &str) -> String {
        command::quote(ident)
    }

    fn quote_table(&self, table: &TableRef) -> String {
        command::quote(&table.name)
    }

    fn browse(
        &self,
        table: &TableRef,
        columns: &[String],
        filter: &str,
        _sort: Option<(&str, bool)>,
    ) -> String {
        let key = command::quote(&table.name);
        let pattern = filter.trim();
        let names: Vec<&str> = columns.iter().map(String::as_str).collect();
        let scan = |name: &str| {
            format!(
                "{name} {key} 0 MATCH {} COUNT 1000",
                command::quote(pattern)
            )
        };
        match names.as_slice() {
            ["field", "value"] if !pattern.is_empty() => scan("HSCAN"),
            ["field", "value"] => format!("HGETALL {key}"),
            ["index", "value"] => format!("LRANGE {key} 0 -1"),
            ["member"] if !pattern.is_empty() => scan("SSCAN"),
            ["member"] => format!("SMEMBERS {key}"),
            ["member", "score"] if !pattern.is_empty() => scan("ZSCAN"),
            ["member", "score"] => format!("ZRANGE {key} 0 -1 WITHSCORES"),
            ["id", "fields"] => format!("XRANGE {key} - +"),
            ["json"] => format!("JSON.GET {key}"),
            _ => format!("GET {key}"),
        }
    }

    fn sorts(&self) -> bool {
        false
    }

    fn filter_hint(&self) -> &'static str {
        "Match fields or members with a pattern, such as user:*"
    }

    fn read_only_reason(&self) -> Option<&'static str> {
        Some("Redis keys are changed with commands in a query tab, such as HSET, LPUSH or DEL.")
    }
}

/// Opens Redis sessions.
#[derive(Debug, Clone, Copy)]
pub struct RedisDriver;

#[async_trait]
impl Driver for RedisDriver {
    fn kind(&self) -> DbKind {
        DbKind::Redis
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Redis(cfg) = cfg else {
            return Err(Error::Config("not a Redis connection".into()));
        };
        let db = match cfg.database.trim() {
            "" => 0,
            text => text.parse::<i64>().map_err(|_| {
                Error::Config(format!("the database must be a number, not {text:?}"))
            })?,
        };
        let client = client(cfg, secret.as_ref().map(Secret::expose), db)?;
        let cfg = cfg.clone();
        let session = bridge(async move {
            let conn = open(&client, db)
                .await
                .map_err(|error| Error::Connect(connect_problem(&error, &cfg)))?;
            Ok(RedisSession {
                client,
                state: Arc::new(Mutex::new(State {
                    conn: Some(conn),
                    db,
                })),
                cancel: CancelSignal::new(),
            })
        })
        .await?;
        Ok(Box::new(session))
    }
}

fn client(cfg: &ServerConfig, password: Option<&str>, db: i64) -> Result<Client> {
    let addr = match cfg.tls {
        // Redis has no way to ask for TLS on a plain connection, so
        // "prefer" means plain text.
        TlsMode::Disable | TlsMode::Prefer => ConnectionAddr::Tcp(cfg.host.clone(), cfg.port),
        TlsMode::Require | TlsMode::VerifyFull => ConnectionAddr::TcpTls {
            host: cfg.host.clone(),
            port: cfg.port,
            insecure: cfg.tls == TlsMode::Require,
            tls_params: None,
        },
    };
    let mut redis = RedisConnectionInfo::default().set_db(db);
    if !cfg.user.trim().is_empty() {
        redis = redis.set_username(cfg.user.trim());
    }
    if let Some(password) = password.filter(|p| !p.is_empty()) {
        redis = redis.set_password(password);
    }
    let info = addr
        .into_connection_info()
        .map_err(|error| Error::Config(error.to_string()))?
        .set_redis_settings(redis);
    Client::open(info).map_err(|error| Error::Config(error.to_string()))
}

async fn open(client: &Client, db: i64) -> redis::RedisResult<MultiplexedConnection> {
    let config = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(CONNECT_TIMEOUT))
        // Blocking commands such as BLPOP may wait as long as they are told.
        .set_response_timeout(None);
    let mut conn = client
        .get_multiplexed_async_connection_with_config(&config)
        .await?;
    if db != 0 {
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut conn)
            .await?;
    }
    Ok(conn)
}

fn connect_problem(error: &redis::RedisError, cfg: &ServerConfig) -> String {
    let text = error.to_string();
    let at = format!("{}:{}", cfg.host, cfg.port);
    if text.contains("WRONGPASS")
        || text.contains("invalid password")
        || error.kind() == redis::ErrorKind::AuthenticationFailed
    {
        return format!("the server at {at} rejected the user name or password");
    }
    if text.contains("NOAUTH") || text.contains("Authentication required") {
        return format!("the server at {at} wants a password");
    }
    if error.is_connection_refusal() {
        return format!("nothing is accepting connections at {at}; is the server running?");
    }
    if error.is_timeout() {
        return format!("{at} did not answer in time");
    }
    if text.contains("DB index is out of range") {
        return format!("database {} does not exist on {at}", cfg.database);
    }
    format!("could not reach {at}: {text}")
}

/// A command failure as a query error. Connection failures end the session.
fn command_error(error: &redis::RedisError) -> Error {
    if error.is_io_error() || error.is_connection_dropped() || error.is_unrecoverable_error() {
        return Error::Closed;
    }
    let message = match (error.code(), error.detail()) {
        (Some(code), Some(detail)) => format!("{code} {detail}"),
        _ => error.to_string(),
    };
    Error::Query {
        message,
        position: None,
        code: error.code().map(str::to_owned),
        detail: None,
        hint: None,
    }
}

struct State {
    /// `None` after a cancel or a dropped connection; opened again on use.
    conn: Option<MultiplexedConnection>,
    /// The database `SELECT` last chose.
    db: i64,
}

impl State {
    async fn connection(&mut self, client: &Client) -> Result<MultiplexedConnection> {
        if let Some(conn) = &self.conn {
            return Ok(conn.clone());
        }
        let conn = open(client, self.db).await.map_err(|_| Error::Closed)?;
        self.conn = Some(conn.clone());
        Ok(conn)
    }
}

struct RedisSession {
    client: Client,
    state: Arc<Mutex<State>>,
    cancel: CancelSignal,
}

/// Commands that turn the connection into a stream of pushes, which a
/// query tab cannot show.
const STREAMING: [&str; 6] = [
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "MONITOR",
    "SYNC",
    "PSYNC",
];

fn build(args: &[Vec<u8>], name: &str) -> redis::Cmd {
    let mut cmd = redis::cmd(name);
    for arg in &args[1..] {
        cmd.arg(arg.as_slice());
    }
    cmd
}

#[async_trait]
impl Session for RedisSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        if !params.is_empty() {
            return Err(Error::query("Redis commands take no parameters"));
        }
        let args = command::parse(sql)?;
        if args.is_empty() {
            return Err(Error::query("there is no command to run"));
        }
        let words: Vec<String> = args
            .iter()
            .enumerate()
            .map(|(ix, arg)| {
                let text = String::from_utf8_lossy(arg).into_owned();
                if ix == 0 {
                    text.to_ascii_uppercase()
                } else {
                    text
                }
            })
            .collect();
        if STREAMING.contains(&words[0].as_str()) {
            return Err(Error::query(format!(
                "{} streams replies without end; run it in redis-cli",
                words[0]
            )));
        }
        let client = self.client.clone();
        let state = Arc::clone(&self.state);
        let cancel = self.cancel.clone();

        stream::start(move |ready| async move {
            let mut state = state.lock_owned().await;
            let mut conn = match state.connection(&client).await {
                Ok(conn) => conn,
                Err(error) => return ready.fail(error),
            };
            let pressed = cancel.pressed();
            tokio::pin!(pressed);

            if let Some(scan) = command::scan(&words) {
                let mut feed = ready.columns(scan.columns.clone());
                let mut args = args;
                loop {
                    let cmd = build(&args, &words[0]);
                    let reply = tokio::select! {
                        biased;
                        () = &mut pressed => {
                            state.conn = None;
                            return feed.fail(Error::Cancelled).await;
                        }
                        reply = cmd.query_async::<Reply>(&mut conn) => reply,
                    };
                    let reply = match reply {
                        Ok(reply) => reply,
                        Err(error) => {
                            let error = command_error(&error);
                            if error == Error::Closed {
                                state.conn = None;
                            }
                            return feed.fail(error).await;
                        }
                    };
                    let mut parts = command::items(reply).into_iter();
                    let cursor = parts
                        .next()
                        .map(|cursor| command::cell(&cursor).to_string())
                        .unwrap_or_default();
                    let elements = parts.next().map(command::items).unwrap_or_default();
                    for chunk in elements.chunks(scan.width) {
                        if !feed.push(chunk.iter().map(command::cell).collect()).await {
                            return;
                        }
                    }
                    if cursor == "0" || cursor.is_empty() {
                        break;
                    }
                    args[scan.cursor_at] = cursor.into_bytes();
                }
                return feed.finish(None, Vec::new()).await;
            }

            let cmd = build(&args, &words[0]);
            let reply = tokio::select! {
                biased;
                () = &mut pressed => {
                    state.conn = None;
                    return ready.fail(Error::Cancelled);
                }
                reply = cmd.query_async::<Reply>(&mut conn) => reply,
            };
            match reply {
                Ok(reply) => {
                    if words[0] == "SELECT"
                        && let Some(db) = words.get(1).and_then(|db| db.parse().ok())
                    {
                        state.db = db;
                    }
                    let (shape, column) = command::shape(&words);
                    let map = matches!(reply, Reply::Map(_));
                    let rows = shape.rows(reply);
                    let columns = if shape == command::Shape::Values && map {
                        command::Shape::Pairs("key", "value").columns("")
                    } else {
                        shape.columns(column)
                    };
                    let mut feed = ready.columns(columns);
                    for row in rows {
                        if !feed.push(row).await {
                            return;
                        }
                    }
                    feed.finish(None, Vec::new()).await;
                }
                Err(error) => {
                    let error = command_error(&error);
                    if error == Error::Closed {
                        state.conn = None;
                    }
                    ready.fail(error);
                }
            }
        })
        .await
    }

    fn canceller(&self) -> Canceller {
        self.cancel.canceller()
    }

    async fn catalog(&self) -> Result<Catalog> {
        let client = self.client.clone();
        let state = Arc::clone(&self.state);
        bridge(async move {
            let mut state = state.lock().await;
            let mut conn = state.connection(&client).await?;
            let mut keys: Vec<String> = Vec::new();
            let mut cursor = "0".to_owned();
            loop {
                let reply: Reply = redis::cmd("SCAN")
                    .arg(&cursor)
                    .arg("COUNT")
                    .arg(1000)
                    .query_async(&mut conn)
                    .await
                    .map_err(|error| command_error(&error))?;
                let mut parts = command::items(reply).into_iter();
                cursor = parts
                    .next()
                    .map(|c| command::cell(&c).to_string())
                    .unwrap_or_default();
                for key in parts.next().map(command::items).unwrap_or_default() {
                    keys.push(command::cell(&key).to_string());
                }
                if cursor == "0" || cursor.is_empty() || keys.len() >= CATALOG_KEYS {
                    break;
                }
            }
            keys.sort();
            keys.dedup();
            keys.truncate(CATALOG_KEYS);
            Ok(Catalog {
                schemas: vec![Schema {
                    name: format!("db{}", state.db),
                    relations: keys
                        .into_iter()
                        .map(|name| Relation {
                            name,
                            kind: RelationKind::Key,
                        })
                        .collect(),
                }],
            })
        })
        .await
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let client = self.client.clone();
        let state = Arc::clone(&self.state);
        let key = table.name.clone();
        bridge(async move {
            let mut state = state.lock().await;
            let mut conn = state.connection(&client).await?;
            let kind: String = redis::cmd("TYPE")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .map_err(|error| command_error(&error))?;
            let names: &[(&str, &str)] = match kind.as_str() {
                "string" => &[("value", "string")],
                "hash" => &[("field", "string"), ("value", "string")],
                "list" => &[("index", "integer"), ("value", "string")],
                "set" => &[("member", "string")],
                "zset" => &[("member", "string"), ("score", "double")],
                "stream" => &[("id", "string"), ("fields", "json")],
                "ReJSON-RL" => &[("json", "json")],
                "none" => return Err(Error::query(format!("key {key:?} does not exist"))),
                other => {
                    return Err(Error::query(format!(
                        "key {key:?} holds a {other}, which Sonde cannot show as rows"
                    )));
                }
            };
            Ok(TableDetail {
                columns: names
                    .iter()
                    .map(|(name, type_name)| ColumnInfo {
                        name: (*name).to_owned(),
                        type_name: (*type_name).to_owned(),
                        nullable: true,
                        default: None,
                    })
                    .collect(),
                ..TableDetail::default()
            })
        })
        .await
    }

    async fn apply(&self, _: &[EditStatement]) -> Result<()> {
        Err(Error::Config(
            RedisDialect.read_only_reason().unwrap_or_default().into(),
        ))
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &RedisDialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        let state = Arc::clone(&self.state);
        bridge(async move {
            state.lock().await.conn = None;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browse(columns: &[&str], filter: &str) -> String {
        let columns: Vec<String> = columns.iter().map(|c| (*c).to_owned()).collect();
        RedisDialect.browse(&TableRef::new("db0", "user:1"), &columns, filter, None)
    }

    #[test]
    fn browse_should_read_each_type_whole() {
        assert_eq!(browse(&["value"], ""), "GET user:1");
        assert_eq!(browse(&["field", "value"], ""), "HGETALL user:1");
        assert_eq!(
            browse(&["member", "score"], ""),
            "ZRANGE user:1 0 -1 WITHSCORES"
        );
    }

    #[test]
    fn browse_should_scan_with_the_filter_as_a_pattern() {
        assert_eq!(
            browse(&["field", "value"], "na*"),
            "HSCAN user:1 0 MATCH na* COUNT 1000"
        );
    }
}
