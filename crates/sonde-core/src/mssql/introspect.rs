//! Schemas, tables and their columns, keys and indexes, from SQL Server's
//! `sys` catalog views, for the database the connection opened.

use tiberius::{Query, Row};

use super::{Conn, query_error};
use crate::{
    error::{Error, Result},
    schema::{
        Catalog, ColumnInfo, ForeignKey, IndexInfo, Relation, RelationKind, Schema, TableDetail,
        TableRef,
    },
};

/// The object id of the table named by `@P1` (schema) and `@P2` (name).
const OBJECT: &str = "OBJECT_ID(QUOTENAME(@P1) + '.' + QUOTENAME(@P2))";

async fn rows(conn: &mut Conn, sql: &str, table: Option<(&str, &str)>) -> Result<Vec<Row>> {
    let mut query = Query::new(sql.to_owned());
    if let Some((schema, name)) = table {
        query.bind(schema.to_owned());
        query.bind(name.to_owned());
    }
    query
        .query(conn)
        .await
        .map_err(|error| query_error(error, sql))?
        .into_first_result()
        .await
        .map_err(|error| query_error(error, sql))
}

fn text(row: &Row, ix: usize) -> String {
    row.get::<&str, _>(ix).unwrap_or_default().to_owned()
}

pub(super) async fn catalog(conn: &mut Conn) -> Result<Catalog> {
    let schemas_sql = "SELECT name FROM sys.schemas \
                       WHERE schema_id < 16384 AND name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') \
                       ORDER BY CASE WHEN name = 'dbo' THEN 0 ELSE 1 END, name";
    let mut schemas: Vec<Schema> = rows(conn, schemas_sql, None)
        .await?
        .iter()
        .map(|row| Schema {
            name: text(row, 0),
            relations: Vec::new(),
        })
        .collect();
    let relations_sql = "SELECT s.name, o.name, o.type FROM sys.objects o \
                         JOIN sys.schemas s ON s.schema_id = o.schema_id \
                         WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
                         ORDER BY s.name, o.name";
    for row in rows(conn, relations_sql, None).await? {
        let schema = text(&row, 0);
        if let Some(schema) = schemas.iter_mut().find(|s| s.name == schema) {
            schema.relations.push(Relation {
                name: text(&row, 1),
                kind: if text(&row, 2).trim() == "V" {
                    RelationKind::View
                } else {
                    RelationKind::Table
                },
            });
        }
    }
    Ok(Catalog { schemas })
}

/// A column's type the way it is written in DDL, such as `nvarchar(50)`.
fn spelled(type_name: &str, max_length: i16, precision: u8, scale: u8) -> String {
    let length = |chars: i16| {
        if max_length == -1 {
            "max".to_owned()
        } else {
            chars.to_string()
        }
    };
    match type_name {
        "varchar" | "char" | "varbinary" | "binary" => {
            format!("{type_name}({})", length(max_length))
        }
        "nvarchar" | "nchar" => format!("{type_name}({})", length(max_length / 2)),
        "decimal" | "numeric" => format!("{type_name}({precision},{scale})"),
        _ => type_name.to_owned(),
    }
}

/// A default constraint's definition without the parentheses SQL Server
/// wraps it in: `((0))` is `0`.
fn unwrapped(definition: &str) -> String {
    let mut text = definition.trim();
    while text.len() >= 2 && text.starts_with('(') && text.ends_with(')') {
        text = &text[1..text.len() - 1];
    }
    text.to_owned()
}

pub(super) async fn table_detail(conn: &mut Conn, table: &TableRef) -> Result<TableDetail> {
    let schema = table.schema.as_deref().unwrap_or("dbo");
    let key = Some((schema, table.name.as_str()));

    let columns_sql = format!(
        "SELECT c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable, \
                dc.definition, c.is_identity \
         FROM sys.columns c \
         JOIN sys.types t ON t.user_type_id = c.user_type_id \
         LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
         WHERE c.object_id = {OBJECT} ORDER BY c.column_id"
    );
    let columns: Vec<ColumnInfo> = rows(conn, &columns_sql, key)
        .await?
        .iter()
        .map(|row| ColumnInfo {
            name: text(row, 0),
            type_name: spelled(
                &text(row, 1),
                row.get(2).unwrap_or_default(),
                row.get(3).unwrap_or_default(),
                row.get(4).unwrap_or_default(),
            ),
            nullable: row.get(5).unwrap_or(true),
            default: row.get::<&str, _>(6).map(unwrapped).or_else(|| {
                row.get::<bool, _>(7)
                    .filter(|&id| id)
                    .map(|_| "identity".to_owned())
            }),
        })
        .collect();
    if columns.is_empty() {
        return Err(Error::query(format!(
            "table [{schema}].[{}] does not exist",
            table.name
        )));
    }

    let indexes_sql = format!(
        "SELECT i.name, c.name, i.is_unique, i.is_primary_key FROM sys.indexes i \
         JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
              AND ic.is_included_column = 0 \
         JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE i.object_id = {OBJECT} AND i.name IS NOT NULL \
         ORDER BY i.is_primary_key DESC, i.name, ic.key_ordinal"
    );
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for row in rows(conn, &indexes_sql, key).await? {
        let name = text(&row, 0);
        let column = Some(text(&row, 1));
        match indexes.last_mut() {
            Some(index) if index.name == name => index.columns.push(column),
            _ => indexes.push(IndexInfo {
                name,
                columns: vec![column],
                unique: row.get(2).unwrap_or(false),
                primary: row.get(3).unwrap_or(false),
            }),
        }
    }
    let primary_key = indexes
        .iter()
        .find(|index| index.primary)
        .map(|index| index.columns.iter().flatten().cloned().collect())
        .unwrap_or_default();

    let keys_sql = format!(
        "SELECT fk.name, pc.name, SCHEMA_NAME(rt.schema_id), rt.name, rc.name \
         FROM sys.foreign_keys fk \
         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id \
              AND pc.column_id = fkc.parent_column_id \
         JOIN sys.tables rt ON rt.object_id = fkc.referenced_object_id \
         JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
              AND rc.column_id = fkc.referenced_column_id \
         WHERE fk.parent_object_id = {OBJECT} \
         ORDER BY fk.name, fkc.constraint_column_id"
    );
    let mut foreign_keys: Vec<ForeignKey> = Vec::new();
    for row in rows(conn, &keys_sql, key).await? {
        let name = text(&row, 0);
        match foreign_keys.last_mut() {
            Some(key) if key.name == name => {
                key.columns.push(text(&row, 1));
                key.referenced_columns.push(text(&row, 4));
            }
            _ => foreign_keys.push(ForeignKey {
                name,
                columns: vec![text(&row, 1)],
                references: TableRef::new(text(&row, 2), text(&row, 3)),
                referenced_columns: vec![text(&row, 4)],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spelled_should_write_lengths_like_ddl() {
        assert_eq!(spelled("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(spelled("varbinary", -1, 0, 0), "varbinary(max)");
        assert_eq!(spelled("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(spelled("int", 4, 10, 0), "int");
    }

    #[test]
    fn unwrapped_should_drop_the_parentheses() {
        assert_eq!(unwrapped("((0))"), "0");
        assert_eq!(unwrapped("(N'x')"), "N'x'");
        assert_eq!(unwrapped("(getdate())"), "getdate()");
    }
}
