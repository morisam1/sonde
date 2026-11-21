//! The Snowflake driver against a stand-in for the SQL API: there is no
//! local Snowflake, so a small HTTP server answers the way the real one
//! does for the requests the driver makes.

mod common;

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
};

use common::{run, table};
use futures::executor::block_on;
use serde_json::{Value as Json, json};
use sonde_core::{ConnectionConfig, Error, Secret, ServerConfig, Session, TlsMode, Value};

/// Statements the stand-in received, with the warehouse each ran under.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

fn answer(method: &str, path: &str, body: &Json, seen: &Seen) -> (u16, Json) {
    let row_type = |names: &[(&str, &str)]| -> Json {
        names
            .iter()
            .map(|(name, kind)| json!({"name": name, "type": kind, "scale": 0, "precision": 38}))
            .collect()
    };
    if method == "GET" && path.starts_with("/api/v2/statements/big") {
        return (200, json!({"data": [["3"], ["4"]]}));
    }
    if method == "GET" && path.starts_with("/api/v2/statements/slow") {
        return (
            200,
            json!({
                "statementHandle": "slow",
                "resultSetMetaData": {"rowType": row_type(&[("DONE", "boolean")]), "partitionInfo": [{}]},
                "data": [["true"]]
            }),
        );
    }
    let statement = body["statement"].as_str().unwrap_or("").to_owned();
    seen.lock().unwrap().push((
        statement.clone(),
        body["warehouse"].as_str().map(str::to_owned),
    ));
    match statement.as_str() {
        "SELECT CURRENT_VERSION()" => (
            200,
            json!({
                "statementHandle": "v",
                "resultSetMetaData": {"rowType": row_type(&[("V", "text")]), "partitionInfo": [{}]},
                "data": [["9.1.0"]]
            }),
        ),
        "SELECT n, d FROM t" => (
            200,
            json!({
                "statementHandle": "big",
                "resultSetMetaData": {
                    "rowType": [
                        {"name": "N", "type": "fixed", "scale": 0, "precision": 38},
                        {"name": "D", "type": "date"}
                    ],
                    "partitionInfo": [{}, {}]
                },
                "data": [["1", "19723"], ["2", null]]
            }),
        ),
        "SELECT slow" => (202, json!({"statementHandle": "slow"})),
        "SHOW TERSE DATABASES" => (
            200,
            json!({
                "statementHandle": "dbs",
                "resultSetMetaData": {
                    "rowType": row_type(&[("created_on", "timestamp_ltz"), ("name", "text"), ("kind", "text")]),
                    "partitionInfo": [{}]
                },
                "data": [["1704067200.0", "ANALYTICS", "STANDARD"], ["1704067200.0", "RAW", "STANDARD"]]
            }),
        ),
        _ => (
            422,
            json!({
                "code": "001003",
                "sqlState": "42000",
                "message": "SQL compilation error:\nsyntax error line 1 at position 7 unexpected 'nonsense'."
            }),
        ),
    }
}

/// Start the stand-in; it answers until the test process ends.
fn stand_in() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let (mut length, mut authorized) = (0, false);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim();
                if header.is_empty() {
                    break;
                }
                let lower = header.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if lower == "authorization: bearer tok" {
                    authorized = true;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
            let (status, reply) = if authorized {
                answer(&method, &path, &body, &log)
            } else {
                (401, json!({"message": "Invalid token"}))
            };
            let text = reply.to_string();
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (url, seen)
}

fn config(url: &str) -> ConnectionConfig {
    let mut server = ServerConfig {
        host: url.to_owned(),
        port: 443,
        database: "DB".into(),
        user: "me".into(),
        tls: TlsMode::VerifyFull,
        options: Default::default(),
    };
    server.options.insert("warehouse".into(), "WH".into());
    ConnectionConfig::Snowflake(server)
}

fn connect(url: &str) -> Box<dyn Session> {
    block_on(sonde_core::connect(&config(url), Some(Secret::new("tok")))).unwrap()
}

#[test]
fn results_should_be_typed_and_read_across_partitions() {
    let (url, _) = stand_in();
    let session = connect(&url);

    let (columns, rows) = table(&*session, "SELECT n, d FROM t").unwrap();

    assert_eq!(columns, ["N", "D"]);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1].to_string(), "2024-01-01");
    assert_eq!(rows[1][1], Value::Null);
    assert_eq!(rows[3][0], Value::Int(4));
}

#[test]
fn a_statement_still_running_should_be_waited_for() {
    let (url, _) = stand_in();
    let session = connect(&url);

    let (_, rows) = table(&*session, "SELECT slow").unwrap();

    assert_eq!(rows, [[Value::Bool(true)]]);
}

#[test]
fn use_should_carry_over_to_the_next_statements() {
    let (url, seen) = stand_in();
    let session = connect(&url);

    run(&*session, "USE WAREHOUSE OTHER_WH", &[]).unwrap();
    let _ = run(&*session, "SELECT n, d FROM t", &[]);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.first().unwrap().1.as_deref(), Some("WH"));
    assert_eq!(seen.last().unwrap().1.as_deref(), Some("OTHER_WH"));
}

#[test]
fn errors_should_point_at_the_problem() {
    let (url, _) = stand_in();
    let session = connect(&url);
    let sql = "SELECT nonsense";

    let error = run(&*session, sql, &[]).unwrap_err();

    assert_eq!(
        error.position().map(|p| &sql[p..]),
        Some("nonsense"),
        "{error}"
    );
}

#[test]
fn a_bad_token_should_be_explained() {
    let (url, _) = stand_in();

    let outcome = block_on(sonde_core::connect(
        &config(&url),
        Some(Secret::new("wrong")),
    ));

    let Err(Error::Connect(message)) = outcome else {
        panic!("a wrong token should not connect");
    };
    assert!(message.contains("access token"), "{message}");
}

#[test]
fn databases_should_be_listed_by_name() {
    let (url, _) = stand_in();
    let session = connect(&url);

    assert_eq!(block_on(session.databases()).unwrap(), ["ANALYTICS", "RAW"]);
}
