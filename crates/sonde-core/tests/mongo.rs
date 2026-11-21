//! The MongoDB driver against the one-member replica set in
//! `tests/docker-compose.yml`. Each test works in its own database.

mod common;

use std::sync::Arc;

use common::{local, open, run, scalar, setup, table};
use futures::executor::block_on;
use sonde_core::{
    ConnectionConfig, DbKind, Error, Session, TableRef, Value, edit, schema::RelationKind,
};

fn config(db: &str) -> ConnectionConfig {
    let mut config = local(DbKind::Mongodb, 27019, db, "");
    if let ConnectionConfig::Mongodb(server) = &mut config {
        // The replica set names its member localhost:27017, which is not
        // where the test reaches it.
        server
            .options
            .insert("options".into(), "directConnection=true".into());
    }
    config
}

/// A session on a fresh database named after the test.
fn fresh(name: &str) -> Box<dyn Session> {
    let session = open(&config(&format!("sonde_{name}")), None);
    setup(&*session, &["db.dropDatabase()"]);
    session
}

fn text(value: &str) -> Value {
    Value::text(value)
}

#[test]
fn find_should_return_documents_as_rows_of_their_fields() {
    let session = fresh("find");
    setup(
        &*session,
        &[r#"db.people.insertMany([
                {_id: 1, name: "ada", born: ISODate("1815-12-10T00:00:00Z"), tags: ["math"]},
                {_id: 2, name: "alan", age: NumberLong(41), nested: {a: {b: 1}}}
            ])"#],
    );

    let (columns, rows) = table(&*session, "db.people.find({}).sort({_id: 1})").unwrap();

    assert_eq!(columns, ["_id", "name", "born", "tags", "age", "nested"]);
    assert_eq!(rows[0][1], text("ada"));
    assert!(matches!(rows[0][2], Value::Timestamp(_)));
    assert_eq!(rows[0][3], Value::Json(Arc::from(r#"["math"]"#)));
    assert_eq!(rows[0][4], Value::Null);
    assert_eq!(rows[1][4], Value::Int(41));
    assert_eq!(rows[1][5], Value::Json(Arc::from(r#"{"a":{"b":1}}"#)));

    let (_, rows) = table(
        &*session,
        "db.people.find({name: /^al/}, {name: 1, _id: 0})",
    )
    .unwrap();
    assert_eq!(rows, [[text("alan")]]);
}

#[test]
fn writes_should_report_what_they_changed() {
    let session = fresh("writes");
    setup(&*session, &["db.t.insertMany([{n: 1}, {n: 2}, {n: 3}])"]);

    let (_, summary) = run(
        &*session,
        "db.t.updateMany({n: {$gt: 1}}, {$inc: {n: 10}})",
        &[],
    )
    .unwrap();
    assert_eq!(summary.rows_affected, Some(2));
    assert_eq!(
        scalar(&*session, "db.t.countDocuments({n: {$gt: 10}})"),
        Value::Int(2)
    );

    let (_, summary) = run(&*session, "db.t.deleteOne({n: 1})", &[]).unwrap();
    assert_eq!(summary.rows_affected, Some(1));
    assert_eq!(
        scalar(&*session, "db.t.estimatedDocumentCount()"),
        Value::Int(2)
    );
}

#[test]
fn aggregate_and_commands_should_return_rows() {
    let session = fresh("aggregate");
    setup(
        &*session,
        &["db.s.insertMany([{k: 'a', v: 1}, {k: 'a', v: 2}, {k: 'b', v: 5}])"],
    );

    let (columns, rows) = table(
        &*session,
        "db.s.aggregate([{$group: {_id: '$k', total: {$sum: '$v'}}}, {$sort: {_id: 1}}])",
    )
    .unwrap();
    assert_eq!(columns, ["_id", "total"]);
    assert_eq!(
        rows,
        [[text("a"), Value::Int(3)], [text("b"), Value::Int(5)]]
    );

    let (_, rows) = table(&*session, "db.runCommand({ping: 1})").unwrap();
    assert_eq!(rows[0][0], Value::Float(1.0));

    let (columns, _) = table(&*session, "show dbs").unwrap();
    assert_eq!(columns, ["name", "sizeOnDisk", "empty"]);

    let (_, rows) = table(&*session, "db.s.distinct('k')").unwrap();
    assert_eq!(rows, [[text("a")], [text("b")]]);
}

#[test]
fn errors_should_point_at_the_problem_or_carry_the_server_message() {
    let session = fresh("errors");

    let error = run(&*session, "db.t.find({a: })", &[]).unwrap_err();
    assert_eq!(error.position(), Some(14));

    let error = run(&*session, "db.t.aggregate([{$nope: 1}])", &[]).unwrap_err();
    let Error::Query { message, .. } = error else {
        panic!("expected a query error, got {error:?}");
    };
    assert!(message.contains("$nope"), "{message}");
}

#[test]
fn use_should_switch_the_current_database() {
    let session = fresh("use");
    setup(
        &*session,
        &[
            "use sonde_use_other",
            "db.dropDatabase()",
            "db.x.insertOne({a: 1})",
        ],
    );

    assert_eq!(scalar(&*session, "db.x.countDocuments({})"), Value::Int(1));
    let catalog = block_on(session.catalog()).unwrap();
    assert_eq!(catalog.schemas[0].name, "sonde_use_other");
}

#[test]
fn catalog_should_list_databases_and_their_collections() {
    let session = fresh("catalog");
    setup(
        &*session,
        &[
            "db.b.insertOne({})",
            "db.a.insertOne({})",
            "db.runCommand({create: 'v', viewOn: 'a', pipeline: []})",
        ],
    );

    let catalog = block_on(session.catalog()).unwrap();

    let schema = &catalog.schemas[0];
    assert_eq!(schema.name, "sonde_catalog");
    let relations: Vec<(&str, RelationKind)> = schema
        .relations
        .iter()
        .map(|r| (r.name.as_str(), r.kind))
        .collect();
    assert_eq!(
        relations,
        [
            ("a", RelationKind::Collection),
            ("b", RelationKind::Collection),
            ("v", RelationKind::View)
        ]
    );
    assert!(catalog.schemas.iter().all(|s| s.name != "local"));
}

#[test]
fn table_detail_should_sample_fields_and_list_indexes() {
    let session = fresh("detail");
    setup(
        &*session,
        &[
            "db.t.insertMany([{name: 'a', n: 1}, {name: 'b', n: 2.5, extra: true}])",
            "db.t.createIndex({name: 1}, {unique: true, name: 'by_name'})",
        ],
    );

    let detail = block_on(session.table_detail(&TableRef::new("sonde_detail", "t"))).unwrap();

    let columns: Vec<(&str, &str)> = detail
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.type_name.as_str()))
        .collect();
    assert_eq!(
        columns,
        [
            ("_id", "objectId"),
            ("name", "string"),
            ("n", "mixed"),
            ("extra", "bool")
        ]
    );
    assert_eq!(detail.primary_key, ["_id"]);
    assert!(
        detail
            .indexes
            .iter()
            .any(|i| i.name == "by_name" && i.unique)
    );
    assert!(block_on(session.table_detail(&TableRef::new("sonde_detail", "nope"))).is_err());
}

#[test]
fn edits_should_round_trip_through_the_table_view() {
    let session = fresh("edits");
    setup(
        &*session,
        &["db.t.insertMany([{name: 'a', n: 1}, {name: 'b', n: 2}])"],
    );
    let table_ref = TableRef::new("sonde_edits", "t");
    let detail = block_on(session.table_detail(&table_ref)).unwrap();
    let names: Vec<String> = detail.columns.iter().map(|c| c.name.clone()).collect();
    let dialect = session.dialect();

    let sql = dialect.browse(&table_ref, &names, "name: 'a'", None);
    let (rows, _) = run(&*session, &sql, &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let id = rows[0][0].clone();

    let typed = sonde_core::value::parse("int", "7").unwrap();
    let changes = [
        edit::update(
            dialect,
            &table_ref,
            &["_id"],
            std::slice::from_ref(&id),
            &["n"],
            &[typed],
        ),
        edit::insert(
            dialect,
            &table_ref,
            &["name", "n"],
            &[text("c"), Value::Null],
        ),
    ];
    block_on(session.apply(&changes)).unwrap();
    assert_eq!(
        scalar(&*session, "db.t.countDocuments({n: 7})"),
        Value::Int(1)
    );
    assert_eq!(
        scalar(
            &*session,
            "db.t.countDocuments({name: 'c', n: {$exists: false}})"
        ),
        Value::Int(1)
    );

    // A change keyed on a document that went away rolls the set back.
    setup(&*session, &["db.t.deleteOne({name: 'b'})"]);
    let gone = sonde_core::value::parse("objectId", "65a1b2c3d4e5f60718293a4b").unwrap();
    let changes = [
        edit::delete(dialect, &table_ref, &["_id"], &[id]),
        edit::update(
            dialect,
            &table_ref,
            &["_id"],
            &[gone],
            &["n"],
            &[Value::Int(1)],
        ),
    ];
    assert_eq!(block_on(session.apply(&changes)), Err(Error::Stale));
    assert_eq!(
        scalar(&*session, "db.t.countDocuments({name: 'a'})"),
        Value::Int(1)
    );
}

#[test]
fn connect_should_explain_a_closed_port() {
    let config = local(DbKind::Mongodb, 1, "test", "");

    let Err(Error::Connect(message)) = block_on(sonde_core::connect(&config, None)) else {
        panic!("nothing listens on port 1");
    };
    assert!(message.contains("1"), "{message}");
}
