//! Cut a script into statements.
//!
//! Only the tokenizer is used: it knows quotes, comments and dollar-quoting,
//! which is all that is needed to find the semicolons that end statements.

use sqlparser::{
    dialect::{
        Dialect, GenericDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
        SnowflakeDialect,
    },
    tokenizer::{Location, Token, TokenWithSpan, Tokenizer},
};

use crate::driver::{DbKind, Language};

/// One statement of a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Statement<'a> {
    /// The statement's text, without its terminating semicolon.
    pub sql: &'a str,
    /// Byte offset of `sql` in the script. Add it to an error position to
    /// place the error in the script.
    pub offset: usize,
    /// Line of the script the statement starts on, counting from 1.
    pub line: usize,
}

/// Split `script` into its statements, in order.
///
/// Statements holding nothing but whitespace and comments are dropped. A
/// script the tokenizer cannot finish, such as one with an unterminated
/// string, keeps everything from the last good semicolon as its final
/// statement, so the database gets to report the real error.
pub fn split(script: &str, kind: DbKind) -> Vec<Statement<'_>> {
    match kind.language() {
        Language::Redis => return lines(script),
        Language::Mongo => return calls(script),
        Language::Elastic => return requests(script),
        Language::Sql | Language::Cql => {}
    }
    split_sql(script, kind)
}

fn split_sql(script: &str, kind: DbKind) -> Vec<Statement<'_>> {
    let dialect: &dyn Dialect = match kind {
        DbKind::Postgres => &PostgreSqlDialect {},
        DbKind::Mysql | DbKind::Mariadb => &MySqlDialect {},
        DbKind::SqlServer => &MsSqlDialect {},
        DbKind::Sqlite => &SQLiteDialect {},
        DbKind::Snowflake => &SnowflakeDialect {},
        _ => &GenericDialect {},
    };
    let mut tokens = Vec::new();
    // On error the buffer still holds every token before the bad one, and
    // the tail of the script is swept into the last statement below.
    let _ = Tokenizer::new(dialect, script)
        .with_unescape(false)
        .tokenize_with_location_into_buf(&mut tokens);

    let offsets = Offsets::new(script);
    let mut statements = Vec::new();
    let mut current = Pending::new(kind);

    for TokenWithSpan { token, span } in &tokens {
        match token {
            Token::Whitespace(_) | Token::EOF => {}
            // SQL Server tools end a batch with GO on a line of its own.
            Token::Word(word)
                if kind == DbKind::SqlServer
                    && word.quote_style.is_none()
                    && word.value.eq_ignore_ascii_case("GO")
                    && alone_on_line(script, &offsets, span.start, span.end) =>
            {
                if let Some(start) = current.start {
                    let end = offsets.of(span.start);
                    statements.push(statement(script, &offsets, start, end));
                }
                current = Pending::new(kind);
            }
            Token::SemiColon if current.ends_here() => {
                if let Some(start) = current.start {
                    let end = offsets.of(span.start);
                    statements.push(statement(script, &offsets, start, end));
                }
                current = Pending::new(kind);
            }
            token => current.push(token, span.start),
        }
    }
    if let Some(start) = current.start {
        statements.push(statement(script, &offsets, start, script.len()));
    }
    statements
}

/// Whether the token between `start` and `end` is the only thing on its line.
fn alone_on_line(script: &str, offsets: &Offsets, start: Location, end: Location) -> bool {
    let (from, to) = (offsets.of(start), offsets.of(end));
    let line_start = script[..from].rfind('\n').map_or(0, |ix| ix + 1);
    let line_end = script[to..].find('\n').map_or(script.len(), |ix| to + ix);
    script[line_start..from].trim().is_empty() && script[to..line_end].trim().is_empty()
}

fn statement<'a>(script: &'a str, offsets: &Offsets, start: Location, end: usize) -> Statement<'a> {
    let offset = offsets.of(start);
    Statement {
        sql: script[offset..end].trim_end(),
        offset,
        line: usize::try_from(start.line).unwrap_or(usize::MAX),
    }
}

/// The statement being accumulated.
struct Pending {
    start: Option<Location>,
    kind: DbKind,
    /// Non-whitespace tokens seen so far, capped once the prefix is decided.
    seen: usize,
    starts_with_create: bool,
    /// Inside `CREATE TRIGGER ... BEGIN ... END`, whose body has its own
    /// semicolons.
    trigger: bool,
    case_depth: usize,
    /// The last token was the `END` that closes a trigger body.
    after_body_end: bool,
    /// Inside a MySQL `CREATE PROCEDURE`, `FUNCTION`, `TRIGGER` or `EVENT`,
    /// whose `BEGIN ... END` blocks have their own semicolons.
    routine: bool,
    /// Open `BEGIN` blocks of a MySQL routine.
    blocks: usize,
    /// An `END` whose meaning depends on the next word: `END IF` and the
    /// like close no block.
    end_pending: bool,
}

impl Pending {
    /// How many leading words may sit between `CREATE` and `TRIGGER`.
    const PREFIX_WORDS: usize = 4;
    /// How many tokens may come before MySQL's `PROCEDURE` and kin:
    /// ``DEFINER = `user`@`host` `` alone is five.
    const MYSQL_PREFIX_TOKENS: usize = 12;

    fn new(kind: DbKind) -> Self {
        Self {
            start: None,
            kind,
            seen: 0,
            starts_with_create: false,
            trigger: false,
            case_depth: 0,
            after_body_end: false,
            routine: false,
            blocks: 0,
            end_pending: false,
        }
    }

    /// Track MySQL's nested blocks.
    fn push_routine_word(&mut self, word: &str) {
        if std::mem::take(&mut self.end_pending) {
            match word {
                "IF" | "LOOP" | "WHILE" | "REPEAT" => return,
                "CASE" => {
                    self.case_depth = self.case_depth.saturating_sub(1);
                    return;
                }
                _ => self.close_end(),
            }
        }
        match word {
            "BEGIN" => self.blocks += 1,
            "CASE" => self.case_depth += 1,
            "END" => self.end_pending = true,
            _ => {}
        }
    }

    fn close_end(&mut self) {
        if self.case_depth > 0 {
            self.case_depth -= 1;
        } else {
            self.blocks = self.blocks.saturating_sub(1);
        }
    }

    fn push(&mut self, token: &Token, at: Location) {
        self.start.get_or_insert(at);
        self.after_body_end = false;

        let word = match token {
            Token::Word(word) if word.quote_style.is_none() => word.value.to_ascii_uppercase(),
            _ => String::new(),
        };
        if self.seen == 0 {
            self.starts_with_create = word == "CREATE";
        } else if matches!(self.kind, DbKind::Mysql | DbKind::Mariadb)
            && self.starts_with_create
            && !self.routine
            && self.seen < Self::MYSQL_PREFIX_TOKENS
            && matches!(
                word.as_str(),
                "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT"
            )
        {
            self.routine = true;
        } else if self.seen < Self::PREFIX_WORDS
            && self.starts_with_create
            && self.kind == DbKind::Sqlite
            && word == "TRIGGER"
        {
            self.trigger = true;
        }
        self.seen += 1;

        if self.routine && !word.is_empty() {
            self.push_routine_word(&word);
        } else if self.routine {
            // A non-word token settles a pending END: `END;` or `END$`.
            if std::mem::take(&mut self.end_pending) {
                self.close_end();
            }
        }

        if self.trigger {
            match word.as_str() {
                "CASE" => self.case_depth += 1,
                "END" if self.case_depth > 0 => self.case_depth -= 1,
                "END" => self.after_body_end = true,
                _ => {}
            }
        }
    }

    /// Whether a semicolon at this point ends the statement.
    fn ends_here(&mut self) -> bool {
        if self.routine && std::mem::take(&mut self.end_pending) {
            self.close_end();
        }
        (!self.trigger || self.after_body_end) && (!self.routine || self.blocks == 0)
    }
}

/// Converts the tokenizer's line and column (in characters, from 1) to byte
/// offsets.
struct Offsets<'a> {
    script: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Offsets<'a> {
    fn new(script: &'a str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(script.match_indices('\n').map(|(ix, _)| ix + 1))
            .collect();
        Self {
            script,
            line_starts,
        }
    }

    fn of(&self, location: Location) -> usize {
        let line = usize::try_from(location.line.saturating_sub(1)).unwrap_or(usize::MAX);
        let Some(&line_start) = self.line_starts.get(line) else {
            return self.script.len();
        };
        let column = usize::try_from(location.column.saturating_sub(1)).unwrap_or(usize::MAX);
        self.script[line_start..]
            .char_indices()
            .nth(column)
            .map_or(self.script.len(), |(ix, _)| line_start + ix)
    }
}

/// The lines of `script` with something to run: one Redis command each.
/// Blank lines and lines starting with `#` or `//` are skipped.
fn lines(script: &str) -> Vec<Statement<'_>> {
    let mut statements = Vec::new();
    let mut offset = 0;
    for (ix, line) in script.split_inclusive('\n').enumerate() {
        let text = line.trim();
        if !text.is_empty() && !text.starts_with('#') && !text.starts_with("//") {
            let lead = line.len() - line.trim_start().len();
            statements.push(Statement {
                sql: text,
                offset: offset + lead,
                line: ix + 1,
            });
        }
        offset += line.len();
    }
    statements
}

/// The byte where the string starting at `from` (on its quote) ends.
fn string_end(script: &str, from: usize) -> usize {
    let bytes = script.as_bytes();
    let quote = bytes[from];
    let mut ix = from + 1;
    while ix < bytes.len() {
        match bytes[ix] {
            b'\\' => ix += 2,
            b if b == quote => return ix + 1,
            _ => ix += 1,
        }
    }
    bytes.len()
}

fn line_of(script: &str, offset: usize) -> usize {
    script[..offset].bytes().filter(|&b| b == b'\n').count() + 1
}

/// Push the trimmed text between `start` and `end`, unless it is empty.
fn push_trimmed<'a>(
    statements: &mut Vec<Statement<'a>>,
    script: &'a str,
    start: usize,
    end: usize,
) {
    let text = &script[start..end];
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let offset = start + (text.len() - text.trim_start().len());
    statements.push(Statement {
        sql: trimmed,
        offset,
        line: line_of(script, offset),
    });
}

/// `mongosh` calls: a `;`, or a line break outside any brackets, ends one,
/// unless the next line carries on with `.method(...)`. Comments (`//`
/// and `/* */`) are skipped over.
fn calls(script: &str) -> Vec<Statement<'_>> {
    let bytes = script.as_bytes();
    let mut statements = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let mut ix = 0;
    while ix < bytes.len() {
        match bytes[ix] {
            b'"' | b'\'' | b'`' => {
                ix = string_end(script, ix);
                continue;
            }
            b'/' if bytes.get(ix + 1) == Some(&b'/') => {
                let end = script[ix..].find('\n').map_or(bytes.len(), |n| ix + n);
                if script[start..ix].trim().is_empty() {
                    start = end;
                }
                ix = end;
                continue;
            }
            b'/' if bytes.get(ix + 1) == Some(&b'*') => {
                let end = script[ix + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |n| ix + 2 + n + 2);
                if script[start..ix].trim().is_empty() {
                    start = end;
                }
                ix = end;
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => {
                push_trimmed(&mut statements, script, start, ix);
                start = ix + 1;
            }
            b'\n' if depth == 0 => {
                let rest = script[ix + 1..].trim_start();
                if !rest.starts_with('.') && !script[start..ix].trim().is_empty() {
                    push_trimmed(&mut statements, script, start, ix);
                    start = ix + 1;
                }
            }
            _ => {}
        }
        ix += 1;
    }
    push_trimmed(&mut statements, script, start, bytes.len());
    statements
}

const METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "HEAD"];

/// Whether `line` opens a REST request, as in `GET /index/_search`.
pub fn is_request_line(line: &str) -> bool {
    let line = line.trim_start();
    METHODS.iter().any(|method| {
        line.len() > method.len()
            && line[..method.len()].eq_ignore_ascii_case(method)
            && line[method.len()..].starts_with([' ', '\t'])
    })
}

/// Kibana-console requests, each a method line and the JSON body under it,
/// mixed with SQL statements, which `;` ends as usual.
fn requests(script: &str) -> Vec<Statement<'_>> {
    let mut statements = Vec::new();
    let mut offset = 0;
    let mut sql_start = 0;
    let mut request: Option<usize> = None;
    fn close_sql<'a>(statements: &mut Vec<Statement<'a>>, script: &'a str, from: usize, to: usize) {
        for statement in split_sql(&script[from..to], DbKind::Elasticsearch) {
            statements.push(Statement {
                sql: statement.sql,
                offset: from + statement.offset,
                line: line_of(script, from + statement.offset),
            });
        }
    }
    for line in script.split_inclusive('\n') {
        if is_request_line(line) {
            if let Some(begin) = request {
                push_trimmed(&mut statements, script, begin, offset);
            } else {
                close_sql(&mut statements, script, sql_start, offset);
            }
            request = Some(offset);
        } else if let Some(begin) = request {
            // A body is JSON lines; anything at the start of a line that is
            // not JSON ends the request and starts SQL again.
            let trimmed = line.trim_start();
            let body_so_far = &script[begin..offset];
            let open =
                body_so_far.matches(['{', '[']).count() > body_so_far.matches(['}', ']']).count();
            if !open && !trimmed.is_empty() && !trimmed.starts_with(['{', '[', '}', ']']) {
                push_trimmed(&mut statements, script, begin, offset);
                request = None;
                sql_start = offset;
            }
        }
        offset += line.len();
    }
    match request {
        Some(begin) => push_trimmed(&mut statements, script, begin, script.len()),
        None => close_sql(&mut statements, script, sql_start, script.len()),
    }
    statements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_should_keep_a_mysql_procedure_body_whole() {
        let script = "CREATE DEFINER=`root`@`%` PROCEDURE p(x int)\nBEGIN\n  IF x > 0 THEN\n    \
                      SELECT CASE WHEN x > 1 THEN 'a' ELSE 'b' END;\n  END IF;\n  \
                      WHILE x > 0 DO SET x = x - 1; END WHILE;\n  BEGIN SELECT 1; END;\nEND;\n\
                      SELECT 2";

        let statements = texts(script, DbKind::Mysql);

        assert_eq!(statements.len(), 2, "{statements:?}");
        assert!(statements[0].ends_with("END"));
        assert_eq!(statements[1], "SELECT 2");
    }

    #[test]
    fn split_should_end_sql_server_batches_at_go() {
        let script = "select 1\nselect 2\nGO\nselect [go]\n  go  \nselect 'go'";

        assert_eq!(
            texts(script, DbKind::SqlServer),
            ["select 1\nselect 2", "select [go]", "select 'go'"]
        );
    }

    #[test]
    fn split_should_read_mysql_backticks_and_hash_comments() {
        let script = "select `a;b` from t # c;d\n; select 'it''s;'";

        assert_eq!(
            texts(script, DbKind::Mysql),
            ["select `a;b` from t # c;d", "select 'it''s;'"]
        );
    }

    fn texts(script: &str, kind: DbKind) -> Vec<&str> {
        split(script, kind).into_iter().map(|s| s.sql).collect()
    }

    #[test]
    fn split_should_cut_at_semicolons() {
        assert_eq!(
            texts("select 1; select 2;", DbKind::Postgres),
            ["select 1", "select 2"]
        );
    }

    #[test]
    fn split_should_keep_a_final_statement_without_semicolon() {
        assert_eq!(
            texts("select 1;\nselect 2", DbKind::Postgres),
            ["select 1", "select 2"]
        );
    }

    #[test]
    fn split_should_ignore_semicolons_in_strings() {
        assert_eq!(
            texts("select 'a;b'; select 2", DbKind::Postgres),
            ["select 'a;b'", "select 2"]
        );
    }

    #[test]
    fn split_should_ignore_semicolons_in_comments() {
        assert_eq!(
            texts(
                "select 1 -- not; here\n; /* nor; here */ select 2",
                DbKind::Postgres
            ),
            ["select 1 -- not; here", "select 2"]
        );
    }

    #[test]
    fn split_should_ignore_semicolons_in_dollar_quotes() {
        let script = "do $body$ begin perform 1; end $body$; select 2";

        assert_eq!(
            texts(script, DbKind::Postgres),
            ["do $body$ begin perform 1; end $body$", "select 2"]
        );
    }

    #[test]
    fn split_should_drop_empty_statements() {
        assert_eq!(
            texts(";; -- nothing\n ; select 1;;", DbKind::Postgres),
            ["select 1"]
        );
    }

    #[test]
    fn split_should_report_byte_offset_after_multibyte_text() {
        let script = "select 'héé';\n  select 2";

        let second = split(script, DbKind::Postgres)[1];

        assert_eq!(&script[second.offset..], "select 2");
    }

    #[test]
    fn split_should_report_the_starting_line() {
        let second = split("select 1;\n\nselect 2", DbKind::Postgres)[1];

        assert_eq!(second.line, 3);
    }

    #[test]
    fn split_should_keep_the_tail_when_a_string_is_unterminated() {
        assert_eq!(
            texts("select 1; select 'oops", DbKind::Postgres),
            ["select 1", "select 'oops"]
        );
    }

    #[test]
    fn split_should_keep_a_sqlite_trigger_body_whole() {
        let trigger = "create trigger t after insert on a begin \
                       update b set n = case when 1 then 2 end; \
                       delete from c; end";
        let script = format!("{trigger}; select 1");

        assert_eq!(texts(&script, DbKind::Sqlite), [trigger, "select 1"]);
    }

    #[test]
    fn redis_should_run_one_command_per_line() {
        let script = "SET a 1\n\n# note\n  GET a\n";

        let statements = split(script, DbKind::Redis);

        assert_eq!(statements.len(), 2);
        assert_eq!(statements[1].sql, "GET a");
        assert_eq!(statements[1].line, 4);
        assert_eq!(&script[statements[1].offset..][..5], "GET a");
    }

    #[test]
    fn mongo_should_keep_multi_line_calls_and_chains_whole() {
        let script = "db.a.find({\n  x: 1\n})\n  .sort({x: -1})\n// next\ndb.b.countDocuments({}); db.c.drop()";

        assert_eq!(
            texts(script, DbKind::Mongodb),
            [
                "db.a.find({\n  x: 1\n})\n  .sort({x: -1})",
                "db.b.countDocuments({})",
                "db.c.drop()"
            ]
        );
    }

    #[test]
    fn mongo_should_ignore_brackets_inside_strings() {
        assert_eq!(
            texts("db.a.find({n: \"(\"})\ndb.b.find()", DbKind::Mongodb),
            ["db.a.find({n: \"(\"})", "db.b.find()"]
        );
    }

    #[test]
    fn elastic_should_split_requests_with_their_bodies_and_sql() {
        let script =
            "SELECT 1;\nGET /logs/_search\n{\n  \"size\": 1\n}\nPOST /a/_refresh\nSELECT 2";

        assert_eq!(
            texts(script, DbKind::Elasticsearch),
            [
                "SELECT 1",
                "GET /logs/_search\n{\n  \"size\": 1\n}",
                "POST /a/_refresh",
                "SELECT 2"
            ]
        );
    }
}
