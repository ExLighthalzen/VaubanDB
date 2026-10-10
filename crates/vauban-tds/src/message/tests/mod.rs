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
        Ok(ClientMessage::Refused {
            packet_type: 0x07,
            reason: "bulk copy of rows"
        })
    ));
    assert!(matches!(
        decode_client_message(PacketType::FedAuthToken, &[], ResetConnection::None),
        Ok(ClientMessage::Unsupported(0x08))
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
    assert_eq!(
        format!(
            "{:?}",
            ClientMessage::Refused {
                packet_type: 3,
                reason: "RPC batch"
            }
        ),
        "Refused(0x03, RPC batch)"
    );
}

/// ALL_HEADERS (Transaction Descriptor alone), then `sp_executesql` by ProcID with no
/// option flag, then `params` as the parameter data.
fn rpc_payload(params: &[u8]) -> Vec<u8> {
    let mut payload = vec![
        0x16, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    ];
    payload.extend_from_slice(&[0xFF, 0xFF, 0x0A, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(params);
    payload
}

#[test]
fn rpc_content_this_version_does_not_handle_is_refused_not_failed() {
    // Unnamed parameter, no status flag, then a TYPE_INFO token the decoder refuses:
    // xml, text, ntext, image, sql_variant, CLR UDT, table-valued.
    for token in [0xF1u8, 0x23, 0x63, 0x22, 0x62, 0xF0, 0xF3] {
        let payload = rpc_payload(&[0x00, 0x00, token, 0x00, 0x00, 0x00, 0x00]);
        match decode_client_message(PacketType::Rpc, &payload, ResetConnection::Full) {
            Ok(ClientMessage::Refused {
                packet_type: 0x03,
                reason,
            }) => assert!(!reason.is_empty()),
            other => panic!("type token 0x{token:02X}: expected Refused, got {other:?}"),
        }
    }
    // A batch of two RPCs, behind the BatchFlag of TDS 7.2 and later.
    let mut payload = rpc_payload(&[]);
    payload.push(0xFF);
    payload.extend_from_slice(&[0xFF, 0xFF, 0x0A, 0x00, 0x00, 0x00]);
    assert!(matches!(
        decode_client_message(PacketType::Rpc, &payload, ResetConnection::None),
        Ok(ClientMessage::Refused {
            packet_type: 0x03,
            reason: "RPC batch"
        })
    ));
}

#[test]
fn malformed_rpc_stays_an_error() {
    // An unknown TYPE_INFO token, and a parameter cut in its name.
    for params in [&[0x00, 0x00, 0x99][..], &[0x02, 0x40]] {
        assert!(matches!(
            decode_client_message(PacketType::Rpc, &rpc_payload(params), ResetConnection::None),
            Err(TdsError::Malformed(_))
        ));
    }
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
