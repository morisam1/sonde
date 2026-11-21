//! The MySQL driver against the fixture in `tests/docker-compose.yml`.
//!
//! Every test works in its own database, so they can run in parallel.

mod common;

use std::time::{Duration, Instant};

use chrono::{NaiveDate, NaiveTime};
use common::{mysql, mysql_config, run, scalar, setup};
use futures::{StreamExt as _, executor::block_on};
use sonde_core::{
    ConnectionConfig, Error, Secret, Session, TableRef, Value, edit, schema::RelationKind,
};

/// A fresh database named after the test, made current.
fn fresh(name: &str) -> Box<dyn Session> {
    let session = mysql();
    setup(
        &*session,
        &[
            &format!("DROP DATABASE IF EXISTS sonde_{name}"),
            &format!("CREATE DATABASE sonde_{name}"),
            &format!("USE sonde_{name}"),
        ],
    );
    session
}

fn text(value: &str) -> Value {
    Value::text(value)
}

#[test]
fn decode_should_read_every_common_type() {
    let session = fresh("decode");
    setup(
        &*session,
        &[
            "CREATE TABLE t (
                 i INT, bi BIGINT, ub BIGINT UNSIGNED, f DOUBLE, d DECIMAL(10,2),
                 s VARCHAR(20), b VARBINARY(4), dt DATETIME(3), da DATE, ti TIME,
                 j JSON, e ENUM('x','y'), bit1 BIT(1), y YEAR, tx TEXT, bl BLOB
             )",
            "INSERT INTO t VALUES (
                 -1, 9007199254740993, 18446744073709551615, 1.5, 10.25,
                 'héllo', x'00ff', '2024-02-29 13:14:15.250', '1999-12-31', '23:59:58',
                 '{\"a\": [1, 2]}', 'y', b'1', 2024, 'long text', x'0102'
             )",
            "INSERT INTO t () VALUES ()",
        ],
    );

    let (rows, _) = run(&*session, "SELECT * FROM t", &[]).unwrap();

    let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    assert_eq!(
        rows[0],
        vec![
            Value::Int(-1),
            Value::Int(9_007_199_254_740_993),
            Value::Decimal(u64::MAX.into()),
            Value::Float(1.5),
            Value::Decimal("10.25".parse().unwrap()),
            text("héllo"),
            Value::Bytes(vec![0, 255].into()),
            Value::Timestamp(date.and_hms_milli_opt(13, 14, 15, 250).unwrap()),
            Value::Date(NaiveDate::from_ymd_opt(1999, 12, 31).unwrap()),
            Value::Time(NaiveTime::from_hms_opt(23, 59, 58).unwrap()),
            // MariaDB's JSON is LONGTEXT, so results carry it as text.
            if common::mariadb() {
                text("{\"a\": [1, 2]}")
            } else {
                Value::Json("{\"a\": [1, 2]}".into())
            },
            text("y"),
            Value::Int(1),
            Value::Int(2024),
            text("long text"),
            Value::Bytes(vec![1, 2].into()),
        ]
    );
    assert!(rows[1].iter().all(Value::is_null));
}

#[test]
fn execute_should_report_mysql_type_names() {
    let session = fresh("types");
    setup(
        &*session,
        &["CREATE TABLE t (a INT UNSIGNED, b DECIMAL(5,1), c VARCHAR(3), d BLOB)"],
    );

    let handle = block_on(session.execute("SELECT * FROM t", &[])).unwrap();

    let names: Vec<&str> = handle.columns.iter().map(|c| &*c.type_name).collect();
    assert_eq!(names, ["int unsigned", "decimal", "varchar", "blob"]);
    let origin = handle.columns[0].origin.as_ref().unwrap();
    assert_eq!((&*origin.table, &*origin.column), ("t", "a"));
}

#[test]
fn execute_should_report_rows_affected_and_returned() {
    let session = fresh("affected");
    setup(
        &*session,
        &[
            "CREATE TABLE t (a INT)",
            "INSERT INTO t VALUES (1), (2), (3)",
        ],
    );

    let (_, update) = run(&*session, "UPDATE t SET a = a + 1 WHERE a > 1", &[]).unwrap();
    let (rows, select) = run(&*session, "SELECT a FROM t", &[]).unwrap();

    assert_eq!(update.rows_affected, Some(2));
    assert_eq!(select.rows_affected, None);
    assert_eq!((rows.len(), select.rows_returned), (3, 3));
}

#[test]
fn execute_should_return_columns_for_an_empty_result() {
    let session = fresh("empty");
    setup(&*session, &["CREATE TABLE t (a INT, b TEXT)"]);

    let (rows, _) = run(&*session, "SELECT * FROM t", &[]).unwrap();
    let handle = block_on(session.execute("SELECT * FROM t", &[])).unwrap();

    assert!(rows.is_empty());
    assert_eq!(handle.columns.len(), 2);
}

#[test]
fn execute_should_bind_parameters() {
    let session = mysql();

    let value = scalar_with(
        &*session,
        "SELECT CONCAT(?, '-', ? + 1)",
        &[text("a"), Value::Int(41)],
    );

    assert_eq!(value, text("a-42"));
}

fn scalar_with(session: &dyn Session, sql: &str, params: &[Value]) -> Value {
    common::scalar_with(session, sql, params)
}

#[test]
fn execute_should_point_at_a_syntax_error() {
    let session = mysql();

    let error = run(&*session, "SELECT * FORM dual", &[]).unwrap_err();

    let Error::Query { position, code, .. } = &error else {
        panic!("expected a query error, got {error:?}");
    };
    assert_eq!(*position, Some(9));
    assert!(code.as_deref().is_some_and(|c| c.starts_with("1064")));
}

#[test]
fn session_should_stay_usable_after_an_error() {
    let session = mysql();

    assert!(run(&*session, "SELECT * FROM no_such_table", &[]).is_err());

    assert_eq!(scalar(&*session, "SELECT 7"), Value::Int(7));
}

#[test]
fn dropping_the_handle_should_free_the_session() {
    let session = mysql();
    setup(
        &*session,
        &[if common::mariadb() {
            "SET SESSION max_recursive_iterations = 10000000"
        } else {
            "SET SESSION cte_max_recursion_depth = 10000000"
        }],
    );
    let mut handle = block_on(session.execute(
        "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 5000000) \
         SELECT n FROM s",
        &[],
    ))
    .unwrap();
    let first = block_on(handle.pages.next()).unwrap().unwrap();
    assert_eq!(first.rows[0], vec![Value::Int(1)]);

    let started = Instant::now();
    drop(handle);
    let next = scalar(&*session, "SELECT 'free'");

    assert_eq!(next, text("free"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn cancel_should_stop_a_running_query() {
    let session = mysql();
    let canceller = session.canceller();
    let started = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });

    let outcome = run(
        &*session,
        "SELECT COUNT(*) FROM information_schema.columns a, information_schema.columns b, \
         information_schema.columns c",
        &[],
    );

    assert_eq!(outcome.map(|_| ()), Err(Error::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(scalar(&*session, "SELECT 1"), Value::Int(1));
}

#[test]
fn catalog_should_list_databases_with_the_current_one_first() {
    let session = fresh("catalog");
    setup(
        &*session,
        &[
            "CREATE TABLE book (id INT PRIMARY KEY)",
            "CREATE VIEW book_ids AS SELECT id FROM book",
        ],
    );

    let catalog = block_on(session.catalog()).unwrap();

    assert_eq!(catalog.schemas[0].name, "sonde");
    let mine = catalog
        .schemas
        .iter()
        .find(|schema| schema.name == "sonde_catalog")
        .unwrap();
    let relations: Vec<(&str, RelationKind)> = mine
        .relations
        .iter()
        .map(|r| (r.name.as_str(), r.kind))
        .collect();
    assert_eq!(
        relations,
        [
            ("book", RelationKind::Table),
            ("book_ids", RelationKind::View)
        ]
    );
    assert!(
        !catalog
            .schemas
            .iter()
            .any(|s| s.name == "mysql" || s.name == "sys")
    );
}

#[test]
fn table_detail_should_describe_columns_keys_and_indexes() {
    let session = fresh("detail");
    setup(
        &*session,
        &[
            "CREATE TABLE author (id INT PRIMARY KEY)",
            "CREATE TABLE book (
                 shelf INT, pos INT, title VARCHAR(50) NOT NULL DEFAULT 'untitled',
                 author_id INT, PRIMARY KEY (pos, shelf),
                 UNIQUE KEY by_title (title),
                 CONSTRAINT written_by FOREIGN KEY (author_id) REFERENCES author (id)
             )",
        ],
    );

    let detail = block_on(session.table_detail(&TableRef::new("sonde_detail", "book"))).unwrap();

    assert_eq!(detail.primary_key, ["pos", "shelf"]);
    let title = &detail.columns[2];
    assert_eq!(
        (
            title.name.as_str(),
            title.type_name.as_str(),
            title.nullable,
            title.default.as_deref()
        ),
        ("title", "varchar(50)", false, Some("untitled"))
    );
    assert!(
        detail
            .indexes
            .iter()
            .any(|i| i.name == "by_title" && i.unique && !i.primary)
    );
    assert_eq!(detail.foreign_keys[0].name, "written_by");
    assert_eq!(
        detail.foreign_keys[0].references,
        TableRef::new("sonde_detail", "author")
    );
}

#[test]
fn table_detail_should_fail_for_a_missing_table() {
    let session = mysql();

    assert!(block_on(session.table_detail(&TableRef::new("sonde", "nope"))).is_err());
}

#[test]
fn apply_should_round_trip_edits_and_defaults() {
    let session = fresh("apply");
    setup(
        &*session,
        &[
            "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20) DEFAULT 'x', \
             amount DECIMAL(8,2), at DATETIME, flag TINYINT(1))",
            "INSERT INTO t (name, amount) VALUES ('a', 1.00), ('b', 2.00)",
        ],
    );
    let dialect = session.dialect();
    let table = TableRef::new("sonde_apply", "t");
    let at = NaiveDate::from_ymd_opt(2020, 5, 6)
        .unwrap()
        .and_hms_opt(7, 8, 9)
        .unwrap();

    block_on(session.apply(&[
        edit::update(
            dialect,
            &table,
            &["id"],
            &[Value::Int(1)],
            &["name", "amount", "at", "flag"],
            &[
                text("it's"),
                Value::Decimal("9.99".parse().unwrap()),
                Value::Timestamp(at),
                Value::Bool(true),
            ],
        ),
        // Writing the value a row already has still counts as one row.
        edit::update(
            dialect,
            &table,
            &["id"],
            &[Value::Int(2)],
            &["name"],
            &[text("b")],
        ),
        edit::delete(dialect, &table, &["id"], &[Value::Int(2)]),
        edit::insert(dialect, &table, &[], &[]),
    ]))
    .unwrap();

    let (rows, _) = run(&*session, "SELECT * FROM t ORDER BY id", &[]).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Int(1),
                text("it's"),
                Value::Decimal("9.99".parse().unwrap()),
                Value::Timestamp(at),
                Value::Int(1)
            ],
            vec![
                Value::Int(3),
                text("x"),
                Value::Null,
                Value::Null,
                Value::Null
            ],
        ]
    );
}

#[test]
fn apply_should_roll_back_when_a_row_is_gone() {
    let session = fresh("stale");
    setup(
        &*session,
        &[
            "CREATE TABLE t (id INT PRIMARY KEY, v INT)",
            "INSERT INTO t VALUES (1, 1)",
        ],
    );
    let dialect = session.dialect();
    let table = TableRef::new("sonde_stale", "t");

    let outcome = block_on(session.apply(&[
        edit::update(
            dialect,
            &table,
            &["id"],
            &[Value::Int(1)],
            &["v"],
            &[Value::Int(5)],
        ),
        edit::delete(dialect, &table, &["id"], &[Value::Int(99)]),
    ]));

    assert_eq!(outcome, Err(Error::Stale));
    assert_eq!(
        scalar(&*session, "SELECT v FROM sonde_stale.t"),
        Value::Int(1)
    );
}

#[test]
fn apply_should_report_a_database_error_and_write_nothing() {
    let session = fresh("apply_error");
    setup(
        &*session,
        &[
            "CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)",
            "INSERT INTO t VALUES (1, 1)",
        ],
    );
    let dialect = session.dialect();
    let table = TableRef::new("sonde_apply_error", "t");

    let outcome = block_on(session.apply(&[
        edit::update(
            dialect,
            &table,
            &["id"],
            &[Value::Int(1)],
            &["v"],
            &[Value::Int(5)],
        ),
        edit::insert(
            dialect,
            &table,
            &["id", "v"],
            &[Value::Int(1), Value::Int(2)],
        ),
    ]));

    assert!(matches!(outcome, Err(Error::Query { .. })), "{outcome:?}");
    assert_eq!(
        scalar(&*session, "SELECT v FROM sonde_apply_error.t"),
        Value::Int(1)
    );
}

#[test]
fn edit_statements_should_render_with_bare_placeholders() {
    let statement = edit::update(
        sonde_core::dialect(sonde_core::DbKind::Mysql),
        &TableRef::new("db", "we?ird"),
        &["id"],
        &[Value::Int(1)],
        &["name"],
        &[text("a?b")],
    );

    assert_eq!(
        statement.sql,
        "UPDATE `db`.`we?ird` SET `name` = ? WHERE `id` = ?"
    );
    assert_eq!(
        statement.rendered(),
        "UPDATE `db`.`we?ird` SET `name` = 'a?b' WHERE `id` = 1"
    );
}

fn connect_error(
    change: impl FnOnce(&mut sonde_core::ServerConfig),
    password: Option<&str>,
) -> String {
    let config = mysql_config();
    let mut server = config.server().unwrap().clone();
    change(&mut server);
    match block_on(sonde_core::connect(
        &ConnectionConfig::for_server(config.kind(), server).unwrap(),
        password.map(Secret::new),
    )) {
        Err(Error::Connect(message)) => message,
        other => panic!("expected a connect error, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn connect_should_explain_common_failures() {
    assert!(connect_error(|_| {}, Some("wrong")).contains("rejected the password"));
    assert!(connect_error(|c| c.port = 1, Some("sonde")).contains("nothing is accepting"));
    assert!(
        connect_error(|c| c.database = "no_such_db".into(), Some("sonde"))
            .contains("may not open the database")
            || connect_error(|c| c.database = "no_such_db".into(), Some("sonde"))
                .contains("does not exist")
    );
}

#[test]
fn connect_should_work_with_tls_required_and_disabled() {
    for tls in [sonde_core::TlsMode::Require, sonde_core::TlsMode::Disable] {
        let config = mysql_config();
        let mut server = config.server().unwrap().clone();
        server.tls = tls;
        let session = block_on(sonde_core::connect(
            &ConnectionConfig::for_server(config.kind(), server).unwrap(),
            Some(Secret::new("sonde")),
        ))
        .unwrap_or_else(|error| panic!("{tls:?}: {error}"));
        assert_eq!(scalar(&*session, "SELECT 1"), Value::Int(1));
    }
}

#[test]
fn execute_should_report_a_killed_session_as_lost() {
    let victim = mysql();
    let id = scalar(&*victim, "SELECT CONNECTION_ID()");
    let killer = mysql();
    setup(&*killer, &[&format!("KILL {id}")]);

    let error = run(&*victim, "SELECT 1", &[]).unwrap_err();

    assert!(error.is_connection_lost(), "{error:?}");
}
