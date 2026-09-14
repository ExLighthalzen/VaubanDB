//! [MS-TDS] 2.2.5.1.2 Collation Rule Definition: the 5-byte collation that follows the
//! maximum length in the TYPE_INFO of `char`, `varchar`, `nchar` and `nvarchar`.
//!
//! The rule packs a 20-bit LCID, 8 bits of comparison flags and a 4-bit version into one
//! little-endian `u32`, then the `SortId` on one byte:
//!
//! ```text
//! u32 = lcid | (flags << 20) | (version << 28)     little-endian
//! then sort_id
//! ```
//!
//! `SQL_Latin1_General_CP1_CI_AS` ([`Collation::DEFAULT`]) is `09 04 D0 00 34`.

use vauban_types::Collation;

/// Size of the `Collation` rule on the wire.
pub(crate) const COLLATION_LEN: usize = 5;

/// Encodes `c` as the 5-byte `Collation` rule ([MS-TDS] 2.2.5.1.2).
///
/// The fields are masked to their width in the rule (LCID 20 bits, flags 8 bits, version
/// 4 bits): a `Collation` built with wider values is not an error, its extra bits are dropped.
pub(crate) fn encode_collation(c: &Collation) -> [u8; COLLATION_LEN] {
    let packed =
        (c.lcid & 0x000F_FFFF) | (u32::from(c.flags) << 20) | (u32::from(c.version & 0x0F) << 28);
    let le = packed.to_le_bytes();
    [le[0], le[1], le[2], le[3], c.sort_id]
}

/// `SQL_Latin1_General_CP1_CI_AS` on the wire, as every TYPE_INFO test of `types/` expects
/// it after the maximum length.
#[cfg(test)]
pub(crate) const DEFAULT_COLLATION_BYTES: [u8; COLLATION_LEN] = [0x09, 0x04, 0xD0, 0x00, 0x34];

#[cfg(test)]
mod tests {
    use vauban_types::Collation;

    use super::{DEFAULT_COLLATION_BYTES, encode_collation};

    #[test]
    fn collation_default_bytes() {
        assert_eq!(
            encode_collation(&Collation::DEFAULT),
            DEFAULT_COLLATION_BYTES
        );
    }

    #[test]
    fn collation_packs_flags_and_version() {
        // Latin1_General_100_CI_AS: LCID 0x0409, flags 0x0D, version 2, no SortId.
        let c = Collation {
            lcid: 0x0409,
            flags: 0x0D,
            version: 2,
            sort_id: 0,
        };
        assert_eq!(encode_collation(&c), [0x09, 0x04, 0xD0, 0x20, 0x00]);

        // French (0x040C), binary flag 0x01 alone, version 1, SortId 0.
        let c = Collation {
            lcid: 0x040C,
            flags: 0x01,
            version: 1,
            sort_id: 0,
        };
        assert_eq!(encode_collation(&c), [0x0C, 0x04, 0x10, 0x10, 0x00]);
    }

    #[test]
    fn collation_masks_wide_fields() {
        let c = Collation {
            lcid: 0xFFF0_0409,
            flags: 0xFF,
            version: 0xF2,
            sort_id: 1,
        };
        assert_eq!(encode_collation(&c), [0x09, 0x04, 0xF0, 0x2F, 0x01]);
    }
}
