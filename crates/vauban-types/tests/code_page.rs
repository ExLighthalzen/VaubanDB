//! Integration tests of `code_page`: the code page 1252 of the default collation, byte by
//! byte.
//!
//! The reference is the Windows-1252 code page layout: the identity on `0x00`-`0x7F` and
//! `0xA0`-`0xFF`, 27 characters of its own in `0x80`-`0x9F`, and five unassigned bytes in
//! that same range.

use vauban_types::{Collation, code_page};

/// The 32 bytes where Windows-1252 differs from ISO 8859-1, with the character of each;
/// `None` for the five unassigned ones.
const HIGH_RANGE: [(u8, Option<char>); 32] = [
    (0x80, Some('€')), // U+20AC euro sign
    (0x81, None),
    (0x82, Some('‚')), // U+201A single low-9 quotation mark
    (0x83, Some('ƒ')), // U+0192 latin small letter f with hook
    (0x84, Some('„')), // U+201E double low-9 quotation mark
    (0x85, Some('…')), // U+2026 horizontal ellipsis
    (0x86, Some('†')), // U+2020 dagger
    (0x87, Some('‡')), // U+2021 double dagger
    (0x88, Some('ˆ')), // U+02C6 modifier letter circumflex accent
    (0x89, Some('‰')), // U+2030 per mille sign
    (0x8A, Some('Š')), // U+0160 latin capital letter s with caron
    (0x8B, Some('‹')), // U+2039 single left-pointing angle quotation mark
    (0x8C, Some('Œ')), // U+0152 latin capital ligature oe
    (0x8D, None),
    (0x8E, Some('Ž')), // U+017D latin capital letter z with caron
    (0x8F, None),
    (0x90, None),
    (0x91, Some('‘')), // U+2018 left single quotation mark
    (0x92, Some('’')), // U+2019 right single quotation mark
    (0x93, Some('“')), // U+201C left double quotation mark
    (0x94, Some('”')), // U+201D right double quotation mark
    (0x95, Some('•')), // U+2022 bullet
    (0x96, Some('–')), // U+2013 en dash
    (0x97, Some('—')), // U+2014 em dash
    (0x98, Some('˜')), // U+02DC small tilde
    (0x99, Some('™')), // U+2122 trade mark sign
    (0x9A, Some('š')), // U+0161 latin small letter s with caron
    (0x9B, Some('›')), // U+203A single right-pointing angle quotation mark
    (0x9C, Some('œ')), // U+0153 latin small ligature oe
    (0x9D, None),
    (0x9E, Some('ž')), // U+017E latin small letter z with caron
    (0x9F, Some('Ÿ')), // U+0178 latin capital letter y with diaeresis
];

/// The five bytes code page 1252 leaves unassigned: they decode to the C1 control character
/// of the same value (`CHAR(129)` is U+0081).
const UNASSIGNED: [u8; 5] = [0x81, 0x8D, 0x8F, 0x90, 0x9D];

#[test]
fn round_trip_over_the_whole_page() {
    let latin1 = Collation::DEFAULT;
    for byte in 0x00u8..=0xFF {
        match code_page::decode(byte, &latin1) {
            Some(c) => assert_eq!(
                code_page::encode(c, &latin1),
                Some(byte),
                "{byte:#04X} decodes to {c:?}, which encodes back to something else"
            ),
            None => panic!("{byte:#04X} should decode to a character"),
        }
    }
    // The count is the other half of the round trip: every byte carries a character.
    let decoded = (0x00u8..=0xFF)
        .filter(|byte| code_page::decode(*byte, &latin1).is_some())
        .count();
    assert_eq!(decoded, 256);
}

#[test]
fn high_range_matches_the_published_page() {
    let latin1 = Collation::DEFAULT;
    for (byte, expected) in HIGH_RANGE {
        let observed = expected.or(Some(byte as char));
        assert_eq!(code_page::decode(byte, &latin1), observed, "{byte:#04X}");
        if let Some(c) = expected {
            assert_eq!(code_page::encode(c, &latin1), Some(byte), "{c:?}");
        }
    }
}

#[test]
fn unassigned_bytes_are_c1_controls() {
    // `CHAR(129)` is U+0081 and `ASCII(NCHAR(129))` is 129.
    let latin1 = Collation::DEFAULT;
    for byte in UNASSIGNED {
        let c = byte as char;
        assert_eq!(code_page::decode(byte, &latin1), Some(c), "{byte:#04X}");
        assert_eq!(code_page::encode(c, &latin1), Some(byte), "{byte:#04X}");
    }
}

#[test]
fn ascii_and_latin1_supplement_are_the_identity() {
    let latin1 = Collation::DEFAULT;
    for code_point in (0x00u32..=0x7F).chain(0xA0..=0xFF) {
        let c = char::from_u32(code_point).expect("a valid code point");
        let byte = code_point as u8;
        assert_eq!(code_page::decode(byte, &latin1), Some(c), "{byte:#04X}");
        assert_eq!(code_page::encode(c, &latin1), Some(byte), "{c:?}");
    }
}

#[test]
fn characters_outside_the_page() {
    let latin1 = Collation::DEFAULT;
    // Beyond the Latin-1 supplement and outside the 27 characters of the high range.
    assert_eq!(code_page::encode('日', &latin1), None);
    assert_eq!(code_page::encode('α', &latin1), None);
    assert_eq!(code_page::encode('Ā', &latin1), None); // U+0100, next to `Œ` U+0152
    assert_eq!(code_page::encode('☺', &latin1), None);
    // The C1 control characters whose byte names a character of the page (`€`, `Ÿ`).
    assert_eq!(code_page::encode('\u{80}', &latin1), None);
    assert_eq!(code_page::encode('\u{9F}', &latin1), None);
}

#[test]
fn the_cases_char_and_ascii_rest_on() {
    let latin1 = Collation::DEFAULT;
    // `CHAR(128)` is `€` and `ASCII('€')` is 128 under the default collation.
    assert_eq!(code_page::encode('€', &latin1), Some(0x80));
    assert_eq!(code_page::decode(128, &latin1), Some('€'));
    assert_eq!(code_page::decode(0x9C, &latin1), Some('œ'));
    assert_eq!(code_page::encode('œ', &latin1), Some(0x9C));
    assert_eq!(code_page::decode(65, &latin1), Some('A'));
    assert_eq!(code_page::encode('A', &latin1), Some(65));
}

#[test]
fn every_accepted_collation_is_code_page_1252_for_now() {
    // The same page whatever the collation, the default one included.
    let names = [
        "SQL_Latin1_General_CP1_CI_AS",
        "SQL_Latin1_General_CP1_CS_AS",
        "Latin1_General_100_CI_AS",
        "Latin1_General_BIN2",
    ];
    for name in names {
        let collation = match Collation::parse(name) {
            Ok(collation) => collation,
            Err(error) => panic!("`{name}` should parse, got {error}"),
        };
        assert_eq!(code_page::decode(0x80, &collation), Some('€'), "{name}");
        assert_eq!(code_page::encode('€', &collation), Some(0x80), "{name}");
        assert_eq!(
            code_page::decode(0x8D, &collation),
            Some('\u{8D}'),
            "{name}"
        );
    }
}
