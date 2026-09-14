//! [MS-TDS] 2.2.7, token RETURNVALUE: encoder.
//!
//! Layout (TDS 7.2 and later): `TokenType` (0xAC), `ParamOrdinal` (USHORT), `ParamName`
//! (B_VARCHAR, with its leading `@`), `Status` (BYTE), `UserType` (ULONG), `Flags`
//! (USHORT, same bits as COLMETADATA), TYPE_INFO, then the value in the layout the
//! TYPE_INFO announces. TYPE_INFO and value come from [`crate::types::encode_type_info`]
//! and [`crate::types::encode_value`], so every type the crate encodes in a ROW can be
//! returned in a RETURNVALUE, PLP `(max)` and NULL included.
//!
//! `Status` is always 0x01 (an OUTPUT parameter): `Token::ReturnValue` has no field for
//! the 0x02 form (return value of a user-defined function), which the V1 does not emit.
//! `UserType` and `Flags` are written as 0, as SQL Server does for a plain OUTPUT
//! parameter; the nullability of the parameter is carried by its TYPE_INFO. SQL Server
//! sends one RETURNVALUE per OUTPUT parameter, after RETURNSTATUS and before the closing
//! DONEPROC (module documentation of `rpc.rs`).

use bytes::{BufMut, BytesMut};

use super::{EncodeContext, Token, put_b_varchar};
use crate::error::TdsError;
use crate::types::{encode_type_info, encode_value};

/// `TokenType` of RETURNVALUE.
const TOKEN_RETURNVALUE: u8 = 0xAC;
/// `Status` bit: the value is that of an OUTPUT parameter.
const STATUS_OUTPUT_PARAM: u8 = 0x01;
/// `UserType` of a parameter that is not a user-defined type.
const USER_TYPE_NONE: u32 = 0;
/// `Flags` of a plain OUTPUT parameter.
const FLAGS_NONE: u16 = 0;

/// Encodes `Token::ReturnValue` into `out`. Nothing is written when an error is returned
/// (a name beyond 255 characters, a `Value` that does not match `ty`…).
pub(crate) fn encode(
    token: &Token,
    _ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let Token::ReturnValue {
        name,
        ordinal,
        ty,
        value,
    } = token
    else {
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        unreachable!("return_value::encode called with a token other than ReturnValue");
    };

    let mut buf = BytesMut::new();
    buf.put_u8(TOKEN_RETURNVALUE);
    buf.put_u16_le(*ordinal);
    put_b_varchar(&mut buf, name)?;
    buf.put_u8(STATUS_OUTPUT_PARAM);
    buf.put_u32_le(USER_TYPE_NONE);
    buf.put_u16_le(FLAGS_NONE);
    encode_type_info(ty, &mut buf)?;
    encode_value(ty, value, &mut buf)?;
    out.put_slice(&buf);
    Ok(())
}

#[cfg(test)]
mod tests {
    use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

    use super::*;

    /// Parses `"AC 00 00"` into bytes.
    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    fn return_value(name: &str, ordinal: u16, ty: TypeInfo, value: Value) -> Token {
        Token::ReturnValue {
            name: name.into(),
            ordinal,
            ty,
            value,
        }
    }

    fn encode_one(token: &Token) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode(token, &mut EncodeContext::default(), &mut out).unwrap();
        out.to_vec()
    }

    #[test]
    fn return_value_vector() {
        let out = encode_one(&return_value(
            "@p",
            0,
            TypeInfo::new(SqlType::Int, true),
            Value::I32(7),
        ));
        assert_eq!(
            out,
            hex("AC 00 00 02 40 00 70 00 01 00 00 00 00 00 00 26 04 04 07 00 00 00")
        );
    }

    #[test]
    fn return_value_null_nvarchar() {
        let out = encode_one(&return_value(
            "@s",
            1,
            TypeInfo::new(SqlType::NVarChar(Len::Fixed(10)), true),
            Value::Null,
        ));
        assert!(out.ends_with(&[0xFF, 0xFF]), "CHARBIN_NULL, got {out:02X?}");
        // Ordinal 1, name "@s", status, UserType, Flags, then NVARCHARTYPE 20 bytes + collation.
        assert_eq!(&out[..3], &[0xAC, 0x01, 0x00]);
        assert_eq!(&out[3..8], &hex("02 40 00 73 00")[..]);
        assert_eq!(&out[8..15], &hex("01 00 00 00 00 00 00")[..]);
        assert_eq!(&out[15..18], &[0xE7, 0x14, 0x00]);
        assert_eq!(out.len(), 18 + 5 + 2);
    }

    #[test]
    fn return_value_nvarchar_text() {
        let out = encode_one(&return_value(
            "@s",
            2,
            TypeInfo::new(SqlType::NVarChar(Len::Fixed(10)), true),
            Value::String(SqlString { text: "ab".into() }),
        ));
        // Value: USHORT byte length 4, then "ab" in UTF-16LE.
        assert!(out.ends_with(&hex("04 00 61 00 62 00")));
    }

    #[test]
    fn return_value_nvarchar_max_null_is_plp_null() {
        let out = encode_one(&return_value(
            "@s",
            0,
            TypeInfo::new(SqlType::NVarChar(Len::Max), true),
            Value::Null,
        ));
        assert!(out.ends_with(&[0xFF; 8]), "PLP_NULL, got {out:02X?}");
    }

    #[test]
    fn return_value_empty_name_and_ordinal() {
        let out = encode_one(&return_value(
            "",
            0x0201,
            TypeInfo::new(SqlType::Bit, false),
            Value::Bit(true),
        ));
        assert_eq!(out, hex("AC 01 02 00 01 00 00 00 00 00 00 32 01"));
    }

    #[test]
    fn return_value_errors_write_nothing() {
        let mut ctx = EncodeContext::default();

        // A value that does not match its TYPE_INFO.
        let mut out = BytesMut::new();
        let err = encode(
            &return_value("@p", 0, TypeInfo::new(SqlType::Int, true), Value::Bit(true)),
            &mut ctx,
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, TdsError::ValueTypeMismatch { .. }), "{err:?}");
        assert!(out.is_empty());

        // NULL in a non-nullable TYPE_INFO.
        let mut out = BytesMut::new();
        let err = encode(
            &return_value("@p", 0, TypeInfo::new(SqlType::Int, false), Value::Null),
            &mut ctx,
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, TdsError::NullInNotNullable), "{err:?}");
        assert!(out.is_empty());

        // A name beyond the B_VARCHAR limit.
        let mut out = BytesMut::new();
        let err = encode(
            &return_value(
                &"x".repeat(256),
                0,
                TypeInfo::new(SqlType::Int, true),
                Value::I32(1),
            ),
            &mut ctx,
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, TdsError::Malformed(_)), "{err:?}");
        assert!(out.is_empty());
    }
}
