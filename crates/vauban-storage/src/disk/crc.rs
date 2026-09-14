//! CRC-32/ISO-HDLC, computed inside this crate: the workspace carries no checksum crate, and
//! the on-disk format is VaubanDB's own.
//!
//! Parameters: reflected polynomial `0xEDB8_8320`, initial register `0xFFFF_FFFF`, final xor
//! `0xFFFF_FFFF`, reflected input and output. The check value of these parameters — the
//! checksum of the nine ASCII bytes `123456789` — is `0xCBF4_3926`, asserted by the test
//! `crc32_check_value_123456789`.

/// Reflected form of the polynomial.
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// Initial value of the register, and the mask applied to it at the end.
const SEED: u32 = 0xFFFF_FFFF;

/// One table entry per byte value.
const TABLE_LEN: usize = 256;

/// Remainders of the polynomial, indexed by byte, built at compile time.
const TABLE: [u32; TABLE_LEN] = build_table();

/// Builds [`TABLE`]: entry `b` is `b` shifted out bit by bit through the polynomial.
const fn build_table() -> [u32; TABLE_LEN] {
    let mut table = [0u32; TABLE_LEN];
    let mut byte = 0usize;
    while byte < TABLE_LEN {
        let mut remainder = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            remainder = if remainder & 1 == 1 {
                (remainder >> 1) ^ POLYNOMIAL
            } else {
                remainder >> 1
            };
            bit += 1;
        }
        table[byte] = remainder;
        byte += 1;
    }
    table
}

/// Running CRC-32 register, for a checksum spread over several slices.
///
/// A page is checksummed over three slices (the bytes before the checksum field, four zero
/// bytes in its place, the bytes after it), which is why a running register is exposed
/// alongside the one-shot [`crc32`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Crc32(u32);

impl Crc32 {
    /// A register seeded with `0xFFFF_FFFF`, before any byte is fed in.
    pub(crate) fn new() -> Self {
        Self(SEED)
    }

    /// Feeds a slice into the register.
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            let index = ((self.0 ^ u32::from(byte)) & 0xFF) as usize;
            self.0 = (self.0 >> 8) ^ TABLE[index];
        }
    }

    /// Applies the final xor and yields the checksum.
    pub(crate) fn finish(self) -> u32 {
        self.0 ^ SEED
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// Checksum of a single slice.
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut register = Crc32::new();
    register.update(bytes);
    register.finish()
}

#[cfg(test)]
mod tests {
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
}
