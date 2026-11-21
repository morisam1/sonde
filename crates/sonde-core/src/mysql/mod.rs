//! The MySQL driver, which also serves MariaDB.
//!
//! One `mysql_async` connection per session, behind a lock that doubles as
//! the session's "one statement at a time" rule. Plain statements use the
//! text protocol, so anything the server accepts can be run; edits use
//! prepared statements, so their values travel as parameters.

mod decode;
mod introspect;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{
    SinkExt as _,
    channel::{mpsc, oneshot},
};
use mysql_async::{
    Conn, Opts, OptsBuilder, QueryResult, SslOpts,
    prelude::{Protocol, Queryable as _},
};
use tokio::sync::Mutex;

use crate::{
    driver::{
        CHANNEL_PAGES, Canceller, ConnectionConfig, DbKind, Dialect, Driver, PAGE_ROWS,
        QueryHandle, QuerySummary, Secret, ServerConfig, Session, TlsMode,
    },
    edit::EditStatement,
    error::{Error, Result},
    runtime,
    schema::{Catalog, TableDetail, TableRef},
    value::{ColumnMeta, ResultPage, Value},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// `ER_QUERY_INTERRUPTED`: the statement was stopped by `KILL QUERY`.
const QUERY_INTERRUPTED: u16 = 1317;
/// Server errors that mean the session is over.
const SESSION_ENDED: [u16; 4] = [
    1053, // ER_SERVER_SHUTDOWN
    1927, // ER_CONNECTION_KILLED
    2006, // CR_SERVER_GONE_ERROR
    2013, // CR_SERVER_LOST
];

/// MySQL's SQL spelling.
#[derive(Debug, Clone, Copy)]
pub struct MysqlDialect;

impl Dialect for MysqlDialect {
    fn kind(&self) -> DbKind {
        DbKind::Mysql
    }

    fn placeholder(&self, _: usize) -> String {
        "?".to_owned()
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn insert_defaults(&self, table: &TableRef) -> String {
        format!("INSERT INTO {} () VALUES ()", self.quote_table(table))
    }
}

/// MariaDB's SQL spelling, which is MySQL's.
#[derive(Debug, Clone, Copy)]
pub struct MariadbDialect;

impl Dialect for MariadbDialect {
    fn kind(&self) -> DbKind {
        DbKind::Mariadb
    }

    fn placeholder(&self, n: usize) -> String {
        MysqlDialect.placeholder(n)
    }

    fn quote_ident(&self, ident: &str) -> String {
        MysqlDialect.quote_ident(ident)
    }

    fn insert_defaults(&self, table: &TableRef) -> String {
        MysqlDialect.insert_defaults(table)
    }
}

/// Opens MySQL sessions.
#[derive(Debug, Clone, Copy)]
pub struct MysqlDriver;

/// Opens MariaDB sessions, through the same protocol as MySQL.
#[derive(Debug, Clone, Copy)]
pub struct MariadbDriver;

#[async_trait]
impl Driver for MysqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::Mysql
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Mysql(cfg) = cfg else {
            return Err(Error::Config("not a MySQL connection".into()));
        };
        connect(cfg, secret, &MysqlDialect).await
    }
}

#[async_trait]
impl Driver for MariadbDriver {
    fn kind(&self) -> DbKind {
        DbKind::Mariadb
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Mariadb(cfg) = cfg else {
            return Err(Error::Config("not a MariaDB connection".into()));
        };
        connect(cfg, secret, &MariadbDialect).await
    }
}

async fn connect(
    cfg: &ServerConfig,
    secret: Option<Secret>,
    dialect: &'static dyn Dialect,
) -> Result<Box<dyn Session>> {
    let cfg = cfg.clone();
    let password = secret.map(|secret| secret.expose().to_owned());
    let session = bridge(async move {
        let (conn, opts) = open(&cfg, password).await?;
        Ok(MysqlSession {
            id: conn.id(),
            conn: Arc::new(Mutex::new(conn)),
            opts,
            database: cfg.database.clone(),
            dialect,
        })
    })
    .await?;
    Ok(Box::new(session))
}

fn options(cfg: &ServerConfig, password: Option<&str>, tls: Option<SslOpts>) -> Opts {
    OptsBuilder::default()
        .ip_or_hostname(cfg.host.clone())
        .tcp_port(cfg.port)
        .user(Some(cfg.user.clone()))
        .pass(password.map(str::to_owned))
        .db_name((!cfg.database.is_empty()).then(|| cfg.database.clone()))
        .ssl_opts(tls)
        .prefer_socket(false)
        // Report rows matched rather than rows changed, so an edit that
        // writes a value a row already has still counts as one row.
        .client_found_rows(true)
        .into()
}

fn tls_options(mode: TlsMode) -> Option<SslOpts> {
    match mode {
        TlsMode::Disable => None,
        TlsMode::Prefer | TlsMode::Require => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        TlsMode::VerifyFull => Some(SslOpts::default()),
    }
}

/// Connect, falling back to plain text under [`TlsMode::Prefer`] when the
/// server does not do TLS.
async fn open(cfg: &ServerConfig, password: Option<String>) -> Result<(Conn, Opts)> {
    let attempt = |tls: Option<SslOpts>| {
        let opts = options(cfg, password.as_deref(), tls);
        async move {
            match tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts.clone())).await {
                Ok(Ok(conn)) => Ok((conn, opts)),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(mysql_async::Error::Io(mysql_async::IoError::Io(
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out"),
                ))),
            }
        }
    };
    match attempt(tls_options(cfg.tls)).await {
        Ok(opened) => Ok(opened),
        Err(error) if cfg.tls == TlsMode::Prefer && is_tls_refusal(&error) => attempt(None)
            .await
            .map_err(|error| Error::Connect(connect_problem(&error, cfg))),
        Err(error) => Err(Error::Connect(connect_problem(&error, cfg))),
    }
}

fn is_tls_refusal(error: &mysql_async::Error) -> bool {
    matches!(
        error,
        mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer)
    ) || error.to_string().to_lowercase().contains("tls")
}

/// Why a connection attempt failed, in words that say what to check.
fn connect_problem(error: &mysql_async::Error, cfg: &ServerConfig) -> String {
    let address = format!("{}:{}", cfg.host, cfg.port);
    if let mysql_async::Error::Server(server) = error {
        return match server.code {
            // ER_ACCESS_DENIED_ERROR
            1045 => format!(
                "the server rejected the password for user \"{}\". Check the password and \
                 the user name.",
                cfg.user
            ),
            // ER_BAD_DB_ERROR
            1049 => format!(
                "the database \"{}\" does not exist on {address}.",
                cfg.database
            ),
            // ER_DBACCESS_DENIED_ERROR
            1044 => format!(
                "user \"{}\" may not open the database \"{}\".",
                cfg.user, cfg.database
            ),
            // ER_HOST_NOT_PRIVILEGED
            1130 => format!(
                "{}. The server does not accept logins from here.",
                server.message
            ),
            // ER_CON_COUNT_ERROR
            1040 => format!("the server at {address} has no free connections left."),
            _ => server.message.clone(),
        };
    }
    let text = error.to_string();
    let lower = text.to_lowercase();
    if lower.contains("connection refused") {
        format!(
            "nothing is accepting connections at {address}. Check the host and port, and \
             that the server is running."
        )
    } else if lower.contains("lookup address")
        || lower.contains("name or service not known")
        || lower.contains("failed to lookup")
    {
        format!(
            "the host \"{}\" could not be found. Check its name.",
            cfg.host
        )
    } else if lower.contains("timed out") {
        format!(
            "no answer from {address} within {} seconds. The host may be wrong, or a \
             firewall may be blocking the port.",
            CONNECT_TIMEOUT.as_secs()
        )
    } else if matches!(
        error,
        mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer)
    ) {
        "the server does not accept TLS connections. Set TLS to Prefer or Disable for this \
         connection."
            .to_owned()
    } else if lower.contains("certificate") {
        format!(
            "the server's TLS certificate could not be verified ({text}). Set TLS to Require to \
             encrypt without checking the certificate."
        )
    } else {
        text
    }
}

/// Run `future` on the driver runtime and wait for it from any executor.
async fn bridge<T, F>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    runtime::spawn(future).await.unwrap_or(Err(Error::Closed))
}

pub(super) fn query_error(error: mysql_async::Error, sql: &str) -> Error {
    if error.is_fatal() {
        return Error::Closed;
    }
    let mysql_async::Error::Server(server) = error else {
        return Error::query(error.to_string());
    };
    if server.code == QUERY_INTERRUPTED {
        return Error::Cancelled;
    }
    if SESSION_ENDED.contains(&server.code) {
        return Error::Closed;
    }
    Error::Query {
        position: position(&server.message, sql),
        message: server.message,
        code: Some(format!("{} ({})", server.code, server.state)),
        detail: None,
        hint: None,
    }
}

/// Where in `sql` a syntax error is. MySQL quotes the text from the bad
/// token on, as in `... near 'frm t' at line 1`, rather than giving an offset.
fn position(message: &str, sql: &str) -> Option<usize> {
    let start = message.find("near '")? + "near '".len();
    let end = message.rfind("' at line")?;
    let quoted = message.get(start..end)?;
    if quoted.is_empty() {
        return Some(sql.trim_end().len());
    }
    // MySQL cuts the quote at 80 characters; the start is what matters.
    let probe: String = quoted.chars().take(40).collect();
    sql.rfind(&probe)
}

struct MysqlSession {
    conn: Arc<Mutex<Conn>>,
    /// The server's id for the connection, for `KILL QUERY`.
    id: u32,
    opts: Opts,
    database: String,
    dialect: &'static dyn Dialect,
}

#[async_trait]
impl Session for MysqlSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        let conn = Arc::clone(&self.conn);
        let sql = sql.to_owned();
        let params = params
            .iter()
            .map(decode::param)
            .collect::<Result<Vec<_>>>()?;
        let opts = self.opts.clone();
        let id = self.id;

        bridge(async move {
            let (ready, ready_rx) = oneshot::channel();
            let (pages, pages_rx) = mpsc::channel(CHANNEL_PAGES);
            let (summary, summary_rx) = oneshot::channel();
            tokio::spawn(async move {
                let mut conn = conn.lock_owned().await;
                let started = Instant::now();
                let stream = Stream {
                    ready,
                    pages,
                    summary,
                    started,
                    opts,
                    id,
                };
                if params.is_empty() {
                    match conn.query_iter(sql.as_str()).await {
                        Ok(result) => stream.run(result, &sql).await,
                        Err(error) => drop(stream.ready.send(Err(query_error(error, &sql)))),
                    }
                } else {
                    match conn.exec_iter(sql.as_str(), params).await {
                        Ok(result) => stream.run(result, &sql).await,
                        Err(error) => drop(stream.ready.send(Err(query_error(error, &sql)))),
                    }
                }
            });
            let columns = ready_rx.await.map_err(|_| Error::Closed)??;
            Ok(QueryHandle {
                columns,
                pages: pages_rx,
                summary: summary_rx,
            })
        })
        .await
    }

    fn canceller(&self) -> Canceller {
        let opts = self.opts.clone();
        let id = self.id;
        Canceller::new(move || {
            runtime::handle().spawn(kill_query(opts.clone(), id));
        })
    }

    async fn catalog(&self) -> Result<Catalog> {
        let conn = Arc::clone(&self.conn);
        let database = self.database.clone();
        bridge(async move {
            let mut conn = conn.lock().await;
            introspect::catalog(&mut conn, &database).await
        })
        .await
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let conn = Arc::clone(&self.conn);
        let table = table.clone();
        let database = self.database.clone();
        let mariadb = self.dialect.kind() == DbKind::Mariadb;
        bridge(async move {
            let mut conn = conn.lock().await;
            introspect::table_detail(&mut conn, &table, &database, mariadb).await
        })
        .await
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let statements = statements
            .iter()
            .map(|statement| {
                let params = statement
                    .params
                    .iter()
                    .map(decode::param)
                    .collect::<Result<Vec<_>>>()?;
                Ok((statement.sql.clone(), params))
            })
            .collect::<Result<Vec<_>>>()?;
        bridge(async move {
            let mut conn = conn.lock().await;
            conn.query_drop("START TRANSACTION")
                .await
                .map_err(|error| query_error(error, "START TRANSACTION"))?;
            let mut failed = None;
            for (sql, params) in &statements {
                let outcome = conn.exec_drop(sql.as_str(), params.clone()).await;
                failed = match outcome {
                    Ok(()) if conn.affected_rows() == 1 => None,
                    Ok(()) => Some(Error::Stale),
                    Err(error) => Some(query_error(error, sql)),
                };
                if failed.is_some() {
                    break;
                }
            }
            match failed {
                None => conn
                    .query_drop("COMMIT")
                    .await
                    .map_err(|error| query_error(error, "COMMIT")),
                Some(error) => {
                    let _ = conn.query_drop("ROLLBACK").await;
                    Err(error)
                }
            }
        })
        .await
    }

    fn dialect(&self) -> &'static dyn Dialect {
        self.dialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        bridge(async move {
            let mut conn = conn.lock().await;
            conn.query_drop("DO 0").await.map_err(|_| Error::Closed)
        })
        .await
    }
}

/// Where a running statement reports to.
struct Stream {
    ready: oneshot::Sender<Result<Vec<ColumnMeta>>>,
    pages: mpsc::Sender<Result<ResultPage>>,
    summary: oneshot::Sender<QuerySummary>,
    started: Instant,
    opts: Opts,
    id: u32,
}

impl Stream {
    /// Report the columns, then feed the rows through in pages.
    async fn run<P: Protocol>(self, mut result: QueryResult<'_, '_, P>, sql: &str) {
        let Self {
            ready,
            mut pages,
            summary,
            started,
            opts,
            id,
        } = self;
        let columns = result.columns().unwrap_or_default();
        let decoders: Vec<decode::Decoder> =
            columns.iter().map(decode::Decoder::for_column).collect();
        let meta = columns
            .iter()
            .zip(&decoders)
            .map(|(column, decoder)| decoder.meta(column))
            .collect();
        if ready.send(Ok(meta)).is_err() {
            return;
        }

        let mut page = Vec::with_capacity(PAGE_ROWS);
        let mut rows_returned = 0_u64;
        let outcome: Result<()> = async {
            while let Some(row) = result.next().await.map_err(|e| query_error(e, sql))? {
                page.push(decode::row(row, &decoders));
                rows_returned += 1;
                if page.len() == PAGE_ROWS {
                    let rows = std::mem::replace(&mut page, Vec::with_capacity(PAGE_ROWS));
                    send(&mut pages, Ok(ResultPage { rows })).await?;
                }
            }
            if !page.is_empty() {
                send(&mut pages, Ok(ResultPage { rows: page })).await?;
            }
            Ok(())
        }
        .await;
        let affected = result.affected_rows();
        match outcome {
            Ok(()) => {
                // A procedure can return more result sets; only the first is
                // shown, the rest are read and dropped.
                let _ = result.drop_result().await;
                let _ = summary.send(QuerySummary {
                    rows_affected: columns.is_empty().then_some(affected),
                    rows_returned,
                    elapsed: started.elapsed(),
                    notices: Vec::new(),
                });
            }
            // The handle was dropped mid-stream: stop the server rather than
            // read every remaining row, then clear what is in flight before
            // the next statement gets the lock.
            Err(Error::Closed) => {
                kill_query(opts, id).await;
                let _ = result.drop_result().await;
            }
            Err(error) => {
                drop(result);
                let _ = pages.send(Err(error)).await;
            }
        }
    }
}

/// Stop whatever connection `id` is running, from a connection of its own.
/// If the request cannot be delivered the statement simply runs on.
async fn kill_query(opts: Opts, id: u32) {
    if let Ok(Ok(mut conn)) = tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts)).await {
        let _ = conn.query_drop(format!("KILL QUERY {id}")).await;
        let _ = conn.disconnect().await;
    }
}

/// Send one item, mapping a vanished consumer to [`Error::Closed`].
async fn send(
    pages: &mut mpsc::Sender<Result<ResultPage>>,
    item: Result<ResultPage>,
) -> Result<()> {
    pages.send(item).await.map_err(|_| Error::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_should_find_the_quoted_tail_of_a_syntax_error() {
        let sql = "select * frm t";
        let message = "You have an error in your SQL syntax; check the manual that \
                       corresponds to your MySQL server version for the right syntax to use \
                       near 'frm t' at line 1";

        assert_eq!(position(message, sql), Some(9));
    }

    #[test]
    fn position_should_point_at_the_end_for_an_unexpected_end() {
        let message = "... for the right syntax to use near '' at line 1";

        assert_eq!(position(message, "select * from "), Some(13));
    }

    #[test]
    fn insert_defaults_should_use_empty_lists() {
        assert_eq!(
            MysqlDialect.insert_defaults(&TableRef::new("db", "t")),
            "INSERT INTO `db`.`t` () VALUES ()"
        );
    }
}
