//! How values are shown in a grid cell, copied, and opened in full.

use gpui_kit::SharedString;
use sonde_core::Value;

/// Longest text drawn in a cell. The full value is a double-click away.
const CELL_CHARS: usize = 200;

/// Whether a column of this database type holds numbers, which are
/// right-aligned so their digits line up.
pub fn is_numeric(type_name: &str) -> bool {
    let name = type_name.to_ascii_lowercase();
    [
        "int", "float", "numeric", "decimal", "real", "double", "serial", "money", "oid",
    ]
    .iter()
    .any(|numeric| name.contains(numeric))
        && !name.starts_with('_')
        && !name.contains("interval")
        && !name.contains("point")
}

/// A starting width for a column, in pixels, from what its type usually
/// holds. The user can drag it from there.
pub fn column_width(name: &str, type_name: &str) -> f32 {
    /// Roughly one monospace character at the grid's size, plus cell padding.
    const CHARACTER: f32 = 8.;
    const PADDING: f32 = crate::design::GRID_CELL_INSET * 2. + 4.;

    let kind = type_name.to_ascii_lowercase();
    let characters: usize = if kind.contains("timestamp") || kind.contains("datetime") {
        29
    } else if kind.contains("uuid") {
        36
    } else if kind.contains("bool") {
        5
    } else if kind == "date" || kind == "time" {
        12
    } else if is_numeric(&kind) {
        14
    } else {
        26
    };
    let characters = characters.max(name.chars().count() + 1).min(48);
    #[expect(
        clippy::cast_precision_loss,
        reason = "at most 48, far inside what f32 holds exactly"
    )]
    let width = characters as f32 * CHARACTER + PADDING;
    width
}

/// A duration at the precision a person reads it: milliseconds below a
/// second, seconds above.
pub fn elapsed(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 0.001 {
        "under 1 ms".to_owned()
    } else if seconds < 1. {
        format!("{:.0} ms", seconds * 1000.)
    } else {
        format!("{seconds:.2} s")
    }
}

/// The single line a cell shows, or `None` for `NULL`.
pub fn display(value: &Value) -> Option<SharedString> {
    if value.is_null() {
        return None;
    }
    let full = value.to_string();
    let mut line = String::with_capacity(full.len().min(CELL_CHARS + 4));
    for (count, character) in full.chars().enumerate() {
        if count == CELL_CHARS {
            line.push('…');
            break;
        }
        // A cell is one line high; line breaks would be clipped away.
        line.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    Some(line.into())
}

/// The whole value as text for the detail view. JSON is indented; anything
/// that fails to parse as JSON is shown as it is.
pub fn detail(value: &Value) -> (String, bool) {
    if let Value::Json(source) = value {
        let pretty = serde_json::from_str::<serde_json::Value>(source)
            .ok()
            .and_then(|parsed| serde_json::to_string_pretty(&parsed).ok());
        return (pretty.unwrap_or_else(|| source.to_string()), true);
    }
    (value.to_string(), false)
}

/// Rows as tab-separated text, one line per row. `NULL` is an empty field,
/// and tabs and line breaks inside a value are written as `\t` and `\n` so
/// each row stays on its line.
pub fn tsv<'a>(rows: impl Iterator<Item = &'a [Value]>) -> String {
    let mut out = String::new();
    for row in rows {
        for (ix, value) in row.iter().enumerate() {
            if ix > 0 {
                out.push('\t');
            }
            for character in value.to_string().chars() {
                match character {
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    other => out.push(other),
                }
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use core::prelude::v1::test;

    use super::*;

    #[test]
    fn is_numeric_should_accept_postgres_and_sqlite_number_types() {
        let numeric = [
            "int4",
            "numeric",
            "float8",
            "INTEGER",
            "REAL",
            "DECIMAL(10,2)",
        ];

        assert!(numeric.iter().all(|name| is_numeric(name)));
    }

    #[test]
    fn is_numeric_should_reject_text_arrays_and_intervals() {
        let other = ["text", "_int4", "interval", "timestamptz", "", "point"];

        assert!(other.iter().all(|name| !is_numeric(name)));
    }

    #[test]
    fn column_width_should_fit_a_long_header() {
        assert!(column_width("a_rather_long_column_name", "bool") > column_width("ok", "bool"));
    }

    #[test]
    fn elapsed_should_use_milliseconds_below_a_second() {
        assert_eq!(elapsed(std::time::Duration::from_millis(42)), "42 ms");
    }

    #[test]
    fn elapsed_should_use_seconds_from_one_second() {
        assert_eq!(elapsed(std::time::Duration::from_millis(4615)), "4.62 s");
    }

    #[test]
    fn display_should_be_none_for_null() {
        assert_eq!(display(&Value::Null), None);
    }

    #[test]
    fn display_should_keep_a_multiline_value_on_one_line() {
        assert_eq!(display(&Value::text("a\nb")).as_deref(), Some("a b"));
    }

    #[test]
    fn display_should_cut_long_text_with_an_ellipsis() {
        let shown = display(&Value::text("x".repeat(500))).unwrap();

        assert_eq!(shown.chars().count(), CELL_CHARS + 1);
    }

    #[test]
    fn detail_should_indent_json() {
        let (text, _) = detail(&Value::Json("{\"a\":[1,2]}".into()));

        assert_eq!(text, "{\n  \"a\": [\n    1,\n    2\n  ]\n}");
    }

    #[test]
    fn tsv_should_escape_tabs_and_newlines_and_leave_null_empty() {
        let rows = [
            vec![Value::Int(1), Value::text("a\tb\nc")],
            vec![Value::Null, Value::Bool(true)],
        ];

        let text = tsv(rows.iter().map(Vec::as_slice));

        assert_eq!(text, "1\ta\\tb\\nc\n\ttrue\n");
    }
}
