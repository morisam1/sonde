//! Redis command lines: reading them the way `redis-cli` does, writing
//! them back, and turning replies into rows.

use std::sync::Arc;

use redis::Value as Reply;

use crate::{
    error::{Error, Result},
    stream::meta,
    value::{ColumnMeta, Value},
};

/// Split a command line into its arguments. Double quotes take the
/// `redis-cli` escapes (`\n`, `\t`, `\"`, `\\`, `\xHH`, ...); single quotes
/// take only `\'`.
pub(crate) fn parse(line: &str) -> Result<Vec<Vec<u8>>> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            break;
        };
        let mut arg = Vec::new();
        match first {
            '"' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err(Error::query("a double quote is not closed")),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => arg.push(b'\n'),
                            Some('r') => arg.push(b'\r'),
                            Some('t') => arg.push(b'\t'),
                            Some('b') => arg.push(8),
                            Some('a') => arg.push(7),
                            Some('x') => {
                                let hex: String = chars.by_ref().take(2).collect();
                                match u8::from_str_radix(&hex, 16) {
                                    Ok(byte) if hex.len() == 2 => arg.push(byte),
                                    _ => {
                                        return Err(Error::query(format!(
                                            "\\x{hex} is not a hex byte"
                                        )));
                                    }
                                }
                            }
                            Some(other) => push_char(&mut arg, other),
                            None => return Err(Error::query("a double quote is not closed")),
                        },
                        Some(other) => push_char(&mut arg, other),
                    }
                }
            }
            '\'' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err(Error::query("a single quote is not closed")),
                        Some('\'') => break,
                        Some('\\') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            arg.push(b'\'');
                        }
                        Some(other) => push_char(&mut arg, other),
                    }
                }
            }
            _ => {
                while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                    push_char(&mut arg, c);
                }
            }
        }
        args.push(arg);
    }
    Ok(args)
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// An argument written so [`parse`] reads it back unchanged.
pub(crate) fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && c != '"' && c != '\'' && c != '\\');
    if plain {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Bytes as text when they are UTF-8, otherwise as bytes.
pub(crate) fn bytes_value(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => Value::text(text),
        Err(_) => Value::Bytes(Arc::from(bytes)),
    }
}

/// A reply as one cell. Nested replies become JSON.
pub(crate) fn cell(reply: &Reply) -> Value {
    match reply {
        Reply::Nil => Value::Null,
        Reply::Int(n) => Value::Int(*n),
        Reply::BulkString(bytes) => bytes_value(bytes),
        Reply::SimpleString(text) => Value::text(text.as_str()),
        Reply::Okay => Value::text("OK"),
        Reply::Double(n) => Value::Float(*n),
        Reply::Boolean(b) => Value::Bool(*b),
        Reply::VerbatimString { text, .. } => Value::text(text.as_str()),
        Reply::BigNumber(n) => Value::text(String::from_utf8_lossy(n).into_owned()),
        Reply::Attribute { data, .. } => cell(data),
        Reply::ServerError(error) => Value::text(format!("(error) {error:?}")),
        other => Value::Json(Arc::from(json(other).to_string())),
    }
}

fn json(reply: &Reply) -> serde_json::Value {
    use serde_json::Value as J;
    match reply {
        Reply::Nil => J::Null,
        Reply::Int(n) => J::from(*n),
        Reply::Double(n) => J::from(*n),
        Reply::Boolean(b) => J::Bool(*b),
        Reply::Array(items) | Reply::Set(items) | Reply::Push { data: items, .. } => {
            J::Array(items.iter().map(json).collect())
        }
        Reply::Map(pairs) => {
            let all_text = pairs
                .iter()
                .all(|(key, _)| matches!(cell(key), Value::Text(_)));
            if all_text {
                J::Object(
                    pairs
                        .iter()
                        .map(|(key, value)| (cell(key).to_string(), json(value)))
                        .collect(),
                )
            } else {
                J::Array(
                    pairs
                        .iter()
                        .map(|(key, value)| J::Array(vec![json(key), json(value)]))
                        .collect(),
                )
            }
        }
        Reply::Attribute { data, .. } => json(data),
        other => J::String(cell(other).to_string()),
    }
}

/// The elements of a list-like reply.
pub(crate) fn items(reply: Reply) -> Vec<Reply> {
    match reply {
        Reply::Array(items) | Reply::Set(items) => items,
        Reply::Map(pairs) => pairs
            .into_iter()
            .flat_map(|(key, value)| [key, value])
            .collect(),
        Reply::Nil => Vec::new(),
        other => vec![other],
    }
}

/// How a command's reply is laid out as rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// One `value` column; a list reply is one row per element.
    Values,
    /// Alternating keys and values, as from `HGETALL`: two columns.
    Pairs(&'static str, &'static str),
    /// `LRANGE`: each element with its index, counting from `start`.
    Indexed(i64),
    /// `XRANGE`: entries of an id and a field list.
    Stream,
    /// `INFO`: `key:value` lines under `# Section` headings.
    Info,
}

impl Shape {
    pub(crate) fn columns(self, first: &str) -> Vec<ColumnMeta> {
        match self {
            Self::Values => vec![meta(first, "")],
            Self::Pairs(key, value) => vec![meta(key, "string"), meta(value, "string")],
            Self::Indexed(_) => vec![meta("index", "integer"), meta("value", "string")],
            Self::Stream => vec![meta("id", "string"), meta("fields", "json")],
            Self::Info => vec![
                meta("section", "string"),
                meta("key", "string"),
                meta("value", "string"),
            ],
        }
    }

    /// The rows of a whole reply.
    pub(crate) fn rows(self, reply: Reply) -> Vec<Vec<Value>> {
        match self {
            Self::Values => match reply {
                Reply::Array(_) | Reply::Set(_) => {
                    items(reply).iter().map(|item| vec![cell(item)]).collect()
                }
                Reply::Map(pairs) => pairs
                    .iter()
                    .map(|(key, value)| vec![cell(key), cell(value)])
                    .collect(),
                other => vec![vec![cell(&other)]],
            },
            Self::Pairs(..) => {
                let items = items(reply);
                items
                    .chunks(2)
                    .map(|pair| pair.iter().map(cell).collect())
                    .collect()
            }
            Self::Indexed(start) => items(reply)
                .iter()
                .enumerate()
                .map(|(ix, item)| {
                    let index = if start >= 0 {
                        Value::Int(start + ix as i64)
                    } else {
                        Value::Null
                    };
                    vec![index, cell(item)]
                })
                .collect(),
            Self::Stream => items(reply)
                .into_iter()
                .map(|entry| {
                    let mut parts = items(entry).into_iter();
                    let id = parts.next().map_or(Value::Null, |id| cell(&id));
                    let fields = parts.next().map_or(Value::Null, |fields| {
                        let pairs: serde_json::Map<String, serde_json::Value> = items(fields)
                            .chunks(2)
                            .map(|pair| {
                                (
                                    cell(&pair[0]).to_string(),
                                    pair.get(1).map_or(serde_json::Value::Null, json),
                                )
                            })
                            .collect();
                        Value::Json(Arc::from(serde_json::Value::Object(pairs).to_string()))
                    });
                    vec![id, fields]
                })
                .collect(),
            Self::Info => {
                let text = cell(&reply).to_string();
                let mut section = String::new();
                text.lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        if let Some(name) = line.strip_prefix('#') {
                            section = name.trim().to_owned();
                            return None;
                        }
                        let (key, value) = line.split_once(':')?;
                        Some(vec![
                            Value::text(section.as_str()),
                            Value::text(key),
                            Value::text(value),
                        ])
                    })
                    .collect()
            }
        }
    }
}

fn has_word(args: &[String], word: &str) -> bool {
    args.iter()
        .skip(1)
        .any(|arg| arg.eq_ignore_ascii_case(word))
}

/// How the reply to `args` (upper-cased command first) is shaped, and what
/// its single column is called when it has one.
pub(crate) fn shape(args: &[String]) -> (Shape, &'static str) {
    let name = args.first().map_or("", String::as_str);
    let sub = args.get(1).map(|s| s.to_ascii_uppercase());
    match (name, sub.as_deref()) {
        ("HGETALL", _) => (Shape::Pairs("field", "value"), ""),
        ("CONFIG", Some("GET")) => (Shape::Pairs("parameter", "value"), ""),
        ("ZRANGE" | "ZREVRANGE" | "ZRANGEBYSCORE" | "ZREVRANGEBYSCORE", _)
            if has_word(args, "WITHSCORES") =>
        {
            (Shape::Pairs("member", "score"), "")
        }
        ("LRANGE", _) => (
            Shape::Indexed(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(-1)),
            "",
        ),
        ("XRANGE" | "XREVRANGE", _) => (Shape::Stream, ""),
        ("INFO", _) => (Shape::Info, ""),
        ("KEYS", _) => (Shape::Values, "key"),
        ("SMEMBERS" | "ZRANGE" | "ZREVRANGE", _) => (Shape::Values, "member"),
        _ => (Shape::Values, "value"),
    }
}

/// The `SCAN` family, which Sonde runs to the end, streaming every page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scan {
    /// Where the cursor argument sits.
    pub cursor_at: usize,
    pub columns: Vec<ColumnMeta>,
    /// Elements per row: 1 for keys and members, 2 for fields and scores.
    pub width: usize,
}

pub(crate) fn scan(args: &[String]) -> Option<Scan> {
    let name = args.first()?.as_str();
    let (cursor_at, names): (usize, &[&str]) = match name {
        "SCAN" => (1, &["key"]),
        "SSCAN" => (2, &["member"]),
        "HSCAN" if has_word(args, "NOVALUES") => (2, &["field"]),
        "HSCAN" => (2, &["field", "value"]),
        "ZSCAN" => (2, &["member", "score"]),
        _ => return None,
    };
    (args.get(cursor_at)? == "0").then(|| Scan {
        cursor_at,
        columns: names.iter().map(|name| meta(name, "string")).collect(),
        width: names.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Vec<String> {
        parse(line)
            .unwrap()
            .into_iter()
            .map(|arg| String::from_utf8(arg).unwrap())
            .collect()
    }

    #[test]
    fn parse_should_take_quotes_and_escapes_like_redis_cli() {
        assert_eq!(
            texts(r#"SET "a key" 'it\'s' "tab\there\x41""#),
            ["SET", "a key", "it's", "tab\thereA"]
        );
    }

    #[test]
    fn parse_should_refuse_an_open_quote() {
        assert!(parse(r#"GET "oops"#).is_err());
    }

    #[test]
    fn quote_should_round_trip_through_parse() {
        for arg in [
            "plain",
            "with space",
            "q\"uote",
            "back\\slash",
            "new\nline",
            "",
        ] {
            assert_eq!(texts(&quote(arg)), [arg], "{arg:?}");
        }
    }

    #[test]
    fn shape_should_pair_hash_fields_and_scores() {
        let args = |line: &str| -> Vec<String> {
            texts(line)
                .into_iter()
                .enumerate()
                .map(|(ix, arg)| if ix == 0 { arg.to_uppercase() } else { arg })
                .collect()
        };
        assert_eq!(shape(&args("hgetall h")).0, Shape::Pairs("field", "value"));
        assert_eq!(
            shape(&args("zrange z 0 -1 withscores")).0,
            Shape::Pairs("member", "score")
        );
        assert_eq!(shape(&args("lrange l 5 9")).0, Shape::Indexed(5));
        assert!(scan(&args("scan 0 match a*")).is_some());
        assert!(scan(&args("scan 17")).is_none());
    }

    #[test]
    fn info_should_become_section_key_value_rows() {
        let reply = Reply::BulkString(
            b"# Server\r\nredis_version:7.4.0\r\n\r\n# Clients\r\nconnected_clients:1\r\n".to_vec(),
        );

        let rows = Shape::Info.rows(reply);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], Value::text("Clients"));
        assert_eq!(rows[1][2], Value::text("1"));
    }
}
