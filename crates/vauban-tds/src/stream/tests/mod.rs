use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, DuplexStream, duplex};
use tokio_rustls::TlsConnector;
use tokio_rustls::client;

use super::*;
use crate::prelogin::Encryption;
use crate::tls::test_tls_configs;
use crate::tokens::DoneStatus;

const ENCRYPT_OFF: u8 = 0x00;
const ENCRYPT_ON: u8 = 0x01;
const ENCRYPT_REQ: u8 = 0x03;

/// DONE token (FINAL, cur_cmd 0xC1, no row count), 13 bytes on the wire.
const DONE_BYTES: [u8; 13] = [
    0xFD, 0x00, 0x00, 0xC1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

fn done_token() -> Token {
    Token::Done {
        status: DoneStatus::FINAL,
        cur_cmd: 0xC1,
        row_count: None,
    }
}

/// Minimal client PRELOGIN ([MS-TDS] 2.2.6.5): VERSION 15.0.2000.0 and ENCRYPTION.
fn client_prelogin(encryption: u8) -> Vec<u8> {
    vec![
        0x00, 0x00, 0x0B, 0x00, 0x06, // VERSION @11, 6
        0x01, 0x00, 0x11, 0x00, 0x01, // ENCRYPTION @17, 1
        0xFF, // TERMINATOR
        0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00, // VERSION data
        encryption,
    ]
}

/// Builds one raw packet from its header fields and body.
fn packet(kind: PacketType, status: u8, packet_id: u8, body: &[u8]) -> Vec<u8> {
    let header = PacketHeader {
        kind,
        status: PacketStatus(status),
        length: (HEADER_LEN + body.len()) as u16,
        spid: 0,
        packet_id,
        window: 0,
    };
    let mut out = header.encode().to_vec();
    out.extend_from_slice(body);
    out
}

/// Reads one packet: decoded header and body.
async fn read_packet<R: AsyncRead + Unpin>(r: &mut R) -> (PacketHeader, Vec<u8>) {
    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header).await.unwrap();
    let header = PacketHeader::decode(&header).unwrap();
    let mut body = vec![0u8; usize::from(header.length) - HEADER_LEN];
    r.read_exact(&mut body).await.unwrap();
    (header, body)
}

/// Sends a header-only ATTENTION packet ([MS-TDS] 2.2.1.7).
async fn send_attention<W: AsyncWrite + Unpin>(w: &mut W) {
    w.write_all(&packet(PacketType::Attention, 0x01, 1, &[]))
        .await
        .unwrap();
    w.flush().await.unwrap();
}

/// Client side of the tests: clear text, or TLS over the PRELOGIN adapter.
enum Client {
    Plain(DuplexStream),
    Tls(Box<client::TlsStream<PreloginTlsIo<DuplexStream>>>),
}

impl Client {
    /// Stops TLS without `close_notify`: what a client does after ENCRYPT_OFF.
    fn into_plain(self) -> DuplexStream {
        match self {
            Self::Plain(io) => io,
            Self::Tls(tls) => tls.into_inner().0.into_inner(),
        }
    }
}

impl AsyncRead for Client {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Client {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, data),
            Self::Tls(s) => Pin::new(s).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// TLS 1.2 handshake through a PRELOGIN adapter, trusting any certificate, then
/// passthrough.
async fn client_handshake(io: DuplexStream) -> client::TlsStream<PreloginTlsIo<DuplexStream>> {
    let (_, config) = test_tls_configs();
    let name = ServerName::try_from("localhost").unwrap();
    let mut tls = TlsConnector::from(config)
        .connect(name, PreloginTlsIo::new(io, 4096))
        .await
        .expect("client handshake");
    tls.get_mut().0.set_passthrough();
    tls
}

/// Client side of `accept`: sends a PRELOGIN with `encryption`, checks the ENCRYPTION
/// byte of the response, then does the handshake when `handshake` is set.
async fn connect(
    mut io: DuplexStream,
    encryption: u8,
    expected: Encryption,
    handshake: bool,
) -> Client {
    io.write_all(&packet(
        PacketType::PreLogin,
        0x01,
        1,
        &client_prelogin(encryption),
    ))
    .await
    .unwrap();
    let (header, body) = read_packet(&mut io).await;
    assert_eq!(header.kind, PacketType::TabularResult);
    assert!(header.status.contains(PacketStatus::EOM));
    assert_eq!(header.packet_id, 1);
    assert_eq!(header.spid, 0);
    assert_eq!(prelogin::decode(&body).unwrap().encryption, expected);
    if handshake {
        Client::Tls(Box::new(client_handshake(io).await))
    } else {
        Client::Plain(io)
    }
}

/// Runs `accept_on` against a scripted client over `duplex`.
async fn accept_pair(
    policy: EncryptPolicy,
    with_tls: bool,
    encryption: u8,
    expected: Encryption,
    handshake: bool,
) -> (Result<TdsStream<DuplexStream>, TdsError>, Client) {
    let (server_config, _) = test_tls_configs();
    let (client_io, server_io) = duplex(1 << 16);
    let tls = with_tls.then_some(server_config);
    tokio::join!(
        TdsStream::accept_on(server_io, tls, policy),
        connect(client_io, encryption, expected, handshake),
    )
}

/// A stream in clear text without any PRELOGIN exchange: `PacketID` starts at 1.
fn plain_pair() -> (TdsStream<DuplexStream>, DuplexStream) {
    let (client_io, server_io) = duplex(1 << 16);
    let stream = TdsStream {
        transport: Transport::Plain(server_io),
        mode: TlsMode::None,
        write: WriteState::new(),
        max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
    };
    (stream, client_io)
}

#[tokio::test]
async fn accept_plain_when_not_supported() {
    let (stream, mut client) = accept_pair(
        EncryptPolicy::Off,
        false,
        ENCRYPT_OFF,
        Encryption::NotSup,
        false,
    )
    .await;
    let mut stream = stream.expect("accept");
    assert_eq!(stream.mode, TlsMode::None);
    assert!(matches!(stream.transport, Transport::Plain(_)));

    send_attention(&mut client).await;
    assert!(matches!(
        stream.read_message().await,
        Ok(ClientMessage::Attention)
    ));
}

#[tokio::test]
async fn accept_refuses_when_required_but_unsupported() {
    let (stream, mut client) = accept_pair(
        EncryptPolicy::Off,
        false,
        ENCRYPT_REQ,
        Encryption::NotSup,
        false,
    )
    .await;
    assert!(matches!(stream, Err(TdsError::EncryptionRefused)));
    // The response was the last thing sent: the server closed after it.
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn accept_rejects_non_prelogin_first_packet() {
    let (mut client, server) = duplex(1 << 16);
    client
        .write_all(&packet(PacketType::SqlBatch, 0x01, 1, b"SELECT 1"))
        .await
        .unwrap();
    let result = TdsStream::accept_on(server, None, EncryptPolicy::Off).await;
    assert!(matches!(result, Err(TdsError::UnexpectedPacketType(0x01))));
}

#[tokio::test]
async fn accept_fails_without_tls_config_when_encryption_negotiated() {
    let (mut client, server) = duplex(1 << 16);
    client
        .write_all(&packet(
            PacketType::PreLogin,
            0x01,
            1,
            &client_prelogin(ENCRYPT_ON),
        ))
        .await
        .unwrap();
    let result = TdsStream::accept_on(server, None, EncryptPolicy::Required).await;
    assert!(matches!(result, Err(TdsError::Unsupported(_))));
    // Nothing was promised to the client.
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn accept_full_tls() {
    let (stream, mut client) = accept_pair(
        EncryptPolicy::Required,
        true,
        ENCRYPT_ON,
        Encryption::On,
        true,
    )
    .await;
    let mut stream = stream.expect("accept");
    assert_eq!(stream.mode, TlsMode::Full);
    assert!(matches!(stream.transport, Transport::Tls(_)));

    send_attention(&mut client).await;
    assert!(matches!(
        stream.read_message().await,
        Ok(ClientMessage::Attention)
    ));

    stream
        .write_message(PacketType::TabularResult, &[0xFD; 20])
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let (header, body) = read_packet(&mut client).await;
    assert_eq!(header.kind, PacketType::TabularResult);
    assert_eq!(header.length, 28);
    assert!(header.status.contains(PacketStatus::EOM));
    // PRELOGIN response was 1, the handshake flights took at least one more.
    assert!(header.packet_id >= 2);
    assert_eq!(body, vec![0xFD; 20]);
}

#[tokio::test]
async fn accept_login_only_then_downgrade() {
    let (stream, mut client) = accept_pair(
        EncryptPolicy::Optional,
        true,
        ENCRYPT_OFF,
        Encryption::Off,
        true,
    )
    .await;
    let mut stream = stream.expect("accept");
    assert_eq!(stream.mode, TlsMode::LoginOnly);

    // Stands for the LOGIN7: encrypted.
    send_attention(&mut client).await;
    assert!(matches!(
        stream.read_message().await,
        Ok(ClientMessage::Attention)
    ));

    let mut stream = stream.downgrade_encryption().await;
    assert!(matches!(stream.transport, Transport::Plain(_)));
    let mut client = client.into_plain();

    send_attention(&mut client).await;
    assert!(matches!(
        stream.read_message().await,
        Ok(ClientMessage::Attention)
    ));

    stream
        .write_message(PacketType::TabularResult, b"hello")
        .await
        .unwrap();
    let (header, body) = read_packet(&mut client).await;
    assert_eq!(header.kind, PacketType::TabularResult);
    assert!(header.status.contains(PacketStatus::EOM));
    assert_eq!(body, b"hello");
}

#[tokio::test]
async fn downgrade_is_noop_outside_login_only() {
    // Full: still encrypted after the call.
    let (stream, mut client) = accept_pair(
        EncryptPolicy::Required,
        true,
        ENCRYPT_ON,
        Encryption::On,
        true,
    )
    .await;
    let stream = stream.expect("accept");
    let mut stream = stream.downgrade_encryption().await;
    assert_eq!(stream.mode, TlsMode::Full);
    assert!(matches!(stream.transport, Transport::Tls(_)));
    send_attention(&mut client).await;
    assert!(matches!(
        stream.read_message().await,
        Ok(ClientMessage::Attention)
    ));

    // None: unchanged.
    let (stream, mut client) = accept_pair(
        EncryptPolicy::Off,
        false,
        ENCRYPT_OFF,
        Encryption::NotSup,
        false,
    )
    .await;
    let stream = stream.expect("accept");
    let mut stream = stream.downgrade_encryption().await;
    assert_eq!(stream.mode, TlsMode::None);
    assert!(matches!(stream.transport, Transport::Plain(_)));
    send_attention(&mut client).await;
    assert!(matches!(
        stream.read_message().await,
        Ok(ClientMessage::Attention)
    ));
}

#[tokio::test]
async fn packet_size_applies_to_writes() {
    let (mut stream, mut client) = plain_pair();
    stream.set_packet_size(512);
    stream
        .write_message(PacketType::TabularResult, &[1u8; 1000])
        .await
        .unwrap();

    let (first, body) = read_packet(&mut client).await;
    assert_eq!(first.length, 512);
    assert_eq!(body.len(), 504);
    assert_eq!(first.status, PacketStatus::NORMAL);
    assert_eq!(first.packet_id, 1);
    let (second, body) = read_packet(&mut client).await;
    assert_eq!(second.length, 504);
    assert_eq!(body.len(), 496);
    assert_eq!(second.status, PacketStatus::EOM);
    assert_eq!(second.packet_id, 2);
}

#[tokio::test]
async fn flush_sets_eom_on_last_packet_only() {
    let (mut stream, mut client) = plain_pair();
    stream.push_payload(&[0xAB; 5000]).await.unwrap();

    // The full packet is on the wire before `flush`.
    let mut first = [0u8; 4096];
    client.read_exact(&mut first).await.unwrap();
    assert_eq!(
        &first[..8],
        &[0x04, 0x00, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00]
    );
    assert!(first[8..].iter().all(|b| *b == 0xAB));

    stream.flush().await.unwrap();
    let mut second = [0u8; 920];
    client.read_exact(&mut second).await.unwrap();
    assert_eq!(
        &second[..8],
        &[0x04, 0x01, 0x03, 0x98, 0x00, 0x00, 0x02, 0x00]
    );
    assert!(second[8..].iter().all(|b| *b == 0xAB));

    // The next response starts a fresh message.
    stream.push_payload(b"next").await.unwrap();
    stream.flush().await.unwrap();
    let (header, body) = read_packet(&mut client).await;
    assert_eq!(header.status, PacketStatus::EOM);
    assert_eq!(header.packet_id, 3);
    assert_eq!(body, b"next");
}

#[tokio::test]
async fn flush_after_exact_multiple_sends_header_only_eom() {
    let (mut stream, mut client) = plain_pair();
    stream.push_payload(&[0x11; 4088]).await.unwrap();
    let (first, body) = read_packet(&mut client).await;
    assert_eq!(first.status, PacketStatus::NORMAL);
    assert_eq!(body.len(), 4088);

    stream.flush().await.unwrap();
    let (second, body) = read_packet(&mut client).await;
    assert_eq!(
        second.encode(),
        [0x04, 0x01, 0x00, 0x08, 0x00, 0x00, 0x02, 0x00]
    );
    assert!(body.is_empty());
}

#[tokio::test]
async fn flush_without_pending_data_sends_nothing() {
    let (mut stream, mut client) = plain_pair();
    stream.write_tokens(&[]).await.unwrap();
    stream.flush().await.unwrap();
    stream.flush().await.unwrap();
    drop(stream);
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn write_tokens_error_leaves_buffer_and_wire_untouched() {
    let (mut stream, mut client) = plain_pair();
    // A LOGINACK whose program name exceeds a B_VARCHAR (255 characters) is refused by
    // its encoder: an encoding failure anywhere in the batch must leave nothing behind.
    let bad = Token::LoginAck {
        tds_version: 0x7400_0004,
        program_name: "x".repeat(256),
        version: [16, 0, 0, 0],
    };
    let err = stream.write_tokens(&[done_token(), bad]).await.unwrap_err();
    assert!(matches!(err, TdsError::Malformed(_)));
    assert!(stream.write.buf.is_empty());
    assert!(!stream.write.packets_since_flush);

    // The implemented token then goes out alone, in one EOM packet.
    stream.write_tokens(&[done_token()]).await.unwrap();
    stream.flush().await.unwrap();
    drop(stream);
    let (header, body) = read_packet(&mut client).await;
    assert_eq!(header.kind, PacketType::TabularResult);
    assert_eq!(header.status, PacketStatus::EOM);
    assert_eq!(header.packet_id, 1);
    assert_eq!(body, DONE_BYTES);
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn spid_written_in_headers_after_set_spid() {
    let (mut stream, mut client) = plain_pair();
    stream
        .write_message(PacketType::TabularResult, b"a")
        .await
        .unwrap();
    let (header, _) = read_packet(&mut client).await;
    assert_eq!(header.encode()[4..6], [0x00, 0x00]);

    stream.set_spid(0x1234);
    stream
        .write_message(PacketType::TabularResult, b"b")
        .await
        .unwrap();
    let (header, _) = read_packet(&mut client).await;
    assert_eq!(header.encode()[4..6], [0x12, 0x34]);

    stream.write_tokens(&[done_token()]).await.unwrap();
    stream.flush().await.unwrap();
    let (header, body) = read_packet(&mut client).await;
    assert_eq!(header.encode()[4..6], [0x12, 0x34]);
    assert_eq!(body, DONE_BYTES);

    // Multi-packet responses carry it on every packet, and a negative SPID keeps
    // its bits.
    stream.set_spid(-2);
    stream.push_payload(&[0u8; 5000]).await.unwrap();
    stream.flush().await.unwrap();
    for _ in 0..2 {
        let (header, _) = read_packet(&mut client).await;
        assert_eq!(header.spid, 0xFFFE);
    }
}

#[tokio::test]
async fn split_allows_concurrent_read_and_write() {
    let (mut stream, mut client) = plain_pair();
    stream
        .write_message(PacketType::TabularResult, b"before split")
        .await
        .unwrap();
    let (header, _) = read_packet(&mut client).await;
    assert_eq!(header.packet_id, 1);
    stream.set_packet_size(512);
    stream.set_spid(7);

    let (mut reader, mut writer) = stream.split();
    let reading = tokio::spawn(async move { reader.read_message().await });
    let writing = tokio::spawn(async move {
        writer
            .write_message(PacketType::TabularResult, &[9u8; 600])
            .await
            .unwrap();
        writer.write_tokens(&[done_token()]).await.unwrap();
        writer.flush().await.unwrap();
        writer
    });

    // Packet size, SPID and PacketID carried over into the writer.
    let (first, body) = read_packet(&mut client).await;
    assert_eq!(first.length, 512);
    assert_eq!(body.len(), 504);
    assert_eq!(first.packet_id, 2);
    assert_eq!(first.spid, 7);
    assert_eq!(first.status, PacketStatus::NORMAL);
    let (second, body) = read_packet(&mut client).await;
    assert_eq!(body.len(), 96);
    assert_eq!(second.packet_id, 3);
    assert_eq!(second.spid, 7);
    assert_eq!(second.status, PacketStatus::EOM);
    let (third, body) = read_packet(&mut client).await;
    assert_eq!(third.packet_id, 4);
    assert_eq!(third.spid, 7);
    assert_eq!(body, DONE_BYTES);
    let writer = writing.await.unwrap();
    assert_eq!(writer.write.next_packet_id, 5);

    // The reader was waiting all along.
    send_attention(&mut client).await;
    assert!(matches!(
        reading.await.unwrap(),
        Ok(ClientMessage::Attention)
    ));
}

/// SQL_BATCH payload: ALL_HEADERS with a transaction descriptor, then `text` in UTF-16LE.
fn batch_payload(text: &str) -> Vec<u8> {
    let mut body = vec![
        0x16, 0x00, 0x00, 0x00, // ALL_HEADERS total length 22
        0x12, 0x00, 0x00, 0x00, // header length 18
        0x02, 0x00, // transaction descriptor header
        0, 0, 0, 0, 0, 0, 0, 0, // descriptor
        0x01, 0x00, 0x00, 0x00, // outstanding requests
    ];
    for unit in text.encode_utf16() {
        body.extend_from_slice(&unit.to_le_bytes());
    }
    body
}

/// Sends `payload` as one SQL_BATCH message cut into packets of `chunk` body bytes.
async fn send_batch<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8], chunk: usize, status: u8) {
    let chunks: Vec<&[u8]> = payload.chunks(chunk).collect();
    for (i, body) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        let flags = if last { 0x01 | status } else { 0x00 };
        w.write_all(&packet(PacketType::SqlBatch, flags, (i + 1) as u8, body))
            .await
            .unwrap();
    }
}

fn batch_text(message: ClientMessage) -> String {
    match message {
        ClientMessage::SqlBatch(batch) => batch.text,
        other => panic!("expected a SQL batch, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_message_is_skipped_and_the_next_one_is_read() {
    let (mut stream, mut client) = plain_pair();
    stream.set_max_message_size(1000);
    let big = batch_payload(&"x".repeat(2000));
    let big_len = big.len();
    let client_task = tokio::spawn(async move {
        send_batch(&mut client, &big, 300, 0).await;
        send_batch(&mut client, &batch_payload("SELECT 1"), 300, 0).await;
        client
    });
    match stream.read_message().await {
        Err(TdsError::MessageTooLarge(size)) => assert_eq!(size, big_len),
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }
    assert_eq!(batch_text(stream.read_message().await.unwrap()), "SELECT 1");
    client_task.await.unwrap();
}

#[tokio::test]
async fn message_at_the_limit_is_read_whole() {
    let (mut stream, mut client) = plain_pair();
    let payload = batch_payload("SELECT 42");
    stream.set_max_message_size(payload.len());
    send_batch(&mut client, &payload, 7, 0).await;
    assert_eq!(
        batch_text(stream.read_message().await.unwrap()),
        "SELECT 42"
    );
}

#[tokio::test]
async fn oversized_message_with_ignore_is_dropped_without_error() {
    let (mut stream, mut client) = plain_pair();
    stream.set_max_message_size(100);
    // IGNORE (0x02) on the last packet: the client withdrew the message.
    send_batch(&mut client, &batch_payload(&"y".repeat(500)), 200, 0x02).await;
    send_batch(&mut client, &batch_payload("SELECT 2"), 200, 0).await;
    assert_eq!(batch_text(stream.read_message().await.unwrap()), "SELECT 2");
}

#[tokio::test]
async fn reader_half_keeps_the_message_limit() {
    let (mut stream, mut client) = plain_pair();
    stream.set_max_message_size(64);
    let (mut reader, _writer) = stream.split();
    let big = batch_payload(&"z".repeat(100));
    let big_len = big.len();
    send_batch(&mut client, &big, 512, 0).await;
    send_batch(&mut client, &batch_payload("SELECT 3"), 512, 0).await;
    match reader.read_message().await {
        Err(TdsError::MessageTooLarge(size)) => assert_eq!(size, big_len),
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }
    assert_eq!(batch_text(reader.read_message().await.unwrap()), "SELECT 3");
}

#[tokio::test]
async fn oversized_message_keeps_its_reset_request() {
    let (mut stream, mut client) = plain_pair();
    stream.set_max_message_size(100);
    let (mut reader, _writer) = stream.split();
    // RESETCONNECTION (0x08) rides on the first packet only.
    let big = batch_payload(&"r".repeat(300));
    let mut first = packet(PacketType::SqlBatch, 0x08, 1, &big[..200]);
    first.extend_from_slice(&packet(PacketType::SqlBatch, 0x01, 2, &big[200..]));
    client.write_all(&first).await.unwrap();
    assert!(matches!(
        reader.read_message().await,
        Err(TdsError::MessageTooLarge(_))
    ));
    assert_eq!(reader.take_skipped_reset(), ResetConnection::Full);
    assert_eq!(reader.take_skipped_reset(), ResetConnection::None);

    // RESETCONNECTIONSKIPTRAN (0x10), then a message without any reset.
    client
        .write_all(&packet(PacketType::SqlBatch, 0x10 | 0x01, 3, &big))
        .await
        .unwrap();
    send_batch(&mut client, &big, 512, 0).await;
    assert!(reader.read_message().await.is_err());
    assert_eq!(
        reader.take_skipped_reset(),
        ResetConnection::SkipTransaction
    );
    assert!(reader.read_message().await.is_err());
    assert_eq!(reader.take_skipped_reset(), ResetConnection::None);
}

#[tokio::test]
async fn stays_encrypted_after_login_follows_the_negotiated_mode() {
    // Optional policy, client asks for encryption: the whole connection is encrypted.
    let (stream, _client) = accept_pair(
        EncryptPolicy::Optional,
        true,
        ENCRYPT_ON,
        Encryption::On,
        true,
    )
    .await;
    let stream = stream.expect("accept").downgrade_encryption().await;
    assert!(stream.stays_encrypted_after_login());

    // Required policy, client ENCRYPT_OFF: the server forces full encryption.
    let (stream, _client) = accept_pair(
        EncryptPolicy::Required,
        true,
        ENCRYPT_OFF,
        Encryption::Req,
        true,
    )
    .await;
    let stream = stream.expect("accept").downgrade_encryption().await;
    assert!(stream.stays_encrypted_after_login());

    // Optional policy, client ENCRYPT_OFF: only the login is encrypted.
    let (stream, _client) = accept_pair(
        EncryptPolicy::Optional,
        true,
        ENCRYPT_OFF,
        Encryption::Off,
        true,
    )
    .await;
    let stream = stream.expect("accept");
    assert!(!stream.stays_encrypted_after_login());
    let stream = stream.downgrade_encryption().await;
    assert!(!stream.stays_encrypted_after_login());

    // Off policy: clear text from the start.
    let (stream, _client) = accept_pair(
        EncryptPolicy::Off,
        false,
        ENCRYPT_OFF,
        Encryption::NotSup,
        false,
    )
    .await;
    let stream = stream.expect("accept").downgrade_encryption().await;
    assert!(!stream.stays_encrypted_after_login());
}
