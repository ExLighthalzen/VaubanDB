//! `CAST` and `CONVERT`: the entry point and its dispatch on the target family.
//!
//! The rules themselves live in one submodule per target family; this module routes.

mod binary;
mod datetime;
mod from_character;
mod numeric;
mod to_character;

use vauban_errors::SqlResult;

use crate::{TypeFamily, TypeInfo, Value};

/// Converts `v`, read as type `from`, to type `to`, under the optional `CONVERT` style.
///
/// `NULL` converts to `NULL` whatever the target: the nullability of `to` is the caller's
/// business (a `NOT NULL` column rejects the value at insert time, not here).
///
/// The source type is a parameter because it is not deducible from the value:
/// [`Value::String`] does not say whether it came from `varchar` or `nvarchar` (error 245
/// names one or the other), [`Value::DateTime`] serves both `datetime` and
/// `smalldatetime`, [`Value::Bytes`] both `binary` and `varbinary`, and the collation of a
/// string-to-string result is the collation of the source.
///
/// `style` is the third argument of `CONVERT`; `CAST` passes `None`. A style that means
/// nothing for the pair of types raises error 9809.
pub fn convert(v: &Value, from: &TypeInfo, to: &TypeInfo, style: Option<i32>) -> SqlResult<Value> {
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    match to.ty.family() {
        TypeFamily::Bit
        | TypeFamily::Integer
        | TypeFamily::ExactNumeric
        | TypeFamily::ApproxNumeric
        | TypeFamily::Money => numeric::to_numeric(v, from, to, style),
        TypeFamily::Character => to_character::to_character(v, from, to, style),
        TypeFamily::DateTime => datetime::to_datetime(v, from, to, style),
        TypeFamily::Binary => binary::to_binary(v, from, to, style),
        TypeFamily::Guid => binary::to_guid(v, from, style),
    }
}
