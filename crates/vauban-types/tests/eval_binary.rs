//! `eval_binary`: the value of a binary operation, from the public API of the crate.
//!
//! The arithmetic, concatenation and bitwise operators, family by family.
//!
//! The types of the results are not recomputed here: they are the ones `binary_op_type`
//! returns, which `tests/op_type.rs` pins.

use vauban_errors::SqlError;
use vauban_types::{
    BinaryOp, Date, DateTime, Decimal, Len, SqlString, SqlType, TypeInfo, Value, eval_binary,
};

/// The nine operators, in the order of `BinaryOp`.
const EVERY_OPERATOR: [BinaryOp; 9] = [
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::Mul,
    BinaryOp::Div,
    BinaryOp::Mod,
    BinaryOp::BitAnd,
    BinaryOp::BitOr,
    BinaryOp::BitXor,
    BinaryOp::Concat,
];

/// A non-nullable result type.
fn info(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, false)
}

fn num(precision: u8, scale: u8) -> SqlType {
    SqlType::Numeric { precision, scale }
}

/// A `decimal` value written as its mantissa, precision and scale.
fn decimal(mantissa: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal(Decimal {
        mantissa,
        precision,
        scale,
    })
}

fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

/// The value of `a op b`, which the vector expects to succeed.
fn value(op: BinaryOp, a: &Value, b: &Value, out: SqlType) -> Value {
    eval_binary(op, a, b, &info(out))
        .unwrap_or_else(|e| panic!("{a:?} {op:?} {b:?} must have a value, got {}", e.message))
}

/// The error of `a op b`, which the vector expects to fail.
fn error(op: BinaryOp, a: &Value, b: &Value, out: SqlType) -> SqlError {
    match eval_binary(op, a, b, &info(out)) {
        Ok(v) => panic!("{a:?} {op:?} {b:?} must fail, got {v:?}"),
        Err(e) => e,
    }
}

/// The mantissa of an exact result, with its precision and its scale.
fn mantissa(v: &Value) -> (i128, u8, u8) {
    match v {
        Value::Decimal(d) => (d.mantissa, d.precision, d.scale),
        other => panic!("{other:?} is not a decimal"),
    }
}

/// `NULL` wins over every operator, on either side, concatenation included: `'a' + NULL`
/// is `NULL` under `CONCAT_NULL_YIELDS_NULL ON`, the only setting this engine supports
///.
#[test]
fn null_propagates() {
    let int = info(SqlType::Int);
    for op in EVERY_OPERATOR {
        assert_eq!(
            eval_binary(op, &Value::Null, &Value::I32(1), &int),
            Ok(Value::Null),
            "NULL {op:?} 1"
        );
        assert_eq!(
            eval_binary(op, &Value::I32(1), &Value::Null, &int),
            Ok(Value::Null),
            "1 {op:?} NULL"
        );
    }
    let varchar = info(SqlType::VarChar(Len::Fixed(10)));
    assert_eq!(
        eval_binary(BinaryOp::Add, &text("a"), &Value::Null, &varchar),
        Ok(Value::Null)
    );
    assert_eq!(
        eval_binary(BinaryOp::Concat, &Value::Null, &text("a"), &varchar),
        Ok(Value::Null)
    );
}

/// The integers: an integral division truncated towards zero, a remainder that keeps the
/// sign of the dividend, and the two errors of the family.
#[test]
fn integer_arithmetic() {
    let seven = Value::I32(7);
    let minus_seven = Value::I32(-7);
    let two = Value::I32(2);

    assert_eq!(
        value(BinaryOp::Div, &seven, &two, SqlType::Int),
        Value::I32(3)
    );
    assert_eq!(
        value(BinaryOp::Div, &minus_seven, &two, SqlType::Int),
        Value::I32(-3)
    );
    assert_eq!(
        value(BinaryOp::Mod, &seven, &two, SqlType::Int),
        Value::I32(1)
    );
    assert_eq!(
        value(BinaryOp::Mod, &minus_seven, &two, SqlType::Int),
        Value::I32(-1)
    );

    let overflow = error(
        BinaryOp::Add,
        &Value::I32(2_147_483_647),
        &Value::I32(1),
        SqlType::Int,
    );
    assert_eq!(overflow.number, 8115);
    // The state too: 8115 comes with state 2 and 8134 with state 1.
    assert_eq!(overflow.state, 2);
    assert_eq!(
        overflow.message,
        "Converting expression to data type int overflowed."
    );

    let zero = error(BinaryOp::Div, &Value::I32(1), &Value::I32(0), SqlType::Int);
    assert_eq!(zero.number, 8134);
    assert_eq!(zero.state, 1);
    assert_eq!(zero.message, "Division by zero.");
    assert_eq!(
        error(BinaryOp::Mod, &Value::I32(1), &Value::I32(0), SqlType::Int).number,
        8134
    );

    let five = Value::I32(5);
    let three = Value::I32(3);
    assert_eq!(
        value(BinaryOp::BitAnd, &five, &three, SqlType::Int),
        Value::I32(1)
    );
    assert_eq!(
        value(BinaryOp::BitOr, &five, &three, SqlType::Int),
        Value::I32(7)
    );
    assert_eq!(
        value(BinaryOp::BitXor, &five, &three, SqlType::Int),
        Value::I32(6)
    );
    // `bit & bit` stays a `bit`.
    assert_eq!(
        value(
            BinaryOp::BitAnd,
            &Value::Bit(true),
            &Value::Bit(false),
            SqlType::Bit
        ),
        Value::Bit(false)
    );
}

/// Every integral overflow of an *operator* is 8115, `tinyint` and `smallint` included:
/// the 220 with the value that a `CAST` raises is not what `+` raises.
/// The result is rebuilt in the variant `out` calls for, so `tinyint + tinyint` stays a
/// `tinyint`.
#[test]
fn integer_overflow_is_always_8115() {
    assert_eq!(
        value(
            BinaryOp::Add,
            &Value::I8(1),
            &Value::I8(2),
            SqlType::TinyInt
        ),
        Value::I8(3)
    );
    let overflow = error(
        BinaryOp::Add,
        &Value::I8(255),
        &Value::I8(1),
        SqlType::TinyInt,
    );
    assert_eq!(overflow.number, 8115);
    assert_eq!(
        overflow.message,
        "Converting expression to data type tinyint overflowed."
    );

    let below_zero = error(
        BinaryOp::Sub,
        &Value::I8(0),
        &Value::I8(1),
        SqlType::TinyInt,
    );
    assert_eq!(below_zero.number, 8115);

    let smallint = error(
        BinaryOp::Add,
        &Value::I16(32_767),
        &Value::I16(1),
        SqlType::SmallInt,
    );
    assert_eq!(smallint.number, 8115);
    assert_eq!(
        smallint.message,
        "Converting expression to data type smallint overflowed."
    );

    let bigint = error(
        BinaryOp::Add,
        &Value::I64(i64::MAX),
        &Value::I64(1),
        SqlType::BigInt,
    );
    assert_eq!(bigint.number, 8115);
    assert_eq!(
        bigint.message,
        "Converting expression to data type bigint overflowed."
    );
}

/// `decimal`: the value is exact at the scale `binary_op_type` computed, and the division
/// rounds half away from zero.
#[test]
fn decimal_arithmetic() {
    // 1.5 * 2.25 = 3.375, numeric(6, 3).
    let product = value(
        BinaryOp::Mul,
        &decimal(15, 2, 1),
        &decimal(225, 3, 2),
        num(6, 3),
    );
    assert_eq!(mantissa(&product), (3375, 6, 3));

    // 1.5 + 2.25 = 3.75, numeric(4, 2).
    let sum = value(
        BinaryOp::Add,
        &decimal(15, 2, 1),
        &decimal(225, 3, 2),
        num(4, 2),
    );
    assert_eq!(mantissa(&sum), (375, 4, 2));

    // 1.5 - 2.25 = -0.75.
    let difference = value(
        BinaryOp::Sub,
        &decimal(15, 2, 1),
        &decimal(225, 3, 2),
        num(4, 2),
    );
    assert_eq!(mantissa(&difference), (-75, 4, 2));

    // 1.0 / 3.0 = 0.333333 and 2.0 / 3.0 = 0.666666: a quotient is **truncated** towards
    // zero, not rounded.
    let third = value(
        BinaryOp::Div,
        &decimal(10, 2, 1),
        &decimal(30, 2, 1),
        num(8, 6),
    );
    assert_eq!(mantissa(&third), (333_333, 8, 6));
    let two_thirds = value(
        BinaryOp::Div,
        &decimal(20, 2, 1),
        &decimal(30, 2, 1),
        num(8, 6),
    );
    assert_eq!(mantissa(&two_thirds), (666_666, 8, 6));
    let minus_two_thirds = value(
        BinaryOp::Div,
        &decimal(-20, 2, 1),
        &decimal(30, 2, 1),
        num(8, 6),
    );
    assert_eq!(mantissa(&minus_two_thirds), (-666_666, 8, 6));

    // 123.45 % 12.3 = 0.45, numeric(4, 2), and the remainder keeps the sign of the
    // dividend.
    let remainder = value(
        BinaryOp::Mod,
        &decimal(12_345, 5, 2),
        &decimal(123, 3, 1),
        num(4, 2),
    );
    assert_eq!(mantissa(&remainder), (45, 4, 2));
    let negative = value(
        BinaryOp::Mod,
        &decimal(-12_345, 5, 2),
        &decimal(123, 3, 1),
        num(4, 2),
    );
    assert_eq!(mantissa(&negative), (-45, 4, 2));

    // The largest numeric(38, 0) times ten needs 39 digits.
    let biggest = decimal(10i128.pow(38) - 1, 38, 0);
    let overflow = error(BinaryOp::Mul, &biggest, &decimal(10, 38, 0), num(38, 0));
    assert_eq!(overflow.number, 8115);
    // State 2, the one of the two-type message, not the 8 a `CAST` towards `numeric`
    // sends.
    assert_eq!(overflow.state, 2);
    assert_eq!(
        overflow.message,
        "Converting expression to data type numeric overflowed."
    );

    let zero = error(
        BinaryOp::Div,
        &decimal(15, 2, 1),
        &decimal(0, 2, 1),
        num(8, 6),
    );
    assert_eq!(zero.number, 8134);
    assert_eq!(zero.message, "Division by zero.");
    assert_eq!(
        error(
            BinaryOp::Mod,
            &decimal(15, 2, 1),
            &decimal(0, 2, 1),
            num(4, 1)
        )
        .number,
        8134
    );
}

/// A reduction that is not a quotient rounds half away from zero: the product below gives
/// `1.00000000000000001`, one more in the last digit, and the sum `0.0000000001`.
#[test]
fn reductions_round_half_away_from_zero() {
    // 1.00000000000000000600 * 1, at scale 40, reduced to decimal(38, 17).
    let a = decimal(100_000_000_000_000_000_600, 30, 20);
    let one = decimal(100_000_000_000_000_000_000, 30, 20);
    let product = value(
        BinaryOp::Mul,
        &a,
        &one,
        SqlType::Decimal {
            precision: 38,
            scale: 17,
        },
    );
    assert_eq!(mantissa(&product), (100_000_000_000_000_001, 38, 17));

    // 0.00000000005 (scale 30) + 0 (scale 10), whose type is decimal(38, 10).
    let tiny = decimal(5 * 10i128.pow(19), 38, 30);
    let zero = decimal(0, 38, 10);
    let sum = value(
        BinaryOp::Add,
        &tiny,
        &zero,
        SqlType::Decimal {
            precision: 38,
            scale: 10,
        },
    );
    assert_eq!(mantissa(&sum), (1, 38, 10));
}

/// The worked example of the reduction: `decimal(30, 20) * decimal(30, 20)` is a
/// `decimal(38, 17)`. The exact product of the
/// two mantissas has sixty digits — far beyond an `i128` — and the result exists all the
/// same, which is why the product is computed exactly and reduced afterwards.
#[test]
fn a_product_wider_than_i128_still_has_a_value() {
    let a = decimal(15 * 10i128.pow(19), 30, 20); // 1.5
    let b = decimal(225 * 10i128.pow(18), 30, 20); // 2.25
    let product = value(
        BinaryOp::Mul,
        &a,
        &b,
        SqlType::Decimal {
            precision: 38,
            scale: 17,
        },
    );
    // 3.375 at scale 17.
    assert_eq!(mantissa(&product), (3375 * 10i128.pow(14), 38, 17));
}

/// `money` is an exact integer of ten-thousandths: `10 / 3` is 3.3333 and the sum of two
/// amounts is exact.
#[test]
fn money_arithmetic() {
    assert_eq!(
        value(
            BinaryOp::Div,
            &Value::Money(100_000),
            &Value::Money(30_000),
            SqlType::Money
        ),
        Value::Money(33_333)
    );
    assert_eq!(
        value(
            BinaryOp::Add,
            &Value::Money(15_000),
            &Value::Money(15_000),
            SqlType::Money
        ),
        Value::Money(30_000)
    );
    // 1.5 * 2.5 = 3.75, rounded at scale 4 like every product of two amounts.
    assert_eq!(
        value(
            BinaryOp::Mul,
            &Value::Money(15_000),
            &Value::Money(25_000),
            SqlType::Money
        ),
        Value::Money(37_500)
    );

    // 2 / 3 is 0.6666: a quotient truncates here too.
    assert_eq!(
        value(
            BinaryOp::Div,
            &Value::Money(20_000),
            &Value::Money(30_000),
            SqlType::Money
        ),
        Value::Money(6_666)
    );
    // 1.5 * 0.0001 is 0.00015, which the product rounds up to 0.0002.
    assert_eq!(
        value(
            BinaryOp::Mul,
            &Value::Money(15_000),
            &Value::Money(1),
            SqlType::Money
        ),
        Value::Money(2)
    );

    let overflow = error(
        BinaryOp::Add,
        &Value::Money(i64::MAX),
        &Value::Money(1),
        SqlType::Money,
    );
    assert_eq!(overflow.number, 8115);
    assert_eq!(
        overflow.message,
        "Converting expression to data type money overflowed."
    );

    // `smallmoney` has its own bounds, read off `out`: ±214 748.364 8.
    let small = error(
        BinaryOp::Add,
        &Value::Money(2_147_483_647),
        &Value::Money(1),
        SqlType::SmallMoney,
    );
    assert_eq!(small.number, 8115);
    assert_eq!(
        small.message,
        "Converting expression to data type smallmoney overflowed."
    );

    assert_eq!(
        error(
            BinaryOp::Div,
            &Value::Money(1),
            &Value::Money(0),
            SqlType::Money
        )
        .number,
        8134
    );
}

/// `float` and `real`: an approximate computation, but a division by zero is still error
/// 8134 and not an IEEE infinity.
#[test]
fn float_arithmetic() {
    let third = value(
        BinaryOp::Div,
        &Value::F64(1.0),
        &Value::F64(3.0),
        SqlType::Float,
    );
    match third {
        Value::F64(v) => assert!((v - 1.0 / 3.0).abs() < 1e-15, "{v}"),
        other => panic!("{other:?} is not a float"),
    }

    let zero = error(
        BinaryOp::Div,
        &Value::F64(1.0),
        &Value::F64(0.0),
        SqlType::Float,
    );
    assert_eq!(zero.number, 8134);
    assert_eq!(zero.message, "Division by zero.");

    assert_eq!(
        value(
            BinaryOp::Add,
            &Value::F32(1.5),
            &Value::F32(1.5),
            SqlType::Real
        ),
        Value::F32(3.0)
    );

    // An infinite result is an overflow, not an infinity.
    let huge = error(
        BinaryOp::Mul,
        &Value::F64(1e308),
        &Value::F64(10.0),
        SqlType::Float,
    );
    assert_eq!(huge.number, 8115);
    assert_eq!(
        huge.message,
        "Converting expression to data type float overflowed."
    );
    // The same computation whose result leaves `real` but not `float`.
    let real = error(
        BinaryOp::Mul,
        &Value::F32(1e38),
        &Value::F32(10.0),
        SqlType::Real,
    );
    assert_eq!(real.number, 8115);
}

/// Concatenation, on characters and on bytes, truncated to the declared length of the
/// result.
#[test]
fn concatenation() {
    assert_eq!(
        value(
            BinaryOp::Concat,
            &text("ab"),
            &text("cd"),
            SqlType::VarChar(Len::Fixed(10))
        ),
        text("abcd")
    );
    // Truncation is silent.
    assert_eq!(
        value(
            BinaryOp::Concat,
            &text("ab"),
            &text("cd"),
            SqlType::VarChar(Len::Fixed(3))
        ),
        text("abc")
    );
    // `varchar(max)` never truncates.
    assert_eq!(
        value(
            BinaryOp::Concat,
            &text("ab"),
            &text("cd"),
            SqlType::VarChar(Len::Max)
        ),
        text("abcd")
    );
    // The `+` token spells the same operation.
    assert_eq!(
        value(
            BinaryOp::Add,
            &text("ab"),
            &text("cd"),
            SqlType::NVarChar(Len::Fixed(10))
        ),
        text("abcd")
    );

    assert_eq!(
        value(
            BinaryOp::Concat,
            &Value::Bytes(vec![0x01]),
            &Value::Bytes(vec![0x02]),
            SqlType::VarBinary(Len::Fixed(4))
        ),
        Value::Bytes(vec![0x01, 0x02])
    );
    assert_eq!(
        value(
            BinaryOp::Concat,
            &Value::Bytes(vec![0x01, 0x02]),
            &Value::Bytes(vec![0x03]),
            SqlType::VarBinary(Len::Fixed(2))
        ),
        Value::Bytes(vec![0x01, 0x02])
    );
}

/// `datetime` and `smalldatetime` add as day counts: `2000-01-01 + 2000-01-01` is a
/// `datetime` of the year 2100, and the same sum on two `smalldatetime` operands leaves
/// the range of that type: a value and error 8115.
#[test]
fn datetime_arithmetic() {
    // 2000-01-01 is day 36 524 of the `datetime` epoch, 1900-01-01.
    let millennium = Value::DateTime(DateTime {
        days: 36_524,
        ticks_300th: 0,
    });
    // Day 73 048, which SQL Server prints as 2099-12-31.
    assert_eq!(
        value(BinaryOp::Add, &millennium, &millennium, SqlType::DateTime),
        Value::DateTime(DateTime {
            days: 73_048,
            ticks_300th: 0
        })
    );
    assert_eq!(
        value(BinaryOp::Sub, &millennium, &millennium, SqlType::DateTime),
        Value::DateTime(DateTime {
            days: 0,
            ticks_300th: 0
        })
    );

    let overflow = error(
        BinaryOp::Add,
        &millennium,
        &millennium,
        SqlType::SmallDateTime,
    );
    assert_eq!(overflow.number, 8115);
    assert_eq!(
        overflow.message,
        "Converting expression to data type smalldatetime overflowed."
    );

    // Before 1753-01-01, `datetime` overflows too.
    let below = error(
        BinaryOp::Sub,
        &Value::DateTime(DateTime {
            days: -53_690,
            ticks_300th: 0,
        }),
        &Value::DateTime(DateTime {
            days: 1,
            ticks_300th: 0,
        }),
        SqlType::DateTime,
    );
    assert_eq!(below.number, 8115);
}

/// A pair of operands from two families is a broken precondition of the crate — the
/// caller converts both operands to their common type first — so it is an internal error
/// (50000), never a SQL error a query could have caused.
#[test]
fn incompatible_operands_are_bug() {
    let e = error(
        BinaryOp::Add,
        &Value::I32(1),
        &text("a"),
        SqlType::VarChar(Len::Fixed(10)),
    );
    assert_eq!(e.number, 50000);
    assert!(e.message.starts_with("Internal error: "), "{}", e.message);

    // A concatenation of two numbers, and a `date`, which has no arithmetic at all.
    assert_eq!(
        error(
            BinaryOp::Concat,
            &Value::I32(1),
            &Value::I32(2),
            SqlType::Int
        )
        .number,
        50000
    );
    assert_eq!(
        error(
            BinaryOp::Add,
            &Value::Date(Date { days: 1 }),
            &Value::Date(Date { days: 1 }),
            SqlType::Date
        )
        .number,
        50000
    );
    // An `out` that does not match the operands is the same broken precondition.
    assert_eq!(
        error(
            BinaryOp::Add,
            &Value::I32(1),
            &Value::I32(2),
            SqlType::Float
        )
        .number,
        50000
    );
}
