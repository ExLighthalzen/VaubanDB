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
    let out =
        eval_convert(&Value::Null, &varchar(3), &int(true), None, true, 0).expect("NULL converts");
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
