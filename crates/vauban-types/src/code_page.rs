//! Code page of a collation: the single-byte encoding of non-Unicode character data.
//!
//! `char` and `varchar` hold one byte per character, and that byte is read through the code
//! page the collation names: under `SQL_Latin1_General_CP1_CI_AS`, code page 1252, the byte
//! `0x80` is `€` and `0x9C` is `œ`, where a Latin-1 reading would give the C1 control
//! characters instead. [`encode`] goes from a character to its byte, [`decode`] the other
//! way; both answer `None` when the character is not part of the page. Every byte decodes:
//! the five bytes Windows-1252 leaves unassigned (`0x81`, `0x8D`, `0x8F`, `0x90`, `0x9D`)
//! stand for the C1 control characters of the same value (`CHAR(129)` is U+0081,
//! `ASCII(NCHAR(129))` is 129).
//!
//! This is the crate-wide table: the sort weights of
//! [`Collation::compare`](crate::Collation::compare) are indexed by the byte [`encode`]
//! returns, and `sysfn` builds `CHAR` and `ASCII` on the two functions below.

use crate::Collation;

/// The 32 code points code page 1252 puts in its `0x80`-`0x9F` range, in byte order.
///
/// This range is the whole of what separates Windows-1252 from ISO 8859-1 (Latin-1): 27 of
/// the 32 bytes name characters taken from elsewhere in Unicode (`€` U+20AC, `Œ` U+0152,
/// `’` U+2019...), and the five remaining ones — `0x81`, `0x8D`, `0x8F`, `0x90` and `0x9D` —
/// are unassigned in the published layout: `None` here, and `decode`/`encode` treat them as
/// the identity on the C1 control character. Outside the range the code page is the
/// identity on the Unicode code point, both for ASCII (`0x00`-`0x7F`) and for the Latin-1
/// supplement (`0xA0`-`0xFF`).
const CP1252_HIGH_RANGE: [Option<char>; 32] = [
    Some('€'),
    None,
    Some('‚'),
    Some('ƒ'),
    Some('„'),
    Some('…'),
    Some('†'),
    Some('‡'),
    Some('ˆ'),
    Some('‰'),
    Some('Š'),
    Some('‹'),
    Some('Œ'),
    None,
    Some('Ž'),
    None,
    None,
    Some('‘'),
    Some('’'),
    Some('“'),
    Some('”'),
    Some('•'),
    Some('–'),
    Some('—'),
    Some('˜'),
    Some('™'),
    Some('š'),
    Some('›'),
    Some('œ'),
    None,
    Some('ž'),
    Some('Ÿ'),
];

/// First byte of the range code page 1252 fills with characters of its own.
const HIGH_RANGE_START: u8 = 0x80;
/// Last byte of that range.
const HIGH_RANGE_END: u8 = 0x9F;

/// The code page byte of `c` under `collation`, or `None` when the code page has no byte
/// for that character — an ideograph, an emoji, or one of the C1 control characters
/// U+0080..=U+009F, which code page 1252 replaces by 27 characters of its own.
///
/// This is the primitive of `ASCII` and of the conversions of a character value to its
/// non-Unicode bytes.
///
/// # Collations
///
/// Every collation VaubanDB accepts today is a code page 1252 collation, so `collation`
/// is not consulted yet; the day a second page is accepted, this function is where it is
/// selected, and callers already pass the collation of their operand.
///
/// ```
/// use vauban_types::{Collation, code_page};
///
/// let latin1 = Collation::DEFAULT;
/// assert_eq!(code_page::encode('A', &latin1), Some(0x41));
/// assert_eq!(code_page::encode('é', &latin1), Some(0xE9));
/// assert_eq!(code_page::encode('€', &latin1), Some(0x80));
/// assert_eq!(code_page::encode('日', &latin1), None);
/// ```
pub fn encode(c: char, collation: &Collation) -> Option<u8> {
    // One code page for now; the collation starts being read when a second one exists.
    let _ = collation;
    match c {
        // The guard keeps the code point below 256, so the cast is exact.
        '\u{0}'..='\u{7F}' | '\u{A0}'..='\u{FF}' => Some(c as u8),
        // A C1 control character encodes to its own byte only where the page has no
        // character of its own for that byte (the five unassigned ones).
        '\u{80}'..='\u{9F}' => {
            let byte = c as u8;
            CP1252_HIGH_RANGE[(byte - HIGH_RANGE_START) as usize]
                .is_none()
                .then_some(byte)
        }
        _ => CP1252_HIGH_RANGE
            .iter()
            .position(|entry| *entry == Some(c))
            // The array has 32 entries, so the index fits in a `u8` past `0x80`.
            .map(|index| HIGH_RANGE_START + index as u8),
    }
}

/// The character `byte` stands for under `collation`. Every byte has one: the five bytes
/// code page 1252 leaves unassigned (`0x81`, `0x8D`, `0x8F`, `0x90` and `0x9D`) stand for
/// the C1 control character of the same value.
///
/// This is the primitive of `CHAR` and of the readings of non-Unicode
/// bytes as text. `collation` is not consulted yet, for the reason [`encode`] gives.
///
/// ```
/// use vauban_types::{Collation, code_page};
///
/// let latin1 = Collation::DEFAULT;
/// assert_eq!(code_page::decode(0x41, &latin1), Some('A'));
/// assert_eq!(code_page::decode(0x80, &latin1), Some('€'));
/// assert_eq!(code_page::decode(0x9C, &latin1), Some('œ'));
/// assert_eq!(code_page::decode(0x81, &latin1), Some('\u{81}'));
/// ```
pub fn decode(byte: u8, collation: &Collation) -> Option<char> {
    // Same as `encode`: one page for now.
    let _ = collation;
    match byte {
        HIGH_RANGE_START..=HIGH_RANGE_END => {
            Some(CP1252_HIGH_RANGE[(byte - HIGH_RANGE_START) as usize].unwrap_or(byte as char))
        }
        // Outside that range the byte is its own code point (ASCII and Latin-1 supplement).
        _ => Some(byte as char),
    }
}

#[cfg(test)]
mod tests {
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
}
