use vauban_catalog::ColumnMeta;
use vauban_errors::SqlResult;
use vauban_types::{TypeInfo, Value, convert};

/// Assigns `value`, read as type `from`, to column `col`, converting it implicitly to
/// the type of the column. The conversion raises the errors `types::convert` propagates:
/// 8152 for a string or binary truncation, 245 for a refused conversion, 220 and 8115
/// for an overflow.
pub(crate) fn assign_value(value: Value, from: &TypeInfo, col: &ColumnMeta) -> SqlResult<Value> {
    if matches!(&value, Value::Null) {
        return Ok(Value::Null);
    }
    convert(&value, from, &col.ty, None)
}
