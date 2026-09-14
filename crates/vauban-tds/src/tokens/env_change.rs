//! [MS-TDS] 2.2.7, token ENVCHANGE: encoder.

use bytes::{BufMut, BytesMut};
use vauban_types::Collation;

use super::{EncodeContext, EnvChange, Token, put_b_varbyte, put_b_varchar};
use crate::error::TdsError;

/// `TokenType` of ENVCHANGE.
const TOKEN_TYPE: u8 = 0xE3;
/// `Type` 1: database.
const TYPE_DATABASE: u8 = 1;
/// `Type` 2: language.
const TYPE_LANGUAGE: u8 = 2;
/// `Type` 4: packet size.
const TYPE_PACKET_SIZE: u8 = 4;
/// `Type` 7: SQL collation.
const TYPE_COLLATION: u8 = 7;
/// `Type` 8: begin transaction.
const TYPE_BEGIN_TRANSACTION: u8 = 8;
/// `Type` 9: commit transaction.
const TYPE_COMMIT_TRANSACTION: u8 = 9;
/// `Type` 10: rollback transaction.
const TYPE_ROLLBACK_TRANSACTION: u8 = 10;

/// Encodes `Token::EnvChange` into `out`.
///
/// Layout ([MS-TDS] 2.2.7, ENVCHANGE): `TokenType`, `Length` (USHORT, number of bytes after
/// it), `Type` (BYTE), then `NewValue` followed by `OldValue`:
/// - types 1, 2 and 4: both B_VARCHAR; the packet size travels as decimal text;
/// - type 7: both B_VARBYTE holding the 5-byte collation, `OldValue` empty when unknown;
/// - types 8, 9 and 10: both B_VARBYTE; the 8-byte transaction descriptor sits in
///   `NewValue` for begin and in `OldValue` for commit and rollback, the other one empty.
pub(crate) fn encode(
    token: &Token,
    _ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let Token::EnvChange(change) = token else {
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        unreachable!("env_change::encode called with a token other than Token::EnvChange");
    };

    // Built apart so that a failure leaves `out` untouched.
    let mut body = BytesMut::new();
    match change {
        EnvChange::Database { old, new } => put_varchar_pair(&mut body, TYPE_DATABASE, new, old)?,
        EnvChange::Language { old, new } => put_varchar_pair(&mut body, TYPE_LANGUAGE, new, old)?,
        EnvChange::PacketSize { old, new } => put_varchar_pair(
            &mut body,
            TYPE_PACKET_SIZE,
            &new.to_string(),
            &old.to_string(),
        )?,
        EnvChange::Collation { old, new } => {
            body.put_u8(TYPE_COLLATION);
            put_b_varbyte(&mut body, &collation_bytes(*new))?;
            match old {
                Some(old) => put_b_varbyte(&mut body, &collation_bytes(*old))?,
                None => put_b_varbyte(&mut body, &[])?,
            }
        }
        EnvChange::BeginTransaction(descriptor) => {
            body.put_u8(TYPE_BEGIN_TRANSACTION);
            put_b_varbyte(&mut body, &descriptor.to_le_bytes())?;
            put_b_varbyte(&mut body, &[])?;
        }
        EnvChange::CommitTransaction(descriptor) => {
            body.put_u8(TYPE_COMMIT_TRANSACTION);
            put_b_varbyte(&mut body, &[])?;
            put_b_varbyte(&mut body, &descriptor.to_le_bytes())?;
        }
        EnvChange::RollbackTransaction(descriptor) => {
            body.put_u8(TYPE_ROLLBACK_TRANSACTION);
            put_b_varbyte(&mut body, &[])?;
            put_b_varbyte(&mut body, &descriptor.to_le_bytes())?;
        }
    }

    // Both values are B_VARCHAR or B_VARBYTE: the body never exceeds 1 + 2 * 511 bytes.
    let len = u16::try_from(body.len())
        .map_err(|_| TdsError::Malformed("ENVCHANGE longer than 65535 bytes"))?;
    out.put_u8(TOKEN_TYPE);
    out.put_u16_le(len);
    out.put_slice(&body);
    Ok(())
}

/// Writes `Type`, then `NewValue` and `OldValue` as B_VARCHAR.
fn put_varchar_pair(
    body: &mut BytesMut,
    env_type: u8,
    new: &str,
    old: &str,
) -> Result<(), TdsError> {
    body.put_u8(env_type);
    put_b_varchar(body, new)?;
    put_b_varchar(body, old)
}

/// Wire form of a collation ([MS-TDS] 2.2.5.1.2 Collation): a little-endian DWORD holding
/// the LCID (20 bits), the comparison flags (8 bits) and the version (4 bits), then `SortId`.
/// Fields are masked to their width so that an out-of-range one cannot bleed into the next.
fn collation_bytes(collation: Collation) -> [u8; 5] {
    let info = (collation.lcid & 0x000F_FFFF)
        | (u32::from(collation.flags) << 20)
        | (u32::from(collation.version & 0x0F) << 28);
    let [b0, b1, b2, b3] = info.to_le_bytes();
    [b0, b1, b2, b3, collation.sort_id]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_change(change: EnvChange) -> BytesMut {
        let mut out = BytesMut::new();
        encode(
            &Token::EnvChange(change),
            &mut EncodeContext::default(),
            &mut out,
        )
        .unwrap();
        out
    }

    const MASTER_UTF16LE: [u8; 12] = [
        0x6D, 0x00, 0x61, 0x00, 0x73, 0x00, 0x74, 0x00, 0x65, 0x00, 0x72, 0x00,
    ];

    #[test]
    fn env_change_database() {
        let out = encode_change(EnvChange::Database {
            old: "master".into(),
            new: "master".into(),
        });
        let mut expected = vec![0xE3, 0x1B, 0x00, 0x01, 0x06];
        expected.extend_from_slice(&MASTER_UTF16LE);
        expected.push(0x06);
        expected.extend_from_slice(&MASTER_UTF16LE);
        assert_eq!(out.len(), 30);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn env_change_database_new_then_old() {
        let out = encode_change(EnvChange::Database {
            old: "a".into(),
            new: "b".into(),
        });
        assert_eq!(
            &out[..],
            &[0xE3, 0x07, 0x00, 0x01, 0x01, 0x62, 0x00, 0x01, 0x61, 0x00]
        );
    }

    #[test]
    fn env_change_packet_size() {
        let out = encode_change(EnvChange::PacketSize {
            old: 4096,
            new: 4096,
        });
        assert_eq!(
            &out[..],
            &[
                0xE3, 0x13, 0x00, 0x04, // TokenType, Length = 19, Type = 4
                0x04, 0x34, 0x00, 0x30, 0x00, 0x39, 0x00, 0x36, 0x00, // NewValue "4096"
                0x04, 0x34, 0x00, 0x30, 0x00, 0x39, 0x00, 0x36, 0x00, // OldValue "4096"
            ]
        );
    }

    #[test]
    fn env_change_collation() {
        let out = encode_change(EnvChange::Collation {
            old: None,
            new: Collation::DEFAULT,
        });
        assert_eq!(
            &out[..],
            &[
                0xE3, 0x08, 0x00, 0x07, 0x05, 0x09, 0x04, 0xD0, 0x00, 0x34, 0x00
            ]
        );

        let out = encode_change(EnvChange::Collation {
            old: Some(Collation::DEFAULT),
            new: Collation {
                lcid: 0x040C,
                flags: 0x00,
                version: 2,
                sort_id: 0,
            },
        });
        assert_eq!(
            &out[..],
            &[
                0xE3, 0x0D, 0x00, 0x07, // Length = 13
                0x05, 0x0C, 0x04, 0x00, 0x20, 0x00, // NewValue: LCID 0x040C, version 2
                0x05, 0x09, 0x04, 0xD0, 0x00, 0x34, // OldValue: SQL_Latin1_General_CP1_CI_AS
            ]
        );
    }

    #[test]
    fn collation_fields_are_masked() {
        let bytes = collation_bytes(Collation {
            lcid: 0xFFFF_FFFF,
            flags: 0x00,
            version: 0x10,
            sort_id: 1,
        });
        assert_eq!(bytes, [0xFF, 0xFF, 0x0F, 0x00, 0x01]);
    }

    #[test]
    fn env_change_language() {
        let out = encode_change(EnvChange::Language {
            old: String::new(),
            new: "us_english".into(),
        });
        let mut expected = vec![0xE3, 0x17, 0x00, 0x02, 0x0A];
        for unit in "us_english".encode_utf16() {
            expected.extend_from_slice(&unit.to_le_bytes());
        }
        expected.push(0x00);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn env_change_transactions() {
        let descriptor = 0x0102_0304_0506_0708_u64;
        let descriptor_le = [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];

        let out = encode_change(EnvChange::BeginTransaction(descriptor));
        let mut expected = vec![0xE3, 0x0B, 0x00, 0x08, 0x08];
        expected.extend_from_slice(&descriptor_le);
        expected.push(0x00);
        assert_eq!(&out[..], &expected[..]);

        let out = encode_change(EnvChange::CommitTransaction(descriptor));
        let mut expected = vec![0xE3, 0x0B, 0x00, 0x09, 0x00, 0x08];
        expected.extend_from_slice(&descriptor_le);
        assert_eq!(&out[..], &expected[..]);

        let out = encode_change(EnvChange::RollbackTransaction(descriptor));
        let mut expected = vec![0xE3, 0x0B, 0x00, 0x0A, 0x00, 0x08];
        expected.extend_from_slice(&descriptor_le);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn env_change_name_too_long_writes_nothing() {
        let token = Token::EnvChange(EnvChange::Database {
            old: String::new(),
            new: "d".repeat(256),
        });
        let mut out = BytesMut::new();
        assert!(matches!(
            encode(&token, &mut EncodeContext::default(), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
    }
}
