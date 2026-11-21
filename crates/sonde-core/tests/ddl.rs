//! Structure changes run against every database fixture in
//! `tests/docker-compose.yml`: each engine creates, renames, empties and
//! drops, and the catalog has to agree after each step.

mod common;

use common::{local, open, run, setup};
use futures::executor::block_on;
use sonde_core::{
    ConnectionConfig, DbKind, Session, TableRef,
    ddl::{Change, KeyType, NewColumn, statements},
};

fn apply(session: &dyn Session, kind: DbKind, change: &Change) {
    for sql in statements(kind, change).unwrap_or_else(|e| panic!("{change:?}: {e}")) {
        run(session, &sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    }
}

fn tables(session: &dyn Session, schema: &str) -> Vec<String> {
    let catalog = block_on(session.catalog()).unwrap();
    let mut names: Vec<String> = catalog
        .schemas
        .iter()
        .filter(|s| s.name == schema)
        .flat_map(|s| s.relations.iter().map(|r| r.name.clone()))
        .collect();
    names.sort();
    names
}

fn columns(session: &dyn Session, table: &TableRef) -> Vec<String> {
    block_on(session.table_detail(table))
        .unwrap()
        .columns
        .into_iter()
        .map(|c| c.name)
        .collect()
}

fn new_columns(kind: DbKind) -> Vec<NewColumn> {
    let int = match kind {
        DbKind::Elasticsearch => "long",
        DbKind::Sqlite => "INTEGER",
        _ => "int",
    };
    vec![
        NewColumn {
            name: "id".into(),
            type_name: int.into(),
            nullable: false,
            primary_key: true,
        },
        NewColumn {
            name: "name".into(),
            type_name: sonde_core::ddl::default_type(kind).into(),
            nullable: true,
            primary_key: false,
        },
    ]
}

/// Create a table, add, rename and drop a column, rename, empty and drop
/// the table, checking the catalog along the way.
fn table_round_trip(session: &dyn Session, kind: DbKind, schema: &str) {
    let ops = |op| sonde_core::ddl::supports(kind, op);
    use sonde_core::ddl::Op;
    let table = TableRef::new(schema, "sonde_ddl_t");
    let _ = run(
        session,
        &statements(
            kind,
            &Change::DropTable(TableRef::new(schema, "sonde_ddl_u")),
        )
        .unwrap()[0],
        &[],
    );
    let _ = run(
        session,
        &statements(kind, &Change::DropTable(table.clone())).unwrap()[0],
        &[],
    );

    apply(
        session,
        kind,
        &Change::CreateTable {
            table: table.clone(),
            columns: new_columns(kind),
        },
    );
    if kind == DbKind::Mongodb {
        setup(
            session,
            &[&format!(
                "db.getSiblingDB('{schema}').sonde_ddl_t.insertOne({{id: 1, name: 'a'}})"
            )],
        );
    }
    assert!(
        tables(session, schema).contains(&"sonde_ddl_t".to_owned()),
        "{kind:?} create"
    );

    apply(
        session,
        kind,
        &Change::AddColumn {
            table: table.clone(),
            column: NewColumn {
                name: "extra".into(),
                type_name: sonde_core::ddl::default_type(kind).into(),
                nullable: true,
                primary_key: false,
            },
        },
    );
    assert!(
        columns(session, &table).contains(&"extra".to_owned()),
        "{kind:?} add column"
    );

    if ops(Op::RenameColumn) {
        apply(
            session,
            kind,
            &Change::RenameColumn {
                table: table.clone(),
                column: "extra".into(),
                to: "more".into(),
            },
        );
        assert!(
            columns(session, &table).contains(&"more".to_owned()),
            "{kind:?} rename column"
        );
    }
    if ops(Op::DropColumn) {
        let column = if ops(Op::RenameColumn) {
            "more"
        } else {
            "extra"
        };
        apply(
            session,
            kind,
            &Change::DropColumn {
                table: table.clone(),
                column: column.into(),
            },
        );
        assert!(
            !columns(session, &table).iter().any(|c| c == column),
            "{kind:?} drop column"
        );
    }

    apply(session, kind, &Change::EmptyTable(table.clone()));

    let table = if ops(Op::RenameTable) {
        apply(
            session,
            kind,
            &Change::RenameTable {
                table: table.clone(),
                to: "sonde_ddl_u".into(),
            },
        );
        assert!(
            tables(session, schema).contains(&"sonde_ddl_u".to_owned()),
            "{kind:?} rename"
        );
        TableRef::new(schema, "sonde_ddl_u")
    } else {
        table
    };
    apply(session, kind, &Change::DropTable(table.clone()));
    assert!(
        !tables(session, schema).contains(&table.name),
        "{kind:?} drop"
    );
}

#[test]
fn postgres_should_manage_schemas_and_tables() {
    let session = common::postgres();
    setup(&*session, &["DROP SCHEMA IF EXISTS sonde_ddl CASCADE"]);
    apply(
        &*session,
        DbKind::Postgres,
        &Change::CreateSchema("sonde_ddl".into()),
    );
    table_round_trip(&*session, DbKind::Postgres, "sonde_ddl");
    apply(
        &*session,
        DbKind::Postgres,
        &Change::DropSchema("sonde_ddl".into()),
    );
    let catalog = block_on(session.catalog()).unwrap();
    assert!(catalog.schemas.iter().all(|s| s.name != "sonde_ddl"));
}

#[test]
fn mysql_should_manage_databases_and_tables() {
    let session = common::mysql();
    let kind = common::mysql_config().kind();
    setup(&*session, &["DROP DATABASE IF EXISTS sonde_ddl"]);
    apply(
        &*session,
        kind,
        &Change::CreateDatabase {
            name: "sonde_ddl".into(),
            first: None,
        },
    );
    table_round_trip(&*session, kind, "sonde_ddl");
    apply(&*session, kind, &Change::DropDatabase("sonde_ddl".into()));
}

#[test]
fn sqlserver_should_manage_schemas_and_tables() {
    let session = common::mssql();
    setup(
        &*session,
        &["IF SCHEMA_ID('sonde_ddl') IS NOT NULL EXEC('DROP SCHEMA sonde_ddl')"],
    );
    let _ = run(&*session, "DROP TABLE IF EXISTS sonde_ddl.sonde_ddl_t", &[]);
    apply(
        &*session,
        DbKind::SqlServer,
        &Change::CreateSchema("sonde_ddl".into()),
    );
    table_round_trip(&*session, DbKind::SqlServer, "sonde_ddl");
    apply(
        &*session,
        DbKind::SqlServer,
        &Change::DropSchema("sonde_ddl".into()),
    );
}

#[test]
fn sqlite_should_manage_tables() {
    let session = common::sqlite();
    table_round_trip(&*session, DbKind::Sqlite, "main");
}

#[test]
fn mongodb_should_manage_databases_collections_and_fields() {
    let mut config = local(DbKind::Mongodb, 27019, "test", "");
    if let ConnectionConfig::Mongodb(server) = &mut config {
        server
            .options
            .insert("options".into(), "directConnection=true".into());
    }
    let session = open(&config, None);
    let _ = run(
        &*session,
        "db.getSiblingDB('sonde_ddl').dropDatabase()",
        &[],
    );
    apply(
        &*session,
        DbKind::Mongodb,
        &Change::CreateDatabase {
            name: "sonde_ddl".into(),
            first: Some("first".into()),
        },
    );
    assert_eq!(tables(&*session, "sonde_ddl"), ["first"]);
    table_round_trip(&*session, DbKind::Mongodb, "sonde_ddl");
    apply(
        &*session,
        DbKind::Mongodb,
        &Change::DropDatabase("sonde_ddl".into()),
    );
}

#[test]
fn cassandra_should_manage_keyspaces_and_tables() {
    let session = open(&local(DbKind::Cassandra, 19042, "", ""), None);
    setup(&*session, &["DROP KEYSPACE IF EXISTS sonde_ddl"]);
    apply(
        &*session,
        DbKind::Cassandra,
        &Change::CreateDatabase {
            name: "sonde_ddl".into(),
            first: None,
        },
    );
    table_round_trip(&*session, DbKind::Cassandra, "sonde_ddl");
    apply(
        &*session,
        DbKind::Cassandra,
        &Change::DropDatabase("sonde_ddl".into()),
    );
}

#[test]
fn elasticsearch_should_manage_indices_and_fields() {
    let session = open(&local(DbKind::Elasticsearch, 19200, "", ""), None);
    let cluster = block_on(session.catalog()).unwrap().schemas[0].name.clone();
    table_round_trip(&*session, DbKind::Elasticsearch, &cluster);
}

#[test]
fn redis_should_create_rename_and_delete_keys() {
    let session = open(&local(DbKind::Redis, 63799, "11", ""), None);
    apply(&*session, DbKind::Redis, &Change::FlushDatabase);
    apply(
        &*session,
        DbKind::Redis,
        &Change::CreateKey {
            key: "user:1".into(),
            kind: KeyType::Hash,
            value: "name ada".into(),
        },
    );
    assert_eq!(tables(&*session, "db11"), ["user:1"]);
    apply(
        &*session,
        DbKind::Redis,
        &Change::RenameTable {
            table: TableRef::new("db11", "user:1"),
            to: "user:2".into(),
        },
    );
    assert_eq!(tables(&*session, "db11"), ["user:2"]);
    apply(
        &*session,
        DbKind::Redis,
        &Change::DropTable(TableRef::new("db11", "user:2")),
    );
    assert!(tables(&*session, "db11").is_empty());
}

#[test]
fn database_lists_should_follow_creates_and_drops() {
    let session = common::postgres();
    let _ = run(&*session, "DROP DATABASE IF EXISTS sonde_ddl_db", &[]);
    assert!(
        block_on(session.databases())
            .unwrap()
            .contains(&"sonde".to_owned())
    );
    apply(
        &*session,
        DbKind::Postgres,
        &Change::CreateDatabase {
            name: "sonde_ddl_db".into(),
            first: None,
        },
    );
    assert!(
        block_on(session.databases())
            .unwrap()
            .contains(&"sonde_ddl_db".to_owned())
    );

    // A session opened through the listed name reaches that database.
    let other = block_on(sonde_core::connect(
        &common::postgres_config().with_database("sonde_ddl_db"),
        Some(sonde_core::Secret::new("sonde")),
    ))
    .unwrap();
    assert_eq!(
        common::scalar(&*other, "SELECT current_database()"),
        sonde_core::Value::text("sonde_ddl_db")
    );
    drop(other);

    apply(
        &*session,
        DbKind::Postgres,
        &Change::DropDatabase("sonde_ddl_db".into()),
    );
    assert!(
        !block_on(session.databases())
            .unwrap()
            .contains(&"sonde_ddl_db".to_owned())
    );

    let mssql = common::mssql();
    assert!(
        block_on(mssql.databases())
            .unwrap()
            .contains(&"master".to_owned())
    );

    let redis = open(&local(DbKind::Redis, 63799, "12", ""), None);
    setup(&*redis, &["SET listed 1"]);
    assert!(
        block_on(redis.databases())
            .unwrap()
            .contains(&"db12".to_owned())
    );
    let twelve = open(
        &local(DbKind::Redis, 63799, "0", "").with_database("db12"),
        None,
    );
    assert_eq!(
        common::scalar(&*twelve, "GET listed"),
        sonde_core::Value::text("1")
    );
}
