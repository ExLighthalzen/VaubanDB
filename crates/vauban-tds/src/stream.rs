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
    decode_client_message(raw.kind, &raw.payload)
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
mod tests {
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
}
