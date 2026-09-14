//! [MS-TDS] 2.2.7, tokens INFO and ERROR (same layout): encoder.

use bytes::{BufMut, BytesMut};

use super::{EncodeContext, Token, put_b_varchar, put_us_varchar};
use crate::error::TdsError;

/// `TokenType` of ERROR.
const TOKEN_ERROR: u8 = 0xAA;
/// `TokenType` of INFO.
const TOKEN_INFO: u8 = 0xAB;
/// `ServerName`: empty in V1, `EncodeContext` carries no server name.
const SERVER_NAME: &str = "";

/// The fields shared by INFO and ERROR, borrowed from the token.
struct Fields<'a> {
    token_type: u8,
    number: u32,
    state: u8,
    class: u8,
    message: &'a str,
    procedure: &'a str,
    line: u32,
}

/// Encodes `Token::Info` or `Token::Error` into `out`.
///
/// Layout ([MS-TDS] 2.2.7, INFO and ERROR, TDS 7.2 and later): `TokenType`, `Length`
/// (USHORT, number of bytes after it), `Number` (LONG), `State` (BYTE), `Class` (BYTE),
/// `MsgText` (US_VARCHAR), `ServerName` (B_VARCHAR), `ProcName` (B_VARCHAR), `LineNumber`
/// (LONG). `Class` carries the severity of both `InfoMessage` and `SqlError`.
pub(crate) fn encode(
    token: &Token,
    _ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let fields = match token {
        Token::Info(info) => Fields {
            token_type: TOKEN_INFO,
            number: info.number,
            state: info.state,
            class: info.severity,
            message: &info.message,
            procedure: "",
            line: info.line,
        },
        Token::Error(error) => Fields {
            token_type: TOKEN_ERROR,
            number: error.number,
            state: error.state,
            class: error.severity,
            message: &error.message,
            procedure: error.procedure.as_deref().unwrap_or(""),
            line: error.line,
        },
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        _ => unreachable!("message::encode called with a token other than Info or Error"),
    };

    // Built apart so that a failure leaves `out` untouched.
    let mut body =
        BytesMut::with_capacity(14 + 2 * fields.message.len() + 2 * fields.procedure.len());
    body.put_u32_le(fields.number);
    body.put_u8(fields.state);
    body.put_u8(fields.class);
    put_us_varchar(&mut body, fields.message)?;
    put_b_varchar(&mut body, SERVER_NAME)?;
    put_b_varchar(&mut body, fields.procedure)?;
    body.put_u32_le(fields.line);

    // A US_VARCHAR may hold 65535 code units (131070 bytes), more than `Length` can count.
    let len = u16::try_from(body.len())
        .map_err(|_| TdsError::Malformed("INFO/ERROR longer than 65535 bytes"))?;
    out.put_u8(fields.token_type);
    out.put_u16_le(len);
    out.put_slice(&body);
    Ok(())
}

#[cfg(test)]
mod tests {
    use vauban_errors::{InfoMessage, SqlError};

    use super::*;

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn encode_one(token: &Token) -> BytesMut {
        let mut out = BytesMut::new();
        encode(token, &mut EncodeContext::default(), &mut out).unwrap();
        out
    }

    fn info_5701(severity: u8) -> Token {
        Token::Info(InfoMessage {
            number: 5701,
            severity,
            state: 2,
            message: "Changed database context to 'master'.".into(),
            line: 1,
        })
    }

    #[test]
    fn info_5701_vector() {
        let out = encode_one(&info_5701(0));
        let mut expected = vec![
            0xAB, 0x58, 0x00, // TokenType, Length = 88
            0x45, 0x16, 0x00, 0x00, // Number = 5701
            0x02, // State
            0x00, // Class
            0x25, 0x00, // MsgText length = 37
        ];
        expected.extend_from_slice(&utf16le("Changed database context to 'master'."));
        expected.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(out.len(), 91);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn info_class_is_severity() {
        let out = encode_one(&info_5701(10));
        assert_eq!(out[8], 0x0A);
        // Everything else is unchanged.
        let baseline = encode_one(&info_5701(0));
        assert_eq!(&out[..8], &baseline[..8]);
        assert_eq!(&out[9..], &baseline[9..]);
    }

    #[test]
    fn error_208_vector() {
        let error = SqlError {
            number: 208,
            severity: 16,
            state: 1,
            message: "Invalid object name 'dbo.t'.".into(),
            line: 1,
            procedure: None,
        };
        let out = encode_one(&Token::Error(error.clone()));
        let mut expected = vec![
            0xAA, 0x46, 0x00, // TokenType, Length = 4 + 1 + 1 + 2 + 56 + 1 + 1 + 4 = 70
            0xD0, 0x00, 0x00, 0x00, // Number = 208
            0x01, // State
            0x10, // Class = 16
            0x1C, 0x00, // MsgText length = 28
        ];
        expected.extend_from_slice(&utf16le("Invalid object name 'dbo.t'."));
        expected.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(&out[..], &expected[..]);

        let out = encode_one(&Token::Error(SqlError {
            procedure: Some("p".into()),
            ..error
        }));
        let mut expected = vec![
            0xAA, 0x48, 0x00, 0xD0, 0x00, 0x00, 0x00, 0x01, 0x10, 0x1C, 0x00,
        ];
        expected.extend_from_slice(&utf16le("Invalid object name 'dbo.t'."));
        expected.extend_from_slice(&[0x00, 0x01, 0x70, 0x00, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn message_too_long_writes_nothing() {
        // 40000 code units fit a US_VARCHAR but overflow the token's `Length`.
        let token = Token::Error(SqlError::new(50000, 16, 1, "m".repeat(40000)));
        let mut out = BytesMut::new();
        assert!(matches!(
            encode(&token, &mut EncodeContext::default(), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
    }
}
