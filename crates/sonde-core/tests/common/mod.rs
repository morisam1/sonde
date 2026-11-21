//! Helpers shared by the integration tests.
#![allow(dead_code, reason = "each test binary uses its own subset")]

use futures::{StreamExt as _, executor::block_on};
use sonde_core::{
    ConnectionConfig, DbKind, Error, PostgresConfig, QuerySummary, Result, Secret, ServerConfig,
    Session, SqliteConfig, TlsMode, Value,
};

/// The Postgres fixture from `tests/docker-compose.yml`.
pub fn postgres_config() -> ConnectionConfig {
    ConnectionConfig::Postgres(PostgresConfig {
        host: std::env::var("SONDE_TEST_PG_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("SONDE_TEST_PG_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(54329),
        database: "sonde".into(),
        user: "sonde".into(),
        tls: TlsMode::Prefer,
        options: Default::default(),
    })
}

pub fn postgres() -> Box<dyn Session> {
    block_on(sonde_core::connect(
        &postgres_config(),
        Some(Secret::new("sonde")),
    ))
    .expect("the Postgres fixture is running (docker compose -f tests/docker-compose.yml up -d)")
}

/// Whether the MySQL tests run against the MariaDB fixture instead
/// (`SONDE_TEST_MARIADB=1`).
pub fn mariadb() -> bool {
    std::env::var_os("SONDE_TEST_MARIADB").is_some_and(|v| v != "0")
}

/// The MySQL fixture from `tests/docker-compose.yml`, or the MariaDB one
/// when [`mariadb`] says so.
pub fn mysql_config() -> ConnectionConfig {
    let server = PostgresConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("SONDE_TEST_MYSQL_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(if mariadb() { 33070 } else { 33069 }),
        database: "sonde".into(),
        user: "sonde".into(),
        tls: TlsMode::Prefer,
        options: Default::default(),
    };
    if mariadb() {
        ConnectionConfig::Mariadb(server)
    } else {
        ConnectionConfig::Mysql(server)
    }
}

/// A server fixture on localhost.
pub fn local(kind: DbKind, port: u16, database: &str, user: &str) -> ConnectionConfig {
    ConnectionConfig::for_server(
        kind,
        ServerConfig {
            host: "127.0.0.1".into(),
            port,
            database: database.into(),
            user: user.into(),
            tls: TlsMode::Prefer,
            options: Default::default(),
        },
    )
    .expect("a server kind")
}

pub fn open(config: &ConnectionConfig, password: Option<&str>) -> Box<dyn Session> {
    block_on(sonde_core::connect(config, password.map(Secret::new))).unwrap_or_else(|error| {
        panic!(
            "the {} fixture is running (docker compose -f tests/docker-compose.yml up -d): {error}",
            config.kind().name()
        )
    })
}

pub fn mysql() -> Box<dyn Session> {
    block_on(sonde_core::connect(
        &mysql_config(),
        Some(Secret::new("sonde")),
    ))
    .expect("the MySQL fixture is running (docker compose -f tests/docker-compose.yml up -d)")
}

/// The SQL Server fixture from `tests/docker-compose.yml`.
pub fn mssql_config() -> ConnectionConfig {
    ConnectionConfig::SqlServer(PostgresConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("SONDE_TEST_MSSQL_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(14339),
        database: "master".into(),
        user: "sa".into(),
        tls: TlsMode::Prefer,
        options: Default::default(),
    })
}

pub fn mssql() -> Box<dyn Session> {
    block_on(sonde_core::connect(
        &mssql_config(),
        Some(Secret::new("Sonde-pass1")),
    ))
    .expect("the SQL Server fixture is running (docker compose -f tests/docker-compose.yml up -d)")
}

pub fn sqlite() -> Box<dyn Session> {
    let config = ConnectionConfig::Sqlite(SqliteConfig {
        path: ":memory:".into(),
        read_only: false,
    });
    block_on(sonde_core::connect(&config, None)).expect("in-memory SQLite opens")
}

/// Run a statement to completion and return its column names and rows.
pub fn table(session: &dyn Session, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    block_on(async {
        let mut handle = session.execute(sql, &[]).await?;
        let columns = handle.columns.iter().map(|c| c.name.to_string()).collect();
        let mut rows = Vec::new();
        while let Some(page) = handle.pages.next().await {
            rows.extend(page?.rows);
        }
        Ok((columns, rows))
    })
}

/// Run a statement to completion and return everything it produced.
pub fn run(
    session: &dyn Session,
    sql: &str,
    params: &[Value],
) -> Result<(Vec<Vec<Value>>, QuerySummary)> {
    block_on(async {
        let mut handle = session.execute(sql, params).await?;
        let mut rows = Vec::new();
        while let Some(page) = handle.pages.next().await {
            rows.extend(page?.rows);
        }
        let summary = handle.summary.await.map_err(|_| Error::Closed)?;
        Ok((rows, summary))
    })
}

/// Run statements that are expected to work and return nothing of interest.
pub fn setup(session: &dyn Session, statements: &[&str]) {
    for sql in statements {
        run(session, sql, &[]).unwrap_or_else(|error| panic!("setup `{sql}` failed: {error}"));
    }
}

/// The single cell of a one-row, one-column query.
pub fn scalar(session: &dyn Session, sql: &str) -> Value {
    scalar_with(session, sql, &[])
}

pub fn scalar_with(session: &dyn Session, sql: &str, params: &[Value]) -> Value {
    let (mut rows, _) = run(session, sql, params).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    rows.pop()
        .and_then(|mut row| row.pop())
        .unwrap_or_else(|| panic!("`{sql}` returned no cell"))
}

/// Resident memory of this process, in bytes.
pub fn resident_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("procfs is available");
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .and_then(|field| field.parse().ok())
        .expect("statm has a resident field");
    pages * 4096
}
