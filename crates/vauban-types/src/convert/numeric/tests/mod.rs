use super::*;
use crate::convert::convert;

fn float(v: f64) -> (Value, TypeInfo) {
    (Value::F64(v), TypeInfo::new(SqlType::Float, false))
}

fn real(v: f32) -> (Value, TypeInfo) {
    (Value::F32(v), TypeInfo::new(SqlType::Real, false))
}

/// An approximate source too large for an integer target raises 232 with the value,
/// except towards `bigint`, which raises 8115 on `expression`.
///
/// `SELECT CAST(CAST(1e20 AS float) AS tinyint);` raises 232 state 1,
/// `… AS smallint);` state 2, `… AS int);` state 3, `… AS money);` state 2, while
/// `… AS bigint);` raises 8115, severity 16, state 2, on `expression`. The `real`
/// source, `-1e20`, `1e300` and `POWER(CAST(2 AS bigint), 100)` answer the same.
#[test]
fn float_overflow_is_232_except_towards_bigint() {
    for (to, state) in [
        (SqlType::TinyInt, 1),
        (SqlType::SmallInt, 2),
        (SqlType::Int, 3),
        (SqlType::Money, 2),
    ] {
        let (v, from) = float(1e20);
        let e = convert(&v, &from, &TypeInfo::new(to, true), None).unwrap_err();
        assert_eq!(e.number, 232, "{to:?}");
        assert_eq!(e.state, state, "{to:?}");
    }

    let bigint = TypeInfo::new(SqlType::BigInt, true);
    for (v, from) in [float(1e20), float(-1e20), float(1e300), real(1e20)] {
        let e = convert(&v, &from, &bigint, None).unwrap_err();
        assert_eq!(e.number, 8115);
        assert_eq!(e.severity, 16);
        assert_eq!(e.state, 2);
        assert_eq!(
            e.message,
            "Converting expression to data type bigint overflowed."
        );
    }
}

/// The exact expansion of a double has as many decimal digits as it has significant
/// bits below the binary point: none for an integer, 1074 for the smallest subnormal.
#[test]
fn fraction_bits_counts_the_digits_of_the_exact_expansion() {
    assert_eq!(fraction_bits(1.0), 0);
    assert_eq!(fraction_bits(3.0e15), 0);
    assert_eq!(fraction_bits(1.0e300), 0);
    assert_eq!(fraction_bits(0.5), 1);
    assert_eq!(fraction_bits(0.75), 2);
    assert_eq!(fraction_bits(1.5), 1);
    // 0.1 = 3602879701896397 × 2^-55.
    assert_eq!(fraction_bits(0.1), 55);
    assert_eq!(fraction_bits(f64::MIN_POSITIVE), 1022);
    assert_eq!(fraction_bits(5e-324), 1074);
    assert_eq!(fraction_bits(-0.1), 55);
    assert_eq!(fraction_bits(0.0), 0);
}

/// `round_scaled` reads the exact value: a subnormal is a long string of zeros at any
/// scale of `numeric`, a value that is not finite or too wide for an `i128` is `None`,
/// and both zeros are zero.
#[test]
fn round_scaled_reads_the_exact_value() {
    assert_eq!(round_scaled(0.0, 38), Some(0));
    assert_eq!(round_scaled(-0.0, 4), Some(0));
    assert_eq!(round_scaled(5e-324, 38), Some(0));
    assert_eq!(round_scaled(f64::MIN_POSITIVE, 38), Some(0));
    // 2^-128 = 2.938…e-39 is below half a unit of the 38th decimal, so zero; 2^-127 =
    // 5.877…e-39 is above it, so one unit.
    assert_eq!(round_scaled(2f64.powi(-128), 38), Some(0));
    assert_eq!(round_scaled(2f64.powi(-127), 38), Some(1));
    assert_eq!(
        round_scaled(0.1, 37),
        Some(1_000_000_000_000_000_055_511_151_231_257_827_021)
    );
    assert_eq!(round_scaled(-0.125, 2), Some(-13));
    // The double nearest 1.005 is 1.00499999999999989…: below the half.
    assert_eq!(round_scaled(1.005, 2), Some(100));
    assert_eq!(round_scaled(1e23, 0), Some(99_999_999_999_999_991_611_392));
    assert_eq!(
        round_scaled(1e38, 0),
        Some(99_999_999_999_999_997_748_809_823_456_034_029_568)
    );
    assert_eq!(
        round_scaled(1.7e38, 0),
        Some(169_999_999_999_999_998_061_923_293_023_115_935_744)
    );
    // 2^127 does not fit an `i128`, nor does anything wider.
    assert_eq!(round_scaled(2f64.powi(127), 0), None);
    assert_eq!(round_scaled(1e39, 0), None);
    assert_eq!(round_scaled(1e308, 0), None);
    assert_eq!(round_scaled(f64::MAX, 38), None);
    assert_eq!(round_scaled(f64::INFINITY, 0), None);
    assert_eq!(round_scaled(f64::NAN, 0), None);
}

/// The decimal text of `m / 10^s` is what the standard library rounds, once, to the
/// target precision: the same exact value gives a different last bit in `float` and in
/// `real` when it sits on a `real` midpoint.
#[test]
fn exact_to_float_and_real_round_once_each() {
    assert_eq!(
        exact_to_f64(19_991_145_855_581_982, 10).map(f64::to_bits),
        Ok(0x413E_810A_95E7_245F)
    );
    assert_eq!(exact_to_f64(1, 0), Ok(1.0));
    assert_eq!(exact_to_f64(-15, 1), Ok(-1.5));
    assert_eq!(exact_to_f64(0, 38), Ok(0.0));
    assert_eq!(exact_to_f64(10i128.pow(38) - 1, 38), Ok(1.0));
    assert_eq!(
        exact_to_f32(83_886_085_000_000_001, 10).map(f32::to_bits),
        Ok(0x4B00_0001)
    );
    assert_eq!(exact_to_f32(16_777_217, 0), Ok(16_777_216.0));
    assert_eq!(exact_to_f32(-(10i128.pow(38) - 1), 0), Ok(-1e38));
}

/// A `datetime` source keeps its own 8115 towards `bigint` as towards any integer:
/// `SELECT CAST(CAST('2000-01-01' AS datetime) AS tinyint);`.
#[test]
fn datetime_overflow_stays_8115() {
    let from = TypeInfo::new(SqlType::DateTime, false);
    let v = Value::DateTime(crate::DateTime {
        days: 36_524,
        ticks_300th: 0,
    });
    let e = convert(&v, &from, &TypeInfo::new(SqlType::TinyInt, true), None).unwrap_err();
    assert_eq!(e.number, 8115);
}

/// Which integral source raises 8115 and which raises 220.
///
/// With `DECLARE @n <source> = <value>; SELECT CAST(@n AS <target>);` on the six
/// couples of integers that can overflow: a `bigint` source raises 8115 state 2 on
/// `int`, `smallint` and `tinyint`; an `int` source raises 220 on `smallint` (state 1)
/// and `tinyint` (state 2), and a `smallint` source raises 220 on `tinyint`. The
/// bounds are the counter-vectors: 2147483647, 32767 and 255 go through, one more
/// does not.
#[test]
fn a_bigint_source_overflows_with_8115_where_an_int_source_overflows_with_220() {
    let bigint = TypeInfo::new(SqlType::BigInt, true);
    for to in [SqlType::Int, SqlType::SmallInt, SqlType::TinyInt] {
        let e = convert(
            &Value::I64(99_999_999_999),
            &bigint,
            &TypeInfo::new(to, true),
            None,
        )
        .unwrap_err();
        assert_eq!((e.number, e.state), (8115, 2), "{to:?}");
        assert_eq!(
            e.message,
            format!(
                "Converting expression to data type {} overflowed.",
                to.error_name()
            )
        );
        // The same amount, held in the same `i128`, but declared `int`: 220 towards
        // `smallint` (state 1) and `tinyint` (state 2), and `int` -> `int` goes through.
        let int = TypeInfo::new(SqlType::Int, true);
        let got = convert(&Value::I32(99_999), &int, &TypeInfo::new(to, true), None);
        match to {
            SqlType::Int => assert_eq!(got, Ok(Value::I32(99_999))),
            SqlType::SmallInt => {
                let e = got.expect_err("220");
                assert_eq!((e.number, e.state), (220, 1));
            }
            _ => {
                let e = got.expect_err("220");
                assert_eq!((e.number, e.state), (220, 2));
            }
        }
    }
    let smallint = TypeInfo::new(SqlType::SmallInt, true);
    let e = convert(
        &Value::I16(300),
        &smallint,
        &TypeInfo::new(SqlType::TinyInt, true),
        None,
    )
    .unwrap_err();
    assert_eq!((e.number, e.state), (220, 2));
    assert_eq!(e.message, "Value out of range for data type tinyint: 300.");
    // The bounds of each target are reached, not refused.
    for (value, to, expected) in [
        (2_147_483_647i64, SqlType::Int, Value::I32(2_147_483_647)),
        (-2_147_483_648, SqlType::Int, Value::I32(-2_147_483_648)),
        (32_767, SqlType::SmallInt, Value::I16(32_767)),
        (255, SqlType::TinyInt, Value::I8(255)),
    ] {
        assert_eq!(
            convert(&Value::I64(value), &bigint, &TypeInfo::new(to, true), None),
            Ok(expected),
            "{value} {to:?}"
        );
    }
    for (value, to) in [
        (2_147_483_648i64, SqlType::Int),
        (32_768, SqlType::SmallInt),
        (256, SqlType::TinyInt),
        (-1, SqlType::TinyInt),
    ] {
        let e = convert(&Value::I64(value), &bigint, &TypeInfo::new(to, true), None).unwrap_err();
        assert_eq!(e.number, 8115, "{value} {to:?}");
    }
}
