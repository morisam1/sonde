//! The Redis driver against the Redis and Valkey fixtures in
//! `tests/docker-compose.yml`. Each test works in its own database number.

mod common;

use std::time::{Duration, Instant};

use common::{local, open, run, scalar, setup, table};
use futures::executor::block_on;
use sonde_core::{DbKind, Error, Secret, Session, TableRef, Value, schema::RelationKind};

fn redis_port() -> u16 {
    std::env::var("SONDE_TEST_REDIS_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(63799)
}

/// A session on database `db`, emptied first.
fn fresh(db: u8) -> Box<dyn Session> {
    let session = open(
        &local(DbKind::Redis, redis_port(), &db.to_string(), ""),
        None,
    );
    setup(&*session, &["FLUSHDB"]);
    session
}

fn text(value: &str) -> Value {
    Value::text(value)
}

#[test]
fn commands_should_return_their_replies_as_rows() {
    let session = fresh(1);
    setup(
        &*session,
        &["SET greeting \"hello world\"", "RPUSH l a b c"],
    );

    assert_eq!(scalar(&*session, "GET greeting"), text("hello world"));
    assert_eq!(scalar(&*session, "INCR counter"), Value::Int(1));
    assert_eq!(scalar(&*session, "GET missing"), Value::Null);

    let (columns, rows) = table(&*session, "LRANGE l 1 -1").unwrap();
    assert_eq!(columns, ["index", "value"]);
    assert_eq!(
        rows,
        [[Value::Int(1), text("b")], [Value::Int(2), text("c")]]
    );
}

#[test]
fn hashes_and_sorted_sets_should_come_back_as_pairs() {
    let session = fresh(2);
    setup(&*session, &["HSET h name ada lang en", "ZADD z 2 b 1 a"]);

    let (columns, mut rows) = table(&*session, "hgetall h").unwrap();
    rows.sort_by_key(|row| row[0].to_string());
    assert_eq!(columns, ["field", "value"]);
    assert_eq!(
        rows,
        [[text("lang"), text("en")], [text("name"), text("ada")]]
    );

    let (columns, rows) = table(&*session, "ZRANGE z 0 -1 WITHSCORES").unwrap();
    assert_eq!(columns, ["member", "score"]);
    assert_eq!(rows[0], [text("a"), text("1")]);
}

#[test]
fn scan_should_run_to_the_end_and_stream_every_key() {
    let session = fresh(3);
    let keys: Vec<String> = (0..2500).map(|n| format!("k{n} {n}")).collect();
    setup(&*session, &[&format!("MSET {}", keys.join(" "))]);

    let (rows, summary) = run(&*session, "SCAN 0 MATCH k1* COUNT 100", &[]).unwrap();

    // k1, k10..k19, k100..k199, k1000..k1999
    assert_eq!(rows.len(), 1 + 10 + 100 + 1000);
    assert_eq!(summary.rows_returned, 1111);
}

#[test]
fn errors_should_carry_the_server_message() {
    let session = fresh(4);
    setup(&*session, &["SET s x"]);

    let error = run(&*session, "LPUSH s y", &[]).unwrap_err();
    let Error::Query { message, code, .. } = error else {
        panic!("expected a query error, got {error:?}");
    };
    assert!(message.contains("WRONGTYPE"), "{message}");
    assert_eq!(code.as_deref(), Some("WRONGTYPE"));

    assert!(run(&*session, "NOSUCHCOMMAND", &[]).is_err());
    assert!(run(&*session, "GET \"open", &[]).is_err());
}

#[test]
fn cancel_should_stop_a_blocking_command_and_leave_the_session_usable() {
    let session = fresh(5);
    let canceller = session.canceller();
    let started = Instant::now();

    let outcome = block_on(async {
        let running = session.execute("BLPOP nothing 30", &[]);
        let cancel = async {
            futures_timer(Duration::from_millis(300)).await;
            canceller.cancel();
        };
        let (outcome, ()) = futures::join!(running, cancel);
        outcome.map(|_| ())
    });

    assert_eq!(outcome.unwrap_err(), Error::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(scalar(&*session, "PING"), text("PONG"));
}

async fn futures_timer(duration: Duration) {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        let _ = tx.send(());
    });
    let _ = rx.await;
}

#[test]
fn catalog_should_list_the_keys_of_the_current_database() {
    let session = fresh(6);
    setup(&*session, &["SET b 1", "HSET a f v"]);

    let catalog = block_on(session.catalog()).unwrap();

    assert_eq!(catalog.schemas.len(), 1);
    assert_eq!(catalog.schemas[0].name, "db6");
    let keys: Vec<(&str, RelationKind)> = catalog.schemas[0]
        .relations
        .iter()
        .map(|r| (r.name.as_str(), r.kind))
        .collect();
    assert_eq!(keys, [("a", RelationKind::Key), ("b", RelationKind::Key)]);
}

#[test]
fn table_view_should_read_a_key_in_the_shape_of_its_type() {
    let session = fresh(7);
    setup(
        &*session,
        &["HSET user:1 name ada lang en", "SADD tags x y"],
    );
    let dialect = session.dialect();

    for (key, filter, want) in [
        ("user:1", "", vec!["field", "value"]),
        ("tags", "x*", vec!["member"]),
    ] {
        let detail = block_on(session.table_detail(&TableRef::new("db7", key))).unwrap();
        let names: Vec<String> = detail.columns.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, want);
        assert!(dialect.read_only_reason().is_some());

        let sql = dialect.browse(&TableRef::new("db7", key), &names, filter, None);
        let (rows, _) = run(&*session, &sql, &[]).unwrap();
        assert!(!rows.is_empty(), "{sql}");
        assert!(rows.iter().all(|row| row.len() == names.len()), "{sql}");
    }
    assert!(block_on(session.table_detail(&TableRef::new("db7", "missing"))).is_err());
}

#[test]
fn select_should_move_the_session_and_its_catalog() {
    let session = fresh(8);
    setup(&*session, &["SELECT 9", "FLUSHDB", "SET only-in-9 1"]);

    let catalog = block_on(session.catalog()).unwrap();

    assert_eq!(catalog.schemas[0].name, "db9");
    assert_eq!(catalog.schemas[0].relations[0].name, "only-in-9");
}

#[test]
fn info_should_become_a_table() {
    let session = fresh(10);

    let (columns, rows) = table(&*session, "INFO server").unwrap();

    assert_eq!(columns, ["section", "key", "value"]);
    assert!(rows.iter().any(|row| row[1] == text("redis_version")));
}

#[test]
fn valkey_should_need_and_take_a_password() {
    let config = local(DbKind::Redis, 63800, "0", "");

    let refused = block_on(sonde_core::connect(&config, Some(Secret::new("wrong"))));
    let Err(Error::Connect(message)) = refused else {
        panic!("a wrong password should not connect");
    };
    assert!(message.contains("rejected"), "{message}");

    let session = open(&config, Some("sonde"));
    assert_eq!(scalar(&*session, "PING"), text("PONG"));
}

#[test]
fn connect_should_explain_a_closed_port() {
    let config = local(DbKind::Redis, 1, "0", "");

    let Err(Error::Connect(message)) = block_on(sonde_core::connect(&config, None)) else {
        panic!("nothing listens on port 1");
    };
    assert!(
        message.contains("nothing is accepting connections"),
        "{message}"
    );
}
