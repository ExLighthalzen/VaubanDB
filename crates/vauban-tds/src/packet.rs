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
mod tests;
