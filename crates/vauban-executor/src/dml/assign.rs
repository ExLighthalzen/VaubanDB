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

/// Refuses an assignment that would silently cut a string or binary value to the column
/// length. Explicit `CAST`/`CONVERT` already applied the cut in `types::convert` before
/// the value reaches here.
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
            let count = s.text.chars().count();
            (
                count > limit,
                s.text.chars().take(limit).collect::<String>(),
            )
        }
        (Payload::Bytes, Value::Bytes(b)) => (b.len() > limit, String::new()),
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
