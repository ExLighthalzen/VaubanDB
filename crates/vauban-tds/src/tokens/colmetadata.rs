//! [MS-TDS] 2.2.7, token COLMETADATA: encoder.
//!
//! Layout (TDS 7.4): `TokenType` (0x81), `Count` (USHORT), then one `ColumnData` per column:
//! `UserType` (ULONG since TDS 7.2), `Flags` (USHORT), `TYPE_INFO`, `ColName` (B_VARCHAR).
//! The `TableName` that follows the TYPE_INFO of `text` / `ntext` / `image` never appears:
//! those types do not exist in `SqlType`.

use bytes::{BufMut, BytesMut};

use super::{ColumnFlags, ColumnMeta, EncodeContext, Token, put_b_varchar};
use crate::error::TdsError;
use crate::types::encode_type_info;

/// `TokenType` of COLMETADATA.
const TOKEN_COLMETADATA: u8 = 0x81;
/// `Count` value meaning "no metadata" ([MS-TDS] 2.2.7, COLMETADATA, `Count` rule).
const NO_METADATA: u16 = 0xFFFF;
/// `UserType`: 0 for every column, the value SQL Server sends for plain result sets.
const USER_TYPE: u32 = 0;

/// `Flags` bit `fNullable`.
const FLAG_NULLABLE: u16 = 0x0001;
/// `Flags` bit `fCaseSen`.
const FLAG_CASE_SENSITIVE: u16 = 0x0002;
/// `Flags` field `usUpdateable` (2 bits) set to 1 (read/write). The spec lists the bits in
/// transmission order, which puts `usUpdateable = 1` at 0x0008, not 0x0004: a nullable
/// read/write `int` column (`SELECT c FROM t`) carries `Flags = 09 00`.
const FLAG_UPDATABLE: u16 = 0x0008;
/// `Flags` bit `fIdentity`.
const FLAG_IDENTITY: u16 = 0x0010;
/// `Flags` bit `fComputed`.
const FLAG_COMPUTED: u16 = 0x0020;

/// Encodes `Token::ColMetaData` into `out` and records the columns in `ctx.columns`,
/// replacing the previous ones.
///
/// An empty column list is encoded as `Count = 0xFFFF` ("no metadata"). More than 65534
/// columns fail with `Malformed`. Nothing is written and `ctx` is left untouched when an
/// error is returned.
pub(crate) fn encode(
    token: &Token,
    ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let Token::ColMetaData(columns) = token else {
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        unreachable!("colmetadata::encode called with a token other than ColMetaData");
    };

    let start = out.len();
    if let Err(err) = encode_columns(columns, out) {
        out.truncate(start);
        return Err(err);
    }
    ctx.columns = columns.clone();
    Ok(())
}

/// Writes the token and its `ColumnData` entries.
fn encode_columns(columns: &[ColumnMeta], out: &mut BytesMut) -> Result<(), TdsError> {
    let count = match columns.len() {
        0 => NO_METADATA,
        n => match u16::try_from(n) {
            Ok(count) if count != NO_METADATA => count,
            _ => {
                return Err(TdsError::Malformed(
                    "COLMETADATA with more than 65534 columns",
                ));
            }
        },
    };

    out.put_u8(TOKEN_COLMETADATA);
    out.put_u16_le(count);
    for column in columns {
        out.put_u32_le(USER_TYPE);
        out.put_u16_le(flags_bits(column.flags));
        encode_type_info(&column.ty, out)?;
        put_b_varchar(out, &column.name)?;
    }
    Ok(())
}

/// Builds the `Flags` field. `fNullable` comes from `flags.nullable`, not from
/// `ty.nullable`: keeping the two consistent is the producer's job.
fn flags_bits(flags: ColumnFlags) -> u16 {
    let mut bits = 0;
    if flags.nullable {
        bits |= FLAG_NULLABLE;
    }
    if flags.case_sensitive {
        bits |= FLAG_CASE_SENSITIVE;
    }
    if flags.updatable {
        bits |= FLAG_UPDATABLE;
    }
    if flags.identity {
        bits |= FLAG_IDENTITY;
    }
    if flags.computed {
        bits |= FLAG_COMPUTED;
    }
    bits
}

#[cfg(test)]
mod tests {
    use vauban_types::{Len, SqlType, TypeInfo};

    use super::*;

    /// Parses `"81 01 00"` into bytes.
    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    fn col(name: &str, ty: SqlType, nullable: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            ty: TypeInfo::new(ty, nullable),
            flags: ColumnFlags {
                nullable,
                updatable: true,
                ..ColumnFlags::default()
            },
        }
    }

    fn encode_with(columns: Vec<ColumnMeta>, ctx: &mut EncodeContext) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode(&Token::ColMetaData(columns), ctx, &mut out).unwrap();
        out.to_vec()
    }

    fn encode_one(columns: Vec<ColumnMeta>) -> Vec<u8> {
        encode_with(columns, &mut EncodeContext::default())
    }

    #[test]
    fn colmetadata_select_1() {
        let mut column = col("", SqlType::Int, false);
        column.flags = ColumnFlags::default();
        assert_eq!(
            encode_one(vec![column]),
            hex("81 01 00  00 00 00 00  00 00  38  00")
        );
    }

    #[test]
    fn colmetadata_nullable_int_named() {
        assert_eq!(
            encode_one(vec![col("c", SqlType::Int, true)]),
            hex("81 01 00  00 00 00 00  09 00  26 04  01 63 00")
        );
    }

    #[test]
    fn colmetadata_nvarchar() {
        assert_eq!(
            encode_one(vec![col("n", SqlType::NVarChar(Len::Fixed(10)), true)]),
            hex("81 01 00  00 00 00 00  09 00  E7 14 00 09 04 D0 00 34  01 6E 00")
        );
    }

    #[test]
    fn colmetadata_flags_bits() {
        let flags = |f: ColumnFlags| -> u16 {
            let mut column = col("x", SqlType::Int, false);
            column.flags = f;
            let out = encode_one(vec![column]);
            u16::from_le_bytes([out[7], out[8]])
        };
        let none = ColumnFlags::default();
        assert_eq!(flags(none), 0x0000);
        assert_eq!(
            flags(ColumnFlags {
                nullable: true,
                ..none
            }),
            0x0001
        );
        assert_eq!(
            flags(ColumnFlags {
                case_sensitive: true,
                ..none
            }),
            0x0002
        );
        assert_eq!(
            flags(ColumnFlags {
                updatable: true,
                ..none
            }),
            0x0008
        );
        assert_eq!(
            flags(ColumnFlags {
                identity: true,
                ..none
            }),
            0x0010
        );
        assert_eq!(
            flags(ColumnFlags {
                computed: true,
                ..none
            }),
            0x0020
        );
        // `updatable: false` clears both bits of `usUpdateable`.
        assert_eq!(
            flags(ColumnFlags {
                nullable: true,
                identity: true,
                updatable: false,
                ..none
            }),
            0x0011
        );
        assert_eq!(
            flags(ColumnFlags {
                nullable: true,
                case_sensitive: true,
                updatable: true,
                identity: true,
                computed: true,
            }),
            0x003B
        );
    }

    #[test]
    fn colmetadata_updates_context() {
        let mut ctx = EncodeContext::default();
        let first = vec![col("a", SqlType::Int, true), col("b", SqlType::Bit, false)];
        encode_with(first.clone(), &mut ctx);
        assert_eq!(ctx.columns, first);

        // A second COLMETADATA replaces the previous columns.
        let second = vec![col("n", SqlType::NVarChar(Len::Max), true)];
        encode_with(second.clone(), &mut ctx);
        assert_eq!(ctx.columns, second);
    }

    #[test]
    fn colmetadata_two_columns() {
        assert_eq!(
            encode_one(vec![
                col("a", SqlType::Int, true),
                col("bb", SqlType::Bit, false),
            ]),
            hex("81 02 00 \
                 00 00 00 00  09 00  26 04  01 61 00 \
                 00 00 00 00  08 00  32  02 62 00 62 00")
        );
    }

    #[test]
    fn colmetadata_empty_is_no_metadata() {
        let mut ctx = EncodeContext {
            columns: vec![col("a", SqlType::Int, true)],
        };
        assert_eq!(encode_with(vec![], &mut ctx), hex("81 FF FF"));
        assert!(ctx.columns.is_empty());
    }

    #[test]
    fn colmetadata_errors_write_nothing() {
        // A column name longer than a B_VARCHAR allows.
        let previous = vec![col("keep", SqlType::Int, true)];
        let mut ctx = EncodeContext {
            columns: previous.clone(),
        };
        let mut out = BytesMut::new();
        let token = Token::ColMetaData(vec![col(&"x".repeat(256), SqlType::Int, true)]);
        assert!(matches!(
            encode(&token, &mut ctx, &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
        assert_eq!(ctx.columns, previous);

        // A TYPE_INFO that cannot be encoded (`char(max)` does not exist).
        let token = Token::ColMetaData(vec![col("c", SqlType::Char(Len::Max), true)]);
        assert!(matches!(
            encode(&token, &mut ctx, &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
        assert_eq!(ctx.columns, previous);

        // Too many columns for the `Count` field.
        let many = vec![col("c", SqlType::Int, true); 0xFFFF];
        assert!(matches!(
            encode(&Token::ColMetaData(many), &mut ctx, &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
        assert_eq!(ctx.columns, previous);
    }
}
