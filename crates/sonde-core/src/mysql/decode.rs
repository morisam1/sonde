//! MySQL cells to [`Value`], and [`Value`] parameters to MySQL.
//!
//! Plain statements use the text protocol, where every cell arrives as text;
//! prepared ones arrive typed. A decoder is chosen per column from its type
//! and flags and accepts both.

use std::sync::Arc;

use chrono::{Datelike as _, NaiveDate, NaiveDateTime, NaiveTime, Timelike as _};
use mysql_async::{
    Column, Row, Value as My,
    consts::{ColumnFlags, ColumnType},
};
use rust_decimal::Decimal;

use crate::{
    error::{Error, Result},
    value::{ColumnMeta, ColumnOrigin, Value},
};

/// MySQL's character set number for binary data.
const BINARY_CHARSET: u16 = 63;

/// How one column's cells are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decoder {
    Int,
    UInt,
    Float,
    Decimal,
    Date,
    DateTime,
    Time,
    Json,
    Bits,
    Bytes,
    Text,
    /// Kept as raw bytes under the type's name, such as spatial values.
    Opaque(&'static str),
}

impl Decoder {
    pub(super) fn for_column(column: &Column) -> Self {
        use ColumnType::*;

        let unsigned = column.flags().contains(ColumnFlags::UNSIGNED_FLAG);
        let binary = column.character_set() == BINARY_CHARSET;
        match column.column_type() {
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
            | MYSQL_TYPE_YEAR => Self::Int,
            MYSQL_TYPE_LONGLONG if unsigned => Self::UInt,
            MYSQL_TYPE_LONGLONG => Self::Int,
            MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => Self::Float,
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Self::Decimal,
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => Self::Date,
            MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_TIMESTAMP2 => Self::DateTime,
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => Self::Time,
            MYSQL_TYPE_JSON => Self::Json,
            MYSQL_TYPE_BIT => Self::Bits,
            MYSQL_TYPE_GEOMETRY => Self::Opaque("geometry"),
            MYSQL_TYPE_VECTOR => Self::Opaque("vector"),
            _ if binary => Self::Bytes,
            _ => Self::Text,
        }
    }

    /// The column's description, with a type name the rest of Sonde reads:
    /// the editor parses by it and the grid aligns numbers by it.
    pub(super) fn meta(self, column: &Column) -> ColumnMeta {
        use ColumnType::*;

        let unsigned = column.flags().contains(ColumnFlags::UNSIGNED_FLAG);
        let binary = column.character_set() == BINARY_CHARSET;
        let base = match column.column_type() {
            MYSQL_TYPE_TINY => "tinyint",
            MYSQL_TYPE_SHORT => "smallint",
            MYSQL_TYPE_INT24 => "mediumint",
            MYSQL_TYPE_LONG => "int",
            MYSQL_TYPE_LONGLONG => "bigint",
            MYSQL_TYPE_YEAR => "year",
            MYSQL_TYPE_FLOAT => "float",
            MYSQL_TYPE_DOUBLE => "double",
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
            MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
            MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
            MYSQL_TYPE_JSON => "json",
            MYSQL_TYPE_BIT => "bit",
            MYSQL_TYPE_ENUM => "enum",
            MYSQL_TYPE_SET => "set",
            MYSQL_TYPE_GEOMETRY => "geometry",
            MYSQL_TYPE_VECTOR => "vector",
            MYSQL_TYPE_NULL => "null",
            MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB => {
                if binary {
                    "blob"
                } else {
                    "text"
                }
            }
            MYSQL_TYPE_STRING => {
                if column.flags().contains(ColumnFlags::ENUM_FLAG) {
                    "enum"
                } else if column.flags().contains(ColumnFlags::SET_FLAG) {
                    "set"
                } else if binary {
                    "binary"
                } else {
                    "char"
                }
            }
            _ if binary => "varbinary",
            _ => "varchar",
        };
        let type_name = if unsigned && self != Self::Float && self != Self::Decimal {
            format!("{base} unsigned")
        } else {
            base.to_owned()
        };
        let table = column.org_table_str();
        let origin = (!table.is_empty()).then(|| {
            let schema = column.schema_str();
            ColumnOrigin {
                schema: (!schema.is_empty()).then(|| Arc::from(schema.as_ref())),
                table: Arc::from(table.as_ref()),
                column: Arc::from(column.org_name_str().as_ref()),
            }
        });
        ColumnMeta {
            name: Arc::from(column.name_str().as_ref()),
            type_name: Arc::from(type_name),
            origin,
        }
    }

    pub(super) fn decode(self, value: My) -> Value {
        match value {
            My::NULL => Value::Null,
            My::Int(n) => match self {
                Self::Float => Value::Float(n as f64),
                Self::Decimal => Value::Decimal(Decimal::from(n)),
                _ => Value::Int(n),
            },
            My::UInt(n) => {
                i64::try_from(n).map_or_else(|_| Value::Decimal(Decimal::from(n)), Value::Int)
            }
            My::Float(n) => Value::Float(f64::from(n)),
            My::Double(n) => Value::Float(n),
            My::Date(year, month, day, hour, minute, second, micros) => {
                let date = NaiveDate::from_ymd_opt(year.into(), month.into(), day.into());
                let time = NaiveTime::from_hms_micro_opt(
                    hour.into(),
                    minute.into(),
                    second.into(),
                    micros,
                );
                match (self, date, time) {
                    (Self::Date, Some(date), _) => Value::Date(date),
                    (_, Some(date), Some(time)) => Value::Timestamp(NaiveDateTime::new(date, time)),
                    // MySQL's zero date, 0000-00-00, is no calendar date.
                    _ => Value::text(format!("{year:04}-{month:02}-{day:02}")),
                }
            }
            My::Time(negative, days, hours, minutes, seconds, micros) => {
                let in_day = !negative && days == 0;
                match NaiveTime::from_hms_micro_opt(
                    hours.into(),
                    minutes.into(),
                    seconds.into(),
                    micros,
                )
                .filter(|_| in_day)
                {
                    Some(time) => Value::Time(time),
                    None => {
                        let hours = u32::from(hours) + days * 24;
                        let sign = if negative { "-" } else { "" };
                        Value::text(format!("{sign}{hours:02}:{minutes:02}:{seconds:02}"))
                    }
                }
            }
            My::Bytes(bytes) => self.decode_bytes(bytes),
        }
    }

    /// A cell as the text protocol sends it.
    fn decode_bytes(self, bytes: Vec<u8>) -> Value {
        if matches!(self, Self::Bytes) {
            return Value::Bytes(Arc::from(bytes));
        }
        if let Self::Opaque(type_name) = self {
            return Value::Unknown {
                type_name: Arc::from(type_name),
                raw: Arc::from(bytes),
            };
        }
        if let Self::Bits = self {
            if bytes.len() <= 8 {
                let n = bytes
                    .iter()
                    .fold(0_u64, |n, byte| (n << 8) | u64::from(*byte));
                return i64::try_from(n)
                    .map_or_else(|_| Value::Decimal(Decimal::from(n)), Value::Int);
            }
            return Value::Bytes(Arc::from(bytes));
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => return Value::Bytes(Arc::from(error.into_bytes())),
        };
        let parsed = match self {
            Self::Int => text.parse().ok().map(Value::Int),
            Self::UInt => text.parse::<u64>().ok().map(|n| {
                i64::try_from(n).map_or_else(|_| Value::Decimal(Decimal::from(n)), Value::Int)
            }),
            Self::Float => text.parse().ok().map(Value::Float),
            Self::Decimal => text.parse().ok().map(Value::Decimal),
            Self::Date => NaiveDate::parse_from_str(&text, "%Y-%m-%d")
                .ok()
                .map(Value::Date),
            Self::DateTime => NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(Value::Timestamp),
            Self::Time => NaiveTime::parse_from_str(&text, "%H:%M:%S%.f")
                .ok()
                .map(Value::Time),
            Self::Json => Some(Value::Json(Arc::from(text.as_str()))),
            _ => None,
        };
        parsed.unwrap_or_else(|| Value::text(text))
    }
}

pub(super) fn row(row: Row, decoders: &[Decoder]) -> Vec<Value> {
    row.unwrap_raw()
        .into_iter()
        .zip(decoders)
        .map(|(cell, decoder)| decoder.decode(cell.unwrap_or(My::NULL)))
        .collect()
}

/// A value as a statement parameter.
pub(super) fn param(value: &Value) -> Result<My> {
    let date = |date: NaiveDate, time: NaiveTime| {
        My::Date(
            u16::try_from(date.year()).unwrap_or(0),
            date.month() as u8,
            date.day() as u8,
            time.hour() as u8,
            time.minute() as u8,
            time.second() as u8,
            time.nanosecond() / 1_000,
        )
    };
    Ok(match value {
        Value::Null => My::NULL,
        Value::Bool(value) => My::Int(i64::from(*value)),
        Value::Int(value) => My::Int(*value),
        Value::Float(value) => My::Double(*value),
        Value::Decimal(value) => My::Bytes(value.to_string().into_bytes()),
        Value::Text(text) | Value::Json(text) => My::Bytes(text.as_bytes().to_vec()),
        Value::Bytes(bytes) => My::Bytes(bytes.to_vec()),
        Value::Date(value) => date(*value, NaiveTime::MIN),
        Value::Time(value) => My::Time(
            false,
            0,
            value.hour() as u8,
            value.minute() as u8,
            value.second() as u8,
            value.nanosecond() / 1_000,
        ),
        Value::Timestamp(value) => date(value.date(), value.time()),
        Value::TimestampTz(value) => date(value.date_naive(), value.time()),
        Value::Uuid(value) => My::Bytes(value.to_string().into_bytes()),
        Value::Unknown { raw, .. } => My::Bytes(raw.to_vec()),
        Value::Interval { .. } | Value::Array(_) => {
            return Err(Error::Parameter {
                value: value.kind(),
                expected: "a MySQL value".into(),
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_cells_should_decode_by_column_type() {
        let cell = |decoder: Decoder, text: &str| decoder.decode(My::Bytes(text.into()));

        assert_eq!(cell(Decoder::Int, "-7"), Value::Int(-7));
        assert_eq!(
            cell(Decoder::UInt, "18446744073709551615"),
            Value::Decimal(Decimal::from(u64::MAX))
        );
        assert_eq!(
            cell(Decoder::Decimal, "10.50"),
            Value::Decimal("10.50".parse().unwrap())
        );
        assert_eq!(
            cell(Decoder::DateTime, "2024-02-29 13:14:15.5"),
            Value::Timestamp(
                NaiveDate::from_ymd_opt(2024, 2, 29)
                    .unwrap()
                    .and_hms_milli_opt(13, 14, 15, 500)
                    .unwrap()
            )
        );
        assert_eq!(cell(Decoder::Date, "0000-00-00"), Value::text("0000-00-00"));
        assert_eq!(cell(Decoder::Time, "838:59:59"), Value::text("838:59:59"));
        assert_eq!(cell(Decoder::Bits, "\u{1}"), Value::Int(1));
        assert_eq!(
            cell(Decoder::Json, "{\"a\":1}"),
            Value::Json(Arc::from("{\"a\":1}"))
        );
    }

    #[test]
    fn binary_cells_should_decode_too() {
        assert_eq!(
            Decoder::Time.decode(My::Time(true, 1, 2, 3, 4, 0)),
            Value::text("-26:03:04")
        );
        assert_eq!(
            Decoder::Date.decode(My::Date(2020, 1, 2, 0, 0, 0, 0)),
            Value::Date(NaiveDate::from_ymd_opt(2020, 1, 2).unwrap())
        );
    }

    #[test]
    fn param_should_refuse_what_mysql_cannot_store() {
        assert!(param(&Value::Array(Arc::from([]))).is_err());
        assert_eq!(param(&Value::Bool(true)).unwrap(), My::Int(1));
    }
}
