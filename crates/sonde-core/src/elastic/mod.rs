//! The Elasticsearch driver, which also serves OpenSearch.
//!
//! The query tab takes SQL, sent to the SQL endpoint and paged through its
//! cursor, and REST requests written the way Kibana's console takes them:
//!
//! ```text
//! GET /logs/_search
//! { "query": { "match": { "message": "error" } } }
//! ```
//!
//! A search's hits come back as rows of their `_source` fields; other
//! replies as a row of their top-level fields, or a row per item when the
//! reply is a list.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde_json::{Value as Json, json};

use crate::{
    driver::{
        Canceller, ConnectionConfig, DbKind, Dialect, Driver, QueryHandle, Secret, ServerConfig,
        Session, TlsMode,
    },
    edit::EditStatement,
    error::{Error, Result},
    schema::{Catalog, ColumnInfo, Relation, RelationKind, Schema, TableDetail, TableRef},
    split::is_request_line,
    stream::{
        self, CancelSignal, Columns, Feed, Ready, bridge, json_table, json_type, json_value, meta,
    },
    value::{ColumnMeta, Value},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_SIZE: u64 = 1_000;

/// Elasticsearch SQL's spelling.
#[derive(Debug, Clone, Copy)]
pub struct ElasticDialect;

impl Dialect for ElasticDialect {
    fn kind(&self) -> DbKind {
        DbKind::Elasticsearch
    }

    fn placeholder(&self, _: usize) -> String {
        "?".to_owned()
    }

    /// Indices have no schema in SQL; the browser's schema is the cluster.
    fn quote_table(&self, table: &TableRef) -> String {
        self.quote_ident(&table.name)
    }

    fn limit_clause(&self, limit: u64, _offset: u64) -> String {
        format!("LIMIT {limit}")
    }

    fn read_only_reason(&self) -> Option<&'static str> {
        Some(
            "Elasticsearch SQL cannot change documents. Use a REST request in a query tab, \
             such as POST /index/_update/<id>.",
        )
    }
}

/// Opens Elasticsearch and OpenSearch sessions.
#[derive(Debug, Clone, Copy)]
pub struct ElasticDriver;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Elasticsearch,
    OpenSearch,
}

#[derive(Clone)]
struct Cluster {
    http: Client,
    base: String,
    user: Option<String>,
    secret: Option<String>,
    flavor: Flavor,
    name: String,
}

impl Cluster {
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let path = path.trim();
        let url = format!(
            "{}{}{}",
            self.base,
            if path.starts_with('/') { "" } else { "/" },
            path
        );
        let request = self.http.request(method, url);
        match (&self.user, &self.secret) {
            (Some(user), secret) => request.basic_auth(user, secret.as_ref()),
            (None, Some(key)) => request.header("Authorization", format!("ApiKey {key}")),
            (None, None) => request,
        }
    }

    /// Send a request and read its JSON reply, turning error replies into
    /// query errors.
    async fn call(&self, method: Method, path: &str, body: Option<Body>) -> Result<Json> {
        let mut request = self.request(method, path);
        match body {
            Some(Body::Json(json)) => request = request.json(&json),
            Some(Body::Text(text, ndjson)) => {
                let kind = if ndjson {
                    "application/x-ndjson"
                } else {
                    "application/json"
                };
                request = request.header("Content-Type", kind).body(text);
            }
            None => {}
        }
        let response = request.send().await.map_err(|error| http_error(&error))?;
        let status = response.status();
        let text = response.text().await.map_err(|error| http_error(&error))?;
        let json = serde_json::from_str::<Json>(&text).unwrap_or(Json::String(text));
        if status.is_success() {
            Ok(json)
        } else {
            Err(reply_error(status, &json))
        }
    }
}

enum Body {
    Json(Json),
    /// Raw text, and whether it is newline-delimited JSON.
    Text(String, bool),
}

fn http_error(error: &reqwest::Error) -> Error {
    if error.is_connect() || error.is_timeout() {
        Error::Closed
    } else {
        Error::query(error.to_string())
    }
}

/// The message in an error reply, with the position SQL errors give as
/// `line L:C`.
fn reply_error(status: StatusCode, json: &Json) -> Error {
    let error = &json["error"];
    let reason = error["reason"]
        .as_str()
        .or_else(|| error["root_cause"][0]["reason"].as_str())
        .or_else(|| error["details"].as_str())
        .or_else(|| json["message"].as_str())
        .map(str::to_owned)
        .or_else(|| error.as_str().map(str::to_owned))
        .unwrap_or_else(|| match json {
            Json::String(text) if !text.is_empty() => text.clone(),
            _ => format!("the server answered {status}"),
        });
    let kind = error["type"].as_str().map(str::to_owned);
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Error::Query {
            message: reason,
            position: None,
            code: Some(status.as_u16().to_string()),
            detail: None,
            hint: Some("check the user name and password, or the API key".into()),
        };
    }
    Error::Query {
        position: None,
        detail: kind.clone(),
        code: kind.or_else(|| Some(status.as_u16().to_string())),
        message: reason,
        hint: None,
    }
}

/// The byte offset of a `line L:C` position in `sql`.
fn sql_position(message: &str, sql: &str) -> Option<usize> {
    let at = message.find("line ")?;
    let rest = &message[at + 5..];
    let (line, rest) = rest.split_once(':')?;
    let column: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let (line, column): (usize, usize) = (line.parse().ok()?, column.parse().ok()?);
    let start = sql
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum::<usize>();
    let offset = start + column.saturating_sub(1);
    (offset <= sql.len()).then_some(offset)
}

fn base_url(cfg: &ServerConfig, https: bool) -> String {
    let host = cfg.host.trim().trim_end_matches('/');
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.to_owned();
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    format!(
        "{}://{host}:{}",
        if https { "https" } else { "http" },
        cfg.port
    )
}

#[async_trait]
impl Driver for ElasticDriver {
    fn kind(&self) -> DbKind {
        DbKind::Elasticsearch
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Elasticsearch(cfg) = cfg else {
            return Err(Error::Config("not an Elasticsearch connection".into()));
        };
        let cfg = cfg.clone();
        let secret = secret
            .map(|secret| secret.expose().to_owned())
            .filter(|s| !s.is_empty());
        let session = bridge(async move {
            let http = Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .danger_accept_invalid_certs(cfg.tls != TlsMode::VerifyFull)
                .build()
                .map_err(|error| Error::Config(error.to_string()))?;
            let user = Some(cfg.user.trim().to_owned()).filter(|u| !u.is_empty());
            let mut cluster = Cluster {
                http,
                base: base_url(&cfg, cfg.tls != TlsMode::Disable),
                user,
                secret,
                flavor: Flavor::Elasticsearch,
                name: String::new(),
            };
            let explicit = cfg.host.starts_with("http");
            let mut root = cluster.call(Method::GET, "/", None).await;
            // "Prefer" tries TLS first and falls back to plain HTTP.
            if cfg.tls == TlsMode::Prefer && !explicit && matches!(root, Err(Error::Closed)) {
                cluster.base = base_url(&cfg, false);
                root = cluster.call(Method::GET, "/", None).await;
            }
            let root = root.map_err(|error| Error::Connect(connect_problem(&error, &cfg)))?;
            if root["version"]["distribution"].as_str() == Some("opensearch") {
                cluster.flavor = Flavor::OpenSearch;
            }
            cluster.name = root["cluster_name"]
                .as_str()
                .unwrap_or("cluster")
                .to_owned();
            Ok(ElasticSession {
                cluster,
                cancel: CancelSignal::new(),
            })
        })
        .await?;
        Ok(Box::new(session))
    }
}

fn connect_problem(error: &Error, cfg: &ServerConfig) -> String {
    let at = base_url(cfg, cfg.tls != TlsMode::Disable);
    match error {
        Error::Closed => format!("nothing answered at {at}; is the cluster running?"),
        Error::Query { code, .. } if code.as_deref() == Some("401") => {
            format!("{at} rejected the user name and password, or the API key")
        }
        Error::Query { message, .. } => format!("{at} answered with an error: {message}"),
        other => format!("could not reach {at}: {other}"),
    }
}

struct ElasticSession {
    cluster: Cluster,
    cancel: CancelSignal,
}

/// A cell from a SQL result, by the column's type.
fn sql_cell(value: &Json, type_name: &str) -> Value {
    if matches!(type_name, "datetime" | "date" | "timestamp")
        && let Some(text) = value.as_str()
        && let Ok(t) = chrono::DateTime::parse_from_rfc3339(text)
    {
        return Value::TimestampTz(t.with_timezone(&chrono::Utc));
    }
    json_value(value)
}

/// The columns and rows of a SQL reply, in either engine's layout.
fn sql_page(reply: &Json) -> (Vec<(String, String)>, Vec<Vec<Json>>) {
    let columns = reply["columns"]
        .as_array()
        .or_else(|| reply["schema"].as_array())
        .map(|columns| {
            columns
                .iter()
                .map(|c| {
                    let name = c["alias"]
                        .as_str()
                        .or_else(|| c["name"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    (name, c["type"].as_str().unwrap_or("").to_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    let rows = reply["rows"]
        .as_array()
        .or_else(|| reply["datarows"].as_array())
        .map(|rows| {
            rows.iter()
                .map(|row| row.as_array().cloned().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();
    (columns, rows)
}

impl Cluster {
    fn sql_path(&self) -> &'static str {
        match self.flavor {
            Flavor::Elasticsearch => "/_sql?format=json",
            Flavor::OpenSearch => "/_plugins/_sql?format=jdbc",
        }
    }

    async fn close_cursor(&self, cursor: String) {
        let path = match self.flavor {
            Flavor::Elasticsearch => "/_sql/close",
            Flavor::OpenSearch => "/_plugins/_sql/close",
        };
        let _ = self
            .call(
                Method::POST,
                path,
                Some(Body::Json(json!({"cursor": cursor}))),
            )
            .await;
    }

    async fn sql(&self, sql: String, ready: Ready, cancel: CancelSignal) {
        let pressed = cancel.pressed();
        tokio::pin!(pressed);
        let mut body = json!({"query": sql, "fetch_size": FETCH_SIZE});
        if self.flavor == Flavor::Elasticsearch {
            body["field_multi_value_leniency"] = json!(true);
        }
        let first = tokio::select! {
            biased;
            () = &mut pressed => return ready.fail(Error::Cancelled),
            reply = self.call(Method::POST, self.sql_path(), Some(Body::Json(body))) => reply,
        };
        let reply = match first {
            Ok(reply) => reply,
            Err(mut error) => {
                if let Error::Query {
                    message, position, ..
                } = &mut error
                {
                    *position = sql_position(message, &sql);
                }
                return ready.fail(error);
            }
        };
        let (columns, mut rows) = sql_page(&reply);
        let types: Vec<String> = columns.iter().map(|(_, t)| t.clone()).collect();
        let mut feed = ready.columns(
            columns
                .iter()
                .map(|(name, type_name)| meta(name, type_name))
                .collect(),
        );
        let mut cursor = reply["cursor"].as_str().map(str::to_owned);
        loop {
            for row in &rows {
                let cells = row
                    .iter()
                    .zip(&types)
                    .map(|(value, type_name)| sql_cell(value, type_name))
                    .collect();
                if !feed.push(cells).await {
                    if let Some(cursor) = cursor {
                        self.close_cursor(cursor).await;
                    }
                    return;
                }
            }
            let Some(next) = cursor.take().filter(|c| !c.is_empty()) else {
                break;
            };
            let reply = tokio::select! {
                biased;
                () = &mut pressed => {
                    self.close_cursor(next).await;
                    return feed.fail(Error::Cancelled).await;
                }
                reply = self.call(
                    Method::POST,
                    self.sql_path(),
                    Some(Body::Json(json!({"cursor": next}))),
                ) => reply,
            };
            match reply {
                Ok(reply) => {
                    rows = sql_page(&reply).1;
                    cursor = reply["cursor"].as_str().map(str::to_owned);
                    if rows.is_empty() {
                        break;
                    }
                }
                Err(error) => return feed.fail(error).await,
            }
        }
        feed.finish(None, Vec::new()).await;
    }

    async fn rest(&self, request: &str, ready: Ready, cancel: CancelSignal) {
        let (line, body) = request.split_once('\n').unwrap_or((request, ""));
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("GET").to_ascii_uppercase();
        let mut path = parts.next().unwrap_or("/").to_owned();
        let Ok(method) = Method::from_bytes(method.as_bytes()) else {
            return ready.fail(Error::query(format!("{method} is not an HTTP method")));
        };
        // _cat endpoints answer in columns of text unless asked for JSON.
        if path.trim_start_matches('/').starts_with("_cat") && !path.contains("format=") {
            path.push(if path.contains('?') { '&' } else { '?' });
            path.push_str("format=json");
        }
        let body = body.trim();
        let body = (!body.is_empty()).then(|| {
            let ndjson = path.contains("_bulk") || path.contains("_msearch");
            let mut text = body.to_owned();
            if ndjson {
                text.push('\n');
            }
            Body::Text(text, ndjson)
        });
        let reply = tokio::select! {
            biased;
            () = cancel.pressed() => return ready.fail(Error::Cancelled),
            reply = self.call(method, &path, body) => reply,
        };
        match reply {
            Ok(reply) => shape(reply, ready).await,
            Err(error) => ready.fail(error),
        }
    }
}

/// A REST reply as rows.
async fn shape(reply: Json, ready: Ready) {
    if let Some(hits) = reply["hits"]["hits"].as_array() {
        let aggregations = reply.get("aggregations");
        if hits.is_empty()
            && let Some(Json::Object(aggs)) = aggregations
            && aggs.len() == 1
            && let Some(buckets) = aggs.values().next().and_then(|a| a["buckets"].as_array())
        {
            return rows(ready, json_table(buckets)).await;
        }
        if !hits.is_empty() || aggregations.is_none() {
            return search_hits(hits, ready).await;
        }
    }
    match &reply {
        Json::Array(items) => rows(ready, json_table(items)).await,
        Json::Object(_) => rows(ready, json_table(std::slice::from_ref(&reply))).await,
        other => {
            let mut feed = ready.columns(vec![meta("response", json_type(other))]);
            feed.push(vec![json_value(other)]).await;
            feed.finish(None, Vec::new()).await;
        }
    }
}

async fn rows(ready: Ready, (columns, rows): (Vec<ColumnMeta>, Vec<Vec<Value>>)) {
    let mut feed = ready.columns(columns);
    for row in rows {
        if !feed.push(row).await {
            return;
        }
    }
    feed.finish(None, Vec::new()).await;
}

async fn search_hits(hits: &[Json], ready: Ready) {
    let mut columns = Columns::default();
    columns.add("_index", "keyword");
    columns.add("_id", "keyword");
    columns.add("_score", "float");
    for hit in hits {
        for (key, value) in hit["_source"].as_object().into_iter().flatten() {
            columns.add(key, json_type(value));
        }
    }
    let mut feed: Feed = ready.columns(columns.meta());
    for hit in hits {
        let mut row = vec![
            json_value(&hit["_index"]),
            json_value(&hit["_id"]),
            json_value(&hit["_score"]),
        ];
        row.resize(columns.len(), Value::Null);
        for (key, value) in hit["_source"].as_object().into_iter().flatten() {
            if let Some(ix) = columns.position(key) {
                row[ix] = json_value(value);
            }
        }
        if !feed.push(row).await {
            return;
        }
    }
    feed.finish(None, Vec::new()).await;
}

/// The fields of a mapping as dotted names and types. Nested fields are
/// left out: SQL cannot read them as columns.
fn fields(properties: &Json, prefix: &str, out: &mut Vec<(String, String)>) {
    for (name, field) in properties.as_object().into_iter().flatten() {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let kind = field["type"].as_str();
        match (kind, field.get("properties")) {
            (Some("nested"), _) => {}
            (None | Some("object"), Some(children)) => fields(children, &path, out),
            (Some(kind), _) => {
                if !out.iter().any(|(existing, _)| *existing == path) {
                    out.push((path, kind.to_owned()));
                }
            }
            (None, None) => {}
        }
    }
}

#[async_trait]
impl Session for ElasticSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        if !params.is_empty() {
            return Err(Error::query("parameters are not supported here"));
        }
        let cluster = self.cluster.clone();
        let cancel = self.cancel.clone();
        let text = sql.trim().to_owned();
        let request = is_request_line(text.lines().next().unwrap_or(""));
        stream::start(move |ready| async move {
            if request {
                cluster.rest(&text, ready, cancel).await;
            } else {
                cluster.sql(text, ready, cancel).await;
            }
        })
        .await
    }

    fn canceller(&self) -> Canceller {
        self.cancel.canceller()
    }

    async fn catalog(&self) -> Result<Catalog> {
        let cluster = self.cluster.clone();
        bridge(async move {
            let indices = cluster
                .call(
                    Method::GET,
                    "/_cat/indices?format=json&h=index&s=index",
                    None,
                )
                .await?;
            let mut relations: Vec<Relation> = indices
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row["index"].as_str())
                .filter(|name| !name.starts_with('.'))
                .map(|name| Relation {
                    name: name.to_owned(),
                    kind: RelationKind::Collection,
                })
                .collect();
            if let Ok(aliases) = cluster
                .call(Method::GET, "/_cat/aliases?format=json&h=alias", None)
                .await
            {
                for alias in aliases.as_array().into_iter().flatten() {
                    if let Some(name) = alias["alias"].as_str()
                        && !name.starts_with('.')
                        && !relations.iter().any(|r| r.name == name)
                    {
                        relations.push(Relation {
                            name: name.to_owned(),
                            kind: RelationKind::View,
                        });
                    }
                }
            }
            if let Ok(streams) = cluster.call(Method::GET, "/_data_stream", None).await {
                for stream in streams["data_streams"].as_array().into_iter().flatten() {
                    if let Some(name) = stream["name"].as_str()
                        && !name.starts_with('.')
                    {
                        relations.push(Relation {
                            name: name.to_owned(),
                            kind: RelationKind::Collection,
                        });
                    }
                }
            }
            relations.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(Catalog {
                schemas: vec![Schema {
                    name: cluster.name.clone(),
                    relations,
                }],
            })
        })
        .await
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let cluster = self.cluster.clone();
        let name = table.name.clone();
        bridge(async move {
            let path = format!("/{}/_mapping", encode_path(&name));
            let mapping = match cluster.call(Method::GET, &path, None).await {
                Err(Error::Query { code, .. })
                    if code.as_deref() == Some("index_not_found_exception") =>
                {
                    return Err(Error::query(format!("index {name:?} does not exist")));
                }
                other => other?,
            };
            let mut found = Vec::new();
            for index in mapping.as_object().into_iter().flatten().map(|(_, v)| v) {
                fields(&index["mappings"]["properties"], "", &mut found);
            }
            Ok(TableDetail {
                columns: found
                    .into_iter()
                    .map(|(name, type_name)| ColumnInfo {
                        name,
                        type_name,
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
            ElasticDialect.read_only_reason().unwrap_or_default().into(),
        ))
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &ElasticDialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

fn encode_path(name: &str) -> String {
    let mut out = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'*' | b',') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_position_should_find_the_line_and_column() {
        let sql = "SELECT a\nFROM b WHERE nope";

        assert_eq!(
            sql_position("line 2:14: Unknown column [nope]", sql),
            Some(22)
        );
        assert_eq!(&sql[22..], "nope");
    }

    #[test]
    fn fields_should_flatten_objects_and_skip_nested() {
        let mapping: Json = serde_json::from_str(
            r#"{"a": {"type": "keyword"}, "o": {"properties": {"x": {"type": "long"}}},
                "n": {"type": "nested", "properties": {"y": {"type": "long"}}}}"#,
        )
        .unwrap();
        let mut out = Vec::new();

        fields(&mapping, "", &mut out);

        assert_eq!(
            out,
            [
                ("a".to_owned(), "keyword".to_owned()),
                ("o.x".into(), "long".into())
            ]
        );
    }
}
