//! The Elasticsearch driver against the single node in
//! `tests/docker-compose.yml`. Each test works in its own indices.

mod common;

use common::{local, open, run, setup, table};
use futures::executor::block_on;
use sonde_core::{DbKind, Error, Session, TableRef, Value, schema::RelationKind};

fn session() -> Box<dyn Session> {
    open(&local(DbKind::Elasticsearch, 19200, "", ""), None)
}

/// A fresh index `name` holding `docs`, searchable at once.
fn fresh(name: &str, docs: &[&str]) -> Box<dyn Session> {
    let session = session();
    let _ = run(&*session, &format!("DELETE /{name}"), &[]);
    setup(
        &*session,
        &[&format!(
            "PUT /{name}\n{{\"mappings\": {{\"properties\": {{\"name\": {{\"type\": \"keyword\"}}, \
             \"n\": {{\"type\": \"long\"}}, \"at\": {{\"type\": \"date\"}}, \
             \"geo\": {{\"properties\": {{\"city\": {{\"type\": \"keyword\"}}}}}}}}}}}}"
        )],
    );
    let mut bulk = format!("POST /{name}/_bulk?refresh=true\n");
    for (ix, doc) in docs.iter().enumerate() {
        bulk.push_str(&format!("{{\"index\": {{\"_id\": \"{ix}\"}}}}\n{doc}\n"));
    }
    setup(&*session, &[&bulk]);
    session
}

fn text(value: &str) -> Value {
    Value::text(value)
}

#[test]
fn sql_should_return_typed_rows() {
    let session = fresh(
        "sonde_sql",
        &[
            r#"{"name": "a", "n": 1, "at": "2024-01-02T03:04:05Z"}"#,
            r#"{"name": "b", "n": 2}"#,
        ],
    );

    let (columns, rows) = table(&*session, "SELECT name, n, at FROM sonde_sql ORDER BY n").unwrap();

    assert_eq!(columns, ["name", "n", "at"]);
    assert_eq!(rows[0][0], text("a"));
    assert_eq!(rows[0][1], Value::Int(1));
    assert!(matches!(rows[0][2], Value::TimestampTz(_)));
    assert_eq!(rows[1][2], Value::Null);
}

#[test]
fn sql_should_page_through_its_cursor() {
    let docs: Vec<String> = (0..2500)
        .map(|n| format!(r#"{{"name": "x", "n": {n}}}"#))
        .collect();
    let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
    let session = fresh("sonde_paging", &docs);

    let (rows, summary) = run(&*session, "SELECT n FROM sonde_paging", &[]).unwrap();

    assert_eq!(rows.len(), 2500);
    assert_eq!(summary.rows_returned, 2500);
}

#[test]
fn sql_errors_should_point_at_the_problem() {
    let session = fresh("sonde_errors", &[r#"{"name": "a"}"#]);
    let sql = "SELECT nope FROM sonde_errors";

    let error = run(&*session, sql, &[]).unwrap_err();

    let Error::Query {
        message, position, ..
    } = &error
    else {
        panic!("expected a query error, got {error:?}");
    };
    assert!(message.contains("nope"), "{message}");
    assert_eq!(position.map(|p| &sql[p..p + 4]), Some("nope"));
}

#[test]
fn rest_requests_should_show_hits_and_replies_as_rows() {
    let session = fresh(
        "sonde_rest",
        &[r#"{"name": "a", "n": 1}"#, r#"{"name": "b", "n": 2}"#],
    );

    let (columns, rows) = table(
        &*session,
        "GET /sonde_rest/_search\n{\"query\": {\"term\": {\"name\": \"b\"}}}",
    )
    .unwrap();
    assert_eq!(columns, ["_index", "_id", "_score", "name", "n"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][3], text("b"));

    let (columns, rows) = table(
        &*session,
        "POST /sonde_rest/_search\n{\"size\": 0, \"aggs\": {\"by\": {\"terms\": {\"field\": \"name\"}}}}",
    )
    .unwrap();
    assert_eq!(columns, ["key", "doc_count"]);
    assert_eq!(rows.len(), 2);

    // _cat answers in JSON without being asked.
    let (columns, _) = table(&*session, "GET /_cat/indices/sonde_rest").unwrap();
    assert!(columns.contains(&"index".to_owned()), "{columns:?}");

    let (columns, rows) = table(&*session, "GET /sonde_rest/_count").unwrap();
    assert_eq!(
        rows[0][columns.iter().position(|c| c == "count").unwrap()],
        Value::Int(2)
    );
}

#[test]
fn rest_errors_should_carry_the_reason() {
    let session = session();

    let error = run(&*session, "GET /sonde_no_such_index/_search", &[]).unwrap_err();

    let Error::Query { message, code, .. } = error else {
        panic!("expected a query error");
    };
    assert!(message.contains("no such index"), "{message}");
    assert_eq!(code.as_deref(), Some("index_not_found_exception"));
}

#[test]
fn catalog_and_detail_should_describe_indices() {
    let session = fresh("sonde_catalog", &[r#"{"name": "a"}"#]);
    setup(
        &*session,
        &[
            "POST /_aliases\n{\"actions\": [{\"add\": {\"index\": \"sonde_catalog\", \"alias\": \"sonde_alias\"}}]}",
        ],
    );

    let catalog = block_on(session.catalog()).unwrap();
    let relations = &catalog.schemas[0].relations;
    assert!(
        relations
            .iter()
            .any(|r| r.name == "sonde_catalog" && r.kind == RelationKind::Collection)
    );
    assert!(
        relations
            .iter()
            .any(|r| r.name == "sonde_alias" && r.kind == RelationKind::View)
    );
    assert!(relations.iter().all(|r| !r.name.starts_with('.')));

    let table_ref = TableRef::new(catalog.schemas[0].name.clone(), "sonde_catalog");
    let detail = block_on(session.table_detail(&table_ref)).unwrap();
    let columns: Vec<(&str, &str)> = detail
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.type_name.as_str()))
        .collect();
    assert!(columns.contains(&("geo.city", "keyword")), "{columns:?}");
    assert!(session.dialect().read_only_reason().is_some());

    let names: Vec<String> = detail.columns.iter().map(|c| c.name.clone()).collect();
    let sql = session
        .dialect()
        .browse(&table_ref, &names, "name = 'a'", Some(("name", true)));
    let (rows, _) = run(&*session, &sql, &[]).unwrap();
    assert_eq!(rows.len(), 1, "{sql}");
    assert!(block_on(session.table_detail(&TableRef::unqualified("sonde_nope"))).is_err());
}

#[test]
fn connect_should_explain_a_closed_port() {
    let config = local(DbKind::Elasticsearch, 1, "", "");

    let Err(Error::Connect(message)) = block_on(sonde_core::connect(&config, None)) else {
        panic!("nothing listens on port 1");
    };
    assert!(message.contains("nothing answered"), "{message}");
}
