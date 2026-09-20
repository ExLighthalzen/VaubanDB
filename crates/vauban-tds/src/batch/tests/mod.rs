use tokio::io::duplex;

use super::*;
use crate::message::{ClientMessage, decode_client_message};
use crate::packet::{PacketType, read_message, write_message};

/// The 22-byte minimal ALL_HEADERS block: Transaction Descriptor 0, one request.
const HEADERS: [u8; 22] = [
    0x16, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
];

/// Encodes `text` in UTF-16LE.
fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[test]
fn decode_select_1() {
    let mut payload = HEADERS.to_vec();
    payload.extend_from_slice(&[
        0x53, 0x00, 0x45, 0x00, 0x4C, 0x00, 0x45, 0x00, 0x43, 0x00, 0x54, 0x00, 0x20, 0x00, 0x31,
        0x00,
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
