//! The SQLite driver.
//!
//! `rusqlite` is synchronous, so each session owns a thread that holds the
//! connection and takes commands over a channel. One thread per connection
//! also gives SQLite the single writer it wants, without locks.

mod introspect;

use std::{
    sync::{Arc, mpsc as std_mpsc},
    thread,
    time::Instant,
};

use async_trait::async_trait;
use futures::{
    SinkExt as _,
    channel::{mpsc, oneshot},
    executor::block_on,
};
use rusqlite::{
    Connection, ErrorCode, InterruptHandle, OpenFlags,
    types::{Value as SqlValue, ValueRef},
};

use crate::{
    driver::{
        CHANNEL_PAGES, Canceller, ConnectionConfig, DbKind, Dialect, Driver, PAGE_ROWS,
        QueryHandle, QuerySummary, Secret, Session, SqliteConfig,
    },
    edit::EditStatement,
    error::{Error, Result},
    schema::{Catalog, TableDetail, TableRef},
    value::{ColumnMeta, ColumnOrigin, ResultPage, Value},
};

/// SQLite's SQL spelling.
#[derive(Debug, Clone, Copy)]
pub struct SqliteDialect;

impl Dialect for SqliteDialect {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    fn placeholder(&self, n: usize) -> String {
        format!("?{n}")
    }
}

/// Opens SQLite sessions.
#[derive(Debug, Clone, Copy)]
pub struct SqliteDriver;

#[async_trait]
impl Driver for SqliteDriver {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    async fn connect(&self, cfg: &ConnectionConfig, _: Option<Secret>) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Sqlite(cfg) = cfg else {
            return Err(Error::Config("not a SQLite connection".into()));
        };
        let cfg = cfg.clone();
        let (commands, inbox) = std_mpsc::channel();
        let (opened, opened_rx) = oneshot::channel();

        thread::Builder::new()
            .name("sonde-sqlite".into())
            .spawn(move || {
                let connection = match open(&cfg) {
                    Ok(connection) => connection,
                    Err(error) => {
                        // The caller may have stopped waiting.
                        let _ = opened.send(Err(error));
                        return;
                    }
                };
                if opened
                    .send(Ok(Arc::new(connection.get_interrupt_handle())))
                    .is_ok()
                {
                    serve(&connection, &inbox);
                }
            })
            .map_err(|error| Error::Connect(format!("could not start a thread: {error}")))?;

        let interrupt = opened_rx.await.map_err(|_| Error::Closed)??;
        Ok(Box::new(SqliteSession {
            commands,
            interrupt,
        }))
    }
}

fn open(cfg: &SqliteConfig) -> Result<Connection> {
    let in_memory = cfg.path.as_os_str() == ":memory:";
    let flags = if cfg.read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else if in_memory {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        // No CREATE: a mistyped path should fail, not make an empty database.
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let connection =
        Connection::open_with_flags(&cfg.path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(|error| Error::Connect(error.to_string()))?;
    // Opening is lazy; this is what notices a file that is not a database.
    connection
        .query_row("select count(*) from sqlite_schema", [], |_| Ok(()))
        .map_err(|error| Error::Connect(error.to_string()))?;
    Ok(connection)
}

enum Command {
    Execute {
        sql: String,
        params: Vec<Value>,
        reply: oneshot::Sender<Result<QueryHandle>>,
    },
    Catalog {
        reply: oneshot::Sender<Result<Catalog>>,
    },
    TableDetail {
        table: TableRef,
        reply: oneshot::Sender<Result<TableDetail>>,
    },
    Apply {
        statements: Vec<EditStatement>,
        reply: oneshot::Sender<Result<()>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

/// The session thread's loop. Ends when the session is closed or dropped.
fn serve(connection: &Connection, inbox: &std_mpsc::Receiver<Command>) {
    // A reply nobody waits for is simply discarded throughout.
    while let Ok(command) = inbox.recv() {
        match command {
            Command::Execute { sql, params, reply } => execute(connection, &sql, &params, reply),
            Command::Catalog { reply } => {
                let _ = reply.send(introspect::catalog(connection).map_err(query_error));
            }
            Command::TableDetail { table, reply } => {
                let _ = reply.send(introspect::table_detail(connection, &table));
            }
            Command::Apply { statements, reply } => {
                let _ = reply.send(apply(connection, &statements));
            }
            Command::Close { reply } => {
                let _ = reply.send(());
                return;
            }
        }
    }
}

/// Run one statement, streaming its rows. Returns when the statement is
/// finished, failed, or abandoned by its consumer.
fn execute(
    connection: &Connection,
    sql: &str,
    params: &[Value],
    reply: oneshot::Sender<Result<QueryHandle>>,
) {
    let started = Instant::now();
    let prepared = connection.prepare(sql).and_then(|mut statement| {
        for (ix, value) in params.iter().enumerate() {
            statement.raw_bind_parameter(ix + 1, to_sql(value))?;
        }
        Ok(statement)
    });
    let mut statement = match prepared {
        Ok(statement) => statement,
        Err(error) => {
            let _ = reply.send(Err(query_error(error)));
            return;
        }
    };

    let declared: Vec<Option<String>> = statement
        .columns()
        .map(|column| column.decl_type().map(str::to_owned))
        .collect();
    let columns = statement
        .columns_with_metadata()
        .zip(declared)
        .map(|(column, declared)| ColumnMeta {
            name: column.name().into(),
            type_name: declared.unwrap_or_default().into(),
            origin: column
                .table_name()
                .zip(column.origin_name())
                .map(|(table, origin)| ColumnOrigin {
                    schema: column.database_name().map(Into::into),
                    table: table.into(),
                    column: origin.into(),
                }),
        })
        .collect::<Vec<_>>();
    let width = columns.len();
    let changes_rows = width == 0 && !statement.readonly();

    let (mut pages, pages_rx) = mpsc::channel(CHANNEL_PAGES);
    let (summary, summary_rx) = oneshot::channel();
    let handle = QueryHandle {
        columns,
        pages: pages_rx,
        summary: summary_rx,
    };
    if reply.send(Ok(handle)).is_err() {
        return;
    }

    let mut rows = statement.raw_query();
    let mut page = Vec::with_capacity(PAGE_ROWS);
    let mut rows_returned = 0_u64;
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let cells = (0..width).map(|ix| row.get_ref(ix).map(from_sql)).collect();
                match cells {
                    Ok(cells) => page.push(cells),
                    Err(error) => {
                        let _ = block_on(pages.send(Err(query_error(error))));
                        return;
                    }
                }
                rows_returned += 1;
                if page.len() == PAGE_ROWS {
                    let rows = std::mem::replace(&mut page, Vec::with_capacity(PAGE_ROWS));
                    // Blocks while the consumer is behind, which is the
                    // backpressure. An error means the consumer left.
                    if block_on(pages.send(Ok(ResultPage { rows }))).is_err() {
                        return;
                    }
                }
            }
            Ok(None) => break,
            Err(error) => {
                let _ = block_on(pages.send(Err(query_error(error))));
                return;
            }
        }
    }
    if !page.is_empty() && block_on(pages.send(Ok(ResultPage { rows: page }))).is_err() {
        return;
    }
    let _ = summary.send(QuerySummary {
        rows_affected: changes_rows.then(|| connection.changes()),
        rows_returned,
        elapsed: started.elapsed(),
        notices: Vec::new(),
    });
}

/// Run a change set in one transaction. Each statement must affect exactly
/// one row; when one does not, the transaction is rolled back and the set is
/// reported as stale. Explicit `BEGIN`/`COMMIT` keep this callable with a
/// shared reference, like the rest of the session thread's work.
fn apply(connection: &Connection, statements: &[EditStatement]) -> Result<()> {
    connection.execute_batch("BEGIN").map_err(query_error)?;
    let result: Result<()> = (|| {
        for statement in statements {
            let mut prepared = connection.prepare(&statement.sql).map_err(query_error)?;
            for (ix, value) in statement.params.iter().enumerate() {
                prepared
                    .raw_bind_parameter(ix + 1, to_sql(value))
                    .map_err(query_error)?;
            }
            let affected = prepared.raw_execute().map_err(query_error)?;
            if affected != 1 {
                return Err(Error::Stale);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = connection.execute_batch("ROLLBACK");
        return Err(error);
    }
    connection.execute_batch("COMMIT").map_err(query_error)?;
    Ok(())
}

fn from_sql(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::Int(value),
        ValueRef::Real(value) => Value::Float(value),
        // SQLite does not promise valid UTF-8 in a text cell.
        ValueRef::Text(text) => Value::text(String::from_utf8_lossy(text)),
        ValueRef::Blob(bytes) => Value::Bytes(bytes.into()),
    }
}

/// SQLite has five storage classes; everything richer goes in as its text.
fn to_sql(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(value) => SqlValue::Integer((*value).into()),
        Value::Int(value) => SqlValue::Integer(*value),
        Value::Float(value) => SqlValue::Real(*value),
        Value::Bytes(bytes) => SqlValue::Blob(bytes.to_vec()),
        other => SqlValue::Text(other.to_string()),
    }
}

pub(super) fn query_error(error: rusqlite::Error) -> Error {
    match error {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::OperationInterrupted =>
        {
            Error::Cancelled
        }
        rusqlite::Error::SqlInputError {
            error, msg, offset, ..
        } => Error::Query {
            message: msg,
            position: usize::try_from(offset).ok(),
            code: Some(format!("{:?}", error.code)),
            detail: None,
            hint: None,
        },
        rusqlite::Error::SqliteFailure(failure, message) => Error::Query {
            message: message.unwrap_or_else(|| failure.to_string()),
            position: None,
            code: Some(format!("{:?}", failure.code)),
            detail: None,
            hint: None,
        },
        other => Error::query(other.to_string()),
    }
}

struct SqliteSession {
    commands: std_mpsc::Sender<Command>,
    interrupt: Arc<InterruptHandle>,
}

impl SqliteSession {
    /// Send a command built around a reply channel and wait for the answer.
    async fn ask<T>(&self, command: impl FnOnce(oneshot::Sender<T>) -> Command) -> Result<T> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| Error::Closed)?;
        answer.await.map_err(|_| Error::Closed)
    }
}

#[async_trait]
impl Session for SqliteSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        let (sql, params) = (sql.to_owned(), params.to_vec());
        self.ask(|reply| Command::Execute { sql, params, reply })
            .await?
    }

    fn canceller(&self) -> Canceller {
        let interrupt = Arc::clone(&self.interrupt);
        Canceller::new(move || interrupt.interrupt())
    }

    async fn catalog(&self) -> Result<Catalog> {
        self.ask(|reply| Command::Catalog { reply }).await?
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let table = table.clone();
        self.ask(|reply| Command::TableDetail { table, reply })
            .await?
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<()> {
        self.ask(|reply| Command::Apply {
            statements: statements.to_vec(),
            reply,
        })
        .await?
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &SqliteDialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        self.ask(|reply| Command::Close { reply }).await
    }
}
