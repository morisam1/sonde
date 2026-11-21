//! The value model: one enum for every cell, whichever database it came from.

use std::{fmt, sync::Arc};

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

/// A single cell.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// A boolean.
    Bool(bool),
    /// Any integer up to 64 bits.
    Int(i64),
    /// A binary floating point number.
    Float(f64),
    /// An exact decimal number.
    Decimal(Decimal),
    /// Text.
    Text(Arc<str>),
    /// Raw bytes.
    Bytes(Arc<[u8]>),
    /// A calendar date.
    Date(NaiveDate),
    /// A time of day.
    Time(NaiveTime),
    /// A date and time with no zone.
    Timestamp(NaiveDateTime),
    /// An instant.
    TimestampTz(DateTime<Utc>),
    /// A span of time, kept in the three parts Postgres stores.
    Interval {
        /// Whole months.
        months: i32,
        /// Whole days.
        days: i32,
        /// Microseconds.
        micros: i64,
    },
    /// A UUID.
    Uuid(Uuid),
    /// JSON, kept as source text.
    Json(Arc<str>),
    /// An array. Multi-dimensional arrays nest.
    Array(Arc<[Value]>),
    /// A type without a decoder. `raw` is whatever the database sent, or a
    /// text rendering when the driver could produce one.
    Unknown {
        /// The database's name for the type.
        type_name: Arc<str>,
        /// The undecoded bytes.
        raw: Arc<[u8]>,
    },
}

impl Value {
    /// Text from anything string-like.
    pub fn text(text: impl Into<Arc<str>>) -> Self {
        Self::Text(text.into())
    }

    /// Whether this is SQL `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// A short name for the variant, for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Int(_) => "integer",
            Self::Float(_) => "float",
            Self::Decimal(_) => "decimal",
            Self::Text(_) => "text",
            Self::Bytes(_) => "bytes",
            Self::Date(_) => "date",
            Self::Time(_) => "time",
            Self::Timestamp(_) => "timestamp",
            Self::TimestampTz(_) => "timestamp with time zone",
            Self::Interval { .. } => "interval",
            Self::Uuid(_) => "uuid",
            Self::Json(_) => "json",
            Self::Array(_) => "array",
            Self::Unknown { .. } => "unknown",
        }
    }
}

/// Plain text for a cell: what a grid shows and what copy puts on the
/// clipboard. `NULL` renders as an empty string; callers that need to tell it
/// apart check [`Value::is_null`].
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => Ok(()),
            Self::Bool(value) => value.fmt(f),
            Self::Int(value) => value.fmt(f),
            Self::Float(value) => value.fmt(f),
            Self::Decimal(value) => value.fmt(f),
            Self::Text(value) | Self::Json(value) => f.write_str(value),
            Self::Bytes(bytes) => write_hex(f, bytes),
            Self::Date(value) => value.fmt(f),
            Self::Time(value) => value.fmt(f),
            Self::Timestamp(value) => value.fmt(f),
            Self::TimestampTz(value) => write!(f, "{}", value.format("%Y-%m-%d %H:%M:%S%.f%:z")),
            Self::Interval {
                months,
                days,
                micros,
            } => write_interval(f, *months, *days, *micros),
            Self::Uuid(value) => value.fmt(f),
            Self::Array(items) => {
                f.write_str("{")?;
                for (ix, item) in items.iter().enumerate() {
                    if ix > 0 {
                        f.write_str(",")?;
                    }
                    if item.is_null() {
                        f.write_str("NULL")?;
                    } else {
                        write_array_item(f, item)?;
                    }
                }
                f.write_str("}")
            }
            Self::Unknown { raw, .. } => match std::str::from_utf8(raw) {
                Ok(text) if !text.contains('\0') => f.write_str(text),
                _ => write_hex(f, raw),
            },
        }
    }
}

fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    f.write_str("\\x")?;
    bytes.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
}

/// One element of an array literal. Elements that the literal's grammar would
/// misread are quoted, Postgres-style: a `"` doubled inside. Nested arrays are
/// never quoted; their braces already delimit them.
fn write_array_item(f: &mut fmt::Formatter<'_>, item: &Value) -> fmt::Result {
    if matches!(item, Value::Array(_)) {
        return write!(f, "{item}");
    }
    let text = item.to_string();
    let needs_quotes = text.is_empty()
        || text
            .chars()
            .any(|c| matches!(c, ',' | '"' | '{' | '}' | ' ' | '\t' | '\n'));
    if !needs_quotes {
        return f.write_str(&text);
    }
    f.write_str("\"")?;
    f.write_str(&text.replace('"', "\"\""))?;
    f.write_str("\"")
}

/// Postgres's default interval style: `1 year 2 mons 3 days 04:05:06.7`.
fn write_interval(f: &mut fmt::Formatter<'_>, months: i32, days: i32, micros: i64) -> fmt::Result {
    let mut parts = Vec::new();
    let (years, months) = (months / 12, months % 12);
    for (amount, unit) in [(years, "year"), (months, "mon"), (days, "day")] {
        if amount != 0 {
            let plural = if amount.abs() == 1 { "" } else { "s" };
            parts.push(format!("{amount} {unit}{plural}"));
        }
    }
    if micros != 0 || parts.is_empty() {
        let sign = if micros < 0 { "-" } else { "" };
        let total = micros.unsigned_abs();
        let (seconds, fraction) = (total / 1_000_000, total % 1_000_000);
        let mut clock = format!(
            "{sign}{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
        if fraction != 0 {
            let digits = format!("{fraction:06}");
            clock.push('.');
            clock.push_str(digits.trim_end_matches('0'));
        }
        parts.push(clock);
    }
    f.write_str(&parts.join(" "))
}

/// Where a result column comes from, when the database can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnOrigin {
    /// Schema of the source table, if the database has schemas.
    pub schema: Option<Arc<str>>,
    /// Source table.
    pub table: Arc<str>,
    /// Source column, which differs from the result column's name under an
    /// alias.
    pub column: Arc<str>,
}

/// Parse typed text for a column, such as from an in-cell editor.
///
/// An empty string is `NULL`. `type_name` is the column's type as the database
/// writes it, such as `numeric(10,2)` or `INTEGER`. The parse accepts at least
/// everything [`Display`](fmt::Display) writes for the type it produces, so a
/// read value can always be edited and written back.
pub fn parse(type_name: &str, text: &str) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    let name = type_name.to_ascii_lowercase();

    // MongoDB's names, which the tests below would misread.
    if name == "objectid" {
        let valid = text.len() == 24 && text.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(format!("{text:?} is not an ObjectId (want 24 hex digits)"));
        }
        return Ok(Value::Unknown {
            type_name: Arc::from(type_name),
            raw: Arc::from(text.to_ascii_lowercase().as_bytes()),
        });
    }
    if name == "object" || name == "array" || name == "document" {
        serde_json::from_str::<serde_json::Value>(text)
            .map_err(|_| format!("`{text}` is not valid JSON"))?;
        return Ok(Value::Json(Arc::from(text)));
    }
    if array_element_type(&name).is_some() {
        return parse_array(array_element_type(&name).unwrap(), text);
    }
    if name.contains("json") {
        serde_json::from_str::<serde_json::Value>(text)
            .map_err(|_| format!("`{text}` is not valid JSON"))?;
        return Ok(Value::Json(Arc::from(text)));
    }
    if name.contains("uuid") {
        return Uuid::parse_str(text)
            .map(Value::Uuid)
            .map_err(|_| format!("{text:?} is not a UUID"));
    }
    if name.contains("bool") {
        return parse_bool(text);
    }
    if is_numeric_name(&name) {
        return parse_number(&name, text);
    }
    if name.contains("timestamp") || name.contains("datetime") {
        return parse_timestamp(
            text,
            name.contains("timestamptz") || name.contains("with time zone"),
        );
    }
    if name == "date" || (name.contains("date") && !name.contains("time")) {
        return NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .map(Value::Date)
            .map_err(|_| format!("{text:?} is not a date (want YYYY-MM-DD)"));
    }
    if name.contains("time") {
        return NaiveTime::parse_from_str(text, "%H:%M:%S")
            .or_else(|_| NaiveTime::parse_from_str(text, "%H:%M:%S%.f"))
            .map(Value::Time)
            .map_err(|_| format!("{text:?} is not a time (want HH:MM:SS)"));
    }
    if name.contains("interval") {
        return parse_interval(text);
    }
    if name.contains("bytea")
        || name.contains("blob")
        || name.contains("binary")
        || name == "bindata"
    {
        return parse_bytes(text);
    }
    // Text, and anything the parse does not name: kept as text. A database
    // that cannot store it there will say so when the edit is applied.
    Ok(Value::text(text.to_owned()))
}

fn parse_bool(text: &str) -> Result<Value, String> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "1" => Ok(Value::Bool(true)),
        "false" | "f" | "no" | "0" => Ok(Value::Bool(false)),
        _ => Err(format!("{text:?} is not a boolean")),
    }
}

/// The same numeric family test the grid uses to align a column.
fn is_numeric_name(name: &str) -> bool {
    [
        "int", "float", "numeric", "decimal", "real", "double", "serial", "money", "oid",
    ]
    .iter()
    .any(|numeric| name.contains(numeric))
        && !name.starts_with('_')
        && !name.contains("interval")
        && !name.contains("point")
}

fn parse_number(name: &str, text: &str) -> Result<Value, String> {
    if name.contains("numeric") || name.contains("decimal") {
        return text
            .parse::<Decimal>()
            .map(Value::Decimal)
            .map_err(|_| format!("{text:?} is not a decimal number"));
    }
    if name.contains("float") || name.contains("real") || name.contains("double") {
        return text
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| format!("{text:?} is not a number"));
    }
    text.parse::<i64>()
        .map(Value::Int)
        .map_err(|_| format!("{text:?} is not an integer"))
}

fn parse_timestamp(text: &str, with_zone: bool) -> Result<Value, String> {
    if with_zone {
        // Accept the offset Display writes, treating the rest as a timestamp.
        let rfc3339 = text.replace(' ', "T");
        if let Ok(parsed) = DateTime::parse_from_rfc3339(&rfc3339) {
            return Ok(Value::TimestampTz(parsed.with_timezone(&Utc)));
        }
        // Postgres writes short offsets such as `+02`; `%#z` takes those as
        // well as `+0530` and `+05:30`.
        for format in ["%Y-%m-%d %H:%M:%S%#z", "%Y-%m-%d %H:%M:%S%.f%#z"] {
            if let Ok(parsed) = DateTime::parse_from_str(text, format) {
                return Ok(Value::TimestampTz(parsed.with_timezone(&Utc)));
            }
        }
    }
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f"));
    let naive =
        naive.map_err(|_| format!("{text:?} is not a timestamp (want YYYY-MM-DD HH:MM:SS)"))?;
    if with_zone {
        Ok(Value::TimestampTz(DateTime::from_naive_utc_and_offset(
            naive, Utc,
        )))
    } else {
        Ok(Value::Timestamp(naive))
    }
}

fn parse_bytes(text: &str) -> Result<Value, String> {
    let hex = text.strip_prefix(r#"\x"#).unwrap_or(text);
    let bytes = hex_decode(hex).ok_or_else(|| format!("{text:?} is not bytea hex"))?;
    Ok(Value::Bytes(bytes.into()))
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|ix| u8::from_str_radix(&hex[ix..ix + 2], 16).ok())
        .collect()
}

/// The same shapes [`Display`](fmt::Display) writes for an interval: a clock
/// time, with `N year(s)`/`N mon(s)`/`N day(s)` parts around it, in any order.
/// Number and unit are separate tokens.
fn parse_interval(text: &str) -> Result<Value, String> {
    let mut months = 0i32;
    let mut days = 0i32;
    let mut micros = 0i64;
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut ix = 0;
    while ix < tokens.len() {
        let token = tokens[ix];
        let bad = || format!("cannot read the interval {text:?} at {token:?}");
        match token.parse::<i64>() {
            Ok(number) => {
                let unit = tokens
                    .get(ix + 1)
                    .ok_or_else(|| format!("the interval {text:?} ends mid-part"))?;
                match unit.to_ascii_lowercase().as_str() {
                    "year" | "years" => {
                        let value = i64::checked_mul(number, 12).ok_or_else(bad)?;
                        months = months
                            .checked_add(i32::try_from(value).map_err(|_| bad())?)
                            .ok_or_else(bad)?;
                    }
                    "mon" | "mons" => {
                        months = months
                            .checked_add(i32::try_from(number).map_err(|_| bad())?)
                            .ok_or_else(bad)?;
                    }
                    "day" | "days" => {
                        days = days
                            .checked_add(i32::try_from(number).map_err(|_| bad())?)
                            .ok_or_else(bad)?;
                    }
                    _ => return Err(bad()),
                }
                ix += 2;
            }
            Err(_) => {
                micros += parse_clock(token, text)?;
                ix += 1;
            }
        }
    }
    Ok(Value::Interval {
        months,
        days,
        micros,
    })
}

/// A clock part: `HH:MM:SS[.fraction]`, optionally negated.
fn parse_clock(token: &str, whole: &str) -> Result<i64, String> {
    let err = || format!("{token:?} is not a clock time in {whole:?}");
    let negative = token.starts_with('-');
    let digits = token.strip_prefix('-').unwrap_or(token);
    let parts: Vec<&str> = digits.split(':').collect();
    if parts.len() > 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(err());
    }
    let multipliers = [3_600_000_000f64, 60_000_000f64, 1_000_000f64];
    let values = parts
        .iter()
        .map(|part| part.parse::<f64>().ok())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(err)?;
    let sum: f64 = values
        .iter()
        .zip(multipliers.iter())
        .map(|(value, multiplier)| value * multiplier)
        .sum();
    let rounded = sum.round();
    if !rounded.is_finite() || rounded.abs() > i64::MAX as f64 {
        return Err(err());
    }
    // Rounded and range-checked, so the cast is exact.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is an integer within i64's range"
    )]
    let micros = rounded as i64;
    if negative && micros != 0 {
        // The magnitude fit, so its negation can only be i64::MIN, which the
        // wrapping negation lands on anyway.
        Ok(micros.wrapping_neg())
    } else {
        Ok(micros)
    }
}

/// `{a, b, {c, d}}`: Postgres's array literal. Elements parse with the
/// element type; `NULL` and `""` are both null.
fn parse_array(element_type: &str, text: &str) -> Result<Value, String> {
    let inner = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| format!("{text:?} is not an array literal"))?;
    let items = split_top_level(inner)
        .into_iter()
        .map(|item| parse_array_item(element_type, &item))
        .collect::<Result<Vec<Value>, _>>()?;
    Ok(Value::Array(items.into()))
}

fn parse_array_item(element_type: &str, item: &str) -> Result<Value, String> {
    let item = item.trim();
    // Unquoted, case-insensitive: the literal's null. A quoted empty string
    // is an empty value, not a null.
    if item.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    let quoted = item
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .map(|body| body.replace("\"\"", "\""));
    match quoted {
        // The literal's spelling of an empty text value.
        Some(body) if body.is_empty() => Ok(Value::text(String::new())),
        Some(body) => parse(element_type, &body),
        None if array_element_type(element_type).is_some() => {
            parse_array(array_element_type(element_type).unwrap(), item)
        }
        None => parse(element_type, item),
    }
}

/// The element type of an array type name, in either of Postgres's two
/// spellings: `int4[]` and `_int4`.
fn array_element_type(name: &str) -> Option<&str> {
    name.strip_suffix("[]").or_else(|| name.strip_prefix('_'))
}

/// Split on commas that sit at brace depth zero.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quoted = false;
    for character in text.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            '{' if !quoted => {
                depth += 1;
                current.push(character);
            }
            '}' if !quoted => {
                depth -= 1;
                current.push(character);
            }
            ',' if !quoted && depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() || !parts.is_empty() {
        parts.push(current);
    }
    parts
}

/// Description of one result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    /// The column's name in the result.
    pub name: Arc<str>,
    /// The database's own type name. Empty when the database has none, as for
    /// a SQLite expression.
    pub type_name: Arc<str>,
    /// The table column this result column reads, when known. SQLite reports
    /// it; the Postgres driver does not look it up yet.
    pub origin: Option<ColumnOrigin>,
}

/// A batch of rows from a running query.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultPage {
    /// The rows, each as wide as the query's column list.
    pub rows: Vec<Vec<Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_should_take_the_short_offsets_postgres_writes() {
        let want = DateTime::parse_from_rfc3339("2000-01-01T02:00:00+02:00")
            .unwrap()
            .with_timezone(&Utc);
        for text in [
            "2000-01-01 02:00:00+02",
            "2000-01-01 02:00:00+0200",
            "2000-01-01 02:00:00+02:00",
            "2000-01-01 02:00:00.000+02",
        ] {
            assert_eq!(
                parse("timestamp with time zone", text),
                Ok(Value::TimestampTz(want)),
                "{text}"
            );
        }
    }

    /// What in-cell editing must survive: read a value, display it, edit it
    /// back in.
    #[test]
    fn parse_should_round_trip_every_displayed_value() {
        let cases: [(Value, &str); 14] = [
            (Value::Bool(true), "bool"),
            (Value::Int(i64::MAX), "int8"),
            (Value::Int(-42), "int4"),
            (Value::Float(1.5), "float8"),
            (Value::Float(-0.25), "float4"),
            (Value::Decimal(Decimal::from(1234567890123u64)), "numeric"),
            (Value::Decimal("1234.50".parse().unwrap()), "numeric(10,2)"),
            (Value::text("a plain word"), "text"),
            (Value::Bytes(vec![0xde, 0xad].into()), "bytea"),
            (
                Value::Date(NaiveDate::from_ymd_opt(2026, 11, 14).unwrap()),
                "date",
            ),
            (
                Value::Time(NaiveTime::from_hms_opt(12, 34, 56).unwrap()),
                "time",
            ),
            (
                Value::Timestamp(
                    NaiveDate::from_ymd_opt(2026, 11, 14)
                        .unwrap()
                        .and_hms_opt(1, 2, 3)
                        .unwrap(),
                ),
                "timestamp",
            ),
            (
                Value::TimestampTz(DateTime::from_naive_utc_and_offset(
                    NaiveDate::from_ymd_opt(2026, 11, 14)
                        .unwrap()
                        .and_hms_opt(1, 2, 3)
                        .unwrap(),
                    Utc,
                )),
                "timestamptz",
            ),
            (
                Value::Uuid("123e4567-e89b-12d3-a456-426614174000".parse().unwrap()),
                "uuid",
            ),
        ];
        for (value, type_name) in cases {
            assert_eq!(
                parse(type_name, &value.to_string()).unwrap(),
                value,
                "round trip through {type_name}"
            );
        }
    }

    #[test]
    fn parse_should_round_trip_json_intervals_and_arrays() {
        let json = Value::Json(r#"{"a": [1, 2], "b": "x,y"}"#.into());
        assert_eq!(parse("jsonb", &json.to_string()).unwrap(), json);

        let interval = Value::Interval {
            months: 14,
            days: 3,
            micros: 14_706_700_000,
        };
        assert_eq!(parse("interval", &interval.to_string()).unwrap(), interval);

        let array = Value::Array(Arc::from([
            Value::Int(1),
            Value::Null,
            Value::text("has, comma"),
        ]));
        assert!(
            parse("int4[]", &array.to_string()).is_err(),
            "a mixed-type array is not claimed"
        );
        let ints = Value::Array(Arc::from([Value::Int(1), Value::Null, Value::Int(3)]));
        assert_eq!(parse("int4[]", &ints.to_string()).unwrap(), ints);

        let words = Value::Array(Arc::from([
            Value::text("one"),
            Value::text("two, three"),
            Value::text(""),
        ]));
        assert_eq!(parse("text[]", &words.to_string()).unwrap(), words);
    }

    #[test]
    fn parse_should_treat_an_empty_string_as_null() {
        assert_eq!(parse("int4", "  ").unwrap(), Value::Null);
    }

    #[test]
    fn parse_should_accept_sqlite_declared_types() {
        assert_eq!(parse("INTEGER", "7").unwrap(), Value::Int(7));
        assert_eq!(parse("REAL", "1.5").unwrap(), Value::Float(1.5));
        assert_eq!(
            parse("DECIMAL(10,2)", "12.30").unwrap(),
            Value::Decimal("12.30".parse().unwrap())
        );
        assert_eq!(
            parse("DATETIME", "2026-11-14 08:00:00").unwrap(),
            Value::Timestamp(
                NaiveDate::from_ymd_opt(2026, 11, 14)
                    .unwrap()
                    .and_hms_opt(8, 0, 0)
                    .unwrap()
            )
        );
        assert_eq!(
            parse("BLOB", "\\x00ff").unwrap(),
            Value::Bytes(vec![0, 255].into())
        );
    }

    #[test]
    fn parse_should_reject_mismatched_values() {
        for text in ["7", "abc", ""] {
            if !text.is_empty() {
                assert!(parse("bool", text).is_err(), "bool from {text:?}");
            }
        }
        assert!(parse("int4", "1.5").is_err());
        assert!(parse("uuid", "nope").is_err());
        assert!(parse("date", "14/11/2026").is_err());
        assert!(parse("json", "not json").is_err());
    }

    fn interval(months: i32, days: i32, micros: i64) -> String {
        Value::Interval {
            months,
            days,
            micros,
        }
        .to_string()
    }

    #[test]
    fn display_should_render_bytes_as_hex() {
        assert_eq!(Value::Bytes(Arc::from([0xde, 0xad])).to_string(), "\\xdead");
    }

    #[test]
    fn display_should_render_null_inside_array_by_name() {
        let array = Value::Array(Arc::from([Value::Int(1), Value::Null]));

        assert_eq!(array.to_string(), "{1,NULL}");
    }

    #[test]
    fn display_should_render_unknown_as_text_when_utf8() {
        let value = Value::Unknown {
            type_name: "mood".into(),
            raw: Arc::from(*b"happy"),
        };

        assert_eq!(value.to_string(), "happy");
    }

    #[test]
    fn display_should_render_unknown_as_hex_when_not_utf8() {
        let value = Value::Unknown {
            type_name: "point".into(),
            raw: Arc::from([0xff, 0x00]),
        };

        assert_eq!(value.to_string(), "\\xff00");
    }

    #[test]
    fn display_should_render_interval_with_every_part() {
        assert_eq!(
            interval(14, 3, 14_706_700_000),
            "1 year 2 mons 3 days 04:05:06.7"
        );
    }

    #[test]
    fn display_should_render_zero_interval_as_clock() {
        assert_eq!(interval(0, 0, 0), "00:00:00");
    }

    #[test]
    fn display_should_render_negative_interval_time() {
        assert_eq!(interval(0, 1, -1_000_000), "1 day -00:00:01");
    }
}
