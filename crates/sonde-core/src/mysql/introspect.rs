//! Schemas, tables and their columns, keys and indexes, from
//! `information_schema`. A MySQL database is shown as a schema.

use mysql_async::{Conn, prelude::Queryable as _};

use super::query_error;
use crate::{
    error::{Error, Result},
    schema::{
        Catalog, ColumnInfo, ForeignKey, IndexInfo, Relation, RelationKind, Schema, TableDetail,
        TableRef,
    },
};

/// Databases that belong to the server, not to the user.
const SYSTEM: &str = "('mysql', 'information_schema', 'performance_schema', 'sys')";

pub(super) async fn catalog(conn: &mut Conn, current: &str) -> Result<Catalog> {
    let schemas_sql = format!(
        "SELECT schema_name FROM information_schema.schemata \
         WHERE schema_name NOT IN {SYSTEM} ORDER BY schema_name"
    );
    let names: Vec<String> = conn
        .query(&schemas_sql)
        .await
        .map_err(|error| query_error(error, &schemas_sql))?;
    let relations_sql = format!(
        "SELECT table_schema, table_name, table_type FROM information_schema.tables \
         WHERE table_schema NOT IN {SYSTEM} ORDER BY table_schema, table_name"
    );
    let relations: Vec<(String, String, String)> = conn
        .query(&relations_sql)
        .await
        .map_err(|error| query_error(error, &relations_sql))?;

    let mut schemas: Vec<Schema> = names
        .into_iter()
        .map(|name| Schema {
            name,
            relations: Vec::new(),
        })
        .collect();
    for (schema, name, kind) in relations {
        if let Some(schema) = schemas.iter_mut().find(|s| s.name == schema) {
            schema.relations.push(Relation {
                name,
                kind: if kind.contains("VIEW") {
                    RelationKind::View
                } else {
                    RelationKind::Table
                },
            });
        }
    }
    // The database the connection opened comes first: it is where
    // unqualified names resolve.
    schemas.sort_by_key(|schema| schema.name != current);
    Ok(Catalog { schemas })
}

/// A MariaDB column default as MySQL writes it: MariaDB quotes literal
/// strings and writes a missing default as `NULL`.
fn mariadb_default(default: Option<String>) -> Option<String> {
    let default = default?;
    if default == "NULL" {
        return None;
    }
    match default
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        Some(inner) => Some(inner.replace("''", "'").replace("\\'", "'")),
        None => Some(default),
    }
}

pub(super) async fn table_detail(
    conn: &mut Conn,
    table: &TableRef,
    current: &str,
    mariadb: bool,
) -> Result<TableDetail> {
    let schema = table.schema.clone().unwrap_or_else(|| current.to_owned());
    let key = (schema.clone(), table.name.clone());

    let columns_sql = "SELECT column_name, column_type, is_nullable, column_default, extra \
                       FROM information_schema.columns \
                       WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position";
    let columns: Vec<(String, String, String, Option<String>, String)> = conn
        .exec(columns_sql, key.clone())
        .await
        .map_err(|error| query_error(error, columns_sql))?;
    if columns.is_empty() {
        return Err(Error::query(format!(
            "table \"{schema}\".\"{}\" does not exist",
            table.name
        )));
    }
    // MariaDB's JSON is LONGTEXT with a json_valid() check.
    let mut json_columns: Vec<String> = Vec::new();
    if mariadb {
        let checks_sql = "SELECT check_clause FROM information_schema.check_constraints \
                          WHERE constraint_schema = ? AND table_name = ?";
        let checks: Vec<String> = conn
            .exec(checks_sql, key.clone())
            .await
            .map_err(|error| query_error(error, checks_sql))?;
        for check in checks {
            if let Some(name) = check
                .trim()
                .strip_prefix("json_valid(`")
                .and_then(|rest| rest.strip_suffix("`)"))
            {
                json_columns.push(name.replace("``", "`"));
            }
        }
    }
    let columns = columns
        .into_iter()
        .map(|(name, type_name, nullable, default, extra)| ColumnInfo {
            type_name: if json_columns.contains(&name) {
                "json".to_owned()
            } else {
                type_name
            },
            name,
            nullable: nullable == "YES",
            default: if mariadb {
                mariadb_default(default)
            } else {
                default
            }
            .or_else(|| {
                extra
                    .to_lowercase()
                    .contains("auto_increment")
                    .then(|| "auto_increment".to_owned())
            }),
        })
        .collect();

    let indexes_sql = "SELECT index_name, column_name, non_unique \
                       FROM information_schema.statistics \
                       WHERE table_schema = ? AND table_name = ? \
                       ORDER BY index_name <> 'PRIMARY', index_name, seq_in_index";
    let rows: Vec<(String, Option<String>, i64)> = conn
        .exec(indexes_sql, key.clone())
        .await
        .map_err(|error| query_error(error, indexes_sql))?;
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for (name, column, non_unique) in rows {
        match indexes.last_mut() {
            Some(index) if index.name == name => index.columns.push(column),
            _ => indexes.push(IndexInfo {
                primary: name == "PRIMARY",
                unique: non_unique == 0,
                columns: vec![column],
                name,
            }),
        }
    }
    let primary_key = indexes
        .iter()
        .find(|index| index.primary)
        .map(|index| index.columns.iter().flatten().cloned().collect())
        .unwrap_or_default();

    let keys_sql = "SELECT constraint_name, column_name, referenced_table_schema, \
                    referenced_table_name, referenced_column_name \
                    FROM information_schema.key_column_usage \
                    WHERE table_schema = ? AND table_name = ? \
                    AND referenced_table_name IS NOT NULL \
                    ORDER BY constraint_name, ordinal_position";
    let rows: Vec<(String, String, String, String, String)> = conn
        .exec(keys_sql, key)
        .await
        .map_err(|error| query_error(error, keys_sql))?;
    let mut foreign_keys: Vec<ForeignKey> = Vec::new();
    for (name, column, ref_schema, ref_table, ref_column) in rows {
        match foreign_keys.last_mut() {
            Some(key) if key.name == name => {
                key.columns.push(column);
                key.referenced_columns.push(ref_column);
            }
            _ => foreign_keys.push(ForeignKey {
                name,
                columns: vec![column],
                references: TableRef::new(ref_schema, ref_table),
                referenced_columns: vec![ref_column],
            }),
        }
    }

    Ok(TableDetail {
        columns,
        primary_key,
        foreign_keys,
        indexes,
        has_rowid: false,
    })
}
