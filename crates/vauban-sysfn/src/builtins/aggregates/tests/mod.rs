use super::*;
use crate::all;
use crate::builtins::register_builtins;
use vauban_types::{Decimal, Len, SqlString};

fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

fn decimal(precision: u8, scale: u8) -> TypeInfo {
    TypeInfo::new(SqlType::Decimal { precision, scale }, true)
}

fn numeric(precision: u8, scale: u8) -> TypeInfo {
    TypeInfo::new(SqlType::Numeric { precision, scale }, true)
}

fn varchar(length: u16) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(length)), true)
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

fn dec(mantissa: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal(Decimal {
        mantissa,
        precision,
        scale,
    })
}

/// Builds the accumulator of `def` over an argument of type `arg`.
fn state(def: &FunctionDef, arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
    let factory = def
        .aggregate
        .expect("an aggregate definition must expose its factory");
    factory(arg)
}

/// Feeds `values` to a fresh accumulator of `def` and returns its result.
fn aggregate(def: &FunctionDef, arg: &TypeInfo, values: &[Value]) -> SqlResult<Value> {
    let mut state = state(def, arg)?;
    for value in values {
        state.step(value)?;
    }
    state.finish()
}

/// Feeds `values` to a fresh accumulator of `def` and returns its `null_eliminated`
/// flag together with its result.
fn aggregate_with_flag(
    def: &FunctionDef,
    arg: &TypeInfo,
    values: &[Value],
) -> SqlResult<(Value, bool)> {
    let mut state = state(def, arg)?;
    for value in values {
        state.step(value)?;
    }
    let flag = state.null_eliminated();
    Ok((state.finish()?, flag))
}

#[test]
fn aggregate_field_matches_kind() {
    register_builtins();
    for def in all() {
        assert_eq!(
            def.aggregate.is_some(),
            def.kind == FunctionKind::Aggregate,
            "{} breaks the invariant `aggregate.is_some() == (kind == Aggregate)`",
            def.name
        );
    }
    // The six of this module are really in there, under any case.
    for name in ["count", "COUNT_BIG", "Sum", "avg", "MIN", "max"] {
        let def = crate::lookup(name).expect("the aggregate must be registered");
        assert_eq!(def.kind, FunctionKind::Aggregate);
        assert_eq!(def.arity, Arity::Exact(1));
        assert!(def.deterministic);
    }
}

#[test]
fn count_ignores_nulls() {
    let values = [Value::I32(1), Value::Null, Value::I32(3)];
    assert_eq!(
        aggregate(&COUNT_DEF, &int(true), &values),
        Ok(Value::I32(2))
    );
    assert_eq!(
        aggregate(&COUNT_BIG_DEF, &int(true), &values),
        Ok(Value::I64(2))
    );

    assert_eq!(aggregate(&COUNT_DEF, &int(true), &[]), Ok(Value::I32(0)));
    assert_eq!(
        aggregate(&COUNT_BIG_DEF, &int(true), &[]),
        Ok(Value::I64(0))
    );

    let nulls = [Value::Null, Value::Null];
    assert_eq!(
        aggregate_with_flag(&COUNT_DEF, &int(true), &nulls),
        Ok((Value::I32(0), true))
    );
    // A group without any NULL leaves the flag down.
    assert_eq!(
        aggregate_with_flag(&COUNT_DEF, &int(true), &[Value::I32(1)]),
        Ok((Value::I32(1), false))
    );

    // COUNT is an int and never NULL, COUNT_BIG a bigint.
    let counted = (COUNT_DEF.return_type)(&[int(true)]).expect("COUNT must type");
    assert_eq!(counted.ty, SqlType::Int);
    assert!(!counted.nullable);
    let counted_big = (COUNT_BIG_DEF.return_type)(&[int(true)]).expect("COUNT_BIG must type");
    assert_eq!(counted_big.ty, SqlType::BigInt);
    assert!(!counted_big.nullable);
}

#[test]
fn sum_of_ints_is_int() {
    assert_eq!(
        aggregate(&SUM_DEF, &int(true), &[Value::I32(1), Value::I32(2)]),
        Ok(Value::I32(3))
    );

    let from_smallint =
        (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::SmallInt, true)]).expect("must type");
    assert_eq!(from_smallint.ty, SqlType::Int);
    assert!(from_smallint.nullable);
    let from_tinyint =
        (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::TinyInt, true)]).expect("must type");
    assert_eq!(from_tinyint.ty, SqlType::Int);
    let from_bigint =
        (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::BigInt, true)]).expect("must type");
    assert_eq!(from_bigint.ty, SqlType::BigInt);
    let from_real =
        (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::Real, true)]).expect("must type");
    assert_eq!(from_real.ty, SqlType::Float);
    let from_smallmoney =
        (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::SmallMoney, true)]).expect("must type");
    assert_eq!(from_smallmoney.ty, SqlType::Money);

    // A smallint sums in an int: the widening happens before the addition, so the
    // total is not bounded by the argument type.
    let smallint = TypeInfo::new(SqlType::SmallInt, true);
    assert_eq!(
        aggregate(
            &SUM_DEF,
            &smallint,
            &[Value::I16(30_000), Value::I16(30_000)]
        ),
        Ok(Value::I32(60_000))
    );
}

#[test]
fn sum_of_decimal_is_38_s() {
    let from_decimal = (SUM_DEF.return_type)(&[decimal(9, 2)]).expect("must type");
    assert_eq!(
        from_decimal.ty,
        SqlType::Decimal {
            precision: 38,
            scale: 2
        }
    );
    let from_numeric = (SUM_DEF.return_type)(&[numeric(5, 4)]).expect("must type");
    assert_eq!(
        from_numeric.ty,
        SqlType::Numeric {
            precision: 38,
            scale: 4
        }
    );
    // The sum itself keeps the scale of the argument.
    assert_eq!(
        aggregate(&SUM_DEF, &decimal(9, 2), &[dec(100, 9, 2), dec(250, 9, 2)]),
        Ok(dec(350, 38, 2))
    );
}

#[test]
fn sum_overflow_is_an_error() {
    let err = aggregate(&SUM_DEF, &int(true), &[Value::I32(i32::MAX), Value::I32(1)])
        .expect_err("the sum must overflow the int result type");
    assert_eq!(err.number, 8115);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 2);
    assert_eq!(
        err.message,
        "Converting expression to data type int overflowed."
    );
}

#[test]
fn sum_of_a_string_is_8117() {
    let err = match state(&SUM_DEF, &varchar(10)) {
        Err(err) => err,
        // `Box<dyn AggregateState>` is not `Debug`, so `expect_err` is out of reach.
        Ok(_) => panic!("a varchar cannot be summed"),
    };
    assert_eq!(err.number, 8117);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "Data type varchar is not accepted by the sum operator."
    );
    // The same refusal at binding time, and the same for AVG with its own word.
    let bound = (SUM_DEF.return_type)(&[varchar(10)]).expect_err("must be refused");
    assert_eq!(bound.number, 8117);
    assert_eq!(bound.message, err.message);
    let avg = (AVG_DEF.return_type)(&[varchar(10)]).expect_err("must be refused");
    assert_eq!(
        avg.message,
        "Data type varchar is not accepted by the avg operator."
    );
    // `bit` is no more summable than a string.
    let bit =
        (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::Bit, true)]).expect_err("must be refused");
    assert_eq!(bit.number, 8117);
    assert_eq!(
        bit.message,
        "Data type bit is not accepted by the sum operator."
    );
    // The name of the type comes from `SqlType::error_name`: `numeric` for a
    // `decimal`, which is why a decimal argument is accepted and a date is not.
    let date = (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::Date, true)]).expect_err("refused");
    assert_eq!(
        date.message,
        "Data type date is not accepted by the sum operator."
    );
}

#[test]
fn avg_of_int_truncates() {
    assert_eq!(
        aggregate(
            &AVG_DEF,
            &int(true),
            &[Value::I32(1), Value::I32(2), Value::I32(2)]
        ),
        Ok(Value::I32(1))
    );
    // Truncation towards zero for a negative average: -5 / 2 is -2, not -3.
    assert_eq!(
        aggregate(&AVG_DEF, &int(true), &[Value::I32(-3), Value::I32(-2)]),
        Ok(Value::I32(-2))
    );
    let typed = (AVG_DEF.return_type)(&[int(true)]).expect("must type");
    assert_eq!(typed.ty, SqlType::Int);
    assert!(typed.nullable);
}

#[test]
fn avg_of_decimal_keeps_the_scale() {
    // The scale of the argument is never lost, but the division raises it to 6: the
    // result is `decimal(38, 2)` divided by an `int`, not `decimal(38, 2)`:
    // SELECT AVG(1.50); answers 1.500000.
    let typed = (AVG_DEF.return_type)(&[decimal(9, 2)]).expect("must type");
    assert_eq!(
        typed.ty,
        SqlType::Decimal {
            precision: 38,
            scale: 6
        }
    );
    assert!(typed.nullable);
    // The `numeric` spelling survives the division (`numeric`, not `decimal`).
    let from_numeric = (AVG_DEF.return_type)(&[numeric(5, 4)]).expect("must type");
    assert_eq!(
        from_numeric.ty,
        SqlType::Numeric {
            precision: 38,
            scale: 6
        }
    );
    // 1.00 and 2.00 average to 1.50, written at the scale of the quotient.
    assert_eq!(
        aggregate(&AVG_DEF, &decimal(9, 2), &[dec(100, 9, 2), dec(200, 9, 2)]),
        Ok(dec(1_500_000, 38, 6))
    );
    // 1.00, 2.00 and 2.00 average to 1.666666: the quotient is truncated, not
    // rounded (a rounding would have written 1.666667).
    assert_eq!(
        aggregate(
            &AVG_DEF,
            &decimal(9, 2),
            &[dec(100, 9, 2), dec(200, 9, 2), dec(200, 9, 2)]
        ),
        Ok(dec(1_666_666, 38, 6))
    );
}

#[test]
fn sum_and_avg_of_the_other_families() {
    // float and real both aggregate as float, money and smallmoney as money: the
    // widening happens at `step`, so a `real` value is added as an `f64`.
    let real = TypeInfo::new(SqlType::Real, true);
    assert_eq!(
        aggregate(&SUM_DEF, &real, &[Value::F32(1.5), Value::F32(2.25)]),
        Ok(Value::F64(3.75))
    );
    assert_eq!(
        aggregate(&AVG_DEF, &real, &[Value::F32(1.5), Value::F32(2.5)]),
        Ok(Value::F64(2.0))
    );
    assert_eq!(
        (AVG_DEF.return_type)(std::slice::from_ref(&real))
            .expect("must type")
            .ty,
        SqlType::Float
    );

    // money is an exact integer of ten-thousandths: 1.5000 + 2.2500 = 3.7500.
    let money = TypeInfo::new(SqlType::Money, true);
    assert_eq!(
        aggregate(
            &SUM_DEF,
            &money,
            &[Value::Money(15_000), Value::Money(22_500)]
        ),
        Ok(Value::Money(37_500))
    );
    assert_eq!(
        aggregate(
            &AVG_DEF,
            &money,
            &[Value::Money(15_000), Value::Money(22_500)]
        ),
        Ok(Value::Money(18_750))
    );
    assert_eq!(
        (AVG_DEF.return_type)(std::slice::from_ref(&money))
            .expect("must type")
            .ty,
        SqlType::Money
    );

    // bigint stays bigint on both sides, and the average is still integral.
    let bigint = TypeInfo::new(SqlType::BigInt, true);
    assert_eq!(
        aggregate(&AVG_DEF, &bigint, &[Value::I64(5), Value::I64(2)]),
        Ok(Value::I64(3))
    );
    assert_eq!(
        (AVG_DEF.return_type)(std::slice::from_ref(&bigint))
            .expect("must type")
            .ty,
        SqlType::BigInt
    );
}

#[test]
fn min_max_use_the_collation() {
    let ty = varchar(10);
    assert_eq!(ty.collation, Some(Collation::DEFAULT));
    let values = [text("abc"), text("ABD")];
    // Under the default collation 'abc' < 'ABD'; an ordinal comparison of the bytes
    // would answer 'ABD' for the minimum.
    assert_eq!(aggregate(&MIN_DEF, &ty, &values), Ok(text("abc")));
    assert_eq!(aggregate(&MAX_DEF, &ty, &values), Ok(text("ABD")));
    assert_eq!(aggregate(&MIN_DEF, &ty, &[]), Ok(Value::Null));

    let typed = (MIN_DEF.return_type)(std::slice::from_ref(&ty)).expect("must type");
    assert_eq!(typed.ty, SqlType::VarChar(Len::Fixed(10)));
    assert_eq!(typed.collation, Some(Collation::DEFAULT));
    assert!(typed.nullable);
    assert_eq!((MAX_DEF.return_type)(std::slice::from_ref(&ty)), Ok(typed));

    // The factory remembers the collation of the argument...
    assert_eq!(
        ExtremumState::new(&ty, Ordering::Less).collation,
        Collation::DEFAULT
    );
    // ...and falls back on the default one when the type carries none.
    let uncollated = TypeInfo {
        collation: None,
        ..ty
    };
    assert_eq!(
        ExtremumState::new(&uncollated, Ordering::Less).collation,
        Collation::DEFAULT
    );
    assert_eq!(aggregate(&MIN_DEF, &uncollated, &values), Ok(text("abc")));
}

#[test]
fn empty_group_is_null_except_count() {
    for def in [&SUM_DEF, &AVG_DEF, &MIN_DEF, &MAX_DEF] {
        assert_eq!(
            aggregate(def, &int(true), &[]),
            Ok(Value::Null),
            "{} over an empty group",
            def.name
        );
        // A group of nothing but NULLs is an empty group that raises the flag.
        assert_eq!(
            aggregate_with_flag(def, &int(true), &[Value::Null]),
            Ok((Value::Null, true)),
            "{} over a group of NULLs",
            def.name
        );
    }
    assert_eq!(aggregate(&COUNT_DEF, &int(true), &[]), Ok(Value::I32(0)));
}

#[test]
fn null_eliminated_flag() {
    assert_eq!(
        aggregate_with_flag(&SUM_DEF, &int(true), &[Value::I32(1), Value::Null]),
        Ok((Value::I32(1), true))
    );
    assert_eq!(
        aggregate_with_flag(&SUM_DEF, &int(true), &[Value::I32(1)]),
        Ok((Value::I32(1), false))
    );
    // The same for the other four that skip NULLs.
    assert_eq!(
        aggregate_with_flag(&AVG_DEF, &int(true), &[Value::I32(4), Value::Null]),
        Ok((Value::I32(4), true))
    );
    assert_eq!(
        aggregate_with_flag(&MAX_DEF, &int(true), &[Value::I32(4), Value::Null]),
        Ok((Value::I32(4), true))
    );
    assert_eq!(
        aggregate_with_flag(&COUNT_BIG_DEF, &int(true), &[Value::Null]),
        Ok((Value::I64(0), true))
    );
}

#[test]
fn eval_of_an_aggregate_is_a_bug() {
    let ctx = crate::context::StaticContext::default();
    let result = TypeInfo::new(SqlType::Int, true);
    for def in [
        &COUNT_DEF,
        &COUNT_BIG_DEF,
        &SUM_DEF,
        &AVG_DEF,
        &MIN_DEF,
        &MAX_DEF,
    ] {
        let args = EvalArgs {
            values: &[Value::I32(1)],
            types: std::slice::from_ref(&result),
            result: &result,
        };
        let err = (def.eval)(&args, &ctx).expect_err("an aggregate has no scalar eval");
        assert!(
            err.message.contains("use FunctionDef::aggregate"),
            "{}: {}",
            def.name,
            err.message
        );
    }
}
