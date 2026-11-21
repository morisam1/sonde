//! Integration tests against in-memory SQLite.

mod common;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use common::{resident_bytes, run, scalar, scalar_with, setup, sqlite};
use futures::{StreamExt as _, executor::block_on};
use sonde_core::{
    ColumnOrigin, ConnectionConfig, Error, Session, SqliteConfig, TableRef, Value,
    schema::{RelationKind, TableDetail},
};

/// A file path that does not exist yet, unique to one test.
fn scratch_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("sonde-{}-{name}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn open(path: &Path, read_only: bool) -> sonde_core::Result<Box<dyn Session>> {
    let config = ConnectionConfig::Sqlite(SqliteConfig {
        path: path.to_path_buf(),
        read_only,
    });
    block_on(sonde_core::connect(&config, None))
}

macro_rules! decodes {
    ($($name:ident: $sql:expr => $expected:expr;)*) => {$(
        #[test]
        fn $name() {
            assert_eq!(scalar(&*sqlite(), concat!("select ", $sql)), $expected);
        }
    )*};
}

decodes! {
    decode_should_read_integer: "-9223372036854775807 - 1" => Value::Int(i64::MIN);
    decode_should_read_real: "1.5" => Value::Float(1.5);
    decode_should_read_text: "'héllo'" => Value::text("héllo");
    decode_should_read_blob: "x'deadbeef'" => Value::Bytes(Arc::from([0xde, 0xad, 0xbe, 0xef]));
    decode_should_read_null: "null" => Value::Null;
}

#[test]
fn execute_should_return_columns_for_an_empty_result() {
    let session = sqlite();

    let handle = block_on(session.execute("select 1 as a, 'x' as b where 0", &[])).unwrap();

    let names: Vec<&str> = handle.columns.iter().map(|c| &*c.name).collect();
    assert_eq!(names, ["a", "b"]);
}

#[test]
fn execute_should_return_no_rows_for_an_empty_result() {
    let (rows, _) = run(&*sqlite(), "select 1 where 0", &[]).unwrap();

    assert!(rows.is_empty());
}

#[test]
fn execute_should_report_declared_types() {
    let session = sqlite();
    setup(
        &*session,
        &["create table t (id integer, at DATETIME, loose)"],
    );

    let handle = block_on(session.execute("select id, at, loose, 1 + 1 from t", &[])).unwrap();

    let types: Vec<&str> = handle.columns.iter().map(|c| &*c.type_name).collect();
    assert_eq!(types, ["INTEGER", "DATETIME", "", ""]);
}

#[test]
fn execute_should_report_the_origin_of_an_aliased_column() {
    let session = sqlite();
    setup(&*session, &["create table t (id integer)"]);

    let handle = block_on(session.execute("select id as renamed from t", &[])).unwrap();

    assert_eq!(
        handle.columns[0].origin,
        Some(ColumnOrigin {
            schema: Some("main".into()),
            table: "t".into(),
            column: "id".into(),
        })
    );
}

#[test]
fn execute_should_report_no_origin_for_an_expression() {
    let session = sqlite();

    let handle = block_on(session.execute("select 1 + 1", &[])).unwrap();

    assert_eq!(handle.columns[0].origin, None);
}

#[test]
fn execute_should_report_rows_affected() {
    let session = sqlite();
    setup(
        &*session,
        &["create table t (n)", "insert into t values (1), (2), (3)"],
    );

    let (_, summary) = run(&*session, "update t set n = n + 1 where n > 1", &[]).unwrap();

    assert_eq!(summary.rows_affected, Some(2));
}

#[test]
fn execute_should_not_report_rows_affected_for_a_select() {
    let (_, summary) = run(&*sqlite(), "select 1", &[]).unwrap();

    assert_eq!(summary.rows_affected, None);
}

#[test]
fn execute_should_bind_parameters() {
    let value = scalar_with(
        &*sqlite(),
        "select ?1 || '-' || ?2",
        &[Value::text("a"), Value::Int(2)],
    );

    assert_eq!(value, Value::text("a-2"));
}

#[test]
fn execute_should_report_syntax_error_position() {
    let sql = "select 'é' + from t";

    let error = run(&*sqlite(), sql, &[]).unwrap_err();

    assert_eq!(error.position().map(|at| &sql[at..]), Some("from t"));
}

#[test]
fn session_should_stay_usable_after_an_error() {
    let session = sqlite();
    let _ = run(&*session, "select nonsense(", &[]);

    assert_eq!(scalar(&*session, "select 7"), Value::Int(7));
}

/// A query that never finishes on its own.
const ENDLESS: &str = "with recursive n(i) as (select 1 union all select i + 1 from n) \
                       select count(*) from n";

#[test]
fn cancel_should_stop_a_running_query() {
    let session = sqlite();
    let canceller = session.canceller();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });

    let error = run(&*session, ENDLESS, &[]).unwrap_err();

    assert_eq!(error, Error::Cancelled);
}

#[test]
fn session_should_stay_usable_after_a_cancel() {
    let session = sqlite();
    let canceller = session.canceller();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });
    let _ = run(&*session, ENDLESS, &[]);

    assert_eq!(scalar(&*session, "select 7"), Value::Int(7));
}

#[test]
fn dropping_the_handle_should_free_the_session() {
    let session = sqlite();
    let mut handle = block_on(session.execute(
        "with recursive n(i) as (select 1 union all select i + 1 from n) select i from n",
        &[],
    ))
    .unwrap();
    let _ = block_on(handle.pages.next());

    drop(handle);

    assert_eq!(scalar(&*session, "select 7"), Value::Int(7));
}

#[test]
fn stream_should_keep_memory_flat_over_a_million_rows() {
    const ROWS: usize = 1_000_000;
    const BUDGET_BYTES: usize = 48 * 1024 * 1024;
    let session = sqlite();
    let sql = format!(
        "with recursive n(i) as (select 1 union all select i + 1 from n limit {ROWS}) \
         select i, hex(randomblob(60)) from n"
    );
    let mut handle = block_on(session.execute(&sql, &[])).unwrap();
    let first = block_on(handle.pages.next()).unwrap().unwrap();
    let baseline = resident_bytes();

    std::thread::sleep(Duration::from_millis(500));
    let mut peak = resident_bytes();
    let mut rows = first.rows.len();
    while let Some(page) = block_on(handle.pages.next()) {
        rows += page.unwrap().rows.len();
        peak = peak.max(resident_bytes());
    }

    assert_eq!(
        (rows, peak.saturating_sub(baseline) < BUDGET_BYTES),
        (ROWS, true),
        "grew by {} MB",
        peak.saturating_sub(baseline) / (1024 * 1024)
    );
}

#[test]
fn connect_should_fail_for_a_missing_file() {
    let result = open(&scratch_path("missing"), false);

    assert!(matches!(result, Err(Error::Connect(_))));
}

#[test]
fn connect_should_fail_for_a_file_that_is_not_a_database() {
    let path = scratch_path("garbage");
    std::fs::write(&path, "this is not a database, just some text".repeat(40)).unwrap();

    let result = open(&path, false);

    let _ = std::fs::remove_file(&path);
    assert!(matches!(result, Err(Error::Connect(_))));
}

#[test]
fn read_only_session_should_refuse_writes() {
    let path = scratch_path("readonly");
    rusqlite::Connection::open(&path)
        .and_then(|c| c.execute_batch("create table t (n)"))
        .unwrap();
    let session = open(&path, true).unwrap();

    let result = run(&*session, "insert into t values (1)", &[]);

    let _ = std::fs::remove_file(&path);
    assert!(matches!(result, Err(Error::Query { .. })));
}

#[test]
fn close_should_end_the_session() {
    let session = sqlite();

    assert_eq!(block_on(session.close()), Ok(()));
}

fn catalog_fixture() -> Box<dyn Session> {
    let session = sqlite();
    setup(
        &*session,
        &[
            "create table author (id integer primary key, name text not null, email text unique)",
            "create table book (
             author_id integer not null references author (id),
             isbn text not null,
             title text not null default 'untitled',
             price numeric(10, 2),
             primary key (author_id, isbn)) without rowid",
            "create index book_title_lower on book (lower(title), price)",
            "create view titles as select title from book",
        ],
    );
    session
}

fn detail(table: &str) -> TableDetail {
    block_on(catalog_fixture().table_detail(&TableRef::unqualified(table))).unwrap()
}

#[test]
fn catalog_should_list_tables_and_views() {
    let catalog = block_on(catalog_fixture().catalog()).unwrap();

    let main = catalog.schemas.iter().find(|s| s.name == "main").unwrap();
    let relations: Vec<(&str, RelationKind)> = main
        .relations
        .iter()
        .map(|r| (r.name.as_str(), r.kind))
        .collect();
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
fn table_detail_should_describe_columns() {
    let detail = detail("book");

    let columns: Vec<(&str, &str, bool, Option<&str>)> = detail
        .columns
        .iter()
        .map(|c| (&*c.name, &*c.type_name, c.nullable, c.default.as_deref()))
        .collect();
    assert_eq!(
        columns,
        [
            ("author_id", "INTEGER", false, None),
            ("isbn", "TEXT", false, None),
            ("title", "TEXT", false, Some("'untitled'")),
            ("price", "numeric(10, 2)", true, None),
        ]
    );
}

#[test]
fn table_detail_should_report_a_composite_primary_key_in_order() {
    assert_eq!(detail("book").primary_key, ["author_id", "isbn"]);
}

#[test]
fn table_detail_should_report_foreign_keys() {
    let detail = detail("book");

    let keys: Vec<(&[String], &str, &[String])> = detail
        .foreign_keys
        .iter()
        .map(|k| (&*k.columns, &*k.references.name, &*k.referenced_columns))
        .collect();
    assert_eq!(
        keys,
        [(
            &["author_id".to_owned()][..],
            "author",
            &["id".to_owned()][..]
        )]
    );
}

#[test]
fn table_detail_should_report_indexes_with_expression_entries() {
    let detail = detail("book");

    let named: Vec<(&str, Vec<Option<&str>>, bool)> = detail
        .indexes
        .iter()
        .filter(|i| !i.primary)
        .map(|i| {
            (
                &*i.name,
                i.columns.iter().map(Option::as_deref).collect(),
                i.unique,
            )
        })
        .collect();
    assert_eq!(
        named,
        [("book_title_lower", vec![None, Some("price")], false)]
    );
}

#[test]
fn table_detail_should_report_rowid_for_an_ordinary_table() {
    assert!(detail("author").has_rowid);
}

#[test]
fn table_detail_should_report_no_rowid_for_a_without_rowid_table() {
    assert!(!detail("book").has_rowid);
}

#[test]
fn table_detail_should_report_no_rowid_for_a_view() {
    assert!(!detail("titles").has_rowid);
}

#[test]
fn table_detail_should_fail_for_a_missing_table() {
    let result = block_on(sqlite().table_detail(&TableRef::unqualified("missing")));

    assert!(matches!(result, Err(Error::Query { .. })));
}

#[test]
fn apply_should_round_trip_the_storage_classes() {
    let session = sqlite();
    setup(
        &*session,
        &[
            "create table sonde_edit (id integer primary key, n real, t text, b blob, i int)",
            "insert into sonde_edit values (1, 1.5, 'x', X'0102', 7)",
        ],
    );
    let columns = vec!["n", "t", "b", "i"];
    let values = vec![
        sonde_core::value::parse("REAL", "-2.5").unwrap(),
        sonde_core::value::parse("TEXT", "y'z").unwrap(),
        sonde_core::value::parse("BLOB", "\\x0304").unwrap(),
        sonde_core::value::parse("INTEGER", "8").unwrap(),
    ];
    let update = sonde_core::edit::update(
        session.dialect(),
        &TableRef::unqualified("sonde_edit"),
        &["id"],
        &[Value::Int(1)],
        &columns,
        &values,
    );
    block_on(session.apply(&[update])).expect("the update applies");

    let (rows, _) =
        run(&*session, "select n, t, b, i from sonde_edit", &[]).expect("the row reads back");
    let row = rows.into_iter().next().unwrap();
    assert_eq!(row[0], Value::Float(-2.5));
    assert_eq!(row[1], Value::text("y'z"));
    assert_eq!(row[2], Value::Bytes(Arc::from([0x03, 0x04])));
    assert_eq!(row[3], Value::Int(8));

    // An insert with only the edited columns, and a delete, in one set.
    let insert = sonde_core::edit::insert(
        session.dialect(),
        &TableRef::unqualified("sonde_edit"),
        &["id", "t"],
        &[Value::Int(2), Value::text("new")],
    );
    let delete = sonde_core::edit::delete(
        session.dialect(),
        &TableRef::unqualified("sonde_edit"),
        &["id"],
        &[Value::Int(1)],
    );
    block_on(session.apply(&[insert, delete])).expect("the insert and delete apply");

    assert_eq!(
        scalar(&*session, "select count(*) from sonde_edit"),
        Value::Int(1)
    );
    assert_eq!(
        scalar(&*session, "select t from sonde_edit"),
        Value::text("new")
    );
    // The unedited column came back as its absent value, not a default.
    assert_eq!(
        scalar(&*session, "select i is null from sonde_edit"),
        Value::Int(1)
    );
}

#[test]
fn apply_should_key_nulls_with_is_null() {
    let session = sqlite();
    setup(
        &*session,
        &[
            "create table sonde_null_key (k text primary key, v text)",
            "insert into sonde_null_key values ('x', '1'), (null, '2')",
        ],
    );
    let table = TableRef::unqualified("sonde_null_key");
    let statement = sonde_core::edit::update(
        session.dialect(),
        &table,
        &["k"],
        &[Value::Null],
        &["v"],
        &[Value::text("3")],
    );
    assert!(statement.sql.contains("\"k\" IS NULL"));
    assert_eq!(statement.params, vec![Value::text("3")]);

    block_on(session.apply(&[statement])).expect("the null-keyed update applies");
    assert_eq!(
        scalar(&*session, "select v from sonde_null_key where k is null"),
        Value::text("3")
    );

    // A second null-keyed row makes the key ambiguous: the update would touch
    // two rows, so it must not.
    run(
        &*session,
        "insert into sonde_null_key values (null, '4')",
        &[],
    )
    .unwrap();
    let ambiguous = sonde_core::edit::update(
        session.dialect(),
        &table,
        &["k"],
        &[Value::Null],
        &["v"],
        &[Value::text("5")],
    );
    let result = block_on(session.apply(&[ambiguous]));

    assert_eq!(result, Err(Error::Stale));
    assert_eq!(
        scalar(
            &*session,
            "select count(*) from sonde_null_key where k is null and v = '3'"
        ),
        Value::Int(1)
    );
}

#[test]
fn apply_should_fail_without_writing_on_a_read_only_database() {
    let path = scratch_path("apply_readonly");
    rusqlite::Connection::open(&path)
        .and_then(|c| {
            c.execute_batch(
                "create table t (id integer primary key, v text); insert into t values (1, 'a')",
            )
        })
        .unwrap();
    let session = open(&path, true).unwrap();

    let result = block_on(session.apply(&[sonde_core::edit::update(
        session.dialect(),
        &TableRef::unqualified("t"),
        &["id"],
        &[Value::Int(1)],
        &["v"],
        &[Value::text("b")],
    )]));

    let _ = std::fs::remove_file(&path);
    assert!(matches!(result, Err(Error::Query { .. })));
}
