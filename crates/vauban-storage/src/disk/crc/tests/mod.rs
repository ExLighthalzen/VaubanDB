use super::*;

/// Check value of CRC-32/ISO-HDLC, the standard vector of the parameters above.
#[test]
fn crc32_check_value_123456789() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn crc32_of_an_empty_slice_is_zero() {
    // The seed cancels itself against the final xor when nothing is fed in.
    assert_eq!(crc32(b""), 0);
    assert_eq!(Crc32::new().finish(), 0);
    assert_eq!(Crc32::default().finish(), 0);
}

#[test]
fn crc32_in_several_slices_matches_one_shot() {
    let bytes = b"123456789";
    let mut split = Crc32::new();
    split.update(&bytes[..4]);
    split.update(&bytes[4..]);
    assert_eq!(split.finish(), crc32(bytes));
}

#[test]
fn crc32_reacts_to_a_flipped_bit() {
    let mut flipped = *b"123456789";
    flipped[0] ^= 0x01;
    assert_ne!(crc32(&flipped), crc32(b"123456789"));
}
