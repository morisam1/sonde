//! The catalog model every database shares.

/// Everything a session can browse: schemas and the relations in them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Catalog {
    /// Schemas in display order.
    pub schemas: Vec<Schema>,
}

/// One schema. SQLite's attached databases (`main`, `temp`, ...) appear as
/// schemas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// The schema's name.
    pub name: String,
    /// Tables and views, sorted by name.
    pub relations: Vec<Relation>,
}

/// A table or view listed in a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// The relation's name.
    pub name: String,
    /// What kind of relation it is.
    pub kind: RelationKind,
}

/// The kinds of relation the browser shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    /// An ordinary or partitioned table.
    Table,
    /// A view.
    View,
    /// A materialised view (Postgres only).
    MaterializedView,
    /// A foreign table (Postgres only).
    ForeignTable,
    /// A MongoDB collection, or an Elasticsearch index.
    Collection,
    /// A Redis key.
    Key,
}

/// Names a relation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    /// The schema, or `None` to let the database resolve the name.
    pub schema: Option<String>,
    /// The relation's name.
    pub name: String,
}

impl TableRef {
    /// A reference qualified with a schema.
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    /// A reference the database resolves through its search path.
    pub fn unqualified(name: impl Into<String>) -> Self {
        Self {
            schema: None,
            name: name.into(),
        }
    }
}

/// Columns, keys and indexes of one relation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableDetail {
    /// Columns in table order.
    pub columns: Vec<ColumnInfo>,
    /// Primary key column names in key order. Empty when there is none.
    pub primary_key: Vec<String>,
    /// Foreign keys leaving this table.
    pub foreign_keys: Vec<ForeignKey>,
    /// Indexes, including the ones backing constraints.
    pub indexes: Vec<IndexInfo>,
    /// Whether rows can be addressed by SQLite's `rowid`. Always false for
    /// Postgres.
    pub has_rowid: bool,
}

/// One table column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    /// The column's name.
    pub name: String,
    /// The type as the database writes it, such as `numeric(10,2)`.
    pub type_name: String,
    /// Whether the column accepts `NULL`.
    pub nullable: bool,
    /// The default expression, as source text.
    pub default: Option<String>,
}

/// A foreign key constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// The constraint's name. SQLite keys are unnamed and get `fk_<id>`.
    pub name: String,
    /// Referencing columns, in key order.
    pub columns: Vec<String>,
    /// The referenced table.
    pub references: TableRef,
    /// Referenced columns, matching `columns` by position.
    pub referenced_columns: Vec<String>,
}

/// An index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    /// The index's name.
    pub name: String,
    /// Indexed columns in index order. An expression entry is `None`.
    pub columns: Vec<Option<String>>,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
    /// Whether the index backs the primary key.
    pub primary: bool,
}
