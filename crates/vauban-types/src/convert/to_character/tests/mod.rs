use super::*;

/// The scientific rendering at the level of the private helper: one digit before the
/// point, the rest after, and a signed three-digit exponent.
#[test]
fn scientific_pads_the_exponent_to_three_digits() {
    assert_eq!(scientific(1234567.0, 8), "1.2345670e+006");
    assert_eq!(scientific(1234567.0, 16), "1.234567000000000e+006");
    assert_eq!(scientific(1234567.0, 17), "1.2345670000000000e+006");
    assert_eq!(scientific(0.1, 17), "1.0000000000000001e-001");
    assert_eq!(scientific(0.0, 8), "0.0000000e+000");
    assert_eq!(scientific(-1.5, 8), "-1.5000000e+000");
    assert_eq!(scientific(1e100, 8), "1.0000000e+100");
}

/// The decoding table of this file and the encoding table of `collation.rs` describe the
/// same code page: every byte decodes to a character that encodes back to it. The five
/// bytes the published layout leaves undefined stand for the C1 control character of the
/// same number, which `code_page::encode` reads back.
#[test]
fn cp1252_char_agrees_with_cp1252_byte() {
    for byte in 0..=u8::MAX {
        let decoded = cp1252_char(byte);
        assert_eq!(
            crate::collation::cp1252_byte(decoded),
            Some(byte),
            "{byte:#04X}"
        );
    }
    assert_eq!(cp1252_char(0x4E), 'N');
    assert_eq!(cp1252_char(0xE9), 'é');
    assert_eq!(cp1252_char(0x80), '\u{20AC}');
}

/// Style 1 and style 2 drop whole bytes; style 1 loses two characters to its prefix.
#[test]
fn hex_styles_keep_whole_bytes() {
    let name = [0x4E, 0x61, 0x6D, 0x65];
    let varchar = |n: u16| SqlType::VarChar(Len::Fixed(n));
    let text = |to: SqlType, style: i32| {
        binary_as_character(&name, &to, Some(style)).expect("a supported style")
    };
    assert_eq!(text(varchar(40), 1), "0x4E616D65");
    assert_eq!(text(SqlType::Char(Len::Fixed(8)), 1), "0x4E616D");
    assert_eq!(text(varchar(3), 1), "0x");
    assert_eq!(text(varchar(1), 1), "");
    assert_eq!(text(varchar(3), 2), "4E");
    assert_eq!(text(SqlType::VarChar(Len::Max), 2), "4E616D65");
    assert_eq!(text(varchar(20), 0), "Name");
    assert_eq!(
        binary_as_character(&name, &varchar(20), Some(3))
            .expect_err("style 3 is not a binary style")
            .number,
        9809
    );
}

/// A national target reads the bytes as UTF-16LE, a last odd byte included.
#[test]
fn national_targets_read_utf16() {
    assert_eq!(decode_bytes(&[0x4E, 0x61, 0x6D, 0x65], true), "慎敭");
    assert_eq!(decode_bytes(&[0x4E, 0x61, 0x6D], true), "慎m");
    assert_eq!(decode_bytes(&[0x4E, 0x61, 0x6D, 0x65], false), "Name");
    assert_eq!(decode_bytes(&[], true), "");
}

#[test]
fn four_decimals_keeps_every_ten_thousandth() {
    assert_eq!(four_decimals(42359819), "4235.9819");
    assert_eq!(four_decimals(-42359819), "-4235.9819");
    assert_eq!(four_decimals(5), "0.0005");
    assert_eq!(four_decimals(0), "0.0000");
}

#[test]
fn group_thousands_starts_from_the_decimal_point() {
    assert_eq!(group_thousands("4235.98"), "4,235.98");
    assert_eq!(group_thousands("-4235.98"), "-4,235.98");
    assert_eq!(group_thousands("1234567890.12"), "1,234,567,890.12");
    assert_eq!(group_thousands("0.50"), "0.50");
    assert_eq!(group_thousands("100"), "100");
    assert_eq!(group_thousands("1000"), "1,000");
}
