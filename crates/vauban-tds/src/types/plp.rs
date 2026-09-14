//! [MS-TDS] 2.2.5.4.3 Partially Length-Prefixed Data Types (PARTLENTYPE) and 2.2.5.2.3
//! Partially Length-Prefixed Bytes: TYPE_INFO and value encoding of `varchar(max)`,
//! `nvarchar(max)` and `varbinary(max)`.
//!
//! # TYPE_INFO ([MS-TDS] 2.2.5.6)
//!
//! The same tokens as the USHORTLEN forms (BIGVARCHRTYPE, NVARCHARTYPE, BIGVARBINTYPE), a
//! maximum length of `0xFFFF` ([`PLP_MAX_LEN`]) that announces the PLP layout, then the
//! 5-byte collation for the two character types. The data itself is transcoded exactly as in
//! [`super::ushortlen`] (code page 1252, UTF-16LE, raw bytes).
//!
//! # Values ([MS-TDS] 2.2.5.2.3)
//!
//! ```text
//! PLP_BODY = PLP_NULL
//!          | total length (u64 LE)  { chunk length (u32 LE)  chunk data }*  00 00 00 00
//! ```
//!
//! The total length is the number of data bytes ([`UNKNOWN_PLP_LEN`] when the sender does not
//! know it yet; this encoder always knows it and never emits it). Chunks are at most
//! [`PLP_CHUNK`] bytes and never empty; a zero chunk length is the terminator, which is
//! written even for an empty value (total length `0`, no chunk, terminator). NULL is
//! [`PLP_NULL`] alone. A value longer than 2^31 - 1 bytes, the limit of the `(max)` types, is
//! refused with `ValueTypeMismatch`.

use bytes::{BufMut, BytesMut};
use vauban_types::{Collation, Len, SqlType, TypeInfo, Value};

use super::collation::encode_collation;
use super::ushortlen::{Encoding, encode_payload, mismatch};
use super::{BIGVARBINTYPE, BIGVARCHRTYPE, NVARCHARTYPE};
use crate::error::TdsError;

/// [MS-TDS] 2.2.5.4.3: the maximum length that announces a PLP type in a TYPE_INFO.
pub(crate) const PLP_MAX_LEN: u16 = 0xFFFF;
/// [MS-TDS] 2.2.5.2.3 PLP_NULL: the total length that stands for a NULL value.
pub(crate) const PLP_NULL: u64 = 0xFFFF_FFFF_FFFF_FFFF;
/// [MS-TDS] 2.2.5.2.3 UNKNOWN_PLP_LEN: the total length of a stream whose size is not known
/// in advance. Never emitted by this encoder; the decoder (`decode.rs`) accepts it.
pub(crate) const UNKNOWN_PLP_LEN: u64 = 0xFFFF_FFFF_FFFF_FFFE;
/// [MS-TDS] 2.2.5.2.3 PLP_TERMINATOR: a chunk length of zero closes the value.
pub(crate) const PLP_TERMINATOR: u32 = 0;
/// Largest chunk this encoder writes, in bytes.
pub(crate) const PLP_CHUNK: usize = 8000;
/// Largest value of a `(max)` type, in bytes: 2^31 - 1.
pub(crate) const PLP_MAX_VALUE_LEN: usize = 0x7FFF_FFFF;

/// The wire form of a PLP type.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wire {
    /// Type token.
    token: u8,
    /// Data layout.
    encoding: Encoding,
    /// Collation written after the maximum length, for the character types.
    collation: Option<Collation>,
}

/// Chooses the wire form of `ti`.
fn wire(ti: &TypeInfo) -> Result<Wire, TdsError> {
    let (token, encoding) = match ti.ty {
        SqlType::VarChar(Len::Max) => (BIGVARCHRTYPE, Encoding::Cp1252),
        SqlType::NVarChar(Len::Max) => (NVARCHARTYPE, Encoding::Utf16Le),
        SqlType::VarBinary(Len::Max) => (BIGVARBINTYPE, Encoding::Raw),
        _ => {
            // `types::family` never routes these here; kept as an error rather than a panic.
            return Err(TdsError::Malformed(
                "type is not a PLP (max) string or binary type",
            ));
        }
    };
    let collation = ti
        .ty
        .is_string()
        .then(|| ti.collation.unwrap_or(Collation::DEFAULT));
    Ok(Wire {
        token,
        encoding,
        collation,
    })
}

/// Appends the TYPE_INFO of `ti` to `out` ([MS-TDS] 2.2.5.6).
pub(crate) fn encode_type_info(ti: &TypeInfo, out: &mut BytesMut) -> Result<(), TdsError> {
    let wire = wire(ti)?;
    out.put_u8(wire.token);
    out.put_u16_le(PLP_MAX_LEN);
    if let Some(collation) = wire.collation {
        out.put_slice(&encode_collation(&collation));
    }
    Ok(())
}

/// Appends `value` to `out` as a PLP_BODY ([MS-TDS] 2.2.5.2.3). Nothing is written when an
/// error is returned.
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
        out.put_u64_le(PLP_NULL);
        return Ok(());
    }
    let payload = encode_payload(ti.ty, wire.encoding, value)?;
    if payload.len() > PLP_MAX_VALUE_LEN {
        return Err(mismatch(ti.ty));
    }
    out.put_u64_le(payload.len() as u64);
    for chunk in payload.chunks(PLP_CHUNK) {
        // `chunk.len() <= PLP_CHUNK`: the cast cannot truncate.
        out.put_u32_le(chunk.len() as u32);
        out.put_slice(chunk);
    }
    out.put_u32_le(PLP_TERMINATOR);
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value};

    use super::{PLP_NULL, UNKNOWN_PLP_LEN};
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

    /// Parses `"E7 FF FF"` into bytes.
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

    #[test]
    fn plp_known_length() {
        let t = ti(SqlType::NVarChar(Len::Max), true);
        assert_eq!(type_info(&t), hex("E7 FF FF 09 04 D0 00 34"));
        assert_eq!(
            value(&t, &s("ab")),
            hex("04 00 00 00 00 00 00 00  04 00 00 00  61 00 62 00  00 00 00 00")
        );
    }

    #[test]
    fn plp_null_and_empty() {
        let t = ti(SqlType::NVarChar(Len::Max), true);
        assert_eq!(value(&t, &Value::Null), hex("FF FF FF FF FF FF FF FF"));
        assert_eq!(
            value(&t, &s("")),
            hex("00 00 00 00 00 00 00 00  00 00 00 00")
        );
        let t = ti(SqlType::VarBinary(Len::Max), true);
        assert_eq!(
            value(&t, &Value::Bytes(vec![])),
            hex("00 00 00 00 00 00 00 00  00 00 00 00")
        );
    }

    #[test]
    fn plp_chunks() {
        let t = ti(SqlType::VarBinary(Len::Max), true);
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let out = value(&t, &Value::Bytes(data.clone()));

        let mut expected = hex("10 27 00 00 00 00 00 00");
        expected.extend_from_slice(&hex("40 1F 00 00"));
        expected.extend_from_slice(&data[..8000]);
        expected.extend_from_slice(&hex("D0 07 00 00"));
        expected.extend_from_slice(&data[8000..]);
        expected.extend_from_slice(&hex("00 00 00 00"));
        assert_eq!(out, expected);

        // Exactly one full chunk: no empty second chunk before the terminator.
        let data = vec![0xABu8; 8000];
        let out = value(&t, &Value::Bytes(data.clone()));
        let mut expected = hex("40 1F 00 00 00 00 00 00  40 1F 00 00");
        expected.extend_from_slice(&data);
        expected.extend_from_slice(&hex("00 00 00 00"));
        assert_eq!(out, expected);
    }

    #[test]
    fn varchar_max_and_varbinary_max_type_info() {
        let t = ti(SqlType::VarChar(Len::Max), false);
        assert_eq!(type_info(&t), hex("A7 FF FF 09 04 D0 00 34"));
        assert_eq!(
            value(&t, &s("é€ы")),
            hex("03 00 00 00 00 00 00 00  03 00 00 00  E9 80 3F  00 00 00 00")
        );

        let t = ti(SqlType::VarBinary(Len::Max), true);
        assert_eq!(type_info(&t), hex("A5 FF FF"));

        // Collation comes from the TypeInfo; `None` on a string type means the default.
        let t = TypeInfo {
            ty: SqlType::NVarChar(Len::Max),
            nullable: true,
            collation: Some(Collation {
                lcid: 0x040C,
                flags: 0x00,
                version: 2,
                sort_id: 0,
            }),
        };
        assert_eq!(type_info(&t), hex("E7 FF FF 0C 04 00 20 00"));
        let t = TypeInfo {
            ty: SqlType::VarChar(Len::Max),
            nullable: true,
            collation: None,
        };
        let mut expected = hex("A7 FF FF");
        expected.extend_from_slice(&DEFAULT_COLLATION_BYTES);
        assert_eq!(type_info(&t), expected);
    }

    #[test]
    fn plp_errors() {
        let t = ti(SqlType::VarChar(Len::Max), false);
        assert!(matches!(
            value_err(&t, &Value::Null),
            TdsError::NullInNotNullable
        ));
        assert!(matches!(
            value_err(&t, &Value::Bytes(vec![1])),
            TdsError::ValueTypeMismatch {
                expected: "varchar"
            }
        ));
        let t = ti(SqlType::VarBinary(Len::Max), true);
        assert!(matches!(
            value_err(&t, &s("a")),
            TdsError::ValueTypeMismatch {
                expected: "varbinary"
            }
        ));
        assert!(matches!(
            value_err(&t, &Value::I32(1)),
            TdsError::ValueTypeMismatch {
                expected: "varbinary"
            }
        ));
    }

    #[test]
    fn plp_sentinels() {
        assert_eq!(PLP_NULL.to_le_bytes(), [0xFF; 8]);
        assert_eq!(
            UNKNOWN_PLP_LEN.to_le_bytes(),
            [0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }
}
