//! The Postgres driver.

mod decode;
mod introspect;
pub(crate) mod tls;

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{
    SinkExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    pin_mut,
};
use tokio_postgres::{
    AsyncMessage, CancelToken, Client, Config, Row, config::SslMode, error::ErrorPosition,
    types::ToSql,
};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::{
    driver::{
        CHANNEL_PAGES, Canceller, ConnectionConfig, DbKind, Dialect, Driver, PAGE_ROWS,
        QueryHandle, QuerySummary, Secret, Session, TlsMode,
    },
    edit::EditStatement,
    error::{Error, Result},
    runtime,
    schema::{Catalog, TableDetail, TableRef},
    value::{ColumnMeta, ResultPage, Value},
};

use self::decode::{Cell, Param};

/// SQLSTATE `query_canceled`.
const QUERY_CANCELED: &str = "57014";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Postgres's SQL spelling.
#[derive(Debug, Clone, Copy)]
pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }
}

/// Opens Postgres sessions.
#[derive(Debug, Clone, Copy)]
pub struct PostgresDriver;

#[async_trait]
impl Driver for PostgresDriver {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::Postgres(cfg) = cfg else {
            return Err(Error::Config("not a Postgres connection".into()));
        };

        let mut config = Config::new();
        config
            .host(&cfg.host)
            .port(cfg.port)
            .dbname(&cfg.database)
            .user(&cfg.user)
            .application_name("sonde")
            .connect_timeout(CONNECT_TIMEOUT)
            .ssl_mode(match cfg.tls {
                TlsMode::Disable => SslMode::Disable,
                TlsMode::Prefer => SslMode::Prefer,
                TlsMode::Require | TlsMode::VerifyFull => SslMode::Require,
            });
        if let Some(secret) = &secret {
            config.password(secret.expose());
        }
        let tls = MakeRustlsConnect::new(tls::client_config(cfg.tls)?);

        let target = cfg.clone();
        let session = bridge(async move {
            let (client, mut connection) = config
                .connect(tls)
                .await
                .map_err(|error| Error::Connect(connect_problem(&error, &target)))?;

            // The connection future does the socket I/O. Polling it for
            // messages, rather than just awaiting it, is what surfaces
            // notices.
            let notices = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&notices);
            tokio::spawn(async move {
                let mut messages = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
                while let Some(Ok(message)) = messages.next().await {
                    if let AsyncMessage::Notice(notice) = message {
                        sink.lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(format!("{}: {}", notice.severity(), notice.message()));
                    }
                }
            });

            let cancel = client.cancel_token();
            Ok(PostgresSession {
                client: Arc::new(client),
                cancel,
                cancel_tls: tls::client_config(TlsMode::Prefer).map(MakeRustlsConnect::new)?,
                notices,
                busy: Arc::default(),
            })
        })
        .await?;
        Ok(Box::new(session))
    }
}

/// Run `future` on the driver runtime and wait for it from any executor.
async fn bridge<T, F>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    // The task only disappears without answering if the runtime is going
    // away, which means the connection is too.
    runtime::spawn(future).await.unwrap_or(Err(Error::Closed))
}

struct PostgresSession {
    client: Arc<Client>,
    cancel: CancelToken,
    /// Cancel requests travel on their own short-lived connection. They carry
    /// only the backend's key, so certificate checks add nothing here.
    cancel_tls: MakeRustlsConnect,
    notices: Arc<Mutex<Vec<String>>>,
    /// Held from the moment a statement is sent until its last message has
    /// been read. The next statement waits for it, so a cancel aimed at an
    /// abandoned statement can never land on its successor.
    busy: Arc<tokio::sync::Mutex<()>>,
}

impl PostgresSession {
    fn fire_cancel(cancel: CancelToken, tls: MakeRustlsConnect) {
        runtime::handle().spawn(async move {
            // If the request cannot be delivered the statement simply runs on.
            let _ = cancel.cancel_query(tls).await;
        });
    }
}

#[async_trait]
impl Session for PostgresSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        let client = Arc::clone(&self.client);
        let notices = Arc::clone(&self.notices);
        let cancel = self.cancel.clone();
        let cancel_tls = self.cancel_tls.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        let busy = Arc::clone(&self.busy);

        bridge(async move {
            let turn = busy.lock_owned().await;
            let started = Instant::now();
            notices
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();

            let located = |error| in_bytes(query_error(error), &sql);
            let statement = client.prepare(&sql).await.map_err(located)?;
            let columns = statement
                .columns()
                .iter()
                .map(|column| ColumnMeta {
                    name: column.name().into(),
                    type_name: column.type_().name().into(),
                    origin: None,
                })
                .collect();
            let returns_rows = !statement.columns().is_empty();
            let rows = client
                .query_raw(&statement, params.iter().map(Param))
                .await
                .map_err(located)?;

            let (mut pages, pages_rx) = mpsc::channel(CHANNEL_PAGES);
            let (summary, summary_rx) = oneshot::channel();
            tokio::spawn(async move {
                // Released when this task ends, which is when the statement
                // is over on the wire.
                let _turn = turn;
                pin_mut!(rows);
                let mut page = Vec::with_capacity(PAGE_ROWS);
                let mut rows_returned = 0_u64;
                let outcome: Result<()> = async {
                    while let Some(row) = rows.next().await {
                        page.push(decode_row(&row.map_err(query_error)?)?);
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

                match outcome {
                    Ok(()) => {
                        // Nobody may be waiting for the summary; that is fine.
                        let _ = summary.send(QuerySummary {
                            // Postgres also tags a SELECT with a row count;
                            // that is rows returned, not rows changed.
                            rows_affected: rows.rows_affected().filter(|_| !returns_rows),
                            rows_returned,
                            elapsed: started.elapsed(),
                            notices: std::mem::take(
                                &mut *notices.lock().unwrap_or_else(PoisonError::into_inner),
                            ),
                        });
                    }
                    // The consumer dropped the handle mid-stream. Tell the
                    // server to stop, or it would send every remaining row,
                    // then read whatever was already on its way. Waiting for
                    // the cancel request to be delivered before giving up the
                    // turn is what keeps it away from the next statement.
                    Err(Error::Closed) => {
                        let _ = cancel.cancel_query(cancel_tls).await;
                        while rows.next().await.is_some() {}
                    }
                    // The consumer may also be gone; nothing more to do then.
                    Err(error) => drop(pages.send(Err(error)).await),
                }
            });

            Ok(QueryHandle {
                columns,
                pages: pages_rx,
                summary: summary_rx,
            })
        })
        .await
    }

    fn canceller(&self) -> Canceller {
        let cancel = self.cancel.clone();
        let tls = self.cancel_tls.clone();
        Canceller::new(move || Self::fire_cancel(cancel.clone(), tls.clone()))
    }

    async fn catalog(&self) -> Result<Catalog> {
        let client = Arc::clone(&self.client);
        let busy = Arc::clone(&self.busy);
        bridge(async move {
            let _turn = busy.lock().await;
            introspect::catalog(&client).await
        })
        .await
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let client = Arc::clone(&self.client);
        let table = table.clone();
        let busy = Arc::clone(&self.busy);
        bridge(async move {
            let _turn = busy.lock().await;
            introspect::table_detail(&client, &table).await
        })
        .await
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<()> {
        let client = Arc::clone(&self.client);
        let statements = statements.to_vec();
        let busy = Arc::clone(&self.busy);

        bridge(async move {
            // One turn for the whole change set, so a cancel or a successor
            // statement cannot land mid-transaction.
            let _turn = busy.lock_owned().await;
            client
                .execute("BEGIN", &[])
                .await
                .map_err(|error| in_bytes(query_error(error), "BEGIN"))?;
            let mut failed = None;
            for statement in &statements {
                if failed.is_some() {
                    break;
                }
                let boxes: Vec<Box<dyn ToSql + Sync + Send>> = statement
                    .params
                    .iter()
                    .map(|value| Box::new(Param(value)) as Box<dyn ToSql + Sync + Send>)
                    .collect();
                let refs: Vec<&(dyn ToSql + Sync)> = boxes
                    .iter()
                    .map(|boxed| boxed.as_ref() as &(dyn ToSql + Sync))
                    .collect();
                let affected = client
                    .execute(&statement.sql, &refs)
                    .await
                    .map_err(|error| in_bytes(query_error(error), &statement.sql));
                failed = match affected {
                    Ok(1) => None,
                    // The row this statement keyed no longer matches one row:
                    // it moved or went away.
                    Ok(_) => Some(Error::Stale),
                    Err(error) => Some(error),
                };
            }
            match failed {
                None => {
                    client
                        .execute("COMMIT", &[])
                        .await
                        .map_err(|error| in_bytes(query_error(error), "COMMIT"))?;
                }
                // The backend is holding an aborted (or uncommitted)
                // transaction; end it, or the session's next statement runs
                // inside it.
                Some(error) => {
                    let _ = client.execute("ROLLBACK", &[]).await;
                    return Err(error);
                }
            }
            Ok(())
        })
        .await
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &PostgresDialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        if self.client.is_closed() {
            return Err(Error::Closed);
        }
        // Dropping the last client handle makes the connection task send
        // Terminate and finish.
        Ok(())
    }
}

/// Send one item, mapping a vanished consumer to [`Error::Closed`].
async fn send(
    pages: &mut mpsc::Sender<Result<ResultPage>>,
    item: Result<ResultPage>,
) -> Result<()> {
    pages.send(item).await.map_err(|_| Error::Closed)
}

fn decode_row(row: &Row) -> Result<Vec<Value>> {
    (0..row.len())
        .map(|ix| {
            row.try_get::<_, Cell>(ix)
                .map(|cell| cell.0)
                .map_err(query_error)
        })
        .collect()
}

/// The most specific message an error has.
fn describe(error: &tokio_postgres::Error) -> String {
    use std::error::Error as _;

    match (error.as_db_error(), error.source()) {
        (Some(db), _) => db.message().to_owned(),
        (None, Some(source)) => format!("{error}: {source}"),
        (None, None) => error.to_string(),
    }
}

/// Why a connection attempt failed, in words that say what to check.
fn connect_problem(error: &tokio_postgres::Error, cfg: &crate::driver::PostgresConfig) -> String {
    let address = format!("{}:{}", cfg.host, cfg.port);
    if let Some(db) = error.as_db_error() {
        return match db.code().code() {
            // invalid_password
            "28P01" => format!(
                "the server rejected the password for user \"{}\". Check the password and \
                 the user name.",
                cfg.user
            ),
            // invalid_authorization_specification, mostly pg_hba.conf
            "28000" => format!(
                "{}. The server's pg_hba.conf does not allow this login.",
                db.message()
            ),
            // invalid_catalog_name
            "3D000" => format!(
                "the database \"{}\" does not exist on {address}.",
                cfg.database
            ),
            // too_many_connections
            "53300" => format!("the server at {address} has no free connections left."),
            _ => db.message().to_owned(),
        };
    }
    let text = describe(error);
    let lower = text.to_lowercase();
    if lower.contains("password missing") {
        "the server asks for a password and none was given.".to_owned()
    } else if lower.contains("connection refused") {
        format!(
            "nothing is accepting connections at {address}. Check the host and port, and \
             that the server is running."
        )
    } else if lower.contains("lookup address") || lower.contains("name or service not known") {
        format!(
            "the host \"{}\" could not be found. Check its name.",
            cfg.host
        )
    } else if lower.contains("timed out") || lower.contains("timeout") {
        format!(
            "no answer from {address} within {} seconds. The host may be wrong, or a \
             firewall may be blocking the port.",
            CONNECT_TIMEOUT.as_secs()
        )
    } else if lower.contains("server does not support tls") {
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

/// SQLSTATE codes that mean the server ended the session.
const CONNECTION_ENDED: [&str; 3] = ["57P01", "57P02", "57P03"];

pub(super) fn query_error(error: tokio_postgres::Error) -> Error {
    use std::error::Error as _;

    if error.is_closed()
        || error
            .as_db_error()
            .is_some_and(|db| CONNECTION_ENDED.contains(&db.code().code()))
    {
        return Error::Closed;
    }
    // A parameter our encoder refused surfaces as the error's source.
    if let Some(parameter) = error
        .source()
        .and_then(|source| source.downcast_ref::<Error>())
    {
        return parameter.clone();
    }
    let Some(db) = error.as_db_error() else {
        return Error::query(describe(&error));
    };
    if db.code().code() == QUERY_CANCELED {
        return Error::Cancelled;
    }
    Error::Query {
        message: db.message().to_owned(),
        // Postgres counts characters from 1. This is still a character
        // index; `in_bytes` finishes the conversion where the text is known.
        position: match db.position() {
            Some(ErrorPosition::Original(position)) => {
                usize::try_from(*position).ok().map(|p| p.saturating_sub(1))
            }
            _ => None,
        },
        code: Some(db.code().code().to_owned()),
        detail: db.detail().map(str::to_owned),
        hint: db.hint().map(str::to_owned),
    }
}

/// Convert a query error's position from a character index to a byte offset
/// into `sql`.
fn in_bytes(mut error: Error, sql: &str) -> Error {
    if let Error::Query { position, .. } = &mut error {
        *position = position.map(|chars| {
            sql.char_indices()
                .nth(chars)
                .map_or(sql.len(), |(offset, _)| offset)
        });
    }
    error
}
