//! [MS-TDS] 2.2.7, token LOGINACK: encoder.

use bytes::{BufMut, BytesMut};

use super::{EncodeContext, Token, put_b_varchar};
use crate::error::TdsError;

/// `TokenType` of LOGINACK.
const TOKEN_TYPE: u8 = 0xAD;
/// `Interface` value SQL_TSQL: the server speaks T-SQL.
const INTERFACE_SQL_TSQL: u8 = 0x01;

/// Encodes `Token::LoginAck` into `out`.
///
/// Layout ([MS-TDS] 2.2.7, LOGINACK): `TokenType`, `Length` (USHORT, number of bytes after
/// it), `Interface` (BYTE), `TDSVersion` (DWORD), `ProgName` (B_VARCHAR), then `MajorVer`,
/// `MinorVer`, `BuildNumHi`, `BuildNumLow` (one BYTE each).
///
/// `TDSVersion` is written most significant byte first (`74 00 00 04` for 7.4), the reverse
/// of the little-endian `TDSVersion` of LOGIN7: this is what the "Login Response" example of
/// [MS-TDS] chapter 4 shows (`72 09 00 02` for 7.2).
pub(crate) fn encode(
    token: &Token,
    _ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let Token::LoginAck {
        tds_version,
        program_name,
        version,
    } = token
    else {
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        unreachable!("login_ack::encode called with a token other than Token::LoginAck");
    };

    // Built apart so that a failure leaves `out` untouched.
    let mut body = BytesMut::with_capacity(9 + 2 * program_name.len());
    body.put_u8(INTERFACE_SQL_TSQL);
    body.put_u32(*tds_version);
    put_b_varchar(&mut body, program_name)?;
    body.put_slice(version);

    // `ProgName` is a B_VARCHAR: the body never exceeds 9 + 510 bytes.
    let len = u16::try_from(body.len())
        .map_err(|_| TdsError::Malformed("LOGINACK longer than 65535 bytes"))?;
    out.put_u8(TOKEN_TYPE);
    out.put_u16_le(len);
    out.put_slice(&body);
    Ok(())
}

#[cfg(test)]
mod tests {
    use vauban_errors::InfoMessage;

    use super::super::{DoneStatus, EnvChange, encode_tokens};
    use super::*;

    fn login_ack() -> Token {
        Token::LoginAck {
            tds_version: 0x7400_0004,
            program_name: "VaubanDB".into(),
            version: [0x10, 0x00, 0x03, 0xE8],
        }
    }

    #[test]
    fn login_ack_vector() {
        let mut out = BytesMut::new();
        encode(&login_ack(), &mut EncodeContext::default(), &mut out).unwrap();
        let expected: [u8; 29] = [
            0xAD, 0x1A, 0x00, // TokenType, Length = 26
            0x01, // Interface = SQL_TSQL
            0x74, 0x00, 0x00, 0x04, // TDSVersion 7.4
            0x08, // ProgName length (8 chars)
            0x56, 0x00, 0x61, 0x00, 0x75, 0x00, 0x62, 0x00, // "Vaub"
            0x61, 0x00, 0x6E, 0x00, 0x44, 0x00, 0x42, 0x00, // "anDB"
            0x10, 0x00, 0x03, 0xE8, // MajorVer, MinorVer, BuildNumHi, BuildNumLow
        ];
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn login_ack_empty_program_name() {
        let token = Token::LoginAck {
            tds_version: 0x7400_0004,
            program_name: String::new(),
            version: [0, 0, 0, 0],
        };
        let mut out = BytesMut::new();
        encode(&token, &mut EncodeContext::default(), &mut out).unwrap();
        assert_eq!(
            &out[..],
            &[
                0xAD, 0x0A, 0x00, 0x01, 0x74, 0x00, 0x00, 0x04, 0x00, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn login_ack_program_name_too_long_writes_nothing() {
        let token = Token::LoginAck {
            tds_version: 0x7400_0004,
            program_name: "x".repeat(256),
            version: [0, 0, 0, 0],
        };
        let mut out = BytesMut::new();
        assert!(matches!(
            encode(&token, &mut EncodeContext::default(), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn encode_tokens_login_sequence() {
        let tokens = [
            login_ack(),
            Token::EnvChange(EnvChange::Database {
                old: "master".into(),
                new: "master".into(),
            }),
            Token::EnvChange(EnvChange::PacketSize {
                old: 4096,
                new: 4096,
            }),
            Token::Info(InfoMessage {
                number: 5701,
                severity: 0,
                state: 2,
                message: "Changed database context to 'master'.".into(),
                line: 1,
            }),
            Token::Done {
                status: DoneStatus::FINAL,
                cur_cmd: 0,
                row_count: None,
            },
        ];

        let mut ctx = EncodeContext::default();
        let mut all = BytesMut::new();
        encode_tokens(&tokens, &mut ctx, &mut all).unwrap();

        // Same bytes as the concatenation of each token encoded on its own.
        let mut expected = BytesMut::new();
        for token in &tokens {
            let mut ctx = EncodeContext::default();
            encode_tokens(std::slice::from_ref(token), &mut ctx, &mut expected).unwrap();
        }
        assert_eq!(all, expected);

        // Token boundaries: LOGINACK 29, ENVCHANGE 30 and 22, INFO 91, DONE 13 bytes.
        // LOGINACK carries the program name, so its size follows the product name's length.
        assert_eq!(all.len(), 29 + 30 + 22 + 91 + 13);
        assert_eq!(all[0], 0xAD);
        assert_eq!(all[29], 0xE3);
        assert_eq!(all[59], 0xE3);
        assert_eq!(all[81], 0xAB);
        assert_eq!(all[172], 0xFD);
    }
}
