//! [MS-TDS] 2.2.5.4.1 Fixed-Length Data Types and 2.2.5.4.2 Variable-Length Data Types
//! (BYTELEN_TYPE): TYPE_INFO and value encoding of every `SqlType` that is neither a string
//! nor a binary.
//!
//! # Fixed form or N form
//!
//! `bit`, `tinyint`, `smallint`, `int`, `bigint`, `real`, `float`, `money`, `smallmoney`,
//! `datetime` and `smalldatetime` exist in two wire forms: a fixed-length token (BITTYPE,
//! INT4TYPE…) whose values cannot be NULL, and a BYTELEN token (BITNTYPE, INTNTYPE…) whose
//! values can. This module picks the fixed form when `TypeInfo::nullable` is `false` and the
//! N form otherwise. SQL Server itself sends the N forms for anything read from a table and the
//! fixed forms for non-null constants; clients accept both.
//!
//! `decimal`, `numeric`, `uniqueidentifier`, `date`, `time`, `datetime2` and `datetimeoffset`
//! only exist in the N form, whatever `nullable` says. A `Value::Null` with
//! `nullable == false` is refused with `NullInNotNullable` in every case, N form included.
//!
//! # Value layouts ([MS-TDS] 2.2.5.5.1)
//!
//! Everything is little-endian. `money` is the amount in ten-thousandths as an `i64`, written
//! high 32-bit word first, then low word; `smallmoney` is the same amount as an `i32`.
//! `datetime` is the `i32` day count since 1900-01-01 then the `u32` count of 1/300 s since
//! midnight; `smalldatetime` is the `u16` day count then the `u16` minute count. `date` is
//! the day count since 0001-01-01 on 3 bytes. `time(s)` is the count of `10^-s` s since
//! midnight on 3 (s ≤ 2), 4 (s ≤ 4) or 5 (s ≤ 7) bytes; `datetime2(s)` is `time(s)` then
//! `date`; `datetimeoffset(s)` is `datetime2(s)` in UTC then the offset in minutes as an
//! `i16`. `decimal` and `numeric` share one layout: one sign byte (1 positive or zero, 0
//! negative) then the magnitude on 4, 8, 12 or 16 bytes for precisions 1-9, 10-19, 20-28,
//! 29-38 (announced length 5, 9, 13, 17); only their TYPE_INFO token tells them apart. `uniqueidentifier` is the 16 bytes of `Value::Guid`, already in wire order.
//!
//! # No conversion
//!
//! A `Value` is assumed to match its `TypeInfo`. One that does not fit the announced layout
//! (a `Time` with more digits than the scale, a `Decimal` with another scale or too large a
//! magnitude, a `Money` outside `smallmoney`, a `DateTime` with seconds for `smalldatetime`…)
//! is refused with `ValueTypeMismatch` rather than rounded or truncated.

use bytes::{BufMut, BytesMut};
use vauban_types::{Date, SqlType, Time, TypeInfo, Value};

use super::{
    BITNTYPE, BITTYPE, DATENTYPE, DATETIM4TYPE, DATETIME2NTYPE, DATETIMEOFFSETNTYPE, DATETIMETYPE,
    DATETIMNTYPE, DECIMALNTYPE, FLT4TYPE, FLT8TYPE, FLTNTYPE, GUIDTYPE, INT1TYPE, INT2TYPE,
    INT4TYPE, INT8TYPE, INTNTYPE, MONEY4TYPE, MONEYNTYPE, MONEYTYPE, NUMERICNTYPE, TIMENTYPE,
};
use crate::error::TdsError;

/// Ticks of 1/300 s in one minute, for the `datetime` → `smalldatetime` conversion.
const TICKS_300TH_PER_MINUTE: u32 = 300 * 60;

/// How the TYPE_INFO and the values of a type travel ([MS-TDS] 2.2.5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// Fixed-length type: the TYPE_INFO is the token alone, values have no length prefix.
    Fixed(u8),
    /// BYTELEN type whose TYPE_INFO is the token then the maximum length.
    ByteLen {
        /// Type token.
        token: u8,
        /// Maximum length of a value, also the actual length of every non-NULL value.
        max_len: u8,
    },
    /// DECIMALNTYPE or NUMERICNTYPE: token, length, precision, scale.
    Decimal {
        /// Type token: DECIMALNTYPE for `decimal(p, s)`, NUMERICNTYPE for `numeric(p, s)`.
        token: u8,
        /// Announced length: 5, 9, 13 or 17.
        len: u8,
        /// Declared precision.
        precision: u8,
        /// Declared scale.
        scale: u8,
    },
    /// DATENTYPE: token alone in the TYPE_INFO, yet the values are length-prefixed.
    Date,
    /// TIMENTYPE, DATETIME2NTYPE, DATETIMEOFFSETNTYPE: token then scale.
    Scaled {
        /// Type token.
        token: u8,
        /// Declared fractional-seconds scale, `0..=7`.
        scale: u8,
    },
}

/// Chooses the wire form of `ti` and validates the parameters that shape the TYPE_INFO
/// (decimal precision, fractional-seconds scale).
fn wire(ti: &TypeInfo) -> Result<Wire, TdsError> {
    let two_forms = |fixed: u8, n_token: u8, len: u8| {
        if ti.nullable {
            Wire::ByteLen {
                token: n_token,
                max_len: len,
            }
        } else {
            Wire::Fixed(fixed)
        }
    };
    Ok(match ti.ty {
        SqlType::Bit => two_forms(BITTYPE, BITNTYPE, 1),
        SqlType::TinyInt => two_forms(INT1TYPE, INTNTYPE, 1),
        SqlType::SmallInt => two_forms(INT2TYPE, INTNTYPE, 2),
        SqlType::Int => two_forms(INT4TYPE, INTNTYPE, 4),
        SqlType::BigInt => two_forms(INT8TYPE, INTNTYPE, 8),
        SqlType::Real => two_forms(FLT4TYPE, FLTNTYPE, 4),
        SqlType::Float => two_forms(FLT8TYPE, FLTNTYPE, 8),
        SqlType::Money => two_forms(MONEYTYPE, MONEYNTYPE, 8),
        SqlType::SmallMoney => two_forms(MONEY4TYPE, MONEYNTYPE, 4),
        SqlType::DateTime => two_forms(DATETIMETYPE, DATETIMNTYPE, 8),
        SqlType::SmallDateTime => two_forms(DATETIM4TYPE, DATETIMNTYPE, 4),
        SqlType::Decimal { precision, scale } => Wire::Decimal {
            token: DECIMALNTYPE,
            len: decimal_len(precision)?,
            precision,
            scale,
        },
        SqlType::Numeric { precision, scale } => Wire::Decimal {
            token: NUMERICNTYPE,
            len: decimal_len(precision)?,
            precision,
            scale,
        },
        SqlType::UniqueIdentifier => Wire::ByteLen {
            token: GUIDTYPE,
            max_len: 16,
        },
        SqlType::Date => Wire::Date,
        SqlType::Time(scale) => Wire::Scaled {
            token: TIMENTYPE,
            scale: checked_scale(scale)?,
        },
        SqlType::DateTime2(scale) => Wire::Scaled {
            token: DATETIME2NTYPE,
            scale: checked_scale(scale)?,
        },
        SqlType::DateTimeOffset(scale) => Wire::Scaled {
            token: DATETIMEOFFSETNTYPE,
            scale: checked_scale(scale)?,
        },
        SqlType::Char(_)
        | SqlType::VarChar(_)
        | SqlType::NChar(_)
        | SqlType::NVarChar(_)
        | SqlType::Binary(_)
        | SqlType::VarBinary(_) => {
            // `types::family` never routes these here; kept as an error rather than a panic.
            return Err(TdsError::Malformed(
                "string or binary type is not a fixed-length or BYTELEN type",
            ));
        }
    })
}

/// Appends the TYPE_INFO of `ti` to `out` ([MS-TDS] 2.2.5.6).
pub(crate) fn encode_type_info(ti: &TypeInfo, out: &mut BytesMut) -> Result<(), TdsError> {
    match wire(ti)? {
        Wire::Fixed(token) => out.put_u8(token),
        Wire::ByteLen { token, max_len } => {
            out.put_u8(token);
            out.put_u8(max_len);
        }
        Wire::Decimal {
            token,
            len,
            precision,
            scale,
        } => {
            out.put_u8(token);
            out.put_u8(len);
            out.put_u8(precision);
            out.put_u8(scale);
        }
        Wire::Date => out.put_u8(DATENTYPE),
        Wire::Scaled { token, scale } => {
            out.put_u8(token);
            out.put_u8(scale);
        }
    }
    Ok(())
}

/// Appends `value` to `out` in the layout announced by the TYPE_INFO of `ti`
/// ([MS-TDS] 2.2.5.5.1). Nothing is written when an error is returned.
pub(crate) fn encode_value(
    ti: &TypeInfo,
    value: &Value,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let wire = wire(ti)?;
    if matches!(value, Value::Null) {
        if !ti.nullable {
            return Err(TdsError::NullInNotNullable);
        }
        // `nullable == true` never selects `Wire::Fixed`, so a zero length byte is the NULL.
        out.put_u8(0);
        return Ok(());
    }
    let prefixed = !matches!(wire, Wire::Fixed(_));
    let mismatch = || TdsError::ValueTypeMismatch {
        expected: type_name(ti.ty),
    };

    match (ti.ty, value) {
        (SqlType::Bit, Value::Bit(b)) => put(out, prefixed, &[u8::from(*b)]),
        (SqlType::TinyInt, Value::I8(v)) => put(out, prefixed, &[*v]),
        (SqlType::SmallInt, Value::I16(v)) => put(out, prefixed, &v.to_le_bytes()),
        (SqlType::Int, Value::I32(v)) => put(out, prefixed, &v.to_le_bytes()),
        (SqlType::BigInt, Value::I64(v)) => put(out, prefixed, &v.to_le_bytes()),
        (SqlType::Real, Value::F32(v)) => put(out, prefixed, &v.to_le_bytes()),
        (SqlType::Float, Value::F64(v)) => put(out, prefixed, &v.to_le_bytes()),
        (SqlType::Money, Value::Money(v)) => {
            // High 32-bit word first, then low word, each little-endian.
            let le = v.to_le_bytes();
            let mut payload = [0u8; 8];
            payload[..4].copy_from_slice(&le[4..]);
            payload[4..].copy_from_slice(&le[..4]);
            put(out, prefixed, &payload);
        }
        (SqlType::SmallMoney, Value::Money(v)) => {
            let v = i32::try_from(*v).map_err(|_| mismatch())?;
            put(out, prefixed, &v.to_le_bytes());
        }
        (SqlType::DateTime, Value::DateTime(dt)) => {
            let mut payload = [0u8; 8];
            payload[..4].copy_from_slice(&dt.days.to_le_bytes());
            payload[4..].copy_from_slice(&dt.ticks_300th.to_le_bytes());
            put(out, prefixed, &payload);
        }
        (SqlType::SmallDateTime, Value::DateTime(dt)) => {
            let days = u16::try_from(dt.days).map_err(|_| mismatch())?;
            if !dt.ticks_300th.is_multiple_of(TICKS_300TH_PER_MINUTE) {
                return Err(mismatch());
            }
            let minutes =
                u16::try_from(dt.ticks_300th / TICKS_300TH_PER_MINUTE).map_err(|_| mismatch())?;
            let mut payload = [0u8; 4];
            payload[..2].copy_from_slice(&days.to_le_bytes());
            payload[2..].copy_from_slice(&minutes.to_le_bytes());
            put(out, prefixed, &payload);
        }
        (SqlType::Decimal { scale, .. } | SqlType::Numeric { scale, .. }, Value::Decimal(d)) => {
            let Wire::Decimal { len, .. } = wire else {
                return Err(mismatch());
            };
            if d.scale != scale {
                return Err(mismatch());
            }
            let n = usize::from(len - 1);
            let magnitude = d.mantissa.unsigned_abs().to_le_bytes();
            if magnitude[n..].iter().any(|b| *b != 0) {
                return Err(mismatch());
            }
            let mut payload = [0u8; 17];
            payload[0] = u8::from(d.mantissa >= 0);
            payload[1..=n].copy_from_slice(&magnitude[..n]);
            put(out, true, &payload[..=n]);
        }
        (SqlType::UniqueIdentifier, Value::Guid(g)) => put(out, true, g),
        (SqlType::Date, Value::Date(d)) => put(out, true, &date_bytes(d, mismatch)?),
        (SqlType::Time(scale), Value::Time(t)) => {
            let (bytes, n) = time_bytes(t, scale, mismatch)?;
            put(out, true, &bytes[..n]);
        }
        (SqlType::DateTime2(scale), Value::DateTime2(dt)) => {
            let (time, n) = time_bytes(&dt.time, scale, mismatch)?;
            let date = date_bytes(&dt.date, mismatch)?;
            let mut payload = [0u8; 8];
            payload[..n].copy_from_slice(&time[..n]);
            payload[n..n + 3].copy_from_slice(&date);
            put(out, true, &payload[..n + 3]);
        }
        (SqlType::DateTimeOffset(scale), Value::DateTimeOffset(dto)) => {
            let (time, n) = time_bytes(&dto.utc.time, scale, mismatch)?;
            let date = date_bytes(&dto.utc.date, mismatch)?;
            let mut payload = [0u8; 10];
            payload[..n].copy_from_slice(&time[..n]);
            payload[n..n + 3].copy_from_slice(&date);
            payload[n + 3..n + 5].copy_from_slice(&dto.offset_minutes.to_le_bytes());
            put(out, true, &payload[..n + 5]);
        }
        _ => return Err(mismatch()),
    }
    Ok(())
}

/// Writes `payload`, preceded by its length on one byte when the wire form is BYTELEN.
fn put(out: &mut BytesMut, prefixed: bool, payload: &[u8]) {
    if prefixed {
        // Every payload of this module is at most 17 bytes: the cast cannot truncate.
        out.put_u8(payload.len() as u8);
    }
    out.put_slice(payload);
}

/// Announced length of a `decimal(p, _)` value: sign byte plus 4, 8, 12 or 16 bytes of
/// magnitude ([MS-TDS] 2.2.5.5.1, DECIMALNTYPE).
fn decimal_len(precision: u8) -> Result<u8, TdsError> {
    match precision {
        1..=9 => Ok(5),
        10..=19 => Ok(9),
        20..=28 => Ok(13),
        29..=38 => Ok(17),
        _ => Err(TdsError::Malformed(
            "TYPE_INFO decimal precision out of range 1..=38",
        )),
    }
}

/// Validates a fractional-seconds scale.
fn checked_scale(scale: u8) -> Result<u8, TdsError> {
    if scale <= 7 {
        Ok(scale)
    } else {
        Err(TdsError::Malformed(
            "TYPE_INFO fractional-seconds scale out of range 0..=7",
        ))
    }
}

/// Number of bytes of a `time(s)` value: 3 for `s <= 2`, 4 for `s <= 4`, 5 beyond.
fn time_len(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

/// The `date` payload: the day count on 3 little-endian bytes.
fn date_bytes(d: &Date, mismatch: impl Fn() -> TdsError) -> Result<[u8; 3], TdsError> {
    let days = u32::try_from(d.days)
        .ok()
        .filter(|days| *days <= 0x00FF_FFFF)
        .ok_or_else(mismatch)?;
    let le = days.to_le_bytes();
    Ok([le[0], le[1], le[2]])
}

/// The `time(scale)` payload: the count of `10^-scale` s units, little-endian, in the first
/// `time_len(scale)` bytes of the returned array. `scale` has been validated by [`wire`].
fn time_bytes(
    t: &Time,
    scale: u8,
    mismatch: impl Fn() -> TdsError,
) -> Result<([u8; 8], usize), TdsError> {
    let dropped_digits = 7u8.checked_sub(scale).ok_or_else(|| {
        TdsError::Malformed("TYPE_INFO fractional-seconds scale out of range 0..=7")
    })?;
    let divisor = 10u64.pow(u32::from(dropped_digits));
    if !t.ticks_100ns.is_multiple_of(divisor) {
        return Err(mismatch());
    }
    let n = time_len(scale);
    let bytes = (t.ticks_100ns / divisor).to_le_bytes();
    if bytes[n..].iter().any(|b| *b != 0) {
        return Err(mismatch());
    }
    Ok((bytes, n))
}

/// SQL name of a type, for `ValueTypeMismatch::expected`.
fn type_name(ty: SqlType) -> &'static str {
    match ty {
        SqlType::Bit => "bit",
        SqlType::TinyInt => "tinyint",
        SqlType::SmallInt => "smallint",
        SqlType::Int => "int",
        SqlType::BigInt => "bigint",
        SqlType::Decimal { .. } => "decimal",
        SqlType::Numeric { .. } => "numeric",
        SqlType::Float => "float",
        SqlType::Real => "real",
        SqlType::Money => "money",
        SqlType::SmallMoney => "smallmoney",
        SqlType::Char(_) => "char",
        SqlType::VarChar(_) => "varchar",
        SqlType::NChar(_) => "nchar",
        SqlType::NVarChar(_) => "nvarchar",
        SqlType::Binary(_) => "binary",
        SqlType::VarBinary(_) => "varbinary",
        SqlType::Date => "date",
        SqlType::Time(_) => "time",
        SqlType::DateTime => "datetime",
        SqlType::SmallDateTime => "smalldatetime",
        SqlType::DateTime2(_) => "datetime2",
        SqlType::DateTimeOffset(_) => "datetimeoffset",
        SqlType::UniqueIdentifier => "uniqueidentifier",
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use vauban_types::{
        Date, DateTime, DateTime2, DateTimeOffset, Decimal, SqlType, Time, TypeInfo, Value,
    };

    use crate::error::TdsError;
    // Through the dispatch of `types/mod.rs`, so that the routing is covered too.
    use crate::types::{encode_type_info, encode_value};

    fn ti(ty: SqlType, nullable: bool) -> TypeInfo {
        TypeInfo::new(ty, nullable)
    }

    /// Parses `"26 04"` into bytes.
    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    fn type_info(ti: &TypeInfo) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode_type_info(ti, &mut out).unwrap();
        out.to_vec()
    }

    fn value(ti: &TypeInfo, v: &Value) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode_value(ti, v, &mut out).unwrap();
        out.to_vec()
    }

    fn value_err(ti: &TypeInfo, v: &Value) -> TdsError {
        let mut out = BytesMut::new();
        let err = encode_value(ti, v, &mut out).unwrap_err();
        assert!(out.is_empty(), "nothing must be written on error");
        err
    }

    fn assert_mismatch(ti: &TypeInfo, v: &Value, expected: &str) {
        match value_err(ti, v) {
            TdsError::ValueTypeMismatch { expected: got } => assert_eq!(got, expected),
            other => panic!("expected ValueTypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn int_not_null_is_fixed() {
        let t = ti(SqlType::Int, false);
        assert_eq!(type_info(&t), hex("38"));
        assert_eq!(value(&t, &Value::I32(1)), hex("01 00 00 00"));
        assert_eq!(value(&t, &Value::I32(-1)), hex("FF FF FF FF"));
    }

    #[test]
    fn int_nullable_is_intn() {
        let t = ti(SqlType::Int, true);
        assert_eq!(type_info(&t), hex("26 04"));
        assert_eq!(value(&t, &Value::I32(1)), hex("04 01 00 00 00"));
        assert_eq!(value(&t, &Value::Null), hex("00"));
    }

    #[test]
    fn integer_family() {
        assert_eq!(type_info(&ti(SqlType::TinyInt, false)), hex("30"));
        assert_eq!(type_info(&ti(SqlType::SmallInt, false)), hex("34"));
        assert_eq!(type_info(&ti(SqlType::BigInt, false)), hex("7F"));
        assert_eq!(type_info(&ti(SqlType::TinyInt, true)), hex("26 01"));
        assert_eq!(type_info(&ti(SqlType::SmallInt, true)), hex("26 02"));
        assert_eq!(type_info(&ti(SqlType::BigInt, true)), hex("26 08"));

        assert_eq!(
            value(&ti(SqlType::TinyInt, false), &Value::I8(255)),
            hex("FF")
        );
        assert_eq!(
            value(&ti(SqlType::TinyInt, true), &Value::I8(7)),
            hex("01 07")
        );
        assert_eq!(
            value(&ti(SqlType::SmallInt, false), &Value::I16(-2)),
            hex("FE FF")
        );
        assert_eq!(
            value(&ti(SqlType::SmallInt, true), &Value::I16(0x1234)),
            hex("02 34 12")
        );
        assert_eq!(
            value(&ti(SqlType::BigInt, false), &Value::I64(1)),
            hex("01 00 00 00 00 00 00 00")
        );
        assert_eq!(
            value(&ti(SqlType::BigInt, true), &Value::I64(-1)),
            hex("08 FF FF FF FF FF FF FF FF")
        );

        assert_eq!(type_info(&ti(SqlType::Bit, false)), hex("32"));
        assert_eq!(type_info(&ti(SqlType::Bit, true)), hex("68 01"));
        assert_eq!(
            value(&ti(SqlType::Bit, false), &Value::Bit(true)),
            hex("01")
        );
        assert_eq!(
            value(&ti(SqlType::Bit, false), &Value::Bit(false)),
            hex("00")
        );
        assert_eq!(
            value(&ti(SqlType::Bit, true), &Value::Bit(true)),
            hex("01 01")
        );
        assert_eq!(
            value(&ti(SqlType::Bit, true), &Value::Bit(false)),
            hex("01 00")
        );
        assert_eq!(value(&ti(SqlType::Bit, true), &Value::Null), hex("00"));
    }

    #[test]
    fn float_family() {
        assert_eq!(type_info(&ti(SqlType::Float, false)), hex("3E"));
        assert_eq!(type_info(&ti(SqlType::Float, true)), hex("6D 08"));
        assert_eq!(type_info(&ti(SqlType::Real, false)), hex("3B"));
        assert_eq!(type_info(&ti(SqlType::Real, true)), hex("6D 04"));

        assert_eq!(
            value(&ti(SqlType::Float, false), &Value::F64(1.5)),
            hex("00 00 00 00 00 00 F8 3F")
        );
        assert_eq!(
            value(&ti(SqlType::Float, true), &Value::F64(1.5)),
            hex("08 00 00 00 00 00 00 F8 3F")
        );
        assert_eq!(
            value(&ti(SqlType::Real, false), &Value::F32(1.5)),
            hex("00 00 C0 3F")
        );
        assert_eq!(
            value(&ti(SqlType::Real, true), &Value::F32(-2.0)),
            hex("04 00 00 00 C0")
        );
        assert_eq!(value(&ti(SqlType::Real, true), &Value::Null), hex("00"));
    }

    #[test]
    fn money_family() {
        let money = ti(SqlType::Money, false);
        assert_eq!(type_info(&money), hex("3C"));
        assert_eq!(
            value(&money, &Value::Money(15000)),
            hex("00 00 00 00 98 3A 00 00")
        );
        // -1.5000: two's complement, high word FFFFFFFF then low word FFFFC568.
        assert_eq!(
            value(&money, &Value::Money(-15000)),
            hex("FF FF FF FF 68 C5 FF FF")
        );
        // An amount above 2^32 ten-thousandths uses the high word.
        assert_eq!(
            value(&money, &Value::Money(0x0000_0001_0000_0002)),
            hex("01 00 00 00 02 00 00 00")
        );

        let small = ti(SqlType::SmallMoney, false);
        assert_eq!(type_info(&small), hex("7A"));
        assert_eq!(value(&small, &Value::Money(15000)), hex("98 3A 00 00"));

        assert_eq!(type_info(&ti(SqlType::Money, true)), hex("6E 08"));
        assert_eq!(type_info(&ti(SqlType::SmallMoney, true)), hex("6E 04"));
        assert_eq!(
            value(&ti(SqlType::Money, true), &Value::Money(15000)),
            hex("08 00 00 00 00 98 3A 00 00")
        );
        assert_eq!(
            value(&ti(SqlType::SmallMoney, true), &Value::Money(15000)),
            hex("04 98 3A 00 00")
        );
        assert_eq!(value(&ti(SqlType::Money, true), &Value::Null), hex("00"));

        // Beyond smallmoney: no truncation.
        assert_mismatch(&small, &Value::Money(i64::from(i32::MAX) + 1), "smallmoney");
    }

    #[test]
    fn datetime_family() {
        let dt = ti(SqlType::DateTime, false);
        assert_eq!(type_info(&dt), hex("3D"));
        assert_eq!(
            value(
                &dt,
                &Value::DateTime(DateTime {
                    days: 1,
                    ticks_300th: 0
                })
            ),
            hex("01 00 00 00 00 00 00 00")
        );
        assert_eq!(
            value(
                &dt,
                &Value::DateTime(DateTime {
                    days: -1,
                    ticks_300th: 300
                })
            ),
            hex("FF FF FF FF 2C 01 00 00")
        );

        let sdt = ti(SqlType::SmallDateTime, false);
        assert_eq!(type_info(&sdt), hex("3A"));
        assert_eq!(
            value(
                &sdt,
                &Value::DateTime(DateTime {
                    days: 1,
                    ticks_300th: 18000
                })
            ),
            hex("01 00 01 00")
        );

        assert_eq!(type_info(&ti(SqlType::DateTime, true)), hex("6F 08"));
        assert_eq!(type_info(&ti(SqlType::SmallDateTime, true)), hex("6F 04"));
        assert_eq!(
            value(
                &ti(SqlType::SmallDateTime, true),
                &Value::DateTime(DateTime {
                    days: 1,
                    ticks_300th: 18000
                })
            ),
            hex("04 01 00 01 00")
        );
        assert_eq!(value(&ti(SqlType::DateTime, true), &Value::Null), hex("00"));

        // smalldatetime cannot hold seconds nor dates before 1900-01-01: no rounding.
        assert_mismatch(
            &sdt,
            &Value::DateTime(DateTime {
                days: 1,
                ticks_300th: 18001,
            }),
            "smalldatetime",
        );
        assert_mismatch(
            &sdt,
            &Value::DateTime(DateTime {
                days: -1,
                ticks_300th: 0,
            }),
            "smalldatetime",
        );
    }

    #[test]
    fn decimal_vectors() {
        let dec = |precision, scale| SqlType::Decimal { precision, scale };
        let val = |mantissa, precision, scale| {
            Value::Decimal(Decimal {
                mantissa,
                precision,
                scale,
            })
        };

        for nullable in [false, true] {
            let t = ti(dec(5, 2), nullable);
            assert_eq!(type_info(&t), hex("6A 05 05 02"));
            assert_eq!(value(&t, &val(12345, 5, 2)), hex("05 01 39 30 00 00"));

            let t = ti(dec(3, 2), nullable);
            assert_eq!(type_info(&t), hex("6A 05 03 02"));
            assert_eq!(value(&t, &val(-100, 3, 2)), hex("05 00 64 00 00 00"));
        }

        assert_eq!(type_info(&ti(dec(10, 0), true)), hex("6A 09 0A 00"));
        assert_eq!(type_info(&ti(dec(20, 4), true)), hex("6A 0D 14 04"));
        assert_eq!(type_info(&ti(dec(29, 0), true)), hex("6A 11 1D 00"));
        assert_eq!(type_info(&ti(dec(38, 38), true)), hex("6A 11 26 26"));

        assert_eq!(
            value(&ti(dec(10, 0), true), &val(1 << 32, 10, 0)),
            hex("09 01 00 00 00 00 01 00 00 00")
        );
        assert_eq!(
            value(&ti(dec(38, 0), true), &val(-1, 38, 0)),
            hex("11 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00")
        );
        // Zero is written with the positive sign.
        assert_eq!(
            value(&ti(dec(5, 2), true), &val(0, 5, 2)),
            hex("05 01 00 00 00 00")
        );
        assert_eq!(value(&ti(dec(5, 2), true), &Value::Null), hex("00"));

        // Another scale or a magnitude wider than the announced length: no conversion.
        assert_mismatch(&ti(dec(5, 2), true), &val(12345, 5, 1), "decimal");
        assert_mismatch(&ti(dec(9, 0), true), &val(1 << 32, 10, 0), "decimal");

        // Precision outside 1..=38 cannot be announced.
        let mut out = BytesMut::new();
        assert!(matches!(
            encode_type_info(&ti(dec(0, 0), true), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(matches!(
            encode_type_info(&ti(dec(39, 0), true), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
    }

    /// `numeric(p, s)` travels as NUMERICNTYPE (0x6C), `decimal(p, s)` as DECIMALNTYPE
    /// (0x6A); everything else is the same layout ([MS-TDS] 2.2.5.4.2).
    #[test]
    fn numeric_vectors() {
        let num = |precision, scale| SqlType::Numeric { precision, scale };
        let dec = |precision, scale| SqlType::Decimal { precision, scale };
        let val = |mantissa, precision, scale| {
            Value::Decimal(Decimal {
                mantissa,
                precision,
                scale,
            })
        };

        // The literal `1.5` is typed `numeric(2, 1)`.
        for nullable in [false, true] {
            assert_eq!(type_info(&ti(num(2, 1), nullable)), hex("6C 05 02 01"));
            assert_eq!(type_info(&ti(dec(2, 1), nullable)), hex("6A 05 02 01"));
        }

        // Only the token differs: same length, precision, scale and value bytes.
        for (precision, scale, mantissa) in [(5u8, 2u8, 12345i128), (20, 4, -1), (38, 0, 1 << 32)] {
            let n = ti(num(precision, scale), true);
            let d = ti(dec(precision, scale), true);
            let mut n_info = type_info(&n);
            let d_info = type_info(&d);
            assert_eq!(n_info[0], 0x6C);
            assert_eq!(d_info[0], 0x6A);
            n_info[0] = 0x6A;
            assert_eq!(n_info, d_info);
            let v = val(mantissa, precision, scale);
            assert_eq!(value(&n, &v), value(&d, &v));
        }

        assert_eq!(value(&ti(num(2, 1), true), &Value::Null), hex("00"));

        // A mismatch names `numeric`, not `decimal`.
        assert_mismatch(&ti(num(5, 2), true), &val(12345, 5, 1), "numeric");
        assert_mismatch(&ti(num(5, 2), false), &Value::I32(1), "numeric");

        // Precision outside 1..=38 cannot be announced, as for `decimal`.
        let mut out = BytesMut::new();
        assert!(matches!(
            encode_type_info(&ti(num(39, 0), true), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn guid_vectors() {
        let raw: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ];
        for nullable in [false, true] {
            let t = ti(SqlType::UniqueIdentifier, nullable);
            assert_eq!(type_info(&t), hex("24 10"));
            let mut expected = vec![0x10];
            expected.extend_from_slice(&raw);
            assert_eq!(value(&t, &Value::Guid(raw)), expected);
        }
        assert_eq!(
            value(&ti(SqlType::UniqueIdentifier, true), &Value::Null),
            hex("00")
        );
    }

    #[test]
    fn date_vectors() {
        // 2000-01-01 is day 730_119 = 0x0B2407 (1999 * 365 + 484 leap days), hence 07 24 0B.
        let day = Value::Date(Date { days: 730_119 });
        for nullable in [false, true] {
            let t = ti(SqlType::Date, nullable);
            assert_eq!(type_info(&t), hex("28"));
            assert_eq!(value(&t, &day), hex("03 07 24 0B"));
        }
        assert_eq!(value(&ti(SqlType::Date, true), &Value::Null), hex("00"));
        assert_eq!(
            value(&ti(SqlType::Date, true), &Value::Date(Date { days: 0 })),
            hex("03 00 00 00")
        );

        // Outside the 3-byte range: refused, not wrapped.
        assert_mismatch(
            &ti(SqlType::Date, true),
            &Value::Date(Date { days: -1 }),
            "date",
        );
        assert_mismatch(
            &ti(SqlType::Date, true),
            &Value::Date(Date { days: 0x0100_0000 }),
            "date",
        );
    }

    #[test]
    fn time_vectors() {
        let one_second = Value::Time(Time {
            ticks_100ns: 10_000_000,
        });
        for nullable in [false, true] {
            let t7 = ti(SqlType::Time(7), nullable);
            assert_eq!(type_info(&t7), hex("29 07"));
            assert_eq!(value(&t7, &one_second), hex("05 80 96 98 00 00"));
        }
        let t0 = ti(SqlType::Time(0), true);
        assert_eq!(type_info(&t0), hex("29 00"));
        assert_eq!(value(&t0, &one_second), hex("03 01 00 00"));

        let t3 = ti(SqlType::Time(3), true);
        assert_eq!(type_info(&t3), hex("29 03"));
        assert_eq!(value(&t3, &one_second), hex("04 E8 03 00 00"));

        // Byte count per scale: 3 up to 2, 4 up to 4, 5 beyond.
        assert_eq!(value(&ti(SqlType::Time(2), true), &one_second).len(), 4);
        assert_eq!(value(&ti(SqlType::Time(4), true), &one_second).len(), 5);
        assert_eq!(value(&ti(SqlType::Time(5), true), &one_second).len(), 6);

        // 23:59:59.9999999 fits in 5 bytes.
        assert_eq!(
            value(
                &ti(SqlType::Time(7), true),
                &Value::Time(Time {
                    ticks_100ns: 863_999_999_999
                })
            ),
            hex("05 FF BF 69 2A C9")
        );
        assert_eq!(value(&t3, &Value::Null), hex("00"));

        // Digits beyond the scale are not rounded away.
        assert_mismatch(
            &t3,
            &Value::Time(Time {
                ticks_100ns: 10_000_001,
            }),
            "time",
        );
        // A count that does not fit the byte width is refused.
        assert_mismatch(
            &t0,
            &Value::Time(Time {
                ticks_100ns: 0x0100_0000 * 10_000_000,
            }),
            "time",
        );

        // Scale outside 0..=7 cannot be announced.
        let mut out = BytesMut::new();
        assert!(matches!(
            encode_type_info(&ti(SqlType::Time(8), true), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(matches!(
            encode_value(&ti(SqlType::DateTime2(8), true), &Value::Null, &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn datetime2_and_offset_vectors() {
        let instant = DateTime2 {
            date: Date { days: 730_119 },
            time: Time {
                ticks_100ns: 10_000_000,
            },
        };
        for nullable in [false, true] {
            let t = ti(SqlType::DateTime2(7), nullable);
            assert_eq!(type_info(&t), hex("2A 07"));
            assert_eq!(
                value(&t, &Value::DateTime2(instant)),
                hex("08 80 96 98 00 00 07 24 0B")
            );

            let t = ti(SqlType::DateTimeOffset(7), nullable);
            assert_eq!(type_info(&t), hex("2B 07"));
            assert_eq!(
                value(
                    &t,
                    &Value::DateTimeOffset(DateTimeOffset {
                        utc: instant,
                        offset_minutes: 0
                    })
                ),
                hex("0A 80 96 98 00 00 07 24 0B 00 00")
            );
        }

        assert_eq!(
            value(&ti(SqlType::DateTime2(0), true), &Value::DateTime2(instant)),
            hex("06 01 00 00 07 24 0B")
        );
        assert_eq!(
            value(
                &ti(SqlType::DateTimeOffset(3), true),
                &Value::DateTimeOffset(DateTimeOffset {
                    utc: instant,
                    offset_minutes: -120
                })
            ),
            hex("09 E8 03 00 00 07 24 0B 88 FF")
        );
        assert_eq!(
            value(&ti(SqlType::DateTime2(7), true), &Value::Null),
            hex("00")
        );
        assert_eq!(
            value(&ti(SqlType::DateTimeOffset(7), true), &Value::Null),
            hex("00")
        );

        assert_mismatch(
            &ti(SqlType::DateTime2(0), true),
            &Value::DateTime2(DateTime2 {
                date: Date { days: 1 },
                time: Time { ticks_100ns: 1 },
            }),
            "datetime2",
        );
        assert_mismatch(
            &ti(SqlType::DateTimeOffset(7), true),
            &Value::DateTimeOffset(DateTimeOffset {
                utc: DateTime2 {
                    date: Date { days: -1 },
                    time: Time { ticks_100ns: 0 },
                },
                offset_minutes: 0,
            }),
            "datetimeoffset",
        );
    }

    #[test]
    fn null_in_not_nullable_is_error() {
        assert!(matches!(
            value_err(&ti(SqlType::Int, false), &Value::Null),
            TdsError::NullInNotNullable
        ));
        // Also for the types that only have an N form on the wire.
        assert!(matches!(
            value_err(&ti(SqlType::UniqueIdentifier, false), &Value::Null),
            TdsError::NullInNotNullable
        ));
        assert!(matches!(
            value_err(
                &ti(
                    SqlType::Decimal {
                        precision: 5,
                        scale: 2
                    },
                    false
                ),
                &Value::Null
            ),
            TdsError::NullInNotNullable
        ));
        assert!(matches!(
            value_err(&ti(SqlType::Date, false), &Value::Null),
            TdsError::NullInNotNullable
        ));
    }

    #[test]
    fn value_type_mismatch() {
        assert_mismatch(&ti(SqlType::Int, true), &Value::Bit(true), "int");
        assert_mismatch(&ti(SqlType::Int, false), &Value::I64(1), "int");
        assert_mismatch(&ti(SqlType::TinyInt, false), &Value::I16(1), "tinyint");
        assert_mismatch(&ti(SqlType::Float, false), &Value::F32(1.0), "float");
        assert_mismatch(&ti(SqlType::Money, true), &Value::I64(1), "money");
        assert_mismatch(
            &ti(SqlType::DateTime, true),
            &Value::Date(Date { days: 1 }),
            "datetime",
        );
        assert_mismatch(
            &ti(SqlType::UniqueIdentifier, true),
            &Value::Bytes(vec![0; 16]),
            "uniqueidentifier",
        );
    }
}
