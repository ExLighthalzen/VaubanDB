//! Civil calendar: days since an epoch ↔ year, month, day.
//!
//! Public module, imported as-is by `sysfn` rather than reimplemented there.
//!
//! The calendar is the **proleptic Gregorian** one, the calendar of `date`, `datetime2` and
//! `datetimeoffset` over their whole range: a year is a leap year when it is divisible by 4
//! and not by 100, or when it is divisible by 400, with no Julian period and no missing days
//! in 1582.
//!
//! Everything here is integer arithmetic: no date crate is allowed in this workspace, and
//! floating point would lose days on the far end of the range.
//!
//! The conversions use the classical "shifted year" trick: counting from the 1st of March
//! puts the leap day at the end of the year, so a year is 365 or 366 consecutive days and a
//! 400-year era is exactly 146 097 days. Days are then re-based on 0001-01-01, which is
//! 306 days after 0000-03-01.

/// Days of one 400-year Gregorian era (`400 * 365 + 97`).
const DAYS_PER_ERA: i32 = 146_097;

/// Days from 0000-03-01 (the epoch of the shifted-year arithmetic) to 0001-01-01.
const OFFSET_0001: i32 = 306;

/// Days from 0001-01-01 to 1900-01-01, the epoch of SQL Server's `datetime`.
///
/// `Value::Date` counts days from 0001-01-01 while [`crate::DateTime`] counts them from
/// 1900-01-01: `date_days == DAYS_1900 + datetime_days`.
pub const DAYS_1900: i32 = 693_595;

/// Splits a day count into a civil date `(year, month, day)` of the proleptic Gregorian
/// calendar.
///
/// `days` is counted from **0001-01-01, which is day 0**, the convention of
/// [`crate::Value::Date`]. The returned month and day are 1-based. Day counts outside
/// `0..=3_652_058` (that is, outside `0001-01-01..=9999-12-31`) are still converted — the
/// arithmetic is exact for the whole `i32` range — but they are not values SQL Server can
/// store; use [`is_valid_civil`] to reject them.
///
/// # Examples
///
/// ```
/// use vauban_types::calendar::{civil_from_days, DAYS_1900};
///
/// assert_eq!(civil_from_days(0), (1, 1, 1));
/// assert_eq!(civil_from_days(DAYS_1900), (1900, 1, 1));
/// assert_eq!(civil_from_days(730_119), (2000, 1, 1));
/// ```
pub fn civil_from_days(days: i32) -> (i32, u8, u8) {
    // Re-base on 0000-03-01 so that February, and its leap day, ends the year.
    let shifted = days as i64 + OFFSET_0001 as i64;
    let era = shifted.div_euclid(DAYS_PER_ERA as i64);
    let day_of_era = shifted.rem_euclid(DAYS_PER_ERA as i64); // 0..=146_096
    // Year of the era: subtract the leap days the era has accumulated so far.
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let shifted_year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // 0..=365
    // Months of the shifted year have lengths 31 30 31 30 31 31 30 31 30 31 31 28/29, whose
    // running sum is exactly `(153 * m + 2) / 5`.
    let shifted_month = (5 * day_of_year + 2) / 153; // 0 = March .. 11 = February
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u8;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u8;
    let year = if month <= 2 {
        shifted_year + 1
    } else {
        shifted_year
    };
    (year as i32, month, day)
}

/// Counts the days from 0001-01-01 to the civil date `(y, m, d)` of the proleptic
/// Gregorian calendar.
///
/// The inverse of [`civil_from_days`], with the same origin: **0001-01-01 is day 0**, the
/// convention of [`crate::Value::Date`]. `m` and `d` are 1-based. The result is only
/// meaningful for a date [`is_valid_civil`] accepts; a nonsensical day such as
/// `(2000, 2, 31)` is normalised into the following month instead of being rejected, since
/// the function returns a number and not a result.
pub fn days_from_civil(y: i32, m: u8, d: u8) -> i32 {
    let month = m as i64;
    // Shift the year so that it starts on the 1st of March.
    let shifted_year = y as i64 - i64::from(month <= 2);
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400); // 0..=399
    let shifted_month = if month > 2 { month - 3 } else { month + 9 }; // 0 = March
    let day_of_year = (153 * shifted_month + 2) / 5 + d as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * DAYS_PER_ERA as i64 + day_of_era - OFFSET_0001 as i64) as i32
}

/// Tells whether `(y, m, d)` is a date a `date` can store: a real day of the proleptic
/// Gregorian calendar, between 0001-01-01 and 9999-12-31.
///
/// The bounds are those of `date`, `datetime2` and `datetimeoffset`; the narrower ranges of
/// `datetime` (from 1753-01-01) and `smalldatetime` (1900-01-01 to 2079-06-06) are checked by
/// the conversion to those types, not here.
pub fn is_valid_civil(y: i32, m: u8, d: u8) -> bool {
    (1..=9999).contains(&y) && (1..=12).contains(&m) && (1..=days_in_month(y, m)).contains(&d)
}

/// Number of days of month `m` of year `y`, `0` when `m` is not a month.
pub(crate) fn days_in_month(y: i32, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Gregorian leap year: divisible by 4, except centuries that are not divisible by 400.
pub(crate) fn is_leap_year(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// 100-nanosecond ticks in one second.
pub(crate) const TICKS_PER_SECOND: u64 = 10_000_000;

/// 100-nanosecond ticks in one day.
pub(crate) const TICKS_PER_DAY: u64 = 24 * 60 * 60 * TICKS_PER_SECOND;

/// Splits a time of day into `(hours, minutes, seconds, fraction)`.
///
/// `ticks_100ns` counts 100-nanosecond units since midnight, the representation of
/// [`crate::Time`]. The fraction is the leftover within the second, still in 100 ns units,
/// so `0..=9_999_999` — seven digits, the precision of `time(7)`. A tick count of a full day
/// or more (which [`crate::Time`] never holds) wraps around.
pub(crate) fn hms_from_ticks(ticks_100ns: u64) -> (u8, u8, u8, u32) {
    let ticks = ticks_100ns % TICKS_PER_DAY;
    let seconds = ticks / TICKS_PER_SECOND;
    let fraction = (ticks % TICKS_PER_SECOND) as u32;
    (
        (seconds / 3600) as u8,
        ((seconds / 60) % 60) as u8,
        (seconds % 60) as u8,
        fraction,
    )
}

/// Converts the 1/300 s ticks of a `datetime` into 100-nanosecond ticks.
///
/// `t * 10_000_000 / 300`, rounded to the nearest, halves away from zero (`t` is never
/// negative, so away from zero is up). That is why a `datetime` shows milliseconds ending in
/// 0, 3 and 7: tick 1 is 3 333 333 units of 100 ns, that is 3.3333 ms.
pub(crate) fn ticks_300th_to_100ns(t: u32) -> u64 {
    // round(n / 300) == (2n + 300) / 600 for n >= 0.
    (2 * t as u64 * TICKS_PER_SECOND + 300) / 600
}

/// Converts 100-nanosecond ticks into the 1/300 s ticks of a `datetime`.
///
/// The inverse of [`ticks_300th_to_100ns`], rounded the same way: to the nearest, halves
/// away from zero. Rounding, not truncation, is what makes the round trip stable.
pub(crate) fn ticks_100ns_to_300th(ticks_100ns: u64) -> u32 {
    // round(t * 300 / 10_000_000) == (600 t + 10_000_000) / 20_000_000 for t >= 0.
    ((600 * ticks_100ns + TICKS_PER_SECOND) / (2 * TICKS_PER_SECOND)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Days from 0001-01-01 to 9999-12-31, the last day SQL Server's `date` can hold.
    const MAX_DAYS: i32 = 3_652_058;

    #[test]
    fn epoch_and_bounds() {
        assert_eq!(civil_from_days(0), (1, 1, 1));
        assert_eq!(days_from_civil(1, 1, 1), 0);
        assert_eq!(civil_from_days(MAX_DAYS), (9999, 12, 31));
        assert_eq!(days_from_civil(9999, 12, 31), MAX_DAYS);
        assert_eq!(days_from_civil(1900, 1, 1), DAYS_1900);
        assert_eq!(civil_from_days(DAYS_1900), (1900, 1, 1));
    }

    /// Every day of the supported range converts back to itself. Slow enough to stay a
    /// single loop, cheap enough to run in a debug build (3.6 million iterations).
    #[test]
    fn round_trip_over_the_whole_range() {
        let mut previous = (0, 0, 0);
        for days in 0..=MAX_DAYS {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "day {days}");
            assert!(is_valid_civil(y, m, d), "{y}-{m}-{d}");
            assert!((y, m, d) > previous, "not increasing at day {days}");
            previous = (y, m, d);
        }
    }

    #[test]
    fn ticks_conversions_round_trip() {
        assert_eq!(hms_from_ticks(0), (0, 0, 0, 0));
        assert_eq!(hms_from_ticks(471_061_234_567), (13, 5, 6, 1_234_567));
        assert_eq!(hms_from_ticks(TICKS_PER_DAY - 1), (23, 59, 59, 9_999_999));

        assert_eq!(ticks_300th_to_100ns(0), 0);
        assert_eq!(ticks_300th_to_100ns(1), 33_333);
        assert_eq!(ticks_300th_to_100ns(300), TICKS_PER_SECOND);
        // The last tick of a day: 23:59:59.997.
        assert_eq!(hms_from_ticks(ticks_300th_to_100ns(25_919_999)).0, 23);
        for t in [0_u32, 1, 2, 3, 150, 299, 300, 14_130_000, 25_919_999] {
            assert_eq!(ticks_100ns_to_300th(ticks_300th_to_100ns(t)), t, "tick {t}");
        }
    }

    #[test]
    fn month_lengths_follow_the_gregorian_rule() {
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2000, 13), 0);
        assert_eq!(
            (1..=12).map(|m| days_in_month(2023, m) as u32).sum::<u32>(),
            365
        );
        assert_eq!(
            (1..=12).map(|m| days_in_month(2024, m) as u32).sum::<u32>(),
            366
        );
    }
}
