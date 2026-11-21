//! Writing a result to CSV or JSON.

use std::{collections::HashSet, io::Write};

use crate::value::{ColumnMeta, Value};

/// The file formats a result can be saved as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Comma-separated values with a header row. `NULL` is an empty field.
    Csv,
    /// A JSON array with one object per row, keyed by column name.
    Json,
}

impl Format {
    /// The usual file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }
}

/// Write `rows` under `columns` in `format`. Returns how many rows were
/// written.
///
/// # Errors
///
/// Whatever `out` reports.
pub fn write<'a>(
    format: Format,
    columns: &[ColumnMeta],
    rows: impl IntoIterator<Item = &'a [Value]>,
    out: impl Write,
) -> std::io::Result<u64> {
    match format {
        Format::Csv => write_csv(columns, rows, out),
        Format::Json => write_json(columns, rows, out),
    }
}

fn write_csv<'a>(
    columns: &[ColumnMeta],
    rows: impl IntoIterator<Item = &'a [Value]>,
    out: impl Write,
) -> std::io::Result<u64> {
    let mut writer = csv::Writer::from_writer(out);
    writer.write_record(columns.iter().map(|column| column.name.as_bytes()))?;
    let mut count = 0;
    for row in rows {
        writer.write_record(row.iter().map(Value::to_string))?;
        count += 1;
    }
    writer.flush()?;
    Ok(count)
}

fn write_json<'a>(
    columns: &[ColumnMeta],
    rows: impl IntoIterator<Item = &'a [Value]>,
    mut out: impl Write,
) -> std::io::Result<u64> {
    let keys = unique_keys(columns);
    out.write_all(b"[")?;
    let mut count = 0;
    for row in rows {
        out.write_all(if count == 0 { b"\n  " } else { b",\n  " })?;
        let object: serde_json::Map<String, serde_json::Value> =
            keys.iter().cloned().zip(row.iter().map(to_json)).collect();
        serde_json::to_writer(&mut out, &object)?;
        count += 1;
    }
    out.write_all(if count == 0 { b"]\n" } else { b"\n]\n" })?;
    out.flush()?;
    Ok(count)
}

/// Column names usable as object keys: a repeated name gets `_2`, `_3` and so
/// on, so no value is lost to a duplicate key.
fn unique_keys(columns: &[ColumnMeta]) -> Vec<String> {
    let mut seen = HashSet::new();
    columns
        .iter()
        .map(|column| {
            let mut key = column.name.to_string();
            let mut n = 1;
            while !seen.insert(key.clone()) {
                n += 1;
                key = format!("{}_{n}", column.name);
            }
            key
        })
        .collect()
}

/// A cell as JSON. Numbers stay numbers where JSON can hold them exactly;
/// decimals become strings so no digit is lost, and JSON columns are embedded
/// as JSON rather than as a string of it.
pub fn to_json(value: &Value) -> serde_json::Value {
    use serde_json::Value as Json;

    match value {
        Value::Null => Json::Null,
        Value::Bool(value) => Json::Bool(*value),
        Value::Int(value) => Json::from(*value),
        Value::Float(value) => serde_json::Number::from_f64(*value)
            .map_or_else(|| Json::String(value.to_string()), Json::Number),
        Value::Json(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Json::String(text.to_string()))
        }
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        other => Json::String(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn columns(names: &[&str]) -> Vec<ColumnMeta> {
        names
            .iter()
            .map(|name| ColumnMeta {
                name: Arc::from(*name),
                type_name: Arc::from(""),
                origin: None,
            })
            .collect()
    }

    fn export(format: Format, names: &[&str], rows: &[Vec<Value>]) -> String {
        let mut out = Vec::new();
        let count = write(
            format,
            &columns(names),
            rows.iter().map(Vec::as_slice),
            &mut out,
        )
        .unwrap();
        assert_eq!(count, rows.len() as u64);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn csv_should_quote_fields_that_need_it_and_leave_null_empty() {
        let rows = vec![
            vec![Value::Int(1), Value::text("plain")],
            vec![Value::Int(2), Value::text("a, \"quoted\"\nline")],
            vec![Value::Null, Value::Null],
        ];

        let text = export(Format::Csv, &["id", "note"], &rows);

        assert_eq!(text, "id,note\n1,plain\n2,\"a, \"\"quoted\"\"\nline\"\n,\n");
    }

    #[test]
    fn csv_should_write_a_header_for_an_empty_result() {
        assert_eq!(export(Format::Csv, &["a", "b"], &[]), "a,b\n");
    }

    #[test]
    fn json_should_keep_types_and_embed_json_columns() {
        let rows = vec![vec![
            Value::Int(7),
            Value::Float(1.5),
            Value::Decimal("10.20".parse().unwrap()),
            Value::Bool(true),
            Value::Null,
            Value::Json(Arc::from(r#"{"k":[1,2]}"#)),
            Value::Array(Arc::from([Value::Int(1), Value::Null])),
        ]];

        let text = export(Format::Json, &["i", "f", "d", "b", "n", "j", "a"], &rows);

        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([{
                "i": 7, "f": 1.5, "d": "10.20", "b": true, "n": null,
                "j": {"k": [1, 2]}, "a": [1, null]
            }])
        );
    }

    #[test]
    fn json_should_rename_repeated_columns_instead_of_dropping_them() {
        let rows = vec![vec![Value::Int(1), Value::Int(2), Value::Int(3)]];

        let text = export(Format::Json, &["id", "id", "id"], &rows);

        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, serde_json::json!([{"id": 1, "id_2": 2, "id_3": 3}]));
    }

    #[test]
    fn json_should_write_an_empty_array_for_no_rows() {
        assert_eq!(export(Format::Json, &["a"], &[]), "[]\n");
    }

    #[test]
    fn json_should_keep_non_finite_floats_as_text() {
        assert_eq!(to_json(&Value::Float(f64::NAN)), serde_json::json!("NaN"));
    }
}
