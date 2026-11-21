//! Catalog queries, straight against `pg_catalog`.
//!
//! `information_schema` is avoided: it is slow on large databases and has no
//! view of indexes.

use tokio_postgres::Client;

use super::query_error;
use crate::{
    error::{Error, Result},
    schema::{
        Catalog, ColumnInfo, ForeignKey, IndexInfo, Relation, RelationKind, Schema, TableDetail,
        TableRef,
    },
};

const RELATIONS: &str = "
select n.nspname, c.relname, c.relkind::text
from pg_catalog.pg_namespace n
left join pg_catalog.pg_class c
  on c.relnamespace = n.oid and c.relkind in ('r', 'p', 'v', 'm', 'f')
where n.nspname not like 'pg\\_%' and n.nspname <> 'information_schema'
order by n.nspname, c.relname";

/// Resolves a possibly unqualified name the way a query would.
const TABLE_OID: &str = "
select c.oid
from pg_catalog.pg_class c
join pg_catalog.pg_namespace n on n.oid = c.relnamespace
where c.relname = $2
  and case when $1::text is null then pg_catalog.pg_table_is_visible(c.oid)
           else n.nspname = $1 end
limit 1";

const COLUMNS: &str = "
select a.attname,
       pg_catalog.format_type(a.atttypid, a.atttypmod),
       not a.attnotnull,
       pg_catalog.pg_get_expr(d.adbin, d.adrelid)
from pg_catalog.pg_attribute a
left join pg_catalog.pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped
order by a.attnum";

/// One row per index; expression entries have no matching attribute and come
/// back as nulls inside the array.
const INDEXES: &str = "
select ic.relname,
       i.indisunique,
       i.indisprimary,
       array(select a.attname::text
             from unnest(i.indkey::int2[]) with ordinality as k(attnum, ord)
             left join pg_catalog.pg_attribute a
               on a.attrelid = i.indrelid and a.attnum = k.attnum and k.attnum > 0
             where k.ord <= i.indnkeyatts
             order by k.ord)
from pg_catalog.pg_index i
join pg_catalog.pg_class ic on ic.oid = i.indexrelid
where i.indrelid = $1
order by i.indisprimary desc, ic.relname";

const FOREIGN_KEYS: &str = "
select con.conname,
       array(select a.attname::text
             from unnest(con.conkey) with ordinality as k(attnum, ord)
             join pg_catalog.pg_attribute a
               on a.attrelid = con.conrelid and a.attnum = k.attnum
             order by k.ord),
       fn.nspname,
       fc.relname,
       array(select a.attname::text
             from unnest(con.confkey) with ordinality as k(attnum, ord)
             join pg_catalog.pg_attribute a
               on a.attrelid = con.confrelid and a.attnum = k.attnum
             order by k.ord)
from pg_catalog.pg_constraint con
join pg_catalog.pg_class fc on fc.oid = con.confrelid
join pg_catalog.pg_namespace fn on fn.oid = fc.relnamespace
where con.conrelid = $1 and con.contype = 'f'
order by con.conname";

pub(super) async fn catalog(client: &Client) -> Result<Catalog> {
    let rows = client.query(RELATIONS, &[]).await.map_err(query_error)?;

    let mut schemas: Vec<Schema> = Vec::new();
    for row in rows {
        let schema: String = row.try_get(0).map_err(query_error)?;
        if schemas.last().is_none_or(|last| last.name != schema) {
            schemas.push(Schema {
                name: schema,
                relations: Vec::new(),
            });
        }
        // A schema with no relations still yields one row, with nulls.
        let name: Option<String> = row.try_get(1).map_err(query_error)?;
        let kind: Option<String> = row.try_get(2).map_err(query_error)?;
        if let (Some(name), Some(kind), Some(schema)) = (name, kind, schemas.last_mut()) {
            schema.relations.push(Relation {
                name,
                kind: match kind.as_str() {
                    "v" => RelationKind::View,
                    "m" => RelationKind::MaterializedView,
                    "f" => RelationKind::ForeignTable,
                    _ => RelationKind::Table,
                },
            });
        }
    }
    Ok(Catalog { schemas })
}

pub(super) async fn table_detail(client: &Client, table: &TableRef) -> Result<TableDetail> {
    let oid: u32 = client
        .query_opt(TABLE_OID, &[&table.schema, &table.name])
        .await
        .map_err(query_error)?
        .ok_or_else(|| Error::query(format!("relation \"{}\" does not exist", table.name)))?
        .try_get(0)
        .map_err(query_error)?;

    let by_oid: [&(dyn tokio_postgres::types::ToSql + Sync); 1] = [&oid];
    let (columns, indexes, foreign_keys) = futures::try_join!(
        client.query(COLUMNS, &by_oid),
        client.query(INDEXES, &by_oid),
        client.query(FOREIGN_KEYS, &by_oid),
    )
    .map_err(query_error)?;

    let columns = columns
        .iter()
        .map(|row| {
            Ok(ColumnInfo {
                name: row.try_get(0)?,
                type_name: row.try_get(1)?,
                nullable: row.try_get(2)?,
                default: row.try_get(3)?,
            })
        })
        .collect::<std::result::Result<Vec<_>, tokio_postgres::Error>>()
        .map_err(query_error)?;

    let indexes = indexes
        .iter()
        .map(|row| {
            Ok(IndexInfo {
                name: row.try_get(0)?,
                unique: row.try_get(1)?,
                primary: row.try_get(2)?,
                columns: row.try_get(3)?,
            })
        })
        .collect::<std::result::Result<Vec<_>, tokio_postgres::Error>>()
        .map_err(query_error)?;

    let foreign_keys = foreign_keys
        .iter()
        .map(|row| {
            Ok(ForeignKey {
                name: row.try_get(0)?,
                columns: row.try_get(1)?,
                references: TableRef {
                    schema: Some(row.try_get(2)?),
                    name: row.try_get(3)?,
                },
                referenced_columns: row.try_get(4)?,
            })
        })
        .collect::<std::result::Result<Vec<_>, tokio_postgres::Error>>()
        .map_err(query_error)?;

    let primary_key = indexes
        .iter()
        .find(|index| index.primary)
        .map(|index| index.columns.iter().flatten().cloned().collect())
        .unwrap_or_default();

    Ok(TableDetail {
        columns,
        primary_key,
        foreign_keys,
        indexes,
        has_rowid: false,
    })
}
