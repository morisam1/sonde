//! SQL Server cells to [`Value`], and [`Value`] parameters to SQL Server.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use tiberius::{Column, ColumnData, ColumnType, FromSql, Query, Row};

use crate::{
    error::{Error, Result},
    value::{ColumnMeta, Value},
};

/// How one column's cells are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decoder {
    /// Money arrives as a float; it is shown as the exact decimal it is.
    Money,
    Other,
}

impl Decoder {
    pub(super) fn for_column(column: &Column) -> Self {
        match column.column_type() {
            ColumnType::Money | ColumnType::Money4 => Self::Money,
            _ => Self::Other,
        }
    }

    pub(super) fn meta(self, column: &Column) -> ColumnMeta {
        use ColumnType::*;

        let type_name = match column.column_type() {
            Null => "null",
            Bit | Bitn => "bit",
            Int1 => "tinyint",
            Int2 => "smallint",
            Int4 | Intn => "int",
            Int8 => "bigint",
            Float4 => "real",
            Float8 | Floatn => "float",
            Money => "money",
            Money4 => "smallmoney",
            Decimaln => "decimal",
            Numericn => "numeric",
            Datetime4 => "smalldatetime",
            Datetime | Datetimen => "datetime",
            Datetime2 => "datetime2",
            DatetimeOffsetn => "datetimeoffset",
            Daten => "date",
            Timen => "time",
            Guid => "uniqueidentifier",
            BigVarBin => "varbinary",
            BigBinary => "binary",
            Image => "image",
            BigVarChar => "varchar",
            BigChar => "char",
            NVarchar => "nvarchar",
            NChar => "nchar",
            Text => "text",
            NText => "ntext",
            Xml => "xml",
            Udt => "udt",
            SSVariant => "sql_variant",
        };
        ColumnMeta {
            name: Arc::from(column.name()),
            type_name: Arc::from(type_name),
            origin: None,
        }
    }

    fn decode(self, data: ColumnData<'static>) -> Value {
        let lossy = |what: &str| Value::text(format!("<{what} out of range>"));
        match data {
            ColumnData::U8(value) => value.map_or(Value::Null, |n| Value::Int(n.into())),
            ColumnData::I16(value) => value.map_or(Value::Null, |n| Value::Int(n.into())),
            ColumnData::I32(value) => value.map_or(Value::Null, |n| Value::Int(n.into())),
            ColumnData::I64(value) => value.map_or(Value::Null, Value::Int),
            ColumnData::F32(value) => value.map_or(Value::Null, |n| Value::Float(n.into())),
            ColumnData::F64(None) => Value::Null,
            ColumnData::F64(Some(n)) if self == Self::Money => Decimal::try_from(n)
                .map(|d| Value::Decimal(d.round_dp(4)))
                .unwrap_or(Value::Float(n)),
            ColumnData::F64(Some(n)) => Value::Float(n),
            ColumnData::Bit(value) => value.map_or(Value::Null, Value::Bool),
            ColumnData::String(value) => value.map_or(Value::Null, |s| Value::text(s.as_ref())),
            ColumnData::Guid(value) => value.map_or(Value::Null, Value::Uuid),
            ColumnData::Binary(value) => {
                value.map_or(Value::Null, |b| Value::Bytes(Arc::from(b.as_ref())))
            }
            ColumnData::Xml(value) => value.map_or(Value::Null, |x| Value::text(x.to_string())),
            ref numeric @ ColumnData::Numeric(_) => match Decimal::from_sql(numeric) {
                Ok(Some(d)) => Value::Decimal(d),
                Ok(None) => Value::Null,
                Err(_) => lossy("decimal"),
            },
            ref date @ ColumnData::Date(_) => match NaiveDate::from_sql(date) {
                Ok(Some(d)) => Value::Date(d),
                Ok(None) => Value::Null,
                Err(_) => lossy("date"),
            },
            ref time @ ColumnData::Time(_) => match NaiveTime::from_sql(time) {
                Ok(Some(t)) => Value::Time(t),
                Ok(None) => Value::Null,
                Err(_) => lossy("time"),
            },
            ref offset @ ColumnData::DateTimeOffset(_) => {
                match DateTime::<chrono::FixedOffset>::from_sql(offset) {
                    Ok(Some(t)) => Value::TimestampTz(t.with_timezone(&Utc)),
                    Ok(None) => Value::Null,
                    Err(_) => lossy("datetimeoffset"),
                }
            }
            ref stamp @ (ColumnData::DateTime(_)
            | ColumnData::SmallDateTime(_)
            | ColumnData::DateTime2(_)) => match NaiveDateTime::from_sql(stamp) {
                Ok(Some(t)) => Value::Timestamp(t),
                Ok(None) => Value::Null,
                Err(_) => lossy("datetime"),
            },
        }
    }
}

pub(super) fn row(row: Row, decoders: &[Decoder]) -> Vec<Value> {
    row.into_iter()
        .zip(decoders)
        .map(|(data, decoder)| decoder.decode(data))
        .collect()
}

/// Refuse values SQL Server has no type for.
pub(super) fn check_param(value: &Value) -> Result<()> {
    match value {
        Value::Interval { .. } | Value::Array(_) => Err(Error::Parameter {
            value: value.kind(),
            expected: "a SQL Server value".into(),
        }),
        _ => Ok(()),
    }
}

/// Bind `value` as the query's next parameter. Call [`check_param`] first.
pub(super) fn bind(query: &mut Query<'_>, value: &Value) {
    match value {
        Value::Null | Value::Interval { .. } | Value::Array(_) => query.bind(Option::<&str>::None),
        Value::Bool(value) => query.bind(*value),
        Value::Int(value) => query.bind(*value),
        Value::Float(value) => query.bind(*value),
        Value::Decimal(value) => query.bind(*value),
        Value::Text(text) | Value::Json(text) => query.bind(text.to_string()),
        Value::Bytes(bytes) => query.bind(bytes.to_vec()),
        Value::Date(value) => query.bind(*value),
        Value::Time(value) => query.bind(*value),
        Value::Timestamp(value) => query.bind(*value),
        Value::TimestampTz(value) => query.bind(*value),
        Value::Uuid(value) => query.bind(*value),
        Value::Unknown { raw, .. } => query.bind(raw.to_vec()),
    }
}
