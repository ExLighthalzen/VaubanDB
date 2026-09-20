use crate::Collation;

use super::{CP1252_HIGH_RANGE, decode, encode};

#[test]
fn high_range_holds_twenty_seven_characters() {
    let assigned = CP1252_HIGH_RANGE.iter().filter(|e| e.is_some()).count();
    assert_eq!(assigned, 27);
    // No character of the range is listed twice, or the mapping would not be a
    // bijection and `encode` would not undo `decode`.
    let mut characters: Vec<char> = CP1252_HIGH_RANGE.iter().flatten().copied().collect();
    characters.sort_unstable();
    characters.dedup();
    assert_eq!(characters.len(), 27);
}

#[test]
fn c1_controls_encode_only_where_the_page_has_no_character() {
    let latin1 = Collation::DEFAULT;
    for code_point in 0x80u32..=0x9F {
        let c = char::from_u32(code_point).expect("C1 controls are valid code points");
        let byte = code_point as u8;
        let expected = CP1252_HIGH_RANGE[(byte - 0x80) as usize]
            .is_none()
            .then_some(byte);
        assert_eq!(encode(c, &latin1), expected, "U+{code_point:04X}");
    }
}

#[test]
fn decode_is_the_identity_outside_the_high_range() {
    let latin1 = Collation::DEFAULT;
    for byte in (0x00u8..=0x7F).chain(0xA0..=0xFF) {
        assert_eq!(decode(byte, &latin1), Some(byte as char), "{byte:#04X}");
    }
}
