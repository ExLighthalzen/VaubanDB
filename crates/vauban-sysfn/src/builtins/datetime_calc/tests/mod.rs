//! Unit tests for `builtins::datetime_calc`: `DATEADD`, `DATEDIFF`, `EOMONTH` and
//! `DATEFROMPARTS`, plus the shared calendar core (`Shape`, `Unit`, keyword parsing,
//! civil-day conversion) that backs the four.

use super::*;
use crate::builtins::check_call;
use crate::context::StaticContext;
use crate::{lookup, register_builtins};
use vauban_types::{Decimal, Len, SqlString};

fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

fn string_type() -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(40)), false)
}

fn int_type() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

/// Calls `name` on `values`/`types` through the registry, exactly as the executor does.
fn call(name: &str, values: &[Value], types: &[TypeInfo]) -> SqlResult<Value> {
    register_builtins();
    let def = lookup(name).expect("the function must be registered");
    let result = check_call(def, types)?;
    let args = EvalArgs {
        values,
        types,
        result: &result,
    };
    (def.eval)(&args, &StaticContext::default())
}

/// `CAST('<text>' AS <ty>)`, through `types`.
fn typed(text_value: &str, ty: SqlType) -> (Value, TypeInfo) {
    let info = TypeInfo::new(ty, false);
    let value = convert(&text(text_value), &string_type(), &info, None)
        .unwrap_or_else(|e| panic!("'{text_value}' must read as {:?}: {}", info.ty, e.message));
    (value, info)
}

/// `DATEADD(part, n, '<date>')`: the date is read as a `datetime`.
fn dateadd_str(part: &str, n: i32, date: &str) -> SqlResult<Value> {
    call(
        "DATEADD",
        &[text(part), Value::I32(n), text(date)],
        &[string_type(), int_type(), string_type()],
    )
}

/// `DATEADD(part, n, CAST('<date>' AS <ty>))`.
fn dateadd_typed(part: &str, n: i32, date: &str, ty: SqlType) -> SqlResult<Value> {
    let (value, info) = typed(date, ty);
    call(
        "DATEADD",
        &[text(part), Value::I32(n), value],
        &[string_type(), int_type(), info],
    )
}

/// `DATEDIFF(part, '<start>', '<end>')`.
fn datediff_str(part: &str, start: &str, end: &str) -> SqlResult<Value> {
    call(
        "DATEDIFF",
        &[text(part), text(start), text(end)],
        &[string_type(), string_type(), string_type()],
    )
}

/// `DATEDIFF(part, CAST('<start>' AS <ty>), CAST('<end>' AS <ty>))`.
fn datediff_typed(part: &str, start: &str, end: &str, ty: SqlType) -> SqlResult<Value> {
    let (a, info) = typed(start, ty);
    let (b, _) = typed(end, ty);
    call(
        "DATEDIFF",
        &[text(part), a, b],
        &[string_type(), info.clone(), info],
    )
}

/// The ISO rendering (style 121) of a date or time value under its declared type, the
/// way a client sees it: the scale is applied by `types`.
fn shown(value: &Value, ty: SqlType) -> String {
    match convert(
        value,
        &TypeInfo::new(ty, false),
        &TypeInfo::new(SqlType::VarChar(Len::Fixed(40)), false),
        Some(121),
    ) {
        Ok(Value::String(s)) => s.text,
        other => panic!("rendering answered {other:?}"),
    }
}

/// The rendering at the widest scale of the value's own family.
fn show(value: &Value) -> String {
    let ty = match value {
        Value::Date(_) => SqlType::Date,
        Value::Time(_) => SqlType::Time(7),
        Value::DateTime(_) => SqlType::DateTime,
        Value::DateTime2(_) => SqlType::DateTime2(7),
        Value::DateTimeOffset(_) => SqlType::DateTimeOffset(7),
        other => panic!("not a date: {other:?}"),
    };
    shown(value, ty)
}

/// `'YYYY-MM-DD'` as a `date` value, through the calendar of `types`.
fn date_of(text: &str) -> Value {
    Value::Date(Date {
        days: days_from_civil(
            text[0..4].parse().unwrap(),
            text[5..7].parse().unwrap(),
            text[8..10].parse().unwrap(),
        ),
    })
}

#[test]
fn calendar_bounds() {
    assert_eq!(days_from_civil(9999, 12, 31), MAX_DAYS);
    assert_eq!(days_from_civil(1753, 1, 1), DATETIME_MIN_DAYS);
    assert_eq!(days_from_civil(2079, 6, 6), SMALLDATETIME_MAX_DAYS);
    assert_eq!(days_from_civil(1900, 1, 1), DAYS_1900);
}

#[test]
fn dateadd_basic_parts() {
    for (part, n, date, expected) in [
        ("day", 1, "2020-02-28", "2020-02-29 00:00:00.000"),
        ("day", 1, "2019-02-28", "2019-03-01 00:00:00.000"),
        ("minute", 90, "2020-01-01 23:00", "2020-01-02 00:30:00.000"),
        ("year", -1, "2020-03-01", "2019-03-01 00:00:00.000"),
        // The three spellings of a day count, and the week.
        ("weekday", 7, "2020-01-01", "2020-01-08 00:00:00.000"),
        ("dayofyear", 366, "2020-01-01", "2021-01-01 00:00:00.000"),
        ("week", -1, "2020-01-01", "2019-12-25 00:00:00.000"),
        ("hour", -1, "2020-01-01 00:30", "2019-12-31 23:30:00.000"),
        (
            "second",
            1,
            "2019-12-31 23:59:59",
            "2020-01-01 00:00:00.000",
        ),
        ("day", 1, "1900-02-28", "1900-03-01 00:00:00.000"),
        ("day", 1, "2000-02-28", "2000-02-29 00:00:00.000"),
    ] {
        let value = dateadd_str(part, n, date).unwrap();
        assert!(
            matches!(value, Value::DateTime(_)),
            "a string is a datetime"
        );
        assert_eq!(show(&value), expected, "DATEADD({part}, {n}, '{date}')");
    }
}

#[test]
fn dateadd_clamps_end_of_month() {
    for (part, n, date, expected) in [
        ("month", 1, "2020-01-31", "2020-02-29"),
        ("month", 1, "2021-01-31", "2021-02-28"),
        ("year", 1, "2020-02-29", "2021-02-28"),
        ("quarter", 1, "2020-11-30", "2021-02-28"),
        ("month", -1, "2020-03-31", "2020-02-29"),
        ("month", 13, "2019-01-31", "2020-02-29"),
        ("month", -13, "2021-03-31", "2020-02-29"),
        ("quarter", -1, "2020-05-31", "2020-02-29"),
        ("year", 4, "2020-02-29", "2024-02-29"),
        ("year", 100, "2000-02-29", "2100-02-28"),
    ] {
        let value = dateadd_typed(part, n, date, SqlType::Date).unwrap();
        assert_eq!(value, date_of(expected), "DATEADD({part}, {n}, '{date}')");
    }
    // The clamp moves the day and nothing else.
    let value = dateadd_typed(
        "month",
        1,
        "2020-01-31 13:45:30.1234567",
        SqlType::DateTime2(7),
    )
    .unwrap();
    assert_eq!(show(&value), "2020-02-29 13:45:30.1234567");
}

#[test]
fn dateadd_overflow_is_517() {
    let err = dateadd_str("year", 1000, "9999-01-01").unwrap_err();
    assert_eq!(err.number, 517);
    assert_eq!(
        err.message,
        "The addition overflowed the 'datetime' column."
    );
    let err = dateadd_typed("day", -1, "0001-01-01", SqlType::Date).unwrap_err();
    assert_eq!(err.number, 517);
    assert_eq!(err.message, "The addition overflowed the 'date' column.");
    // The bounds themselves are fine.
    assert_eq!(
        dateadd_typed("day", 0, "0001-01-01", SqlType::Date).unwrap(),
        date_of("0001-01-01")
    );
    assert_eq!(
        dateadd_typed("day", 1, "9999-12-30", SqlType::Date).unwrap(),
        date_of("9999-12-31")
    );
    // A `datetime` starts in 1753, a `smalldatetime` ends on 2079-06-06 23:59.
    let low = dateadd_typed("day", -1, "1753-01-01", SqlType::DateTime).unwrap_err();
    assert_eq!(low.number, 517);
    assert_eq!(
        show(&dateadd_typed("day", 0, "1753-01-01", SqlType::DateTime).unwrap()),
        "1753-01-01 00:00:00.000"
    );
    let err = dateadd_typed("minute", 1, "2079-06-06 23:59", SqlType::SmallDateTime).unwrap_err();
    assert_eq!(err.number, 517);
    assert_eq!(
        err.message,
        "The addition overflowed the 'smalldatetime' column."
    );
    // The rounding to the minute comes before the range check: +30 s overflows, +29 s
    // does not; and the last tick of a `datetime` takes 1 ms but not 2.
    let sdt = SqlType::SmallDateTime;
    assert_eq!(
        dateadd_typed("second", 30, "2079-06-06 23:59", sdt)
            .unwrap_err()
            .number,
        517
    );
    assert_eq!(
        show(&dateadd_typed("second", 29, "2079-06-06 23:59", sdt).unwrap()),
        "2079-06-06 23:59:00.000"
    );
    let top = "9999-12-31 23:59:59.997";
    assert_eq!(
        show(&dateadd_typed("millisecond", 1, top, SqlType::DateTime).unwrap()),
        "9999-12-31 23:59:59.997"
    );
    assert_eq!(
        dateadd_typed("millisecond", 2, top, SqlType::DateTime)
            .unwrap_err()
            .number,
        517
    );
    // A huge number overflows the calendar, never the arithmetic.
    for (part, n) in [
        ("day", i32::MAX),
        ("day", i32::MIN),
        ("year", i32::MAX),
        ("month", i32::MIN),
    ] {
        let err = dateadd_typed(part, n, "2020-01-01", SqlType::Date).unwrap_err();
        assert_eq!(err.number, 517, "DATEADD({part}, {n}, date)");
    }
    for (part, n) in [("hour", i32::MAX), ("hour", i32::MIN), ("second", i32::MAX)] {
        let err = dateadd_typed(part, n, "9999-12-31", SqlType::DateTime2(7)).unwrap_err();
        assert_eq!(err.number, 517, "DATEADD({part}, {n}, datetime2)");
    }
}

#[test]
fn dateadd_time_part_on_date_is_9810() {
    let err = dateadd_typed("hour", 1, "2020-01-01", SqlType::Date).unwrap_err();
    assert_eq!(err.number, 9810);
    assert_eq!(
        err.message,
        "Datepart hour cannot be used with the date function dateadd on data type date."
    );
    // The rest of the refusal matrix: a `time` refuses the date parts, a
    // `datetime` anything finer than the millisecond, and `iso_week` and `tzoffset`
    // are refused everywhere; a string is a `datetime` and says so.
    let time = "12:00";
    for (part, n, date, ty, expected) in [
        ("day", 1, time, SqlType::Time(7), "day"),
        (
            "microsecond",
            1,
            "2020-01-01",
            SqlType::VarChar(Len::Fixed(40)),
            "microsecond",
        ),
        (
            "nanosecond",
            1,
            "2020-01-01",
            SqlType::SmallDateTime,
            "nanosecond",
        ),
        (
            "iso_week",
            1,
            "2020-01-01",
            SqlType::DateTime2(3),
            "iso_week",
        ),
        (
            "tzoffset",
            1,
            "2020-01-01 00:00 +02:00",
            SqlType::DateTimeOffset(0),
            "tzoffset",
        ),
    ] {
        let err = dateadd_typed(part, n, date, ty).unwrap_err();
        let type_name = match ty {
            SqlType::VarChar(_) => "datetime",
            other => other.error_name(),
        };
        assert_eq!(
            err.message,
            format!(
                "Datepart {expected} cannot be used with the date function dateadd on data type {type_name}."
            )
        );
    }
    // What is accepted: every date part on a `date`, every time part on a `time`, the
    // millisecond on a `datetime`.
    for part in [
        "year",
        "quarter",
        "month",
        "dayofyear",
        "day",
        "week",
        "weekday",
    ] {
        assert!(
            dateadd_typed(part, 1, "2020-01-01", SqlType::Date).is_ok(),
            "{part}"
        );
        assert!(
            dateadd_typed(part, 1, time, SqlType::Time(7)).is_err(),
            "{part}"
        );
    }
    for part in [
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
    ] {
        assert!(
            dateadd_typed(part, 1, time, SqlType::Time(7)).is_ok(),
            "{part}"
        );
        assert!(
            dateadd_typed(part, 1, "2020-01-01", SqlType::Date).is_err(),
            "{part}"
        );
        assert!(
            dateadd_typed(part, 1, "2020-01-01", SqlType::DateTime2(7)).is_ok(),
            "{part}"
        );
    }
    assert!(dateadd_typed("millisecond", 1, "2020-01-01", SqlType::DateTime).is_ok());
    // `NULL` wins over the datepart check.
    assert_eq!(
        call(
            "DATEADD",
            &[text("hour"), Value::I32(1), Value::Null],
            &[
                string_type(),
                int_type(),
                TypeInfo::new(SqlType::Date, true)
            ],
        )
        .unwrap(),
        Value::Null
    );
}

#[test]
fn dateadd_keeps_the_argument_type() {
    register_builtins();
    let def = lookup("DATEADD").expect("registered");
    for ty in [
        SqlType::Date,
        SqlType::DateTime,
        SqlType::DateTime2(3),
        SqlType::Time(3),
        SqlType::SmallDateTime,
        SqlType::DateTimeOffset(2),
    ] {
        let info =
            (def.return_type)(&[string_type(), int_type(), TypeInfo::new(ty, false)]).unwrap();
        assert_eq!(info.ty, ty);
        // Nullable even from a literal.
        assert!(info.nullable, "{ty:?}");
    }
    // A string or a number is a `datetime`.
    for ty in [
        SqlType::VarChar(Len::Fixed(30)),
        SqlType::NVarChar(Len::Max),
        SqlType::Int,
        SqlType::Float,
    ] {
        let info =
            (def.return_type)(&[string_type(), int_type(), TypeInfo::new(ty, false)]).unwrap();
        assert_eq!(info.ty, SqlType::DateTime);
    }
    // The values follow, scale included.
    assert_eq!(
        shown(
            &dateadd_typed("minute", 1, "23:59:59.999", SqlType::Time(3)).unwrap(),
            SqlType::Time(3)
        ),
        "00:00:59.999"
    );
    assert_eq!(
        shown(
            &dateadd_typed(
                "second",
                1,
                "2020-01-31 23:59:59.99 -08:00",
                SqlType::DateTimeOffset(2)
            )
            .unwrap(),
            SqlType::DateTimeOffset(2)
        ),
        "2020-02-01 00:00:00.99 -08:00"
    );
    // 206 for the one type with no date reading, 8116 for a number of the wrong family.
    let guid = TypeInfo::new(SqlType::UniqueIdentifier, false);
    let err = (def.return_type)(&[string_type(), int_type(), guid.clone()]).unwrap_err();
    assert_eq!(err.number, 206);
    assert_eq!(
        err.message,
        "Type mismatch: uniqueidentifier cannot be combined with datetime."
    );
    let date = TypeInfo::new(SqlType::Date, false);
    for ty in [
        SqlType::VarChar(Len::Fixed(1)),
        SqlType::Bit,
        SqlType::Binary(Len::Fixed(1)),
        SqlType::Date,
        SqlType::UniqueIdentifier,
    ] {
        let err = (def.return_type)(&[string_type(), TypeInfo::new(ty, false), date.clone()])
            .unwrap_err();
        assert_eq!(err.number, 8116);
    }
    let err = (def.return_type)(&[
        string_type(),
        TypeInfo::new(SqlType::VarChar(Len::Fixed(1)), false),
        date,
    ])
    .unwrap_err();
    assert_eq!(
        err.message,
        "Data type varchar is not accepted for argument 2 of the dateadd function."
    );
}

#[test]
fn dateadd_number_conversions() {
    let date = TypeInfo::new(SqlType::Date, false);
    let add = |number: Value, ty: SqlType| {
        call(
            "DATEADD",
            &[text("day"), number, date_of("2020-01-31")],
            &[string_type(), TypeInfo::new(ty, false), date.clone()],
        )
    };
    assert_eq!(
        add(Value::I64(1), SqlType::BigInt).unwrap(),
        date_of("2020-02-01")
    );
    // `decimal` and `float` truncate, `money` rounds: 1.5 is two days.
    let decimal = |mantissa: i128| {
        Value::Decimal(Decimal {
            mantissa,
            precision: 5,
            scale: 1,
        })
    };
    let decimal_type = SqlType::Decimal {
        precision: 5,
        scale: 1,
    };
    assert_eq!(
        add(decimal(19), decimal_type).unwrap(),
        date_of("2020-02-01")
    );
    assert_eq!(
        add(decimal(-19), decimal_type).unwrap(),
        date_of("2020-01-30")
    );
    assert_eq!(
        add(Value::F64(1.9), SqlType::Float).unwrap(),
        date_of("2020-02-01")
    );
    assert_eq!(
        add(Value::Money(15_000), SqlType::Money).unwrap(),
        date_of("2020-02-02")
    );
    // A `bigint` past the `int` is an overflow of the conversion, 8115; what is
    // checked here is that the conversion is consulted and its error propagated.
    let err = add(Value::I64(2_147_483_648), SqlType::BigInt).unwrap_err();
    assert!(matches!(err.number, 8115 | 220), "{}", err.message);
    assert_eq!(
        add(Value::F64(1e10), SqlType::Float).unwrap_err().number,
        232
    );
    // A `NULL` number is `NULL`.
    assert_eq!(add(Value::Null, SqlType::Int).unwrap(), Value::Null);
}

#[test]
fn dateadd_datetime_rounds_milliseconds_to_ticks() {
    // From tick 1 (.003): the milliseconds become ticks first, half away from zero.
    for (n, expected) in [
        (1, "00:00:00.003"),
        (5, "00:00:00.010"),
        (-5, "23:59:59.997"),
        (7, "00:00:00.010"),
        (9, "00:00:00.013"),
        (-2, "00:00:00.000"),
    ] {
        let value = dateadd_typed(
            "millisecond",
            n,
            "2020-01-01 00:00:00.003",
            SqlType::DateTime,
        )
        .unwrap();
        assert!(
            show(&value).ends_with(expected),
            "{n} ms from .003: {}",
            show(&value)
        );
    }
    for (n, expected) in [
        (1, ".000"),
        (2, ".003"),
        (5, ".007"),
        (999, "01.000"),
        (-1, ".000"),
        (-2, "59.997"),
    ] {
        let value = dateadd_typed(
            "millisecond",
            n,
            "2020-01-01 00:00:00.000",
            SqlType::DateTime,
        )
        .unwrap();
        assert!(
            show(&value).ends_with(expected),
            "{n} ms from .000: {}",
            show(&value)
        );
    }
    // Whole seconds keep the tick.
    assert_eq!(
        show(&dateadd_typed("second", -1, "2020-01-01 00:00:00.003", SqlType::DateTime).unwrap()),
        "2019-12-31 23:59:59.003"
    );
    // `smalldatetime`: 30 s up, 29 998 ms down, 29 999 ms up (it became 30.000 in ticks).
    for (part, n, expected) in [
        ("second", 29, "00:00:00"),
        ("second", 30, "00:01:00"),
        ("millisecond", 29_998, "00:00:00"),
        ("millisecond", 29_999, "00:01:00"),
        ("second", -30, "00:00:00"),
        ("second", -31, "23:59:00"),
    ] {
        let value = dateadd_typed(part, n, "2020-01-01 00:00", SqlType::SmallDateTime).unwrap();
        let text = show(&value);
        assert!(
            text.ends_with(&format!("{expected}.000")),
            "{part} {n}: {text}"
        );
        assert!(matches!(value, Value::DateTime(_)));
    }
}

#[test]
fn dateadd_datetime2_rounds_to_scale() {
    for (part, n, date, scale, expected) in [
        (
            "nanosecond",
            499_999,
            "2020-01-01 00:00:00.000",
            3,
            "2020-01-01 00:00:00.001",
        ),
        (
            "microsecond",
            499,
            "2020-01-01 00:00:00.000",
            3,
            "2020-01-01 00:00:00.000",
        ),
        (
            "microsecond",
            500,
            "2020-01-01 00:00:00.000",
            3,
            "2020-01-01 00:00:00.001",
        ),
        (
            "nanosecond",
            49,
            "2020-01-01 00:00:00",
            7,
            "2020-01-01 00:00:00.0000000",
        ),
        (
            "nanosecond",
            50,
            "2020-01-01 00:00:00",
            7,
            "2020-01-01 00:00:00.0000001",
        ),
        (
            "nanosecond",
            -50,
            "2020-01-01 00:00:00",
            7,
            "2019-12-31 23:59:59.9999999",
        ),
        (
            "nanosecond",
            -150,
            "2020-01-01 00:00:00",
            7,
            "2019-12-31 23:59:59.9999998",
        ),
        (
            "microsecond",
            -500,
            "2020-01-01 00:00:00",
            3,
            "2020-01-01 00:00:00.000",
        ),
        (
            "microsecond",
            -501,
            "2020-01-01 00:00:00",
            3,
            "2019-12-31 23:59:59.999",
        ),
        (
            "millisecond",
            499,
            "2020-01-01 00:00:00",
            0,
            "2020-01-01 00:00:00",
        ),
        (
            "millisecond",
            500,
            "2020-01-01 00:00:00",
            0,
            "2020-01-01 00:00:01",
        ),
        (
            "microsecond",
            999,
            "2020-01-01 23:59:59.999",
            3,
            "2020-01-02 00:00:00.000",
        ),
        // On the last day the exact result decides, and the rounding then clamps.
        (
            "nanosecond",
            999_900,
            "9999-12-31 23:59:59.999",
            3,
            "9999-12-31 23:59:59.999",
        ),
        (
            "millisecond",
            999,
            "9999-12-31 23:59:59",
            0,
            "9999-12-31 23:59:59",
        ),
    ] {
        let value = dateadd_typed(part, n, date, SqlType::DateTime2(scale)).unwrap();
        assert_eq!(
            shown(&value, SqlType::DateTime2(scale)),
            expected,
            "DATEADD({part}, {n}, '{date}' as datetime2({scale}))"
        );
    }
    for (part, n, date, scale) in [
        ("nanosecond", 999_999, "9999-12-31 23:59:59.999", 3),
        ("nanosecond", 50, "9999-12-31 23:59:59.9999999", 7),
        ("nanosecond", -50, "0001-01-01 00:00:00", 7),
        ("microsecond", -1, "0001-01-01 00:00:00", 3),
    ] {
        let err = dateadd_typed(part, n, date, SqlType::DateTime2(scale)).unwrap_err();
        assert_eq!(err.number, 517, "DATEADD({part}, {n}, '{date}')");
        assert_eq!(
            err.message,
            "The addition overflowed the 'datetime2' column."
        );
    }
}

#[test]
fn dateadd_time_wraps_around_midnight() {
    for (part, n, time, scale, expected) in [
        ("hour", 25, "23:00", 0, "00:00:00"),
        ("hour", -1, "00:30", 0, "23:30:00"),
        ("second", 86_400 * 3, "12:00", 0, "12:00:00"),
        ("hour", i32::MAX, "12:00", 0, "19:00:00"),
        ("hour", i32::MIN, "12:00", 0, "04:00:00"),
        ("microsecond", 999, "23:59:59.999", 3, "00:00:00.000"),
        ("nanosecond", -50, "00:00:00", 7, "23:59:59.9999999"),
        ("nanosecond", 1, "23:59:59.9999999", 7, "23:59:59.9999999"),
    ] {
        let value = dateadd_typed(part, n, time, SqlType::Time(scale)).unwrap();
        assert_eq!(
            shown(&value, SqlType::Time(scale)),
            expected,
            "DATEADD({part}, {n}, '{time}')"
        );
    }
}

#[test]
fn dateadd_datetimeoffset_is_local_and_keeps_the_offset() {
    let dto = SqlType::DateTimeOffset(0);
    assert_eq!(
        shown(
            &dateadd_typed("month", 1, "2020-01-31 20:00:00 -05:00", dto).unwrap(),
            dto
        ),
        "2020-02-29 20:00:00 -05:00"
    );
    assert_eq!(
        shown(
            &dateadd_typed("hour", 1, "2020-01-31 23:00:00 -08:00", dto).unwrap(),
            dto
        ),
        "2020-02-01 00:00:00 -08:00"
    );
    // The local result is on the calendar, its universal instant is not.
    let err = dateadd_typed("hour", 2, "9999-12-31 18:00:00 -05:00", dto).unwrap_err();
    assert_eq!(err.number, 517);
    assert_eq!(
        err.message,
        "The addition overflowed the 'datetimeoffset' column."
    );
    // The local result is not, its universal instant is.
    assert_eq!(
        dateadd_typed("hour", 1, "9999-12-31 23:00:00 +05:00", dto)
            .unwrap_err()
            .number,
        517
    );
    assert_eq!(
        shown(
            &dateadd_typed("hour", 1, "9999-12-31 22:00:00 +05:00", dto).unwrap(),
            dto
        ),
        "9999-12-31 23:00:00 +05:00"
    );
    // A string is a `datetime` for DATEADD, on its 1/300 s: the plain string below
    // is read, its +05:30 neighbour is refused with 241.
    let value = dateadd_str("day", 1, "2020-01-31 23:59:59.999").unwrap();
    assert!(matches!(value, Value::DateTime(_)));
    assert_eq!(show(&value), "2020-02-02 00:00:00.000");
    let error = dateadd_str("day", 1, "2020-01-31 23:59:59.999 +05:30").unwrap_err();
    assert_eq!((error.number, error.severity, error.state), (241, 16, 1));
}

#[test]
fn datediff_counts_boundaries() {
    register_builtins();
    for (part, start, end, expected) in [
        ("year", "2019-12-31", "2020-01-01", 1),
        ("day", "2020-01-01 23:59", "2020-01-02 00:01", 1),
        ("hour", "2020-01-01 00:59", "2020-01-01 01:00", 1),
        ("month", "2020-01-31", "2020-02-01", 1),
        ("day", "2020-01-02", "2020-01-01", -1),
        (
            "second",
            "2020-01-01 00:00:00",
            "2020-01-01 00:00:00.999",
            0,
        ),
        ("year", "2020-01-01", "2020-12-31", 0),
        ("quarter", "2020-03-31", "2020-04-01", 1),
        ("quarter", "2020-01-01", "2020-12-31", 3),
        ("minute", "2020-01-01 00:00:59", "2020-01-01 00:01:00", 1),
        ("weekday", "2020-01-01", "2020-12-31", 365),
        ("dayofyear", "2020-01-01", "2021-12-31", 730),
        ("month", "2020-02-01", "2020-01-31", -1),
        ("hour", "2020-01-01 01:00", "2020-01-01 00:59", -1),
        // A string keeps its seven digits.
        (
            "nanosecond",
            "2020-01-01 00:00:00.0000000",
            "2020-01-01 00:00:00.0000001",
            100,
        ),
        (
            "nanosecond",
            "2020-01-01 00:00:00",
            "2020-01-01 00:00:00.001",
            1_000_000,
        ),
        ("day", "1752-12-31", "2020-01-01", 97_520),
        // A string carries its offset, applied.
        (
            "hour",
            "2020-01-01 00:00:00 +00:00",
            "2020-01-01 00:00:00 +05:00",
            -5,
        ),
        ("month", "0001-01-01", "9999-12-31", 119_987),
        ("year", "0001-01-01", "9999-12-31", 9_998),
        ("week", "0001-01-01", "9999-12-31", 521_722),
    ] {
        assert_eq!(
            datediff_str(part, start, end).unwrap(),
            Value::I32(expected),
            "DATEDIFF({part}, '{start}', '{end}')"
        );
    }
    // The weeks start on Sunday; `SET DATEFIRST` is not read (the rule is in the
    // boundaries: Saturday to Sunday is one, Sunday to Monday none).
    for (start, end, expected) in [
        ("2020-02-29", "2020-03-01", 1),
        ("2020-03-01", "2020-03-02", 0),
        ("2020-03-01", "2020-03-07", 0),
        ("2020-03-07", "2020-03-08", 1),
        ("2020-12-31", "2021-01-01", 0),
        ("0001-01-01", "0001-01-07", 1),
        ("1899-12-31", "1900-01-01", 0),
        ("2020-03-08", "2020-03-01", -1),
    ] {
        assert_eq!(
            datediff_str("week", start, end).unwrap(),
            Value::I32(expected),
            "week {start} {end}"
        );
    }
    // Result type: `int`, nullable, and `NULL` in is `NULL` out — even on `iso_week`.
    let def = lookup("DATEDIFF").expect("registered");
    let info = (def.return_type)(&[string_type(), string_type(), string_type()]).unwrap();
    assert_eq!(info.ty, SqlType::Int);
    assert!(info.nullable);
    assert_eq!(
        call(
            "DATEDIFF",
            &[text("iso_week"), Value::Null, text("2020-01-01")],
            &[
                string_type(),
                TypeInfo::new(SqlType::Date, true),
                string_type()
            ]
        )
        .unwrap(),
        Value::Null
    );
    let err = datediff_str("iso_week", "2020-01-01", "2020-01-08").unwrap_err();
    assert_eq!((err.number, err.severity, err.state), (9806, 16, 0));
    assert_eq!(
        err.message,
        "Datepart iso_week cannot be used with the date function datediff."
    );
}

#[test]
fn datediff_types_share_one_line() {
    register_builtins();
    let (dt, dt_info) = typed("2020-03-01 12:00", SqlType::DateTime);
    assert_eq!(
        call(
            "DATEDIFF",
            &[text("hour"), date_of("2020-01-31"), dt],
            &[string_type(), TypeInfo::new(SqlType::Date, false), dt_info]
        )
        .unwrap(),
        Value::I32(732)
    );
    assert_eq!(
        datediff_typed("hour", "01:00", "23:00", SqlType::Time(7)).unwrap(),
        Value::I32(22)
    );
    assert_eq!(
        datediff_typed("hour", "23:00", "01:00", SqlType::Time(7)).unwrap(),
        Value::I32(-22)
    );
    // A `time` sits on 1900-01-01, so against a `date` it is 43 889 days away.
    let (time, time_info) = typed("12:00", SqlType::Time(7));
    assert_eq!(
        call(
            "DATEDIFF",
            &[text("day"), time, date_of("2020-03-01")],
            &[
                string_type(),
                time_info,
                TypeInfo::new(SqlType::Date, false)
            ]
        )
        .unwrap(),
        Value::I32(43_889)
    );
    assert_eq!(
        datediff_typed(
            "second",
            "2020-01-01 00:00",
            "2020-01-01 00:01",
            SqlType::SmallDateTime
        )
        .unwrap(),
        Value::I32(60)
    );
    // A number is a `datetime`: 0.5 is noon on 1900-01-01.
    assert_eq!(
        call(
            "DATEDIFF",
            &[text("hour"), Value::I32(0), Value::F64(0.5)],
            &[
                string_type(),
                int_type(),
                TypeInfo::new(SqlType::Float, false)
            ]
        )
        .unwrap(),
        Value::I32(12)
    );
    // The 1/300 s of a `datetime` are exact: tick 1 is 3 333 333 ns, and .997 to 1.000
    // crosses four milliseconds.
    let dt = SqlType::DateTime;
    let zero = "2020-01-01 00:00:00.000";
    assert_eq!(
        datediff_typed("nanosecond", zero, "2020-01-01 00:00:00.003", dt).unwrap(),
        Value::I32(3_333_333)
    );
    assert_eq!(
        datediff_typed("microsecond", zero, "2020-01-01 00:00:00.003", dt).unwrap(),
        Value::I32(3_333)
    );
    assert_eq!(
        datediff_typed(
            "millisecond",
            "2020-01-01 00:00:00.997",
            "2020-01-01 00:00:01.000",
            dt
        )
        .unwrap(),
        Value::I32(4)
    );
    // `datetimeoffset` on the universal instant, and mixed with a `datetime`.
    let dto = SqlType::DateTimeOffset(0);
    assert_eq!(
        datediff_typed(
            "hour",
            "2020-01-01 00:00:00 +00:00",
            "2020-01-01 00:00:00 +05:00",
            dto
        )
        .unwrap(),
        Value::I32(-5)
    );
    assert_eq!(
        datediff_typed(
            "day",
            "2020-01-01 23:00:00 +00:00",
            "2020-01-01 23:00:00 -05:00",
            dto
        )
        .unwrap(),
        Value::I32(1)
    );
    assert_eq!(
        datediff_typed(
            "year",
            "2019-12-31 23:00:00 -05:00",
            "2019-12-31 18:00:00 -05:00",
            dto
        )
        .unwrap(),
        Value::I32(-1)
    );
    assert_eq!(
        datediff_typed(
            "month",
            "2020-01-31 23:59:59.9999999 +05:30",
            "2020-03-01 00:00:00.0000001 +05:30",
            SqlType::DateTimeOffset(7)
        )
        .unwrap(),
        Value::I32(1)
    );
    let (dto, dto_info) = typed("2020-01-31 23:00:00 -05:00", SqlType::DateTimeOffset(0));
    let (dt, dt_info) = typed("2020-02-01 00:00:00", SqlType::DateTime);
    assert_eq!(
        call(
            "DATEDIFF",
            &[text("hour"), dt, dto],
            &[string_type(), dt_info, dto_info]
        )
        .unwrap(),
        Value::I32(4)
    );
    // 206 for the type with no date reading.
    let def = lookup("DATEDIFF").expect("registered");
    let err = (def.return_type)(&[
        string_type(),
        TypeInfo::new(SqlType::UniqueIdentifier, false),
        string_type(),
    ])
    .unwrap_err();
    assert_eq!(err.number, 206);
}

#[test]
fn datediff_overflow_is_535() {
    let err = datediff_str("nanosecond", "1900-01-01", "2000-01-01").unwrap_err();
    assert_eq!(err.number, 535);
    assert_eq!(
        err.message,
        "datediff overflowed: too many dateparts separate the two instants. Call datediff with a coarser datepart."
    );
    assert_eq!(
        datediff_str("second", "0001-01-01", "9999-12-31")
            .unwrap_err()
            .number,
        535
    );
    assert_eq!(
        datediff_str("minute", "0001-01-01", "9999-12-31")
            .unwrap_err()
            .number,
        535
    );
    // Days and hours over the whole calendar fit.
    let whole = days_from_civil(9999, 12, 31) - days_from_civil(1, 1, 1);
    assert_eq!(
        datediff_str("day", "0001-01-01", "9999-12-31").unwrap(),
        Value::I32(whole)
    );
    assert_eq!(
        datediff_str("day", "9999-12-31", "0001-01-01").unwrap(),
        Value::I32(-whole)
    );
    assert_eq!(
        datediff_str("hour", "0001-01-01", "9999-12-31").unwrap(),
        Value::I32(whole * 24)
    );
    // The thresholds: the largest count of each fine part, and one more.
    let dt2 = SqlType::DateTime2(7);
    let zero = "2020-01-01 00:00:00.0000000";
    assert_eq!(
        datediff_typed("nanosecond", zero, "2020-01-01 00:00:02.1474836", dt2).unwrap(),
        Value::I32(2_147_483_600)
    );
    assert_eq!(
        datediff_typed("nanosecond", zero, "2020-01-01 00:00:02.1474837", dt2)
            .unwrap_err()
            .number,
        535
    );
    assert_eq!(
        datediff_typed("nanosecond", "2020-01-01 00:00:02.1474836", zero, dt2).unwrap(),
        Value::I32(-2_147_483_600)
    );
    assert_eq!(
        datediff_typed("nanosecond", "2020-01-01 00:00:02.1474837", zero, dt2)
            .unwrap_err()
            .number,
        535
    );
    assert_eq!(
        datediff_typed("microsecond", zero, "2020-01-01 00:35:47.4836470", dt2).unwrap(),
        Value::I32(i32::MAX)
    );
    assert_eq!(
        datediff_typed("microsecond", zero, "2020-01-01 00:35:47.4836480", dt2)
            .unwrap_err()
            .number,
        535
    );
    assert_eq!(
        datediff_typed("second", "2000-01-01 00:00:00", "2068-01-19 03:14:07", dt2).unwrap(),
        Value::I32(i32::MAX)
    );
    assert_eq!(
        datediff_typed("second", "2000-01-01 00:00:00", "2068-01-19 03:14:08", dt2)
            .unwrap_err()
            .number,
        535
    );
}

#[test]
fn datediff_no_intermediate_overflow() {
    // The two extreme instants, in nanoseconds: 3.2 × 10²⁰, past an `i64`. Under
    // `cargo test` an overflowing integer panics, so reaching the 535 is the proof.
    let dt2 = SqlType::DateTime2(7);
    let first = "0001-01-01 00:00:00.0000000";
    let last = "9999-12-31 23:59:59.9999999";
    for part in [
        "nanosecond",
        "microsecond",
        "millisecond",
        "second",
        "minute",
    ] {
        let err = datediff_typed(part, first, last, dt2).unwrap_err();
        assert_eq!(err.number, 535, "{part}");
        let err = datediff_typed(part, last, first, dt2).unwrap_err();
        assert_eq!(err.number, 535, "{part}");
    }
    let far = Instant {
        days: MAX_DAYS,
        nanos: NANOS_PER_DAY as i64 - 1,
    };
    assert!(far.total_nanos() > i128::from(i64::MAX));
    // And the arithmetic of DATEADD with an extreme number on an extreme date.
    for (part, n) in [
        ("hour", i32::MAX),
        ("hour", i32::MIN),
        ("nanosecond", i32::MAX),
        ("week", i32::MIN),
    ] {
        assert!(dateadd_typed(part, n, last, dt2).is_err(), "{part}");
    }
}

#[test]
fn eomonth_basics() {
    register_builtins();
    let eomonth = |date: &str| call("EOMONTH", &[text(date)], &[string_type()]);
    let eomonth_offset = |date: &str, n: Value, ty: SqlType| {
        call(
            "EOMONTH",
            &[text(date), n],
            &[string_type(), TypeInfo::new(ty, false)],
        )
    };
    assert_eq!(eomonth("2020-02-05").unwrap(), date_of("2020-02-29"));
    assert_eq!(eomonth("2021-02-05").unwrap(), date_of("2021-02-28"));
    assert_eq!(
        eomonth_offset("2020-01-31", Value::I32(1), SqlType::Int).unwrap(),
        date_of("2020-02-29")
    );
    assert_eq!(
        eomonth_offset("2020-03-31", Value::I32(-1), SqlType::Int).unwrap(),
        date_of("2020-02-29")
    );
    assert_eq!(
        call(
            "EOMONTH",
            &[Value::Null],
            &[TypeInfo::new(SqlType::Date, true)]
        )
        .unwrap(),
        Value::Null
    );
    // A bare `NULL` reaches here as a nullable `int` (binder convention): `NULL` out.
    // The same type carrying a value is the 8116 of `EOMONTH(0)`, one step late.
    let nullable_int = TypeInfo::new(SqlType::Int, true);
    assert_eq!(
        call(
            "EOMONTH",
            &[Value::Null],
            std::slice::from_ref(&nullable_int)
        )
        .unwrap(),
        Value::Null
    );
    let err = call("EOMONTH", &[Value::I32(0)], &[nullable_int]).unwrap_err();
    assert_eq!(err.number, 8116);
    assert_eq!(
        err.message,
        "Data type int is not accepted for argument 1 of the eomonth function."
    );
    assert_eq!(
        eomonth_offset("2020-01-31", Value::I32(13), SqlType::Int).unwrap(),
        date_of("2021-02-28")
    );
    assert_eq!(
        eomonth_offset("2020-01-15", Value::I32(-13), SqlType::Int).unwrap(),
        date_of("2018-12-31")
    );
    // A `NULL` offset is zero, a `NULL` date is `NULL`.
    assert_eq!(
        eomonth_offset("2020-02-05", Value::Null, SqlType::Int).unwrap(),
        date_of("2020-02-29")
    );
    assert_eq!(
        call(
            "EOMONTH",
            &[Value::Null, Value::I32(1)],
            &[TypeInfo::new(SqlType::Date, true), int_type()]
        )
        .unwrap(),
        Value::Null
    );
    // `money` rounds the offset, a string spelling a number is read.
    assert_eq!(
        eomonth_offset("2020-02-05", Value::Money(19_000), SqlType::Money).unwrap(),
        date_of("2020-04-30")
    );
    let varchar = SqlType::VarChar(Len::Fixed(1));
    assert_eq!(
        eomonth_offset("2020-02-05", text("1"), varchar).unwrap(),
        date_of("2020-03-31")
    );
    assert_eq!(
        eomonth_offset("2020-02-05", text("x"), varchar)
            .unwrap_err()
            .number,
        245
    );
    // The same `bigint` overflow as DATEADD's: 8115 on SQL Server, 220 from `types`.
    let err = eomonth_offset("2020-02-05", Value::I64(2_147_483_648), SqlType::BigInt).unwrap_err();
    assert!(matches!(err.number, 8115 | 220), "{}", err.message);
    // Every date type but `time` is read, a `datetimeoffset` on its local day.
    for (date, ty, expected) in [
        ("2020-01-31 23:59:59.997", SqlType::DateTime, "2020-01-31"),
        ("2020-01-31 23:59", SqlType::SmallDateTime, "2020-01-31"),
        (
            "2020-01-31 23:59:59.9999999",
            SqlType::DateTime2(7),
            "2020-01-31",
        ),
        (
            "2020-02-01 01:00:00 +05:00",
            SqlType::DateTimeOffset(0),
            "2020-02-29",
        ),
        ("0001-01-01", SqlType::DateTime2(0), "0001-01-31"),
    ] {
        let (value, info) = typed(date, ty);
        assert_eq!(
            call("EOMONTH", &[value], &[info]).unwrap(),
            date_of(expected),
            "{date}"
        );
    }
    // The calendar's edges: the month after December 9999 has no end, 517 for `date`.
    assert_eq!(eomonth("9999-12-01").unwrap(), date_of("9999-12-31"));
    assert_eq!(eomonth("0001-01-15").unwrap(), date_of("0001-01-31"));
    let err = eomonth_offset("9999-12-01", Value::I32(1), SqlType::Int).unwrap_err();
    assert_eq!(err.number, 517);
    assert_eq!(err.message, "The addition overflowed the 'date' column.");
    assert_eq!(
        eomonth_offset("0001-01-15", Value::I32(-1), SqlType::Int)
            .unwrap_err()
            .number,
        517
    );
    assert_eq!(
        eomonth_offset("2020-01-01", Value::I32(i32::MAX), SqlType::Int)
            .unwrap_err()
            .number,
        517
    );
    // Types: `date`, always nullable; 8116 on argument 1 for `time` and the numbers,
    // 206 on argument 2 for a date type.
    let def = lookup("EOMONTH").expect("registered");
    let info = (def.return_type)(&[string_type()]).unwrap();
    assert_eq!(info.ty, SqlType::Date);
    assert!(info.nullable);
    for ty in [
        SqlType::Time(7),
        SqlType::Int,
        SqlType::Bit,
        SqlType::Float,
        SqlType::UniqueIdentifier,
    ] {
        let err = (def.return_type)(&[TypeInfo::new(ty, false)]).unwrap_err();
        assert_eq!(err.number, 8116);
    }
    assert_eq!(
        (def.return_type)(&[TypeInfo::new(SqlType::Time(7), false)])
            .unwrap_err()
            .message,
        "Data type time is not accepted for argument 1 of the eomonth function."
    );
    let err = (def.return_type)(&[string_type(), TypeInfo::new(SqlType::Date, false)]).unwrap_err();
    assert_eq!(err.number, 206);
    assert_eq!(
        err.message,
        "Type mismatch: date cannot be combined with int."
    );
}

#[test]
fn datefromparts_validates() {
    register_builtins();
    let parts = |y: Value, m: Value, d: Value| {
        call(
            "DATEFROMPARTS",
            &[y, m, d],
            &[TypeInfo::new(SqlType::Int, true), int_type(), int_type()],
        )
    };
    assert_eq!(
        parts(Value::I32(2020), Value::I32(2), Value::I32(29)).unwrap(),
        date_of("2020-02-29")
    );
    assert_eq!(
        parts(Value::I32(1), Value::I32(1), Value::I32(1)).unwrap(),
        date_of("0001-01-01")
    );
    assert_eq!(
        parts(Value::I32(9999), Value::I32(12), Value::I32(31)).unwrap(),
        date_of("9999-12-31")
    );
    for (y, m, d) in [
        (2019, 2, 29),
        (2020, 13, 1),
        (0, 1, 1),
        (10_000, 1, 1),
        (2020, 1, 0),
        (2020, 4, 31),
        (2020, -1, 1),
        (-1, 1, 1),
        (i32::MAX, 1, 1),
        (2020, 1, 300),
    ] {
        let err = parts(Value::I32(y), Value::I32(m), Value::I32(d)).unwrap_err();
        assert_eq!(err.number, 289, "({y}, {m}, {d})");
        assert_eq!(
            err.message,
            "Data type date cannot be built from these arguments: at least one value is out of range."
        );
    }
    // `NULL` anywhere is `NULL`, even next to parts that make no date.
    assert_eq!(
        parts(Value::Null, Value::I32(1), Value::I32(1)).unwrap(),
        Value::Null
    );
    assert_eq!(
        parts(Value::Null, Value::I32(13), Value::I32(99)).unwrap(),
        Value::Null
    );
    // Conversions to `int`: `money` rounds, a string is read, a wrong one is 245.
    assert_eq!(
        call(
            "DATEFROMPARTS",
            &[Value::Money(20_209_000), Value::I32(2), Value::I32(28)],
            &[TypeInfo::new(SqlType::Money, false), int_type(), int_type()]
        )
        .unwrap(),
        date_of("2021-02-28")
    );
    let s = string_type();
    assert_eq!(
        call(
            "DATEFROMPARTS",
            &[text("2020"), text("2"), text("29")],
            &[s.clone(), s.clone(), s.clone()]
        )
        .unwrap(),
        date_of("2020-02-29")
    );
    assert_eq!(
        call(
            "DATEFROMPARTS",
            &[text("x"), Value::I32(2), Value::I32(29)],
            &[s, int_type(), int_type()]
        )
        .unwrap_err()
        .number,
        245
    );
    // Types: `date`, nullable only when an argument is; 206 for a date type, 257 for
    // a `datetime`.
    let def = lookup("DATEFROMPARTS").expect("registered");
    let info = (def.return_type)(&[int_type(), int_type(), int_type()]).unwrap();
    assert_eq!(info.ty, SqlType::Date);
    assert!(!info.nullable);
    let info =
        (def.return_type)(&[TypeInfo::new(SqlType::Int, true), int_type(), int_type()]).unwrap();
    assert!(info.nullable);
    let err = (def.return_type)(&[TypeInfo::new(SqlType::Date, false), int_type(), int_type()])
        .unwrap_err();
    assert_eq!(err.number, 206);
    assert_eq!(
        err.message,
        "Type mismatch: date cannot be combined with int."
    );
    let err = (def.return_type)(&[
        int_type(),
        TypeInfo::new(SqlType::UniqueIdentifier, false),
        int_type(),
    ])
    .unwrap_err();
    assert_eq!(
        err.message,
        "Type mismatch: uniqueidentifier cannot be combined with int."
    );
    let err = (def.return_type)(&[
        int_type(),
        int_type(),
        TypeInfo::new(SqlType::DateTime, false),
    ])
    .unwrap_err();
    assert_eq!(err.number, 257);
}

#[test]
fn keyword_errors_and_definitions() {
    let err = dateadd_str("foo", 1, "2020-01-01").unwrap_err();
    assert_eq!(err.number, 155);
    assert_eq!(err.message, "'foo' is not a known dateadd option.");
    let err = datediff_str("foo", "2020-01-01", "2020-01-02").unwrap_err();
    assert_eq!(err.message, "'foo' is not a known datediff option.");
    // Arity through `check_call`.
    assert_eq!(
        call("DATEADD", &[text("day")], &[string_type()])
            .unwrap_err()
            .number,
        174
    );
    assert_eq!(call("EOMONTH", &[], &[]).unwrap_err().number, 189);
    // Every definition is registered, scalar and deterministic.
    register_builtins();
    for name in ["DATEADD", "DATEDIFF", "EOMONTH", "DATEFROMPARTS"] {
        let def = lookup(name).expect(name);
        assert_eq!(def.kind, FunctionKind::Scalar);
        assert!(def.deterministic);
        assert!(def.aggregate.is_none());
    }
}
