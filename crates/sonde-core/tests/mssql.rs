//! The SQL Server driver against the fixture in `tests/docker-compose.yml`.
//!
//! Each test works in a schema of its own, so they can run in parallel.

mod common;

use std::time::{Duration, Instant};

use chrono::{NaiveDate, NaiveTime, TimeZone as _, Utc};
use common::{mssql, mssql_config, run, scalar, scalar_with, setup};
use futures::{StreamExt as _, executor::block_on};
use sonde_core::{
    ConnectionConfig, Error, Secret, Session, TableRef, Value, edit, schema::RelationKind,
};

/// A fresh, empty schema named after the test.
fn fresh(name: &str) -> Box<dyn Session> {
    let session = mssql();
    let schema = format!("t_{name}");
    setup(
        &*session,
        &[
            &format!(
                "DECLARE @sql nvarchar(max) = N''; \
                 SELECT @sql += N'DROP VIEW ' + QUOTENAME(SCHEMA_NAME(schema_id)) + '.' + QUOTENAME(name) + ';' \
                   FROM sys.views WHERE schema_id = SCHEMA_ID('{schema}'); \
                 SELECT @sql += N'ALTER TABLE ' + QUOTENAME(SCHEMA_NAME(t.schema_id)) + '.' + QUOTENAME(t.name) \
                   + ' DROP CONSTRAINT ' + QUOTENAME(f.name) + ';' \
                   FROM sys.foreign_keys f JOIN sys.tables t ON t.object_id = f.parent_object_id \
                   WHERE t.schema_id = SCHEMA_ID('{schema}'); \
                 SELECT @sql += N'DROP TABLE ' + QUOTENAME(SCHEMA_NAME(schema_id)) + '.' + QUOTENAME(name) + ';' \
                   FROM sys.tables WHERE schema_id = SCHEMA_ID('{schema}'); \
                 EXEC sp_executesql @sql"
            ),
            &format!("IF SCHEMA_ID('{schema}') IS NOT NULL DROP SCHEMA {schema}"),
            &format!("CREATE SCHEMA {schema}"),
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
            "CREATE TABLE t_decode.t (
                 i INT, bi BIGINT, ti TINYINT, b BIT, f FLOAT, d DECIMAL(10,2), m MONEY,
                 s NVARCHAR(20), v VARCHAR(20), bin VARBINARY(4), dt DATETIME2(3), da DATE,
                 tm TIME(0), dto DATETIMEOFFSET(0), g UNIQUEIDENTIFIER, old DATETIME
             )",
            "INSERT INTO t_decode.t VALUES (
                 -1, 9007199254740993, 255, 1, 1.5, 10.25, 12.3456,
                 N'héllo', 'plain', 0x00ff, '2024-02-29 13:14:15.250', '1999-12-31',
                 '23:59:58', '2020-01-01 02:00:00 +02:00',
                 '6f9619ff-8b86-d011-b42d-00c04fc964ff', '2001-02-03 04:05:06'
             )",
            "INSERT INTO t_decode.t DEFAULT VALUES",
        ],
    );

    let (rows, _) = run(&*session, "SELECT * FROM t_decode.t", &[]).unwrap();

    let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    assert_eq!(
        rows[0],
        vec![
            Value::Int(-1),
            Value::Int(9_007_199_254_740_993),
            Value::Int(255),
            Value::Bool(true),
            Value::Float(1.5),
            Value::Decimal("10.25".parse().unwrap()),
            Value::Decimal("12.3456".parse().unwrap()),
            text("héllo"),
            text("plain"),
            Value::Bytes(vec![0, 255].into()),
            Value::Timestamp(date.and_hms_milli_opt(13, 14, 15, 250).unwrap()),
            Value::Date(NaiveDate::from_ymd_opt(1999, 12, 31).unwrap()),
            Value::Time(NaiveTime::from_hms_opt(23, 59, 58).unwrap()),
            Value::TimestampTz(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()),
            Value::Uuid("6f9619ff-8b86-d011-b42d-00c04fc964ff".parse().unwrap()),
            Value::Timestamp(
                NaiveDate::from_ymd_opt(2001, 2, 3)
                    .unwrap()
                    .and_hms_opt(4, 5, 6)
                    .unwrap()
            ),
        ]
    );
    assert!(rows[1].iter().all(Value::is_null));
}

#[test]
fn execute_should_report_type_names() {
    let session = fresh("types");
    setup(
        &*session,
        &["CREATE TABLE t_types.t (a INT, b DECIMAL(5,1), c NVARCHAR(3), d DATETIME2)"],
    );

    let handle = block_on(session.execute("SELECT * FROM t_types.t", &[])).unwrap();

    let names: Vec<&str> = handle.columns.iter().map(|c| &*c.type_name).collect();
    assert_eq!(names, ["int", "decimal", "nvarchar", "datetime2"]);
}

#[test]
fn execute_should_report_rows_affected_and_returned() {
    let session = fresh("affected");
    setup(
        &*session,
        &[
            "CREATE TABLE t_affected.t (a INT)",
            "INSERT INTO t_affected.t VALUES (1), (2), (3)",
        ],
    );

    let (_, update) = run(
        &*session,
        "UPDATE t_affected.t SET a = a + 1 WHERE a > 1",
        &[],
    )
    .unwrap();
    let (rows, select) = run(&*session, "SELECT a FROM t_affected.t", &[]).unwrap();

    assert_eq!(update.rows_affected, Some(2));
    assert_eq!(select.rows_affected, None);
    assert_eq!((rows.len(), select.rows_returned), (3, 3));
}

#[test]
fn execute_should_return_columns_for_an_empty_result() {
    let session = fresh("empty");
    setup(
        &*session,
        &["CREATE TABLE t_empty.t (a INT, b NVARCHAR(10))"],
    );

    let handle = block_on(session.execute("SELECT * FROM t_empty.t", &[])).unwrap();

    assert_eq!(handle.columns.len(), 2);
}

#[test]
fn execute_should_bind_parameters() {
    let session = mssql();

    let value = scalar_with(
        &*session,
        "SELECT CONCAT(@P1, '-', @P2 + 1)",
        &[text("a"), Value::Int(41)],
    );

    assert_eq!(value, text("a-42"));
}

#[test]
fn execute_should_point_at_the_failing_line() {
    let session = mssql();

    let error = run(&*session, "SELECT 1\nSELECT FROM WHERE", &[]).unwrap_err();

    let Error::Query { position, code, .. } = &error else {
        panic!("expected a query error, got {error:?}");
    };
    assert_eq!(*position, Some(9));
    assert_eq!(code.as_deref(), Some("156"));
}

#[test]
fn session_should_stay_usable_after_an_error() {
    let session = mssql();

    assert!(run(&*session, "SELECT * FROM no_such_table", &[]).is_err());

    assert_eq!(scalar(&*session, "SELECT 7"), Value::Int(7));
}

#[test]
fn cancel_should_stop_a_running_query() {
    let session = mssql();
    let canceller = session.canceller();
    let started = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });

    let outcome = run(&*session, "WAITFOR DELAY '00:00:20'", &[]);

    assert_eq!(outcome.map(|_| ()), Err(Error::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(scalar(&*session, "SELECT 1"), Value::Int(1));
}

#[test]
fn dropping_the_handle_should_free_the_session() {
    let session = mssql();
    let mut handle = block_on(session.execute(
        "SELECT TOP 3000000 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n \
         FROM sys.all_objects a CROSS JOIN sys.all_objects b",
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
fn catalog_should_list_schemas_with_dbo_first() {
    let session = fresh("catalog");
    setup(
        &*session,
        &[
            "CREATE TABLE t_catalog.book (id INT PRIMARY KEY)",
            "CREATE VIEW t_catalog.book_ids AS SELECT id FROM t_catalog.book",
        ],
    );

    let catalog = block_on(session.catalog()).unwrap();

    assert_eq!(catalog.schemas[0].name, "dbo");
    let mine = catalog
        .schemas
        .iter()
        .find(|schema| schema.name == "t_catalog")
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
            .any(|s| s.name == "sys" || s.name.starts_with("db_"))
    );
}

#[test]
fn table_detail_should_describe_columns_keys_and_indexes() {
    let session = fresh("detail");
    setup(
        &*session,
        &[
            "CREATE TABLE t_detail.author (id INT PRIMARY KEY)",
            "CREATE TABLE t_detail.book (
                 shelf INT NOT NULL, pos INT NOT NULL,
                 title NVARCHAR(50) NOT NULL CONSTRAINT df_title DEFAULT N'untitled',
                 author_id INT, n INT IDENTITY,
                 CONSTRAINT pk_book PRIMARY KEY (pos, shelf),
                 CONSTRAINT by_title UNIQUE (title),
                 CONSTRAINT written_by FOREIGN KEY (author_id) REFERENCES t_detail.author (id)
             )",
        ],
    );

    let detail = block_on(session.table_detail(&TableRef::new("t_detail", "book"))).unwrap();

    assert_eq!(detail.primary_key, ["pos", "shelf"]);
    let title = &detail.columns[2];
    assert_eq!(
        (
            title.name.as_str(),
            title.type_name.as_str(),
            title.nullable,
            title.default.as_deref()
        ),
        ("title", "nvarchar(50)", false, Some("N'untitled'"))
    );
    assert_eq!(detail.columns[4].default.as_deref(), Some("identity"));
    assert!(
        detail
            .indexes
            .iter()
            .any(|i| i.name == "by_title" && i.unique && !i.primary)
    );
    assert_eq!(detail.foreign_keys[0].name, "written_by");
    assert_eq!(
        detail.foreign_keys[0].references,
        TableRef::new("t_detail", "author")
    );
}

#[test]
fn table_detail_should_fail_for_a_missing_table() {
    let session = mssql();

    assert!(block_on(session.table_detail(&TableRef::new("dbo", "nope"))).is_err());
}

#[test]
fn apply_should_round_trip_edits_and_defaults() {
    let session = fresh("apply");
    setup(
        &*session,
        &[
            "CREATE TABLE t_apply.t (id INT IDENTITY PRIMARY KEY, name NVARCHAR(20) DEFAULT N'x', \
             amount DECIMAL(8,2), at DATETIME2(0), flag BIT)",
            "INSERT INTO t_apply.t (name, amount) VALUES (N'a', 1.00), (N'b', 2.00)",
        ],
    );
    let dialect = session.dialect();
    let table = TableRef::new("t_apply", "t");
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

    let (rows, _) = run(&*session, "SELECT * FROM t_apply.t ORDER BY id", &[]).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Int(1),
                text("it's"),
                Value::Decimal("9.99".parse().unwrap()),
                Value::Timestamp(at),
                Value::Bool(true)
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
            "CREATE TABLE t_stale.t (id INT PRIMARY KEY, v INT)",
            "INSERT INTO t_stale.t VALUES (1, 1)",
        ],
    );
    let dialect = session.dialect();
    let table = TableRef::new("t_stale", "t");

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
    assert_eq!(scalar(&*session, "SELECT v FROM t_stale.t"), Value::Int(1));
}

#[test]
fn apply_should_report_a_database_error_and_write_nothing() {
    let session = fresh("apply_error");
    setup(
        &*session,
        &[
            "CREATE TABLE t_apply_error.t (id INT PRIMARY KEY, v INT NOT NULL)",
            "INSERT INTO t_apply_error.t VALUES (1, 1)",
        ],
    );
    let dialect = session.dialect();
    let table = TableRef::new("t_apply_error", "t");

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
        scalar(&*session, "SELECT v FROM t_apply_error.t"),
        Value::Int(1)
    );
    assert_eq!(scalar(&*session, "SELECT @@TRANCOUNT"), Value::Int(0));
}

#[test]
fn edit_statements_should_render_numbered_placeholders() {
    let statement = edit::update(
        sonde_core::dialect(sonde_core::DbKind::SqlServer),
        &TableRef::new("dbo", "we@P1rd"),
        &["id"],
        &[Value::Int(1)],
        &["name"],
        &[text("a@P2b")],
    );

    assert_eq!(
        statement.sql,
        "UPDATE [dbo].[we@P1rd] SET [name] = @P1 WHERE [id] = @P2"
    );
    assert_eq!(
        statement.rendered(),
        "UPDATE [dbo].[we@P1rd] SET [name] = 'a@P2b' WHERE [id] = 1"
    );
}

fn connect_error(change: impl FnOnce(&mut sonde_core::ServerConfig), password: &str) -> String {
    let ConnectionConfig::SqlServer(mut config) = mssql_config() else {
        unreachable!("the fixture is SQL Server");
    };
    change(&mut config);
    match block_on(sonde_core::connect(
        &ConnectionConfig::SqlServer(config),
        Some(Secret::new(password)),
    )) {
        Err(Error::Connect(message)) => message,
        other => panic!("expected a connect error, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn connect_should_explain_common_failures() {
    assert!(connect_error(|_| {}, "wrong").contains("rejected the password"));
    assert!(connect_error(|c| c.port = 1, "Sonde-pass1").contains("nothing is accepting"));
    assert!(
        connect_error(|c| c.database = "no_such_db".into(), "Sonde-pass1")
            .contains("does not exist")
    );
}

#[test]
fn connect_should_work_with_tls_required_and_disabled() {
    for tls in [sonde_core::TlsMode::Require, sonde_core::TlsMode::Disable] {
        let ConnectionConfig::SqlServer(mut config) = mssql_config() else {
            unreachable!("the fixture is SQL Server");
        };
        config.tls = tls;
        let session = block_on(sonde_core::connect(
            &ConnectionConfig::SqlServer(config),
            Some(Secret::new("Sonde-pass1")),
        ))
        .unwrap_or_else(|error| panic!("{tls:?}: {error}"));
        assert_eq!(scalar(&*session, "SELECT 1"), Value::Int(1));
    }
}

#[test]
fn execute_should_report_a_killed_session_as_lost_then_reconnect() {
    let victim = mssql();
    let spid = scalar(&*victim, "SELECT @@SPID");
    let killer = mssql();
    setup(&*killer, &[&format!("KILL {spid}")]);

    let error = run(&*victim, "SELECT 1", &[]).unwrap_err();

    assert!(error.is_connection_lost(), "{error:?}");
    assert_eq!(scalar(&*victim, "SELECT 2"), Value::Int(2));
}
