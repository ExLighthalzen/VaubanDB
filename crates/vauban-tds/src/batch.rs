//! [MS-TDS] 2.2.6.7 SQL Batch: ALL_HEADERS then the UCS-2 text of the batch.
//!
//! # SQL_BATCH
//!
//! The payload of a SQL_BATCH message (packet type 0x01) is an ALL_HEADERS block
//! ([MS-TDS] 2.2.5.3, mandatory since TDS 7.2) followed by `SQLText`: the raw T-SQL of
//! the batch in UCS-2 little-endian, with neither a length prefix nor a terminator, up
//! to the end of the message. The client has already stripped the `GO` separators of
//! `sqlcmd`. A message with no text after the headers is an empty batch, not an error.
//!
//! # ATTENTION
//!
//! An ATTENTION message ([MS-TDS] 2.2.1.7 Attention; 2.2.3.1.1 Type 0x06) is a single
//! header-only packet with the EOM bit set and no payload. `decode_client_message`
//! turns it into `ClientMessage::Attention`; there is nothing to decode here.
//!
//! What the session owes in return ([MS-TDS] 3.3.5.2, Attention handling): it stops
//! the request in progress, discards whatever response it had not yet flushed, and
//! sends **one** DONE token whose status carries `DoneStatus::ATTN` (`DONE_ATTN`,
//! 0x0020), with `CurCmd` 0 and no row count. Nothing else goes out for that request:
//! no ERROR, no INFO, no further result. The client itself ignores everything it
//! receives until that DONE. An ATTENTION that arrives while the server is still
//! writing a response is handled by `session`, not here.

use crate::ResetConnection;
use crate::error::TdsError;
use crate::headers::decode_all_headers;

/// Decoded SQL_BATCH message ([MS-TDS] 2.2.6.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBatch {
    /// `SQLText`, decoded from UCS-2.
    pub text: String,
    /// `TransactionDescriptor` of the Transaction Descriptor header ([MS-TDS] 2.2.5.3.1);
    /// 0 when absent.
    pub transaction_descriptor: u64,
    /// RESETCONNECTION or RESETCONNECTIONSKIPTRAN from the first packet's status
    /// ([MS-TDS] 2.2.3.1.2).
    pub reset: ResetConnection,
}

/// Decodes the payload of a SQL_BATCH packet ([MS-TDS] 2.2.6.7): ALL_HEADERS, then
/// `SQLText` in UTF-16LE up to the end of the payload.
///
/// An odd number of text bytes is a protocol error (`Malformed`). An invalid surrogate
/// pair is not: it is replaced by U+FFFD and left to the parser, which reports the
/// error the way SQL Server would. A payload with no ALL_HEADERS (TDS 7.1 and older)
/// is refused by `decode_all_headers`.
pub(crate) fn decode(payload: &[u8]) -> Result<SqlBatch, TdsError> {
    let (headers, text) = decode_all_headers(payload)?;
    let (pairs, odd_byte) = text.as_chunks::<2>();
    if !odd_byte.is_empty() {
        return Err(TdsError::Malformed(
            "SQL_BATCH text has an odd number of bytes",
        ));
    }
    let units: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();
    Ok(SqlBatch {
        text: String::from_utf16_lossy(&units),
        transaction_descriptor: headers.transaction_descriptor,
        reset: ResetConnection::None,
    })
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;
    use crate::message::{ClientMessage, decode_client_message};
    use crate::packet::{PacketType, read_message, write_message};

    /// The 22-byte minimal ALL_HEADERS block: Transaction Descriptor 0, one request.
    const HEADERS: [u8; 22] = [
        0x16, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    ];

    /// Encodes `text` in UTF-16LE.
    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn decode_select_1() {
        let mut payload = HEADERS.to_vec();
        payload.extend_from_slice(&[
            0x53, 0x00, 0x45, 0x00, 0x4C, 0x00, 0x45, 0x00, 0x43, 0x00, 0x54, 0x00, 0x20, 0x00,
            0x31, 0x00,
        ]);
        assert_eq!(payload.len(), 38);
        let batch = decode(&payload).unwrap();
        assert_eq!(batch.text, "SELECT 1");
        assert_eq!(batch.transaction_descriptor, 0);
    }

    #[test]
    fn decode_with_transaction_descriptor() {
        let mut payload = HEADERS.to_vec();
        payload[10..18].copy_from_slice(&[0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]);
        payload.extend_from_slice(&utf16le("COMMIT"));
        let batch = decode(&payload).unwrap();
        assert_eq!(batch.transaction_descriptor, 0x0123_4567_89AB_CDEF);
        assert_eq!(batch.text, "COMMIT");
    }

    #[test]
    fn decode_rejects_odd_text_length() {
        let mut payload = HEADERS.to_vec();
        payload.extend_from_slice(&utf16le("SELECT 1"));
        payload.push(0x32);
        assert!(matches!(decode(&payload), Err(TdsError::Malformed(_))));

        // One lone byte is odd too.
        let mut payload = HEADERS.to_vec();
        payload.push(0x53);
        assert!(matches!(decode(&payload), Err(TdsError::Malformed(_))));
    }

    #[test]
    fn decode_empty_batch() {
        let batch = decode(&HEADERS).unwrap();
        assert_eq!(batch.text, "");
        assert_eq!(batch.transaction_descriptor, 0);
    }

    #[test]
    fn decode_keeps_non_ascii_and_replaces_lone_surrogates() {
        let mut payload = HEADERS.to_vec();
        payload.extend_from_slice(&utf16le("SELECT N'été 😀'"));
        assert_eq!(decode(&payload).unwrap().text, "SELECT N'été 😀'");

        // A high surrogate with nothing after it is not a protocol error.
        let mut payload = HEADERS.to_vec();
        payload.extend_from_slice(&utf16le("N'"));
        payload.extend_from_slice(&0xD83Du16.to_le_bytes());
        payload.extend_from_slice(&utf16le("'"));
        assert_eq!(decode(&payload).unwrap().text, "N'\u{FFFD}'");
    }

    #[test]
    fn decode_rejects_payload_without_headers() {
        assert!(matches!(
            decode(&utf16le("SELECT 1")),
            Err(TdsError::Malformed(_))
        ));
        assert!(matches!(decode(&[]), Err(TdsError::Malformed(_))));
    }

    #[test]
    fn dispatch_reaches_batch_decoder() {
        let mut payload = HEADERS.to_vec();
        payload.extend_from_slice(&utf16le("SELECT 1"));
        match decode_client_message(PacketType::SqlBatch, &payload, ResetConnection::None) {
            Ok(ClientMessage::SqlBatch(batch)) => assert_eq!(batch.text, "SELECT 1"),
            other => panic!("expected SqlBatch, got {other:?}"),
        }
    }

    #[test]
    fn attention_is_dispatched() {
        assert!(matches!(
            decode_client_message(PacketType::Attention, &[], ResetConnection::None),
            Ok(ClientMessage::Attention)
        ));
    }

    #[tokio::test]
    async fn decode_multi_packet_text() {
        let text: String = "SELECT 'x' -- "
            .chars()
            .chain(std::iter::repeat('a'))
            .take(5000)
            .collect();
        assert_eq!(text.chars().count(), 5000);
        let mut payload = HEADERS.to_vec();
        payload.extend_from_slice(&utf16le(&text));

        let (mut client, mut server) = duplex(1 << 16);
        let mut next_packet_id = 1u8;
        write_message(
            &mut client,
            PacketType::SqlBatch,
            &payload,
            4096,
            &mut next_packet_id,
        )
        .await
        .unwrap();
        assert!(
            next_packet_id > 2,
            "5000 UCS-2 chars must span several packets"
        );

        let message = read_message(&mut server, 1 << 20).await.unwrap();
        assert_eq!(message.kind, PacketType::SqlBatch);
        let batch = decode(&message.payload).unwrap();
        assert_eq!(batch.text, text);
        assert_eq!(batch.transaction_descriptor, 0);
    }
}
