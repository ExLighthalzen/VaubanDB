//! Packet framing ([MS-TDS] 2.2.3 Packets): 8-byte header, reassembly of a multi-packet
//! message up to the EOM bit, and splitting of a payload into packets of the negotiated
//! size. `TdsStream` (`stream.rs`) is the only intended caller.

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::ResetConnection;
use crate::error::TdsError;

/// Size of the packet header ([MS-TDS] 2.2.3.1).
pub const HEADER_LEN: usize = 8;
/// Smallest packet size a client may negotiate ([MS-TDS] 2.2.6.4, PacketSize).
pub const MIN_PACKET_SIZE: u16 = 512;
/// Largest packet size, header included ([MS-TDS] 2.2.3.1, Length).
pub const MAX_PACKET_SIZE: u16 = 32767;

/// Packet type, first byte of the header ([MS-TDS] 2.2.3.1.1 Type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketType {
    /// SQL_BATCH (0x01).
    SqlBatch,
    /// PRE_TDS7_LOGIN (0x02), refused.
    PreTds7Login,
    /// RPC (0x03).
    Rpc,
    /// TABULAR_RESULT (0x04): every server response.
    TabularResult,
    /// ATTENTION (0x06).
    Attention,
    /// BULK_LOAD (0x07), refused in V1.
    BulkLoad,
    /// FEDAUTH_TOKEN (0x08), refused in V1.
    FedAuthToken,
    /// TRANSACTION_MANAGER (0x0E).
    TransactionManager,
    /// LOGIN7 (0x10).
    Login7,
    /// SSPI (0x11), refused in V1.
    Sspi,
    /// PRELOGIN (0x12).
    PreLogin,
    /// Any value not listed by [MS-TDS] 2.2.3.1.1.
    Unknown(u8),
}

impl PacketType {
    /// Maps the header byte to a packet type.
    pub fn from_u8(byte: u8) -> Self {
        match byte {
            0x01 => Self::SqlBatch,
            0x02 => Self::PreTds7Login,
            0x03 => Self::Rpc,
            0x04 => Self::TabularResult,
            0x06 => Self::Attention,
            0x07 => Self::BulkLoad,
            0x08 => Self::FedAuthToken,
            0x0E => Self::TransactionManager,
            0x10 => Self::Login7,
            0x11 => Self::Sspi,
            0x12 => Self::PreLogin,
            other => Self::Unknown(other),
        }
    }

    /// Header byte of this packet type.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::SqlBatch => 0x01,
            Self::PreTds7Login => 0x02,
            Self::Rpc => 0x03,
            Self::TabularResult => 0x04,
            Self::Attention => 0x06,
            Self::BulkLoad => 0x07,
            Self::FedAuthToken => 0x08,
            Self::TransactionManager => 0x0E,
            Self::Login7 => 0x10,
            Self::Sspi => 0x11,
            Self::PreLogin => 0x12,
            Self::Unknown(other) => other,
        }
    }
}

/// Packet status bit field, second byte of the header ([MS-TDS] 2.2.3.1.2 Status).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PacketStatus(pub u8);

impl PacketStatus {
    /// NORMAL (0x00).
    pub const NORMAL: Self = Self(0x00);
    /// END_OF_MESSAGE (0x01): last packet of the message.
    pub const EOM: Self = Self(0x01);
    /// IGNORE_EVENT (0x02): the whole message is to be discarded (EOM must also be set).
    pub const IGNORE: Self = Self(0x02);
    /// RESETCONNECTION (0x08).
    pub const RESET_CONNECTION: Self = Self(0x08);
    /// RESETCONNECTIONSKIPTRAN (0x10).
    pub const RESET_CONNECTION_SKIP_TRAN: Self = Self(0x10);

    /// `true` when every bit of `other` is set in `self`.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Decoded packet header ([MS-TDS] 2.2.3.1 Packet Header), all fields big-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    /// `Type`.
    pub kind: PacketType,
    /// `Status`.
    pub status: PacketStatus,
    /// `Length`: size of the packet **including** the header, 8..=32767.
    pub length: u16,
    /// `SPID`.
    pub spid: u16,
    /// `PacketID`: incremented modulo 256 for each packet sent.
    pub packet_id: u8,
    /// `Window`, always 0.
    pub window: u8,
}

impl PacketHeader {
    /// Decodes an 8-byte header; rejects a `Length` outside `8..=32767`.
    pub fn decode(bytes: &[u8]) -> Result<Self, TdsError> {
        let [t, s, l0, l1, sp0, sp1, id, w] = bytes else {
            return Err(TdsError::Malformed("packet header must be 8 bytes"));
        };
        let length = u16::from_be_bytes([*l0, *l1]);
        if usize::from(length) < HEADER_LEN || length > MAX_PACKET_SIZE {
            return Err(TdsError::Malformed("packet length out of range 8..=32767"));
        }
        Ok(Self {
            kind: PacketType::from_u8(*t),
            status: PacketStatus(*s),
            length,
            spid: u16::from_be_bytes([*sp0, *sp1]),
            packet_id: *id,
            window: *w,
        })
    }

    /// Encodes the header into its 8-byte wire form.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let [l0, l1] = self.length.to_be_bytes();
        let [sp0, sp1] = self.spid.to_be_bytes();
        [
            self.kind.to_u8(),
            self.status.0,
            l0,
            l1,
            sp0,
            sp1,
            self.packet_id,
            self.window,
        ]
    }
}

/// A complete client message: the concatenated payloads of one or more packets, up to
/// the one carrying the EOM bit ([MS-TDS] 2.2.3 Packets, 2.2.4 Packet Data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessage {
    /// Packet type shared by every packet of the message.
    pub kind: PacketType,
    /// Payload, header bytes removed.
    pub payload: Bytes,
    /// RESETCONNECTION or RESETCONNECTIONSKIPTRAN from the status of the first packet
    /// ([MS-TDS] 2.2.3.1.2); bits on later packets are ignored.
    pub reset: ResetConnection,
}

/// Clamps a negotiated packet size into `MIN_PACKET_SIZE..=MAX_PACKET_SIZE`.
fn clamp_packet_size(packet_size: u16) -> usize {
    usize::from(packet_size.clamp(MIN_PACKET_SIZE, MAX_PACKET_SIZE))
}

/// Splits `payload` into packets of at most `packet_size` bytes (header included), SPID 0,
/// PacketID starting at 1. Only the last packet carries EOM; an empty payload gives exactly
/// one header-only packet with EOM (the shape of ATTENTION). Pure function.
// Kept as a public helper for the framing layer; not called inside this crate.
#[allow(dead_code)]
pub fn split_message(kind: PacketType, payload: &[u8], packet_size: u16) -> Vec<Bytes> {
    let mut next_packet_id = 1u8;
    split_message_with(kind, payload, packet_size, 0, &mut next_packet_id)
}

/// `split_message` with an explicit SPID and a running PacketID counter, for writers that
/// keep state between messages (`write_message`, `TdsWriter`). `packet_size` outside
/// `512..=32767` is clamped.
pub(crate) fn split_message_with(
    kind: PacketType,
    payload: &[u8],
    packet_size: u16,
    spid: u16,
    next_packet_id: &mut u8,
) -> Vec<Bytes> {
    let body_max = clamp_packet_size(packet_size) - HEADER_LEN;
    let mut packets = Vec::with_capacity(payload.len() / body_max + 1);
    let mut chunks = payload.chunks(body_max).peekable();
    if chunks.peek().is_none() {
        // `chunks` yields nothing for an empty slice; ATTENTION is such a message.
        packets.push(build_packet(
            kind,
            PacketStatus::EOM,
            &[],
            spid,
            next_packet_id,
        ));
        return packets;
    }
    while let Some(chunk) = chunks.next() {
        let status = if chunks.peek().is_none() {
            PacketStatus::EOM
        } else {
            PacketStatus::NORMAL
        };
        packets.push(build_packet(kind, status, chunk, spid, next_packet_id));
    }
    packets
}

/// Builds one packet and advances the PacketID counter (modulo 256).
fn build_packet(
    kind: PacketType,
    status: PacketStatus,
    body: &[u8],
    spid: u16,
    next_packet_id: &mut u8,
) -> Bytes {
    // `body.len() <= MAX_PACKET_SIZE - HEADER_LEN` by construction, so the sum fits in u16.
    let length = (HEADER_LEN + body.len()) as u16;
    let header = PacketHeader {
        kind,
        status,
        length,
        spid,
        packet_id: *next_packet_id,
        window: 0,
    };
    *next_packet_id = next_packet_id.wrapping_add(1);
    let mut out = BytesMut::with_capacity(HEADER_LEN + body.len());
    out.put_slice(&header.encode());
    out.put_slice(body);
    out.freeze()
}

/// Reads exactly `buf.len()` bytes, mapping an early EOF to `ConnectionClosed`.
async fn read_exact_or_closed<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut [u8],
) -> Result<(), TdsError> {
    match r.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(TdsError::ConnectionClosed),
        Err(e) => Err(TdsError::Io(e)),
    }
}

/// Reads one complete message: packets are accumulated until one carries EOM. Incoming
/// packets may have any `Length` up to 32767 whatever the negotiated size. Fails with
/// `Malformed` if the packet type changes inside a message, with `MessageTooLarge` as soon
/// as the announced size would exceed `max_message_size` (before reading the body). A
/// message whose last packet carries IGNORE is discarded and the next one is returned.
pub async fn read_message<R: AsyncRead + Unpin>(
    r: &mut R,
    max_message_size: usize,
) -> Result<RawMessage, TdsError> {
    let mut header_buf = [0u8; HEADER_LEN];
    let mut payload = BytesMut::new();
    // `None` until the first packet of the current message has been read.
    let mut first: Option<(PacketType, ResetConnection)> = None;
    loop {
        read_exact_or_closed(r, &mut header_buf).await?;
        let header = PacketHeader::decode(&header_buf)?;
        let (kind, reset) = match first {
            None => {
                let reset = if header
                    .status
                    .contains(PacketStatus::RESET_CONNECTION_SKIP_TRAN)
                {
                    ResetConnection::SkipTransaction
                } else if header.status.contains(PacketStatus::RESET_CONNECTION) {
                    ResetConnection::Full
                } else {
                    ResetConnection::None
                };
                first = Some((header.kind, reset));
                (header.kind, reset)
            }
            Some((kind, _reset)) if kind != header.kind => {
                return Err(TdsError::Malformed("packet type changed inside a message"));
            }
            Some(state) => state,
        };
        let body_len = usize::from(header.length) - HEADER_LEN;
        let total = payload.len() + body_len;
        if total > max_message_size {
            return Err(TdsError::MessageTooLarge(total));
        }
        let start = payload.len();
        payload.resize(total, 0);
        read_exact_or_closed(r, &mut payload[start..]).await?;
        if !header.status.contains(PacketStatus::EOM) {
            continue;
        }
        if header.status.contains(PacketStatus::IGNORE) {
            payload.clear();
            first = None;
            continue;
        }
        return Ok(RawMessage {
            kind,
            payload: payload.freeze(),
            reset,
        });
    }
}

/// Writes `payload` as one message split into packets of `packet_size` bytes, SPID 0,
/// numbering them from `*next_packet_id` and leaving the counter ready for the next
/// message. Flushes the writer once the last packet is written.
// Kept as a public helper for the framing layer; not called inside this crate.
#[allow(dead_code)]
pub async fn write_message<W: AsyncWrite + Unpin>(
    w: &mut W,
    kind: PacketType,
    payload: &[u8],
    packet_size: u16,
    next_packet_id: &mut u8,
) -> Result<(), TdsError> {
    for packet in split_message_with(kind, payload, packet_size, 0, next_packet_id) {
        w.write_all(&packet).await?;
    }
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
        let packets =
            split_message_with(PacketType::TabularResult, &[0u8; 600], 512, 0x1234, &mut id);
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
}
