//! [MS-TDS] 2.2.7, tokens ROW and NBCROW: encoder.
//!
//! ROW: `TokenType` (0xD1) then every value in the layout announced by the TYPE_INFO of its
//! column in the last COLMETADATA. NBCROW (Null Bitmap Compressed Row): `TokenType` (0xD2),
//! a bitmap of `ceil(n / 8)` bytes where bit `i % 8` (least significant first) of byte
//! `i / 8` is set when column `i` is NULL, then the values of the non-NULL columns only.
//!
//! `Token::Row` is encoded as NBCROW as soon as one value is `Value::Null`, as ROW otherwise;
//! clients accept both in the same result set.

use bytes::{BufMut, BytesMut};
use vauban_types::Value;

use super::{ColumnMeta, EncodeContext, Token};
use crate::error::TdsError;
use crate::types::encode_value;

/// `TokenType` of ROW.
const TOKEN_ROW: u8 = 0xD1;
/// `TokenType` of NBCROW.
const TOKEN_NBCROW: u8 = 0xD2;

/// Encodes `Token::Row` as ROW or NBCROW into `out`, using the columns of `ctx`.
///
/// Fails with `RowWithoutMetadata` when the row carries values but no COLMETADATA was
/// encoded before, `ColumnCountMismatch` when the value count differs from the column count,
/// `NullInNotNullable` for a `Value::Null` in a column whose TYPE_INFO is not nullable, and
/// with the error of `encode_value` for a value that does not match its column. Nothing is
/// written when an error is returned.
pub(crate) fn encode(
    token: &Token,
    ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let Token::Row(values) = token else {
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        unreachable!("row::encode called with a token other than Row");
    };

    let columns = &ctx.columns;
    if values.len() != columns.len() {
        // `EncodeContext.columns` is empty before the first COLMETADATA: a row that carries
        // values against no known column is a row without metadata.
        if columns.is_empty() {
            return Err(TdsError::RowWithoutMetadata);
        }
        return Err(TdsError::ColumnCountMismatch {
            expected: columns.len(),
            got: values.len(),
        });
    }
    for (column, value) in columns.iter().zip(values) {
        if matches!(value, Value::Null) && !column.ty.nullable {
            return Err(TdsError::NullInNotNullable);
        }
    }

    let start = out.len();
    let result = if values.iter().any(|v| matches!(v, Value::Null)) {
        encode_nbcrow(columns, values, out)
    } else {
        encode_row(columns, values, out)
    };
    if result.is_err() {
        out.truncate(start);
    }
    result
}

/// Writes a ROW: the token then every value.
fn encode_row(
    columns: &[ColumnMeta],
    values: &[Value],
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    out.put_u8(TOKEN_ROW);
    for (column, value) in columns.iter().zip(values) {
        encode_value(&column.ty, value, out)?;
    }
    Ok(())
}

/// Writes an NBCROW: the token, the null bitmap, then the non-NULL values.
fn encode_nbcrow(
    columns: &[ColumnMeta],
    values: &[Value],
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    out.put_u8(TOKEN_NBCROW);
    let mut bitmap = vec![0u8; values.len().div_ceil(8)];
    for (i, value) in values.iter().enumerate() {
        if matches!(value, Value::Null) {
            bitmap[i / 8] |= 1 << (i % 8);
        }
    }
    out.put_slice(&bitmap);
    for (column, value) in columns.iter().zip(values) {
        if !matches!(value, Value::Null) {
            encode_value(&column.ty, value, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use vauban_types::{Len, SqlString, SqlType, TypeInfo};

    use super::*;
    use crate::tokens::ColumnFlags;

    /// Parses `"D1 01 00"` into bytes.
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

    fn s(text: &str) -> Value {
        Value::String(SqlString { text: text.into() })
    }

    fn ctx_with(columns: Vec<ColumnMeta>) -> EncodeContext {
        EncodeContext { columns }
    }

    fn row(ctx: &mut EncodeContext, values: Vec<Value>) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode(&Token::Row(values), ctx, &mut out).unwrap();
        out.to_vec()
    }

    fn row_err(ctx: &mut EncodeContext, values: Vec<Value>) -> TdsError {
        let mut out = BytesMut::new();
        let err = encode(&Token::Row(values), ctx, &mut out).unwrap_err();
        assert!(out.is_empty(), "nothing must be written on error");
        err
    }

    #[test]
    fn row_without_null_is_row() {
        let mut ctx = ctx_with(vec![col("c", SqlType::Int, true)]);
        assert_eq!(row(&mut ctx, vec![Value::I32(1)]), hex("D1 04 01 00 00 00"));

        let mut ctx = ctx_with(vec![col("", SqlType::Int, false)]);
        assert_eq!(row(&mut ctx, vec![Value::I32(1)]), hex("D1 01 00 00 00"));
    }

    #[test]
    fn row_with_null_is_nbcrow() {
        let mut ctx = ctx_with(vec![
            col("a", SqlType::Int, true),
            col("b", SqlType::Int, true),
        ]);
        // Bit 0 = column 0 is NULL; NULL columns write nothing after the bitmap.
        assert_eq!(
            row(&mut ctx, vec![Value::Null, Value::I32(7)]),
            hex("D2 01 04 07 00 00 00")
        );
        assert_eq!(
            row(&mut ctx, vec![Value::I32(7), Value::Null]),
            hex("D2 02 04 07 00 00 00")
        );
        assert_eq!(row(&mut ctx, vec![Value::Null, Value::Null]), hex("D2 03"));
    }

    #[test]
    fn nbcrow_bitmap_nine_columns() {
        let columns = (0..9)
            .map(|i| col(&format!("c{i}"), SqlType::Int, true))
            .collect();
        let mut ctx = ctx_with(columns);
        let mut values = vec![Value::I32(1); 8];
        values.push(Value::Null);
        let out = row(&mut ctx, values);
        assert_eq!(&out[..3], &hex("D2 00 01")[..]);
        assert_eq!(out.len(), 3 + 8 * 5);
        assert_eq!(&out[3..8], &hex("04 01 00 00 00")[..]);

        // Eight columns need a single byte; the eighth column is bit 7.
        let mut ctx = ctx_with(
            (0..8)
                .map(|i| col(&format!("c{i}"), SqlType::Int, true))
                .collect(),
        );
        let mut values = vec![Value::I32(1); 7];
        values.push(Value::Null);
        let out = row(&mut ctx, values);
        assert_eq!(&out[..2], &hex("D2 80")[..]);
        assert_eq!(out.len(), 2 + 7 * 5);
    }

    #[test]
    fn row_mixed_types() {
        let mut ctx = ctx_with(vec![
            col("i", SqlType::Int, true),
            col("n", SqlType::NVarChar(Len::Max), true),
            col("b", SqlType::Bit, false),
        ]);
        assert_eq!(
            row(&mut ctx, vec![Value::I32(1), s("ab"), Value::Bit(true)]),
            hex("D1 04 01 00 00 00 \
                 04 00 00 00 00 00 00 00  04 00 00 00  61 00 62 00  00 00 00 00 \
                 01")
        );
        // The same columns with a NULL in the middle: NBCROW, the PLP value is skipped.
        assert_eq!(
            row(
                &mut ctx,
                vec![Value::I32(1), Value::Null, Value::Bit(false)]
            ),
            hex("D2 02  04 01 00 00 00  00")
        );
    }

    #[test]
    fn row_errors() {
        let mut ctx = EncodeContext::default();
        assert!(matches!(
            row_err(&mut ctx, vec![Value::I32(1)]),
            TdsError::RowWithoutMetadata
        ));

        let mut ctx = ctx_with(vec![
            col("a", SqlType::Int, true),
            col("b", SqlType::Int, true),
        ]);
        assert!(matches!(
            row_err(&mut ctx, vec![Value::I32(1)]),
            TdsError::ColumnCountMismatch {
                expected: 2,
                got: 1
            }
        ));
        assert!(matches!(
            row_err(&mut ctx, vec![Value::I32(1); 3]),
            TdsError::ColumnCountMismatch {
                expected: 2,
                got: 3
            }
        ));

        let mut ctx = ctx_with(vec![
            col("a", SqlType::Int, true),
            col("b", SqlType::Int, false),
        ]);
        assert!(matches!(
            row_err(&mut ctx, vec![Value::I32(1), Value::Null]),
            TdsError::NullInNotNullable
        ));

        // A value of the wrong type in the second column: the error of `encode_value`,
        // and the bytes of the first column are rolled back.
        assert!(matches!(
            row_err(&mut ctx, vec![Value::I32(1), Value::Bit(true)]),
            TdsError::ValueTypeMismatch { .. }
        ));
        assert!(matches!(
            row_err(&mut ctx, vec![Value::Null, Value::Bit(true)]),
            TdsError::ValueTypeMismatch { .. }
        ));
    }

    #[test]
    fn row_does_not_touch_context() {
        let columns = vec![col("c", SqlType::Int, true)];
        let mut ctx = ctx_with(columns.clone());
        row(&mut ctx, vec![Value::I32(1)]);
        row(&mut ctx, vec![Value::Null]);
        assert_eq!(ctx.columns, columns);
    }

    #[test]
    fn select_1_full_response() {
        // [MS-TDS] chapter 4, "SQL Batch Server Response": COLMETADATA, ROW, DONE for
        // `SELECT 1`.
        use crate::tokens::{DoneStatus, encode_tokens};

        let mut column = col("", SqlType::Int, false);
        column.flags = ColumnFlags::default();
        let tokens = [
            Token::ColMetaData(vec![column]),
            Token::Row(vec![Value::I32(1)]),
            Token::Done {
                status: DoneStatus::COUNT,
                cur_cmd: 0xC1,
                row_count: Some(1),
            },
        ];
        let mut ctx = EncodeContext::default();
        let mut out = BytesMut::new();
        encode_tokens(&tokens, &mut ctx, &mut out).unwrap();
        assert_eq!(
            out.to_vec(),
            hex("81 01 00 00 00 00 00 00 00 38 00 \
                 D1 01 00 00 00 \
                 FD 10 00 C1 00 01 00 00 00 00 00 00 00")
        );
    }

    #[test]
    fn empty_row_against_no_columns_is_bare_row() {
        // `EncodeContext` cannot tell "no COLMETADATA yet" from "COLMETADATA with zero
        // columns"; a row with no value against no column is a bare ROW token.
        let mut ctx = EncodeContext::default();
        assert_eq!(row(&mut ctx, vec![]), hex("D1"));
    }
}
