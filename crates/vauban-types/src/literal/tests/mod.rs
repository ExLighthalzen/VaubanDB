use super::*;
use crate::Collation;

fn parse(kind: LiteralKind, text: &str) -> (Value, TypeInfo) {
    parse_literal(kind, text).expect("literal")
}

#[test]
fn integer_leading_zeros_do_not_count() {
    // `007` is an int, `00000000002147483648` is
    // numeric(10,0) and not numeric(20,0).
    let (v, t) = parse(LiteralKind::Integer, "007");
    assert_eq!(v, Value::I32(7));
    assert_eq!(t.ty, SqlType::Int);

    let (v, t) = parse(LiteralKind::Integer, "00000000002147483648");
    assert_eq!(
        v,
        Value::Decimal(crate::Decimal {
            mantissa: 2_147_483_648,
            precision: 10,
            scale: 0
        })
    );
    assert_eq!(
        t.ty,
        SqlType::Numeric {
            precision: 10,
            scale: 0
        }
    );
}

#[test]
fn thirty_eight_digits_is_the_limit() {
    // numeric(38,0) and numeric(38,2) exist...
    let (_, t) = parse(LiteralKind::Integer, &"1".repeat(38));
    assert_eq!(
        t.ty,
        SqlType::Numeric {
            precision: 38,
            scale: 0
        }
    );
    let (_, t) = parse(
        LiteralKind::Decimal,
        "123456789012345678901234567890123456.78",
    );
    assert_eq!(
        t.ty,
        SqlType::Numeric {
            precision: 38,
            scale: 2
        }
    );

    // ... and one digit more
    // is error 1007, severity 15, state 1 — not a `float`.
    let digits = "123456789012345678901234567890123456789";
    let e = parse_literal(LiteralKind::Integer, digits).expect_err("1007");
    assert_eq!(e.number, 1007);
    assert_eq!(e.severity, 15);
    assert_eq!(e.state, 1);
    assert_eq!(
        e.message,
        format!("The number '{digits}' exceeds the numeric range (precision is limited to 38).")
    );
    let e = parse_literal(
        LiteralKind::Decimal,
        "1234567890123456789012345678901234567.89",
    )
    .expect_err("1007");
    assert_eq!(e.number, 1007);
    assert_eq!(
        e.message,
        "The number '1234567890123456789012345678901234567.89' exceeds the numeric \
         range (precision is limited to 38)."
    );
}

#[test]
fn malformed_payloads_are_internal_errors() {
    for (kind, text) in [
        (LiteralKind::Integer, ""),
        (LiteralKind::Integer, "-1"),
        (LiteralKind::Integer, "1.5"),
        (LiteralKind::Decimal, "15"),
        (LiteralKind::Decimal, "1.2.3"),
        (LiteralKind::Decimal, "-1.5"),
        (LiteralKind::Decimal, "."),
        (LiteralKind::Float, "15"),
        (LiteralKind::Float, "1e"),
        (LiteralKind::Float, "e3"),
        (LiteralKind::Float, "1e2e3"),
        (LiteralKind::Money, "$1.5"),
        (LiteralKind::Money, ""),
        (LiteralKind::Money, "1,234.5"),
        (LiteralKind::Hex, "1"),
        (LiteralKind::Hex, "0x1F"),
        (LiteralKind::Hex, "zz"),
    ] {
        let e = parse_literal(kind, text).expect_err("internal error");
        assert_eq!(e.number, 50_000, "{kind:?} {text}");
    }
}

#[test]
fn float_out_of_range_is_reported() {
    // 168, severity 15, state 1, and the width of a
    // `float` in the message.
    let e = parse_literal(LiteralKind::Float, "1e400").expect_err("168");
    assert_eq!(e.number, 168);
    assert_eq!(e.severity, 15);
    assert_eq!(e.state, 1);
    assert_eq!(
        e.message,
        "The floating point literal '1e400' cannot be represented in 8 bytes."
    );
}

#[test]
fn money_out_of_range_is_reported() {
    // 151, severity 15, state 1, and the literal quoted
    // with the `$` its payload does not carry.
    let e = parse_literal(LiteralKind::Money, "99999999999999999999").expect_err("151");
    assert_eq!(e.number, 151);
    assert_eq!(e.severity, 15);
    assert_eq!(e.state, 1);
    assert_eq!(
        e.message,
        "'$99999999999999999999' cannot be read as a money value."
    );
}

#[test]
fn money_keeps_four_decimals() {
    // $1.23456 prints 1.2346 under style 2.
    assert_eq!(parse(LiteralKind::Money, "1.23456").0, Value::Money(12_346));
    assert_eq!(parse(LiteralKind::Money, "1.5").0, Value::Money(15_000));
    assert_eq!(parse(LiteralKind::Money, "1").0, Value::Money(10_000));
}

#[test]
fn long_strings_become_max() {
    let (_, t) = parse(LiteralKind::Str, &"x".repeat(8_000));
    assert_eq!(t.ty, SqlType::VarChar(Len::Fixed(8_000)));
    let (_, t) = parse(LiteralKind::Str, &"x".repeat(8_001));
    assert_eq!(t.ty, SqlType::VarChar(Len::Max));
    let (_, t) = parse(LiteralKind::NStr, &"y".repeat(4_000));
    assert_eq!(t.ty, SqlType::NVarChar(Len::Fixed(4_000)));
    let (_, t) = parse(LiteralKind::NStr, &"y".repeat(4_001));
    assert_eq!(t.ty, SqlType::NVarChar(Len::Max));
}

/// A literal is never `NULL`, and only a character literal carries a collation.
#[test]
fn literals_are_never_nullable() {
    for (kind, text) in [
        (LiteralKind::Integer, "1"),
        (LiteralKind::Decimal, "1.5"),
        (LiteralKind::Float, "1e3"),
        (LiteralKind::Money, "1.5"),
        (LiteralKind::Hex, "1F"),
        (LiteralKind::Str, "x"),
        (LiteralKind::NStr, "x"),
    ] {
        let (_, t) = parse(kind, text);
        assert!(!t.nullable, "{kind:?}");
        let expected = if t.ty.is_string() {
            Some(Collation::DEFAULT)
        } else {
            None
        };
        assert_eq!(t.collation, expected, "{kind:?}");
    }
}

/// Multi-byte characters count as one character, not as their UTF-8 length.
#[test]
fn string_length_counts_characters() {
    let (v, t) = parse(LiteralKind::NStr, "é");
    assert_eq!(
        v,
        Value::String(SqlString {
            text: "é".to_owned()
        })
    );
    assert_eq!(t.ty, SqlType::NVarChar(Len::Fixed(1)));
}
