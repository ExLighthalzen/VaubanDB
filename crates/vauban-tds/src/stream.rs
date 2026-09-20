//! `TdsStream`, `TdsReader`, `TdsWriter`: PRELOGIN exchange, TLS handshake carried in
//! PRELOGIN packets, framed message I/O with SPID and packet-size bookkeeping
//! ([MS-TDS] 2.2.6.5 PRELOGIN, 3.3.5 server state machine).
//!
//! # Connection life cycle ([MS-TDS] 3.3.5)
//!
//! 1. `accept` reads the client PRELOGIN, negotiates the encryption setting
//!    (`prelogin::negotiate`) and sends the PRELOGIN response.
//! 2. When the negotiated `TlsMode` is `Full` or `LoginOnly`, the TLS handshake runs
//!    through `PreloginTlsIo` (`tls.rs`): every flight travels inside PRELOGIN packets. Once
//!    it completes the adapter is switched to passthrough and the TDS packets travel inside
//!    TLS records.
//! 3. The session reads the LOGIN7 with `read_message`, then calls `downgrade_encryption`
//!    unconditionally: in `LoginOnly` (ENCRYPT_OFF) the raw stream is taken back and the
//!    login response already goes out in clear text; in any other mode nothing changes.
//! 4. Responses are written with `write_tokens` (buffered, full packets leave without EOM
//!    as soon as they are complete) and closed with `flush` (last packet with EOM).
//!
//! `PacketID` ([MS-TDS] 2.2.3.1.5): the PRELOGIN response is packet 1; the adapter numbers
//! the handshake flights with its own counter, also from 1, and the stream continues from
//! the value the adapter reached. The overlap is accepted on purpose: clients do not check
//! this counter and the session never sees it.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{BufMut, BytesMut};
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::error::TdsError;
use crate::message::{ClientMessage, decode_client_message};
use crate::packet::{
    self, HEADER_LEN, MAX_PACKET_SIZE, MIN_PACKET_SIZE, PacketHeader, PacketStatus, PacketType,
    split_message_with,
};
use crate::prelogin::{self, EncryptPolicy, PreLoginResponse, TlsMode};
use crate::tls::PreloginTlsIo;
use crate::tokens::{EncodeContext, Token, encode_tokens};

/// Packet size in force before the client negotiates one in LOGIN7
/// ([MS-TDS] 2.2.6.4 PacketSize: 4096 by default).
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// Upper bound on the size of the client PRELOGIN message: the largest legitimate one
/// (every option present, including TRACEID and NONCEOPT) is under 150 bytes.
const MAX_PRELOGIN_SIZE: usize = 4096;
/// Upper bound on the size of any other client message (a SQL batch or an RPC with
/// large parameters) before `read_message` fails with `MessageTooLarge`.
const MAX_MESSAGE_SIZE: usize = 32 * 1024 * 1024;
/// `UL_VERSION` + `US_SUBBUILD` announced in the PRELOGIN response ([MS-TDS] 2.2.6.5):
/// 16.0.1000, sub-build 0, a 16.x version like the one drivers expect from SQL Server 2022.
const SERVER_VERSION: [u8; 6] = [0x10, 0x00, 0x03, 0xE8, 0x00, 0x00];

/// The byte stream under the framing: the raw socket, or TLS over the PRELOGIN adapter.
/// Generic over `S` so that the tests run on `tokio::io::duplex`.
enum Transport<S> {
    /// Clear text: before any handshake, or after `downgrade_encryption` in `LoginOnly`.
    Plain(S),
    /// TLS after the handshake; the adapter underneath is in passthrough mode. Boxed:
    /// the TLS state is two orders of magnitude larger than a socket.
    Tls(Box<TlsStream<PreloginTlsIo<S>>>),
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Transport<S> {
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

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Transport<S> {
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

/// Write-side state shared by `TdsStream` and `TdsWriter`: the response buffer, the
/// negotiated packet size, the `PacketID` counter and the SPID.
#[derive(Debug)]
struct WriteState {
    /// Negotiated packet size, header included ([MS-TDS] 2.2.3.1.3 Length).
    packet_size: u16,
    /// `SPID` written in every header ([MS-TDS] 2.2.3.1.4); 0 until `set_spid`.
    spid: u16,
    /// `PacketID` of the next packet ([MS-TDS] 2.2.3.1.5), wraps modulo 256.
    next_packet_id: u8,
    /// Encoded tokens of the current response not yet sent.
    buf: BytesMut,
    /// COLMETADATA carried across the tokens of the current response.
    ctx: EncodeContext,
    /// `true` once a packet of the current response left without EOM: `flush` must then
    /// send a closing packet even if nothing is left in `buf`.
    packets_since_flush: bool,
}

impl WriteState {
    fn new() -> Self {
        Self {
            packet_size: DEFAULT_PACKET_SIZE,
            spid: 0,
            next_packet_id: 1,
            buf: BytesMut::new(),
            ctx: EncodeContext::default(),
            packets_since_flush: false,
        }
    }

    /// Largest payload a packet may carry at the current packet size.
    fn body_max(&self) -> usize {
        usize::from(self.packet_size.clamp(MIN_PACKET_SIZE, MAX_PACKET_SIZE)) - HEADER_LEN
    }

    /// Builds the header of the next packet and advances the `PacketID` counter.
    /// `body_len` is at most `body_max()`, so the length fits in a `u16`.
    fn next_header(&mut self, kind: PacketType, status: PacketStatus, body_len: usize) -> [u8; 8] {
        let header = PacketHeader {
            kind,
            status,
            length: (HEADER_LEN + body_len) as u16,
            spid: self.spid,
            packet_id: self.next_packet_id,
            window: 0,
        };
        self.next_packet_id = self.next_packet_id.wrapping_add(1);
        header.encode()
    }
}

/// Reads one complete message and decodes it.
async fn read_message_on<R: AsyncRead + Unpin>(r: &mut R) -> Result<ClientMessage, TdsError> {
    let raw = packet::read_message(r, MAX_MESSAGE_SIZE).await?;
    decode_client_message(raw.kind, &raw.payload, raw.reset)
}

/// Writes `payload` as one complete message (EOM on its last packet) with the current
/// SPID and `PacketID`, independently of the token buffer, then flushes the transport.
async fn write_message_on<W: AsyncWrite + Unpin>(
    w: &mut W,
    st: &mut WriteState,
    kind: PacketType,
    payload: &[u8],
) -> Result<(), TdsError> {
    let packets = split_message_with(
        kind,
        payload,
        st.packet_size,
        st.spid,
        &mut st.next_packet_id,
    );
    for packet in packets {
        w.write_all(&packet).await?;
    }
    w.flush().await?;
    Ok(())
}

/// Appends `bytes` to the response buffer and sends every full packet **without** EOM.
/// Sent packets are flushed so that the client sees them before the response ends.
async fn push_payload_on<W: AsyncWrite + Unpin>(
    w: &mut W,
    st: &mut WriteState,
    bytes: &[u8],
) -> Result<(), TdsError> {
    st.buf.extend_from_slice(bytes);
    let body_max = st.body_max();
    let mut sent = false;
    while st.buf.len() >= body_max {
        let body = st.buf.split_to(body_max);
        let header = st.next_header(PacketType::TabularResult, PacketStatus::NORMAL, body.len());
        let mut packet = BytesMut::with_capacity(HEADER_LEN + body.len());
        packet.put_slice(&header);
        packet.put_slice(&body);
        w.write_all(&packet).await?;
        st.packets_since_flush = true;
        sent = true;
    }
    if sent {
        w.flush().await?;
    }
    Ok(())
}

/// Encodes `tokens` into a scratch buffer and pushes the result; on error nothing reaches
/// the response buffer nor the wire.
async fn write_tokens_on<W: AsyncWrite + Unpin>(
    w: &mut W,
    st: &mut WriteState,
    tokens: &[Token],
) -> Result<(), TdsError> {
    if tokens.is_empty() {
        return Ok(());
    }
    let mut encoded = BytesMut::new();
    encode_tokens(tokens, &mut st.ctx, &mut encoded)?;
    push_payload_on(w, st, &encoded).await
}

/// Ends the current response: sends what is left in the buffer as one packet with EOM.
/// When packets already left and nothing remains, a header-only EOM packet closes the
/// message. When nothing was written since the last `flush`, nothing is sent.
async fn flush_on<W: AsyncWrite + Unpin>(w: &mut W, st: &mut WriteState) -> Result<(), TdsError> {
    if st.buf.is_empty() && !st.packets_since_flush {
        return Ok(());
    }
    // `push_payload_on` keeps the buffer under `body_max`, so this is exactly one packet.
    let rest = st.buf.split();
    let packets = split_message_with(
        PacketType::TabularResult,
        &rest,
        st.packet_size,
        st.spid,
        &mut st.next_packet_id,
    );
    for packet in packets {
        w.write_all(&packet).await?;
    }
    w.flush().await?;
    st.packets_since_flush = false;
    // The COLMETADATA of a response does not apply to the next one.
    st.ctx = EncodeContext::default();
    Ok(())
}

/// A TDS connection after PRELOGIN: raw socket or TLS, framing, packet size, negotiated
/// `TlsMode` and SPID. The only object the session manipulates.
///
/// `S` is the underlying byte stream, `TcpStream` in the server; the tests use
/// `tokio::io::duplex`.
pub struct TdsStream<S = TcpStream> {
    transport: Transport<S>,
    /// Encryption mode negotiated by `accept`; decides what `downgrade_encryption` does.
    mode: TlsMode,
    write: WriteState,
}

impl TdsStream<TcpStream> {
    /// Performs the PRELOGIN exchange and, if negotiated, the TLS handshake carried in
    /// PRELOGIN packets ([MS-TDS] 2.2.6.5, 3.3.5.1). `tls` is required as soon as the
    /// policy may lead to encryption.
    ///
    /// Errors: `UnexpectedPacketType` if the first message is not a PRELOGIN, `Malformed`
    /// on a bad PRELOGIN, `EncryptionRefused` when the client and the policy disagree (the
    /// response has been sent), `Unsupported` when encryption is negotiated without a TLS
    /// configuration, `Io`/`Tls` from the handshake.
    pub async fn accept(
        tcp: TcpStream,
        tls: Option<Arc<ServerConfig>>,
        policy: EncryptPolicy,
    ) -> Result<Self, TdsError> {
        Self::accept_on(tcp, tls, policy).await
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> TdsStream<S> {
    /// `accept` on any byte stream.
    pub(crate) async fn accept_on(
        mut io: S,
        tls: Option<Arc<ServerConfig>>,
        policy: EncryptPolicy,
    ) -> Result<Self, TdsError> {
        let mut write = WriteState::new();

        // [MS-TDS] 3.3.5.1: the first message of a connection is a PRELOGIN.
        let raw = packet::read_message(&mut io, MAX_PRELOGIN_SIZE).await?;
        if raw.kind != PacketType::PreLogin {
            return Err(TdsError::UnexpectedPacketType(raw.kind.to_u8()));
        }
        let client = prelogin::decode(&raw.payload)?;
        let (encryption, mode) = prelogin::negotiate(client.encryption, policy);

        let needs_tls = matches!(mode, TlsMode::Full | TlsMode::LoginOnly);
        let config = match (needs_tls, tls) {
            (true, Some(config)) => Some(config),
            // Never promise an encryption the server cannot provide.
            (true, None) => {
                return Err(TdsError::Unsupported(
                    "encryption negotiated without a TLS configuration",
                ));
            }
            (false, _) => None,
        };

        let response = PreLoginResponse {
            version: SERVER_VERSION,
            encryption,
            // [MS-TDS] 2.2.6.5 FEDAUTHREQUIRED: answered when the client sent it; 0x00 says
            // federated authentication is not required (V1 has SQL authentication only).
            fed_auth_required: client.fed_auth_required.map(|_| 0x00),
        };
        // [MS-TDS] 2.2.6.5: the server's PRELOGIN response travels in a TABULAR RESULT
        // (0x04) packet; only the TLS handshake records are wrapped in PRELOGIN (0x12)
        // packets, which `PreloginTlsIo` handles.
        write_message_on(
            &mut io,
            &mut write,
            PacketType::TabularResult,
            &response.encode(),
        )
        .await?;

        let transport = match (mode, config) {
            // The response has been sent; the client closes on its side too.
            (TlsMode::Refused, _) => return Err(TdsError::EncryptionRefused),
            (_, Some(config)) => {
                // [MS-TDS] 3.3.5.1: the handshake flights travel in PRELOGIN packets.
                let adapter = PreloginTlsIo::new(io, write.packet_size);
                let mut stream = TlsAcceptor::from(config).accept(adapter).await?;
                let adapter = stream.get_mut().0;
                adapter.set_passthrough();
                write.next_packet_id = adapter.packet_id().wrapping_add(1);
                Transport::Tls(Box::new(stream))
            }
            (_, None) => Transport::Plain(io),
        };
        Ok(Self {
            transport,
            mode,
            write,
        })
    }

    /// Reads and decodes the next client message, whatever the transport.
    pub async fn read_message(&mut self) -> Result<ClientMessage, TdsError> {
        read_message_on(&mut self.transport).await
    }

    /// Encodes `tokens` into the response buffer; every packet that becomes full leaves
    /// immediately **without** EOM. On error nothing is buffered nor sent.
    pub async fn write_tokens(&mut self, tokens: &[Token]) -> Result<(), TdsError> {
        write_tokens_on(&mut self.transport, &mut self.write, tokens).await
    }

    /// Ends the current response: sends the remainder with EOM. One response = one `flush`;
    /// a `flush` with nothing written since the previous one sends nothing.
    pub async fn flush(&mut self) -> Result<(), TdsError> {
        flush_on(&mut self.transport, &mut self.write).await
    }

    /// Packet size for the packets sent from now on (clamped to `512..=32767`).
    pub fn set_packet_size(&mut self, size: u16) {
        self.write.packet_size = size;
    }

    /// SPID written in the header of every following packet ([MS-TDS] 2.2.3.1.4); 0 before.
    pub fn set_spid(&mut self, spid: i16) {
        // The header field is an unsigned 16-bit value: same bits, no arithmetic.
        self.write.spid = spid as u16;
    }

    /// Goes back to clear text when the negotiated mode is `LoginOnly` (ENCRYPT_OFF): the
    /// raw stream is taken back without `close_notify`, as the state machine of
    /// [MS-TDS] 3.3.5 sends the login response in clear text. No-op in any other mode.
    /// The session calls it after reading the LOGIN7 and before writing the response,
    /// when the client is waiting: nothing decrypted is pending on either side.
    pub async fn downgrade_encryption(self) -> Self {
        let transport = match (self.mode, self.transport) {
            (TlsMode::LoginOnly, Transport::Tls(tls)) => {
                let (adapter, _connection) = tls.into_inner();
                Transport::Plain(adapter.into_inner())
            }
            (_, transport) => transport,
        };
        Self {
            transport,
            mode: self.mode,
            write: self.write,
        }
    }

    /// Splits the connection into independent halves so that an ATTENTION can be read
    /// while a response is being written. The writer takes the buffer, the packet size,
    /// the `PacketID` counter and the SPID with it.
    pub fn split(self) -> (TdsReader<S>, TdsWriter<S>) {
        let (read, write) = tokio::io::split(self.transport);
        (
            TdsReader { inner: read },
            TdsWriter {
                inner: write,
                write: self.write,
            },
        )
    }

    /// Writes `payload` as one complete message with the current SPID and `PacketID`,
    /// bypassing the token buffer. The PRELOGIN response goes through the same function
    /// (`write_message_on`) before the stream exists; this method serves the tests.
    #[cfg(test)]
    pub(crate) async fn write_message(
        &mut self,
        kind: PacketType,
        payload: &[u8],
    ) -> Result<(), TdsError> {
        write_message_on(&mut self.transport, &mut self.write, kind, payload).await
    }

    /// Appends already encoded token bytes to the response buffer, sending every full
    /// packet without EOM. This is what `write_tokens` does after `encode_tokens`.
    #[cfg(test)]
    pub(crate) async fn push_payload(&mut self, bytes: &[u8]) -> Result<(), TdsError> {
        push_payload_on(&mut self.transport, &mut self.write, bytes).await
    }
}

/// Read half of a `TdsStream` after `split`.
pub struct TdsReader<S = TcpStream> {
    inner: ReadHalf<Transport<S>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> TdsReader<S> {
    /// Reads and decodes the next client message.
    pub async fn read_message(&mut self) -> Result<ClientMessage, TdsError> {
        read_message_on(&mut self.inner).await
    }
}

/// Write half of a `TdsStream` after `split`: response buffer, packet size, `PacketID`
/// and SPID.
pub struct TdsWriter<S = TcpStream> {
    inner: WriteHalf<Transport<S>>,
    write: WriteState,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> TdsWriter<S> {
    /// Same as `TdsStream::write_tokens`.
    pub async fn write_tokens(&mut self, tokens: &[Token]) -> Result<(), TdsError> {
        write_tokens_on(&mut self.inner, &mut self.write, tokens).await
    }

    /// Same as `TdsStream::flush`.
    pub async fn flush(&mut self) -> Result<(), TdsError> {
        flush_on(&mut self.inner, &mut self.write).await
    }

    /// Same as `TdsStream::set_packet_size`.
    pub fn set_packet_size(&mut self, size: u16) {
        self.write.packet_size = size;
    }

    /// Same as `TdsStream::set_spid`.
    pub fn set_spid(&mut self, spid: i16) {
        // The header field is an unsigned 16-bit value: same bits, no arithmetic.
        self.write.spid = spid as u16;
    }

    /// Same as `TdsStream::write_message`.
    #[cfg(test)]
    pub(crate) async fn write_message(
        &mut self,
        kind: PacketType,
        payload: &[u8],
    ) -> Result<(), TdsError> {
        write_message_on(&mut self.inner, &mut self.write, kind, payload).await
    }
}

#[cfg(test)]
mod tests;
