use super::*;
use crate::context::StaticContext;
use vauban_types::Len;

fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn float() -> TypeInfo {
    TypeInfo::new(SqlType::Float, true)
}

fn decimal(precision: u8, scale: u8) -> TypeInfo {
    TypeInfo::new(SqlType::Decimal { precision, scale }, true)
}

fn numeric(precision: u8, scale: u8) -> TypeInfo {
    TypeInfo::new(SqlType::Numeric { precision, scale }, true)
}

fn dec(mantissa: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal(Decimal {
        mantissa,
        precision,
        scale,
    })
}

/// Evaluates `def` the way the `executor` does: the result type of the call comes from
/// `return_type`, so a test never has to guess it.
fn eval(def: &FunctionDef, values: &[Value], types: &[TypeInfo]) -> SqlResult<Value> {
    let result = (def.return_type)(types)?;
    let args = EvalArgs {
        values,
        types,
        result: &result,
    };
    (def.eval)(&args, &StaticContext::default())
}

/// Evaluates a one-argument function on one value of one type.
fn eval_one(def: &FunctionDef, value: Value, ty: &TypeInfo) -> SqlResult<Value> {
    eval(def, &[value], std::slice::from_ref(ty))
}

/// Evaluates `ROUND(x, length)`, `length` being an `int`.
fn round(value: Value, ty: &TypeInfo, length: i32) -> SqlResult<Value> {
    eval(
        &ROUND_DEF,
        &[value, Value::I32(length)],
        &[ty.clone(), int()],
    )
}

/// Evaluates `ROUND(x, length, function)`.
fn round_with(value: Value, ty: &TypeInfo, length: i32, function: i32) -> SqlResult<Value> {
    eval(
        &ROUND_DEF,
        &[value, Value::I32(length), Value::I32(function)],
        &[ty.clone(), int(), int()],
    )
}

/// Evaluates `POWER(base, exponent)` on two values of two types.
fn power(base: Value, base_ty: &TypeInfo, exponent: Value, exp_ty: &TypeInfo) -> SqlResult<Value> {
    eval(
        &POWER_DEF,
        &[base, exponent],
        &[base_ty.clone(), exp_ty.clone()],
    )
}

/// The `f64` a result carries, for the comparisons that need a tolerance.
fn as_f64(value: &Value) -> f64 {
    match value {
        Value::F64(f) => *f,
        other => panic!("expected a float, got {other:?}"),
    }
}

#[test]
fn abs_keeps_the_type() {
    assert_eq!(
        eval_one(&ABS_DEF, Value::I32(-5), &int()),
        Ok(Value::I32(5))
    );
    assert_eq!(
        eval_one(&ABS_DEF, Value::F64(-1.5), &float()),
        Ok(Value::F64(1.5))
    );
    assert_eq!(
        eval_one(&ABS_DEF, dec(-1550, 5, 2), &decimal(5, 2)),
        Ok(dec(1550, 5, 2))
    );
    assert_eq!(eval_one(&ABS_DEF, Value::Null, &int()), Ok(Value::Null));

    assert_eq!(abs_return_type(&[int()]).map(|t| t.ty), Ok(SqlType::Int));
    assert_eq!(
        abs_return_type(&[float()]).map(|t| t.ty),
        Ok(SqlType::Float)
    );
    assert_eq!(
        abs_return_type(&[decimal(5, 2)]).map(|t| t.ty),
        Ok(SqlType::Decimal {
            precision: 5,
            scale: 2
        })
    );
}

#[test]
fn abs_int_min_overflows() {
    // `i32::MIN.abs()` would panic in `debug`; the function answers the overflow
    // error (`SELECT ABS(CAST(-2147483648 AS int));`).
    let err = eval_one(&ABS_DEF, Value::I32(i32::MIN), &int())
        .expect_err("the magnitude of i32::MIN is not an int");
    assert_eq!(err.number, 8115);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 2);
    assert_eq!(
        err.message,
        "Converting expression to data type int overflowed."
    );
    assert_eq!(err, SqlError::arithmetic_overflow("expression", "int"));

    // The same at the two other widths that have a smallest value.
    let bigint = TypeInfo::new(SqlType::BigInt, true);
    let err = eval_one(&ABS_DEF, Value::I64(i64::MIN), &bigint)
        .expect_err("the magnitude of i64::MIN is not a bigint");
    assert_eq!(err.number, 8115);
    let money = TypeInfo::new(SqlType::Money, true);
    let err = eval_one(&ABS_DEF, Value::Money(i64::MIN), &money)
        .expect_err("the magnitude of the smallest money is not a money");
    assert_eq!(err.number, 8115);
    assert_eq!(
        err.message,
        "Converting expression to data type money overflowed."
    );
}

#[test]
fn ceiling_floor_values() {
    let ty = numeric(5, 2);
    assert_eq!(
        eval_one(&CEILING_DEF, dec(12345, 5, 2), &ty),
        Ok(dec(124, 5, 0))
    );
    assert_eq!(
        eval_one(&FLOOR_DEF, dec(12345, 5, 2), &ty),
        Ok(dec(123, 5, 0))
    );
    assert_eq!(
        eval_one(&CEILING_DEF, dec(-12345, 5, 2), &ty),
        Ok(dec(-123, 5, 0))
    );
    assert_eq!(
        eval_one(&FLOOR_DEF, dec(-12345, 5, 2), &ty),
        Ok(dec(-124, 5, 0))
    );
    assert_eq!(
        eval_one(&CEILING_DEF, Value::I32(5), &int()),
        Ok(Value::I32(5))
    );
    assert_eq!(
        eval_one(&FLOOR_DEF, Value::I32(5), &int()),
        Ok(Value::I32(5))
    );
    // A `float` keeps its family, and the negative zero of `(-0.5).ceil()` is
    // normalised (`SELECT CEILING(CAST(-0.5 AS float));` answers `0`).
    assert_eq!(
        eval_one(&CEILING_DEF, Value::F64(-0.5), &float()),
        Ok(Value::F64(0.0))
    );
    assert!(
        !as_f64(&eval_one(&CEILING_DEF, Value::F64(-0.5), &float()).expect("float"))
            .is_sign_negative()
    );
    // A `money` keeps its four decimals.
    let money = TypeInfo::new(SqlType::Money, true);
    assert_eq!(
        eval_one(&CEILING_DEF, Value::Money(15_000), &money),
        Ok(Value::Money(20_000))
    );
}

#[test]
fn ceiling_floor_return_type() {
    for def in [&CEILING_DEF, &FLOOR_DEF] {
        let ty = (def.return_type)(&[decimal(5, 2)]).expect("decimal is accepted");
        assert_eq!(
            ty.ty,
            SqlType::Decimal {
                precision: 5,
                scale: 0
            }
        );
        assert_eq!(
            (def.return_type)(&[int()]).map(|t| t.ty),
            Ok(SqlType::Int),
            "int stays int"
        );
        assert_eq!(
            (def.return_type)(&[float()]).map(|t| t.ty),
            Ok(SqlType::Float),
            "float stays float"
        );
        // `numeric` and `decimal` are two types, and neither becomes the other.
        assert_eq!(
            (def.return_type)(&[numeric(9, 4)]).map(|t| t.ty),
            Ok(SqlType::Numeric {
                precision: 9,
                scale: 0
            })
        );
    }
}

#[test]
fn round_keeps_the_scale() {
    // `SELECT ROUND(123.4545, 2);` prints `123.4500`: the scale of the argument is
    // kept, and the dropped digits come back as zeros.
    let ty = numeric(7, 4);
    assert_eq!(
        round(dec(1_234_545, 7, 4), &ty, 2),
        Ok(dec(1_234_500, 7, 4))
    );

    // `ROUND(2.5, 0)` is `3.0` and not `3`: the literal is a `numeric(2,1)`.
    let ty = numeric(2, 1);
    assert_eq!(round(dec(25, 2, 1), &ty, 0), Ok(dec(30, 2, 1)));
    assert_eq!(round(dec(-25, 2, 1), &ty, 0), Ok(dec(-30, 2, 1)));
    // Half away from zero, not to the even neighbour: 3.5 gives 4.0, not 4.0 by luck.
    assert_eq!(round(dec(35, 2, 1), &ty, 0), Ok(dec(40, 2, 1)));
    assert_eq!(round(dec(45, 2, 1), &ty, 0), Ok(dec(50, 2, 1)));

    // A `NULL` argument, wherever it is, gives `NULL`.
    assert_eq!(round(Value::Null, &ty, 0), Ok(Value::Null));
    assert_eq!(
        eval(
            &ROUND_DEF,
            &[dec(25, 2, 1), Value::Null],
            &[ty.clone(), int()]
        ),
        Ok(Value::Null)
    );
}

#[test]
fn round_negative_length() {
    assert_eq!(round(Value::I32(150), &int(), -2), Ok(Value::I32(200)));
    assert_eq!(round(Value::I32(150), &int(), -3), Ok(Value::I32(0)));
    // `SELECT ROUND(748.58, -4);` answers `0.00`: zero at the scale of the argument.
    let ty = numeric(5, 2);
    assert_eq!(round(dec(74858, 5, 2), &ty, -4), Ok(dec(0, 5, 2)));

    // `SELECT ROUND(CAST(2147483647 AS int), -1);` answers 8115.
    let err =
        round(Value::I32(i32::MAX), &int(), -1).expect_err("2147483650 is not an int any more");
    assert_eq!(err.number, 8115);
    assert_eq!(err, SqlError::arithmetic_overflow("expression", "int"));
    // The same on the precision of a `decimal`: `SELECT ROUND(9.9, 0);`.
    let err =
        round(dec(99, 2, 1), &numeric(2, 1), 0).expect_err("10.0 does not fit in a numeric(2,1)");
    assert_eq!(err.number, 8115);
    assert_eq!(
        err.message,
        "Converting expression to data type numeric overflowed."
    );
}

#[test]
fn round_truncates_with_third_argument() {
    let ty = numeric(5, 2);
    assert_eq!(
        round_with(dec(15075, 5, 2), &ty, 0, 1),
        Ok(dec(15000, 5, 2)),
        "a non-zero third argument truncates"
    );
    assert_eq!(
        round_with(dec(15075, 5, 2), &ty, 0, 0),
        Ok(dec(15100, 5, 2)),
        "a third argument of 0 rounds, like no third argument at all"
    );
    // Any non-zero value truncates, and truncation goes towards zero.
    assert_eq!(
        round_with(dec(15075, 5, 2), &ty, 0, 2),
        Ok(dec(15000, 5, 2))
    );
    assert_eq!(
        round_with(dec(-15075, 5, 2), &ty, 0, 1),
        Ok(dec(-15000, 5, 2))
    );
    assert_eq!(
        round_with(dec(-15075, 5, 2), &ty, 0, 0),
        Ok(dec(-15100, 5, 2))
    );
}

#[test]
fn round_on_a_float() {
    // `SELECT ROUND(CAST(2.5 AS float), 0), ROUND(CAST(150 AS float), -2);`
    assert_eq!(round(Value::F64(2.5), &float(), 0), Ok(Value::F64(3.0)));
    assert_eq!(
        round(Value::F64(150.0), &float(), -2),
        Ok(Value::F64(200.0))
    );
    assert_eq!(round(Value::F64(150.0), &float(), -3), Ok(Value::F64(0.0)));
    // Saturations: rounding to 400 decimals changes nothing, rounding 400 digits to
    // the left of the point leaves zero.
    assert_eq!(round(Value::F64(1.5), &float(), 400), Ok(Value::F64(1.5)));
    assert_eq!(round(Value::F64(1.5), &float(), -400), Ok(Value::F64(0.0)));
}

#[test]
fn power_uses_the_type_of_the_base() {
    assert_eq!(
        power(Value::I32(2), &int(), Value::I32(3), &int()),
        Ok(Value::I32(8))
    );
    assert_eq!(
        power(Value::I32(2), &int(), Value::I32(-1), &int()),
        Ok(Value::I32(0)),
        "0.5 truncates to 0 in the int of the base"
    );
    let root = power(Value::F64(2.0), &float(), Value::F64(0.5), &float())
        .expect("the square root of 2 is a float");
    // 1.414213562373095, to a tolerance of 1e-12.
    assert!(
        (as_f64(&root) - std::f64::consts::SQRT_2).abs() < 1e-12,
        "{root:?}"
    );
    // A `decimal` base keeps its scale and widens to 38 digits, which is what makes
    // `SELECT POWER(2.0, 100);` answer 31 digits before the point.
    assert_eq!(
        power_return_type(&[numeric(2, 1), int()]).map(|t| t.ty),
        Ok(SqlType::Numeric {
            precision: 38,
            scale: 1
        })
    );
    assert_eq!(
        power(dec(20, 2, 1), &numeric(2, 1), Value::I32(3), &int()),
        Ok(dec(80, 38, 1))
    );
}

#[test]
fn power_overflow_is_232() {
    let err = power(Value::I32(2), &int(), Value::I32(64), &int()).expect_err("2^64 is not an int");
    assert_eq!(err.number, 232);
    assert_eq!(err.severity, 16);
    // State 3, the one `SELECT POWER(CAST(2 AS int), 64);` prints.
    assert_eq!(err.state, 3);
    // The value is printed as a `%f`: six decimals and at most 17 significant digits,
    // so 2^64 loses its last three.
    assert_eq!(
        err.message,
        "Value out of range for type int: 18446744073709552000.000000."
    );
}

/// The state of an overflow belongs to the (source, target) pair, not to the number:
/// rewriting it per number would be right for `int` and wrong everywhere else.
///
/// `SELECT POWER(CAST(2 AS money), 100);` answers 232 **state 2**, where
/// `SELECT POWER(CAST(2 AS int), 64);` answers 232 state 3 (test above) and
/// `SELECT POWER(CAST(2 AS numeric(5,1)), 200);` answers 8115 state 6. The three come
/// straight from `types::convert`.
#[test]
fn power_overflow_states_follow_the_target() {
    let money = TypeInfo::new(SqlType::Money, true);
    let err = power(Value::Money(20_000), &money, Value::I32(100), &int())
        .expect_err("2^100 is not a money");
    assert_eq!(err.number, 232);
    assert_eq!(err.state, 2);

    let err = power(dec(20, 5, 1), &numeric(5, 1), Value::I32(200), &int())
        .expect_err("2^200 is not a numeric(38,1)");
    assert_eq!(err.number, 8115);
    assert_eq!(err.state, 6);
    assert_eq!(
        err.message,
        "Converting float to data type numeric overflowed."
    );
}

#[test]
fn power_domain_errors() {
    // `SELECT POWER(CAST(0 AS int), -1);` answers 8134.
    let err = power(Value::I32(0), &int(), Value::I32(-1), &int())
        .expect_err("zero to a negative power is a division by zero");
    assert_eq!(err.number, 8134);
    assert_eq!(err.message, "Division by zero.");
    // `SELECT POWER(CAST(-2 AS float), 0.5);` answers 3623.
    let err = power(Value::F64(-2.0), &float(), Value::F64(0.5), &float())
        .expect_err("the square root of a negative number is not a float");
    assert_eq!(err.number, 3623);
    // `SELECT POWER(CAST(2 AS float), 10000);` answers 8115.
    let err = power(Value::F64(2.0), &float(), Value::F64(10_000.0), &float())
        .expect_err("2^10000 is not a float");
    assert_eq!(err.number, 8115);
    assert_eq!(
        err.message,
        "Converting expression to data type float overflowed."
    );
    // `NULL` on either side gives `NULL`.
    assert_eq!(
        power(Value::Null, &int(), Value::I32(3), &int()),
        Ok(Value::Null)
    );
    assert_eq!(
        power(Value::I32(2), &int(), Value::Null, &int()),
        Ok(Value::Null)
    );
}

#[test]
fn sqrt_of_negative_is_3623() {
    let err = eval_one(&SQRT_DEF, Value::F64(-1.0), &float()).expect_err("SQRT(-1) is not a float");
    assert_eq!(err.number, 3623);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(err.message, "The floating point operation is undefined.");

    assert_eq!(
        eval_one(&SQRT_DEF, Value::F64(4.0), &float()),
        Ok(Value::F64(2.0))
    );
    assert_eq!(
        sqrt_return_type(&[int()]).map(|t| t.ty),
        Ok(SqlType::Float),
        "SQRT always answers a float"
    );
    assert_eq!(
        eval_one(&SQRT_DEF, Value::I32(4), &int()),
        Ok(Value::F64(2.0))
    );
    assert_eq!(eval_one(&SQRT_DEF, Value::Null, &int()), Ok(Value::Null));
}

#[test]
fn sign_decimal_scale_boundary_and_neighbors() {
    // The result type of SIGN on the decimal and numeric pairs around the scale bound.
    for family in [decimal, numeric] {
        for (precision, scale, result_precision, result_scale) in [
            (10, 2, 10, 2),
            (38, 37, 38, 37),
            (38, 36, 38, 36),
            (1, 0, 1, 0),
            (1, 1, 2, 1),
            (5, 5, 6, 5),
            (10, 10, 11, 10),
            (37, 37, 38, 37),
            (38, 38, 38, 37),
        ] {
            let source = family(precision, scale);
            assert_eq!(
                sign_return_type(std::slice::from_ref(&source)),
                Ok(family(result_precision, result_scale))
            );
            for sign in [-1_i128, 0, 1] {
                let mantissa = if scale == 0 {
                    sign
                } else {
                    sign * 5 * 10_i128.pow(u32::from(scale - 1))
                };
                assert_eq!(
                    eval_one(&SIGN_DEF, dec(mantissa, precision, scale), &source),
                    Ok(dec(
                        sign * 10_i128.pow(u32::from(result_scale)),
                        result_precision,
                        result_scale
                    ))
                );
            }
            assert_eq!(eval_one(&SIGN_DEF, Value::Null, &source), Ok(Value::Null));
        }
    }
}

#[test]
fn sign_keeps_the_type() {
    assert_eq!(
        eval_one(&SIGN_DEF, Value::I32(-5), &int()),
        Ok(Value::I32(-1))
    );
    assert_eq!(
        eval_one(&SIGN_DEF, Value::I32(0), &int()),
        Ok(Value::I32(0))
    );
    assert_eq!(
        eval_one(&SIGN_DEF, Value::I32(5), &int()),
        Ok(Value::I32(1))
    );
    // `SELECT SIGN(-5.5);` answers `-1.0`: the scale of the argument is kept, so the
    // mantissa is `-10` and not `-1`.
    let ty = decimal(3, 1);
    assert_eq!(eval_one(&SIGN_DEF, dec(-55, 3, 1), &ty), Ok(dec(-10, 3, 1)));
    assert_eq!(
        sign_return_type(std::slice::from_ref(&ty)).map(|t| t.ty),
        Ok(SqlType::Decimal {
            precision: 3,
            scale: 1
        })
    );
    // A `float` zero answers zero, where `f64::signum` would answer 1.
    assert_eq!(
        eval_one(&SIGN_DEF, Value::F64(0.0), &float()),
        Ok(Value::F64(0.0))
    );
    assert_eq!(
        eval_one(&SIGN_DEF, Value::F64(-1.5), &float()),
        Ok(Value::F64(-1.0))
    );
    // `SELECT SIGN(CAST(0.55 AS numeric(2,2)));` answers `1.00`, so the precision
    // widens rather than overflowing.
    assert_eq!(
        eval_one(&SIGN_DEF, dec(55, 2, 2), &numeric(2, 2)),
        Ok(dec(100, 3, 2))
    );
    assert_eq!(eval_one(&SIGN_DEF, Value::Null, &int()), Ok(Value::Null));
}

#[test]
fn rand_is_in_range() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..10_000 {
        let value = eval(&RAND_DEF, &[], &[]).expect("RAND() must succeed");
        let drawn = as_f64(&value);
        assert!((0.0..1.0).contains(&drawn), "{drawn} is outside [0, 1)");
        seen.insert(drawn.to_bits());
    }
    assert!(seen.len() > 1, "10 000 draws gave one single value");

    // The same seed gives the same value, which is the only property a query can rely
    // on: `SELECT CASE WHEN RAND(1) = RAND(1) THEN 1 ELSE 0 END;` answers 1.
    let first = eval_one(&RAND_DEF, Value::I32(1), &int()).expect("RAND(1) must succeed");
    let second = eval_one(&RAND_DEF, Value::I32(1), &int()).expect("RAND(1) must succeed");
    assert_eq!(first, second);
    assert!((0.0..1.0).contains(&as_f64(&first)));
    // Two different seeds are not expected to agree.
    let other = eval_one(&RAND_DEF, Value::I32(2), &int()).expect("RAND(2) must succeed");
    assert_ne!(first, other);
    // A `NULL` seed answers `NULL`.
    assert_eq!(
        eval_one(&RAND_DEF, Value::Null, &int()),
        Ok(Value::Null),
        "RAND(NULL) is NULL"
    );
}

#[test]
fn pi_value() {
    assert_eq!(
        eval(&PI_DEF, &[], &[]),
        Ok(Value::F64(std::f64::consts::PI))
    );
    let ty = pi_return_type(&[]).expect("PI takes no argument");
    assert_eq!(ty.ty, SqlType::Float);
    assert!(!ty.nullable, "PI() is never NULL");
}

#[test]
fn argument_types_are_refused_as_sql_server_refuses_them() {
    // 206 for the types that convert to no number.
    for def in [&ABS_DEF, &CEILING_DEF, &FLOOR_DEF, &SIGN_DEF, &SQRT_DEF] {
        let err = (def.return_type)(&[TypeInfo::new(SqlType::Time(7), true)])
            .expect_err("time is not a number");
        assert_eq!(err.number, 206, "{}", def.name);
        assert_eq!(err.state, 2);
        assert_eq!(
            err.message,
            "Type mismatch: time cannot be combined with float."
        );
    }
    let err = (ROUND_DEF.return_type)(&[
        TypeInfo::new(SqlType::VarBinary(Len::Fixed(4)), true),
        int(),
    ])
    .expect_err("varbinary is not a number");
    assert_eq!(err.number, 206);
    assert_eq!(
        err.message,
        "Type mismatch: varbinary cannot be combined with float."
    );

    // 257 for `datetime`, which has an explicit conversion but no implicit one
    // (`SELECT ABS(CAST('2020-01-01' AS datetime));`).
    let err = abs_return_type(&[TypeInfo::new(SqlType::DateTime, true)])
        .expect_err("datetime needs an explicit CONVERT");
    assert_eq!(err.number, 257);
    assert_eq!(err.state, 3);
    assert_eq!(
        err.message,
        "No implicit conversion from datetime to float; use CONVERT explicitly."
    );

    // The families that are accepted, and the overload each of them lands on.
    for (arg, expected) in [
        (SqlType::TinyInt, SqlType::Int),
        (SqlType::SmallInt, SqlType::Int),
        (SqlType::Int, SqlType::Int),
        (SqlType::BigInt, SqlType::BigInt),
        (SqlType::Money, SqlType::Money),
        (SqlType::SmallMoney, SqlType::Money),
        (SqlType::Bit, SqlType::Float),
        (SqlType::Real, SqlType::Float),
        (SqlType::Float, SqlType::Float),
        (SqlType::VarChar(Len::Fixed(10)), SqlType::Float),
    ] {
        assert_eq!(
            abs_return_type(&[TypeInfo::new(arg, true)]).map(|t| t.ty),
            Ok(expected),
            "{arg:?}"
        );
    }
}

#[test]
fn the_nine_functions_are_registered() {
    crate::register_builtins();
    for name in [
        "ABS", "CEILING", "FLOOR", "ROUND", "POWER", "SQRT", "SIGN", "RAND", "PI",
    ] {
        let def = crate::lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
        assert_eq!(def.kind, FunctionKind::Scalar);
        assert!(def.aggregate.is_none(), "{name} is not an aggregate");
        assert_eq!(def.deterministic, name != "RAND", "{name}");
    }
}
