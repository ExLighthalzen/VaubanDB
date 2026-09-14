//! Conversions towards `date`, `time`, `datetime`, `smalldatetime`, `datetime2` and
//! `datetimeoffset`, and the textual rendering of a date or time value.
//!
//! The file has two halves. The **style-less** half reads a date out of a character
//! string, converts one date type into another, and raises the errors 241, 242 and 529
//! that come with them. The **style** half holds the table of the fifty-one styles, in both
//! directions, and the four errors a style raises. The two legacy targets, `datetime` and
//! `smalldatetime`, do not validate the style: see [`legacy_style`].
//!
//! # The styles
//!
//! The style table covers the product of the style, the type and the value, in both
//! directions; `tests/convert_datetime.rs` exercises it. Beyond the documented list it
//! covers the styles 26 to 35 and 115, the two ways a fraction of a second is written, and
//! the fact that an unknown style number is an error rather than a fallback to style 0.
//!
//! # Language and date format
//!
//! Everything here assumes the session defaults `us_english` and `SET DATEFORMAT mdy`:
//! `'01/02/2000'` is the 2nd of January, month names are English, and `AM`/`PM` are the
//! meridiem markers. Other languages and other `DATEFORMAT` settings are out of V1.
//!
//! Three literals show how the reading depends on those two settings: `'12-09-2018'`
//! (`mdy`: 2018-12-09; `dmy`: 2018-09-12), `'01-03-2018'` (`mdy`: 3 January 2018; `dmy`:
//! 1 March 2018) and `'28 listopad 2018'` (Polish: 2018-11-28; Croatian: 2018-10-28).
//! VaubanDB reads these three under `us_english` and `DATEFORMAT mdy`: 2018-12-09,
//! 2018-01-03, and error 241 for the Polish month name
//! (`numeric_literals_read_as_mdy` and `listopad_under_frozen_language` in
//! `tests/convert_datetime.rs`). `SET DATEFORMAT` and `SET LANGUAGE` are stored by the
//! session and do not reach this module: a deliberate difference from SQL Server. The
//! statement is bound to these three literals; it says nothing about `ymd`, about another
//! language, or about another separator.
//!
//! # Where the arithmetic lives
//!
//! Nothing here re-implements a calendar: days ↔ civil dates, the 1/300 s of a `datetime` and
//! the split of a time of day all come from [`crate::calendar`]. Every conversion goes
//! through one intermediate shape, [`Local`], so that the six targets share one rounding path
//! and one range check.

use vauban_errors::{SqlError, SqlResult};

use crate::calendar::{
    self, DAYS_1900, TICKS_PER_DAY, TICKS_PER_SECOND, ticks_100ns_to_300th, ticks_300th_to_100ns,
};
use crate::errors;
use crate::{
    Date, DateTime, DateTime2, DateTimeOffset, SqlType, Time, TypeFamily, TypeInfo, Value,
    default_display,
};

/// Days from 0001-01-01 to 9999-12-31, the last day `date`, `datetime2` and `datetimeoffset`
/// can hold. Checked against [`calendar::days_from_civil`] by a unit test.
const MAX_DAYS: i32 = 3_652_058;

/// Days from 0001-01-01 to 1753-01-01, the first day a `datetime` can hold.
const DATETIME_MIN_DAYS: i32 = 639_905;

/// Days from 0001-01-01 to 2079-06-06, the last day a `smalldatetime` can hold.
const SMALLDATETIME_MAX_DAYS: i32 = 759_130;

/// 100-nanosecond ticks in one minute, the resolution of a `smalldatetime`.
const TICKS_PER_MINUTE: u64 = 60 * TICKS_PER_SECOND;

/// 100-nanosecond ticks in one millisecond, the unit of the fourth field of a clock.
const TICKS_PER_MILLISECOND: u64 = TICKS_PER_SECOND / 1000;

/// Minutes in one day, the number of distinct times a `smalldatetime` can hold.
const MINUTES_PER_DAY: u64 = 24 * 60;

/// 1/300 s ticks in one day, the wrap-around point of the time part of a `datetime`.
const TICKS_300TH_PER_DAY: u32 = 300 * 24 * 60 * 60;

/// 1/300 s ticks in one minute, the step of a `smalldatetime`.
///
/// [`super::binary`] writes the stored minute of a `smalldatetime` with the same step and
/// reads this constant rather than keeping a second copy of it.
pub(super) const TICKS_300TH_PER_MINUTE: u32 = 300 * 60;

/// The year a two-digit year stops meaning the 21st century (the default *two digit year
/// cutoff* of SQL Server is 2049).
const TWO_DIGIT_YEAR_CUTOFF: u32 = 49;

/// Fractional-second digits a `time(7)` holds, the finest SQL Server offers.
const MAX_FRACTION_DIGITS: u8 = 7;

/// Fractional-second digits a literal may carry. Beyond that the string is not a date at
/// all: `'…13:05:06.123456789'` reads as `.1234568`, `'…13:05:06.1234567890'` is error 241.
const MAX_FRACTION_DIGITS_WRITTEN: usize = 9;

/// A date and a time of day, both **local**, plus the offset the source carried.
///
/// Every conversion of this module reads its source into a `Local` and writes it back out
/// into the target type. Keeping the reading local (rather than in UTC) is what makes
/// `datetimeoffset` → any other date type keep the time a client would have seen and drop the
/// offset, which is the rule of `datetimeoffset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Local {
    /// Days since 0001-01-01, the origin of [`Date`].
    days: i32,
    /// 100-nanosecond ticks since midnight, `0..=TICKS_PER_DAY`.
    ///
    /// The upper bound is the one value no clock spells: rounding the fraction of a literal
    /// to seven digits turns `'23:59:59.99999995'` into the midnight that **ends** the day,
    /// and the three families of target write that midnight differently, so the reading
    /// keeps it where it is and [`write_local`] decides.
    ticks: u64,
    /// Offset from UTC in minutes, `Some` only when the source named one (a
    /// `datetimeoffset`, or a character string that ended with `+hh:mm`).
    offset: Option<i16>,
}

/// Converts `v` to the date or time target `to`, under the `CONVERT` style when given.
///
/// Without a style, `CAST` or `CONVERT` with style `0`, the source is read as follows:
///
/// * a **character string** is parsed with [`parse_string`]; an unreadable one, or one that
///   names a day that does not exist such as `'2024-02-30'`, raises error **241**;
/// * another **date type** keeps the parts both types have, zeroes the parts the source does
///   not carry (a `date` becomes midnight, a `time` becomes the 1st of January 1900) and
///   rounds the fraction of a second to the precision of the target;
/// * `date` ↔ `time` is the one pair SQL Server refuses outright: error **529**.
///
/// A value that lands outside the range of the target — a `datetime` before 1753, a
/// `smalldatetime` outside 1900-01-01..2079-06-06 — raises error **242**. A
/// `datetimeoffset` is checked on the **universal** instant it stores rather than on the
/// local reading, and raises **8114** there instead: `'0001-01-01T01:59:59+02:00'` is a
/// `datetime2` and no `datetimeoffset` at all
/// (`datetimeoffset_range_is_on_the_universal_instant` in `tests/convert_datetime.rs`).
///
/// A `datetimeoffset` **source** read as another date type is the one source that a style
/// changes: it takes styles `0` and `1` and answers **9809** to every other number, and the
/// two it takes read it differently — `0` (and no style at all) gives the **local** reading,
/// `1` gives the **universal** instant. `CONVERT(date, CAST('2000-01-02 01:05:06 +02:00' AS
/// datetimeoffset(7)), 1)` is the 1st of January where style `0` is the 2nd.
///
/// # A string under a style: two families of targets
///
/// The style rule of a **character** source depends on the target, and the two rules are
/// distinct (`tests/convert_datetime.rs`, 256 styles on the six targets):
///
/// * `date`, `time`, `datetime2` and `datetimeoffset` — the four types of 2008 — read the
///   same way: the style is validated first (**9809** when the pair does not support the
///   number, 214 of the 256), then it constrains the all-numeric date of the string
///   (**9807** under the seventeen strict styles, **241** otherwise). That is
///   [`read_constraint`] and [`parse_with_style`];
/// * `datetime` and `smalldatetime` **never** answer 9809 nor 9807, whatever the number:
///   `CONVERT(smalldatetime, '20000102', 77)` is the 2nd of January 2000 where
///   `CONVERT(date, '20000102', 77)` is 9809, and `CONVERT(datetime, '2000-01-02', 77)` is
///   241 where `date` is 9809 again. The style selects a **grammar** instead, and an
///   unreadable string is 241 (`datetime`) or **295** (`smalldatetime`), an impossible one
///   is 242. That is [`legacy_style`] and [`read_legacy_styled`].
pub(crate) fn to_datetime(
    v: &Value,
    from: &TypeInfo,
    to: &TypeInfo,
    style: Option<i32>,
) -> SqlResult<Value> {
    // A number and a binary are read by [`number_to_datetime`] and [`binary_to_datetime`],
    // which build the target value themselves rather than going through [`Local`]: their
    // rounding is decided on the 1/300 s tick and their refusal is 8115, where the
    // [`Local`] path rounds on the 100 ns tick and raises 242. A style says nothing about
    // either of them and is ignored; the clause stops at a **number and a binary**, since
    // a `datetimeoffset` read as another date type takes styles 0 and 1 thirty lines
    // below. `CONVERT(datetime, CAST(1 AS int), <s>)` is 1900-01-02 for the styles 0, 1,
    // 112 and 77 alike, and so is `CONVERT(datetime, 0x0000000100000000, <s>)` for 112
    // and 77 (`tests/convert_datetime.rs`).
    match from.ty.family() {
        TypeFamily::Bit
        | TypeFamily::Integer
        | TypeFamily::ExactNumeric
        | TypeFamily::ApproxNumeric
        | TypeFamily::Money => return number_to_datetime(v, &from.ty, &to.ty),
        TypeFamily::Binary => return binary_to_datetime(v, &from.ty, &to.ty),
        TypeFamily::Character | TypeFamily::DateTime | TypeFamily::Guid => {}
    }
    let style = match style {
        // `None` is `CAST`; style 0 is the default style of `CONVERT`, the same thing in
        // both directions: a string under style 0 reads like its `CAST`.
        None | Some(0) => None,
        Some(s) => Some(s),
    };
    // A character string read into one of the two legacy targets follows a grammar of its
    // own, with or without a style: see the section "The grammar of the two legacy
    // targets" below. Without a style it is [`read_legacy_cast`]; under a style the two
    // targets do not validate the number, no number being 9809 or 9807 on them,
    // and [`legacy_style`] picks the grammar instead.
    if from.ty.is_string() && matches!(to.ty, SqlType::DateTime | SqlType::SmallDateTime) {
        let Value::String(text) = v else {
            return Err(errors::bug("convert: a string type without a string value"));
        };
        let reading = match style {
            None => read_legacy_cast(&text.text),
            Some(style) => match legacy_style(style) {
                LegacyStyle::Hijri => {
                    return Err(errors::bug(
                        "convert: the Hijri styles 130 and 131 are not implemented",
                    ));
                }
                legacy => read_legacy_styled(&text.text, legacy),
            },
        };
        return match reading {
            LegacyReading::Read(local) => write_local(local, &from.ty, &to.ty),
            LegacyReading::NotADate if to.ty == SqlType::SmallDateTime => {
                Err(SqlError::conversion_failed_smalldatetime())
            }
            LegacyReading::NotADate => Err(SqlError::conversion_failed_datetime()),
            LegacyReading::OutOfRange => Err(errors::out_of_range(&from.ty, &to.ty)),
        };
    }
    let Some(style) = style else {
        let local = read_local(v, &from.ty, &to.ty)?;
        return write_local(local, &from.ty, &to.ty);
    };
    if !from.ty.is_string() {
        // A style says nothing about a source that is not text: `CONVERT(date, <datetime2>,
        // 77)` is the plain conversion, unknown style number and all. The one exception is
        // a `datetimeoffset` **source** read as another date type, which takes styles 0 and
        // 1 and raises 9809 on every other number, the numbers that exist included
        // (`tests/convert_datetime.rs`).
        if matches!(from.ty, SqlType::DateTimeOffset(_))
            && !matches!(to.ty, SqlType::DateTimeOffset(_))
        {
            if style != 1 {
                return Err(errors::unsupported_style(style, &from.ty, &to.ty));
            }
            // The two styles it does take are not two spellings of one answer, they are the
            // two readings of a `datetimeoffset`: style 0 keeps the **local** reading a
            // client sees, style 1 takes the **universal** instant the value stores. The
            // difference is a whole day as soon as the offset crosses midnight —
            // `CONVERT(date, CAST('2000-01-02 01:05:06 +02:00' AS datetimeoffset(7)), 1)` is
            // the 1st of January where style 0 is the 2nd
            // (`datetimeoffset_rejects_other_styles` in `tests/convert_datetime.rs`).
            let Value::DateTimeOffset(dto) = v else {
                return Err(errors::bug("convert: a datetimeoffset without its value"));
            };
            return write_local(utc_of_offset(*dto), &from.ty, &to.ty);
        }
        let local = read_local(v, &from.ty, &to.ty)?;
        return write_local(local, &from.ty, &to.ty);
    }
    let Value::String(text) = v else {
        return Err(errors::bug("convert: a string type without a string value"));
    };
    let constraint =
        read_constraint(style).ok_or_else(|| errors::unsupported_style(style, &from.ty, &to.ty))?;
    if constraint == StyleConstraint::Hijri {
        return Err(errors::bug(
            "convert: the Hijri styles 130 and 131 are not implemented",
        ));
    }
    match parse_with_style(&text.text, constraint) {
        StyleReading::Read(local) => write_local(local, &from.ty, &to.ty),
        StyleReading::NotADate => Err(SqlError::conversion_failed_datetime()),
        StyleReading::DoesNotFollowStyle => Err(errors::does_not_follow_style(style)),
    }
}

/// Renders the date or time value `v`, of type `from`, as the text of the given `CONVERT`
/// style (`None` for the style-less default of `CAST`).
///
/// This half of the conversion is called by `to_character` when the source is a date. No
/// style, and the default style of the source type (`0` for `datetime` and
/// `smalldatetime`, `121` for the four other types), are [`default_display`]; the other
/// numbers go through [`render_with_style`]. `to` is the character target, which the
/// rendering itself ignores, the declared length being applied by `to_character`, and
/// which error 8114 names.
///
/// Style `0` is **not** the default rendering of the four modern types:
/// `CONVERT(varchar(30), CAST('2000-01-02' AS date), 0)` is `Jan  2 2000` where the same
/// `CAST` alone is `2000-01-02`. Style 0 is the `datetime` form, whatever the source
/// (`tests/convert_datetime.rs`).
pub(crate) fn datetime_to_character(
    v: &Value,
    from: &TypeInfo,
    to: &TypeInfo,
    style: Option<i32>,
) -> SqlResult<String> {
    match style {
        None => Ok(default_display(v, from)),
        Some(s) if s == i32::from(default_style(&from.ty)) => Ok(default_display(v, from)),
        Some(s) => render_with_style(v, from, to, s),
    }
}

/// The `CONVERT` style each date type renders with when no style is given.
fn default_style(ty: &SqlType) -> u8 {
    match ty {
        SqlType::DateTime | SqlType::SmallDateTime => 0,
        _ => 121,
    }
}

// ---------------------------------------------------------------------------------------
// The styles of `CONVERT`: which styles exist, what each one writes, what each one demands
// of a string, and which error each refusal raises (`tests/convert_datetime.rs`).
// ---------------------------------------------------------------------------------------

/// How a `CONVERT` style writes a date and a time of day.
///
/// A style is a pair of forms — one for the date half, one for the clock half — plus the way
/// the two are joined. A source that carries only one half writes only that half:
/// `CONVERT(varchar(30), CAST('2000-01-02' AS date), 13)` is `02 Jan 2000` where the same
/// style on a `datetime2` is `02 Jan 2000 13:05:06.1234567`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StyleForm {
    /// The date half.
    date: DateForm,
    /// The clock half.
    time: TimeForm,
    /// The character a **legacy** source writes in front of its milliseconds. The styles
    /// numbered below 20 (and their century counterparts) write `1:05:06:123PM`, the ones
    /// from 20 up write `13:05:06.123`; a `datetime2`, a `time` and a `datetimeoffset`
    /// write a point regardless of the style.
    legacy_fraction: char,
    /// `T` between the two halves, and no blank in front of the offset: styles 126 and 127.
    iso: bool,
    /// The offset is spent rather than written: the instant is shown in UTC and followed by
    /// `Z`. Style 127 alone, and only when the source is a `datetimeoffset`
    /// (`CONVERT(varchar(40), CAST('2000-01-02 13:05:06.1234567 -14:00' AS
    /// datetimeoffset(7)), 127)` is `2000-01-03T03:05:06.1234567Z`).
    utc: bool,
}

/// The date half of a style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateForm {
    /// The style writes no date: styles 8, 14, 24, 108, 114 and 115.
    Nothing,
    /// `Jan  2 2000`: the month abbreviated, the day right-aligned on two characters, the
    /// year on four (styles 0, 9, 100 and 109).
    MonthDayYear,
    /// `02 Jan 2000` or `02 Jan 00` (styles 6, 13, 106 and 113).
    DayMonthYear(Century),
    /// `Jan 02, 2000` or `Jan 02, 00` (styles 7 and 107).
    MonthDayCommaYear(Century),
    /// Three numbers in a fixed order, glued by a separator or by nothing.
    Numbers {
        /// Which of the year, the month and the day comes first, second and third.
        order: Order,
        /// The character between two numbers; `None` for the compact styles 12 and 112.
        separator: Option<char>,
        /// How many digits the year takes.
        century: Century,
    },
}

/// How many digits a style writes the year on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Century {
    /// Two digits: `00` for the year 2000, `99` for 1899 and for 1999 alike.
    Two,
    /// Four digits, zero-padded: `0999`.
    Four,
}

/// The order of the three numbers of an all-numeric date.
///
/// The six orders exist: the ODBC canonical styles 26 to 35 write the year in each of
/// the three slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Ymd,
    Mdy,
    Dmy,
    Ydm,
    Myd,
    Dym,
}

/// The clock half of a style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeForm {
    /// The style writes no clock.
    Nothing,
    /// ` 1:05PM`: a 12-hour clock with no seconds (styles 0 and 100).
    MeridiemMinutes,
    /// ` 1:05:06.1234567PM`: a 12-hour clock with seconds and the fraction of the source
    /// (styles 9 and 109).
    MeridiemSeconds,
    /// `13:05:06`: a 24-hour clock, no fraction (styles 8, 20, 24, 108 and 120).
    Seconds,
    /// `13:05:06.1234567`: a 24-hour clock with the fraction of the source.
    Fraction,
    /// ` 1:05:06 PM`: a 12-hour clock whose hour is **always** right-aligned on two
    /// characters, and a blank before the marker (style 22).
    SpacedMeridiem,
    /// `130506`: six digits, no separator (style 115).
    Compact,
}

/// The form of `style`, `None` when no style bears that number.
///
/// A number this function does not know is error **281** towards a character type and error
/// **9809** towards one of the four modern date types, the two refusals a style number
/// gets over `-2147483648..=2147483647` (`tests/convert_datetime.rs`). A `datetime` or a
/// `smalldatetime` target refuses no number at all: see [`legacy_style`]. The styles that
/// exist are `0..=14`, `20..=35`, `100..=115`, `120`, `121`, `126`, `127`, and the two
/// Hijri styles `130` and `131`, which are not implemented.
fn style_form(style: i32) -> Option<StyleForm> {
    // The date halves, by style.
    let numbers = |order: Order, separator: char, century: Century| DateForm::Numbers {
        order,
        separator: Some(separator),
        century,
    };
    let compact = |century: Century| DateForm::Numbers {
        order: Order::Ymd,
        separator: None,
        century,
    };
    let (date, time) = match style {
        0 | 100 => (DateForm::MonthDayYear, TimeForm::MeridiemMinutes),
        1 => (numbers(Order::Mdy, '/', Century::Two), TimeForm::Nothing),
        2 => (numbers(Order::Ymd, '.', Century::Two), TimeForm::Nothing),
        3 => (numbers(Order::Dmy, '/', Century::Two), TimeForm::Nothing),
        4 => (numbers(Order::Dmy, '.', Century::Two), TimeForm::Nothing),
        5 => (numbers(Order::Dmy, '-', Century::Two), TimeForm::Nothing),
        6 => (DateForm::DayMonthYear(Century::Two), TimeForm::Nothing),
        7 => (DateForm::MonthDayCommaYear(Century::Two), TimeForm::Nothing),
        8 | 24 | 108 => (DateForm::Nothing, TimeForm::Seconds),
        9 | 109 => (DateForm::MonthDayYear, TimeForm::MeridiemSeconds),
        10 => (numbers(Order::Mdy, '-', Century::Two), TimeForm::Nothing),
        11 => (numbers(Order::Ymd, '/', Century::Two), TimeForm::Nothing),
        12 => (compact(Century::Two), TimeForm::Nothing),
        13 | 113 => (DateForm::DayMonthYear(Century::Four), TimeForm::Fraction),
        14 | 114 => (DateForm::Nothing, TimeForm::Fraction),
        20 | 120 => (numbers(Order::Ymd, '-', Century::Four), TimeForm::Seconds),
        21 | 25 | 121 | 126 | 127 => (numbers(Order::Ymd, '-', Century::Four), TimeForm::Fraction),
        22 => (
            numbers(Order::Mdy, '/', Century::Two),
            TimeForm::SpacedMeridiem,
        ),
        23 => (numbers(Order::Ymd, '-', Century::Four), TimeForm::Nothing),
        26 => (numbers(Order::Ydm, '-', Century::Four), TimeForm::Fraction),
        27 => (numbers(Order::Mdy, '-', Century::Four), TimeForm::Fraction),
        28 => (numbers(Order::Myd, '-', Century::Four), TimeForm::Fraction),
        29 => (numbers(Order::Dmy, '-', Century::Four), TimeForm::Fraction),
        30 => (numbers(Order::Dym, '-', Century::Four), TimeForm::Fraction),
        31 => (numbers(Order::Ydm, '-', Century::Four), TimeForm::Nothing),
        32 => (numbers(Order::Mdy, '-', Century::Four), TimeForm::Nothing),
        33 => (numbers(Order::Myd, '-', Century::Four), TimeForm::Nothing),
        34 => (numbers(Order::Dmy, '-', Century::Four), TimeForm::Nothing),
        35 => (numbers(Order::Dym, '-', Century::Four), TimeForm::Nothing),
        101 => (numbers(Order::Mdy, '/', Century::Four), TimeForm::Nothing),
        102 => (numbers(Order::Ymd, '.', Century::Four), TimeForm::Nothing),
        103 => (numbers(Order::Dmy, '/', Century::Four), TimeForm::Nothing),
        104 => (numbers(Order::Dmy, '.', Century::Four), TimeForm::Nothing),
        105 => (numbers(Order::Dmy, '-', Century::Four), TimeForm::Nothing),
        106 => (DateForm::DayMonthYear(Century::Four), TimeForm::Nothing),
        107 => (
            DateForm::MonthDayCommaYear(Century::Four),
            TimeForm::Nothing,
        ),
        110 => (numbers(Order::Mdy, '-', Century::Four), TimeForm::Nothing),
        111 => (numbers(Order::Ymd, '/', Century::Four), TimeForm::Nothing),
        112 => (compact(Century::Four), TimeForm::Nothing),
        115 => (DateForm::Nothing, TimeForm::Compact),
        _ => return None,
    };
    Some(StyleForm {
        date,
        time,
        // The two families of milliseconds of a `datetime` and a `smalldatetime` source:
        // `Jan  2 2000  1:05:06:123PM` at style 9, `2000-01-02 13:05:06.123` at style 21.
        legacy_fraction: match style {
            9 | 13 | 14 | 109 | 113 | 114 => ':',
            _ => '.',
        },
        iso: matches!(style, 126 | 127),
        utc: style == 127,
    })
}

/// The pieces of a date value, as the styles write them.
///
/// `has_date` and `has_time` are what tells a `date` from a `time` from a `datetime2`: a
/// style writes only the halves its source carries.
#[derive(Debug, Clone, Copy)]
struct Parts {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    /// Fraction of a second in 100-nanosecond ticks, `0..10_000_000`.
    fraction: u32,
    /// Fractional digits the type holds: 3 for a legacy type, `s` for the others.
    scale: u8,
    /// A `datetime` or a `smalldatetime`, whose milliseconds are written apart.
    legacy: bool,
    /// The offset of a `datetimeoffset`, in minutes.
    offset: Option<i16>,
    /// The source carries a date (everything but a `time`).
    has_date: bool,
    /// The source carries a clock (everything but a `date`).
    has_time: bool,
}

/// Splits `v`, of type `from`, into the pieces a style writes.
///
/// `utc` asks for the universal instant rather than the local one, which only style 127 of a
/// `datetimeoffset` wants.
fn parts_of(v: &Value, from: &SqlType, utc: bool) -> SqlResult<Parts> {
    let build = |days: i32, ticks: u64, scale: u8, legacy: bool, offset: Option<i16>| {
        let (year, month, day) = calendar::civil_from_days(days);
        let (hour, minute, second, fraction) = calendar::hms_from_ticks(ticks);
        Parts {
            year,
            month,
            day,
            hour,
            minute,
            second,
            fraction,
            scale,
            legacy,
            offset,
            has_date: true,
            has_time: true,
        }
    };
    match (v, from) {
        (Value::Date(d), SqlType::Date) => Ok(Parts {
            has_time: false,
            ..build(d.days, 0, 0, false, None)
        }),
        (Value::Time(t), SqlType::Time(scale)) => Ok(Parts {
            has_date: false,
            ..build(DAYS_1900, t.ticks_100ns, *scale, false, None)
        }),
        (Value::DateTime(dt), SqlType::DateTime | SqlType::SmallDateTime) => Ok(build(
            DAYS_1900.saturating_add(dt.days),
            ticks_300th_to_100ns(dt.ticks_300th),
            3,
            true,
            None,
        )),
        (Value::DateTime2(dt), SqlType::DateTime2(scale)) => Ok(build(
            dt.date.days,
            dt.time.ticks_100ns,
            *scale,
            false,
            None,
        )),
        (Value::DateTimeOffset(dto), SqlType::DateTimeOffset(scale)) => match utc {
            // Style 127 writes the instant the value stores, not the local reading.
            true => Ok(build(
                dto.utc.date.days,
                dto.utc.time.ticks_100ns,
                *scale,
                false,
                Some(0),
            )),
            false => {
                let local = local_of_offset(*dto);
                Ok(build(
                    local.days,
                    local.ticks,
                    *scale,
                    false,
                    Some(dto.offset_minutes),
                ))
            }
        },
        // `to_character` only routes a value of the date family here, and `convert` answers
        // `NULL` before dispatching.
        _ => Err(errors::bug("convert: not a date value for a date style")),
    }
}

/// Renders `v`, of type `from`, in the style `style`.
///
/// Three refusals live here (`tests/convert_datetime.rs`):
///
/// * a number that is no style at all — `77`, `-1`, `36` — is error **281**;
/// * a style that writes only a clock is refused by a `date`, error **8114** for the styles
///   that write `hh:mi:ss` (8, 24, 108) and error **281** for the two that write the
///   milliseconds too (14 and 114) — the same string, two different numbers, which is why
///   the table says so rather than deducing it;
/// * a style that writes no clock, and the compact style 115, are refused by a `time`,
///   error **8114**.
///
/// Style 115 is the exception that shapes the rule: it writes `hhmmss`, a `date` renders it
/// as `000000`, and a `time` refuses it.
///
/// The 8114 names the **target**, in its `var` form whatever the declared type: `date to
/// nvarchar` for an `nchar(60)` and an `nvarchar(max)`, `date to varchar` for a `char(60)`
/// and a `varchar(max)`, the same for a `time` source
/// (`tests::a_style_a_type_cannot_fill_names_the_target`). That is
/// [`super::to_character::character_error_type`], the rule the 9809 of a binary source
/// already follows.
fn render_with_style(v: &Value, from: &TypeInfo, to: &TypeInfo, style: i32) -> SqlResult<String> {
    if matches!(style, 130 | 131) {
        return Err(errors::bug(
            "convert: the Hijri styles 130 and 131 are not implemented",
        ));
    }
    let form = style_form(style).ok_or_else(|| errors::invalid_style_number(style, &from.ty))?;
    let parts = parts_of(v, &from.ty, form.utc)?;
    let national = matches!(to.ty, SqlType::NChar(_) | SqlType::NVarChar(_));
    let cannot_fill = || {
        errors::error_converting(
            &from.ty,
            &super::to_character::character_error_type(national),
        )
    };
    if !parts.has_time && form.date == DateForm::Nothing {
        return match form.time {
            TimeForm::Compact => Ok(render_form(&parts, &form)),
            TimeForm::Fraction => Err(errors::invalid_style_number(style, &from.ty)),
            _ => Err(cannot_fill()),
        };
    }
    if !parts.has_date && matches!(form.time, TimeForm::Nothing | TimeForm::Compact) {
        return Err(cannot_fill());
    }
    Ok(render_form(&parts, &form))
}

/// Writes the two halves and joins them.
fn render_form(parts: &Parts, form: &StyleForm) -> String {
    let date = parts.has_date.then(|| render_date(parts, form)).flatten();
    let time = parts.has_time || form.time == TimeForm::Compact;
    let time = time.then(|| render_time(parts, form)).flatten();
    let mut out = match (date, time) {
        (Some(d), Some(t)) => {
            let join = if form.iso { "T" } else { " " };
            format!("{d}{join}{t}")
        }
        (Some(d), None) => d,
        (None, Some(t)) => t,
        (None, None) => String::new(),
    };
    // The offset follows the clock, and only the clock: a style that writes a date alone
    // writes no offset, and so does the compact style 115.
    if let Some(minutes) = parts.offset
        && !matches!(form.time, TimeForm::Nothing | TimeForm::Compact)
    {
        if form.utc {
            out.push('Z');
        } else {
            if !form.iso {
                out.push(' ');
            }
            let sign = if minutes < 0 { '-' } else { '+' };
            let m = minutes.unsigned_abs();
            out.push_str(&format!("{sign}{:02}:{:02}", m / 60, m % 60));
        }
    }
    out
}

/// Writes the date half, `None` when the style writes none.
fn render_date(parts: &Parts, form: &StyleForm) -> Option<String> {
    let year = |century: Century| match century {
        Century::Two => format!("{:02}", parts.year.rem_euclid(100)),
        Century::Four => format!("{:04}", parts.year),
    };
    let month_name = month_abbreviation(parts.month);
    match form.date {
        DateForm::Nothing => None,
        DateForm::MonthDayYear => Some(format!(
            "{month_name} {:>2} {:04}",
            parts.day, parts.year as u32
        )),
        DateForm::DayMonthYear(century) => {
            Some(format!("{:02} {month_name} {}", parts.day, year(century)))
        }
        DateForm::MonthDayCommaYear(century) => {
            Some(format!("{month_name} {:02}, {}", parts.day, year(century)))
        }
        DateForm::Numbers {
            order,
            separator,
            century,
        } => {
            let y = year(century);
            let m = format!("{:02}", parts.month);
            let d = format!("{:02}", parts.day);
            let pieces = match order {
                Order::Ymd => [y, m, d],
                Order::Mdy => [m, d, y],
                Order::Dmy => [d, m, y],
                Order::Ydm => [y, d, m],
                Order::Myd => [m, y, d],
                Order::Dym => [d, y, m],
            };
            let separator = separator.map(String::from).unwrap_or_default();
            Some(pieces.join(&separator))
        }
    }
}

/// Writes the clock half, `None` when the style writes none.
fn render_time(parts: &Parts, form: &StyleForm) -> Option<String> {
    let hour12 = match parts.hour % 12 {
        0 => 12,
        h => h,
    };
    let meridiem = if parts.hour < 12 { "AM" } else { "PM" };
    // A 12-hour clock right-aligns its hour on two characters — but the styles that follow
    // a `mon dd yyyy` date drop the blank when there is no date in front of it, where style
    // 22 keeps it: `CONVERT(varchar(30), CAST('13:05:06' AS time(0)), 0)` is `1:05PM` and
    // `…, 22)` is ` 1:05:06 PM`.
    let padded = match parts.has_date {
        true => format!("{hour12:>2}"),
        false => format!("{hour12}"),
    };
    let fraction = fraction_text(parts, form);
    match form.time {
        TimeForm::Nothing => None,
        TimeForm::MeridiemMinutes => Some(format!("{padded}:{:02}{meridiem}", parts.minute)),
        TimeForm::MeridiemSeconds => Some(format!(
            "{padded}:{:02}:{:02}{fraction}{meridiem}",
            parts.minute, parts.second
        )),
        TimeForm::Seconds => Some(format!(
            "{:02}:{:02}:{:02}",
            parts.hour, parts.minute, parts.second
        )),
        TimeForm::Fraction => Some(format!(
            "{:02}:{:02}:{:02}{fraction}",
            parts.hour, parts.minute, parts.second
        )),
        TimeForm::SpacedMeridiem => Some(format!(
            "{hour12:>2}:{:02}:{:02} {meridiem}",
            parts.minute, parts.second
        )),
        TimeForm::Compact => Some(format!(
            "{:02}{:02}{:02}",
            parts.hour, parts.minute, parts.second
        )),
    }
}

/// The fraction of a second a style writes, separator included, empty when it writes none.
///
/// A legacy source always writes three digits, **rounded** rather than truncated: the
/// 1/300 s tick of `'1899-07-04 09:08:07.008'` is `.0066667` of a second and style 9 writes
/// `9:08:07:007AM`. Any other source
/// writes the digits its scale holds, and none at all at scale 0. The two ISO styles 126 and
/// 127 drop a fraction that is zero, where style 121 keeps it.
fn fraction_text(parts: &Parts, form: &StyleForm) -> String {
    if !matches!(form.time, TimeForm::MeridiemSeconds | TimeForm::Fraction) {
        return String::new();
    }
    if form.iso && parts.fraction == 0 {
        return String::new();
    }
    if parts.legacy {
        let milliseconds =
            (u64::from(parts.fraction) + TICKS_PER_MILLISECOND / 2) / TICKS_PER_MILLISECOND;
        return format!("{}{milliseconds:03}", form.legacy_fraction);
    }
    let scale = usize::from(parts.scale.min(MAX_FRACTION_DIGITS));
    if scale == 0 {
        return String::new();
    }
    let seven = format!("{:07}", parts.fraction);
    format!(".{}", &seven[..scale])
}

/// The three-letter English month abbreviation the styles print.
///
/// The last arm is unreachable: [`calendar::civil_from_days`] always yields `1..=12`.
fn month_abbreviation(month: u8) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    }
}

/// What a `CONVERT` style demands of a character string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StyleConstraint {
    /// No style, or style `0`: the style-less grammar of [`parse_string`].
    None,
    /// The style writes its date as three numbers: a numeric date must be written in that
    /// order, with a year of that width. The separator is not checked.
    Numbers { order: Order, century: Century },
    /// The style writes its date with a month name, or without separators, or writes no
    /// date at all: an all-numeric date does not follow it, error 9807.
    NotNumbers,
    /// One of the two Hijri styles, not implemented.
    Hijri,
}

impl Order {
    /// Where the year, the month and the day stand in a date written in this order.
    fn slots(self) -> [usize; 3] {
        match self {
            Order::Ymd => [0, 1, 2],
            Order::Mdy => [2, 0, 1],
            Order::Dmy => [2, 1, 0],
            Order::Ydm => [0, 2, 1],
            Order::Myd => [1, 0, 2],
            Order::Dym => [1, 2, 0],
        }
    }
}

/// What `style` demands of a string read into one of the four modern date types, `None`
/// when no conversion from text into them accepts that number (error 9809). The two legacy
/// targets never come here: [`legacy_style`] is their table, and it has no `None`.
///
/// The styles that **read** are not the styles that **write**: `26..=35` and `115` write a
/// date and read none, `CONVERT(date, '2000-01-02', 26)` being error 9809 where
/// `CONVERT(varchar(30), CAST('2000-01-02' AS date), 26)` is `2000-02-01`.
fn read_constraint(style: i32) -> Option<StyleConstraint> {
    match style {
        0 => Some(StyleConstraint::None),
        130 | 131 => Some(StyleConstraint::Hijri),
        26..=35 | 115 => None,
        _ => match style_form(style)?.date {
            DateForm::Numbers {
                order,
                separator: Some(_),
                century,
            } => Some(StyleConstraint::Numbers { order, century }),
            _ => Some(StyleConstraint::NotNumbers),
        },
    }
}

// ---------------------------------------------------------------------------------------
// The grammar of the two legacy targets, `datetime` and `smalldatetime`.
//
// A character string read into a `datetime` or a `smalldatetime` does not go through the
// grammar of the four types of 2008 ([`parse_string`]): it goes through the one below,
// which is a grammar of its own. `tests/convert_datetime.rs` reads each of its forms in
// the six targets, so the two legacy columns say where the two grammars part.
//
// What the legacy grammar does differently, each with the vector that separates it from
// the modern reading:
//
// * only the **space** separates tokens: `'2000-01-02\t13:05:06'` and `'2000-01-02\t'` are
//   refused where the modern grammar reads them;
// * the separators `-`, `/` and `.` are not told apart, even inside one date:
//   `'2000-01/02'` and `'01.02/2000'` are read (refused by the modern grammar);
// * the ISO 8601 spelling is read on its **full width only**, and a blank in front of it
//   shuts nothing: `'2000-1-2T03:05:06'` and `'2000-01-02T13:5:06'` are refused,
//   `' 2000-01-02T13:05:06'` is read — the modern grammar answers the opposite to all three;
// * a number is judged by its **value**, not its width, up to three digits: `'2000-001-02'`
//   and `'Jan 002 2000'` are read, `'01-02-000'` is the year 2000 and `'01-02-100'` is 242,
//   where the modern grammar refuses a month or a day on three digits;
// * a month name and one number are never a date, even beside a clock: `'Jan 2 13:05'` is
//   refused where the modern grammar reads the 1st of January 2002;
// * a clock may stand **before** its date: `'13:05 2000-01-02'` and `'1 PM 2000-01-02'` are
//   read;
// * a comma is admitted in more places (`'Jan,2,2000'`, `'2, Jan 2000'`, `'2000 Jan,2'`),
//   and a separator beside a month name in more too (`'2000-Jan'`, `'2-Jan2000'`,
//   `'2000-Jan 2'`), but not in every place: `'Jan-2000'` and `'2 Jan-2000'` are 242;
// * what is not a date is **241** towards a `datetime` and **295** towards a
//   `smalldatetime`, and what has the shape of a date with a piece that does not fit —
//   `'2024-02-30'`, `'2000-01-02 24:00:00'`, `'13:05AM'` — is **242** towards both, where
//   the modern grammar answers 241 to all of them.
//
// Fractions of zero to nine digits on five spellings, crossed with absent, `Z` and signed
// zones and the six targets: the legacy clock caps fractions at three; `Z` remains valid
// on the ISO, clock-only and year/clock forms (`tests/convert_datetime.rs`).
// ---------------------------------------------------------------------------------------

/// What a `CONVERT` style selects when the target is a `datetime` or a `smalldatetime`.
///
/// The two legacy targets never validate the number: 9809 and 9807 do not exist for them.
/// A style picks one of four grammars instead, and every other number, known to the
/// modern targets or not, negative, `2147483647`, picks the last one
/// (`tests/convert_datetime.rs`: the styles `-1..=256`, `1000` and `2147483647` on the
/// two targets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyStyle {
    /// The style names an order and a year width for the all-numeric date, exactly as
    /// `SET DATEFORMAT` would: `1` and `10` are `mdy` on two digits, `101` and `110` on
    /// four, `2` and `11` are `ymd` on two, `20`, `21`, `102`, `111`, `120` and `121` on
    /// four, `3`, `4` and `5` are `dmy` on two, `103`, `104` and `105` on four. See
    /// [`date_under_legacy_style`] for what the order does to the pieces.
    Numbers { order: Order, century: Century },
    /// Styles 126 and 127: the ISO 8601 spelling on its full width, `127` alone reading a
    /// `Z` behind it. See [`parse_legacy_iso`].
    Iso { zulu: bool },
    /// The two Hijri styles 130 and 131, not implemented.
    Hijri,
    /// Every other number: the loose grammar, minus the all-numeric separated date, which
    /// no order can read. `'20000102'`, `'02 Jan 2000'`, `'13:05:06'` and `''` are read
    /// under style 77 and under style 112 alike; `'2000-01-02'` and `'01/02/2000'` are
    /// 241 (`datetime`) or 295 (`smalldatetime`) under both.
    Other,
}

/// The grammar `style` selects on a legacy target. Style 0 never reaches here: it is the
/// style-less reading of `CAST`, [`read_legacy_cast`].
fn legacy_style(style: i32) -> LegacyStyle {
    let numbers = |order, century| LegacyStyle::Numbers { order, century };
    match style {
        1 | 10 => numbers(Order::Mdy, Century::Two),
        2 | 11 => numbers(Order::Ymd, Century::Two),
        3..=5 => numbers(Order::Dmy, Century::Two),
        101 | 110 => numbers(Order::Mdy, Century::Four),
        20 | 21 | 102 | 111 | 120 | 121 => numbers(Order::Ymd, Century::Four),
        103..=105 => numbers(Order::Dmy, Century::Four),
        126 => LegacyStyle::Iso { zulu: false },
        127 => LegacyStyle::Iso { zulu: true },
        130 | 131 => LegacyStyle::Hijri,
        _ => LegacyStyle::Other,
    }
}

/// What reading a string into a legacy target gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyReading {
    /// The string was read.
    Read(Local),
    /// The string is no date the grammar knows: error 241 towards `datetime`, 295 towards
    /// `smalldatetime`.
    NotADate,
    /// The string has the shape of a date and names a day that does not exist, or a piece
    /// out of its range: error 242 towards both targets.
    OutOfRange,
}

/// How the purely numeric date of a legacy literal, three numbers joined by separators, is
/// read: the one thing a `CONVERT` style changes in the legacy grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyNumbers {
    /// No style: the four-digit piece is the year wherever it stands, and the two others
    /// are month then day (`DATEFORMAT mdy`); with no four-digit piece the three are read
    /// `mdy`, the year by its value. See [`date_under_no_style`].
    Default,
    /// A style that names an order and a year width. See [`date_under_legacy_style`].
    Ordered { order: Order, century: Century },
    /// A style that names no order: no all-numeric separated date reads at all.
    Refused,
    /// Style 127 past ISO: a clock, or a four-digit year with `Z`.
    ClockOnly,
}

/// Reads a string into a legacy target **without a style** — `CAST`, or `CONVERT` with
/// style 0 — under the grammar described at the head of this section.
///
/// Only spaces are trimmed at either end; NUL is handled inside month tokens, not trimmed.
/// `' 2000-01-02T13:05:06'` and
/// `'2000-01-02 '` are read, `'\t2000-01-02'` and `'2000-01-02\t'` are 241. The empty
/// string is the 1st of January 1900. Then the ISO 8601 spelling of
/// [`legacy_iso_under_cast`] is tried, and the loose grammar of [`read_legacy_loose`]
/// after it.
///
/// A capital `T` outside a word that the ISO spelling did not read is no date at all:
/// `'2000-1-2T03:05:06'`, `'2000-01-02 T13:05:06'`, `'20000102T13:05:06'` and
/// `'2000-01-02T13:05:06 13:05'` are 241, not the 242 the same `T` earns under a style
/// that names an order ([`read_legacy_glued_t`]). One exception: a literal that
/// **starts** with the `T` (`'T13:05:06'`, `'T13:05'`) is 242.
fn read_legacy_cast(s: &str) -> LegacyReading {
    let trimmed = s.trim_matches(' ');
    if trimmed.is_empty() {
        return LegacyReading::Read(Local {
            days: DAYS_1900,
            ticks: 0,
            offset: None,
        });
    }
    if let Some(reading) = legacy_iso_under_cast(trimmed) {
        return reading;
    }
    if let Some(index) = glued_t(trimmed) {
        let head = trimmed[..index].trim_end_matches(' ');
        let tail = trimmed[index + 1..].trim_start_matches(' ');
        let bare_number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if head.is_empty() {
            // `'T13:05:06'`, `'T13:05'`, `'T'` and `'T2000'` are 242, `'T13'` is 241: a bare
            // number behind a lone `T` is 241 unless it has four digits.
            return match bare_number(tail) && tail.len() != 4 {
                true => LegacyReading::NotADate,
                false => LegacyReading::OutOfRange,
            };
        }
        // A year alone in front of the `T` is 242 (`'2000T'`), a date is 241
        // (`'2000-01-02T'`, `'2000-01-02T '`).
        if bare_number(head) && head.len() == 4 {
            return LegacyReading::OutOfRange;
        }
        // The two halves are judged for their values before the `T` is judged for its
        // shape: a clock out of its range behind the `T` (`'2000-01-02T24:00'`) and a
        // year the target cannot hold in front of it (`'0102T13:05:06'`,
        // `'1234T13:05:06'`) are 242, where `'2000-1-2T03:05:06'` is 241.
        if read_legacy_loose(tail, LegacyNumbers::Default) == LegacyReading::OutOfRange {
            return LegacyReading::OutOfRange;
        }
        return match read_legacy_loose(head, LegacyNumbers::Default) {
            LegacyReading::Read(local) if local.days < DATETIME_MIN_DAYS => {
                LegacyReading::OutOfRange
            }
            _ => LegacyReading::NotADate,
        };
    }
    read_legacy_loose(trimmed, LegacyNumbers::Default)
}

/// The ISO 8601 spelling a legacy target reads without a style, and what may follow it.
///
/// The core is the one of styles 126 and 127, `yyyy[-mm-dd[Thh:mi:ss[.f{1,3}]]]` on its
/// full width ([`legacy_iso_core`]). Behind it exactly one of three things is admitted,
/// and nothing else:
///
/// * nothing: `'2000-01-02T13:05:06'`, `'2000-01-02'`, `'2000'`;
/// * a `Z`, glued or behind spaces: `'2000-01-02T13:05:06Z'`, `'2000-01-02 Z'`, `'2000 Z'`,
///   `'2000-01-02T13:05:06 Z'` are all read — where `'2000-1-2 Z'`, `'20000102 Z'` and
///   `'Jan 2 2000 Z'` are refused, the `Z` belonging to this spelling alone;
/// * a meridiem marker, glued or behind spaces, when the core carried a clock:
///   `'2000-01-02T13:05:06PM'`, `'2000-01-02T01:05:06PM'` and `'2000-01-02T13:05:06 PM'`
///   are one o'clock in the afternoon, `'2000-01-02T13:05:06AM'` is 242 like `'13:05AM'`.
///
/// Two of them together (`'2000-01-02T13:05:06PM Z'`, `'…Z PM'`), a second `Z`, a clock, a
/// month name or a year behind the core are not this spelling: `None`, and the loose
/// grammar refuses the `T`. A date the calendar refuses is 242 (`'0001-01-01T00:00:00'`
/// being 242 by the range of the target, checked later).
fn legacy_iso_under_cast(s: &str) -> Option<LegacyReading> {
    let (core, rest, has_clock) = legacy_iso_core(s)?;
    let tail = rest.trim_start_matches(' ');
    let local = match core {
        Ok(local) => local,
        Err(()) => return Some(LegacyReading::OutOfRange),
    };
    if tail.is_empty() || tail == "Z" {
        return Some(LegacyReading::Read(local));
    }
    if has_clock && let Some(pm) = meridiem_of(tail) {
        let hour = (local.ticks / TICKS_PER_HOUR) as u32;
        let rest_of_hour = local.ticks % TICKS_PER_HOUR;
        return Some(match hour_under_meridiem(hour, pm) {
            Some(hour) => LegacyReading::Read(Local {
                ticks: u64::from(hour) * TICKS_PER_HOUR + rest_of_hour,
                ..local
            }),
            None => LegacyReading::OutOfRange,
        });
    }
    None
}

/// 100-nanosecond ticks in one hour.
const TICKS_PER_HOUR: u64 = 3600 * TICKS_PER_SECOND;

/// Reads a string into a legacy target under the grammar a `CONVERT` style selects.
///
/// The rules below apply to `datetime` and `smalldatetime` alike, each named with a
/// vector that separates it from its neighbour:
///
/// * blanks on both ends are dropped, whatever the grammar: `'  2000-01-02'` and
///   `' 2000-01-02T13:05:06'` are read under 126 and 127, where the modern targets shut
///   the ISO grammar on a leading blank. A tabulation is treated as under `CAST`, where it
///   is refused;
/// * the empty string is the 1st of January 1900 under every style;
/// * under 126 and 127 the ISO spelling is tried first. Under 127 a clock alone
///   (`'13:05:06'`, `'13:05:06Z'`, `'1 PM'`) also reads, and so do the year/clock
///   neighbours `'2000 13:05:06Z'` and `'13:05:06 2000Z'`; `'20000102'`, `'Jan 2 2000'`
///   and `'2000-Jan-02'` are 241;
/// * a capital `T` that stands outside a word — `'2000-01-02T13:05:06'`, `'2000-01-02 T…'`,
///   `'T13:05:06'`, `'20000102T13:05:06'` — is never read by the loose grammar, and the
///   answer is 241 or 242 by [`read_legacy_glued_t`]: what stands **behind** the `T` is
///   judged first, then what stands in front of it. `'2000-01-02T13:05:06'` is therefore 242
///   under 101 and 241 under 77, `'20000102T13:05:06'` is 242 under both, and
///   `'2000-01-02T13:05:06.'` is 241 under 101 because its clock does not read.
fn read_legacy_styled(s: &str, style: LegacyStyle) -> LegacyReading {
    let trimmed = s.trim_matches(' ');
    if trimmed.is_empty() {
        return LegacyReading::Read(Local {
            days: DAYS_1900,
            ticks: 0,
            offset: None,
        });
    }
    if let LegacyStyle::Iso { zulu } = style {
        match parse_legacy_iso(trimmed, zulu) {
            Some(Ok(local)) => return LegacyReading::Read(local),
            Some(Err(())) => return LegacyReading::OutOfRange,
            None => {}
        }
    }
    if let Some(index) = glued_t(trimmed) {
        let head = trimmed[..index].trim_end_matches(' ');
        let tail = trimmed[index + 1..].trim_start_matches(' ');
        return read_legacy_glued_t(head, tail, style);
    }
    read_legacy_loose(trimmed, legacy_numbers_of(style))
}

/// The all-numeric rule a style selects.
fn legacy_numbers_of(style: LegacyStyle) -> LegacyNumbers {
    match style {
        LegacyStyle::Numbers { order, century } => LegacyNumbers::Ordered { order, century },
        LegacyStyle::Iso { zulu: true } => LegacyNumbers::ClockOnly,
        LegacyStyle::Iso { zulu: false } | LegacyStyle::Other | LegacyStyle::Hijri => {
            LegacyNumbers::Refused
        }
    }
}

/// What a legacy target answers to a literal cut at its glued `T` — `head` before the
/// letter, `tail` behind it — under a style: never a reading, always 241 or 242.
///
/// The same on `datetime` and `smalldatetime`, 241 being 295 towards a `smalldatetime`.
/// The tail is judged first, in three shapes:
///
/// * **a bare number**: three heads (separated date, compact date, empty) crossed with
///   numeric tails and styles 1/77. Up to three digits after a separated date gives 242
///   under 1 and 241 under 77; four digits gives 241 under both. A compact head gives 242
///   up to four digits. An empty head gives 242 for four digits (`T1305`) and 241
///   otherwise (`T13`). Five digits gives 241 on the three heads;
/// * **anything else that is not empty** must read on its own under the loose grammar of
///   the style, or the answer is 241 whatever the head: `'2000-01-02T13:05:06.'` is 241
///   under 101 where `'2000-01-02T13:05:06'` is 242, and so are `'…06.1234567'`, `'…06Z'`
///   and `'…06+02:00'`. A trailing `Z` is refused here before the
///   standalone clock check; the valid ISO spelling was already tried by the caller;
/// * **an empty tail**, or a tail that reads: the head decides, 242 when it reads under
///   the style, 241 when it does not. `'2000-01-02T13:05:06'` and `'2000-01-02T'` are 242
///   under 101 and 241 under 1 and 77; `'20000102T13:05:06'` and `'T13:05:06'` are 242
///   under 1, 77 and 101 alike — `'20000102'` and the empty string read under every
///   grammar but 127's — and `'20000102T13:05:06'` is 241 under 127, which refuses
///   that compact date head.
fn read_legacy_glued_t(head: &str, tail: &str, style: LegacyStyle) -> LegacyReading {
    if tail.ends_with('Z') {
        return LegacyReading::NotADate;
    }
    let numbers = legacy_numbers_of(style);
    if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
        if tail.len() > 4 || (head.is_empty() && tail.len() != 4) {
            return LegacyReading::NotADate;
        }
        let head_reads = !matches!(read_legacy_loose(head, numbers), LegacyReading::NotADate);
        return if head_reads || (tail.len() < 4 && matches!(style, LegacyStyle::Numbers { .. })) {
            LegacyReading::OutOfRange
        } else {
            LegacyReading::NotADate
        };
    }
    if !tail.is_empty() && !matches!(read_legacy_loose(tail, numbers), LegacyReading::Read(_)) {
        return LegacyReading::NotADate;
    }
    match read_legacy_loose(head, numbers) {
        LegacyReading::NotADate => LegacyReading::NotADate,
        LegacyReading::Read(_) | LegacyReading::OutOfRange => LegacyReading::OutOfRange,
    }
}

/// The byte index of the capital `T` that glues a clock to a date in a legacy literal,
/// `None` when there is none.
///
/// The letter counts only outside a word: the `T` of `'OCTOBER'` and of `'SEPT'` is not
/// one, and neither is the lowercase `t` of `'2000-01-02t13:05:06'`, which is refused as
/// no date at all under every style: 241 towards `datetime`, **295** towards
/// `smalldatetime` (`tests/convert_datetime.rs`).
fn glued_t(s: &str) -> Option<usize> {
    s.char_indices().find_map(|(index, c)| {
        if c != 'T' {
            return None;
        }
        let before = s[..index].chars().next_back();
        let after = s[index + 1..].chars().next();
        if before.is_some_and(is_word_letter) || after.is_some_and(is_word_letter) {
            return None;
        }
        let word_char = |c: &char| is_word_letter(*c) || *c == '\0';
        let start = index
            - s[..index]
                .chars()
                .rev()
                .take_while(word_char)
                .map(char::len_utf8)
                .sum::<usize>();
        let end = index
            + s[index..]
                .chars()
                .take_while(word_char)
                .map(char::len_utf8)
                .sum::<usize>();
        let word = &s[start..end];
        if word.contains('\0') && month_of(&word.replace('\0', "")).is_some() {
            return None;
        }
        Some(index)
    })
}

/// The ISO 8601 spelling the legacy styles 126 and 127 read, stricter than the one the
/// style-less reading of the modern targets knows ([`parse_iso_8601_parts`]):
///
/// `yyyy[-mm-dd[Thh:mi:ss[.f{1,3}]]][Z]`
///
/// Every piece is on its full width — `'2000-1-2'`, `'2000-01-02T1:05:06'` and
/// `'2000-01-02T13:5:6'` are 241 under 126 where the modern grammar reads all three — no
/// blank may precede the `T` (`'2000-01-02 T13:05:06'` is 241), the seconds are not
/// optional, the fraction stops at three digits (`'…06.1234'` is 241), and the `Z` is read
/// by 127 only (`'2000-01-02T13:05:06Z'` and `'2000Z'` are read under 127, 241 under 126).
/// An explicit offset is 241 under both. A date the calendar refuses is `Err(())`, error 242.
fn parse_legacy_iso(s: &str, zulu: bool) -> Option<Result<Local, ()>> {
    let (core, rest, _) = legacy_iso_core(s)?;
    match rest {
        "" => Some(core),
        "Z" if zulu => Some(core),
        _ => None,
    }
}

/// The core of the legacy ISO spelling, `yyyy[-mm-dd[Thh:mi:ss[.f{1,3}]]]`, cut off the
/// front of `s`: the reading (`Err(())` for a day the calendar refuses), what follows it,
/// and whether a clock was written.
///
/// The core stops where the spelling stops: `'2000-1-2'` has none (the month is not on two
/// digits), `'2000-01-02T13:05'` has none (the seconds are not optional), and
/// `'2000-01-02 13:05:06'` has the date alone, ` 13:05:06` being what follows.
fn legacy_iso_core(s: &str) -> Option<(Result<Local, ()>, &str, bool)> {
    let mut rest = s;
    let year = take_digits(&mut rest, 4, 4)?;
    let (mut month, mut day, mut ticks) = (1, 1, 0);
    let mut has_clock = false;
    if rest.starts_with('-') {
        take_char(&mut rest, '-')?;
        month = take_digits(&mut rest, 2, 2)?;
        take_char(&mut rest, '-')?;
        day = take_digits(&mut rest, 2, 2)?;
        if rest.starts_with('T') {
            take_char(&mut rest, 'T')?;
            let hour = take_digits(&mut rest, 2, 2)?;
            take_char(&mut rest, ':')?;
            let minute = take_digits(&mut rest, 2, 2)?;
            take_char(&mut rest, ':')?;
            let second = take_digits(&mut rest, 2, 2)?;
            let fraction = match take_char(&mut rest, '.') {
                Some(()) => {
                    let digits =
                        rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                    if !(1..=3).contains(&digits) {
                        return None;
                    }
                    let (written, tail) = rest.split_at(digits);
                    rest = tail;
                    parse_fraction(written)?
                }
                None => 0,
            };
            // A clock out of its range has the shape and not the value: 242, as
            // `'2000-01-02T24:00:00'` and `'2000-01-02T23:60'` are under `CAST`.
            ticks = match time_of(hour, minute, second, fraction) {
                Some(ticks) => ticks,
                None => return Some((Err(()), rest, true)),
            };
            has_clock = true;
        }
    }
    // A digit right behind the core is not a boundary: `'20000102'` and `'2000-01-021'`
    // are not this spelling.
    if rest.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let (month, day) = (to_u8(month)?, to_u8(day)?);
    let core = local_of(year as i32, month, day, ticks, None).ok_or(());
    Some((core, rest, has_clock))
}

/// Drops the spaces that decorate a separator or a colon: `'2000 - 01 - 02 13 : 05'`,
/// `'13:05:06 . 5'`, `'2 .Jan.2000'` and `'2000-Jan- 2'` read like their tight spellings,
/// and `'2 Jan -2000'` is the 242 of `'2 Jan-2000'`.
fn glue_separators(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut pending_spaces = 0;
    for c in body.chars() {
        if c == ' ' {
            pending_spaces += 1;
            continue;
        }
        if pending_spaces > 0
            && !GLUING_SEPARATORS.contains(&c)
            && !out.ends_with(GLUING_SEPARATORS)
        {
            out.push(' ');
        }
        pending_spaces = 0;
        out.push(c);
    }
    if pending_spaces > 0 {
        out.push(' ');
    }
    out
}

/// One token of a legacy literal, as [`legacy_tokens`] cuts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyToken {
    /// A run of digits: its value (saturated past nine digits, which no piece reads) and
    /// its width.
    Number {
        value: u32,
        digits: usize,
    },
    /// An English month name, `1..=12`.
    Month(u8),
    /// `AM` or `PM`, in any case: `Some(true)` for `PM`.
    Meridiem(bool),
    /// A clock, `h:m[:s[.f | :ms]]`: the hour, still on the 24-hour dial the marker may
    /// move, and the ticks past the hour.
    Clock {
        hour: u32,
        past_the_hour: u64,
    },
    /// One of `-`, `/` and `.`; the legacy grammar does not tell them apart.
    Separator,
    Comma,
    /// A colon outside a clock (`':05'`, the one of `'2000-01-02:13:05'`): never a date,
    /// but judged after the pieces, so that `'2000-01-02-13:05:06'` is the 242 of its
    /// fourth number and `'2000-01-02:13:05'` the 241 of the colon.
    Colon,
}

/// One token and whether a space stood in front of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spaced {
    token: LegacyToken,
    spaced: bool,
}

/// Cuts a legacy literal into its tokens, or says why it is not a date.
///
/// Spaces separate tokens. Within a run of letters/NUL only, NUL is ignored when the
/// remaining letters identify a month. It does not separate tokens or normalize
/// numeric/clock/meridiem tokens: NUL+Nov reads, NUL+space+Nov does not.
/// Other controls such as tabulation and line feed are not tokens. A run of letters
/// is a month name or a meridiem marker or nothing
/// (`'zzz'`, `'Sept'`, the `t` of `'2000-01-02t13:05:06'`); a run of digits followed by a
/// `:` is a clock, read by [`legacy_clock`].
fn legacy_tokens(s: &str) -> Result<Vec<Spaced>, LegacyReading> {
    let bytes = s.as_bytes();
    let mut tokens = Vec::new();
    let mut spaced = false;
    let mut at = 0;
    while at < bytes.len() {
        let b = bytes[at];
        let token = if b == b' ' {
            spaced = true;
            at += 1;
            continue;
        } else if b.is_ascii_digit() {
            let end = at + digit_run(&bytes[at..]);
            // A number a separator binds belongs to the date, colon or not: the `02` of
            // `'2000-01-02:13:05'` is its day, and the colon behind it is nothing (241).
            let bound = matches!(
                tokens.last(),
                Some(Spaced {
                    token: LegacyToken::Separator,
                    ..
                })
            );
            if bytes.get(end) == Some(&b':') && !bound {
                let stop = end
                    + bytes[end..]
                        .iter()
                        .take_while(|c| c.is_ascii_digit() || **c == b':' || **c == b'.')
                        .count();
                let clock = legacy_clock(&s[at..stop])?;
                at = stop;
                clock
            } else {
                let (value, digits) = (saturating_number(&s[at..end]), end - at);
                at = end;
                LegacyToken::Number { value, digits }
            }
        } else if let Some(len) = legacy_word_len(&s[at..]) {
            let raw = &s[at..at + len];
            let word = raw.replace('\0', "");
            at += len;
            match (month_of(&word), meridiem_of(raw)) {
                (Some(month), _) => LegacyToken::Month(month),
                (None, Some(pm)) => LegacyToken::Meridiem(pm),
                (None, None) => return Err(LegacyReading::NotADate),
            }
        } else {
            at += 1;
            match b {
                b'-' | b'/' | b'.' => LegacyToken::Separator,
                b',' => LegacyToken::Comma,
                b':' => LegacyToken::Colon,
                _ => return Err(LegacyReading::NotADate),
            }
        };
        tokens.push(Spaced { token, spaced });
        spaced = false;
    }
    Ok(tokens)
}

/// The length of the run of ASCII digits at the front of `bytes`.
fn digit_run(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| b.is_ascii_digit()).count()
}

/// The byte length of the run of letters and NULs at the front of `s` — a word of the legacy
/// grammar — `None` when `s` does not start with one. A letter is what [`is_word_letter`]
/// says, so `'\0Ｊａｎ 2 2000'` and `'Ｊ\0ａｎ 2 2000'` are words like `'\0Jan 2 2000'`.
fn legacy_word_len(s: &str) -> Option<usize> {
    let len = s
        .char_indices()
        .find(|(_, c)| !(is_word_letter(*c) || *c == '\0'))
        .map_or(s.len(), |(index, _)| index);
    (len > 0).then_some(len)
}

/// The ASCII letter a **fullwidth** Latin letter (U+FF21..=U+FF3A, U+FF41..=U+FF5A) stands
/// for; any other character comes back as it is.
///
/// `N'Ｊａｎ 2 2000'`, `N'Jaｎ 2 2000'`, `N'2-Ｊａｎ-2000'` and `N'1 PＭ'` read exactly as
/// their ASCII spellings, in the six targets. The fold does **not** come from the
/// collation of the operand: `SELECT TRY_CAST(N'Ｊａｎ 2 2000' COLLATE Latin1_General_BIN2
/// AS date)` is `2000-01-02` as it is under `_CS_AS`, `_100_CI_AS_WS` and `_100_CI_AI`,
/// `N'jan 2 2000' COLLATE Latin1_General_BIN2` is read too, and `N'Jän 2 2000' COLLATE
/// Latin1_General_100_CI_AI` stays NULL. The date reader folds width and case on its own,
/// regardless of the collation the string carries, which is why this function has no
/// collation parameter.
/// The fold stops there. The fullwidth digits, separators, `Ｔ`, `Ｚ`, colon, sign, comma
/// and ideographic space of the same literals are refused (`N'２000-01-02'`,
/// `N'Jan 2，2000'`, `N'13:05:06 Ｚ'`), and so are the accented, long-s, Greek, Cyrillic,
/// dotless-i, circled and superscript look-alikes of a month name or a marker
/// (`N'Jän 2 2000'`, `N'ſep 2 2000'`, `N'Јan 2 2000'`, `N'1 ΡM'`). A fullwidth letter
/// next to the `T` of a legacy literal is a letter too: `N'ＯＣTＯＢＥＲ 2 2000'` is the 2nd
/// of October, not a glued `T`.
fn fold_fullwidth(c: char) -> char {
    match c {
        '\u{FF21}'..='\u{FF3A}' | '\u{FF41}'..='\u{FF5A}' => {
            char::from_u32(u32::from(c) - 0xFF21 + u32::from('A')).unwrap_or(c)
        }
        _ => c,
    }
}

/// The one ligature the two grammars read inside a month name: `ﬆ` (U+FB06), read as
/// `st` in the spellings of `August` that carry it (`N'Auguﬆ 2 2000'`,
/// `N'2-Auguﬆ-2000'`, `N'2000 Auguﬆ13:05'`, `N'Ａｕｇｕﬆ 2 2000'`, `N'\0Auguﬆ 2 2000'`),
/// where the `ſt` ligature `ﬅ` (U+FB05), the long s of `N'Auguſt 2 2000'` and the
/// letter-like symbols `ℎ`, `ℓ`, `ℊ`, `ℴ`, `ℕ` and the Roman numerals `ⅿ`, `ⅾ`, `ⅽ` are
/// refused. Stated for that ligature; no other month name spells a pair a ligature
/// of the Alphabetic Presentation Forms block stands for.
const ST_LIGATURE: char = '\u{FB06}';

/// A letter as the two grammars see one when they look for a month name or a meridiem
/// marker: an ASCII letter, a fullwidth Latin letter that [`fold_fullwidth`] maps to one,
/// or the [`ST_LIGATURE`].
fn is_word_letter(c: char) -> bool {
    c == ST_LIGATURE || fold_fullwidth(c).is_ascii_alphabetic()
}

/// `s` with its fullwidth letters folded, its [`ST_LIGATURE`] spelled out and its letters
/// lowered: the key under which a month name or a meridiem marker is looked up.
fn folded_lowercase(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ST_LIGATURE => out.push_str("st"),
            _ => out.push(fold_fullwidth(c).to_ascii_lowercase()),
        }
    }
    out
}

/// The value of a run of digits, `u32::MAX` past what a `u32` holds: no piece of a date
/// reads that far, and the width is what refuses it.
fn saturating_number(digits: &str) -> u32 {
    digits.parse::<u32>().unwrap_or(u32::MAX)
}

/// Reads the clock of a legacy literal, `h:m[:s[.f | :ms]]`, every field on one to three
/// digits.
///
/// The shape is 241 when it is not that (`'13:'`, `':05'`, `'13:05:06.'`, `'13:05:06:1234'`,
/// `'13:05:06:7.5'`, `'1:05:06.5:07'`), and 242 when a field is out of its range (`'24:00'`,
/// `'12:60'`, `'23:59:60'`). The fields go to three digits, unlike the modern clock:
/// `'001:05'`, `'1:005'` and `'1:05:006'` are read. Fractions have one to three digits:
/// `.1111`, `.0000` and `.1000` are refused, even when the extra digits are zero.
fn legacy_clock(text: &str) -> Result<LegacyToken, LegacyReading> {
    let fields: Vec<&str> = text.split(':').collect();
    let field = |s: &str| parse_u32(s, 1, 3).ok_or(LegacyReading::NotADate);
    let hour = field(fields.first().copied().unwrap_or(""))?;
    let minute = field(fields.get(1).copied().unwrap_or(""))?;
    let (second, fraction) = match fields.get(2) {
        Some(s) => match s.split_once('.') {
            Some((whole, digits)) if digits.len() <= 3 => (
                field(whole)?,
                parse_fraction(digits).ok_or(LegacyReading::NotADate)?,
            ),
            Some(_) => return Err(LegacyReading::NotADate),
            None => (field(s)?, 0),
        },
        None => (0, 0),
    };
    let milliseconds = match fields.get(3) {
        Some(ms) if fraction == 0 && fields.len() == 4 => {
            u64::from(field(ms)?) * TICKS_PER_MILLISECOND
        }
        Some(_) => return Err(LegacyReading::NotADate),
        None => 0,
    };
    if hour > 23 || minute > 59 || second > 59 {
        return Err(LegacyReading::OutOfRange);
    }
    Ok(LegacyToken::Clock {
        hour,
        past_the_hour: (u64::from(minute) * 60 + u64::from(second)) * TICKS_PER_SECOND
            + fraction
            + milliseconds,
    })
}

/// The hour a meridiem marker makes of `hour`, `None` when the pair is impossible.
///
/// Through the legacy targets: `AM` keeps 0 to 11, turns 12 into 0 and refuses 13 and
/// beyond (`'13:05AM'` is 242); `PM` refuses 0
/// (`'0:05PM'` is 242), moves 1 to 11 past noon, and keeps 12 to 23 (`'13:05PM'` is 13:05).
fn hour_under_meridiem(hour: u32, pm: bool) -> Option<u32> {
    match (pm, hour) {
        (false, 12) => Some(0),
        (false, 0..=11) => Some(hour),
        (true, 12..=23) => Some(hour),
        (true, 1..=11) => Some(hour + 12),
        _ => None,
    }
}

/// One piece of the date half of a legacy literal, once the clock and the marker are out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyPiece {
    /// A number, and whether a separator glued it to the piece before.
    Number {
        value: u32,
        digits: usize,
        bound: bool,
    },
    /// A month name, and whether a separator glued it to the piece before.
    Month { month: u8, bound: bool },
}

impl LegacyPiece {
    fn bound(self) -> bool {
        match self {
            LegacyPiece::Number { bound, .. } | LegacyPiece::Month { bound, .. } => bound,
        }
    }
}

/// Reads the loose grammar of a legacy target: tokens separated by spaces, a date in
/// numbers and month names, a clock before or after it, a meridiem marker.
///
/// The rules, each with the vector that fixes it (the two legacy targets are quoted, 241
/// being 295 towards a `smalldatetime`):
///
/// * a separator must stand between two pieces of the date: `'2000-01-02-'` and
///   `'2000--01-02'` are 242, `'-2000-01-02'` and `'2000/'` are 241; spaces around it are
///   nothing (`'2000 - 01 - 02'` is read, [`glue_separators`]);
/// * a separator behind a month name needs one in front of it: `'2-Jan-2000'`,
///   `'2 .Jan.2000'` and `'2000-Jan-2'` are read, `'Jan-2000'`, `'Jan-2-2000'`,
///   `'2 Jan-2000'` and `'2000 Jan-2'` are 242; a separator in front of a month name needs
///   an unbound number in front: `'2000-Jan'` and `'2-Jan2000'` are read, `'2-2000-Jan'` is
///   242;
/// * a comma may follow a month name or a number of at most three digits, and must be
///   followed by a piece of the date: `'Jan,2,2000'`, `'2, Jan 2000'`, `'Jan 2, 00'` and
///   `'2000 Jan,2'` are read, `'2000,Jan'`, `'Jan 2 2000,'`, `',Jan 2 2000'`,
///   `'Jan 2,, 2000'` and `'Jan 2, 2000, 13:05'` are 242. Three numbers a comma separates
///   are not a date: `'01,02,2000'` and `'2000,01,02'` are 241;
/// * one clock and one marker at most: a second clock or a second marker is 242
///   (`'2000-01-02 13:05:06 13:05'`, `'2000-01-02 13:05 PM AM'`, `'1 PM 2000-01-02 1 PM'`),
///   and two clocks with no date at all are 241 (`'13:05 13:05'`);
/// * a marker with no clock in front of it takes the number just before it as the hour,
///   on one to three digits: `'2000-01-02 5 PM'`, `'2000 Jan001 PM'` and `'Jan 2 2000 1PM'`
///   are read. A marker with nothing before it is 242 (`'PM'`, `'PM 2000-01-02'`); a marker
///   whose number has four digits is 241 (`'Jan 2, 2000 PM'`);
/// * a fault of punctuation is 242 when the pieces still spell a date and 241 when they
///   do not: `'Jan-2000'` is 242 and `'Jan-02'` is 241, `'2000,Jan'` is 242 and
///   `'2000,01,02'` is 241 (see [`legacy_shape_is_complete`]);
/// * the pieces that remain make the date by [`legacy_date`].
fn read_legacy_loose(s: &str, numbers: LegacyNumbers) -> LegacyReading {
    // Signed zones fail on the clock/date spellings. Z is accepted on the
    // clock-alone/year-clock neighbours under CAST and 127, but not 101/120/126; a tab
    // before Z is refused, unlike an ASCII space.
    let (body, offset, zone_written) = split_offset(s);
    if zone_written
        && (!s.ends_with('Z')
            || !matches!(numbers, LegacyNumbers::Default | LegacyNumbers::ClockOnly)
            || s[..s.len() - 1]
                .chars()
                .any(|c| c != ' ' && c.is_ascii_whitespace()))
    {
        return LegacyReading::NotADate;
    }
    let tokens = match legacy_tokens(&glue_separators(body)) {
        Ok(tokens) => tokens,
        Err(refusal) => return refusal,
    };
    // Punctuation alone — `'-'`, `','`, `',,'` — is 242.
    if !tokens.is_empty()
        && tokens
            .iter()
            .all(|t| matches!(t.token, LegacyToken::Separator | LegacyToken::Comma))
    {
        return LegacyReading::OutOfRange;
    }
    // A colon outside a clock is no date, once the pieces have been judged.
    let mut stray_colon = false;
    let mut pieces: Vec<LegacyPiece> = Vec::new();
    let mut clock: Option<(u32, u64)> = None;
    let mut meridiem: Option<bool> = None;
    // The hour a marker took from the number before it, when there was no clock.
    let mut hour_alone: Option<u32> = None;
    let mut pending_separator = false;
    let mut pending_comma = false;
    // The marker took a number a separator had bound: the separator is left dangling,
    // and a number behind the marker may still take it.
    let mut separator_left_by_the_marker = false;
    // A fault of punctuation, judged once the pieces are known: 242 when they spell a
    // date, 241 when they do not.
    let mut placement_fault = false;
    // A second month name, judged at the end too: 242 beside a year, 241 without one
    // (`'Jan Jan 2000'` and `'Jan2Jan'`), and after the marker's hour (`'Jan1 Jan13 AM'` is
    // the 242 of its hour).
    let mut second_month = false;
    for (index, Spaced { token, .. }) in tokens.iter().enumerate() {
        let last = pieces.last().copied();
        let previous = index.checked_sub(1).map(|i| tokens[i].token);
        let previous_is_a_piece = matches!(
            previous,
            Some(LegacyToken::Number { .. } | LegacyToken::Month(_))
        );
        match *token {
            LegacyToken::Separator => {
                let next = tokens.get(index + 1).map(|t| t.token);
                let beside_a_marker = matches!(previous, Some(LegacyToken::Meridiem(_)))
                    || matches!(next, Some(LegacyToken::Meridiem(_)));
                let after_a_clock = matches!(previous, Some(LegacyToken::Clock { .. }));
                if index == 0 || beside_a_marker || (after_a_clock && pieces.is_empty()) {
                    return LegacyReading::NotADate;
                }
                if next.is_none() {
                    // A trailing separator behind a bound month name is nothing:
                    // `'2000-Jan-'` is the 1st of January 2000.
                    if matches!(last, Some(LegacyPiece::Month { bound: true, .. })) {
                        continue;
                    }
                    // A trailing separator: 242 behind a full numeric date, 241 otherwise
                    // (`'2000-01-02-'` and `'2000.01.02.'` are 242, `'2000/'`, `'1/2/'` and
                    // `'02-'` are 241).
                    let full_group = pieces.len() == 3
                        && pieces
                            .iter()
                            .all(|p| matches!(p, LegacyPiece::Number { .. }))
                        && pieces[1..].iter().all(|p| p.bound());
                    return match full_group && hour_alone.is_none() {
                        true => LegacyReading::OutOfRange,
                        false => LegacyReading::NotADate,
                    };
                }
                let unbound_month = matches!(last, Some(LegacyPiece::Month { bound: false, .. }));
                if !previous_is_a_piece || unbound_month {
                    placement_fault = true;
                }
                pending_separator = true;
            }
            LegacyToken::Comma => {
                let after_a_short_number =
                    matches!(last, Some(LegacyPiece::Number { digits, .. }) if digits <= 3);
                let after_a_month = matches!(last, Some(LegacyPiece::Month { .. }));
                if !previous_is_a_piece || !(after_a_short_number || after_a_month) {
                    placement_fault = true;
                }
                pending_comma = true;
            }
            LegacyToken::Number { value, digits } => {
                pieces.push(LegacyPiece::Number {
                    value,
                    digits,
                    bound: pending_separator,
                });
                pending_separator = false;
                pending_comma = false;
                separator_left_by_the_marker = false;
            }
            LegacyToken::Month(month) => {
                if pieces
                    .iter()
                    .any(|p| matches!(p, LegacyPiece::Month { .. }))
                {
                    second_month = true;
                }
                if pending_separator
                    && !matches!(last, Some(LegacyPiece::Number { bound: false, .. }))
                {
                    placement_fault = true;
                }
                pieces.push(LegacyPiece::Month {
                    month,
                    bound: pending_separator,
                });
                pending_separator = false;
                pending_comma = false;
            }
            LegacyToken::Colon => {
                // What follows the colon is not looked at: `'2000-01-0213:05'` is the 241
                // of its colon, not the 242 of a fourth number.
                stray_colon = true;
                break;
            }
            LegacyToken::Clock {
                hour,
                past_the_hour,
            } => {
                if pending_separator || pending_comma {
                    return LegacyReading::OutOfRange;
                }
                if clock.is_some() || meridiem.is_some() {
                    return match pieces.is_empty() {
                        true => LegacyReading::NotADate,
                        false => LegacyReading::OutOfRange,
                    };
                }
                clock = Some((hour, past_the_hour));
            }
            LegacyToken::Meridiem(pm) => {
                if meridiem.is_some() || pending_separator || pending_comma {
                    return LegacyReading::OutOfRange;
                }
                if clock.is_none() {
                    // The hour is the number just before the marker, on at most three
                    // digits; a four-digit number is a year and lends nothing.
                    let hour = match last {
                        None => return LegacyReading::OutOfRange,
                        Some(LegacyPiece::Number {
                            value,
                            digits,
                            bound,
                        }) if digits <= 3 && previous_is_a_piece => {
                            pieces.pop();
                            // `'2000-01-02 PM 1'` is the 1st of January 2000 at two in the
                            // afternoon: the marker took the `02`, and the hyphen it leaves
                            // behind binds the `1`. With nothing behind the marker the
                            // hyphen dangles, and `'2000-01-02PM'` and `'20000102-5 PM'`
                            // are 241, unless the hour was the fourth number of a group,
                            // `'2000-01-02-5 PM'` being 242.
                            if bound {
                                let group =
                                    1 + pieces.iter().rev().take_while(|p| p.bound()).count();
                                if group >= 3 {
                                    return LegacyReading::OutOfRange;
                                }
                                // Behind a bound month name the separator is nothing:
                                // `'2000-Jan-02 PM'` is the 1st of January 2000 at two.
                                if !matches!(
                                    pieces.last(),
                                    Some(LegacyPiece::Month { bound: true, .. })
                                ) {
                                    pending_separator = true;
                                    separator_left_by_the_marker = true;
                                }
                            }
                            value
                        }
                        Some(_) => return LegacyReading::NotADate,
                    };
                    match hour_under_meridiem(hour, pm) {
                        Some(hour) => hour_alone = Some(hour),
                        None => return LegacyReading::OutOfRange,
                    }
                }
                meridiem = Some(pm);
            }
        }
    }
    if pending_comma {
        placement_fault = true;
    }
    let ticks = match (clock, meridiem, hour_alone) {
        (Some((hour, past)), Some(pm), _) => match hour_under_meridiem(hour, pm) {
            Some(hour) => u64::from(hour) * TICKS_PER_HOUR + past,
            None => return LegacyReading::OutOfRange,
        },
        (Some((hour, past)), None, _) => u64::from(hour) * TICKS_PER_HOUR + past,
        (None, _, Some(hour)) => u64::from(hour) * TICKS_PER_HOUR,
        (None, _, None) => 0,
    };
    if second_month {
        let has_year = pieces
            .iter()
            .any(|p| matches!(p, LegacyPiece::Number { digits: 4, .. }));
        return match has_year {
            true => LegacyReading::OutOfRange,
            false => LegacyReading::NotADate,
        };
    }
    if placement_fault {
        return match legacy_shape_is_complete(&pieces) {
            true => LegacyReading::OutOfRange,
            false => LegacyReading::NotADate,
        };
    }
    if separator_left_by_the_marker {
        // `'2000-01-02PM'`, `'20000102-5 PM'` and `'2000-5PM'` are 241; beside a month
        // name the dangling separator is 242 (`'Jan 2000-1 PM'`, `'2000 Jan-1 PM'`).
        let has_month = pieces
            .iter()
            .any(|p| matches!(p, LegacyPiece::Month { .. }));
        return match has_month {
            true => LegacyReading::OutOfRange,
            false => LegacyReading::NotADate,
        };
    }
    // The Z neighbours read with an empty date or a four-digit year, but not a
    // separated/compact date or a named month.
    if zone_written
        && (clock.is_none() && hour_alone.is_none()
            || !matches!(
                pieces.as_slice(),
                [] | [LegacyPiece::Number { digits: 4, .. }]
            ))
    {
        return LegacyReading::NotADate;
    }
    if numbers == LegacyNumbers::ClockOnly && !pieces.is_empty() && !zone_written {
        return LegacyReading::NotADate;
    }
    let (year, month, day) = match legacy_date(&pieces, numbers) {
        Ok(_) if stray_colon => return LegacyReading::NotADate,
        Ok(Some(date)) => date,
        Ok(None) => (1900, 1, 1),
        Err(refusal) => return refusal,
    };
    match local_of(year, month, day, ticks, offset) {
        Some(local) => LegacyReading::Read(local),
        None => LegacyReading::OutOfRange,
    }
}

/// Whether the pieces of a legacy literal have the shape of a date, values aside: what
/// decides between 242 and 241 when the punctuation is at fault.
///
/// Nothing at all, one number of four, six or eight digits, three numbers bound
/// together, a month name and a four-digit year, a month name and two numbers of at most
/// four digits: `'Jan-2000'`, `'2 Jan-2000'`, `'2000,Jan'`, `'Jan 2 2000,'` and
/// `', 2000'` are 242; `'Jan-02'`, `'Jan.'`, `'2000,01,02'` and `',2 Jan'` are 241.
fn legacy_shape_is_complete(pieces: &[LegacyPiece]) -> bool {
    let months = pieces
        .iter()
        .filter(|p| matches!(p, LegacyPiece::Month { .. }))
        .count();
    let widths: Vec<usize> = pieces
        .iter()
        .filter_map(|p| match p {
            LegacyPiece::Number { digits, .. } => Some(*digits),
            LegacyPiece::Month { .. } => None,
        })
        .collect();
    match (months, widths.len()) {
        (0, 0) => true,
        (0, 1) => matches!(widths[0], 4 | 6 | 8),
        (0, 3) => pieces.iter().skip(1).all(|p| p.bound()),
        (1, 1) => widths[0] == 4,
        (1, 2) => widths.iter().all(|w| *w <= 4),
        _ => false,
    }
}

/// The civil date the pieces of a legacy literal spell, `None` when there is none (a
/// clock alone, the empty string), or the refusal.
///
/// The shapes, each with its vectors:
///
/// * **one number**: four digits are a year (`'2000'` is its 1st of January), six and
///   eight digits are `yymmdd` and `yyyymmdd` (`'000102'`, `'20000102'`; `'123456'` is 242
///   for its month), any other width is 241 (`'2'`, `'200'`, `'20001'`, `'200001020'`);
/// * **three numbers joined by separators**, under the rule the style selects
///   ([`date_under_no_style`], [`date_under_legacy_style`]); three numbers not joined are
///   241 (`'01 02 2000'`, `'2000-01 02'`, `'01,02,2000'`) unless two of them are years or
///   one is a compact date, which is 242 (`'2000 0001 0002'`, `'2000 01 000002'`); two
///   numbers are 241 (`'1/2'`, `'2000 2'`) unless both are years or one is a compact date
///   (`'2000 0102'`, `'200001 02'` are 242); four and more are 242 when bound or beside a
///   year (`'2000-01-02-03'`, `'2000-01-02 2000'`), 241 otherwise (`'2 2 2 2000'`);
/// * **a month name and one number**: the number is the year on four digits
///   (`'Jan 2000'`, `'2000-Jan'`, `'Jan,2000'`), and nothing else (`'Jan 2'`, `'Jan 2 13:05'`,
///   `'Jan 100'` are 241, unlike the modern grammar);
/// * **a month name and two numbers**, in any order and however joined: the year is the
///   four-digit number if there is one, otherwise the **second** of the two (`'Jan 2 5'`
///   and `'2 Jan 5'` are the 2nd of January 2005), the other is the day, on one to three
///   digits (`'Jan 002 2000'` and `'2000-Jan-002'` are read, `'Jan 0002 2000'` and
///   `'2000-Jan-0002'` are 242 as a second year, `'2000-Jan-000002'` is 242 as a compact
///   date, `'2000-Jan-00002'` is 241). Two numbers bound to each other beside a month
///   name are 242 (`'Jan 2-2000'`, `'01/02 Jan'`);
/// * **a month name and three numbers**, or a month name alone: 242 and 241
///   (`'Jan 2 2000 2'`, `'Jan'`).
///
/// A year on one to three digits is read by its **value**: `0..=49` is 2000 to 2049,
/// `50..=99` is 1950 to 1999 (`'01-02-000'` and `'Jan 2 0'` are 2000, `'01-02-050'` is
/// 1950), and `100..=999` is 242 (`'01/02/999'`, `'01-02-100'`).
fn legacy_date(
    pieces: &[LegacyPiece],
    numbers: LegacyNumbers,
) -> Result<Option<(i32, u8, u8)>, LegacyReading> {
    let months: Vec<u8> = pieces
        .iter()
        .filter_map(|p| match p {
            LegacyPiece::Month { month, .. } => Some(*month),
            LegacyPiece::Number { .. } => None,
        })
        .collect();
    let nums: Vec<(u32, usize)> = pieces
        .iter()
        .filter_map(|p| match p {
            LegacyPiece::Number { value, digits, .. } => Some((*value, *digits)),
            LegacyPiece::Month { .. } => None,
        })
        .collect();
    let years = nums.iter().filter(|(_, digits)| *digits == 4).count();
    let compact = nums.iter().any(|(_, digits)| matches!(digits, 6 | 8));
    let all_bound = pieces.iter().skip(1).all(|p| p.bound());
    let any_bound = pieces.iter().any(|p| p.bound());
    if let Some(&month) = months.first() {
        // Two numbers bound to each other beside a month name: `'Jan 2-2000'`.
        let numbers_bound_together = pieces.windows(2).any(|w| {
            matches!(w[0], LegacyPiece::Number { .. })
                && matches!(w[1], LegacyPiece::Number { bound: true, .. })
        });
        if numbers_bound_together || (compact && nums.len() >= 2) {
            return Err(LegacyReading::OutOfRange);
        }
        return match nums[..] {
            [] => Err(LegacyReading::NotADate),
            [(year, 4)] => Ok(Some((year as i32, month, 1))),
            [_] => Err(LegacyReading::NotADate),
            [(a, da), (b, db)] => {
                if da > 4 || db > 4 {
                    return Err(LegacyReading::NotADate);
                }
                let (year, day) = match (da == 4, db == 4) {
                    (true, true) => return Err(LegacyReading::OutOfRange),
                    (true, false) => (a as i32, b),
                    (false, true) => (b as i32, a),
                    (false, false) => (legacy_short_year(b).ok_or(LegacyReading::OutOfRange)?, a),
                };
                Ok(Some((
                    year,
                    month,
                    to_u8(day).ok_or(LegacyReading::OutOfRange)?,
                )))
            }
            _ => Err(LegacyReading::OutOfRange),
        };
    }
    match nums[..] {
        [] => Ok(None),
        [(value, digits)] => match digits {
            4 => Ok(Some((value as i32, 1, 1))),
            6 => Ok(Some((
                legacy_short_year(value / 10_000).ok_or(LegacyReading::OutOfRange)?,
                ((value / 100) % 100) as u8,
                (value % 100) as u8,
            ))),
            8 => Ok(Some((
                (value / 10_000) as i32,
                ((value / 100) % 100) as u8,
                (value % 100) as u8,
            ))),
            _ => Err(LegacyReading::NotADate),
        },
        [_, _, _] if all_bound => match numbers {
            LegacyNumbers::Default => date_under_no_style(&nums),
            LegacyNumbers::Ordered { order, century } => {
                date_under_legacy_style(&nums, order, century)
            }
            LegacyNumbers::Refused | LegacyNumbers::ClockOnly => Err(LegacyReading::NotADate),
        },
        [_, _] | [_, _, _] => match !any_bound && (years == nums.len() || compact) {
            true => Err(LegacyReading::OutOfRange),
            false => Err(LegacyReading::NotADate),
        },
        _ => match any_bound || years >= 2 || compact {
            true => Err(LegacyReading::OutOfRange),
            false => Err(LegacyReading::NotADate),
        },
    }
}

/// A year written on one to three digits, by its value: the pivot of 49 (2049) and 50
/// (1950), and nothing past 99.
fn legacy_short_year(value: u32) -> Option<i32> {
    match value {
        0..=TWO_DIGIT_YEAR_CUTOFF => Some((2000 + value) as i32),
        50..=99 => Some((1900 + value) as i32),
        _ => None,
    }
}

/// Reads three numbers joined by separators as a legacy target reads them without a
/// style: `DATEFORMAT mdy`, the year found by its width first.
///
/// One four-digit piece is the year wherever it stands, and the two others are month then
/// day: `'2000-01-02'`, `'01/02/2000'` and `'01/2000/02'` are all the 2nd of January 2000
/// (`'2000-1-002'` too: three digits count as a value). No four-digit piece: month, day,
/// year in that order, the year by its value (`'01-02-000'` is 2000, `'01/02/999'` is 242,
/// `'49-01-02'` is 242 for its month). Two four-digit pieces (`'2000/2000/01'`), or a piece
/// of five digits and more (`'2000-01-00002'`), are 241.
fn date_under_no_style(nums: &[(u32, usize)]) -> Result<Option<(i32, u8, u8)>, LegacyReading> {
    if nums.iter().any(|(_, digits)| *digits > 4) {
        return Err(LegacyReading::NotADate);
    }
    let wide: Vec<usize> = nums
        .iter()
        .enumerate()
        .filter(|(_, (_, digits))| *digits == 4)
        .map(|(slot, _)| slot)
        .collect();
    let piece = |value: u32| to_u8(value).ok_or(LegacyReading::OutOfRange);
    match wide[..] {
        [year_slot] => {
            let mut others = nums
                .iter()
                .enumerate()
                .filter(|(slot, _)| *slot != year_slot)
                .map(|(_, (value, _))| *value);
            let (month, day) = (others.next().unwrap_or(0), others.next().unwrap_or(0));
            Ok(Some((nums[year_slot].0 as i32, piece(month)?, piece(day)?)))
        }
        [] => {
            let year = legacy_short_year(nums[2].0).ok_or(LegacyReading::OutOfRange)?;
            Ok(Some((year, piece(nums[0].0)?, piece(nums[1].0)?)))
        }
        _ => Err(LegacyReading::NotADate),
    }
}

/// Reads three numbers joined by separators as a legacy target reads them under a style
/// that names an order: the `SET DATEFORMAT` rule, not the modern style rule.
///
/// The difference with [`date_under_style`] is that the **year is found by its width**
/// before the order is applied, and the order then places the two remaining pieces
/// (`tests/convert_datetime.rs` holds the vectors below):
///
/// * on four digits, the year is the one four-digit piece wherever it stands, and the other
///   two are month then day under `mdy` **and** `ymd`, day then month under `dmy`:
///   `'2000-01-02'` is the 2nd of January under 101 and 102 and the **1st of February**
///   under 103, `'01/02/2000'` is the 2nd of January under 102 and the 1st of February under
///   105, `'01/2000/02'` the 2nd of January under 101. No four-digit piece, or two of them,
///   is 241 (`'01/02/49'` and `'2000/2000/01'` under 101); a piece out of its range is 242
///   (`'2000-999-01'` under 101, and `'1899-12-31'` under 103, whose `dmy` makes 31 the
///   month);
/// * on two digits, the three pieces are read in the order of the style: `'01/02/49'` is the
///   2nd of January 2049 under 1, the 1st of February under 3, and 242 under 2, the 49th of
///   February 2001. Over the six permutations of `999`, `2000`, `01` under 1, 2, 3 and 10,
///   the answer is 241 except for `999-2000-01`, `999-01-2000` and `2000-999-01` under 2
///   (242). A large month/day therefore does not invariably precede the four-digit
///   rejection: `999-2000-01` under 1 is 241, not 242. Without a four-digit piece,
///   `999-01-02` under 1 and `01/02/999` under 2 are 242.
///
/// The separator is not looked at, as with the modern rule: `'01.02.2000'`, `'01-02-2000'`
/// and `'2000/01/02'` are read under 101.
fn date_under_legacy_style(
    nums: &[(u32, usize)],
    order: Order,
    century: Century,
) -> Result<Option<(i32, u8, u8)>, LegacyReading> {
    let piece = |value: u32| to_u8(value).ok_or(LegacyReading::OutOfRange);
    match century {
        Century::Four => {
            let mut wide = nums
                .iter()
                .enumerate()
                .filter(|(_, (_, digits))| *digits == 4);
            let Some((year_slot, (year, _))) = wide.next() else {
                return Err(LegacyReading::NotADate);
            };
            if wide.next().is_some() {
                return Err(LegacyReading::NotADate);
            }
            let mut others = nums
                .iter()
                .enumerate()
                .filter(|(slot, _)| *slot != year_slot)
                .map(|(_, (value, _))| *value);
            let (first, second) = (others.next().unwrap_or(0), others.next().unwrap_or(0));
            let (month, day) = match order {
                Order::Dmy => (second, first),
                _ => (first, second),
            };
            Ok(Some((*year as i32, piece(month)?, piece(day)?)))
        }
        Century::Two => {
            let [year, month, day] = order.slots();
            // Over the six permutations of 999, 2000 and 01: a four-digit non-year piece
            // wins over a large month/day. Under ymd, a three-digit year or a large middle
            // month wins first instead.
            if nums.iter().any(|(_, width)| *width >= 4) {
                if matches!(order, Order::Ymd)
                    && ((nums[0].1 == 3) || (nums[0].1 == 4 && nums[1].1 < 4 && nums[1].0 >= 100))
                {
                    return Err(LegacyReading::OutOfRange);
                }
                return Err(LegacyReading::NotADate);
            }
            for (slot, (value, digits)) in nums.iter().enumerate() {
                if (slot == year && *digits == 3) || (slot != year && *value >= 100) {
                    return Err(LegacyReading::OutOfRange);
                }
            }
            let (year_value, year_digits) = nums[year];
            let year = expand_year(year_value, year_digits).ok_or(LegacyReading::OutOfRange)?;
            Ok(Some((year, piece(nums[month].0)?, piece(nums[day].0)?)))
        }
    }
}

/// Reads `v`, of type `from`, into the local date and time every target is built from.
fn read_local(v: &Value, from: &SqlType, to: &SqlType) -> SqlResult<Local> {
    // Exactly one pair of date types is refused: a `date` has no time and a `time` has no
    // date, so neither can be read as the other.
    if matches!(
        (from, to),
        (SqlType::Date, SqlType::Time(_)) | (SqlType::Time(_), SqlType::Date)
    ) {
        return Err(errors::explicit_conversion_not_allowed(from, to));
    }
    match v {
        Value::String(s) => parse_string(&s.text).ok_or_else(SqlError::conversion_failed_datetime),
        Value::Date(d) => Ok(Local {
            days: d.days,
            ticks: 0,
            offset: None,
        }),
        Value::Time(t) => Ok(Local {
            days: DAYS_1900,
            ticks: t.ticks_100ns,
            offset: None,
        }),
        Value::DateTime(dt) => Ok(Local {
            days: DAYS_1900.saturating_add(dt.days),
            ticks: ticks_300th_to_100ns(dt.ticks_300th),
            offset: None,
        }),
        Value::DateTime2(dt) => Ok(Local {
            days: dt.date.days,
            ticks: dt.time.ticks_100ns,
            offset: None,
        }),
        Value::DateTimeOffset(dto) => Ok(local_of_offset(*dto)),
        // A `uniqueidentifier` has no date reading at all: SQL Server refuses the cast.
        Value::Guid(_) => Err(errors::explicit_conversion_not_allowed(from, to)),
        // A number and a binary never reach [`Local`]: [`to_datetime`] routes them to
        // [`number_to_datetime`] and [`binary_to_datetime`] before the style is even read.
        Value::Bit(_)
        | Value::I8(_)
        | Value::I16(_)
        | Value::I32(_)
        | Value::I64(_)
        | Value::Decimal(_)
        | Value::F64(_)
        | Value::F32(_)
        | Value::Money(_)
        | Value::Bytes(_) => Err(errors::bug("convert: to_datetime routed a number wrongly")),
        // `convert` answers `NULL` before dispatching, so this is a broken precondition.
        Value::Null => Err(errors::bug("convert: NULL reached to_datetime")),
    }
}

// ---------------------------------------------------------------------------------------
// A number and a binary read as a `datetime`. `tests/convert_datetime.rs` crosses the
// twelve numeric types with twenty-six values and the six targets, the source type with
// its scale, the sign and the two bounds, binary values with the lengths 0 to 12, and
// exact-tie values with `float`, `real` and `decimal`.
// ---------------------------------------------------------------------------------------

/// The first day a `datetime` holds, counted from 1900-01-01: 1753-01-01.
const DATETIME_MIN_DAY: i32 = DATETIME_MIN_DAYS - DAYS_1900;

/// The last day a `datetime` holds, counted from 1900-01-01: 9999-12-31.
const DATETIME_MAX_DAY: i32 = MAX_DAYS - DAYS_1900;

/// The last day a `smalldatetime` holds, counted from 1900-01-01: 2079-06-06. Its first
/// day is 1900-01-01 itself, so the count is a `u16` with no negative half.
const SMALLDATETIME_MAX_DAY: i32 = SMALLDATETIME_MAX_DAYS - DAYS_1900;

/// A day count no source can hold and still name a date, whatever the target.
///
/// The guard exists for the arithmetic rather than for the rule: a `decimal(38, 0)` holds
/// a hundred million million million million million days, and multiplying that by 25 920
/// 000 would overflow an `i128`. Rejecting it first bounds every product below, and costs
/// nothing in fidelity — a day count that large is 8115 either way.
const IMPOSSIBLE_DAY: i128 = 2_958_464;

/// [`IMPOSSIBLE_DAY`] as a double, the guard of the approximate path.
const IMPOSSIBLE_DAY_AS_DOUBLE: f64 = 2_958_464.0;

/// Bytes of the binary form of a `datetime`: a signed day count then a tick count, both
/// big-endian.
const DATETIME_BYTES: usize = 8;

/// Bytes of the binary form of a `smalldatetime`: an unsigned day count then a minute
/// count, both big-endian.
const SMALLDATETIME_BYTES: usize = 4;

/// The exact value a numeric source holds.
///
/// The distinction matters at the tie of the rounding, and only there: SQL Server rounds
/// on the **exact** value of the source, so a `decimal` one unit in the twenty-eighth
/// decimal place below half a tick rounds down, while the `float` written the same way
/// rounds up, the double nearest that literal sitting *above* the tie
/// (`number_to_datetime_reads_an_exact_source_exactly` in `tests/convert_datetime.rs`). A
/// model that multiplied the double by 25 920 000 and rounded the product would answer
/// the same thing for both, and would miss twelve of the twenty-four exact ties tested.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ExactValue {
    /// `mantissa / 10^scale`: `bit`, the four integer types, `decimal`, `numeric`,
    /// `money` (scale 4) and `smallmoney` (scale 4).
    Scaled { mantissa: i128, scale: u8 },
    /// A `float`, or a `real` widened to a double without loss. The value is the exact
    /// binary rational the double holds, not the decimal that was written.
    Double(f64),
}

/// The scale of `money` and `smallmoney`: the amount is stored in ten-thousandths.
///
/// A ten-thousandth of a day is exactly 2 592 ticks of 1/300 s, so **no `money` value ever
/// rounds towards a `datetime`**: `CAST(CAST(0.0001 AS money) AS datetime)` is
/// `00:00:08.640` to the tick, and so is every other one.
///
/// The clause holds for that **target only**, and the function serves two. A
/// `smalldatetime` counts 1 440 minutes in a day, where the same ten-thousandth is 0.144
/// minute and a `money` rounds like any other number, up as well as down:
/// `CAST(CAST(0.0004 AS money) AS smalldatetime)` is `00:01` (0.576 minute) where
/// `CAST(CAST(0.0003 AS money) AS smalldatetime)` is `00:00` (0.432 minute), and
/// `CAST(CAST(0.0035 AS money) AS smalldatetime)` is `00:05` (5.04 minutes). The same three
/// amounts towards a `datetime` are exact to the tick — `00:00:34.560`, `00:00:25.920` and
/// `00:05:02.400`, which is what makes them the vector that tells the two targets apart
/// (`money_rounds_towards_a_smalldatetime` in `tests/convert_datetime.rs`).
const MONEY_SCALE: u8 = 4;

/// Reads `v`, a value of the numeric type `from`, as the date type `to`.
///
/// The integer part of the number counts **days since 1900-01-01** and the fractional part
/// the fraction of a day, negative values running backwards from the origin while their
/// fraction still runs forward: `-1.5` is noon on the 30th of December 1899
/// (`number_to_datetime_before_1900` in `tests/convert_datetime.rs`).
///
/// The rule is one line, and the same for the two targets: the value is multiplied by the
/// number of units the target counts in a day — 25 920 000 ticks of 1/300 s for a
/// `datetime`, 1 440 minutes for a `smalldatetime` — and the product is rounded **half away
/// from zero** on the total, not on the fraction of its own day. Rounding the total is what
/// makes `-1 + 40.5 ticks` land on tick 40 of the 31st of December where `+1 + 40.5 ticks`
/// lands on tick 41 of the 2nd of January. The day is then the Euclidean quotient and
/// the time of day the remainder.
///
/// The range is checked **after** the rounding: a fraction that rounds past the last tick
/// of 9999-12-31 overflows (`number_to_datetime_bounds` in `tests/convert_datetime.rs`). The refusal is **8115**, naming `expression` and the
/// target, state 2, and not the 242 a date source raises: the two paths raise different
/// numbers for the same overflow, which is why a number never goes through
/// [`write_local`].
///
/// The four types of 2008 refuse a number outright: `CAST(CAST(1 AS int) AS date)` is
/// **529**, raised for each of the twelve numeric source types crossed with twenty-six
/// values (`number_to_the_2008_types_is_529` in `tests/convert_datetime.rs`).
fn number_to_datetime(v: &Value, from: &SqlType, to: &SqlType) -> SqlResult<Value> {
    let Some(value) = exact_value(v) else {
        return Err(errors::bug(
            "convert: a numeric type without a numeric value",
        ));
    };
    let units_per_day = match to {
        SqlType::DateTime => TICKS_300TH_PER_DAY,
        SqlType::SmallDateTime => TICKS_300TH_PER_DAY / TICKS_300TH_PER_MINUTE,
        // The four types of 2008 refuse a number, whatever its type and value
        // (`number_to_the_2008_types_is_529` in `tests/convert_datetime.rs`).
        SqlType::Date | SqlType::Time(_) | SqlType::DateTime2(_) | SqlType::DateTimeOffset(_) => {
            return Err(errors::explicit_conversion_not_allowed(from, to));
        }
        _ => return Err(errors::bug("convert: not a date target")),
    };
    let overflow = || errors::arithmetic_overflow_from("expression", to);
    let total = total_units(value, units_per_day).ok_or_else(overflow)?;
    let per_day = i128::from(units_per_day);
    let days = i32::try_from(total.div_euclid(per_day)).map_err(|_| overflow())?;
    // The remainder of a Euclidean division is never negative, so it is the time of day
    // whichever side of 1900 the value falls on.
    let rest = u32::try_from(total.rem_euclid(per_day)).map_err(|_| overflow())?;
    match to {
        SqlType::DateTime => {
            if !(DATETIME_MIN_DAY..=DATETIME_MAX_DAY).contains(&days) {
                return Err(overflow());
            }
            Ok(Value::DateTime(DateTime {
                days,
                ticks_300th: rest,
            }))
        }
        _ => {
            if !(0..=SMALLDATETIME_MAX_DAY).contains(&days) {
                return Err(overflow());
            }
            Ok(Value::DateTime(DateTime {
                days,
                ticks_300th: rest * TICKS_300TH_PER_MINUTE,
            }))
        }
    }
}

/// The exact value behind a numeric [`Value`], `None` when the value is not numeric.
fn exact_value(v: &Value) -> Option<ExactValue> {
    let scaled = |mantissa: i128| ExactValue::Scaled { mantissa, scale: 0 };
    Some(match v {
        Value::Bit(b) => scaled(i128::from(*b)),
        Value::I8(n) => scaled(i128::from(*n)),
        Value::I16(n) => scaled(i128::from(*n)),
        Value::I32(n) => scaled(i128::from(*n)),
        Value::I64(n) => scaled(i128::from(*n)),
        Value::Decimal(d) => ExactValue::Scaled {
            mantissa: d.mantissa,
            scale: d.scale,
        },
        Value::Money(m) => ExactValue::Scaled {
            mantissa: i128::from(*m),
            scale: MONEY_SCALE,
        },
        Value::F64(f) => ExactValue::Double(*f),
        // A `real` widens to a double exactly, and the rule then reads that double: the
        // exact ties of a `real` land where the widened value says they do.
        Value::F32(f) => ExactValue::Double(f64::from(*f)),
        _ => return None,
    })
}

/// `value * units_per_day`, rounded half away from zero, or `None` when the value cannot
/// name a date at all.
fn total_units(value: ExactValue, units_per_day: u32) -> Option<i128> {
    match value {
        ExactValue::Scaled { mantissa, scale } => scaled_units(mantissa, scale, units_per_day),
        ExactValue::Double(f) => double_units(f, units_per_day),
    }
}

/// [`total_units`] for `mantissa / 10^scale`.
///
/// The multiplication is done on the reduced fraction and in two halves — the whole
/// multiples of the denominator first, the remainder second — so that no product ever
/// leaves an `i128`, even for a `decimal(38, 38)`.
fn scaled_units(mantissa: i128, scale: u8, units_per_day: u32) -> Option<i128> {
    let power = 10_i128.checked_pow(u32::from(scale))?;
    let magnitude = i128::try_from(mantissa.unsigned_abs()).ok()?;
    if magnitude / power > IMPOSSIBLE_DAY {
        return None;
    }
    let common = i128::try_from(gcd(u128::from(units_per_day), power.unsigned_abs())).ok()?;
    let factor = i128::from(units_per_day) / common;
    let divisor = power / common;
    let whole = magnitude / divisor;
    let rest = magnitude % divisor;
    let total = whole * factor + div_round_half_away(rest * factor, divisor);
    Some(if mantissa < 0 { -total } else { total })
}

/// [`total_units`] for the exact binary value of a double.
fn double_units(v: f64, units_per_day: u32) -> Option<i128> {
    if !v.is_finite() || v.abs() > IMPOSSIBLE_DAY_AS_DOUBLE {
        return None;
    }
    let (mantissa, exponent) = decompose(v)?;
    // `|mantissa| < 2^53` and `units_per_day < 2^25`, so the product holds in 78 bits.
    let product = mantissa * i128::from(units_per_day);
    if exponent >= 0 {
        // The guard above bounds the value, so the shift cannot run away.
        product.checked_shl(u32::try_from(exponent).ok()?)
    } else {
        let shift = exponent.unsigned_abs();
        if shift >= 127 {
            // The value is smaller than 2^-49 unit: it rounds to none of them.
            return Some(0);
        }
        Some(div_round_half_away(product, 1_i128 << shift))
    }
}

/// Splits a finite double into `(mantissa, exponent)` with `v = mantissa * 2^exponent`,
/// `None` for a NaN or an infinity.
fn decompose(v: f64) -> Option<(i128, i32)> {
    const SIGNIFICAND_BITS: u32 = 52;
    const EXPONENT_MASK: u64 = 0x7FF;
    const BIAS: i32 = 1075;
    let bits = v.to_bits();
    let biased = ((bits >> SIGNIFICAND_BITS) & EXPONENT_MASK) as i32;
    if biased == EXPONENT_MASK as i32 {
        return None;
    }
    let fraction = bits & ((1_u64 << SIGNIFICAND_BITS) - 1);
    let (significand, exponent) = if biased == 0 {
        (fraction, 1 - BIAS)
    } else {
        (fraction | (1_u64 << SIGNIFICAND_BITS), biased - BIAS)
    };
    let magnitude = i128::from(significand);
    Some((
        if v.is_sign_negative() {
            -magnitude
        } else {
            magnitude
        },
        exponent,
    ))
}

/// `numerator / denominator` rounded half **away from zero**; `denominator > 0`.
fn div_round_half_away(numerator: i128, denominator: i128) -> i128 {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder.unsigned_abs() * 2 < denominator.unsigned_abs() {
        quotient
    } else if numerator < 0 {
        quotient - 1
    } else {
        quotient + 1
    }
}

/// The greatest common divisor of two positive integers.
fn gcd(a: u128, b: u128) -> u128 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Reads `v`, the bytes of a `binary` or a `varbinary`, as the date type `to`.
///
/// A `datetime` is eight bytes — a signed day count from 1900-01-01 then an unsigned count
/// of 1/300 s ticks, both big-endian — and a `smalldatetime` four: an unsigned day count
/// then the minute of the day. The value is read **right-aligned** on that width, so a
/// shorter one is padded on the left with zeroes and a longer one loses its leading bytes,
/// silently and whatever they hold: `CAST(0x41 AS datetime)` is `00:00:00.217`, sixty-five
/// ticks, and twelve bytes of `0xFF` in front of a valid eight change nothing
/// (`binary_to_datetime_other_lengths` in `tests/convert_datetime.rs`).
/// The empty binary is midnight on 1900-01-01. Note that this is *not* what a `CAST` to
/// `binary(n)` does: `CAST(0x41 AS binary(8))` pads on the **right** and then reads a day
/// count of 0x41000000, which is out of range.
///
/// A tick count of 25 920 000 or more, a minute of 1 440 or more, or a day outside the
/// range of the target, is refused with **210**, a message that names `datetime` for a
/// `smalldatetime` target too (`binary_to_datetime_out_of_range` in
/// `tests/convert_datetime.rs`).
///
/// # One deliberate difference from SQL Server
///
/// SQL Server reads the four types of 2008 from a binary too, on their own storage width
/// and with a rule of its own: a `date` takes the **leading** three bytes little-endian
/// and demands that the bytes behind them be zero, so `CAST(0x00010000 AS date)` is
/// `0001-09-14` and `CAST(0x0001000A AS date)` is 241. That is not the right-alignment
/// of this function, which stops at `datetime` and `smalldatetime`; the four types
/// raise 8114 here.
fn binary_to_datetime(v: &Value, from: &SqlType, to: &SqlType) -> SqlResult<Value> {
    let Value::Bytes(bytes) = v else {
        return Err(errors::bug("convert: a binary type without a binary value"));
    };
    let refused = || errors::error_converting(from, to);
    let out_of_range_210 = || SqlError::converting_datetime_from_binary();
    match to {
        SqlType::DateTime => {
            let w = right_aligned::<DATETIME_BYTES>(bytes);
            let days = i32::from_be_bytes([w[0], w[1], w[2], w[3]]);
            let ticks_300th = u32::from_be_bytes([w[4], w[5], w[6], w[7]]);
            if ticks_300th >= TICKS_300TH_PER_DAY
                || !(DATETIME_MIN_DAY..=DATETIME_MAX_DAY).contains(&days)
            {
                return Err(out_of_range_210());
            }
            Ok(Value::DateTime(DateTime { days, ticks_300th }))
        }
        SqlType::SmallDateTime => {
            let w = right_aligned::<SMALLDATETIME_BYTES>(bytes);
            // The day count is unsigned and its largest value, 65 535, is 2079-06-06
            // itself: a `smalldatetime` read from bytes cannot overflow its calendar,
            // only its clock.
            let days = i32::from(u16::from_be_bytes([w[0], w[1]]));
            let minutes = u32::from(u16::from_be_bytes([w[2], w[3]]));
            if u64::from(minutes) >= MINUTES_PER_DAY {
                return Err(out_of_range_210());
            }
            Ok(Value::DateTime(DateTime {
                days,
                ticks_300th: minutes * TICKS_300TH_PER_MINUTE,
            }))
        }
        _ => Err(refused()),
    }
}

/// The last `N` bytes of `bytes`, left-padded with zeroes when there are fewer than `N`.
fn right_aligned<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut word = [0_u8; N];
    let taken = bytes.len().min(N);
    word[N - taken..].copy_from_slice(&bytes[bytes.len() - taken..]);
    word
}

/// Reads a `datetimeoffset` as the local date and time a client sees, `utc + offset`.
fn local_of_offset(dto: DateTimeOffset) -> Local {
    let day = TICKS_PER_DAY as i64;
    let shifted =
        dto.utc.time.ticks_100ns as i64 + i64::from(dto.offset_minutes) * TICKS_PER_MINUTE as i64;
    Local {
        days: dto
            .utc
            .date
            .days
            .saturating_add(shifted.div_euclid(day) as i32),
        ticks: shifted.rem_euclid(day) as u64,
        offset: Some(dto.offset_minutes),
    }
}

/// Reads a `datetimeoffset` as the **universal** instant it stores, which is what style 1
/// asks for where the style-less default and style 0 ask for [`local_of_offset`].
fn utc_of_offset(dto: DateTimeOffset) -> Local {
    Local {
        days: dto.utc.date.days,
        ticks: dto.utc.time.ticks_100ns,
        // This reading **is** universal time, so it names an offset of zero rather than the
        // one the source carried.
        offset: Some(0),
    }
}

/// Builds a value of type `to` out of the local date and time `local`, rounding the fraction
/// of a second to the precision of `to` and checking the range of `to` afterwards.
///
/// A literal whose fraction rounded past midnight — [`Local::ticks`] at its upper bound —
/// is written differently by each half of the family, which is why the carry waits here:
/// a target that holds a date **and** a time moves to the next day, a `date` keeps the day
/// that was written, and a `time` keeps the last instant its scale can spell.
/// `CAST(N'2000-01-02T23:59:59.99999995' AS date)` is the 2nd of January and the same
/// literal as a `time(7)` is `23:59:59.9999999`, where as a `datetime2(7)` it is the 3rd at
/// midnight (`fraction_carry_stops_at_a_date_or_a_time` in `tests/convert_datetime.rs`).
///
/// `from` only serves the message of error 242, which names both types.
fn write_local(local: Local, from: &SqlType, to: &SqlType) -> SqlResult<Value> {
    let (carried_days, carried_ticks) = carried_day_and_time(&local);
    match to {
        SqlType::Date => {
            // The date written is the date read: the carry of a fraction has no day of its
            // own to land on here.
            check_range(local.days, 0, MAX_DAYS, from, to)?;
            Ok(Value::Date(Date { days: local.days }))
        }
        SqlType::Time(scale) => {
            // A `time` has no date to carry into, and the carry does not wrap round to
            // midnight either: it stays on the last instant the scale can spell.
            // `CAST(N'23:59:59.9996' AS time(3))` is `23:59:59.999`, and so is the same
            // reading taken through a `time(7)`, and so is a fraction that already rounded
            // past midnight while it was being read.
            let (ticks, carry) = round_to_scale(local.ticks.min(TICKS_PER_DAY - 1), *scale);
            let ticks = match carry != 0 || local.ticks >= TICKS_PER_DAY {
                false => ticks,
                true => last_tick_of_day(*scale),
            };
            Ok(Value::Time(Time { ticks_100ns: ticks }))
        }
        SqlType::DateTime => {
            let (ticks_300th, carry) = round_to_300th(carried_ticks);
            let days = carried_days.saturating_add(carry);
            check_range(days, DATETIME_MIN_DAYS, MAX_DAYS, from, to)?;
            Ok(Value::DateTime(DateTime {
                days: days - DAYS_1900,
                ticks_300th,
            }))
        }
        SqlType::SmallDateTime => {
            // A character string is read the way a `datetime` reads it — snapped to the
            // 1/300 s — **before** the minute is decided, and that snap is what carries
            // `'…13:05:29.999'` over the thirty-second line (see [`legacy_snap`]).
            let (snapped, snap_carry) = legacy_snap(carried_ticks, from);
            let (minutes, carry) = round_to_minute(snapped);
            let days = carried_days
                .saturating_add(snap_carry)
                .saturating_add(carry);
            check_range(days, DAYS_1900, SMALLDATETIME_MAX_DAYS, from, to)?;
            Ok(Value::DateTime(DateTime {
                days: days - DAYS_1900,
                ticks_300th: minutes * TICKS_300TH_PER_MINUTE,
            }))
        }
        SqlType::DateTime2(scale) => {
            let (ticks, days) = round_within_calendar(carried_days, carried_ticks, *scale);
            check_range(days, 0, MAX_DAYS, from, to)?;
            Ok(Value::DateTime2(DateTime2 {
                date: Date { days },
                time: Time { ticks_100ns: ticks },
            }))
        }
        SqlType::DateTimeOffset(scale) => {
            let (ticks, days) = round_within_calendar(carried_days, carried_ticks, *scale);
            check_range(days, 0, MAX_DAYS, from, to)?;
            // Anything that does not carry an offset of its own is read as UTC.
            let offset_minutes = local.offset.unwrap_or(0);
            let utc = utc_of_local(days, ticks, offset_minutes);
            // The range of a `datetimeoffset` is the range of the **universal** instant it
            // stores, not of the local reading: `'0001-01-01T01:59:59+02:00'` and
            // `'9999-12-31T22:00:00-02:00'` are readable `datetime2` values whose UTC falls
            // off the calendar, and the refusal is 8114, naming nvarchar and datetimeoffset,
            // not the 242 of a local overflow. One hour either way,
            // `'0001-01-01T02:00:00+02:00'` and `'9999-12-31T21:59:59-02:00'`, is read.
            if !(0..=MAX_DAYS).contains(&utc.date.days) {
                return Err(errors::error_converting(from, to));
            }
            Ok(Value::DateTimeOffset(DateTimeOffset {
                utc,
                offset_minutes,
            }))
        }
        // `to_datetime` is only reached for a target of the `DateTime` family.
        _ => Err(errors::bug("convert: not a date target")),
    }
}

/// Turns a local reading back into the UTC instant a `datetimeoffset` stores, `local - offset`.
fn utc_of_local(days: i32, ticks: u64, offset_minutes: i16) -> DateTime2 {
    let day = TICKS_PER_DAY as i64;
    let shifted = ticks as i64 - i64::from(offset_minutes) * TICKS_PER_MINUTE as i64;
    DateTime2 {
        date: Date {
            days: days.saturating_add(shifted.div_euclid(day) as i32),
        },
        time: Time {
            ticks_100ns: shifted.rem_euclid(day) as u64,
        },
    }
}

/// Error 242 when `days` falls outside `min..=max`, the range of the target type.
fn check_range(days: i32, min: i32, max: i32, from: &SqlType, to: &SqlType) -> SqlResult<()> {
    if (min..=max).contains(&days) {
        Ok(())
    } else {
        Err(errors::out_of_range(from, to))
    }
}

/// Rounds a time of day to `scale` fractional digits, half **up**, and says whether the
/// rounding reached the next day.
///
/// SQL Server rounds, never truncates: `'23:59:59.9999'` read as a `datetime2(3)` is the
/// following midnight, and `'13:05:06.1235'` is `13:05:06.124`.
fn round_to_scale(ticks: u64, scale: u8) -> (u64, i32) {
    let unit = 10_u64.pow(u32::from(
        MAX_FRACTION_DIGITS.saturating_sub(scale.min(MAX_FRACTION_DIGITS)),
    ));
    let rounded = (ticks + unit / 2) / unit * unit;
    if rounded >= TICKS_PER_DAY {
        (0, 1)
    } else {
        (rounded, 0)
    }
}

/// The last instant of a day a scale of `scale` fractional digits can spell:
/// `23:59:59.9999999` at 7, `23:59:59.999` at 3, `23:59:59` at 0.
fn last_tick_of_day(scale: u8) -> u64 {
    let unit = 10_u64.pow(u32::from(
        MAX_FRACTION_DIGITS.saturating_sub(scale.min(MAX_FRACTION_DIGITS)),
    ));
    TICKS_PER_DAY - unit
}

/// Rounds a time of day to `scale` fractional digits and carries into the next day, unless
/// that would leave the calendar.
///
/// On the 31st of December 9999 the rounding does not overflow: it stays on the last instant
/// the scale can spell. `CAST(N'9999-12-31 23:59:59.9999' AS datetime2(3))` is
/// `9999-12-31 23:59:59.999` and `… AS datetime2(0)` is `9999-12-31 23:59:59`, where the same
/// fraction on the 1st of January 2000 moves to the 2nd.
fn round_within_calendar(days: i32, ticks: u64, scale: u8) -> (u64, i32) {
    let (ticks, carry) = round_to_scale(ticks, scale);
    match carry != 0 && days >= MAX_DAYS {
        true => (last_tick_of_day(scale), days),
        false => (ticks, days.saturating_add(carry)),
    }
}

/// Rounds a time of day to the 1/300 s of a `datetime`, and says whether it reached the next
/// day.
///
/// This is why a `datetime` only ever shows milliseconds ending in 0, 3 and 7: `.998` and
/// `.999` are not representable, and round to `.997` and to the next second.
fn round_to_300th(ticks: u64) -> (u32, i32) {
    let rounded = ticks_100ns_to_300th(ticks);
    if rounded >= TICKS_300TH_PER_DAY {
        (0, 1)
    } else {
        (rounded, 0)
    }
}

/// Snaps a time of day to the 1/300 s of a `datetime` when the source of a `smalldatetime`
/// is a **character string**, and says whether the snap reached the next day.
///
/// A `smalldatetime` holds no second at all, so nothing but the rounding to the minute ever
/// shows this step — and it shows it on exactly one three-digit millisecond, `.999`, which
/// is the only one the 1/300 s snap lifts to a whole second. That single vector is what
/// tells the two readings apart (`smalldatetime_string_rounds_the_fraction` in
/// `tests/convert_datetime.rs`):
///
/// * `CAST('2000-01-02 13:05:29.999' AS smalldatetime)` is **13:06**, where `.998`, `.997`,
///   `.995`, `.501`, `.500`, `.499`, `.001` and `.000` on the same second are all 13:05 —
///   and the reading of the same eight strings as a `datetime` shows why: only `.999`
///   becomes `13:05:30.000`, where `.998`, `.997` and `.995` all become `13:05:29.997`;
/// * every millisecond of second 30 is 13:06, so the line sits between `29.998` and
///   `29.999` and nowhere else.
///
/// This is a rule of the **string** reading, not of the target — of this target: **towards a
/// `smalldatetime`**, a `datetime2`, a `time` and a `datetimeoffset` source do not pass
/// through the 1/300 s, so `CAST(CAST('2000-01-02 13:05:29.9999999' AS datetime2(7)) AS
/// smalldatetime)` is 13:05 where the same characters cast straight to `smalldatetime` are
/// 13:06. Towards a `datetime` the three of them do pass through it, and the very same
/// `datetime2(7)` value is then `13:05:30.000` — but that is the rounding the `datetime`
/// target owes every source, not this snap (`smalldatetime_rounding_by_source_type` in
/// `tests/convert_datetime.rs`). A `datetime` source is already a 1/300 s tick, so the
/// snap is a no-op there.
///
/// It also decides an overflow, because the range is checked after the rounding:
/// `CAST('2079-06-06 23:59:29.999' AS smalldatetime)` is error **242** where `'…29.998'` is
/// 23:59 (`smalldatetime_string_rounds_past_the_range` in `tests/convert_datetime.rs`).
fn legacy_snap(ticks: u64, from: &SqlType) -> (u64, i32) {
    if !from.is_string() {
        return (ticks, 0);
    }
    let (ticks_300th, carry) = round_to_300th(ticks);
    (ticks_300th_to_100ns(ticks_300th), carry)
}

/// Rounds a time of day to the whole minute of a `smalldatetime`, half **up** (30 seconds
/// move to the next minute), and says whether it reached the next day.
///
/// Half **up** and "the whole second is 30 or more" are the same rule here: a time of day
/// is up iff its second and fraction reach 30, whether the fraction is dropped first or not.
/// The fraction of a character string is not dropped, it is snapped to the 1/300 s by
/// [`legacy_snap`] before this rounding sees it.
fn round_to_minute(ticks: u64) -> (u32, i32) {
    let minutes = (ticks + TICKS_PER_MINUTE / 2) / TICKS_PER_MINUTE;
    if minutes >= MINUTES_PER_DAY {
        (0, 1)
    } else {
        (minutes as u32, 0)
    }
}

/// The three characters that separate the pieces of an all-numeric date inside one token.
///
/// The comma is not one of them: it stands in exactly one place — just before a year that
/// ends a date written with a month name — so [`parse_string`] reads it on its own.
const DATE_SEPARATORS: [char; 3] = ['/', '-', '.'];

/// The characters whose neighbouring blanks are not separators but decoration.
///
/// A blank next to one of them is ignored, so `'2000 - 01 - 02'`, `'01/ 02/2000'`,
/// `'13 : 05'` and `'13:05:06 . 5'` read like their tight spellings. The comma is
/// deliberately absent: it has its own single legal place, and joining it would lose it.
const GLUING_SEPARATORS: [char; 4] = ['/', '-', '.', ':'];

/// One piece of the date half of a literal: a run of digits, or an English month name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatePart {
    /// A number, with the count of digits that were written (a two-digit year is not a
    /// four-digit one) and the rank of the whitespace-delimited token it came from.
    Num {
        value: u32,
        digits: usize,
        token: usize,
    },
    /// A month, `1..=12`.
    Month(u8),
}

/// Reads a date and/or time literal, **without a style**, into a local date and time.
///
/// `None` means "no reading", which the caller turns into error 241. A reading that
/// succeeds may still be refused later by the range of the target (error 242).
///
/// # Which forms are read
///
/// The forms `tests/convert_datetime.rs` reads and refuses through `convert` are the
/// list; nothing else here claims to know it.
///
/// # How the reading is organised
///
/// Everything assumes the session defaults `us_english` and `SET DATEFORMAT mdy`.
///
/// Two grammars are tried, in this order.
///
/// 1. The **ISO 8601** grammar of [`parse_iso_8601_parts`], which reads the whole string at once
///    and is the only one that admits a time-zone designator behind a bare date
///    (`'2000-01-02Z'`, `'2000-01-02+02:00'`).
/// 2. Otherwise the **loose** grammar: the string is cut into blank-delimited tokens and
///    read in three halves — a date, a clock, a time-zone designator — the clock and the
///    designator always **behind** the date. A literal with no clock is midnight, one with
///    no date is the 1st of January 1900, and the empty string is both.
///
/// Only the space and the tabulation are blanks. A line feed, a carriage return or a form
/// feed lands inside a token, where nothing can read it, so `'2000-01-02\n13:05:06'` is
/// error 241.
///
/// A two-digit year pivots at 2049, the default *two digit year cutoff* of SQL Server. A
/// fraction of a second longer than seven digits is rounded to seven, and the rounding may
/// carry into the next day.
fn parse_string(s: &str) -> Option<Local> {
    match parse_with_style(s, StyleConstraint::None) {
        StyleReading::Read(local) => Some(local),
        StyleReading::NotADate | StyleReading::DoesNotFollowStyle => None,
    }
}

/// What reading a string under a `CONVERT` style gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StyleReading {
    /// The string was read.
    Read(Local),
    /// The string is no date at all: error 241.
    NotADate,
    /// The string is a date, written in a shape the style does not describe: error 9807
    /// (modern targets only; a legacy target never raises it, see [`read_legacy_styled`]).
    DoesNotFollowStyle,
}

/// Reads a date literal under the constraint a `CONVERT` style puts on it, for one of the
/// four modern targets (the legacy ones go through [`read_legacy_styled`]).
///
/// The style constrains **one** thing: an all-numeric date written in a single token. Every
/// other shape — the ISO 8601 grammar with its `T`, a compact `20000102`, a month name, a
/// clock alone, the empty string — is read exactly as it is without a style, whatever the
/// style says. `CONVERT(date, '2000-01-02T13:05:06', 112)` is therefore read where
/// `CONVERT(date, '2000-01-02 13:05:06', 112)` is error 9807, and
/// `CONVERT(date, '20000102', 6)` is read where `CONVERT(date, '2000-01-02', 6)` is 9807.
///
/// Under one of the sixteen strict styles, the 9807 is decided by [`strict_style_refuses`]
/// on the **head** of the string, before the loose grammar reads anything: a string that
/// starts with a numeric date the style does not write is 9807 even when the rest of it is
/// no date (`'01/02/2000x'`), and a string whose head is not such a date is 241
/// (`'x 01/02/2000'`, `',01/02/2000'`; `tests::strict_style_frontier_9807` and
/// `tests::strict_style_frontier_241`). See the frontier in that function's rustdoc.
fn parse_with_style(s: &str, style: StyleConstraint) -> StyleReading {
    let trimmed = s.trim_matches(is_blank);
    if trimmed.is_empty() {
        return StyleReading::Read(Local {
            days: DAYS_1900,
            ticks: 0,
            offset: None,
        });
    }
    // A blank in **front** shuts the ISO 8601 grammar: `'2000-01-02T13:05:06'`,
    // `'2000-01-02Z'` and `'2000-01-02+02:00'` are read, and the same three one space
    // further right are error 241, where a blank behind them changes nothing. What is
    // left of them is then read by the loose grammar, which refuses a `T`, a lone `Z` and
    // an offset with no clock in front of it.
    if !s.starts_with(is_blank)
        && let Some((local, beyond_the_date)) = parse_iso_8601_parts(trimmed)
        && (style == StyleConstraint::None || beyond_the_date)
    {
        return StyleReading::Read(local);
    }
    if style == StyleConstraint::NotNumbers && strict_style_refuses(trimmed) {
        return StyleReading::DoesNotFollowStyle;
    }
    match parse_loose(trimmed, style) {
        Some(local) => StyleReading::Read(local),
        None => StyleReading::NotADate,
    }
}

/// Whether a strict style — one that writes its date with a month name, without separators,
/// or writes no date — refuses `s` with error 9807 rather than 241 (tests
/// `strict_style_frontier_9807` and `strict_style_frontier_241` in `tests::` below).
///
/// `s` is the literal with its leading and trailing blanks removed. The frontier between the
/// two errors is drawn on valid literals degraded in many ways (a letter, a blank, a comma,
/// a number or a second date in front, behind or inside; an extra separator; a `T`, a `Z`,
/// an offset behind), and the four modern targets answer alike
/// (`tests::strict_style_frontier_is_the_same_on_the_four_modern_targets`). What the
/// frontier looks like:
///
/// * the **sixteen strict styles** alone raise 9807: 6, 7, 8, 9, 12, 13, 14, 24, 100, 106,
///   107, 108, 109, 112, 113, 114. The numeric styles (1, 3, 4, 20, 101, 103, 104, 105,
///   110, 111, 120, 121, 126, 127 and style 0) raise 241 on each degraded literal;
/// * the 9807 is decided on the **head** of the literal, and nothing before it may stand:
///   `'x 01/02/2000'`, `'9 01/02/2000'`, `',01/02/2000'`, `'13:05 01/02/2000'` and
///   `'x01/02/2000'` are 241;
/// * the head is a **numeric date**, `<digits><sep><digits><sep><digits>`, the same separator
///   twice, at least one digit in each piece (`'1/2/3'` and `'00.01.0'` qualify, `'01/02/'`,
///   `'01/02/x'`, `'01/02 2000'` and `'01/02'` do not), the separator being `-`, `/` or
///   `.` — a mixed pair (`'01.02/2000'`, `'2000-01/02'`) is 241, and so is a doubled one
///   (`'01//02/2000'`). A blank beside the separator is decoration (`'01 /02/2000'`,
///   `'2000- 01-02'` are 9807), as in [`tokenise`];
/// * with `/` or `.`, the head decides **alone**: `'01/02/2000x'`, `'01/02/200x'`,
///   `'00.01.0x'`, `'01/02/2000T13:05:06'`, `'01/02/2000/2000'`, `'1/2/3/4/5'`,
///   `'01/02/2000,'`, `'01/02/2000 x'`, `'01/02/2000 Z'`, `'01/02/2000 PM'`,
///   `'01/02/2000 x 3'` and `'01/02/2000+02:00'` are 9807;
/// * with `-`, the head must be a **whole token** — `'2000-01-02x'`, `'1-2-3x'`,
///   `'2000-01-02-03'`, `'2000-01-02/03'`, `'2000-01-02,'` and `'2000-01-02T'` are 241 —
///   and what follows the token decides: nothing (`'2000-01-02'`, `'  2000-01-02  '`) or a
///   **digit** (`'2000-01-02 3'`, `'2000-01-02 3x'`, `'2000-01-02 1x'`, `'2000-01-02\t3'`,
///   `'2000-01-02 13:05 x'`, `'2000-01-02 13:05:06,'`, `'2000-01-02 13:05:06 Jan'`,
///   `'2000-01-02 2000-01-02'`, `'2000-01-02 02 Jan 2000'`, `'2000-01-02 5 PMx'`) is 9807,
///   anything else (`'2000-01-02 x'`, `'2000-01-02 Z'`, `'2000-01-02 PM'`,
///   `'2000-01-02 Jan'`, `'2000-01-02 ,'`, `'2000-01-02 +02:00'`, `'2000-01-02 -03'`,
///   `'2000-01-02 x3'`) is 241.
///
/// The ISO 8601 grammar is tried before this rule by [`parse_with_style`], so a dash date
/// that it reads — `'2000-01-02Z'`, `'2000-01-02+02:00'`, `'2000-01-02T13:05:06'` — does
/// not come here; one it refuses (`'2000-01-02Z 3'`, `'2000-01-02T13:05:06x'`) is not a
/// whole dash token either, and is 241 (test `strict_style_frontier_241` in `tests::`).
fn strict_style_refuses(s: &str) -> bool {
    let bytes = s.as_bytes();
    let blank_run = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count()
    };
    let mut at = digit_run(bytes);
    if at == 0 {
        return false;
    }
    at += blank_run(at);
    let separator = match bytes.get(at) {
        Some(c @ (b'-' | b'/' | b'.')) => *c,
        _ => return false,
    };
    for _ in 0..2 {
        if bytes.get(at) != Some(&separator) {
            return false;
        }
        at += 1 + blank_run(at + 1);
        let digits = digit_run(&bytes[at..]);
        if digits == 0 {
            return false;
        }
        at += digits;
        // On the second turn, `bytes.get(at)` is the separator before the third piece,
        // or what follows a two-piece date, which the loop then refuses.
        at += blank_run(at);
    }
    // `at` sits past the blanks that follow the third piece.
    if separator != b'-' {
        return true;
    }
    let tail_start = at - blank_run_back(bytes, at);
    // A whole token: the third piece is followed by a blank or by nothing.
    if tail_start < bytes.len() && !matches!(bytes[tail_start], b' ' | b'\t') {
        return false;
    }
    bytes.get(at).is_none_or(u8::is_ascii_digit)
}

/// The length of the run of blanks that ends at `end` in `bytes`.
fn blank_run_back(bytes: &[u8], end: usize) -> usize {
    bytes[..end]
        .iter()
        .rev()
        .take_while(|b| **b == b' ' || **b == b'\t')
        .count()
}

/// A blank of a date literal: the pieces of one are separated by a space or a tabulation,
/// and by nothing else.
fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Reads the loose grammar: blank-delimited tokens, a date then a clock then a time-zone
/// designator.
///
/// `style` constrains the numeric date of a single token and nothing else. `None` is
/// error 241: the 9807 of a strict style is decided before this grammar runs, by
/// [`strict_style_refuses`].
fn parse_loose(s: &str, style: StyleConstraint) -> Option<Local> {
    let (body, offset, zone_written) = split_offset(s);
    // The comma is read as a token of its own, wherever it was written: `Jan 2,2000`,
    // `Jan 2, 2000` and `Jan 2 , 2000` are the same literal.
    let body = body.replace(',', " , ");
    let tokens = split_glued_clocks(tokenise(&body));

    let mut clock: Option<String> = None;
    let mut meridiem: Option<bool> = None;
    let mut parts: Vec<DatePart> = Vec::new();
    // Where in `parts` a comma stood, and whether a token used `-`, `/` or `.`.
    let mut comma: Option<usize> = None;
    let mut separator_seen = false;
    // Whether a token spelled its month name between two separators, the one shape that
    // mixes a month name with a separator.
    let mut month_between_separators = false;
    // Where in `parts` the last token left a one-or-two-digit number, as its **last**
    // piece: the hour of `'Jan 2 2024 1 PM'`, should a meridiem marker follow it. The
    // number does not have to be the whole token — `'2000 Jan1 PM'` is one o'clock in the
    // afternoon of the 1st of January 2000, and `'2000 Jan001 PM'` and `'Jan1 2000 PM'`
    // are error 241.
    //
    // A token that used a **date separator** never lends its hour, though: `'2000-1 PM'`,
    // `'2000.01 PM'`, `'20000102-5 PM'` and `'000102/5 PM'` are error 241, where the same
    // hour one blank further — `'2000 1 PM'`, `'20000102 5 PM'`, `'2000-01-02 5 PM'` — is
    // read, and where a separator in an **earlier** token changes nothing
    // (`'2000-Jan-2 1 PM'` is read). The separator has to be in the very token that carries
    // the hour.
    let mut hour_candidate: Option<usize> = None;
    for (rank, token) in tokens.iter().enumerate() {
        let token = token.as_str();
        if token == "," {
            // One comma at most, and it belongs to the date, never to the clock.
            if comma.is_some() || clock.is_some() || meridiem.is_some() {
                return None;
            }
            comma = Some(parts.len());
            hour_candidate = None;
        } else if let Some(pm) = meridiem_of(token) {
            if meridiem.is_some() {
                return None;
            }
            meridiem = Some(pm);
            if clock.is_none() {
                // `1 PM`: an hour with no minutes, written as the token before the marker.
                // A marker with no number before it (`'PM'`) is not a literal.
                let index = hour_candidate?;
                let DatePart::Num { value, .. } = parts.remove(index) else {
                    return None;
                };
                clock = Some(value.to_string());
            }
            hour_candidate = None;
        } else if is_clock(token) {
            if clock.is_some() || meridiem.is_some() {
                return None;
            }
            clock = Some(token.to_owned());
            hour_candidate = None;
        } else {
            // The date comes first: nothing of it may follow the clock or its marker.
            if clock.is_some() || meridiem.is_some() {
                return None;
            }
            let before = parts.len();
            let shape = push_date_parts(token, rank, &mut parts)?;
            separator_seen |= shape.used_a_separator;
            month_between_separators |= shape.month_between_separators;
            hour_candidate = match parts.last() {
                Some(DatePart::Num { digits, .. })
                    if *digits <= 2 && parts.len() > before && !shape.used_a_separator =>
                {
                    Some(parts.len() - 1)
                }
                _ => None,
            };
        }
    }

    let ticks = match &clock {
        Some(text) => parse_time(text, meridiem)?,
        // A meridiem without a clock is not a literal SQL Server reads.
        None if meridiem.is_some() => return None,
        None => 0,
    };
    // Outside the ISO 8601 grammar, a time-zone designator needs a clock in front of it:
    // `'13:05:06 Z'` and `'2000-01-02 1 PM+02:00'` are read, `'2000-01-02 Z'`,
    // `'20000102Z'`, `'Jan 2 2000 Z'`, `'2000 Z'` and `'2000-01-02 +02:00'` are error 241.
    if zone_written && clock.is_none() {
        return None;
    }
    if parts.is_empty() {
        // A comma with nothing to punctuate is not a literal.
        if comma.is_some() {
            return None;
        }
        return Some(Local {
            days: DAYS_1900,
            ticks,
            offset,
        });
    }
    // A month name and a separator only ever meet in one shape, `<number><sep><month
    // name><sep><number>`, written in a single token: `'2-Jan-2000'` and `'2000/Jan/2'` are
    // read, while `'Jan-2-2000'`, `'2-2000-Jan'`, `'Jan/2000'`, `'2000-Jan2'` and
    // `'01/02 Jan'` are error 241.
    let has_month = parts.iter().any(|p| matches!(p, DatePart::Month(_)));
    let glued_month = month_between_separators && parts.len() == 3;
    if has_month && separator_seen && !glued_month {
        return None;
    }
    let shape = Shape {
        has_clock: clock.is_some(),
        month_between_separators: has_month && separator_seen && glued_month,
    };
    let (y, m, d) = match style_applies(&parts, style) {
        // A style only ever judges an all-numeric date written in one token: three numbers
        // spread over three tokens (`'01 02 2000'`) are error 241 under every style, the
        // style-less refusal, and never 9807 (`tests/convert_datetime.rs`).
        None => parse_date(&parts, comma, shape)?,
        // A numeric date under a strict style that `strict_style_refuses` let through:
        // it was not at the head of the literal, or a dash date followed by something
        // that is no digit. `',01/02/2000'`, `'2000-01-02,'` and `'2000-01-02 ,'` land
        // here, and are error 241 (`tests::strict_style_frontier_241`).
        Some(StyleConstraint::NotNumbers) => return None,
        Some(StyleConstraint::Numbers { order, century }) => {
            date_under_style(&parts, comma, order, century)?
        }
        // `to_datetime` refuses a Hijri style before reading, and `StyleConstraint::None`
        // is not a constraint.
        Some(_) => parse_date(&parts, comma, shape)?,
    };
    local_of(y, m, d, ticks, offset)
}

/// The constraint that bears on `parts`, `None` when the style has nothing to say about it.
fn style_applies(parts: &[DatePart], style: StyleConstraint) -> Option<StyleConstraint> {
    if style == StyleConstraint::None {
        return None;
    }
    match parts {
        [
            DatePart::Num { token: a, .. },
            DatePart::Num { token: b, .. },
            DatePart::Num { token: c, .. },
        ] if a == b && b == c => Some(style),
        _ => None,
    }
}

/// Reads three numbers written in one token as the date the style spells.
///
/// The style fixes the **order** of the three pieces and the **width of the year**, and
/// nothing else: the separator is not checked, so `CONVERT(date, '2000-01-02', 102)` — a
/// style written `yyyy.mm.dd` — reads the hyphens without a word, while
/// `CONVERT(date, '2000-01-02', 101)` is error 241 because `2000` is no month, and
/// `CONVERT(date, '2000-01-02', 2)` is error 241 because a two-digit style has no four-digit
/// year. A one-digit year is a two-digit year
/// (`CONVERT(date, '01/02/9', 1)` is 2009-01-02, `… , 101)` is error 241).
fn date_under_style(
    parts: &[DatePart],
    comma: Option<usize>,
    order: Order,
    century: Century,
) -> Option<(i32, u8, u8)> {
    // A comma stands only beside a month name, which an all-numeric date has not.
    if comma.is_some() {
        return None;
    }
    let mut pieces = [(0_u32, 0_usize); 3];
    for (slot, part) in pieces.iter_mut().zip(parts) {
        let DatePart::Num { value, digits, .. } = part else {
            return None;
        };
        *slot = (*value, *digits);
    }
    let [year, month, day] = order.slots();
    let (year_value, year_digits) = pieces[year];
    let year = match century {
        Century::Four if year_digits == 4 => year_value as i32,
        Century::Four => return None,
        Century::Two => expand_year(year_value, year_digits.min(2)).filter(|_| year_digits <= 2)?,
    };
    let (month_value, month_digits) = pieces[month];
    let (day_value, day_digits) = pieces[day];
    Some((
        year,
        month_or_day(month_value, month_digits)?,
        month_or_day(day_value, day_digits)?,
    ))
}

/// Cuts a literal into its tokens, blanks that decorate a separator not counting.
///
/// A blank that sits beside one of [`GLUING_SEPARATORS`] is decoration, not a separation:
/// `'2000 - 01 - 02'` is one token, `'13:05:06 . 5'` is one token, and `'01 02 2000'` — no
/// separator in sight — stays three.
fn tokenise(body: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for piece in body.split(is_blank).filter(|p| !p.is_empty()) {
        match tokens.last_mut() {
            Some(last)
                if last.ends_with(GLUING_SEPARATORS) || piece.starts_with(GLUING_SEPARATORS) =>
            {
                last.push_str(piece);
            }
            _ => tokens.push(piece.to_owned()),
        }
    }
    tokens
}

/// Cuts, in every token that glues a date to a clock, the date off the clock.
///
/// A month name and the digits beside it are cut apart wherever they meet, and a clock is no
/// exception: `'2000 Jan13:05'`, `'2000Jan13:05'`, `'2 JANUARY13:05:06.5'`, `'Jan2 13:05'`
/// and `'2000 Jan1PM'` are read.
/// The cut is where the hour of the clock begins, and it only happens when the date half
/// ends on a letter, so `'13:05:06'` and `'1PM'` stay whole.
fn split_glued_clocks(tokens: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        match clock_start(&token) {
            Some(index) => {
                out.push(token[..index].to_owned());
                out.push(token[index..].to_owned());
            }
            None => out.push(token),
        }
    }
    out
}

/// The byte index at which the clock of a token begins, `None` when the token does not glue
/// a date to a clock.
///
/// The clock is the run of digits that runs up to the first `:`, or the one a glued meridiem
/// marker names an hour (`Jan1PM`). Splitting it off asks for both halves to exist and for
/// the date half to end on a letter — that is, on a month name, the only thing that glues
/// itself to a number here. `'2000-01-02T13:05'` therefore splits at the `T` and is refused
/// a moment later by [`push_date_parts`].
fn clock_start(token: &str) -> Option<usize> {
    let clock_end = match token.find(':') {
        Some(index) => index,
        // `Jan1PM`: an hour with no minutes, named a clock by the marker glued behind it.
        None => split_glued_meridiem(token)?.0.len(),
    };
    let head = token.get(..clock_end)?;
    let start = head.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let glued_to_a_name = token
        .get(..start)
        .is_some_and(|date| date.ends_with(is_word_letter));
    match start < clock_end && glued_to_a_name {
        true => Some(start),
        false => None,
    }
}

/// Builds a reading out of a civil date, a time of day and an offset, refusing a day that
/// does not exist.
fn local_of(year: i32, month: u8, day: u8, ticks: u64, offset: Option<i16>) -> Option<Local> {
    if !calendar::is_valid_civil(year, month, day) {
        return None;
    }
    Some(Local {
        days: calendar::days_from_civil(year, month, day),
        ticks,
        offset,
    })
}

/// The date and the time of day a target that carries **both** halves sees: the midnight
/// that ends a day ([`Local::ticks`] at `TICKS_PER_DAY`) is the next day at 00:00.
///
/// The eighth fractional digit of `'2000-01-02 23:59:59.99999996'` therefore lands on the
/// 3rd, but it has no day to land on for a `date` or a `time`, which is why the reading
/// does not apply it itself.
///
/// The carry stops at the edge of the calendar rather than falling off it:
/// `'9999-12-31T23:59:59.99999995'` and the longer fractions of that instant read as
/// `9999-12-31 23:59:59.9999999`.
fn carried_day_and_time(local: &Local) -> (i32, u64) {
    match local.ticks >= TICKS_PER_DAY {
        false => (local.days, local.ticks),
        true if local.days >= MAX_DAYS => (local.days, TICKS_PER_DAY - 1),
        true => (local.days.saturating_add(1), local.ticks - TICKS_PER_DAY),
    }
}

/// Reads the ISO 8601 grammar, `yyyy-m-d[ T hh:mi:s[.f…]][Z|±hh:mm]`, on the whole string.
///
/// This grammar is tried first, and it is the only one that reads a time-zone designator
/// behind a bare date: `'2000-01-02Z'` and `'2000-01-02+02:00'` are read, while the same
/// designator one blank further, `'2000-01-02 Z'` and `'2000-01-02 +02:00'`, is error 241,
/// and so is the same designator behind any other spelling of the date — `'20000102Z'`,
/// `'2000/01/02Z'`, `'01-02-2000Z'`, `'Jan 2 2000Z'`, `'2000 - 01 - 02Z'`.
///
/// Its spelling is strict, and each rule below is stated against its neighbours:
///
/// * the year is written on **four** digits, the month and the day on one or two
///   (`'2000-1-2T03:05:06'` is read, `'99-01-02T13:05:06'` and `'20000-01-02T13:05:06'` are
///   not), and the separator is the hyphen (`'2000/01/02T13:05:06'` is not read);
/// * the clock is introduced by a capital `T`, which a blank may precede but never follow
///   (`'2000-01-02 T13:05:06'` is read, `'2000-01-02T 13:05:06'` and
///   `'2000-01-02t13:05:06'` are not);
/// * the **hour** is the one piece written on its full width: `'2000-01-02T13:5:6'` is read
///   and `'2000-01-02T1:05:06'` is not, where the loose grammar reads `'2000-01-02 1:05:06'`
///   without blinking;
/// * the seconds are not optional (`'2000-01-02T13:05'` is error 241), no meridiem marker
///   may follow, and the fraction is written on one to nine digits;
/// * the designator is glued to what precedes it, spells UTC with a capital `Z` only, and
///   writes an offset on its full width **with no blank at all**
///   (`'2000-01-02T13:05:06+2:0'`, `'…+ 02:00'` and `'…+02 :00'` are error 241, where
///   `'2000-01-02 13:05:06+2:0'`, `'…+ 02:00'` and `'…+02 :00'` are read: the two grammars
///   pull in opposite directions here, see [`OffsetSpelling`]).
///
/// # What a `CONVERT` style needs from it
///
/// The second half of the answer says whether the literal carried anything **beyond** its
/// date — a `T` and a clock, a `Z`, or an offset.
///
/// That is what tells the two halves of the style rule apart: `'2000-01-02T13:05:06'`,
/// `'2000-01-02Z'` and `'2000-01-02+02:00'` are read under every style, while the bare
/// `'2000-01-02'` is judged by the style like any other all-numeric date and is error 241
/// under style 101 and 9807 under style 112.
fn parse_iso_8601_parts(s: &str) -> Option<(Local, bool)> {
    let mut rest = s;
    let year = take_digits(&mut rest, 4, 4)?;
    take_char(&mut rest, '-')?;
    let month = take_digits(&mut rest, 1, 2)?;
    take_char(&mut rest, '-')?;
    let day = take_digits(&mut rest, 1, 2)?;
    let beyond_the_date = !rest.is_empty();

    let mut ticks = 0;
    let clock = rest.trim_start_matches(is_blank);
    if let Some(after_t) = clock.strip_prefix('T') {
        rest = after_t;
        let hour = take_digits(&mut rest, 2, 2)?;
        take_char(&mut rest, ':')?;
        let minute = take_digits(&mut rest, 1, 2)?;
        take_char(&mut rest, ':')?;
        let second = take_digits(&mut rest, 1, 2)?;
        let fraction = match take_char(&mut rest, '.') {
            Some(()) => {
                let digits =
                    rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                let (written, tail) = rest.split_at(digits);
                rest = tail;
                parse_fraction(written)?
            }
            None => 0,
        };
        ticks = time_of(hour, minute, second, fraction)?;
    } else if !rest.is_empty() && clock.len() != rest.len() {
        // A blank is only ever the run-up to the `T`: `'2000-01-02 Z'` is error 241.
        return None;
    }

    let offset = match rest {
        "" => None,
        "Z" => Some(0),
        // The designator is written on its full width here, and carries no blank at all,
        // unlike the loose grammar's.
        _ => Some(parse_offset(rest, OffsetSpelling::Iso)?),
    };
    let local = local_of(year as i32, to_u8(month)?, to_u8(day)?, ticks, offset)?;
    Some((local, beyond_the_date))
}

/// Takes a run of `low..=high` ASCII digits off the front of `rest`, or answers `None`.
fn take_digits(rest: &mut &str, low: usize, high: usize) -> Option<u32> {
    let end = rest
        .char_indices()
        .take_while(|(index, c)| *index < high && c.is_ascii_digit())
        .count();
    let value = parse_u32(rest.get(..end)?, low, high)?;
    *rest = rest.get(end..)?;
    Some(value)
}

/// Takes `c` off the front of `rest`, or answers `None`.
fn take_char(rest: &mut &str, c: char) -> Option<()> {
    *rest = rest.strip_prefix(c)?;
    Some(())
}

/// Splits a trailing time-zone designator (`±hh:mm`, or the `Z` of UTC) off a literal of the
/// loose grammar, and answers whether one was found.
///
/// The designator is taken off here and judged in [`parse_loose`], which refuses it when the
/// literal turned out to carry no clock: that one rule accounts for `'2000-01-02 Z'`,
/// `'20000102 Z'`, `'Jan 2 2000 Z'`, `'2000 Z'`, `'2000-01-02 +02:00'`, `'2000-01-02 -05:30'`
/// and `'+02:00'`, all error 241, while `'13:05:06 Z'`, `'1:05 PM Z'` and
/// `'2000-01-02 1 PM+02:00'` are read.
///
/// The `Z` is only a designator when it is not glued to a letter: `'1:05 PM Z'` is read
/// where `'1:05PMZ'` and `'1PMZ'` are error 241. Only the capital `Z` names UTC; a
/// lower-case `z` is error 241.
///
/// The signed spelling is looked for at the last sign whose tail spells an offset, which is
/// why the hyphens of `'2000-01-02'` and of `'2-Jan-2000'` stay date separators: `'-02'` and
/// `'-Jan-2000'` are not offsets. What that tail may hold, blanks included, is
/// [`OffsetSpelling::Loose`]'s business.
fn split_offset(s: &str) -> (&str, Option<i16>, bool) {
    if let Some(head) = s.strip_suffix('Z') {
        let glued_to_a_letter = head.ends_with(is_word_letter);
        let body = head.trim_end_matches(is_blank);
        return match body.is_empty() || glued_to_a_letter {
            true => (s, None, false),
            false => (body, Some(0), true),
        };
    }
    for (index, c) in s.char_indices().rev() {
        if (c == '+' || c == '-')
            && let Some(minutes) = parse_offset(&s[index..], OffsetSpelling::Loose)
        {
            return (s[..index].trim_end_matches(is_blank), Some(minutes), true);
        }
    }
    (s, None, false)
}

/// How strictly a time-zone designator is spelled, the one rule that differs between the two
/// grammars in both directions at once.
#[derive(Clone, Copy)]
enum OffsetSpelling {
    /// The ISO 8601 spelling: `±hh:mm` on its full width, and **no blank anywhere**.
    /// `'2000-01-02T13:05:06+ 02:00'`, `'…+  02:00'`, `'…+<tab>02:00'` and
    /// `'2000-01-02+ 02:00'` are error 241.
    Iso,
    /// The loose spelling: one or two digits a side, and blanks allowed around the sign and
    /// around the colon — but never inside a run of digits. `'13:05:06+ 02:00'`,
    /// `'13:05:06+02 :00'`, `'13:05:06 + 02 : 00'` and `'13:05:06+02<tab>:00'` are read,
    /// `'13:05:06+0 2:00'` and `'13:05:06+02:0 0'` are error 241.
    Loose,
}

/// Reads `±hh:mm` into a count of minutes, `-840..=840`.
///
/// The two shorter spellings of ISO 8601 are refused with error 241: `±hhmm`, four digits
/// with no colon, and `±hh`. What else is admitted depends on the grammar that asks, and
/// the two pull in **opposite** directions: see [`OffsetSpelling`].
fn parse_offset(s: &str, spelling: OffsetSpelling) -> Option<i16> {
    // The sign is part of the designator: `'2000-01-02. 13:05'` and `'2000-01-02:13:05'` are
    // error 241, not a date carrying an offset of thirteen hours and five minutes.
    let negative = match s.as_bytes().first()? {
        b'-' => true,
        b'+' => false,
        _ => return None,
    };
    let (h, m) = s.get(1..)?.split_once(':')?;
    let (h, m, low) = match spelling {
        OffsetSpelling::Iso => (h, m, 2),
        OffsetSpelling::Loose => (h.trim_matches(is_blank), m.trim_matches(is_blank), 1),
    };
    let (hours, minutes) = (parse_u32(h, low, 2)?, parse_u32(m, low, 2)?);
    if hours > 14 || minutes > 59 {
        return None;
    }
    let total = (hours * 60 + minutes) as i16;
    if total > 840 {
        return None;
    }
    Some(if negative { -total } else { total })
}

/// `Some(true)` for `PM`, `Some(false)` for `AM`, in either case.
///
/// The dotted spellings are **not** markers: `'1 P.M.'` and `'1:05 P.M.'` are error 241
/// (`a_meridiem_marker_is_written_without_dots` in `tests/convert_datetime.rs`).
fn meridiem_of(token: &str) -> Option<bool> {
    match folded_lowercase(token).as_str() {
        "am" => Some(false),
        "pm" => Some(true),
        _ => None,
    }
}

/// Splits a meridiem marker glued to the end of a token: `1PM` is one o'clock in the
/// afternoon.
///
/// The marker has to follow something, so that the standalone `PM` — which [`meridiem_of`]
/// answers for — is not read as a clock of its own.
///
/// Only a space or a tabulation may stand between the clock and the marker, as everywhere
/// else in this module: `'13:05:06\nPM'`, `'…\rPM'`, `'…\x0cPM'` and `'…\x0bPM'` are error
/// 241, so the trimming here is [`is_blank`]'s and not Unicode's.
fn split_glued_meridiem(token: &str) -> Option<(&str, bool)> {
    let mut chars = token.char_indices().rev();
    let (_, last) = chars.next()?;
    let (cut, first) = chars.next()?;
    if cut == 0 {
        return None;
    }
    let letter = |c: char| fold_fullwidth(c).to_ascii_lowercase();
    let is_pm = match (letter(first), letter(last)) {
        ('a', 'm') => false,
        ('p', 'm') => true,
        _ => return None,
    };
    Some((token[..cut].trim_end_matches(is_blank), is_pm))
}

/// Whether a token of a literal is its clock rather than a piece of its date.
///
/// A clock either carries a `:` (`13:05`, `1:05PM`) or is an hour glued to a meridiem
/// marker, with no minutes (`1PM`). A bare number is *not* a clock: `'2000'` alone
/// is a year, not 8 p.m.
fn is_clock(token: &str) -> bool {
    token.contains(':')
        || split_glued_meridiem(token)
            .is_some_and(|(rest, _)| rest.chars().all(|c| c.is_ascii_digit()))
}

/// What [`push_date_parts`] saw in one token, beyond the parts it pushed.
struct TokenShape {
    /// The token spelled its pieces with one of [`DATE_SEPARATORS`].
    used_a_separator: bool,
    /// The token is `<number><sep><month name><sep><number>`, the one shape in which a month
    /// name and a separator meet.
    month_between_separators: bool,
}

/// Cuts a token of the date half (`01/02/2000`, `2-Jan-2000`, `2000Jan2`) into
/// [`DatePart`]s, `rank` being the position of the token among the blank-delimited ones. The
/// comma is not a separator here: [`parse_loose`] took it out before tokenising.
///
/// A token that carries no separator is cut at every change between digits and letters, so
/// a month name glued to a number is read: `'Jan2 2000'`, `'2Jan2000'`, `'2000JAN'` and
/// `'2 2000JANUARY'` are read.
///
/// `None` on a piece that is neither a number nor a month name, which is how `'not a date'`
/// is rejected, and on three spellings that are refused as well:
///
/// * **two different separators in one token**: `'01-02.2000'` is error 241;
/// * **a separator with nothing after it**: `'2000-01-02-'` is error 241;
/// * **a month name glued to a number beside a separator**: `'2-Jan2000'`, `'2Jan-2000'` and
///   `'2000-Jan2'` are error 241, where `'2-Jan-2000'` is read.
fn push_date_parts(token: &str, rank: usize, parts: &mut Vec<DatePart>) -> Option<TokenShape> {
    let mut separator: Option<char> = None;
    let mut pieces: Vec<&str> = Vec::new();
    let mut start = 0;
    for (index, c) in token.char_indices() {
        if DATE_SEPARATORS.contains(&c) {
            if *separator.get_or_insert(c) != c {
                return None;
            }
            pieces.push(&token[start..index]);
            start = index + c.len_utf8();
        }
    }
    pieces.push(&token[start..]);

    let mut month_between_separators = false;
    for (index, piece) in pieces.iter().enumerate() {
        if piece.is_empty() {
            return None;
        }
        match separator {
            // Beside a separator every piece is a number, save a month name written whole
            // between the two separators of a three-piece token.
            Some(_) => match month_of(piece) {
                Some(month) if index == 1 && pieces.len() == 3 => {
                    month_between_separators = true;
                    parts.push(DatePart::Month(month));
                }
                _ => parts.push(number_part(piece, rank)?),
            },
            // On its own a token is cut at every change between digits and letters.
            None => {
                for run in runs_of_digits_and_letters(piece) {
                    match run.starts_with(|c: char| c.is_ascii_digit()) {
                        true => parts.push(number_part(run, rank)?),
                        false => parts.push(DatePart::Month(month_of(run)?)),
                    }
                }
            }
        }
    }
    Some(TokenShape {
        used_a_separator: separator.is_some(),
        month_between_separators,
    })
}

/// The widest run of digits any date spelling reads.
///
/// One spelling alone goes that far, the day of `<number><sep><month name><sep><number>`:
/// `'2000-Jan-000000002'`, nine digits, is the 2nd of January 2000 on the three separators,
/// in both orders and under every month name, while `'2000-Jan-0000000002'`, ten digits, is
/// error 241. Every other piece is stopped earlier by its own arm, at one, two, four, six or
/// eight digits, so reading nine here opens nothing else: the same widths written at the
/// other places of a date without a month name are refused past two, the control of the
/// exception.
const WIDEST_NUMBER: usize = 9;

/// A run of digits read as a numbered part, remembering how wide it was written.
fn number_part(piece: &str, rank: usize) -> Option<DatePart> {
    Some(DatePart::Num {
        value: parse_u32(piece, 1, WIDEST_NUMBER)?,
        digits: piece.len(),
        token: rank,
    })
}

/// Cuts `piece` at every change between ASCII digits and the rest: `2Jan2000` is `2`, `Jan`,
/// `2000`. A piece of one kind comes out whole, so `20000102` stays one number.
fn runs_of_digits_and_letters(piece: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut kind = None;
    for (index, c) in piece.char_indices() {
        let digit = c.is_ascii_digit();
        if kind.is_some_and(|previous| previous != digit) {
            runs.push(&piece[start..index]);
            start = index;
        }
        kind = Some(digit);
    }
    runs.push(&piece[start..]);
    runs
}

/// What the shape of a literal tells [`parse_date`], beyond the pieces themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    /// Whether the literal carried a time of day, which one arm depends on: see
    /// [`month_and_year`].
    has_clock: bool,
    /// Whether the date is the single-token `<number><sep><month name><sep><number>`, the
    /// one spelling where a month name may meet a separator.
    ///
    /// That spelling writes its day on up to **nine** digits, where every other spelling
    /// stops at two: `'2000-Jan-002'`, `'2000/Jan/00002'`, `'000002-Jan-2000'` and
    /// `'2000-Jan-000000002'` are all the 2nd of January 2000, while
    /// `'2000-Jan-0000000002'` (one digit wider) and `'Jan 002 2000'` (the same date,
    /// blanks instead of hyphens) are error 241. See [`WIDEST_NUMBER`].
    ///
    /// It is also the one spelling that **degrades** on a year it cannot read instead of
    /// refusing the literal: see [`month_date_year`].
    month_between_separators: bool,
}

/// Reads the date half of a literal into a civil `(year, month, day)`, still unvalidated.
///
/// The order of the pieces follows `DATEFORMAT mdy` unless a four-digit number or a month
/// name fixes it: `2000-01-02` is ISO because the year comes first, `01/02/2000` is
/// month-day-year, and `2 Jan 2000` is unambiguous.
///
/// `comma` is the index in `parts` of the piece a comma stood before, if one did. A comma
/// is only ever legal just before a year that **ends** a date written with a month name:
/// `Jan 2, 2000`, `2 Jan, 2000` and `Jan, 2000` are read, while `2000 Jan,2`, `2000,Jan`,
/// `01/02,2000` and `Jan 2 2000,` are refused. Each arm below therefore says which slot,
/// if any, the comma may occupy.
///
/// `shape` carries the two facts about the literal that the arms below need but cannot see
/// in `parts`: see [`Shape`].
fn parse_date(parts: &[DatePart], comma: Option<usize>, shape: Shape) -> Option<(i32, u8, u8)> {
    let (ymd, comma_slot) = match parts {
        // `20000102` and `000102`: the compact ISO forms.
        [
            DatePart::Num {
                value, digits: 8, ..
            },
        ] => (
            (
                (value / 10_000) as i32,
                ((value / 100) % 100) as u8,
                (value % 100) as u8,
            ),
            None,
        ),
        [
            DatePart::Num {
                value, digits: 6, ..
            },
        ] => (
            (
                expand_year(value / 10_000, 2)?,
                ((value / 100) % 100) as u8,
                (value % 100) as u8,
            ),
            None,
        ),
        // `2000` on its own is a year: the 1st of January of it. Two and three digits are
        // not (`'20'` and `'200'` are error 241).
        [
            DatePart::Num {
                value, digits: 4, ..
            },
        ] => ((*value as i32, 1, 1), None),
        // `Jan 2000` and `2000 Jan`: a month and a year, the 1st of the month. Only the
        // first spelling ends on its year, so only it may carry a comma — and only when the
        // year is written on four digits, `'Jan, 2 13:05'` being error 241 where
        // `'Jan, 2000 13:05'` is read.
        [DatePart::Month(m), DatePart::Num { value, digits, .. }] => (
            (month_and_year(*value, *digits, shape.has_clock)?, *m, 1),
            (*digits == 4).then_some(1),
        ),
        [DatePart::Num { value, digits, .. }, DatePart::Month(m)] => (
            (month_and_year(*value, *digits, shape.has_clock)?, *m, 1),
            None,
        ),
        [
            DatePart::Num {
                value: a,
                digits: da,
                token: ta,
            },
            DatePart::Num {
                value: b,
                digits: db,
                token: tb,
            },
            DatePart::Num {
                value: c,
                digits: dc,
                token: tc,
            },
        ] => {
            // An all-numeric date is written in one token: `'01 02 2000'`, three numbers
            // spaced apart, is error 241.
            if ta != tb || tb != tc {
                return None;
            }
            let ymd = if *da == 4 {
                // `2000-01-02`: year first, month, day.
                (*a as i32, month_or_day(*b, *db)?, month_or_day(*c, *dc)?)
            } else {
                // `DATEFORMAT mdy`: month, day, year — the year being the last piece.
                (
                    expand_year(*c, *dc)?,
                    month_or_day(*a, *da)?,
                    month_or_day(*b, *db)?,
                )
            };
            (ymd, None)
        }
        // A month name and two numbers, in any order: `2 Jan 2000`, `Jan 2, 2000`.
        [_, _, _] => {
            let month = parts.iter().find_map(|p| match p {
                DatePart::Month(m) => Some(*m),
                DatePart::Num { .. } => None,
            })?;
            let numbers: Vec<(u32, usize, usize)> = parts
                .iter()
                .enumerate()
                .filter_map(|(index, p)| match p {
                    DatePart::Num { value, digits, .. } => Some((*value, *digits, index)),
                    DatePart::Month(_) => None,
                })
                .collect();
            let [
                (first, first_digits, first_index),
                (second, second_digits, second_index),
            ] = numbers[..]
            else {
                return None;
            };
            // A four-digit number is the year wherever it stands — `0000` excepted, which
            // is no year and leaves the roles to `DATEFORMAT mdy`: day, month, year, the
            // year being the last of the two ([`month_date_year`]).
            // The day of a glued spelling is written on up to nine digits -- ten is error
            // 241, and [`WIDEST_NUMBER`] has already turned it away; everywhere else the
            // day stops at two digits.
            let day = |value: u32, digits: usize| match shape.month_between_separators {
                true => to_u8(value),
                false => month_or_day(value, digits),
            };
            let year_is_first = first_digits == 4 && first != 0;
            let (year_value, year_digits, year_index, day_value, day_digits) = match year_is_first {
                true => (first, first_digits, first_index, second, second_digits),
                false => (second, second_digits, second_index, first, first_digits),
            };
            let ymd = match month_date_year(year_value, year_digits) {
                Some(year) => (year, month, day(day_value, day_digits)?),
                // The one spelling that **degrades** rather than refusing a year it cannot
                // read: the 1st of its month in 1900, the day left unread — so `to_u8` and
                // the calendar check are both skipped here. See [`month_date_year`].
                None if shape.month_between_separators => (1900, month, 1),
                None => return None,
            };
            (ymd, (year_index + 1 == parts.len()).then_some(year_index))
        }
        _ => return None,
    };
    match comma {
        None => Some(ymd),
        Some(index) if comma_slot == Some(index) => Some(ymd),
        Some(_) => None,
    }
}

/// The year a date written with a month name and **two** numbers reads out of the number
/// that holds its year slot, `None` when that number is no year at all.
///
/// One and two digits pivot like [`expand_year`], and four digits are the year itself —
/// `0000` excepted, which is no year: a literal whose first number is `0000` therefore
/// falls back to day-month-year, and `'0000-Feb-2'` is error 241 for its day of zero, not
/// a year of zero. Three digits, and five to nine, are no year either; ten and beyond cannot
/// arrive here, [`WIDEST_NUMBER`] having refused the piece already.
///
/// The `0000` exception is **told apart** from its concurrent ("a four-digit zero is the
/// year, and an unreadable one") in the separator spelling below alone. The
/// blank-separated forms `'0000 Feb 2'`, `'0000 Feb 002'`, `'0000 Feb 0002'`,
/// `'Feb 0000 2'` and `'0000 Feb 2 13:05'` are error 241 under either reading, a year of
/// zero and a day of zero being equally impossible; they separate nothing.
///
/// # The one spelling that degrades instead of refusing
///
/// `<number><separator><month name><separator><number>` answers a **date** where its
/// neighbours answer error 241: a year it cannot read there gives the 1st of that month in
/// 1900, and the day is left unread. `CAST(N'3-Feb-007' AS date)`,
/// `CAST(N'3-Feb-0000' AS date)`, `CAST(N'3.Feb.02000' AS date)` and
/// `CAST(N'32-Feb-007' AS date)` are each `1900-02-01`, where `CAST(N'3 Feb 007' AS date)`,
/// the same pieces with blanks instead of hyphens, is error 241.
///
/// The vectors that place the rule:
///
/// * **the year slot decides, not the width of a piece**: `'2000-Feb-002'` is the 2nd of
///   February 2000, its three-digit piece being the day, where `'2-Feb-002'` and
///   `'007-Feb-002'` are `1900-02-01`;
/// * **`0000` is no year, and the third slot is where that shows**: `'2-Feb-0000'` is
///   `1900-02-01` where `'0000-Feb-2'`, `'0000-Feb-0002'` and `'0002-Feb-0000'` are error
///   241. That pair is the asymmetry of the degradation;
/// * **the day stops being judged**: on the widths 1 to 9 and the values `0` to
///   `999999999`, `'0-Feb-007'`, `'32-Feb-007'`, `'256-Feb-007'` and
///   `'999999999-Feb-007'` are each `1900-02-01`, where the same days beside a year that
///   reads — `'32-Feb-2000'`, `'256-Feb-2000'`, `'0-Feb-2000'` — are error 241;
/// * **a piece of ten digits refuses before any of this**: `'2-Feb-1234567890'` and
///   `'1234567890-Feb-2'` are error 241, as `'2-Feb-123456789'` is `1900-02-01`;
/// * **the month name must stand between the two separators**: `'Feb-2-007'`, `'Feb-007-2'`,
///   `'2-007-Feb'` and `'007-2-Feb'` are error 241 on each of the three separators, and so
///   are the comma spellings `'2,Feb,007'` and `'2-Feb,007'` and the blank-separated
///   `'Feb 2 007'`;
/// * **the date half alone degrades**: `'2-Feb-007 13:05:06'` is `1900-02-01 13:05:06` as a
///   `datetime2(7)` and `13:05:06.0000000` as a `time(7)`. What a literal may carry behind
///   its date is read as it would be beside a year that works: of the nineteen tails
///   crossed, `'2-Feb-007 13:05:06Z'` reads and `'2-Feb-007 Z'` is error 241, just as
///   `'2-Feb-2000 13:05:06Z'` and `'2-Feb-2000 Z'` do.
fn month_date_year(value: u32, digits: usize) -> Option<i32> {
    match digits {
        4 => (value != 0).then_some(value as i32),
        _ => expand_year(value, digits),
    }
}

/// The year of a date written as a month name and one number, `None` when the pair is not a
/// date at all.
///
/// Four digits are a year outright: `'Jan 2000'` and `'2000 Jan'` are the 1st of January
/// 2000. One or two digits are a year **when the literal also carries a clock, and not
/// otherwise**:
/// `'Jan 2'`, `'2 Jan'`, `'Jan2'` and `'Jan 2 Z'` are error 241, while `'Jan 2 13:05'`,
/// `'2 Jan 13:05'`, `'Jan2 13:05'` and `'Jan 2 1 PM'` are the 1st of January 2002,
/// `'Jan 49 13:05'` is 2049 and `'Jan 50 13:05'` is 1950. Any other width is not a year:
/// `'Jan 100 13:05'` and `'Jan 000102 13:05'` are error 241.
fn month_and_year(value: u32, digits: usize, has_clock: bool) -> Option<i32> {
    match digits {
        4 => Some(value as i32),
        1 | 2 if has_clock => expand_year(value, digits),
        _ => None,
    }
}

/// Expands the year of a date, written on one, two or four digits.
///
/// One or two digits pivot at the default *two digit year cutoff* of SQL Server: below 50 the
/// year is in the 21st century, from 50 on in the 20th (`'01/02/9'` is 2009 and `'01/02/50'`
/// is 1950). Any other width is **not** a year: `'01/02/999'` is error 241.
fn expand_year(value: u32, digits: usize) -> Option<i32> {
    match digits {
        1 | 2 if value <= TWO_DIGIT_YEAR_CUTOFF => Some((2000 + value) as i32),
        1 | 2 => Some((1900 + value) as i32),
        4 => Some(value as i32),
        _ => None,
    }
}

/// Narrows a month or a day written beside the other pieces of a date, refusing anything
/// wider than **two digits**.
///
/// Only the year is allowed a third width: `'2000-01-002'`, `'2000-001-02'`,
/// `'001/02/2000'`, `'01.002.2000'`, `'Jan 002 2000'` and `'002 Jan 2000'` are error 241,
/// as are their four- and five-digit neighbours, while the same dates on one and on two
/// digits are read. The value is checked too, since a `u8` stops at 255 and a month at 12.
fn month_or_day(value: u32, digits: usize) -> Option<u8> {
    match digits {
        1 | 2 => to_u8(value),
        _ => None,
    }
}

/// Narrows a month or a day to a `u8`, rejecting anything that could not be one.
fn to_u8(value: u32) -> Option<u8> {
    u8::try_from(value).ok()
}

/// Reads `hh:mi[:ss[.fffffff]]`, or `hh` alone when a meridiem marker follows it, into
/// 100-nanosecond ticks since midnight.
///
/// Each field is written on one or two digits: `'13:5'` and `'13:05:6'` are read, and a
/// clock may stop after the minutes.
///
/// `meridiem` is the marker written as a separate token; one glued to the clock (`1:05PM`,
/// `1PM`) is taken off here.
///
/// # The hours a marker allows
///
/// * `'12:00AM'` is midnight, `'12 PM'` is noon and `'12 AM'` is midnight again;
/// * `PM` leaves an hour that is already past noon alone: `'13:05PM'` is `13:05` and
///   `'23:59PM'` is `23:59`;
/// * `AM` refuses those same hours and `PM` refuses midnight: `'13:05AM'` and `'0:05PM'`
///   are error 241 as a `datetime2`;
/// * `'0 AM'` is midnight.
///
/// Read as a **`datetime`**, `'13:05AM'` and `'0:05PM'` are error **242** and not 241:
/// that is the legacy grammar's answer ([`hour_under_meridiem`]), this parser serving the
/// four modern targets.
fn parse_time(text: &str, meridiem: Option<bool>) -> Option<u64> {
    let mut clock = text;
    let mut pm = meridiem;
    if let Some((rest, is_pm)) = split_glued_meridiem(text) {
        if pm.is_some() {
            return None;
        }
        clock = rest;
        pm = Some(is_pm);
    }

    let fields: Vec<&str> = clock.split(':').collect();
    let hour = parse_u32(fields.first()?, 1, 2)?;
    let minute = match fields.get(1) {
        Some(field) => parse_u32(field, 1, 2)?,
        // An hour with no minutes is a clock only because a marker names it one.
        None if pm.is_some() => 0,
        None => return None,
    };
    let (second, fraction) = match fields.get(2) {
        Some(field) => match field.split_once('.') {
            Some((whole, digits)) => (parse_u32(whole, 1, 2)?, parse_fraction(digits)?),
            None => (parse_u32(field, 1, 2)?, 0),
        },
        None => (0, 0),
    };
    // A fourth field is the ODBC spelling of the milliseconds: `'13:05:06:7'` is seven
    // milliseconds and `'13:05:06:700'` is seven tenths of a second. It is written on one to
    // three digits, carries no fraction of its own, and is the last (`'13:05:06:1234'`,
    // `'13:05:06:07.5'` and `'13:05:06:07:08'` are error 241).
    let milliseconds = match fields.get(3) {
        Some(field) if fraction == 0 => u64::from(parse_u32(field, 1, 3)?) * TICKS_PER_MILLISECOND,
        Some(_) => return None,
        None => 0,
    };
    if fields.len() > 4 || minute > 59 || second > 59 {
        return None;
    }
    let hour = match pm {
        Some(true) => match hour {
            // `'0:05PM'` is refused, `'12:00PM'` is noon, `'13:05PM'` stays at 13.
            0 => return None,
            12 => 12,
            h @ 1..=11 => h + 12,
            h @ 13..=23 => h,
            _ => return None,
        },
        Some(false) => match hour {
            // `'12:00AM'` is midnight, `'0 AM'` too, `'13:05AM'` is refused.
            12 => 0,
            h @ 0..=11 => h,
            _ => return None,
        },
        None if hour > 23 => return None,
        None => hour,
    };
    Some(
        (u64::from(hour) * 3600 + u64::from(minute) * 60 + u64::from(second)) * TICKS_PER_SECOND
            + fraction
            + milliseconds,
    )
}

/// Assembles a time of day out of its pieces, the hour being on the 24-hour clock.
fn time_of(hour: u32, minute: u32, second: u32, fraction: u64) -> Option<u64> {
    if minute > 59 || second > 59 || hour > 23 {
        return None;
    }
    Some(
        (u64::from(hour) * 3600 + u64::from(minute) * 60 + u64::from(second)) * TICKS_PER_SECOND
            + fraction,
    )
}

/// Reads the digits after the decimal point of a second into 100-nanosecond ticks.
///
/// Seven digits is the finest a `time(7)` holds; a longer fraction is **rounded** on its
/// eighth digit, halves upwards — `.12345675` reads as `.1234568` and `.12345674` as
/// `.1234567`. The result may therefore be a whole second, which [`parse_string`] carries
/// into the next day.
///
/// A fraction longer than [`MAX_FRACTION_DIGITS_WRITTEN`] is not a fraction at all:
/// `'…13:05:06.123456789'` is read and `'…13:05:06.1234567890'` is error 241.
fn parse_fraction(digits: &str) -> Option<u64> {
    if digits.is_empty()
        || digits.len() > MAX_FRACTION_DIGITS_WRITTEN
        || !digits.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let kept = usize::from(MAX_FRACTION_DIGITS);
    let digit_at = |index: usize| -> u64 {
        digits
            .as_bytes()
            .get(index)
            .map_or(0, |b| u64::from(*b - b'0'))
    };
    let mut ticks = 0_u64;
    for index in 0..kept {
        ticks = ticks * 10 + digit_at(index);
    }
    // Only the eighth digit decides: rounding halves upwards, the digits after it can never
    // turn a `4` into a `5`.
    if digit_at(kept) >= 5 {
        ticks += 1;
    }
    Some(ticks)
}

/// Reads a run of `low..=high` ASCII digits into a number, rejecting anything else.
fn parse_u32(s: &str, low: usize, high: usize) -> Option<u32> {
    if !(low..=high).contains(&s.len()) || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// The number of an English month written in full or with its usual abbreviation.
///
/// The twelve full names and the twelve three-letter abbreviations are read.
fn month_of(name: &str) -> Option<u8> {
    let lower = folded_lowercase(name);
    let months = [
        ("january", "jan"),
        ("february", "feb"),
        ("march", "mar"),
        ("april", "apr"),
        ("may", "may"),
        ("june", "jun"),
        ("july", "jul"),
        ("august", "aug"),
        ("september", "sep"),
        ("october", "oct"),
        ("november", "nov"),
        ("december", "dec"),
    ];
    for (index, (full, short)) in months.iter().enumerate() {
        if lower == *full || lower == *short {
            return Some((index + 1) as u8);
        }
    }
    // The full name and the three-letter abbreviation, nothing in between: `Sept` is refused,
    // `SELECT CAST('Sept 2 2024' AS date)` being error 241.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Len;
    use crate::calendar::days_from_civil;

    fn ti(ty: SqlType) -> TypeInfo {
        TypeInfo::new(ty, true)
    }

    /// The bounds are written as literals for clarity; the calendar is what defines them.
    #[test]
    fn range_constants_match_the_calendar() {
        assert_eq!(MAX_DAYS, days_from_civil(9999, 12, 31));
        assert_eq!(DATETIME_MIN_DAYS, days_from_civil(1753, 1, 1));
        assert_eq!(SMALLDATETIME_MAX_DAYS, days_from_civil(2079, 6, 6));
        // A `smalldatetime` stores its days in sixteen bits.
        assert_eq!(SMALLDATETIME_MAX_DAYS - DAYS_1900, 65_535);
    }

    /// The style that *is* the default of the source type goes straight to
    /// `default_display`, and no other one does.
    ///
    /// Style 0 is **not** the default of a `date`: it is the `datetime` form, shorn of the
    /// clock a `date` has not.
    #[test]
    fn default_styles_render_without_a_style_table() {
        let date = Value::Date(Date { days: 730_120 });
        let date_type = ti(SqlType::Date);
        let text = ti(SqlType::VarChar(Len::Fixed(40)));
        assert_eq!(
            datetime_to_character(&date, &date_type, &text, None),
            Ok("2000-01-02".to_owned())
        );
        assert_eq!(
            datetime_to_character(&date, &date_type, &text, Some(121)),
            Ok("2000-01-02".to_owned())
        );
        assert_eq!(
            datetime_to_character(&date, &date_type, &text, Some(0)),
            Ok("Jan  2 2000".to_owned())
        );

        let dt = Value::DateTime(DateTime {
            days: 730_120 - DAYS_1900,
            ticks_300th: 0,
        });
        let dt_type = ti(SqlType::DateTime);
        assert_eq!(
            datetime_to_character(&dt, &dt_type, &text, Some(0)),
            Ok("Jan  2 2000 12:00AM".to_owned())
        );
        assert_eq!(
            datetime_to_character(&dt, &dt_type, &text, Some(121)),
            Ok("2000-01-02 00:00:00.000".to_owned())
        );
    }

    /// The 8114 of a style a type cannot fill names the target in its `var` form: `nvarchar`
    /// for an `nchar(60)` and an `nvarchar(max)`, `varchar` for a `char(60)` and a
    /// `varchar(max)`: a `date` under 8, 24, 108 and a `time` under 1, 6, 23, 101, 112,
    /// 115 on the six targets.
    #[test]
    fn a_style_a_type_cannot_fill_names_the_target() {
        let date = Value::Date(Date { days: 730_120 });
        let time = Value::Time(Time {
            ticks_100ns: 13 * 36_000_000_000,
        });
        let targets = [
            (SqlType::VarChar(Len::Fixed(60)), "varchar"),
            (SqlType::Char(Len::Fixed(60)), "varchar"),
            (SqlType::VarChar(Len::Max), "varchar"),
            (SqlType::NVarChar(Len::Fixed(60)), "nvarchar"),
            (SqlType::NChar(Len::Fixed(60)), "nvarchar"),
            (SqlType::NVarChar(Len::Max), "nvarchar"),
        ];
        for (target, named) in targets {
            for style in [8, 24, 108] {
                let err =
                    datetime_to_character(&date, &ti(SqlType::Date), &ti(target), Some(style))
                        .expect_err("a clock-only style on a date");
                assert_eq!(err.number, 8114, "{target:?} under {style}");
                assert_eq!(
                    err.message,
                    format!("Data type date could not be converted to {named}."),
                    "{target:?} under {style}"
                );
            }
            for style in [1, 6, 23, 101, 112, 115] {
                let err =
                    datetime_to_character(&time, &ti(SqlType::Time(7)), &ti(target), Some(style))
                        .expect_err("a date style on a time");
                assert_eq!(err.number, 8114, "{target:?} under {style}");
                assert_eq!(
                    err.message,
                    format!("Data type time could not be converted to {named}."),
                    "{target:?} under {style}"
                );
            }
        }
    }

    /// Every string here reads as the 2nd of January 2000. This test is the shortest way
    /// in; `tests/convert_datetime.rs` covers many more forms.
    #[test]
    fn parse_string_reads_the_second_of_january_many_ways() {
        let day = days_from_civil(2000, 1, 2);
        for text in [
            "2000-01-02",
            "2000/01/02",
            "2000-1-2",
            "20000102",
            "000102",
            "01/02/2000",
            "01-02-2000",
            "01.02.2000",
            "1/2/2000",
            "2 Jan 2000",
            "Jan 2, 2000",
            "Jan 2,2000",
            "January 2, 2000",
            "2000 Jan 2",
            "  2000-01-02  ",
        ] {
            let parsed = parse_string(text);
            assert_eq!(
                parsed,
                Some(Local {
                    days: day,
                    ticks: 0,
                    offset: None
                }),
                "{text}"
            );
        }
    }

    #[test]
    fn parse_string_reads_clocks_and_offsets() {
        let day = days_from_civil(2000, 1, 2);
        let at = |h: u64, m: u64, s: u64| (h * 3600 + m * 60 + s) * TICKS_PER_SECOND;

        assert_eq!(
            parse_string("2000-01-02T13:05:06.123"),
            Some(Local {
                days: day,
                ticks: at(13, 5, 6) + 1_230_000,
                offset: None
            })
        );
        assert_eq!(
            parse_string("2000-01-02 1:05 PM"),
            Some(Local {
                days: day,
                ticks: at(13, 5, 0),
                offset: None
            })
        );
        assert_eq!(
            parse_string("1:05PM"),
            Some(Local {
                days: DAYS_1900,
                ticks: at(13, 5, 0),
                offset: None
            })
        );
        assert_eq!(
            parse_string("2000-01-02 12:00AM"),
            Some(Local {
                days: day,
                ticks: 0,
                offset: None
            })
        );
        assert_eq!(
            parse_string("2000-01-02 13:05:06 +02:00"),
            Some(Local {
                days: day,
                ticks: at(13, 5, 6),
                offset: Some(120)
            })
        );
        // `-05:30`, glued to the clock or spaced from it, and the `Z` of UTC likewise.
        for (text, minutes) in [
            ("2000-01-02 13:05:06 -05:30", -330),
            ("2000-01-02 13:05:06-05:30", -330),
            ("2000-01-02 13:05:06 +2:00", 120),
            ("2000-01-02 13:05:06 +02:0", 120),
        ] {
            assert_eq!(
                parse_string(text),
                Some(Local {
                    days: day,
                    ticks: at(13, 5, 6),
                    offset: Some(minutes)
                }),
                "{text}"
            );
        }
        for text in ["2000-01-02 13:05:06Z", "2000-01-02 13:05:06 Z"] {
            assert_eq!(
                parse_string(text),
                Some(Local {
                    days: day,
                    ticks: at(13, 5, 6),
                    offset: Some(0)
                }),
                "{text}"
            );
        }
        // A clock with no seconds, and one with no date.
        assert_eq!(
            parse_string("2000-01-02 13:05"),
            Some(Local {
                days: day,
                ticks: at(13, 5, 0),
                offset: None
            })
        );
        assert_eq!(
            parse_string("13:05Z"),
            Some(Local {
                days: DAYS_1900,
                ticks: at(13, 5, 0),
                offset: Some(0)
            })
        );
        // `Jan 2000` has no day at all: the 1st.
        assert_eq!(
            parse_string("Jan 2000"),
            Some(Local {
                days: days_from_civil(2000, 1, 1),
                ticks: 0,
                offset: None
            })
        );
    }

    /// An hour with no minutes is a clock as soon as a marker follows it, and `PM` leaves
    /// an afternoon hour alone.
    #[test]
    fn parse_string_reads_an_hour_with_a_meridiem() {
        let at = |h: u64, m: u64, s: u64| (h * 3600 + m * 60 + s) * TICKS_PER_SECOND;
        let noon_ish = days_from_civil(2024, 1, 2);

        for text in ["1PM", "1 pm", "13 PM"] {
            assert_eq!(
                parse_string(text),
                Some(Local {
                    days: DAYS_1900,
                    ticks: at(13, 0, 0),
                    offset: None
                }),
                "{text}"
            );
        }
        assert_eq!(
            parse_string("0 AM"),
            Some(Local {
                days: DAYS_1900,
                ticks: 0,
                offset: None
            })
        );
        for text in ["2024-01-02 1PM", "Jan 2 2024 1 PM"] {
            assert_eq!(
                parse_string(text),
                Some(Local {
                    days: noon_ish,
                    ticks: at(13, 0, 0),
                    offset: None
                }),
                "{text}"
            );
        }
        assert_eq!(
            parse_string("13:05PM"),
            Some(Local {
                days: DAYS_1900,
                ticks: at(13, 5, 0),
                offset: None
            })
        );
        assert_eq!(
            parse_string("23:59PM"),
            Some(Local {
                days: DAYS_1900,
                ticks: at(23, 59, 0),
                offset: None
            })
        );
        assert_eq!(
            parse_string("2024-01-02 13:05:06 PM"),
            Some(Local {
                days: noon_ish,
                ticks: at(13, 5, 6),
                offset: None
            })
        );
    }

    #[test]
    fn parse_string_rejects_what_is_not_a_date() {
        for text in [
            "not a date",
            "2024-02-30",
            "2000-13-01",
            "2000-01-02 25:00",
            "2000-01-02 13:60",
            "13:05:06 PM AM",
            "PM",
            "1/2/3/4",
            "Jan Feb 2000",
            "2000-01-02 13:05:06.",
            // `AM` refuses an afternoon hour, `PM` refuses midnight.
            "13:05AM",
            "0:05PM",
            // `Sept` is not a month name.
            "Sept 2 2024",
            // The clock comes after the date, not before.
            "2 PM 2024-01-02",
            "13:05:06 2024-01-02",
            "2:05 PM 2024-01-02",
            // A meridiem marker is spelled without dots.
            "1 P.M.",
            "1:05 P.M.",
            // The offset is `±hh:mm` or `Z`, nothing shorter and nothing in lower case.
            "2000-01-02 13:05:06-0530",
            "2000-01-02 13:05:06 +02",
            "2000-01-02 13:05:06z",
            // The ISO separator is a capital `T`.
            "2000-01-02t13:05:06",
            // One date, one separator, and nothing after the last one.
            "01-02.2000",
            "2000-01-02-",
            // Three numbers spaced apart are not a date.
            "01 02 2000",
            // A year is written on one, two or four digits; a lone number is a year on four
            // digits, not on two or three (`'2000'` is read, `'200'` and `'20'` are not).
            "01/02/999",
            "200",
            "20",
            // The comma stands just before a year that ends a date written with a month
            // name, and not elsewhere.
            "2000,Jan",
            "Jan, 2 2000",
            "Jan 2 2000,",
            "01/02,2000",
            "Jan 2,,2000",
            // A month name is not spelled with a separator.
            "Jan-2-2000",
            "Jan/2000",
            "01/02 Jan",
            // Behind an ISO `T`, the spelling is strict.
            "2000-01-02T13:05",
            "2000-1-2T1:2:3",
            "2000-01-02T13:05:06PM",
            "2000-01-02T13:05:06 Z",
            // The `Z` needs a literal in front of it.
            "Z",
        ] {
            assert_eq!(parse_string(text), None, "{text}");
        }
    }

    /// Forms that are read and that no other unit test covers: a lone four-digit year, the
    /// one place a comma may stand, and a `Z` behind a date with no clock.
    #[test]
    fn parse_string_reads_a_lone_year_a_comma_and_a_bare_z() {
        let january_the_second = days_from_civil(2000, 1, 2);
        for (text, days, offset) in [
            ("2000", days_from_civil(2000, 1, 1), None),
            ("Jan 2, 2000", january_the_second, None),
            ("Jan 2,2000", january_the_second, None),
            ("Jan 2 , 2000", january_the_second, None),
            ("2 Jan, 2000", january_the_second, None),
            ("Jan, 2000", days_from_civil(2000, 1, 1), None),
            ("2000-01-02Z", january_the_second, Some(0)),
        ] {
            assert_eq!(
                parse_string(text),
                Some(Local {
                    days,
                    ticks: 0,
                    offset
                }),
                "{text}"
            );
        }
    }

    /// The two hours a 12-hour clock numbers 12, and a year of one or two digits beside a
    /// month name.
    #[test]
    fn parse_string_reads_noon_midnight_and_short_years() {
        let at = |h: u64, m: u64| (h * 3600 + m * 60) * TICKS_PER_SECOND;
        for (text, ticks) in [
            ("12 AM", 0),
            ("12 PM", at(12, 0)),
            ("12:30 PM", at(12, 30)),
            ("12:30 AM", at(0, 30)),
        ] {
            assert_eq!(
                parse_string(text),
                Some(Local {
                    days: DAYS_1900,
                    ticks,
                    offset: None
                }),
                "{text}"
            );
        }
        assert_eq!(
            parse_string("Jan 2 9").map(|l| l.days),
            Some(days_from_civil(2009, 1, 2))
        );
        assert_eq!(
            parse_string("Jan 2 49").map(|l| l.days),
            Some(days_from_civil(2049, 1, 2))
        );
    }

    /// An eighth fractional digit rounds the seventh, halves upwards, and the rounding may
    /// reach the next day.
    #[test]
    fn parse_string_rounds_a_fraction_longer_than_seven_digits() {
        let ticks_of = |text: &str| parse_string(text).map(|l| l.ticks);
        let base = (13 * 3600 + 5 * 60 + 6) * TICKS_PER_SECOND;
        assert_eq!(
            ticks_of("2000-01-02 13:05:06.12345678"),
            Some(base + 1_234_568)
        );
        assert_eq!(
            ticks_of("2000-01-02 13:05:06.12345675"),
            Some(base + 1_234_568)
        );
        assert_eq!(
            ticks_of("2000-01-02 13:05:06.12345674"),
            Some(base + 1_234_567)
        );
        assert_eq!(
            ticks_of("2000-01-02 13:05:06.123456789"),
            Some(base + 1_234_568)
        );
        // The reading stops on the midnight that **ends** the 2nd: the carry belongs to
        // the target, a `date` keeping the 2nd where a `datetime2` moves to the 3rd.
        assert_eq!(
            parse_string("2000-01-02 23:59:59.99999996"),
            Some(Local {
                days: days_from_civil(2000, 1, 2),
                ticks: TICKS_PER_DAY,
                offset: None
            })
        );
        assert_eq!(
            carried_day_and_time(&parse_string("2000-01-02 23:59:59.99999996").unwrap()),
            (days_from_civil(2000, 1, 3), 0)
        );
    }

    /// `CAST(<text> AS <to>)` without a style.
    fn cast_text(s: &str, to: SqlType) -> Result<String, u32> {
        let from = ti(SqlType::NVarChar(Len::Fixed(64)));
        let value = Value::String(crate::SqlString { text: s.to_owned() });
        match to_datetime(&value, &from, &ti(to), None) {
            Ok(v) => Ok(default_display(&v, &ti(to))),
            Err(e) => Err(e.number),
        }
    }

    /// Fullwidth forms and their neighbours, in the two grammars: a fullwidth Latin letter
    /// folds to its ASCII letter inside a month name and a meridiem marker, the `ﬆ`
    /// ligature spells `st`, and the rest of the literal folds nothing. This test names
    /// the frontier.
    #[test]
    fn fullwidth_letters_fold_in_a_month_name_and_a_marker() {
        let modern = [SqlType::Date, SqlType::DateTime2(7)];
        let legacy = [SqlType::DateTime, SqlType::SmallDateTime];
        let second = |ty: &SqlType| match ty {
            SqlType::Date => "2000-01-02".to_owned(),
            SqlType::DateTime2(_) => "2000-01-02 00:00:00.0000000".to_owned(),
            _ => "Jan  2 2000 12:00AM".to_owned(),
        };
        // The four basic forms, read in the six targets.
        for ty in modern.iter().chain(&legacy) {
            for text in ["Ｊａｎ 2 2000", "Jaｎ 2 2000", "2-Ｊａｎ-2000"] {
                assert_eq!(cast_text(text, *ty), Ok(second(ty)), "{text:?} as {ty:?}");
            }
        }
        assert_eq!(
            cast_text("1 PＭ", SqlType::DateTime2(7)),
            Ok("1900-01-01 13:00:00.0000000".to_owned())
        );
        assert_eq!(
            cast_text("1 PＭ", SqlType::DateTime),
            Ok("Jan  1 1900  1:00PM".to_owned())
        );
        // The fold reaches the letters of a name and of a marker, in either case, and the
        // `ﬆ` of `August`; a `T` between fullwidth letters is not the `T` of a clock.
        for (text, modern_date, legacy_date) in [
            ("ＪＡＮＵＡＲＹ 2 2000", "2000-01-02", "Jan  2 2000 12:00AM"),
            ("ｊａｎ 2 2000", "2000-01-02", "Jan  2 2000 12:00AM"),
            ("Auguﬆ 2 2000", "2000-08-02", "Aug  2 2000 12:00AM"),
            ("Ａｕｇｕﬆ 2 2000", "2000-08-02", "Aug  2 2000 12:00AM"),
            ("ＯＣTＯＢＥＲ 2 2000", "2000-10-02", "Oct  2 2000 12:00AM"),
            ("2000 Ｊａｎ1ＰＭ", "2000-01-01", "Jan  1 2000  1:00PM"),
        ] {
            assert_eq!(
                cast_text(text, SqlType::Date),
                Ok(modern_date.to_owned()),
                "{text:?}"
            );
            assert_eq!(
                cast_text(text, SqlType::DateTime),
                Ok(legacy_date.to_owned()),
                "{text:?}"
            );
        }
        // What stays refused, in the six targets: the fullwidth digits, separators, `T`,
        // `Z`, colon, sign and comma of the same literals, the look-alikes of a month name
        // and of a marker, and the `ﬅ` ligature.
        for ty in modern.iter().chain(&legacy) {
            for text in [
                "２000-01-02",
                "Jan ２ 2000",
                "Ｊａｎ ２ 2000",
                "2000－01－02",
                "2000-01-02Ｔ13:05:06",
                "13:05:06 Ｚ",
                "13：05",
                "13:05:06＋02:00",
                "Jan 2，2000",
                "１ ＰＭ",
                "Ｓｅｐｔ 2 2000",
                "Jän 2 2000",
                "ſep 2 2000",
                "Јan 2 2000",
                "Μay 2 2000",
                "Aprİl 2 2000",
                "Ⓙan 2 2000",
                "1 ΡM",
                "1 ᴾM",
                "Auguﬅ 2 2000",
                "Marcℎ 2 2000",
                "ⅿay 2 2000",
                "1ＰＭZ",
            ] {
                assert!(cast_text(text, *ty).is_err(), "{text:?} as {ty:?}");
            }
        }
        // The one asymmetry between the two grammars is the one they already had: a NUL
        // inside a fullwidth month name is read by the legacy targets alone.
        assert_eq!(
            cast_text("Ｊ\0ａｎ 2 2000", SqlType::DateTime),
            Ok("Jan  2 2000 12:00AM".to_owned())
        );
        assert_eq!(cast_text("Ｊ\0ａｎ 2 2000", SqlType::Date), Err(241));
    }

    /// The empty string is a reading, not an error: 1900-01-01.
    #[test]
    fn parse_string_reads_the_empty_string_as_1900() {
        for text in ["", "   "] {
            assert_eq!(
                parse_string(text),
                Some(Local {
                    days: DAYS_1900,
                    ticks: 0,
                    offset: None
                }),
                "{text:?}"
            );
        }
    }

    #[test]
    fn two_digit_years_pivot_at_2049() {
        assert_eq!(expand_year(49, 2), Some(2049));
        assert_eq!(expand_year(50, 2), Some(1950));
        assert_eq!(expand_year(0, 2), Some(2000));
        assert_eq!(expand_year(99, 2), Some(1999));
        assert_eq!(expand_year(2000, 4), Some(2000));
        // One digit pivots the same way; three, and anything wider than four, is not a year.
        assert_eq!(expand_year(9, 1), Some(2009));
        assert_eq!(expand_year(999, 3), None);
        assert_eq!(expand_year(12345, 5), None);
        assert_eq!(
            parse_string("01/02/9").map(|l| l.days),
            Some(days_from_civil(2009, 1, 2))
        );
        assert_eq!(
            parse_string("01/02/49").map(|l| l.days),
            Some(days_from_civil(2049, 1, 2))
        );
        assert_eq!(
            parse_string("01/02/50").map(|l| l.days),
            Some(days_from_civil(1950, 1, 2))
        );
    }

    #[test]
    fn rounding_carries_into_the_next_day() {
        assert_eq!(round_to_scale(TICKS_PER_DAY - 1, 3), (0, 1));
        assert_eq!(round_to_scale(TICKS_PER_DAY - 1, 7), (TICKS_PER_DAY - 1, 0));
        assert_eq!(round_to_scale(1_234_500, 3), (1_230_000, 0));
        assert_eq!(round_to_scale(1_235_000, 3), (1_240_000, 0));
        assert_eq!(round_to_minute(TICKS_PER_DAY - 1), (0, 1));
        assert_eq!(round_to_minute(29 * TICKS_PER_SECOND), (0, 0));
        assert_eq!(round_to_minute(30 * TICKS_PER_SECOND), (1, 0));
        assert_eq!(round_to_300th(TICKS_PER_DAY - 1), (0, 1));
        assert_eq!(round_to_300th(9_990_000), (300, 0));
        assert_eq!(round_to_300th(9_980_000), (299, 0));
    }

    // -----------------------------------------------------------------------------------
    // The styles of `CONVERT` on the two legacy targets.
    // -----------------------------------------------------------------------------------

    /// `CONVERT(<to>, N'<s>', <style>)` from an `nvarchar`, rendered without a style, or
    /// the number of the error.
    fn styled(s: &str, to: SqlType, style: i32) -> Result<String, u32> {
        let from = ti(SqlType::NVarChar(Len::Fixed(64)));
        let value = Value::String(crate::SqlString { text: s.to_owned() });
        match to_datetime(&value, &from, &ti(to), Some(style)) {
            Ok(v) => Ok(default_display(&v, &ti(to))),
            Err(e) => Err(e.number),
        }
    }

    /// Four vectors, one per cell of a table: three targets, three answers to the same
    /// unknown style, and 295 where the modern targets say 9807.
    #[test]
    fn legacy_targets_do_not_validate_the_style_number() {
        assert_eq!(
            styled("20000102", SqlType::SmallDateTime, 77),
            Ok("Jan  2 2000 12:00AM".to_owned())
        );
        assert_eq!(styled("2000-01-02", SqlType::DateTime, 77), Err(241));
        assert_eq!(styled("2000-01-02", SqlType::Date, 77), Err(9809));
        assert_eq!(
            styled("2000-01-02 13:05:29.999", SqlType::SmallDateTime, 112),
            Err(295)
        );
    }

    /// The literals of the 241 / 9807 frontier; the rule is the rustdoc of
    /// [`strict_style_refuses`]. Each literal is read under two strict styles (112 and 6)
    /// where it takes the number of the column it stands in, and under the numeric style
    /// 101, where the 75 literals of the two arrays are 241 but two: `'01 /02/2000'` and
    /// `'01/ 02/2000'`, which style 101 reads as the 2nd of January 2000.
    const NINE_EIGHT_O_SEVEN: [&str; 37] = [
        "01/02/2000x",
        "01/02/200x",
        "00.01.0x",
        "1/2/3x",
        "01/02/2000T13:05:06",
        "2000.01.02T13:05:06",
        "01/02/2000/03",
        "01/02/2000/2000",
        "1/2/3/4/5",
        "01/02/2000,",
        "01/02/2000 x",
        "01/02/2000 Z",
        "01/02/2000 PM",
        "01/02/2000 x 3",
        "01/02/2000+02:00",
        "01/02/2000-03",
        "00.01.02 x",
        "01 /02/2000",
        "01/ 02/2000",
        "2000- 01-02",
        "2000-01-02",
        "  2000-01-02  ",
        "2000-01-02 3",
        "2000-01-02 3x",
        "2000-01-02 1x",
        "2000-01-02\t3",
        "2000-01-02 13:05 x",
        "2000-01-02 13:05:06,",
        "2000-01-02 13:05:06 Jan",
        "2000-01-02 13:05:06x",
        "2000-01-02 2000-01-02",
        "2000-01-02 02 Jan 2000",
        "2000-01-02 5 PMx",
        "01-02-2000 3",
        "1-2-3 3",
        "2000-01-02 24:00",
        "2000-01-02 13",
    ];
    const TWO_FOUR_ONE: [&str; 40] = [
        "x 01/02/2000",
        "9 01/02/2000",
        ",01/02/2000",
        ",2000-01-02",
        "13:05 01/02/2000",
        "x01/02/2000",
        "01/02/",
        "01/02/x",
        "01/02 2000",
        "01/02",
        "01.02/2000",
        "2000-01/02",
        "2000/01-02",
        "01//02/2000",
        "2000--01-02",
        "01/x02/2000",
        "2000-01-02x",
        "1-2-3x",
        "2000-01-02-03",
        "2000-01-02/03",
        "2000-01-02.03",
        "2000-01-02,",
        "01-02-2000,",
        "2000-01-02T",
        "2000-01-02 x",
        "2000-01-02 Z",
        "2000-01-02 PM",
        "2000-01-02 Jan",
        "2000-01-02 ,",
        "2000-01-02 , 3",
        "2000-01-02 +02:00",
        "2000-01-02 -03",
        "2000-01-02 x3",
        "2000-01-02\tx",
        "2000-01-02Z 3",
        "2000-01-02T13:05:06x",
        "2000-01-02T13:05:06 3",
        "20000102 3",
        "Jan 2 2000 x",
        "2-Jan-2000x",
    ];

    #[test]
    fn strict_style_frontier_9807() {
        for literal in NINE_EIGHT_O_SEVEN {
            for style in [112, 6] {
                assert_eq!(
                    styled(literal, SqlType::Date, style),
                    Err(9807),
                    "{literal:?}"
                );
            }
            let under_101 = match literal {
                "01 /02/2000" | "01/ 02/2000" => Ok("2000-01-02".to_owned()),
                _ => Err(241),
            };
            assert_eq!(
                styled(literal, SqlType::Date, 101),
                under_101,
                "{literal:?}"
            );
        }
    }

    #[test]
    fn strict_style_frontier_241() {
        for literal in TWO_FOUR_ONE {
            for style in [112, 6, 101] {
                assert_eq!(
                    styled(literal, SqlType::Date, style),
                    Err(241),
                    "{literal:?}"
                );
            }
        }
    }

    /// The three other modern targets answer like `date` on the frontier, under the
    /// styles 0, 6, 101, 112 and 120.
    #[test]
    fn strict_style_frontier_is_the_same_on_the_four_modern_targets() {
        for to in [
            SqlType::Time(7),
            SqlType::DateTime2(7),
            SqlType::DateTimeOffset(7),
        ] {
            assert_eq!(styled("01/02/2000x", to, 112), Err(9807), "{to:?}");
            assert_eq!(styled("2000-01-02 3", to, 112), Err(9807), "{to:?}");
            assert_eq!(styled("2000-01-02 x", to, 112), Err(241), "{to:?}");
            assert_eq!(styled(",01/02/2000", to, 112), Err(241), "{to:?}");
        }
    }

    /// A legacy target never answers 9809 nor 9807: the very styles the modern targets
    /// refuse (77, 200, -1, 2147483647) and the strict one (112) read what the loose
    /// grammar reads and refuse the all-numeric separated date with the style-less error.
    #[test]
    fn legacy_targets_never_validate_the_style() {
        for style in [-1, 15, 77, 112, 200, 256, 1000, 2147483647] {
            for (to, refused) in [(SqlType::DateTime, 241), (SqlType::SmallDateTime, 295)] {
                let noon = "Jan  2 2000 12:00AM".to_owned();
                assert_eq!(styled("20000102", to, style), Ok(noon.clone()), "{style}");
                assert_eq!(styled("02 Jan 2000", to, style), Ok(noon), "{style}");
                assert_eq!(
                    styled("13:05:06", to, style),
                    Ok("Jan  1 1900  1:05PM".to_owned()),
                    "{style}"
                );
                assert_eq!(
                    styled("", to, style),
                    Ok("Jan  1 1900 12:00AM".to_owned()),
                    "{style}"
                );
                assert_eq!(styled("2000-01-02", to, style), Err(refused), "{style}");
                assert_eq!(styled("01/02/2000", to, style), Err(refused), "{style}");
                assert_eq!(styled("zzz", to, style), Err(refused), "{style}");
            }
        }
    }

    /// A style that names an order reads the all-numeric date as `SET DATEFORMAT` would:
    /// the four-digit year is found first, and the order places the two other pieces.
    #[test]
    fn legacy_numbers_follow_the_dateformat_of_the_style() {
        let jan_2 = Ok("Jan  2 2000 12:00AM".to_owned());
        let feb_1 = Ok("Feb  1 2000 12:00AM".to_owned());
        let dt = SqlType::DateTime;
        // Four-digit styles: `mdy` and `ymd` read month then day, `dmy` day then month.
        for style in [101, 110, 102, 111, 20, 21, 120, 121] {
            assert_eq!(styled("2000-01-02", dt, style), jan_2, "{style}");
            assert_eq!(styled("01/02/2000", dt, style), jan_2, "{style}");
            assert_eq!(styled("01/2000/02", dt, style), jan_2, "{style}");
            assert_eq!(styled("01/02/49", dt, style), Err(241), "{style}");
        }
        for style in [103, 104, 105] {
            assert_eq!(styled("2000-01-02", dt, style), feb_1, "{style}");
            assert_eq!(styled("01/02/2000", dt, style), feb_1, "{style}");
            assert_eq!(styled("01/2000/02", dt, style), feb_1, "{style}");
        }
        // The separator is not looked at; two four-digit pieces are none.
        assert_eq!(styled("01.02.2000", dt, 101), jan_2);
        assert_eq!(styled("2000/2000/01", dt, 101), Err(241));
        // Two-digit styles: strict order, a four-digit piece is 241, a three-digit one 242.
        assert_eq!(
            styled("01/02/49", dt, 1),
            Ok("Jan  2 2049 12:00AM".to_owned())
        );
        assert_eq!(
            styled("01/02/49", dt, 3),
            Ok("Feb  1 2049 12:00AM".to_owned())
        );
        assert_eq!(styled("01/02/49", dt, 2), Err(242));
        assert_eq!(
            styled("49-01-02", dt, 2),
            Ok("Jan  2 2049 12:00AM".to_owned())
        );
        assert_eq!(styled("2000-01-02", dt, 1), Err(241));
        assert_eq!(styled("999-01-02", dt, 1), Err(242));
        assert_eq!(styled("01/02/999", dt, 2), Err(242));
        // An impossible date under a style that reads its shape is 242, not 241.
        assert_eq!(styled("2000-13-45", dt, 101), Err(242));
        assert_eq!(styled("2000-13-45", dt, 77), Err(241));
        assert_eq!(styled("1899-12-31", dt, 103), Err(242));
        assert_eq!(
            styled("2000-01-02 13:05", dt, 103),
            Ok("Feb  1 2000  1:05PM".to_owned())
        );
        // The style-less shape of a `smalldatetime` reading still applies past the style.
        assert_eq!(
            styled("2000-01-02 13:05:29.999", SqlType::SmallDateTime, 103),
            Ok("Feb  1 2000  1:06PM".to_owned())
        );
    }

    /// Styles 126 and 127 read the ISO spelling on its full width, and 127 alone reads a
    /// `Z`, a year alone and a clock alone — and nothing else.
    #[test]
    fn legacy_iso_styles_126_and_127() {
        let dt = SqlType::DateTime;
        let one_pm = Ok("Jan  2 2000  1:05PM".to_owned());
        for style in [126, 127] {
            assert_eq!(styled("2000-01-02T13:05:06", dt, style), one_pm, "{style}");
            assert_eq!(
                styled("2000-01-02T13:05:06.999", dt, style),
                Ok("Jan  2 2000  1:05PM".to_owned()),
                "{style}"
            );
            assert_eq!(
                styled("  2000-01-02", dt, style),
                Ok("Jan  2 2000 12:00AM".to_owned()),
                "{style}"
            );
            assert_eq!(styled("2000-1-2", dt, style), Err(241), "{style}");
            assert_eq!(styled("2000-01-02T1:05:06", dt, style), Err(241), "{style}");
            assert_eq!(styled("2000-01-02T13:5:6", dt, style), Err(241), "{style}");
            assert_eq!(
                styled("2000-01-02 T13:05:06", dt, style),
                Err(241),
                "{style}"
            );
            assert_eq!(
                styled("2000-01-02 13:05:06", dt, style),
                Err(241),
                "{style}"
            );
            assert_eq!(styled("2000-01-02T13:05", dt, style), Err(241), "{style}");
            assert_eq!(
                styled("2000-01-02T13:05:06.", dt, style),
                Err(241),
                "{style}"
            );
            assert_eq!(
                styled("2000-01-02T13:05:06.1234", dt, style),
                Err(241),
                "{style}"
            );
            assert_eq!(
                styled("2000-01-02T13:05:06+02:00", dt, style),
                Err(241),
                "{style}"
            );
            assert_eq!(styled("01/02/2000", dt, style), Err(241), "{style}");
            assert_eq!(styled("2024-02-30", dt, style), Err(242), "{style}");
        }
        assert_eq!(styled("2000-01-02T13:05:06Z", dt, 126), Err(241));
        assert_eq!(styled("2000-01-02T13:05:06Z", dt, 127), one_pm);
        assert_eq!(styled("2000Z", dt, 126), Err(241));
        assert_eq!(
            styled("2000Z", dt, 127),
            Ok("Jan  1 2000 12:00AM".to_owned())
        );
        // 126 keeps the loose grammar behind the ISO one; 127 keeps a clock alone.
        for s in ["20000102", "Jan 2 2000", "2000-Jan-02", "2000 Jan 2"] {
            assert_eq!(
                styled(s, dt, 126),
                Ok("Jan  2 2000 12:00AM".to_owned()),
                "{s}"
            );
            assert_eq!(styled(s, dt, 127), Err(241), "{s}");
        }
        for s in ["13:05:06", "13:05:06Z"] {
            assert_eq!(
                styled(s, dt, 127),
                Ok("Jan  1 1900  1:05PM".to_owned()),
                "{s}"
            );
        }
        assert_eq!(
            styled("1 PM", dt, 127),
            Ok("Jan  1 1900  1:00PM".to_owned())
        );
        assert_eq!(styled("2000-01-02T13", dt, 127), Err(241));
    }

    /// A capital `T` outside a word is never read by the loose grammar: 242 when what
    /// stands in front of it reads under the style, 241 when it does not.
    #[test]
    fn legacy_glued_t_is_242_when_the_date_reads() {
        let dt = SqlType::DateTime;
        assert_eq!(styled("2000-01-02T13:05:06", dt, 101), Err(242));
        assert_eq!(styled("2000-01-02T13:05:06", dt, 77), Err(241));
        assert_eq!(styled("2000-01-02T", dt, 101), Err(242));
        assert_eq!(styled("2000-01-02 T13:05:06", dt, 101), Err(242));
        assert_eq!(styled(" 2000-01-02T13:05:06", dt, 77), Err(241));
        assert_eq!(styled("01/02/2000T13:05:06", dt, 101), Err(242));
        for style in [1, 77, 101, 126, 127] {
            assert_eq!(styled("T13:05:06", dt, style), Err(242), "{style}");
            // `'20000102'` reads under every grammar but the clock-only one of 127.
            let compact = if style == 127 { Err(241) } else { Err(242) };
            assert_eq!(styled("20000102T13:05:06", dt, style), compact, "{style}");
            assert_eq!(
                styled("2000-01-02t13:05:06", dt, style),
                Err(241),
                "{style}"
            );
            assert_eq!(
                styled("2000-01-02t13:05:06", SqlType::SmallDateTime, style),
                Err(295),
                "{style}"
            );
        }
        assert_eq!(styled("2000-01-02T13", dt, 126), Err(241));
        assert_eq!(glued_t("OCTOBER 2 2000"), None);
        assert_eq!(glued_t("2 SEPT 2000"), None);
        assert_eq!(glued_t("2000-01-02T13"), Some(10));
        assert_eq!(glued_t("T13"), Some(0));
    }

    /// What stands behind the glued `T` is judged before what stands in front of it: a
    /// clock that does not read is 241 regardless of the head, and a bare number is 242
    /// under each style that names an order.
    #[test]
    fn legacy_glued_t_judges_its_tail_first() {
        let dt = SqlType::DateTime;
        let sdt = SqlType::SmallDateTime;
        // The vector that separates "the tail is judged" from "the head decides":
        // the same head, 242 with a clock behind the `T` and 241 with a dangling point.
        for style in [20, 101, 102, 103, 120] {
            assert_eq!(
                styled("2000-01-02T13:05:06", dt, style),
                Err(242),
                "{style}"
            );
            assert_eq!(
                styled("2000-01-02T13:05:06.", dt, style),
                Err(241),
                "{style}"
            );
            assert_eq!(
                styled("2000-01-02T13:05:06.", sdt, style),
                Err(295),
                "{style}"
            );
        }
        // A bare number behind the `T`: 242 under a style that names an order, even the
        // two-digit ones under which the head `'2000-01-02'` does not read (`'2000-01-02T'`
        // is 241 there), 241 under the others.
        for style in [1, 2, 3, 11, 20, 101, 102, 103, 120] {
            assert_eq!(styled("2000-01-02T13", dt, style), Err(242), "{style}");
            assert_eq!(styled("2000-01-02T13", sdt, style), Err(242), "{style}");
        }
        for style in [1, 2, 3, 11] {
            assert_eq!(styled("2000-01-02T", dt, style), Err(241), "{style}");
            assert_eq!(styled("2000-01-02T13:05", dt, style), Err(241), "{style}");
        }
        for style in [22, 77, 112, 200] {
            assert_eq!(styled("2000-01-02T13", dt, style), Err(241), "{style}");
            assert_eq!(styled("2000-01-02T13", sdt, style), Err(295), "{style}");
        }
    }

    /// Under a two-digit style the pieces are judged as written, and a four-digit year
    /// only after the two others: `'2000-999-01'` is 242 under `ymd`, 241 under `mdy` and
    /// `dmy`.
    #[test]
    fn legacy_two_digit_style_weighs_the_year_last() {
        let dt = SqlType::DateTime;
        let sdt = SqlType::SmallDateTime;
        for style in [2, 11] {
            assert_eq!(styled("2000-999-01", dt, style), Err(242), "{style}");
            assert_eq!(styled("2000-999-01", sdt, style), Err(242), "{style}");
            // The same four-digit year with two pieces in range, or out of range by
            // value but within two digits, stays 241.
            assert_eq!(styled("2000-01-02", dt, style), Err(241), "{style}");
            assert_eq!(styled("2000-13-45", dt, style), Err(241), "{style}");
            assert_eq!(styled("2000-001-02", dt, style), Err(241), "{style}");
            assert_eq!(styled("999-01-02", dt, style), Err(242), "{style}");
            assert_eq!(styled("01/02/999", dt, style), Err(242), "{style}");
        }
        for style in [1, 3] {
            assert_eq!(styled("2000-999-01", dt, style), Err(241), "{style}");
            assert_eq!(styled("2000-999-01", sdt, style), Err(295), "{style}");
            assert_eq!(styled("999-01-02", dt, style), Err(242), "{style}");
            assert_eq!(styled("01/2000/02", dt, style), Err(241), "{style}");
        }
    }

    /// The grammar of a style is a function of its number alone.
    #[test]
    fn legacy_style_table() {
        let numbers = |order, century| LegacyStyle::Numbers { order, century };
        assert_eq!(legacy_style(1), numbers(Order::Mdy, Century::Two));
        assert_eq!(legacy_style(10), numbers(Order::Mdy, Century::Two));
        assert_eq!(legacy_style(2), numbers(Order::Ymd, Century::Two));
        assert_eq!(legacy_style(11), numbers(Order::Ymd, Century::Two));
        for style in [3, 4, 5] {
            assert_eq!(legacy_style(style), numbers(Order::Dmy, Century::Two));
        }
        for style in [101, 110] {
            assert_eq!(legacy_style(style), numbers(Order::Mdy, Century::Four));
        }
        for style in [20, 21, 102, 111, 120, 121] {
            assert_eq!(legacy_style(style), numbers(Order::Ymd, Century::Four));
        }
        for style in [103, 104, 105] {
            assert_eq!(legacy_style(style), numbers(Order::Dmy, Century::Four));
        }
        assert_eq!(legacy_style(126), LegacyStyle::Iso { zulu: false });
        assert_eq!(legacy_style(127), LegacyStyle::Iso { zulu: true });
        assert_eq!(legacy_style(130), LegacyStyle::Hijri);
        assert_eq!(legacy_style(131), LegacyStyle::Hijri);
        // 22 exists for the modern targets and names no order here; 6 to 9, 12 to 14, 24,
        // 100, 106 to 109 and 112 to 115 are the same, and so is every unknown number.
        for style in [
            6, 9, 12, 14, 22, 23, 24, 25, 100, 106, 109, 112, 115, -1, 77, 200,
        ] {
            assert_eq!(legacy_style(style), LegacyStyle::Other, "{style}");
        }
    }
}
