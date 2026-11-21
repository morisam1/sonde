//! Turning the browser's state into tree items.

use std::collections::{HashMap, HashSet};

use gpui_kit::{SharedString, component::tree::TreeItem};
use sonde_core::{
    Catalog, TableDetail, TableRef,
    schema::{ForeignKey, IndexInfo, Relation, RelationKind, Schema},
};

use super::{Entry, Load, Status};

/// How a row's label is coloured.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Normal,
    /// Supporting text, and things that are not live.
    Quiet,
    /// A failure message.
    Problem,
}

/// What activating a row acts on.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum Target {
    Connection(String),
    Schema {
        connection: String,
        schema: String,
    },
    Table {
        connection: String,
        table: TableRef,
    },
    Column {
        connection: String,
        table: TableRef,
        column: String,
    },
    #[default]
    Nothing,
}

/// What sort of object a row is, which picks its icon.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    Connection,
    /// One of the databases listed under a connection.
    Database,
    Schema,
    Table,
    View,
    /// Columns, Indexes, Foreign keys.
    Group,
    Column {
        key: bool,
    },
    Index,
    ForeignKey,
    /// A status line standing in for children.
    #[default]
    Note,
}

/// How one tree row is drawn and what it stands for.
#[derive(Clone, Default)]
pub struct Row {
    pub kind: Kind,
    pub label: SharedString,
    pub engine: Option<sonde_core::DbKind>,
    /// Shown after the label in a quieter style: a type, a status.
    pub secondary: Option<SharedString>,
    pub tone: Tone,
    /// Drawn heavier: a connection with a live session.
    pub strong: bool,
    pub target: Target,
}

/// Row identifiers. A row's id is its ancestors' ids plus its own part, so a
/// row can be traced back to its connection and identity survives a rebuild.
pub mod id {
    use gpui_kit::SharedString;

    /// Separates the parts of an id. A control character, so it cannot
    /// collide with any name a database allows.
    const SEP: char = '\u{1f}';

    pub fn connection(connection: &str) -> SharedString {
        format!("c{SEP}{connection}").into()
    }

    pub fn child(parent: &SharedString, part: &str) -> SharedString {
        format!("{parent}{SEP}{part}").into()
    }

    /// Whether `row` is `ancestor` or sits below it.
    pub fn is_within(row: &SharedString, ancestor: &SharedString) -> bool {
        row.strip_prefix(ancestor.as_ref())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(SEP))
    }

    /// The saved-connection id a row belongs to.
    pub fn connection_of(row: &SharedString) -> Option<&str> {
        row.split(SEP).nth(1)
    }
}

type Rows = HashMap<SharedString, Row>;

/// Shared context for building one connection's subtree.
struct Builder<'a> {
    connection: &'a str,
    details: &'a HashMap<(String, TableRef), Load<TableDetail>>,
    expanded: &'a HashSet<SharedString>,
    rows: &'a mut Rows,
}

impl Builder<'_> {
    fn item(&mut self, id: SharedString, row: Row) -> TreeItem {
        let item =
            TreeItem::new(id.clone(), row.label.clone()).expanded(self.expanded.contains(&id));
        self.rows.insert(id, row);
        item
    }

    /// A line of text standing in for children: a status or a failure. Not
    /// selectable.
    fn note(
        &mut self,
        parent: &SharedString,
        text: impl Into<SharedString>,
        tone: Tone,
    ) -> TreeItem {
        let row = Row {
            label: text.into(),
            tone,
            ..Row::default()
        };
        self.item(id::child(parent, "note"), row).disabled(true)
    }

    fn load_note<T>(&mut self, parent: &SharedString, load: &Load<T>, loading: &str) -> TreeItem {
        match load {
            Load::Failed(message) => self.note(parent, message.clone(), Tone::Problem),
            Load::Loading | Load::Ready(_) => self.note(parent, loading.to_owned(), Tone::Quiet),
        }
    }

    fn schema(&mut self, parent: &SharedString, schema: &Schema) -> TreeItem {
        let id = id::child(parent, &format!("s:{}", schema.name));
        let children: Vec<TreeItem> = if schema.relations.is_empty() {
            vec![self.note(&id, "No tables or views", Tone::Quiet)]
        } else {
            schema
                .relations
                .iter()
                .map(|relation| self.relation(&id, &schema.name, relation))
                .collect()
        };
        let row = Row {
            kind: Kind::Schema,
            label: schema.name.clone().into(),
            target: Target::Schema {
                connection: self.connection.to_owned(),
                schema: schema.name.clone(),
            },
            ..Row::default()
        };
        self.item(id, row).children(children)
    }

    fn relation(&mut self, parent: &SharedString, schema: &str, relation: &Relation) -> TreeItem {
        let id = id::child(parent, &format!("t:{}", relation.name));
        let table = TableRef::new(schema, relation.name.clone());
        let key = (self.connection.to_owned(), table.clone());

        let children = match self.details.get(&key) {
            Some(Load::Ready(detail)) => self.detail(&id, &table, detail),
            Some(other) => vec![self.load_note(&id, other, "Loading columns")],
            None => vec![self.note(&id, "Loading columns", Tone::Quiet)],
        };
        let row = Row {
            kind: if matches!(
                relation.kind,
                RelationKind::Table | RelationKind::Collection | RelationKind::Key
            ) {
                Kind::Table
            } else {
                Kind::View
            },
            label: relation.name.clone().into(),
            secondary: match relation.kind {
                RelationKind::Table => None,
                RelationKind::View => Some("view".into()),
                RelationKind::MaterializedView => Some("materialized view".into()),
                RelationKind::ForeignTable => Some("foreign table".into()),
                RelationKind::Collection | RelationKind::Key => None,
            },
            target: Target::Table {
                connection: key.0,
                table,
            },
            ..Row::default()
        };
        self.item(id, row).children(children)
    }

    fn detail(
        &mut self,
        parent: &SharedString,
        table: &TableRef,
        detail: &TableDetail,
    ) -> Vec<TreeItem> {
        let mut groups = Vec::new();

        let columns = id::child(parent, "columns");
        let children: Vec<TreeItem> = detail
            .columns
            .iter()
            .map(|column| {
                let mut facts = vec![column.type_name.clone()];
                if detail.primary_key.contains(&column.name) {
                    facts.push("primary key".into());
                } else if !column.nullable {
                    facts.push("not null".into());
                }
                let row = Row {
                    kind: Kind::Column {
                        key: detail.primary_key.contains(&column.name),
                    },
                    label: column.name.clone().into(),
                    secondary: Some(facts.join(", ").into()),
                    target: Target::Column {
                        connection: self.connection.to_owned(),
                        table: table.clone(),
                        column: column.name.clone(),
                    },
                    ..Row::default()
                };
                self.item(id::child(&columns, &column.name), row)
            })
            .collect();
        groups.push(self.group(columns, "Columns", children));

        if !detail.indexes.is_empty() {
            let indexes = id::child(parent, "indexes");
            let children = detail
                .indexes
                .iter()
                .map(|index| {
                    let row = Row {
                        kind: Kind::Index,
                        label: index.name.clone().into(),
                        secondary: Some(describe_index(index).into()),
                        ..Row::default()
                    };
                    self.item(id::child(&indexes, &index.name), row)
                })
                .collect();
            groups.push(self.group(indexes, "Indexes", children));
        }

        if !detail.foreign_keys.is_empty() {
            let keys = id::child(parent, "foreign-keys");
            let children = detail
                .foreign_keys
                .iter()
                .map(|key| {
                    let row = Row {
                        kind: Kind::ForeignKey,
                        label: key.name.clone().into(),
                        secondary: Some(describe_foreign_key(key).into()),
                        ..Row::default()
                    };
                    self.item(id::child(&keys, &key.name), row)
                })
                .collect();
            groups.push(self.group(keys, "Foreign keys", children));
        }
        groups
    }

    fn group(
        &mut self,
        id: SharedString,
        label: &'static str,
        children: Vec<TreeItem>,
    ) -> TreeItem {
        let row = Row {
            kind: Kind::Group,
            label: label.into(),
            secondary: Some(children.len().to_string().into()),
            tone: Tone::Quiet,
            ..Row::default()
        };
        self.item(id, row).children(children)
    }
}

fn describe_index(index: &IndexInfo) -> String {
    let columns: Vec<&str> = index
        .columns
        .iter()
        .map(|column| column.as_deref().unwrap_or("expression"))
        .collect();
    let kind = if index.primary {
        "primary key "
    } else if index.unique {
        "unique "
    } else {
        ""
    };
    format!("{kind}({})", columns.join(", "))
}

fn describe_foreign_key(key: &ForeignKey) -> String {
    let target = match &key.references.schema {
        Some(schema) => format!("{schema}.{}", key.references.name),
        None => key.references.name.clone(),
    };
    let referenced = if key.referenced_columns.is_empty() {
        String::new()
    } else {
        format!(" ({})", key.referenced_columns.join(", "))
    };
    format!(
        "({}) references {target}{referenced}",
        key.columns.join(", ")
    )
}

/// Build the subtree for one saved connection, recording each row in `rows`.
/// `databases` are the entries listed under it, when it lists them.
pub fn connection(
    entry: &Entry,
    databases: &[&Entry],
    details: &HashMap<(String, TableRef), Load<TableDetail>>,
    expanded: &HashSet<SharedString>,
    rows: &mut Rows,
) -> TreeItem {
    let id = id::connection(&entry.saved.id);
    let mut builder = Builder {
        connection: &entry.saved.id,
        details,
        expanded,
        rows,
    };

    let (secondary, children) = match &entry.status {
        // Expanding an idle connection is what connects it, so it needs a
        // child to be expandable at all; this is what shows meanwhile.
        Status::Idle => (None, vec![builder.note(&id, "Connecting", Tone::Quiet)]),
        Status::Connecting => (
            Some("connecting"),
            vec![builder.note(&id, "Connecting", Tone::Quiet)],
        ),
        Status::Failed(message) => (
            Some("failed"),
            vec![builder.note(&id, message.clone(), Tone::Problem)],
        ),
        Status::Connected { .. } if entry.databases.is_some() => {
            let children = match &entry.databases {
                Some(Load::Ready(names)) if names.is_empty() => {
                    vec![builder.note(&id, "No databases", Tone::Quiet)]
                }
                Some(Load::Ready(_)) => databases
                    .iter()
                    .map(|child| connection(child, &[], details, expanded, builder.rows))
                    .collect(),
                Some(other) => vec![builder.load_note(&id, other, "Loading databases")],
                None => Vec::new(),
            };
            (Some("connected"), children)
        }
        Status::Connected { catalog, .. } => {
            let children = match catalog {
                Load::Ready(Catalog { schemas }) if schemas.is_empty() => {
                    vec![builder.note(&id, "No schemas", Tone::Quiet)]
                }
                Load::Ready(Catalog { schemas }) => schemas
                    .iter()
                    .map(|schema| builder.schema(&id, schema))
                    .collect(),
                other => vec![builder.load_note(&id, other, "Loading schemas")],
            };
            (Some("connected"), children)
        }
    };

    let live = matches!(entry.status, Status::Connected { .. });
    let row = Row {
        kind: if entry.parent.is_some() {
            Kind::Database
        } else {
            Kind::Connection
        },
        label: entry.saved.name.clone().into(),
        engine: Some(entry.saved.config.kind()),
        secondary: secondary.map(Into::into),
        tone: if live { Tone::Normal } else { Tone::Quiet },
        strong: live,
        target: Target::Connection(entry.saved.id.clone()),
    };
    builder.item(id, row).children(children)
}

#[cfg(test)]
mod tests {
    use core::prelude::v1::test;

    use super::*;

    #[test]
    fn is_within_should_accept_a_descendant() {
        let parent = id::connection("a");

        assert!(id::is_within(&id::child(&parent, "s:public"), &parent));
    }

    #[test]
    fn is_within_should_reject_a_connection_whose_id_only_shares_a_prefix() {
        assert!(!id::is_within(&id::connection("ab"), &id::connection("a")));
    }

    #[test]
    fn connection_of_should_find_the_owner_of_a_nested_row() {
        let row = id::child(&id::child(&id::connection("a1"), "s:public"), "t:book");

        assert_eq!(id::connection_of(&row), Some("a1"));
    }

    #[test]
    fn describe_index_should_name_expression_entries() {
        let index = IndexInfo {
            name: "ix".into(),
            columns: vec![None, Some("price".into())],
            unique: true,
            primary: false,
        };

        assert_eq!(describe_index(&index), "unique (expression, price)");
    }

    #[test]
    fn describe_foreign_key_should_qualify_the_target() {
        let key = ForeignKey {
            name: "fk".into(),
            columns: vec!["author_id".into()],
            references: TableRef::new("public", "author"),
            referenced_columns: vec!["id".into()],
        };

        assert_eq!(
            describe_foreign_key(&key),
            "(author_id) references public.author (id)"
        );
    }
}
