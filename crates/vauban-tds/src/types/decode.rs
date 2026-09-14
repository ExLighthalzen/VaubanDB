//! Decoding of a TYPE_INFO and of a typed value sent by a client, as they appear in the
//! parameters of an RPC request ([MS-TDS] 2.2.6.6 RPC Request, ParameterData / TYPE_VARBYTE).
//!
//! This is the exact inverse of [`super::encode_type_info`] and [`super::encode_value`]:
//! everything the encoders write is read back to the same `TypeInfo` / `Value`, plus the
//! forms only clients send (PLP of unknown length, legacy DECIMALTYPE / NUMERICTYPE tokens).
//!
//! # TYPE_INFO ([MS-TDS] 2.2.5.4, 2.2.5.6 Type Info Rule Definition)
//!
//! - fixed-length tokens ([MS-TDS] 2.2.5.4.1): the token alone; `nullable = false`;
//! - BYTELEN tokens ([MS-TDS] 2.2.5.4.2): the token then one byte of maximum length, which
//!   selects the type for INTNTYPE (1, 2, 4, 8), FLTNTYPE (4, 8), MONEYNTYPE (4, 8) and
//!   DATETIMNTYPE (4, 8); DECIMALNTYPE / NUMERICNTYPE (and the legacy DECIMALTYPE /
//!   NUMERICTYPE) carry length, precision and scale; DATENTYPE carries nothing; TIMENTYPE,
//!   DATETIME2NTYPE and DATETIMEOFFSETNTYPE carry the scale; `nullable = true`;
//! - USHORTLEN tokens ([MS-TDS] 2.2.5.4.2): the token, the maximum length in bytes on two
//!   bytes, then the 5-byte `Collation` rule for the four character types; a maximum length
//!   of `0xFFFF` announces the PLP form ([MS-TDS] 2.2.5.4.3) of `varchar`, `nvarchar` and
//!   `varbinary`; `nullable = true`.
//!
//! A TYPE_INFO does not carry the nullability of a parameter, only its wire form: `nullable`
//! is deduced from the form, as described above.
//!
//! # Values ([MS-TDS] 2.2.5.5.1 Data Type Dependent Data Streams, 2.2.5.2.3 PLP Bytes)
//!
//! The layout is the one [`super::encode_value`] would announce for the same `TypeInfo`:
//! no length prefix for a fixed token (`nullable == false` on a type that has a fixed form),
//! one length byte for the BYTELEN types (`0` is NULL), two length bytes for the USHORTLEN
//! types (`0xFFFF` is NULL, `0` is an empty value), and a PLP_BODY for the `(max)` types:
//! PLP_NULL, or a total length (or UNKNOWN_PLP_LEN) then chunks up to the zero terminator.
//! `char` / `varchar` are read from code page 1252 (the inverse of [`super::cp1252`]),
//! `nchar` / `nvarchar` from UTF-16LE.
//!
//! # Errors and cursor
//!
//! A truncated or inconsistent stream (a BYTELEN value whose length differs from the one the
//! TYPE_INFO announces, a PLP chunk longer than the remaining bytes, a decimal precision
//! above 38, a scale above 7…) fails with `Malformed`; the types this version does not decode
//! (`text`, `ntext`, `image`, `xml`, `sql_variant`, UDT, table-valued parameters, the
//! pre-TDS 7 short forms of `char` / `binary`, and the untyped NULLTYPE) fail with
//! `Unsupported`. The functions never panic on any input. The cursor advances past the bytes
//! consumed **only on success**; after an error its position is unspecified and the caller
//! must abandon the whole message.
//!
//! No semantic validation happens on values: a `decimal` whose magnitude exceeds its precision,
//! a `date` beyond 9999-12-31 or a `time` past midnight are returned as they are.

use vauban_types::{
    Collation, Date, DateTime, DateTime2, DateTimeOffset, Decimal, Len, SqlString, SqlType, Time,
    TypeInfo, Value,
};

use super::collation::COLLATION_LEN;
use super::cp1252::CP1252_80_9F;
use super::plp::{PLP_MAX_LEN, PLP_MAX_VALUE_LEN, PLP_NULL, PLP_TERMINATOR, UNKNOWN_PLP_LEN};
use super::ushortlen::{CHARBIN_NULL, Encoding, MAX_BYTE_LEN, MAX_UNICODE_LEN};
use super::{
    BIGBINARYTYPE, BIGCHARTYPE, BIGVARBINTYPE, BIGVARCHRTYPE, BITNTYPE, BITTYPE, DATENTYPE,
    DATETIM4TYPE, DATETIME2NTYPE, DATETIMEOFFSETNTYPE, DATETIMETYPE, DATETIMNTYPE, DECIMALNTYPE,
    FLT4TYPE, FLT8TYPE, FLTNTYPE, GUIDTYPE, INT1TYPE, INT2TYPE, INT4TYPE, INT8TYPE, INTNTYPE,
    MONEY4TYPE, MONEYNTYPE, MONEYTYPE, NCHARTYPE, NULLTYPE, NUMERICNTYPE, NVARCHARTYPE, TIMENTYPE,
};
use crate::error::TdsError;

// ---- Tokens that only reach this decoder ([MS-TDS] 2.2.5.4.2, 2.2.5.4.3) ----

/// [MS-TDS] 2.2.5.4.2 DECIMALTYPE: legacy `decimal`, same TYPE_INFO layout as DECIMALNTYPE.
const DECIMALTYPE: u8 = 0x37;
/// [MS-TDS] 2.2.5.4.2 NUMERICTYPE: legacy `numeric`, same TYPE_INFO layout as NUMERICNTYPE.
const NUMERICTYPE: u8 = 0x3F;
/// [MS-TDS] 2.2.5.4.2 VARBINARYTYPE: pre-TDS 7 `varbinary`, BYTELEN form.
const VARBINARYTYPE: u8 = 0x25;
/// [MS-TDS] 2.2.5.4.2 VARCHARTYPE: pre-TDS 7 `varchar`, BYTELEN form.
const VARCHARTYPE: u8 = 0x27;
/// [MS-TDS] 2.2.5.4.2 BINARYTYPE: pre-TDS 7 `binary`, BYTELEN form.
const BINARYTYPE: u8 = 0x2D;
/// [MS-TDS] 2.2.5.4.2 CHARTYPE: pre-TDS 7 `char`, BYTELEN form.
const CHARTYPE: u8 = 0x2F;
/// [MS-TDS] 2.2.5.4.3 IMAGETYPE (`image`).
const IMAGETYPE: u8 = 0x22;
/// [MS-TDS] 2.2.5.4.3 TEXTTYPE (`text`).
const TEXTTYPE: u8 = 0x23;
/// [MS-TDS] 2.2.5.4.2 SSVARIANTTYPE (`sql_variant`).
const SSVARIANTTYPE: u8 = 0x62;
/// [MS-TDS] 2.2.5.4.3 NTEXTTYPE (`ntext`).
const NTEXTTYPE: u8 = 0x63;
/// [MS-TDS] 2.2.5.4.3 UDTTYPE (CLR user-defined type).
const UDTTYPE: u8 = 0xF0;
/// [MS-TDS] 2.2.5.4.3 XMLTYPE (`xml`).
const XMLTYPE: u8 = 0xF1;
/// [MS-TDS] 2.2.5.5.5.1 TVPTYPE (table-valued parameter).
const TVPTYPE: u8 = 0xF3;

/// Largest buffer preallocated from the total length a PLP value announces: a hostile
/// client must not make the server allocate gigabytes before sending a single chunk.
const PLP_PREALLOC_CAP: usize = 1 << 20;

/// Ticks of 1/300 s in one minute, for the `smalldatetime` → `DateTime` conversion.
const TICKS_300TH_PER_MINUTE: u32 = 300 * 60;

/// Reads a TYPE_INFO ([MS-TDS] 2.2.5.6) from the front of `input` and advances it.
///
/// See the module documentation for the forms accepted and how `nullable` is deduced.
/// On error the position of `input` is unspecified.
// Read by the RPC decoder; kept even when nothing in this build calls it.
#[allow(dead_code)]
pub(crate) fn decode_type_info(input: &mut &[u8]) -> Result<TypeInfo, TdsError> {
    let token = read_u8(input, "TYPE_INFO type token")?;
    let fixed = |ty| Ok(TypeInfo::new(ty, false));
    let nullable = |ty| Ok(TypeInfo::new(ty, true));
    match token {
        // [MS-TDS] 2.2.5.4.1: the token alone, values never NULL.
        INT1TYPE => fixed(SqlType::TinyInt),
        BITTYPE => fixed(SqlType::Bit),
        INT2TYPE => fixed(SqlType::SmallInt),
        INT4TYPE => fixed(SqlType::Int),
        DATETIM4TYPE => fixed(SqlType::SmallDateTime),
        FLT4TYPE => fixed(SqlType::Real),
        MONEYTYPE => fixed(SqlType::Money),
        DATETIMETYPE => fixed(SqlType::DateTime),
        FLT8TYPE => fixed(SqlType::Float),
        MONEY4TYPE => fixed(SqlType::SmallMoney),
        INT8TYPE => fixed(SqlType::BigInt),
        // NULLTYPE is a zero-length fixed type: its value takes no byte at all. `SqlType`
        // has no variant for an untyped NULL, and mapping it to any existing type would make
        // `decode_value` read a length byte that is not on the wire. Refused rather than
        // guessed; the RPC decoder handles NULLTYPE itself.
        NULLTYPE => Err(TdsError::Unsupported("NULLTYPE (untyped NULL parameter)")),

        // [MS-TDS] 2.2.5.4.2, BYTELEN: the maximum length selects the type.
        INTNTYPE => nullable(match read_u8(input, "TYPE_INFO INTNTYPE length")? {
            1 => SqlType::TinyInt,
            2 => SqlType::SmallInt,
            4 => SqlType::Int,
            8 => SqlType::BigInt,
            _ => {
                return Err(TdsError::Malformed(
                    "TYPE_INFO INTNTYPE length not 1, 2, 4 or 8",
                ));
            }
        }),
        BITNTYPE => match read_u8(input, "TYPE_INFO BITNTYPE length")? {
            1 => nullable(SqlType::Bit),
            _ => Err(TdsError::Malformed("TYPE_INFO BITNTYPE length not 1")),
        },
        FLTNTYPE => nullable(match read_u8(input, "TYPE_INFO FLTNTYPE length")? {
            4 => SqlType::Real,
            8 => SqlType::Float,
            _ => return Err(TdsError::Malformed("TYPE_INFO FLTNTYPE length not 4 or 8")),
        }),
        MONEYNTYPE => nullable(match read_u8(input, "TYPE_INFO MONEYNTYPE length")? {
            4 => SqlType::SmallMoney,
            8 => SqlType::Money,
            _ => {
                return Err(TdsError::Malformed(
                    "TYPE_INFO MONEYNTYPE length not 4 or 8",
                ));
            }
        }),
        DATETIMNTYPE => nullable(match read_u8(input, "TYPE_INFO DATETIMNTYPE length")? {
            4 => SqlType::SmallDateTime,
            8 => SqlType::DateTime,
            _ => {
                return Err(TdsError::Malformed(
                    "TYPE_INFO DATETIMNTYPE length not 4 or 8",
                ));
            }
        }),
        GUIDTYPE => match read_u8(input, "TYPE_INFO GUIDTYPE length")? {
            16 => nullable(SqlType::UniqueIdentifier),
            _ => Err(TdsError::Malformed("TYPE_INFO GUIDTYPE length not 16")),
        },
        DECIMALNTYPE | NUMERICNTYPE | DECIMALTYPE | NUMERICTYPE => {
            let len = read_u8(input, "TYPE_INFO decimal length")?;
            let precision = read_u8(input, "TYPE_INFO decimal precision")?;
            let scale = read_u8(input, "TYPE_INFO decimal scale")?;
            if len != decimal_len(precision)? {
                return Err(TdsError::Malformed(
                    "TYPE_INFO decimal length does not match its precision",
                ));
            }
            if scale > precision {
                return Err(TdsError::Malformed(
                    "TYPE_INFO decimal scale greater than its precision",
                ));
            }
            // Same layout for the four tokens, two names: NUMERICNTYPE is `numeric`,
            // DECIMALNTYPE is `decimal`. The N forms are told apart by their token; the
            // two pre-TDS 7.2 tokens both read as `decimal`.
            nullable(if token == NUMERICNTYPE {
                SqlType::Numeric { precision, scale }
            } else {
                SqlType::Decimal { precision, scale }
            })
        }
        DATENTYPE => nullable(SqlType::Date),
        TIMENTYPE => nullable(SqlType::Time(read_scale(input)?)),
        DATETIME2NTYPE => nullable(SqlType::DateTime2(read_scale(input)?)),
        DATETIMEOFFSETNTYPE => nullable(SqlType::DateTimeOffset(read_scale(input)?)),

        // [MS-TDS] 2.2.5.4.2 USHORTLEN and 2.2.5.4.3 PLP.
        BIGCHARTYPE | BIGVARCHRTYPE | NCHARTYPE | NVARCHARTYPE | BIGBINARYTYPE | BIGVARBINTYPE => {
            decode_string_type_info(token, input)
        }

        // Valid tokens this version refuses on purpose.
        TEXTTYPE => Err(TdsError::Unsupported("TEXTTYPE (text)")),
        NTEXTTYPE => Err(TdsError::Unsupported("NTEXTTYPE (ntext)")),
        IMAGETYPE => Err(TdsError::Unsupported("IMAGETYPE (image)")),
        XMLTYPE => Err(TdsError::Unsupported("XMLTYPE (xml)")),
        SSVARIANTTYPE => Err(TdsError::Unsupported("SSVARIANTTYPE (sql_variant)")),
        UDTTYPE => Err(TdsError::Unsupported("UDTTYPE (CLR user-defined type)")),
        TVPTYPE => Err(TdsError::Unsupported("TVPTYPE (table-valued parameter)")),
        VARBINARYTYPE => Err(TdsError::Unsupported("VARBINARYTYPE (pre-TDS 7 varbinary)")),
        VARCHARTYPE => Err(TdsError::Unsupported("VARCHARTYPE (pre-TDS 7 varchar)")),
        BINARYTYPE => Err(TdsError::Unsupported("BINARYTYPE (pre-TDS 7 binary)")),
        CHARTYPE => Err(TdsError::Unsupported("CHARTYPE (pre-TDS 7 char)")),

        _ => Err(TdsError::Malformed("TYPE_INFO unknown type token")),
    }
}

/// Reads the rest of the TYPE_INFO of a USHORTLEN or PLP type: maximum length, then the
/// collation for the character types ([MS-TDS] 2.2.5.6, rules USHORTLEN and Collation).
fn decode_string_type_info(token: u8, input: &mut &[u8]) -> Result<TypeInfo, TdsError> {
    let max_bytes = read_u16_le(input, "TYPE_INFO maximum length")?;
    let len = if max_bytes == PLP_MAX_LEN {
        Len::Max
    } else {
        Len::Fixed(max_bytes)
    };
    let ty = match (token, len) {
        (BIGVARCHRTYPE, Len::Max) => SqlType::VarChar(Len::Max),
        (NVARCHARTYPE, Len::Max) => SqlType::NVarChar(Len::Max),
        (BIGVARBINTYPE, Len::Max) => SqlType::VarBinary(Len::Max),
        (BIGCHARTYPE | NCHARTYPE | BIGBINARYTYPE, Len::Max) => {
            return Err(TdsError::Malformed(
                "TYPE_INFO char, nchar and binary have no (max) form",
            ));
        }
        (BIGCHARTYPE, Len::Fixed(n)) => SqlType::Char(Len::Fixed(checked_byte_len(n)?)),
        (BIGVARCHRTYPE, Len::Fixed(n)) => SqlType::VarChar(Len::Fixed(checked_byte_len(n)?)),
        (NCHARTYPE, Len::Fixed(n)) => SqlType::NChar(Len::Fixed(checked_unicode_len(n)?)),
        (NVARCHARTYPE, Len::Fixed(n)) => SqlType::NVarChar(Len::Fixed(checked_unicode_len(n)?)),
        (BIGBINARYTYPE, Len::Fixed(n)) => SqlType::Binary(Len::Fixed(checked_byte_len(n)?)),
        (BIGVARBINTYPE, Len::Fixed(n)) => SqlType::VarBinary(Len::Fixed(checked_byte_len(n)?)),
        // `decode_type_info` only routes the six tokens above here.
        _ => return Err(TdsError::Malformed("TYPE_INFO unknown type token")),
    };
    let collation = if ty.is_string() {
        Some(decode_collation(input)?)
    } else {
        None
    };
    Ok(TypeInfo {
        ty,
        nullable: true,
        collation,
    })
}

/// Validates the maximum length of `char(n)`, `varchar(n)`, `binary(n)`, `varbinary(n)`
/// (in bytes) and returns `n`.
fn checked_byte_len(max_bytes: u16) -> Result<u16, TdsError> {
    if max_bytes == 0 || max_bytes > MAX_BYTE_LEN {
        return Err(TdsError::Malformed(
            "TYPE_INFO char/varchar/binary/varbinary length out of range 1..=8000",
        ));
    }
    Ok(max_bytes)
}

/// Validates the maximum length of `nchar(n)` / `nvarchar(n)` (in bytes, twice `n`) and
/// returns `n`.
fn checked_unicode_len(max_bytes: u16) -> Result<u16, TdsError> {
    if max_bytes == 0 || !max_bytes.is_multiple_of(2) || max_bytes / 2 > MAX_UNICODE_LEN {
        return Err(TdsError::Malformed(
            "TYPE_INFO nchar/nvarchar length out of range 2..=8000 or odd",
        ));
    }
    Ok(max_bytes / 2)
}

/// Reads the 5-byte `Collation` rule ([MS-TDS] 2.2.5.1.2), the inverse of
/// [`super::collation::encode_collation`].
fn decode_collation(input: &mut &[u8]) -> Result<Collation, TdsError> {
    let bytes = take(input, COLLATION_LEN, "TYPE_INFO collation")?;
    let (packed, sort_id) = match bytes {
        [b0, b1, b2, b3, sort_id] => (u32::from_le_bytes([*b0, *b1, *b2, *b3]), *sort_id),
        // `take` returned exactly `COLLATION_LEN` bytes.
        _ => return Err(TdsError::Malformed("TYPE_INFO collation")),
    };
    Ok(Collation {
        lcid: packed & 0x000F_FFFF,
        // Both shifts leave at most 8 and 4 bits: the casts cannot truncate.
        flags: ((packed >> 20) & 0xFF) as u8,
        version: ((packed >> 28) & 0x0F) as u8,
        sort_id,
    })
}

/// Reads a fractional-seconds scale and validates it.
fn read_scale(input: &mut &[u8]) -> Result<u8, TdsError> {
    checked_scale(read_u8(input, "TYPE_INFO fractional-seconds scale")?)
}

/// Reads the value announced by the TYPE_INFO of `ti` from the front of `input` and
/// advances it ([MS-TDS] 2.2.5.5.1, 2.2.5.2.3).
///
/// The layout read is the one [`super::encode_value`] writes for the same `ti`, so a value
/// encoded then decoded with the same `TypeInfo` comes back equal. A NULL on the wire is
/// returned as `Value::Null` whatever `ti.nullable` says: a TYPE_INFO sent by a client does
/// not carry nullability. On error the position of `input` is unspecified.
// Read by the RPC decoder; kept even when nothing in this build calls it.
#[allow(dead_code)]
pub(crate) fn decode_value(ti: &TypeInfo, input: &mut &[u8]) -> Result<Value, TdsError> {
    match layout(ti)? {
        Layout::Fixed(len) => {
            let bytes = take(input, len, "fixed-length value")?;
            decode_scalar(ti.ty, bytes)
        }
        Layout::ByteLen(len) => {
            let actual = usize::from(read_u8(input, "BYTELEN value length")?);
            if actual == 0 {
                return Ok(Value::Null);
            }
            if actual != len {
                return Err(TdsError::Malformed(
                    "BYTELEN value length does not match the TYPE_INFO",
                ));
            }
            let bytes = take(input, actual, "BYTELEN value data")?;
            decode_scalar(ti.ty, bytes)
        }
        Layout::UShortLen {
            max_bytes,
            encoding,
        } => {
            let actual = read_u16_le(input, "USHORTLEN value length")?;
            if actual == CHARBIN_NULL {
                return Ok(Value::Null);
            }
            if actual > max_bytes {
                return Err(TdsError::Malformed(
                    "USHORTLEN value longer than the TYPE_INFO maximum length",
                ));
            }
            let bytes = take(input, usize::from(actual), "USHORTLEN value data")?;
            decode_payload(encoding, bytes)
        }
        Layout::Plp(encoding) => {
            let total = read_u64_le(input, "PLP total length")?;
            if total == PLP_NULL {
                return Ok(Value::Null);
            }
            let bytes = read_plp_chunks(input, total)?;
            decode_payload(encoding, &bytes)
        }
    }
}

/// How the value of a `TypeInfo` travels: the inverse of the `wire` choices of
/// [`super::bytelen`], [`super::ushortlen`] and [`super::plp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// No length prefix; the payload has exactly this many bytes.
    Fixed(usize),
    /// One length byte, `0` for NULL, otherwise exactly this many bytes.
    ByteLen(usize),
    /// Two length bytes, `0xFFFF` for NULL, otherwise up to `max_bytes` of data.
    UShortLen {
        /// Maximum length announced by the TYPE_INFO, in bytes.
        max_bytes: u16,
        /// Data layout.
        encoding: Encoding,
    },
    /// A PLP_BODY.
    Plp(Encoding),
}

/// Chooses the layout of the values of `ti`, validating the parameters that shape it.
fn layout(ti: &TypeInfo) -> Result<Layout, TdsError> {
    let two_forms = |len: usize| {
        if ti.nullable {
            Layout::ByteLen(len)
        } else {
            Layout::Fixed(len)
        }
    };
    Ok(match ti.ty {
        SqlType::Bit | SqlType::TinyInt => two_forms(1),
        SqlType::SmallInt => two_forms(2),
        SqlType::Int | SqlType::Real | SqlType::SmallMoney | SqlType::SmallDateTime => two_forms(4),
        SqlType::BigInt | SqlType::Float | SqlType::Money | SqlType::DateTime => two_forms(8),
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            if scale > precision {
                return Err(TdsError::Malformed(
                    "TYPE_INFO decimal scale greater than its precision",
                ));
            }
            Layout::ByteLen(usize::from(decimal_len(precision)?))
        }
        SqlType::UniqueIdentifier => Layout::ByteLen(16),
        SqlType::Date => Layout::ByteLen(3),
        SqlType::Time(scale) => Layout::ByteLen(time_len(checked_scale(scale)?)),
        SqlType::DateTime2(scale) => Layout::ByteLen(time_len(checked_scale(scale)?) + 3),
        SqlType::DateTimeOffset(scale) => Layout::ByteLen(time_len(checked_scale(scale)?) + 5),
        SqlType::VarChar(Len::Max) => Layout::Plp(Encoding::Cp1252),
        SqlType::NVarChar(Len::Max) => Layout::Plp(Encoding::Utf16Le),
        SqlType::VarBinary(Len::Max) => Layout::Plp(Encoding::Raw),
        SqlType::Char(Len::Max) | SqlType::NChar(Len::Max) | SqlType::Binary(Len::Max) => {
            return Err(TdsError::Malformed(
                "TYPE_INFO char, nchar and binary have no (max) form",
            ));
        }
        SqlType::Char(Len::Fixed(n))
        | SqlType::VarChar(Len::Fixed(n))
        | SqlType::Binary(Len::Fixed(n))
        | SqlType::VarBinary(Len::Fixed(n)) => Layout::UShortLen {
            max_bytes: checked_byte_len(n)?,
            encoding: string_encoding(ti.ty)?,
        },
        SqlType::NChar(Len::Fixed(n)) | SqlType::NVarChar(Len::Fixed(n)) => {
            if n == 0 || n > MAX_UNICODE_LEN {
                return Err(TdsError::Malformed(
                    "TYPE_INFO nchar/nvarchar length out of range 1..=4000",
                ));
            }
            Layout::UShortLen {
                // `n <= 4000`: no overflow.
                max_bytes: n * 2,
                encoding: string_encoding(ti.ty)?,
            }
        }
    })
}

/// The data layout of a string or binary type; `Malformed` for any other type.
fn string_encoding(ty: SqlType) -> Result<Encoding, TdsError> {
    Encoding::of(ty).ok_or(TdsError::Malformed("type is not a string or binary type"))
}

/// Decodes the payload of a non-NULL fixed-length or BYTELEN value; `bytes` has the exact
/// length that [`layout`] announced for `ty`.
fn decode_scalar(ty: SqlType, bytes: &[u8]) -> Result<Value, TdsError> {
    let bad = || TdsError::Malformed("value payload does not match its TYPE_INFO");
    Ok(match ty {
        SqlType::Bit => Value::Bit(*bytes.first().ok_or_else(bad)? != 0),
        SqlType::TinyInt => Value::I8(*bytes.first().ok_or_else(bad)?),
        SqlType::SmallInt => Value::I16(i16::from_le_bytes(array(bytes)?)),
        SqlType::Int => Value::I32(i32::from_le_bytes(array(bytes)?)),
        SqlType::BigInt => Value::I64(i64::from_le_bytes(array(bytes)?)),
        SqlType::Real => Value::F32(f32::from_le_bytes(array(bytes)?)),
        SqlType::Float => Value::F64(f64::from_le_bytes(array(bytes)?)),
        SqlType::Money => {
            // High 32-bit word first, then low word, each little-endian.
            let (high, low) = bytes.split_at_checked(4).ok_or_else(bad)?;
            let high = u32::from_le_bytes(array(high)?);
            let low = u32::from_le_bytes(array(low)?);
            // Reassembling the two words of an `i64`: the cast is the intended reinterpretation.
            Value::Money(((u64::from(high) << 32) | u64::from(low)) as i64)
        }
        SqlType::SmallMoney => Value::Money(i64::from(i32::from_le_bytes(array(bytes)?))),
        SqlType::DateTime => {
            let (days, ticks) = bytes.split_at_checked(4).ok_or_else(bad)?;
            Value::DateTime(DateTime {
                days: i32::from_le_bytes(array(days)?),
                ticks_300th: u32::from_le_bytes(array(ticks)?),
            })
        }
        SqlType::SmallDateTime => {
            let (days, minutes) = bytes.split_at_checked(2).ok_or_else(bad)?;
            Value::DateTime(DateTime {
                days: i32::from(u16::from_le_bytes(array(days)?)),
                // `minutes <= 65535`: the product fits in a `u32`.
                ticks_300th: u32::from(u16::from_le_bytes(array(minutes)?))
                    * TICKS_300TH_PER_MINUTE,
            })
        }
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            // One sign byte (0 negative, anything else positive) then the magnitude,
            // little-endian, on the 4, 8, 12 or 16 remaining bytes.
            let (sign, magnitude) = bytes.split_first().ok_or_else(bad)?;
            let magnitude = i128::try_from(le_u128(magnitude))
                .map_err(|_| TdsError::Malformed("decimal magnitude does not fit in 127 bits"))?;
            Value::Decimal(Decimal {
                mantissa: if *sign == 0 { -magnitude } else { magnitude },
                precision,
                scale,
            })
        }
        SqlType::UniqueIdentifier => Value::Guid(array(bytes)?),
        SqlType::Date => Value::Date(date_from_bytes(bytes)?),
        SqlType::Time(scale) => Value::Time(time_from_bytes(bytes, scale)?),
        SqlType::DateTime2(scale) => {
            let (time, date) = bytes.split_at_checked(time_len(scale)).ok_or_else(bad)?;
            Value::DateTime2(DateTime2 {
                date: date_from_bytes(date)?,
                time: time_from_bytes(time, scale)?,
            })
        }
        SqlType::DateTimeOffset(scale) => {
            let (time, rest) = bytes.split_at_checked(time_len(scale)).ok_or_else(bad)?;
            let (date, offset) = rest.split_at_checked(3).ok_or_else(bad)?;
            Value::DateTimeOffset(DateTimeOffset {
                utc: DateTime2 {
                    date: date_from_bytes(date)?,
                    time: time_from_bytes(time, scale)?,
                },
                offset_minutes: i16::from_le_bytes(array(offset)?),
            })
        }
        SqlType::Char(_)
        | SqlType::VarChar(_)
        | SqlType::NChar(_)
        | SqlType::NVarChar(_)
        | SqlType::Binary(_)
        | SqlType::VarBinary(_) => {
            // `layout` never routes these here; kept as an error rather than a panic.
            return Err(TdsError::Malformed(
                "string or binary type is not a fixed-length or BYTELEN type",
            ));
        }
    })
}

/// Transcodes the data bytes of a string or binary value, the inverse of
/// [`super::ushortlen::encode_payload`].
fn decode_payload(encoding: Encoding, bytes: &[u8]) -> Result<Value, TdsError> {
    Ok(match encoding {
        Encoding::Cp1252 => Value::String(SqlString {
            text: decode_cp1252(bytes),
        }),
        Encoding::Utf16Le => Value::String(SqlString {
            text: decode_utf16le(bytes)?,
        }),
        Encoding::Raw => Value::Bytes(bytes.to_vec()),
    })
}

/// Reads the chunks of a PLP_BODY up to the terminator ([MS-TDS] 2.2.5.2.3), once the total
/// length has been read and found not to be PLP_NULL.
///
/// When the total length is known, the chunks must add up to it exactly; when it is
/// [`UNKNOWN_PLP_LEN`], the terminator alone ends the value. The buffer is preallocated
/// from the announced length only up to [`PLP_PREALLOC_CAP`].
fn read_plp_chunks(input: &mut &[u8], total: u64) -> Result<Vec<u8>, TdsError> {
    let known = if total == UNKNOWN_PLP_LEN {
        None
    } else {
        let len = usize::try_from(total)
            .ok()
            .filter(|len| *len <= PLP_MAX_VALUE_LEN)
            .ok_or(TdsError::Malformed(
                "PLP total length exceeds 2^31 - 1 bytes",
            ))?;
        Some(len)
    };
    let mut data = Vec::with_capacity(known.unwrap_or(0).min(PLP_PREALLOC_CAP));
    loop {
        let chunk_len = read_u32_le(input, "PLP chunk length")?;
        if chunk_len == PLP_TERMINATOR {
            break;
        }
        let chunk_len = usize::try_from(chunk_len)
            .map_err(|_| TdsError::Malformed("PLP chunk length does not fit in memory"))?;
        let chunk = take(input, chunk_len, "PLP chunk data")?;
        if known.is_some_and(|len| chunk.len() > len - data.len()) {
            return Err(TdsError::Malformed(
                "PLP chunks exceed the announced total length",
            ));
        }
        data.extend_from_slice(chunk);
    }
    if known.is_some_and(|len| data.len() != len) {
        return Err(TdsError::Malformed(
            "PLP chunks do not add up to the announced total length",
        ));
    }
    Ok(data)
}

// ---- Scalar helpers (inverse of `bytelen.rs`) ----

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
fn date_from_bytes(bytes: &[u8]) -> Result<Date, TdsError> {
    if bytes.len() != 3 {
        return Err(TdsError::Malformed("date value is not 3 bytes"));
    }
    // At most 24 bits: fits in an `i32`.
    let days = i32::try_from(le_u128(bytes))
        .map_err(|_| TdsError::Malformed("date value is not 3 bytes"))?;
    Ok(Date { days })
}

/// The `time(scale)` payload: the count of `10^-scale` s units, little-endian, on
/// `time_len(scale)` bytes. `scale` has been validated by [`layout`].
fn time_from_bytes(bytes: &[u8], scale: u8) -> Result<Time, TdsError> {
    if bytes.len() != time_len(scale) {
        return Err(TdsError::Malformed(
            "time value length does not match its scale",
        ));
    }
    let dropped_digits = 7u8.checked_sub(scale).ok_or(TdsError::Malformed(
        "TYPE_INFO fractional-seconds scale out of range 0..=7",
    ))?;
    let multiplier = 10u64.pow(u32::from(dropped_digits));
    // At most 40 bits of count: fits in a `u64`.
    let count = u64::try_from(le_u128(bytes))
        .map_err(|_| TdsError::Malformed("time value length does not match its scale"))?;
    let ticks_100ns = count
        .checked_mul(multiplier)
        .ok_or(TdsError::Malformed("time value overflows"))?;
    Ok(Time { ticks_100ns })
}

/// Little-endian unsigned integer of up to 16 bytes. Longer inputs are not expected: only
/// the low 16 bytes are kept (callers bound the length beforehand).
fn le_u128(bytes: &[u8]) -> u128 {
    bytes
        .iter()
        .take(16)
        .rev()
        .fold(0u128, |acc, b| (acc << 8) | u128::from(*b))
}

/// Converts a slice of the expected length into an array, `Malformed` otherwise.
fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N], TdsError> {
    <[u8; N]>::try_from(bytes)
        .map_err(|_| TdsError::Malformed("value payload does not match its TYPE_INFO"))
}

// ---- Text helpers ----

/// Decodes code page 1252 to Unicode, the inverse of [`super::cp1252::encode_cp1252`].
///
/// Bytes below `0x80` and from `0xA0` are their own code point (Latin-1); `0x80..=0x9F` go
/// through [`CP1252_80_9F`]. The five bytes the code page leaves unassigned (`0x81`, `0x8D`,
/// `0x8F`, `0x90`, `0x9D`) are mapped to the C1 control of the same code point, as the
/// Windows conversion of code page 1252 does, rather than refused.
fn decode_cp1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| match b {
            0x80..=0x9F => CP1252_80_9F
                .get(usize::from(b - 0x80))
                .copied()
                .flatten()
                .unwrap_or(char::from(*b)),
            _ => char::from(*b),
        })
        .collect()
}

/// Decodes UTF-16LE; an odd byte count or an unpaired surrogate is `Malformed`.
fn decode_utf16le(bytes: &[u8]) -> Result<String, TdsError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(TdsError::Malformed(
            "nchar/nvarchar value has an odd number of bytes",
        ));
    }
    let (pairs, _remainder) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
    String::from_utf16(&units)
        .map_err(|_| TdsError::Malformed("nchar/nvarchar value is not valid UTF-16"))
}

// ---- Cursor helpers ----

/// Takes the first `n` bytes of `input` and advances it; `Malformed(what)` when fewer remain.
fn take<'a>(input: &mut &'a [u8], n: usize, what: &'static str) -> Result<&'a [u8], TdsError> {
    let (head, tail) = input.split_at_checked(n).ok_or(TdsError::Malformed(what))?;
    *input = tail;
    Ok(head)
}

fn read_u8(input: &mut &[u8], what: &'static str) -> Result<u8, TdsError> {
    Ok(u8::from_le_bytes(
        take(input, 1, what)?
            .try_into()
            .map_err(|_| TdsError::Malformed(what))?,
    ))
}

fn read_u16_le(input: &mut &[u8], what: &'static str) -> Result<u16, TdsError> {
    Ok(u16::from_le_bytes(
        take(input, 2, what)?
            .try_into()
            .map_err(|_| TdsError::Malformed(what))?,
    ))
}

fn read_u32_le(input: &mut &[u8], what: &'static str) -> Result<u32, TdsError> {
    Ok(u32::from_le_bytes(
        take(input, 4, what)?
            .try_into()
            .map_err(|_| TdsError::Malformed(what))?,
    ))
}

fn read_u64_le(input: &mut &[u8], what: &'static str) -> Result<u64, TdsError> {
    Ok(u64::from_le_bytes(
        take(input, 8, what)?
            .try_into()
            .map_err(|_| TdsError::Malformed(what))?,
    ))
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use vauban_types::{
        Collation, Date, DateTime, DateTime2, DateTimeOffset, Decimal, Len, SqlString, SqlType,
        Time, TypeInfo, Value,
    };

    use super::{decode_type_info, decode_value};
    use crate::error::TdsError;
    use crate::types::collation::DEFAULT_COLLATION_BYTES;
    use crate::types::{encode_type_info, encode_value};

    fn ti(ty: SqlType, nullable: bool) -> TypeInfo {
        TypeInfo::new(ty, nullable)
    }

    fn s(text: &str) -> Value {
        Value::String(SqlString { text: text.into() })
    }

    /// Parses `"A7 0A 00"` into bytes.
    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    /// `hex(prefix)` followed by the default collation.
    fn hex_collated(prefix: &str) -> Vec<u8> {
        let mut v = hex(prefix);
        v.extend_from_slice(&DEFAULT_COLLATION_BYTES);
        v
    }

    fn encode_ti(ti: &TypeInfo) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode_type_info(ti, &mut out).unwrap();
        out.to_vec()
    }

    fn encode_val(ti: &TypeInfo, v: &Value) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode_value(ti, v, &mut out).unwrap();
        out.to_vec()
    }

    /// Decodes a TYPE_INFO from `bytes` followed by two sentinel bytes, and checks that the
    /// cursor stops exactly on them.
    fn type_info(bytes: &[u8]) -> TypeInfo {
        let mut buf = bytes.to_vec();
        buf.extend_from_slice(&[0xAA, 0xBB]);
        let mut cursor = &buf[..];
        let ti = decode_type_info(&mut cursor).unwrap();
        assert_eq!(cursor, &[0xAA, 0xBB], "cursor after TYPE_INFO {bytes:02X?}");
        ti
    }

    /// Same as [`type_info`] for a value.
    fn value(ti: &TypeInfo, bytes: &[u8]) -> Value {
        let mut buf = bytes.to_vec();
        buf.extend_from_slice(&[0xAA, 0xBB]);
        let mut cursor = &buf[..];
        let v = decode_value(ti, &mut cursor).unwrap();
        assert_eq!(cursor, &[0xAA, 0xBB], "cursor after value {bytes:02X?}");
        v
    }

    fn type_info_err(bytes: &[u8]) -> TdsError {
        decode_type_info(&mut &bytes[..]).unwrap_err()
    }

    fn value_err(ti: &TypeInfo, bytes: &[u8]) -> TdsError {
        decode_value(ti, &mut &bytes[..]).unwrap_err()
    }

    /// Fixed-length tokens ([MS-TDS] 2.2.5.4.1): the only ones whose TYPE_INFO says
    /// `nullable = false`.
    fn is_fixed_token(token: u8) -> bool {
        matches!(
            token,
            0x30 | 0x32 | 0x34 | 0x38 | 0x3A | 0x3B | 0x3C | 0x3D | 0x3E | 0x7A | 0x7F
        )
    }

    /// Every `(TypeInfo, Value)` couple of the encoder tests (`bytelen.rs`, `ushortlen.rs`,
    /// `plp.rs`) that the encoders write without altering the value. Fixed-width `char` /
    /// `nchar` / `binary` values are given at their full width (the encoder pads them), and
    /// `varchar` values only use characters code page 1252 can represent (the encoder
    /// writes `?` otherwise).
    fn encoder_vectors() -> Vec<(TypeInfo, Value)> {
        let dec = |precision, scale| SqlType::Decimal { precision, scale };
        let num = |precision, scale| SqlType::Numeric { precision, scale };
        let val = |mantissa, precision, scale| {
            Value::Decimal(Decimal {
                mantissa,
                precision,
                scale,
            })
        };
        let guid: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ];
        let instant = DateTime2 {
            date: Date { days: 730_119 },
            time: Time {
                ticks_100ns: 10_000_000,
            },
        };
        let one_second = Value::Time(Time {
            ticks_100ns: 10_000_000,
        });
        let custom = Collation {
            lcid: 0x040C,
            flags: 0x00,
            version: 2,
            sort_id: 0,
        };
        let with_collation = |ty, collation| TypeInfo {
            ty,
            nullable: true,
            collation: Some(collation),
        };
        let big: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();

        let mut v = vec![
            // Integers, bit.
            (ti(SqlType::Int, false), Value::I32(1)),
            (ti(SqlType::Int, false), Value::I32(-1)),
            (ti(SqlType::Int, true), Value::I32(1)),
            (ti(SqlType::Int, true), Value::Null),
            (ti(SqlType::TinyInt, false), Value::I8(255)),
            (ti(SqlType::TinyInt, true), Value::I8(7)),
            (ti(SqlType::SmallInt, false), Value::I16(-2)),
            (ti(SqlType::SmallInt, true), Value::I16(0x1234)),
            (ti(SqlType::BigInt, false), Value::I64(1)),
            (ti(SqlType::BigInt, true), Value::I64(-1)),
            (ti(SqlType::Bit, false), Value::Bit(true)),
            (ti(SqlType::Bit, false), Value::Bit(false)),
            (ti(SqlType::Bit, true), Value::Bit(true)),
            (ti(SqlType::Bit, true), Value::Bit(false)),
            (ti(SqlType::Bit, true), Value::Null),
            // Floats.
            (ti(SqlType::Float, false), Value::F64(1.5)),
            (ti(SqlType::Float, true), Value::F64(1.5)),
            (ti(SqlType::Real, false), Value::F32(1.5)),
            (ti(SqlType::Real, true), Value::F32(-2.0)),
            (ti(SqlType::Real, true), Value::Null),
            // Money.
            (ti(SqlType::Money, false), Value::Money(15000)),
            (ti(SqlType::Money, false), Value::Money(-15000)),
            (
                ti(SqlType::Money, false),
                Value::Money(0x0000_0001_0000_0002),
            ),
            (ti(SqlType::SmallMoney, false), Value::Money(15000)),
            (ti(SqlType::Money, true), Value::Money(15000)),
            (ti(SqlType::SmallMoney, true), Value::Money(15000)),
            (ti(SqlType::SmallMoney, true), Value::Money(-15000)),
            (ti(SqlType::Money, true), Value::Null),
            // datetime, smalldatetime.
            (
                ti(SqlType::DateTime, false),
                Value::DateTime(DateTime {
                    days: 1,
                    ticks_300th: 0,
                }),
            ),
            (
                ti(SqlType::DateTime, false),
                Value::DateTime(DateTime {
                    days: -1,
                    ticks_300th: 300,
                }),
            ),
            (
                ti(SqlType::SmallDateTime, false),
                Value::DateTime(DateTime {
                    days: 1,
                    ticks_300th: 18000,
                }),
            ),
            (
                ti(SqlType::SmallDateTime, true),
                Value::DateTime(DateTime {
                    days: 1,
                    ticks_300th: 18000,
                }),
            ),
            (ti(SqlType::DateTime, true), Value::Null),
            // decimal.
            (ti(dec(5, 2), false), val(12345, 5, 2)),
            (ti(dec(5, 2), true), val(12345, 5, 2)),
            (ti(dec(3, 2), false), val(-100, 3, 2)),
            (ti(dec(3, 2), true), val(-100, 3, 2)),
            (ti(dec(10, 0), true), val(1 << 32, 10, 0)),
            (ti(dec(20, 4), true), val(-(1 << 70), 20, 4)),
            (ti(dec(38, 0), true), val(-1, 38, 0)),
            (ti(dec(38, 38), true), val(i128::MAX, 38, 38)),
            (ti(dec(5, 2), true), val(0, 5, 2)),
            (ti(dec(5, 2), true), Value::Null),
            // numeric: the same layout under the NUMERICNTYPE token.
            (ti(num(2, 1), false), val(15, 2, 1)),
            (ti(num(2, 1), true), val(15, 2, 1)),
            (ti(num(10, 0), true), val(1 << 32, 10, 0)),
            (ti(num(20, 4), true), val(-(1 << 70), 20, 4)),
            (ti(num(38, 38), true), val(i128::MAX, 38, 38)),
            (ti(num(5, 2), true), Value::Null),
            // uniqueidentifier.
            (ti(SqlType::UniqueIdentifier, false), Value::Guid(guid)),
            (ti(SqlType::UniqueIdentifier, true), Value::Guid(guid)),
            (ti(SqlType::UniqueIdentifier, true), Value::Null),
            // date.
            (
                ti(SqlType::Date, false),
                Value::Date(Date { days: 730_119 }),
            ),
            (ti(SqlType::Date, true), Value::Date(Date { days: 730_119 })),
            (ti(SqlType::Date, true), Value::Date(Date { days: 0 })),
            (ti(SqlType::Date, true), Value::Null),
            // time.
            (ti(SqlType::Time(7), false), one_second.clone()),
            (ti(SqlType::Time(7), true), one_second.clone()),
            (ti(SqlType::Time(0), true), one_second.clone()),
            (ti(SqlType::Time(2), true), one_second.clone()),
            (ti(SqlType::Time(3), true), one_second.clone()),
            (ti(SqlType::Time(4), true), one_second.clone()),
            (ti(SqlType::Time(5), true), one_second.clone()),
            (
                ti(SqlType::Time(7), true),
                Value::Time(Time {
                    ticks_100ns: 863_999_999_999,
                }),
            ),
            (ti(SqlType::Time(3), true), Value::Null),
            // datetime2, datetimeoffset.
            (ti(SqlType::DateTime2(7), false), Value::DateTime2(instant)),
            (ti(SqlType::DateTime2(7), true), Value::DateTime2(instant)),
            (ti(SqlType::DateTime2(0), true), Value::DateTime2(instant)),
            (
                ti(SqlType::DateTimeOffset(7), false),
                Value::DateTimeOffset(DateTimeOffset {
                    utc: instant,
                    offset_minutes: 0,
                }),
            ),
            (
                ti(SqlType::DateTimeOffset(7), true),
                Value::DateTimeOffset(DateTimeOffset {
                    utc: instant,
                    offset_minutes: 0,
                }),
            ),
            (
                ti(SqlType::DateTimeOffset(3), true),
                Value::DateTimeOffset(DateTimeOffset {
                    utc: instant,
                    offset_minutes: -120,
                }),
            ),
            (ti(SqlType::DateTime2(7), true), Value::Null),
            (ti(SqlType::DateTimeOffset(7), true), Value::Null),
            // USHORTLEN.
            (ti(SqlType::VarChar(Len::Fixed(10)), true), s("ab")),
            (ti(SqlType::VarChar(Len::Fixed(10)), true), s("")),
            (ti(SqlType::VarChar(Len::Fixed(10)), true), Value::Null),
            (ti(SqlType::NVarChar(Len::Fixed(10)), true), s("ab")),
            (ti(SqlType::NVarChar(Len::Fixed(10)), true), s("")),
            (ti(SqlType::NVarChar(Len::Fixed(10)), true), Value::Null),
            (ti(SqlType::Char(Len::Fixed(3)), false), s("abc")),
            (ti(SqlType::Char(Len::Fixed(3)), false), s("   ")),
            (ti(SqlType::NChar(Len::Fixed(2)), true), s("a ")),
            (ti(SqlType::NChar(Len::Fixed(2)), true), Value::Null),
            (
                ti(SqlType::VarBinary(Len::Fixed(8)), true),
                Value::Bytes(vec![1, 2, 3]),
            ),
            (
                ti(SqlType::VarBinary(Len::Fixed(8)), true),
                Value::Bytes(vec![]),
            ),
            (ti(SqlType::VarBinary(Len::Fixed(8)), true), Value::Null),
            (
                ti(SqlType::Binary(Len::Fixed(4)), false),
                Value::Bytes(vec![1, 0, 0, 0]),
            ),
            (ti(SqlType::VarChar(Len::Fixed(10)), true), s("é€œ")),
            (ti(SqlType::NVarChar(Len::Fixed(2)), true), s("éé")),
            (ti(SqlType::NVarChar(Len::Fixed(4)), true), s("\u{1F600}")),
            (
                with_collation(SqlType::VarChar(Len::Fixed(5)), custom),
                s("abc"),
            ),
            (ti(SqlType::VarChar(Len::Fixed(8000)), true), Value::Null),
            (ti(SqlType::NVarChar(Len::Fixed(4000)), true), Value::Null),
            (ti(SqlType::VarBinary(Len::Fixed(8000)), true), Value::Null),
            // PLP.
            (ti(SqlType::NVarChar(Len::Max), true), s("ab")),
            (ti(SqlType::NVarChar(Len::Max), true), Value::Null),
            (ti(SqlType::NVarChar(Len::Max), true), s("")),
            (ti(SqlType::VarBinary(Len::Max), true), Value::Bytes(vec![])),
            (ti(SqlType::VarBinary(Len::Max), true), Value::Bytes(big)),
            (
                ti(SqlType::VarBinary(Len::Max), true),
                Value::Bytes(vec![0xAB; 8000]),
            ),
            (ti(SqlType::VarChar(Len::Max), false), s("é€œ")),
            (ti(SqlType::VarBinary(Len::Max), true), Value::Null),
            (
                with_collation(SqlType::NVarChar(Len::Max), custom),
                s("Œuvre — “ok”"),
            ),
        ];
        // A long `nvarchar(max)`: more than one chunk of UTF-16.
        v.push((ti(SqlType::NVarChar(Len::Max), true), s(&"é".repeat(5000))));
        v
    }

    #[test]
    fn roundtrip_every_encoder_vector() {
        let vectors = encoder_vectors();
        // Guards against an accidental emptying of the table.
        assert!(vectors.len() >= 90, "{} vectors", vectors.len());
        for (t, v) in &vectors {
            let ti_bytes = encode_ti(t);
            let decoded = type_info(&ti_bytes);
            assert_eq!(decoded.ty, t.ty, "type of {t:?}");
            assert_eq!(decoded.collation, t.collation, "collation of {t:?}");
            if is_fixed_token(ti_bytes[0]) {
                assert!(!decoded.nullable, "fixed token of {t:?}");
            } else {
                assert!(decoded.nullable, "N or variable form of {t:?}");
            }

            let value_bytes = encode_val(t, v);
            // Decoded with the caller's `TypeInfo`…
            assert_eq!(&value(t, &value_bytes), v, "value {v:?} of {t:?}");
            // …and with the one read back from the wire, as the RPC decoder does.
            assert_eq!(
                &value(&decoded, &value_bytes),
                v,
                "value {v:?} of decoded {decoded:?}"
            );
        }
    }

    #[test]
    fn decode_nvarchar_8000_param() {
        let t = type_info(&hex("E7 40 1F 09 04 D0 00 34"));
        assert_eq!(t.ty, SqlType::NVarChar(Len::Fixed(4000)));
        assert!(t.nullable);
        assert_eq!(t.collation, Some(Collation::DEFAULT));

        let mut bytes = hex("10 00");
        bytes.extend("SELECT 1".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(value(&t, &bytes), s("SELECT 1"));
    }

    #[test]
    fn decode_plp_unknown_length() {
        let t = type_info(&hex("A5 FF FF"));
        assert_eq!(t, TypeInfo::new(SqlType::VarBinary(Len::Max), true));
        let bytes =
            hex("FE FF FF FF FF FF FF FF  03 00 00 00 01 02 03  02 00 00 00 04 05  00 00 00 00");
        assert_eq!(value(&t, &bytes), Value::Bytes(vec![1, 2, 3, 4, 5]));

        // Unknown length with no chunk at all: empty, not NULL.
        assert_eq!(
            value(&t, &hex("FE FF FF FF FF FF FF FF  00 00 00 00")),
            Value::Bytes(vec![])
        );

        // The same stream for a character type is transcoded.
        let t = type_info(&hex_collated("A7 FF FF"));
        assert_eq!(
            value(
                &t,
                &hex("FE FF FF FF FF FF FF FF  02 00 00 00 61 62  01 00 00 00 E9  00 00 00 00")
            ),
            s("abé")
        );
    }

    #[test]
    fn decode_plp_known_length_must_add_up() {
        let t = ti(SqlType::VarBinary(Len::Max), true);
        // Announces 5 bytes, delivers 3.
        assert!(matches!(
            value_err(
                &t,
                &hex("05 00 00 00 00 00 00 00  03 00 00 00 01 02 03  00 00 00 00")
            ),
            TdsError::Malformed(_)
        ));
        // Announces 2 bytes, delivers 3.
        assert!(matches!(
            value_err(
                &t,
                &hex("02 00 00 00 00 00 00 00  03 00 00 00 01 02 03  00 00 00 00")
            ),
            TdsError::Malformed(_)
        ));
        // A total length above 2^31 - 1 cannot be a `(max)` value.
        assert!(matches!(
            value_err(&t, &hex("00 00 00 80 00 00 00 00  00 00 00 00")),
            TdsError::Malformed(_)
        ));
        // A huge announced length with a short stream does not allocate 16 GiB.
        assert!(matches!(
            value_err(&t, &hex("00 00 00 00 04 00 00 00  00 00 00 00")),
            TdsError::Malformed(_)
        ));
    }

    #[test]
    fn decode_nulls() {
        let t = type_info(&hex("26 04"));
        assert_eq!(t, TypeInfo::new(SqlType::Int, true));
        assert_eq!(value(&t, &hex("00")), Value::Null);

        let t = type_info(&hex_collated("A7 0A 00"));
        assert_eq!(t, TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true));
        assert_eq!(value(&t, &hex("FF FF")), Value::Null);
        assert_eq!(value(&t, &hex("00 00")), s(""));

        let t = type_info(&hex_collated("E7 FF FF"));
        assert_eq!(t, TypeInfo::new(SqlType::NVarChar(Len::Max), true));
        assert_eq!(value(&t, &hex("FF FF FF FF FF FF FF FF")), Value::Null);

        // A NULL on the wire is delivered even when the caller's TypeInfo says not
        // nullable: the client's TYPE_INFO carries no nullability.
        assert_eq!(
            value(
                &ti(
                    SqlType::Decimal {
                        precision: 5,
                        scale: 2
                    },
                    false
                ),
                &hex("00")
            ),
            Value::Null
        );
    }

    #[test]
    fn decode_cp1252() {
        let t = type_info(&hex_collated("A7 0A 00"));
        assert_eq!(value(&t, &hex("03 00 E9 80 9C")), s("é€œ"));
        // Latin-1 edges and the whole assigned part of 0x80..=0x9F.
        assert_eq!(value(&t, &hex("04 00 00 7F A0 FF")), s("\0\u{7F}\u{A0}ÿ"));
        assert_eq!(value(&t, &hex("06 00 8C 20 97 20 93 94")), s("Œ — “”"));
        // Unassigned bytes are mapped to the C1 control of the same code point.
        assert_eq!(
            value(&t, &hex("05 00 81 8D 8F 90 9D")),
            s("\u{81}\u{8D}\u{8F}\u{90}\u{9D}")
        );
        // Every byte of the code page decodes; the 27 assigned positions of 0x80..=0x9F
        // round-trip through the encoder's table.
        for b in 0x80u8..=0x9F {
            let decoded = value(&t, &[1, 0, b]);
            let Value::String(text) = &decoded else {
                panic!("{decoded:?}");
            };
            assert_eq!(text.text.chars().count(), 1);
            if let Some(c) = crate::types::cp1252::CP1252_80_9F[usize::from(b - 0x80)] {
                assert_eq!(text.text, c.to_string(), "{b:02X}");
            }
        }
    }

    #[test]
    fn decode_utf16_errors() {
        let t = ti(SqlType::NVarChar(Len::Fixed(10)), true);
        // Odd length.
        assert!(matches!(
            value_err(&t, &hex("03 00 61 00 62")),
            TdsError::Malformed(_)
        ));
        // Lone high surrogate.
        assert!(matches!(
            value_err(&t, &hex("02 00 3D D8")),
            TdsError::Malformed(_)
        ));
        // Longer than the declared maximum.
        let t = ti(SqlType::NVarChar(Len::Fixed(1)), true);
        assert!(matches!(
            value_err(&t, &hex("04 00 61 00 62 00")),
            TdsError::Malformed(_)
        ));
    }

    #[test]
    fn decode_legacy_tokens() {
        let decimal = TypeInfo::new(
            SqlType::Decimal {
                precision: 5,
                scale: 2,
            },
            true,
        );
        let numeric = TypeInfo::new(
            SqlType::Numeric {
                precision: 5,
                scale: 2,
            },
            true,
        );
        // DECIMALTYPE, NUMERICTYPE, NUMERICNTYPE: same layout as DECIMALNTYPE. NUMERICNTYPE
        // reads as `numeric`; the two pre-TDS 7.2 tokens read as `decimal`, as before.
        assert_eq!(type_info(&hex("37 05 05 02")), decimal);
        assert_eq!(type_info(&hex("3F 05 05 02")), decimal);
        assert_eq!(type_info(&hex("6A 05 05 02")), decimal);
        assert_eq!(type_info(&hex("6C 05 05 02")), numeric);
        assert_eq!(
            value(&type_info(&hex("37 05 05 02")), &hex("05 01 39 30 00 00")),
            Value::Decimal(Decimal {
                mantissa: 12345,
                precision: 5,
                scale: 2
            })
        );

        assert_eq!(
            type_info(&hex("26 01")),
            TypeInfo::new(SqlType::TinyInt, true)
        );
        assert_eq!(
            type_info(&hex("26 02")),
            TypeInfo::new(SqlType::SmallInt, true)
        );
        assert_eq!(type_info(&hex("26 04")), TypeInfo::new(SqlType::Int, true));
        assert_eq!(
            type_info(&hex("26 08")),
            TypeInfo::new(SqlType::BigInt, true)
        );
        assert_eq!(
            value(&type_info(&hex("26 01")), &hex("01 07")),
            Value::I8(7)
        );
        assert_eq!(
            value(
                &type_info(&hex("26 08")),
                &hex("08 FF FF FF FF FF FF FF FF")
            ),
            Value::I64(-1)
        );

        assert_eq!(type_info(&hex("38")), TypeInfo::new(SqlType::Int, false));
        assert_eq!(
            value(&type_info(&hex("38")), &hex("01 00 00 00")),
            Value::I32(1)
        );

        // The other N tokens with a length that selects the type.
        assert_eq!(type_info(&hex("6D 04")), TypeInfo::new(SqlType::Real, true));
        assert_eq!(
            type_info(&hex("6D 08")),
            TypeInfo::new(SqlType::Float, true)
        );
        assert_eq!(
            type_info(&hex("6E 04")),
            TypeInfo::new(SqlType::SmallMoney, true)
        );
        assert_eq!(
            type_info(&hex("6E 08")),
            TypeInfo::new(SqlType::Money, true)
        );
        assert_eq!(
            type_info(&hex("6F 04")),
            TypeInfo::new(SqlType::SmallDateTime, true)
        );
        assert_eq!(
            type_info(&hex("6F 08")),
            TypeInfo::new(SqlType::DateTime, true)
        );
        assert_eq!(type_info(&hex("68 01")), TypeInfo::new(SqlType::Bit, true));
        assert_eq!(
            type_info(&hex("24 10")),
            TypeInfo::new(SqlType::UniqueIdentifier, true)
        );
        assert_eq!(type_info(&hex("28")), TypeInfo::new(SqlType::Date, true));
    }

    #[test]
    fn decode_collation_fields() {
        // Latin1_General_100_CI_AS: LCID 0x0409, flags 0x0D, version 2, no SortId.
        let t = type_info(&hex("A7 05 00 09 04 D0 20 00"));
        assert_eq!(
            t.collation,
            Some(Collation {
                lcid: 0x0409,
                flags: 0x0D,
                version: 2,
                sort_id: 0,
            })
        );
        let t = type_info(&hex("EF 02 00 0C 04 10 10 00"));
        assert_eq!(t.ty, SqlType::NChar(Len::Fixed(1)));
        assert_eq!(
            t.collation,
            Some(Collation {
                lcid: 0x040C,
                flags: 0x01,
                version: 1,
                sort_id: 0,
            })
        );
        // Binary types carry no collation, and none is read.
        let t = type_info(&hex("AD 04 00"));
        assert_eq!(t, TypeInfo::new(SqlType::Binary(Len::Fixed(4)), true));
        assert_eq!(t.collation, None);
    }

    /// The vectors of the tests above, TYPE_INFO then value, for the truncation test.
    fn named_vectors() -> Vec<Vec<u8>> {
        let mut nvarchar_8000 = hex("E7 40 1F 09 04 D0 00 34 10 00");
        nvarchar_8000.extend("SELECT 1".encode_utf16().flat_map(u16::to_le_bytes));
        let mut vectors = vec![
            nvarchar_8000,
            hex(
                "A5 FF FF  FE FF FF FF FF FF FF FF  03 00 00 00 01 02 03  02 00 00 00 04 05  00 00 00 00",
            ),
            hex("26 04 00"),
            hex("38 01 00 00 00"),
            hex("37 05 05 02 05 01 39 30 00 00"),
            hex("29 07 05 80 96 98 00 00"),
            hex("2B 03 09 E8 03 00 00 07 24 0B 88 FF"),
        ];
        for tail in ["FF FF", "00 00", "03 00 E9 80 9C"] {
            let mut v = hex_collated("A7 0A 00");
            v.extend(hex(tail));
            vectors.push(v);
        }
        let mut v = hex_collated("E7 FF FF");
        v.extend(hex("FF FF FF FF FF FF FF FF"));
        vectors.push(v);
        let mut v = hex_collated("E7 FF FF");
        v.extend(hex(
            "04 00 00 00 00 00 00 00  04 00 00 00  61 00 62 00  00 00 00 00",
        ));
        vectors.push(v);
        vectors
    }

    #[test]
    fn decode_truncated_is_malformed() {
        let mut streams = named_vectors();
        for (t, v) in encoder_vectors() {
            let mut bytes = encode_ti(&t);
            bytes.extend(encode_val(&t, &v));
            streams.push(bytes);
        }
        for stream in &streams {
            // Sanity: the whole stream decodes.
            let mut cursor = &stream[..];
            let t = decode_type_info(&mut cursor).unwrap();
            decode_value(&t, &mut cursor).unwrap();
            assert!(cursor.is_empty());

            for cut in 0..stream.len() {
                let truncated = &stream[..cut];
                let mut cursor = truncated;
                let result =
                    decode_type_info(&mut cursor).and_then(|t| decode_value(&t, &mut cursor));
                assert!(
                    matches!(result, Err(TdsError::Malformed(_))),
                    "{stream:02X?} cut at {cut}: {result:?}"
                );
            }
        }

        // A PLP chunk announcing more bytes than available.
        let t = ti(SqlType::VarBinary(Len::Max), true);
        assert!(matches!(
            value_err(&t, &hex("FE FF FF FF FF FF FF FF  05 00 00 00 01 02 03")),
            TdsError::Malformed(_)
        ));
        assert!(matches!(
            value_err(&t, &hex("03 00 00 00 00 00 00 00  FF FF FF 7F 01 02 03")),
            TdsError::Malformed(_)
        ));
        // A chunk that fits but no terminator.
        assert!(matches!(
            value_err(&t, &hex("03 00 00 00 00 00 00 00  03 00 00 00 01 02 03")),
            TdsError::Malformed(_)
        ));
        // Empty input for both functions.
        assert!(matches!(type_info_err(&[]), TdsError::Malformed(_)));
        assert!(matches!(value_err(&t, &[]), TdsError::Malformed(_)));
        assert!(matches!(
            value_err(&ti(SqlType::Int, false), &[]),
            TdsError::Malformed(_)
        ));
    }

    #[test]
    fn decode_inconsistent_lengths() {
        // INTNTYPE of maximum length 4 with a value of length 3.
        let t = type_info(&hex("26 04"));
        assert!(matches!(
            value_err(&t, &hex("03 01 00 00")),
            TdsError::Malformed(_)
        ));
        assert!(matches!(
            value_err(&t, &hex("08 01 00 00 00 00 00 00 00")),
            TdsError::Malformed(_)
        ));
        // DECIMALN with precision 40; length that does not match the precision; scale
        // above precision.
        assert!(matches!(
            type_info_err(&hex("6A 11 28 00")),
            TdsError::Malformed(_)
        ));
        assert!(matches!(
            type_info_err(&hex("6A 09 05 02")),
            TdsError::Malformed(_)
        ));
        assert!(matches!(
            type_info_err(&hex("6A 05 05 06")),
            TdsError::Malformed(_)
        ));
        // NUMERICN goes through the same checks.
        assert!(matches!(
            type_info_err(&hex("6C 11 28 00")),
            TdsError::Malformed(_)
        ));
        assert!(matches!(
            type_info_err(&hex("6C 09 05 02")),
            TdsError::Malformed(_)
        ));
        assert!(matches!(
            type_info_err(&hex("6C 05 05 06")),
            TdsError::Malformed(_)
        ));
        // TIMEN with scale 8, and the same for datetime2 / datetimeoffset.
        assert!(matches!(
            type_info_err(&hex("29 08")),
            TdsError::Malformed(_)
        ));
        assert!(matches!(
            type_info_err(&hex("2A 08")),
            TdsError::Malformed(_)
        ));
        assert!(matches!(
            type_info_err(&hex("2B 08")),
            TdsError::Malformed(_)
        ));
        // Lengths that select no type.
        for bytes in [
            "26 03", "26 00", "68 02", "6D 02", "6E 01", "6F 02", "24 08",
        ] {
            assert!(
                matches!(type_info_err(&hex(bytes)), TdsError::Malformed(_)),
                "{bytes}"
            );
        }
        // USHORTLEN bounds: zero, above 8000 / 4000, odd nvarchar, (max) on char.
        for bytes in [
            "A7 00 00", "A7 41 1F", "E7 00 00", "E7 41 1F", "E7 42 1F", "AF FF FF", "EF FF FF",
            "AD FF FF", "A5 00 00", "AD 41 1F",
        ] {
            assert!(
                matches!(type_info_err(&hex(bytes)), TdsError::Malformed(_)),
                "{bytes}"
            );
        }
        // A `time(7)` value of the wrong length under a `TypeInfo` from the caller.
        assert!(matches!(
            value_err(&ti(SqlType::Time(7), true), &hex("03 01 00 00")),
            TdsError::Malformed(_)
        ));
        // `TypeInfo`s the encoders refuse are refused here too.
        for t in [
            ti(
                SqlType::Decimal {
                    precision: 0,
                    scale: 0,
                },
                true,
            ),
            ti(
                SqlType::Decimal {
                    precision: 39,
                    scale: 0,
                },
                true,
            ),
            ti(
                SqlType::Numeric {
                    precision: 39,
                    scale: 0,
                },
                true,
            ),
            ti(SqlType::Time(8), true),
            ti(SqlType::Char(Len::Max), true),
            ti(SqlType::NChar(Len::Max), true),
            ti(SqlType::Binary(Len::Max), true),
            ti(SqlType::VarChar(Len::Fixed(0)), true),
            ti(SqlType::NVarChar(Len::Fixed(4001)), true),
        ] {
            assert!(
                matches!(value_err(&t, &hex("00")), TdsError::Malformed(_)),
                "{t:?}"
            );
        }
    }

    #[test]
    fn decode_unsupported_types() {
        for (bytes, label) in [
            ("23", "TEXTTYPE"),
            ("63", "NTEXTTYPE"),
            ("22", "IMAGETYPE"),
            ("F1", "XMLTYPE"),
            ("62", "SSVARIANTTYPE"),
            ("F0", "UDTTYPE"),
            ("F3", "TVPTYPE"),
            ("1F", "NULLTYPE"),
            ("25", "VARBINARYTYPE"),
            ("27", "VARCHARTYPE"),
            ("2D", "BINARYTYPE"),
            ("2F", "CHARTYPE"),
        ] {
            match type_info_err(&hex(bytes)) {
                TdsError::Unsupported(name) => assert!(name.starts_with(label), "{bytes}: {name}"),
                other => panic!("{bytes}: {other:?}"),
            }
        }
        // A token the spec does not define is malformed, not unsupported.
        assert!(matches!(type_info_err(&hex("00")), TdsError::Malformed(_)));
        assert!(matches!(type_info_err(&hex("FF")), TdsError::Malformed(_)));
    }

    #[test]
    fn decode_scalar_edges() {
        // Bit: any non-zero byte is true.
        assert_eq!(
            value(&ti(SqlType::Bit, true), &hex("01 02")),
            Value::Bit(true)
        );
        // Money reassembles the two words.
        assert_eq!(
            value(&ti(SqlType::Money, false), &hex("FF FF FF FF 68 C5 FF FF")),
            Value::Money(-15000)
        );
        // Decimal: negative zero decodes as zero; magnitude beyond 127 bits is refused.
        let t = ti(
            SqlType::Decimal {
                precision: 38,
                scale: 0,
            },
            true,
        );
        assert_eq!(
            value(
                &t,
                &hex("11 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00")
            ),
            Value::Decimal(Decimal {
                mantissa: 0,
                precision: 38,
                scale: 0
            })
        );
        assert!(matches!(
            value_err(
                &t,
                &hex("11 01 FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF")
            ),
            TdsError::Malformed(_)
        ));
        // A decimal wider than its precision is returned as it is (no semantic check).
        let t = ti(
            SqlType::Decimal {
                precision: 1,
                scale: 0,
            },
            true,
        );
        assert_eq!(
            value(&t, &hex("05 01 FF FF FF FF")),
            Value::Decimal(Decimal {
                mantissa: 0xFFFF_FFFF,
                precision: 1,
                scale: 0
            })
        );
        // smalldatetime: minutes become 1/300 s ticks.
        assert_eq!(
            value(&ti(SqlType::SmallDateTime, true), &hex("04 01 00 02 00")),
            Value::DateTime(DateTime {
                days: 1,
                ticks_300th: 36000
            })
        );
    }
}
