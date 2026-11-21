//! Autocomplete for the SQL editor: keywords, schemas, tables, and the
//! columns of the tables the statement under the cursor names.
//!
//! Completion works in two steps so the caller can fetch what it lacks in
//! between. [`analyze`] reads the text around the cursor and says which
//! tables the statement mentions; the caller makes sure it knows their
//! columns; [`Analysis::candidates`] then ranks what fits.
//!
//! The text is read with a small lexer of its own rather than a SQL parser:
//! a statement being typed is almost never valid SQL.

use std::{collections::HashSet, ops::Range};

mod native;

use crate::{
    driver::{DbKind, Language},
    schema::{Catalog, ColumnInfo, RelationKind, TableRef},
};

/// What a candidate names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    /// A column of a table the statement mentions.
    Column,
    /// A table.
    Table,
    /// A view, materialised view or foreign table.
    View,
    /// A schema.
    Schema,
    /// A SQL keyword.
    Keyword,
}

/// One suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What the menu shows.
    pub label: String,
    /// What replaces the word being typed: the label, quoted when the
    /// database would otherwise misread it.
    pub insert: String,
    /// What the candidate names.
    pub kind: CandidateKind,
    /// A short note shown beside the label: a column's type, the table a
    /// column belongs to, a relation's schema.
    pub detail: Option<String>,
}

/// A table named in the statement, with the alias it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// The name as written, schema included when one was written.
    pub table: TableRef,
    /// The alias, if any.
    pub alias: Option<String>,
}

/// What [`analyze`] found at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// The bytes the chosen candidate replaces: the word being typed.
    pub replace: Range<usize>,
    /// The part of the word already typed.
    pub prefix: String,
    /// The names before a dot, as in `schema.` or `alias.`. Empty when the
    /// word is not qualified.
    pub qualifier: Vec<String>,
    /// Tables the statement under the cursor names.
    pub mentions: Vec<Mention>,
    /// Whether a table name is expected here: after `FROM`, `JOIN`,
    /// `UPDATE`, `INTO` or `TABLE`.
    pub wants_table: bool,
}

/// Most candidates returned at once.
pub const LIMIT: usize = 200;

/// Read the text around `cursor`, a byte offset into `sql`.
///
/// Returns `None` where completion makes no sense: inside a string or a
/// comment, or, unless `explicit` (the user asked for completions), where
/// nothing has been typed yet and no dot asks for a member.
pub fn analyze(sql: &str, cursor: usize, kind: DbKind, explicit: bool) -> Option<Analysis> {
    let cursor = floor_boundary(sql, cursor.min(sql.len()));
    match kind.language() {
        Language::Redis => return native::redis(sql, cursor, explicit),
        Language::Mongo => return native::mongo(sql, cursor, explicit),
        Language::Elastic if native::in_request(sql, cursor) => return None,
        _ => {}
    }
    let tokens = lex(sql, kind);

    if tokens.iter().any(|token| {
        matches!(token.kind, Tok::Str | Tok::Comment)
            && token.span.start < cursor
            && (cursor < token.span.end || (cursor == token.span.end && token.open))
    }) {
        return None;
    }

    // The statement the cursor is in.
    let start = tokens
        .iter()
        .rposition(|token| token.kind == Tok::Punct(';') && token.span.end <= cursor)
        .map_or(0, |ix| ix + 1);
    let end = tokens[start..]
        .iter()
        .position(|token| token.kind == Tok::Punct(';') && token.span.start >= cursor)
        .map_or(tokens.len(), |ix| start + ix);
    let statement = &tokens[start..end];

    // The word being typed, if the cursor is at the end of or inside one.
    let word = statement.iter().position(|token| {
        matches!(token.kind, Tok::Ident { .. })
            && token.span.start < cursor
            && cursor <= token.span.end
    });
    let (replace, prefix, before) = match word {
        Some(ix) => {
            let token = &statement[ix];
            let quoted = matches!(token.kind, Tok::Ident { quoted: true });
            let typed_from = if quoted {
                token.span.start + 1
            } else {
                token.span.start
            };
            let typed = sql[typed_from.min(cursor)..cursor].to_owned();
            (token.span.start..token.span.end.max(cursor), typed, ix)
        }
        None => (
            cursor..cursor,
            String::new(),
            statement
                .iter()
                .position(|token| token.span.start >= cursor)
                .unwrap_or(statement.len()),
        ),
    };
    let earlier = &statement[..before];

    // `a.b.` before the word.
    let mut qualifier = Vec::new();
    let mut ix = earlier.len();
    while ix >= 2
        && earlier[ix - 1].kind == Tok::Punct('.')
        && earlier[ix - 1].span.end == earlier.get(ix).map_or(replace.start, |t| t.span.start)
        && matches!(earlier[ix - 2].kind, Tok::Ident { .. })
    {
        qualifier.insert(0, earlier[ix - 2].text.clone());
        ix -= 2;
    }
    // A dot right before the cursor with nothing typed after it.
    if qualifier.is_empty()
        && prefix.is_empty()
        && let [.., name, dot] = earlier
        && dot.kind == Tok::Punct('.')
        && dot.span.end == cursor
        && matches!(name.kind, Tok::Ident { .. })
    {
        qualifier.push(name.text.clone());
        ix = earlier.len() - 2;
    }

    if prefix.is_empty() && qualifier.is_empty() && !explicit {
        return None;
    }

    let wants_table = qualifier.len() < 2 && expects_table(&earlier[..ix]);

    Some(Analysis {
        replace,
        prefix,
        qualifier,
        mentions: mentions(statement, word),
        wants_table,
    })
}

impl Analysis {
    /// The tables whose columns [`Analysis::candidates`] would offer,
    /// resolved against `catalog`. The caller should know their columns
    /// before asking for candidates.
    pub fn tables_needed(&self, catalog: &Catalog) -> Vec<TableRef> {
        match self.qualifier.as_slice() {
            [] if self.wants_table => Vec::new(),
            [] => self
                .mentions
                .iter()
                .filter_map(|mention| resolve(catalog, &mention.table))
                .collect(),
            [one] => self
                .mention_named(one)
                .and_then(|mention| resolve(catalog, &mention.table))
                .or_else(|| resolve(catalog, &TableRef::unqualified(one.clone())))
                .into_iter()
                .collect(),
            [.., schema, table] => resolve(catalog, &TableRef::new(schema.clone(), table.clone()))
                .into_iter()
                .collect(),
        }
    }

    /// The mention an alias or bare table name refers to.
    fn mention_named(&self, name: &str) -> Option<&Mention> {
        let by_alias = self.mentions.iter().find(|mention| {
            mention
                .alias
                .as_deref()
                .is_some_and(|alias| alias.eq_ignore_ascii_case(name))
        });
        by_alias.or_else(|| {
            self.mentions
                .iter()
                .find(|mention| mention.table.name.eq_ignore_ascii_case(name))
        })
    }

    /// What fits at the cursor, best first.
    ///
    /// `columns` returns a table's columns when they are known; tables it
    /// knows nothing about contribute none.
    pub fn candidates<'a>(
        &self,
        catalog: &Catalog,
        kind: DbKind,
        columns: impl Fn(&TableRef) -> Option<&'a [ColumnInfo]>,
    ) -> Vec<Candidate> {
        match kind.language() {
            Language::Redis => return native::redis_candidates(self, catalog),
            Language::Mongo => return native::mongo_candidates(self, catalog),
            _ => {}
        }
        let prefix = self.prefix.to_lowercase();
        let fits = |name: &str| name.to_lowercase().starts_with(&prefix);
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |out: &mut Vec<Candidate>, candidate: Candidate| {
            if seen.insert((
                candidate.kind,
                candidate.label.clone(),
                candidate.detail.clone(),
            )) {
                out.push(candidate);
            }
        };
        let column_candidates = |out: &mut Vec<Candidate>,
                                 push: &mut dyn FnMut(&mut Vec<Candidate>, Candidate),
                                 tables: &[TableRef],
                                 name_table: bool| {
            for table in tables {
                for column in columns(table).unwrap_or_default() {
                    if fits(&column.name) {
                        let detail = if name_table {
                            format!("{} · {}", column.type_name, table.name)
                        } else {
                            column.type_name.clone()
                        };
                        push(
                            out,
                            Candidate {
                                label: column.name.clone(),
                                insert: quote_if_needed(&column.name, kind),
                                kind: CandidateKind::Column,
                                detail: Some(detail),
                            },
                        );
                    }
                }
            }
        };

        match self.qualifier.as_slice() {
            [] => {
                let tables = self.tables_needed(catalog);
                let relations =
                    |out: &mut Vec<Candidate>,
                     push: &mut dyn FnMut(&mut Vec<Candidate>, Candidate)| {
                        for schema in &catalog.schemas {
                            for relation in &schema.relations {
                                if fits(&relation.name) {
                                    push(
                                        out,
                                        relation_candidate(
                                            &relation.name,
                                            relation.kind,
                                            &schema.name,
                                            kind,
                                        ),
                                    );
                                }
                            }
                        }
                        for schema in &catalog.schemas {
                            if fits(&schema.name) {
                                push(
                                    out,
                                    Candidate {
                                        label: schema.name.clone(),
                                        insert: quote_if_needed(&schema.name, kind),
                                        kind: CandidateKind::Schema,
                                        detail: Some("schema".into()),
                                    },
                                );
                            }
                        }
                    };
                if self.wants_table {
                    relations(&mut out, &mut push);
                } else {
                    column_candidates(&mut out, &mut push, &tables, tables.len() > 1);
                    // Aliases are as good as table names after SELECT.
                    for mention in &self.mentions {
                        if let Some(alias) = &mention.alias
                            && fits(alias)
                        {
                            push(
                                &mut out,
                                Candidate {
                                    label: alias.clone(),
                                    insert: alias.clone(),
                                    kind: CandidateKind::Table,
                                    detail: Some(format!("alias of {}", mention.table.name)),
                                },
                            );
                        }
                    }
                    relations(&mut out, &mut push);
                }
                let upper = !self.prefix.chars().any(char::is_lowercase);
                for keyword in KEYWORDS {
                    if fits(keyword) {
                        let spelled = if upper {
                            (*keyword).to_owned()
                        } else {
                            keyword.to_lowercase()
                        };
                        push(
                            &mut out,
                            Candidate {
                                label: spelled.clone(),
                                insert: spelled,
                                kind: CandidateKind::Keyword,
                                detail: None,
                            },
                        );
                    }
                }
            }
            [one] => {
                // `schema.` lists the schema's relations; `alias.` or
                // `table.` lists columns.
                let tables = self.tables_needed(catalog);
                column_candidates(&mut out, &mut push, &tables, false);
                if let Some(schema) = catalog
                    .schemas
                    .iter()
                    .find(|schema| schema.name == *one)
                    .or_else(|| {
                        catalog
                            .schemas
                            .iter()
                            .find(|schema| schema.name.eq_ignore_ascii_case(one))
                    })
                {
                    for relation in &schema.relations {
                        if fits(&relation.name) {
                            push(
                                &mut out,
                                relation_candidate(
                                    &relation.name,
                                    relation.kind,
                                    &schema.name,
                                    kind,
                                ),
                            );
                        }
                    }
                }
            }
            _ => {
                let tables = self.tables_needed(catalog);
                column_candidates(&mut out, &mut push, &tables, false);
            }
        }

        out.truncate(LIMIT);
        out
    }
}

fn relation_candidate(name: &str, relation: RelationKind, schema: &str, kind: DbKind) -> Candidate {
    Candidate {
        label: name.to_owned(),
        insert: quote_if_needed(name, kind),
        kind: if relation == RelationKind::Table {
            CandidateKind::Table
        } else {
            CandidateKind::View
        },
        detail: Some(schema.to_owned()),
    }
}

/// Find `table` in `catalog`. A written schema is matched exactly, then
/// ignoring case; an unwritten one is looked for in every schema, the
/// default ones (`public`, `main`, `dbo`) first.
pub fn resolve(catalog: &Catalog, table: &TableRef) -> Option<TableRef> {
    let matches = |schema: &crate::schema::Schema, exact: bool| -> Option<String> {
        schema
            .relations
            .iter()
            .find(|relation| {
                if exact {
                    relation.name == table.name
                } else {
                    relation.name.eq_ignore_ascii_case(&table.name)
                }
            })
            .map(|relation| relation.name.clone())
    };
    for exact in [true, false] {
        let mut schemas: Vec<&crate::schema::Schema> = match &table.schema {
            Some(name) => catalog
                .schemas
                .iter()
                .filter(|schema| {
                    if exact {
                        schema.name == *name
                    } else {
                        schema.name.eq_ignore_ascii_case(name)
                    }
                })
                .collect(),
            None => catalog.schemas.iter().collect(),
        };
        schemas.sort_by_key(|schema| !matches!(schema.name.as_str(), "public" | "main" | "dbo"));
        for schema in schemas {
            if let Some(relation) = matches(schema, exact) {
                return Some(TableRef::new(schema.name.clone(), relation));
            }
        }
    }
    None
}

/// Quote `name` when the database would not read it back as written.
pub fn quote_if_needed(name: &str, kind: DbKind) -> String {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic());
    let rest_ok = chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
    // Postgres folds unquoted names to lower case.
    let case_ok = kind != DbKind::Postgres || !name.chars().any(|c| c.is_ascii_uppercase());
    let reserved = KEYWORDS
        .iter()
        .any(|keyword| keyword.eq_ignore_ascii_case(name));
    if first_ok && rest_ok && case_ok && !reserved {
        return name.to_owned();
    }
    crate::driver::dialect(kind).quote_ident(name)
}

/// Whether the nearest clause keyword before the end of `tokens` takes a
/// table name.
fn expects_table(tokens: &[Token]) -> bool {
    let mut ix = tokens.len();
    // Skip back over `name AS alias,` pieces of a FROM list.
    while ix > 0 {
        ix -= 1;
        let token = &tokens[ix];
        match &token.kind {
            Tok::Ident { quoted: false } => {
                let word = token.text.to_ascii_uppercase();
                match word.as_str() {
                    "FROM" | "JOIN" | "UPDATE" | "INTO" | "TABLE" => {
                        return ix + 1 == tokens.len() || in_from_list(&tokens[ix + 1..]);
                    }
                    _ if is_keyword(&word) && word != "AS" => return false,
                    _ => {}
                }
            }
            Tok::Ident { quoted: true } | Tok::Punct('.' | ',') => {}
            _ => return false,
        }
    }
    false
}

/// Whether `tokens`, which follow a FROM, end just after a comma of its
/// table list, where another table name goes.
fn in_from_list(tokens: &[Token]) -> bool {
    tokens
        .last()
        .is_some_and(|token| token.kind == Tok::Punct(','))
}

/// The tables `statement` names after FROM, JOIN, UPDATE, INTO and TABLE.
/// `skip` is the token being typed, which is not a finished name.
fn mentions(statement: &[Token], skip: Option<usize>) -> Vec<Mention> {
    let is_name = |ix: usize| {
        Some(ix) != skip
            && statement.get(ix).is_some_and(|token| match token.kind {
                Tok::Ident { quoted: true } => true,
                Tok::Ident { quoted: false } => !is_keyword(&token.text.to_ascii_uppercase()),
                _ => false,
            })
    };
    let mut found = Vec::new();
    let mut ix = 0;
    while ix < statement.len() {
        let token = &statement[ix];
        let starts = matches!(token.kind, Tok::Ident { quoted: false })
            && matches!(
                token.text.to_ascii_uppercase().as_str(),
                "FROM" | "JOIN" | "UPDATE" | "INTO" | "TABLE"
            );
        ix += 1;
        if !starts {
            continue;
        }
        // One or more `name [.name] [[AS] alias]`, separated by commas.
        loop {
            if !is_name(ix) {
                break;
            }
            let mut parts = vec![statement[ix].text.clone()];
            ix += 1;
            while statement.get(ix).is_some_and(|t| t.kind == Tok::Punct('.')) && is_name(ix + 1) {
                parts.push(statement[ix + 1].text.clone());
                ix += 2;
            }
            let has_as = statement.get(ix).is_some_and(|t| {
                matches!(t.kind, Tok::Ident { quoted: false }) && t.text.eq_ignore_ascii_case("AS")
            });
            if has_as {
                ix += 1;
            }
            let alias = is_name(ix).then(|| {
                ix += 1;
                statement[ix - 1].text.clone()
            });
            let name = parts.pop().unwrap_or_default();
            let table = match parts.pop() {
                Some(schema) => TableRef::new(schema, name),
                None => TableRef::unqualified(name),
            };
            found.push(Mention { table, alias });
            if statement.get(ix).is_some_and(|t| t.kind == Tok::Punct(',')) {
                ix += 1;
            } else {
                break;
            }
        }
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Ident {
        quoted: bool,
    },
    /// The opening character of a quoted name with nothing after it yet.
    QuotedOpen,
    Str,
    Comment,
    Number,
    Punct(char),
}

#[derive(Debug, Clone)]
struct Token {
    kind: Tok,
    span: Range<usize>,
    /// For identifiers, the name with quotes removed.
    text: String,
    /// Whether a string, comment or quoted name runs to the end of the text
    /// without being closed.
    open: bool,
}

fn floor_boundary(text: &str, mut offset: usize) -> usize {
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn lex(sql: &str, kind: DbKind) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut ix = 0;
    let ident_char = |c: char| c == '_' || c == '$' || c.is_alphanumeric();
    while ix < sql.len() {
        let rest = &sql[ix..];
        let c = rest.chars().next().unwrap_or(' ');
        let start = ix;
        let token = |kind, end: usize, text: String, open| Token {
            kind,
            span: start..end,
            text,
            open,
        };
        if c.is_whitespace() {
            ix += c.len_utf8();
            continue;
        }
        if rest.starts_with("--") || (backtick_quotes(kind) && rest.starts_with('#')) {
            let end = rest.find('\n').map_or(sql.len(), |n| ix + n);
            tokens.push(token(Tok::Comment, end, String::new(), end == sql.len()));
            ix = end;
            continue;
        }
        if let Some(body) = rest.strip_prefix("/*") {
            let (end, open) = body
                .find("*/")
                .map_or((sql.len(), true), |n| (ix + 2 + n + 2, false));
            tokens.push(token(Tok::Comment, end, String::new(), open));
            ix = end;
            continue;
        }
        if c == '\'' {
            let (end, open) = closing(sql, ix + 1, '\'');
            tokens.push(token(Tok::Str, end, String::new(), open));
            ix = end;
            continue;
        }
        if kind == DbKind::Postgres
            && c == '$'
            && let Some(tag_len) = rest[1..].find('$').filter(|&n| {
                rest[1..1 + n]
                    .chars()
                    .all(|c| c == '_' || c.is_alphabetic())
                    && !rest[1..].starts_with(|c: char| c.is_ascii_digit())
            })
        {
            let tag = &rest[..tag_len + 2];
            let body = ix + tag.len();
            let (end, open) = sql[body..]
                .find(tag)
                .map_or((sql.len(), true), |n| (body + n + tag.len(), false));
            tokens.push(token(Tok::Str, end, String::new(), open));
            ix = end;
            continue;
        }
        let close = match c {
            '"' => Some('"'),
            '`' if backtick_quotes(kind) => Some('`'),
            '[' if kind == DbKind::SqlServer => Some(']'),
            _ => None,
        };
        if let Some(close) = close {
            let (end, open) = closing(sql, ix + 1, close);
            let inner_end = if open { end } else { end - close.len_utf8() };
            let text =
                sql[ix + 1..inner_end].replace(&format!("{close}{close}"), &close.to_string());
            let kind = if open && end == ix + 1 {
                Tok::QuotedOpen
            } else {
                Tok::Ident { quoted: true }
            };
            tokens.push(token(kind, end, text, open));
            ix = end;
            continue;
        }
        if c.is_ascii_digit() {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                .map_or(sql.len(), |n| ix + n);
            tokens.push(token(Tok::Number, end, String::new(), false));
            ix = end;
            continue;
        }
        if ident_char(c) && c != '$' {
            let end = rest
                .find(|c: char| !ident_char(c))
                .map_or(sql.len(), |n| ix + n);
            tokens.push(token(
                Tok::Ident { quoted: false },
                end,
                sql[ix..end].to_owned(),
                false,
            ));
            ix = end;
            continue;
        }
        tokens.push(token(
            Tok::Punct(c),
            ix + c.len_utf8(),
            String::new(),
            false,
        ));
        ix += c.len_utf8();
    }
    tokens
}

/// Where a quoted run that opened just before `from` ends, past its closing
/// character. A doubled closing character is an escaped one. Returns the end
/// of the text and `true` when it never closes.
fn closing(sql: &str, from: usize, close: char) -> (usize, bool) {
    let mut chars = sql[from..].char_indices().peekable();
    while let Some((n, c)) = chars.next() {
        if c == close {
            if chars.peek().is_some_and(|&(_, next)| next == close) {
                chars.next();
                continue;
            }
            return (from + n + c.len_utf8(), false);
        }
    }
    (sql.len(), true)
}

/// Whether the database quotes names with backticks (and so also takes `#`
/// comments).
fn backtick_quotes(kind: DbKind) -> bool {
    crate::driver::dialect(kind)
        .quote_ident("x")
        .starts_with('`')
}

fn is_keyword(upper: &str) -> bool {
    KEYWORDS.binary_search(&upper).is_ok()
}

/// Keywords offered and treated as reserved, sorted for binary search.
const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "REFERENCES",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TO",
    "TOP",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Relation, Schema};

    fn catalog() -> Catalog {
        let relation = |name: &str, kind| Relation {
            name: name.into(),
            kind,
        };
        Catalog {
            schemas: vec![
                Schema {
                    name: "public".into(),
                    relations: vec![
                        relation("orders", RelationKind::Table),
                        relation("users", RelationKind::Table),
                        relation("user_stats", RelationKind::View),
                    ],
                },
                Schema {
                    name: "audit".into(),
                    relations: vec![relation("events", RelationKind::Table)],
                },
            ],
        }
    }

    fn column(name: &str, type_name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
            default: None,
        }
    }

    fn columns_of(table: &TableRef) -> Option<&'static [ColumnInfo]> {
        use std::sync::OnceLock;
        static USERS: OnceLock<Vec<ColumnInfo>> = OnceLock::new();
        static ORDERS: OnceLock<Vec<ColumnInfo>> = OnceLock::new();
        static EVENTS: OnceLock<Vec<ColumnInfo>> = OnceLock::new();
        match (table.schema.as_deref(), table.name.as_str()) {
            (Some("public"), "users") => Some(USERS.get_or_init(|| {
                vec![
                    column("id", "int4"),
                    column("name", "text"),
                    column("Email", "text"),
                ]
            })),
            (Some("public"), "orders") => Some(ORDERS.get_or_init(|| {
                vec![
                    column("id", "int4"),
                    column("user_id", "int4"),
                    column("total", "numeric"),
                ]
            })),
            (Some("audit"), "events") => Some(EVENTS.get_or_init(|| vec![column("kind", "text")])),
            _ => None,
        }
    }

    /// Complete at the `|` in `text`.
    fn complete(text: &str, explicit: bool) -> Option<(Analysis, Vec<Candidate>)> {
        let cursor = text.find('|').expect("a cursor mark");
        let sql = text.replace('|', "");
        let analysis = analyze(&sql, cursor, DbKind::Postgres, explicit)?;
        let candidates = analysis.candidates(&catalog(), DbKind::Postgres, columns_of);
        Some((analysis, candidates))
    }

    fn labels(text: &str) -> Vec<String> {
        complete(text, false)
            .map(|(_, candidates)| candidates.into_iter().map(|c| c.label).collect())
            .unwrap_or_default()
    }

    #[test]
    fn should_offer_tables_after_from() {
        assert_eq!(
            labels("select * from us|"),
            ["users", "user_stats", "using"]
        );
    }

    #[test]
    fn should_offer_tables_after_a_comma_in_the_from_list() {
        assert_eq!(
            labels("select * from users u, or|"),
            ["orders", "or", "order"]
        );
    }

    #[test]
    fn should_offer_columns_of_the_named_tables_before_anything_else() {
        let labels = labels("select na| from users");

        assert_eq!(labels.first().map(String::as_str), Some("name"));
    }

    #[test]
    fn should_offer_columns_through_an_alias() {
        assert_eq!(
            labels("select o.| from orders o join users u on u.id = o.user_id"),
            ["id", "user_id", "total"]
        );
    }

    #[test]
    fn should_offer_columns_through_a_table_name() {
        assert_eq!(labels("select users.na| from users"), ["name"]);
    }

    #[test]
    fn should_offer_relations_of_a_schema() {
        assert_eq!(labels("select * from audit.|"), ["events"]);
    }

    #[test]
    fn should_offer_columns_of_a_schema_qualified_table() {
        assert_eq!(labels("select audit.events.| from audit.events"), ["kind"]);
    }

    #[test]
    fn should_match_the_typed_case_for_keywords() {
        assert!(labels("sel|").contains(&"select".to_owned()));
        assert!(labels("SEL|").contains(&"SELECT".to_owned()));
    }

    #[test]
    fn should_quote_names_postgres_would_fold() {
        let (_, candidates) = complete("select em| from users", false).unwrap();

        let email = candidates.iter().find(|c| c.label == "Email").unwrap();
        assert_eq!(email.insert, "\"Email\"");
    }

    #[test]
    fn should_replace_the_whole_word_under_the_cursor() {
        let (analysis, _) = complete("select * from us|ers", false).unwrap();

        assert_eq!(analysis.replace, 14..19);
        assert_eq!(analysis.prefix, "us");
    }

    #[test]
    fn should_stay_quiet_inside_strings_and_comments() {
        assert!(complete("select 'us|", false).is_none());
        assert!(complete("select 'a' -- fr|", false).is_none());
        assert!(complete("select /* fr| */ 1", false).is_none());
        assert!(complete("select $$ fr| $$", false).is_none());
    }

    #[test]
    fn should_stay_quiet_on_nothing_typed_unless_asked() {
        assert!(complete("select * from |", false).is_none());
        let (_, candidates) = complete("select * from |", true).unwrap();
        assert!(candidates.iter().any(|c| c.label == "users"));
    }

    #[test]
    fn should_read_only_the_statement_under_the_cursor() {
        let (analysis, _) = complete("select * from orders; select na| from users", false).unwrap();

        assert_eq!(
            analysis.mentions,
            [Mention {
                table: TableRef::unqualified("users"),
                alias: None
            }]
        );
    }

    #[test]
    fn should_find_mentions_with_schemas_and_aliases() {
        let (analysis, _) =
            complete("select | from audit.events as e join users u on true", true).unwrap();

        assert_eq!(
            analysis.mentions,
            [
                Mention {
                    table: TableRef::new("audit", "events"),
                    alias: Some("e".into())
                },
                Mention {
                    table: TableRef::unqualified("users"),
                    alias: Some("u".into())
                },
            ]
        );
    }

    #[test]
    fn should_name_the_table_beside_columns_when_several_are_mentioned() {
        let (_, candidates) = complete("select tot| from users, orders", false).unwrap();

        assert_eq!(candidates[0].detail.as_deref(), Some("numeric · orders"));
    }

    #[test]
    fn tables_needed_should_resolve_unqualified_names_to_the_default_schema() {
        let (analysis, _) = complete("select | from users", true).unwrap();

        assert_eq!(
            analysis.tables_needed(&catalog()),
            [TableRef::new("public", "users")]
        );
    }

    #[test]
    fn should_quote_for_each_database() {
        assert_eq!(quote_if_needed("order", DbKind::Sqlite), "\"order\"");
        assert_eq!(quote_if_needed("Name", DbKind::Sqlite), "Name");
        assert_eq!(quote_if_needed("Name", DbKind::Postgres), "\"Name\"");
    }

    #[test]
    fn keywords_should_be_sorted() {
        assert!(KEYWORDS.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
