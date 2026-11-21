//! The MongoDB driver.
//!
//! The query tab takes `mongosh`-style calls (see [`shell`]). Documents come
//! back as rows: every top-level field of the first page is a column, and
//! nested documents and arrays are shown as extended JSON. The table view
//! edits documents by `_id`, inside a transaction when the server runs as a
//! replica set or behind `mongos`.

mod shell;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bson::{Bson, Document, doc};
use futures::{StreamExt as _, TryStreamExt as _};
use mongodb::{
    Client, Collection, Database, IndexModel,
    options::{AggregateOptions, ClientOptions, FindOptions, IndexOptions},
    results::CollectionType,
};
use tokio::sync::Mutex;

use self::shell::{Call, Db};
use crate::{
    driver::{
        Canceller, ConnectionConfig, DbKind, Dialect, Driver, PAGE_ROWS, QueryHandle, Secret,
        ServerConfig, Session, TlsMode,
    },
    edit::EditStatement,
    error::{Error, Result},
    schema::{
        Catalog, ColumnInfo, IndexInfo, Relation, RelationKind, Schema, TableDetail, TableRef,
    },
    stream::{self, CancelSignal, Columns, Feed, Ready, bridge, meta},
    value::Value,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Documents read to learn a collection's fields.
const SAMPLE: i64 = 200;
/// Databases the browser leaves out unless the session is in one.
const SYSTEM: [&str; 3] = ["admin", "config", "local"];

/// How documents are addressed. There is no SQL; the table view's
/// statements are `mongosh` calls.
#[derive(Debug, Clone, Copy)]
pub struct MongoDialect;

/// A string as a shell literal.
fn quoted(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

/// A cell's value as a shell literal, of the BSON type it was read as.
pub(crate) fn literal(value: &Value) -> String {
    use base64::Engine as _;
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) if i32::try_from(*n).is_ok() => n.to_string(),
        Value::Int(n) => format!("NumberLong(\"{n}\")"),
        Value::Float(n) if n.is_finite() => format!("{n:?}"),
        Value::Float(n) if n.is_nan() => "NaN".to_owned(),
        Value::Float(n) => if *n > 0.0 { "Infinity" } else { "-Infinity" }.to_owned(),
        Value::Decimal(d) => format!("NumberDecimal(\"{d}\")"),
        Value::Text(text) => quoted(text),
        Value::Json(text) => text.to_string(),
        Value::Bytes(bytes) => format!(
            "BinData(0, \"{}\")",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
        Value::Uuid(uuid) => format!("UUID(\"{uuid}\")"),
        Value::Timestamp(t) => format!("ISODate(\"{}\")", t.and_utc().to_rfc3339()),
        Value::TimestampTz(t) => format!("ISODate(\"{}\")", t.to_rfc3339()),
        Value::Date(d) => format!("ISODate(\"{d}T00:00:00Z\")"),
        Value::Unknown { type_name, raw } if &**type_name == "objectId" => {
            format!("ObjectId({})", quoted(&String::from_utf8_lossy(raw)))
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(literal).collect::<Vec<_>>().join(", ")
        ),
        other => quoted(&other.to_string()),
    }
}

/// `{"a": 1, "b": "x"}` from columns and values.
fn document(columns: &[&str], values: &[Value]) -> String {
    let fields: Vec<String> = columns
        .iter()
        .zip(values)
        .map(|(column, value)| format!("{}: {}", quoted(column), literal(value)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

impl Dialect for MongoDialect {
    fn kind(&self) -> DbKind {
        DbKind::Mongodb
    }

    fn placeholder(&self, _: usize) -> String {
        "?".to_owned()
    }

    fn quote_ident(&self, ident: &str) -> String {
        quoted(ident)
    }

    fn quote_table(&self, table: &TableRef) -> String {
        match &table.schema {
            Some(db) => format!(
                "db.getSiblingDB({}).getCollection({})",
                quoted(db),
                quoted(&table.name)
            ),
            None => format!("db.getCollection({})", quoted(&table.name)),
        }
    }

    fn browse(
        &self,
        table: &TableRef,
        columns: &[String],
        filter: &str,
        sort: Option<(&str, bool)>,
    ) -> String {
        let filter = filter.trim();
        let filter = if filter.is_empty() {
            "{}".to_owned()
        } else if filter.starts_with('{') {
            filter.to_owned()
        } else {
            format!("{{{filter}}}")
        };
        // The projection names the columns, in order, so the rows line up
        // with them whatever each document holds.
        let projection: Vec<String> = columns
            .iter()
            .map(|c| format!("{}: 1", quoted(c)))
            .collect();
        let mut call = format!(
            "{}.find({filter}, {{{}}})",
            self.quote_table(table),
            projection.join(", ")
        );
        if let Some((column, ascending)) = sort {
            call.push_str(&format!(
                ".sort({{{}: {}}})",
                quoted(column),
                if ascending { 1 } else { -1 }
            ));
        }
        call
    }

    fn filter_hint(&self) -> &'static str {
        "Filter documents with a query, such as {status: \"open\"}"
    }

    fn insert(&self, table: &TableRef, columns: &[&str], values: &[Value]) -> EditStatement {
        let (columns, values): (Vec<&str>, Vec<Value>) = columns
            .iter()
            .zip(values)
            .filter(|(_, value)| !value.is_null())
            .map(|(column, value)| (*column, value.clone()))
            .unzip();
        EditStatement {
            sql: format!(
                "{}.insertOne({})",
                self.quote_table(table),
                document(&columns, &values)
            ),
            params: Vec::new(),
        }
    }

    fn update(
        &self,
        table: &TableRef,
        key_columns: &[&str],
        key: &[Value],
        columns: &[&str],
        values: &[Value],
    ) -> EditStatement {
        EditStatement {
            sql: format!(
                "{}.updateOne({}, {{\"$set\": {}}})",
                self.quote_table(table),
                document(key_columns, key),
                document(columns, values)
            ),
            params: Vec::new(),
        }
    }

    fn delete(&self, table: &TableRef, key_columns: &[&str], key: &[Value]) -> EditStatement {
        EditStatement {
            sql: format!(
                "{}.deleteOne({})",
                self.quote_table(table),
                document(key_columns, key)
            ),
            params: Vec::new(),
        }
    }
}

/// Opens MongoDB sessions.
#[derive(Debug, Clone, Copy)]
pub struct MongoDriver;

#[async_trait]
impl Driver for MongoDriver {
    fn kind(&self) -> DbKind {
        DbKind::Mongodb
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Mongodb(cfg) = cfg else {
            return Err(Error::Config("not a MongoDB connection".into()));
        };
        let uri = uri(cfg, secret.as_ref().map(Secret::expose));
        let cfg = cfg.clone();
        let session = bridge(async move {
            let mut options = ClientOptions::parse(&uri)
                .await
                .map_err(|error| Error::Connect(connect_problem(&error, &cfg)))?;
            options.app_name = Some("Sonde".into());
            options.connect_timeout = Some(CONNECT_TIMEOUT);
            options.server_selection_timeout = Some(CONNECT_TIMEOUT);
            let client = Client::with_options(options)
                .map_err(|error| Error::Connect(connect_problem(&error, &cfg)))?;
            // The client connects lazily; a ping proves the server and the
            // login.
            client
                .database("admin")
                .run_command(doc! {"ping": 1})
                .await
                .map_err(|error| Error::Connect(connect_problem(&error, &cfg)))?;
            let database = match cfg.database.trim() {
                "" => "test".to_owned(),
                name => name.to_owned(),
            };
            Ok(MongoSession {
                client,
                current: Arc::new(Mutex::new(database)),
                cancel: CancelSignal::new(),
            })
        })
        .await?;
        Ok(Box::new(session))
    }
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The connection string for `cfg`.
fn uri(cfg: &ServerConfig, password: Option<&str>) -> String {
    let srv = cfg.option("srv").is_some_and(|v| v == "true");
    let mut uri = String::from(if srv { "mongodb+srv://" } else { "mongodb://" });
    let user = cfg.user.trim();
    if !user.is_empty() {
        uri.push_str(&encode(user));
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            uri.push(':');
            uri.push_str(&encode(password));
        }
        uri.push('@');
    }
    uri.push_str(&cfg.host);
    if !srv {
        uri.push_str(&format!(":{}", cfg.port));
    }
    uri.push('/');
    uri.push_str(&encode(cfg.database.trim()));
    let mut query: Vec<String> = cfg
        .option("options")
        .map(|options| {
            options
                .trim_start_matches('?')
                .split('&')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let has = |query: &[String], key: &str| {
        query.iter().any(|part| {
            part.split('=')
                .next()
                .is_some_and(|k| k.eq_ignore_ascii_case(key))
        })
    };
    if !has(&query, "tls") && !has(&query, "ssl") {
        match cfg.tls {
            TlsMode::Disable => query.push("tls=false".into()),
            // MongoDB cannot try TLS and fall back, so "prefer" means plain
            // text, except for SRV records, which imply TLS.
            TlsMode::Prefer if !srv => query.push("tls=false".into()),
            TlsMode::Prefer | TlsMode::Require => {
                query.push("tls=true".into());
                query.push("tlsAllowInvalidCertificates=true".into());
            }
            TlsMode::VerifyFull => query.push("tls=true".into()),
        }
    }
    if !query.is_empty() {
        uri.push('?');
        uri.push_str(&query.join("&"));
    }
    uri
}

fn connect_problem(error: &mongodb::error::Error, cfg: &ServerConfig) -> String {
    use mongodb::error::ErrorKind;
    let at = format!("{}:{}", cfg.host, cfg.port);
    match &*error.kind {
        ErrorKind::Authentication { .. } => format!(
            "the server at {at} rejected the user name or password \
             (users usually live in the admin database: add authSource=admin)"
        ),
        ErrorKind::Command(command) if command.code == 18 => {
            format!("the server at {at} rejected the user name or password")
        }
        ErrorKind::Command(command) if command.code == 13 => {
            format!("the server at {at} needs a user name and password")
        }
        ErrorKind::ServerSelection { message, .. } if message.contains("refused") => {
            format!("nothing is accepting connections at {at}; is the server running?")
        }
        ErrorKind::ServerSelection { message, .. } => format!("could not reach {at}: {message}"),
        ErrorKind::InvalidArgument { message, .. } => {
            format!("the connection settings are not valid: {message}")
        }
        ErrorKind::DnsResolve { message, .. } => {
            format!("could not look up {}: {message}", cfg.host)
        }
        _ => format!("could not reach {at}: {error}"),
    }
}

/// A server or driver failure as a query error.
fn query_error(error: mongodb::error::Error) -> Error {
    use mongodb::error::{ErrorKind, WriteFailure};
    let (message, code) = match &*error.kind {
        ErrorKind::Command(command) => (
            format!("{} ({})", command.message, command.code_name),
            Some(command.code_name.clone()),
        ),
        ErrorKind::Write(WriteFailure::WriteError(write)) => {
            (write.message.clone(), Some(write.code.to_string()))
        }
        ErrorKind::Io(_) | ErrorKind::ConnectionPoolCleared { .. } | ErrorKind::Shutdown => {
            return Error::Closed;
        }
        ErrorKind::ServerSelection { .. } => return Error::Closed,
        _ => (error.to_string(), None),
    };
    Error::Query {
        message,
        position: None,
        code,
        detail: None,
        hint: None,
    }
}

/// The BSON type names the grid shows. [`crate::value::parse`] reads cells
/// typed under them back as the same types.
fn type_name(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(b) if b.subtype == bson::spec::BinarySubtype::Uuid => "uuid",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "datetime",
        Bson::Null | Bson::Undefined => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "bsonTimestamp",
        Bson::Decimal128(_) => "decimal",
        _ => "other",
    }
}

fn cell(value: Bson) -> Value {
    match value {
        Bson::Double(n) => Value::Float(n),
        Bson::String(s) => Value::text(s),
        Bson::Boolean(b) => Value::Bool(b),
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Int32(n) => Value::Int(n.into()),
        Bson::Int64(n) => Value::Int(n),
        Bson::ObjectId(id) => Value::Unknown {
            type_name: Arc::from("objectId"),
            raw: Arc::from(id.to_hex().as_bytes()),
        },
        Bson::DateTime(t) => chrono::DateTime::from_timestamp_millis(t.timestamp_millis())
            .map_or_else(
                || Value::text(t.to_string()),
                |t| Value::Timestamp(t.naive_utc()),
            ),
        Bson::Decimal128(d) => d
            .to_string()
            .parse()
            .map(Value::Decimal)
            .unwrap_or_else(|_| Value::text(d.to_string())),
        Bson::Binary(b) if b.subtype == bson::spec::BinarySubtype::Uuid => b
            .to_uuid()
            .map(|uuid| Value::Uuid(uuid::Uuid::from_bytes(uuid.bytes())))
            .unwrap_or_else(|_| Value::Bytes(Arc::from(b.bytes.as_slice()))),
        Bson::Binary(b) if b.subtype == bson::spec::BinarySubtype::Generic => {
            Value::Bytes(Arc::from(b.bytes.as_slice()))
        }
        Bson::RegularExpression(r) => Value::text(format!("/{}/{}", r.pattern, r.options)),
        other @ (Bson::Document(_) | Bson::Array(_)) => {
            Value::Json(Arc::from(other.into_relaxed_extjson().to_string()))
        }
        other => Value::text(other.into_relaxed_extjson().to_string()),
    }
}

/// The columns of a page of documents.
fn columns_of(docs: &[Document]) -> Columns {
    let mut columns = Columns::default();
    if docs.iter().any(|doc| doc.contains_key("_id")) {
        columns.add("_id", "objectId");
    }
    for doc in docs {
        for (key, value) in doc {
            columns.add(key, type_name(value));
        }
    }
    columns
}

fn row(doc: Document, columns: &Columns) -> Vec<Value> {
    let mut row = vec![Value::Null; columns.len()];
    for (key, value) in doc {
        if let Some(ix) = columns.position(&key) {
            row[ix] = cell(value);
        }
    }
    row
}

/// Stream a cursor's documents. The columns are the fields of the first
/// page, or `fixed` when the call named them (a projection that lists only
/// fields to include).
async fn documents<S>(
    ready: Ready,
    mut docs: S,
    fixed: Option<Vec<String>>,
    pressed: &mut (impl Future<Output = ()> + Unpin),
) where
    S: futures::Stream<Item = mongodb::error::Result<Document>> + Unpin,
{
    let mut first = Vec::new();
    let mut ended = false;
    while first.len() < PAGE_ROWS {
        let next = tokio::select! {
            biased;
            () = &mut *pressed => return ready.fail(Error::Cancelled),
            next = docs.next() => next,
        };
        match next {
            Some(Ok(doc)) => first.push(doc),
            Some(Err(error)) => return ready.fail(query_error(error)),
            None => {
                ended = true;
                break;
            }
        }
    }
    let mut columns = Columns::default();
    if let Some(fixed) = fixed {
        let found = columns_of(&first);
        for name in fixed {
            let ty = found
                .position(&name)
                .map_or("null", |ix| found.types()[ix].as_str())
                .to_owned();
            columns.add(&name, &ty);
        }
    } else {
        columns = columns_of(&first);
    }
    let mut feed = ready.columns(columns.meta());
    for doc in first {
        if !feed.push(row(doc, &columns)).await {
            return;
        }
    }
    if !ended {
        loop {
            let next = tokio::select! {
                biased;
                () = &mut *pressed => return feed.fail(Error::Cancelled).await,
                next = docs.next() => next,
            };
            match next {
                Some(Ok(doc)) => {
                    if !feed.push(row(doc, &columns)).await {
                        return;
                    }
                }
                Some(Err(error)) => return feed.fail(query_error(error)).await,
                None => break,
            }
        }
    }
    feed.finish(None, Vec::new()).await;
}

/// One row of named values.
async fn single(ready: Ready, fields: Vec<(&str, Bson)>, affected: Option<u64>) {
    let columns: Vec<_> = fields
        .iter()
        .map(|(name, v)| meta(name, type_name(v)))
        .collect();
    let mut feed = ready.columns(columns);
    feed.push(fields.into_iter().map(|(_, value)| cell(value)).collect())
        .await;
    feed.finish(affected, Vec::new()).await;
}

/// A list of values in one column.
async fn list(ready: Ready, name: &str, values: Vec<Bson>) {
    let ty = values.first().map_or("string", type_name);
    let mut feed = ready.columns(vec![meta(name, ty)]);
    for value in values {
        if !feed.push(vec![cell(value)]).await {
            return;
        }
    }
    feed.finish(None, Vec::new()).await;
}

/// A command's reply: its cursor's batch as rows when it has one, else the
/// reply as one row.
async fn reply(ready: Ready, mut reply: Document) {
    if let Ok(cursor) = reply.get_document_mut("cursor")
        && let Some(Bson::Array(batch)) = cursor.remove("firstBatch")
    {
        let docs: Vec<Document> = batch
            .into_iter()
            .filter_map(|item| match item {
                Bson::Document(doc) => Some(doc),
                _ => None,
            })
            .collect();
        let columns = columns_of(&docs);
        let mut feed: Feed = ready.columns(columns.meta());
        for doc in docs {
            if !feed.push(row(doc, &columns)).await {
                return;
            }
        }
        return feed.finish(None, Vec::new()).await;
    }
    let columns = columns_of(std::slice::from_ref(&reply));
    let mut feed = ready.columns(columns.meta());
    feed.push(row(reply, &columns)).await;
    feed.finish(None, Vec::new()).await;
}

fn arg_doc(args: &[Bson], ix: usize, what: &str) -> Result<Document> {
    match args.get(ix) {
        None => Ok(Document::new()),
        Some(Bson::Document(doc)) => Ok(doc.clone()),
        Some(_) => Err(Error::query(format!("{what} must be a document"))),
    }
}

fn arg_docs(args: &[Bson], ix: usize, what: &str) -> Result<Vec<Document>> {
    match args.get(ix) {
        Some(Bson::Array(items)) => items
            .iter()
            .map(|item| match item {
                Bson::Document(doc) => Ok(doc.clone()),
                _ => Err(Error::query(format!("{what} must be a list of documents"))),
            })
            .collect(),
        _ => Err(Error::query(format!("{what} must be a list of documents"))),
    }
}

fn arg_i64(args: &[Bson], what: &str) -> Result<i64> {
    match args.first() {
        Some(Bson::Int32(n)) => Ok((*n).into()),
        Some(Bson::Int64(n)) => Ok(*n),
        Some(Bson::Double(n)) if n.fract() == 0.0 => Ok(*n as i64),
        _ => Err(Error::query(format!("{what}(...) takes a whole number"))),
    }
}

/// The fields a projection lists to include, when it lists nothing else.
fn included(projection: &Document) -> Option<Vec<String>> {
    if projection.is_empty() {
        return None;
    }
    let mut names = Vec::new();
    for (key, value) in projection {
        let on = match value {
            Bson::Int32(n) => *n != 0,
            Bson::Int64(n) => *n != 0,
            Bson::Double(n) => *n != 0.0,
            Bson::Boolean(b) => *b,
            _ => return None,
        };
        if !on {
            return None;
        }
        names.push(key.clone());
    }
    Some(names)
}

struct MongoSession {
    client: Client,
    /// The database `use` last chose.
    current: Arc<Mutex<String>>,
    cancel: CancelSignal,
}

fn database(client: &Client, current: &str, db: &Db) -> Database {
    match db {
        Db::Current => client.database(current),
        Db::Named(name) => client.database(name),
    }
}

async fn run(
    client: Client,
    current: Arc<Mutex<String>>,
    call: Call,
    ready: Ready,
    mut pressed: impl Future<Output = ()> + Unpin + Send,
) {
    let mut current_name = current.lock().await;
    macro_rules! wait {
        ($work:expr) => {
            tokio::select! {
                biased;
                () = &mut pressed => return ready.fail(Error::Cancelled),
                outcome = $work => match outcome {
                    Ok(value) => value,
                    Err(error) => return ready.fail(query_error(error)),
                },
            }
        };
    }
    match call {
        Call::Use(name) => {
            *current_name = name.clone();
            single(ready, vec![("database", Bson::String(name))], None).await;
        }
        Call::ShowDbs => {
            let specs = wait!(client.list_databases());
            let mut feed = ready.columns(vec![
                meta("name", "string"),
                meta("sizeOnDisk", "long"),
                meta("empty", "bool"),
            ]);
            for spec in specs {
                feed.push(vec![
                    Value::text(spec.name),
                    Value::Int(spec.size_on_disk as i64),
                    Value::Bool(spec.empty),
                ])
                .await;
            }
            feed.finish(None, Vec::new()).await;
        }
        Call::ShowCollections(db) => {
            let db = database(&client, &current_name, &db);
            let mut names = wait!(db.list_collection_names());
            names.sort();
            list(ready, "name", names.into_iter().map(Bson::String).collect()).await;
        }
        Call::Database { db, method, args } => {
            let db = database(&client, &current_name, &db);
            match method.as_str() {
                "runCommand" | "adminCommand" => {
                    let command = match arg_doc(&args, 0, "the command") {
                        Ok(command) => command,
                        Err(error) => return ready.fail(error),
                    };
                    let target = if method == "adminCommand" {
                        client.database("admin")
                    } else {
                        db
                    };
                    let answer = wait!(target.run_command(command));
                    reply(ready, answer).await;
                }
                "stats" => {
                    let answer = wait!(db.run_command(doc! {"dbStats": 1}));
                    reply(ready, answer).await;
                }
                "createCollection" => match args.first() {
                    Some(Bson::String(name)) => {
                        wait!(db.create_collection(name));
                        ready.done(None, Vec::new()).await;
                    }
                    _ => ready.fail(Error::query("createCollection takes a name")),
                },
                "dropDatabase" => {
                    wait!(db.drop());
                    ready.done(None, Vec::new()).await;
                }
                other => ready.fail(Error::query(format!("db.{other}() is not supported"))),
            }
        }
        Call::Collection {
            db,
            collection,
            method,
            args,
            chain,
        } => {
            let db = database(&client, &current_name, &db);
            drop(current_name);
            let coll: Collection<Document> = db.collection(&collection);
            let affected = |n: u64| Some(n);
            match method.as_str() {
                "find" | "findOne" => {
                    let filter = match arg_doc(&args, 0, "the filter") {
                        Ok(filter) => filter,
                        Err(error) => return ready.fail(error),
                    };
                    let mut options = FindOptions::default();
                    let mut fixed = None;
                    if let Ok(projection) = arg_doc(&args, 1, "the projection")
                        && !projection.is_empty()
                    {
                        fixed = included(&projection);
                        options.projection = Some(projection);
                    }
                    if method == "findOne" {
                        options.limit = Some(1);
                    }
                    for (name, args) in chain {
                        let outcome = match name.as_str() {
                            "sort" => arg_doc(&args, 0, "sort").map(|doc| options.sort = Some(doc)),
                            "projection" => arg_doc(&args, 0, "projection").map(|doc| {
                                fixed = included(&doc);
                                options.projection = Some(doc);
                            }),
                            "limit" => arg_i64(&args, "limit").map(|n| options.limit = Some(n)),
                            "skip" => arg_i64(&args, "skip").map(|n| options.skip = Some(n as u64)),
                            "batchSize" => arg_i64(&args, "batchSize")
                                .map(|n| options.batch_size = Some(n as u32)),
                            "hint" => arg_doc(&args, 0, "hint")
                                .map(|doc| options.hint = Some(mongodb::options::Hint::Keys(doc))),
                            "pretty" | "toArray" => Ok(()),
                            other => Err(Error::query(format!(
                                ".{other}() is not supported after find"
                            ))),
                        };
                        if let Err(error) = outcome {
                            return ready.fail(error);
                        }
                    }
                    let cursor = wait!(coll.find(filter).with_options(options));
                    documents(ready, cursor, fixed, &mut pressed).await;
                }
                "aggregate" => {
                    let pipeline = match arg_docs(&args, 0, "the pipeline") {
                        Ok(pipeline) => pipeline,
                        Err(error) => return ready.fail(error),
                    };
                    let mut options = AggregateOptions::default();
                    options.allow_disk_use = Some(true);
                    let cursor = wait!(coll.aggregate(pipeline).with_options(options));
                    documents(ready, cursor, None, &mut pressed).await;
                }
                "countDocuments" | "count" => {
                    let filter = match arg_doc(&args, 0, "the filter") {
                        Ok(filter) => filter,
                        Err(error) => return ready.fail(error),
                    };
                    let n = wait!(coll.count_documents(filter));
                    single(ready, vec![("count", Bson::Int64(n as i64))], None).await;
                }
                "estimatedDocumentCount" => {
                    let n = wait!(coll.estimated_document_count());
                    single(ready, vec![("count", Bson::Int64(n as i64))], None).await;
                }
                "distinct" => {
                    let Some(Bson::String(field)) = args.first() else {
                        return ready.fail(Error::query("distinct takes a field name"));
                    };
                    let filter = match arg_doc(&args, 1, "the filter") {
                        Ok(filter) => filter,
                        Err(error) => return ready.fail(error),
                    };
                    let values = wait!(coll.distinct(field, filter));
                    list(ready, field, values).await;
                }
                "insertOne" => {
                    let doc = match arg_doc(&args, 0, "the document") {
                        Ok(doc) => doc,
                        Err(error) => return ready.fail(error),
                    };
                    let result = wait!(coll.insert_one(doc));
                    single(ready, vec![("insertedId", result.inserted_id)], affected(1)).await;
                }
                "insertMany" => {
                    let docs = match arg_docs(&args, 0, "the documents") {
                        Ok(docs) => docs,
                        Err(error) => return ready.fail(error),
                    };
                    let result = wait!(coll.insert_many(docs));
                    let mut ids: Vec<(usize, Bson)> = result.inserted_ids.into_iter().collect();
                    ids.sort_by_key(|(ix, _)| *ix);
                    let n = ids.len() as u64;
                    let mut feed = ready.columns(vec![meta("insertedId", "objectId")]);
                    for (_, id) in ids {
                        feed.push(vec![cell(id)]).await;
                    }
                    feed.finish(affected(n), Vec::new()).await;
                }
                "updateOne" | "updateMany" | "replaceOne" => {
                    let filter = match arg_doc(&args, 0, "the filter") {
                        Ok(filter) => filter,
                        Err(error) => return ready.fail(error),
                    };
                    let upsert = arg_doc(&args, 2, "the options")
                        .ok()
                        .and_then(|options| options.get_bool("upsert").ok());
                    let result = if method == "replaceOne" {
                        let doc = match arg_doc(&args, 1, "the replacement") {
                            Ok(doc) => doc,
                            Err(error) => return ready.fail(error),
                        };
                        let mut action = coll.replace_one(filter, doc);
                        if let Some(upsert) = upsert {
                            action = action.upsert(upsert);
                        }
                        wait!(action)
                    } else {
                        let update: mongodb::options::UpdateModifications = match args.get(1) {
                            Some(Bson::Document(doc)) => doc.clone().into(),
                            Some(Bson::Array(_)) => match arg_docs(&args, 1, "the update") {
                                Ok(pipeline) => pipeline.into(),
                                Err(error) => return ready.fail(error),
                            },
                            _ => return ready.fail(Error::query("the update must be a document")),
                        };
                        let mut action = if method == "updateOne" {
                            coll.update_one(filter, update)
                        } else {
                            coll.update_many(filter, update)
                        };
                        if let Some(upsert) = upsert {
                            action = action.upsert(upsert);
                        }
                        wait!(action)
                    };
                    single(
                        ready,
                        vec![
                            ("matchedCount", Bson::Int64(result.matched_count as i64)),
                            ("modifiedCount", Bson::Int64(result.modified_count as i64)),
                            ("upsertedId", result.upserted_id.unwrap_or(Bson::Null)),
                        ],
                        affected(result.modified_count),
                    )
                    .await;
                }
                "deleteOne" | "deleteMany" => {
                    let filter = match arg_doc(&args, 0, "the filter") {
                        Ok(filter) => filter,
                        Err(error) => return ready.fail(error),
                    };
                    let result = if method == "deleteOne" {
                        wait!(coll.delete_one(filter))
                    } else {
                        wait!(coll.delete_many(filter))
                    };
                    single(
                        ready,
                        vec![("deletedCount", Bson::Int64(result.deleted_count as i64))],
                        affected(result.deleted_count),
                    )
                    .await;
                }
                "getIndexes" => {
                    let cursor = wait!(coll.list_indexes());
                    let docs = cursor.map_ok(|index| {
                        let mut doc = doc! {"key": index.keys};
                        if let Some(options) = index.options {
                            if let Some(name) = options.name {
                                doc.insert("name", name);
                            }
                            if let Some(unique) = options.unique {
                                doc.insert("unique", unique);
                            }
                        }
                        doc
                    });
                    documents(ready, Box::pin(docs), None, &mut pressed).await;
                }
                "createIndex" => {
                    let keys = match arg_doc(&args, 0, "the keys") {
                        Ok(keys) => keys,
                        Err(error) => return ready.fail(error),
                    };
                    let options = arg_doc(&args, 1, "the options")
                        .ok()
                        .and_then(|doc| bson::from_document::<IndexOptions>(doc).ok());
                    let model = IndexModel::builder().keys(keys).options(options).build();
                    let result = wait!(coll.create_index(model));
                    single(ready, vec![("name", Bson::String(result.index_name))], None).await;
                }
                "dropIndex" => match args.first() {
                    Some(Bson::String(name)) => {
                        wait!(coll.drop_index(name));
                        ready.done(None, Vec::new()).await;
                    }
                    _ => ready.fail(Error::query("dropIndex takes an index name")),
                },
                "drop" => {
                    wait!(coll.drop());
                    ready.done(None, Vec::new()).await;
                }
                other => ready.fail(Error::query(format!(
                    "{other}() is not supported; try find, aggregate, countDocuments, \
                     distinct, insertOne, updateOne, deleteOne or db.runCommand"
                ))),
            }
        }
    }
}

/// One change of a change set, parsed back from its statement.
enum Change {
    Insert(Collection<Document>, Document),
    Update(Collection<Document>, Document, Document),
    Delete(Collection<Document>, Document),
}

fn change(client: &Client, current: &str, statement: &EditStatement) -> Result<Change> {
    let Call::Collection {
        db,
        collection,
        method,
        args,
        ..
    } = shell::parse(&statement.sql)?
    else {
        return Err(Error::query(
            "a change must be insertOne, updateOne or deleteOne",
        ));
    };
    let coll = database(client, current, &db).collection(&collection);
    match method.as_str() {
        "insertOne" => Ok(Change::Insert(coll, arg_doc(&args, 0, "the document")?)),
        "updateOne" => Ok(Change::Update(
            coll,
            arg_doc(&args, 0, "the filter")?,
            arg_doc(&args, 1, "the update")?,
        )),
        "deleteOne" => Ok(Change::Delete(coll, arg_doc(&args, 0, "the filter")?)),
        _ => Err(Error::query(
            "a change must be insertOne, updateOne or deleteOne",
        )),
    }
}

/// Whether the server refused a transaction because it is a standalone.
fn no_transactions(error: &mongodb::error::Error) -> bool {
    use mongodb::error::ErrorKind;
    match &*error.kind {
        ErrorKind::Command(command) => command.code == 20 || command.code == 263,
        ErrorKind::SessionsNotSupported | ErrorKind::Transaction { .. } => true,
        _ => error
            .to_string()
            .contains("Transaction numbers are only allowed"),
    }
}

async fn apply_changes(
    changes: &[Change],
    mut session: Option<&mut mongodb::ClientSession>,
) -> std::result::Result<bool, mongodb::error::Error> {
    for change in changes {
        let matched = match change {
            Change::Insert(coll, doc) => {
                let action = coll.insert_one(doc.clone());
                match session.as_deref_mut() {
                    Some(s) => action.session(s).await?,
                    None => action.await?,
                };
                true
            }
            Change::Update(coll, filter, update) => {
                let action = coll.update_one(filter.clone(), update.clone());
                let result = match session.as_deref_mut() {
                    Some(s) => action.session(s).await?,
                    None => action.await?,
                };
                result.matched_count == 1
            }
            Change::Delete(coll, filter) => {
                let action = coll.delete_one(filter.clone());
                let result = match session.as_deref_mut() {
                    Some(s) => action.session(s).await?,
                    None => action.await?,
                };
                result.deleted_count == 1
            }
        };
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

#[async_trait]
impl Session for MongoSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        if !params.is_empty() {
            return Err(Error::query("MongoDB calls take no parameters"));
        }
        let call = shell::parse(sql)?;
        let client = self.client.clone();
        let current = Arc::clone(&self.current);
        let cancel = self.cancel.clone();
        stream::start(move |ready| async move {
            let pressed = Box::pin(cancel.pressed());
            run(client, current, call, ready, pressed).await;
        })
        .await
    }

    fn canceller(&self) -> Canceller {
        self.cancel.canceller()
    }

    async fn catalog(&self) -> Result<Catalog> {
        let client = self.client.clone();
        let current = Arc::clone(&self.current);
        bridge(async move {
            let current = current.lock().await.clone();
            let mut names = client.list_database_names().await.map_err(query_error)?;
            names.retain(|name| !SYSTEM.contains(&name.as_str()) || *name == current);
            if !names.contains(&current) {
                names.push(current.clone());
            }
            names.sort_by_key(|name| (*name != current, name.clone()));
            let mut schemas = Vec::new();
            for name in names {
                let mut specs: Vec<_> = client
                    .database(&name)
                    .list_collections()
                    .await
                    .map_err(query_error)?
                    .try_collect()
                    .await
                    .map_err(query_error)?;
                specs.sort_by(|a, b| a.name.cmp(&b.name));
                schemas.push(Schema {
                    name,
                    relations: specs
                        .into_iter()
                        .filter(|spec| !spec.name.starts_with("system."))
                        .map(|spec| Relation {
                            kind: if spec.collection_type == CollectionType::View {
                                RelationKind::View
                            } else {
                                RelationKind::Collection
                            },
                            name: spec.name,
                        })
                        .collect(),
                });
            }
            Ok(Catalog { schemas })
        })
        .await
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let client = self.client.clone();
        let current = Arc::clone(&self.current);
        let table = table.clone();
        bridge(async move {
            let current = current.lock().await.clone();
            let db = client.database(table.schema.as_deref().unwrap_or(&current));
            let exists = db
                .list_collection_names()
                .await
                .map_err(query_error)?
                .contains(&table.name);
            if !exists {
                return Err(Error::query(format!(
                    "collection {:?} does not exist",
                    table.name
                )));
            }
            let coll: Collection<Document> = db.collection(&table.name);
            let docs: Vec<Document> = coll
                .find(Document::new())
                .limit(SAMPLE)
                .await
                .map_err(query_error)?
                .try_collect()
                .await
                .map_err(query_error)?;
            let columns = columns_of(&docs);
            let mut indexes = Vec::new();
            // Views have no indexes, and refuse to list them.
            if let Ok(cursor) = coll.list_indexes().await {
                let models: Vec<IndexModel> = cursor.try_collect().await.unwrap_or_default();
                for model in models {
                    let options = model.options.unwrap_or_default();
                    let name = options.name.unwrap_or_default();
                    indexes.push(IndexInfo {
                        primary: name == "_id_",
                        unique: options.unique.unwrap_or(name == "_id_"),
                        name,
                        columns: model.keys.keys().map(|k| Some(k.clone())).collect(),
                    });
                }
            }
            let primary =
                indexes.iter().any(|index| index.primary) && columns.position("_id").is_some();
            Ok(TableDetail {
                columns: columns
                    .names()
                    .iter()
                    .zip(columns.types())
                    .map(|(name, type_name)| ColumnInfo {
                        name: name.clone(),
                        type_name: type_name.clone(),
                        nullable: name != "_id",
                        default: None,
                    })
                    .collect(),
                primary_key: if primary {
                    vec!["_id".into()]
                } else {
                    Vec::new()
                },
                foreign_keys: Vec::new(),
                indexes,
                has_rowid: false,
            })
        })
        .await
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<()> {
        let client = self.client.clone();
        let current = Arc::clone(&self.current);
        let statements = statements.to_vec();
        bridge(async move {
            let current = current.lock().await.clone();
            let changes = statements
                .iter()
                .map(|statement| change(&client, &current, statement))
                .collect::<Result<Vec<_>>>()?;
            let mut session = client.start_session().await.map_err(query_error)?;
            match session.start_transaction().await {
                Ok(()) => {}
                Err(error) if no_transactions(&error) => {
                    return apply_standalone(&changes).await;
                }
                Err(error) => return Err(query_error(error)),
            }
            match apply_changes(&changes, Some(&mut session)).await {
                Ok(true) => session.commit_transaction().await.map_err(query_error),
                Ok(false) => {
                    let _ = session.abort_transaction().await;
                    Err(Error::Stale)
                }
                Err(error) if no_transactions(&error) => {
                    let _ = session.abort_transaction().await;
                    apply_standalone(&changes).await
                }
                Err(error) => {
                    let _ = session.abort_transaction().await;
                    Err(query_error(error))
                }
            }
        })
        .await
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &MongoDialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        let client = self.client.clone();
        bridge(async move {
            client.shutdown().await;
            Ok(())
        })
        .await
    }
}

/// A standalone server has no transactions: check every keyed document is
/// still there first, then write. A row that changes between the check and
/// the write can still slip through.
async fn apply_standalone(changes: &[Change]) -> Result<()> {
    for change in changes {
        let (coll, filter) = match change {
            Change::Update(coll, filter, _) | Change::Delete(coll, filter) => (coll, filter),
            Change::Insert(..) => continue,
        };
        let found = coll
            .count_documents(filter.clone())
            .await
            .map_err(query_error)?;
        if found != 1 {
            return Err(Error::Stale);
        }
    }
    match apply_changes(changes, None).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::Stale),
        Err(error) => Err(query_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_should_carry_the_login_and_options() {
        let mut cfg = ServerConfig {
            host: "db.example".into(),
            port: 27017,
            database: "shop".into(),
            user: "a@b".into(),
            tls: TlsMode::Prefer,
            options: Default::default(),
        };
        cfg.options
            .insert("options".into(), "authSource=admin".into());

        assert_eq!(
            uri(&cfg, Some("p:ss")),
            "mongodb://a%40b:p%3Ass@db.example:27017/shop?authSource=admin&tls=false"
        );
        cfg.options.insert("srv".into(), "true".into());
        assert_eq!(
            uri(&cfg, None),
            "mongodb+srv://a%40b@db.example/shop?authSource=admin&tls=true&tlsAllowInvalidCertificates=true"
        );
    }

    #[test]
    fn edits_should_be_shell_calls_keyed_on_the_id() {
        let table = TableRef::new("shop", "orders");
        let id = Value::Unknown {
            type_name: Arc::from("objectId"),
            raw: Arc::from(&b"65a1b2c3d4e5f60718293a4b"[..]),
        };

        let statement = MongoDialect.update(
            &table,
            &["_id"],
            &[id],
            &["n", "s"],
            &[Value::Int(5), Value::text("x\"y")],
        );

        assert_eq!(
            statement.sql,
            r#"db.getSiblingDB("shop").getCollection("orders").updateOne({"_id": ObjectId("65a1b2c3d4e5f60718293a4b")}, {"$set": {"n": 5, "s": "x\"y"}})"#
        );
        assert!(shell::parse(&statement.sql).is_ok());
        assert_eq!(statement.rendered(), statement.sql);
    }

    #[test]
    fn browse_should_project_the_columns_in_order() {
        let sql = MongoDialect.browse(
            &TableRef::new("shop", "orders"),
            &["_id".into(), "total".into()],
            "status: 'open'",
            Some(("total", false)),
        );

        assert_eq!(
            sql,
            r#"db.getSiblingDB("shop").getCollection("orders").find({status: 'open'}, {"_id": 1, "total": 1}).sort({"total": -1})"#
        );
        assert!(shell::parse(&sql).is_ok());
    }

    #[test]
    fn literal_should_round_trip_through_the_shell() {
        for value in [
            Value::Int(3_000_000_000),
            Value::Float(2.0),
            Value::Decimal("1.50".parse().unwrap()),
            Value::Timestamp(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                    .unwrap()
                    .and_hms_opt(3, 4, 5)
                    .unwrap(),
            ),
            Value::Json(Arc::from(r#"{"a":{"$oid":"65a1b2c3d4e5f60718293a4b"}}"#)),
        ] {
            let bson = shell::literal(&literal(&value)).unwrap();
            assert_eq!(cell(bson), value, "{value:?}");
        }
    }
}
