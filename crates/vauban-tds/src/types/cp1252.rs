//! Windows code page 1252, the single-byte encoding of `char` and `varchar` under the
//! default collation `SQL_Latin1_General_CP1_CI_AS` (`CP1` = code page 1252).
//!
//! Code page 1252 is ISO-8859-1 (Latin-1) except in the range `0x80..=0x9F`, where Latin-1 has
//! C1 control characters and 1252 has 27 printable characters (euro sign, curly quotes,
//! dashes, `Œ`, `Š`, `Ž`…); five positions (`0x81`, `0x8D`, `0x8F`, `0x90`, `0x9D`) are
//! unassigned.
//!
//! Only the direction Unicode → 1252 is needed by the encoder; [`CP1252_80_9F`] is exposed
//! for the decoder (`decode.rs`), which walks it in the other direction.

/// The 32 positions `0x80..=0x9F` of code page 1252, indexed by `byte - 0x80`: the Unicode
/// character at that position, or `None` where the code page assigns nothing.
pub(crate) const CP1252_80_9F: [Option<char>; 32] = [
    Some('\u{20AC}'), // 0x80 € EURO SIGN
    None,             // 0x81 unassigned
    Some('\u{201A}'), // 0x82 ‚ SINGLE LOW-9 QUOTATION MARK
    Some('\u{0192}'), // 0x83 ƒ LATIN SMALL LETTER F WITH HOOK
    Some('\u{201E}'), // 0x84 „ DOUBLE LOW-9 QUOTATION MARK
    Some('\u{2026}'), // 0x85 … HORIZONTAL ELLIPSIS
    Some('\u{2020}'), // 0x86 † DAGGER
    Some('\u{2021}'), // 0x87 ‡ DOUBLE DAGGER
    Some('\u{02C6}'), // 0x88 ˆ MODIFIER LETTER CIRCUMFLEX ACCENT
    Some('\u{2030}'), // 0x89 ‰ PER MILLE SIGN
    Some('\u{0160}'), // 0x8A Š LATIN CAPITAL LETTER S WITH CARON
    Some('\u{2039}'), // 0x8B ‹ SINGLE LEFT-POINTING ANGLE QUOTATION MARK
    Some('\u{0152}'), // 0x8C Œ LATIN CAPITAL LIGATURE OE
    None,             // 0x8D unassigned
    Some('\u{017D}'), // 0x8E Ž LATIN CAPITAL LETTER Z WITH CARON
    None,             // 0x8F unassigned
    None,             // 0x90 unassigned
    Some('\u{2018}'), // 0x91 ‘ LEFT SINGLE QUOTATION MARK
    Some('\u{2019}'), // 0x92 ’ RIGHT SINGLE QUOTATION MARK
    Some('\u{201C}'), // 0x93 “ LEFT DOUBLE QUOTATION MARK
    Some('\u{201D}'), // 0x94 ” RIGHT DOUBLE QUOTATION MARK
    Some('\u{2022}'), // 0x95 • BULLET
    Some('\u{2013}'), // 0x96 – EN DASH
    Some('\u{2014}'), // 0x97 — EM DASH
    Some('\u{02DC}'), // 0x98 ˜ SMALL TILDE
    Some('\u{2122}'), // 0x99 ™ TRADE MARK SIGN
    Some('\u{0161}'), // 0x9A š LATIN SMALL LETTER S WITH CARON
    Some('\u{203A}'), // 0x9B › SINGLE RIGHT-POINTING ANGLE QUOTATION MARK
    Some('\u{0153}'), // 0x9C œ LATIN SMALL LIGATURE OE
    None,             // 0x9D unassigned
    Some('\u{017E}'), // 0x9E ž LATIN SMALL LETTER Z WITH CARON
    Some('\u{0178}'), // 0x9F Ÿ LATIN CAPITAL LETTER Y WITH DIAERESIS
];

/// The byte SQL Server substitutes for a character the code page cannot represent: `?`.
pub(crate) const CP1252_REPLACEMENT: u8 = 0x3F;

/// Encodes `c` in code page 1252, or `None` when the code page has no byte for it.
///
/// `U+0080..=U+009F` (the C1 controls) are not representable: the five unassigned bytes of
/// the range are not used as an identity mapping.
pub(crate) fn encode_cp1252_char(c: char) -> Option<u8> {
    match u32::from(c) {
        cp @ (0x00..=0x7F | 0xA0..=0xFF) => {
            // Latin-1 range: the code point is the byte. The range guard bounds the cast.
            Some(cp as u8)
        }
        0x80..=0x9F => None,
        _ => CP1252_80_9F
            .iter()
            .position(|entry| *entry == Some(c))
            // The table has 32 entries: the position fits in a `u8`.
            .map(|i| 0x80 + i as u8),
    }
}

/// Transcodes `text` to code page 1252, one byte per character; a character the code page
/// cannot represent becomes [`CP1252_REPLACEMENT`] (`?`), as SQL Server does when it
/// converts an `nvarchar` to a `varchar` of a `CP1` collation.
pub(crate) fn encode_cp1252(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| encode_cp1252_char(c).unwrap_or(CP1252_REPLACEMENT))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{CP1252_80_9F, encode_cp1252, encode_cp1252_char};

    #[test]
    fn cp1252_vectors() {
        assert_eq!(encode_cp1252("é"), [0xE9]);
        assert_eq!(encode_cp1252("€"), [0x80]);
        assert_eq!(encode_cp1252("œ"), [0x9C]);
        assert_eq!(encode_cp1252("ы"), [0x3F]);
        assert_eq!(encode_cp1252("abc"), [0x61, 0x62, 0x63]);
    }

    #[test]
    fn cp1252_table_has_27_assigned_positions() {
        assert_eq!(CP1252_80_9F.iter().flatten().count(), 27);
        for unassigned in [0x81u8, 0x8D, 0x8F, 0x90, 0x9D] {
            assert_eq!(CP1252_80_9F[usize::from(unassigned - 0x80)], None);
        }
    }

    #[test]
    fn cp1252_table_round_trips() {
        for (i, entry) in CP1252_80_9F.iter().enumerate() {
            if let Some(c) = entry {
                assert_eq!(encode_cp1252_char(*c), Some(0x80 + i as u8), "{c:?}");
            }
        }
    }

    #[test]
    fn cp1252_edges() {
        assert_eq!(encode_cp1252("\0"), [0x00]);
        assert_eq!(encode_cp1252("\u{7F}"), [0x7F]);
        assert_eq!(encode_cp1252("\u{A0}"), [0xA0]);
        assert_eq!(encode_cp1252("ÿ"), [0xFF]);
        // C1 controls are not representable, including the five unassigned bytes.
        assert_eq!(
            encode_cp1252("\u{80}\u{81}\u{9D}\u{9F}"),
            [0x3F, 0x3F, 0x3F, 0x3F]
        );
        // Beyond the BMP: one `?` per character, not per UTF-16 unit.
        assert_eq!(encode_cp1252("a\u{1F600}b"), [0x61, 0x3F, 0x62]);
        assert_eq!(encode_cp1252(""), Vec::<u8>::new());
        assert_eq!(
            encode_cp1252("Œuvre — “ok”"),
            [
                0x8C, 0x75, 0x76, 0x72, 0x65, 0x20, 0x97, 0x20, 0x93, 0x6F, 0x6B, 0x94
            ]
        );
    }
}
