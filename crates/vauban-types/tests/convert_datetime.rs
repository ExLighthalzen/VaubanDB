//! Integration test: `CAST` towards and between the six date and time types, **without an
//! explicit style**, then the styles of `CONVERT`, then the numeric and binary sources and
//! targets.

use vauban_types::calendar::{DAYS_1900, days_from_civil};
use vauban_types::{
    Date, DateTime, DateTime2, DateTimeOffset, Decimal, Len, SqlString, SqlType, Time, TypeInfo,
    Value, convert, default_display,
};

/// 100-nanosecond ticks in one second, the unit of [`Time`].
const TICKS_PER_SECOND: u64 = 10_000_000;

fn ti(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, true)
}

/// The type of a string literal as the binder types it: a `varchar(n)`.
fn varchar() -> TypeInfo {
    ti(SqlType::VarChar(Len::Fixed(40)))
}

fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

/// `CAST(<string> AS <type>)`: no style at all.
fn cast_text(s: &str, to: SqlType) -> vauban_errors::SqlResult<Value> {
    convert(&text(s), &varchar(), &ti(to), None)
}

/// `CAST(<value of type `from`> AS <type `to`>)`.
fn cast(v: &Value, from: SqlType, to: SqlType) -> vauban_errors::SqlResult<Value> {
    convert(v, &ti(from), &ti(to), None)
}

/// 100-nanosecond ticks of a time of day.
fn at(h: u64, m: u64, s: u64) -> u64 {
    (h * 3600 + m * 60 + s) * TICKS_PER_SECOND
}

/// The five style-less literal forms of a `date` all denote the 2nd of January 2000, day
/// 730 120 of the proleptic Gregorian calendar; the ISO `T`, the fraction of a second and
/// the 12-hour clock follow.
#[test]
fn string_to_date_formats() {
    let second_of_january = Value::Date(Date { days: 730_120 });
    for literal in [
        "2000-01-02",
        "20000102",
        "01/02/2000",
        "2 Jan 2000",
        "Jan 2, 2000",
    ] {
        assert_eq!(
            cast_text(literal, SqlType::Date),
            Ok(second_of_january.clone()),
            "{literal}"
        );
    }

    assert_eq!(
        cast_text("2000-01-02T13:05:06.123", SqlType::DateTime2(3)),
        Ok(Value::DateTime2(DateTime2 {
            date: Date { days: 730_120 },
            time: Time {
                ticks_100ns: at(13, 5, 6) + 1_230_000
            },
        }))
    );

    // A clock without a date takes the 1st of January 1900.
    assert_eq!(
        cast_text("13:05:06", SqlType::DateTime),
        Ok(Value::DateTime(DateTime {
            days: 0,
            ticks_300th: 300 * (13 * 3600 + 5 * 60 + 6),
        }))
    );
}

/// A string that is no date, a date that does not exist, and the empty string.
#[test]
fn string_to_date_errors() {
    let error = cast_text("not a date", SqlType::DateTime).unwrap_err();
    assert_eq!(error.number, 241);
    assert_eq!(error.severity, 16);
    assert_eq!(
        error.message,
        "The character string could not be converted to a date or time."
    );

    // `'2024-02-30'` reads as a date and is not one: 241, not 242.
    let error = cast_text("2024-02-30", SqlType::Date).unwrap_err();
    assert_eq!(error.number, 241);

    // The empty string is **not** an error: it is 1900-01-01.
    assert_eq!(
        cast_text("", SqlType::DateTime),
        Ok(Value::DateTime(DateTime {
            days: 0,
            ticks_300th: 0
        }))
    );
    assert_eq!(
        cast_text("", SqlType::Date),
        Ok(Value::Date(Date { days: DAYS_1900 }))
    );
}

/// The forms a meridiem marker takes, and the hours it allows.
///
/// The rendering is the one of `datetime2(0)`.
#[test]
fn string_with_a_meridiem_marker() {
    let rendered = |literal: &str| match cast_text(literal, SqlType::DateTime2(0)) {
        Ok(v) => default_display(&v, &ti(SqlType::DateTime2(0))),
        Err(e) => panic!("{literal}: {e:?}"),
    };

    // An hour with no minutes at all, glued to its marker or not; `PM` leaves an hour that
    // is already past noon alone; `'0 AM'` is midnight.
    for literal in ["1PM", "1 pm", "13 PM"] {
        assert_eq!(rendered(literal), "1900-01-01 13:00:00", "{literal}");
    }
    assert_eq!(rendered("0 AM"), "1900-01-01 00:00:00");
    assert_eq!(rendered("13:05PM"), "1900-01-01 13:05:00");
    assert_eq!(rendered("23:59PM"), "1900-01-01 23:59:00");
    assert_eq!(rendered("1:05PM"), "1900-01-01 13:05:00");

    // The same hour beside a date, which always comes first.
    assert_eq!(rendered("2024-01-02 1PM"), "2024-01-02 13:00:00");
    assert_eq!(rendered("Jan 2 2024 1 PM"), "2024-01-02 13:00:00");
    assert_eq!(rendered("2024-01-02 13:05:06 PM"), "2024-01-02 13:05:06");
    assert_eq!(rendered("2000-01-02 1:05 PM"), "2000-01-02 13:05:00");
    assert_eq!(rendered("2000-01-02 12:00AM"), "2000-01-02 00:00:00");
}

/// The strings the same family refuses, all read as a `datetime2(0)`: `AM` on an
/// afternoon hour, `PM` on midnight, a clock written before its date and the month name
/// `Sept`.
#[test]
fn string_meridiem_and_month_forms_that_are_refused() {
    for literal in [
        "13:05AM",
        "0:05PM",
        "2 PM 2024-01-02",
        "13:05:06 2024-01-02",
        "2:05 PM 2024-01-02",
        "Sept 2 2024",
    ] {
        let error = cast_text(literal, SqlType::DateTime2(0)).unwrap_err();
        assert_eq!(error.number, 241, "{literal}");
        assert_eq!(
            error.message, "The character string could not be converted to a date or time.",
            "{literal}"
        );
    }

    // `'Sept 2 2024'` is refused by every target, `datetime` included.
    assert_eq!(
        cast_text("Sept 2 2024", SqlType::Date).unwrap_err().number,
        241
    );
    assert_eq!(
        cast_text("Sept 2 2024", SqlType::DateTime)
            .unwrap_err()
            .number,
        241
    );
}

/// A date type read as another one: the parts it lacks are zeroed, the fraction is
/// rounded, and a `datetimeoffset` keeps its local time.
#[test]
fn date_to_date_conversions() {
    let day = days_from_civil(2000, 1, 2);

    // A `date` has no time: midnight.
    assert_eq!(
        cast(
            &Value::Date(Date { days: day }),
            SqlType::Date,
            SqlType::DateTime
        ),
        Ok(Value::DateTime(DateTime {
            days: day - DAYS_1900,
            ticks_300th: 0
        }))
    );

    // A `time` has no date: the 1st of January 1900.
    assert_eq!(
        cast(
            &Value::Time(Time {
                ticks_100ns: at(13, 5, 6)
            }),
            SqlType::Time(7),
            SqlType::DateTime
        ),
        Ok(Value::DateTime(DateTime {
            days: 0,
            ticks_300th: 300 * (13 * 3600 + 5 * 60 + 6),
        }))
    );

    // 13:05:06.1234567 down to the 1/300 s of a `datetime`…
    let precise = Value::DateTime2(DateTime2 {
        date: Date { days: day },
        time: Time {
            ticks_100ns: at(13, 5, 6) + 1_234_567,
        },
    });
    let Ok(Value::DateTime(rounded)) = cast(&precise, SqlType::DateTime2(7), SqlType::DateTime)
    else {
        panic!("a datetime2 converts to a datetime");
    };
    assert_eq!(rounded.days, day - DAYS_1900);
    assert_eq!(rounded.ticks_300th, 14_131_837);

    // … and to the whole minute of a `smalldatetime`: 13:05, the seconds dropped.
    assert_eq!(
        cast(&precise, SqlType::DateTime2(7), SqlType::SmallDateTime),
        Ok(Value::DateTime(DateTime {
            days: day - DAYS_1900,
            ticks_300th: (13 * 60 + 5) * 300 * 60,
        }))
    );

    // A `datetimeoffset` keeps its **local** reading and drops the offset: a value whose UTC
    // is 13:05 at +02:00 becomes 15:05.
    let offset = Value::DateTimeOffset(DateTimeOffset {
        utc: DateTime2 {
            date: Date { days: day },
            time: Time {
                ticks_100ns: at(13, 5, 0),
            },
        },
        offset_minutes: 120,
    });
    assert_eq!(
        cast(&offset, SqlType::DateTimeOffset(7), SqlType::DateTime2(7)),
        Ok(Value::DateTime2(DateTime2 {
            date: Date { days: day },
            time: Time {
                ticks_100ns: at(15, 5, 0)
            },
        }))
    );

    // The other way round, a date without an offset is read at +00:00.
    assert_eq!(
        cast(
            &Value::Date(Date { days: day }),
            SqlType::Date,
            SqlType::DateTimeOffset(0)
        ),
        Ok(Value::DateTimeOffset(DateTimeOffset {
            utc: DateTime2 {
                date: Date { days: day },
                time: Time { ticks_100ns: 0 },
            },
            offset_minutes: 0,
        }))
    );
}

/// The `smalldatetime` a whole minute of the 2nd of January 2000 spells.
fn sdt_2000_01_02(hour: u32, minute: u32) -> Value {
    Value::DateTime(DateTime {
        days: days_from_civil(2000, 1, 2) - DAYS_1900,
        ticks_300th: (hour * 60 + minute) * 300 * 60,
    })
}

/// The minute of a `smalldatetime` read from a **character string** is decided on the
/// second *and its fraction*, and the fraction is first snapped to the 1/300 s the legacy
/// reading uses.
///
/// Six fractions crossed with the two seconds that bound the rule, plus the five
/// milliseconds just under `.999`, `.994` to `.998`, the only place the two candidate
/// rules disagree. A rule that dropped the fraction before comparing to thirty seconds
/// would answer 13:05 on `29.999`; a plain half-up rounding to the minute would answer
/// 13:05 there too. The answer is **13:06**, and 13:05 on `29.998`.
#[test]
fn smalldatetime_string_rounds_the_fraction() {
    // Second 29: everything up to `.998` stays on 13:05, `.999` alone moves to 13:06.
    for fraction in [
        "000", "001", "250", "499", "500", "501", "750", "900", "994", "995", "996", "997", "998",
    ] {
        let literal = format!("2000-01-02 13:05:29.{fraction}");
        assert_eq!(
            cast_text(&literal, SqlType::SmallDateTime),
            Ok(sdt_2000_01_02(13, 5)),
            "{literal}"
        );
    }
    assert_eq!(
        cast_text("2000-01-02 13:05:29.999", SqlType::SmallDateTime),
        Ok(sdt_2000_01_02(13, 6))
    );

    // Second 30: every fraction is 13:06, including `.000`, so the line sits between
    // `29.998` and `29.999` and nowhere else.
    for fraction in [
        "000", "001", "250", "499", "500", "501", "750", "900", "994", "995", "996", "997", "998",
        "999",
    ] {
        let literal = format!("2000-01-02 13:05:30.{fraction}");
        assert_eq!(
            cast_text(&literal, SqlType::SmallDateTime),
            Ok(sdt_2000_01_02(13, 6)),
            "{literal}"
        );
    }

    // Why `.999` is the one that moves: read as a `datetime`, the same four strings snap to
    // the 1/300 s, and only `.999` reaches a whole `13:05:30.000`.
    for (fraction, ticks_300th) in [
        ("995", 300 * (13 * 3600 + 5 * 60 + 29) + 299),
        ("997", 300 * (13 * 3600 + 5 * 60 + 29) + 299),
        ("998", 300 * (13 * 3600 + 5 * 60 + 29) + 299),
        ("999", 300 * (13 * 3600 + 5 * 60 + 30)),
    ] {
        let literal = format!("2000-01-02 13:05:29.{fraction}");
        assert_eq!(
            cast_text(&literal, SqlType::DateTime),
            Ok(Value::DateTime(DateTime {
                days: days_from_civil(2000, 1, 2) - DAYS_1900,
                ticks_300th,
            })),
            "{literal}"
        );
    }
}

/// The snap to the 1/300 s belongs to the **string** reading, not to the target.
///
/// A `datetime2`, a `time` and a `datetimeoffset` carry a finer fraction and never pass
/// through it, so `29.9999999` stays on 13:05 for all three where the same characters cast
/// straight to `smalldatetime` are 13:06. A `datetime` source is already a 1/300 s tick and
/// answers like the string.
#[test]
fn smalldatetime_rounding_by_source_type() {
    let almost_thirty = at(13, 5, 29) + 9_999_999;
    let day = days_from_civil(2000, 1, 2);

    assert_eq!(
        cast(
            &Value::DateTime2(DateTime2 {
                date: Date { days: day },
                time: Time {
                    ticks_100ns: almost_thirty
                },
            }),
            SqlType::DateTime2(7),
            SqlType::SmallDateTime
        ),
        Ok(sdt_2000_01_02(13, 5))
    );
    assert_eq!(
        cast(
            &Value::Time(Time {
                ticks_100ns: almost_thirty
            }),
            SqlType::Time(7),
            SqlType::SmallDateTime
        ),
        Ok(Value::DateTime(DateTime {
            days: 0,
            ticks_300th: (13 * 60 + 5) * 300 * 60,
        }))
    );
    assert_eq!(
        cast(
            &Value::DateTimeOffset(DateTimeOffset {
                utc: DateTime2 {
                    date: Date { days: day },
                    time: Time {
                        ticks_100ns: almost_thirty
                    },
                },
                offset_minutes: 0,
            }),
            SqlType::DateTimeOffset(7),
            SqlType::SmallDateTime
        ),
        Ok(sdt_2000_01_02(13, 5))
    );

    // The same instant written as a string is 13:06: the two readings differ.
    assert_eq!(
        cast_text("2000-01-02 13:05:29.999", SqlType::SmallDateTime),
        Ok(sdt_2000_01_02(13, 6))
    );

    // A `datetime` holding `13:05:30.000` — what the same string becomes — rounds up, and
    // the 1/300 s tick just below it does not.
    for (ticks_300th, minute) in [
        (300 * (13 * 3600 + 5 * 60 + 30), 6),
        (300 * (13 * 3600 + 5 * 60 + 29) + 299, 5),
    ] {
        assert_eq!(
            cast(
                &Value::DateTime(DateTime {
                    days: day - DAYS_1900,
                    ticks_300th,
                }),
                SqlType::DateTime,
                SqlType::SmallDateTime
            ),
            Ok(sdt_2000_01_02(13, minute))
        );
    }

    // The sentence holds for **this** target only. Sent to a
    // `datetime`, the very same `datetime2(7)` value is rounded to the 1/300 s and reaches
    // `13:05:30.000` — the rounding the `datetime` target owes every source.
    assert_eq!(
        cast(
            &Value::DateTime2(DateTime2 {
                date: Date { days: day },
                time: Time {
                    ticks_100ns: almost_thirty
                },
            }),
            SqlType::DateTime2(7),
            SqlType::DateTime
        ),
        Ok(Value::DateTime(DateTime {
            days: day - DAYS_1900,
            ticks_300th: 300 * (13 * 3600 + 5 * 60 + 30),
        }))
    );
}

/// The carry of the same `.999` is confined neither to the minute nor to the upper bound.
///
/// `'2000-01-02 13:59:29.999'` crosses the **hour** and is 14:00, and
/// `'1899-12-31 23:59:29.999'` crosses into 1900-01-01, the first `smalldatetime` there is —
/// where its neighbour one millisecond lower stays in 1899 and is refused with a 242. A rule
/// that dropped the fraction before comparing to thirty seconds answers 13:59 on the first
/// and refuses the second.
#[test]
fn smalldatetime_string_carries_the_hour_and_the_low_bound() {
    assert_eq!(
        cast_text("2000-01-02 13:59:29.999", SqlType::SmallDateTime),
        Ok(sdt_2000_01_02(14, 0))
    );
    for literal in ["2000-01-02 13:59:29.998", "2000-01-02 13:59:29.499"] {
        assert_eq!(
            cast_text(literal, SqlType::SmallDateTime),
            Ok(sdt_2000_01_02(13, 59)),
            "{literal}"
        );
    }

    // The low bound: 1900-01-01 00:00 is day 0 of a `smalldatetime`.
    assert_eq!(
        cast_text("1899-12-31 23:59:29.999", SqlType::SmallDateTime),
        Ok(Value::DateTime(DateTime {
            days: 0,
            ticks_300th: 0,
        }))
    );
    let error = cast_text("1899-12-31 23:59:29.998", SqlType::SmallDateTime).unwrap_err();
    assert_eq!(error.number, 242);
    assert_eq!(
        error.message,
        "Converting varchar to smalldatetime produced a value outside the target range."
    );
}

/// The range is checked **after** the rounding, so the last minute of 2079-06-06
/// overflows on the same `.999`.
///
/// The neighbouring `.998` and `.499` are read: the overflow belongs to
/// the rounding and not to the day. On any other day the same fraction simply carries into
/// the next day.
#[test]
fn smalldatetime_string_rounds_past_the_range() {
    let error = cast_text("2079-06-06 23:59:29.999", SqlType::SmallDateTime).unwrap_err();
    assert_eq!(error.number, 242);
    assert_eq!(
        error.message,
        "Converting varchar to smalldatetime produced a value outside the target range."
    );

    let last_minute = Value::DateTime(DateTime {
        days: days_from_civil(2079, 6, 6) - DAYS_1900,
        ticks_300th: 1_439 * TICKS_300TH_PER_MINUTE,
    });
    for literal in ["2079-06-06 23:59:29.998", "2079-06-06 23:59:29.499"] {
        assert_eq!(
            cast_text(literal, SqlType::SmallDateTime),
            Ok(last_minute.clone()),
            "{literal}"
        );
    }

    // A `datetime2` source does not snap, so the very same characters read through one stay
    // inside the range.
    assert_eq!(
        cast(
            &Value::DateTime2(DateTime2 {
                date: Date {
                    days: days_from_civil(2079, 6, 6)
                },
                time: Time {
                    ticks_100ns: at(23, 59, 29) + 999 * 10_000
                },
            }),
            SqlType::DateTime2(3),
            SqlType::SmallDateTime
        ),
        Ok(last_minute)
    );

    // Away from the bound, the rounding carries into the next day.
    assert_eq!(
        cast_text("2000-01-02 23:59:29.999", SqlType::SmallDateTime),
        Ok(Value::DateTime(DateTime {
            days: days_from_civil(2000, 1, 3) - DAYS_1900,
            ticks_300th: 0,
        }))
    );
    assert_eq!(
        cast_text("2000-01-02 23:59:29.499", SqlType::SmallDateTime),
        Ok(Value::DateTime(DateTime {
            days: days_from_civil(2000, 1, 2) - DAYS_1900,
            ticks_300th: 1_439 * TICKS_300TH_PER_MINUTE,
        }))
    );
}

/// A date source outside the range of the target is 242.
#[test]
fn out_of_range_is_242() {
    let discovery = Value::Date(Date {
        days: days_from_civil(1492, 8, 3),
    });
    let error = cast(&discovery, SqlType::Date, SqlType::DateTime).unwrap_err();
    assert_eq!(error.number, 242);
    assert_eq!(error.severity, 16);
    assert_eq!(
        error.message,
        "Converting date to datetime produced a value outside the target range."
    );

    let too_late = Value::DateTime2(DateTime2 {
        date: Date {
            days: days_from_civil(2100, 1, 1),
        },
        time: Time { ticks_100ns: 0 },
    });
    let error = cast(&too_late, SqlType::DateTime2(0), SqlType::SmallDateTime).unwrap_err();
    assert_eq!(error.number, 242);
    assert_eq!(
        error.message,
        "Converting datetime2 to smalldatetime produced a value outside the target range."
    );

    // A string is refused the same way, and the message names the source type.
    let error = cast_text("1492-08-03", SqlType::DateTime).unwrap_err();
    assert_eq!(error.number, 242);
    assert_eq!(
        error.message,
        "Converting varchar to datetime produced a value outside the target range."
    );
}

/// The only fractions a `datetime` holds are multiples
/// of 1/300 s, so `.998` and `.999` are not representable.
#[test]
fn datetime_rounding() {
    let read = |literal: &str| match cast_text(literal, SqlType::DateTime) {
        Ok(v) => v,
        Err(e) => panic!("{literal}: {e:?}"),
    };

    let midnight = read("2000-01-01 00:00:00.999");
    assert_eq!(
        default_display(&midnight, &ti(SqlType::DateTime)),
        "Jan  1 2000 12:00AM"
    );
    // `.999` rounds up to the whole second that follows.
    assert_eq!(
        cast(&midnight, SqlType::DateTime, SqlType::DateTime2(3)),
        Ok(Value::DateTime2(DateTime2 {
            date: Date {
                days: days_from_civil(2000, 1, 1)
            },
            time: Time {
                ticks_100ns: TICKS_PER_SECOND
            },
        }))
    );

    // `.997` is representable, and `.998` falls back on it.
    for literal in ["2000-01-01 00:00:00.997", "2000-01-01 00:00:00.998"] {
        let value = read(literal);
        let Ok(rendered) = cast(&value, SqlType::DateTime, SqlType::DateTime2(3)) else {
            panic!("{literal}");
        };
        assert_eq!(
            default_display(&rendered, &ti(SqlType::DateTime2(3))),
            "2000-01-01 00:00:00.997",
            "{literal}"
        );
    }
}

/// The one pair of date types with no conversion at all, error 529.
#[test]
fn date_and_time_do_not_convert_into_each_other() {
    let error = cast(
        &Value::Date(Date { days: 730_120 }),
        SqlType::Date,
        SqlType::Time(0),
    )
    .unwrap_err();
    assert_eq!(error.number, 529);
    assert_eq!(
        error.message,
        "No explicit conversion exists from date to time."
    );

    let error = cast(
        &Value::Time(Time {
            ticks_100ns: at(13, 5, 6),
        }),
        SqlType::Time(0),
        SqlType::Date,
    )
    .unwrap_err();
    assert_eq!(error.number, 529);
    assert_eq!(
        error.message,
        "No explicit conversion exists from time to date."
    );
}

/// The fraction of a literal is rounded, not truncated,
/// to the precision of the target, and the rounding may carry into the next day.
#[test]
fn fraction_rounds_to_the_scale_of_the_target() {
    let day = days_from_civil(2000, 1, 2);
    let at_scale_3 = |literal: &str| match cast_text(literal, SqlType::DateTime2(3)) {
        Ok(v) => default_display(&v, &ti(SqlType::DateTime2(3))),
        Err(e) => panic!("{literal}: {e:?}"),
    };
    assert_eq!(
        at_scale_3("2000-01-02 13:05:06.1235"),
        "2000-01-02 13:05:06.124"
    );
    assert_eq!(
        at_scale_3("2000-01-02 13:05:06.1234"),
        "2000-01-02 13:05:06.123"
    );
    assert_eq!(
        at_scale_3("2000-01-02 23:59:59.9999"),
        "2000-01-03 00:00:00.000"
    );

    assert_eq!(
        cast_text("2000-01-02 13:05:06.6", SqlType::DateTime2(0)),
        Ok(Value::DateTime2(DateTime2 {
            date: Date { days: day },
            time: Time {
                ticks_100ns: at(13, 5, 7)
            },
        }))
    );
}

/// The bounds of each type, a two-digit year, six digits, a month and a year alone, and
/// the other separators.
#[test]
fn bounds_and_remaining_literal_forms() {
    assert_eq!(
        cast_text("0001-01-01", SqlType::Date),
        Ok(Value::Date(Date { days: 0 }))
    );
    assert_eq!(
        cast_text("9999-12-31", SqlType::Date),
        Ok(Value::Date(Date { days: 3_652_058 }))
    );
    assert_eq!(
        cast_text("1753-01-01", SqlType::DateTime),
        Ok(Value::DateTime(DateTime {
            days: days_from_civil(1753, 1, 1) - DAYS_1900,
            ticks_300th: 0,
        }))
    );
    assert_eq!(
        cast_text("2079-06-06", SqlType::SmallDateTime),
        Ok(Value::DateTime(DateTime {
            days: 65_535,
            ticks_300th: 0
        }))
    );

    let expect_date = |literal: &str, y: i32, m: u8, d: u8| {
        assert_eq!(
            cast_text(literal, SqlType::Date),
            Ok(Value::Date(Date {
                days: days_from_civil(y, m, d)
            })),
            "{literal}"
        );
    };
    expect_date("01/02/49", 2049, 1, 2);
    expect_date("01/02/50", 1950, 1, 2);
    expect_date("000102", 2000, 1, 2);
    expect_date("Jan 2000", 2000, 1, 1);
    expect_date("01-02-2000", 2000, 1, 2);
    expect_date("01.02.2000", 2000, 1, 2);
    expect_date("January 2, 2000", 2000, 1, 2);
}

/// The two halves of a
/// literal are independent, and the missing one takes its neutral value.
#[test]
fn the_two_halves_of_a_literal_are_independent() {
    assert_eq!(
        cast_text("13:05:06", SqlType::Date),
        Ok(Value::Date(Date { days: DAYS_1900 }))
    );
    assert_eq!(
        cast_text("2000-01-02", SqlType::Time(0)),
        Ok(Value::Time(Time { ticks_100ns: 0 }))
    );
}

/// `P.M.` is
/// not a meridiem marker, with or without minutes beside it.
#[test]
fn a_meridiem_marker_is_written_without_dots() {
    for literal in ["1 P.M.", "1:05 P.M."] {
        let error = cast_text(literal, SqlType::DateTime2(0)).unwrap_err();
        assert_eq!(error.number, 241, "{literal}");
        assert_eq!(
            error.message, "The character string could not be converted to a date or time.",
            "{literal}"
        );
    }
}

/// The spellings of a time-zone offset that are read, each vector rendered as a
/// `datetimeoffset(0)`, then the spellings that are refused: four digits, hours alone, a
/// lower-case `z`.
#[test]
fn offset_forms_read_and_refused() {
    let rendered = |literal: &str| match cast_text(literal, SqlType::DateTimeOffset(0)) {
        Ok(v) => default_display(&v, &ti(SqlType::DateTimeOffset(0))),
        Err(e) => panic!("{literal}: {e:?}"),
    };
    for literal in ["2000-01-02 13:05:06 Z", "2000-01-02 13:05:06Z"] {
        assert_eq!(rendered(literal), "2000-01-02 13:05:06 +00:00", "{literal}");
    }
    for literal in ["2000-01-02 13:05:06 -05:30", "2000-01-02 13:05:06-05:30"] {
        assert_eq!(rendered(literal), "2000-01-02 13:05:06 -05:30", "{literal}");
    }
    // One-digit hours and minutes in the offset.
    for literal in ["2000-01-02 13:05:06 +2:00", "2000-01-02 13:05:06 +02:0"] {
        assert_eq!(rendered(literal), "2000-01-02 13:05:06 +02:00", "{literal}");
    }
    // An offset behind a clock with no date.
    assert_eq!(rendered("13:05Z"), "1900-01-01 13:05:00 +00:00");

    for literal in [
        "2000-01-02 13:05:06-0530",
        "2000-01-02 13:05:06 +02",
        "2000-01-02 13:05:06z",
    ] {
        let error = cast_text(literal, SqlType::DateTimeOffset(0)).unwrap_err();
        assert_eq!(error.number, 241, "{literal}");
    }
}

/// The two hours a 12-hour clock numbers 12.
#[test]
fn noon_and_midnight_on_a_twelve_hour_clock() {
    let rendered = |literal: &str| match cast_text(literal, SqlType::DateTime2(0)) {
        Ok(v) => default_display(&v, &ti(SqlType::DateTime2(0))),
        Err(e) => panic!("{literal}: {e:?}"),
    };
    assert_eq!(rendered("12 AM"), "1900-01-01 00:00:00");
    assert_eq!(rendered("12 PM"), "1900-01-01 12:00:00");
    assert_eq!(rendered("12:30 PM"), "1900-01-01 12:30:00");
    assert_eq!(rendered("12:30 AM"), "1900-01-01 00:30:00");
}

/// More literal forms, each vector rendered as read: a clock without seconds, one-digit
/// minutes and seconds, the ISO order with slashes, one-digit parts, a year before a month
/// name, a comma without a space, surrounding spaces, a year on one digit, a month name
/// with a short year, and the month names in full and abbreviated.
#[test]
fn more_literal_forms() {
    let as_date = |literal: &str| match cast_text(literal, SqlType::Date) {
        Ok(v) => default_display(&v, &ti(SqlType::Date)),
        Err(e) => panic!("{literal}: {e:?}"),
    };
    let as_datetime2 = |literal: &str| match cast_text(literal, SqlType::DateTime2(0)) {
        Ok(v) => default_display(&v, &ti(SqlType::DateTime2(0))),
        Err(e) => panic!("{literal}: {e:?}"),
    };

    assert_eq!(as_datetime2("13:05"), "1900-01-01 13:05:00");
    assert_eq!(as_datetime2("2000-01-02 13:05"), "2000-01-02 13:05:00");
    assert_eq!(as_datetime2("13:5"), "1900-01-01 13:05:00");
    assert_eq!(as_datetime2("13:05:6"), "1900-01-01 13:05:06");
    assert_eq!(as_datetime2("   "), "1900-01-01 00:00:00");

    for literal in [
        "2000/01/02",
        "2000-1-2",
        "1/2/2000",
        "2000 Jan 2",
        "Jan 2,2000",
        "  2000-01-02  ",
    ] {
        assert_eq!(as_date(literal), "2000-01-02", "{literal}");
    }
    assert_eq!(as_date("2000 Jan"), "2000-01-01");
    assert_eq!(as_date("01/02/9"), "2009-01-02");
    assert_eq!(as_date("Jan 2 9"), "2009-01-02");
    assert_eq!(as_date("Jan 2 49"), "2049-01-02");
    assert_eq!(
        as_datetime2("  2000-01-02 13:05:06  "),
        "2000-01-02 13:05:06"
    );
    assert_eq!(as_datetime2("2000-01-02\t13:05:06"), "2000-01-02 13:05:06");

    // The twelve months, abbreviated and in full, all on the 2nd day.
    let months = [
        ("Jan", "January"),
        ("Feb", "February"),
        ("Mar", "March"),
        ("Apr", "April"),
        ("May", "May"),
        ("Jun", "June"),
        ("Jul", "July"),
        ("Aug", "August"),
        ("Sep", "September"),
        ("Oct", "October"),
        ("Nov", "November"),
        ("Dec", "December"),
    ];
    for (index, (short, full)) in months.iter().enumerate() {
        let expected = format!("2000-{:02}-02", index + 1);
        assert_eq!(as_date(&format!("{short} 2 2000")), expected);
        assert_eq!(as_date(&format!("{full} 2 2000")), expected);
    }
}

/// The strings that are **not** dates: a lower-case ISO `t`, mixed separators, a trailing
/// separator, a numeric date separated by spaces and a year on three digits.
#[test]
fn literal_forms_that_are_refused() {
    for literal in [
        "2000-01-02t13:05:06",
        "01-02.2000",
        "2000-01-02-",
        "01 02 2000",
        "01/02/999",
    ] {
        let error = cast_text(literal, SqlType::Date).unwrap_err();
        assert_eq!(error.number, 241, "{literal}");
        assert_eq!(
            error.message, "The character string could not be converted to a date or time.",
            "{literal}"
        );
    }
}

/// A fraction longer than a `datetime2(7)` holds is rounded on its eighth digit, halves
/// upwards, and the rounding carries into the next day.
#[test]
fn a_fraction_longer_than_seven_digits_is_rounded() {
    let at_scale_7 = |literal: &str| match cast_text(literal, SqlType::DateTime2(7)) {
        Ok(v) => default_display(&v, &ti(SqlType::DateTime2(7))),
        Err(e) => panic!("{literal}: {e:?}"),
    };
    assert_eq!(
        at_scale_7("2000-01-02 13:05:06.12345678"),
        "2000-01-02 13:05:06.1234568"
    );
    assert_eq!(
        at_scale_7("2000-01-02 13:05:06.12345675"),
        "2000-01-02 13:05:06.1234568"
    );
    assert_eq!(
        at_scale_7("2000-01-02 13:05:06.12345674"),
        "2000-01-02 13:05:06.1234567"
    );
    assert_eq!(
        at_scale_7("2000-01-02 13:05:06.123456789"),
        "2000-01-02 13:05:06.1234568"
    );
    assert_eq!(
        at_scale_7("2000-01-02 23:59:59.99999996"),
        "2000-01-03 00:00:00.0000000"
    );
}

/// A rounding that reaches midnight carries into the next day everywhere but on the last day
/// the calendar holds, where it stays on the last instant the scale can spell — and a `time`,
/// which has no next day, never wraps round to midnight either.
#[test]
fn rounding_stops_at_the_top_of_the_calendar() {
    let last_day = "9999-12-31 23:59:59.9999";
    let show = |literal: &str, to: SqlType| match cast_text(literal, to) {
        Ok(v) => default_display(&v, &ti(to)),
        Err(e) => format!("error {}", e.number),
    };
    assert_eq!(
        show(last_day, SqlType::DateTime2(3)),
        "9999-12-31 23:59:59.999"
    );
    assert_eq!(show(last_day, SqlType::DateTime2(0)), "9999-12-31 23:59:59");
    // The same fraction one year earlier does carry, so the rounding itself is unchanged.
    assert_eq!(
        show("2000-01-01 23:59:59.9999", SqlType::DateTime2(3)),
        "2000-01-02 00:00:00.000"
    );
    assert_eq!(show("23:59:59.9996", SqlType::Time(3)), "23:59:59.999");

    let midnight_of_a_time_7 = Value::Time(Time {
        ticks_100ns: 24 * 3600 * TICKS_PER_SECOND - 1,
    });
    let reduced = cast(&midnight_of_a_time_7, SqlType::Time(7), SqlType::Time(3)).unwrap();
    assert_eq!(
        default_display(&reduced, &ti(SqlType::Time(3))),
        "23:59:59.999"
    );
}

/// A fraction that rounds past midnight is
/// carried by the targets that hold a date **and** a time, and by no other. A `date` keeps
/// the day that was written and a `time` keeps the last instant its scale can spell — the
/// same literal, read three ways, lands on three different answers.
#[test]
fn fraction_carry_stops_at_a_date_or_a_time() {
    let literal = "2000-01-02T23:59:59.99999995";
    let show = |to: SqlType| match cast_text(literal, to) {
        Ok(v) => default_display(&v, &ti(to)),
        Err(e) => format!("error {}", e.number),
    };
    assert_eq!(show(SqlType::Date), "2000-01-02");
    assert_eq!(show(SqlType::Time(7)), "23:59:59.9999999");
    assert_eq!(show(SqlType::Time(3)), "23:59:59.999");
    assert_eq!(show(SqlType::Time(0)), "23:59:59");
    assert_eq!(show(SqlType::DateTime2(7)), "2000-01-03 00:00:00.0000000");
    // The carry itself is unchanged where the target holds both halves, and it still stops
    // at the top of the calendar rather than falling off it.
    assert_eq!(
        cast_text("9999-12-31T23:59:59.99999995", SqlType::Date)
            .map(|v| default_display(&v, &ti(SqlType::Date))),
        Ok("9999-12-31".to_owned())
    );
    assert_eq!(
        cast_text("9999-12-31T23:59:59.99999995", SqlType::DateTime2(7))
            .map(|v| default_display(&v, &ti(SqlType::DateTime2(7)))),
        Ok("9999-12-31 23:59:59.9999999".to_owned())
    );
    // A fraction that does not reach midnight is untouched by the rule.
    assert_eq!(
        cast_text("2000-01-02T23:59:59.99999994", SqlType::Date)
            .map(|v| default_display(&v, &ti(SqlType::Date))),
        Ok("2000-01-02".to_owned())
    );
}

/// A month and a
/// day are written on one or two digits and on no more, where the year takes a third width
/// of four. The neighbours matter as much as the rule: the same dates on one and on two
/// digits are read.
#[test]
fn month_or_day_wider_than_two_digits_is_241() {
    for literal in [
        "2000-01-002",
        "2000-001-02",
        "001/02/2000",
        "01.002.2000",
        "Jan 002 2000",
        "002 Jan 2000",
        "2000-01-00002",
        "01/0002/2000",
    ] {
        let error = cast_text(literal, SqlType::Date).unwrap_err();
        assert_eq!(error.number, 241, "{literal}");
    }
    let second_of_january = Value::Date(Date { days: 730_120 });
    for literal in [
        "2000-1-2",
        "2000-01-02",
        "1/2/2000",
        "01/02/2000",
        "Jan 02 2000",
        "Jan 2 2000",
        "02 Jan 2000",
    ] {
        assert_eq!(
            cast_text(literal, SqlType::Date),
            Ok(second_of_january.clone()),
            "{literal}"
        );
    }
}

/// A blank in **front** shuts the ISO 8601 grammar and shuts nothing else. What is left is
/// read by the loose grammar, which refuses a `T`, a lone `Z` and an offset with no clock.
#[test]
fn a_leading_blank_shuts_the_iso_grammar() {
    for literal in [
        " 2000-01-02T13:05:06",
        "  2000-01-02T13:05:06",
        "\t2000-01-02T13:05:06",
        " 2000-01-02Z",
        " 2000-01-02+02:00",
        " 2000-1-2T13:05:06",
    ] {
        let error = cast_text(literal, SqlType::DateTime2(0)).unwrap_err();
        assert_eq!(error.number, 241, "{literal:?}");
    }
    // Without the blank, behind the blank, and in front of a form the loose grammar reads:
    // all three are read.
    for (literal, shown) in [
        ("2000-01-02T13:05:06", "2000-01-02 13:05:06"),
        ("2000-01-02T13:05:06 ", "2000-01-02 13:05:06"),
        (" 2000-01-02 13:05:06", "2000-01-02 13:05:06"),
        (" 2000-01-02", "2000-01-02 00:00:00"),
        (" 20000102", "2000-01-02 00:00:00"),
        (" Jan 2 2000", "2000-01-02 00:00:00"),
    ] {
        let value = cast_text(literal, SqlType::DateTime2(0)).unwrap();
        assert_eq!(
            default_display(&value, &ti(SqlType::DateTime2(0))),
            shown,
            "{literal:?}"
        );
    }
}

/// A meridiem marker written on its own borrows its hour from the last number of the token
/// before it, but only when that token used no date separator. The same hour one blank
/// further is read, and a separator carried by an **earlier** token changes nothing, which
/// is what tells the rule from "no separator anywhere".
#[test]
fn meridiem_hour_never_comes_from_a_token_that_used_a_separator() {
    let rendered = |literal: &str| match cast_text(literal, SqlType::DateTime2(0)) {
        Ok(v) => default_display(&v, &ti(SqlType::DateTime2(0))),
        Err(e) => panic!("{literal}: {e:?}"),
    };
    for literal in [
        "2000-1 PM",
        "2000-01 PM",
        "2000/01 PM",
        "2000.01 PM",
        "20000102-5 PM",
        "20000102/5 PM",
        "20000102.5 PM",
        "000102-5 PM",
        "2000-5PM",
    ] {
        let error = cast_text(literal, SqlType::DateTime2(0)).unwrap_err();
        assert_eq!(error.number, 241, "{literal}");
    }
    // One blank instead of the separator, and the hour is read again.
    assert_eq!(rendered("2000 1 PM"), "2000-01-01 13:00:00");
    assert_eq!(rendered("2000 01 PM"), "2000-01-01 13:00:00");
    assert_eq!(rendered("20000102 5 PM"), "2000-01-02 17:00:00");
    assert_eq!(rendered("000102 5PM"), "2000-01-02 17:00:00");
    // A separator in an earlier token is no obstacle, and neither is a month name glued to
    // the hour inside the lending token.
    assert_eq!(rendered("2000-01-02 5 PM"), "2000-01-02 17:00:00");
    assert_eq!(rendered("01/02/2000 5 PM"), "2000-01-02 17:00:00");
    assert_eq!(rendered("2000-Jan-2 1 PM"), "2000-01-02 13:00:00");
    assert_eq!(rendered("2000Jan1 PM"), "2000-01-01 13:00:00");
    assert_eq!(rendered("2000 Jan1 PM"), "2000-01-01 13:00:00");
}

/// `<number><sep><month name><sep><number>` is the one spelling whose day goes past two
/// digits, and it stops at nine. The same date written with blanks stops at two, and the
/// tenth digit is error 241 on both, which is the point of testing all three.
#[test]
fn a_glued_month_name_writes_its_day_on_up_to_nine_digits() {
    let second_of_january = Value::Date(Date { days: 730_120 });
    for literal in [
        "2000-Jan-2",
        "2000-Jan-02",
        "2000-Jan-002",
        "2000-Jan-000002",
        "2000-Jan-000000002",
        "2/Jan/2000",
        "00002-Jan-2000",
        "000000002-Jan-2000",
        "2000/JANUARY/000002",
        "2000.Jan.0002",
    ] {
        assert_eq!(
            cast_text(literal, SqlType::Date),
            Ok(second_of_january.clone()),
            "{literal}"
        );
    }
    // A tenth digit closes the spelling, the width is still checked where the pieces are
    // blank-separated, and the day is still a day: `2000-Jan-32` is no date at all.
    for literal in [
        "2000-Jan-0000000002",
        "0000000002-Jan-2000",
        "Jan 002 2000",
        "002 Jan 2000",
        "2000-Jan-32",
        "2000-Jan-0032",
    ] {
        let error = cast_text(literal, SqlType::Date).unwrap_err();
        assert_eq!(error.number, 241, "{literal}");
    }
}

/// The range of a `datetimeoffset` bears on the instant it stores, not on the local
/// reading. The same string that is a readable `datetime2` is no `datetimeoffset` at all,
/// and the answer is 8114 there, not 242.
#[test]
fn datetimeoffset_range_is_on_the_universal_instant() {
    let error = cast_text("0001-01-01T01:59:59+02:00", SqlType::DateTimeOffset(7)).unwrap_err();
    assert_eq!(error.number, 8114);
    assert_eq!(
        error.message,
        "Data type varchar could not be converted to datetimeoffset."
    );
    // The same reading is a `datetime2` without a murmur.
    assert!(cast_text("0001-01-01T01:59:59+02:00", SqlType::DateTime2(7)).is_ok());
    // Two hours later at the bottom, two hours earlier at the top: both are read.
    for (literal, shown) in [
        (
            "0001-01-01T02:00:00+02:00",
            "0001-01-01 02:00:00.0000000 +02:00",
        ),
        (
            "9999-12-31T21:59:59-02:00",
            "9999-12-31 21:59:59.0000000 -02:00",
        ),
    ] {
        let value = cast_text(literal, SqlType::DateTimeOffset(7)).unwrap();
        assert_eq!(
            default_display(&value, &ti(SqlType::DateTimeOffset(7))),
            shown
        );
    }
    let error = cast_text("9999-12-31T22:00:00-02:00", SqlType::DateTimeOffset(7)).unwrap_err();
    assert_eq!(error.number, 8114);
}

// ---------------------------------------------------------------------------------------
// The styles of `CONVERT`.
// ---------------------------------------------------------------------------------------

/// `CONVERT(varchar(40), <value>, <style>)`.
fn convert_to_varchar(v: &Value, from: SqlType, style: i32) -> vauban_errors::SqlResult<String> {
    match convert(v, &ti(from), &varchar(), Some(style)) {
        Ok(Value::String(s)) => Ok(s.text),
        Ok(other) => panic!("a character target gave {other:?}"),
        Err(e) => Err(e),
    }
}

/// `CONVERT(<type>, '<string>', <style>)`.
fn convert_with_style(s: &str, to: SqlType, style: i32) -> vauban_errors::SqlResult<Value> {
    convert(&text(s), &varchar(), &ti(to), Some(style))
}

/// Ten styles on the `datetime` of 2000-01-02 13:05:06.123.
#[test]
fn date_to_string_styles() {
    let Ok(dt) = cast_text("2000-01-02 13:05:06.123", SqlType::DateTime) else {
        panic!("the literal is a datetime");
    };
    for (style, expected) in [
        (0, "Jan  2 2000  1:05PM"),
        (101, "01/02/2000"),
        (103, "02/01/2000"),
        (112, "20000102"),
        (120, "2000-01-02 13:05:06"),
        (121, "2000-01-02 13:05:06.123"),
        (126, "2000-01-02T13:05:06.123"),
        (23, "2000-01-02"),
        (108, "13:05:06"),
        (1, "01/02/00"),
    ] {
        assert_eq!(
            convert_to_varchar(&dt, SqlType::DateTime, style).as_deref(),
            Ok(expected),
            "style {style}"
        );
    }
}

/// The **order** of the reading path: "is this style supported for this pair?" (9809)
/// comes before "does the string follow the strict style?" (9807), and a supported style
/// that the string simply does not denote is the plain 241.
///
/// Over the 256 styles of two strings:
///
/// * `CONVERT(date, 'zzz', N)` — a string that is no date at all — answers **241** for 42
///   styles (0..=14, 20..=25, 100..=114, 120, 121, 126, 127, 130, 131) and **9809** for the
///   other **214**. Not one answers 9807: the string is never read under an unsupported
///   style.
/// * `CONVERT(date, '2000-01-02', N)` — a well-formed date in the ISO shape — answers the
///   very same **214** times 9809, **9807** for the seventeen **strict** styles 6, 7, 8, 9,
///   12, 13, 14, 24, 100, 106, 107, 108, 109, 112, 113, 114 and 130, 241 for fourteen more
///   and reads the date under the remaining eleven.
///
/// The second string is the vector that separates the two orders: style 200 on that date is
/// 9809 and style 112 on the same date is 9807, so a reading that judged the shape first
/// would answer 9807 to both.
#[test]
fn an_unsupported_style_is_judged_before_the_string() {
    // The two Hijri styles are not implemented and answer the internal error; they are
    // among the 42 and among the seventeen, and counted apart here.
    const HIJRI: [i32; 2] = [130, 131];
    const LENIENT_FOR_ZZZ: [i32; 42] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 20, 21, 22, 23, 24, 25, 100, 101, 102,
        103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 120, 121, 126, 127, 130, 131,
    ];
    const STRICT: [i32; 17] = [
        6, 7, 8, 9, 12, 13, 14, 24, 100, 106, 107, 108, 109, 112, 113, 114, 130,
    ];

    let (mut lenient, mut unsupported) = (0, 0);
    for style in 0..=255 {
        if HIJRI.contains(&style) {
            continue;
        }
        let number = convert_with_style("zzz", SqlType::Date, style).map_err(|e| e.number);
        if LENIENT_FOR_ZZZ.contains(&style) {
            assert_eq!(number, Err(241), "style {style} on 'zzz'");
            lenient += 1;
        } else {
            assert_eq!(number, Err(9809), "style {style} on 'zzz'");
            unsupported += 1;
        }
    }
    assert_eq!((lenient, unsupported), (40, 214));

    let (mut strict, mut unsupported) = (0, 0);
    for style in 0..=255 {
        if HIJRI.contains(&style) {
            continue;
        }
        let answer = convert_with_style("2000-01-02", SqlType::Date, style).map_err(|e| e.number);
        if STRICT.contains(&style) {
            assert_eq!(answer, Err(9807), "style {style} on '2000-01-02'");
            strict += 1;
        } else if !LENIENT_FOR_ZZZ.contains(&style) {
            assert_eq!(answer, Err(9809), "style {style} on '2000-01-02'");
            unsupported += 1;
        }
    }
    assert_eq!((strict, unsupported), (16, 214));

    // Three vectors side by side: the same date, two style numbers, two errors.
    assert_eq!(
        convert_with_style("2000-01-02", SqlType::Date, 200).map_err(|e| e.number),
        Err(9809)
    );
    assert_eq!(
        convert_with_style("2000-01-02", SqlType::Date, 112).map_err(|e| e.number),
        Err(9807)
    );
    assert_eq!(
        convert_with_style("zzz", SqlType::Date, 100).map_err(|e| e.number),
        Err(241)
    );
}

/// A string the style does not describe is error **9807** and not 241 (`CONVERT(date,
/// '01/02/2000', 112)`), and 241 stays for a string that follows the style and denotes
/// nothing (`'2000-01-02'` read as a month-day-year date).
#[test]
fn style_forces_format() {
    let day = |y: i32, m: u8, d: u8| {
        Ok(Value::Date(Date {
            days: days_from_civil(y, m, d),
        }))
    };
    assert_eq!(
        convert_with_style("01/02/2000", SqlType::Date, 103),
        day(2000, 2, 1)
    );
    assert_eq!(
        convert_with_style("01/02/2000", SqlType::Date, 101),
        day(2000, 1, 2)
    );
    assert_eq!(
        convert_with_style("20000102", SqlType::Date, 112),
        day(2000, 1, 2)
    );
    // `'01/02/2000'` is not written the way style 112 writes a date: 9807, state 0
    // (`SqlError::input_does_not_follow_style`).
    let refused = convert_with_style("01/02/2000", SqlType::Date, 112);
    let Err(e) = refused else {
        panic!("style 112 refuses a separated date")
    };
    assert_eq!(e.number, 9807);
    assert_eq!(e.severity, 16);
    assert_eq!(e.state, 0);
    assert_eq!(
        e.message,
        "The input string does not match style 112; change the string or the style."
    );
    // A string that *is* written the way the style writes one, and yet denotes no date, is
    // the plain 241: `2000` is no month.
    assert_eq!(
        convert_with_style("2000-01-02", SqlType::Date, 101).map_err(|e| e.number),
        Err(241)
    );
}

/// The pivot of a style with no century is the pivot of the style-less grammar.
#[test]
fn two_digit_year_pivots_at_2049() {
    assert_eq!(
        convert_with_style("01/02/49", SqlType::Date, 1),
        Ok(Value::Date(Date {
            days: days_from_civil(2049, 1, 2)
        }))
    );
    assert_eq!(
        convert_with_style("01/02/50", SqlType::Date, 1),
        Ok(Value::Date(Date {
            days: days_from_civil(1950, 1, 2)
        }))
    );
    // The neighbour that tells the rule from "the style decides the century": the same two
    // years written for a four-digit style are error 241.
    assert_eq!(
        convert_with_style("01/02/49", SqlType::Date, 101).map_err(|e| e.number),
        Err(241)
    );
}

/// The milliseconds a `datetime` can hold are multiples of 1/300 s, and style 121 shows
/// them rounded, never truncated.
#[test]
fn datetime_rounding_at_style_121() {
    for (literal, expected) in [
        ("2000-01-01 00:00:00.999", "2000-01-01 00:00:01.000"),
        ("2000-01-01 00:00:00.998", "2000-01-01 00:00:00.997"),
        ("2000-01-01 00:00:00.997", "2000-01-01 00:00:00.997"),
    ] {
        let Ok(dt) = cast_text(literal, SqlType::DateTime) else {
            panic!("{literal} is a datetime");
        };
        assert_eq!(
            convert_to_varchar(&dt, SqlType::DateTime, 121).as_deref(),
            Ok(expected),
            "{literal}"
        );
    }
}

/// The type that refuses a style is a `datetimeoffset` **source** read as another date
/// type, not a `datetimeoffset` target. `CONVERT(datetimeoffset, '2000-01-02', 112)` is a
/// plain 9807 (the string does not follow style 112), while
/// `CONVERT(date, <datetimeoffset>, 112)` is error 9809.
///
/// The two styles it does take are asserted on their **answer**, not on their acceptance:
/// `0` gives the local reading, `1` gives the universal instant, and they part company by a
/// whole day as soon as the offset crosses midnight.
#[test]
fn datetimeoffset_rejects_other_styles() {
    let Ok(dto) = cast_text("2000-01-02 13:05:06 +02:00", SqlType::DateTimeOffset(7)) else {
        panic!("the literal is a datetimeoffset");
    };
    for style in [2, 101, 103, 112, 120, 121, 126] {
        let refused = convert(
            &dto,
            &ti(SqlType::DateTimeOffset(7)),
            &ti(SqlType::DateTime2(3)),
            Some(style),
        );
        let Err(e) = refused else {
            panic!("style {style} is refused from a datetimeoffset")
        };
        assert_eq!(e.number, 9809, "style {style}");
        assert_eq!(
            e.message,
            format!("Style {style} is not defined for converting datetimeoffset to datetime2.")
        );
    }
    // Styles 0 and 1, and only those two, are accepted — and they do **not** answer the same
    // thing, which is the whole point of there being two: style 0 reads the local time a
    // client sees, style 1 reads the universal instant the value stores. Asserting `is_ok()`
    // here would test the acceptance and never the answer.
    let read = |style| {
        let read = convert(
            &dto,
            &ti(SqlType::DateTimeOffset(7)),
            &ti(SqlType::DateTime2(3)),
            Some(style),
        );
        match read {
            Ok(v) => convert_to_varchar(&v, SqlType::DateTime2(3), 121),
            Err(e) => Err(e),
        }
    };
    assert_eq!(read(0).as_deref(), Ok("2000-01-02 13:05:06.000"));
    assert_eq!(read(1).as_deref(), Ok("2000-01-02 11:05:06.000"));
    // The offset that makes the two readings fall on different **days**: an early morning at
    // +02:00 is the evening before, in UTC. Towards a `date` the divergence is the day itself.
    let Ok(dawn) = cast_text("2000-01-02 01:05:06 +02:00", SqlType::DateTimeOffset(7)) else {
        panic!("the literal is a datetimeoffset");
    };
    let day = |style| {
        let read = convert(
            &dawn,
            &ti(SqlType::DateTimeOffset(7)),
            &ti(SqlType::Date),
            Some(style),
        );
        match read {
            Ok(v) => convert_to_varchar(&v, SqlType::Date, 112),
            Err(e) => Err(e),
        }
    };
    assert_eq!(day(0).as_deref(), Ok("20000102"));
    assert_eq!(day(1).as_deref(), Ok("20000101"));
    // No style at all is the reading of style 0, not of style 1.
    let Ok(plain) = convert(
        &dawn,
        &ti(SqlType::DateTimeOffset(7)),
        &ti(SqlType::Date),
        None,
    ) else {
        panic!("a datetimeoffset reads as a date without a style")
    };
    assert_eq!(
        convert_to_varchar(&plain, SqlType::Date, 112).as_deref(),
        Ok("20000102")
    );
    // A `datetimeoffset` **target** takes every style a `date` takes, and towards a
    // character type every style is rendered.
    assert!(convert_with_style("2000-01-02", SqlType::DateTimeOffset(0), 0).is_ok());
    assert!(convert_to_varchar(&dto, SqlType::DateTimeOffset(7), 112).is_ok());
}

/// A style number that does not exist is error 281, not a fallback to style 0: every
/// style of `{15..=19, 36..=99, 116..=119, 122..=125, 128, 129, 132…}` and every negative
/// one.
///
/// The message names the **source** type and calls the target "a character string"
/// whatever it is (`SqlError::invalid_style_number`).
#[test]
fn unknown_style_is_281_and_never_style_zero() {
    let Ok(dt) = cast_text("2000-01-02 13:05:06.123", SqlType::DateTime) else {
        panic!("the literal is a datetime");
    };
    let zero = convert_to_varchar(&dt, SqlType::DateTime, 0);
    assert_eq!(zero.as_deref(), Ok("Jan  2 2000  1:05PM"));
    for style in [-1, 15, 19, 36, 77, 99, 116, 125, 128, 132, 200, 1000] {
        let Err(e) = convert_to_varchar(&dt, SqlType::DateTime, style) else {
            panic!("style {style} does not exist")
        };
        assert_eq!(e.number, 281, "style {style}");
        assert_eq!(e.severity, 16, "style {style}");
        assert_eq!(e.state, 1, "style {style}");
        assert_eq!(
            e.message,
            format!("Style {style} is not defined for converting datetime to a character string."),
            "style {style}"
        );
    }
    // The neighbours: the numbers just inside the ranges that do exist.
    for style in [14, 20, 35, 100, 115, 121, 127] {
        assert!(
            convert_to_varchar(&dt, SqlType::DateTime, style).is_ok(),
            "style {style}"
        );
    }
    // Towards a date type the same number is 9809, which the catalogue does carry.
    assert_eq!(
        convert_with_style("2000-01-02", SqlType::Date, 77).map_err(|e| e.number),
        Err(9809)
    );
}

/// A style that writes only a clock is refused by a `date`, and a style that writes only a
/// date by a `time` — but the two refusals are not the same error, and style 115 is refused
/// by neither the way its form suggests.
#[test]
fn a_style_a_type_cannot_fill_is_refused() {
    let Ok(date) = cast_text("2000-01-02", SqlType::Date) else {
        panic!("the literal is a date")
    };
    let Ok(time) = cast_text("13:05:06.1234567", SqlType::Time(7)) else {
        panic!("the literal is a time")
    };
    for style in [8, 24, 108] {
        let Err(e) = convert_to_varchar(&date, SqlType::Date, style) else {
            panic!("a date has no clock for style {style}")
        };
        assert_eq!(e.number, 8114, "style {style}");
        assert_eq!(
            e.message,
            "Data type date could not be converted to varchar."
        );
    }
    for style in [14, 114] {
        let Err(e) = convert_to_varchar(&date, SqlType::Date, style) else {
            panic!("style {style} is no style for a date")
        };
        assert_eq!(e.number, 281, "style {style}");
        assert_eq!(
            e.message,
            format!("Style {style} is not defined for converting date to a character string."),
            "style {style}"
        );
    }
    for style in [1, 12, 23, 101, 112, 115] {
        let Err(e) = convert_to_varchar(&time, SqlType::Time(7), style) else {
            panic!("a time has no date for style {style}")
        };
        assert_eq!(e.number, 8114, "style {style}");
    }
    // Style 115 writes `hhmmss`, and a `date` writes it with the zeros it does not hold.
    assert_eq!(
        convert_to_varchar(&date, SqlType::Date, 115).as_deref(),
        Ok("000000")
    );
    // Style 0 of a `date` is the `datetime` form without its clock, not the ISO default.
    assert_eq!(
        convert_to_varchar(&date, SqlType::Date, 0).as_deref(),
        Ok("Jan  2 2000")
    );
    assert_eq!(default_display(&date, &ti(SqlType::Date)), "2000-01-02");
}

/// A style says nothing about a source that is not a character string: the conversion is the
/// style-less one, unknown style numbers included.
#[test]
fn a_style_is_ignored_when_the_source_is_not_text() {
    let Ok(dt2) = cast_text("2000-01-02 13:05:06", SqlType::DateTime2(7)) else {
        panic!("the literal is a datetime2")
    };
    for style in [1, 77, 103, 112, 200] {
        assert_eq!(
            convert(
                &dt2,
                &ti(SqlType::DateTime2(7)),
                &ti(SqlType::Date),
                Some(style)
            ),
            Ok(Value::Date(Date {
                days: days_from_civil(2000, 1, 2)
            })),
            "style {style}"
        );
    }
}

/// The one shape a style judges is an all-numeric date written in a single token: the ISO
/// 8601 `T` grammar, a compact `20000102`, a month name and a clock alone are read whatever
/// the style says.
#[test]
fn a_style_only_judges_a_numeric_date() {
    for text in [
        "2000-01-02T13:05:06",
        "20000102",
        "02 Jan 2000",
        "2000-Jan-02",
        "13:05:06",
        "",
    ] {
        assert!(
            convert_with_style(text, SqlType::DateTime2(3), 112).is_ok()
                && convert_with_style(text, SqlType::DateTime2(3), 6).is_ok(),
            "{text}"
        );
    }
    // The neighbour that makes the rule a rule: the same date with a blank in front, which
    // shuts the ISO grammar, is judged by the style again.
    assert!(convert_with_style(" 2000-01-02", SqlType::DateTime2(3), 6).is_err());
    assert!(convert_with_style(" 2000-01-02", SqlType::DateTime2(3), 121).is_ok());
    // Three numbers spread over three tokens are not one date: 241 under every style,
    // never 9807.
    assert_eq!(
        convert_with_style("01 02 2000", SqlType::Date, 112).map_err(|e| e.number),
        Err(241)
    );
}

/// 1/300 s ticks in one day, the unit of [`DateTime::ticks_300th`].
const TICKS_300TH_PER_DAY: u32 = 25_920_000;

/// 1/300 s ticks in one minute, the step of a `smalldatetime`.
const TICKS_300TH_PER_MINUTE: u32 = 300 * 60;

/// `CAST(<decimal(38, scale)> AS <to>)`.
fn cast_decimal(mantissa: i128, scale: u8, to: SqlType) -> vauban_errors::SqlResult<Value> {
    let v = Value::Decimal(Decimal {
        mantissa,
        precision: 38,
        scale,
    });
    cast(
        &v,
        SqlType::Decimal {
            precision: 38,
            scale,
        },
        to,
    )
}

/// `CAST(<float> AS <to>)`.
fn cast_float(f: f64, to: SqlType) -> vauban_errors::SqlResult<Value> {
    cast(&Value::F64(f), SqlType::Float, to)
}

/// `CAST(<varbinary> AS <to>)`.
fn cast_bytes(bytes: &[u8], to: SqlType) -> vauban_errors::SqlResult<Value> {
    cast(
        &Value::Bytes(bytes.to_vec()),
        SqlType::VarBinary(Len::Fixed(8)),
        to,
    )
}

/// A `datetime` from its two stored halves.
fn moment(days: i32, ticks_300th: u32) -> Value {
    Value::DateTime(DateTime { days, ticks_300th })
}

/// The integer part counts days from
/// 1900-01-01, the fractional part the fraction of a day.
#[test]
fn number_to_datetime_counts_days_from_1900() {
    assert_eq!(
        cast(&Value::I32(0), SqlType::Int, SqlType::DateTime),
        Ok(moment(0, 0))
    );
    assert_eq!(
        cast(&Value::I32(1), SqlType::Int, SqlType::DateTime),
        Ok(moment(1, 0))
    );
    // 36 524 days is the 1st of January 2000.
    assert_eq!(
        cast(&Value::I32(36_524), SqlType::Int, SqlType::DateTime),
        Ok(moment(36_524, 0))
    );
    // 1.5 is noon on the 2nd of January 1900, 1.25 is six in the morning.
    assert_eq!(
        cast_decimal(1_500_000, 6, SqlType::DateTime),
        Ok(moment(1, TICKS_300TH_PER_DAY / 2))
    );
    assert_eq!(
        cast_decimal(1_250_000, 6, SqlType::DateTime),
        Ok(moment(1, TICKS_300TH_PER_DAY / 4))
    );
}

/// Each numeric type reads the same number the same way. `bit` is the only one that
/// answers differently, and it does so before the
/// date conversion runs: `CAST(1.5 AS bit)` is already 1.
#[test]
fn number_to_datetime_reads_every_numeric_type_alike() {
    let noon = Ok(moment(1, TICKS_300TH_PER_DAY / 2));
    assert_eq!(cast_float(1.5, SqlType::DateTime), noon);
    assert_eq!(
        cast(&Value::F32(1.5), SqlType::Real, SqlType::DateTime),
        noon
    );
    // `money` and `smallmoney` hold ten-thousandths: 1.5 is 15 000 of them.
    assert_eq!(
        cast(&Value::Money(15_000), SqlType::Money, SqlType::DateTime),
        noon
    );
    assert_eq!(
        cast(
            &Value::Money(15_000),
            SqlType::SmallMoney,
            SqlType::DateTime
        ),
        noon
    );
    assert_eq!(cast_decimal(1_500_000, 6, SqlType::DateTime), noon);
    // The integer types truncate before they ever reach a date.
    assert_eq!(
        cast(&Value::I64(1), SqlType::BigInt, SqlType::DateTime),
        Ok(moment(1, 0))
    );
    assert_eq!(
        cast(&Value::I16(1), SqlType::SmallInt, SqlType::DateTime),
        Ok(moment(1, 0))
    );
    assert_eq!(
        cast(&Value::I8(1), SqlType::TinyInt, SqlType::DateTime),
        Ok(moment(1, 0))
    );
    assert_eq!(
        cast(&Value::Bit(true), SqlType::Bit, SqlType::DateTime),
        Ok(moment(1, 0))
    );
}

/// `smallint` on its own, like every other numeric type. Its two bounds are the vector no
/// other small type can write: `-32768` is
/// the 15th of April 1810, which exercises the path before 1900, and `32767` the 18th of
/// September 1989.
#[test]
fn number_to_datetime_reads_a_smallint() {
    assert_eq!(
        cast(&Value::I16(1), SqlType::SmallInt, SqlType::DateTime),
        Ok(moment(1, 0))
    );
    // The dates, tied to the day counts they come from.
    assert_eq!(
        cast(&Value::I16(i16::MIN), SqlType::SmallInt, SqlType::DateTime),
        Ok(moment(days_from_civil(1810, 4, 15) - DAYS_1900, 0))
    );
    assert_eq!(
        cast(&Value::I16(i16::MAX), SqlType::SmallInt, SqlType::DateTime),
        Ok(moment(days_from_civil(1989, 9, 18) - DAYS_1900, 0))
    );
    assert_eq!(
        cast(&Value::I16(1), SqlType::SmallInt, SqlType::SmallDateTime),
        Ok(moment(1, 0))
    );
    // A `smalldatetime` has no negative half: -1 overflows, with the 8115 of a number and
    // not the 242 of a date source.
    let e = cast(&Value::I16(-1), SqlType::SmallInt, SqlType::SmallDateTime)
        .expect_err("before the first day of a smalldatetime");
    assert_eq!(e.number, 8115);
    assert_eq!(e.state, 2);
    assert_eq!(
        e.message,
        "Converting expression to data type smalldatetime overflowed."
    );
    // And the four types of 2008 refuse it like the eleven other numeric types.
    let e = cast(&Value::I16(1), SqlType::SmallInt, SqlType::Date)
        .expect_err("a number is not a date source");
    assert_eq!(e.number, 529);
    assert_eq!(
        e.message,
        "No explicit conversion exists from smallint to date."
    );
}

/// The fraction lands on a tick of
/// 1/300 s and **rounds** rather than truncating. 0.0000001 day is 2.592 ticks and reads as
/// three, which a truncation would read as two.
#[test]
fn number_to_datetime_rounds_the_fraction() {
    assert_eq!(cast_decimal(1, 7, SqlType::DateTime), Ok(moment(0, 3)));
    // 0.000003125 day is exactly 81 ticks.
    assert_eq!(cast_decimal(3_125, 9, SqlType::DateTime), Ok(moment(0, 81)));
    // 0.99999 day is 25 919 740.8 ticks.
    assert_eq!(
        cast_decimal(99_999, 5, SqlType::DateTime),
        Ok(moment(0, 25_919_741))
    );
    // A `money` never rounds **towards a `datetime`**: a ten-thousandth of a day is
    // exactly 2 592 ticks. The clause stops at that target; see
    // `money_rounds_towards_a_smalldatetime` for the other one.
    assert_eq!(
        cast(&Value::Money(1), SqlType::Money, SqlType::DateTime),
        Ok(moment(0, 2_592))
    );
    assert_eq!(
        cast(&Value::Money(9_999), SqlType::Money, SqlType::DateTime),
        Ok(moment(0, 25_917_408))
    );
}

/// The same `money` amounts towards the other target
/// of this function. A `smalldatetime` counts 1 440 minutes in a day, so a ten-thousandth
/// of a day is 0.144 minute and a `money` rounds there like any other number — up as well
/// as down, which is what tells this rule apart from a truncation.
#[test]
fn money_rounds_towards_a_smalldatetime() {
    // 0.0004 day is 0.576 minute and rounds **up** to 00:01, where a truncation says 00:00.
    assert_eq!(
        cast(&Value::Money(4), SqlType::Money, SqlType::SmallDateTime),
        Ok(moment(0, TICKS_300TH_PER_MINUTE))
    );
    // 0.0003 day is 0.432 minute and rounds **down** to 00:00.
    assert_eq!(
        cast(&Value::Money(3), SqlType::Money, SqlType::SmallDateTime),
        Ok(moment(0, 0))
    );
    // 0.0035 day is 5.04 minutes, and 0.0034 day is 4.896: both land on 00:05, from either
    // side of the minute.
    assert_eq!(
        cast(&Value::Money(35), SqlType::Money, SqlType::SmallDateTime),
        Ok(moment(0, 5 * TICKS_300TH_PER_MINUTE))
    );
    assert_eq!(
        cast(&Value::Money(34), SqlType::Money, SqlType::SmallDateTime),
        Ok(moment(0, 5 * TICKS_300TH_PER_MINUTE))
    );
    // 1.0004 keeps the day and rounds the fraction the same way.
    assert_eq!(
        cast(
            &Value::Money(10_004),
            SqlType::Money,
            SqlType::SmallDateTime
        ),
        Ok(moment(1, TICKS_300TH_PER_MINUTE))
    );
    // The same three amounts towards a `datetime` are exact to the tick: 2 592 ticks a
    // ten-thousandth, and no rounding anywhere.
    assert_eq!(
        cast(&Value::Money(4), SqlType::Money, SqlType::DateTime),
        Ok(moment(0, 4 * 2_592))
    );
    assert_eq!(
        cast(&Value::Money(3), SqlType::Money, SqlType::DateTime),
        Ok(moment(0, 3 * 2_592))
    );
    assert_eq!(
        cast(&Value::Money(35), SqlType::Money, SqlType::DateTime),
        Ok(moment(0, 35 * 2_592))
    );
}

/// The twenty-four values whose double product falls **exactly** on half a tick, crossed
/// with `float` and `decimal`, read back through `CAST(<datetime> AS binary(8))` so that
/// the tick is exact rather than rounded to a millisecond.
///
/// This is the vector that tells the rule apart from its neighbour: the rounding is on
/// the **exact** value of the source, so the `decimal` — which holds the literal exactly —
/// always rounds up, while the `float` follows the double, which sits above the tie for
/// twelve of them and below for the other twelve. An implementation that multiplied the
/// double by 25 920 000 and rounded the product would answer `up` for all twenty-four and
/// fail half of them.
#[test]
fn number_to_datetime_decides_the_tie_on_the_exact_value() {
    const TIES: [(&str, i128, u32, u32); 24] = [
        ("0.0000015625", 15625000000000000000000_i128, 41, 41),
        ("0.0000078125", 78125000000000000000000_i128, 203, 203),
        ("0.0000109375", 109375000000000000000000_i128, 283, 284),
        ("0.0000140625", 140625000000000000000000_i128, 364, 365),
        ("0.0000171875", 171875000000000000000000_i128, 446, 446),
        ("0.0000203125", 203125000000000000000000_i128, 526, 527),
        ("0.0000234375", 234375000000000000000000_i128, 608, 608),
        ("0.0000265625", 265625000000000000000000_i128, 688, 689),
        ("0.0000296875", 296875000000000000000000_i128, 769, 770),
        ("0.0000328125", 328125000000000000000000_i128, 850, 851),
        ("0.0000359375", 359375000000000000000000_i128, 931, 932),
        ("0.0000390625", 390625000000000000000000_i128, 1013, 1013),
        ("0.0000421875", 421875000000000000000000_i128, 1094, 1094),
        ("0.0000453125", 453125000000000000000000_i128, 1174, 1175),
        ("0.0000484375", 484375000000000000000000_i128, 1255, 1256),
        ("0.0000515625", 515625000000000000000000_i128, 1336, 1337),
        ("0.0000546875", 546875000000000000000000_i128, 1418, 1418),
        ("0.0000578125", 578125000000000000000000_i128, 1499, 1499),
        ("0.0000609375", 609375000000000000000000_i128, 1579, 1580),
        ("0.0000671875", 671875000000000000000000_i128, 1742, 1742),
        ("0.0000734375", 734375000000000000000000_i128, 1904, 1904),
        ("0.0000765625", 765625000000000000000000_i128, 1984, 1985),
        ("0.0000796875", 796875000000000000000000_i128, 2066, 2066),
        ("0.0000828125", 828125000000000000000000_i128, 2147, 2147),
    ];
    for (literal, mantissa, from_float, from_decimal) in TIES {
        let f: f64 = literal.parse().expect("a literal of the table");
        assert_eq!(
            cast_float(f, SqlType::DateTime),
            Ok(moment(0, from_float)),
            "float {literal}"
        );
        assert_eq!(
            cast_decimal(mantissa, 28, SqlType::DateTime),
            Ok(moment(0, from_decimal)),
            "decimal {literal}"
        );
    }
}

/// The same rule seen from the
/// other side. A `decimal` one unit in the twenty-eighth place below half a tick rounds
/// down; the `float` written the same way rounds up, its double sitting above the tie.
#[test]
fn number_to_datetime_reads_an_exact_source_exactly() {
    // 0.0000015625 day is exactly 40.5 ticks.
    assert_eq!(
        cast_decimal(15_625_000_000_000_000_000_000, 28, SqlType::DateTime),
        Ok(moment(0, 41))
    );
    assert_eq!(
        cast_decimal(15_624_999_999_999_999_999_999, 28, SqlType::DateTime),
        Ok(moment(0, 40))
    );
    // The literal is written in full on purpose: the nearest double to it is the same as
    // the nearest double to `0.0000015625`, which sits above the tie, so the `float` rounds
    // up where the `decimal` above rounded down. Parsed rather than written as a literal,
    // which `clippy::excessive_precision` would shorten and so erase the point.
    let above: f64 = "0.0000015624999999999999999999"
        .parse()
        .expect("a well-formed double");
    assert_eq!(cast_float(above, SqlType::DateTime), Ok(moment(0, 41)));
}

/// A negative number runs backwards from the
/// origin while its fraction still runs forward, and the rounding is decided on the
/// **total**, not on the fraction of its own day (keys `Rd;-1` and `Rd;1`).
#[test]
fn number_to_datetime_before_1900() {
    assert_eq!(
        cast(&Value::I32(-1), SqlType::Int, SqlType::DateTime),
        Ok(moment(-1, 0))
    );
    // -0.5 is noon on the 31st of December 1899, -1.5 noon on the 30th.
    assert_eq!(
        cast_decimal(-500_000, 6, SqlType::DateTime),
        Ok(moment(-1, TICKS_300TH_PER_DAY / 2))
    );
    assert_eq!(
        cast_decimal(-1_500_000, 6, SqlType::DateTime),
        Ok(moment(-2, TICKS_300TH_PER_DAY / 2))
    );
    // 1753-01-01, the first day a `datetime` holds.
    assert_eq!(
        cast(&Value::I32(-53_690), SqlType::Int, SqlType::DateTime),
        Ok(moment(-53_690, 0))
    );
    // -1 + 40.5 ticks rounds **away from zero** on the total, so it lands on tick 40 of
    // the day before, where +1 + 40.5 ticks lands on tick 41.
    assert_eq!(
        cast_decimal(
            -9_999_984_375_000_000_000_000_000_000,
            28,
            SqlType::DateTime
        ),
        Ok(moment(-1, 40))
    );
    assert_eq!(
        cast_decimal(
            10_000_015_625_000_000_000_000_000_000,
            28,
            SqlType::DateTime
        ),
        Ok(moment(1, 41))
    );
}

/// The range is checked **after** the rounding, and the refusal is 8115, not the 242 a
/// date source raises for the same overflow.
#[test]
fn number_to_datetime_bounds() {
    assert_eq!(
        cast(&Value::I32(2_958_463), SqlType::Int, SqlType::DateTime),
        Ok(moment(2_958_463, 0))
    );
    // 2958463.9999983 is tick 25 919 956 of the last day.
    assert_eq!(
        cast_decimal(29_584_639_999_983, 7, SqlType::DateTime),
        Ok(moment(2_958_463, 25_919_956))
    );
    for value in [-53_691, 2_958_464, i32::MAX, i32::MIN] {
        let e = cast(&Value::I32(value), SqlType::Int, SqlType::DateTime)
            .expect_err("out of the range of a datetime");
        assert_eq!(e.number, 8115, "{value}");
        assert_eq!(e.state, 2, "{value}");
        assert_eq!(
            e.message,
            "Converting expression to data type datetime overflowed."
        );
    }
    // A fraction that rounds past the last tick overflows too.
    assert_eq!(
        cast_decimal(295_846_399_999_999, 8, SqlType::DateTime)
            .expect_err("the carry leaves the range")
            .number,
        8115
    );
    // A double no date could ever hold, and the two the guard must not let through.
    for f in [1e308, -1e308, f64::INFINITY, f64::NAN] {
        assert_eq!(
            cast_float(f, SqlType::DateTime)
                .expect_err("out of the range of a datetime")
                .number,
            8115
        );
    }
    // The smallest doubles are simply midnight.
    assert_eq!(cast_float(1e-308, SqlType::DateTime), Ok(moment(0, 0)));
    assert_eq!(cast_float(0.0, SqlType::DateTime), Ok(moment(0, 0)));
}

/// A `smalldatetime` counts minutes instead of ticks, and starts at 1900-01-01.
#[test]
fn number_to_smalldatetime() {
    let minute = |days: i32, minutes: u32| moment(days, minutes * TICKS_300TH_PER_MINUTE);
    assert_eq!(
        cast_decimal(500_000, 6, SqlType::SmallDateTime),
        Ok(minute(0, 720))
    );
    // 0.003125 day is exactly 4.5 minutes and rounds away from zero.
    assert_eq!(
        cast_decimal(3_125, 6, SqlType::SmallDateTime),
        Ok(minute(0, 5))
    );
    // 65535.9996 is minute 1 439 of the last day; 65535.9997 rounds into the next one.
    assert_eq!(
        cast_decimal(655_359_996, 4, SqlType::SmallDateTime),
        Ok(minute(65_535, 1_439))
    );
    assert_eq!(
        cast_decimal(655_359_997, 4, SqlType::SmallDateTime)
            .expect_err("the carry leaves the range")
            .number,
        8115
    );
    // 36524.99999 rounds to the next midnight.
    assert_eq!(
        cast_decimal(3_652_499_999, 5, SqlType::SmallDateTime),
        Ok(minute(36_525, 0))
    );
    assert_eq!(
        cast(&Value::I32(0), SqlType::Int, SqlType::SmallDateTime),
        Ok(minute(0, 0))
    );
    assert_eq!(
        cast(&Value::I32(65_535), SqlType::Int, SqlType::SmallDateTime),
        Ok(minute(65_535, 0))
    );
    for value in [-1, 65_536] {
        let e = cast(&Value::I32(value), SqlType::Int, SqlType::SmallDateTime)
            .expect_err("out of the range of a smalldatetime");
        assert_eq!(e.number, 8115, "{value}");
        assert_eq!(
            e.message,
            "Converting expression to data type smalldatetime overflowed."
        );
    }
    // Half a minute short of the origin still rounds to it; a hair more does not.
    assert_eq!(
        cast_decimal(-3_472, 7, SqlType::SmallDateTime),
        Ok(minute(0, 0))
    );
    assert_eq!(
        cast_decimal(-3_473, 7, SqlType::SmallDateTime)
            .expect_err("half a minute before 1900-01-01")
            .number,
        8115
    );
}

/// The four types of 2008 refuse a number outright, whichever numeric type it comes from.
#[test]
fn number_to_the_2008_types_is_529() {
    for to in [
        SqlType::Date,
        SqlType::Time(3),
        SqlType::DateTime2(3),
        SqlType::DateTimeOffset(3),
    ] {
        for (v, from) in [
            (Value::I32(1), SqlType::Int),
            (Value::F64(1.0), SqlType::Float),
            (Value::Money(10_000), SqlType::Money),
            (Value::Bit(true), SqlType::Bit),
        ] {
            let e = cast(&v, from, to).expect_err("no such conversion");
            assert_eq!(e.number, 529, "{from:?} -> {to:?}");
            assert_eq!(
                e.message,
                format!(
                    "No explicit conversion exists from {} to {}.",
                    from.error_name(),
                    to.error_name()
                ),
                "{from:?} -> {to:?}"
            );
        }
    }
}

/// A NULL of any numeric type is a NULL `datetime`.
#[test]
fn number_to_datetime_null() {
    for from in [SqlType::Float, SqlType::Money, SqlType::Int] {
        assert_eq!(cast(&Value::Null, from, SqlType::DateTime), Ok(Value::Null));
    }
}

/// Four big-endian bytes of signed day count then
/// four of 1/300 s ticks.
#[test]
fn binary_to_datetime_eight_bytes() {
    assert_eq!(
        cast_bytes(&[0, 0, 0, 1, 0x00, 0xC5, 0xC1, 0x00], SqlType::DateTime),
        Ok(moment(1, TICKS_300TH_PER_DAY / 2))
    );
    assert_eq!(
        cast_bytes(
            &[0xFF, 0xFF, 0xFF, 0xFE, 0x00, 0xC5, 0xC1, 0x00],
            SqlType::DateTime
        ),
        Ok(moment(-2, TICKS_300TH_PER_DAY / 2))
    );
    assert_eq!(
        cast_bytes(
            &[0x00, 0x2D, 0x24, 0x7F, 0x01, 0x8B, 0x81, 0xFF],
            SqlType::DateTime
        ),
        Ok(moment(2_958_463, 25_919_999))
    );
    assert_eq!(
        cast_bytes(&[0xFF, 0xFF, 0x2E, 0x46, 0, 0, 0, 0], SqlType::DateTime),
        Ok(moment(-53_690, 0))
    );
}

/// The bytes are read **right-aligned**, a
/// shorter value being padded on the left with zeroes and a longer one losing its leading
/// bytes whatever they hold (keys `L;<length>` and `H;<length>`).
#[test]
fn binary_to_datetime_other_lengths() {
    assert_eq!(cast_bytes(&[], SqlType::DateTime), Ok(moment(0, 0)));
    assert_eq!(cast_bytes(&[0x41], SqlType::DateTime), Ok(moment(0, 65)));
    assert_eq!(
        cast_bytes(&[0x41, 0x42, 0x43], SqlType::DateTime),
        Ok(moment(0, 0x41_4243))
    );
    assert_eq!(
        cast_bytes(&[0x00, 0x01, 0x00, 0x00], SqlType::DateTime),
        Ok(moment(0, 65_536))
    );
    assert_eq!(
        cast_bytes(
            &[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 1, 0x00, 0xC5, 0xC1, 0x00],
            SqlType::DateTime
        ),
        Ok(moment(1, TICKS_300TH_PER_DAY / 2))
    );
    // Sixteen bytes of 0xFF in front of eight zeroes are still midnight on 1900-01-01.
    let mut long = vec![0xFF_u8; 8];
    long.extend_from_slice(&[0; 8]);
    assert_eq!(cast_bytes(&long, SqlType::DateTime), Ok(moment(0, 0)));
}

/// The same right-alignment on four bytes, two of
/// unsigned day count and two of the minute of the day.
#[test]
fn binary_to_smalldatetime() {
    let minute = |days: i32, minutes: u32| moment(days, minutes * TICKS_300TH_PER_MINUTE);
    assert_eq!(
        cast_bytes(&[0, 1, 0, 0x0A], SqlType::SmallDateTime),
        Ok(minute(1, 10))
    );
    assert_eq!(
        cast_bytes(&[0x41], SqlType::SmallDateTime),
        Ok(minute(0, 65))
    );
    // The day count is unsigned: 0xFFFF is 2079-06-06, the last day of the type.
    assert_eq!(
        cast_bytes(&[0xFF, 0xFF, 0, 0], SqlType::SmallDateTime),
        Ok(minute(65_535, 0))
    );
    // Eight bytes lose their leading four.
    assert_eq!(
        cast_bytes(&[0, 0, 0, 1, 0, 0, 0, 0], SqlType::SmallDateTime),
        Ok(minute(0, 0))
    );
    assert_eq!(cast_bytes(&[], SqlType::SmallDateTime), Ok(minute(0, 0)));
}

/// The test asserts 210, state 1 and the message naming `datetime` on five vectors, on
/// two distinct axes:
///
/// * clock out of range, day inside the calendar: `[0, 0, 0, 0, 0x01, 0x8B, 0x82, 0x00]`
///   (25 920 000 ticks), `[0, 0, 0x05, 0xA0]` (minute 1 440) and `[0, 0, 0xFF, 0xFF]`
///   (minute 65 535);
/// * calendar out of range, ticks at **zero**: `[0xFF, 0xFF, 0x2E, 0x45, 0, 0, 0, 0]`
///   (`SELECT CAST(0xFFFF2E4500000000 AS datetime);`, the day before 1753-01-01) and
///   `[0, 0x2D, 0x24, 0x80, 0, 0, 0, 0]` (`SELECT CAST(0x002D248000000000 AS datetime);`,
///   the day after 9999-12-31).
#[test]
fn binary_to_datetime_out_of_range() {
    for (bytes, to) in [
        (vec![0, 0, 0, 0, 0x01, 0x8B, 0x82, 0x00], SqlType::DateTime),
        (vec![0xFF, 0xFF, 0x2E, 0x45, 0, 0, 0, 0], SqlType::DateTime),
        (vec![0x00, 0x2D, 0x24, 0x80, 0, 0, 0, 0], SqlType::DateTime),
        (vec![0, 0, 0x05, 0xA0], SqlType::SmallDateTime),
        (vec![0, 0, 0xFF, 0xFF], SqlType::SmallDateTime),
    ] {
        let e = cast_bytes(&bytes, to).expect_err("out of range");
        assert_eq!(e.number, 210, "{bytes:?} -> {to:?}");
        assert_eq!(e.severity, 16);
        assert_eq!(e.state, 1);
        assert_eq!(
            e.message,
            "A binary or varbinary value could not be converted to datetime."
        );
    }
    // A deliberate difference: SQL Server reads the four types of 2008 from a binary too,
    // on a rule of its own (a `date` takes the leading three bytes little-endian).
    // VaubanDB refuses rather than guessing.
    for to in [
        SqlType::Date,
        SqlType::Time(3),
        SqlType::DateTime2(3),
        SqlType::DateTimeOffset(3),
    ] {
        assert_eq!(
            cast_bytes(&[0, 0, 0, 0, 0, 0, 0, 0], to)
                .expect_err("not implemented")
                .number,
            8114
        );
    }
}

/// A NULL binary is a NULL date.
#[test]
fn binary_to_datetime_null() {
    assert_eq!(
        cast(
            &Value::Null,
            SqlType::VarBinary(Len::Fixed(8)),
            SqlType::DateTime
        ),
        Ok(Value::Null)
    );
    assert_eq!(
        cast(
            &Value::Null,
            SqlType::Binary(Len::Fixed(8)),
            SqlType::SmallDateTime
        ),
        Ok(Value::Null)
    );
}

/// The way back, guarded here since `CAST` accepts both directions.
#[test]
fn datetime_to_number_is_the_other_direction() {
    let noon = moment(1, TICKS_300TH_PER_DAY / 2);
    assert_eq!(
        cast(&noon, SqlType::DateTime, SqlType::Float),
        Ok(Value::F64(1.5))
    );
    // An integer target rounds where the number itself is halfway.
    assert_eq!(
        cast(&noon, SqlType::DateTime, SqlType::Int),
        Ok(Value::I32(2))
    );
    let before = moment(-2, TICKS_300TH_PER_DAY / 2);
    assert_eq!(
        cast(&before, SqlType::DateTime, SqlType::Float),
        Ok(Value::F64(-1.5))
    );
    assert_eq!(
        cast(&before, SqlType::DateTime, SqlType::Int),
        Ok(Value::I32(-2))
    );
    // And the round trip through a `float` gives the number back.
    for value in [0.0, 1.0, -1.0, 1.5, -1.5, 36_524.5, -53_690.0] {
        let dt = cast_float(value, SqlType::DateTime).expect("a datetime");
        assert_eq!(
            cast(&dt, SqlType::DateTime, SqlType::Float),
            Ok(Value::F64(value)),
            "{value}"
        );
    }
}

/// A `CONVERT` style says nothing about a number or a binary source: the six styles
/// below give the plain conversion, never 9809.
#[test]
fn a_style_says_nothing_about_a_number_or_a_binary() {
    let noon = Ok(moment(1, TICKS_300TH_PER_DAY / 2));
    for style in [0, 1, 121, 126, 77, -1] {
        assert_eq!(
            convert(
                &Value::F64(1.5),
                &ti(SqlType::Float),
                &ti(SqlType::DateTime),
                Some(style)
            ),
            noon,
            "style {style}"
        );
        assert_eq!(
            convert(
                &Value::Bytes(vec![0, 0, 0, 1, 0, 0, 0, 0]),
                &ti(SqlType::VarBinary(Len::Fixed(8))),
                &ti(SqlType::DateTime),
                Some(style)
            ),
            Ok(moment(1, 0)),
            "style {style}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// A `datetime` and a `smalldatetime` written to a `binary`: nine values crossed with
// `binary(n)` and `varbinary(n)` for the fourteen widths 1 to 12, 16 and 20, plus the
// length-less forms, plus the round trip.
// ---------------------------------------------------------------------------------------

/// `CAST(<v of type from> AS <to>)`, rendered as hexadecimal.
fn cast_to_hex(v: &Value, from: SqlType, to: SqlType) -> String {
    match cast(v, from, to) {
        Ok(Value::Bytes(bytes)) => {
            let mut hex = String::from("0x");
            for byte in &bytes {
                hex.push_str(&format!("{byte:02X}"));
            }
            hex
        }
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("error {}", e.number),
    }
}

/// A `binary(n)`
/// **pads on the left** and truncates on the left, a `varbinary(n)` truncates on the left
/// and never pads.
///
/// It is the opposite of what a byte string does — a `binary` source is
/// `CAST(0x0102030405 AS binary(8))` = `0x0102030405000000`, padded on the *right* — but not
/// the opposite of *every other* source: padding end and truncation end are two independent
/// properties, and `decimal` pads on the left like a `datetime` while it truncates on the
/// right like a byte string:
/// `CAST(CAST(1.5 AS decimal(5,2)) AS binary(12))` = `0x000000000502000196000000`, four zero
/// bytes in front, and `… AS binary(3))` = `0x050200`, the three leading bytes.
/// `crates/vauban-types/src/convert/binary.rs` documents the two properties; what follows
/// only fixes the `datetime` corner of that table.
///
/// The widths below tell the left-hand rule apart from the right-hand one on every value: at
/// `binary(4)`, day 1 at noon is `0x00C5C100` (the tick count kept, the day dropped) where
/// the right-hand rule would give `0x00000001` (the day kept, the ticks dropped).
#[test]
fn datetime_to_binary_pads_and_truncates_on_the_left() {
    // (n, `binary(n)`, `varbinary(n)`), as hexadecimal.
    type Widths = &'static [(u16, &'static str, &'static str)];
    let vectors: &[(SqlType, Value, Widths)] = &[
        // `dt_p1`: day 1, tick 12_960_000 of the day.
        (
            SqlType::DateTime,
            moment(1, 12_960_000),
            &[
                (1, "0x00", "0x00"),
                (2, "0xC100", "0xC100"),
                (3, "0xC5C100", "0xC5C100"),
                (4, "0x00C5C100", "0x00C5C100"),
                (5, "0x0100C5C100", "0x0100C5C100"),
                (6, "0x000100C5C100", "0x000100C5C100"),
                (7, "0x00000100C5C100", "0x00000100C5C100"),
                (8, "0x0000000100C5C100", "0x0000000100C5C100"),
                (9, "0x000000000100C5C100", "0x0000000100C5C100"),
                (10, "0x00000000000100C5C100", "0x0000000100C5C100"),
                (11, "0x0000000000000100C5C100", "0x0000000100C5C100"),
                (12, "0x000000000000000100C5C100", "0x0000000100C5C100"),
                (
                    16,
                    "0x00000000000000000000000100C5C100",
                    "0x0000000100C5C100",
                ),
                (
                    20,
                    "0x0000000000000000000000000000000100C5C100",
                    "0x0000000100C5C100",
                ),
            ],
        ),
        // `dt_m2`: day -2, tick 12_960_000 of the day.
        (
            SqlType::DateTime,
            moment(-2, 12_960_000),
            &[
                (1, "0x00", "0x00"),
                (2, "0xC100", "0xC100"),
                (3, "0xC5C100", "0xC5C100"),
                (4, "0x00C5C100", "0x00C5C100"),
                (5, "0xFE00C5C100", "0xFE00C5C100"),
                (6, "0xFFFE00C5C100", "0xFFFE00C5C100"),
                (7, "0xFFFFFE00C5C100", "0xFFFFFE00C5C100"),
                (8, "0xFFFFFFFE00C5C100", "0xFFFFFFFE00C5C100"),
                (9, "0x00FFFFFFFE00C5C100", "0xFFFFFFFE00C5C100"),
                (10, "0x0000FFFFFFFE00C5C100", "0xFFFFFFFE00C5C100"),
                (11, "0x000000FFFFFFFE00C5C100", "0xFFFFFFFE00C5C100"),
                (12, "0x00000000FFFFFFFE00C5C100", "0xFFFFFFFE00C5C100"),
                (
                    16,
                    "0x0000000000000000FFFFFFFE00C5C100",
                    "0xFFFFFFFE00C5C100",
                ),
                (
                    20,
                    "0x000000000000000000000000FFFFFFFE00C5C100",
                    "0xFFFFFFFE00C5C100",
                ),
            ],
        ),
        // `dt_min`: day -53_690, tick 0 of the day.
        (
            SqlType::DateTime,
            moment(-53_690, 0),
            &[
                (1, "0x00", "0x00"),
                (2, "0x0000", "0x0000"),
                (3, "0x000000", "0x000000"),
                (4, "0x00000000", "0x00000000"),
                (5, "0x4600000000", "0x4600000000"),
                (6, "0x2E4600000000", "0x2E4600000000"),
                (7, "0xFF2E4600000000", "0xFF2E4600000000"),
                (8, "0xFFFF2E4600000000", "0xFFFF2E4600000000"),
                (9, "0x00FFFF2E4600000000", "0xFFFF2E4600000000"),
                (10, "0x0000FFFF2E4600000000", "0xFFFF2E4600000000"),
                (11, "0x000000FFFF2E4600000000", "0xFFFF2E4600000000"),
                (12, "0x00000000FFFF2E4600000000", "0xFFFF2E4600000000"),
                (
                    16,
                    "0x0000000000000000FFFF2E4600000000",
                    "0xFFFF2E4600000000",
                ),
                (
                    20,
                    "0x000000000000000000000000FFFF2E4600000000",
                    "0xFFFF2E4600000000",
                ),
            ],
        ),
        // `dt_max`: day 2_958_463, tick 25_919_999 of the day.
        (
            SqlType::DateTime,
            moment(2_958_463, 25_919_999),
            &[
                (1, "0xFF", "0xFF"),
                (2, "0x81FF", "0x81FF"),
                (3, "0x8B81FF", "0x8B81FF"),
                (4, "0x018B81FF", "0x018B81FF"),
                (5, "0x7F018B81FF", "0x7F018B81FF"),
                (6, "0x247F018B81FF", "0x247F018B81FF"),
                (7, "0x2D247F018B81FF", "0x2D247F018B81FF"),
                (8, "0x002D247F018B81FF", "0x002D247F018B81FF"),
                (9, "0x00002D247F018B81FF", "0x002D247F018B81FF"),
                (10, "0x0000002D247F018B81FF", "0x002D247F018B81FF"),
                (11, "0x000000002D247F018B81FF", "0x002D247F018B81FF"),
                (12, "0x00000000002D247F018B81FF", "0x002D247F018B81FF"),
                (
                    16,
                    "0x0000000000000000002D247F018B81FF",
                    "0x002D247F018B81FF",
                ),
                (
                    20,
                    "0x000000000000000000000000002D247F018B81FF",
                    "0x002D247F018B81FF",
                ),
            ],
        ),
        // `dt_zero`: day 0, tick 0 of the day.
        (
            SqlType::DateTime,
            moment(0, 0),
            &[
                (1, "0x00", "0x00"),
                (2, "0x0000", "0x0000"),
                (3, "0x000000", "0x000000"),
                (4, "0x00000000", "0x00000000"),
                (5, "0x0000000000", "0x0000000000"),
                (6, "0x000000000000", "0x000000000000"),
                (7, "0x00000000000000", "0x00000000000000"),
                (8, "0x0000000000000000", "0x0000000000000000"),
                (9, "0x000000000000000000", "0x0000000000000000"),
                (10, "0x00000000000000000000", "0x0000000000000000"),
                (11, "0x0000000000000000000000", "0x0000000000000000"),
                (12, "0x000000000000000000000000", "0x0000000000000000"),
                (
                    16,
                    "0x00000000000000000000000000000000",
                    "0x0000000000000000",
                ),
                (
                    20,
                    "0x0000000000000000000000000000000000000000",
                    "0x0000000000000000",
                ),
            ],
        ),
        // `dt_2000`: day 36_525, tick 14_131_837 of the day.
        (
            SqlType::DateTime,
            moment(36_525, 14_131_837),
            &[
                (1, "0x7D", "0x7D"),
                (2, "0xA27D", "0xA27D"),
                (3, "0xD7A27D", "0xD7A27D"),
                (4, "0x00D7A27D", "0x00D7A27D"),
                (5, "0xAD00D7A27D", "0xAD00D7A27D"),
                (6, "0x8EAD00D7A27D", "0x8EAD00D7A27D"),
                (7, "0x008EAD00D7A27D", "0x008EAD00D7A27D"),
                (8, "0x00008EAD00D7A27D", "0x00008EAD00D7A27D"),
                (9, "0x0000008EAD00D7A27D", "0x00008EAD00D7A27D"),
                (10, "0x000000008EAD00D7A27D", "0x00008EAD00D7A27D"),
                (11, "0x00000000008EAD00D7A27D", "0x00008EAD00D7A27D"),
                (12, "0x0000000000008EAD00D7A27D", "0x00008EAD00D7A27D"),
                (
                    16,
                    "0x000000000000000000008EAD00D7A27D",
                    "0x00008EAD00D7A27D",
                ),
                (
                    20,
                    "0x00000000000000000000000000008EAD00D7A27D",
                    "0x00008EAD00D7A27D",
                ),
            ],
        ),
        // `sdt_zero`: day 0, tick 0 of the day.
        (
            SqlType::SmallDateTime,
            moment(0, 0),
            &[
                (1, "0x00", "0x00"),
                (2, "0x0000", "0x0000"),
                (3, "0x000000", "0x000000"),
                (4, "0x00000000", "0x00000000"),
                (5, "0x0000000000", "0x00000000"),
                (6, "0x000000000000", "0x00000000"),
                (7, "0x00000000000000", "0x00000000"),
                (8, "0x0000000000000000", "0x00000000"),
                (9, "0x000000000000000000", "0x00000000"),
                (10, "0x00000000000000000000", "0x00000000"),
                (11, "0x0000000000000000000000", "0x00000000"),
                (12, "0x000000000000000000000000", "0x00000000"),
                (16, "0x00000000000000000000000000000000", "0x00000000"),
                (
                    20,
                    "0x0000000000000000000000000000000000000000",
                    "0x00000000",
                ),
            ],
        ),
        // `sdt_2000`: day 36_525, tick 14_130_000 of the day.
        (
            SqlType::SmallDateTime,
            moment(36_525, 14_130_000),
            &[
                (1, "0x11", "0x11"),
                (2, "0x0311", "0x0311"),
                (3, "0xAD0311", "0xAD0311"),
                (4, "0x8EAD0311", "0x8EAD0311"),
                (5, "0x008EAD0311", "0x8EAD0311"),
                (6, "0x00008EAD0311", "0x8EAD0311"),
                (7, "0x0000008EAD0311", "0x8EAD0311"),
                (8, "0x000000008EAD0311", "0x8EAD0311"),
                (9, "0x00000000008EAD0311", "0x8EAD0311"),
                (10, "0x0000000000008EAD0311", "0x8EAD0311"),
                (11, "0x000000000000008EAD0311", "0x8EAD0311"),
                (12, "0x00000000000000008EAD0311", "0x8EAD0311"),
                (16, "0x0000000000000000000000008EAD0311", "0x8EAD0311"),
                (
                    20,
                    "0x000000000000000000000000000000008EAD0311",
                    "0x8EAD0311",
                ),
            ],
        ),
        // `sdt_max`: day 65_535, tick 25_902_000 of the day.
        (
            SqlType::SmallDateTime,
            moment(65_535, 25_902_000),
            &[
                (1, "0x9F", "0x9F"),
                (2, "0x059F", "0x059F"),
                (3, "0xFF059F", "0xFF059F"),
                (4, "0xFFFF059F", "0xFFFF059F"),
                (5, "0x00FFFF059F", "0xFFFF059F"),
                (6, "0x0000FFFF059F", "0xFFFF059F"),
                (7, "0x000000FFFF059F", "0xFFFF059F"),
                (8, "0x00000000FFFF059F", "0xFFFF059F"),
                (9, "0x0000000000FFFF059F", "0xFFFF059F"),
                (10, "0x000000000000FFFF059F", "0xFFFF059F"),
                (11, "0x00000000000000FFFF059F", "0xFFFF059F"),
                (12, "0x0000000000000000FFFF059F", "0xFFFF059F"),
                (16, "0x000000000000000000000000FFFF059F", "0xFFFF059F"),
                (
                    20,
                    "0x00000000000000000000000000000000FFFF059F",
                    "0xFFFF059F",
                ),
            ],
        ),
    ];
    for (from, value, widths) in vectors {
        for &(n, fixed, varying) in *widths {
            assert_eq!(
                cast_to_hex(value, *from, SqlType::Binary(Len::Fixed(n))),
                fixed,
                "{value:?} AS binary({n})"
            );
            assert_eq!(
                cast_to_hex(value, *from, SqlType::VarBinary(Len::Fixed(n))),
                varying,
                "{value:?} AS varbinary({n})"
            );
        }
    }
}

/// The two length-less forms of a `CAST`, which the binder resolves to `binary(30)` and
/// `varbinary(30)`, and `varbinary(max)`, which keeps the eight or four stored bytes.
#[test]
fn datetime_to_binary_without_a_length() {
    let noon = moment(1, TICKS_300TH_PER_DAY / 2);
    assert_eq!(
        cast_to_hex(&noon, SqlType::DateTime, SqlType::Binary(Len::Fixed(30))),
        "0x000000000000000000000000000000000000000000000000000100C5C100"
    );
    assert_eq!(
        cast_to_hex(&noon, SqlType::DateTime, SqlType::VarBinary(Len::Fixed(30))),
        "0x0000000100C5C100"
    );
    assert_eq!(
        cast_to_hex(&noon, SqlType::DateTime, SqlType::VarBinary(Len::Max)),
        "0x0000000100C5C100"
    );
    let minute = moment(65_535, 1_439 * TICKS_300TH_PER_MINUTE);
    assert_eq!(
        cast_to_hex(
            &minute,
            SqlType::SmallDateTime,
            SqlType::Binary(Len::Fixed(30))
        ),
        "0x0000000000000000000000000000000000000000000000000000FFFF059F"
    );
    assert_eq!(
        cast_to_hex(
            &minute,
            SqlType::SmallDateTime,
            SqlType::VarBinary(Len::Max)
        ),
        "0xFFFF059F"
    );
}

/// Nine values written to their own storage width and read back give the value they
/// started from, on `binary(n)` and `varbinary(n)` alike, for every `n` at least as wide
/// as the storage.
///
/// Three of the nine are older than 1900 and carry a negative day count, which is exactly
/// where an alignment mistake shows: the sign bytes are the leading ones.
#[test]
fn datetime_to_binary_round_trip() {
    let datetimes = [
        moment(1, TICKS_300TH_PER_DAY / 2),
        moment(-2, TICKS_300TH_PER_DAY / 2),
        moment(-53_690, 0),
        moment(2_958_463, 25_919_999),
        moment(0, 0),
        moment(36_525, 14_131_837),
    ];
    for value in &datetimes {
        for n in [8, 9, 12, 16, 30] {
            for to in [
                SqlType::Binary(Len::Fixed(n)),
                SqlType::VarBinary(Len::Fixed(n)),
            ] {
                let written = cast(value, SqlType::DateTime, to).expect("a datetime is bytes");
                assert_eq!(
                    cast(&written, to, SqlType::DateTime),
                    Ok(value.clone()),
                    "{value:?} through {}",
                    to.declaration()
                );
            }
        }
        let written = cast(value, SqlType::DateTime, SqlType::VarBinary(Len::Max))
            .expect("a datetime is bytes");
        assert_eq!(
            cast(&written, SqlType::VarBinary(Len::Max), SqlType::DateTime),
            Ok(value.clone())
        );
    }

    let smalls = [
        moment(0, 0),
        moment(36_525, 785 * TICKS_300TH_PER_MINUTE),
        moment(65_535, 1_439 * TICKS_300TH_PER_MINUTE),
    ];
    for value in &smalls {
        for n in [4, 5, 8, 12] {
            for to in [
                SqlType::Binary(Len::Fixed(n)),
                SqlType::VarBinary(Len::Fixed(n)),
            ] {
                let written =
                    cast(value, SqlType::SmallDateTime, to).expect("a smalldatetime is bytes");
                assert_eq!(
                    cast(&written, to, SqlType::SmallDateTime),
                    Ok(value.clone()),
                    "{value:?} through {}",
                    to.declaration()
                );
            }
        }
    }
}

/// A target narrower than the storage width loses the leading bytes and the round trip
/// stops closing — the vector that proves the truncation happens on the left and not on
/// the right, since a right-hand truncation would keep the day and lose the ticks.
///
/// `CAST(CAST(CAST('1900-01-02 12:00:00' AS datetime) AS binary(4)) AS datetime)` is
/// `1900-01-01 12:00:00.000`, and through `binary(2)` it is `1900-01-01 00:02:44.693`.
#[test]
fn datetime_to_a_narrow_binary_loses_the_leading_bytes() {
    let noon = moment(1, TICKS_300TH_PER_DAY / 2);
    let through = |n: u16| {
        let written = cast(&noon, SqlType::DateTime, SqlType::Binary(Len::Fixed(n)))
            .expect("a datetime is bytes");
        cast(&written, SqlType::Binary(Len::Fixed(n)), SqlType::DateTime)
    };
    assert_eq!(through(4), Ok(moment(0, TICKS_300TH_PER_DAY / 2)));
    assert_eq!(through(3), Ok(moment(0, TICKS_300TH_PER_DAY / 2)));
    assert_eq!(through(2), Ok(moment(0, 0xC100)));
    assert_eq!(through(1), Ok(moment(0, 0)));
    // Five bytes are enough here only because the day fits in one: at day -2 they are not.
    assert_eq!(through(5), Ok(noon));
    let long_ago = moment(-2, TICKS_300TH_PER_DAY / 2);
    let written = cast(&long_ago, SqlType::DateTime, SqlType::Binary(Len::Fixed(5)))
        .expect("a datetime is bytes");
    assert_eq!(
        cast(&written, SqlType::Binary(Len::Fixed(5)), SqlType::DateTime),
        Ok(moment(254, TICKS_300TH_PER_DAY / 2))
    );
}

// ---------------------------------------------------------------------------------------
// The four types of 2008 written to a `binary`: the eight scales of each scaled type, six
// zones, both ends of each calendar, the widths around the storage, and the refusal at one
// byte less.
// ---------------------------------------------------------------------------------------

/// `CAST(CAST(<literal> AS <ty>) AS varbinary(max))`, rendered as hexadecimal.
fn stored_hex(literal: &str, ty: SqlType) -> String {
    let value = cast_text(literal, ty).expect("the literal is a date the type holds");
    cast_to_hex(&value, ty, SqlType::VarBinary(Len::Max))
}

/// A `date` is three little-endian bytes of its day count
/// since 0001-01-01, and the declared length acts on the right — `binary(4)` pads behind
/// the value, `binary(2)` and `binary(1)` keep the leading bytes, in silence.
#[test]
fn date_to_binary_widths() {
    let second_of_january = cast_text("2000-01-02", SqlType::Date).expect("a date");
    let hex = |to: SqlType| cast_to_hex(&second_of_january, SqlType::Date, to);
    assert_eq!(hex(SqlType::VarBinary(Len::Max)), "0x08240B");
    assert_eq!(hex(SqlType::Binary(Len::Fixed(3))), "0x08240B");
    assert_eq!(hex(SqlType::Binary(Len::Fixed(4))), "0x08240B00");
    assert_eq!(
        hex(SqlType::Binary(Len::Fixed(12))),
        "0x08240B000000000000000000"
    );
    assert_eq!(hex(SqlType::VarBinary(Len::Fixed(12))), "0x08240B");
    assert_eq!(hex(SqlType::Binary(Len::Fixed(2))), "0x0824");
    assert_eq!(hex(SqlType::VarBinary(Len::Fixed(2))), "0x0824");
    assert_eq!(stored_hex("0001-01-01", SqlType::Date), "0x000000");
    assert_eq!(stored_hex("9999-12-31", SqlType::Date), "0xDAB937");
}

/// The eight scales of each scaled type, which give the stored widths 4/5/6, 7/8/9 and
/// 9/10/11. The scale is the leading byte; a `datetime2(s)`
/// appends the three date bytes to the clock of a `time(s)`, and a `datetimeoffset(s)`
/// stores the **UTC** clock and date, then the offset in minutes.
///
/// The clock is the same instant read at each scale, so the value stored at the scale 4 is
/// `.1235`: the engine rounds the fraction while reading the string, and the bytes here are
/// the bytes of that rounded value.
#[test]
fn the_eight_scales_of_2008_write_a_scale_byte_then_their_storage() {
    let time = [
        "0x0002B800",
        "0x01153007",
        "0x02D4E047",
        "0x034BC8CE02",
        "0x04F3D2131C",
        "0x057A3DC61801",
        "0x06C166BEF70A",
        "0x07870370AD6D",
    ];
    let datetime2 = [
        "0x0002B80008240B",
        "0x0115300708240B",
        "0x02D4E04708240B",
        "0x034BC8CE0208240B",
        "0x04F3D2131C08240B",
        "0x057A3DC6180108240B",
        "0x06C166BEF70A08240B",
        "0x07870370AD6D08240B",
    ];
    let offset = [
        "0x00E29B0008240B7800",
        "0x01D5160608240B7800",
        "0x0254E43C08240B7800",
        "0x034BEB600208240B7800",
        "0x04F330C91708240B7800",
        "0x057AE9DBED0008240B7800",
        "0x06C11E974A0908240B7800",
        "0x078733E7E95C08240B7800",
    ];
    for scale in 0..=7_u8 {
        let s = usize::from(scale);
        assert_eq!(
            stored_hex("13:05:06.1234567", SqlType::Time(scale)),
            time[s],
            "time({scale})"
        );
        assert_eq!(
            stored_hex("2000-01-02 13:05:06.1234567", SqlType::DateTime2(scale)),
            datetime2[s],
            "datetime2({scale})"
        );
        assert_eq!(
            stored_hex(
                "2000-01-02 13:05:06.1234567 +02:00",
                SqlType::DateTimeOffset(scale)
            ),
            offset[s],
            "datetimeoffset({scale})"
        );
    }
}

/// Six zones on the same instant. The stored clock
/// and date follow UTC — `+14:00` moves the date back a day, `-14:00` forward — and the two
/// offset bytes are signed little-endian: `-05:30` writes `0xB6FE` and `-00:30` `0xE2FF`.
#[test]
fn datetimeoffset_to_binary_writes_utc_then_a_signed_offset() {
    for (zone, expected) in [
        ("+00:00", "0x0002B80008240B0000"),
        ("-05:30", "0x005A050108240BB6FE"),
        ("+14:00", "0x00A2440107240B4803"),
        ("-14:00", "0x00622B0009240BB8FC"),
        ("+05:45", "0x0026670008240B5901"),
        ("-00:30", "0x000ABF0008240BE2FF"),
    ] {
        assert_eq!(
            stored_hex(
                &format!("2000-01-02 13:05:06 {zone}"),
                SqlType::DateTimeOffset(0)
            ),
            expected,
            "{zone}"
        );
    }
}

/// Both ends of each type, where a formula right in the middle of the range can still be
/// wrong.
#[test]
fn the_bounds_of_the_2008_types_to_binary() {
    assert_eq!(stored_hex("00:00:00", SqlType::Time(7)), "0x070000000000");
    assert_eq!(
        stored_hex("23:59:59.9999999", SqlType::Time(7)),
        "0x07FFBF692AC9"
    );
    assert_eq!(
        stored_hex("0001-01-01 00:00:00", SqlType::DateTime2(7)),
        "0x070000000000000000"
    );
    assert_eq!(
        stored_hex("9999-12-31 23:59:59.9999999", SqlType::DateTime2(7)),
        "0x07FFBF692AC9DAB937"
    );
    assert_eq!(
        stored_hex("0001-01-01 14:00:00 +14:00", SqlType::DateTimeOffset(7)),
        "0x0700000000000000004803"
    );
    assert_eq!(
        stored_hex(
            "9999-12-31 09:59:59.9999999 -14:00",
            SqlType::DateTimeOffset(7)
        ),
        "0x07FFBF692AC9DAB937B8FC"
    );
    assert_eq!(
        stored_hex("0001-01-01 00:00:00 +00:00", SqlType::DateTimeOffset(0)),
        "0x000000000000000000"
    );
}

/// A target wider than the storage pads
/// **behind** the value, where a `datetime` pads in front of it, and `varbinary(n)` pads on
/// neither end. Three more scales at their own exact width close the widths 5, 9 and 9.
#[test]
fn temporal_to_binary_pads_at_the_tail() {
    let time = cast_text("13:05:06.123", SqlType::Time(3)).expect("a time");
    let datetime2 = cast_text("2000-01-02 13:05:06.123", SqlType::DateTime2(3)).expect("a value");
    let offset =
        cast_text("2000-01-02 13:05:06 +02:00", SqlType::DateTimeOffset(0)).expect("a value");
    assert_eq!(
        cast_to_hex(&time, SqlType::Time(3), SqlType::Binary(Len::Fixed(12))),
        "0x034BC8CE0200000000000000"
    );
    assert_eq!(
        cast_to_hex(&time, SqlType::Time(3), SqlType::VarBinary(Len::Fixed(12))),
        "0x034BC8CE02"
    );
    assert_eq!(
        cast_to_hex(
            &datetime2,
            SqlType::DateTime2(3),
            SqlType::Binary(Len::Fixed(12))
        ),
        "0x034BC8CE0208240B00000000"
    );
    assert_eq!(
        cast_to_hex(
            &offset,
            SqlType::DateTimeOffset(0),
            SqlType::Binary(Len::Fixed(12))
        ),
        "0x00E29B0008240B7800000000"
    );
    // The exact widths, which pad and cut nothing.
    assert_eq!(
        cast_to_hex(&time, SqlType::Time(3), SqlType::Binary(Len::Fixed(5))),
        "0x034BC8CE02"
    );
    assert_eq!(
        cast_to_hex(
            &datetime2,
            SqlType::DateTime2(3),
            SqlType::Binary(Len::Fixed(8))
        ),
        "0x034BC8CE0208240B"
    );
    assert_eq!(
        cast_to_hex(
            &offset,
            SqlType::DateTimeOffset(0),
            SqlType::Binary(Len::Fixed(9))
        ),
        "0x00E29B0008240B7800"
    );
    let intermediates = [
        ("13:05:06.1234", SqlType::Time(4), 5, "0x04F2D2131C"),
        (
            "2000-01-02 13:05:06.123456",
            SqlType::DateTime2(6),
            9,
            "0x06C066BEF70A08240B",
        ),
        (
            "2000-01-02 13:05:06.1 +02:00",
            SqlType::DateTimeOffset(1),
            9,
            "0x01D5160608240B7800",
        ),
    ];
    for (literal, ty, width, expected) in intermediates {
        let value = cast_text(literal, ty).expect("a date the type holds");
        assert_eq!(
            cast_to_hex(&value, ty, SqlType::Binary(Len::Fixed(width))),
            expected,
            "{}",
            ty.declaration()
        );
    }
}

/// A target narrower than the storage width is error 8152, severity 16, state 17, on
/// `varbinary(n)` as on `binary(n)`, at the scales 0, 3 and 7 of each scaled type.
///
/// The counter-proof is in the same test: a `date` and a `datetime` given the same narrow
/// target answer a value (`0x08` and `0x58`), so the refusal belongs to the three scaled
/// types and not to the width.
#[test]
fn a_narrow_binary_is_refused_by_the_three_scaled_types_of_2008() {
    // Literal, type, a width that is refused, and the storage width that is not.
    let refused = [
        ("13:05:06.123", SqlType::Time(3), 4, 5),
        ("13:05:06", SqlType::Time(0), 3, 4),
        ("13:05:06.1234567", SqlType::Time(7), 5, 6),
        ("13:05:06.123", SqlType::Time(3), 1, 5),
        ("2000-01-02 13:05:06.123", SqlType::DateTime2(3), 7, 8),
        ("2000-01-02 13:05:06", SqlType::DateTime2(0), 6, 7),
        ("2000-01-02 13:05:06.1234567", SqlType::DateTime2(7), 8, 9),
        (
            "2000-01-02 13:05:06.123 +02:00",
            SqlType::DateTimeOffset(3),
            9,
            10,
        ),
        (
            "2000-01-02 13:05:06 +02:00",
            SqlType::DateTimeOffset(0),
            8,
            9,
        ),
        (
            "2000-01-02 13:05:06.1234567 +02:00",
            SqlType::DateTimeOffset(7),
            10,
            11,
        ),
    ];
    for (literal, ty, width, storage) in refused {
        let value = cast_text(literal, ty).expect("a date the type holds");
        for to in [
            SqlType::Binary(Len::Fixed(width)),
            SqlType::VarBinary(Len::Fixed(width)),
        ] {
            let e = cast(&value, ty, to).expect_err("narrower than the storage width");
            assert_eq!(
                e.number,
                8152,
                "{} -> {}",
                ty.declaration(),
                to.declaration()
            );
            assert_eq!(e.severity, 16);
            assert_eq!(e.state, 17);
            assert_eq!(
                e.message,
                "Data too long: the string or binary value would be cut."
            );
        }
        // The storage width itself converts, so the refusal is the width and not the type.
        let wide = SqlType::Binary(Len::Fixed(storage));
        assert!(cast(&value, ty, wide).is_ok(), "{}", ty.declaration());
    }

    // The other family, on the narrowest target there is.
    let date = cast_text("2000-01-02", SqlType::Date).expect("a date");
    let legacy = cast_text("2000-01-02 13:05:06", SqlType::DateTime).expect("a datetime");
    assert_eq!(
        cast_to_hex(&date, SqlType::Date, SqlType::Binary(Len::Fixed(1))),
        "0x08"
    );
    assert_eq!(
        cast_to_hex(&legacy, SqlType::DateTime, SqlType::Binary(Len::Fixed(1))),
        "0x58"
    );
}

/// Five values of each type written to their storage width and read back give the value
/// they started from.
///
/// VaubanDB does not read a binary as one of these four types (the reading stops at
/// `datetime`), so the reading here is the test's own, written from the layout the tests
/// above pin down. What it adds to them: the stored width and the scale byte are enough
/// to recover the value, which a wrong width or a dropped scale byte would break.
#[test]
fn the_2008_storage_reads_back_to_the_value_it_came_from() {
    /// Bytes the tick count of a scale takes.
    fn width_of(scale: u8) -> usize {
        match scale {
            0..=2 => 3,
            3..=4 => 4,
            _ => 5,
        }
    }
    /// The little-endian integer the `n` bytes at `start` spell.
    fn little_endian(bytes: &[u8], start: usize, n: usize) -> u64 {
        let mut word = [0_u8; 8];
        word[..n].copy_from_slice(&bytes[start..start + n]);
        u64::from_le_bytes(word)
    }
    fn read_back(bytes: &[u8], ty: SqlType) -> Value {
        match ty {
            SqlType::Date => {
                assert_eq!(bytes.len(), 3);
                Value::Date(Date {
                    days: little_endian(bytes, 0, 3) as i32,
                })
            }
            SqlType::Time(scale) => {
                let w = width_of(scale);
                assert_eq!(bytes.len(), 1 + w);
                assert_eq!(bytes[0], scale);
                Value::Time(Time {
                    ticks_100ns: little_endian(bytes, 1, w) * 10_u64.pow(u32::from(7 - scale)),
                })
            }
            SqlType::DateTime2(scale) => {
                let w = width_of(scale);
                assert_eq!(bytes.len(), 1 + w + 3);
                let Value::Time(time) = read_back(&bytes[..1 + w], SqlType::Time(scale)) else {
                    unreachable!("a time reads back as a time")
                };
                let Value::Date(date) = read_back(&bytes[1 + w..], SqlType::Date) else {
                    unreachable!("a date reads back as a date")
                };
                Value::DateTime2(DateTime2 { date, time })
            }
            SqlType::DateTimeOffset(scale) => {
                let w = width_of(scale);
                assert_eq!(bytes.len(), 1 + w + 3 + 2);
                let Value::DateTime2(utc) =
                    read_back(&bytes[..1 + w + 3], SqlType::DateTime2(scale))
                else {
                    unreachable!("a datetime2 reads back as a datetime2")
                };
                Value::DateTimeOffset(DateTimeOffset {
                    utc,
                    offset_minutes: i16::from_le_bytes([bytes[1 + w + 3], bytes[1 + w + 4]]),
                })
            }
            other => unreachable!("{} is not one of the four", other.declaration()),
        }
    }

    let vectors = [
        ("2000-01-02", SqlType::Date),
        ("0001-01-01", SqlType::Date),
        ("9999-12-31", SqlType::Date),
        ("1900-01-01", SqlType::Date),
        ("2024-02-29", SqlType::Date),
        ("13:05:06.123", SqlType::Time(3)),
        ("00:00:00", SqlType::Time(3)),
        ("23:59:59.999", SqlType::Time(3)),
        ("13:05:06", SqlType::Time(0)),
        ("13:05:06.1234567", SqlType::Time(7)),
        ("23:59:59.9999999", SqlType::Time(7)),
        ("2000-01-02 13:05:06.123", SqlType::DateTime2(3)),
        ("0001-01-01 00:00:00", SqlType::DateTime2(0)),
        ("9999-12-31 23:59:59.9999999", SqlType::DateTime2(7)),
        ("1900-01-01 12:00:00", SqlType::DateTime2(0)),
        ("2024-02-29 23:59:59.999999", SqlType::DateTime2(6)),
        ("2000-01-02 13:05:06 +02:00", SqlType::DateTimeOffset(0)),
        ("2000-01-02 13:05:06 -05:30", SqlType::DateTimeOffset(0)),
        ("2000-01-02 13:05:06.123 +00:00", SqlType::DateTimeOffset(3)),
        (
            "2000-01-02 13:05:06.1234567 +14:00",
            SqlType::DateTimeOffset(7),
        ),
        ("2000-01-02 13:05:06 -14:00", SqlType::DateTimeOffset(0)),
        (
            "2000-01-02 13:05:06.12345 -00:30",
            SqlType::DateTimeOffset(5),
        ),
    ];
    for (literal, ty) in vectors {
        let value = cast_text(literal, ty).expect("a date the type holds");
        let written = cast(&value, ty, SqlType::VarBinary(Len::Max)).expect("bytes");
        let Value::Bytes(bytes) = &written else {
            unreachable!("a binary target answers bytes")
        };
        assert_eq!(
            read_back(bytes, ty),
            value,
            "{literal} as {}",
            ty.declaration()
        );
    }
}

/// A fraction, a zone or a `Z` the legacy grammar refuses is 241/16/1 towards `datetime`
/// and 295/16/3 towards `smalldatetime`: the error identity remains specific to the
/// target.
#[test]
fn legacy_fraction_zone_errors_keep_the_target_identity() {
    for suffix in [".1111", ".0000", ".1000", " +02:00", "Z"] {
        let form = format!("2000-01-02 13:05:06{suffix}");
        for (ty, number, state) in [
            (SqlType::DateTime, 241, 1),
            (SqlType::SmallDateTime, 295, 3),
        ] {
            let error = cast_text(&form, ty).unwrap_err();
            assert_eq!(
                (error.number, error.severity, error.state),
                (number, 16, state),
                "{form}"
            );
        }
    }
}

/// Two numeric literals whose reading depends on the date format, read without a style
/// under the defaults `us_english` and `DATEFORMAT mdy`. `'12-09-2018'` is the vector that
/// tells `mdy` from `dmy`: 12 and 09 are both valid months, and `dmy` would give
/// 2018-09-12.
#[test]
fn numeric_literals_read_as_mdy() {
    assert_eq!(
        cast_text("12-09-2018", SqlType::Date),
        Ok(Value::Date(Date {
            days: days_from_civil(2018, 12, 9)
        }))
    );
    assert_eq!(
        cast_text("01-03-2018", SqlType::Date),
        Ok(Value::Date(Date {
            days: days_from_civil(2018, 1, 3)
        }))
    );
}

/// `'28 listopad 2018'` is 2018-11-28 under Polish and 2018-10-28 under Croatian; under
/// the default language it is error 241, severity 16, state 1. The English gloss
/// `'28 November 2018'` is the counter-vector: it reads as 2018-11-28, so the 241 is "not
/// an English month name", not "no month name at all".
#[test]
fn listopad_under_frozen_language() {
    let error = cast_text("28 listopad 2018", SqlType::Date).unwrap_err();
    assert_eq!((error.number, error.severity, error.state), (241, 16, 1));
    assert_eq!(
        error.message,
        "The character string could not be converted to a date or time."
    );
    assert_eq!(
        cast_text("28 November 2018", SqlType::Date),
        Ok(Value::Date(Date {
            days: days_from_civil(2018, 11, 28)
        }))
    );
}
