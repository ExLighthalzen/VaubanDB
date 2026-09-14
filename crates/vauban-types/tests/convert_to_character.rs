//! `convert` towards `char`, `varchar`, `nchar` and `nvarchar`.
//!
//! Binary sources are in `convert_binary.rs`: `CONVERT(varchar, 0x4E616D65)` reads the
//! bytes as characters.

use vauban_types::{Decimal, Len, SqlString, SqlType, TypeInfo, Value, convert};

/// A nullable `TypeInfo`, the only shape these tests need.
fn ti(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, true)
}

/// The text of a successful conversion to a character type.
fn text(v: &Value, from: &TypeInfo, to: &TypeInfo, style: Option<i32>) -> String {
    match convert(v, from, to, style) {
        Ok(Value::String(SqlString { text })) => text,
        other => panic!("expected a string, got {other:?}"),
    }
}

/// The number and message of a conversion that must fail.
fn failure(v: &Value, from: &TypeInfo, to: &TypeInfo) -> (u32, String) {
    match convert(v, from, to, None) {
        Err(e) => (e.number, e.message),
        Ok(v) => panic!("expected an error, got {v:?}"),
    }
}

/// The state of a conversion that must fail: the number and the message are asserted
/// apart.
fn state(v: &Value, from: &TypeInfo, to: &TypeInfo) -> u8 {
    match convert(v, from, to, None) {
        Err(e) => e.state,
        Ok(v) => panic!("expected an error, got {v:?}"),
    }
}

fn string(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

/// `SELECT CAST('abcdef' AS varchar(3));` is `abc`, without error and without warning;
/// `SELECT '[' + CAST('ab' AS char(5)) + ']';` is `[ab   ]`.
#[test]
fn string_to_string_truncates_silently() {
    let from = ti(SqlType::VarChar(Len::Fixed(10)));
    assert_eq!(
        text(
            &string("abcdef"),
            &from,
            &ti(SqlType::VarChar(Len::Fixed(3))),
            None
        ),
        "abc"
    );
    assert_eq!(
        text(
            &string("abcdef"),
            &from,
            &ti(SqlType::Char(Len::Fixed(3))),
            None
        ),
        "abc"
    );
    assert_eq!(
        text(
            &string("ab"),
            &from,
            &ti(SqlType::Char(Len::Fixed(5))),
            None
        ),
        "ab   "
    );
    assert_eq!(
        text(
            &string("abcdef"),
            &from,
            &ti(SqlType::VarChar(Len::Max)),
            None
        ),
        "abcdef"
    );
    // A national target truncates just as silently, and counts characters, not bytes.
    assert_eq!(
        text(
            &string("éàèùô"),
            &from,
            &ti(SqlType::NVarChar(Len::Fixed(3))),
            None
        ),
        "éàè"
    );
}

/// `SELECT CAST(123456 AS varchar(2));` is `*`, and `SELECT '[' + CAST(@i AS char(2)) + ']';`
/// is `[* ]`: the padding of the `char` applies after the `*`.
///
/// Towards a national target the same value raises 8115, whose message names **no type**:
/// `DECLARE @i int = 123456; SELECT CAST(@i AS nvarchar(2));` raises 8115 on `expression`
/// and `nvarchar`, with a typed variable as with a literal.
#[test]
fn int_too_short_is_star() {
    let from = ti(SqlType::Int);
    let v = Value::I32(123456);
    assert_eq!(
        text(&v, &from, &ti(SqlType::VarChar(Len::Fixed(2))), None),
        "*"
    );
    assert_eq!(
        text(&v, &from, &ti(SqlType::Char(Len::Fixed(2))), None),
        "* "
    );
    assert_eq!(
        failure(&v, &from, &ti(SqlType::NVarChar(Len::Fixed(2)))),
        (
            8115,
            "Converting expression to data type nvarchar overflowed.".to_owned()
        )
    );
    // `tinyint` and `smallint` follow `int`; `bigint` is an error even towards `varchar`
    // (`DECLARE @b bigint = 123456; SELECT CAST(@b AS varchar(2));`).
    assert_eq!(
        text(
            &Value::I8(123),
            &ti(SqlType::TinyInt),
            &ti(SqlType::VarChar(Len::Fixed(2))),
            None
        ),
        "*"
    );
    assert_eq!(
        failure(
            &Value::I16(12345),
            &ti(SqlType::SmallInt),
            &ti(SqlType::NVarChar(Len::Fixed(2)))
        ),
        (
            8115,
            "Converting expression to data type nvarchar overflowed.".to_owned()
        )
    );
    assert_eq!(
        failure(
            &Value::I64(123456),
            &ti(SqlType::BigInt),
            &ti(SqlType::VarChar(Len::Fixed(2)))
        )
        .0,
        8115
    );
    // A target long enough renders normally, `*` or not.
    assert_eq!(
        text(&v, &from, &ti(SqlType::VarChar(Len::Fixed(6))), None),
        "123456"
    );
}

/// `SELECT CAST(CAST(123.45 AS numeric(5,2)) AS varchar(2));`
/// raises 8115 **state 5**, and its message names the source `numeric`.
///
/// A `float` source raises **232**, not 8115 (`DECLARE @f float = 1234567;
/// SELECT CAST(@f AS varchar(3));`, which quotes the value), and a `money` source **234**,
/// a `smallmoney` source **292** (`DECLARE @m money = 922337203685477.58; SELECT CAST(@m
/// AS varchar(3));`).
///
/// Towards `nchar` and `nvarchar` only the two money numbers survive: every other source
/// answers 8115 state 2 on `expression`, the `numeric` and the `float` included
/// (`… SELECT CAST(@f AS nchar(3));`). And the target always prints its variable-length
/// name: a `char(3)` reads `varchar` in all of them.
#[test]
fn numeric_too_short_is_error() {
    let numeric = ti(SqlType::Numeric {
        precision: 5,
        scale: 2,
    });
    let v = Value::Decimal(Decimal {
        mantissa: 12345,
        precision: 5,
        scale: 2,
    });
    assert_eq!(
        failure(&v, &numeric, &ti(SqlType::VarChar(Len::Fixed(2)))),
        (
            8115,
            "Converting numeric to data type varchar overflowed.".to_owned()
        )
    );
    assert_eq!(state(&v, &numeric, &ti(SqlType::VarChar(Len::Fixed(2)))), 5);
    // A `decimal` is named `numeric` in the message too, and a `char(2)` target reads
    // `varchar` (`DECLARE @n numeric(20,0) = 99999999999999999;
    // SELECT CAST(@n AS char(3));`).
    let decimal = ti(SqlType::Decimal {
        precision: 5,
        scale: 2,
    });
    assert_eq!(
        failure(&v, &decimal, &ti(SqlType::Char(Len::Fixed(2)))),
        (
            8115,
            "Converting numeric to data type varchar overflowed.".to_owned()
        )
    );
    // The national target flattens it to `expression`, state 2.
    assert_eq!(
        failure(&v, &decimal, &ti(SqlType::NVarChar(Len::Fixed(2)))),
        (
            8115,
            "Converting expression to data type nvarchar overflowed.".to_owned()
        )
    );
    assert_eq!(
        state(&v, &decimal, &ti(SqlType::NVarChar(Len::Fixed(2)))),
        2
    );

    assert_eq!(
        failure(
            &Value::F64(1234567.0),
            &ti(SqlType::Float),
            &ti(SqlType::VarChar(Len::Fixed(3)))
        ),
        (
            232,
            "Value out of range for type varchar: 1234567.000000.".to_owned()
        )
    );
    assert_eq!(
        failure(
            &Value::F32(1234567.0),
            &ti(SqlType::Real),
            &ti(SqlType::VarChar(Len::Fixed(3)))
        )
        .0,
        232
    );
    // The same `float`, towards a `char(3)`: still `varchar` in the message; towards an
    // `nchar(3)`: 8115 on `expression`.
    assert_eq!(
        failure(
            &Value::F64(1234567.0),
            &ti(SqlType::Float),
            &ti(SqlType::Char(Len::Fixed(3)))
        ),
        (
            232,
            "Value out of range for type varchar: 1234567.000000.".to_owned()
        )
    );
    assert_eq!(
        failure(
            &Value::F64(1234567.0),
            &ti(SqlType::Float),
            &ti(SqlType::NChar(Len::Fixed(3)))
        ),
        (
            8115,
            "Converting expression to data type nvarchar overflowed.".to_owned()
        )
    );

    // 234 and 292, state 2, with the variable-length name of the target.
    assert_eq!(
        failure(
            &Value::Money(42359819),
            &ti(SqlType::Money),
            &ti(SqlType::Char(Len::Fixed(3)))
        ),
        (
            234,
            "A money value does not fit in the result type varchar.".to_owned()
        )
    );
    assert_eq!(
        failure(
            &Value::Money(42359819),
            &ti(SqlType::Money),
            &ti(SqlType::NVarChar(Len::Fixed(3)))
        ),
        (
            234,
            "A money value does not fit in the result type nvarchar.".to_owned()
        )
    );
    assert_eq!(
        failure(
            &Value::Money(42359819),
            &ti(SqlType::SmallMoney),
            &ti(SqlType::VarChar(Len::Fixed(3)))
        ),
        (
            292,
            "A smallmoney value does not fit in the result type varchar.".to_owned()
        )
    );
    assert_eq!(
        state(
            &Value::Money(42359819),
            &ti(SqlType::SmallMoney),
            &ti(SqlType::VarChar(Len::Fixed(3)))
        ),
        2
    );
    // `varchar(max)` never overflows, whatever the source.
    assert_eq!(
        text(
            &Value::F64(1234567.0),
            &ti(SqlType::Float),
            &ti(SqlType::VarChar(Len::Max)),
            None
        ),
        "1.23457e+006"
    );
}

/// The four float styles on 1234567: `1.23457e+006`, `1.2345670e+006`,
/// `1.234567000000000e+006`, `1.2345670000000000e+006`.
///
/// Style 3 is scientific too, with 17 digits.
#[test]
fn float_styles() {
    let from = ti(SqlType::Float);
    let to = ti(SqlType::VarChar(Len::Fixed(40)));
    let v = Value::F64(1234567.0);
    assert_eq!(text(&v, &from, &to, None), "1.23457e+006");
    assert_eq!(text(&v, &from, &to, Some(0)), "1.23457e+006");
    assert_eq!(text(&v, &from, &to, Some(1)), "1.2345670e+006");
    assert_eq!(text(&v, &from, &to, Some(2)), "1.234567000000000e+006");
    assert_eq!(text(&v, &from, &to, Some(3)), "1.2345670000000000e+006");
    // Unknown styles fall back to style 0; style 126 behaves like style 2. With
    // `DECLARE @f float = 1234567;`: `CONVERT(varchar(40), @f, 4)`, `…, 5)`, `…, 100)` and
    // `…, -1)` give `1.23457e+006`, `…, 126)` gives the 16-digit form.
    assert_eq!(text(&v, &from, &to, Some(4)), "1.23457e+006");
    assert_eq!(text(&v, &from, &to, Some(-1)), "1.23457e+006");
    assert_eq!(text(&v, &from, &to, Some(126)), "1.234567000000000e+006");
    // `DECLARE @f float = 0.1; SELECT CONVERT(varchar(40), @f, 1), …, 2), …, 3);` and
    // `CONVERT(varchar(40), CAST(1.5 AS real), 1)`, `CONVERT(varchar(40), CAST(0 AS float), 1)`.
    let tenth = Value::F64(0.1);
    assert_eq!(text(&tenth, &from, &to, Some(1)), "1.0000000e-001");
    assert_eq!(text(&tenth, &from, &to, Some(2)), "1.000000000000000e-001");
    assert_eq!(text(&tenth, &from, &to, Some(3)), "1.0000000000000001e-001");
    assert_eq!(
        text(&Value::F32(1.5), &ti(SqlType::Real), &to, Some(1)),
        "1.5000000e+000"
    );
    assert_eq!(
        text(&Value::F64(0.0), &from, &to, Some(1)),
        "0.0000000e+000"
    );
}

/// The three money styles on 4235.9819: `4235.98`, `4,235.98`, `4235.9819`.
///
/// An unknown style does not fall back to style 0 but to style **1**. With
/// `DECLARE @m money = 4235.9819;`: styles 3, 4, 5,
/// 6, 7, 8, 9, 10, 20, 21, 99, 100, 127 and -1 all give `4,235.98`, while 0 gives `4235.98`
/// and 2 and 126 give `4235.9819`.
#[test]
fn money_styles() {
    let from = ti(SqlType::Money);
    let to = ti(SqlType::VarChar(Len::Fixed(40)));
    let v = Value::Money(42359819);
    assert_eq!(text(&v, &from, &to, None), "4235.98");
    assert_eq!(text(&v, &from, &to, Some(0)), "4235.98");
    assert_eq!(text(&v, &from, &to, Some(1)), "4,235.98");
    assert_eq!(text(&v, &from, &to, Some(2)), "4235.9819");
    assert_eq!(text(&v, &from, &to, Some(126)), "4235.9819");
    assert_eq!(text(&v, &from, &to, Some(99)), "4,235.98");
    assert_eq!(
        text(&Value::Money(-42359819), &from, &to, Some(1)),
        "-4,235.98"
    );
    // `DECLARE @m money = 1234567890.1234;` and two small amounts:
    // `CONVERT(varchar(40), @m, 1)` = `1,234,567,890.12`, `…, 2)` = `1234567890.1234`,
    // `CONVERT(varchar(40), CAST(0.5 AS money), 1)` = `0.50`,
    // `CONVERT(varchar(40), CAST(-0.005 AS money), 1)` = `-0.01`.
    let big = Value::Money(12345678901234);
    assert_eq!(text(&big, &from, &to, Some(1)), "1,234,567,890.12");
    assert_eq!(text(&big, &from, &to, Some(2)), "1234567890.1234");
    assert_eq!(text(&Value::Money(5000), &from, &to, Some(1)), "0.50");
    assert_eq!(text(&Value::Money(-50), &from, &to, Some(1)), "-0.01");
    // `smallmoney` follows the same styles.
    assert_eq!(
        text(&Value::Money(15000), &ti(SqlType::SmallMoney), &to, Some(2)),
        "1.5000"
    );
}

/// `CONVERT(varchar(3), NULL)` is `NULL`, whatever the target and whatever the style.
#[test]
fn null_stays_null() {
    assert_eq!(
        convert(
            &Value::Null,
            &ti(SqlType::Int),
            &ti(SqlType::VarChar(Len::Fixed(3))),
            None
        ),
        Ok(Value::Null)
    );
    assert_eq!(
        convert(
            &Value::Null,
            &ti(SqlType::Float),
            &ti(SqlType::Char(Len::Fixed(3))),
            Some(2)
        ),
        Ok(Value::Null)
    );
}

/// A `bit` is truncated in silence; a `uniqueidentifier` is **not**.
///
/// A GUID needs its 36 characters: `DECLARE @g uniqueidentifier =
/// '0E984725-C51C-4BF4-9960-E1C80E27ABA0'; SELECT CAST(@g AS varchar(35));` answers 8170
/// state 2, `A uniqueidentifier value does not fit in the char result.`, which
/// names neither the target nor its length; `varchar(36)` is the shortest target that
/// works, and a `char(10)` answers the same 8170. The national family answers 8115 state 2
/// on `expression` instead (`… SELECT CAST(@g AS nvarchar(35));`), as it does for every
/// other source.
#[test]
fn guid_needs_its_thirty_six_characters() {
    let guid = Value::Guid([
        0x25, 0x47, 0x98, 0x0e, 0x1c, 0xc5, 0xf4, 0x4b, 0x99, 0x60, 0xe1, 0xc8, 0x0e, 0x27, 0xab,
        0xa0,
    ]);
    let from = ti(SqlType::UniqueIdentifier);
    assert_eq!(
        text(&guid, &from, &ti(SqlType::VarChar(Len::Fixed(36))), None),
        "0E984725-C51C-4BF4-9960-E1C80E27ABA0"
    );
    assert_eq!(
        failure(&guid, &from, &ti(SqlType::VarChar(Len::Fixed(35)))),
        (
            8170,
            "A uniqueidentifier value does not fit in the char result.".to_owned()
        )
    );
    assert_eq!(
        state(&guid, &from, &ti(SqlType::VarChar(Len::Fixed(35)))),
        2
    );
    assert_eq!(
        failure(&guid, &from, &ti(SqlType::Char(Len::Fixed(10)))).0,
        8170
    );
    assert_eq!(
        failure(&guid, &from, &ti(SqlType::NVarChar(Len::Fixed(35)))),
        (
            8115,
            "Converting expression to data type nvarchar overflowed.".to_owned()
        )
    );
    // `varchar(max)` holds it whole.
    assert_eq!(
        text(&guid, &from, &ti(SqlType::VarChar(Len::Max)), None),
        "0E984725-C51C-4BF4-9960-E1C80E27ABA0"
    );
    assert_eq!(
        text(
            &Value::Bit(true),
            &ti(SqlType::Bit),
            &ti(SqlType::VarChar(Len::Fixed(1))),
            None
        ),
        "1"
    );
    assert_eq!(
        text(
            &Value::Bit(false),
            &ti(SqlType::Bit),
            &ti(SqlType::Char(Len::Fixed(3))),
            None
        ),
        "0  "
    );
}
