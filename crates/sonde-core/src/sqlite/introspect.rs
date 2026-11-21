//! Catalog queries through `sqlite_schema` and the pragma table functions.

use rusqlite::{Connection, params};

use super::{SqliteDialect, query_error};
use crate::{
    driver::Dialect as _,
    error::{Error, Result},
    schema::{
        Catalog, ColumnInfo, ForeignKey, IndexInfo, Relation, RelationKind, Schema, TableDetail,
        TableRef,
    },
};

pub(super) fn catalog(connection: &Connection) -> rusqlite::Result<Catalog> {
    let databases: Vec<String> = connection
        .prepare("select name from pragma_database_list order by seq")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let schemas = databases
        .into_iter()
        .map(|name| {
            let sql = format!(
                "select name, type from {}.sqlite_schema \
                 where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\' \
                 order by name",
                SqliteDialect.quote_ident(&name)
            );
            let relations = connection
                .prepare(&sql)?
                .query_map([], |row| {
                    let kind: String = row.get(1)?;
                    Ok(Relation {
                        name: row.get(0)?,
                        kind: if kind == "view" {
                            RelationKind::View
                        } else {
                            RelationKind::Table
                        },
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(Schema { name, relations })
        })
        .collect::<rusqlite::Result<_>>()?;
    Ok(Catalog { schemas })
}

pub(super) fn table_detail(connection: &Connection, table: &TableRef) -> Result<TableDetail> {
    detail(connection, table)
        .map_err(query_error)?
        .ok_or_else(|| Error::query(format!("no such table: {}", table.name)))
}

fn detail(connection: &Connection, table: &TableRef) -> rusqlite::Result<Option<TableDetail>> {
    // The pragma functions take the schema as an optional second argument;
    // passing NULL is the same as leaving it out.
    let (name, schema) = (&table.name, &table.schema);

    let mut keyed: Vec<(i64, String)> = Vec::new();
    let columns: Vec<ColumnInfo> = connection
        .prepare(
            "select name, type, \"notnull\", dflt_value, pk \
             from pragma_table_info(?1, ?2) order by cid",
        )?
        .query_map(params![name, schema], |row| {
            let column = ColumnInfo {
                name: row.get(0)?,
                type_name: row.get(1)?,
                nullable: !row.get::<_, bool>(2)?,
                default: row.get(3)?,
            };
            let ordinal: i64 = row.get(4)?;
            if ordinal > 0 {
                keyed.push((ordinal, column.name.clone()));
            }
            Ok(column)
        })?
        .collect::<rusqlite::Result<_>>()?;
    if columns.is_empty() {
        return Ok(None);
    }
    keyed.sort_unstable();

    let listed: Vec<(String, bool, String)> = connection
        .prepare("select name, \"unique\", origin from pragma_index_list(?1, ?2) order by seq")?
        .query_map(params![name, schema], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let indexes = listed
        .into_iter()
        .map(|(index, unique, origin)| {
            let columns = connection
                .prepare("select name from pragma_index_info(?1, ?2) order by seqno")?
                .query_map(params![index, schema], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(IndexInfo {
                name: index,
                columns,
                unique,
                primary: origin == "pk",
            })
        })
        .collect::<rusqlite::Result<_>>()?;

    let mut foreign_keys: Vec<ForeignKey> = Vec::new();
    let mut statement = connection.prepare(
        "select id, \"table\", \"from\", \"to\" \
         from pragma_foreign_key_list(?1, ?2) order by id, seq",
    )?;
    let mut rows = statement.query(params![name, schema])?;
    while let Some(row) = rows.next()? {
        let key = format!("fk_{}", row.get::<_, i64>(0)?);
        if foreign_keys.last().is_none_or(|last| last.name != key) {
            foreign_keys.push(ForeignKey {
                name: key,
                columns: Vec::new(),
                references: TableRef {
                    schema: schema.clone(),
                    name: row.get(1)?,
                },
                referenced_columns: Vec::new(),
            });
        }
        if let Some(key) = foreign_keys.last_mut() {
            key.columns.push(row.get(2)?);
            // NULL when the key points at the parent's primary key implicitly.
            if let Some(to) = row.get::<_, Option<String>>(3)? {
                key.referenced_columns.push(to);
            }
        }
    }

    // Views and WITHOUT ROWID tables have no rowid to select.
    let has_rowid = connection
        .prepare(&format!(
            "select rowid from {} limit 0",
            SqliteDialect.quote_table(table)
        ))
        .is_ok();

    Ok(Some(TableDetail {
        columns,
        primary_key: keyed.into_iter().map(|(_, name)| name).collect(),
        foreign_keys,
        indexes,
        has_rowid,
    }))
}
