//! The Cassandra driver, which also serves ScyllaDB, through the `scylla`
//! crate.
//!
//! CQL runs as written, paged by the driver. The table view reads a table
//! without sorting, since CQL orders rows only within a partition, and adds
//! `ALLOW FILTERING` to a filter. Edits are lightweight transactions
//! (`IF EXISTS`), one at a time: Cassandra has no multi-row transactions,
//! so a change set that fails part way keeps the changes before the failure.
//!
//! Every connection goes to the node the user named. The addresses nodes
//! advertise are often unreachable from a desktop (a container's internal
//! address, a cloud node's private one), and a coordinator forwards to the
//! rest of the cluster anyway.

use std::{net::IpAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::StreamExt as _;
use scylla::{
    client::{session::Session as Cluster, session_builder::SessionBuilder},
    errors::TranslationError,
    frame::response::result::{ColumnType, NativeType},
    policies::address_translator::{AddressTranslator, UntranslatedPeer},
    value::{CqlDate, CqlDecimal, CqlTime, CqlTimestamp, CqlValue, Row},
};
use tokio::sync::Mutex;

use crate::{
    driver::{
        Canceller, ConnectionConfig, DbKind, Dialect, Driver, QueryHandle, Secret, ServerConfig,
        Session, TlsMode,
    },
    edit::{self, EditStatement},
    error::{Error, Result},
    schema::{
        Catalog, ColumnInfo, IndexInfo, Relation, RelationKind, Schema, TableDetail, TableRef,
    },
    stream::{self, CancelSignal, bridge, meta},
    value::Value,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// CQL's spelling.
#[derive(Debug, Clone, Copy)]
pub struct CassandraDialect;

impl Dialect for CassandraDialect {
    fn kind(&self) -> DbKind {
        DbKind::Cassandra
    }

    fn placeholder(&self, _: usize) -> String {
        "?".to_owned()
    }

    fn limit_clause(&self, limit: u64, _offset: u64) -> String {
        format!("LIMIT {limit}")
    }

    fn browse(
        &self,
        table: &TableRef,
        columns: &[String],
        filter: &str,
        _sort: Option<(&str, bool)>,
    ) -> String {
        let names: Vec<String> = columns.iter().map(|c| self.quote_ident(c)).collect();
        let mut sql = format!(
            "SELECT {} FROM {}",
            names.join(", "),
            self.quote_table(table)
        );
        let filter = filter.trim();
        if !filter.is_empty() {
            // CQL has no parenthesised conditions; it has no OR either, so
            // there is nothing for them to protect.
            sql.push_str(&format!(" WHERE {filter}\nALLOW FILTERING"));
        }
        sql
    }

    fn sorts(&self) -> bool {
        false
    }

    fn filter_hint(&self) -> &'static str {
        "Filter rows with a CQL condition, such as id = 10"
    }

    fn insert(&self, table: &TableRef, columns: &[&str], values: &[Value]) -> EditStatement {
        let mut statement = edit::sql_insert(self, table, columns, values);
        statement.sql.push_str(" IF NOT EXISTS");
        statement
    }

    fn update(
        &self,
        table: &TableRef,
        key_columns: &[&str],
        key: &[Value],
        columns: &[&str],
        values: &[Value],
    ) -> EditStatement {
        let mut statement = edit::sql_update(self, table, key_columns, key, columns, values);
        statement.sql.push_str(" IF EXISTS");
        statement
    }

    fn delete(&self, table: &TableRef, key_columns: &[&str], key: &[Value]) -> EditStatement {
        let mut statement = edit::sql_delete(self, table, key_columns, key);
        statement.sql.push_str(" IF EXISTS");
        statement
    }
}

/// Opens Cassandra sessions.
#[derive(Debug, Clone, Copy)]
pub struct CassandraDriver;

#[async_trait]
impl Driver for CassandraDriver {
    fn kind(&self) -> DbKind {
        DbKind::Cassandra
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Cassandra(cfg) = cfg else {
            return Err(Error::Config("not a Cassandra connection".into()));
        };
        let cfg = cfg.clone();
        let password = secret.map(|s| s.expose().to_owned()).unwrap_or_default();
        let session = bridge(async move {
            let host = if cfg.host.contains(':') && !cfg.host.starts_with('[') {
                format!("[{}]", cfg.host)
            } else {
                cfg.host.clone()
            };
            let contact = tokio::net::lookup_host(format!("{host}:{}", cfg.port))
                .await
                .ok()
                .and_then(|mut addrs| addrs.next())
                .ok_or_else(|| Error::Connect(format!("could not look up {}", cfg.host)))?;
            let mut builder = SessionBuilder::new()
                .known_node_addr(contact)
                .address_translator(Arc::new(ThroughContact(contact)))
                .connection_timeout(CONNECT_TIMEOUT);
            if !cfg.user.trim().is_empty() {
                builder = builder.user(cfg.user.trim(), password);
            }
            if matches!(cfg.tls, TlsMode::Require | TlsMode::VerifyFull) {
                let tls = crate::postgres::tls::client_config(cfg.tls)?;
                builder = builder.tls_context(Some(Arc::new(tls)));
            }
            let keyspace = cfg.database.trim().to_owned();
            if !keyspace.is_empty() {
                builder = builder.use_keyspace(&keyspace, false);
            }
            let cluster = tokio::time::timeout(CONNECT_TIMEOUT * 2, builder.build())
                .await
                .map_err(|_| Error::Connect(format!("{} did not answer in time", at(&cfg))))?
                .map_err(|error| Error::Connect(connect_problem(&error.to_string(), &cfg)))?;
            Ok(CassandraSession {
                cluster: Arc::new(cluster),
                keyspace: Arc::new(Mutex::new(keyspace)),
                cancel: CancelSignal::new(),
            })
        })
        .await?;
        Ok(Box::new(session))
    }
}

/// Sends every node's traffic to the address the user gave.
struct ThroughContact(std::net::SocketAddr);

#[async_trait]
impl AddressTranslator for ThroughContact {
    async fn translate_address(
        &self,
        _: &UntranslatedPeer,
    ) -> std::result::Result<std::net::SocketAddr, TranslationError> {
        Ok(self.0)
    }
}

fn at(cfg: &ServerConfig) -> String {
    format!("{}:{}", cfg.host, cfg.port)
}

fn connect_problem(text: &str, cfg: &ServerConfig) -> String {
    let at = at(cfg);
    if text.contains("Authentication")
        || text.contains("authentication")
        || text.contains("credentials")
    {
        return format!("the cluster at {at} rejected the user name or password");
    }
    if text.contains("Keyspace") && (text.contains("does not exist") || text.contains("not exist"))
    {
        return format!("keyspace {} does not exist on {at}", cfg.database);
    }
    if text.contains("refused")
        || text.contains("Could not reach any")
        || text.contains("ConnectionRefused")
    {
        return format!("nothing is accepting connections at {at}; is the cluster running?");
    }
    format!("could not reach {at}: {text}")
}

/// A driver error as a query error. Cassandra's syntax errors carry
/// `line L:C`, with the column counted from 0.
fn query_error(text: &str, sql: &str) -> Error {
    let message = text
        .split_once("Error message: ")
        .map_or(text, |(_, message)| message)
        .trim()
        .to_owned();
    if text.contains("Connection broken")
        || text.contains("Broken pipe")
        || text.contains("No connections in the pool")
    {
        return Error::Closed;
    }
    let position = message.find("line ").and_then(|ix| {
        let rest = &message[ix + 5..];
        let (line, rest) = rest.split_once(':')?;
        let column: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let (line, column): (usize, usize) = (line.parse().ok()?, column.parse().ok()?);
        let start: usize = sql
            .split_inclusive('\n')
            .take(line.saturating_sub(1))
            .map(str::len)
            .sum();
        Some(start + column).filter(|&offset| offset <= sql.len())
    });
    Error::Query {
        message,
        position,
        code: None,
        detail: None,
        hint: None,
    }
}

fn type_name(typ: &ColumnType<'_>) -> String {
    match typ {
        ColumnType::Native(native) => format!("{native:?}").to_ascii_lowercase(),
        ColumnType::Collection { typ, .. } => {
            let text = format!("{typ:?}").to_ascii_lowercase();
            text.split(['(', ' ', '{'])
                .next()
                .unwrap_or("collection")
                .to_owned()
        }
        ColumnType::Vector { .. } => "vector".to_owned(),
        ColumnType::UserDefinedType { .. } => "udt".to_owned(),
        ColumnType::Tuple(_) => "tuple".to_owned(),
        _ => "unknown".to_owned(),
    }
}

/// Days from CQL's date origin (2^31 is the Unix epoch) to a date.
fn date(days: u32) -> Option<chrono::NaiveDate> {
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?;
    epoch.checked_add_signed(chrono::TimeDelta::days(i64::from(days) - (1 << 31)))
}

fn decimal(value: &CqlDecimal) -> Option<rust_decimal::Decimal> {
    let (bytes, scale) = value.as_signed_be_bytes_slice_and_exponent();
    if bytes.len() > 12 || !(0..=28).contains(&scale) {
        return None;
    }
    let mut n: i128 = if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        -1
    } else {
        0
    };
    for byte in bytes {
        n = (n << 8) | i128::from(*byte);
    }
    rust_decimal::Decimal::try_from_i128_with_scale(n, scale as u32).ok()
}

fn json(value: &CqlValue) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => {
            J::Array(items.iter().map(json).collect())
        }
        CqlValue::Map(pairs) => {
            if pairs
                .iter()
                .all(|(k, _)| matches!(k, CqlValue::Text(_) | CqlValue::Ascii(_)))
            {
                J::Object(
                    pairs
                        .iter()
                        .map(|(k, v)| (cell(Some(k.clone())).to_string(), json(v)))
                        .collect(),
                )
            } else {
                J::Array(
                    pairs
                        .iter()
                        .map(|(k, v)| J::Array(vec![json(k), json(v)]))
                        .collect(),
                )
            }
        }
        CqlValue::Tuple(items) => J::Array(
            items
                .iter()
                .map(|item| item.as_ref().map_or(J::Null, json))
                .collect(),
        ),
        CqlValue::UserDefinedType { fields, .. } => J::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), value.as_ref().map_or(J::Null, json)))
                .collect(),
        ),
        other => match cell(Some(other.clone())) {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(b),
            Value::Int(n) => J::from(n),
            Value::Float(n) => J::from(n),
            other => J::String(other.to_string()),
        },
    }
}

fn cell(value: Option<CqlValue>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    match value {
        CqlValue::Ascii(s) | CqlValue::Text(s) => Value::text(s),
        CqlValue::Boolean(b) => Value::Bool(b),
        CqlValue::Blob(bytes) => Value::Bytes(Arc::from(bytes)),
        CqlValue::Counter(c) => Value::Int(c.0),
        CqlValue::Decimal(d) => {
            decimal(&d).map_or_else(|| Value::text(format!("{d:?}")), Value::Decimal)
        }
        CqlValue::Date(CqlDate(days)) => date(days).map_or(Value::Null, Value::Date),
        CqlValue::Double(n) => Value::Float(n),
        CqlValue::Float(n) => Value::Float(n.into()),
        CqlValue::Duration(d) => Value::Interval {
            months: d.months,
            days: d.days,
            micros: d.nanoseconds / 1000,
        },
        CqlValue::Empty => Value::Null,
        CqlValue::Int(n) => Value::Int(n.into()),
        CqlValue::BigInt(n) => Value::Int(n),
        CqlValue::SmallInt(n) => Value::Int(n.into()),
        CqlValue::TinyInt(n) => Value::Int(n.into()),
        CqlValue::Timestamp(CqlTimestamp(ms)) => {
            chrono::DateTime::from_timestamp_millis(ms).map_or(Value::Null, Value::TimestampTz)
        }
        CqlValue::Time(CqlTime(nanos)) => chrono::NaiveTime::from_num_seconds_from_midnight_opt(
            (nanos / 1_000_000_000) as u32,
            (nanos % 1_000_000_000) as u32,
        )
        .map_or(Value::Null, Value::Time),
        CqlValue::Inet(ip) => Value::text(ip.to_string()),
        CqlValue::Uuid(uuid) => Value::Uuid(uuid),
        CqlValue::Timeuuid(uuid) => Value::Uuid(*uuid.as_ref()),
        CqlValue::Varint(v) => Value::text(format!("{v:?}")),
        other @ (CqlValue::List(_)
        | CqlValue::Set(_)
        | CqlValue::Map(_)
        | CqlValue::Tuple(_)
        | CqlValue::UserDefinedType { .. }
        | CqlValue::Vector(_)) => Value::Json(Arc::from(json(&other).to_string())),
        other => Value::text(format!("{other:?}")),
    }
}

/// A value as the CQL type a bind marker wants.
fn bind(value: &Value, typ: &ColumnType<'_>) -> Result<Option<CqlValue>> {
    let refuse = || Error::Parameter {
        value: value.kind(),
        expected: type_name(typ),
    };
    if value.is_null() {
        return Ok(None);
    }
    let ColumnType::Native(native) = typ else {
        return Err(refuse());
    };
    let int = |value: &Value| match value {
        Value::Int(n) => Some(*n),
        Value::Text(t) => t.trim().parse().ok(),
        _ => None,
    };
    let bound = match native {
        NativeType::Ascii => CqlValue::Ascii(value.to_string()),
        NativeType::Text => CqlValue::Text(value.to_string()),
        NativeType::Boolean => match value {
            Value::Bool(b) => CqlValue::Boolean(*b),
            _ => return Err(refuse()),
        },
        NativeType::Int => CqlValue::Int(
            int(value)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(refuse)?,
        ),
        NativeType::BigInt => CqlValue::BigInt(int(value).ok_or_else(refuse)?),
        NativeType::SmallInt => CqlValue::SmallInt(
            int(value)
                .and_then(|n| i16::try_from(n).ok())
                .ok_or_else(refuse)?,
        ),
        NativeType::TinyInt => CqlValue::TinyInt(
            int(value)
                .and_then(|n| i8::try_from(n).ok())
                .ok_or_else(refuse)?,
        ),
        NativeType::Double => match value {
            Value::Float(n) => CqlValue::Double(*n),
            Value::Int(n) => CqlValue::Double(*n as f64),
            Value::Decimal(d) => CqlValue::Double(d.to_string().parse().map_err(|_| refuse())?),
            _ => return Err(refuse()),
        },
        NativeType::Float => match value {
            Value::Float(n) => CqlValue::Float(*n as f32),
            Value::Int(n) => CqlValue::Float(*n as f32),
            _ => return Err(refuse()),
        },
        NativeType::Decimal => {
            let d = match value {
                Value::Decimal(d) => *d,
                Value::Int(n) => rust_decimal::Decimal::from(*n),
                Value::Float(n) => rust_decimal::Decimal::try_from(*n).map_err(|_| refuse())?,
                _ => return Err(refuse()),
            };
            let bytes = d.mantissa().to_be_bytes();
            // The shortest two's complement form.
            let mut start = 0;
            while start < 15
                && ((bytes[start] == 0 && bytes[start + 1] & 0x80 == 0)
                    || (bytes[start] == 0xff && bytes[start + 1] & 0x80 != 0))
            {
                start += 1;
            }
            CqlValue::Decimal(CqlDecimal::from_signed_be_bytes_slice_and_exponent(
                &bytes[start..],
                d.scale() as i32,
            ))
        }
        NativeType::Blob => match value {
            Value::Bytes(bytes) => CqlValue::Blob(bytes.to_vec()),
            _ => return Err(refuse()),
        },
        NativeType::Uuid => match value {
            Value::Uuid(uuid) => CqlValue::Uuid(*uuid),
            _ => return Err(refuse()),
        },
        NativeType::Timeuuid => match value {
            Value::Uuid(uuid) => CqlValue::Timeuuid((*uuid).into()),
            _ => return Err(refuse()),
        },
        NativeType::Timestamp => match value {
            Value::TimestampTz(t) => CqlValue::Timestamp(CqlTimestamp(t.timestamp_millis())),
            Value::Timestamp(t) => {
                CqlValue::Timestamp(CqlTimestamp(t.and_utc().timestamp_millis()))
            }
            _ => return Err(refuse()),
        },
        NativeType::Date => match value {
            Value::Date(d) => {
                let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).ok_or_else(refuse)?;
                let days = (*d - epoch).num_days() + (1 << 31);
                CqlValue::Date(CqlDate(u32::try_from(days).map_err(|_| refuse())?))
            }
            _ => return Err(refuse()),
        },
        NativeType::Time => match value {
            Value::Time(t) => {
                use chrono::Timelike as _;
                CqlValue::Time(CqlTime(
                    i64::from(t.num_seconds_from_midnight()) * 1_000_000_000
                        + i64::from(t.nanosecond()),
                ))
            }
            _ => return Err(refuse()),
        },
        NativeType::Inet => {
            CqlValue::Inet(value.to_string().parse::<IpAddr>().map_err(|_| refuse())?)
        }
        _ => return Err(refuse()),
    };
    Ok(Some(bound))
}

/// The keyspace a `USE` statement names, if `sql` is one.
fn use_keyspace(sql: &str) -> Option<String> {
    let mut words = sql.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("use") {
        return None;
    }
    let name = words.next()?.trim_end_matches(';');
    words
        .next()
        .is_none()
        .then(|| name.trim_matches('"').to_owned())
}

fn reads_rows(sql: &str) -> bool {
    let first = sql.split_whitespace().next().unwrap_or("");
    first.eq_ignore_ascii_case("select")
}

struct CassandraSession {
    cluster: Arc<Cluster>,
    keyspace: Arc<Mutex<String>>,
    cancel: CancelSignal,
}

#[async_trait]
impl Session for CassandraSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        if !params.is_empty() {
            return Err(Error::query("parameters are only used by edits"));
        }
        let cluster = Arc::clone(&self.cluster);
        let keyspace = Arc::clone(&self.keyspace);
        let cancel = self.cancel.clone();
        let sql = sql.to_owned();
        stream::start(move |ready| async move {
            let pressed = cancel.pressed();
            tokio::pin!(pressed);
            if let Some(name) = use_keyspace(&sql) {
                return match cluster.use_keyspace(&name, false).await {
                    Ok(()) => {
                        *keyspace.lock().await = name;
                        ready.done(None, Vec::new()).await;
                    }
                    Err(error) => ready.fail(query_error(&error.to_string(), &sql)),
                };
            }
            if !reads_rows(&sql) {
                let outcome = tokio::select! {
                    biased;
                    () = &mut pressed => return ready.fail(Error::Cancelled),
                    outcome = cluster.query_unpaged(sql.as_str(), ()) => outcome,
                };
                let result = match outcome {
                    Ok(result) => result,
                    Err(error) => return ready.fail(query_error(&error.to_string(), &sql)),
                };
                // LWTs and the like answer with rows of their own.
                let Ok(rows) = result.into_rows_result() else {
                    return ready.done(None, Vec::new()).await;
                };
                let columns = rows
                    .column_specs()
                    .iter()
                    .map(|spec| meta(spec.name(), &type_name(spec.typ())))
                    .collect();
                let mut feed = ready.columns(columns);
                if let Ok(iter) = rows.rows::<Row>() {
                    for row in iter.flatten() {
                        feed.push(row.columns.into_iter().map(cell).collect()).await;
                    }
                }
                return feed.finish(None, Vec::new()).await;
            }
            let pager = tokio::select! {
                biased;
                () = &mut pressed => return ready.fail(Error::Cancelled),
                pager = cluster.query_iter(sql.as_str(), ()) => pager,
            };
            let pager = match pager {
                Ok(pager) => pager,
                Err(error) => return ready.fail(query_error(&error.to_string(), &sql)),
            };
            let columns = pager
                .column_specs()
                .iter()
                .map(|spec| meta(spec.name(), &type_name(spec.typ())))
                .collect();
            let mut rows = match pager.rows_stream::<Row>() {
                Ok(rows) => rows,
                Err(error) => return ready.fail(Error::query(error.to_string())),
            };
            let mut feed = ready.columns(columns);
            loop {
                let next = tokio::select! {
                    biased;
                    () = &mut pressed => return feed.fail(Error::Cancelled).await,
                    next = rows.next() => next,
                };
                match next {
                    Some(Ok(row)) => {
                        if !feed.push(row.columns.into_iter().map(cell).collect()).await {
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        return feed.fail(query_error(&error.to_string(), &sql)).await;
                    }
                    None => break,
                }
            }
            feed.finish(None, Vec::new()).await;
        })
        .await
    }

    fn canceller(&self) -> Canceller {
        self.cancel.canceller()
    }

    async fn catalog(&self) -> Result<Catalog> {
        let cluster = Arc::clone(&self.cluster);
        let keyspace = Arc::clone(&self.keyspace);
        bridge(async move {
            let current = keyspace.lock().await.clone();
            let strings = |sql: &'static str| {
                let cluster = Arc::clone(&cluster);
                async move {
                    let result = cluster
                        .query_unpaged(sql, ())
                        .await
                        .map_err(|error| query_error(&error.to_string(), sql))?
                        .into_rows_result()
                        .map_err(|error| Error::query(error.to_string()))?;
                    let rows: Vec<(String, String)> = result
                        .rows::<(String, String)>()
                        .map_err(|error| Error::query(error.to_string()))?
                        .filter_map(std::result::Result::ok)
                        .collect();
                    Ok::<_, Error>(rows)
                }
            };
            let keyspaces =
                strings("SELECT keyspace_name, replication['class'] FROM system_schema.keyspaces")
                    .await?;
            let tables =
                strings("SELECT keyspace_name, table_name FROM system_schema.tables").await?;
            let views = strings("SELECT keyspace_name, view_name FROM system_schema.views").await?;
            let mut names: Vec<String> = keyspaces
                .into_iter()
                .map(|(name, _)| name)
                .filter(|name| !name.starts_with("system") || *name == current)
                .collect();
            names.sort_by_key(|name| (*name != current, name.clone()));
            let schemas = names
                .into_iter()
                .map(|name| {
                    let mut relations: Vec<Relation> = tables
                        .iter()
                        .filter(|(ks, _)| *ks == name)
                        .map(|(_, table)| Relation {
                            name: table.clone(),
                            kind: RelationKind::Table,
                        })
                        .chain(views.iter().filter(|(ks, _)| *ks == name).map(|(_, view)| {
                            Relation {
                                name: view.clone(),
                                kind: RelationKind::View,
                            }
                        }))
                        .collect();
                    relations.sort_by(|a, b| a.name.cmp(&b.name));
                    Schema { name, relations }
                })
                .collect();
            Ok(Catalog { schemas })
        })
        .await
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let cluster = Arc::clone(&self.cluster);
        let keyspace = Arc::clone(&self.keyspace);
        let table = table.clone();
        bridge(async move {
            let current = keyspace.lock().await.clone();
            let ks = table.schema.clone().unwrap_or(current);
            let sql = "SELECT column_name, kind, position, type FROM system_schema.columns \
                       WHERE keyspace_name = ? AND table_name = ?";
            let result = cluster
                .query_unpaged(sql, (&ks, &table.name))
                .await
                .map_err(|error| query_error(&error.to_string(), sql))?
                .into_rows_result()
                .map_err(|error| Error::query(error.to_string()))?;
            let mut columns: Vec<(String, String, i32, String)> = result
                .rows::<(String, String, i32, String)>()
                .map_err(|error| Error::query(error.to_string()))?
                .filter_map(std::result::Result::ok)
                .collect();
            if columns.is_empty() {
                return Err(Error::query(format!("table \"{ks}\".\"{}\" does not exist", table.name)));
            }
            let rank = |kind: &str| match kind {
                "partition_key" => 0,
                "clustering" => 1,
                "static" => 2,
                _ => 3,
            };
            columns.sort_by(|a, b| (rank(&a.1), a.2, &a.0).cmp(&(rank(&b.1), b.2, &b.0)));
            let is_view = cluster
                .query_unpaged(
                    "SELECT view_name FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?",
                    (&ks, &table.name),
                )
                .await
                .ok()
                .and_then(|result| result.into_rows_result().ok())
                .is_some_and(|rows| rows.rows_num() > 0);
            let primary_key = if is_view {
                Vec::new()
            } else {
                columns
                    .iter()
                    .filter(|c| rank(&c.1) <= 1)
                    .map(|c| c.0.clone())
                    .collect()
            };
            let mut indexes = vec![IndexInfo {
                name: "primary key".into(),
                columns: columns.iter().filter(|c| rank(&c.1) <= 1).map(|c| Some(c.0.clone())).collect(),
                unique: true,
                primary: true,
            }];
            let index_sql = "SELECT index_name, options FROM system_schema.indexes \
                             WHERE keyspace_name = ? AND table_name = ?";
            if let Ok(result) = cluster.query_unpaged(index_sql, (&ks, &table.name)).await
                && let Ok(rows) = result.into_rows_result()
                && let Ok(iter) = rows.rows::<(String, std::collections::HashMap<String, String>)>()
            {
                for (name, options) in iter.flatten() {
                    indexes.push(IndexInfo {
                        name,
                        columns: vec![options.get("target").cloned()],
                        unique: false,
                        primary: false,
                    });
                }
            }
            Ok(TableDetail {
                columns: columns
                    .into_iter()
                    .map(|(name, kind, _, type_name)| ColumnInfo {
                        nullable: rank(&kind) > 1,
                        name,
                        type_name,
                        default: None,
                    })
                    .collect(),
                primary_key,
                foreign_keys: Vec::new(),
                indexes,
                has_rowid: false,
            })
        })
        .await
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<()> {
        let cluster = Arc::clone(&self.cluster);
        let statements = statements.to_vec();
        bridge(async move {
            for statement in &statements {
                let prepared = cluster
                    .prepare(statement.sql.as_str())
                    .await
                    .map_err(|error| query_error(&error.to_string(), &statement.sql))?;
                let specs = prepared.get_variable_col_specs();
                let values = statement
                    .params
                    .iter()
                    .zip(specs.iter())
                    .map(|(value, spec)| bind(value, spec.typ()))
                    .collect::<Result<Vec<_>>>()?;
                let result = cluster
                    .execute_unpaged(&prepared, values)
                    .await
                    .map_err(|error| query_error(&error.to_string(), &statement.sql))?;
                // An LWT answers with `[applied]` first.
                let applied = result
                    .into_rows_result()
                    .ok()
                    .and_then(|rows| {
                        rows.rows::<Row>()
                            .ok()?
                            .next()?
                            .ok()
                            .and_then(|row| row.columns.into_iter().next().flatten())
                    })
                    .is_none_or(|value| value == CqlValue::Boolean(true));
                if !applied {
                    return Err(Error::Stale);
                }
            }
            Ok(())
        })
        .await
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &CassandraDialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_should_be_lightweight_transactions() {
        let table = TableRef::new("ks", "t");

        let update =
            CassandraDialect.update(&table, &["id"], &[Value::Int(1)], &["n"], &[Value::Int(2)]);
        let delete = CassandraDialect.delete(&table, &["id"], &[Value::Int(1)]);

        assert_eq!(
            update.sql,
            "UPDATE \"ks\".\"t\" SET \"n\" = ? WHERE \"id\" = ? IF EXISTS"
        );
        assert_eq!(
            delete.sql,
            "DELETE FROM \"ks\".\"t\" WHERE \"id\" = ? IF EXISTS"
        );
    }

    #[test]
    fn browse_should_allow_filtering_and_not_sort() {
        let sql = CassandraDialect.browse(
            &TableRef::new("ks", "t"),
            &["id".into()],
            "n > 1",
            Some(("id", true)),
        );

        assert_eq!(
            sql,
            "SELECT \"id\" FROM \"ks\".\"t\" WHERE n > 1\nALLOW FILTERING"
        );
    }

    #[test]
    fn decimal_should_read_negative_values() {
        let d = CqlDecimal::from_signed_be_bytes_slice_and_exponent(&[0xff, 0x85], 2);

        assert_eq!(decimal(&d).unwrap().to_string(), "-1.23");
    }

    #[test]
    fn use_keyspace_should_spot_use_statements() {
        assert_eq!(use_keyspace("USE shop;").as_deref(), Some("shop"));
        assert_eq!(use_keyspace("select 1"), None);
    }
}
