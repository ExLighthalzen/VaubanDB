//! [MS-TDS] 2.2.7, tokens DONE, DONEPROC, DONEINPROC: encoder.

use bytes::{BufMut, BytesMut};

use super::{DoneStatus, EncodeContext, Token};
use crate::error::TdsError;

/// `TokenType` of DONE.
const TOKEN_DONE: u8 = 0xFD;
/// `TokenType` of DONEPROC.
const TOKEN_DONEPROC: u8 = 0xFE;
/// `TokenType` of DONEINPROC.
const TOKEN_DONEINPROC: u8 = 0xFF;

/// Encodes `Token::Done`, `Token::DoneProc` or `Token::DoneInProc` into `out`.
///
/// Layout ([MS-TDS] 2.2.7, DONE / DONEPROC / DONEINPROC, TDS 7.2 and later): `TokenType`,
/// `Status` (USHORT), `CurCmd` (USHORT), `DoneRowCount` (ULONGLONG). No `Length` field.
///
/// `row_count` drives `DONE_COUNT`: `Some(n)` sets the bit and writes `n`; `None` clears
/// the bit and writes 0, whatever `status` says.
pub(crate) fn encode(
    token: &Token,
    _ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let (token_type, status, cur_cmd, row_count) = match token {
        Token::Done {
            status,
            cur_cmd,
            row_count,
        } => (TOKEN_DONE, *status, *cur_cmd, *row_count),
        Token::DoneProc {
            status,
            cur_cmd,
            row_count,
        } => (TOKEN_DONEPROC, *status, *cur_cmd, *row_count),
        Token::DoneInProc {
            status,
            cur_cmd,
            row_count,
        } => (TOKEN_DONEINPROC, *status, *cur_cmd, *row_count),
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        _ => unreachable!("done::encode called with a token other than Done*"),
    };

    let (status, count) = match row_count {
        Some(count) => (status | DoneStatus::COUNT, count),
        None => (DoneStatus(status.0 & !DoneStatus::COUNT.0), 0),
    };

    out.put_u8(token_type);
    out.put_u16_le(status.0);
    out.put_u16_le(cur_cmd);
    out.put_u64_le(count);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_one(token: &Token) -> BytesMut {
        let mut out = BytesMut::new();
        encode(token, &mut EncodeContext::default(), &mut out).unwrap();
        out
    }

    #[test]
    fn done_vectors() {
        let out = encode_one(&Token::Done {
            status: DoneStatus::COUNT,
            cur_cmd: 0xC1,
            row_count: Some(1),
        });
        assert_eq!(
            &out[..],
            &[
                0xFD, 0x10, 0x00, 0xC1, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
            ]
        );

        // `None` clears DONE_COUNT even when `status` carries it, and writes a zero count.
        let out = encode_one(&Token::Done {
            status: DoneStatus::COUNT,
            cur_cmd: 0xC1,
            row_count: None,
        });
        assert_eq!(
            &out[..],
            &[
                0xFD, 0x00, 0x00, 0xC1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
            ]
        );

        // `Some` sets DONE_COUNT even when `status` lacks it.
        let out = encode_one(&Token::Done {
            status: DoneStatus::FINAL,
            cur_cmd: 0xC1,
            row_count: Some(0x0102_0304_0506_0708),
        });
        assert_eq!(
            &out[..],
            &[
                0xFD, 0x10, 0x00, 0xC1, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01
            ]
        );

        // DONEINPROC and DONEPROC: same body, different `TokenType`.
        let body = [
            0x10, 0x00, 0xC1, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let out = encode_one(&Token::DoneInProc {
            status: DoneStatus::COUNT,
            cur_cmd: 0xC1,
            row_count: Some(1),
        });
        assert_eq!(out[0], 0xFF);
        assert_eq!(&out[1..], &body[..]);
        let out = encode_one(&Token::DoneProc {
            status: DoneStatus::COUNT,
            cur_cmd: 0xC1,
            row_count: Some(1),
        });
        assert_eq!(out[0], 0xFE);
        assert_eq!(&out[1..], &body[..]);

        // Combined bits, little-endian.
        let out = encode_one(&Token::Done {
            status: DoneStatus::MORE | DoneStatus::ERROR,
            cur_cmd: 0,
            row_count: None,
        });
        assert_eq!(&out[1..3], &[0x03, 0x00]);
        let out = encode_one(&Token::Done {
            status: DoneStatus::SRVERROR | DoneStatus::INXACT,
            cur_cmd: 0,
            row_count: None,
        });
        assert_eq!(&out[1..3], &[0x04, 0x01]);
    }
}
