//! Odd hexadecimal payload normalization and parse/display round trips.

use vauban_parser::{ParseOptions, parse_batch};

#[test]
fn odd_hex_roundtrips_after_padding() {
    for digits in ["", "0", "ABC", "abc", "AbC", "012345", "12345"] {
        let source = format!("SELECT 0x{digits};");
        let batch = parse_batch(&source, &ParseOptions::default()).unwrap();
        let printed = batch.to_string();
        assert_eq!(
            batch,
            parse_batch(&printed, &ParseOptions::default()).unwrap()
        );
        let expected = if digits.len() % 2 == 0 {
            digits.to_owned()
        } else {
            format!("0{digits}")
        };
        assert!(printed.contains(&format!("0x{expected}")), "{printed}");
    }
}

#[test]
fn large_odd_hex_roundtrips() {
    for size in [15999, 16000, 16001] {
        let source = format!("SELECT 0x{};", "A".repeat(size));
        let batch = parse_batch(&source, &ParseOptions::default()).unwrap();
        assert_eq!(
            batch,
            parse_batch(&batch.to_string(), &ParseOptions::default()).unwrap()
        );
    }
}
