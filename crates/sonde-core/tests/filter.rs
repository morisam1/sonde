//! Built filters and summaries run against every database fixture in
//! `tests/docker-compose.yml`: each condition the builder offers has to be
//! accepted, and keep exactly the rows it says.

mod common;

use common::{local, open, run, setup, table};
use sonde_core::{
    ConnectionConfig, DbKind, Session, TableRef, Value,
    filter::{ANY_FIELD, Bucket, Filter, Measure, Op, Rule, Summary, condition, summary},
};

/// The rows every fixture gets:
///
/// | id | name     | n    | at                  | flag  |
/// |----|----------|------|---------------------|-------|
/// | 1  | Alpha    | 1.5  | 2024-01-01 10:00:00 | true  |
/// | 2  | beta_50% | 20   | 2024-02-29 12:00:00 | false |
/// | 3  | Gamma    | NULL | yesterday           | true  |
/// | 4  | NULL     | 300  | NULL                | NULL  |
fn yesterday() -> String {
    (chrono::Utc::now() - chrono::Duration::days(1))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// A rule, its expected ids, and the engines it is skipped on.
struct Case {
    rule: Rule,
    ids: &'static [i64],
}

fn case(column: &str, op: Op, value: &str, value2: &str, ids: &'static [i64]) -> Case {
    let mut rule = Rule::new(column, "", op, value);
    rule.value2 = value2.into();
    Case { rule, ids }
}

fn cases(kind: DbKind) -> Vec<Case> {
    let mut cases = vec![
        case("name", Op::Is, "Alpha", "", &[1]),
        case("name", Op::OneOf, "Alpha, Gamma", "", &[1, 3]),
        case("n", Op::Is, "20", "", &[2]),
        case("n", Op::Greater, "1.5", "", &[2, 4]),
        case("n", Op::AtLeast, "20", "", &[2, 4]),
        case("n", Op::Less, "20", "", &[1]),
        case("n", Op::AtMost, "20", "", &[1, 2]),
        case("n", Op::Between, "1", "20", &[1, 2]),
        case("n", Op::OneOf, "20, 300", "", &[2, 4]),
        case("at", Op::Is, "2024-02-29", "", &[2]),
        case("at", Op::Before, "2024-02-01", "", &[1]),
        case("at", Op::After, "2024-02-01", "", &[2, 3]),
        case("at", Op::Between, "2024-01-01", "2024-03-01", &[1, 2]),
        case("at", Op::InLastDays, "7", "", &[3]),
        case("flag", Op::IsTrue, "", "", &[1, 3]),
        case("flag", Op::IsFalse, "", "", &[2]),
    ];
    if kind == DbKind::Cassandra {
        return cases;
    }
    // MongoDB's $ne, and Elasticsearch's negations, also keep documents
    // that lack the field.
    let missing_counts = matches!(kind, DbKind::Mongodb | DbKind::Elasticsearch);
    let not_alpha: &[i64] = if missing_counts { &[2, 3, 4] } else { &[2, 3] };
    cases.extend([
        case("name", Op::IsNot, "Alpha", "", not_alpha),
        case("name", Op::Contains, "a", "", &[1, 2, 3]),
        case(
            "name",
            Op::NotContains,
            "eta",
            "",
            if missing_counts { &[1, 3, 4] } else { &[1, 3] },
        ),
        case("name", Op::StartsWith, "Gam", "", &[3]),
        case("name", Op::IsEmpty, "", "", &[4]),
        case("name", Op::IsNotEmpty, "", "", &[1, 2, 3]),
        case("n", Op::IsEmpty, "", "", &[3]),
        case("at", Op::IsNotEmpty, "", "", &[1, 2, 3]),
        case("flag", Op::IsEmpty, "", "", &[4]),
    ]);
    if kind != DbKind::Elasticsearch {
        // Elasticsearch SQL's LIKE has no escape, so `%` is a wildcard.
        cases.extend([
            case("name", Op::Contains, "50%", "", &[2]),
            case("name", Op::EndsWith, "%", "", &[2]),
            case("name", Op::Contains, "a_5", "", &[2]),
        ]);
    } else {
        cases.push(case(ANY_FIELD, Op::Matches, "Alpha", "", &[1]));
        cases.push(case("name", Op::Matches, "Gamma", "", &[3]));
    }
    cases
}

fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
    let mut ids: Vec<i64> = rows
        .iter()
        .map(|row| match &row[0] {
            Value::Int(n) => *n,
            Value::Float(n) => *n as i64,
            Value::Decimal(d) => d.to_string().parse::<f64>().unwrap() as i64,
            other => panic!("id {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// Run every case and every summary on `t`, and report each failure.
fn check(session: &dyn Session, kind: DbKind, t: &TableRef) {
    let detail = futures::executor::block_on(session.table_detail(t)).unwrap();
    let type_of = |column: &str| {
        detail
            .columns
            .iter()
            .find(|c| c.name == column)
            .map(|c| c.type_name.clone())
            .unwrap_or_default()
    };
    let dialect = sonde_core::dialect(kind);
    let mut failures = Vec::new();
    for mut case in cases(kind) {
        case.rule.type_name = type_of(&case.rule.column);
        let filter = Filter {
            rules: vec![case.rule.clone()],
            any: false,
        };
        let text = condition(kind, &filter).unwrap();
        let sql = dialect.browse(t, &["id".to_owned()], &text, None);
        match table(session, &sql) {
            Ok((_, rows)) if ids(&rows) == case.ids => {}
            Ok((_, rows)) => failures.push(format!(
                "{} {} {:?}: got {:?}, want {:?}\n  {sql}",
                case.rule.column,
                case.rule.op.label(),
                case.rule.value,
                ids(&rows),
                case.ids
            )),
            Err(error) => failures.push(format!(
                "{} {} {:?}: {error}\n  {sql}",
                case.rule.column,
                case.rule.op.label(),
                case.rule.value
            )),
        }
    }

    if sonde_core::filter::supports_any(kind) {
        let filter = Filter {
            rules: vec![
                Rule::new("name", &type_of("name"), Op::Is, "Alpha"),
                Rule::new("n", &type_of("n"), Op::Is, "300"),
            ],
            any: true,
        };
        let sql = dialect.browse(
            t,
            &["id".to_owned()],
            &condition(kind, &filter).unwrap(),
            None,
        );
        match table(session, &sql) {
            Ok((_, rows)) if ids(&rows) == [1, 4] => {}
            other => failures.push(format!("any: {other:?}\n  {sql}")),
        }
    }

    if sonde_core::filter::summarizes(kind) {
        let flag = condition(
            kind,
            &Filter {
                rules: vec![Rule::new("n", &type_of("n"), Op::IsNotEmpty, "")],
                any: false,
            },
        )
        .unwrap();
        let shapes = [
            (
                Summary {
                    group: None,
                    bucket: Bucket::Value,
                    measure: Measure::Sum,
                    of: Some("n".into()),
                },
                "",
                1,
            ),
            (
                Summary {
                    group: Some("flag".into()),
                    bucket: Bucket::Value,
                    measure: Measure::Count,
                    of: None,
                },
                "",
                3,
            ),
            (
                Summary {
                    group: Some("at".into()),
                    bucket: Bucket::Month,
                    measure: Measure::Average,
                    of: Some("n".into()),
                },
                flag.as_str(),
                3,
            ),
            (
                Summary {
                    group: Some("at".into()),
                    bucket: Bucket::Year,
                    measure: Measure::Max,
                    of: Some("n".into()),
                },
                "",
                3,
            ),
            (
                Summary {
                    group: Some("at".into()),
                    bucket: Bucket::Day,
                    measure: Measure::Min,
                    of: Some("n".into()),
                },
                "",
                4,
            ),
        ];
        for (shape, filter, groups) in shapes {
            let sql = summary(kind, t, filter, &shape).unwrap();
            match table(session, &sql) {
                Ok((columns, rows))
                    if rows.len() == groups
                        && columns.len() == 2 - usize::from(shape.group.is_none()) =>
                {
                    if shape.group.is_none() {
                        let total = rows[0][0].to_string().parse::<f64>().unwrap_or_default();
                        if (total - 321.5).abs() > 0.001 {
                            failures.push(format!("sum: got {total}\n  {sql}"));
                        }
                    }
                }
                Ok((columns, rows)) => {
                    failures.push(format!("summary {shape:?}: {columns:?} {rows:?}\n  {sql}"))
                }
                Err(error) => failures.push(format!("summary {shape:?}: {error}\n  {sql}")),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} on {}:\n{}",
        failures.len(),
        kind.name(),
        failures.join("\n")
    );
}

/// The four rows as SQL values, for the engines that take an INSERT.
fn sql_rows(true_: &str, false_: &str) -> Vec<String> {
    let at = yesterday();
    vec![
        format!("(1, 'Alpha', 1.5, '2024-01-01 10:00:00', {true_})"),
        format!("(2, 'beta_50%', 20, '2024-02-29 12:00:00', {false_})"),
        format!("(3, 'Gamma', NULL, '{at}', {true_})"),
        "(4, NULL, 300, NULL, NULL)".to_owned(),
    ]
}

fn sql_fixture(
    session: &dyn Session,
    kind: DbKind,
    t: &TableRef,
    create: &str,
    true_: &str,
    false_: &str,
) {
    let name = sonde_core::dialect(kind).quote_table(t);
    let _ = run(session, &format!("DROP TABLE {name}"), &[]);
    setup(session, &[&format!("CREATE TABLE {name} {create}")]);
    for row in sql_rows(true_, false_) {
        setup(
            session,
            &[&format!(
                "INSERT INTO {name} (id, name, n, at, flag) VALUES {row}"
            )],
        );
    }
    check(session, kind, t);
    let _ = run(session, &format!("DROP TABLE {name}"), &[]);
}

#[test]
fn postgres_filters_and_summaries_should_keep_the_right_rows() {
    let session = common::postgres();
    sql_fixture(
        &*session,
        DbKind::Postgres,
        &TableRef::new("public", "sonde_filter_t"),
        "(id int primary key, name text, n numeric(10,2), at timestamp, flag boolean)",
        "true",
        "false",
    );
}

#[test]
fn mysql_filters_and_summaries_should_keep_the_right_rows() {
    let session = common::mysql();
    let kind = common::mysql_config().kind();
    let database = common::mysql_config().server().unwrap().database.clone();
    sql_fixture(
        &*session,
        kind,
        &TableRef::new(database, "sonde_filter_t"),
        "(id int primary key, name varchar(50), n decimal(10,2), at datetime, flag tinyint(1))",
        "1",
        "0",
    );
}

#[test]
fn sqlserver_filters_and_summaries_should_keep_the_right_rows() {
    let session = common::mssql();
    sql_fixture(
        &*session,
        DbKind::SqlServer,
        &TableRef::new("dbo", "sonde_filter_t"),
        "(id int primary key, name nvarchar(50), n decimal(10,2), at datetime2, flag bit)",
        "1",
        "0",
    );
}

#[test]
fn sqlite_filters_and_summaries_should_keep_the_right_rows() {
    let session = common::sqlite();
    sql_fixture(
        &*session,
        DbKind::Sqlite,
        &TableRef::new("main", "sonde_filter_t"),
        "(id integer primary key, name text, n real, at timestamp, flag boolean)",
        "1",
        "0",
    );
}

#[test]
fn cassandra_filters_should_keep_the_right_rows() {
    let session = open(&local(DbKind::Cassandra, 19042, "", ""), None);
    setup(
        &*session,
        &[
            "CREATE KEYSPACE IF NOT EXISTS sonde_filter WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}",
            "DROP TABLE IF EXISTS sonde_filter.t",
            "CREATE TABLE sonde_filter.t (id int PRIMARY KEY, name text, n decimal, at timestamp, flag boolean)",
        ],
    );
    let at = yesterday();
    for row in [
        "(1, 'Alpha', 1.5, '2024-01-01 10:00:00+0000', true)".to_owned(),
        "(2, 'beta_50%', 20, '2024-02-29 12:00:00+0000', false)".to_owned(),
        format!("(3, 'Gamma', null, '{at}+0000', true)"),
        "(4, null, 300, null, null)".to_owned(),
    ] {
        setup(
            &*session,
            &[&format!(
                "INSERT INTO sonde_filter.t (id, name, n, at, flag) VALUES {row}"
            )],
        );
    }
    check(
        &*session,
        DbKind::Cassandra,
        &TableRef::new("sonde_filter", "t"),
    );
    setup(&*session, &["DROP KEYSPACE sonde_filter"]);
}

#[test]
fn mongodb_filters_and_summaries_should_keep_the_right_rows() {
    let mut config = local(DbKind::Mongodb, 27019, "sonde_filter", "");
    if let ConnectionConfig::Mongodb(server) = &mut config {
        server
            .options
            .insert("options".into(), "directConnection=true".into());
    }
    let session = open(&config, None);
    let at = yesterday().replacen(' ', "T", 1);
    let _ = run(&*session, "db.t.drop()", &[]);
    setup(
        &*session,
        &[&format!(
            "db.t.insertMany([\
             {{id: 1, name: \"Alpha\", n: 1.5, at: ISODate(\"2024-01-01T10:00:00Z\"), flag: true}}, \
             {{id: 2, name: \"beta_50%\", n: 20.0, at: ISODate(\"2024-02-29T12:00:00Z\"), flag: false}}, \
             {{id: 3, name: \"Gamma\", n: null, at: ISODate(\"{at}Z\"), flag: true}}, \
             {{id: 4, name: null, n: 300.0, at: null, flag: null}}])"
        )],
    );
    check(
        &*session,
        DbKind::Mongodb,
        &TableRef::new("sonde_filter", "t"),
    );
    let _ = run(&*session, "db.dropDatabase()", &[]);
}

#[test]
fn elasticsearch_filters_and_summaries_should_keep_the_right_rows() {
    let session = open(&local(DbKind::Elasticsearch, 19200, "", ""), None);
    let _ = run(&*session, "DELETE /sonde_filter", &[]);
    setup(
        &*session,
        &[
            "PUT /sonde_filter\n{\"mappings\": {\"properties\": {\"id\": {\"type\": \"long\"}, \
           \"name\": {\"type\": \"keyword\"}, \"n\": {\"type\": \"double\"}, \
           \"at\": {\"type\": \"date\", \"format\": \"yyyy-MM-dd HH:mm:ss\"}, \"flag\": {\"type\": \"boolean\"}}}}",
        ],
    );
    let at = yesterday();
    let mut bulk = "POST /sonde_filter/_bulk?refresh=true\n".to_owned();
    for doc in [
        "{\"id\": 1, \"name\": \"Alpha\", \"n\": 1.5, \"at\": \"2024-01-01 10:00:00\", \"flag\": true}".to_owned(),
        "{\"id\": 2, \"name\": \"beta_50%\", \"n\": 20, \"at\": \"2024-02-29 12:00:00\", \"flag\": false}".to_owned(),
        format!("{{\"id\": 3, \"name\": \"Gamma\", \"at\": \"{at}\", \"flag\": true}}"),
        "{\"id\": 4, \"n\": 300}".to_owned(),
    ] {
        bulk.push_str("{\"index\": {}}\n");
        bulk.push_str(&doc);
        bulk.push('\n');
    }
    setup(&*session, &[&bulk]);
    check(
        &*session,
        DbKind::Elasticsearch,
        &TableRef::unqualified("sonde_filter"),
    );
    let _ = run(&*session, "DELETE /sonde_filter", &[]);
}
