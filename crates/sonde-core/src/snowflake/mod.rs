//! The Snowflake driver, through the SQL REST API (`/api/v2/statements`).
//!
//! It signs in with a programmatic access token, which goes in the
//! password field. Every statement is its own request, so the session
//! keeps the warehouse, role, database and schema itself: `USE WAREHOUSE`,
//! `USE ROLE`, `USE DATABASE` and `USE SCHEMA` change them for the
//! statements that follow. Results arrive in partitions, which are fetched
//! one at a time as the grid asks for rows.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value as Json, json};
use tokio::sync::Mutex;

use crate::{
    driver::{
        Canceller, ConnectionConfig, DbKind, Dialect, Driver, QueryHandle, Secret, ServerConfig,
        Session,
    },
    edit::EditStatement,
    error::{Error, Result},
    schema::{Catalog, ColumnInfo, Relation, RelationKind, Schema, TableDetail, TableRef},
    stream::{self, CancelSignal, Ready, bridge, meta},
    value::Value,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How often a statement that is still running is asked about.
const POLL: Duration = Duration::from_millis(500);

/// Snowflake's SQL spelling.
#[derive(Debug, Clone, Copy)]
pub struct SnowflakeDialect;

impl Dialect for SnowflakeDialect {
    fn kind(&self) -> DbKind {
        DbKind::Snowflake
    }

    fn placeholder(&self, _: usize) -> String {
        "?".to_owned()
    }
}

/// Opens Snowflake sessions.
#[derive(Debug, Clone, Copy)]
pub struct SnowflakeDriver;

/// What a statement runs under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Context {
    warehouse: Option<String>,
    role: Option<String>,
    database: Option<String>,
    schema: Option<String>,
}

#[derive(Clone)]
struct Account {
    http: Client,
    base: String,
    token: String,
}

static REQUESTS: AtomicU64 = AtomicU64::new(1);

/// A fresh request id, which Snowflake wants as a UUID.
fn request_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let count = u128::from(REQUESTS.fetch_add(1, Ordering::Relaxed));
    uuid::Uuid::from_u128((nanos << 32) ^ count).to_string()
}

/// The account's API address. A bare account identifier gets Snowflake's
/// domain; a full host name or URL is taken as given.
fn base_url(cfg: &ServerConfig) -> String {
    let host = cfg.host.trim().trim_end_matches('/');
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.to_owned();
    }
    let host = if host.contains('.') {
        host.to_owned()
    } else {
        format!("{host}.snowflakecomputing.com")
    };
    if cfg.port == 443 || cfg.port == 0 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{}", cfg.port)
    }
}

impl Account {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Json>,
    ) -> Result<(StatusCode, Json)> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header(
                "X-Snowflake-Authorization-Token-Type",
                "PROGRAMMATIC_ACCESS_TOKEN",
            )
            .header("Accept", "application/json")
            .header("User-Agent", "Sonde/1.0");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|error| {
            if error.is_connect() || error.is_timeout() {
                Error::Closed
            } else {
                Error::query(error.to_string())
            }
        })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| Error::query(error.to_string()))?;
        let json = serde_json::from_str(&text).unwrap_or(Json::String(text));
        Ok((status, json))
    }

    /// Submit a statement and wait until it has a result.
    async fn submit(&self, sql: &str, context: &Context, bindings: Option<Json>) -> Result<Json> {
        let mut body = json!({
            "statement": sql,
            "timeout": 0,
            "parameters": {"MULTI_STATEMENT_COUNT": "1"},
        });
        for (key, value) in [
            ("warehouse", &context.warehouse),
            ("role", &context.role),
            ("database", &context.database),
            ("schema", &context.schema),
        ] {
            if let Some(value) = value {
                body[key] = json!(value);
            }
        }
        if let Some(bindings) = bindings {
            body["bindings"] = bindings;
        }
        let path = format!("/api/v2/statements?requestId={}", request_id());
        let (mut status, mut reply) = self.call(Method::POST, &path, Some(body)).await?;
        // 202: accepted and still running.
        while status == StatusCode::ACCEPTED {
            let Some(handle) = reply["statementHandle"].as_str().map(str::to_owned) else {
                break;
            };
            tokio::time::sleep(POLL).await;
            (status, reply) = self
                .call(Method::GET, &format!("/api/v2/statements/{handle}"), None)
                .await?;
        }
        if status.is_success() {
            Ok(reply)
        } else {
            Err(reply_error(status, &reply, sql))
        }
    }

    async fn cancel(&self, handle: &str) {
        let _ = self
            .call(
                Method::POST,
                &format!("/api/v2/statements/{handle}/cancel"),
                Some(json!({})),
            )
            .await;
    }
}

/// An error reply. Compilation errors say `line L at position C`, with the
/// position counted from 0.
fn reply_error(status: StatusCode, reply: &Json, sql: &str) -> Error {
    let message = reply["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| match reply {
            Json::String(text) if !text.is_empty() => text.clone(),
            _ => format!("Snowflake answered {status}"),
        });
    if status == StatusCode::UNAUTHORIZED {
        return Error::Connect(
            "Snowflake rejected the access token; check that it is a programmatic access \
             token for this user and has not expired"
                .into(),
        );
    }
    if reply["code"].as_str() == Some("000604") {
        return Error::Cancelled;
    }
    let position = message.find("line ").and_then(|ix| {
        let rest = &message[ix + 5..];
        let line: usize = rest.split_whitespace().next()?.parse().ok()?;
        let after = rest.find("at position ")? + "at position ".len();
        let column: usize = rest[after..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()?;
        let start: usize = sql
            .split_inclusive('\n')
            .take(line.saturating_sub(1))
            .map(str::len)
            .sum();
        Some(start + column).filter(|&p| p <= sql.len())
    });
    Error::Query {
        message,
        position,
        code: reply["sqlState"]
            .as_str()
            .or(reply["code"].as_str())
            .map(str::to_owned),
        detail: None,
        hint: None,
    }
}

#[async_trait]
impl Driver for SnowflakeDriver {
    fn kind(&self) -> DbKind {
        DbKind::Snowflake
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Snowflake(cfg) = cfg else {
            return Err(Error::Config("not a Snowflake connection".into()));
        };
        let Some(token) = secret
            .map(|s| s.expose().to_owned())
            .filter(|t| !t.is_empty())
        else {
            return Err(Error::Connect(
                "Snowflake needs a programmatic access token in the password field".into(),
            ));
        };
        let cfg = cfg.clone();
        let session = bridge(async move {
            let http = Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(|error| Error::Config(error.to_string()))?;
            let account = Account {
                http,
                base: base_url(&cfg),
                token,
            };
            let context = Context {
                warehouse: cfg.option("warehouse").map(str::to_owned),
                role: cfg.option("role").map(str::to_owned),
                database: Some(cfg.database.trim().to_owned()).filter(|d| !d.is_empty()),
                schema: cfg.option("schema").map(str::to_owned),
            };
            // A metadata-only statement: it proves the token without
            // waking a warehouse.
            account
                .submit("SELECT CURRENT_VERSION()", &context, None)
                .await
                .map_err(|error| match error {
                    Error::Connect(message) => Error::Connect(message),
                    Error::Closed => {
                        Error::Connect(format!("nothing answered at {}", account.base))
                    }
                    other => Error::Connect(format!("Snowflake refused the session: {other}")),
                })?;
            Ok(SnowflakeSession {
                account,
                context: Arc::new(Mutex::new(context)),
                cancel: CancelSignal::new(),
            })
        })
        .await?;
        Ok(Box::new(session))
    }
}

/// A cell from the API's text encoding, by the column's type.
fn cell(value: &Json, column: &Json) -> Value {
    let Some(text) = value.as_str() else {
        return Value::Null;
    };
    let kind = column["type"].as_str().unwrap_or("text");
    let scale = column["scale"].as_i64().unwrap_or(0);
    let seconds = |text: &str| -> Option<(i64, u32)> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, "0"));
        let secs: i64 = whole.parse().ok()?;
        let mut nanos = format!("{fraction:0<9}");
        nanos.truncate(9);
        let nanos: u32 = nanos.parse().ok()?;
        // A negative time with a fraction counts back from the second.
        Some(if secs < 0 && nanos > 0 && whole.starts_with('-') {
            (secs - 1, 1_000_000_000 - nanos)
        } else {
            (secs, nanos)
        })
    };
    let parsed = match kind {
        "fixed" if scale == 0 => text
            .parse::<i64>()
            .map(Value::Int)
            .ok()
            .or_else(|| text.parse().ok().map(Value::Decimal)),
        "fixed" => text.parse().ok().map(Value::Decimal),
        "real" | "float" | "double" => text.parse().ok().map(Value::Float),
        "boolean" => Some(Value::Bool(text == "true" || text == "1")),
        "date" => text.parse::<i64>().ok().and_then(|days| {
            chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?
                .checked_add_signed(chrono::TimeDelta::days(days))
                .map(Value::Date)
        }),
        "time" => seconds(text).and_then(|(secs, nanos)| {
            chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs as u32, nanos)
                .map(Value::Time)
        }),
        "timestamp_ntz" => seconds(text).and_then(|(secs, nanos)| {
            chrono::DateTime::from_timestamp(secs, nanos).map(|t| Value::Timestamp(t.naive_utc()))
        }),
        "timestamp_ltz" => seconds(text).and_then(|(secs, nanos)| {
            chrono::DateTime::from_timestamp(secs, nanos).map(Value::TimestampTz)
        }),
        "timestamp_tz" => {
            let instant = text.split_whitespace().next().unwrap_or(text);
            seconds(instant).and_then(|(secs, nanos)| {
                chrono::DateTime::from_timestamp(secs, nanos).map(Value::TimestampTz)
            })
        }
        "variant" | "object" | "array" => Some(Value::Json(Arc::from(text))),
        "binary" => {
            let bytes: Option<Vec<u8>> = (0..text.len())
                .step_by(2)
                .map(|ix| {
                    text.get(ix..ix + 2)
                        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                })
                .collect();
            bytes.map(|bytes| Value::Bytes(Arc::from(bytes)))
        }
        _ => None,
    };
    parsed.unwrap_or_else(|| Value::text(text))
}

/// The type name shown for a column, written the way DDL writes it.
fn type_name(column: &Json) -> String {
    match column["type"].as_str().unwrap_or("") {
        "fixed" => match (column["precision"].as_i64(), column["scale"].as_i64()) {
            (Some(p), Some(s)) => format!("number({p},{s})"),
            _ => "number".into(),
        },
        "text" => "varchar".into(),
        "real" => "float".into(),
        other => other.to_owned(),
    }
}

/// What a `USE` statement changes, if `sql` is one.
fn use_statement(sql: &str) -> Option<(&'static str, String)> {
    let words: Vec<&str> = sql
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect();
    if !words.first()?.eq_ignore_ascii_case("use") {
        return None;
    }
    let (what, name) = match words.as_slice() {
        [_, kind, name] => {
            let what = match kind.to_ascii_uppercase().as_str() {
                "WAREHOUSE" => "warehouse",
                "ROLE" => "role",
                "DATABASE" => "database",
                "SCHEMA" => "schema",
                _ => return None,
            };
            (what, *name)
        }
        [_, name] => (
            if name.contains('.') {
                "schema"
            } else {
                "database"
            },
            *name,
        ),
        _ => return None,
    };
    Some((what, name.to_owned()))
}

struct SnowflakeSession {
    account: Account,
    context: Arc<Mutex<Context>>,
    cancel: CancelSignal,
}

impl SnowflakeSession {
    async fn run(
        account: Account,
        context: Context,
        sql: String,
        ready: Ready,
        cancel: CancelSignal,
    ) {
        let pressed = cancel.pressed();
        tokio::pin!(pressed);
        let submit = account.submit(&sql, &context, None);
        tokio::pin!(submit);
        let reply = tokio::select! {
            biased;
            // The handle is not known until the reply; a cancel before then
            // abandons the request, and Snowflake ends it with the timeout.
            () = &mut pressed => return ready.fail(Error::Cancelled),
            reply = &mut submit => reply,
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => return ready.fail(error),
        };
        let row_type = reply["resultSetMetaData"]["rowType"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let columns = row_type
            .iter()
            .map(|c| meta(c["name"].as_str().unwrap_or(""), &type_name(c)))
            .collect();
        let handle = reply["statementHandle"].as_str().unwrap_or("").to_owned();
        let partitions = reply["resultSetMetaData"]["partitionInfo"]
            .as_array()
            .map_or(1, Vec::len);
        let mut feed = ready.columns(columns);
        let mut data = reply["data"].as_array().cloned().unwrap_or_default();
        let mut partition = 0;
        loop {
            for row in &data {
                let cells = row
                    .as_array()
                    .into_iter()
                    .flatten()
                    .zip(&row_type)
                    .map(|(value, column)| cell(value, column))
                    .collect();
                if !feed.push(cells).await {
                    return;
                }
            }
            partition += 1;
            if partition >= partitions {
                break;
            }
            let path = format!("/api/v2/statements/{handle}?partition={partition}");
            let next = tokio::select! {
                biased;
                () = &mut pressed => {
                    account.cancel(&handle).await;
                    return feed.fail(Error::Cancelled).await;
                }
                next = account.call(Method::GET, &path, None) => next,
            };
            match next {
                Ok((status, page)) if status.is_success() => {
                    data = page["data"].as_array().cloned().unwrap_or_default();
                }
                Ok((status, page)) => return feed.fail(reply_error(status, &page, &sql)).await,
                Err(error) => return feed.fail(error).await,
            }
        }
        feed.finish(None, Vec::new()).await;
    }

    /// A statement's rows as text, for introspection.
    async fn texts(&self, sql: &str, bindings: Option<Json>) -> Result<Vec<Vec<Option<String>>>> {
        let account = self.account.clone();
        let context = self.context.lock().await.clone();
        let sql = sql.to_owned();
        bridge(async move {
            let reply = account.submit(&sql, &context, bindings).await?;
            let partitions = reply["resultSetMetaData"]["partitionInfo"]
                .as_array()
                .map_or(1, Vec::len);
            let handle = reply["statementHandle"].as_str().unwrap_or("").to_owned();
            let mut rows = Vec::new();
            let mut page = reply;
            for partition in 0..partitions {
                if partition > 0 {
                    let path = format!("/api/v2/statements/{handle}?partition={partition}");
                    page = account.call(Method::GET, &path, None).await?.1;
                }
                for row in page["data"].as_array().into_iter().flatten() {
                    rows.push(
                        row.as_array()
                            .into_iter()
                            .flatten()
                            .map(|v| v.as_str().map(str::to_owned))
                            .collect(),
                    );
                }
            }
            Ok(rows)
        })
        .await
    }

    async fn database(&self) -> Result<Option<String>> {
        if let Some(db) = self.context.lock().await.database.clone() {
            return Ok(Some(db));
        }
        let rows = self.texts("SELECT CURRENT_DATABASE()", None).await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next().flatten()))
    }
}

fn binding(text: &str) -> Json {
    json!({"type": "TEXT", "value": text})
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[async_trait]
impl Session for SnowflakeSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        if !params.is_empty() {
            return Err(Error::query("parameters are only used by edits"));
        }
        if let Some((what, name)) = use_statement(sql) {
            let mut context = self.context.lock().await;
            let name = name.trim_matches('"').to_owned();
            match what {
                "warehouse" => context.warehouse = Some(name),
                "role" => context.role = Some(name),
                "database" => {
                    context.database = Some(name);
                    context.schema = None;
                }
                _ => match name.split_once('.') {
                    Some((db, schema)) => {
                        context.database = Some(db.to_owned());
                        context.schema = Some(schema.to_owned());
                    }
                    None => context.schema = Some(name),
                },
            }
            return stream::start(|ready| async move { ready.done(None, Vec::new()).await }).await;
        }
        let account = self.account.clone();
        let context = self.context.lock().await.clone();
        let cancel = self.cancel.clone();
        let sql = sql.to_owned();
        stream::start(move |ready| Self::run(account, context, sql, ready, cancel)).await
    }

    fn canceller(&self) -> Canceller {
        self.cancel.canceller()
    }

    async fn catalog(&self) -> Result<Catalog> {
        let Some(db) = self.database().await? else {
            return Ok(Catalog::default());
        };
        let sql = format!(
            "SELECT table_schema, table_name, table_type FROM {}.information_schema.tables \
             WHERE table_schema <> 'INFORMATION_SCHEMA' ORDER BY 1, 2",
            ident(&db)
        );
        let rows = self.texts(&sql, None).await?;
        let schemas_sql = format!(
            "SELECT schema_name FROM {}.information_schema.schemata \
             WHERE schema_name <> 'INFORMATION_SCHEMA' ORDER BY 1",
            ident(&db)
        );
        let mut schemas: Vec<Schema> = self
            .texts(&schemas_sql, None)
            .await?
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .map(|name| Schema {
                name,
                relations: Vec::new(),
            })
            .collect();
        for row in rows {
            let [Some(schema), Some(name), kind] =
                <[Option<String>; 3]>::try_from(row).unwrap_or([None, None, None])
            else {
                continue;
            };
            let kind = match kind.as_deref() {
                Some("VIEW") => RelationKind::View,
                Some("MATERIALIZED VIEW") => RelationKind::MaterializedView,
                Some("EXTERNAL TABLE") => RelationKind::ForeignTable,
                _ => RelationKind::Table,
            };
            match schemas.iter_mut().find(|s| s.name == schema) {
                Some(found) => found.relations.push(Relation { name, kind }),
                None => schemas.push(Schema {
                    name: schema,
                    relations: vec![Relation { name, kind }],
                }),
            }
        }
        let current = self
            .context
            .lock()
            .await
            .schema
            .clone()
            .unwrap_or_else(|| "PUBLIC".into());
        schemas.sort_by_key(|s| (s.name != current, s.name.clone()));
        Ok(Catalog { schemas })
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let db = self
            .database()
            .await?
            .ok_or_else(|| Error::query("no database is selected"))?;
        let schema = match &table.schema {
            Some(schema) => schema.clone(),
            None => self
                .context
                .lock()
                .await
                .schema
                .clone()
                .unwrap_or_else(|| "PUBLIC".into()),
        };
        let sql = format!(
            "SELECT column_name, data_type, is_nullable, column_default, numeric_precision, \
                    numeric_scale, character_maximum_length \
             FROM {}.information_schema.columns \
             WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
            ident(&db)
        );
        let bindings = json!({"1": binding(&schema), "2": binding(&table.name)});
        let rows = self.texts(&sql, Some(bindings)).await?;
        if rows.is_empty() {
            return Err(Error::query(format!(
                "table {}.{} does not exist",
                ident(&schema),
                ident(&table.name)
            )));
        }
        let columns = rows
            .into_iter()
            .map(|row| {
                let get = |ix: usize| row.get(ix).cloned().flatten();
                let data_type = get(1).unwrap_or_default();
                let type_name = match (data_type.as_str(), get(4), get(5), get(6)) {
                    ("NUMBER", Some(p), Some(s), _) => format!("number({p},{s})"),
                    ("TEXT", _, _, Some(len)) => format!("varchar({len})"),
                    (other, ..) => other.to_ascii_lowercase(),
                };
                ColumnInfo {
                    name: get(0).unwrap_or_default(),
                    type_name,
                    nullable: get(2).as_deref() == Some("YES"),
                    default: get(3),
                }
            })
            .collect();
        let keys_sql = format!(
            "SHOW PRIMARY KEYS IN TABLE {}.{}.{}",
            ident(&db),
            ident(&schema),
            ident(&table.name)
        );
        // Columns of SHOW PRIMARY KEYS: ..., column_name (4), key_sequence (5).
        let mut keys: Vec<(i64, String)> = self
            .texts(&keys_sql, None)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let name = row.get(4).cloned().flatten()?;
                let seq = row.get(5).cloned().flatten()?.parse().ok()?;
                Some((seq, name))
            })
            .collect();
        keys.sort();
        Ok(TableDetail {
            columns,
            primary_key: keys.into_iter().map(|(_, name)| name).collect(),
            ..TableDetail::default()
        })
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<()> {
        // A scripting block keeps the change set in one transaction across
        // what would otherwise be separate requests.
        let mut script = String::from("EXECUTE IMMEDIATE $$\nBEGIN\n  BEGIN TRANSACTION;\n");
        for statement in statements {
            script.push_str(&format!(
                "  {};\n  IF (SQLROWCOUNT <> 1) THEN\n    ROLLBACK;\n    RETURN 'stale';\n  END IF;\n",
                statement.rendered()
            ));
        }
        script.push_str("  COMMIT;\n  RETURN 'ok';\nEND;\n$$");
        let rows = self.texts(&script, None).await?;
        match rows
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .as_deref()
        {
            Some("ok") => Ok(()),
            Some("stale") => Err(Error::Stale),
            other => Err(Error::query(format!("the change set ended with {other:?}"))),
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &SnowflakeDialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_should_read_the_api_encodings() {
        let col = |kind: &str, scale: i64| json!({"type": kind, "scale": scale});

        assert_eq!(cell(&json!("42"), &col("fixed", 0)), Value::Int(42));
        assert_eq!(cell(&json!("1.50"), &col("fixed", 2)).to_string(), "1.50");
        assert_eq!(
            cell(&json!("19723"), &col("date", 0)).to_string(),
            "2024-01-01"
        );
        assert_eq!(
            cell(&json!("1704067200.250000000"), &col("timestamp_ntz", 9)).to_string(),
            "2024-01-01 00:00:00.250"
        );
        assert_eq!(
            cell(&json!("1704067200.000000000 1500"), &col("timestamp_tz", 9)).to_string(),
            "2024-01-01 00:00:00+00:00"
        );
        assert_eq!(
            cell(&json!("3723.5"), &col("time", 9)).to_string(),
            "01:02:03.500"
        );
        assert_eq!(
            cell(&json!("00ff"), &col("binary", 0)),
            Value::Bytes(vec![0, 255].into())
        );
        assert_eq!(cell(&json!(null), &col("text", 0)), Value::Null);
    }

    #[test]
    fn use_statement_should_name_what_changes() {
        assert_eq!(
            use_statement("use warehouse wh;"),
            Some(("warehouse", "wh".into()))
        );
        assert_eq!(
            use_statement("USE db.sch"),
            Some(("schema", "db.sch".into()))
        );
        assert_eq!(use_statement("select 1"), None);
    }

    #[test]
    fn base_url_should_add_the_domain_to_an_account() {
        let cfg = |host: &str| ServerConfig {
            host: host.into(),
            port: 443,
            database: String::new(),
            user: String::new(),
            tls: crate::TlsMode::VerifyFull,
            options: Default::default(),
        };
        assert_eq!(
            base_url(&cfg("myorg-acct")),
            "https://myorg-acct.snowflakecomputing.com"
        );
        assert_eq!(
            base_url(&cfg("x.eu-west-1.aws.snowflakecomputing.com")),
            "https://x.eu-west-1.aws.snowflakecomputing.com"
        );
    }

    #[test]
    fn errors_should_point_at_the_line_and_position() {
        let sql = "select 1\nfrom nope x y";
        let reply = json!({"message": "SQL compilation error:\nsyntax error line 2 at position 12 unexpected 'y'.", "sqlState": "42000"});

        let error = reply_error(StatusCode::UNPROCESSABLE_ENTITY, &reply, sql);

        assert_eq!(error.position().map(|p| &sql[p..]), Some("y"));
    }
}
