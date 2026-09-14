//! Integration test: `parse_literal` seen from outside the crate.
//!
//! The value and the type of each kind of literal, and the three errors.

use vauban_types::{
    Collation, Decimal, Len, LiteralKind, SqlString, SqlType, TypeInfo, Value, parse_literal,
};

fn parse(kind: LiteralKind, text: &str) -> (Value, TypeInfo) {
    parse_literal(kind, text).unwrap_or_else(|e| panic!("{kind:?} {text:?}: {}", e.message))
}

fn numeric(precision: u8, scale: u8) -> SqlType {
    SqlType::Numeric { precision, scale }
}

/// `int` while it fits, then `numeric(p, 0)`. SQL Server never
/// produces a `bigint` literal, so `2147483648` is `numeric(10,0)`.
#[test]
fn literal_integers() {
    let (v, t) = parse(LiteralKind::Integer, "1");
    assert_eq!(v, Value::I32(1));
    assert_eq!(t.ty, SqlType::Int);

    let (v, t) = parse(LiteralKind::Integer, "0");
    assert_eq!(v, Value::I32(0));
    assert_eq!(t.ty, SqlType::Int);

    let (v, t) = parse(LiteralKind::Integer, "2147483647");
    assert_eq!(v, Value::I32(2_147_483_647));
    assert_eq!(t.ty, SqlType::Int);

    let (v, t) = parse(LiteralKind::Integer, "2147483648");
    assert_eq!(
        v,
        Value::Decimal(Decimal {
            mantissa: 2_147_483_648,
            precision: 10,
            scale: 0,
        })
    );
    assert_eq!(t.ty, numeric(10, 0));
    assert_ne!(t.ty, SqlType::BigInt);

    // 23 digits.
    let (v, t) = parse(LiteralKind::Integer, "99999999999999999999999");
    assert_eq!(
        v,
        Value::Decimal(Decimal {
            mantissa: 99_999_999_999_999_999_999_999,
            precision: 23,
            scale: 0,
        })
    );
    assert_eq!(t.ty, numeric(23, 0));
}

/// The digits are counted as written, minus the leading zeros of the integer part.
#[test]
fn literal_decimals() {
    let (v, t) = parse(LiteralKind::Decimal, "1.50");
    assert_eq!(
        v,
        Value::Decimal(Decimal {
            mantissa: 150,
            precision: 3,
            scale: 2,
        })
    );
    assert_eq!(t.ty, numeric(3, 2));

    let (_, t) = parse(LiteralKind::Decimal, "1.5");
    assert_eq!(t.ty, numeric(2, 1));

    let (v, t) = parse(LiteralKind::Decimal, ".5");
    assert_eq!(
        v,
        Value::Decimal(Decimal {
            mantissa: 5,
            precision: 1,
            scale: 1,
        })
    );
    assert_eq!(t.ty, numeric(1, 1));

    // `0.000` is numeric(3,3), not numeric(4,3): the zero of the integer part does not
    // count.
    let (v, t) = parse(LiteralKind::Decimal, "0.000");
    assert_eq!(
        v,
        Value::Decimal(Decimal {
            mantissa: 0,
            precision: 3,
            scale: 3,
        })
    );
    assert_eq!(t.ty, numeric(3, 3));

    let (v, t) = parse(LiteralKind::Decimal, "123.456");
    assert_eq!(
        v,
        Value::Decimal(Decimal {
            mantissa: 123_456,
            precision: 6,
            scale: 3,
        })
    );
    assert_eq!(t.ty, numeric(6, 3));

    // Leading zeros and a trailing point.
    let (_, t) = parse(LiteralKind::Decimal, "01.50");
    assert_eq!(t.ty, numeric(3, 2));
    let (v, t) = parse(LiteralKind::Decimal, "1.");
    assert_eq!(
        v,
        Value::Decimal(Decimal {
            mantissa: 1,
            precision: 1,
            scale: 0,
        })
    );
    assert_eq!(t.ty, numeric(1, 0));
}

/// A `float` and a `money` literal. The `$` never appears in `text`: the lexer removes
/// it.
#[test]
fn literal_float_and_money() {
    let (v, t) = parse(LiteralKind::Float, "1e3");
    assert_eq!(v, Value::F64(1000.0));
    assert_eq!(t.ty, SqlType::Float);

    let (v, t) = parse(LiteralKind::Float, "1.5E-2");
    assert_eq!(v, Value::F64(1.5E-2));
    assert_eq!(t.ty, SqlType::Float);

    let (v, t) = parse(LiteralKind::Money, "1.5");
    assert_eq!(v, Value::Money(15_000));
    assert_eq!(t.ty, SqlType::Money);

    let (v, t) = parse(LiteralKind::Money, "-1.50");
    assert_eq!(v, Value::Money(-15_000));
    assert_eq!(t.ty, SqlType::Money);

    let (v, t) = parse(LiteralKind::Money, "0");
    assert_eq!(v, Value::Money(0));
    assert_eq!(t.ty, SqlType::Money);
}

/// A binary literal and the two kinds of string literal.
#[test]
fn literal_binary_and_strings() {
    let (v, t) = parse(LiteralKind::Hex, "1F");
    assert_eq!(v, Value::Bytes(vec![0x1F]));
    assert_eq!(t.ty, SqlType::VarBinary(Len::Fixed(1)));

    let (v, t) = parse(LiteralKind::Hex, "0102");
    assert_eq!(v, Value::Bytes(vec![0x01, 0x02]));
    assert_eq!(t.ty, SqlType::VarBinary(Len::Fixed(2)));

    // `0x` alone: an empty value in a `varbinary(1)`.
    let (v, t) = parse(LiteralKind::Hex, "");
    assert_eq!(v, Value::Bytes(Vec::new()));
    assert_eq!(t.ty, SqlType::VarBinary(Len::Fixed(1)));

    let string = |s: &str| Value::String(SqlString { text: s.to_owned() });

    let (v, t) = parse(LiteralKind::Str, "x");
    assert_eq!(v, string("x"));
    assert_eq!(t.ty, SqlType::VarChar(Len::Fixed(1)));

    let (v, t) = parse(LiteralKind::Str, "");
    assert_eq!(v, string(""));
    assert_eq!(t.ty, SqlType::VarChar(Len::Fixed(1)));

    // The text arrives already unescaped: `parse_literal` unescapes nothing.
    let (v, t) = parse(LiteralKind::Str, "a'b");
    assert_eq!(v, string("a'b"));
    assert_eq!(t.ty, SqlType::VarChar(Len::Fixed(3)));

    let (v, t) = parse(LiteralKind::NStr, "x");
    assert_eq!(v, string("x"));
    assert_eq!(t.ty, SqlType::NVarChar(Len::Fixed(1)));

    for (kind, text) in [
        (LiteralKind::Str, "x"),
        (LiteralKind::Str, ""),
        (LiteralKind::NStr, "x"),
    ] {
        let (_, t) = parse(kind, text);
        assert_eq!(t.collation, Some(Collation::DEFAULT), "{kind:?} {text:?}");
        assert!(!t.nullable, "{kind:?} {text:?}");
    }

    // A binary literal carries no collation.
    assert_eq!(parse(LiteralKind::Hex, "1F").1.collation, None);
}
