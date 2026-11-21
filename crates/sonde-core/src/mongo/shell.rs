//! The subset of `mongosh` the query tab takes: `db.<collection>.<method>(...)`
//! calls with chained cursor methods, `db.runCommand(...)`, `show dbs`,
//! `use <db>`, and the shell's literals (unquoted keys, single quotes,
//! `ObjectId(...)`, `ISODate(...)`, `NumberLong(...)`, regexes and the like).

use std::str::FromStr as _;

use bson::{Bson, Document, oid::ObjectId};

use crate::error::{Error, Result};

/// Which database a call addresses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Db {
    /// The session's current database.
    Current,
    /// `db.getSiblingDB("name")`.
    Named(String),
}

/// One parsed statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Call {
    /// `show dbs`.
    ShowDbs,
    /// `show collections`, `db.getCollectionNames()`.
    ShowCollections(Db),
    /// `use name`.
    Use(String),
    /// A method of the database, such as `runCommand`.
    Database {
        db: Db,
        method: String,
        args: Vec<Bson>,
    },
    /// A method of a collection, with any cursor methods chained after it.
    Collection {
        db: Db,
        collection: String,
        method: String,
        args: Vec<Bson>,
        chain: Vec<(String, Vec<Bson>)>,
    },
}

/// Parse one statement.
pub(crate) fn parse(text: &str) -> Result<Call> {
    let mut p = Parser::new(text);
    p.space();
    let call = if p.word_is("show") {
        p.ident()?;
        let what = p.ident()?;
        match what.as_str() {
            "dbs" | "databases" => Call::ShowDbs,
            "collections" | "tables" => Call::ShowCollections(Db::Current),
            other => return Err(p.error(format!("`show {other}` is not supported"))),
        }
    } else if p.word_is("use") {
        p.ident()?;
        let name = p.ident()?;
        Call::Use(name)
    } else {
        p.call()?
    };
    p.space();
    p.eat(';');
    p.space();
    if !p.done() {
        return Err(p.error("unexpected text after the statement".to_owned()));
    }
    Ok(call)
}

/// Parse a shell literal on its own.
#[cfg(test)]
pub(crate) fn literal(text: &str) -> Result<Bson> {
    let mut p = Parser::new(text);
    let value = p.value()?;
    p.space();
    if !p.done() {
        return Err(p.error("unexpected text after the value".to_owned()));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    fn done(&self) -> bool {
        self.at >= self.text.len()
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn error(&self, message: String) -> Error {
        Error::Query {
            message,
            position: Some(self.at.min(self.text.len())),
            code: None,
            detail: None,
            hint: None,
        }
    }

    /// Skip whitespace and comments.
    fn space(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.at += rest.len() - trimmed.len();
            if let Some(after) = trimmed.strip_prefix("//") {
                self.at += 2 + after.find('\n').unwrap_or(after.len());
            } else if let Some(after) = trimmed.strip_prefix("/*") {
                self.at += 2 + after.find("*/").map_or(after.len(), |n| n + 2);
            } else {
                return;
            }
        }
    }

    fn eat(&mut self, c: char) -> bool {
        self.space();
        if self.peek() == Some(c) {
            self.at += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: char) -> Result<()> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.error(format!("expected `{c}`")))
        }
    }

    fn word_is(&mut self, word: &str) -> bool {
        self.space();
        let rest = self.rest();
        rest.starts_with(word)
            && !rest[word.len()..]
                .starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$')
    }

    fn ident(&mut self) -> Result<String> {
        self.space();
        let rest = self.rest();
        let len = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '-'))
            .unwrap_or(rest.len());
        if len == 0 || rest.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(self.error("expected a name".to_owned()));
        }
        self.at += len;
        Ok(rest[..len].to_owned())
    }

    fn string(&mut self) -> Result<String> {
        self.space();
        let quote = match self.peek() {
            Some(q @ ('"' | '\'' | '`')) => q,
            _ => return Err(self.error("expected a string".to_owned())),
        };
        let start = self.at;
        self.at += 1;
        let mut out = String::new();
        let mut chars = self.rest().char_indices();
        while let Some((ix, c)) = chars.next() {
            match c {
                c if c == quote => {
                    self.at += ix + 1;
                    return Ok(out);
                }
                '\\' => match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, 'r')) => out.push('\r'),
                    Some((_, 'b')) => out.push('\u{8}'),
                    Some((_, 'f')) => out.push('\u{c}'),
                    Some((_, '0')) => out.push('\0'),
                    Some((_, 'u')) => {
                        let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                        let code = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                        match code {
                            Some(c) => out.push(c),
                            None => {
                                self.at = start;
                                return Err(self.error(format!("\\u{hex} is not a character")));
                            }
                        }
                    }
                    Some((_, other)) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        self.at = start;
        Err(self.error("this string is not closed".to_owned()))
    }

    /// A collection or method name after a `.`, or a `["name"]` index.
    fn member(&mut self) -> Result<String> {
        if self.eat('[') {
            let name = self.string()?;
            self.expect(']')?;
            Ok(name)
        } else {
            self.expect('.')?;
            self.ident()
        }
    }

    fn args(&mut self) -> Result<Vec<Bson>> {
        self.expect('(')?;
        let mut args = Vec::new();
        if self.eat(')') {
            return Ok(args);
        }
        loop {
            args.push(self.value()?);
            if self.eat(')') {
                return Ok(args);
            }
            self.expect(',')?;
            if self.eat(')') {
                return Ok(args);
            }
        }
    }

    fn call(&mut self) -> Result<Call> {
        if !self.word_is("db") {
            return Err(self.error(
                "expected db.<collection>.<method>(...), show dbs, or use <database>".to_owned(),
            ));
        }
        self.ident()?;
        let mut db = Db::Current;
        let mut name = self.member()?;
        if name == "getSiblingDB" || name == "getSisterDB" {
            let args = self.args()?;
            match args.first() {
                Some(Bson::String(named)) => db = Db::Named(named.clone()),
                _ => return Err(self.error("getSiblingDB takes a database name".to_owned())),
            }
            name = self.member()?;
        }
        let collection = if name == "getCollection" {
            let args = self.args()?;
            match args.into_iter().next() {
                Some(Bson::String(named)) => named,
                _ => return Err(self.error("getCollection takes a collection name".to_owned())),
            }
        } else {
            self.space();
            if self.peek() == Some('(') {
                let args = self.args()?;
                if name == "getCollectionNames" {
                    return Ok(Call::ShowCollections(db));
                }
                return Ok(Call::Database {
                    db,
                    method: name,
                    args,
                });
            }
            name
        };
        let method = self.member()?;
        let args = self.args()?;
        let mut chain = Vec::new();
        loop {
            self.space();
            if self.peek() != Some('.') {
                break;
            }
            let name = self.member()?;
            let args = self.args()?;
            chain.push((name, args));
        }
        Ok(Call::Collection {
            db,
            collection,
            method,
            args,
            chain,
        })
    }

    fn value(&mut self) -> Result<Bson> {
        self.space();
        match self.peek() {
            None => Err(self.error("expected a value".to_owned())),
            Some('{') => self.document().map(wrapped),
            Some('[') => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    if self.eat(']') {
                        return Ok(Bson::Array(items));
                    }
                    items.push(self.value()?);
                    if !self.eat(',') {
                        self.expect(']')?;
                        return Ok(Bson::Array(items));
                    }
                }
            }
            Some('"' | '\'' | '`') => self.string().map(Bson::String),
            Some('/') => self.regex(),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => self.number(),
            Some(_) => self.word_value(),
        }
    }

    fn document(&mut self) -> Result<Document> {
        self.expect('{')?;
        let mut doc = Document::new();
        loop {
            if self.eat('}') {
                break;
            }
            self.space();
            let key = match self.peek() {
                Some('"' | '\'' | '`') => self.string()?,
                Some(c) if c.is_ascii_digit() => {
                    let rest = self.rest();
                    let len = rest
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(rest.len());
                    self.at += len;
                    rest[..len].to_owned()
                }
                _ => {
                    // Dotted paths such as `address.city` are keys too.
                    let mut key = self.ident()?;
                    while self.rest().starts_with('.') {
                        self.at += 1;
                        key.push('.');
                        key.push_str(&self.ident()?);
                    }
                    key
                }
            };
            self.expect(':')?;
            let value = self.value()?;
            doc.insert(key, value);
            if !self.eat(',') {
                self.expect('}')?;
                break;
            }
        }
        Ok(doc)
    }

    fn number(&mut self) -> Result<Bson> {
        let rest = self.rest();
        let len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.')))
            .unwrap_or(rest.len());
        let text = &rest[..len];
        let parsed = if let Ok(n) = text.parse::<i32>() {
            Some(Bson::Int32(n))
        } else if let Ok(n) = text.parse::<i64>() {
            Some(Bson::Int64(n))
        } else {
            text.parse::<f64>().ok().map(Bson::Double)
        };
        match parsed {
            Some(value) => {
                self.at += len;
                Ok(value)
            }
            None => Err(self.error(format!("`{text}` is not a number"))),
        }
    }

    fn regex(&mut self) -> Result<Bson> {
        let start = self.at;
        self.at += 1;
        let mut pattern = String::new();
        let mut chars = self.rest().char_indices();
        let mut end = None;
        while let Some((ix, c)) = chars.next() {
            match c {
                '\\' => {
                    pattern.push('\\');
                    if let Some((_, next)) = chars.next() {
                        pattern.push(next);
                    }
                }
                '/' => {
                    end = Some(ix);
                    break;
                }
                c => pattern.push(c),
            }
        }
        let Some(end) = end else {
            self.at = start;
            return Err(self.error("this regular expression is not closed".to_owned()));
        };
        self.at += end + 1;
        let rest = self.rest();
        let flags_len = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let mut flags: Vec<char> = rest[..flags_len].chars().collect();
        flags.sort_unstable();
        self.at += flags_len;
        Ok(Bson::RegularExpression(bson::Regex {
            pattern,
            options: flags.into_iter().collect(),
        }))
    }

    /// `true`, `null`, `ObjectId("...")`, `new Date(...)` and friends.
    fn word_value(&mut self) -> Result<Bson> {
        let start = self.at;
        let mut word = self.ident()?;
        if word == "new" {
            word = self.ident()?;
        }
        match word.as_str() {
            "true" => return Ok(Bson::Boolean(true)),
            "false" => return Ok(Bson::Boolean(false)),
            "null" | "undefined" => return Ok(Bson::Null),
            "NaN" => return Ok(Bson::Double(f64::NAN)),
            "Infinity" => return Ok(Bson::Double(f64::INFINITY)),
            "MinKey" | "MaxKey" => {
                self.space();
                if self.peek() == Some('(') {
                    self.args()?;
                }
                return Ok(if word == "MinKey" {
                    Bson::MinKey
                } else {
                    Bson::MaxKey
                });
            }
            _ => {}
        }
        self.space();
        if self.peek() != Some('(') {
            self.at = start;
            return Err(self.error(format!("`{word}` is not a value")));
        }
        let args = self.args()?;
        let bad = |p: &Self, what: &str| p.error(format!("{word}(...) takes {what}"));
        let text_arg = |args: &[Bson]| match args.first() {
            Some(Bson::String(s)) => Some(s.clone()),
            Some(Bson::Int32(n)) => Some(n.to_string()),
            Some(Bson::Int64(n)) => Some(n.to_string()),
            Some(Bson::Double(n)) => Some(n.to_string()),
            _ => None,
        };
        match word.as_str() {
            "ObjectId" => match text_arg(&args) {
                None if args.is_empty() => Ok(Bson::ObjectId(ObjectId::new())),
                Some(hex) => ObjectId::parse_str(&hex)
                    .map(Bson::ObjectId)
                    .map_err(|_| bad(self, "24 hex digits")),
                None => Err(bad(self, "24 hex digits")),
            },
            "ISODate" | "Date" => match args.first() {
                None => Ok(Bson::DateTime(bson::DateTime::now())),
                Some(Bson::String(text)) => date(text)
                    .map(Bson::DateTime)
                    .ok_or_else(|| bad(self, "an ISO 8601 date")),
                Some(Bson::Int32(ms)) => {
                    Ok(Bson::DateTime(bson::DateTime::from_millis((*ms).into())))
                }
                Some(Bson::Int64(ms)) => Ok(Bson::DateTime(bson::DateTime::from_millis(*ms))),
                Some(Bson::Double(ms)) => {
                    Ok(Bson::DateTime(bson::DateTime::from_millis(*ms as i64)))
                }
                _ => Err(bad(self, "an ISO 8601 date or milliseconds")),
            },
            "NumberInt" | "Int32" => text_arg(&args)
                .and_then(|t| t.parse::<f64>().ok())
                .map(|n| Bson::Int32(n as i32))
                .ok_or_else(|| bad(self, "a number")),
            "NumberLong" | "Long" => text_arg(&args)
                .and_then(|t| t.parse::<i64>().ok())
                .map(Bson::Int64)
                .ok_or_else(|| bad(self, "a whole number")),
            "Double" => text_arg(&args)
                .and_then(|t| t.parse::<f64>().ok())
                .map(Bson::Double)
                .ok_or_else(|| bad(self, "a number")),
            "NumberDecimal" | "Decimal128" => text_arg(&args)
                .and_then(|t| bson::Decimal128::from_str(&t).ok())
                .map(Bson::Decimal128)
                .ok_or_else(|| bad(self, "a decimal number")),
            "UUID" => text_arg(&args)
                .and_then(|t| bson::Uuid::parse_str(t).ok())
                .map(|uuid| Bson::Binary(bson::Binary::from_uuid(uuid)))
                .ok_or_else(|| bad(self, "a UUID")),
            "BinData" => match (args.first(), args.get(1)) {
                (Some(Bson::Int32(subtype)), Some(Bson::String(data))) => {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|_| bad(self, "a subtype and base64"))?;
                    Ok(Bson::Binary(bson::Binary {
                        subtype: (*subtype as u8).into(),
                        bytes,
                    }))
                }
                _ => Err(bad(self, "a subtype and base64")),
            },
            "Timestamp" => match (args.first(), args.get(1)) {
                (Some(Bson::Int32(time)), Some(Bson::Int32(increment))) => {
                    Ok(Bson::Timestamp(bson::Timestamp {
                        time: *time as u32,
                        increment: *increment as u32,
                    }))
                }
                _ => Err(bad(self, "seconds and an increment")),
            },
            "RegExp" => match (args.first(), args.get(1)) {
                (Some(Bson::String(pattern)), flags) => Ok(Bson::RegularExpression(bson::Regex {
                    pattern: pattern.clone(),
                    options: match flags {
                        Some(Bson::String(flags)) => flags.clone(),
                        _ => String::new(),
                    },
                })),
                _ => Err(bad(self, "a pattern")),
            },
            other => {
                self.at = start;
                Err(self.error(format!("`{other}(...)` is not supported")))
            }
        }
    }
}

fn date(text: &str) -> Option<bson::DateTime> {
    if let Ok(date) = bson::DateTime::parse_rfc3339_str(text) {
        return Some(date);
    }
    // The shell also takes a bare date, or a time without a zone, as UTC.
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f"))
        .ok()
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })?;
    Some(bson::DateTime::from_millis(
        naive.and_utc().timestamp_millis(),
    ))
}

/// A document, or the value it stands for when it is an extended JSON
/// wrapper such as `{"$oid": "..."}`: the grid shows nested values that
/// way, so they have to read back as the same values.
fn wrapped(doc: Document) -> Bson {
    let is_wrapper = doc.len() <= 2
        && doc.keys().next().is_some_and(|key| {
            matches!(
                key.as_str(),
                "$oid"
                    | "$date"
                    | "$numberLong"
                    | "$numberInt"
                    | "$numberDouble"
                    | "$numberDecimal"
                    | "$binary"
                    | "$uuid"
                    | "$regularExpression"
                    | "$timestamp"
                    | "$symbol"
                    | "$minKey"
                    | "$maxKey"
            )
        });
    if is_wrapper {
        let json = Bson::Document(doc.clone()).into_relaxed_extjson();
        if let Ok(value) = Bson::try_from(json) {
            return value;
        }
    }
    Bson::Document(doc)
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    #[test]
    fn parse_should_read_a_find_with_a_chain() {
        let call =
            parse("db.orders.find({status: 'open', total: {$gt: 10}}).sort({total: -1}).limit(5)")
                .unwrap();

        assert_eq!(
            call,
            Call::Collection {
                db: Db::Current,
                collection: "orders".into(),
                method: "find".into(),
                args: vec![Bson::Document(
                    doc! {"status": "open", "total": {"$gt": 10}}
                )],
                chain: vec![
                    ("sort".into(), vec![Bson::Document(doc! {"total": -1})]),
                    ("limit".into(), vec![Bson::Int32(5)]),
                ],
            }
        );
    }

    #[test]
    fn parse_should_take_named_databases_and_collections() {
        let call =
            parse(r#"db.getSiblingDB("shop").getCollection("order items").countDocuments({})"#)
                .unwrap();

        let Call::Collection { db, collection, .. } = call else {
            panic!("a collection call");
        };
        assert_eq!(db, Db::Named("shop".into()));
        assert_eq!(collection, "order items");
        assert!(matches!(
            parse("db['weird-name'].find()").unwrap(),
            Call::Collection { collection, .. } if collection == "weird-name"
        ));
    }

    #[test]
    fn parse_should_read_shell_commands() {
        assert_eq!(parse("show dbs").unwrap(), Call::ShowDbs);
        assert_eq!(parse("use shop;").unwrap(), Call::Use("shop".into()));
        assert_eq!(
            parse("db.getCollectionNames()").unwrap(),
            Call::ShowCollections(Db::Current)
        );
        assert!(matches!(
            parse("db.runCommand({ping: 1})").unwrap(),
            Call::Database { method, .. } if method == "runCommand"
        ));
    }

    #[test]
    fn literal_should_read_the_shell_constructors() {
        let value = literal(
            r#"{_id: ObjectId("65a1b2c3d4e5f60718293a4b"), at: ISODate("2024-01-02T03:04:05Z"),
                n: NumberLong(7), d: NumberDecimal("1.50"), r: /ab+c/i, big: 3000000000,
                f: 1.5, list: [1, "two",], nested: {"a.b": null}}"#,
        )
        .unwrap();

        let Bson::Document(doc) = value else {
            panic!("a document");
        };
        assert!(matches!(doc.get("_id"), Some(Bson::ObjectId(_))));
        assert!(matches!(doc.get("at"), Some(Bson::DateTime(_))));
        assert_eq!(doc.get("n"), Some(&Bson::Int64(7)));
        assert_eq!(doc.get("d").unwrap().to_string(), "1.50");
        assert!(matches!(doc.get("r"), Some(Bson::RegularExpression(r)) if r.options == "i"));
        assert_eq!(doc.get("big"), Some(&Bson::Int64(3_000_000_000)));
        assert_eq!(doc.get("f"), Some(&Bson::Double(1.5)));
        assert_eq!(doc.get_array("list").unwrap().len(), 2);
    }

    #[test]
    fn extended_json_should_read_back_as_values() {
        let value =
            literal(r#"{"a": {"$oid": "65a1b2c3d4e5f60718293a4b"}, "n": {"$numberLong": "5"}}"#)
                .unwrap();

        let Bson::Document(doc) = value else {
            panic!("a document");
        };
        assert!(matches!(doc.get("a"), Some(Bson::ObjectId(_))));
        assert_eq!(doc.get("n"), Some(&Bson::Int64(5)));
    }

    #[test]
    fn errors_should_point_at_the_problem() {
        let error = parse("db.orders.find({status: })").unwrap_err();

        assert_eq!(error.position(), Some(24));
        assert!(parse("SELECT 1").is_err());
    }
}
