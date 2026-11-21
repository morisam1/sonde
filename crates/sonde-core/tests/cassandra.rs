//! The Cassandra driver against the single node in
//! `tests/docker-compose.yml`. Each test works in its own keyspace.

mod common;

use common::{local, open, run, scalar, setup, table};
use futures::executor::block_on;
use sonde_core::{DbKind, Error, Session, TableRef, Value, edit, schema::RelationKind};

/// A session in a fresh keyspace named after the test.
fn fresh(name: &str) -> Box<dyn Session> {
    let session = open(&local(DbKind::Cassandra, 19042, "", ""), None);
    let keyspace = format!("sonde_{name}");
    setup(
        &*session,
        &[
            &format!("DROP KEYSPACE IF EXISTS {keyspace}"),
            &format!(
                "CREATE KEYSPACE {keyspace} WITH replication = \
                 {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
            ),
            &format!("USE {keyspace}"),
        ],
    );
    session
}

fn text(value: &str) -> Value {
    Value::text(value)
}

#[test]
fn select_should_decode_common_types() {
    let session = fresh("types");
    setup(
        &*session,
        &[
            "CREATE TABLE t (id int PRIMARY KEY, b bigint, d decimal, f double, s text, \
             u uuid, ts timestamp, da date, ok boolean, l list<int>, m map<text, int>, bl blob)",
            "INSERT INTO t (id, b, d, f, s, u, ts, da, ok, l, m, bl) VALUES (1, 9007199254740993, \
             10.25, 1.5, 'héllo', 6ba7b810-9dad-11d1-80b4-00c04fd430c8, '2024-02-29 13:14:15+0000', \
             '1999-12-31', true, [1, 2], {'a': 1}, 0x00ff)",
            "INSERT INTO t (id) VALUES (2)",
        ],
    );

    let (columns, rows) = table(
        &*session,
        "SELECT id, b, d, f, s, u, ts, da, ok, l, m, bl FROM t WHERE id = 1",
    )
    .unwrap();

    assert_eq!(columns[0], "id");
    let row = &rows[0];
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(row[1], Value::Int(9_007_199_254_740_993));
    assert_eq!(row[2], Value::Decimal("10.25".parse().unwrap()));
    assert_eq!(row[3], Value::Float(1.5));
    assert_eq!(row[4], text("héllo"));
    assert!(matches!(row[5], Value::Uuid(_)));
    assert_eq!(row[6].to_string(), "2024-02-29 13:14:15+00:00");
    assert_eq!(row[7].to_string(), "1999-12-31");
    assert_eq!(row[8], Value::Bool(true));
    assert_eq!(row[9], Value::Json("[1,2]".into()));
    assert_eq!(row[10], Value::Json("{\"a\":1}".into()));
    assert_eq!(row[11], Value::Bytes(vec![0, 255].into()));

    let (_, rows) = table(&*session, "SELECT b FROM t WHERE id = 2").unwrap();
    assert_eq!(rows, [[Value::Null]]);
}

#[test]
fn select_should_page_through_many_rows() {
    let session = fresh("paging");
    setup(
        &*session,
        &["CREATE TABLE t (p int, c int, PRIMARY KEY (p, c))"],
    );
    let mut batch = String::from("BEGIN UNLOGGED BATCH\n");
    for c in 0..2500 {
        batch.push_str(&format!("INSERT INTO t (p, c) VALUES (1, {c});\n"));
    }
    batch.push_str("APPLY BATCH");
    setup(&*session, &[&batch]);

    let (rows, summary) = run(&*session, "SELECT c FROM t WHERE p = 1", &[]).unwrap();

    assert_eq!(rows.len(), 2500);
    assert_eq!(summary.rows_returned, 2500);
}

#[test]
fn syntax_errors_should_point_at_the_problem() {
    let session = fresh("errors");
    let sql = "SELECT * FORM t";

    let error = run(&*session, sql, &[]).unwrap_err();

    let Error::Query { position, .. } = &error else {
        panic!("expected a query error, got {error:?}");
    };
    assert_eq!(position.map(|p| &sql[p..p + 4]), Some("FORM"), "{error}");
}

#[test]
fn catalog_and_detail_should_describe_keyspaces_and_keys() {
    let session = fresh("catalog");
    setup(
        &*session,
        &[
            "CREATE TABLE t (a int, b text, c int, v text, PRIMARY KEY ((a, b), c))",
            "CREATE INDEX t_by_v ON t (v)",
        ],
    );

    let catalog = block_on(session.catalog()).unwrap();
    assert_eq!(catalog.schemas[0].name, "sonde_catalog");
    assert_eq!(catalog.schemas[0].relations[0].name, "t");
    assert_eq!(catalog.schemas[0].relations[0].kind, RelationKind::Table);
    assert!(
        catalog
            .schemas
            .iter()
            .all(|s| !s.name.starts_with("system"))
    );

    let detail = block_on(session.table_detail(&TableRef::new("sonde_catalog", "t"))).unwrap();
    let names: Vec<&str> = detail.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c", "v"]);
    assert_eq!(detail.primary_key, ["a", "b", "c"]);
    assert!(detail.indexes.iter().any(|i| i.name == "t_by_v"));
    assert!(block_on(session.table_detail(&TableRef::new("sonde_catalog", "nope"))).is_err());
}

#[test]
fn edits_should_apply_and_spot_a_vanished_row() {
    let session = fresh("edits");
    setup(
        &*session,
        &[
            "CREATE TABLE t (id int PRIMARY KEY, name text, n bigint)",
            "INSERT INTO t (id, name, n) VALUES (1, 'a', 1)",
        ],
    );
    let table_ref = TableRef::new("sonde_edits", "t");
    let dialect = session.dialect();

    let changes = [
        edit::update(
            dialect,
            &table_ref,
            &["id"],
            &[Value::Int(1)],
            &["name", "n"],
            &[text("b"), Value::Int(5)],
        ),
        edit::insert(
            dialect,
            &table_ref,
            &["id", "name"],
            &[Value::Int(2), text("c")],
        ),
    ];
    block_on(session.apply(&changes)).unwrap();
    assert_eq!(
        scalar(&*session, "SELECT name FROM t WHERE id = 1"),
        text("b")
    );
    assert_eq!(
        scalar(&*session, "SELECT name FROM t WHERE id = 2"),
        text("c")
    );

    let gone = [edit::delete(dialect, &table_ref, &["id"], &[Value::Int(9)])];
    assert_eq!(block_on(session.apply(&gone)), Err(Error::Stale));

    let names = vec!["id".to_owned(), "name".to_owned(), "n".to_owned()];
    let sql = dialect.browse(&table_ref, &names, "name = 'b'", None);
    let (rows, _) = run(&*session, &sql, &[]).unwrap();
    assert_eq!(rows, [[Value::Int(1), text("b"), Value::Int(5)]]);
}

#[test]
fn connect_should_explain_a_closed_port() {
    let config = local(DbKind::Cassandra, 1, "", "");

    let Err(Error::Connect(message)) = block_on(sonde_core::connect(&config, None)) else {
        panic!("nothing listens on port 1");
    };
    assert!(message.contains("127.0.0.1:1"), "{message}");
}
