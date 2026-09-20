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
