//! Adapter carrying TLS records inside PRELOGIN packets during the handshake
//! ([MS-TDS] 2.2.6.5 PRELOGIN, 3.3.5.1).
//!
//! In TDS 7.4 the TLS handshake travels **inside** TDS packets of type PRELOGIN (0x12):
//! every flight the client sends is one PRELOGIN message (possibly several packets), and
//! so is every flight the server sends back. Once the handshake completes the connection
//! becomes plain TLS and the TDS packets travel inside TLS records. [`PreloginTlsIo`] hides
//! that switch from `rustls`: in [`Mode::Handshake`] it strips the packet headers on the way
//! in and wraps the records on the way out; in [`Mode::Passthrough`] it is transparent.
//! It never looks at the content of a record nor at the content of a PRELOGIN message.
//!
//! `TdsStream` (`stream.rs`) is the only intended caller.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::packet::{HEADER_LEN, PacketHeader, PacketType, split_message_with};

/// What the adapter does with the bytes going through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// TLS records are wrapped in PRELOGIN packets ([MS-TDS] 3.3.5.1): headers are removed
    /// on read, and every flush emits one PRELOGIN message.
    Handshake,
    /// Plain TLS after the handshake: reads and writes go straight to the inner stream.
    Passthrough,
}

/// Where the read side stands inside the current PRELOGIN packet (`Handshake` mode).
#[derive(Debug)]
enum ReadState {
    /// Collecting the 8-byte packet header ([MS-TDS] 2.2.3.1); `filled` bytes so far.
    Header {
        buf: [u8; HEADER_LEN],
        filled: usize,
    },
    /// Delivering the `remaining` payload bytes of the current packet to the caller.
    Body { remaining: usize },
}

/// `AsyncRead + AsyncWrite` adapter that carries TLS records inside PRELOGIN packets
/// while in [`Mode::Handshake`] and is transparent in [`Mode::Passthrough`].
///
/// Read side (`Handshake`): the payload of each PRELOGIN packet is handed to the caller
/// as soon as it arrives, packet by packet, without waiting for the EOM bit: `rustls`
/// reassembles its own records. Reads never look past the end of the current packet, so
/// switching to `Passthrough` between two packets loses nothing. A packet of another type
/// fails with [`io::ErrorKind::InvalidData`].
///
/// Write side (`Handshake`): [`AsyncWrite::poll_write`] only appends to a buffer;
/// [`AsyncWrite::poll_flush`] turns everything buffered so far into **one** PRELOGIN message
/// split at `packet_size` (EOM on the last packet only), writes it, then flushes the inner
/// stream. A flush with nothing buffered writes nothing. `tokio-rustls` flushes after every
/// handshake flight, so one flight maps to one message.
#[derive(Debug)]
pub(crate) struct PreloginTlsIo<S> {
    inner: S,
    mode: Mode,
    /// Negotiated packet size, header included, used to split outgoing messages.
    packet_size: u16,
    /// `PacketID` of the next packet to emit ([MS-TDS] 2.2.3.1.5), starts at 1.
    next_packet_id: u8,
    read_state: ReadState,
    /// Bytes accepted by `poll_write` and not yet turned into packets.
    write_buf: BytesMut,
    /// Packets built by a previous `poll_flush` and not yet fully written to `inner`.
    pending: Vec<Bytes>,
    /// Index in `pending` of the first packet not fully written.
    pending_index: usize,
}

// Kept as helpers for the framing layer; not called inside this crate.
#[allow(dead_code)]
impl<S> PreloginTlsIo<S> {
    /// Wraps `inner` in `Handshake` mode; outgoing messages are split at `packet_size`.
    pub(crate) fn new(inner: S, packet_size: u16) -> Self {
        Self {
            inner,
            mode: Mode::Handshake,
            packet_size,
            next_packet_id: 1,
            read_state: ReadState::Header {
                buf: [0; HEADER_LEN],
                filled: 0,
            },
            write_buf: BytesMut::new(),
            pending: Vec::new(),
            pending_index: 0,
        }
    }

    /// Switches to `Passthrough`: from now on bytes go straight to the inner stream.
    /// Meant to be called once the handshake is complete, with nothing left buffered.
    pub(crate) fn set_passthrough(&mut self) {
        self.mode = Mode::Passthrough;
    }

    /// Current mode.
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// Gives the inner stream back.
    pub(crate) fn into_inner(self) -> S {
        self.inner
    }

    /// `PacketID` of the last packet emitted (0 before the first one), so that the caller
    /// can continue the counter for the packets it sends after the handshake.
    pub(crate) fn packet_id(&self) -> u8 {
        self.next_packet_id.wrapping_sub(1)
    }
}

impl<S: AsyncRead + Unpin> PreloginTlsIo<S> {
    /// Polls the inner stream for at most `max` more bytes into `buf`; the size of the
    /// filled region is returned. `buf` must have at least `max` bytes remaining.
    fn poll_inner_read(
        inner: &mut S,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
        max: usize,
    ) -> Poll<io::Result<usize>> {
        // `take` hands the inner stream a view on the unfilled part of `buf`; writes made
        // through that view are not visible to `buf`'s bookkeeping, so the region is
        // initialized up front (safe, cheap for a handshake) and only advanced afterwards.
        buf.initialize_unfilled_to(max);
        let mut sub = buf.take(max);
        ready!(Pin::new(inner).poll_read(cx, &mut sub))?;
        let n = sub.filled().len();
        buf.advance(n);
        Poll::Ready(Ok(n))
    }

    /// `Handshake` read: strips PRELOGIN headers and delivers the payload of each packet.
    fn poll_read_handshake(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            match &mut self.read_state {
                ReadState::Header {
                    buf: header,
                    filled,
                } => {
                    if *filled == HEADER_LEN {
                        let decoded = PacketHeader::decode(&header[..])
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                        if decoded.kind != PacketType::PreLogin {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "expected a PRELOGIN packet during the TLS handshake",
                            )));
                        }
                        // `decode` guarantees `length >= HEADER_LEN`.
                        self.read_state = ReadState::Body {
                            remaining: usize::from(decoded.length) - HEADER_LEN,
                        };
                        continue;
                    }
                    let mut tmp = ReadBuf::new(&mut header[*filled..]);
                    let n = ready!(Self::poll_inner_read(
                        &mut self.inner,
                        cx,
                        &mut tmp,
                        HEADER_LEN - *filled
                    ))?;
                    if n == 0 {
                        return if *filled == 0 {
                            // Clean end of stream between two packets.
                            Poll::Ready(Ok(()))
                        } else {
                            Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
                        };
                    }
                    *filled += n;
                }
                ReadState::Body { remaining } => {
                    if *remaining == 0 {
                        // Header-only packet (or end of the current payload): next header.
                        self.read_state = ReadState::Header {
                            buf: [0; HEADER_LEN],
                            filled: 0,
                        };
                        continue;
                    }
                    let max = (*remaining).min(buf.remaining());
                    let n = ready!(Self::poll_inner_read(&mut self.inner, cx, buf, max))?;
                    if n == 0 {
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                    }
                    *remaining -= n;
                    if *remaining == 0 {
                        self.read_state = ReadState::Header {
                            buf: [0; HEADER_LEN],
                            filled: 0,
                        };
                    }
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> PreloginTlsIo<S> {
    /// `Handshake` flush: wraps the buffered bytes into one PRELOGIN message, writes every
    /// packet (resuming a message left half-written by a previous `Pending`), then flushes
    /// the inner stream.
    fn poll_flush_handshake(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            while let Some(packet) = self.pending.get_mut(self.pending_index) {
                let n = ready!(Pin::new(&mut self.inner).poll_write(cx, packet))?;
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                packet.advance(n);
                if packet.is_empty() {
                    self.pending_index += 1;
                }
            }
            self.pending.clear();
            self.pending_index = 0;
            if self.write_buf.is_empty() {
                break;
            }
            // [MS-TDS] 2.2.6.5: the TLS payload is wrapped in a PRELOGIN message, SPID 0.
            let payload = self.write_buf.split();
            self.pending = split_message_with(
                PacketType::PreLogin,
                &payload,
                self.packet_size,
                0,
                &mut self.next_packet_id,
            );
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for PreloginTlsIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.mode {
            Mode::Handshake => this.poll_read_handshake(cx, buf),
            Mode::Passthrough => Pin::new(&mut this.inner).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for PreloginTlsIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.mode {
            Mode::Handshake => {
                this.write_buf.extend_from_slice(data);
                Poll::Ready(Ok(data.len()))
            }
            Mode::Passthrough => Pin::new(&mut this.inner).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.mode {
            Mode::Handshake => this.poll_flush_handshake(cx),
            Mode::Passthrough => Pin::new(&mut this.inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Self-signed test certificate (`CN=localhost`, RSA 2048, 10 years) generated with
/// `openssl req -x509 -newkey rsa:2048 -nodes -keyout test-key.pem -out test-cert.pem
/// -days 3650 -subj "/CN=localhost"`. Tests only.
#[cfg(test)]
const TEST_CERT_PEM: &[u8] = include_bytes!("../tests/fixtures/test-cert.pem");
/// PKCS#8 private key matching [`TEST_CERT_PEM`]. Tests only.
#[cfg(test)]
const TEST_KEY_PEM: &[u8] = include_bytes!("../tests/fixtures/test-key.pem");

/// Server and client `rustls` configurations for the tests of this crate (`tls.rs` and
/// `stream.rs`), built from the fixtures.
///
/// **TLS 1.2 only**, on both sides: in TDS 7.4 the handshake is carried in PRELOGIN packets
/// and the client stops wrapping as soon as its handshake is complete. With TLS 1.3 the
/// server sends post-handshake messages (session tickets) that a TDS 7.4 client cannot
/// receive inside a PRELOGIN packet; TLS 1.2 has no such message. The client side trusts
/// any server certificate (the equivalent of `TrustServerCertificate=true`) while still
/// checking the handshake signatures.
#[cfg(test)]
pub(crate) fn test_tls_configs() -> (
    std::sync::Arc<rustls::ServerConfig>,
    std::sync::Arc<rustls::ClientConfig>,
) {
    use std::sync::Arc;

    let certs = rustls_pemfile::certs(&mut &TEST_CERT_PEM[..])
        .collect::<Result<Vec<_>, _>>()
        .expect("test certificate fixture is valid PEM");
    let key = rustls_pemfile::private_key(&mut &TEST_KEY_PEM[..])
        .expect("test key fixture is valid PEM")
        .expect("test key fixture holds a private key");
    let server = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("test certificate and key match");

    let builder = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12]);
    let verifier = test_support::TrustAnyServerCert {
        provider: builder.crypto_provider().clone(),
    };
    let client = builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    (Arc::new(server), Arc::new(client))
}

/// Test-only certificate verifier: accepts any server certificate, verifies signatures.
#[cfg(test)]
mod test_support {
    use std::sync::Arc;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};

    /// `TrustServerCertificate=true`: no chain validation, real signature checks.
    #[derive(Debug)]
    pub(super) struct TrustAnyServerCert {
        pub(super) provider: Arc<CryptoProvider>,
    }

    impl ServerCertVerifier for TrustAnyServerCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::*;
    use crate::packet::PacketStatus;

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

    /// Reads from `io` until `len` bytes are collected.
    async fn read_n<R: AsyncRead + Unpin>(io: &mut R, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        io.read_exact(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn wrapper_strips_prelogin_headers() {
        let payload: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let (mut peer, inner) = duplex(1 << 16);
        peer.write_all(&packet(PacketType::PreLogin, 0x00, 1, &payload[..150]))
            .await
            .unwrap();
        peer.write_all(&packet(PacketType::PreLogin, 0x01, 2, &payload[150..]))
            .await
            .unwrap();
        let mut io = PreloginTlsIo::new(inner, 4096);
        assert_eq!(read_n(&mut io, 300).await, payload);
        assert_eq!(io.mode(), Mode::Handshake);
    }

    #[tokio::test]
    async fn wrapper_delivers_each_packet_without_waiting_for_eom() {
        // A single packet without EOM is enough for the payload to reach the caller.
        let (mut peer, inner) = duplex(1 << 16);
        peer.write_all(&packet(PacketType::PreLogin, 0x00, 1, b"hello"))
            .await
            .unwrap();
        let mut io = PreloginTlsIo::new(inner, 4096);
        let mut buf = [0u8; 32];
        let n = io.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[tokio::test]
    async fn wrapper_read_reports_clean_eof_between_packets() {
        let (mut peer, inner) = duplex(1 << 16);
        peer.write_all(&packet(PacketType::PreLogin, 0x01, 1, b"ab"))
            .await
            .unwrap();
        drop(peer);
        let mut io = PreloginTlsIo::new(inner, 4096);
        let mut out = Vec::new();
        io.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"ab");
    }

    #[tokio::test]
    async fn wrapper_read_reports_truncated_packet() {
        let (mut peer, inner) = duplex(1 << 16);
        let mut truncated = packet(PacketType::PreLogin, 0x01, 1, &[0u8; 100]);
        truncated.truncate(HEADER_LEN + 40);
        peer.write_all(&truncated).await.unwrap();
        drop(peer);
        let mut io = PreloginTlsIo::new(inner, 4096);
        let mut out = Vec::new();
        let err = io.read_to_end(&mut out).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn wrapper_wraps_on_flush() {
        let (mut peer, inner) = duplex(1 << 16);
        let mut io = PreloginTlsIo::new(inner, 4096);
        io.write_all(&[0xAAu8; 5000]).await.unwrap();
        io.flush().await.unwrap();
        let first = read_n(&mut peer, 4096).await;
        assert_eq!(
            &first[..8],
            &[0x12, 0x00, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00]
        );
        assert!(first[8..].iter().all(|b| *b == 0xAA));
        let second = read_n(&mut peer, 920).await;
        assert_eq!(
            &second[..8],
            &[0x12, 0x01, 0x03, 0x98, 0x00, 0x00, 0x02, 0x00]
        );
        assert!(second[8..].iter().all(|b| *b == 0xAA));
        assert_eq!(io.packet_id(), 2);
    }

    #[tokio::test]
    async fn wrapper_flush_without_data_sends_nothing() {
        let (mut peer, inner) = duplex(1 << 16);
        let mut io = PreloginTlsIo::new(inner, 4096);
        io.flush().await.unwrap();
        drop(io);
        let mut out = Vec::new();
        peer.read_to_end(&mut out).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn wrapper_packet_id_increments() {
        let (mut peer, inner) = duplex(1 << 16);
        let mut io = PreloginTlsIo::new(inner, 4096);
        assert_eq!(io.packet_id(), 0);
        io.write_all(b"first").await.unwrap();
        io.flush().await.unwrap();
        assert_eq!(io.packet_id(), 1);
        io.write_all(b"second").await.unwrap();
        io.flush().await.unwrap();
        assert_eq!(io.packet_id(), 2);

        let first = read_n(&mut peer, HEADER_LEN + 5).await;
        assert_eq!(
            &first[..8],
            &[0x12, 0x01, 0x00, 0x0D, 0x00, 0x00, 0x01, 0x00]
        );
        assert_eq!(&first[8..], b"first");
        let second = read_n(&mut peer, HEADER_LEN + 6).await;
        assert_eq!(
            &second[..8],
            &[0x12, 0x01, 0x00, 0x0E, 0x00, 0x00, 0x02, 0x00]
        );
        assert_eq!(&second[8..], b"second");
    }

    #[tokio::test]
    async fn wrapper_rejects_non_prelogin_packet() {
        let (mut peer, inner) = duplex(1 << 16);
        peer.write_all(&packet(PacketType::SqlBatch, 0x01, 1, b"SELECT 1"))
            .await
            .unwrap();
        let mut io = PreloginTlsIo::new(inner, 4096);
        let mut buf = [0u8; 32];
        let err = io.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn wrapper_passthrough_is_transparent() {
        let (mut peer, inner) = duplex(1 << 16);
        let mut io = PreloginTlsIo::new(inner, 4096);
        io.set_passthrough();
        assert_eq!(io.mode(), Mode::Passthrough);

        io.write_all(b"raw out").await.unwrap();
        io.flush().await.unwrap();
        assert_eq!(read_n(&mut peer, 7).await, b"raw out");

        peer.write_all(b"raw in").await.unwrap();
        assert_eq!(read_n(&mut io, 6).await, b"raw in");
        assert_eq!(io.packet_id(), 0);

        let mut inner = io.into_inner();
        peer.write_all(b"direct").await.unwrap();
        assert_eq!(read_n(&mut inner, 6).await, b"direct");
    }

    #[tokio::test]
    async fn handshake_through_two_wrappers() {
        let (server_config, client_config) = test_tls_configs();
        let (client_io, server_io) = duplex(1 << 16);
        let acceptor = TlsAcceptor::from(server_config);
        let connector = TlsConnector::from(client_config);
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();

        let (server, client) = tokio::join!(
            acceptor.accept(PreloginTlsIo::new(server_io, 4096)),
            connector.connect(server_name, PreloginTlsIo::new(client_io, 4096)),
        );
        let mut server = server.expect("server handshake");
        let mut client = client.expect("client handshake");
        assert_eq!(
            server.get_ref().1.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_2)
        );
        // Every handshake flight went out as at least one PRELOGIN packet on each side.
        assert!(server.get_ref().0.packet_id() >= 1);
        assert!(client.get_ref().0.packet_id() >= 1);

        server.get_mut().0.set_passthrough();
        client.get_mut().0.set_passthrough();

        let from_server: Vec<u8> = (0..100u8).collect();
        let from_client: Vec<u8> = (100..200u8).collect();
        server.write_all(&from_server).await.unwrap();
        server.flush().await.unwrap();
        assert_eq!(read_n(&mut client, 100).await, from_server);
        client.write_all(&from_client).await.unwrap();
        client.flush().await.unwrap();
        assert_eq!(read_n(&mut server, 100).await, from_client);
    }

    /// Drives a synchronous rustls handshake in memory until both sides are done or one
    /// of them fails. Returns the first error raised while processing incoming records.
    fn pump(
        client: &mut rustls::ClientConnection,
        server: &mut rustls::ServerConnection,
    ) -> Result<(), rustls::Error> {
        let mut wire = Vec::new();
        while client.is_handshaking() || server.is_handshaking() {
            let mut progressed = false;
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
                progressed = true;
            }
            let mut slice = &wire[..];
            while !slice.is_empty() {
                server.read_tls(&mut slice).unwrap();
            }
            wire.clear();
            server.process_new_packets()?;
            while server.wants_write() {
                server.write_tls(&mut wire).unwrap();
                progressed = true;
            }
            let mut slice = &wire[..];
            while !slice.is_empty() {
                client.read_tls(&mut slice).unwrap();
            }
            wire.clear();
            client.process_new_packets()?;
            assert!(progressed, "handshake stalled");
        }
        Ok(())
    }

    #[test]
    fn fixtures_load() {
        let certs = rustls_pemfile::certs(&mut &TEST_CERT_PEM[..])
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(certs.len(), 1);
        let key = rustls_pemfile::private_key(&mut &TEST_KEY_PEM[..])
            .unwrap()
            .expect("fixture holds a private key");
        let server = Arc::new(
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .unwrap(),
        );
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();

        // TLS 1.2 is the only enabled version: a TLS 1.3-only client is refused...
        let (_, tls12_client) = test_tls_configs();
        let tls13_client = {
            let builder =
                rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13]);
            let verifier = test_support::TrustAnyServerCert {
                provider: builder.crypto_provider().clone(),
            };
            Arc::new(
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(verifier))
                    .with_no_client_auth(),
            )
        };
        let mut client = rustls::ClientConnection::new(tls13_client, server_name.clone()).unwrap();
        let mut srv = rustls::ServerConnection::new(server.clone()).unwrap();
        assert!(matches!(
            pump(&mut client, &mut srv),
            Err(rustls::Error::PeerIncompatible(_))
        ));

        // ...and a TLS 1.2 client negotiates TLS 1.2 with the fixture certificate.
        let mut client = rustls::ClientConnection::new(tls12_client, server_name).unwrap();
        let mut srv = rustls::ServerConnection::new(server).unwrap();
        pump(&mut client, &mut srv).unwrap();
        assert_eq!(
            srv.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_2)
        );
        assert_eq!(
            client.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_2)
        );
    }
}
