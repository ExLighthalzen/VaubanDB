use super::*;
use crate::calendar::{DAYS_1900, days_from_civil};

/// The three day counts the `datetime` range check uses, recomputed from the
/// calendar: 1753-01-01 and 9999-12-31 for `datetime`, 2079-06-06 for
/// `smalldatetime`, whose two unsigned bytes of days start at 1900-01-01.
#[test]
fn datetime_bounds_are_the_calendar_ones() {
    assert_eq!(days_from_civil(1753, 1, 1) - DAYS_1900, DATETIME_MIN_DAYS);
    assert_eq!(days_from_civil(9999, 12, 31) - DAYS_1900, DATETIME_MAX_DAYS);
    assert_eq!(
        days_from_civil(2079, 6, 6) - DAYS_1900,
        SMALLDATETIME_MAX_DAYS
    );
}

/// The long multiplication against the `i128` one, on values small enough for both.
#[test]
fn long_multiplication_agrees_with_i128() {
    let vectors = [
        (0i128, 0i128),
        (1, 1),
        (999_999_999, 999_999_999),
        (1_000_000_000, 1_000_000_000),
        (-123_456_789_012_345, 987_654_321),
        (170_141_183_460_469_231, -1),
        (12_345_678_901_234_567_890, 1_000_000_007),
    ];
    for (x, y) in vectors {
        let product = BigInt::from_i128(x).times(&BigInt::from_i128(y));
        assert_eq!(product.to_i128(), Some(x * y), "{x} * {y}");
    }
}

/// A product wider than an `i128`, checked digit by digit: `(10^38 - 1)^2` is
/// `10^76 - 2 · 10^38 + 1`, whose decimal form is 37 nines, then an 8, then 37 zeros,
/// then a 1.
#[test]
fn long_multiplication_goes_beyond_i128() {
    let max = BigUint::from_u128(1)
        .times_pow10(38)
        .minus(&BigUint::from_u128(1));
    let square = max.times(&max);
    assert_eq!(
        square.to_u128(),
        None,
        "the square needs more than 128 bits"
    );

    let expected = BigUint::from_u128(1)
        .times_pow10(76)
        .minus(&BigUint::from_u128(2).times_pow10(38))
        .plus(&BigUint::from_u128(1));
    assert_eq!(square, expected);
}

/// The long division against the `i128` one, quotient and remainder, signs included.
#[test]
fn long_division_agrees_with_i128() {
    let vectors = [
        (7i128, 2i128),
        (-7, 2),
        (7, -2),
        (-7, -2),
        (0, 5),
        (1_000_000_000_000_000_000, 999_999_999),
        (i128::MAX, 3),
        (-123_456_789_012_345_678, 1_000_003),
    ];
    for (n, d) in vectors {
        let (quotient, remainder) = BigInt::from_i128(n).divided_by(&BigInt::from_i128(d));
        assert_eq!(quotient.to_i128(), Some(n / d), "{n} / {d}");
        assert_eq!(remainder.to_i128(), Some(n % d), "{n} % {d}");
    }
}

/// The rounding is half away from zero, on both signs, and never banker's rounding.
#[test]
fn rounding_is_half_away_from_zero() {
    assert_eq!(divide_rounded(25, 10), Some(3));
    assert_eq!(divide_rounded(-25, 10), Some(-3));
    assert_eq!(divide_rounded(35, 10), Some(4));
    assert_eq!(divide_rounded(-35, 10), Some(-4));
    assert_eq!(divide_rounded(24, 10), Some(2));
    assert_eq!(divide_rounded(-24, 10), Some(-2));
    assert_eq!(divide_rounded(1, 0), None);
}

/// A quotient that needs more than 128 bits is refused rather than wrapped.
#[test]
fn a_quotient_beyond_i128_does_not_fit() {
    let huge = BigInt::from_i128(1).times_pow10(60);
    assert_eq!(huge.to_i128(), None);
    assert_eq!(huge.mantissa(38), None);
    assert_eq!(BigInt::from_i128(-50).mantissa(1), None);
    assert_eq!(BigInt::from_i128(-50).mantissa(2), Some(-50));
}

/// `rescaled` grows exactly and shrinks with the rounding rule.
#[test]
fn rescaling_rounds_only_when_it_shrinks() {
    let value = BigInt::from_i128(12_345);
    assert_eq!(rescaled(&value, 2, 4).to_i128(), Some(1_234_500));
    assert_eq!(rescaled(&value, 2, 2).to_i128(), Some(12_345));
    assert_eq!(rescaled(&value, 2, 1).to_i128(), Some(1_235));
    assert_eq!(rescaled(&value.negated(), 2, 1).to_i128(), Some(-1_235));
}
