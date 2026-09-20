//! Date arithmetic: `DATEADD`, `DATEDIFF`, `EOMONTH` and `DATEFROMPARTS`.
//!
//! Microsoft Learn ("DATEADD", "DATEDIFF", "EOMONTH", "DATEFROMPARTS") gives the shape of
//! the rules below; the roundings, the states and the implicit type of a string are the
//! finer points, each covered by a test of this file.
//!
//! ## `DATEADD`
//!
//! - The result has the **type of the third argument**, scale included, and is nullable
//!   even from a literal (`dateadd_keeps_the_argument_type`). A string or a number is
//!   read as a **`datetime`**: `DATEADD(day, 1, '2020-01-31 23:59:59 +05:30')` is 241,
//!   where `DATEDIFF` and `DATEPART` read the same string as a `datetimeoffset`, and
//!   `DATEADD(microsecond, 1, '2020-01-01')` is 9810 *for data type datetime*.
//! - Which `datepart` a type accepts is [`Shape::accepts_add`]
//!   (`dateadd_time_part_on_date_is_9810`): a `date` refuses the time parts, a `time` the
//!   date parts, a `datetime` and a `smalldatetime` refuse `microsecond` and `nanosecond`,
//!   and `iso_week` and `tzoffset` are refused on each shape. A `NULL` date answers `NULL`
//!   **before** that check: `DATEADD(hour, 1, CAST(NULL AS date))` is `NULL`, not 9810.
//! - The number is converted to `int` by [`vauban_types::convert`], hence truncated from a
//!   `decimal` or a `float` and **rounded** from a `money` (`1.5` is two days), 8115 from a
//!   `bigint` out of range, 232 from a huge `float`. A `bit`, a string, a binary, a date
//!   or a `uniqueidentifier` is refused at binding with 8116 on argument 2. Nothing is
//!   restated here: what `types` answers for the conversion is what `DATEADD` answers
//!   (`dateadd_number_conversions`).
//! - Months (and quarters, and years) clamp the day to the end of the target month and
//!   leave the time of day alone. Weeks are seven days; `weekday` and `dayofyear` are days.
//! - A `datetime` counts 1/300 s: the milliseconds added become ticks first, rounded half
//!   away from zero (`n * 3 / 10`), then the ticks are added. From `.003` (tick 1), +5 ms is
//!   tick 3 (`.010`), where rounding the sum of the displayed milliseconds would give tick 2.
//!   A `smalldatetime` is that `datetime` rounded to the minute, 30 s going up, **before**
//!   the range check: 2079-06-06 23:59 + 30 s is 517.
//! - A `datetime2` and a `datetimeoffset` compute at 100 ns (nanoseconds are rounded to a
//!   tick half away from zero: 50 ns is one, 49 none, -50 minus one), check the range on
//!   that exact result, then round half up to the declared scale, staying on the last
//!   instant of 9999-12-31 when the rounding would leave the calendar — the rule
//!   `vauban_types::convert` already applies, which is why the rounding is a conversion
//!   here. A `time` wraps around midnight instead, in both directions.
//! - A `datetimeoffset` is computed on its **local** reading and keeps its offset; both the
//!   local result and its universal instant must fit the calendar (517 either way).
//! - The `DATEADD` overflows send 517/1 for `datetime`, 517/2 at the `smalldatetime`
//!   bound, and 517/3 for `date`, `datetime2`, `datetimeoffset`. A `smalldatetime` plus
//!   2147483647 days instead overflows the intermediate `datetime` and sends 517/1
//!   (`dateadd_overflow_is_517`). The 9810 states are 1 for `time`, 1 for `date` on each
//!   refused part but `iso_week` (state 2 there), 0 for `datetime`, 3 for
//!   `smalldatetime`, 2 for `datetime2` and `datetimeoffset`
//!   (`dateadd_time_part_on_date_is_9810`); the constructors of `vauban-errors` carry
//!   them.
//! - A string a `datetime` cannot read is 241 **before** the `datepart` is checked: a
//!   refused part on `'2020-01-31 23:59:59.9999999 +05:30'` answers 241 and not 9810,
//!   which is why [`dateadd_eval`] converts before it checks the part.
//!
//! ## `DATEDIFF`
//!
//! - Counts the **boundaries** of the `datepart` crossed between the two instants, on a
//!   common line: a `date` is midnight, a `time` sits on 1900-01-01, a number is a
//!   `datetime`, a string keeps its seven digits and its offset, and a `datetimeoffset` is
//!   taken on its **universal** instant (midnight +00:00 to midnight +05:00 is -5 hours).
//!   The ticks of a `datetime` are read exactly: tick 1 is 3 333 333 ns.
//! - `week` starts on **Sunday** whatever `SET DATEFIRST` says (with `DATEFIRST 1`,
//!   Saturday to Sunday is still one week and Sunday to Monday none).
//! - The count is computed in `i128` and must fit an `int`, else 535: 2 147 483 600 ns
//!   fits, 2 147 483 700 does not, and neither does -2 147 483 700.
//! - `iso_week` and `tzoffset` answer **9806**. `NULL` wins over that error too.
//! - `int`, nullable.
//!
//! ## `EOMONTH`
//!
//! - `date`, nullable. Reads the date types but `time`, and the strings (a
//!   `datetimeoffset` on its local day); a number, a `bit`, a binary, a `time` or a
//!   `uniqueidentifier` is 8116 on argument 1: the one function here that does not read
//!   a number as a `datetime` (`eomonth_basics`).
//! - The offset is converted to `int` like the number of `DATEADD`, but a string is
//!   accepted (`'1'`), and a **`NULL` offset counts as zero**: `EOMONTH('2020-02-05',
//!   NULL)` is 2020-02-29, from a literal or from a variable. A `NULL` date is `NULL`.
//! - Past the calendar is 517 for `date`, state 1 (where `DATEADD` on a `date` sends 3).
//!
//! ## `DATEFROMPARTS`
//!
//! - `date`, nullable **only when an argument is**: from three literals `is_nullable` is 0.
//! - Each argument is converted to `int` (`money` rounds, `decimal` truncates, `'2020'`
//!   is read, `'x'` is 245); a `date`, a `time` or a `uniqueidentifier` is 206 *is
//!   incompatible with int*, a `datetime` is 257. Any `NULL` argument is `NULL`, even next
//!   to a month of 13. What is not a day of the proleptic Gregorian calendar between
//!   0001-01-01 and 9999-12-31 is 289, state 1.

use vauban_errors::{SqlError, SqlResult};
use vauban_types::calendar::{DAYS_1900, civil_from_days, days_from_civil, is_valid_civil};
use vauban_types::{
    Date, DateTime, DateTime2, DateTimeOffset, SqlType, Time, TypeFamily, TypeInfo, Value, convert,
    default_display,
};

use super::args::invalid_argument_type;
use super::datetime_clock::{DatePart, days_in_month, parse_datepart};
use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// 100 ns ticks in one second, the resolution of `time(7)` and `datetime2(7)`.
const TICKS_PER_SECOND: i128 = 10_000_000;

/// 100 ns ticks in one day.
const TICKS_PER_DAY: i128 = 86_400 * TICKS_PER_SECOND;

/// 1/300 s ticks in one second, the resolution of a `datetime`.
const TICKS_300TH_PER_SECOND: i64 = 300;

/// 1/300 s ticks in one day.
const TICKS_300TH_PER_DAY: i64 = 86_400 * TICKS_300TH_PER_SECOND;

/// 1/300 s ticks in one minute, the resolution of a `smalldatetime`.
const TICKS_300TH_PER_MINUTE: i64 = 60 * TICKS_300TH_PER_SECOND;

/// Nanoseconds in one day.
const NANOS_PER_DAY: i128 = 86_400 * 1_000_000_000;

/// Days from 0001-01-01 to 9999-12-31, the last day of `date`, `datetime2` and
/// `datetimeoffset`. Checked against the calendar of `types` by `calendar_bounds`.
const MAX_DAYS: i32 = 3_652_058;

/// Days from 0001-01-01 to 1753-01-01, the first day of a `datetime`.
const DATETIME_MIN_DAYS: i32 = 639_905;

/// Days from 0001-01-01 to 2079-06-06, the last day of a `smalldatetime`.
const SMALLDATETIME_MAX_DAYS: i32 = 759_130;

/// The first spelling of each `datepart` in the table of `datetime_clock`, the one SQL
/// Server prints in 9810 (`The datepart iso_week is not supported …`).
fn keyword_name(part: DatePart) -> &'static str {
    match part {
        DatePart::Year => "year",
        DatePart::Quarter => "quarter",
        DatePart::Month => "month",
        DatePart::DayOfYear => "dayofyear",
        DatePart::Day => "day",
        DatePart::Week => "week",
        DatePart::IsoWeek => "iso_week",
        DatePart::Weekday => "weekday",
        DatePart::Hour => "hour",
        DatePart::Minute => "minute",
        DatePart::Second => "second",
        DatePart::Millisecond => "millisecond",
        DatePart::Microsecond => "microsecond",
        DatePart::Nanosecond => "nanosecond",
        DatePart::TzOffset => "tzoffset",
    }
}

/// The `datepart` keyword an evaluation received, or error 155.
///
/// The `binder` has already refused an unknown keyword; this is the safety net of an
/// evaluation reached another way. The binder lowers a keyword to a string value and
/// rejects the written string expression `DATEADD('day', 1, CAST('2020-01-31' AS date))`
/// with 1023 before evaluation.
fn keyword_of(args: &EvalArgs<'_>, function: &str) -> SqlResult<DatePart> {
    let name = match &args.values[0] {
        Value::String(s) => s.text.clone(),
        other => default_display(other, &args.types[0]),
    };
    parse_datepart(&name).ok_or_else(|| SqlError::not_a_recognized_option(&name, function))
}

/// The unit a `datepart` adds or counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    /// `year`: twelve months; `quarter`: three; `month`: one.
    Months(i64),
    /// `day`, `dayofyear`, `weekday`: one day; `week`: seven.
    Days(i64),
    /// The time parts, in nanoseconds.
    Nanos(i64),
    /// `iso_week` and `tzoffset`, which neither function computes with.
    None,
}

impl Unit {
    fn of(part: DatePart) -> Self {
        match part {
            DatePart::Year => Unit::Months(12),
            DatePart::Quarter => Unit::Months(3),
            DatePart::Month => Unit::Months(1),
            DatePart::Week => Unit::Days(7),
            DatePart::Day | DatePart::DayOfYear | DatePart::Weekday => Unit::Days(1),
            DatePart::Hour => Unit::Nanos(3_600_000_000_000),
            DatePart::Minute => Unit::Nanos(60_000_000_000),
            DatePart::Second => Unit::Nanos(1_000_000_000),
            DatePart::Millisecond => Unit::Nanos(1_000_000),
            DatePart::Microsecond => Unit::Nanos(1_000),
            DatePart::Nanosecond => Unit::Nanos(1),
            DatePart::IsoWeek | DatePart::TzOffset => Unit::None,
        }
    }
}

/// How a string argument is read when it has to become a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringReading {
    /// `DATEADD`: as a `datetime` (no offset, 1/300 s, from 1753).
    AsDateTime,
    /// `DATEDIFF`: as a `datetimeoffset(7)` (seven digits, offset applied).
    AsDateTimeOffset,
}

/// The shape of a date argument once the `binder` has typed it: which value it converts
/// to, which `datepart` it accepts, and the name 9810 and 517 print for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `date`.
    DateOnly,
    /// `time(s)`.
    TimeOnly(u8),
    /// `datetime`, and every string or number `DATEADD` reads as one.
    DateTime,
    /// `smalldatetime`.
    SmallDateTime,
    /// `datetime2(s)`.
    DateTime2(u8),
    /// `datetimeoffset(s)`, and the strings `DATEDIFF` reads as one.
    WithOffset(u8),
}

impl Shape {
    /// The shape of a declared type, or the 206 a type with no date reading deserves.
    ///
    /// `uniqueidentifier` is the one such type: 206 against `datetime`, state 2, on both
    /// functions.
    fn of(ty: &TypeInfo, strings: StringReading) -> SqlResult<Self> {
        Ok(match &ty.ty {
            SqlType::Date => Shape::DateOnly,
            SqlType::Time(scale) => Shape::TimeOnly(*scale),
            SqlType::DateTime => Shape::DateTime,
            SqlType::SmallDateTime => Shape::SmallDateTime,
            SqlType::DateTime2(scale) => Shape::DateTime2(*scale),
            SqlType::DateTimeOffset(scale) => Shape::WithOffset(*scale),
            other => match other.family() {
                TypeFamily::Character => match strings {
                    StringReading::AsDateTime => Shape::DateTime,
                    StringReading::AsDateTimeOffset => Shape::WithOffset(7),
                },
                // `DATEADD(day, 1, 0)` is 1900-01-02 and `DATEDIFF(hour, 0, 0.5)` is 12:
                // a number is a `datetime`, as it is for `DATEPART`.
                TypeFamily::Integer
                | TypeFamily::ExactNumeric
                | TypeFamily::ApproxNumeric
                | TypeFamily::Money
                | TypeFamily::Bit
                | TypeFamily::Binary => Shape::DateTime,
                TypeFamily::Guid | TypeFamily::DateTime => {
                    return Err(SqlError::operand_type_clash(other.error_name(), "datetime"));
                }
            },
        })
    }

    /// The type a value of this shape is converted to before it is computed with.
    ///
    /// A `datetimeoffset` is read through a `datetime2(7)`, which gives the **local**
    /// reading (`convert` without a style), because `DATEADD` computes on that reading; the
    /// offset itself is read from the value apart. A `smalldatetime` is read as a
    /// `datetime`: same struct, whole minutes.
    fn canonical(self) -> SqlType {
        match self {
            Shape::DateOnly => SqlType::Date,
            Shape::TimeOnly(_) => SqlType::Time(7),
            Shape::DateTime | Shape::SmallDateTime => SqlType::DateTime,
            Shape::DateTime2(_) | Shape::WithOffset(_) => SqlType::DateTime2(7),
        }
    }

    /// The type of the result of `DATEADD` on this shape.
    fn result_type(self) -> SqlType {
        match self {
            Shape::DateOnly => SqlType::Date,
            Shape::TimeOnly(scale) => SqlType::Time(scale),
            Shape::DateTime => SqlType::DateTime,
            Shape::SmallDateTime => SqlType::SmallDateTime,
            Shape::DateTime2(scale) => SqlType::DateTime2(scale),
            Shape::WithOffset(scale) => SqlType::DateTimeOffset(scale),
        }
    }

    /// The name 9810 and 517 print for this shape: the type without its scale, and
    /// `datetime` for a string or a number.
    fn error_name(self) -> &'static str {
        self.result_type().error_name()
    }

    /// Whether `DATEADD` accepts `part` on this shape.
    fn accepts_add(self, part: DatePart) -> bool {
        match Unit::of(part) {
            Unit::None => false,
            Unit::Months(_) | Unit::Days(_) => !matches!(self, Shape::TimeOnly(_)),
            Unit::Nanos(unit) => match self {
                Shape::DateOnly => false,
                Shape::TimeOnly(_) | Shape::DateTime2(_) | Shape::WithOffset(_) => true,
                // Nothing finer than the millisecond on the 1/300 s types.
                Shape::DateTime | Shape::SmallDateTime => unit >= 1_000_000,
            },
        }
    }
}

/// A date and a time of day on one line, the form `DATEDIFF` computes with.
///
/// The time of day is in **nanoseconds** rather than 100 ns ticks, because a `datetime`
/// counts 1/300 s and `DATEDIFF(nanosecond, …)` reports tick 1 as 3 333 333 ns, the exact
/// third, where a detour through `datetime2(7)` would say 3 333 300.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Instant {
    /// Days since 0001-01-01.
    days: i32,
    /// Nanoseconds since midnight, `0..NANOS_PER_DAY`.
    nanos: i64,
}

impl Instant {
    /// Nanoseconds since 0001-01-01, in `i128` because the calendar holds 3.2 × 10²⁰ of them.
    fn total_nanos(self) -> i128 {
        i128::from(self.days) * NANOS_PER_DAY + i128::from(self.nanos)
    }
}

/// Reads the argument at `index` as an [`Instant`] on the common line of `DATEDIFF`.
///
/// `Ok(None)` is `NULL`. Every conversion is [`vauban_types::convert`]'s; only the reading
/// of the converted value is done here. A `datetimeoffset` gives its universal instant.
fn instant_of(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<Instant>> {
    let value = &args.values[index];
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let shape = Shape::of(&args.types[index], StringReading::AsDateTimeOffset)?;
    let target = match shape {
        Shape::WithOffset(_) => SqlType::DateTimeOffset(7),
        other => other.canonical(),
    };
    let converted = convert(
        value,
        &args.types[index],
        &TypeInfo::new(target, true),
        None,
    )?;
    Ok(Some(match converted {
        Value::Date(d) => Instant {
            days: d.days,
            nanos: 0,
        },
        // A `time` sits on 1900-01-01, like a `datetime` with no date part.
        Value::Time(t) => Instant {
            days: DAYS_1900,
            nanos: ticks_to_nanos(t.ticks_100ns),
        },
        Value::DateTime(dt) => Instant {
            days: dt.days + DAYS_1900,
            nanos: nanos_of_300th(dt.ticks_300th),
        },
        Value::DateTime2(dt) => instant_of_datetime2(dt),
        Value::DateTimeOffset(dto) => instant_of_datetime2(dto.utc),
        // `convert` towards a date type answers a date type; this is unreachable, and
        // treated as `NULL` rather than as a query error.
        _ => return Ok(None),
    }))
}

fn instant_of_datetime2(dt: DateTime2) -> Instant {
    Instant {
        days: dt.date.days,
        nanos: ticks_to_nanos(dt.time.ticks_100ns),
    }
}

/// 100 ns ticks since midnight to nanoseconds, exact.
fn ticks_to_nanos(ticks_100ns: u64) -> i64 {
    // At most 8.64 × 10¹³: fits.
    (ticks_100ns * 100) as i64
}

/// 1/300 s ticks since midnight to nanoseconds, truncated as SQL Server truncates them:
/// tick 1 is 3 333 333 ns.
fn nanos_of_300th(ticks_300th: u32) -> i64 {
    i64::from(ticks_300th) * 10_000_000 / 3
}

/// The number of an argument as an `int`, through [`vauban_types::convert`].
///
/// This is where a `decimal` truncates, a `money` rounds, a `bigint` overflows with 8115 and
/// a `float` with 232: none of those rules is restated here. `None` is `NULL`.
fn int_of(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<i32>> {
    let value = &args.values[index];
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let target = TypeInfo::new(SqlType::Int, true);
    match convert(value, &args.types[index], &target, None)? {
        Value::I32(n) => Ok(Some(n)),
        // `convert` towards `int` answers an `int`.
        _ => Ok(None),
    }
}

/// Adds `months` to a day count, clamping the day to the end of the target month.
///
/// `None` when the target year is outside `1..=9999`: the caller names the overflow.
fn add_months(days: i32, months: i64) -> Option<i32> {
    let (year, month, day) = civil_from_days(days);
    let total = i64::from(year) * 12 + i64::from(month) - 1 + months;
    let target_year = total.div_euclid(12);
    let target_month = (total.rem_euclid(12) + 1) as u8;
    if !(1..=9999).contains(&target_year) {
        return None;
    }
    let target_year = target_year as i32;
    let last = days_in_month(target_year, target_month);
    Some(days_from_civil(target_year, target_month, day.min(last)))
}

/// Adds `n` days to a day count, `None` when the result leaves an `i32` — which is far
/// outside the calendar anyway; the caller checks the calendar itself.
fn add_days(days: i32, n: i64) -> Option<i32> {
    i32::try_from(i64::from(days) + n).ok()
}

/// Rounds `numerator / denominator` half away from zero; `denominator` is positive.
fn div_round_half_away(numerator: i64, denominator: i64) -> i64 {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder.abs() * 2 >= denominator {
        quotient + numerator.signum()
    } else {
        quotient
    }
}

/// Rounds a time of day in 100 ns ticks to `scale` fractional digits, half up, and wraps
/// to midnight when the rounding reaches the next day. This is the `time` rounding of
/// `DATEADD`, which wraps where a `CAST` stays on the last instant of the day.
fn round_ticks_wrapping(ticks: i128, scale: u8) -> u64 {
    let unit = 10_i128.pow(u32::from(7 - scale.min(7)));
    let rounded = (ticks + unit / 2) / unit * unit;
    (rounded % TICKS_PER_DAY) as u64
}

/// Splits a total of 100 ns ticks since 0001-01-01 into a day and a time of day.
fn split_ticks(total: i128) -> (i128, u64) {
    (
        total.div_euclid(TICKS_PER_DAY),
        total.rem_euclid(TICKS_PER_DAY) as u64,
    )
}

/// The 517 of the type `shape` names.
fn overflow(shape: Shape) -> SqlError {
    SqlError::datetime_overflow(shape.error_name())
}

/// Adds `n` units of `part` to the calendar date `days`, for the parts that move the date.
///
/// Time parts do not move the date here: they move the time of day, which each shape
/// stores in its own resolution, and the carry follows. `None` is an overflow.
fn add_to_date(days: i32, part: DatePart, n: i32) -> Option<i32> {
    match Unit::of(part) {
        Unit::Months(per) => add_months(days, i64::from(n) * per),
        Unit::Days(per) => add_days(days, i64::from(n) * per),
        Unit::Nanos(_) | Unit::None => Some(days),
    }
}

/// `DATEADD` on a `date`.
fn dateadd_date(date: Date, part: DatePart, n: i32) -> SqlResult<Value> {
    let days = add_to_date(date.days, part, n).filter(|d| (0..=MAX_DAYS).contains(d));
    days.map(|days| Value::Date(Date { days }))
        .ok_or_else(|| overflow(Shape::DateOnly))
}

/// `DATEADD` on a `time(scale)`: the time of day moves and wraps around midnight.
fn dateadd_time(time: Time, part: DatePart, n: i32, scale: u8) -> Value {
    let delta = match Unit::of(part) {
        Unit::Nanos(unit) => nanos_to_ticks(i128::from(n) * i128::from(unit)),
        Unit::Months(_) | Unit::Days(_) | Unit::None => 0,
    };
    let total = (i128::from(time.ticks_100ns) + delta).rem_euclid(TICKS_PER_DAY);
    Value::Time(Time {
        ticks_100ns: round_ticks_wrapping(total, scale),
    })
}

/// Nanoseconds to 100 ns ticks, rounded half away from zero: the rounding of the number
/// of `DATEADD(nanosecond, …)`, where 50 ns is a tick and -50 ns is minus one.
fn nanos_to_ticks(nanos: i128) -> i128 {
    let quotient = nanos / 100;
    let remainder = nanos % 100;
    if remainder.abs() >= 50 {
        quotient + nanos.signum()
    } else {
        quotient
    }
}

/// `DATEADD` on a `datetime`, in its own 1/300 s ticks. `days` and `ticks` are relative to
/// 1900-01-01, as the value stores them; the pair returned is still unchecked.
fn dateadd_300th(dt: DateTime, part: DatePart, n: i32) -> Option<(i32, i64)> {
    let days = add_to_date(dt.days + DAYS_1900, part, n)? - DAYS_1900;
    let delta = match Unit::of(part) {
        // The milliseconds become ticks first (half away from zero), then are added:
        // see the module documentation.
        Unit::Nanos(1_000_000) => div_round_half_away(i64::from(n) * 3, 10),
        Unit::Nanos(unit) => i64::from(n) * (unit / 1_000_000_000) * TICKS_300TH_PER_SECOND,
        Unit::Months(_) | Unit::Days(_) | Unit::None => 0,
    };
    let total = i64::from(days) * TICKS_300TH_PER_DAY + i64::from(dt.ticks_300th) + delta;
    let days = i32::try_from(total.div_euclid(TICKS_300TH_PER_DAY)).ok()?;
    Some((days, total.rem_euclid(TICKS_300TH_PER_DAY)))
}

/// `DATEADD` on a `datetime`: the 1753-01-01..9999-12-31 range is checked on the ticks.
fn dateadd_datetime(dt: DateTime, part: DatePart, n: i32) -> SqlResult<Value> {
    let (days, ticks) = dateadd_300th(dt, part, n).ok_or_else(|| overflow(Shape::DateTime))?;
    let absolute = days + DAYS_1900;
    if !(DATETIME_MIN_DAYS..=MAX_DAYS).contains(&absolute) {
        return Err(overflow(Shape::DateTime));
    }
    Ok(Value::DateTime(DateTime {
        days,
        ticks_300th: ticks as u32,
    }))
}

/// `DATEADD` on a `smalldatetime`: the `datetime` result rounded to the minute, 30 s going
/// up, then checked against 1900-01-01..2079-06-06 23:59.
fn dateadd_smalldatetime(dt: DateTime, part: DatePart, n: i32) -> SqlResult<Value> {
    let (days, ticks) =
        dateadd_300th(dt, part, n).ok_or_else(SqlError::smalldatetime_intermediate_overflow)?;
    let minutes = (ticks + TICKS_300TH_PER_MINUTE / 2) / TICKS_300TH_PER_MINUTE;
    let (days, minutes) = if minutes >= 24 * 60 {
        (days + 1, 0)
    } else {
        (days, minutes)
    };
    let absolute = days + DAYS_1900;
    if !(DAYS_1900..=SMALLDATETIME_MAX_DAYS).contains(&absolute) {
        return Err(if (DATETIME_MIN_DAYS..=MAX_DAYS).contains(&absolute) {
            overflow(Shape::SmallDateTime)
        } else {
            SqlError::smalldatetime_intermediate_overflow()
        });
    }
    Ok(Value::DateTime(DateTime {
        days,
        ticks_300th: (minutes * TICKS_300TH_PER_MINUTE) as u32,
    }))
}

/// `DATEADD` at 100 ns on a local reading: the exact result must be on the calendar
/// (checked before any rounding), then it is rounded to `scale` by the conversion of
/// `types`, which stays on the last instant of 9999-12-31 rather than leaving it.
fn dateadd_100ns(
    local: DateTime2,
    part: DatePart,
    n: i32,
    shape: Shape,
    scale: u8,
) -> SqlResult<DateTime2> {
    let days = add_to_date(local.date.days, part, n).ok_or_else(|| overflow(shape))?;
    let delta = match Unit::of(part) {
        Unit::Nanos(unit) => nanos_to_ticks(i128::from(n) * i128::from(unit)),
        Unit::Months(_) | Unit::Days(_) | Unit::None => 0,
    };
    let total = i128::from(days) * TICKS_PER_DAY + i128::from(local.time.ticks_100ns) + delta;
    let (days, ticks) = split_ticks(total);
    if !(0..=i128::from(MAX_DAYS)).contains(&days) {
        return Err(overflow(shape));
    }
    let exact = Value::DateTime2(DateTime2 {
        date: Date { days: days as i32 },
        time: Time { ticks_100ns: ticks },
    });
    match convert(
        &exact,
        &TypeInfo::new(SqlType::DateTime2(7), true),
        &TypeInfo::new(SqlType::DateTime2(scale), true),
        None,
    )? {
        Value::DateTime2(rounded) => Ok(rounded),
        // A `datetime2` converts to a `datetime2`.
        _ => Err(overflow(shape)),
    }
}

/// `DATEADD` on a `datetimeoffset(scale)`: computed on the local reading, offset kept, and
/// the universal instant must be on the calendar as well.
fn dateadd_offset(
    local: DateTime2,
    offset_minutes: i16,
    part: DatePart,
    n: i32,
    scale: u8,
) -> SqlResult<Value> {
    let shape = Shape::WithOffset(scale);
    let local = dateadd_100ns(local, part, n, shape, scale)?;
    let total = i128::from(local.date.days) * TICKS_PER_DAY + i128::from(local.time.ticks_100ns)
        - i128::from(offset_minutes) * 60 * TICKS_PER_SECOND;
    let (days, ticks) = split_ticks(total);
    if !(0..=i128::from(MAX_DAYS)).contains(&days) {
        return Err(overflow(shape));
    }
    Ok(Value::DateTimeOffset(DateTimeOffset {
        utc: DateTime2 {
            date: Date { days: days as i32 },
            time: Time { ticks_100ns: ticks },
        },
        offset_minutes,
    }))
}

/// Evaluates `DATEADD(datepart, number, date)`.
fn dateadd_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let part = keyword_of(args, "dateadd")?;
    let shape = Shape::of(&args.types[2], StringReading::AsDateTime)?;
    // `NULL` before the datepart check: `DATEADD(hour, 1, CAST(NULL AS date))` is `NULL`.
    let (Some(n), false) = (int_of(args, 1)?, matches!(args.values[2], Value::Null)) else {
        return Ok(Value::Null);
    };
    // The value is converted **before** the datepart check: a string an offset makes
    // unreadable as a `datetime` is 241 on SQL Server under `iso_week`
    // (`dateadd_iso_week_varchar`), not 9810. See the module documentation.
    let target = TypeInfo::new(shape.canonical(), true);
    let value = convert(&args.values[2], &args.types[2], &target, None)?;
    if !shape.accepts_add(part) {
        return Err(SqlError::datepart_not_supported(
            keyword_name(part),
            "dateadd",
            shape.error_name(),
        ));
    }
    match (shape, value) {
        (Shape::DateOnly, Value::Date(d)) => dateadd_date(d, part, n),
        (Shape::TimeOnly(scale), Value::Time(t)) => Ok(dateadd_time(t, part, n, scale)),
        (Shape::DateTime, Value::DateTime(dt)) => dateadd_datetime(dt, part, n),
        (Shape::SmallDateTime, Value::DateTime(dt)) => dateadd_smalldatetime(dt, part, n),
        (Shape::DateTime2(scale), Value::DateTime2(dt)) => {
            dateadd_100ns(dt, part, n, shape, scale).map(Value::DateTime2)
        }
        (Shape::WithOffset(scale), Value::DateTime2(local)) => {
            let offset = match &args.values[2] {
                Value::DateTimeOffset(dto) => dto.offset_minutes,
                // A string read as a `datetimeoffset` does not reach `DATEADD`.
                _ => 0,
            };
            dateadd_offset(local, offset, part, n, scale)
        }
        // `convert` towards `shape.canonical()` answers that type; nothing else can appear.
        _ => Ok(Value::Null),
    }
}

/// Result type of `DATEADD`: the type of the third argument, always nullable.
///
/// The number is refused at binding for the families SQL Server refuses (8116 on
/// argument 2 for a string, a `bit`, a binary, a date type and a `uniqueidentifier`) and
/// accepted for the
/// integer, exact, approximate and money families, whose conversion to `int` happens at
/// evaluation.
fn dateadd_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    match args[1].ty.family() {
        TypeFamily::Integer
        | TypeFamily::ExactNumeric
        | TypeFamily::ApproxNumeric
        | TypeFamily::Money => {}
        TypeFamily::Bit
        | TypeFamily::Character
        | TypeFamily::Binary
        | TypeFamily::DateTime
        | TypeFamily::Guid => return Err(invalid_argument_type(&args[1].ty, 2, "dateadd")),
    }
    let shape = Shape::of(&args[2], StringReading::AsDateTime)?;
    Ok(TypeInfo::new(shape.result_type(), true))
}

/// The count of `part` boundaries from the origin to `instant`.
///
/// `week` boundaries fall on Sundays whatever `DATEFIRST` says: 0001-01-01 is a Monday, so
/// the Sundays are the days congruent to 6, and `(days + 1) / 7` changes on each of them.
fn boundaries(instant: Instant, part: DatePart) -> i128 {
    match Unit::of(part) {
        Unit::Months(per) => {
            let (year, month, _) = civil_from_days(instant.days);
            (i128::from(year) * 12 + i128::from(month) - 1).div_euclid(i128::from(per))
        }
        Unit::Days(per) => (i128::from(instant.days) + 1).div_euclid(i128::from(per)),
        Unit::Nanos(unit) => instant.total_nanos().div_euclid(i128::from(unit)),
        Unit::None => 0,
    }
}

/// Evaluates `DATEDIFF(datepart, start, end)`.
fn datediff_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let part = keyword_of(args, "datediff")?;
    let (Some(start), Some(end)) = (instant_of(args, 1)?, instant_of(args, 2)?) else {
        return Ok(Value::Null);
    };
    if Unit::of(part) == Unit::None {
        return Err(SqlError::datediff_datepart_not_supported(
            keyword_name(part),
            Shape::of(&args.types[1], StringReading::AsDateTimeOffset)?.error_name(),
            Shape::of(&args.types[2], StringReading::AsDateTimeOffset)?.error_name(),
        ));
    }
    let count = boundaries(end, part) - boundaries(start, part);
    i32::try_from(count)
        .map(Value::I32)
        .map_err(|_| SqlError::datediff_overflow())
}

/// Result type of `DATEDIFF`: `int`, always nullable; 206 for a type with no date reading.
fn datediff_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Shape::of(&args[1], StringReading::AsDateTimeOffset)?;
    Shape::of(&args[2], StringReading::AsDateTimeOffset)?;
    Ok(TypeInfo::new(SqlType::Int, true))
}

/// The last day of the month `months` months after the month of `days`, or the 517 of a
/// `date` when that month is off the calendar.
fn end_of_month(days: i32, months: i64) -> SqlResult<Value> {
    let moved = add_months(days, months).ok_or_else(SqlError::eomonth_overflow)?;
    let (year, month, _) = civil_from_days(moved);
    Ok(Value::Date(Date {
        days: days_from_civil(year, month, days_in_month(year, month)),
    }))
}

/// Evaluates `EOMONTH(date [, months])`.
fn eomonth_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    if matches!(args.values[0], Value::Null) {
        return Ok(Value::Null);
    }
    if !eomonth_reads(&args.types[0]) {
        // The nullable `int` [`eomonth_type`] let through for the sake of a bare `NULL`
        // carried a value after all: the 8116 of the binding, one step late.
        return Err(invalid_argument_type(&args.types[0].ty, 1, "eomonth"));
    }
    // A `NULL` offset is zero, from a literal and from a variable alike.
    let months = match args.values.get(1) {
        Some(_) => int_of(args, 1)?.unwrap_or(0),
        None => 0,
    };
    let target = TypeInfo::new(SqlType::Date, true);
    match convert(&args.values[0], &args.types[0], &target, None)? {
        Value::Date(d) => end_of_month(d.days, i64::from(months)),
        _ => Ok(Value::Null),
    }
}

/// Whether `EOMONTH` reads a value of this type as a date: every date type but `time`,
/// and every string.
fn eomonth_reads(ty: &TypeInfo) -> bool {
    match (&ty.ty, ty.ty.family()) {
        (SqlType::Time(_), _) => false,
        (_, TypeFamily::DateTime | TypeFamily::Character) => true,
        _ => false,
    }
}

/// Result type of `EOMONTH`: `date`, always nullable.
///
/// 8116 on argument 1 for everything that is not a date type or a string, `time`
/// included; 206 on argument 2 for a date type or a `uniqueidentifier`, which have no
/// implicit `int`.
///
/// One exception, for the binder's sake: a **nullable `int`** is let through, and refused
/// by [`eomonth_eval`] if it carries a value. `SELECT EOMONTH(NULL);` answers `NULL` on SQL
/// Server, and the `binder` types a bare `NULL` as a nullable `int` when no sibling says
/// better (`bind_literal`, `retype_untyped_nulls`); refusing every `int` here would refuse
/// that `NULL`. A literal `0` is an `int` that cannot be `NULL`, so `SELECT EOMONTH(0);`
/// keeps its 8116 at binding. The exception is wider than the bare `NULL` it serves:
/// `EOMONTH(CAST(NULL AS int))` and `EOMONTH(@i)` with `@i int` unset answer `NULL`
/// here, where SQL Server refuses the typed `int` with 8116 at binding, as it does
/// `EOMONTH(0)`: a deliberate difference from SQL Server.
fn eomonth_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let bare_null = args[0].ty == SqlType::Int && args[0].nullable;
    if !eomonth_reads(&args[0]) && !bare_null {
        return Err(invalid_argument_type(&args[0].ty, 1, "eomonth"));
    }
    if let Some(offset) = args.get(1) {
        int_argument_type(offset)?;
    }
    Ok(TypeInfo::new(SqlType::Date, true))
}

/// The bind-time check of an argument that must become an `int`: a date type or a
/// `uniqueidentifier` is 206 (type clash with `int`), a `datetime` or a `smalldatetime`
/// 257 (no implicit conversion to `int`).
///
/// 206 for `date`, `time`, `datetime2`, `datetimeoffset` and `uniqueidentifier`, 257 for
/// `datetime` and `smalldatetime` (same conversions as `datetime`), on both
/// `DATEFROMPARTS` and the offset of `EOMONTH`.
fn int_argument_type(ty: &TypeInfo) -> SqlResult<()> {
    match &ty.ty {
        SqlType::DateTime | SqlType::SmallDateTime => Err(
            SqlError::implicit_conversion_not_allowed(ty.ty.error_name(), "int"),
        ),
        other => match other.family() {
            TypeFamily::DateTime | TypeFamily::Guid => {
                Err(SqlError::operand_type_clash(other.error_name(), "int"))
            }
            _ => Ok(()),
        },
    }
}

/// Evaluates `DATEFROMPARTS(year, month, day)`.
fn datefromparts_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(year), Some(month), Some(day)) =
        (int_of(args, 0)?, int_of(args, 1)?, int_of(args, 2)?)
    else {
        return Ok(Value::Null);
    };
    let (Ok(month), Ok(day)) = (u8::try_from(month), u8::try_from(day)) else {
        return Err(SqlError::cannot_construct_type("date"));
    };
    if !is_valid_civil(year, month, day) {
        return Err(SqlError::cannot_construct_type("date"));
    }
    Ok(Value::Date(Date {
        days: days_from_civil(year, month, day),
    }))
}

/// Result type of `DATEFROMPARTS`: `date`, nullable when an argument is.
fn datefromparts_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    for arg in args {
        int_argument_type(arg)?;
    }
    Ok(TypeInfo::new(
        SqlType::Date,
        args.iter().any(|a| a.nullable),
    ))
}

/// `DATEADD`: Microsoft Learn, "DATEADD (Transact-SQL)".
const DATEADD_DEF: FunctionDef = FunctionDef {
    name: "DATEADD",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(3),
    return_type: dateadd_type,
    eval: dateadd_eval,
    aggregate: None,
};

/// `DATEDIFF`: Microsoft Learn, "DATEDIFF (Transact-SQL)". Deterministic: its `week` does
/// not read `SET DATEFIRST` (`datediff_counts_boundaries`).
const DATEDIFF_DEF: FunctionDef = FunctionDef {
    name: "DATEDIFF",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(3),
    return_type: datediff_type,
    eval: datediff_eval,
    aggregate: None,
};

/// `EOMONTH`: Microsoft Learn, "EOMONTH (Transact-SQL)".
const EOMONTH_DEF: FunctionDef = FunctionDef {
    name: "EOMONTH",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Range(1, 2),
    return_type: eomonth_type,
    eval: eomonth_eval,
    aggregate: None,
};

/// `DATEFROMPARTS`: Microsoft Learn, "DATEFROMPARTS (Transact-SQL)".
const DATEFROMPARTS_DEF: FunctionDef = FunctionDef {
    name: "DATEFROMPARTS",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(3),
    return_type: datefromparts_type,
    eval: datefromparts_eval,
    aggregate: None,
};

/// Registers the functions of this module.
pub(crate) fn register_all() {
    register(DATEADD_DEF);
    register(DATEDIFF_DEF);
    register(EOMONTH_DEF);
    register(DATEFROMPARTS_DEF);
}

#[cfg(test)]
mod tests;
