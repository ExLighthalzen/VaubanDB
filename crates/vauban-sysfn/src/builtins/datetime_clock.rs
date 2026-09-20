//! The clock (`GETDATE`, `GETUTCDATE`, `SYSDATETIME`, `SYSUTCDATETIME`) and the extraction
//! of one component of a date (`DATEPART`, `DATENAME`, `YEAR`, `MONTH`, `DAY`).
//!
//! # Where the calendar is
//!
//! Not here. Days to civil dates and the epoch of `datetime` come from
//! [`vauban_types::calendar`], and a conversion between two date types goes through
//! [`vauban_types::convert`]: two calendars would be two chances to disagree on
//! [`DAYS_1900`]. What this file owns is the table of the `datepart` keywords, which is a
//! T-SQL vocabulary and not a calendar, plus the two arithmetic helpers
//! [`is_leap_year`] and [`days_in_month`] that `datetime_calc` reuses (the equivalents
//! of `vauban_types::calendar` are private to that crate).
//!
//! # The clocks
//!
//! `GETDATE()` cannot be pinned to a value; what a test can fix is the **type** and the
//! **precision** of each clock and the relations between them
//! (`getdate_converts_to_datetime`, `sysdatetime_keeps_full_precision`):
//!
//! | property | answer |
//! |---|---|
//! | type of `GETDATE`, `GETUTCDATE` | `datetime`, precision 23, scale 3, **not** nullable |
//! | type of `SYSDATETIME`, `SYSUTCDATETIME` | `datetime2`, precision 27, scale 7, not nullable |
//! | type of `DATEPART`, `YEAR`, `MONTH`, `DAY` | `int`, **nullable even on a literal** |
//! | type of `DATENAME` | `nvarchar(30)` |
//! | rounding of `GETDATE` | 1/300 s, not milliseconds: the last digit of the millisecond is 0, 3 or 7 |
//! | local vs UTC | the two clocks share their sub-minute part: the offset is a whole number of minutes |
//! | stability inside a statement | `GETDATE() = GETDATE()` is 1: one instant per statement |
//!
//! # `DATEPART` on the argument types
//!
//! A `datepart` that the type cannot answer raises 9810 rather than returning zero
//! (`DATEPART(hour, CAST('2020-03-01' AS date))`). The matrix is in
//! [`DateKind::accepts`]. Two fields of that 9810 carry information:
//!
//! - The **function name** is not in each case the function that was called. `DATEPART`
//!   says `datepart` and `DATENAME` says `datename`, but `YEAR`, `MONTH` and `DAY` say
//!   `datepart` too: `SELECT YEAR(CAST('13:45:30' AS time(7)));` names `datepart`
//!   (`the_shorthands_name_datepart_in_9810`). See [`year_month_day_eval`].
//! - The **state** depends on both the type and the function: 2, 3, 6 for `datepart` on
//!   `date`, `time`, `datetime`; 4, 5, 7 for `datename` on the same three. `YEAR`, `MONTH`
//!   and `DAY` follow `datepart` here as well (state 3 on a `time`).
//!   `SqlError::datepart_not_supported` supplies the states.
//!
//! A type that has no date reading is **not** error 8116 but error 206: `SELECT
//! DATEPART(year, CAST(... AS uniqueidentifier));` is a type clash with `datetime`, state
//! 2 (`a_uniqueidentifier_is_an_operand_type_clash`). The argument is refused by the
//! implicit conversion, not by the function, the distinction `builtins::args` documents.
//! Among the types [`vauban_types::SqlType`] can express, `uniqueidentifier` is the one
//! refused that way: `binary`, `bit`, `int`, `decimal`, `money` and `float` convert to a
//! `datetime` and answer 1900. SQL Server also answers 206 state 2 for `xml`, `geography`
//! and `hierarchyid`, and 257 state 3 for `sql_variant`; those four types do not exist in
//! `SqlType` and are not reachable here.

use vauban_errors::{SqlError, SqlResult};
use vauban_types::calendar::{DAYS_1900, civil_from_days, days_from_civil};
use vauban_types::{
    Len, SqlString, SqlType, TypeFamily, TypeInfo, Value, convert, default_display,
};

use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// A component of a date or of a time of day, as the first argument of `DATEPART` and
/// `DATENAME` names it.
///
/// The list is closed: SQL Server accepts these fifteen components and nothing else. Near
/// misses such as `dayofweek`, `doy`, `isoweek`, `iso_wk`, `mon`, `sec`, `min`, `h`,
/// `yyy`, `milliseconds`, `weeks`, `qtr`, `mcsec`, `nsec` and `epoch` are 155
/// (`parse_datepart_accepts_every_spelling`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePart {
    /// `year`, `yy`, `yyyy`.
    Year,
    /// `quarter`, `qq`, `q`.
    Quarter,
    /// `month`, `mm`, `m`.
    Month,
    /// `dayofyear`, `dy`, `y`.
    DayOfYear,
    /// `day`, `dd`, `d`.
    Day,
    /// `week`, `wk`, `ww`: week 1 is the week holding 1 January, cut by `SET DATEFIRST`.
    Week,
    /// `iso_week`, `isowk`, `isoww`: ISO 8601 week, independent of `SET DATEFIRST`.
    IsoWeek,
    /// `weekday`, `dw`, `w`: day of the week numbered from `SET DATEFIRST`.
    Weekday,
    /// `hour`, `hh`.
    Hour,
    /// `minute`, `mi`, `n`.
    Minute,
    /// `second`, `ss`, `s`.
    Second,
    /// `millisecond`, `ms`.
    Millisecond,
    /// `microsecond`, `mcs`.
    Microsecond,
    /// `nanosecond`, `ns`.
    Nanosecond,
    /// `tzoffset`, `tz`: offset from UTC, in minutes.
    TzOffset,
}

/// Every accepted spelling of a `datepart`, and the component it names.
///
/// Each row is one keyword; the table is walked case-insensitively by [`parse_datepart`].
/// `n` is *minute* and `s` is *second*: neither is an abbreviation of nanosecond, which is
/// `ns`. `y` is *day of year*, not year, and `w` is *weekday*, not week: the three traps
/// of the table.
const DATEPART_KEYWORDS: [(&str, DatePart); 40] = [
    ("year", DatePart::Year),
    ("yy", DatePart::Year),
    ("yyyy", DatePart::Year),
    ("quarter", DatePart::Quarter),
    ("qq", DatePart::Quarter),
    ("q", DatePart::Quarter),
    ("month", DatePart::Month),
    ("mm", DatePart::Month),
    ("m", DatePart::Month),
    ("dayofyear", DatePart::DayOfYear),
    ("dy", DatePart::DayOfYear),
    ("y", DatePart::DayOfYear),
    ("day", DatePart::Day),
    ("dd", DatePart::Day),
    ("d", DatePart::Day),
    ("week", DatePart::Week),
    ("wk", DatePart::Week),
    ("ww", DatePart::Week),
    ("weekday", DatePart::Weekday),
    ("dw", DatePart::Weekday),
    ("w", DatePart::Weekday),
    ("hour", DatePart::Hour),
    ("hh", DatePart::Hour),
    ("minute", DatePart::Minute),
    ("mi", DatePart::Minute),
    ("n", DatePart::Minute),
    ("second", DatePart::Second),
    ("ss", DatePart::Second),
    ("s", DatePart::Second),
    ("millisecond", DatePart::Millisecond),
    ("ms", DatePart::Millisecond),
    ("microsecond", DatePart::Microsecond),
    ("mcs", DatePart::Microsecond),
    ("nanosecond", DatePart::Nanosecond),
    ("ns", DatePart::Nanosecond),
    ("iso_week", DatePart::IsoWeek),
    ("isowk", DatePart::IsoWeek),
    ("isoww", DatePart::IsoWeek),
    ("tzoffset", DatePart::TzOffset),
    ("tz", DatePart::TzOffset),
];

/// Resolves a `datepart` keyword, whatever its case; `None` when it is not one.
///
/// `binder` is the intended caller: SQL Server refuses an unknown keyword while binding,
/// before a row exists, and only `binder` sees the keyword at that moment (`eval` gets it
/// as a value, `return_type` does not get it at all). The `eval` of this file calls it
/// again as a safety net (`datepart_eval` is private, so the link is spelled out rather
/// than made).
pub fn parse_datepart(name: &str) -> Option<DatePart> {
    DATEPART_KEYWORDS
        .iter()
        .find(|(keyword, _)| keyword.eq_ignore_ascii_case(name))
        .map(|(_, part)| *part)
}

/// Gregorian leap year: divisible by 4, except centuries that are not divisible by 400.
///
/// Here rather than in `vauban_types::calendar`, whose own copy is private to that crate,
/// because `datetime_calc` needs it. A unit test checks that the two agree, through the
/// public [`days_from_civil`].
// Allowed: the `datepart` extraction reads the calendar of `types` instead.
#[allow(dead_code)]
pub(crate) fn is_leap_year(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Number of days in month `m` of year `y`, `0` when `m` is not a month.
// Allowed for the same reason as [`is_leap_year`]: unused by the extraction itself.
#[allow(dead_code)]
pub(crate) fn days_in_month(y: i32, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Nanoseconds in one second.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// 1/300 s ticks in one second, the resolution of a `datetime`.
const TICKS_300TH_PER_SECOND: u32 = 300;

/// 100 ns ticks in one second, the resolution of a `time(7)`.
const TICKS_100NS_PER_SECOND: u64 = 10_000_000;

/// The English names of the months, index 0 = January (`DATENAME(month, …)`).
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The English names of the days, index 0 = Monday, the ISO 8601 numbering
/// (`DATENAME(weekday, …)`). Independent of `SET DATEFIRST`, which renumbers
/// `DATEPART(weekday, …)` but never renames a day.
const DAY_NAMES: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// What a typed argument of `DATEPART` can answer, once the `binder` has typed it.
///
/// The shape matters twice: it says which conversion brings the value into a canonical
/// form, and which `datepart` the type is allowed to answer; see
/// [`DateKind::accepts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateKind {
    /// `date`: a calendar date and no time of day.
    DateOnly,
    /// `time(s)`: a time of day and no calendar date.
    TimeOnly,
    /// `datetime`, `smalldatetime`, and every type that converts implicitly to `datetime`
    /// (`int`, `float`, `decimal`, `money`, `bit`): a date and a time, no zone.
    DateAndTime,
    /// `datetime2(s)`: a date and a time at 100 ns, no zone but `tzoffset` answers 0.
    Full,
    /// `datetimeoffset(s)` and the character types, which SQL Server reads as one.
    WithOffset,
}

impl DateKind {
    /// The shape of a declared type, `None` when the type is not a date and does not
    /// convert to one.
    ///
    /// Character types land in [`DateKind::WithOffset`]:
    /// `DATEPART(tzoffset, '2020-03-01 13:45:30.123')` answers `0` instead of raising
    /// 9810, and `DATEPART(nanosecond, …)` on the same string answers `123000000` and not
    /// the `123333333` a `datetime` would give — so the string is read as a
    /// `datetimeoffset`, not as a `datetime`.
    fn of(ty: &SqlType) -> Option<Self> {
        match ty {
            SqlType::Date => Some(DateKind::DateOnly),
            SqlType::Time(_) => Some(DateKind::TimeOnly),
            SqlType::DateTime | SqlType::SmallDateTime => Some(DateKind::DateAndTime),
            SqlType::DateTime2(_) => Some(DateKind::Full),
            SqlType::DateTimeOffset(_) => Some(DateKind::WithOffset),
            other => match other.family() {
                TypeFamily::Character => Some(DateKind::WithOffset),
                // `SELECT DATEPART(year, 123);` answers 1900: the number is converted to a
                // `datetime`, which is also the type the 9810 of `tzoffset` names.
                // `SELECT DATEPART(year, CAST(0x01 AS binary(1)));` answers 1900 too:
                // binary is read as the stored bytes of a `datetime`, not refused.
                TypeFamily::Integer
                | TypeFamily::ExactNumeric
                | TypeFamily::ApproxNumeric
                | TypeFamily::Money
                | TypeFamily::Bit
                | TypeFamily::Binary => Some(DateKind::DateAndTime),
                // The only type with no date reading at all.
                TypeFamily::Guid | TypeFamily::DateTime => None,
            },
        }
    }

    /// Whether this shape can answer `part`.
    ///
    /// A `date` refuses the time components, a `time` refuses the date components, and
    /// the two types that carry a zone (or a string, read as one) answer `tzoffset`;
    /// `datetime2` answers it too, with `0` (`datepart_refuses_what_the_type_cannot_answer`).
    fn accepts(self, part: DatePart) -> bool {
        match self {
            DateKind::DateOnly => !is_time_part(part) && part != DatePart::TzOffset,
            DateKind::TimeOnly => is_time_part(part),
            DateKind::DateAndTime => part != DatePart::TzOffset,
            DateKind::Full | DateKind::WithOffset => true,
        }
    }

    /// The type SQL Server names in the 9810 message for this shape.
    ///
    /// `smalldatetime`, `int` and `float` all print `datetime`: the value has been brought
    /// to a `datetime` before the component is looked for.
    fn error_name(self) -> &'static str {
        match self {
            DateKind::DateOnly => "date",
            DateKind::TimeOnly => "time",
            DateKind::DateAndTime | DateKind::Full | DateKind::WithOffset => "datetime",
        }
    }

    /// The type every value of this shape is converted to before its components are read.
    ///
    /// [`DateKind::WithOffset`] reads through a `datetime2`, **not** through a
    /// `datetimeoffset`: [`vauban_types::Value::DateTimeOffset`] stores the *universal*
    /// instant, while `DATEPART` reports the *local* wall clock the value was written with.
    /// `DATEPART(hour, CAST('2020-03-01 01:30:00 +05:00' AS datetimeoffset(7)))` is 1 and
    /// its `day` is 1, where the universal instant is the 29th of February at
    /// 20:30. Converting to `datetime2` is how `types` applies that shift
    /// (`convert::datetime::local_of_offset`), so the shift is not restated here. The
    /// offset itself is read separately, by [`offset_of`].
    fn canonical(self) -> SqlType {
        match self {
            DateKind::DateOnly => SqlType::Date,
            DateKind::TimeOnly => SqlType::Time(7),
            DateKind::DateAndTime => SqlType::DateTime,
            DateKind::Full | DateKind::WithOffset => SqlType::DateTime2(7),
        }
    }
}

/// Whether `part` names a component of the time of day rather than of the calendar date.
///
/// `tzoffset` is neither: it belongs to the zone, and is handled apart everywhere.
fn is_time_part(part: DatePart) -> bool {
    matches!(
        part,
        DatePart::Hour
            | DatePart::Minute
            | DatePart::Second
            | DatePart::Millisecond
            | DatePart::Microsecond
            | DatePart::Nanosecond
    )
}

/// A date-time value broken into the pieces a `datepart` reads.
///
/// The fraction is kept in **nanoseconds** and not in the 100 ns of a `time(7)`, because a
/// `datetime` does not measure in either: it counts 1/300 s, and SQL Server reports
/// `DATEPART(nanosecond, CAST('…30.123' AS datetime))` as `123333333`, the exact third of a
/// second, where a detour through `datetime2(7)` would have answered `123333300`. That is
/// the reason this structure exists instead of a plain `DateTime2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Instant {
    /// Days since 0001-01-01, `None` for a `time`, which carries no date.
    days: Option<i32>,
    /// Whole seconds since midnight, `0` for a `date`.
    seconds: u32,
    /// Fraction of the second, in nanoseconds, `0..1_000_000_000`.
    nanos: u64,
    /// Offset from UTC in minutes; `0` when the type carries no zone.
    offset_minutes: i32,
}

/// Splits a `time(7)` tick count into whole seconds and a fraction in nanoseconds.
fn split_ticks_100ns(ticks_100ns: u64) -> (u32, u64) {
    let seconds = ticks_100ns / TICKS_100NS_PER_SECOND;
    let fraction = ticks_100ns % TICKS_100NS_PER_SECOND;
    // 100 ns units to nanoseconds: exact, no rounding.
    (seconds as u32, fraction * 100)
}

/// Breaks a canonical value into its components.
///
/// `value` has already been converted to `kind.canonical()`, so only the matching variant
/// can appear; anything else would be a bug in [`vauban_types::convert`] rather than a
/// query error, hence the `None`, which the caller turns into `NULL`.
fn split(value: &Value) -> Option<Instant> {
    match value {
        Value::Date(d) => Some(Instant {
            days: Some(d.days),
            seconds: 0,
            nanos: 0,
            offset_minutes: 0,
        }),
        Value::Time(t) => {
            let (seconds, nanos) = split_ticks_100ns(t.ticks_100ns);
            Some(Instant {
                days: None,
                seconds,
                nanos,
                offset_minutes: 0,
            })
        }
        Value::DateTime(dt) => Some(Instant {
            days: Some(dt.days + DAYS_1900),
            seconds: dt.ticks_300th / TICKS_300TH_PER_SECOND,
            // 1/300 s to nanoseconds, truncated as SQL Server truncates it: tick 37 of a
            // second reads 123 333 333 ns, not 123 333 300.
            nanos: u64::from(dt.ticks_300th % TICKS_300TH_PER_SECOND) * NANOS_PER_SECOND
                / u64::from(TICKS_300TH_PER_SECOND),
            offset_minutes: 0,
        }),
        Value::DateTime2(dt) => {
            let (seconds, nanos) = split_ticks_100ns(dt.time.ticks_100ns);
            Some(Instant {
                days: Some(dt.date.days),
                seconds,
                nanos,
                offset_minutes: 0,
            })
        }
        _ => None,
    }
}

/// Day of the week of `days`, ISO 8601 numbering: 1 = Monday … 7 = Sunday.
///
/// 0001-01-01 is a Monday in the proleptic Gregorian calendar; three distant points:
/// 1900-01-01 Monday, 2000-01-01 Saturday, 2020-03-01 Sunday.
fn iso_weekday(days: i32) -> i32 {
    days.rem_euclid(7) + 1
}

/// `DATEPART(weekday, …)`: the day of the week numbered from `SET DATEFIRST`.
///
/// With `DATEFIRST` 7 (the `us_english` default) Sunday is 1, with `DATEFIRST` 1 Monday
/// is 1.
fn weekday(days: i32, datefirst: u8) -> i32 {
    (iso_weekday(days) + 7 - i32::from(datefirst)) % 7 + 1
}

/// `DATEPART(week, …)`: the SQL Server week, where week 1 holds 1 January.
///
/// A week starts on the `DATEFIRST` day, so the answer moves with the setting
/// (`datepart_iso_week` checks dates spread over year boundaries).
fn sql_week(year: i32, day_of_year: i32, datefirst: u8) -> i32 {
    // Week 2 starts on the first day of the week that follows the 1st of January, so the
    // 1st of January itself sits `first_weekday - 1` days into week 1.
    let first_weekday = weekday(days_from_civil(year, 1, 1), datefirst);
    (day_of_year + first_weekday - 2) / 7 + 1
}

/// `DATEPART(iso_week, …)`: the ISO 8601 week, whose week 1 is the one holding the first
/// Thursday of the year.
///
/// Computed through that Thursday, which is what makes 2021-01-01 (a Friday) belong to week
/// 53 of 2020 while `week` calls it week 1. Independent of `SET DATEFIRST`.
fn iso_week(days: i32) -> i32 {
    let thursday = days - (iso_weekday(days) - 4);
    let (thursday_year, _, _) = civil_from_days(thursday);
    (thursday - days_from_civil(thursday_year, 1, 1)) / 7 + 1
}

/// The value of `part` on `instant`, under `datefirst`.
///
/// The caller has already checked, through [`DateKind::accepts`], that the type can answer
/// `part`: a component the value does not carry cannot be asked for here.
fn component(instant: &Instant, part: DatePart, datefirst: u8) -> i32 {
    let (year, month, day) = match instant.days {
        Some(days) => civil_from_days(days),
        // Only the time components are reachable without a date, and none of them reads
        // this triple.
        None => (1, 1, 1),
    };
    match part {
        DatePart::Year => year,
        DatePart::Quarter => (i32::from(month) - 1) / 3 + 1,
        DatePart::Month => i32::from(month),
        DatePart::Day => i32::from(day),
        DatePart::DayOfYear => day_of_year(instant, year),
        DatePart::Week => sql_week(year, day_of_year(instant, year), datefirst),
        DatePart::IsoWeek => instant.days.map_or(1, iso_week),
        DatePart::Weekday => instant.days.map_or(1, |days| weekday(days, datefirst)),
        DatePart::Hour => (instant.seconds / 3600) as i32,
        DatePart::Minute => ((instant.seconds / 60) % 60) as i32,
        DatePart::Second => (instant.seconds % 60) as i32,
        DatePart::Millisecond => (instant.nanos / 1_000_000) as i32,
        DatePart::Microsecond => (instant.nanos / 1_000) as i32,
        DatePart::Nanosecond => instant.nanos as i32,
        DatePart::TzOffset => instant.offset_minutes,
    }
}

/// Day of the year, 1 for 1 January.
fn day_of_year(instant: &Instant, year: i32) -> i32 {
    instant
        .days
        .map_or(1, |days| days - days_from_civil(year, 1, 1) + 1)
}

/// Brings the argument of `DATEPART` into the canonical shape of its declared type.
///
/// `Ok(None)` means `NULL`, which propagates. The conversion is
/// [`vauban_types::convert`]: reading a string as a date, or a number as a `datetime`, is a
/// rule of `types` and is not restated here.
fn instant_of(args: &EvalArgs<'_>, index: usize, kind: DateKind) -> SqlResult<Option<Instant>> {
    let value = &args.values[index];
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let target = TypeInfo::new(kind.canonical(), true);
    let converted = convert(value, &args.types[index], &target, None)?;
    let Some(mut instant) = split(&converted) else {
        return Ok(None);
    };
    if kind == DateKind::WithOffset {
        instant.offset_minutes = offset_of(args, index)?;
    }
    Ok(Some(instant))
}

/// The zone offset of the argument, in minutes.
///
/// A second conversion, of the same value to `datetimeoffset(7)`, rather than a local rule
/// for reading an offset out of a string: a `varchar` carries one when it is written
/// (`'… +02:30'` gives 150) and none otherwise (`0`), and deciding that is `types`' job.
/// Only [`DateKind::WithOffset`] reaches here; a `datetime2` answers `0` without asking,
/// which is what SQL Server answers too.
fn offset_of(args: &EvalArgs<'_>, index: usize) -> SqlResult<i32> {
    let target = TypeInfo::new(SqlType::DateTimeOffset(7), true);
    match convert(&args.values[index], &args.types[index], &target, None)? {
        Value::DateTimeOffset(dto) => Ok(i32::from(dto.offset_minutes)),
        _ => Ok(0),
    }
}

/// The `datepart` keyword an evaluation received, or error 155.
///
/// `function` is the word printed before `option`, and it is the **name of the
/// function**, not the kind of argument: `SELECT DATENAME(foo, GETDATE());` names
/// `datename` (`datepart_unknown_is_155`).
fn keyword_of(args: &EvalArgs<'_>, function: &str) -> SqlResult<DatePart> {
    let name = match &args.values[0] {
        Value::String(s) => s.text.clone(),
        // The binder rejects expression-shaped dateparts with 1023. This fallback
        // formats direct evaluator inputs for the keyword lookup.
        other => default_display(other, &args.types[0]),
    };
    parse_datepart(&name).ok_or_else(|| SqlError::not_a_recognized_option(&name, function))
}

/// The shape of the date argument, or the error its type deserves.
///
/// Error **206**, not 8116: a `uniqueidentifier` is refused here through the implicit
/// conversion to `datetime`, state 2, on `DATEPART`, `DATENAME` and `YEAR` alike, which
/// is why neither the position of the argument nor the name of the function appears in
/// it.
fn kind_of(ty: &TypeInfo) -> SqlResult<DateKind> {
    DateKind::of(&ty.ty).ok_or_else(|| SqlError::operand_type_clash(ty.ty.error_name(), "datetime"))
}

/// Result type of `DATEPART`, `YEAR`, `MONTH` and `DAY`: `int`, nullable.
///
/// Nullable even when the argument is a literal that cannot be `NULL`, where `GETDATE()`
/// is not nullable; that is the reason nullability is not derived from the argument here.
fn datepart_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    kind_of(&args[1])?;
    Ok(TypeInfo::new(SqlType::Int, true))
}

/// Result type of `DATENAME`: `nvarchar(30)`, nullable.
///
/// Thirty UCS-2 characters, 60 bytes.
fn datename_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    kind_of(&args[1])?;
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(30)), true))
}

/// Result type of `YEAR`, `MONTH` and `DAY`: `int`, nullable. One argument, so the type
/// that can raise 8116 is the first one.
fn year_month_day_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    kind_of(&args[0])?;
    Ok(TypeInfo::new(SqlType::Int, true))
}

/// Result type of `GETDATE` and `GETUTCDATE`: `datetime`, never `NULL`.
fn datetime_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::DateTime, false))
}

/// Result type of `SYSDATETIME` and `SYSUTCDATETIME`: `datetime2(7)`, never `NULL`.
fn datetime2_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::DateTime2(7), false))
}

/// The clock of the context, rounded into a `datetime`.
///
/// The rounding is [`vauban_types::convert`]'s, not a local one: it is what turns the
/// 100 ns of the context into the 1/300 s a `datetime` stores, which is why `GETDATE()`
/// shows milliseconds ending in 0, 3 or 7. A clock before 1753 is outside the range of
/// `datetime` and raises the conversion error of `types`, as it would on SQL Server.
fn to_datetime(now: vauban_types::DateTime2) -> SqlResult<Value> {
    convert(
        &Value::DateTime2(now),
        &TypeInfo::new(SqlType::DateTime2(7), false),
        &TypeInfo::new(SqlType::DateTime, false),
        None,
    )
}

/// Evaluates `GETDATE()`: the local clock of the server, as a `datetime`.
fn getdate_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    to_datetime(ctx.now_local())
}

/// Evaluates `GETUTCDATE()`: the UTC clock of the server, as a `datetime`.
fn getutcdate_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    to_datetime(ctx.now_utc())
}

/// Evaluates `SYSDATETIME()`: the local clock, at the full precision of `datetime2(7)`.
fn sysdatetime_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::DateTime2(ctx.now_local()))
}

/// Evaluates `SYSUTCDATETIME()`: the UTC clock, at the full precision of `datetime2(7)`.
fn sysutcdatetime_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::DateTime2(ctx.now_utc()))
}

/// The component `DATEPART` and `DATENAME` share: keyword, type check, extraction.
///
/// `Ok(None)` is `NULL`. Error 9810 lives here rather than in `return_type` for the same
/// reason as 155: only an evaluation sees the keyword.
fn extract(
    args: &EvalArgs<'_>,
    ctx: &dyn EvalContext,
    function: &str,
) -> SqlResult<Option<(DatePart, Instant, i32)>> {
    let part = keyword_of(args, function)?;
    let kind = kind_of(&args.types[1])?;
    if !kind.accepts(part) {
        return Err(SqlError::datepart_not_supported(
            canonical_keyword(part),
            function,
            kind.error_name(),
        ));
    }
    let Some(instant) = instant_of(args, 1, kind)? else {
        return Ok(None);
    };
    let value = component(&instant, part, ctx.datefirst());
    Ok(Some((part, instant, value)))
}

/// The long spelling of `part`, the one SQL Server prints in the 9810 message.
fn canonical_keyword(part: DatePart) -> &'static str {
    match DATEPART_KEYWORDS.iter().find(|(_, p)| *p == part) {
        Some((keyword, _)) => keyword,
        // The table covers every variant; the first spelling of each is the long one.
        None => unreachable!("every DatePart has a keyword in DATEPART_KEYWORDS"),
    }
}

/// Evaluates `DATEPART(datepart, date)`.
fn datepart_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(match extract(args, ctx, "datepart")? {
        Some((_, _, value)) => Value::I32(value),
        None => Value::Null,
    })
}

/// Evaluates `DATENAME(datepart, date)`.
///
/// The month and the weekday come back as English names; `tzoffset` comes back as a signed
/// `+HH:MM` or `-HH:MM` offset, the one exception to "everything else is the decimal
/// `DATEPART`" (`datename_renders_a_zone_offset`). `us_english` is the one language
/// implemented: a session in another language still gets the English names.
fn datename_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some((part, instant, value)) = extract(args, ctx, "datename")? else {
        return Ok(Value::Null);
    };
    let text = match part {
        DatePart::Month => MONTH_NAMES
            .get((value - 1) as usize)
            .map_or_else(|| value.to_string(), ToString::to_string),
        DatePart::Weekday => instant.days.map_or_else(
            || value.to_string(),
            |days| DAY_NAMES[(iso_weekday(days) - 1) as usize].to_owned(),
        ),
        DatePart::TzOffset => format_offset(value),
        _ => value.to_string(),
    };
    Ok(Value::String(SqlString { text }))
}

/// Renders a zone offset in minutes as `±HH:MM`, the form `DATENAME(tzoffset, …)` uses.
fn format_offset(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let absolute = minutes.abs();
    format!("{sign}{:02}:{:02}", absolute / 60, absolute % 60)
}

/// Evaluates `YEAR`, `MONTH` and `DAY`, which are `DATEPART` with the keyword written in.
///
/// The 9810 they raise names **`datepart`**, not the shorthand that was called, where
/// `DATENAME` does name the function called (`the_shorthands_name_datepart_in_9810`):
///
/// ```text
/// SELECT YEAR(CAST('13:45:30' AS time(7)));
/// -- Datepart year cannot be used with the date function datepart on data type time.
/// SELECT DATENAME(year, CAST('13:45:30' AS time(7)));
/// -- Datepart year cannot be used with the date function datename on data type time.
/// ```
///
/// `MONTH` and `DAY` on the same `time` answer the same way, and so does `DATEPART(year, …)`
/// down to the state (3, where `DATENAME` sends 5): the three shorthands are the engine's
/// `datepart` with the keyword written in.
fn year_month_day_eval(
    args: &EvalArgs<'_>,
    ctx: &dyn EvalContext,
    part: DatePart,
) -> SqlResult<Value> {
    let kind = kind_of(&args.types[0])?;
    if !kind.accepts(part) {
        return Err(SqlError::datepart_not_supported(
            canonical_keyword(part),
            "datepart",
            kind.error_name(),
        ));
    }
    Ok(match instant_of(args, 0, kind)? {
        Some(instant) => Value::I32(component(&instant, part, ctx.datefirst())),
        None => Value::Null,
    })
}

/// Evaluates `YEAR(date)`.
fn year_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    year_month_day_eval(args, ctx, DatePart::Year)
}

/// Evaluates `MONTH(date)`.
fn month_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    year_month_day_eval(args, ctx, DatePart::Month)
}

/// Evaluates `DAY(date)`.
fn day_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    year_month_day_eval(args, ctx, DatePart::Day)
}

/// `GETDATE`: Microsoft Learn, "GETDATE (Transact-SQL)". Never deterministic.
const GETDATE_DEF: FunctionDef = FunctionDef {
    name: "GETDATE",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: datetime_type,
    eval: getdate_eval,
    aggregate: None,
};

/// `CURRENT_TIMESTAMP`: Microsoft Learn, "CURRENT_TIMESTAMP (Transact-SQL)", the ANSI
/// spelling of `GETDATE()`.
///
/// **Niladic**: it is written without parentheses, so the parser hands it over as a
/// one-part column reference and the `binder` recognises it before raising error 207
/// (`bind_niladic`). The registry sees an ordinary function of arity zero; the way it is
/// spelled is the parser's business, not this file's.
///
/// Same `eval` as [`GETDATE_DEF`], deliberately (`current_timestamp_is_getdate`):
///
/// - its type is `datetime`, precision 23, scale 3, not nullable, the type of `GETDATE()`;
/// - `SELECT CASE WHEN CURRENT_TIMESTAMP = GETDATE() THEN 1 ELSE 0 END;` answers 1, which
///   a mapping onto `SYSDATETIME()` would not: a `datetime2(7)` clock compared with the
///   1/300 s of `GETDATE()` would answer 0 most of the time;
/// - `SELECT LEN(CONVERT(varchar(40), CURRENT_TIMESTAMP, 121));` answers 23, and the last
///   digit of its millisecond is 0, 3 or 7: the 1/300 s rounding of a `datetime`, not the
///   seven digits of a `datetime2`.
const CURRENT_TIMESTAMP_DEF: FunctionDef = FunctionDef {
    name: "CURRENT_TIMESTAMP",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: datetime_type,
    eval: getdate_eval,
    aggregate: None,
};

/// `GETUTCDATE`: Microsoft Learn, "GETUTCDATE (Transact-SQL)".
const GETUTCDATE_DEF: FunctionDef = FunctionDef {
    name: "GETUTCDATE",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: datetime_type,
    eval: getutcdate_eval,
    aggregate: None,
};

/// `SYSDATETIME`: Microsoft Learn, "SYSDATETIME (Transact-SQL)".
const SYSDATETIME_DEF: FunctionDef = FunctionDef {
    name: "SYSDATETIME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: datetime2_type,
    eval: sysdatetime_eval,
    aggregate: None,
};

/// `SYSUTCDATETIME`: Microsoft Learn, "SYSUTCDATETIME (Transact-SQL)".
const SYSUTCDATETIME_DEF: FunctionDef = FunctionDef {
    name: "SYSUTCDATETIME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: datetime2_type,
    eval: sysutcdatetime_eval,
    aggregate: None,
};

/// `DATEPART`: Microsoft Learn, "DATEPART (Transact-SQL)".
///
/// **Not deterministic**, because `weekday` and `week` read `SET DATEFIRST`. `iso_week`,
/// `year` and the rest *are* deterministic, but [`FunctionDef`] cannot express a
/// determinism that depends on the value of an argument; the whole function is therefore
/// declared non-deterministic.
const DATEPART_DEF: FunctionDef = FunctionDef {
    name: "DATEPART",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(2),
    return_type: datepart_type,
    eval: datepart_eval,
    aggregate: None,
};

/// `DATENAME`: Microsoft Learn, "DATENAME (Transact-SQL)". Not deterministic: it reads
/// `SET LANGUAGE` as well as `SET DATEFIRST`.
const DATENAME_DEF: FunctionDef = FunctionDef {
    name: "DATENAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(2),
    return_type: datename_type,
    eval: datename_eval,
    aggregate: None,
};

/// `YEAR`: Microsoft Learn, "YEAR (Transact-SQL)". Deterministic, unlike `DATEPART`: the
/// component is written in and does not depend on any `SET`.
const YEAR_DEF: FunctionDef = FunctionDef {
    name: "YEAR",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: year_month_day_type,
    eval: year_eval,
    aggregate: None,
};

/// `MONTH`: Microsoft Learn, "MONTH (Transact-SQL)".
const MONTH_DEF: FunctionDef = FunctionDef {
    name: "MONTH",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: year_month_day_type,
    eval: month_eval,
    aggregate: None,
};

/// `DAY`: Microsoft Learn, "DAY (Transact-SQL)".
const DAY_DEF: FunctionDef = FunctionDef {
    name: "DAY",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: year_month_day_type,
    eval: day_eval,
    aggregate: None,
};

/// Registers the functions of this module.
pub(crate) fn register_all() {
    register(GETDATE_DEF);
    register(CURRENT_TIMESTAMP_DEF);
    register(GETUTCDATE_DEF);
    register(SYSDATETIME_DEF);
    register(SYSUTCDATETIME_DEF);
    register(DATEPART_DEF);
    register(DATENAME_DEF);
    register(YEAR_DEF);
    register(MONTH_DEF);
    register(DAY_DEF);
}

#[cfg(test)]
mod tests;
