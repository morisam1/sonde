//! Staged table edits and the parameterised SQL they become.
//!
//! An edit is always keyed on the row's key values *as they were when the row
//! was read*: `UPDATE` and `DELETE` match on them, so a row that moved or went
//! away in the meantime makes the statement touch zero rows, and
//! [`Session::apply`](crate::Session::apply) rolls the whole change set back.

use crate::{driver::Dialect, schema::TableRef, value::Value};

/// One parameterised statement of a change set.
#[derive(Debug, Clone, PartialEq)]
pub struct EditStatement {
    /// The statement text with the dialect's placeholders.
    pub sql: String,
    /// The values for the placeholders, in order.
    pub params: Vec<Value>,
}

impl EditStatement {
    /// The statement with its values written in, for a human to review.
    /// Identifiers keep their quotes; text values are single-quoted.
    pub fn rendered(&self) -> String {
        render(&self.sql, &self.params, self.params.len())
    }
}

/// Whether the text so far leaves an identifier or string open.
fn inside_quotes(done: &str, pending: &str) -> bool {
    let count = |quote: char| {
        done.chars()
            .chain(pending.chars())
            .filter(|&c| c == quote)
            .count()
    };
    count('`') % 2 == 1 || count('"') % 2 == 1 || count('\'') % 2 == 1 || count('[') > count(']')
}

fn render(sql: &str, params: &[Value], count: usize) -> String {
    let mut out = String::new();
    let mut pending = String::new();
    let mut next = 1;
    let mut chars = sql.chars().peekable();
    while let Some(character) = chars.next() {
        // SQL Server's `@P1`: step over the `P` so the digits follow.
        if character == '@' && chars.peek() == Some(&'P') {
            let mut ahead = chars.clone();
            ahead.next();
            if ahead.peek().is_some_and(char::is_ascii_digit) && !inside_quotes(&out, &pending) {
                chars.next();
            }
        }
        let numbered = matches!(character, '$' | '?' | '@')
            && chars.peek().is_some_and(|digit| digit.is_ascii_digit());
        // MySQL's placeholders are a bare `?`. Generated SQL quotes every
        // identifier, so a `?` outside quotes is always a placeholder.
        let bare = character == '?' && !numbered && !inside_quotes(&out, &pending);
        if numbered || bare {
            if next <= count {
                out.push_str(&std::mem::take(&mut pending));
                out.push_str(&literate(&params[next - 1]));
                next += 1;
            } else {
                pending.push(character);
            }
            while chars.peek().is_some_and(|digit| digit.is_ascii_digit()) {
                chars.next();
            }
        } else {
            pending.push(character);
        }
    }
    out.push_str(&pending);
    out
}

/// A value the way it would sit in hand-written SQL.
fn literate(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Text(text) | Value::Json(text) => format!("'{}'", text.replace('\'', "''")),
        Value::Bytes(bytes) => {
            let mut out = String::from("'\\x");
            for byte in bytes.iter() {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push('\'');
            out
        }
        Value::Array(items) => format!(
            "'{{{}}}'",
            items
                .iter()
                .map(literal_unquoted)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Unknown { raw, .. } => match std::str::from_utf8(raw) {
            Ok(text) if !text.contains('\0') => format!("'{}'", text.replace('\'', "''")),
            _ => literate(&Value::Bytes(raw.clone())),
        },
        // Dates, times, intervals and UUIDs are strings to SQL; unquoted they
        // would read as arithmetic.
        Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_)
        | Value::TimestampTz(_)
        | Value::Interval { .. }
        | Value::Uuid(_) => format!("'{value}'"),
        other => other.to_string(),
    }
}

fn literal_unquoted(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Text(text) | Value::Json(text) => format!("'{}'", text.replace('\'', "''")),
        other => other.to_string(),
    }
}

/// `INSERT INTO table (a, b) VALUES ($1, $2)`, or `DEFAULT VALUES` when no
/// column is given.
pub fn insert(
    dialect: &dyn Dialect,
    table: &TableRef,
    columns: &[&str],
    values: &[Value],
) -> EditStatement {
    dialect.insert(table, columns, values)
}

/// [`insert`] as SQL; what [`Dialect::insert`] does unless a database
/// overrides it.
pub fn sql_insert<D: Dialect + ?Sized>(
    dialect: &D,
    table: &TableRef,
    columns: &[&str],
    values: &[Value],
) -> EditStatement {
    debug_assert_eq!(columns.len(), values.len(), "one value per column");
    if columns.is_empty() {
        return EditStatement {
            sql: dialect.insert_defaults(table),
            params: Vec::new(),
        };
    }
    let mut sql = format!("INSERT INTO {} (", dialect.quote_table(table));
    let mut params = Vec::with_capacity(values.len());
    for (ix, column) in columns.iter().enumerate() {
        if ix > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&dialect.quote_ident(column));
        params.push(values[ix].clone());
    }
    sql.push_str(") VALUES (");
    for (ix, _) in columns.iter().enumerate() {
        if ix > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&dialect.placeholder(ix + 1));
    }
    sql.push(')');
    EditStatement { sql, params }
}

/// `UPDATE table SET a = $1 WHERE k1 = $2 AND k2 IS NULL`.
///
/// Key columns whose read value was `NULL` match with `IS NULL` and take no
/// parameter, because `=` never matches a `NULL`.
pub fn update(
    dialect: &dyn Dialect,
    table: &TableRef,
    key_columns: &[&str],
    key: &[Value],
    columns: &[&str],
    values: &[Value],
) -> EditStatement {
    dialect.update(table, key_columns, key, columns, values)
}

/// [`update`] as SQL; what [`Dialect::update`] does unless a database
/// overrides it.
pub fn sql_update<D: Dialect + ?Sized>(
    dialect: &D,
    table: &TableRef,
    key_columns: &[&str],
    key: &[Value],
    columns: &[&str],
    values: &[Value],
) -> EditStatement {
    debug_assert_eq!(key_columns.len(), key.len(), "one key value per key column");
    debug_assert_eq!(columns.len(), values.len(), "one value per column");

    let mut sql = format!("UPDATE {} SET ", dialect.quote_table(table));
    let mut params = Vec::with_capacity(key.len() + values.len());
    for (ix, column) in columns.iter().enumerate() {
        if ix > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&dialect.quote_ident(column));
        sql.push_str(" = ");
        sql.push_str(&dialect.placeholder(params.len() + 1));
        params.push(values[ix].clone());
    }
    sql.push_str(" WHERE ");
    append_key_condition(dialect, &mut sql, &mut params, key_columns, key);
    EditStatement { sql, params }
}

/// `DELETE FROM table WHERE k1 = $1`.
pub fn delete(
    dialect: &dyn Dialect,
    table: &TableRef,
    key_columns: &[&str],
    key: &[Value],
) -> EditStatement {
    dialect.delete(table, key_columns, key)
}

/// [`delete`] as SQL; what [`Dialect::delete`] does unless a database
/// overrides it.
pub fn sql_delete<D: Dialect + ?Sized>(
    dialect: &D,
    table: &TableRef,
    key_columns: &[&str],
    key: &[Value],
) -> EditStatement {
    debug_assert_eq!(key_columns.len(), key.len(), "one key value per key column");

    let mut params = Vec::with_capacity(key.len());
    let mut tail = String::new();
    append_key_condition(dialect, &mut tail, &mut params, key_columns, key);
    EditStatement {
        sql: format!("DELETE FROM {} WHERE {}", dialect.quote_table(table), tail),
        params,
    }
}

/// `k1 = $n AND k2 IS NULL ...` for a row's key, appended to `sql`, which is
/// expected to end at the start of the condition list.
fn append_key_condition<D: Dialect + ?Sized>(
    dialect: &D,
    sql: &mut String,
    params: &mut Vec<Value>,
    key_columns: &[&str],
    key: &[Value],
) {
    let mut first = true;
    for (column, value) in key_columns.iter().zip(key) {
        if !first {
            sql.push_str(" AND ");
        }
        first = false;
        sql.push_str(&dialect.quote_ident(column));
        match value {
            Value::Null => sql.push_str(" IS NULL"),
            value => {
                sql.push_str(" = ");
                sql.push_str(&dialect.placeholder(params.len() + 1));
                params.push(value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{schema::TableRef, value::Value};

    const PG: &crate::postgres::PostgresDialect = &crate::postgres::PostgresDialect;
    const SQLITE: &crate::sqlite::SqliteDialect = &crate::sqlite::SqliteDialect;

    fn table() -> TableRef {
        TableRef::new("public", "Order Items")
    }

    #[test]
    fn insert_should_quote_the_table_and_columns() {
        let statement = insert(PG, &table(), &["a b", "c"], &[Value::Int(1), Value::Null]);

        assert_eq!(
            statement.sql,
            "INSERT INTO \"public\".\"Order Items\" (\"a b\", \"c\") VALUES ($1, $2)"
        );
        assert_eq!(statement.params, vec![Value::Int(1), Value::Null]);
    }

    #[test]
    fn update_should_bind_set_values_before_key_values() {
        let statement = update(
            SQLITE,
            &table(),
            &["id"],
            &[Value::Int(3)],
            &["price"],
            &[Value::text("19.99")],
        );

        assert_eq!(
            statement.sql,
            "UPDATE \"public\".\"Order Items\" SET \"price\" = ?1 WHERE \"id\" = ?2"
        );
        assert_eq!(statement.params, vec![Value::text("19.99"), Value::Int(3)]);
    }

    #[test]
    fn update_should_key_a_composite_key_in_order() {
        let statement = update(
            PG,
            &table(),
            &["a", "b"],
            &[Value::Int(1), Value::text("x")],
            &["c"],
            &[Value::Bool(true)],
        );

        assert_eq!(
            statement.sql,
            "UPDATE \"public\".\"Order Items\" SET \"c\" = $1 WHERE \"a\" = $2 AND \"b\" = $3"
        );
        assert_eq!(
            statement.params,
            vec![Value::Bool(true), Value::Int(1), Value::text("x")]
        );
    }

    #[test]
    fn null_key_values_should_match_with_is_null_and_take_no_parameter() {
        let statement = delete(
            SQLITE,
            &table(),
            &["k1", "k2"],
            &[Value::Null, Value::Int(7)],
        );

        assert_eq!(
            statement.sql,
            "DELETE FROM \"public\".\"Order Items\" WHERE \"k1\" IS NULL AND \"k2\" = ?1"
        );
        assert_eq!(statement.params, vec![Value::Int(7)]);
    }

    #[test]
    fn delete_should_quote_every_key_column() {
        let statement = delete(PG, &table(), &["id"], &[Value::Int(9)]);

        assert_eq!(
            statement.sql,
            "DELETE FROM \"public\".\"Order Items\" WHERE \"id\" = $1"
        );
        assert_eq!(statement.params, vec![Value::Int(9)]);
    }

    #[test]
    fn unqualified_tables_should_not_gain_a_schema() {
        let plain = TableRef::unqualified("note");

        assert_eq!(
            insert(PG, &plain, &["body"], &[Value::text("x")]).sql,
            "INSERT INTO \"note\" (\"body\") VALUES ($1)"
        );
    }

    #[test]
    fn insert_without_columns_should_use_default_values() {
        let statement = insert(SQLITE, &table(), &[], &[]);

        assert_eq!(
            statement.sql,
            "INSERT INTO \"public\".\"Order Items\" DEFAULT VALUES"
        );
        assert!(statement.params.is_empty());
    }

    #[test]
    fn rendered_should_write_values_in_place_of_the_placeholders() {
        let statement = update(
            PG,
            &table(),
            &["id"],
            &[Value::Int(3)],
            &["title", "note"],
            &[Value::text("O'brien"), Value::Null],
        );

        assert_eq!(
            statement.rendered(),
            "UPDATE \"public\".\"Order Items\" SET \"title\" = 'O''brien', \"note\" = NULL WHERE \"id\" = 3"
        );
    }

    #[test]
    fn rendered_should_quote_dates_and_uuids() {
        let statement = insert(
            PG,
            &table(),
            &["d", "u"],
            &[
                crate::value::parse("date", "1969-07-20").unwrap(),
                crate::value::parse("uuid", "01234567-89ab-4cde-9f01-23456789abcd").unwrap(),
            ],
        );

        assert_eq!(
            statement.rendered(),
            "INSERT INTO \"public\".\"Order Items\" (\"d\", \"u\") \
             VALUES ('1969-07-20', '01234567-89ab-4cde-9f01-23456789abcd')"
        );
    }

    #[test]
    fn rendered_should_quote_text_and_bytes() {
        let statement = insert(
            PG,
            &table(),
            &["blob"],
            &[Value::Bytes(vec![0xde, 0xad].into())],
        );

        assert_eq!(
            statement.rendered(),
            "INSERT INTO \"public\".\"Order Items\" (\"blob\") VALUES ('\\xdead')"
        );
    }
}
