use vauban_catalog::ColumnMeta;
use vauban_errors::{SqlError, SqlResult};
use vauban_types::{Len, SqlType, TypeInfo, Value, convert};

/// Assigns `value`, read as type `from`, to column `col`, converting it implicitly to
/// the type of the column. The conversion raises the errors `types::convert` propagates:
/// 245 for a refused conversion, 220 and 8115 for an overflow. A value too long for a
/// fixed `varchar`, `nvarchar` or `varbinary` column raises 2628 state 1; a `NULL` on a
/// column that refuses it raises 515.
pub(crate) fn assign_value(
    value: Value,
    from: &TypeInfo,
    col: &ColumnMeta,
    table_name: &str,
    statement: &str,
) -> SqlResult<Value> {
    if matches!(&value, Value::Null) {
        if !col.ty.nullable {
            return Err(SqlError::cannot_insert_null(
                &col.name, table_name, statement,
            ));
        }
        return Ok(Value::Null);
    }
    refuse_truncation(&value, from, col, table_name)?;
    convert(&value, from, &col.ty, None)
}

/// Refuses an assignment that would silently cut a string or binary value to the column's
/// length. `value` and `from` are what the caller means to write, at the length it still
/// has: a written `CAST`/`CONVERT` has already narrowed both to the type it named, so
/// `from` reports that narrower length here, and assigning its result to a still shorter
/// column keeps raising 2628.
fn refuse_truncation(
    value: &Value,
    from: &TypeInfo,
    col: &ColumnMeta,
    table_name: &str,
) -> SqlResult<()> {
    let max_len = match &col.ty.ty {
        SqlType::VarChar(Len::Fixed(n)) | SqlType::NVarChar(Len::Fixed(n)) => {
            if !from.ty.is_string() {
                return Ok(());
            }
            (*n, Payload::Chars)
        }
        SqlType::VarBinary(Len::Fixed(n)) => {
            if !matches!(from.ty, SqlType::Binary(_) | SqlType::VarBinary(_)) {
                return Ok(());
            }
            (*n, Payload::Bytes)
        }
        _ => return Ok(()),
    };

    let unbounded = unbounded_type(&col.ty);
    let full = convert(value, from, &unbounded, None)?;
    let limit = usize::from(max_len.0);
    let (too_long, truncated_display) = match (max_len.1, &full) {
        (Payload::Chars, Value::String(s)) => {
            // Trailing spaces past the column's length are cut in silence, like a
            // `CAST`: what is left once they are trimmed decides whether the value is
            // too long. `'ABCDEFGHIJ   '` into a `varchar(10)` writes `'ABCDEFGHIJ'`,
            // no error, because the ten letters alone already fit.
            let content = s.text.trim_end_matches(' ').chars().count();
            (
                content > limit,
                s.text.chars().take(limit).collect::<String>(),
            )
        }
        (Payload::Bytes, Value::Bytes(b)) => {
            // Trailing zero bytes past the column's length are cut in silence, the
            // binary counterpart of the trailing spaces above: `0x01020000` into a
            // `varbinary(2)` writes `0x0102`, no error, because the two significant
            // bytes alone already fit.
            let trailing_zeros = b.iter().rev().take_while(|&&byte| byte == 0).count();
            let content = b.len() - trailing_zeros;
            (content > limit, String::new())
        }
        _ => return Ok(()),
    };
    if too_long {
        return Err(SqlError::string_or_binary_data_truncated(
            table_name,
            &col.name,
            &truncated_display,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Payload {
    Chars,
    Bytes,
}

fn unbounded_type(col: &TypeInfo) -> TypeInfo {
    let ty = match col.ty {
        SqlType::VarChar(_) => SqlType::VarChar(Len::Max),
        SqlType::NVarChar(_) => SqlType::NVarChar(Len::Max),
        SqlType::VarBinary(_) => SqlType::VarBinary(Len::Max),
        _ => return col.clone(),
    };
    TypeInfo {
        ty,
        nullable: col.nullable,
        collation: col.collation,
    }
}
