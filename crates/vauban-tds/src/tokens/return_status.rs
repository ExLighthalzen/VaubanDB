//! [MS-TDS] 2.2.7, token RETURNSTATUS: encoder.
//!
//! Layout: `TokenType` (0x79) then `Value` (LONG, little-endian): the value of the `RETURN`
//! statement of a stored procedure, 0 by default. SQL Server sends it once per RPC, after
//! the result sets and before the RETURNVALUE tokens and the closing DONEPROC (module
//! documentation of `rpc.rs`).

use bytes::{BufMut, BytesMut};

use super::{EncodeContext, Token};
use crate::error::TdsError;

/// `TokenType` of RETURNSTATUS.
const TOKEN_RETURNSTATUS: u8 = 0x79;

/// Encodes `Token::ReturnStatus` into `out`.
pub(crate) fn encode(
    token: &Token,
    _ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let Token::ReturnStatus(status) = token else {
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        unreachable!("return_status::encode called with a token other than ReturnStatus");
    };
    out.put_u8(TOKEN_RETURNSTATUS);
    out.put_i32_le(*status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_one(token: &Token) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode(token, &mut EncodeContext::default(), &mut out).unwrap();
        out.to_vec()
    }

    #[test]
    fn return_status_vector() {
        assert_eq!(
            encode_one(&Token::ReturnStatus(0)),
            [0x79, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            encode_one(&Token::ReturnStatus(-1)),
            [0x79, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            encode_one(&Token::ReturnStatus(0x0102_0304)),
            [0x79, 0x04, 0x03, 0x02, 0x01]
        );
    }

    #[test]
    fn return_status_appends_after_existing_bytes() {
        let mut out = BytesMut::from(&[0xAA][..]);
        encode(
            &Token::ReturnStatus(7),
            &mut EncodeContext::default(),
            &mut out,
        )
        .unwrap();
        assert_eq!(&out[..], &[0xAA, 0x79, 0x07, 0x00, 0x00, 0x00]);
    }
}
