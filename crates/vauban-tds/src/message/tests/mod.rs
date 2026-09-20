use super::*;

#[test]
fn dispatch_attention_and_unsupported() {
    assert!(matches!(
        decode_client_message(PacketType::Attention, &[], ResetConnection::None),
        Ok(ClientMessage::Attention)
    ));
    assert!(matches!(
        decode_client_message(PacketType::Attention, &[0], ResetConnection::None),
        Err(TdsError::Malformed(_))
    ));
    assert!(matches!(
        decode_client_message(PacketType::from_u8(0x07), &[], ResetConnection::None),
        Ok(ClientMessage::Unsupported(0x07))
    ));
    assert!(matches!(
        decode_client_message(PacketType::PreLogin, &[], ResetConnection::None),
        Ok(ClientMessage::Unsupported(0x12))
    ));
    assert!(matches!(
        decode_client_message(PacketType::Unknown(0x55), &[], ResetConnection::None),
        Ok(ClientMessage::Unsupported(0x55))
    ));
    // LOGIN7, SQL_BATCH, RPC and TM are dispatched to their own decoders; a three-byte
    // payload is too short for each of them, so each must fail with a decoding error
    // rather than panic, whether or not the decoder is still a stub.
    for kind in [
        PacketType::Login7,
        PacketType::SqlBatch,
        PacketType::Rpc,
        PacketType::TransactionManager,
    ] {
        assert!(
            decode_client_message(kind, &[1, 2, 3], ResetConnection::None).is_err(),
            "{kind:?} must reject a 3-byte payload"
        );
    }
}

// Building a `Login7` here would couple this test to its field list; the password
// masking itself is checked in `login7.rs` (`debug_never_prints_password`).
#[test]
fn debug_prints_unsupported_kind_in_hex() {
    assert_eq!(
        format!("{:?}", ClientMessage::Unsupported(7)),
        "Unsupported(0x07)"
    );
    assert_eq!(format!("{:?}", ClientMessage::Attention), "Attention");
}

#[test]
fn reset_is_relayed_to_sql_batch_and_rpc() {
    // Minimal ALL_HEADERS (22 bytes) followed by UCS-2 "SELECT 1".
    let headers: [u8; 22] = [
        0x16, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    ];
    let utf16le = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect() };

    let mut payload = headers.to_vec();
    payload.extend_from_slice(&utf16le("SELECT 1"));
    match decode_client_message(PacketType::SqlBatch, &payload, ResetConnection::Full).unwrap() {
        ClientMessage::SqlBatch(b) => assert_eq!(b.reset, ResetConnection::Full),
        other => panic!("expected SqlBatch, got {other:?}"),
    }
}
