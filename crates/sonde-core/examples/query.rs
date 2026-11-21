//! Run SQL from the command line, for checking a driver by hand.
//!
//! ```sh
//! cargo run -p sonde-core --example query -- postgres://user:pass@host:5432/db "select 1"
//! cargo run -p sonde-core --example query -- path/to/file.db "select * from t; select 2"
//! ```
//!
//! Anything that does not start with `postgres://` is taken as a SQLite path.
//! A script of several statements runs them in order and stops at the first
//! error.

use std::{process::ExitCode, str::FromStr as _};

use futures::{StreamExt as _, executor::block_on};
use sonde_core::{
    ConnectionConfig, Error, PostgresConfig, Secret, Session, SqliteConfig, TlsMode, split,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [target, script] = args.as_slice() else {
        eprintln!("usage: query <postgres-url | sqlite-path> <sql>");
        return ExitCode::from(2);
    };
    match block_on(run(target, script)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(target: &str, script: &str) -> Result<(), String> {
    let (config, secret) = parse_target(target)?;
    let kind = config.kind();
    let session = sonde_core::connect(&config, secret)
        .await
        .map_err(|error| error.to_string())?;

    for statement in split::split(script, kind) {
        execute(&*session, statement.sql)
            .await
            .map_err(|error| describe(&error, &statement))?;
    }
    session.close().await.map_err(|error| error.to_string())
}

async fn execute(session: &dyn Session, sql: &str) -> sonde_core::Result<()> {
    let mut handle = session.execute(sql, &[]).await?;
    if !handle.columns.is_empty() {
        let header: Vec<&str> = handle.columns.iter().map(|c| &*c.name).collect();
        println!("{}", header.join("\t"));
    }
    while let Some(page) = handle.pages.next().await {
        for row in page?.rows {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| {
                    if cell.is_null() {
                        "NULL".to_owned()
                    } else {
                        cell.to_string()
                    }
                })
                .collect();
            println!("{}", cells.join("\t"));
        }
    }
    let summary = handle.summary.await.map_err(|_| Error::Closed)?;
    for notice in &summary.notices {
        eprintln!("{notice}");
    }
    match summary.rows_affected {
        Some(rows) => eprintln!("-- {rows} rows affected, {:.1?}", summary.elapsed),
        None => eprintln!("-- {} rows, {:.1?}", summary.rows_returned, summary.elapsed),
    }
    Ok(())
}

/// The error with the line it points at, when it points anywhere.
fn describe(error: &Error, statement: &split::Statement<'_>) -> String {
    let Some(position) = error.position() else {
        return format!("error: {error}");
    };
    let before = &statement.sql[..position.min(statement.sql.len())];
    let line = statement.line + before.matches('\n').count();
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    format!("error at line {line}, column {column}: {error}")
}

fn parse_target(target: &str) -> Result<(ConnectionConfig, Option<Secret>), String> {
    if !target.starts_with("postgres://") && !target.starts_with("postgresql://") {
        let config = ConnectionConfig::Sqlite(SqliteConfig {
            path: target.into(),
            read_only: false,
        });
        return Ok((config, None));
    }

    let parsed = tokio_postgres::Config::from_str(target).map_err(|error| error.to_string())?;
    let host = match parsed.get_hosts().first() {
        Some(tokio_postgres::config::Host::Tcp(host)) => host.clone(),
        _ => return Err("the URL needs a host name".to_owned()),
    };
    let user = parsed.get_user().ok_or("the URL needs a user")?.to_owned();
    let secret = parsed
        .get_password()
        .map(|password| Secret::new(String::from_utf8_lossy(password)));
    let config = ConnectionConfig::Postgres(PostgresConfig {
        host,
        port: parsed.get_ports().first().copied().unwrap_or(5432),
        database: parsed.get_dbname().unwrap_or(&user).to_owned(),
        user,
        tls: TlsMode::Prefer,
        options: Default::default(),
    });
    Ok((config, secret))
}
