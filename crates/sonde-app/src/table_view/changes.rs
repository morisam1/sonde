//! Edits staged against a table, not yet written.
//!
//! Loaded rows are addressed by their key values as they were read, never by
//! position, so staged edits survive a reload, a new sort or a new filter,
//! and the statements they become match on the original key.

use std::collections::BTreeMap;

use sonde_core::{
    Value,
    driver::Dialect,
    edit::{self, EditStatement},
    schema::TableRef,
};

/// A loaded row's key values, in key-column order, rendered to one string so
/// it can be looked up. Values of different kinds never collide.
pub type RowKey = String;

pub fn row_key(key: &[Value]) -> RowKey {
    let mut out = String::new();
    for value in key {
        out.push_str(value.kind());
        out.push(':');
        out.push_str(&value.to_string());
        out.push('\u{1f}');
    }
    out
}

struct Update {
    key: Vec<Value>,
    /// Column index to the staged value.
    cells: BTreeMap<usize, Value>,
}

/// Every staged change of one table view.
#[derive(Default)]
pub struct Changes {
    updates: BTreeMap<RowKey, Update>,
    deletes: BTreeMap<RowKey, Vec<Value>>,
    /// New rows, by column index. A column left out takes its default.
    inserts: Vec<BTreeMap<usize, Value>>,
}

impl Changes {
    /// How many rows the change set touches. A row that is edited and then
    /// deleted counts once.
    pub fn count(&self) -> usize {
        self.deletes.len()
            + self
                .updates
                .keys()
                .filter(|key| !self.deletes.contains_key(*key))
                .count()
            + self.inserts.len()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Stage `value` for a loaded cell. Staging the value the cell was read
    /// with takes the edit back.
    pub fn set_cell(&mut self, key: &[Value], column: usize, original: &Value, value: Value) {
        let id = row_key(key);
        if &value == original {
            if let Some(update) = self.updates.get_mut(&id) {
                update.cells.remove(&column);
                if update.cells.is_empty() {
                    self.updates.remove(&id);
                }
            }
            return;
        }
        self.updates
            .entry(id)
            .or_insert_with(|| Update {
                key: key.to_vec(),
                cells: BTreeMap::new(),
            })
            .cells
            .insert(column, value);
    }

    /// The staged value of a loaded cell, if it has one.
    pub fn cell(&self, key: &RowKey, column: usize) -> Option<&Value> {
        self.updates.get(key)?.cells.get(&column)
    }

    pub fn is_deleted(&self, key: &RowKey) -> bool {
        self.deletes.contains_key(key)
    }

    /// Stage a loaded row for deletion, or take that back. Its staged edits
    /// are kept, so restoring the row restores them too. Returns whether the
    /// row is now staged for deletion.
    pub fn toggle_delete(&mut self, key: &[Value]) -> bool {
        let id = row_key(key);
        if self.deletes.remove(&id).is_some() {
            false
        } else {
            self.deletes.insert(id, key.to_vec());
            true
        }
    }

    pub fn inserts(&self) -> usize {
        self.inserts.len()
    }

    /// Stage an empty new row and return its index among the new rows.
    pub fn add_insert(&mut self) -> usize {
        self.inserts.push(BTreeMap::new());
        self.inserts.len() - 1
    }

    pub fn remove_insert(&mut self, ix: usize) {
        if ix < self.inserts.len() {
            self.inserts.remove(ix);
        }
    }

    /// Set a new row's cell, or with `None` leave it to the column default.
    pub fn set_insert_cell(&mut self, ix: usize, column: usize, value: Option<Value>) {
        let Some(insert) = self.inserts.get_mut(ix) else {
            return;
        };
        match value {
            Some(value) => insert.insert(column, value),
            None => insert.remove(&column),
        };
    }

    /// A new row's cell, or `None` when it takes the column default.
    pub fn insert_cell(&self, ix: usize, column: usize) -> Option<&Value> {
        self.inserts.get(ix)?.get(&column)
    }

    /// The change set as statements: deletes first, so a key they free can
    /// be reused by an update or insert, then updates, then inserts.
    ///
    /// `columns` names every column index the changes may refer to; the key
    /// columns name the row key's values in order.
    pub fn statements(
        &self,
        dialect: &dyn Dialect,
        table: &TableRef,
        key_columns: &[&str],
        columns: &[&str],
    ) -> Vec<EditStatement> {
        let mut statements = Vec::with_capacity(self.count());
        for key in self.deletes.values() {
            statements.push(edit::delete(dialect, table, key_columns, key));
        }
        for (id, update) in &self.updates {
            if self.deletes.contains_key(id) {
                continue;
            }
            let (names, values): (Vec<&str>, Vec<Value>) = update
                .cells
                .iter()
                .map(|(column, value)| (columns[*column], value.clone()))
                .unzip();
            statements.push(edit::update(
                dialect,
                table,
                key_columns,
                &update.key,
                &names,
                &values,
            ));
        }
        for insert in &self.inserts {
            let (names, values): (Vec<&str>, Vec<Value>) = insert
                .iter()
                .map(|(column, value)| (columns[*column], value.clone()))
                .unzip();
            statements.push(edit::insert(dialect, table, &names, &values));
        }
        statements
    }
}

#[cfg(test)]
mod tests {
    use sonde_core::sqlite::SqliteDialect;

    use super::*;

    const COLUMNS: &[&str] = &["id", "name", "price"];

    fn table() -> TableRef {
        TableRef::unqualified("item")
    }

    fn sql(changes: &Changes) -> Vec<String> {
        changes
            .statements(&SqliteDialect, &table(), &["id"], COLUMNS)
            .into_iter()
            .map(|statement| statement.rendered())
            .collect()
    }

    #[test]
    fn staging_the_original_value_should_take_the_edit_back() {
        let mut changes = Changes::default();
        let key = [Value::Int(1)];

        changes.set_cell(&key, 1, &Value::text("a"), Value::text("b"));
        assert_eq!(changes.count(), 1);
        changes.set_cell(&key, 1, &Value::text("a"), Value::text("a"));

        assert_eq!(changes.count(), 0);
        assert!(sql(&changes).is_empty());
    }

    #[test]
    fn edits_to_one_row_should_become_one_update() {
        let mut changes = Changes::default();
        let key = [Value::Int(1)];

        changes.set_cell(&key, 1, &Value::text("a"), Value::text("b"));
        changes.set_cell(&key, 2, &Value::Null, Value::Float(2.5));

        assert_eq!(
            sql(&changes),
            vec!["UPDATE \"item\" SET \"name\" = 'b', \"price\" = 2.5 WHERE \"id\" = 1"]
        );
    }

    #[test]
    fn a_deleted_row_should_only_be_deleted_and_counted_once() {
        let mut changes = Changes::default();
        let key = [Value::Int(4)];

        changes.set_cell(&key, 1, &Value::text("a"), Value::text("b"));
        assert!(changes.toggle_delete(&key));

        assert_eq!(changes.count(), 1);
        assert_eq!(sql(&changes), vec!["DELETE FROM \"item\" WHERE \"id\" = 4"]);
    }

    #[test]
    fn restoring_a_deleted_row_should_restore_its_edits() {
        let mut changes = Changes::default();
        let key = [Value::Int(4)];

        changes.set_cell(&key, 1, &Value::text("a"), Value::text("b"));
        changes.toggle_delete(&key);
        assert!(!changes.toggle_delete(&key));

        assert_eq!(
            sql(&changes),
            vec!["UPDATE \"item\" SET \"name\" = 'b' WHERE \"id\" = 4"]
        );
    }

    #[test]
    fn statements_should_delete_then_update_then_insert() {
        let mut changes = Changes::default();
        let ix = changes.add_insert();
        changes.set_insert_cell(ix, 1, Some(Value::text("new")));
        changes.set_cell(&[Value::Int(2)], 2, &Value::Int(1), Value::Int(3));
        changes.toggle_delete(&[Value::Int(1)]);

        assert_eq!(
            sql(&changes),
            vec![
                "DELETE FROM \"item\" WHERE \"id\" = 1",
                "UPDATE \"item\" SET \"price\" = 3 WHERE \"id\" = 2",
                "INSERT INTO \"item\" (\"name\") VALUES ('new')",
            ]
        );
    }

    #[test]
    fn a_new_row_should_leave_unset_columns_to_their_defaults() {
        let mut changes = Changes::default();
        let ix = changes.add_insert();
        changes.set_insert_cell(ix, 2, Some(Value::Null));
        changes.set_insert_cell(ix, 1, Some(Value::text("x")));
        changes.set_insert_cell(ix, 1, None);

        assert_eq!(
            sql(&changes),
            vec!["INSERT INTO \"item\" (\"price\") VALUES (NULL)"]
        );
    }

    #[test]
    fn an_untouched_new_row_should_insert_default_values() {
        let mut changes = Changes::default();
        changes.add_insert();

        assert_eq!(sql(&changes), vec!["INSERT INTO \"item\" DEFAULT VALUES"]);
    }

    #[test]
    fn row_keys_should_tell_kinds_apart() {
        assert_ne!(row_key(&[Value::Int(1)]), row_key(&[Value::text("1")]));
        assert_ne!(
            row_key(&[Value::Int(1), Value::Int(23)]),
            row_key(&[Value::Int(12), Value::Int(3)])
        );
    }
}
