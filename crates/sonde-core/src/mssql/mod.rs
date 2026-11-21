//! The SQL Server driver, over TDS with `tiberius`.
//!
//! A running statement borrows the client, so a cancel cannot reach it
//! directly. Each statement races its rows against a cancel signal instead;
//! when the signal wins, the statement's stream is dropped and an attention
//! request tells the server to stop.

mod decode;
mod introspect;

use std::{
    sync::{Arc, Mutex as StdMutex, PoisonError},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{
    FutureExt as _, SinkExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, Query, QueryItem};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

use crate::{
    driver::{
        CHANNEL_PAGES, Canceller, ConnectionConfig, DbKind, Dialect, Driver, PAGE_ROWS,
        QueryHandle, QuerySummary, Secret, ServerConfig, Session, TlsMode,
    },
    edit::EditStatement,
    error::{Error, Result},
    runtime,
    schema::{Catalog, TableDetail, TableRef},
    value::{ResultPage, Value},
};

type Conn = Client<Compat<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// SQL Server's SQL spelling.
#[derive(Debug, Clone, Copy)]
pub struct MssqlDialect;

impl Dialect for MssqlDialect {
    fn kind(&self) -> DbKind {
        DbKind::SqlServer
    }

    fn placeholder(&self, n: usize) -> String {
        format!("@P{n}")
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("[{}]", ident.replace(']', "]]"))
    }

    fn limit_clause(&self, limit: u64, offset: u64) -> String {
        format!("OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY")
    }
}

/// Opens SQL Server sessions.
#[derive(Debug, Clone, Copy)]
pub struct MssqlDriver;

#[async_trait]
impl Driver for MssqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::SqlServer
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>> {
        let ConnectionConfig::SqlServer(cfg) = cfg else {
            return Err(Error::Config("not a SQL Server connection".into()));
        };
        let cfg = cfg.clone();
        let password = secret.map(|secret| secret.expose().to_owned());
        let session = bridge(async move {
            let client = open(&cfg, password.as_deref()).await?;
            Ok(MssqlSession {
                state: Arc::new(Mutex::new(Some(client))),
                cfg,
                password,
                cancel: Arc::default(),
            })
        })
        .await?;
        Ok(Box::new(session))
    }
}

fn config(cfg: &ServerConfig, password: Option<&str>, encryption: EncryptionLevel) -> Config {
    let mut config = Config::new();
    config.host(&cfg.host);
    config.port(cfg.port);
    if !cfg.database.is_empty() {
        config.database(&cfg.database);
    }
    config.application_name("sonde");
    config.authentication(AuthMethod::sql_server(
        &cfg.user,
        password.unwrap_or_default(),
    ));
    config.encryption(encryption);
    if cfg.tls != TlsMode::VerifyFull {
        config.trust_cert();
    }
    config
}

async fn attempt(config: Config) -> std::result::Result<Conn, tiberius::error::Error> {
    let connect = async {
        let tcp = TcpStream::connect(config.get_addr()).await?;
        tcp.set_nodelay(true)?;
        Client::connect(config, tcp.compat_write()).await
    };
    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(outcome) => outcome,
        Err(_) => {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out").into())
        }
    }
}

/// Connect, falling back to plain text under [`TlsMode::Prefer`] when the
/// server does not do TLS.
async fn open(cfg: &ServerConfig, password: Option<&str>) -> Result<Conn> {
    crate::tls_provider();
    let level = match cfg.tls {
        TlsMode::Disable => EncryptionLevel::NotSupported,
        TlsMode::Prefer => EncryptionLevel::On,
        TlsMode::Require | TlsMode::VerifyFull => EncryptionLevel::Required,
    };
    match attempt(config(cfg, password, level)).await {
        Ok(client) => Ok(client),
        Err(error) if cfg.tls == TlsMode::Prefer && is_tls_problem(&error) => {
            attempt(config(cfg, password, EncryptionLevel::NotSupported))
                .await
                .map_err(|error| Error::Connect(connect_problem(&error, cfg)))
        }
        Err(error) => Err(Error::Connect(connect_problem(&error, cfg))),
    }
}

fn is_tls_problem(error: &tiberius::error::Error) -> bool {
    matches!(error, tiberius::error::Error::Tls(_))
        || error.to_string().to_lowercase().contains("encrypt")
}

/// Why a connection attempt failed, in words that say what to check.
fn connect_problem(error: &tiberius::error::Error, cfg: &ServerConfig) -> String {
    let address = format!("{}:{}", cfg.host, cfg.port);
    if let tiberius::error::Error::Server(server) = error {
        return match server.code() {
            18456 => format!(
                "the server rejected the password for user \"{}\". Check the password and \
                 the user name, and that the server allows SQL Server logins.",
                cfg.user
            ),
            4060 => format!(
                "the database \"{}\" does not exist on {address}, or user \"{}\" may not \
                 open it.",
                cfg.database, cfg.user
            ),
            _ => server.message().to_owned(),
        };
    }
    let text = error.to_string();
    let lower = text.to_lowercase();
    if lower.contains("connection refused") {
        format!(
            "nothing is accepting connections at {address}. Check the host and port, and \
             that the server is running."
        )
    } else if lower.contains("lookup") || lower.contains("name or service not known") {
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
    } else if lower.contains("certificate") {
        format!(
            "the server's TLS certificate could not be verified ({text}). Set TLS to Require to \
             encrypt without checking the certificate."
        )
    } else {
        text
    }
}

async fn bridge<T, F>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    runtime::spawn(future).await.unwrap_or(Err(Error::Closed))
}

pub(super) fn query_error(error: tiberius::error::Error, sql: &str) -> Error {
    use tiberius::error::Error as T;
    match error {
        T::Server(server) => {
            // Errors of class 20 and above end the session.
            if server.class() >= 20 {
                return Error::Closed;
            }
            Error::Query {
                position: line_start(sql, server.line()),
                message: server.message().to_owned(),
                code: Some(server.code().to_string()),
                detail: None,
                hint: None,
            }
        }
        T::Io { .. } | T::Protocol(_) | T::Tls(_) => Error::Closed,
        other => Error::query(other.to_string()),
    }
}

/// The byte offset where line `line` (counting from 1) of `sql` starts.
/// SQL Server reports the line of an error but not the column.
fn line_start(sql: &str, line: u32) -> Option<usize> {
    if line <= 1 {
        return (line == 1).then_some(sql.len() - sql.trim_start().len());
    }
    sql.match_indices('\n')
        .nth(usize::try_from(line).ok()? - 2)
        .map(|(ix, _)| ix + 1)
}

/// The signal that stops the running statement, if there is one.
type CancelSlot = Arc<StdMutex<Option<oneshot::Sender<()>>>>;

struct MssqlSession {
    /// `None` after the connection was found broken; the next statement
    /// opens a new one.
    state: Arc<Mutex<Option<Conn>>>,
    cfg: ServerConfig,
    password: Option<String>,
    cancel: CancelSlot,
}

/// The client, reopening it when the last one broke.
async fn client<'a>(
    state: &'a mut Option<Conn>,
    cfg: &ServerConfig,
    password: Option<&str>,
) -> Result<&'a mut Conn> {
    if state.is_none() {
        *state = Some(open(cfg, password).await.map_err(|_| Error::Closed)?);
    }
    state.as_mut().ok_or(Error::Closed)
}

#[async_trait]
impl Session for MssqlSession {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle> {
        let state = Arc::clone(&self.state);
        let cfg = self.cfg.clone();
        let password = self.password.clone();
        let slot = Arc::clone(&self.cancel);
        let sql = sql.to_owned();
        let params = params.to_vec();
        // Refuse unsendable values before taking the session.
        for value in &params {
            decode::check_param(value)?;
        }

        bridge(async move {
            let (ready, ready_rx) = oneshot::channel();
            let (pages, pages_rx) = mpsc::channel(CHANNEL_PAGES);
            let (summary, summary_rx) = oneshot::channel();
            tokio::spawn(async move {
                let mut state = state.lock_owned().await;
                let (cancel, cancelled) = oneshot::channel();
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(cancel);
                let outcome = run(
                    &mut state,
                    &cfg,
                    password.as_deref(),
                    &sql,
                    &params,
                    Report {
                        ready,
                        pages,
                        summary,
                    },
                    cancelled,
                )
                .await;
                slot.lock().unwrap_or_else(PoisonError::into_inner).take();
                if outcome.is_err() {
                    // The connection broke; the next statement reopens it.
                    *state = None;
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
        let slot = Arc::clone(&self.cancel);
        Canceller::new(move || {
            if let Some(cancel) = slot.lock().unwrap_or_else(PoisonError::into_inner).take() {
                let _ = cancel.send(());
            }
        })
    }

    async fn catalog(&self) -> Result<Catalog> {
        let state = Arc::clone(&self.state);
        let cfg = self.cfg.clone();
        let password = self.password.clone();
        bridge(async move {
            let mut state = state.lock().await;
            let conn = client(&mut state, &cfg, password.as_deref()).await?;
            introspect::catalog(conn).await
        })
        .await
    }

    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail> {
        let state = Arc::clone(&self.state);
        let cfg = self.cfg.clone();
        let password = self.password.clone();
        let table = table.clone();
        bridge(async move {
            let mut state = state.lock().await;
            let conn = client(&mut state, &cfg, password.as_deref()).await?;
            introspect::table_detail(conn, &table).await
        })
        .await
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<()> {
        for statement in statements {
            for value in &statement.params {
                decode::check_param(value)?;
            }
        }
        let state = Arc::clone(&self.state);
        let cfg = self.cfg.clone();
        let password = self.password.clone();
        let statements = statements.to_vec();
        bridge(async move {
            let mut state = state.lock().await;
            let conn = client(&mut state, &cfg, password.as_deref()).await?;
            conn.simple_query("BEGIN TRANSACTION")
                .await
                .map_err(|error| query_error(error, "BEGIN TRANSACTION"))?
                .into_results()
                .await
                .map_err(|error| query_error(error, "BEGIN TRANSACTION"))?;
            let mut failed = None;
            for statement in &statements {
                let mut query = Query::new(statement.sql.clone());
                for value in &statement.params {
                    decode::bind(&mut query, value);
                }
                failed = match query.execute(conn).await.map(|result| result.total()) {
                    Ok(1) => None,
                    Ok(_) => Some(Error::Stale),
                    Err(error) => Some(query_error(error, &statement.sql)),
                };
                if failed.is_some() {
                    break;
                }
            }
            let end = if failed.is_none() {
                "COMMIT"
            } else {
                "IF @@TRANCOUNT > 0 ROLLBACK"
            };
            let ended = match conn.simple_query(end).await {
                Ok(stream) => stream.into_results().await.map(drop),
                Err(error) => Err(error),
            };
            match (failed, ended) {
                (Some(error), _) => Err(error),
                (None, Err(error)) => Err(query_error(error, end)),
                (None, Ok(())) => Ok(()),
            }
        })
        .await
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &MssqlDialect
    }

    async fn close(self: Box<Self>) -> Result<()> {
        let state = Arc::clone(&self.state);
        bridge(async move {
            match state.lock().await.take() {
                Some(client) => client.close().await.map_err(|_| Error::Closed),
                None => Err(Error::Closed),
            }
        })
        .await
    }
}

/// Where a running statement reports to.
struct Report {
    ready: oneshot::Sender<Result<Vec<crate::value::ColumnMeta>>>,
    pages: mpsc::Sender<Result<ResultPage>>,
    summary: oneshot::Sender<QuerySummary>,
}

/// How a statement's stream ended.
enum End {
    Done,
    /// The user cancelled, or the consumer went away.
    Stopped {
        told_consumer: bool,
    },
    Failed(Error),
}

/// Run one statement and stream its first result set. Returns `Err` only
/// when the connection is no longer usable.
async fn run(
    state: &mut Option<Conn>,
    cfg: &ServerConfig,
    password: Option<&str>,
    sql: &str,
    params: &[Value],
    report: Report,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<()> {
    let Report {
        ready,
        mut pages,
        summary,
    } = report;
    let conn = match client(state, cfg, password).await {
        Ok(conn) => conn,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Err(Error::Closed);
        }
    };
    let started = Instant::now();
    let mut ready = Some(ready);
    let mut columns_known = false;
    let mut rows_returned = 0_u64;

    let end = {
        let started_stream = async {
            if params.is_empty() {
                conn.simple_query(sql).await
            } else {
                let mut query = Query::new(sql.to_owned());
                for value in params {
                    decode::bind(&mut query, value);
                }
                query.query(conn).await
            }
        };
        // The server may take a long time to answer at all, as with
        // WAITFOR, so the cancel races the first answer too.
        let stream = futures::select_biased! {
            _ = (&mut cancelled).fuse() => None,
            stream = started_stream.fuse() => Some(stream),
        };
        match stream {
            None => End::Stopped {
                told_consumer: false,
            },
            Some(Err(error)) => End::Failed(query_error(error, sql)),
            Some(Ok(mut stream)) => {
                let mut decoders = Vec::new();
                let mut page = Vec::with_capacity(PAGE_ROWS);
                loop {
                    let item = futures::select_biased! {
                        _ = (&mut cancelled).fuse() => break End::Stopped { told_consumer: false },
                        item = stream.next().fuse() => item,
                    };
                    match item {
                        None => {
                            if !page.is_empty() {
                                let rows = std::mem::take(&mut page);
                                if pages.send(Ok(ResultPage { rows })).await.is_err() {
                                    break End::Stopped {
                                        told_consumer: true,
                                    };
                                }
                            }
                            break End::Done;
                        }
                        Some(Err(error)) => break End::Failed(query_error(error, sql)),
                        // Only the first result set is shown; later ones are
                        // read and dropped.
                        Some(Ok(QueryItem::Metadata(meta))) if !columns_known => {
                            columns_known = true;
                            decoders = meta
                                .columns()
                                .iter()
                                .map(decode::Decoder::for_column)
                                .collect();
                            let described = meta
                                .columns()
                                .iter()
                                .zip(&decoders)
                                .map(|(column, decoder)| decoder.meta(column))
                                .collect();
                            if let Some(ready) = ready.take()
                                && ready.send(Ok(described)).is_err()
                            {
                                break End::Stopped {
                                    told_consumer: true,
                                };
                            }
                        }
                        Some(Ok(QueryItem::Metadata(meta))) if meta.result_index() > 0 => {
                            // A later result set: its rows are skipped.
                            decoders.clear();
                        }
                        Some(Ok(QueryItem::Metadata(_))) => {}
                        Some(Ok(QueryItem::Row(row))) => {
                            if decoders.is_empty() {
                                continue;
                            }
                            page.push(decode::row(row, &decoders));
                            rows_returned += 1;
                            if page.len() == PAGE_ROWS {
                                let rows =
                                    std::mem::replace(&mut page, Vec::with_capacity(PAGE_ROWS));
                                if pages.send(Ok(ResultPage { rows })).await.is_err() {
                                    break End::Stopped {
                                        told_consumer: true,
                                    };
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    match end {
        End::Done => {
            if let Some(ready) = ready.take() {
                let _ = ready.send(Ok(Vec::new()));
            }
            // TDS reports the count in a token the stream does not surface;
            // ask for it while it still describes this statement.
            let rows_affected = if columns_known {
                None
            } else {
                rowcount(conn).await
            };
            let _ = summary.send(QuerySummary {
                rows_affected,
                rows_returned,
                elapsed: started.elapsed(),
                notices: Vec::new(),
            });
            Ok(())
        }
        End::Stopped { told_consumer } => {
            // Tell the server to stop and wait until it has, so the next
            // statement starts on a quiet connection.
            let stopped = conn.cancel_query().await;
            if !told_consumer {
                match ready.take() {
                    Some(ready) => drop(ready.send(Err(Error::Cancelled))),
                    None => drop(pages.send(Err(Error::Cancelled)).await),
                }
            }
            stopped.map_err(|_| Error::Closed)
        }
        End::Failed(error) => {
            let broken = error.is_connection_lost();
            match ready.take() {
                Some(ready) => drop(ready.send(Err(error))),
                None => drop(pages.send(Err(error)).await),
            }
            if broken { Err(Error::Closed) } else { Ok(()) }
        }
    }
}

async fn rowcount(conn: &mut Conn) -> Option<u64> {
    let row = conn
        .simple_query("SELECT CAST(@@ROWCOUNT AS bigint)")
        .await
        .ok()?
        .into_row()
        .await
        .ok()??;
    row.get::<i64, _>(0).and_then(|n| u64::try_from(n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_start_should_find_the_reported_line() {
        let sql = "  select 1\nfrom t\nwhere x";

        assert_eq!(line_start(sql, 1), Some(2));
        assert_eq!(line_start(sql, 3), Some(18));
        assert_eq!(line_start(sql, 9), None);
    }

    #[test]
    fn dialect_should_bracket_names_and_number_placeholders() {
        assert_eq!(MssqlDialect.quote_ident("a]b"), "[a]]b]");
        assert_eq!(MssqlDialect.placeholder(3), "@P3");
    }
}
