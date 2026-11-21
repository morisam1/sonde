//! Integration tests against the Postgres fixture
//! (`docker compose -f tests/docker-compose.yml up -d`).

mod common;

use std::{sync::Arc, time::Duration};

use chrono::{NaiveDate, NaiveTime, TimeZone as _, Utc};
use common::{postgres, postgres_config, run, scalar, scalar_with, setup};
use futures::{StreamExt as _, executor::block_on};
use sonde_core::{
    ConnectionConfig, Error, Secret, TableRef, TlsMode, Value,
    schema::{RelationKind, TableDetail},
};

fn text(value: &str) -> Value {
    Value::text(value)
}

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("valid test date")
}

/// One test per type: the literal must decode to the expected value.
macro_rules! decodes {
    ($($name:ident: $sql:expr => $expected:expr;)*) => {$(
        #[test]
        fn $name() {
            assert_eq!(scalar(&*postgres(), concat!("select ", $sql)), $expected);
        }
    )*};
}

decodes! {
    decode_should_read_bool: "true" => Value::Bool(true);
    decode_should_read_int2: "-7::int2" => Value::Int(-7);
    decode_should_read_int4: "2147483647::int4" => Value::Int(2_147_483_647);
    decode_should_read_int8: "(-9223372036854775808)::int8" => Value::Int(i64::MIN);
    decode_should_read_float4: "1.5::float4" => Value::Float(1.5);
    decode_should_read_float8: "-2.25::float8" => Value::Float(-2.25);
    decode_should_read_numeric: "12345.6700::numeric" => Value::Decimal("12345.6700".parse().unwrap());
    decode_should_read_small_numeric: "0.000012::numeric" => Value::Decimal("0.000012".parse().unwrap());
    decode_should_read_negative_numeric: "-100000000::numeric" => Value::Decimal("-100000000".parse().unwrap());
    decode_should_read_text: "'héllo'::text" => text("héllo");
    decode_should_read_varchar: "'abc'::varchar(10)" => text("abc");
    decode_should_read_padded_char: "'ab'::char(4)" => text("ab  ");
    decode_should_read_name: "'pg_class'::name" => text("pg_class");
    decode_should_read_bytea: "'\\xdeadbeef'::bytea" => Value::Bytes(Arc::from([0xde, 0xad, 0xbe, 0xef]));
    decode_should_read_date: "'2024-02-29'::date" => Value::Date(date(2024, 2, 29));
    decode_should_read_date_before_epoch: "'1969-07-20'::date" => Value::Date(date(1969, 7, 20));
    decode_should_read_time: "'13:14:15.250'::time" =>
        Value::Time(NaiveTime::from_hms_milli_opt(13, 14, 15, 250).unwrap());
    decode_should_read_timestamp: "'2024-02-29 13:14:15'::timestamp" =>
        Value::Timestamp(date(2024, 2, 29).and_hms_opt(13, 14, 15).unwrap());
    decode_should_read_timestamptz: "'2024-02-29 13:14:15+02'::timestamptz" =>
        Value::TimestampTz(Utc.with_ymd_and_hms(2024, 2, 29, 11, 14, 15).unwrap());
    decode_should_read_interval: "'1 year 2 mons 3 days 04:05:06'::interval" =>
        Value::Interval { months: 14, days: 3, micros: 14_706_000_000 };
    decode_should_read_uuid: "'6f1c9e0a-58a5-4c7e-9a43-0c3f1d2b4e5f'::uuid" =>
        Value::Uuid("6f1c9e0a-58a5-4c7e-9a43-0c3f1d2b4e5f".parse().unwrap());
    decode_should_read_json: "'{\"a\": 1}'::json" => Value::Json("{\"a\": 1}".into());
    decode_should_read_jsonb: "'{\"a\":1}'::jsonb" => Value::Json("{\"a\": 1}".into());
    decode_should_read_null: "null::int4" => Value::Null;
    decode_should_read_int_array: "array[1, 2, 3]" =>
        Value::Array(Arc::from([Value::Int(1), Value::Int(2), Value::Int(3)]));
    decode_should_read_text_array_with_null: "array['a', null]::text[]" =>
        Value::Array(Arc::from([text("a"), Value::Null]));
    decode_should_read_empty_array: "'{}'::int4[]" => Value::Array(Arc::from([]));
    decode_should_read_nested_array: "array[[1, 2], [3, 4]]" => Value::Array(Arc::from([
        Value::Array(Arc::from([Value::Int(1), Value::Int(2)])),
        Value::Array(Arc::from([Value::Int(3), Value::Int(4)])),
    ]));
}

/// Types outside the decoder list must still show something sensible.
macro_rules! displays {
    ($($name:ident: $sql:expr => $expected:expr;)*) => {$(
        #[test]
        fn $name() {
            assert_eq!(scalar(&*postgres(), concat!("select ", $sql)).to_string(), $expected);
        }
    )*};
}

displays! {
    display_should_keep_numeric_nan: "'NaN'::numeric" => "NaN";
    display_should_keep_numeric_beyond_decimal: "(10::numeric ^ 40) + 0.5" =>
        "10000000000000000000000000000000000000000.5";
    display_should_keep_infinite_timestamp: "'infinity'::timestamp" => "infinity";
    display_should_keep_infinite_date: "'-infinity'::date" => "-infinity";
}

#[test]
fn decode_should_read_enum_as_text() {
    let session = postgres();
    setup(
        &*session,
        &[
            "drop type if exists sonde_mood",
            "create type sonde_mood as enum ('sad', 'happy')",
        ],
    );

    let value = scalar(&*session, "select 'happy'::sonde_mood");

    setup(&*session, &["drop type sonde_mood"]);
    assert_eq!(value, text("happy"));
}

#[test]
fn decode_should_read_domain_as_its_base_type() {
    let session = postgres();
    setup(
        &*session,
        &[
            "drop domain if exists sonde_positive",
            "create domain sonde_positive as int4 check (value > 0)",
        ],
    );

    let value = scalar(&*session, "select 5::sonde_positive");

    setup(&*session, &["drop domain sonde_positive"]);
    assert_eq!(value, Value::Int(5));
}

#[test]
fn decode_should_keep_undecoded_types_as_unknown() {
    let value = scalar(&*postgres(), "select point(1, 2)");

    assert!(matches!(value, Value::Unknown { type_name, .. } if &*type_name == "point"));
}

#[test]
fn execute_should_return_columns_for_an_empty_result() {
    let session = postgres();

    let handle = block_on(session.execute("select 1 as a, 'x' as b where false", &[])).unwrap();

    let names: Vec<&str> = handle.columns.iter().map(|c| &*c.name).collect();
    assert_eq!(names, ["a", "b"]);
}

#[test]
fn execute_should_return_no_rows_for_an_empty_result() {
    let (rows, _) = run(&*postgres(), "select 1 where false", &[]).unwrap();

    assert!(rows.is_empty());
}

#[test]
fn execute_should_report_column_type_names() {
    let session = postgres();

    let handle = block_on(session.execute("select 1::int8, now()", &[])).unwrap();

    let types: Vec<&str> = handle.columns.iter().map(|c| &*c.type_name).collect();
    assert_eq!(types, ["int8", "timestamptz"]);
}

#[test]
fn execute_should_report_rows_affected() {
    let session = postgres();
    setup(&*session, &["create temp table affected (n int)"]);

    let (_, summary) = run(
        &*session,
        "insert into affected select generate_series(1, 42)",
        &[],
    )
    .unwrap();

    assert_eq!(summary.rows_affected, Some(42));
}

#[test]
fn execute_should_not_report_rows_affected_for_a_select() {
    let (_, summary) = run(&*postgres(), "select 1", &[]).unwrap();

    assert_eq!(summary.rows_affected, None);
}

#[test]
fn execute_should_count_rows_returned() {
    let (_, summary) = run(&*postgres(), "select generate_series(1, 2500)", &[]).unwrap();

    assert_eq!(summary.rows_returned, 2500);
}

#[test]
fn execute_should_collect_notices() {
    let session = postgres();

    let (_, summary) = run(
        &*session,
        "do $$ begin raise notice 'hello from sonde'; end $$",
        &[],
    )
    .unwrap();

    assert_eq!(summary.notices, ["NOTICE: hello from sonde"]);
}

#[test]
fn execute_should_report_syntax_error_position() {
    let sql = "select 'é' + from pg_class";

    let error = run(&*postgres(), sql, &[]).unwrap_err();

    assert_eq!(error.position().map(|at| &sql[at..]), Some("from pg_class"));
}

#[test]
fn execute_should_report_sqlstate() {
    let error = run(&*postgres(), "select * from sonde_no_such_table", &[]).unwrap_err();

    assert!(matches!(error, Error::Query { code: Some(code), .. } if code == "42P01"));
}

#[test]
fn execute_should_report_errors_raised_while_streaming() {
    let error = run(
        &*postgres(),
        "select 1 / (5000 - g) from generate_series(1, 10000) g",
        &[],
    )
    .unwrap_err();

    assert!(matches!(error, Error::Query { code: Some(code), .. } if code == "22012"));
}

#[test]
fn session_should_stay_usable_after_an_error() {
    let session = postgres();
    let _ = run(&*session, "select nonsense(", &[]);

    assert_eq!(scalar(&*session, "select 7"), Value::Int(7));
}

#[test]
fn cancel_should_stop_a_running_query() {
    let session = postgres();
    let canceller = session.canceller();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });

    let error = run(&*session, "select pg_sleep(30)", &[]).unwrap_err();

    assert_eq!(error, Error::Cancelled);
}

#[test]
fn cancel_should_return_control_within_a_second() {
    let session = postgres();
    let canceller = session.canceller();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });
    let started = std::time::Instant::now();

    let _ = run(&*session, "select pg_sleep(30)", &[]);

    assert!(started.elapsed() < Duration::from_millis(1300));
}

#[test]
fn dropping_the_handle_should_free_the_session() {
    let session = postgres();
    let mut handle = block_on(session.execute(
        "select g, repeat('x', 200) from generate_series(1, 5000000) g",
        &[],
    ))
    .unwrap();
    let _ = block_on(handle.pages.next());

    drop(handle);

    assert_eq!(scalar(&*session, "select 7"), Value::Int(7));
}

#[test]
fn abandoned_statement_should_not_cancel_the_next_one() {
    let session = postgres();
    for _ in 0..20 {
        let mut handle =
            block_on(session.execute("select g from generate_series(1, 200000) g", &[])).unwrap();
        let _ = block_on(handle.pages.next());
        drop(handle);

        // Long enough for a stray cancel to arrive while it runs.
        let next = run(&*session, "select pg_sleep(0.05), 7", &[]);

        assert!(
            next.is_ok(),
            "the following statement was cancelled: {next:?}"
        );
    }
}

/// One test per type: a value sent as a parameter must come back equal.
macro_rules! round_trips {
    ($($name:ident: $cast:literal, $value:expr;)*) => {$(
        #[test]
        fn $name() {
            let value: Value = $value;

            let back = scalar_with(
                &*postgres(),
                concat!("select $1::", $cast),
                std::slice::from_ref(&value),
            );

            assert_eq!(back, value);
        }
    )*};
}

round_trips! {
    param_should_round_trip_bool: "bool", Value::Bool(true);
    param_should_round_trip_int2: "int2", Value::Int(-12);
    param_should_round_trip_int4: "int4", Value::Int(123_456);
    param_should_round_trip_int8: "int8", Value::Int(i64::MAX);
    param_should_round_trip_float8: "float8", Value::Float(0.1);
    param_should_round_trip_numeric: "numeric", Value::Decimal("-98765.4321000".parse().unwrap());
    param_should_round_trip_tiny_numeric: "numeric", Value::Decimal("0.00000001".parse().unwrap());
    param_should_round_trip_text: "text", text("it's \"quoted\"; -- ok");
    param_should_round_trip_bytea: "bytea", Value::Bytes(Arc::from([0, 1, 255]));
    param_should_round_trip_date: "date", Value::Date(date(1999, 12, 31));
    param_should_round_trip_time: "time", Value::Time(NaiveTime::from_hms_micro_opt(1, 2, 3, 4).unwrap());
    param_should_round_trip_timestamp: "timestamp",
        Value::Timestamp(date(2030, 1, 2).and_hms_micro_opt(3, 4, 5, 6).unwrap());
    param_should_round_trip_timestamptz: "timestamptz",
        Value::TimestampTz(Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap());
    param_should_round_trip_interval: "interval", Value::Interval { months: 1, days: -2, micros: 3 };
    param_should_round_trip_uuid: "uuid", Value::Uuid("6f1c9e0a-58a5-4c7e-9a43-0c3f1d2b4e5f".parse().unwrap());
    param_should_round_trip_jsonb: "jsonb", Value::Json("{\"k\": [1, 2]}".into());
    param_should_round_trip_int_array: "int4[]", Value::Array(Arc::from([Value::Int(1), Value::Null]));
    param_should_round_trip_null: "int4", Value::Null;
}

#[test]
fn param_should_be_refused_when_its_type_does_not_fit() {
    let error = run(&*postgres(), "select $1::int4", &[text("nope")]).unwrap_err();

    assert_eq!(
        error,
        Error::Parameter {
            value: "text",
            expected: "int4".into(),
        }
    );
}

#[test]
fn connect_should_fail_with_a_wrong_password() {
    let result = block_on(sonde_core::connect(
        &postgres_config(),
        Some(Secret::new("wrong")),
    ));

    let Err(Error::Connect(message)) = result else {
        panic!("expected a connect error");
    };
    assert!(message.contains("rejected the password"), "{message}");
}

#[test]
fn connect_should_explain_a_missing_password() {
    let result = block_on(sonde_core::connect(&postgres_config(), None));

    let Err(Error::Connect(message)) = result else {
        panic!("expected a connect error");
    };
    assert!(message.contains("none was given"), "{message}");
}

#[test]
fn connect_should_explain_a_closed_port() {
    let ConnectionConfig::Postgres(mut config) = postgres_config() else {
        unreachable!("the fixture is Postgres");
    };
    config.port = 1;

    let result = block_on(sonde_core::connect(
        &ConnectionConfig::Postgres(config),
        Some(Secret::new("sonde")),
    ));

    let Err(Error::Connect(message)) = result else {
        panic!("expected a connect error");
    };
    assert!(
        message.contains("nothing is accepting connections at 127.0.0.1:1"),
        "{message}"
    );
}

#[test]
fn connect_should_explain_a_missing_database() {
    let ConnectionConfig::Postgres(mut config) = postgres_config() else {
        unreachable!("the fixture is Postgres");
    };
    config.database = "no_such_db".into();

    let result = block_on(sonde_core::connect(
        &ConnectionConfig::Postgres(config),
        Some(Secret::new("sonde")),
    ));

    let Err(Error::Connect(message)) = result else {
        panic!("expected a connect error");
    };
    assert!(
        message.contains("\"no_such_db\" does not exist"),
        "{message}"
    );
}

#[test]
fn execute_should_report_a_terminated_session_as_lost() {
    let victim = postgres();
    let pid = scalar(&*victim, "select pg_backend_pid()");
    let killer = postgres();
    let _ = scalar(&*killer, &format!("select pg_terminate_backend({pid})"));

    let mut lost = false;
    for _ in 0..20 {
        match block_on(victim.execute("select 1", &[])) {
            Err(error) if error.is_connection_lost() => {
                lost = true;
                break;
            }
            Err(error) => panic!("unexpected error {error:?}"),
            Ok(handle) => {
                // Drain, then try again once the socket closes.
                let pages: Vec<_> = block_on(handle.pages.collect());
                if pages
                    .iter()
                    .any(|page| matches!(page, Err(e) if e.is_connection_lost()))
                {
                    lost = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    assert!(
        lost,
        "the terminated session should report a lost connection"
    );
}

#[test]
fn connect_should_fail_when_tls_is_required_but_not_offered() {
    let ConnectionConfig::Postgres(mut config) = postgres_config() else {
        unreachable!("the fixture is Postgres");
    };
    config.tls = TlsMode::Require;

    let result = block_on(sonde_core::connect(
        &ConnectionConfig::Postgres(config),
        Some(Secret::new("sonde")),
    ));

    assert!(matches!(result, Err(Error::Connect(_))));
}

/// A schema with two related tables, a view and an expression index.
fn catalog_fixture(schema: &str) -> Box<dyn sonde_core::Session> {
    let session = postgres();
    let statements = [
        format!("drop schema if exists {schema} cascade"),
        format!("create schema {schema}"),
        format!(
            "create table {schema}.author (
                 id int4 generated always as identity primary key,
                 name text not null,
                 email varchar(120) unique)"
        ),
        format!(
            "create table {schema}.book (
                 author_id int4 not null references {schema}.author (id),
                 isbn text not null,
                 title text not null default 'untitled',
                 price numeric(10, 2),
                 primary key (author_id, isbn))"
        ),
        format!("create index book_title_lower on {schema}.book (lower(title), price)"),
        format!("create view {schema}.titles as select title from {schema}.book"),
    ];
    let statements: Vec<&str> = statements.iter().map(String::as_str).collect();
    setup(&*session, &statements);
    session
}

fn book_detail(schema: &str) -> TableDetail {
    let session = catalog_fixture(schema);
    block_on(session.table_detail(&TableRef::new(schema, "book"))).unwrap()
}

#[test]
fn catalog_should_list_tables_and_views_of_a_schema() {
    let session = catalog_fixture("sonde_cat_list");

    let catalog = block_on(session.catalog()).unwrap();

    let relations: Vec<(&str, RelationKind)> = catalog
        .schemas
        .iter()
        .find(|schema| schema.name == "sonde_cat_list")
        .map(|schema| {
            schema
                .relations
                .iter()
                .map(|r| (r.name.as_str(), r.kind))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        relations,
        [
            ("author", RelationKind::Table),
            ("book", RelationKind::Table),
            ("titles", RelationKind::View),
        ]
    );
}

#[test]
fn catalog_should_hide_system_schemas() {
    let catalog = block_on(postgres().catalog()).unwrap();

    assert!(
        catalog
            .schemas
            .iter()
            .all(|s| s.name != "pg_catalog" && s.name != "information_schema")
    );
}

#[test]
fn table_detail_should_describe_columns() {
    let detail = book_detail("sonde_cat_columns");

    let columns: Vec<(&str, &str, bool, Option<&str>)> = detail
        .columns
        .iter()
        .map(|c| (&*c.name, &*c.type_name, c.nullable, c.default.as_deref()))
        .collect();
    assert_eq!(
        columns,
        [
            ("author_id", "integer", false, None),
            ("isbn", "text", false, None),
            ("title", "text", false, Some("'untitled'::text")),
            ("price", "numeric(10,2)", true, None),
        ]
    );
}

#[test]
fn table_detail_should_report_a_composite_primary_key_in_order() {
    let detail = book_detail("sonde_cat_pk");

    assert_eq!(detail.primary_key, ["author_id", "isbn"]);
}

#[test]
fn table_detail_should_report_foreign_keys() {
    let detail = book_detail("sonde_cat_fk");

    let keys: Vec<(&[String], &TableRef, &[String])> = detail
        .foreign_keys
        .iter()
        .map(|k| (&*k.columns, &k.references, &*k.referenced_columns))
        .collect();
    assert_eq!(
        keys,
        [(
            &["author_id".to_owned()][..],
            &TableRef::new("sonde_cat_fk", "author"),
            &["id".to_owned()][..]
        )]
    );
}

#[test]
fn table_detail_should_report_indexes_with_expression_entries() {
    let detail = book_detail("sonde_cat_ix");

    let indexes: Vec<(&str, Vec<Option<&str>>, bool, bool)> = detail
        .indexes
        .iter()
        .map(|i| {
            let columns = i.columns.iter().map(Option::as_deref).collect();
            (&*i.name, columns, i.unique, i.primary)
        })
        .collect();
    assert_eq!(
        indexes,
        [
            (
                "book_pkey",
                vec![Some("author_id"), Some("isbn")],
                true,
                true
            ),
            ("book_title_lower", vec![None, Some("price")], false, false),
        ]
    );
}

#[test]
fn table_detail_should_fail_for_a_missing_table() {
    let result = block_on(postgres().table_detail(&TableRef::new("public", "sonde_missing")));

    assert!(matches!(result, Err(Error::Query { .. })));
}

/// The full M4 round trip: read a row, parse typed text for every column the
/// editor would, apply the change keyed on the original key, read it back.
#[test]
fn apply_should_round_trip_every_type_as_staged_edits() {
    let session = postgres();
    setup(
        &*session,
        &[
            "drop table if exists sonde_edit_types",
            "create table sonde_edit_types (
                id int4 primary key,
                flag bool, i2 int2, i4 int4, i8 int8, f4 float4, f8 float8,
                num numeric(12, 3), txt text, ch varchar(9), bin bytea,
                d date, t time, ts timestamp, tstz timestamptz, iv interval,
                uid uuid, js json, jsb jsonb, tags int4[])",
            "insert into sonde_edit_types values (
                1, true, -7, 42, -9223372036854775808, 1.5, -2.25,
                12345.670, 'héllo', 'abc', '\\xdeadbeef',
                '2024-02-29', '13:14:15.250', '2024-02-29 13:14:15', '2024-02-29 13:14:15+02',
                '1 year 2 mons 3 days 04:05:06',
                '6f1c9e0a-58a5-4c7e-9a43-0c3f1d2b4e5f', '{\"a\": 1}', '{\"a\": [1, 2]}', '{1, 2, 3}')",
        ],
    );

    // What the editor produces: one parsed value per column.
    let edited: [(&str, &str, &str); 19] = [
        ("flag", "bool", "false"),
        ("i2", "int2", "8"),
        ("i4", "int4", "-42"),
        ("i8", "int8", "9223372036854775807"),
        ("f4", "float4", "-1.25"),
        ("f8", "float8", "3.5"),
        ("num", "numeric(12,3)", "0.001"),
        ("txt", "text", "edited 'quote'"),
        ("ch", "varchar(9)", "xyz"),
        ("bin", "bytea", "\\xcafe"),
        ("d", "date", "1969-07-20"),
        ("t", "time", "01:02:03"),
        ("ts", "timestamp", "1999-12-31 23:59:59"),
        ("tstz", "timestamptz", "2000-01-01 00:00:00"),
        ("iv", "interval", "2 days 05:00:00"),
        ("uid", "uuid", "01234567-89ab-4cde-9f01-23456789abcd"),
        ("js", "json", "{\"b\": 2}"),
        ("jsb", "jsonb", "{\"b\": [3]}"),
        ("tags", "int4[]", "{4, 5}"),
    ];
    let columns: Vec<&str> = edited.iter().map(|(column, _, _)| *column).collect();
    let values: Vec<Value> = edited
        .iter()
        .map(|(_, type_name, text)| {
            sonde_core::value::parse(type_name, text).expect("the test value parses")
        })
        .collect();

    let table = TableRef::unqualified("sonde_edit_types");
    let update = sonde_core::edit::update(
        session.dialect(),
        &table,
        &["id"],
        &[Value::Int(1)],
        &columns,
        &values,
    );
    block_on(session.apply(&[update])).expect("the update applies");

    let (rows, _) = run(
        &*session,
        "select flag, i2, i4, i8, f4, f8, num, txt, ch, bin, d, t, ts, tstz, iv, uid, js, jsb, tags
             from sonde_edit_types",
        &[],
    )
    .expect("the row reads back");
    let row = rows.into_iter().next().expect("one row");
    assert_eq!(row[0], Value::Bool(false));
    assert_eq!(row[1], Value::Int(8));
    assert_eq!(row[2], Value::Int(-42));
    assert_eq!(row[3], Value::Int(i64::MAX));
    assert_eq!(row[4], Value::Float(-1.25));
    assert_eq!(row[5], Value::Float(3.5));
    assert_eq!(row[6], Value::Decimal("0.001".parse().unwrap()));
    assert_eq!(row[7], Value::text("edited 'quote'"));
    assert_eq!(row[8], Value::text("xyz"));
    assert_eq!(row[9], Value::Bytes(Arc::from([0xca, 0xfe])));
    assert_eq!(row[10], Value::Date(date(1969, 7, 20)));
    assert_eq!(
        row[11],
        Value::Time(NaiveTime::from_hms_opt(1, 2, 3).unwrap())
    );
    assert_eq!(
        row[12],
        Value::Timestamp(date(1999, 12, 31).and_hms_opt(23, 59, 59).unwrap())
    );
    assert_eq!(
        row[13],
        Value::TimestampTz(Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap())
    );
    assert_eq!(
        row[14],
        Value::Interval {
            months: 0,
            days: 2,
            micros: 18_000_000_000
        }
    );
    assert_eq!(
        row[15],
        Value::Uuid("01234567-89ab-4cde-9f01-23456789abcd".parse().unwrap())
    );
    assert_eq!(row[16], Value::Json("{\"b\": 2}".into()));
    assert_eq!(row[17], Value::Json("{\"b\": [3]}".into()));
    assert_eq!(
        row[18],
        Value::Array(Arc::from([Value::Int(4), Value::Int(5)]))
    );

    // A change set may mix statements: insert the successor, delete the
    // original, all or nothing.
    let insert = sonde_core::edit::insert(
        session.dialect(),
        &table,
        &["id", "flag"],
        &[Value::Int(2), Value::Bool(true)],
    );
    let delete = sonde_core::edit::delete(session.dialect(), &table, &["id"], &[Value::Int(1)]);
    block_on(session.apply(&[insert, delete])).expect("the insert and delete apply");

    assert_eq!(
        scalar(&*session, "select count(*) from sonde_edit_types"),
        Value::Int(1)
    );
    assert_eq!(
        scalar(&*session, "select flag from sonde_edit_types"),
        Value::Bool(true)
    );
    setup(&*session, &["drop table sonde_edit_types"]);
}

#[test]
fn apply_should_roll_back_everything_when_a_key_no_longer_matches() {
    let session = postgres();
    setup(
        &*session,
        &[
            "drop table if exists sonde_edit_conflict",
            "create table sonde_edit_conflict (id int4 primary key, name text)",
            "insert into sonde_edit_conflict values (1, 'a'), (2, 'b')",
        ],
    );
    let table = TableRef::unqualified("sonde_edit_conflict");
    let first = sonde_core::edit::update(
        session.dialect(),
        &table,
        &["id"],
        &[Value::Int(1)],
        &["name"],
        &[Value::text("a2")],
    );

    // The row the second statement keys moves while the set is staged.
    run(
        &*session,
        "update sonde_edit_conflict set id = 3 where id = 2",
        &[],
    )
    .expect("the interfering change lands");
    let second = sonde_core::edit::delete(session.dialect(), &table, &["id"], &[Value::Int(2)]);

    let result = block_on(session.apply(&[first, second]));

    assert_eq!(result, Err(Error::Stale));
    // The first statement must have rolled back with the second.
    assert_eq!(
        scalar(
            &*session,
            "select name from sonde_edit_conflict where id = 1"
        ),
        Value::text("a")
    );
    setup(&*session, &["drop table sonde_edit_conflict"]);
}

#[test]
fn apply_should_report_a_database_error_and_rollback_the_rest() {
    let session = postgres();
    setup(
        &*session,
        &[
            "drop table if exists sonde_edit_error",
            "create table sonde_edit_error (id int4 primary key, name text)",
            "insert into sonde_edit_error values (1, 'a'), (2, 'b')",
        ],
    );
    let table = TableRef::unqualified("sonde_edit_error");
    let first = sonde_core::edit::update(
        session.dialect(),
        &table,
        &["id"],
        &[Value::Int(1)],
        &["name"],
        &[Value::text("x")],
    );
    let duplicate = sonde_core::edit::insert(session.dialect(), &table, &["id"], &[Value::Int(1)]);

    let result = block_on(session.apply(&[first, duplicate]));

    assert!(matches!(result, Err(Error::Query { .. })));
    assert_eq!(
        scalar(&*session, "select name from sonde_edit_error where id = 1"),
        Value::text("a")
    );
    setup(&*session, &["drop table sonde_edit_error"]);
}
