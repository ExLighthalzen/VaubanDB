//! `CAST`, `CONVERT` and their `TRY_` twins: the routing, and the `TRY_` rule.
//!
//! Not one conversion rule is written here. Rounding, truncation, date formats, output
//! collation and the wording of every error belong to `vauban_types::convert`, which
//! takes the **source** type as a parameter precisely because a [`Value`] does not carry
//! it: `Value::String` says nothing of `varchar` against `nvarchar`, and error 245 names
//! one or the other. The source type is read off the `BoundExpr` of the operand, never
//! deduced from the value.
//!
//! This file holds two decisions and no more: a `NULL` input converts to `NULL` without
//! calling `types::convert` at all, and which errors `TRY_CAST`/`TRY_CONVERT` swallow.

use vauban_errors::SqlResult;
use vauban_types::{TypeInfo, Value, convert};

use crate::errors::at;

/// The errors a `TRY_CAST` or a `TRY_CONVERT` still raises. Everything else it swallows.
///
/// A `TRY_` conversion answers `NULL` when the conversion fails and raises when the
/// conversion is not permitted, which leaves an overflow and a bad style to
/// decide. Both are swallowed:
///
/// | expression | plain form | `TRY_` form |
/// |---|---|---|
/// | `TRY_CAST('abc' AS int)` | 245 | `NULL` |
/// | `TRY_CAST('2020-13-01' AS date)` | 241 | `NULL` |
/// | `TRY_CAST('1700-01-01' AS smalldatetime)` | 242 | `NULL` |
/// | `TRY_CAST('abc' AS uniqueidentifier)` | 8169 | `NULL` |
/// | `TRY_CAST(300 AS tinyint)` | 220 | `NULL` |
/// | `TRY_CAST(1e30 AS int)` | 232 | `NULL` |
/// | `TRY_CAST(123456.789 AS decimal(5, 2))` | 8115 | `NULL` |
/// | `TRY_CONVERT(varbinary(10), 'abc', 5)` | 9809 | `NULL` |
///
/// An arithmetic overflow does not keep raising through a `TRY_CAST`: `SELECT TRY_CAST(300
/// AS tinyint);` answers `NULL` (`convert::tests::a_try_cast_swallows_an_overflow`). A
/// `CONVERT` style that means nothing for the pair of types is a fault of the query rather
/// than of the value, and is swallowed too: `SELECT TRY_CONVERT(varbinary(10), CAST('abc'
/// AS varchar(10)), 5);` answers `NULL`, while the same `CONVERT` without `TRY_` raises
/// 9809 (`convert::tests::a_try_convert_hides_an_impossible_style_too`). The rule is
/// therefore stated the other way round: a `TRY_` swallows every error `types::convert`
/// raises, save the two below.
///
/// - **529**, the pair of types with no explicit conversion at all, is the one thing a
///   `TRY_` does not hide: `SELECT TRY_CAST(CAST('2020-01-01' AS date) AS int);` raises
///   529 on SQL Server. `call::is_castable` refuses that pair at bind time, so VaubanDB
///   does not reach this function with it — the entry is a guard, not a live path
///   (`convert::tests::a_try_cast_still_raises_the_forbidden_pair`).
/// - **50000** is the internal error of the project (`errors::InternalError`), never a
///   message a client wrote: swallowing it would turn a bug of the binder or of `types`
///   into a silent `NULL`.
///
/// Anything the evaluator raises around the conversion (8134 in the operand, 8116 in an
/// argument) is not a conversion error and does not reach this function.
const RAISED_THROUGH_TRY: [u32; 2] = [529, 50000];

/// Evaluates one [`vauban_binder::BoundExprKind::Convert`] node.
///
/// `from` is the type of the operand as the binder typed it, `to` the type of the
/// conversion node itself, `style` the third argument of `CONVERT` (`None` for `CAST`),
/// and `try_` is true for `TRY_CAST` and `TRY_CONVERT`.
///
/// A `NULL` input answers `NULL` for every target, without calling `types::convert`:
/// `CAST(NULL AS int)` is `NULL`, and so is `TRY_CAST(NULL AS int)`.
///
/// # Errors
///
/// Whatever `types::convert` raises, unchanged — 245, 8114, 220, 8115, 9809… — unless
/// `try_` is set, which turns every number but those of [`RAISED_THROUGH_TRY`] into
/// [`Value::Null`]. `line` is the line of the conversion node, put on whatever survives
/// (see [`crate::errors::at`]); an error a `TRY_` swallows never gets one, since it never
/// leaves this function.
pub(crate) fn eval_convert(
    value: &Value,
    from: &TypeInfo,
    to: &TypeInfo,
    style: Option<i32>,
    try_: bool,
    line: u32,
) -> SqlResult<Value> {
    // `types::convert` short-circuits `NULL` as well; doing it here too states that the
    // rule is the executor's, and spares the dispatch on the target family.
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    match convert(value, from, to, style) {
        Ok(converted) => Ok(converted),
        Err(error) if try_ && !RAISED_THROUGH_TRY.contains(&error.number) => Ok(Value::Null),
        Err(error) => Err(at(error, line)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::{Len, SqlString, SqlType};

    /// `varchar(n)`, the type the binder gives to a character literal of `n` characters.
    fn varchar(n: u16) -> TypeInfo {
        TypeInfo::new(SqlType::VarChar(Len::Fixed(n)), false)
    }

    /// `int`, nullable or not.
    fn int(nullable: bool) -> TypeInfo {
        TypeInfo::new(SqlType::Int, nullable)
    }

    /// A character value.
    fn text(s: &str) -> Value {
        Value::String(SqlString { text: s.to_owned() })
    }

    #[test]
    fn null_converts_to_null_for_every_target() {
        let out = eval_convert(&Value::Null, &int(true), &varchar(10), None, false, 0)
            .expect("NULL converts");
        assert_eq!(out, Value::Null);
        let out = eval_convert(&Value::Null, &varchar(3), &int(true), None, true, 0)
            .expect("NULL converts");
        assert_eq!(out, Value::Null);
    }

    #[test]
    fn a_cast_forwards_the_error_of_types() {
        let error = eval_convert(&text("abc"), &varchar(3), &int(true), None, false, 0)
            .expect_err("'abc' is not an int");
        assert_eq!(error.number, 245);
        assert_eq!(error.severity, 16);
        assert_eq!(error.state, 1);
    }

    #[test]
    fn a_conversion_error_carries_the_line_of_its_node() {
        let error = eval_convert(&text("abc"), &varchar(3), &int(true), None, false, 2)
            .expect_err("'abc' is not an int");
        assert_eq!(error.number, 245);
        assert_eq!(error.line, 2);
    }

    #[test]
    fn a_try_cast_swallows_a_conversion_failure() {
        let out = eval_convert(&text("abc"), &varchar(3), &int(true), None, true, 0)
            .expect("TRY_CAST answers NULL");
        assert_eq!(out, Value::Null);
    }

    #[test]
    fn a_try_cast_swallows_an_overflow() {
        // `TRY_CAST(300 AS tinyint)`: 220 on a plain `CAST`, `NULL` on a `TRY_CAST`.
        let tinyint = TypeInfo::new(SqlType::TinyInt, true);
        let error = eval_convert(&Value::I32(300), &int(false), &tinyint, None, false, 0)
            .expect_err("300 does not fit a tinyint");
        assert_eq!(error.number, 220);
        let out = eval_convert(&Value::I32(300), &int(false), &tinyint, None, true, 0)
            .expect("TRY_CAST answers NULL");
        assert_eq!(out, Value::Null);
    }

    #[test]
    fn a_try_convert_hides_an_impossible_style_too() {
        // `CONVERT(varbinary(10), CAST('abc' AS varchar(10)), 5)` is 9809, and its `TRY_`
        // twin is `NULL` even though the fault is in the query rather than in the value.
        let varbinary = TypeInfo::new(SqlType::VarBinary(Len::Fixed(10)), true);
        let error = eval_convert(&text("abc"), &varchar(3), &varbinary, Some(5), false, 0)
            .expect_err("style 5 means nothing here");
        assert_eq!(error.number, 9809);
        assert_eq!(error.severity, 16);

        let out = eval_convert(&text("abc"), &varchar(3), &varbinary, Some(5), true, 0)
            .expect("TRY_CONVERT answers NULL");
        assert_eq!(out, Value::Null);
    }

    #[test]
    fn a_try_cast_still_raises_the_forbidden_pair() {
        // `SELECT TRY_CAST(CAST('2020-01-01' AS date) AS int);` answers 529 on SQL Server:
        // a conversion that is not permitted. `call::is_castable` refuses the pair
        // at bind time, so a query does not reach this function with it; the assertion
        // guards the rule, not a live path.
        let date = TypeInfo::new(SqlType::Date, false);
        let value = Value::Date(vauban_types::Date { days: 0 });
        let error = eval_convert(&value, &date, &int(true), None, true, 0)
            .expect_err("a date does not convert to an int, TRY_ or not");
        assert_eq!(error.number, 529);
        assert_eq!(error.severity, 16);
    }
}
