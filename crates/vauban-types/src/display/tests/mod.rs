use super::*;

/// The pieces that have no direct vector in `tests/display.rs` because they are private.
#[test]
fn rescale_rounds_half_away_from_zero() {
    assert_eq!(rescale(150, 2, 2), Some(150));
    assert_eq!(rescale(15, 1, 3), Some(1500));
    assert_eq!(rescale(105, 3, 2), Some(11)); // 0.105 -> 0.11, not 0.10
    assert_eq!(rescale(-105, 3, 2), Some(-11));
    assert_eq!(rescale(104, 3, 2), Some(10));
    assert_eq!(rescale(-104, 3, 2), Some(-10));
    assert_eq!(rescale(5, 1, 0), Some(1)); // 0.5 -> 1
    assert_eq!(rescale(-5, 1, 0), Some(-1));
    assert_eq!(rescale(i128::MAX, 0, 38), None);
}

#[test]
fn trim_zeros_keeps_integers_intact() {
    assert_eq!(trim_zeros("1000"), "1000");
    assert_eq!(trim_zeros("1.00000"), "1");
    assert_eq!(trim_zeros("0.100000"), "0.1");
    assert_eq!(trim_zeros("1.23457"), "1.23457");
}

#[test]
fn pad_right_counts_characters_not_bytes() {
    assert_eq!(pad_right("ab", 5), "ab   ");
    assert_eq!(pad_right("éé", 4), "éé  ");
    assert_eq!(pad_right("abcdef", 3), "abcdef");
}
