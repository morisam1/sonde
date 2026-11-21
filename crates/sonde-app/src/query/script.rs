//! Deciding which statements a run command means.

use std::ops::Range;

use sonde_core::{DbKind, split};

/// One statement queued to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub sql: String,
    /// Byte offset of `sql` in the editor's text, for placing errors.
    pub offset: usize,
}

/// Every statement in the script.
pub fn all(script: &str, kind: DbKind) -> Vec<Planned> {
    within(script, 0..script.len(), kind)
}

/// The statements inside a selected range of the script.
pub fn within(script: &str, range: Range<usize>, kind: DbKind) -> Vec<Planned> {
    let Some(selected) = script.get(range.clone()) else {
        return Vec::new();
    };
    split::split(selected, kind)
        .into_iter()
        .map(|statement| Planned {
            sql: statement.sql.to_owned(),
            offset: range.start + statement.offset,
        })
        .collect()
}

/// The statement the cursor is in.
///
/// Between statements, that is the one just before the cursor, since that is
/// where the cursor rests after typing a statement's semicolon. Before the
/// first statement it is the first.
pub fn at_cursor(script: &str, cursor: usize, kind: DbKind) -> Option<Planned> {
    let statements = all(script, kind);
    let before = statements
        .iter()
        .rposition(|statement| statement.offset <= cursor)
        .unwrap_or(0);
    statements.into_iter().nth(before)
}

/// Line and column of a byte offset, both counted from 1, columns in
/// characters.
pub fn line_column(script: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(script.len());
    while !script.is_char_boundary(end) {
        end -= 1;
    }
    let before = &script[..end];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use core::prelude::v1::test;

    use super::*;

    const SCRIPT: &str = "select 1;\n\nselect 2;\nselect 3";

    fn sql_at(cursor: usize) -> Option<String> {
        at_cursor(SCRIPT, cursor, DbKind::Postgres).map(|planned| planned.sql)
    }

    #[test]
    fn at_cursor_should_pick_the_statement_containing_the_cursor() {
        assert_eq!(
            sql_at(SCRIPT.find("2").unwrap()).as_deref(),
            Some("select 2")
        );
    }

    #[test]
    fn at_cursor_should_pick_the_statement_just_finished() {
        assert_eq!(sql_at("select 1;".len()).as_deref(), Some("select 1"));
    }

    #[test]
    fn at_cursor_should_pick_the_last_statement_at_the_end() {
        assert_eq!(sql_at(SCRIPT.len()).as_deref(), Some("select 3"));
    }

    #[test]
    fn at_cursor_should_pick_the_first_statement_when_before_it() {
        let planned = at_cursor("\n\n  select 1; select 2", 0, DbKind::Postgres);

        assert_eq!(planned.map(|p| p.sql).as_deref(), Some("select 1"));
    }

    #[test]
    fn at_cursor_should_find_nothing_in_an_empty_script() {
        assert_eq!(at_cursor("  -- nothing\n", 3, DbKind::Postgres), None);
    }

    #[test]
    fn within_should_report_offsets_in_the_whole_script() {
        let start = SCRIPT.find("select 2").unwrap();

        let planned = within(SCRIPT, start..SCRIPT.len(), DbKind::Postgres);

        assert_eq!(
            planned
                .iter()
                .map(|p| &SCRIPT[p.offset..p.offset + 8])
                .collect::<Vec<_>>(),
            ["select 2", "select 3"]
        );
    }

    #[test]
    fn line_column_should_count_characters_not_bytes() {
        let script = "select 'é',\n  'ü' frm";

        assert_eq!(line_column(script, script.find("frm").unwrap()), (2, 7));
    }
}
