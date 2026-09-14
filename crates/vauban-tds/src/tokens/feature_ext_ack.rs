//! [MS-TDS] 2.2.7, token FEATUREEXTACK: encoder.

use bytes::{BufMut, BytesMut};

use super::{EncodeContext, Token};
use crate::error::TdsError;

/// `TokenType` of FEATUREEXTACK.
const TOKEN_TYPE: u8 = 0xAE;
/// `FeatureId` TERMINATOR: closes the list of acknowledged features.
const TERMINATOR: u8 = 0xFF;

/// Encodes `Token::FeatureExtAck` into `out`.
///
/// Layout ([MS-TDS] 2.2.7, FEATUREEXTACK): `TokenType`, then for each acknowledged feature
/// `FeatureId` (BYTE), `FeatureAckDataLen` (DWORD) and `FeatureAckData`, then the
/// `TERMINATOR` byte `0xFF`. There is no `Length` field. A `FeatureId` of `0xFF` is
/// refused: it would end the list early.
pub(crate) fn encode(
    token: &Token,
    _ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    let Token::FeatureExtAck(acks) = token else {
        // `encode_tokens` dispatches on the variant; no other token reaches this file.
        unreachable!("feature_ext_ack::encode called with a token other than FeatureExtAck");
    };

    // Built apart so that a failure leaves `out` untouched.
    let mut body = BytesMut::new();
    for ack in acks {
        if ack.id == TERMINATOR {
            return Err(TdsError::Malformed(
                "FEATUREEXTACK FeatureId 0xFF is the terminator",
            ));
        }
        let len = u32::try_from(ack.data.len())
            .map_err(|_| TdsError::Malformed("FEATUREEXTACK data longer than 4 GiB"))?;
        body.put_u8(ack.id);
        body.put_u32_le(len);
        body.put_slice(&ack.data);
    }

    out.put_u8(TOKEN_TYPE);
    out.put_slice(&body);
    out.put_u8(TERMINATOR);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::FeatureAck;
    use super::*;

    #[test]
    fn feature_ext_ack_vectors() {
        let mut out = BytesMut::new();
        let token = Token::FeatureExtAck(vec![FeatureAck {
            id: 0x0A,
            data: vec![1],
        }]);
        encode(&token, &mut EncodeContext::default(), &mut out).unwrap();
        assert_eq!(&out[..], &[0xAE, 0x0A, 0x01, 0x00, 0x00, 0x00, 0x01, 0xFF]);

        let mut out = BytesMut::new();
        encode(
            &Token::FeatureExtAck(vec![]),
            &mut EncodeContext::default(),
            &mut out,
        )
        .unwrap();
        assert_eq!(&out[..], &[0xAE, 0xFF]);

        // Several acks, one with empty data, in order.
        let mut out = BytesMut::new();
        let token = Token::FeatureExtAck(vec![
            FeatureAck {
                id: 0x04,
                data: vec![],
            },
            FeatureAck {
                id: 0x0A,
                data: vec![1, 2],
            },
        ]);
        encode(&token, &mut EncodeContext::default(), &mut out).unwrap();
        assert_eq!(
            &out[..],
            &[
                0xAE, // TokenType
                0x04, 0x00, 0x00, 0x00, 0x00, // id 4, no data
                0x0A, 0x02, 0x00, 0x00, 0x00, 0x01, 0x02, // id 10, two bytes
                0xFF, // TERMINATOR
            ]
        );
    }

    #[test]
    fn feature_ext_ack_refuses_terminator_id() {
        let mut out = BytesMut::new();
        let token = Token::FeatureExtAck(vec![FeatureAck {
            id: 0xFF,
            data: vec![],
        }]);
        assert!(matches!(
            encode(&token, &mut EncodeContext::default(), &mut out),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
    }
}
