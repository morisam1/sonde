//! Turning Postgres's binary wire values into [`Value`]s and back.

use std::{error::Error as StdError, sync::Arc};

use bytes::BytesMut;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use postgres_protocol::types as wire;
use rust_decimal::Decimal;
use tokio_postgres::types::{FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};
use uuid::Uuid;

use crate::value::Value;

type BoxError = Box<dyn StdError + Sync + Send>;

/// A cell read from a row. Accepts every type and decides by OID, so one
/// `FromSql` implementation serves any query.
pub(super) struct Cell(pub Value);

impl<'a> FromSql<'a> for Cell {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        Ok(Self(decode(ty, raw)))
    }

    fn from_sql_null(_: &Type) -> Result<Self, BoxError> {
        Ok(Self(Value::Null))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

/// Decode one non-null value. A value that does not parse as its type, or a
/// type without a decoder, comes back as [`Value::Unknown`] rather than
/// failing the row.
fn decode(ty: &Type, raw: &[u8]) -> Value {
    known(ty, raw).unwrap_or_else(|| Value::Unknown {
        type_name: ty.name().into(),
        raw: raw.into(),
    })
}

fn known(ty: &Type, raw: &[u8]) -> Option<Value> {
    let value = match *ty {
        Type::BOOL => Value::Bool(wire::bool_from_sql(raw).ok()?),
        Type::INT2 => Value::Int(wire::int2_from_sql(raw).ok()?.into()),
        Type::INT4 => Value::Int(wire::int4_from_sql(raw).ok()?.into()),
        Type::INT8 => Value::Int(wire::int8_from_sql(raw).ok()?),
        Type::OID => Value::Int(wire::oid_from_sql(raw).ok()?.into()),
        Type::FLOAT4 => Value::Float(wire::float4_from_sql(raw).ok()?.into()),
        Type::FLOAT8 => Value::Float(wire::float8_from_sql(raw).ok()?),
        Type::NUMERIC => numeric(raw)?,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN | Type::XML => {
            Value::text(wire::text_from_sql(raw).ok()?)
        }
        Type::CHAR => Value::text(char::from(*raw.first()?).to_string()),
        Type::BYTEA => Value::Bytes(raw.into()),
        Type::DATE => date(wire::date_from_sql(raw).ok()?),
        Type::TIME => Value::Time(time(wire::time_from_sql(raw).ok()?)?),
        Type::TIMESTAMP => timestamp(wire::timestamp_from_sql(raw).ok()?, false),
        Type::TIMESTAMPTZ => timestamp(wire::timestamp_from_sql(raw).ok()?, true),
        Type::INTERVAL => interval(raw)?,
        Type::UUID => Value::Uuid(Uuid::from_bytes(wire::uuid_from_sql(raw).ok()?)),
        Type::JSON => Value::Json(wire::text_from_sql(raw).ok()?.into()),
        // jsonb is a one-byte format version followed by the JSON text.
        Type::JSONB => match raw.split_first()? {
            (1, text) => Value::Json(std::str::from_utf8(text).ok()?.into()),
            _ => return None,
        },
        _ => match ty.kind() {
            Kind::Array(element) => array(element, raw)?,
            Kind::Domain(base) => return known(base, raw),
            Kind::Enum(_) => Value::text(wire::text_from_sql(raw).ok()?),
            _ => return None,
        },
    };
    Some(value)
}

/// Text for a value Postgres can hold but our types cannot, such as
/// `infinity` or a 100-digit numeric.
fn out_of_range(type_name: &str, text: &str) -> Value {
    Value::Unknown {
        type_name: type_name.into(),
        raw: text.as_bytes().into(),
    }
}

/// The epoch of Postgres dates and timestamps.
fn pg_epoch() -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(2000, 1, 1)
}

fn date(days: i32) -> Value {
    let special = match days {
        i32::MAX => Some("infinity"),
        i32::MIN => Some("-infinity"),
        _ => None,
    };
    special
        .map(|text| out_of_range("date", text))
        .or_else(|| {
            pg_epoch()?
                .checked_add_signed(Duration::days(days.into()))
                .map(Value::Date)
        })
        .unwrap_or_else(|| out_of_range("date", &format!("{days} days from 2000-01-01")))
}

fn time(micros: i64) -> Option<NaiveTime> {
    let seconds = u32::try_from(micros.div_euclid(1_000_000)).ok()?;
    let nanos = u32::try_from(micros.rem_euclid(1_000_000) * 1_000).ok()?;
    NaiveTime::from_num_seconds_from_midnight_opt(seconds, nanos)
}

fn timestamp(micros: i64, with_zone: bool) -> Value {
    let type_name = if with_zone {
        "timestamptz"
    } else {
        "timestamp"
    };
    let special = match micros {
        i64::MAX => Some("infinity"),
        i64::MIN => Some("-infinity"),
        _ => None,
    };
    special
        .map(|text| out_of_range(type_name, text))
        .or_else(|| {
            let at: NaiveDateTime = pg_epoch()?
                .and_hms_opt(0, 0, 0)?
                .checked_add_signed(Duration::microseconds(micros))?;
            Some(if with_zone {
                Value::TimestampTz(DateTime::from_naive_utc_and_offset(at, chrono::Utc))
            } else {
                Value::Timestamp(at)
            })
        })
        .unwrap_or_else(|| {
            out_of_range(type_name, &format!("{micros} microseconds from 2000-01-01"))
        })
}

fn interval(raw: &[u8]) -> Option<Value> {
    let micros = i64::from_be_bytes(raw.get(0..8)?.try_into().ok()?);
    let days = i32::from_be_bytes(raw.get(8..12)?.try_into().ok()?);
    let months = i32::from_be_bytes(raw.get(12..16)?.try_into().ok()?);
    Some(Value::Interval {
        months,
        days,
        micros,
    })
}

/// Decode `numeric`'s wire form: a header and base-10000 digits.
///
/// Values that fit become [`Value::Decimal`]; `NaN`, the infinities and
/// numbers beyond `Decimal`'s 28 digits keep their exact text instead.
fn numeric(raw: &[u8]) -> Option<Value> {
    const NEGATIVE: u16 = 0x4000;
    const NAN: u16 = 0xC000;
    const INFINITY: u16 = 0xD000;
    const NEGATIVE_INFINITY: u16 = 0xF000;

    let field = |ix: usize| {
        Some(u16::from_be_bytes(
            raw.get(ix * 2..ix * 2 + 2)?.try_into().ok()?,
        ))
    };
    let digit_count = usize::from(field(0)?);
    let weight = i32::from(field(1)?.cast_signed());
    let sign = field(2)?;
    let scale = usize::from(field(3)?);
    match sign {
        NAN => return Some(out_of_range("numeric", "NaN")),
        INFINITY => return Some(out_of_range("numeric", "Infinity")),
        NEGATIVE_INFINITY => return Some(out_of_range("numeric", "-Infinity")),
        _ => {}
    }
    let digits = (0..digit_count)
        .map(|ix| field(4 + ix))
        .collect::<Option<Vec<u16>>>()?;

    let mut text = String::new();
    if sign == NEGATIVE {
        text.push('-');
    }
    // Digit `ix` has weight `weight - ix`; weights >= 0 are the integer part.
    if weight < 0 {
        text.push('0');
    } else {
        for position in 0..=weight {
            let digit = usize::try_from(position)
                .ok()
                .and_then(|ix| digits.get(ix))
                .copied()
                .unwrap_or(0);
            if position == 0 {
                text.push_str(&digit.to_string());
            } else {
                text.push_str(&format!("{digit:04}"));
            }
        }
    }
    if scale > 0 {
        text.push('.');
        let mut fraction = String::new();
        // The digit worth 10000^e sits at index `weight - e`.
        let mut exponent = -1;
        while fraction.len() < scale {
            let ix = weight - exponent;
            let digit = usize::try_from(ix)
                .ok()
                .and_then(|ix| digits.get(ix))
                .copied()
                .unwrap_or(0);
            fraction.push_str(&format!("{digit:04}"));
            exponent -= 1;
        }
        fraction.truncate(scale);
        text.push_str(&fraction);
    }

    Some(match text.parse::<Decimal>() {
        Ok(decimal) => Value::Decimal(decimal),
        Err(_) => out_of_range("numeric", &text),
    })
}

fn array(element: &Type, raw: &[u8]) -> Option<Value> {
    use fallible_iterator::FallibleIterator as _;
    use postgres_protocol::types::ArrayDimension;

    let array = wire::array_from_sql(raw).ok()?;
    let dimensions: Vec<ArrayDimension> = array.dimensions().collect().ok()?;
    let mut values = Vec::new();
    let mut items = array.values();
    while let Some(item) = items.next().ok()? {
        values.push(item.map_or(Value::Null, |raw| decode(element, raw)));
    }

    // Values arrive flattened in row-major order; fold them back up from the
    // innermost dimension.
    for dimension in dimensions.iter().skip(1).rev() {
        let width = usize::try_from(dimension.len).ok().filter(|&len| len > 0)?;
        values = values
            .chunks(width)
            .map(|chunk| Value::Array(chunk.into()))
            .collect();
    }
    Some(Value::Array(values.into()))
}

/// A [`Value`] bound as a statement parameter. Encodes for whatever type the
/// server inferred for the placeholder.
#[derive(Debug)]
pub(super) struct Param<'a>(pub &'a Value);

impl ToSql for Param<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        let mismatch = || -> BoxError {
            Box::new(crate::Error::Parameter {
                value: self.0.kind(),
                expected: ty.name().to_owned(),
            })
        };
        let ty = match ty.kind() {
            Kind::Domain(base) => base,
            _ => ty,
        };
        let is_text = matches!(
            *ty,
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN | Type::XML
        ) || matches!(ty.kind(), Kind::Enum(_));

        match (self.0, ty) {
            (Value::Null, _) => return Ok(IsNull::Yes),
            (Value::Bool(value), &Type::BOOL) => wire::bool_to_sql(*value, out),
            (Value::Int(value), &Type::INT2) => {
                wire::int2_to_sql(i16::try_from(*value).map_err(|_| mismatch())?, out);
            }
            (Value::Int(value), &Type::INT4) => {
                wire::int4_to_sql(i32::try_from(*value).map_err(|_| mismatch())?, out);
            }
            (Value::Int(value), &Type::INT8) => wire::int8_to_sql(*value, out),
            (Value::Int(value), &Type::OID) => {
                wire::oid_to_sql(u32::try_from(*value).map_err(|_| mismatch())?, out);
            }
            #[expect(
                clippy::cast_precision_loss,
                reason = "a float8 column rounds by definition"
            )]
            (Value::Int(value), &Type::FLOAT8) => wire::float8_to_sql(*value as f64, out),
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a float4 column rounds by definition"
            )]
            (Value::Float(value), &Type::FLOAT4) => wire::float4_to_sql(*value as f32, out),
            (Value::Float(value), &Type::FLOAT8) => wire::float8_to_sql(*value, out),
            (Value::Decimal(value), &Type::NUMERIC) => numeric_to_sql(value, out),
            (Value::Int(value), &Type::NUMERIC) => numeric_to_sql(&Decimal::from(*value), out),
            (Value::Text(value) | Value::Json(value), _) if is_text => {
                wire::text_to_sql(value, out);
            }
            (Value::Json(value) | Value::Text(value), &Type::JSON) => wire::text_to_sql(value, out),
            (Value::Json(value) | Value::Text(value), &Type::JSONB) => {
                out.extend_from_slice(&[1]);
                wire::text_to_sql(value, out);
            }
            (Value::Bytes(value), &Type::BYTEA) => wire::bytea_to_sql(value, out),
            (Value::Date(value), &Type::DATE) => {
                let days = value
                    .signed_duration_since(pg_epoch().ok_or_else(mismatch)?)
                    .num_days();
                wire::date_to_sql(i32::try_from(days).map_err(|_| mismatch())?, out);
            }
            (Value::Time(value), &Type::TIME) => {
                let micros = value
                    .signed_duration_since(NaiveTime::MIN)
                    .num_microseconds()
                    .ok_or_else(mismatch)?;
                wire::time_to_sql(micros, out);
            }
            (Value::Timestamp(value), &Type::TIMESTAMP) => {
                wire::timestamp_to_sql(timestamp_micros(*value).ok_or_else(mismatch)?, out);
            }
            (Value::TimestampTz(value), &Type::TIMESTAMPTZ) => {
                let micros = timestamp_micros(value.naive_utc()).ok_or_else(mismatch)?;
                wire::timestamp_to_sql(micros, out);
            }
            (
                Value::Interval {
                    months,
                    days,
                    micros,
                },
                &Type::INTERVAL,
            ) => {
                out.extend_from_slice(&micros.to_be_bytes());
                out.extend_from_slice(&days.to_be_bytes());
                out.extend_from_slice(&months.to_be_bytes());
            }
            (Value::Uuid(value), &Type::UUID) => wire::uuid_to_sql(*value.as_bytes(), out),
            (Value::Array(items), ty) => {
                let Kind::Array(element) = ty.kind() else {
                    return Err(mismatch());
                };
                array_to_sql(items, element, out)?;
            }
            _ => return Err(mismatch()),
        }
        Ok(IsNull::No)
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

fn timestamp_micros(at: NaiveDateTime) -> Option<i64> {
    at.signed_duration_since(pg_epoch()?.and_hms_opt(0, 0, 0)?)
        .num_microseconds()
}

fn numeric_to_sql(value: &Decimal, out: &mut BytesMut) {
    let scale = value.scale();
    let text = value.abs().to_string();
    let (integer, fraction) = text.split_once('.').unwrap_or((&text, ""));

    // Pad both sides out to whole base-10000 digits.
    let mut padded = "0".repeat((4 - integer.len() % 4) % 4);
    padded.push_str(integer);
    let integer_groups = padded.len() / 4;
    padded.push_str(fraction);
    padded.push_str(&"0".repeat((4 - padded.len() % 4) % 4));

    let mut digits: Vec<u16> = padded
        .as_bytes()
        .chunks(4)
        .map(|group| {
            group
                .iter()
                .fold(0, |acc, digit| acc * 10 + u16::from(digit - b'0'))
        })
        .collect();
    let leading = digits.iter().take_while(|&&digit| digit == 0).count();
    digits.drain(..leading);
    while digits.last() == Some(&0) {
        digits.pop();
    }
    let weight = i16::try_from(integer_groups).unwrap_or(i16::MAX)
        - 1
        - i16::try_from(leading).unwrap_or(i16::MAX);

    let count = u16::try_from(digits.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&(if digits.is_empty() { 0 } else { weight }).to_be_bytes());
    let sign: u16 = if value.is_sign_negative() && !digits.is_empty() {
        0x4000
    } else {
        0
    };
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&u16::try_from(scale).unwrap_or(u16::MAX).to_be_bytes());
    for digit in digits {
        out.extend_from_slice(&digit.to_be_bytes());
    }
}

fn array_to_sql(items: &Arc<[Value]>, element: &Type, out: &mut BytesMut) -> Result<(), BoxError> {
    let dimension = wire::ArrayDimension {
        len: i32::try_from(items.len())?,
        lower_bound: 1,
    };
    wire::array_to_sql(
        Some(dimension),
        element.oid(),
        items.iter(),
        |item, out| match Param(item).to_sql(element, out)? {
            IsNull::Yes => Ok(postgres_protocol::IsNull::Yes),
            IsNull::No => Ok(postgres_protocol::IsNull::No),
        },
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric_round_trip(text: &str) -> String {
        let decimal: Decimal = text.parse().expect("test input is a decimal");
        let mut wire = BytesMut::new();
        numeric_to_sql(&decimal, &mut wire);
        numeric(&wire).expect("encoded numeric decodes").to_string()
    }

    #[test]
    fn numeric_should_round_trip_an_integer() {
        assert_eq!(numeric_round_trip("1234567890123"), "1234567890123");
    }

    #[test]
    fn numeric_should_round_trip_a_fraction_with_trailing_zeros() {
        assert_eq!(numeric_round_trip("-12.3400"), "-12.3400");
    }

    #[test]
    fn numeric_should_round_trip_a_small_fraction() {
        assert_eq!(numeric_round_trip("0.000012345"), "0.000012345");
    }

    #[test]
    fn numeric_should_round_trip_zero_with_scale() {
        assert_eq!(numeric_round_trip("0.00"), "0.00");
    }

    #[test]
    fn numeric_should_round_trip_a_multiple_of_ten_thousand() {
        assert_eq!(numeric_round_trip("100000000"), "100000000");
    }
}
