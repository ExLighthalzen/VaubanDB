use super::*;

const SQL_BATCH_EOM_HEADER: [u8; 8] = [0x01, 0x01, 0x00, 0x1E, 0x00, 0x00, 0x01, 0x00];

#[test]
fn header_roundtrip() {
    let header = PacketHeader::decode(&SQL_BATCH_EOM_HEADER).unwrap();
    assert_eq!(header.kind, PacketType::SqlBatch);
    assert!(header.status.contains(PacketStatus::EOM));
    assert_eq!(header.length, 30);
    assert_eq!(header.spid, 0);
    assert_eq!(header.packet_id, 1);
    assert_eq!(header.window, 0);
    assert_eq!(header.encode(), SQL_BATCH_EOM_HEADER);
}

#[test]
fn header_rejects_bad_length() {
    let short = [0x01, 0x01, 0x00, 0x07, 0x00, 0x00, 0x01, 0x00];
    assert!(matches!(
        PacketHeader::decode(&short),
        Err(TdsError::Malformed(_))
    ));
    let long = [0x01, 0x01, 0x80, 0x00, 0x00, 0x00, 0x01, 0x00]; // 32768
    assert!(matches!(
        PacketHeader::decode(&long),
        Err(TdsError::Malformed(_))
    ));
    assert!(matches!(
        PacketHeader::decode(&SQL_BATCH_EOM_HEADER[..7]),
        Err(TdsError::Malformed(_))
    ));
    let exact_min = [0x01, 0x01, 0x00, 0x08, 0x00, 0x00, 0x01, 0x00];
    assert!(PacketHeader::decode(&exact_min).is_ok());
    let exact_max = [0x01, 0x01, 0x7F, 0xFF, 0x00, 0x00, 0x01, 0x00];
    assert!(PacketHeader::decode(&exact_max).is_ok());
}

#[test]
fn packet_type_roundtrip() {
    for byte in 0u8..=255 {
        assert_eq!(PacketType::from_u8(byte).to_u8(), byte);
    }
    assert_eq!(PacketType::from_u8(0x07), PacketType::BulkLoad);
    assert_eq!(PacketType::from_u8(0x05), PacketType::Unknown(0x05));
}

#[test]
fn split_two_packets() {
    let packets = split_message(PacketType::TabularResult, &[0u8; 5000], 4096);
    assert_eq!(packets.len(), 2);
    assert_eq!(
        &packets[0][..8],
        &[0x04, 0x00, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00]
    );
    assert_eq!(packets[0].len(), 4096);
    assert_eq!(
        &packets[1][..8],
        &[0x04, 0x01, 0x03, 0x98, 0x00, 0x00, 0x02, 0x00]
    );
    assert_eq!(packets[1].len(), 920);
}

#[test]
fn split_exact_multiple_has_eom_only_on_last() {
    let packets = split_message(PacketType::TabularResult, &[7u8; 4088 * 2], 4096);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0][1], 0x00);
    assert_eq!(packets[1][1], 0x01);
}

#[test]
fn split_empty_payload() {
    let packets = split_message(PacketType::Attention, &[], 4096);
    assert_eq!(packets.len(), 1);
    assert_eq!(
        &packets[0][..],
        &[0x06, 0x01, 0x00, 0x08, 0x00, 0x00, 0x01, 0x00]
    );
}

#[test]
fn split_with_wraps_packet_id_and_sets_spid() {
    let mut id = 0xFFu8;
    let packets = split_message_with(PacketType::TabularResult, &[0u8; 600], 512, 0x1234, &mut id);
    assert_eq!(packets.len(), 2);
    assert_eq!(&packets[0][4..7], &[0x12, 0x34, 0xFF]);
    assert_eq!(&packets[1][4..7], &[0x12, 0x34, 0x00]);
    assert_eq!(id, 1);
}

/// Builds one raw packet from its header fields and body.
fn packet(kind: PacketType, status: u8, body: &[u8]) -> Vec<u8> {
    let header = PacketHeader {
        kind,
        status: PacketStatus(status),
        length: (HEADER_LEN + body.len()) as u16,
        spid: 0,
        packet_id: 1,
        window: 0,
    };
    let mut out = header.encode().to_vec();
    out.extend_from_slice(body);
    out
}

#[tokio::test]
async fn read_message_reassembles() {
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    client
        .write_all(&packet(PacketType::SqlBatch, 0x00, b"abc"))
        .await
        .unwrap();
    client
        .write_all(&packet(PacketType::SqlBatch, 0x00, b"def"))
        .await
        .unwrap();
    client
        .write_all(&packet(PacketType::SqlBatch, 0x01, b"gh"))
        .await
        .unwrap();
    let message = read_message(&mut server, 1 << 20).await.unwrap();
    assert_eq!(message.kind, PacketType::SqlBatch);
    assert_eq!(&message.payload[..], b"abcdefgh");
    assert_eq!(message.reset, ResetConnection::None);
}

#[tokio::test]
async fn reset_bit_on_first_packet_is_reported() {
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    // 0x08 → Full
    client
        .write_all(&packet(PacketType::Rpc, 0x09, b"full"))
        .await
        .unwrap();
    let message = read_message(&mut server, 1 << 20).await.unwrap();
    assert_eq!(message.reset, ResetConnection::Full);

    // 0x10 → SkipTransaction
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    client
        .write_all(&packet(PacketType::Rpc, 0x11, b"skip"))
        .await
        .unwrap();
    let message = read_message(&mut server, 1 << 20).await.unwrap();
    assert_eq!(message.reset, ResetConnection::SkipTransaction);

    // `ResetConnection::None` when neither bit is set
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    client
        .write_all(&packet(PacketType::Rpc, 0x01, b"none"))
        .await
        .unwrap();
    let message = read_message(&mut server, 1 << 20).await.unwrap();
    assert_eq!(message.reset, ResetConnection::None);

    // both bits → SkipTransaction (RESETCONNECTIONSKIPTRAN implies RESETCONNECTION)
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    client
        .write_all(&packet(PacketType::Rpc, 0x19, b"both"))
        .await
        .unwrap();
    let message = read_message(&mut server, 1 << 20).await.unwrap();
    assert_eq!(message.reset, ResetConnection::SkipTransaction);
}

#[tokio::test]
async fn reset_bit_on_a_later_packet_is_ignored() {
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    // First packet: no reset bit. Second packet: RESETCONNECTION (0x08) + EOM (0x01).
    // The first packet's status decides the reset value.
    client
        .write_all(&packet(PacketType::SqlBatch, 0x00, b"first"))
        .await
        .unwrap();
    client
        .write_all(&packet(PacketType::SqlBatch, 0x09, b"second"))
        .await
        .unwrap();
    let message = read_message(&mut server, 1 << 20).await.unwrap();
    assert_eq!(message.kind, PacketType::SqlBatch);
    assert_eq!(&message.payload[..], b"firstsecond");
    assert_eq!(message.reset, ResetConnection::None);
}

#[tokio::test]
async fn read_message_rejects_mixed_types() {
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    client
        .write_all(&packet(PacketType::SqlBatch, 0x00, b"abc"))
        .await
        .unwrap();
    client
        .write_all(&packet(PacketType::Rpc, 0x01, b"def"))
        .await
        .unwrap();
    assert!(matches!(
        read_message(&mut server, 1 << 20).await,
        Err(TdsError::Malformed(_))
    ));
}

#[tokio::test]
async fn read_message_enforces_max_size() {
    // Only the header is sent: a reader that allocated or read the body first would
    // block or see EOF instead of failing on the announced size.
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    let mut header = packet(PacketType::SqlBatch, 0x01, &[]);
    header[2..4].copy_from_slice(&1008u16.to_be_bytes());
    client.write_all(&header).await.unwrap();
    drop(client);
    assert!(matches!(
        read_message(&mut server, 500).await,
        Err(TdsError::MessageTooLarge(1000))
    ));

    // Accumulated size across packets counts too.
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    client
        .write_all(&packet(PacketType::SqlBatch, 0x00, &[0u8; 300]))
        .await
        .unwrap();
    client
        .write_all(&packet(PacketType::SqlBatch, 0x01, &[0u8; 300]))
        .await
        .unwrap();
    assert!(matches!(
        read_message(&mut server, 500).await,
        Err(TdsError::MessageTooLarge(600))
    ));
}

#[tokio::test]
async fn read_message_skips_ignored() {
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    client
        .write_all(&packet(PacketType::SqlBatch, 0x00, b"dropped"))
        .await
        .unwrap();
    client
        .write_all(&packet(PacketType::SqlBatch, 0x03, b"too"))
        .await
        .unwrap();
    client
        .write_all(&packet(PacketType::Rpc, 0x01, b"kept"))
        .await
        .unwrap();
    let message = read_message(&mut server, 1 << 20).await.unwrap();
    assert_eq!(message.kind, PacketType::Rpc);
    assert_eq!(&message.payload[..], b"kept");
}

#[tokio::test]
async fn read_message_reports_closed_connection() {
    let (client, mut server) = tokio::io::duplex(1 << 16);
    drop(client);
    assert!(matches!(
        read_message(&mut server, 1 << 20).await,
        Err(TdsError::ConnectionClosed)
    ));
}

#[tokio::test]
async fn write_message_roundtrips_through_read_message() {
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    let payload = vec![0xABu8; 1000];
    let mut next_id = 1u8;
    write_message(
        &mut server,
        PacketType::TabularResult,
        &payload,
        512,
        &mut next_id,
    )
    .await
    .unwrap();
    assert_eq!(next_id, 3);
    let message = read_message(&mut client, 1 << 20).await.unwrap();
    assert_eq!(message.kind, PacketType::TabularResult);
    assert_eq!(message.payload, payload);
}
