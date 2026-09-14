//! [MS-TDS] 2.2.5.4.2 Variable-Length Data Types, USHORTLEN_TYPE: TYPE_INFO and value encoding
//! of `char(n)`, `varchar(n)`, `nchar(n)`, `nvarchar(n)`, `binary(n)` and `varbinary(n)`.
//!
//! # TYPE_INFO ([MS-TDS] 2.2.5.6)
//!
//! Token, then the maximum length on two little-endian bytes, then, for the four character
//! types only, the 5-byte `Collation` rule ([`super::collation`]). The maximum length is in
//! **bytes**: `n` for the single-byte and binary types, `2n` for `nchar(n)` / `nvarchar(n)`.
//! Bounds: `n` in `1..=8000` for `char`, `varchar`, `binary`, `varbinary`, `1..=4000` for
//! `nchar` and `nvarchar`. `Len::Max` does not reach this file for `varchar`, `nvarchar`
//! and `varbinary` (PLP, `plp.rs`); `char(max)`, `nchar(max)` and `binary(max)` do not exist
//! in SQL Server and are refused with `Malformed`.
//!
//! When a character type carries no collation (`TypeInfo::collation == None`),
//! `Collation::DEFAULT` is written.
//!
//! # Values ([MS-TDS] 2.2.5.5.1)
//!
//! The actual length on two little-endian bytes, then the data: code page 1252 for `char` /
//! `varchar` ([`super::cp1252`]), UTF-16LE for `nchar` / `nvarchar`, raw bytes for `binary` /
//! `varbinary`. `0xFFFF` (CHARBIN_NULL) is NULL; an empty string or binary is a length of `0`
//! and no data, which is **not** NULL.
//!
//! The fixed-width types are padded on the value: `char(n)` with spaces and `nchar(n)` with
//! UTF-16 spaces up to `n` characters, `binary(n)` with zeros up to `n` bytes, so that every
//! non-NULL value has the declared length, as SQL Server sends them. A value longer than the
//! declared length is refused with `ValueTypeMismatch`: truncation belongs to the engine.

use bytes::{BufMut, BytesMut};
use vauban_types::{Collation, Len, SqlType, TypeInfo, Value};

use super::collation::encode_collation;
use super::cp1252::encode_cp1252;
use super::{BIGBINARYTYPE, BIGCHARTYPE, BIGVARBINTYPE, BIGVARCHRTYPE, NCHARTYPE, NVARCHARTYPE};
use crate::error::TdsError;

/// [MS-TDS] 2.2.5.4.2 CHARBIN_NULL: the length that stands for NULL in a USHORTLEN value.
pub(crate) const CHARBIN_NULL: u16 = 0xFFFF;

/// Largest `n` of `char(n)`, `varchar(n)`, `binary(n)` and `varbinary(n)`.
pub(crate) const MAX_BYTE_LEN: u16 = 8000;
/// Largest `n` of `nchar(n)` and `nvarchar(n)`, in UTF-16 code units.
pub(crate) const MAX_UNICODE_LEN: u16 = 4000;

/// How the data of a string or binary type is laid out on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Encoding {
    /// `char`, `varchar`: one byte per character in code page 1252.
    Cp1252,
    /// `nchar`, `nvarchar`: UTF-16LE.
    Utf16Le,
    /// `binary`, `varbinary`: the bytes themselves.
    Raw,
}

impl Encoding {
    /// The encoding of a string or binary type, `None` for any other type.
    pub(super) fn of(ty: SqlType) -> Option<Encoding> {
        match ty {
            SqlType::Char(_) | SqlType::VarChar(_) => Some(Encoding::Cp1252),
            SqlType::NChar(_) | SqlType::NVarChar(_) => Some(Encoding::Utf16Le),
            SqlType::Binary(_) | SqlType::VarBinary(_) => Some(Encoding::Raw),
            _ => None,
        }
    }

    /// The bytes used to pad a fixed-width value: a space for the character types, a zero
    /// for the binary types.
    fn padding(self) -> &'static [u8] {
        match self {
            Encoding::Cp1252 => b" ",
            Encoding::Utf16Le => &[0x20, 0x00],
            Encoding::Raw => &[0x00],
        }
    }
}

/// The wire form of a USHORTLEN type.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wire {
    /// Type token.
    token: u8,
    /// Maximum length of a value in bytes, as announced in the TYPE_INFO.
    max_bytes: u16,
    /// Data layout.
    encoding: Encoding,
    /// Whether non-NULL values are padded up to `max_bytes` (`char`, `nchar`, `binary`).
    fixed_width: bool,
    /// Collation written after the maximum length, for the character types.
    collation: Option<Collation>,
}

/// Chooses the wire form of `ti` and validates its declared length.
fn wire(ti: &TypeInfo) -> Result<Wire, TdsError> {
    let (token, len, fixed_width, unit_bytes) = match ti.ty {
        SqlType::Char(len) => (BIGCHARTYPE, len, true, 1),
        SqlType::VarChar(len) => (BIGVARCHRTYPE, len, false, 1),
        SqlType::NChar(len) => (NCHARTYPE, len, true, 2),
        SqlType::NVarChar(len) => (NVARCHARTYPE, len, false, 2),
        SqlType::Binary(len) => (BIGBINARYTYPE, len, true, 1),
        SqlType::VarBinary(len) => (BIGVARBINTYPE, len, false, 1),
        _ => {
            // `types::family` never routes these here; kept as an error rather than a panic.
            return Err(TdsError::Malformed(
                "type is not a USHORTLEN string or binary type",
            ));
        }
    };
    let n = match len {
        Len::Fixed(n) => n,
        // `varchar(max)`, `nvarchar(max)` and `varbinary(max)` go to `plp.rs`; the other
        // three have no `(max)` form in SQL Server.
        Len::Max => {
            return Err(TdsError::Malformed(
                "TYPE_INFO char, nchar and binary have no (max) form",
            ));
        }
    };
    let (bound, out_of_range) = if unit_bytes == 2 {
        (
            MAX_UNICODE_LEN,
            "TYPE_INFO nchar/nvarchar length out of range 1..=4000",
        )
    } else {
        (
            MAX_BYTE_LEN,
            "TYPE_INFO char/varchar/binary/varbinary length out of range 1..=8000",
        )
    };
    if n == 0 || n > bound {
        return Err(TdsError::Malformed(out_of_range));
    }
    // `encoding` is `Some` for every type matched above.
    let encoding = Encoding::of(ti.ty).ok_or(TdsError::Malformed(
        "type is not a USHORTLEN string or binary type",
    ))?;
    let collation = ti
        .ty
        .is_string()
        .then(|| ti.collation.unwrap_or(Collation::DEFAULT));
    Ok(Wire {
        token,
        // `n <= 4000` when `unit_bytes == 2`: no overflow.
        max_bytes: n * unit_bytes,
        encoding,
        fixed_width,
        collation,
    })
}

/// Appends the TYPE_INFO of `ti` to `out` ([MS-TDS] 2.2.5.6).
pub(crate) fn encode_type_info(ti: &TypeInfo, out: &mut BytesMut) -> Result<(), TdsError> {
    let wire = wire(ti)?;
    out.put_u8(wire.token);
    out.put_u16_le(wire.max_bytes);
    if let Some(collation) = wire.collation {
        out.put_slice(&encode_collation(&collation));
    }
    Ok(())
}

/// Appends `value` to `out` in the layout announced by the TYPE_INFO of `ti`
/// ([MS-TDS] 2.2.5.5.1, USHORTLEN). Nothing is written when an error is returned.
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
        out.put_u16_le(CHARBIN_NULL);
        return Ok(());
    }
    let mut payload = encode_payload(ti.ty, wire.encoding, value)?;
    let max_bytes = usize::from(wire.max_bytes);
    if payload.len() > max_bytes {
        return Err(mismatch(ti.ty));
    }
    if wire.fixed_width {
        let unit = wire.encoding.padding();
        // `max_bytes` is a multiple of the unit size and `payload.len()` too.
        while payload.len() < max_bytes {
            payload.extend_from_slice(unit);
        }
    }
    // `payload.len() <= max_bytes <= 8000`: the cast cannot truncate.
    out.put_u16_le(payload.len() as u16);
    out.put_slice(&payload);
    Ok(())
}

/// Transcodes a non-NULL `value` to the data bytes of a string or binary type: `Value::String`
/// for the character types, `Value::Bytes` for the binary types; anything else is a
/// `ValueTypeMismatch`. Shared with `plp.rs`, which chunks the result.
pub(super) fn encode_payload(
    ty: SqlType,
    encoding: Encoding,
    value: &Value,
) -> Result<Vec<u8>, TdsError> {
    match (encoding, value) {
        (Encoding::Cp1252, Value::String(s)) => Ok(encode_cp1252(&s.text)),
        (Encoding::Utf16Le, Value::String(s)) => {
            Ok(s.text.encode_utf16().flat_map(u16::to_le_bytes).collect())
        }
        (Encoding::Raw, Value::Bytes(b)) => Ok(b.clone()),
        _ => Err(mismatch(ty)),
    }
}

/// The `ValueTypeMismatch` error for `ty`.
pub(super) fn mismatch(ty: SqlType) -> TdsError {
    TdsError::ValueTypeMismatch {
        expected: type_name(ty),
    }
}

/// Name of a string or binary type, for `ValueTypeMismatch`.
fn type_name(ty: SqlType) -> &'static str {
    match ty {
        SqlType::Char(_) => "char",
        SqlType::VarChar(_) => "varchar",
        SqlType::NChar(_) => "nchar",
        SqlType::NVarChar(_) => "nvarchar",
        SqlType::Binary(_) => "binary",
        SqlType::VarBinary(_) => "varbinary",
        // `wire` and `Encoding::of` refuse every other type before a mismatch can be built.
        _ => "string or binary",
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value};

    use crate::error::TdsError;
    // Through the dispatch of `types/mod.rs`, so that the routing is covered too.
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

    fn type_info(ti: &TypeInfo) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode_type_info(ti, &mut out).unwrap();
        out.to_vec()
    }

    fn type_info_err(ti: &TypeInfo) -> TdsError {
        let mut out = BytesMut::new();
        let err = encode_type_info(ti, &mut out).unwrap_err();
        assert!(out.is_empty(), "nothing must be written on error");
        err
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

    #[test]
    fn varchar_vectors() {
        let t = ti(SqlType::VarChar(Len::Fixed(10)), true);
        assert_eq!(type_info(&t), hex("A7 0A 00 09 04 D0 00 34"));
        assert_eq!(value(&t, &s("ab")), hex("02 00 61 62"));
        // Empty, not NULL.
        assert_eq!(value(&t, &s("")), hex("00 00"));
        assert_eq!(value(&t, &Value::Null), hex("FF FF"));
    }

    #[test]
    fn nvarchar_vectors() {
        let t = ti(SqlType::NVarChar(Len::Fixed(10)), true);
        // Maximum length in bytes: 2 × 10.
        assert_eq!(type_info(&t), hex("E7 14 00 09 04 D0 00 34"));
        assert_eq!(value(&t, &s("ab")), hex("04 00 61 00 62 00"));
        assert_eq!(value(&t, &s("")), hex("00 00"));
        assert_eq!(value(&t, &Value::Null), hex("FF FF"));
    }

    #[test]
    fn char_and_nchar_pad() {
        let t = ti(SqlType::Char(Len::Fixed(3)), false);
        assert_eq!(type_info(&t), hex_collated("AF 03 00"));
        assert_eq!(value(&t, &s("ab")), hex("03 00 61 62 20"));
        assert_eq!(value(&t, &s("")), hex("03 00 20 20 20"));
        assert_eq!(value(&t, &s("abc")), hex("03 00 61 62 63"));

        let t = ti(SqlType::NChar(Len::Fixed(2)), true);
        assert_eq!(type_info(&t), hex_collated("EF 04 00"));
        assert_eq!(value(&t, &s("a")), hex("04 00 61 00 20 00"));
        assert_eq!(value(&t, &Value::Null), hex("FF FF"));
    }

    #[test]
    fn binary_vectors() {
        let t = ti(SqlType::VarBinary(Len::Fixed(8)), true);
        // No collation after the maximum length.
        assert_eq!(type_info(&t), hex("A5 08 00"));
        assert_eq!(
            value(&t, &Value::Bytes(vec![1, 2, 3])),
            hex("03 00 01 02 03")
        );
        assert_eq!(value(&t, &Value::Bytes(vec![])), hex("00 00"));
        assert_eq!(value(&t, &Value::Null), hex("FF FF"));

        let t = ti(SqlType::Binary(Len::Fixed(4)), false);
        assert_eq!(type_info(&t), hex("AD 04 00"));
        assert_eq!(value(&t, &Value::Bytes(vec![1])), hex("04 00 01 00 00 00"));
    }

    #[test]
    fn too_long_is_mismatch() {
        let t = ti(SqlType::VarChar(Len::Fixed(2)), true);
        assert!(matches!(
            value_err(&t, &s("abc")),
            TdsError::ValueTypeMismatch {
                expected: "varchar"
            }
        ));
        // The bound is in characters (UTF-16 units) for the Unicode types…
        let t = ti(SqlType::NVarChar(Len::Fixed(2)), true);
        assert_eq!(value(&t, &s("éé")), hex("04 00 E9 00 E9 00"));
        assert!(matches!(
            value_err(&t, &s("abc")),
            TdsError::ValueTypeMismatch {
                expected: "nvarchar"
            }
        ));
        // …and in bytes for the others.
        let t = ti(SqlType::Char(Len::Fixed(1)), true);
        assert!(matches!(
            value_err(&t, &s("ab")),
            TdsError::ValueTypeMismatch { expected: "char" }
        ));
        let t = ti(SqlType::Binary(Len::Fixed(1)), true);
        assert!(matches!(
            value_err(&t, &Value::Bytes(vec![1, 2])),
            TdsError::ValueTypeMismatch { expected: "binary" }
        ));
    }

    #[test]
    fn varchar_uses_cp1252_with_question_mark_fallback() {
        let t = ti(SqlType::VarChar(Len::Fixed(10)), true);
        assert_eq!(value(&t, &s("é€ы")), hex("03 00 E9 80 3F"));
        // Length is counted after transcoding: two characters, two bytes.
        let t = ti(SqlType::VarChar(Len::Fixed(2)), true);
        assert_eq!(value(&t, &s("ыы")), hex("02 00 3F 3F"));
    }

    #[test]
    fn nvarchar_uses_utf16le_with_surrogates() {
        let t = ti(SqlType::NVarChar(Len::Fixed(4)), true);
        // U+1F600 is one character but two UTF-16 units.
        assert_eq!(value(&t, &s("\u{1F600}")), hex("04 00 3D D8 00 DE"));
        let t = ti(SqlType::NVarChar(Len::Fixed(1)), true);
        assert!(matches!(
            value_err(&t, &s("\u{1F600}")),
            TdsError::ValueTypeMismatch {
                expected: "nvarchar"
            }
        ));
    }

    #[test]
    fn collation_comes_from_type_info() {
        let custom = Collation {
            lcid: 0x040C,
            flags: 0x00,
            version: 2,
            sort_id: 0,
        };
        let t = TypeInfo {
            ty: SqlType::VarChar(Len::Fixed(5)),
            nullable: true,
            collation: Some(custom),
        };
        assert_eq!(type_info(&t), hex("A7 05 00 0C 04 00 20 00"));

        // A string type without collation falls back to the default one.
        let t = TypeInfo {
            ty: SqlType::NChar(Len::Fixed(1)),
            nullable: true,
            collation: None,
        };
        assert_eq!(type_info(&t), hex_collated("EF 02 00"));

        // A collation on a binary type is ignored.
        let t = TypeInfo {
            ty: SqlType::Binary(Len::Fixed(1)),
            nullable: true,
            collation: Some(Collation::DEFAULT),
        };
        assert_eq!(type_info(&t), hex("AD 01 00"));
    }

    #[test]
    fn bounds_of_declared_length() {
        assert_eq!(
            type_info(&ti(SqlType::VarChar(Len::Fixed(8000)), true)),
            hex_collated("A7 40 1F")
        );
        assert_eq!(
            type_info(&ti(SqlType::NVarChar(Len::Fixed(4000)), true)),
            hex_collated("E7 40 1F")
        );
        assert_eq!(
            type_info(&ti(SqlType::VarBinary(Len::Fixed(8000)), true)),
            hex("A5 40 1F")
        );
        for ty in [
            SqlType::Char(Len::Fixed(0)),
            SqlType::VarChar(Len::Fixed(8001)),
            SqlType::NChar(Len::Fixed(0)),
            SqlType::NVarChar(Len::Fixed(4001)),
            SqlType::Binary(Len::Fixed(0)),
            SqlType::VarBinary(Len::Fixed(8001)),
        ] {
            assert!(
                matches!(type_info_err(&ti(ty, true)), TdsError::Malformed(_)),
                "{ty:?}"
            );
            assert!(
                matches!(
                    value_err(&ti(ty, true), &Value::Null),
                    TdsError::Malformed(_)
                ),
                "{ty:?}"
            );
        }
    }

    #[test]
    fn char_nchar_binary_have_no_max_form() {
        for ty in [
            SqlType::Char(Len::Max),
            SqlType::NChar(Len::Max),
            SqlType::Binary(Len::Max),
        ] {
            assert!(
                matches!(type_info_err(&ti(ty, true)), TdsError::Malformed(_)),
                "{ty:?}"
            );
            assert!(
                matches!(value_err(&ti(ty, true), &s("a")), TdsError::Malformed(_)),
                "{ty:?}"
            );
        }
    }

    #[test]
    fn null_in_not_nullable_and_wrong_variant() {
        let t = ti(SqlType::VarChar(Len::Fixed(2)), false);
        assert!(matches!(
            value_err(&t, &Value::Null),
            TdsError::NullInNotNullable
        ));
        assert!(matches!(
            value_err(&t, &Value::I32(1)),
            TdsError::ValueTypeMismatch {
                expected: "varchar"
            }
        ));
        assert!(matches!(
            value_err(&t, &Value::Bytes(vec![0x61])),
            TdsError::ValueTypeMismatch {
                expected: "varchar"
            }
        ));
        let t = ti(SqlType::VarBinary(Len::Fixed(2)), true);
        assert!(matches!(
            value_err(&t, &s("a")),
            TdsError::ValueTypeMismatch {
                expected: "varbinary"
            }
        ));
    }
}
