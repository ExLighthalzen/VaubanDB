use super::*;
use crate::builtins::check_call;
use crate::context::StaticContext;
use crate::{lookup, register_builtins};
use vauban_types::calendar::is_valid_civil;
use vauban_types::{Date, DateTime2, Time};

/// The definitions this file registers, in registration order.
const NAMES: [&str; 10] = [
    "GETDATE",
    "CURRENT_TIMESTAMP",
    "GETUTCDATE",
    "SYSDATETIME",
    "SYSUTCDATETIME",
    "DATEPART",
    "DATENAME",
    "YEAR",
    "MONTH",
    "DAY",
];

/// 2020-03-01 13:45:30.1234567, the instant every test of this module reads.
///
/// The day count is **computed** from the calendar of `types`, never written down: a
/// literal 737 484 would be one more place for the two epochs to drift apart.
fn instant() -> DateTime2 {
    let seconds = 13 * 3600 + 45 * 60 + 30;
    DateTime2 {
        date: Date {
            days: days_from_civil(2020, 3, 1),
        },
        time: Time {
            ticks_100ns: seconds * TICKS_100NS_PER_SECOND + 1_234_567,
        },
    }
}

/// A context whose clock is [`instant`], with the `us_english` default `DATEFIRST`.
fn ctx() -> StaticContext {
    StaticContext {
        now: instant(),
        ..StaticContext::default()
    }
}

fn string_type() -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), false)
}

fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

/// Calls `name` on `values`/`types` through the registry, exactly as the executor does.
fn call(name: &str, values: &[Value], types: &[TypeInfo], ctx: &StaticContext) -> SqlResult<Value> {
    let def = lookup(name).expect("the function must be registered");
    let result = check_call(def, types)?;
    let args = EvalArgs {
        values,
        types,
        result: &result,
    };
    (def.eval)(&args, ctx)
}

/// `DATEPART(part, <datetime2 literal>)` on [`instant`], under the default `DATEFIRST`.
fn datepart_of(part: &str, value: Value, ty: SqlType) -> SqlResult<Value> {
    call(
        "DATEPART",
        &[text(part), value],
        &[string_type(), TypeInfo::new(ty, false)],
        &ctx(),
    )
}

/// `DATEPART(part, <the instant>)` as an `i32`.
fn part(name: &str) -> i32 {
    match datepart_of(name, Value::DateTime2(instant()), SqlType::DateTime2(7)) {
        Ok(Value::I32(n)) => n,
        other => panic!("DATEPART({name}, …) answered {other:?}"),
    }
}

/// `DATEPART(part, <date literal>)` under `datefirst`, the date given as a string.
fn part_of_date(name: &str, date: &str, datefirst: u8) -> i32 {
    let context = StaticContext { datefirst, ..ctx() };
    let value = call(
        "DATEPART",
        &[text(name), text(date)],
        &[string_type(), string_type()],
        &context,
    );
    match value {
        Ok(Value::I32(n)) => n,
        other => panic!("DATEPART({name}, '{date}') answered {other:?}"),
    }
}

#[test]
fn datetime_epoch_offset() {
    register_builtins();
    // The constant agrees with the calendar of `types`.
    assert_eq!(days_from_civil(1900, 1, 1), DAYS_1900);
    // The value `value.rs` documents for the epoch of the year 2000...
    assert_eq!(days_from_civil(2000, 1, 1), 730_119);
    // ...and the century between the two, 36 524 days (100 years, 24 of them leap).
    assert_eq!(730_119 - 36_524, 693_595);
    assert_eq!(DAYS_1900, 693_595);
}

#[test]
fn month_helpers_agree_with_the_calendar() {
    for year in 1899..=2101 {
        for month in 1..=12u8 {
            let next = if month == 12 {
                days_from_civil(year + 1, 1, 1)
            } else {
                days_from_civil(year, month + 1, 1)
            };
            let from_calendar = next - days_from_civil(year, month, 1);
            assert_eq!(
                i32::from(days_in_month(year, month)),
                from_calendar,
                "{year}-{month:02}"
            );
        }
        assert_eq!(is_leap_year(year), days_in_month(year, 2) == 29, "{year}");
    }
    assert_eq!(days_in_month(2019, 2), 28);
    assert_eq!(days_in_month(2020, 2), 29);
    // A month number that is not a month has no days, and `types` agrees that the
    // dates it would build are invalid.
    assert_eq!(days_in_month(2020, 0), 0);
    assert_eq!(days_in_month(2020, 13), 0);
    assert!(!is_valid_civil(2019, 2, 29));
    assert!(is_valid_civil(2020, 2, 29));
    // The three centuries that show the 400-year rule.
    assert!(!is_leap_year(1900));
    assert!(is_leap_year(2000));
    assert!(!is_leap_year(2100));
}

#[test]
fn getdate_converts_to_datetime() {
    register_builtins();
    let context = ctx();
    let value = call("GETDATE", &[], &[], &context).expect("GETDATE must evaluate");
    let Value::DateTime(dt) = value else {
        panic!("GETDATE answered {value:?}");
    };
    assert_eq!(dt.days, instant().date.days - DAYS_1900);
    // 13:45:30.1234567 in 1/300 s: the whole seconds, plus round(0.1234567 * 300) = 37.
    let seconds = 13 * 3600 + 45 * 60 + 30;
    assert_eq!(dt.ticks_300th, seconds * 300 + 37);
    // The rounding is what makes a `datetime` show 0, 3 or 7 as its last millisecond
    // digit: tick 37 reads .123.
    assert_eq!(u64::from(dt.ticks_300th % 300) * 1000 / 300, 123);

    let def = lookup("GETDATE").expect("registered");
    let ty = (def.return_type)(&[]).expect("no argument, no error");
    assert_eq!(ty.ty, SqlType::DateTime);
    assert!(!ty.nullable);
    // GETUTCDATE reads the other clock through the same conversion.
    let utc = call("GETUTCDATE", &[], &[], &context).expect("GETUTCDATE must evaluate");
    assert_eq!(utc, Value::DateTime(dt));
}

#[test]
fn sysdatetime_keeps_full_precision() {
    register_builtins();
    let context = ctx();
    let value = call("SYSDATETIME", &[], &[], &context).expect("SYSDATETIME must evaluate");
    assert_eq!(value, Value::DateTime2(instant()));
    assert_eq!(
        call("SYSUTCDATETIME", &[], &[], &context).expect("must evaluate"),
        Value::DateTime2(instant())
    );

    for name in ["SYSDATETIME", "SYSUTCDATETIME"] {
        let def = lookup(name).expect("registered");
        let ty = (def.return_type)(&[]).expect("no argument, no error");
        assert_eq!(ty.ty, SqlType::DateTime2(7), "{name}");
        assert!(!ty.nullable, "{name}");
    }
    // The whole point of the pair: SYSDATETIME keeps the four digits GETDATE rounds
    // away (a rendered length of 27 against 23).
    let Value::DateTime(rounded) = call("GETDATE", &[], &[], &context).expect("must evaluate")
    else {
        panic!("GETDATE must answer a datetime");
    };
    assert_ne!(
        u64::from(rounded.ticks_300th % 300) * NANOS_PER_SECOND / 300,
        1_234_567 * 100
    );
}

#[test]
fn parse_datepart_accepts_every_spelling() {
    let expected: [(&str, DatePart); 40] = DATEPART_KEYWORDS;
    for (spelling, part) in expected {
        assert_eq!(parse_datepart(spelling), Some(part), "{spelling}");
        assert_eq!(
            parse_datepart(&spelling.to_uppercase()),
            Some(part),
            "{spelling} in upper case"
        );
    }
    // The three traps of the table: `n` is minute, `s` is second, `y` is day of year.
    assert_eq!(parse_datepart("n"), Some(DatePart::Minute));
    assert_eq!(parse_datepart("s"), Some(DatePart::Second));
    assert_eq!(parse_datepart("y"), Some(DatePart::DayOfYear));
    assert_eq!(parse_datepart("w"), Some(DatePart::Weekday));
    // Near misses, which SQL Server refuses with 155.
    for unknown in [
        "foo",
        "dayofweek",
        "doy",
        "isoweek",
        "iso_wk",
        "mon",
        "sec",
        "min",
        "h",
        "yyy",
        "milliseconds",
        "weeks",
        "qtr",
        "mcsec",
        "nsec",
        "epoch",
        "",
    ] {
        assert_eq!(parse_datepart(unknown), None, "{unknown}");
    }
}

#[test]
fn datepart_unknown_is_155() {
    register_builtins();
    let err = datepart_of("foo", Value::DateTime2(instant()), SqlType::DateTime2(7))
        .expect_err("an unknown keyword must be refused");
    assert_eq!(err.number, 155);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(err.message, "'foo' is not a known datepart option.");

    // DATENAME names itself in the message, not `datepart`.
    let err = call(
        "DATENAME",
        &[text("foo"), Value::DateTime2(instant())],
        &[string_type(), TypeInfo::new(SqlType::DateTime2(7), false)],
        &ctx(),
    )
    .expect_err("an unknown keyword must be refused");
    assert_eq!(err.number, 155);
    assert_eq!(err.message, "'foo' is not a known datename option.");
}

#[test]
fn deterministic_flags() {
    register_builtins();
    for name in [
        "DATEPART",
        "DATENAME",
        "GETDATE",
        "CURRENT_TIMESTAMP",
        "GETUTCDATE",
        "SYSDATETIME",
        "SYSUTCDATETIME",
    ] {
        let def = lookup(name).expect("registered");
        assert!(!def.deterministic, "{name} must not be deterministic");
    }
    for name in ["YEAR", "MONTH", "DAY"] {
        let def = lookup(name).expect("registered");
        assert!(def.deterministic, "{name} must be deterministic");
    }
    for name in NAMES {
        let def = lookup(name).expect("registered");
        assert!(def.aggregate.is_none(), "{name} is not an aggregate");
        assert_eq!(def.kind, FunctionKind::Scalar, "{name}");
    }
}

#[test]
fn datepart_extracts_components() {
    register_builtins();
    assert_eq!(part("year"), 2020);
    assert_eq!(part("quarter"), 1);
    assert_eq!(part("month"), 3);
    assert_eq!(part("day"), 1);
    assert_eq!(part("dayofyear"), 61);
    assert_eq!(part("hour"), 13);
    assert_eq!(part("minute"), 45);
    assert_eq!(part("second"), 30);
    assert_eq!(part("millisecond"), 123);
    assert_eq!(part("microsecond"), 123_456);
    assert_eq!(part("nanosecond"), 123_456_700);
    // 2020 is a leap year, so the 1st of March is day 61 and not 60.
    assert_eq!(part("dayofyear"), 31 + 29 + 1);
    // Every abbreviation answers like its long form.
    for (short, long) in [
        ("yy", "year"),
        ("yyyy", "year"),
        ("qq", "quarter"),
        ("q", "quarter"),
        ("mm", "month"),
        ("m", "month"),
        ("dd", "day"),
        ("d", "day"),
        ("dy", "dayofyear"),
        ("y", "dayofyear"),
        ("hh", "hour"),
        ("mi", "minute"),
        ("n", "minute"),
        ("ss", "second"),
        ("s", "second"),
        ("ms", "millisecond"),
        ("mcs", "microsecond"),
        ("ns", "nanosecond"),
        ("wk", "week"),
        ("ww", "week"),
        ("dw", "weekday"),
        ("w", "weekday"),
        ("isowk", "iso_week"),
        ("isoww", "iso_week"),
        ("tz", "tzoffset"),
    ] {
        assert_eq!(part(short), part(long), "{short} vs {long}");
    }
}

#[test]
fn datepart_of_a_datetime_reads_three_hundredths() {
    register_builtins();
    // A `datetime` counts 1/300 s, so its fraction is a third of a second and not a
    // round number of 100 ns units: SQL Server answers 123 333 333 ns, which a detour
    // through `datetime2(7)` would have turned into 123 333 300.
    let value = call("GETDATE", &[], &[], &ctx()).expect("must evaluate");
    for (name, expected) in [
        ("millisecond", 123),
        ("microsecond", 123_333),
        ("nanosecond", 123_333_333),
    ] {
        assert_eq!(
            datepart_of(name, value.clone(), SqlType::DateTime).expect("must evaluate"),
            Value::I32(expected),
            "{name}"
        );
    }
}

#[test]
fn datepart_weekday_uses_datefirst() {
    register_builtins();
    // 2020-03-01 is a Sunday: with the `us_english` default the week starts on Sunday,
    // so it is day 1; with the week starting on Monday it is day 7.
    assert_eq!(part_of_date("weekday", "2020-03-01", 7), 1);
    assert_eq!(part_of_date("weekday", "2020-03-01", 1), 7);
    // The Monday and the Saturday that follow, under both settings.
    assert_eq!(part_of_date("weekday", "2020-03-02", 7), 2);
    assert_eq!(part_of_date("weekday", "2020-03-02", 1), 1);
    assert_eq!(part_of_date("weekday", "2020-03-07", 7), 7);
    assert_eq!(part_of_date("weekday", "2020-03-07", 1), 6);
    // Each DATEFIRST value.
    for (datefirst, expected) in [(1, 7), (2, 6), (3, 5), (4, 4), (5, 3), (6, 2), (7, 1)] {
        assert_eq!(
            part_of_date("weekday", "2020-03-01", datefirst),
            expected,
            "DATEFIRST {datefirst}"
        );
    }
}

#[test]
fn datepart_iso_week() {
    register_builtins();
    // The two rules disagree: 2021-01-01 is a Friday, so ISO 8601 leaves it in week 53
    // of 2020 while SQL Server's `week` opens week 1 on it.
    assert_eq!(part_of_date("iso_week", "2021-01-01", 7), 53);
    assert_eq!(part_of_date("week", "2021-01-01", 7), 1);
    // Year boundaries, `week` then `iso_week`.
    for (date, week, iso) in [
        ("2019-12-30", 53, 1),
        ("2020-01-01", 1, 1),
        ("2020-12-31", 53, 53),
        ("2021-01-02", 1, 53),
        ("2021-01-03", 2, 53),
        ("2021-01-04", 2, 1),
        ("2015-12-31", 53, 53),
        ("2016-01-01", 1, 53),
        ("2000-01-01", 1, 52),
        ("2026-01-01", 1, 1),
        ("1900-01-01", 1, 1),
        ("2020-03-01", 10, 9),
    ] {
        assert_eq!(part_of_date("week", date, 7), week, "week of {date}");
        assert_eq!(part_of_date("iso_week", date, 7), iso, "iso_week of {date}");
    }
    // `week` moves with DATEFIRST and `iso_week` does not. The 31st of December 2020 is
    // the vector that separates them: it reaches week 54 under DATEFIRST 4 alone.
    for (datefirst, week) in [
        (1, 53),
        (2, 53),
        (3, 53),
        (4, 54),
        (5, 53),
        (6, 53),
        (7, 53),
    ] {
        assert_eq!(
            part_of_date("week", "2020-12-31", datefirst),
            week,
            "week under DATEFIRST {datefirst}"
        );
        assert_eq!(
            part_of_date("iso_week", "2020-12-31", datefirst),
            53,
            "iso_week under DATEFIRST {datefirst}"
        );
    }
    assert_eq!(part_of_date("week", "2021-01-03", 1), 1);
    assert_eq!(part_of_date("week", "2021-01-03", 6), 2);
}

#[test]
fn datename_gives_english_names() {
    register_builtins();
    let name = |part: &str, date: &str| {
        let value = call(
            "DATENAME",
            &[text(part), text(date)],
            &[string_type(), string_type()],
            &ctx(),
        )
        .expect("DATENAME must evaluate");
        match value {
            Value::String(s) => s.text,
            other => panic!("DATENAME({part}, '{date}') answered {other:?}"),
        }
    };
    assert_eq!(name("month", "2020-03-01"), "March");
    assert_eq!(name("weekday", "2020-03-01"), "Sunday");
    assert_eq!(name("year", "2020-03-01"), "2020");
    // The twelve months and the seven days.
    for (month, expected) in MONTH_NAMES.iter().enumerate() {
        let date = format!("2020-{:02}-15", month + 1);
        assert_eq!(&name("month", &date), expected, "{date}");
    }
    for (offset, expected) in [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ]
    .iter()
    .enumerate()
    {
        let date = format!("2020-03-{:02}", offset + 1);
        assert_eq!(&name("weekday", &date), expected, "{date}");
    }
    // The day names do not follow DATEFIRST, only the numbering does.
    let monday_first = StaticContext {
        datefirst: 1,
        ..ctx()
    };
    let value = call(
        "DATENAME",
        &[text("weekday"), text("2020-03-01")],
        &[string_type(), string_type()],
        &monday_first,
    )
    .expect("must evaluate");
    assert_eq!(value, text("Sunday"));
    // Everything else is the decimal DATEPART.
    assert_eq!(name("quarter", "2020-03-01"), "1");
    assert_eq!(name("dayofyear", "2020-03-01"), "61");
    assert_eq!(name("day", "2020-03-01"), "1");
    assert_eq!(name("week", "2020-03-01"), "10");
    assert_eq!(name("iso_week", "2021-01-01"), "53");

    let def = lookup("DATENAME").expect("registered");
    let ty = (def.return_type)(&[string_type(), string_type()]).expect("valid call");
    assert_eq!(ty.ty, SqlType::NVarChar(Len::Fixed(30)));
    assert!(ty.nullable);
}

#[test]
fn datename_renders_a_zone_offset() {
    register_builtins();
    // The one exception to "everything else is the decimal DATEPART": `'+02:30'`
    // where DATEPART answers 150, and `'-05:45'` where it answers -345.
    assert_eq!(format_offset(150), "+02:30");
    assert_eq!(format_offset(-345), "-05:45");
    assert_eq!(format_offset(0), "+00:00");
    let value = call(
        "DATENAME",
        &[text("tzoffset"), text("2020-03-01 13:45:30 +02:30")],
        &[string_type(), string_type()],
        &ctx(),
    )
    .expect("must evaluate");
    assert_eq!(value, text("+02:30"));
    assert_eq!(
        part_of_date("tzoffset", "2020-03-01 13:45:30 +02:30", 7),
        150
    );
    assert_eq!(
        part_of_date("tzoffset", "2020-03-01 13:45:30 -05:45", 7),
        -345
    );
    // No zone written, no offset.
    assert_eq!(part_of_date("tzoffset", "2020-03-01 13:45:30", 7), 0);
}

#[test]
fn datepart_reads_a_datetimeoffset_locally() {
    register_builtins();
    // `Value::DateTimeOffset` stores the universal instant, but DATEPART reports the
    // wall clock the value was written
    // with. Written at 01:30 in +05:00, the universal instant is the previous day at
    // 20:30, and reading it universally would change the **day**.
    let dto = TypeInfo::new(SqlType::DateTimeOffset(7), false);
    let value = convert(
        &text("2020-03-01 01:30:00 +05:00"),
        &string_type(),
        &dto,
        None,
    )
    .expect("the string must read as a datetimeoffset");
    for (name, expected) in [("hour", 1), ("day", 1), ("month", 3), ("tzoffset", 300)] {
        assert_eq!(
            datepart_of(name, value.clone(), SqlType::DateTimeOffset(7)).expect("must evaluate"),
            Value::I32(expected),
            "{name}"
        );
    }
    // The other direction, where the universal instant would fall on the next day.
    let value = convert(
        &text("2020-03-01 23:30:00 -05:00"),
        &string_type(),
        &dto,
        None,
    )
    .expect("the string must read as a datetimeoffset");
    for (name, expected) in [("hour", 23), ("day", 1), ("tzoffset", -300)] {
        assert_eq!(
            datepart_of(name, value.clone(), SqlType::DateTimeOffset(7)).expect("must evaluate"),
            Value::I32(expected),
            "{name}"
        );
    }
}

#[test]
fn datepart_refuses_what_the_type_cannot_answer() {
    register_builtins();
    // A `date` has no time of day: 9810, not a zero.
    let date = convert(
        &text("2020-03-01"),
        &string_type(),
        &TypeInfo::new(SqlType::Date, false),
        None,
    )
    .expect("must convert");
    let err = datepart_of("hour", date.clone(), SqlType::Date).expect_err("9810 expected");
    assert_eq!(err.number, 9810);
    assert_eq!(err.severity, 16);
    assert_eq!(
        err.message,
        "Datepart hour cannot be used with the date function datepart on data type date."
    );
    assert!(datepart_of("tzoffset", date.clone(), SqlType::Date).is_err());
    assert!(datepart_of("year", date, SqlType::Date).is_ok());

    // A `time` has no calendar date.
    let time = convert(
        &text("13:45:30.1234567"),
        &string_type(),
        &TypeInfo::new(SqlType::Time(7), false),
        None,
    )
    .expect("must convert");
    let err = datepart_of("year", time.clone(), SqlType::Time(7)).expect_err("9810 expected");
    assert_eq!(err.number, 9810);
    assert_eq!(
        err.message,
        "Datepart year cannot be used with the date function datepart on data type time."
    );
    assert!(datepart_of("weekday", time.clone(), SqlType::Time(7)).is_err());
    assert_eq!(
        datepart_of("hour", time, SqlType::Time(7)).expect("must evaluate"),
        Value::I32(13)
    );

    // A `datetime` carries no zone, so only `tzoffset` is refused, and the message
    // names `datetime` for `smalldatetime` too.
    let now = call("GETDATE", &[], &[], &ctx()).expect("must evaluate");
    let err = datepart_of("tzoffset", now.clone(), SqlType::DateTime).expect_err("9810 expected");
    assert_eq!(
        err.message,
        "Datepart tzoffset cannot be used with the date function datepart on data type datetime."
    );
    assert!(datepart_of("year", now, SqlType::DateTime).is_ok());
    // A `datetime2` answers `tzoffset` with a zero rather than refusing it.
    assert_eq!(
        datepart_of(
            "tzoffset",
            Value::DateTime2(instant()),
            SqlType::DateTime2(7)
        )
        .expect("must evaluate"),
        Value::I32(0)
    );
    // DATENAME says `datename` in the same message.
    let err = call(
        "DATENAME",
        &[text("hour"), Value::Date(Date { days: 0 })],
        &[string_type(), TypeInfo::new(SqlType::Date, false)],
        &ctx(),
    )
    .expect_err("9810 expected");
    assert_eq!(
        err.message,
        "Datepart hour cannot be used with the date function datename on data type date."
    );
}

/// The 9810 of `YEAR`, `MONTH` and `DAY` names `datepart`, not the shorthand called.
///
/// `DATEPART` and `DATENAME` cannot decide this: on those two, "the message names the
/// function called" and "the message names the engine primitive" give the same answer.
/// The vector that separates them is `SELECT YEAR(CAST('13:45:30' AS time(7)));`,
/// which answers `datepart`. `MONTH`, `DAY` and the two explicit functions are checked
/// alongside, so both readings are pinned here and neither can drift alone.
#[test]
fn the_shorthands_name_datepart_in_9810() {
    register_builtins();
    let time_type = TypeInfo::new(SqlType::Time(7), false);
    let time = convert(&text("13:45:30"), &string_type(), &time_type, None)
        .expect("the string must read as a time");

    // The three shorthands: the message says `datepart`, not `year`/`month`/`day`.
    for (function, keyword) in [("YEAR", "year"), ("MONTH", "month"), ("DAY", "day")] {
        let err = call(
            function,
            std::slice::from_ref(&time),
            std::slice::from_ref(&time_type),
            &ctx(),
        )
        .expect_err("9810 expected");
        assert_eq!(err.number, 9810, "{function}");
        assert_eq!(
            err.message,
            format!(
                "Datepart {keyword} cannot be used with the date function datepart \
on data type time."
            ),
            "{function}"
        );
    }

    // The other side of the same vector: the explicit functions do name themselves, so
    // the rule is "the shorthands are `datepart`", not "the name is never the callee".
    let err = datepart_of("year", time.clone(), SqlType::Time(7)).expect_err("9810 expected");
    assert_eq!(
        err.message,
        "Datepart year cannot be used with the date function datepart on data type time."
    );
    let err = call(
        "DATENAME",
        &[text("year"), time],
        &[string_type(), time_type],
        &ctx(),
    )
    .expect_err("9810 expected");
    assert_eq!(
        err.message,
        "Datepart year cannot be used with the date function datename on data type time."
    );
}

#[test]
fn a_uniqueidentifier_is_an_operand_type_clash() {
    register_builtins();
    // 206 and not 8116: the argument is refused by the implicit conversion, on
    // DATEPART, DATENAME and YEAR alike.
    let guid = TypeInfo::new(SqlType::UniqueIdentifier, false);
    let err = (lookup("DATEPART").expect("registered").return_type)(&[string_type(), guid.clone()])
        .expect_err("206 expected");
    assert_eq!(err.number, 206);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 2);
    assert_eq!(
        err.message,
        "Type mismatch: uniqueidentifier cannot be combined with datetime."
    );
    assert!((lookup("YEAR").expect("registered").return_type)(&[guid]).is_err());
    // Every other family is accepted, as SQL Server accepts it: a `binary`, a `bit`, an
    // `int` and a `float` all read as a `datetime` there.
    for ty in [
        SqlType::Binary(Len::Fixed(1)),
        SqlType::Bit,
        SqlType::Int,
        SqlType::Float,
        SqlType::Money,
        SqlType::Decimal {
            precision: 10,
            scale: 2,
        },
    ] {
        assert!(
            (lookup("DATEPART").expect("registered").return_type)(&[
                string_type(),
                TypeInfo::new(ty, false)
            ])
            .is_ok(),
            "{ty:?} must be accepted"
        );
    }
}

#[test]
fn year_month_day_match_datepart() {
    register_builtins();
    for date in [
        "2020-03-01",
        "1899-12-31",
        "1753-01-01",
        "9999-12-31",
        "2000-02-29",
    ] {
        for (function, keyword) in [("YEAR", "year"), ("MONTH", "month"), ("DAY", "day")] {
            let shorthand =
                call(function, &[text(date)], &[string_type()], &ctx()).expect("must evaluate");
            assert_eq!(
                shorthand,
                Value::I32(part_of_date(keyword, date, 7)),
                "{function}('{date}')"
            );
        }
    }
    // 1899-12-31 is before the epoch of `datetime`: the extraction is a calendar
    // computation, not an offset from 1900.
    assert_eq!(
        call("YEAR", &[text("1899-12-31")], &[string_type()], &ctx()).expect("must evaluate"),
        Value::I32(1899)
    );
    for name in ["YEAR", "MONTH", "DAY"] {
        let def = lookup(name).expect("registered");
        let ty = (def.return_type)(&[string_type()]).expect("valid call");
        assert_eq!(ty.ty, SqlType::Int, "{name}");
        // Nullable even on a literal.
        assert!(ty.nullable, "{name}");
    }
}

#[test]
fn null_propagates() {
    register_builtins();
    let nullable_date = TypeInfo::new(SqlType::DateTime2(7), true);
    for name in ["YEAR", "MONTH", "DAY"] {
        assert_eq!(
            call(
                name,
                &[Value::Null],
                std::slice::from_ref(&nullable_date),
                &ctx()
            )
            .expect("must evaluate"),
            Value::Null,
            "{name}"
        );
    }
    for name in ["DATEPART", "DATENAME"] {
        assert_eq!(
            call(
                name,
                &[text("year"), Value::Null],
                &[string_type(), nullable_date.clone()],
                &ctx()
            )
            .expect("must evaluate"),
            Value::Null,
            "{name}"
        );
    }
}

#[test]
fn arity_is_checked() {
    register_builtins();
    // The message names the function in lower case.
    let err = check_call(lookup("GETDATE").expect("registered"), &[string_type()])
        .expect_err("GETDATE takes no argument");
    assert_eq!(err.number, 174);
    assert_eq!(
        err.message,
        "The function getdate takes exactly 0 argument(s)."
    );
    let err = check_call(lookup("DATEPART").expect("registered"), &[string_type()])
        .expect_err("DATEPART takes two arguments");
    assert_eq!(
        err.message,
        "The function datepart takes exactly 2 argument(s)."
    );
    let err = check_call(
        lookup("YEAR").expect("registered"),
        &[string_type(), string_type()],
    )
    .expect_err("YEAR takes one argument");
    assert_eq!(
        err.message,
        "The function year takes exactly 1 argument(s)."
    );
}

#[test]
fn every_function_is_registered_once() {
    register_builtins();
    for name in NAMES {
        let def = lookup(name).expect("registered");
        assert_eq!(def.name, name);
        // Lookup is case-insensitive.
        assert_eq!(lookup(&name.to_lowercase()).expect("registered").name, name);
    }
}

// --- CURRENT_TIMESTAMP ----------------------------------------------------------------

/// `CURRENT_TIMESTAMP` reads the same clock as `GETDATE()` and answers the same type.
///
/// The two halves are separate claims and each needs its own vector: a definition that
/// took the type of `GETDATE()` but called `SYSDATETIME`'s evaluation would pass the
/// second assertion and fail the first, and one that kept the clock but declared
/// `datetime2(7)` would do the opposite. What the equality of *values* fixes is the
/// 1/300 s rounding: [`to_datetime`] is what makes 13:45:30.1234567 land on
/// 13:45:30.123, and a `CURRENT_TIMESTAMP` returning the raw [`EvalContext::now_local`]
/// would differ here, where
/// `SELECT CASE WHEN CURRENT_TIMESTAMP = GETDATE() THEN 1 ELSE 0 END;` answers 1.
#[test]
fn current_timestamp_is_getdate() {
    register_builtins();
    let ctx = ctx();
    let now = call("CURRENT_TIMESTAMP", &[], &[], &ctx).expect("CURRENT_TIMESTAMP");
    assert_eq!(now, call("GETDATE", &[], &[], &ctx).expect("GETDATE"));
    assert!(
        matches!(now, Value::DateTime(_)),
        "CURRENT_TIMESTAMP must answer a datetime, got {now:?}"
    );
    // …and not the unrounded reading of the context, which is what `SYSDATETIME`
    // answers: the two clocks must not be interchangeable.
    assert_ne!(
        now,
        call("SYSDATETIME", &[], &[], &ctx).expect("SYSDATETIME")
    );

    // `datetime`, precision 23, scale 3, not nullable: the type of `GETDATE()`.
    let info =
        check_call(lookup("CURRENT_TIMESTAMP").expect("registered"), &[]).expect("no argument");
    assert_eq!(info.ty, SqlType::DateTime);
    assert!(!info.nullable);
}

/// The niladic name is a name like another for the registry: found whatever the case,
/// of arity zero, and refused with 174 when given an argument.
///
/// The message names the function in **lower case**, as `check_call` does for each
/// built-in; on SQL Server `CURRENT_TIMESTAMP(1)` does not reach the stage that counts
/// arguments (it is error 102 at the parenthesis), so 174 is what the registry
/// answers on its own.
#[test]
fn current_timestamp_arity_and_case() {
    register_builtins();
    for spelling in [
        "current_timestamp",
        "Current_TimeStamp",
        "CURRENT_TIMESTAMP",
    ] {
        assert_eq!(
            lookup(spelling).expect("registered").name,
            "CURRENT_TIMESTAMP"
        );
    }
    let err = check_call(
        lookup("CURRENT_TIMESTAMP").expect("registered"),
        &[string_type()],
    )
    .expect_err("CURRENT_TIMESTAMP takes no argument");
    assert_eq!(err.number, 174);
    assert_eq!(
        err.message,
        "The function current_timestamp takes exactly 0 argument(s)."
    );
}
