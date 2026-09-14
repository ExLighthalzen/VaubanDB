//! [MS-TDS] 2.2.7, token ORDER: encoder.
//!
//! Layout: `TokenType` (0xA9), `Length` (USHORT, number of bytes that follow), then one
//! `ColNum` (USHORT, 1-based ordinal in the COLMETADATA) per column of the ORDER BY clause.

use bytes::{BufMut, BytesMut};

use super::{EncodeContext, Token};
use crate::error::TdsError;

/// `TokenType` of ORDER.
const TOKEN_ORDER: u8 = 0xA9;

/// Encodes `Token::Order` into `out`. More than 32767 ordinals do not fit in `Length` and
/// fail with `Malformed`; nothing is written in that case.
pub(crate) fn encode(
    token: &Token,
    _ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let Token::Order(ordinals) = token else {
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        unreachable!("order::encode called with a token other than Order");
    };

    let length = u16::try_from(ordinals.len() * 2)
        .map_err(|_| TdsError::Malformed("ORDER with more than 32767 columns"))?;
    out.put_u8(TOKEN_ORDER);
    out.put_u16_le(length);
    for ordinal in ordinals {
        out.put_u16_le(*ordinal);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(ordinals: Vec<u16>) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode(
            &Token::Order(ordinals),
            &mut EncodeContext::default(),
            &mut out,
        )
        .unwrap();
        out.to_vec()
    }

    #[test]
    fn order_vector() {
        assert_eq!(
            order(vec![1, 2]),
            [0xA9, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00]
        );
        assert_eq!(order(vec![0x0102]), [0xA9, 0x02, 0x00, 0x02, 0x01]);
        assert_eq!(order(vec![]), [0xA9, 0x00, 0x00]);
    }

    #[test]
    fn order_too_long() {
        let mut out = BytesMut::new();
        let token = Token::Order(vec![1; 32768]);
        assert!(matches!(
            encode(&token, &mut EncodeContext::default(), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
        assert_eq!(order(vec![1; 32767]).len(), 3 + 2 * 32767);
    }
}
