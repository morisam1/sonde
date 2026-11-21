//! Creating, renaming and dropping databases, schemas, tables and columns,
//! as statements in each database's own language.
//!
//! The app shows these statements before it runs them, the same way it
//! shows table edits, so nothing changes without a look first. What a
//! database cannot do is reported by [`supports`] and never offered.

use crate::{
    driver::{DbKind, Language, dialect},
    schema::TableRef,
};

/// A column for a new table, or one being added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewColumn {
    /// The column's name.
    pub name: String,
    /// Its type, as the database writes it. Ignored by MongoDB.
    pub type_name: String,
    /// Whether it may hold no value.
    pub nullable: bool,
    /// Whether it is part of the primary key.
    pub primary_key: bool,
    /// Its default, as an expression in the database's language, such as
    /// `0`, `'new'` or `now()`. Empty for none.
    pub default: Option<String>,
}

/// A column type a database offers, for the type chooser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnType {
    /// The type's name, as written in DDL: `varchar`, `list`.
    pub name: &'static str,
    /// The arguments it takes, with sensible values: `255` for `varchar`,
    /// `10,2` for `numeric`, `text` for a Cassandra `list`. `None` when it
    /// takes none.
    pub args: Option<&'static str>,
    /// Whether the arguments go in angle brackets, as for Cassandra's
    /// collections (`list<text>`), rather than parentheses.
    pub angle: bool,
}

impl ColumnType {
    const fn plain(name: &'static str) -> Self {
        Self {
            name,
            args: None,
            angle: false,
        }
    }

    const fn with(name: &'static str, args: &'static str) -> Self {
        Self {
            name,
            args: Some(args),
            angle: false,
        }
    }

    const fn generic(name: &'static str, args: &'static str) -> Self {
        Self {
            name,
            args: Some(args),
            angle: true,
        }
    }

    /// The type written out with `args`: `varchar(100)`, or `list<int>` for
    /// Cassandra's collections. Empty arguments leave the name alone.
    pub fn spelled(&self, args: &str) -> String {
        let args = args.trim();
        if self.args.is_none() || args.is_empty() {
            return self.name.to_owned();
        }
        if self.angle {
            format!("{}<{args}>", self.name)
        } else {
            format!("{}({args})", self.name)
        }
    }
}

/// The column types `kind` offers, most used first. Empty for databases
/// without column types (MongoDB, Redis).
pub fn column_types(kind: DbKind) -> &'static [ColumnType] {
    use ColumnType as T;
    const POSTGRES: &[ColumnType] = &[
        T::plain("integer"),
        T::plain("bigint"),
        T::plain("smallint"),
        T::plain("serial"),
        T::plain("bigserial"),
        T::with("numeric", "10,2"),
        T::plain("real"),
        T::plain("double precision"),
        T::plain("boolean"),
        T::plain("text"),
        T::with("varchar", "255"),
        T::with("char", "1"),
        T::plain("uuid"),
        T::plain("date"),
        T::plain("time"),
        T::plain("timestamp"),
        T::plain("timestamptz"),
        T::plain("interval"),
        T::plain("jsonb"),
        T::plain("json"),
        T::plain("bytea"),
        T::plain("inet"),
        T::plain("cidr"),
        T::plain("macaddr"),
        T::plain("money"),
        T::plain("xml"),
        T::plain("tsvector"),
        T::plain("text[]"),
        T::plain("integer[]"),
    ];
    const MYSQL: &[ColumnType] = &[
        T::plain("int"),
        T::plain("bigint"),
        T::plain("smallint"),
        T::plain("tinyint"),
        T::plain("mediumint"),
        T::with("decimal", "10,2"),
        T::plain("float"),
        T::plain("double"),
        T::plain("boolean"),
        T::with("bit", "1"),
        T::with("varchar", "255"),
        T::with("char", "1"),
        T::plain("text"),
        T::plain("tinytext"),
        T::plain("mediumtext"),
        T::plain("longtext"),
        T::plain("json"),
        T::plain("date"),
        T::plain("time"),
        T::plain("datetime"),
        T::plain("timestamp"),
        T::plain("year"),
        T::with("binary", "16"),
        T::with("varbinary", "255"),
        T::plain("blob"),
        T::plain("mediumblob"),
        T::plain("longblob"),
        T::with("enum", "'a','b'"),
        T::with("set", "'a','b'"),
    ];
    const MARIADB: &[ColumnType] = &[
        T::plain("int"),
        T::plain("bigint"),
        T::plain("smallint"),
        T::plain("tinyint"),
        T::plain("mediumint"),
        T::with("decimal", "10,2"),
        T::plain("float"),
        T::plain("double"),
        T::plain("boolean"),
        T::with("bit", "1"),
        T::with("varchar", "255"),
        T::with("char", "1"),
        T::plain("text"),
        T::plain("tinytext"),
        T::plain("mediumtext"),
        T::plain("longtext"),
        T::plain("json"),
        T::plain("uuid"),
        T::plain("inet6"),
        T::plain("date"),
        T::plain("time"),
        T::plain("datetime"),
        T::plain("timestamp"),
        T::plain("year"),
        T::with("binary", "16"),
        T::with("varbinary", "255"),
        T::plain("blob"),
        T::plain("mediumblob"),
        T::plain("longblob"),
        T::with("enum", "'a','b'"),
        T::with("set", "'a','b'"),
    ];
    const SQLSERVER: &[ColumnType] = &[
        T::plain("int"),
        T::plain("bigint"),
        T::plain("smallint"),
        T::plain("tinyint"),
        T::plain("bit"),
        T::with("decimal", "18,2"),
        T::with("numeric", "18,2"),
        T::plain("money"),
        T::plain("float"),
        T::plain("real"),
        T::with("nvarchar", "255"),
        T::with("varchar", "255"),
        T::with("nchar", "1"),
        T::with("char", "1"),
        T::plain("uniqueidentifier"),
        T::plain("date"),
        T::with("time", "7"),
        T::with("datetime2", "7"),
        T::with("datetimeoffset", "7"),
        T::plain("datetime"),
        T::plain("smalldatetime"),
        T::with("varbinary", "max"),
        T::with("binary", "16"),
        T::plain("xml"),
    ];
    const SQLITE: &[ColumnType] = &[
        T::plain("INTEGER"),
        T::plain("TEXT"),
        T::plain("REAL"),
        T::plain("NUMERIC"),
        T::plain("BLOB"),
        T::plain("BOOLEAN"),
        T::plain("DATE"),
        T::plain("DATETIME"),
        T::plain("JSON"),
    ];
    const SNOWFLAKE: &[ColumnType] = &[
        T::with("NUMBER", "38,0"),
        T::plain("INTEGER"),
        T::plain("FLOAT"),
        T::plain("BOOLEAN"),
        T::plain("VARCHAR"),
        T::with("CHAR", "1"),
        T::plain("BINARY"),
        T::plain("DATE"),
        T::plain("TIME"),
        T::plain("TIMESTAMP_NTZ"),
        T::plain("TIMESTAMP_LTZ"),
        T::plain("TIMESTAMP_TZ"),
        T::plain("VARIANT"),
        T::plain("OBJECT"),
        T::plain("ARRAY"),
        T::plain("GEOGRAPHY"),
        T::plain("GEOMETRY"),
    ];
    const CASSANDRA: &[ColumnType] = &[
        T::plain("text"),
        T::plain("int"),
        T::plain("bigint"),
        T::plain("smallint"),
        T::plain("tinyint"),
        T::plain("varint"),
        T::plain("decimal"),
        T::plain("double"),
        T::plain("float"),
        T::plain("boolean"),
        T::plain("uuid"),
        T::plain("timeuuid"),
        T::plain("timestamp"),
        T::plain("date"),
        T::plain("time"),
        T::plain("duration"),
        T::plain("blob"),
        T::plain("inet"),
        T::plain("ascii"),
        T::plain("counter"),
        T::generic("list", "text"),
        T::generic("set", "text"),
        T::generic("map", "text, text"),
        T::generic("frozen", "list<text>"),
    ];
    const ELASTIC: &[ColumnType] = &[
        T::plain("keyword"),
        T::plain("text"),
        T::plain("long"),
        T::plain("integer"),
        T::plain("short"),
        T::plain("byte"),
        T::plain("double"),
        T::plain("float"),
        T::plain("half_float"),
        T::plain("unsigned_long"),
        T::plain("boolean"),
        T::plain("date"),
        T::plain("date_nanos"),
        T::plain("ip"),
        T::plain("binary"),
        T::plain("geo_point"),
        T::plain("geo_shape"),
        T::plain("object"),
        T::plain("nested"),
        T::plain("flattened"),
        T::plain("wildcard"),
        T::plain("match_only_text"),
        T::plain("version"),
    ];
    match kind {
        DbKind::Postgres => POSTGRES,
        DbKind::Mysql => MYSQL,
        DbKind::Mariadb => MARIADB,
        DbKind::SqlServer => SQLSERVER,
        DbKind::Sqlite => SQLITE,
        DbKind::Snowflake => SNOWFLAKE,
        DbKind::Cassandra => CASSANDRA,
        DbKind::Elasticsearch => ELASTIC,
        DbKind::Mongodb | DbKind::Redis => &[],
    }
}

/// Whether `kind` takes a default value for a column.
pub fn supports_defaults(kind: DbKind) -> bool {
    !matches!(
        kind,
        DbKind::Cassandra | DbKind::Elasticsearch | DbKind::Mongodb | DbKind::Redis
    )
}

/// The kinds of value a new Redis key starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    /// A plain string.
    String,
    /// A hash of fields.
    Hash,
    /// A list.
    List,
    /// A set.
    Set,
    /// A sorted set.
    SortedSet,
}

impl KeyType {
    /// Every type, in the order a chooser lists them.
    pub const ALL: [Self; 5] = [
        Self::String,
        Self::Hash,
        Self::List,
        Self::Set,
        Self::SortedSet,
    ];

    /// The name Redis gives the type.
    pub fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::List => "list",
            Self::Set => "set",
            Self::SortedSet => "zset",
        }
    }
}

/// One change to the structure of a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A new database: a MySQL schema, a Cassandra keyspace, a MongoDB
    /// database (made with its first collection).
    CreateDatabase {
        /// Its name.
        name: String,
        /// The first collection, for MongoDB, which has no empty databases.
        first: Option<String>,
    },
    /// Remove a database and everything in it.
    DropDatabase(String),
    /// A new schema inside the connected database.
    CreateSchema(String),
    /// Remove an empty schema.
    DropSchema(String),
    /// A new table, collection or index.
    CreateTable {
        /// Where it goes and its name.
        table: TableRef,
        /// Its columns. MongoDB collections take none.
        columns: Vec<NewColumn>,
    },
    /// Give a table a new name in the same schema.
    RenameTable {
        /// The table.
        table: TableRef,
        /// The new name.
        to: String,
    },
    /// Remove a table and its rows.
    DropTable(TableRef),
    /// Remove every row, keeping the table.
    EmptyTable(TableRef),
    /// Add a column. On MongoDB, set the field to null in every document.
    AddColumn {
        /// The table.
        table: TableRef,
        /// The new column.
        column: NewColumn,
    },
    /// Rename a column, or a field in every MongoDB document.
    RenameColumn {
        /// The table.
        table: TableRef,
        /// The column's name.
        column: String,
        /// Its new name.
        to: String,
    },
    /// Remove a column, or a field from every MongoDB document.
    DropColumn {
        /// The table.
        table: TableRef,
        /// The column's name.
        column: String,
    },
    /// A new Redis key.
    CreateKey {
        /// The key.
        key: String,
        /// What it holds.
        kind: KeyType,
        /// The first value: the string, or the first element. A hash takes
        /// `field value`; a sorted set `score member`.
        value: String,
    },
    /// Remove every key of the selected Redis database.
    FlushDatabase,
}

/// The operations, without their details, for asking what a database
/// supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// [`Change::CreateDatabase`].
    CreateDatabase,
    /// [`Change::DropDatabase`].
    DropDatabase,
    /// [`Change::CreateSchema`].
    CreateSchema,
    /// [`Change::DropSchema`].
    DropSchema,
    /// [`Change::CreateTable`].
    CreateTable,
    /// [`Change::RenameTable`].
    RenameTable,
    /// [`Change::DropTable`].
    DropTable,
    /// [`Change::EmptyTable`].
    EmptyTable,
    /// [`Change::AddColumn`].
    AddColumn,
    /// [`Change::RenameColumn`].
    RenameColumn,
    /// [`Change::DropColumn`].
    DropColumn,
    /// [`Change::CreateKey`].
    CreateKey,
    /// [`Change::FlushDatabase`].
    FlushDatabase,
}

/// Whether the browser's schemas are databases of their own (MySQL,
/// Cassandra keyspaces, MongoDB databases) rather than schemas inside one.
pub fn schemas_are_databases(kind: DbKind) -> bool {
    matches!(
        kind,
        DbKind::Mysql | DbKind::Mariadb | DbKind::Cassandra | DbKind::Mongodb
    )
}

/// Whether `kind` can do `op`.
pub fn supports(kind: DbKind, op: Op) -> bool {
    use DbKind as K;
    use Op as O;
    match op {
        O::CreateDatabase | O::DropDatabase => {
            !matches!(kind, K::Sqlite | K::Redis | K::Elasticsearch)
        }
        O::CreateSchema | O::DropSchema => {
            matches!(kind, K::Postgres | K::SqlServer | K::Snowflake)
        }
        O::CreateTable | O::EmptyTable => kind != K::Redis,
        O::DropTable => true,
        O::RenameTable => !matches!(kind, K::Cassandra | K::Elasticsearch),
        O::AddColumn => kind != K::Redis,
        // CQL renames only key columns, which a GUI should not encourage.
        O::RenameColumn => !matches!(kind, K::Redis | K::Elasticsearch | K::Cassandra),
        O::DropColumn => !matches!(kind, K::Redis | K::Elasticsearch),
        O::CreateKey | O::FlushDatabase => kind == K::Redis,
    }
}

/// A sensible type for a new column, as the column editor's default.
pub fn default_type(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres | DbKind::Cassandra => "text",
        DbKind::Mysql | DbKind::Mariadb => "varchar(255)",
        DbKind::SqlServer => "nvarchar(255)",
        DbKind::Sqlite => "TEXT",
        DbKind::Snowflake => "VARCHAR",
        DbKind::Elasticsearch => "keyword",
        DbKind::Mongodb | DbKind::Redis => "",
    }
}

fn json(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

fn mongo_db(table: &TableRef) -> String {
    match &table.schema {
        Some(db) => format!("db.getSiblingDB({})", json(db)),
        None => "db".to_owned(),
    }
}

fn mongo_coll(table: &TableRef) -> String {
    format!("{}.getCollection({})", mongo_db(table), json(&table.name))
}

fn column_sql(kind: DbKind, column: &NewColumn, quote: &dyn Fn(&str) -> String) -> String {
    let mut sql = format!("{} {}", quote(&column.name), column.type_name.trim());
    if supports_defaults(kind)
        && let Some(default) = column
            .default
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
    {
        sql.push_str(" DEFAULT ");
        sql.push_str(default);
    }
    // Cassandra has no NOT NULL; its key columns can never be null anyway.
    if !column.nullable && kind != DbKind::Cassandra {
        sql.push_str(" NOT NULL");
    }
    sql
}

/// The statements that make `change` on `kind`, in order.
///
/// # Errors
///
/// A plain-language reason when the change is incomplete (a table with no
/// columns, a Cassandra table with no key) or the database cannot do it.
pub fn statements(kind: DbKind, change: &Change) -> Result<Vec<String>, String> {
    let dialect = dialect(kind);
    let q = |name: &str| dialect.quote_ident(name);
    let table_name = |table: &TableRef| dialect.quote_table(table);
    let empty = |name: &str, what: &str| {
        if name.trim().is_empty() {
            Err(format!("Enter the {what}'s name."))
        } else {
            Ok(())
        }
    };
    let op = match change {
        Change::CreateDatabase { .. } => Op::CreateDatabase,
        Change::DropDatabase(_) => Op::DropDatabase,
        Change::CreateSchema(_) => Op::CreateSchema,
        Change::DropSchema(_) => Op::DropSchema,
        Change::CreateTable { .. } => Op::CreateTable,
        Change::RenameTable { .. } => Op::RenameTable,
        Change::DropTable(_) => Op::DropTable,
        Change::EmptyTable(_) => Op::EmptyTable,
        Change::AddColumn { .. } => Op::AddColumn,
        Change::RenameColumn { .. } => Op::RenameColumn,
        Change::DropColumn { .. } => Op::DropColumn,
        Change::CreateKey { .. } => Op::CreateKey,
        Change::FlushDatabase => Op::FlushDatabase,
    };
    if !supports(kind, op) {
        return Err(format!("{} cannot do that.", kind.name()));
    }
    let language = kind.language();

    let one = |sql: String| Ok(vec![sql]);
    match change {
        Change::CreateDatabase { name, first } => {
            empty(name, "database")?;
            match kind {
                DbKind::Cassandra => one(format!(
                    "CREATE KEYSPACE {} WITH replication = \
                     {{'class': 'SimpleStrategy', 'replication_factor': 1}}",
                    q(name)
                )),
                DbKind::Mongodb => {
                    let first = first.as_deref().unwrap_or("").trim();
                    empty(first, "first collection")?;
                    one(format!(
                        "db.getSiblingDB({}).createCollection({})",
                        json(name),
                        json(first)
                    ))
                }
                _ => one(format!("CREATE DATABASE {}", q(name))),
            }
        }
        Change::DropDatabase(name) => match kind {
            DbKind::Cassandra => one(format!("DROP KEYSPACE {}", q(name))),
            DbKind::Mongodb => one(format!("db.getSiblingDB({}).dropDatabase()", json(name))),
            _ => one(format!("DROP DATABASE {}", q(name))),
        },
        Change::CreateSchema(name) => {
            empty(name, "schema")?;
            one(format!("CREATE SCHEMA {}", q(name)))
        }
        Change::DropSchema(name) => one(format!("DROP SCHEMA {}", q(name))),
        Change::CreateTable { table, columns } => {
            empty(&table.name, "table")?;
            match language {
                Language::Mongo => one(format!(
                    "{}.createCollection({})",
                    mongo_db(table),
                    json(&table.name)
                )),
                Language::Elastic => {
                    let properties: serde_json::Map<String, serde_json::Value> = columns
                        .iter()
                        .filter(|c| !c.name.trim().is_empty())
                        .map(|c| {
                            (
                                c.name.trim().to_owned(),
                                serde_json::json!({"type": c.type_name.trim()}),
                            )
                        })
                        .collect();
                    let body = serde_json::json!({"mappings": {"properties": properties}});
                    one(format!(
                        "PUT /{}\n{}",
                        table.name,
                        serde_json::to_string_pretty(&body).unwrap_or_default()
                    ))
                }
                _ => {
                    let columns: Vec<&NewColumn> = columns
                        .iter()
                        .filter(|c| !c.name.trim().is_empty())
                        .collect();
                    if columns.is_empty() {
                        return Err("Add at least one column.".into());
                    }
                    if let Some(untyped) = columns.iter().find(|c| c.type_name.trim().is_empty()) {
                        return Err(format!("Give the column {} a type.", untyped.name));
                    }
                    let keys: Vec<String> = columns
                        .iter()
                        .filter(|c| c.primary_key)
                        .map(|c| q(&c.name))
                        .collect();
                    if keys.is_empty() && kind == DbKind::Cassandra {
                        return Err("Cassandra tables need a primary key.".into());
                    }
                    let mut parts: Vec<String> = columns
                        .iter()
                        .map(|c| {
                            let mut column = (*c).clone();
                            // Key columns are never null.
                            column.nullable &= !c.primary_key;
                            format!("    {}", column_sql(kind, &column, &q))
                        })
                        .collect();
                    if !keys.is_empty() {
                        parts.push(format!("    PRIMARY KEY ({})", keys.join(", ")));
                    }
                    one(format!(
                        "CREATE TABLE {} (\n{}\n)",
                        table_name(table),
                        parts.join(",\n")
                    ))
                }
            }
        }
        Change::RenameTable { table, to } => {
            empty(to, "table")?;
            let renamed = TableRef {
                schema: table.schema.clone(),
                name: to.clone(),
            };
            match kind {
                DbKind::Mysql | DbKind::Mariadb => one(format!(
                    "RENAME TABLE {} TO {}",
                    table_name(table),
                    table_name(&renamed)
                )),
                DbKind::SqlServer => one(format!(
                    "EXEC sp_rename N'{}', N'{}'",
                    table_name(table).replace('\'', "''"),
                    to.replace('\'', "''")
                )),
                DbKind::Snowflake => one(format!(
                    "ALTER TABLE {} RENAME TO {}",
                    table_name(table),
                    table_name(&renamed)
                )),
                DbKind::Mongodb => {
                    let db = table.schema.clone().unwrap_or_default();
                    one(format!(
                        "db.adminCommand({{renameCollection: {}, to: {}}})",
                        json(&format!("{db}.{}", table.name)),
                        json(&format!("{db}.{to}"))
                    ))
                }
                DbKind::Redis => one(format!(
                    "RENAME {} {}",
                    crate::kv::quote_arg(&table.name),
                    crate::kv::quote_arg(to)
                )),
                _ => one(format!(
                    "ALTER TABLE {} RENAME TO {}",
                    table_name(table),
                    q(to)
                )),
            }
        }
        Change::DropTable(table) => match kind {
            DbKind::Mongodb => one(format!("{}.drop()", mongo_coll(table))),
            DbKind::Elasticsearch => one(format!("DELETE /{}", table.name)),
            DbKind::Redis => one(format!("DEL {}", crate::kv::quote_arg(&table.name))),
            _ => one(format!("DROP TABLE {}", table_name(table))),
        },
        Change::EmptyTable(table) => match kind {
            DbKind::Mongodb => one(format!("{}.deleteMany({{}})", mongo_coll(table))),
            DbKind::Elasticsearch => one(format!(
                "POST /{}/_delete_by_query?refresh=true\n{{\"query\": {{\"match_all\": {{}}}}}}",
                table.name
            )),
            DbKind::Sqlite => one(format!("DELETE FROM {}", table_name(table))),
            _ => one(format!("TRUNCATE TABLE {}", table_name(table))),
        },
        Change::AddColumn { table, column } => {
            empty(&column.name, "column")?;
            match kind {
                DbKind::Mongodb => one(format!(
                    "{}.updateMany({{{}: {{$exists: false}}}}, {{$set: {{{}: null}}}})",
                    mongo_coll(table),
                    json(&column.name),
                    json(&column.name)
                )),
                DbKind::Elasticsearch => one(format!(
                    "PUT /{}/_mapping\n{}",
                    table.name,
                    serde_json::json!({"properties": {column.name.trim(): {"type": column.type_name.trim()}}})
                )),
                _ => {
                    if column.type_name.trim().is_empty() {
                        return Err("Give the column a type.".into());
                    }
                    let add = if matches!(kind, DbKind::SqlServer | DbKind::Cassandra) {
                        "ADD"
                    } else {
                        "ADD COLUMN"
                    };
                    one(format!(
                        "ALTER TABLE {} {add} {}",
                        table_name(table),
                        column_sql(kind, column, &q)
                    ))
                }
            }
        }
        Change::RenameColumn { table, column, to } => {
            empty(to, "column")?;
            match kind {
                DbKind::Mongodb => one(format!(
                    "{}.updateMany({{}}, {{$rename: {{{}: {}}}}})",
                    mongo_coll(table),
                    json(column),
                    json(to)
                )),
                DbKind::SqlServer => one(format!(
                    "EXEC sp_rename N'{}.{}', N'{}', 'COLUMN'",
                    table_name(table).replace('\'', "''"),
                    q(column).replace('\'', "''"),
                    to.replace('\'', "''")
                )),
                _ => one(format!(
                    "ALTER TABLE {} RENAME COLUMN {} TO {}",
                    table_name(table),
                    q(column),
                    q(to)
                )),
            }
        }
        Change::DropColumn { table, column } => match kind {
            DbKind::Mongodb => one(format!(
                "{}.updateMany({{}}, {{$unset: {{{}: \"\"}}}})",
                mongo_coll(table),
                json(column)
            )),
            DbKind::Cassandra => one(format!(
                "ALTER TABLE {} DROP {}",
                table_name(table),
                q(column)
            )),
            _ => one(format!(
                "ALTER TABLE {} DROP COLUMN {}",
                table_name(table),
                q(column)
            )),
        },
        Change::CreateKey {
            key,
            kind: key_kind,
            value,
        } => {
            empty(key, "key")?;
            let key = crate::kv::quote_arg(key);
            let parts = crate::kv::split_args(value)?;
            let args = parts
                .iter()
                .map(|part| crate::kv::quote_arg(part))
                .collect::<Vec<_>>()
                .join(" ");
            let (command, arity) = match key_kind {
                KeyType::String => ("SET", 1),
                KeyType::Hash => ("HSET", 2),
                KeyType::List => ("RPUSH", 1),
                KeyType::Set => ("SADD", 1),
                KeyType::SortedSet => ("ZADD", 2),
            };
            if parts.is_empty()
                || (*key_kind == KeyType::String && parts.len() != 1)
                || parts.len() % arity != 0
            {
                return Err(match key_kind {
                    KeyType::String => "Enter the value.".to_owned(),
                    KeyType::Hash => "Enter field and value pairs, such as name ada.".to_owned(),
                    KeyType::SortedSet => "Enter score and member pairs, such as 1 ada.".to_owned(),
                    _ => "Enter at least one element.".to_owned(),
                });
            }
            // SET NX: a key that already exists is not overwritten.
            if *key_kind == KeyType::String {
                return one(format!("SET {key} {args} NX"));
            }
            one(format!("{command} {key} {args}"))
        }
        Change::FlushDatabase => one("FLUSHDB".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Vec<NewColumn> {
        vec![
            NewColumn {
                name: "id".into(),
                type_name: "bigint".into(),
                nullable: true,
                primary_key: true,
                default: None,
            },
            NewColumn {
                name: "name".into(),
                type_name: "text".into(),
                nullable: false,
                primary_key: false,
                default: None,
            },
        ]
    }

    #[test]
    fn create_table_should_quote_and_add_the_key() {
        let change = Change::CreateTable {
            table: TableRef::new("public", "Order Items"),
            columns: columns(),
        };

        assert_eq!(
            statements(DbKind::Postgres, &change).unwrap(),
            [
                "CREATE TABLE \"public\".\"Order Items\" (\n    \"id\" bigint NOT NULL,\n    \"name\" text NOT NULL,\n    PRIMARY KEY (\"id\")\n)"
            ]
        );
        assert!(statements(DbKind::Mysql, &change).unwrap()[0].contains("`Order Items`"));
    }

    #[test]
    fn cassandra_tables_need_a_key() {
        let mut no_key = columns();
        no_key[0].primary_key = false;
        let change = Change::CreateTable {
            table: TableRef::new("ks", "t"),
            columns: no_key,
        };

        assert!(statements(DbKind::Cassandra, &change).is_err());
    }

    #[test]
    fn renames_should_use_each_database_s_spelling() {
        let table = TableRef::new("dbo", "t");
        let rename = |kind| {
            statements(
                kind,
                &Change::RenameTable {
                    table: table.clone(),
                    to: "u".into(),
                },
            )
            .unwrap()[0]
                .clone()
        };

        assert_eq!(
            rename(DbKind::SqlServer),
            "EXEC sp_rename N'[dbo].[t]', N'u'"
        );
        assert_eq!(rename(DbKind::Mysql), "RENAME TABLE `dbo`.`t` TO `dbo`.`u`");
        assert_eq!(
            rename(DbKind::Postgres),
            "ALTER TABLE \"dbo\".\"t\" RENAME TO \"u\""
        );
        assert_eq!(
            rename(DbKind::Mongodb),
            r#"db.adminCommand({renameCollection: "dbo.t", to: "dbo.u"})"#
        );
    }

    #[test]
    fn unsupported_changes_should_be_refused() {
        assert!(!supports(DbKind::Redis, Op::CreateTable));
        assert!(!supports(DbKind::Sqlite, Op::CreateDatabase));
        assert!(supports(DbKind::Cassandra, Op::DropColumn));
        assert!(!supports(DbKind::Cassandra, Op::RenameColumn));
        assert!(
            statements(
                DbKind::Elasticsearch,
                &Change::RenameTable {
                    table: TableRef::unqualified("i"),
                    to: "j".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn redis_keys_should_take_their_first_value() {
        let key = |kind, value: &str| {
            statements(
                DbKind::Redis,
                &Change::CreateKey {
                    key: "user:1".into(),
                    kind,
                    value: value.into(),
                },
            )
        };

        assert_eq!(
            key(KeyType::Hash, "name ada").unwrap(),
            ["HSET user:1 name ada"]
        );
        assert_eq!(
            key(KeyType::String, "\"hello world\"").unwrap(),
            ["SET user:1 \"hello world\" NX"]
        );
        assert!(key(KeyType::Hash, "name").is_err());
    }

    #[test]
    fn column_types_should_spell_their_arguments() {
        let varchar = column_types(DbKind::Postgres)
            .iter()
            .find(|t| t.name == "varchar")
            .unwrap();
        let list = column_types(DbKind::Cassandra)
            .iter()
            .find(|t| t.name == "list")
            .unwrap();

        assert_eq!(varchar.spelled("100"), "varchar(100)");
        assert_eq!(varchar.spelled(" "), "varchar");
        assert_eq!(list.spelled("int"), "list<int>");
        let set = column_types(DbKind::Mysql)
            .iter()
            .find(|t| t.name == "set")
            .unwrap();
        assert_eq!(set.spelled("'a','b'"), "set('a','b')");
        assert!(column_types(DbKind::Mongodb).is_empty());
        for kind in DbKind::ALL {
            if supports(kind, Op::CreateTable) && !matches!(kind, DbKind::Mongodb) {
                assert!(!column_types(kind).is_empty(), "{kind:?}");
            }
        }
    }

    #[test]
    fn defaults_should_come_before_not_null() {
        let change = Change::AddColumn {
            table: TableRef::new("public", "t"),
            column: NewColumn {
                name: "n".into(),
                type_name: "integer".into(),
                nullable: false,
                primary_key: false,
                default: Some("0".into()),
            },
        };

        assert_eq!(
            statements(DbKind::Postgres, &change).unwrap(),
            ["ALTER TABLE \"public\".\"t\" ADD COLUMN \"n\" integer DEFAULT 0 NOT NULL"]
        );
    }
}
