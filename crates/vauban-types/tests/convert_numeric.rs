//! Integration test: `CAST` and `CONVERT` towards `bit`, the integer types,
//! `decimal`/`numeric`, `money`, `float` and `real`.
//!
//! Every vector below is a `SELECT CAST(...)`, or a value no query can ask for directly
//! (a `NULL` of a known type, the identity `decimal(5,2)` → `numeric(5,2)`).

use vauban_errors::SqlError;
use vauban_types::{
    Date, DateTime, DateTime2, Decimal, Len, SqlType, Time, TypeInfo, Value, convert,
};

/// The type of a source or a target, always nullable: nullability plays no part here.
fn ti(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, true)
}

/// `CAST(v AS dst)` where `v` is read as type `src`, without a style.
fn cv(v: Value, src: SqlType, dst: SqlType) -> Result<Value, SqlError> {
    convert(&v, &ti(src), &ti(dst), None)
}

/// The error of a conversion that must fail.
fn err(v: Value, src: SqlType, dst: SqlType) -> SqlError {
    match cv(v.clone(), src, dst) {
        Ok(ok) => panic!("{v:?} as {} unexpectedly gave {ok:?}", dst.declaration()),
        Err(e) => e,
    }
}

/// `numeric(p, s)`, the spelling of every exact literal of SQL Server.
fn num(precision: u8, scale: u8) -> SqlType {
    SqlType::Numeric { precision, scale }
}

/// A `decimal`/`numeric` value.
fn dec(mantissa: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal(Decimal {
        mantissa,
        precision,
        scale,
    })
}

/// Integer to integer: widening is exact, narrowing checks the range of the target and
/// raises 220, which quotes the offending value.
#[test]
fn integer_widening_and_narrowing() {
    assert_eq!(
        cv(Value::I32(1), SqlType::Int, SqlType::BigInt),
        Ok(Value::I64(1))
    );
    assert_eq!(
        cv(Value::I64(1), SqlType::BigInt, SqlType::Int),
        Ok(Value::I32(1))
    );
    assert_eq!(
        cv(Value::I32(255), SqlType::Int, SqlType::TinyInt),
        Ok(Value::I8(255))
    );

    // `tinyint` is unsigned: -1 is as much an overflow as 300.
    let e = err(Value::I32(-1), SqlType::Int, SqlType::TinyInt);
    assert_eq!(e.number, 220);
    assert_eq!(e.message, "Value out of range for data type tinyint: -1.");

    let e = err(Value::I32(300), SqlType::Int, SqlType::TinyInt);
    assert_eq!(e.number, 220);
    assert_eq!(e.message, "Value out of range for data type tinyint: 300.");

    // A `bigint` source is the integral source reported with 8115 and not with 220:
    // `DECLARE @n bigint = 40000; SELECT CAST(@n AS smallint);` raises 8115 on
    // `expression`. The crossing over the narrower targets lives in `convert::numeric`'s
    // own tests.
    let e = err(Value::I64(40_000), SqlType::BigInt, SqlType::SmallInt);
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting expression to data type smallint overflowed."
    );

    // The bounds themselves are not overflows.
    assert_eq!(
        cv(Value::I64(32_767), SqlType::BigInt, SqlType::SmallInt),
        Ok(Value::I16(32_767))
    );
    assert_eq!(
        cv(Value::I64(-2_147_483_648), SqlType::BigInt, SqlType::Int),
        Ok(Value::I32(-2_147_483_648))
    );
}

/// `bit` holds "is it zero": any non-zero value, negative or fractional, becomes 1.
#[test]
fn bit_conversions() {
    assert_eq!(
        cv(Value::I32(0), SqlType::Int, SqlType::Bit),
        Ok(Value::Bit(false))
    );
    assert_eq!(
        cv(Value::I32(2), SqlType::Int, SqlType::Bit),
        Ok(Value::Bit(true))
    );
    assert_eq!(
        cv(Value::I32(-1), SqlType::Int, SqlType::Bit),
        Ok(Value::Bit(true))
    );
    assert_eq!(
        cv(Value::Bit(true), SqlType::Bit, SqlType::Int),
        Ok(Value::I32(1))
    );
    assert_eq!(
        cv(Value::Bit(false), SqlType::Bit, SqlType::Int),
        Ok(Value::I32(0))
    );
    assert_eq!(
        cv(dec(0, 5, 2), num(5, 2), SqlType::Bit),
        Ok(Value::Bit(false))
    );
    // A fraction is not zero, even below one half.
    assert_eq!(
        cv(dec(1, 5, 2), num(5, 2), SqlType::Bit),
        Ok(Value::Bit(true))
    );
}

/// `numeric` → integer **truncates** towards zero; the overflow of an exact source is
/// 8115, not 220.
#[test]
fn numeric_to_integer_truncates() {
    assert_eq!(
        cv(dec(106_496, 6, 4), num(6, 4), SqlType::Int),
        Ok(Value::I32(10))
    );
    assert_eq!(
        cv(dec(-106_496, 6, 4), num(6, 4), SqlType::Int),
        Ok(Value::I32(-10))
    );

    // Message 8115 says `expression` and not `numeric` when the target is an integer,
    // whichever shape the source takes (literal, nested `CAST` or variable).
    let e = err(dec(2_147_483_648, 10, 0), num(10, 0), SqlType::Int);
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting expression to data type int overflowed."
    );
}

/// `numeric` → `numeric` **rounds**, half away from zero and not to the even digit.
#[test]
fn numeric_to_numeric_rounds() {
    assert_eq!(
        cv(dec(106_496, 6, 4), num(6, 4), num(10, 0)),
        Ok(dec(11, 10, 0))
    );
    // 10.6496 rounds to 10.65, not 10.64.
    assert_eq!(
        cv(dec(106_496, 6, 4), num(6, 4), num(10, 2)),
        Ok(dec(1_065, 10, 2))
    );
    assert_eq!(
        cv(dec(-106_496, 6, 4), num(6, 4), num(10, 0)),
        Ok(dec(-11, 10, 0))
    );
    assert_eq!(cv(dec(15, 2, 1), num(2, 1), num(2, 0)), Ok(dec(2, 2, 0)));
    assert_eq!(cv(dec(25, 2, 1), num(2, 1), num(2, 0)), Ok(dec(3, 2, 0)));
    assert_eq!(cv(dec(-25, 2, 1), num(2, 1), num(2, 0)), Ok(dec(-3, 2, 0)));

    // Widening the scale is exact.
    assert_eq!(
        cv(dec(15, 2, 1), num(2, 1), num(10, 4)),
        Ok(dec(15_000, 10, 4))
    );

    // The integral part must hold in `p - s` digits; the target is named `numeric` even
    // when it is declared `decimal`.
    let e = err(
        dec(1_234, 4, 0),
        num(4, 0),
        SqlType::Decimal {
            precision: 3,
            scale: 0,
        },
    );
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting numeric to data type numeric overflowed."
    );

    // An integer source names itself in the message.
    let e = err(
        Value::I32(1_234),
        SqlType::Int,
        SqlType::Decimal {
            precision: 3,
            scale: 0,
        },
    );
    assert_eq!(e.number, 8115);
    assert_eq!(e.message, "Converting int to data type numeric overflowed.");
}

/// `money` is a scale of 4: every conversion in or out of it rounds.
#[test]
fn money_conversions() {
    assert_eq!(
        cv(dec(103_496_847, 8, 7), num(8, 7), SqlType::Money),
        Ok(Value::Money(103_497))
    );
    // `money` → integer rounds, where `numeric` → integer truncates.
    assert_eq!(
        cv(Value::Money(103_497), SqlType::Money, SqlType::Int),
        Ok(Value::I32(10))
    );
    assert_eq!(
        cv(Value::Money(105_000), SqlType::Money, SqlType::Int),
        Ok(Value::I32(11))
    );
    assert_eq!(
        cv(Value::Money(103_497), SqlType::Money, num(10, 2)),
        Ok(dec(1_035, 10, 2))
    );

    // 237, state 3, which names the target.
    let e = err(Value::Money(i64::MAX), SqlType::Money, SqlType::SmallMoney);
    assert_eq!(e.number, 237);
    assert_eq!(e.state, 3);
    assert_eq!(
        e.message,
        "A money value does not fit in the result type smallmoney."
    );
    // The same number, state 1.
    let e = err(Value::Money(i64::MAX), SqlType::Money, SqlType::Int);
    assert_eq!(e.number, 237);
    assert_eq!(e.state, 1);
    assert_eq!(
        e.message,
        "A money value does not fit in the result type int."
    );

    // The three other integer targets of a money source (`DECLARE @m money = <v>; SELECT
    // CAST(@m AS <target>);` and its `smallmoney` twin):
    //
    // * an amount that still fits a four-byte money answers on the target — 220 state 7
    //   quoting the ten-thousandths towards `smallint` (`@m = 40000` prints
    //   `value = 400000000`), 232 state 11 quoting the amount towards `tinyint`
    //   (`@m = 5000` prints `value = 5000.000000`);
    // * past that bound every target answers 237 (`@m = 214749` towards `smallint`).
    let e = err(Value::Money(400_000_000), SqlType::Money, SqlType::SmallInt);
    assert_eq!(e.number, 220);
    assert_eq!(e.state, 7);
    assert_eq!(
        e.message,
        "Value out of range for data type smallint: 400000000."
    );
    let e = err(Value::Money(50_000_000), SqlType::Money, SqlType::TinyInt);
    assert_eq!(e.number, 232);
    assert_eq!(e.state, 11);
    assert_eq!(
        e.message,
        "Value out of range for type tinyint: 5000.000000."
    );
    assert_eq!(
        err(
            Value::Money(2_147_490_000),
            SqlType::Money,
            SqlType::SmallInt
        )
        .number,
        237
    );

    // A `smallmoney` source never reaches that bound and answers on the target alone:
    // 220 state 5 quoting the **amount** towards `smallint` (`DECLARE @s smallmoney =
    // 70000; SELECT CAST(@s AS smallint);` prints `value = 70000`), 8115 state 2 on
    // `expression` towards `tinyint`.
    let e = err(
        Value::Money(700_000_000),
        SqlType::SmallMoney,
        SqlType::SmallInt,
    );
    assert_eq!(e.number, 220);
    assert_eq!(e.state, 5);
    assert_eq!(
        e.message,
        "Value out of range for data type smallint: 70000."
    );
    let e = err(
        Value::Money(3_000_000),
        SqlType::SmallMoney,
        SqlType::TinyInt,
    );
    assert_eq!(e.number, 8115);
    assert_eq!(e.state, 2);
    assert_eq!(
        e.message,
        "Converting expression to data type tinyint overflowed."
    );

    // A `numeric` too large for `money` does raise 8115, and names its source there.
    let e = err(dec(10_i128.pow(20), 21, 0), num(21, 0), SqlType::Money);
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting numeric to data type money overflowed."
    );

    // An integer too large for `money` says `expression` too.
    let e = err(Value::I64(i64::MAX), SqlType::BigInt, SqlType::Money);
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting expression to data type money overflowed."
    );

    assert_eq!(
        cv(Value::F64(1.5), SqlType::Float, SqlType::Money),
        Ok(Value::Money(15_000))
    );
    assert_eq!(
        cv(Value::I64(3), SqlType::BigInt, SqlType::Money),
        Ok(Value::Money(30_000))
    );

    // The bounds of both money types, in ten-thousandths.
    assert_eq!(
        cv(Value::Money(i64::MAX), SqlType::Money, SqlType::Money),
        Ok(Value::Money(i64::MAX))
    );
    assert_eq!(
        cv(
            Value::Money(i64::from(i32::MIN)),
            SqlType::Money,
            SqlType::SmallMoney
        ),
        Ok(Value::Money(i64::from(i32::MIN)))
    );
}

/// `float` → integer truncates, `float` → `numeric` rounds, and an out-of-range `float`
/// raises 232 rather than 220 or 8115.
#[test]
fn float_conversions() {
    assert_eq!(
        cv(Value::F64(10.9), SqlType::Float, SqlType::Int),
        Ok(Value::I32(10))
    );
    assert_eq!(
        cv(Value::F64(-10.9), SqlType::Float, SqlType::Int),
        Ok(Value::I32(-10))
    );
    assert_eq!(
        cv(Value::F64(10.9), SqlType::Float, num(10, 0)),
        Ok(dec(11, 10, 0))
    );

    // The value is printed with `%f`.
    let e = err(Value::F64(1e30), SqlType::Float, SqlType::Int);
    assert_eq!(e.number, 232);
    assert_eq!(
        e.message,
        "Value out of range for type int: 1000000000000000000000000000000.000000."
    );

    // A `float` too large for `money` raises 232 too, but a `float` too large for the
    // `numeric` target raises 8115 and names `float`: the target decides, not the source.
    let e = err(Value::F64(1e30), SqlType::Float, SqlType::Money);
    assert_eq!(e.number, 232);
    assert_eq!(
        e.message,
        "Value out of range for type money: 1000000000000000000000000000000.000000."
    );
    let e = err(Value::F64(1e30), SqlType::Float, num(10, 0));
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting float to data type numeric overflowed."
    );

    assert_eq!(
        cv(Value::F64(1.0), SqlType::Float, SqlType::Real),
        Ok(Value::F32(1.0))
    );
    assert_eq!(
        cv(Value::F32(1.5), SqlType::Real, SqlType::Float),
        Ok(Value::F64(1.5))
    );
    assert_eq!(
        cv(Value::I32(3), SqlType::Int, SqlType::Float),
        Ok(Value::F64(3.0))
    );
    // An exact source reaches `float` too: it is "any source that is neither a string
    // nor a date".
    assert_eq!(
        cv(dec(150, 5, 2), num(5, 2), SqlType::Float),
        Ok(Value::F64(1.5))
    );
    assert_eq!(
        cv(Value::Money(15_000), SqlType::Money, SqlType::Float),
        Ok(Value::F64(1.5))
    );

    // A `float` too large for `real` overflows; one that merely loses digits does not.
    let e = err(Value::F64(1e40), SqlType::Float, SqlType::Real);
    assert_eq!(e.number, 232);
    assert_eq!(
        e.message,
        "Value out of range for type real: 10000000000000000000000000000000000000000.000000."
    );
    assert_eq!(
        cv(Value::F64(1e30), SqlType::Float, SqlType::Real),
        Ok(Value::F32(1e30))
    );
}

/// `datetime` → number is days plus the fraction of the day since 1900-01-01, and the
/// integer target **rounds**.
#[test]
fn datetime_to_number() {
    let midnight = Value::DateTime(DateTime {
        days: 1,
        ticks_300th: 0,
    });
    assert_eq!(
        cv(midnight, SqlType::DateTime, SqlType::Int),
        Ok(Value::I32(1))
    );

    // 18:00 is three quarters of a day: 1.75 rounds to 2.
    let six_pm = Value::DateTime(DateTime {
        days: 1,
        ticks_300th: 18 * 3_600 * 300,
    });
    assert_eq!(
        cv(six_pm.clone(), SqlType::DateTime, SqlType::Int),
        Ok(Value::I32(2))
    );
    assert_eq!(
        cv(six_pm, SqlType::DateTime, SqlType::Float),
        Ok(Value::F64(1.75))
    );

    // 2000-01-01 is day 36 526: too large for `tinyint`, and SQL Server answers 8115
    // there, not 232 as it does for a `float`.
    let y2k = Value::DateTime(DateTime {
        days: 36_526,
        ticks_300th: 0,
    });
    let e = err(y2k.clone(), SqlType::DateTime, SqlType::TinyInt);
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting expression to data type tinyint overflowed."
    );

    // Towards `numeric` the same value names its type.
    let e = err(y2k, SqlType::DateTime, num(3, 0));
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting datetime to data type numeric overflowed."
    );
}

/// `date`, `time`, `datetime2`, `datetimeoffset` and `uniqueidentifier` have no conversion
/// to a number at all: `CAST` refuses the pair of types itself.
#[test]
fn unsupported_sources_fail() {
    // Number and text of the refusal of a date and of a GUID.
    let e = err(
        Value::Date(Date { days: 730_119 }),
        SqlType::Date,
        SqlType::Int,
    );
    assert_eq!(e.number, 529);
    assert_eq!(e.message, "No explicit conversion exists from date to int.");

    let e = err(
        Value::Time(Time { ticks_100ns: 0 }),
        SqlType::Time(7),
        SqlType::Int,
    );
    assert_eq!(e.number, 529);
    assert_eq!(e.message, "No explicit conversion exists from time to int.");

    let e = err(
        Value::Guid([0; 16]),
        SqlType::UniqueIdentifier,
        SqlType::Int,
    );
    assert_eq!(e.number, 529);
    assert_eq!(
        e.message,
        "No explicit conversion exists from uniqueidentifier to int."
    );

    // A binary source towards `decimal` stays unconverted: 8114 (`SELECT CAST(CAST(0x0F
    // AS binary(1)) AS decimal(9,2));`). Towards an integer it converts, which
    // `binary_source_keeps_the_low_bytes` tests.
    let e = err(
        Value::Bytes(vec![0x01]),
        SqlType::VarBinary(vauban_types::Len::Fixed(1)),
        num(9, 2),
    );
    assert_eq!(e.number, 8114);
    assert_eq!(
        e.message,
        "Data type varbinary could not be converted to numeric."
    );

    let e = err(
        Value::DateTime2(DateTime2 {
            date: Date { days: 730_119 },
            time: Time { ticks_100ns: 0 },
        }),
        SqlType::DateTime2(7),
        SqlType::Decimal {
            precision: 10,
            scale: 2,
        },
    );
    assert_eq!(e.number, 529);
}

/// `NULL` stays `NULL` whatever the target, and a conversion that changes nothing gives
/// back the very same value.
#[test]
fn null_and_identity() {
    assert_eq!(
        cv(Value::Null, SqlType::Int, SqlType::TinyInt),
        Ok(Value::Null)
    );
    assert_eq!(
        cv(Value::I32(7), SqlType::Int, SqlType::Int),
        Ok(Value::I32(7))
    );
    // `decimal(5,2)` and `numeric(5,2)` share one representation: only the name changes.
    assert_eq!(
        cv(
            dec(150, 5, 2),
            SqlType::Decimal {
                precision: 5,
                scale: 2
            },
            num(5, 2)
        ),
        Ok(dec(150, 5, 2))
    );
}

/// The `CONVERT` style is accepted and ignored for every numeric target.
#[test]
fn style_is_ignored() {
    let from = ti(SqlType::Float);
    let to = ti(SqlType::Int);
    assert_eq!(
        convert(&Value::F64(10.9), &from, &to, Some(2)),
        Ok(Value::I32(10))
    );
}

// ---------------------------------------------------------------------------------------
// Rounding between the exact and the approximate types.
// ---------------------------------------------------------------------------------------

/// The bits of a `float`, as `CAST(x AS binary(8))` shows them.
fn bits64(v: Result<Value, SqlError>) -> String {
    match v {
        Ok(Value::F64(f)) => format!("{:016X}", f.to_bits()),
        other => panic!("expected a float, got {other:?}"),
    }
}

/// The bits of a `real`, as `CAST(x AS binary(4))` shows them.
fn bits32(v: Result<Value, SqlError>) -> String {
    match v {
        Ok(Value::F32(f)) => format!("{:08X}", f.to_bits()),
        other => panic!("expected a real, got {other:?}"),
    }
}

/// `CAST(v AS varchar(60))`, the text of an exact value.
fn as_text(v: Value, src: SqlType, style: Option<i32>) -> String {
    match convert(&v, &ti(src), &ti(SqlType::VarChar(Len::Fixed(60))), style) {
        Ok(Value::String(s)) => s.text,
        other => panic!("expected a string, got {other:?}"),
    }
}

/// `decimal` → `float` rounds correctly at the last bit: the value is `m / 10^s` taken
/// exactly, not `m as f64 / 10^s` with its two intermediate roundings.
///
/// `CONVERT(varchar(20), CAST(CAST(CAST(1999114.5855581982 AS numeric(38,10)) AS float)
/// AS binary(8)), 2)` is `413E810A95E7245F`; the naive arithmetic gives `…60`.
#[test]
fn numeric_to_float_rounds_correctly_at_the_last_bit() {
    let m = 19_991_145_855_581_982_i128;
    for precision in [17, 19, 38] {
        assert_eq!(
            bits64(cv(
                dec(m, precision, 10),
                num(precision, 10),
                SqlType::Float
            )),
            "413E810A95E7245F",
            "numeric({precision},10)"
        );
    }
    assert_eq!(
        bits64(cv(dec(-m, 38, 10), num(38, 10), SqlType::Float)),
        "C13E810A95E7245F"
    );
    // The same digits shifted by one place either way (keys `F;38;10;19991145.8555819820`
    // and `F;38;10;199911.4585558198`).
    assert_eq!(
        bits64(cv(dec(m * 10, 38, 10), num(38, 10), SqlType::Float)),
        "417310A69DB076BB"
    );
    assert_eq!(
        bits64(cv(dec(m / 10, 38, 10), num(38, 10), SqlType::Float)),
        "4108673BAB1F504B"
    );
    // A small mantissa at a scale beyond 22, where `10f64.powi(s)` is no longer exact
    // (key `F;25;25;0.0000000000000000000000001`).
    assert_eq!(
        bits64(cv(dec(1, 25, 25), num(25, 25), SqlType::Float)),
        "3ABEF2D0F5DA7DD9"
    );
}

/// One ULP on the `float` shifts a `datetime` by three milliseconds three conversions
/// later: `SELECT CAST(CAST(1999114.5855581982 AS float) AS datetime);` is
/// `7373-05-22 14:03:12.227`.
#[test]
fn numeric_to_float_to_datetime_end_to_end() {
    let float = cv(
        dec(19_991_145_855_581_982, 17, 10),
        num(17, 10),
        SqlType::Float,
    )
    .expect("numeric to float");
    let datetime = cv(float, SqlType::Float, SqlType::DateTime).expect("float to datetime");
    // 14:03:12 is tick 50 592 × 300; .227 is tick 68 (68 / 300 = 0.2266…, shown `.227`);
    // the old value gave tick 69, shown `.230`.
    assert_eq!(
        datetime,
        Value::DateTime(DateTime {
            days: 1_999_114,
            ticks_300th: 50_592 * 300 + 68,
        })
    );
    assert_eq!(
        as_text(datetime, SqlType::DateTime, Some(121)),
        "7373-05-22 14:03:12.227"
    );
}

/// A value exactly halfway between two doubles rounds to the even one, in both signs —
/// IEEE, not half away from zero: `9007199254740993` (2^53 + 1, whose lower neighbour is
/// even) and `9007199254740995` (lower neighbour odd) as `numeric(17,0)`.
#[test]
fn numeric_to_float_ties_round_to_even() {
    let two_53 = 1_i128 << 53;
    assert_eq!(
        cv(dec(two_53 + 1, 17, 0), num(17, 0), SqlType::Float),
        Ok(Value::F64(9_007_199_254_740_992.0))
    );
    assert_eq!(
        cv(dec(-(two_53 + 1), 17, 0), num(17, 0), SqlType::Float),
        Ok(Value::F64(-9_007_199_254_740_992.0))
    );
    assert_eq!(
        cv(dec(two_53 + 3, 17, 0), num(17, 0), SqlType::Float),
        Ok(Value::F64(9_007_199_254_740_996.0))
    );
    // The same at the `real` level: 2^24 + 1 sits between 2^24 (even) and 2^24 + 2.
    assert_eq!(
        cv(dec(16_777_217, 9, 0), num(9, 0), SqlType::Real),
        Ok(Value::F32(16_777_216.0))
    );
}

/// `decimal` → `real` rounds **once**, from the exact value, never through `float`:
/// `8388608.5000000001` is just above the midpoint between two reals, but the nearest
/// double *is* that midpoint (10^-10 is below half its ULP, 2^-30), and a second rounding
/// from there would land on the even neighbour 8388608. `8388608.5000000001` as a
/// `numeric(17,10)` is `4B000001`; rounding through `float` gives `4B000000`.
#[test]
fn numeric_to_real_rounds_once() {
    let m = 83_886_085_000_000_001_i128;
    assert_eq!(
        bits32(cv(dec(m, 17, 10), num(17, 10), SqlType::Real)),
        "4B000001"
    );
    assert_eq!(
        bits32(cv(dec(-m, 17, 10), num(17, 10), SqlType::Real)),
        "CB000001"
    );
    // And the value just below the midpoint rounds down.
    assert_eq!(
        bits32(cv(dec(m - 2, 17, 10), num(17, 10), SqlType::Real)),
        "4B000000"
    );
}

/// `float` → `decimal` rounds the **exact** binary value of the double, half away from
/// zero at the target scale: `0.1` is `0.1000000000000000055511151231257827021` in
/// `numeric(38,37)`, `1e23` is `99999999999999991611392`, and a dyadic tie such as
/// `0.125` goes away from zero.
#[test]
fn float_to_numeric_uses_the_exact_expansion() {
    let to_text = |v: f64, p: u8, s: u8| -> Result<String, SqlError> {
        cv(Value::F64(v), SqlType::Float, num(p, s)).map(|d| as_text(d, num(p, s), None))
    };
    assert_eq!(
        to_text(0.1, 38, 37).as_deref(),
        Ok("0.1000000000000000055511151231257827021")
    );
    assert_eq!(
        to_text(1e23, 38, 0).as_deref(),
        Ok("99999999999999991611392")
    );
    assert_eq!(to_text(0.125, 3, 2).as_deref(), Ok("0.13"));
    assert_eq!(to_text(-0.125, 3, 2).as_deref(), Ok("-0.13"));
    assert_eq!(to_text(2.5, 2, 0).as_deref(), Ok("3"));
    assert_eq!(to_text(-2.5, 2, 0).as_deref(), Ok("-3"));
    assert_eq!(
        to_text(0.3, 38, 30).as_deref(),
        Ok("0.299999999999999988897769753748")
    );
    assert_eq!(
        to_text(5e-324, 38, 38).as_deref(),
        Ok("0.00000000000000000000000000000000000000")
    );
    // 1e28 as a double is 9999999999999999583119736832: it fits numeric(38,10), 1e29
    // does not.
    assert_eq!(
        to_text(1e28, 38, 10).as_deref(),
        Ok("9999999999999999583119736832.0000000000")
    );
    let e = to_text(1e29, 38, 10).unwrap_err();
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting float to data type numeric overflowed."
    );
    // `real` follows the same rule on its own exact value: 0.1f is
    // 0.100000001490116119384765625 (key `E;38;37;1.000000000e-01`).
    assert_eq!(
        cv(Value::F32(0.1), SqlType::Real, num(38, 37)).map(|d| as_text(d, num(38, 37), None)),
        Ok("0.1000000014901161193847656250000000000".to_owned())
    );
}

/// `float` → `money` follows the same rule at scale 4: `0.03125` (a dyadic tie) is
/// `0.0313`, and the double nearest `5e-5` lies above the half so it is `0.0001`, while the
/// one below it is `0.0000`.
#[test]
fn float_to_money_uses_the_exact_expansion() {
    assert_eq!(
        cv(Value::F64(0.03125), SqlType::Float, SqlType::Money),
        Ok(Value::Money(313))
    );
    assert_eq!(
        cv(Value::F64(-0.03125), SqlType::Float, SqlType::SmallMoney),
        Ok(Value::Money(-313))
    );
    assert_eq!(
        cv(Value::F64(5e-5), SqlType::Float, SqlType::Money),
        Ok(Value::Money(1))
    );
    assert_eq!(
        cv(
            Value::F64(4.9999999999999996e-5),
            SqlType::Float,
            SqlType::Money
        ),
        Ok(Value::Money(0))
    );
}

/// A `binary` or `varbinary` source keeps the **low-order** bytes of its byte string and
/// reads them as the storage of the target. Each line is a
/// `SELECT CAST(CAST(<hex> AS binary(k)) AS <target>);`.
#[test]
fn binary_source_keeps_the_low_bytes() {
    let bin = |n: u16| SqlType::Binary(Len::Fixed(n));
    let bytes = |b: &[u8]| Value::Bytes(b.to_vec());

    assert_eq!(cv(bytes(&[0x0F]), bin(1), SqlType::Int), Ok(Value::I32(15)));
    // `tinyint` is unsigned, the wider integers are two's complement.
    assert_eq!(
        cv(bytes(&[0xFF]), bin(1), SqlType::TinyInt),
        Ok(Value::I8(255))
    );
    assert_eq!(
        cv(bytes(&[0xFF, 0xFF]), bin(2), SqlType::SmallInt),
        Ok(Value::I16(-1))
    );
    assert_eq!(
        cv(bytes(&[0xFF, 0xFF, 0xFF, 0xFF]), bin(4), SqlType::Int),
        Ok(Value::I32(-1))
    );
    // A source wider than the target keeps its tail, without an error.
    let eight = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    assert_eq!(
        cv(bytes(&eight), bin(8), SqlType::Int),
        Ok(Value::I32(84_281_096))
    );
    assert_eq!(
        cv(bytes(&eight), bin(8), SqlType::BigInt),
        Ok(Value::I64(72_623_859_790_382_856))
    );
    // An empty `varbinary` is zero, and a `bit` reads the low byte alone: `0x0100` is 0.
    assert_eq!(
        cv(bytes(&[]), SqlType::VarBinary(Len::Fixed(4)), SqlType::Int),
        Ok(Value::I32(0))
    );
    assert_eq!(
        cv(bytes(&[0x01, 0x00]), bin(2), SqlType::Bit),
        Ok(Value::Bit(false))
    );
    assert_eq!(
        cv(bytes(&[0x00, 0x01]), bin(2), SqlType::Bit),
        Ok(Value::Bit(true))
    );
    // `money` and `smallmoney` read the bytes as the amount in ten-thousandths.
    assert_eq!(
        cv(bytes(&[0x0F]), bin(1), SqlType::Money),
        Ok(Value::Money(15))
    );
    assert_eq!(
        cv(bytes(&[0x0F]), bin(1), SqlType::SmallMoney),
        Ok(Value::Money(15))
    );
    let sixteen: Vec<u8> = (1u8..=16).collect();
    assert_eq!(
        cv(bytes(&sixteen), bin(16), SqlType::Money),
        Ok(Value::Money(651_345_242_494_996_240))
    );
}

/// Message 8115 calls a `bit` source `tinyint`.
///
/// `SELECT CAST(CAST(1 AS bit) AS decimal(1,1));` raises 8115 naming `tinyint` and
/// `numeric`, and so does `SELECT COALESCE(CAST(1 AS bit), CAST(0.1 AS decimal(1,1)));`.
/// The `int` source of the same overflow keeps its own name, which is the counter-vector:
/// only `bit` changes.
#[test]
fn bit_overflow_names_tinyint() {
    let e = err(Value::Bit(true), SqlType::Bit, num(1, 1));
    assert_eq!(e.number, 8115);
    assert_eq!(
        e.message,
        "Converting tinyint to data type numeric overflowed."
    );

    let e = err(Value::I32(1), SqlType::Int, num(1, 1));
    assert_eq!(e.message, "Converting int to data type numeric overflowed.");
}
