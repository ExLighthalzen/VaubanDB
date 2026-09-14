//! Integration tests of the login: PRELOGIN then a hand-made LOGIN7 through the
//! acceptance loop, the login response, the refusals and the journal.
//!
//! The client side is written by hand from [MS-TDS] 2.2.3.1 (packet header), 2.2.6.4
//! (LOGIN7, password obfuscation), 2.2.6.5 (PRELOGIN) and 2.2.7 (tokens): no driver is
//! involved.

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Instrument, Subscriber, info_span};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use vauban_errors::InternalError;
use vauban_session::{
    Authenticator, EncryptPolicy, Engine, NoAuth, SaPasswordAuthenticator, Server, ServerConfig,
};
use vauban_storage::MemoryStorage;

/// The `sa` password of every server under test that checks passwords.
const SECRET: &str = "Secret1!";
/// A password that is not [`SECRET`].
const WRONG: &str = "wrong";
/// Packet size in force before the login; also the `default_packet_size` of the servers.
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// `TDSVersion` of TDS 7.4 ([MS-TDS] 2.2.6.4).
const TDS_7_4: u32 = 0x7400_0004;
/// First SPID a server hands out.
const FIRST_SPID: u16 = 51;

// ---------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------

fn new_server(authenticator: Arc<dyn Authenticator>) -> Server {
    server_with(authenticator, EncryptPolicy::Off, None)
}

fn server_with(
    authenticator: Arc<dyn Authenticator>,
    encrypt: EncryptPolicy,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> Server {
    Server::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        ServerConfig {
            encrypt,
            tls,
            authenticator,
            server_name: "vauban-test".into(),
            default_packet_size: DEFAULT_PACKET_SIZE,
            program_name: None,
            version_banner: None,
            edition: None,
        },
    )
}

/// A server whose LOGINACK carries `program_name`, `Off` policy like [`new_server`].
fn server_with_program_name(program_name: &str) -> Server {
    Server::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(NoAuth),
            server_name: "vauban-test".into(),
            default_packet_size: DEFAULT_PACKET_SIZE,
            program_name: Some(program_name.to_owned()),
            version_banner: None,
            edition: None,
        },
    )
}

// ---------------------------------------------------------------------------------------
// Journal collection: every span and event rendered to text, plus the SPID of every
// `connection` span, per server under test
// ---------------------------------------------------------------------------------------
//
// One subscriber for the whole test binary, installed once as the global default (the
// interest of a callsite is cached process-wide by `tracing-core`: a per-test subscriber
// is unreliable, see `tests/server.rs`), and a `harness{port}` span that each test enters
// around its own `serve`, so that the `connection` spans of a server can be told apart.
// No filter: every level down to `trace` is kept, which is what the password test
// needs.

/// What the subscriber accumulates.
#[derive(Default)]
struct Journal {
    /// Every span (on creation) and event, rendered as `name{field=value …}`.
    lines: Vec<String>,
    /// `(harness port, spid)` of every `connection` span.
    spids: Vec<(u16, u16)>,
}

type SharedJournal = Arc<Mutex<Journal>>;

/// Port of the server a `harness` span belongs to, stored in the span's extensions.
struct HarnessPort(u16);

struct Collector {
    journal: SharedJournal,
}

impl<S> Layer<S> for Collector
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::new(attrs.metadata().name());
        attrs.record(&mut fields);
        let mut journal = self.journal.lock().unwrap();
        journal.lines.push(fields.finish());
        match attrs.metadata().name() {
            "harness" => {
                if let (Some(port), Some(span)) = (fields.port, ctx.span(id)) {
                    span.extensions_mut().insert(HarnessPort(port));
                }
            }
            "connection" => {
                let port = ctx
                    .span(id)
                    .and_then(|span| span.parent())
                    .and_then(|parent| parent.extensions().get::<HarnessPort>().map(|p| p.0));
                if let (Some(port), Some(spid)) = (port, fields.spid) {
                    journal.spids.push((port, spid));
                }
            }
            _ => {}
        }
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::new(event.metadata().target());
        event.record(&mut fields);
        self.journal.lock().unwrap().lines.push(fields.finish());
    }
}

/// Renders every field of a span or event, and keeps the two integers the SPID collector
/// reads (`spid` of a `connection` span, `port` of a `harness` span).
struct Fields {
    text: String,
    spid: Option<u16>,
    port: Option<u16>,
}

impl Fields {
    fn new(name: &str) -> Self {
        Self {
            text: format!("{name}{{"),
            spid: None,
            port: None,
        }
    }

    fn finish(&self) -> String {
        format!("{}}}", self.text)
    }

    fn integer(&mut self, field: &Field, value: i128) {
        match field.name() {
            "spid" => self.spid = u16::try_from(value).ok(),
            "port" => self.port = u16::try_from(value).ok(),
            _ => {}
        }
    }
}

impl Visit for Fields {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.integer(field, i128::from(value));
        self.record_debug(field, &value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.integer(field, i128::from(value));
        self.record_debug(field, &value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_debug(field, &value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.text, " {}={value:?}", field.name());
    }
}

/// Installs the capturing subscriber as the global default, once per process, and returns
/// the shared journal. Called before any span or event, so that no callsite is ever
/// registered against the `NoSubscriber` default.
fn install_tracing() -> SharedJournal {
    static JOURNAL: OnceLock<SharedJournal> = OnceLock::new();
    JOURNAL
        .get_or_init(|| {
            let journal: SharedJournal = Arc::default();
            let subscriber = tracing_subscriber::registry().with(Collector {
                journal: Arc::clone(&journal),
            });
            tracing::subscriber::set_global_default(subscriber)
                .expect("the test binary installs its subscriber once");
            journal
        })
        .clone()
}

// ---------------------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------------------

struct Running {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), InternalError>>,
    journal: SharedJournal,
}

impl Running {
    /// Cancels the shutdown token and waits for `serve` to return, at most six seconds.
    async fn stop(self) -> Result<(), InternalError> {
        self.shutdown.cancel();
        timeout(Duration::from_secs(6), self.task)
            .await
            .expect("serve must return within 6 s of shutdown")
            .expect("serve task must not panic")
    }

    /// SPIDs of the `connection` spans of this server, in order.
    fn spids(&self) -> Vec<u16> {
        let port = self.addr.port();
        self.journal
            .lock()
            .unwrap()
            .spids
            .iter()
            .filter(|(p, _)| *p == port)
            .map(|(_, spid)| *spid)
            .collect()
    }
}

/// Starts a server with `authenticator`, under a `harness{port}` span.
async fn start(authenticator: Arc<dyn Authenticator>) -> Running {
    start_server(new_server(authenticator)).await
}

/// Starts `server` on an ephemeral port, under a `harness{port}` span.
async fn start_server(server: Server) -> Running {
    let journal = install_tracing();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let harness = info_span!("harness", port = addr.port());
    let task = tokio::spawn(server.serve(listener, shutdown.clone()).instrument(harness));
    Running {
        addr,
        shutdown,
        task,
        journal,
    }
}

async fn start_no_auth() -> Running {
    start(Arc::new(NoAuth)).await
}

async fn start_sa() -> Running {
    start(Arc::new(SaPasswordAuthenticator(SECRET.into()))).await
}

/// A server whose policy is `Optional`: a client announcing ENCRYPT_OFF encrypts its
/// LOGIN7 only ([MS-TDS] 2.2.6.5, `TlsMode::LoginOnly`).
async fn start_login_only(authenticator: Arc<dyn Authenticator>) -> Running {
    start_server(server_with(
        authenticator,
        EncryptPolicy::Optional,
        Some(server_tls_config()),
    ))
    .await
}

// ---------------------------------------------------------------------------------------
// Hand-made TDS client
// ---------------------------------------------------------------------------------------

/// Packet types ([MS-TDS] 2.2.3.1.1).
const PACKET_LOGIN7: u8 = 0x10;
const PACKET_PRELOGIN: u8 = 0x12;
const PACKET_TABULAR_RESULT: u8 = 0x04;
/// Status EOM ([MS-TDS] 2.2.3.1.2).
const STATUS_EOM: u8 = 0x01;
/// `PL_OPTION_TOKEN` VERSION, ENCRYPTION and TERMINATOR ([MS-TDS] 2.2.6.5).
const OPTION_VERSION: u8 = 0x00;
const OPTION_ENCRYPTION: u8 = 0x01;
const OPTION_TERMINATOR: u8 = 0xFF;
/// `B_FENCRYPTION` ENCRYPT_NOT_SUP and ENCRYPT_OFF ([MS-TDS] 2.2.6.5); ENCRYPT_OFF against
/// an `Optional` policy encrypts the LOGIN7 and nothing else.
const ENCRYPT_NOT_SUP: u8 = 0x02;
const ENCRYPT_OFF: u8 = 0x00;
/// `fIntSecurity` bit of `OptionFlags2` ([MS-TDS] 2.2.6.4).
const F_INT_SECURITY: u8 = 0x80;
/// Token types ([MS-TDS] 2.2.7).
const TOKEN_ERROR: u8 = 0xAA;
const TOKEN_INFO: u8 = 0xAB;
const TOKEN_LOGINACK: u8 = 0xAD;
const TOKEN_ENVCHANGE: u8 = 0xE3;
const TOKEN_DONE: u8 = 0xFD;
/// `Status` bit `DONE_ERROR` ([MS-TDS] 2.2.7, DONE).
const DONE_ERROR: u16 = 0x0002;

/// Wraps `payload` in one packet of type `kind` with the EOM status, SPID 0 and
/// PacketID 1 ([MS-TDS] 2.2.3.1).
fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![kind, STATUS_EOM];
    packet.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x00]);
    packet.push(0x01);
    packet.push(0x00);
    packet.extend_from_slice(payload);
    packet
}

/// A minimal client PRELOGIN ([MS-TDS] 2.2.6.5): VERSION and ENCRYPTION (ENCRYPT_NOT_SUP).
fn prelogin_packet() -> Vec<u8> {
    prelogin_packet_with(ENCRYPT_NOT_SUP)
}

/// The same PRELOGIN with the `B_FENCRYPTION` byte of the caller's choosing.
fn prelogin_packet_with(encryption: u8) -> Vec<u8> {
    let version: [u8; 6] = [0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00];
    let data_start = (2 * 5 + 1) as u16;
    let mut payload = Vec::new();
    payload.push(OPTION_VERSION);
    payload.extend_from_slice(&data_start.to_be_bytes());
    payload.extend_from_slice(&(version.len() as u16).to_be_bytes());
    payload.push(OPTION_ENCRYPTION);
    payload.extend_from_slice(&(data_start + version.len() as u16).to_be_bytes());
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.push(OPTION_TERMINATOR);
    payload.extend_from_slice(&version);
    payload.push(encryption);
    packet(PACKET_PRELOGIN, &payload)
}

/// The variable part of a LOGIN7 under test.
struct Login {
    username: &'static str,
    password: &'static str,
    database: &'static str,
    tds_version: u32,
    packet_size: u32,
    sspi: bool,
}

impl Login {
    fn sa(password: &'static str) -> Self {
        Self {
            username: "sa",
            password,
            database: "",
            tds_version: TDS_7_4,
            packet_size: u32::from(DEFAULT_PACKET_SIZE),
            sspi: false,
        }
    }
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Client-side password obfuscation ([MS-TDS] 2.2.6.4, Password): for every byte of the
/// UTF-16LE text, swap the two nibbles, then XOR with 0xA5.
fn obfuscate_password(password: &str) -> Vec<u8> {
    utf16le(password)
        .into_iter()
        .map(|byte| byte.rotate_left(4) ^ 0xA5)
        .collect()
}

/// A LOGIN7 message ([MS-TDS] 2.2.6.4) in one packet: the 94-byte fixed part (TDS 7.2 and
/// later layout) followed by the data section. Every `(ibX, cchX)` pair is little-endian;
/// `cch` counts UTF-16 code units.
fn login7_packet(login: &Login) -> Vec<u8> {
    const FIXED_LEN: usize = 94;

    // Data section, appended in field order; `pairs` gets `(ib, cch)` per field.
    let mut data = Vec::new();
    let mut pairs = Vec::new();
    let mut push = |bytes: Vec<u8>, units: usize| {
        pairs.push(((FIXED_LEN + data.len()) as u16, units as u16));
        data.extend_from_slice(&bytes);
    };
    let text_fields = [
        "testhost",
        login.username,
        "", // password: placed by hand below
        "vauban-login-test",
        "localhost",
        "", // Unused / Extension
        "hand-made",
        "", // Language
        login.database,
    ];
    for (index, field) in text_fields.iter().enumerate() {
        if index == 2 {
            let obfuscated = obfuscate_password(login.password);
            let units = login.password.encode_utf16().count();
            push(obfuscated, units);
        } else {
            push(utf16le(field), field.encode_utf16().count());
        }
    }

    let mut fixed = Vec::with_capacity(FIXED_LEN);
    fixed.extend_from_slice(&((FIXED_LEN + data.len()) as u32).to_le_bytes()); // Length
    fixed.extend_from_slice(&login.tds_version.to_le_bytes()); // TDSVersion
    fixed.extend_from_slice(&login.packet_size.to_le_bytes()); // PacketSize
    fixed.extend_from_slice(&0u32.to_le_bytes()); // ClientProgVer
    fixed.extend_from_slice(&4242u32.to_le_bytes()); // ClientPID
    fixed.extend_from_slice(&0u32.to_le_bytes()); // ConnectionID
    let option_flags2 = if login.sspi { F_INT_SECURITY } else { 0 };
    fixed.extend_from_slice(&[0x00, option_flags2, 0x00, 0x00]); // OptionFlags1..3
    fixed.extend_from_slice(&0i32.to_le_bytes()); // ClientTimeZone
    fixed.extend_from_slice(&0x0409u32.to_le_bytes()); // ClientLCID
    for (ib, cch) in &pairs {
        fixed.extend_from_slice(&ib.to_le_bytes());
        fixed.extend_from_slice(&cch.to_le_bytes());
    }
    fixed.extend_from_slice(&[0u8; 6]); // ClientID
    fixed.extend_from_slice(&[0u8; 4]); // ibSSPI, cbSSPI
    fixed.extend_from_slice(&[0u8; 4]); // ibAtchDBFile, cchAtchDBFile
    fixed.extend_from_slice(&[0u8; 4]); // ibChangePassword, cchChangePassword
    fixed.extend_from_slice(&0u32.to_le_bytes()); // cbSSPILong
    assert_eq!(fixed.len(), FIXED_LEN);

    fixed.extend_from_slice(&data);
    packet(PACKET_LOGIN7, &fixed)
}

/// Reads one packet: the 8-byte header, then the rest as announced by its Length field.
async fn read_packet(client: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 8];
    client.read_exact(&mut header).await.unwrap();
    let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
    assert!(len >= 8, "packet length {len} shorter than its header");
    let mut packet = header.to_vec();
    packet.resize(len, 0);
    client.read_exact(&mut packet[8..]).await.unwrap();
    packet
}

/// One server response: the SPID of every packet header and the concatenated payloads.
struct Response {
    spids: Vec<u16>,
    payload: Vec<u8>,
}

/// Reads packets until one carries EOM ([MS-TDS] 2.2.3.1.2).
async fn read_response(client: &mut TcpStream) -> Response {
    let mut response = Response {
        spids: Vec::new(),
        payload: Vec::new(),
    };
    loop {
        let packet = timeout(Duration::from_secs(5), read_packet(client))
            .await
            .expect("response packet within 5 s");
        assert_eq!(packet[0], PACKET_TABULAR_RESULT, "response packet type");
        response
            .spids
            .push(u16::from_be_bytes([packet[4], packet[5]]));
        response.payload.extend_from_slice(&packet[8..]);
        if packet[1] & STATUS_EOM == STATUS_EOM {
            return response;
        }
    }
}

/// The tokens of a login response, decoded just enough for the assertions.
#[derive(Debug, PartialEq, Eq)]
enum Tok {
    /// ENVCHANGE with its `Type`.
    EnvChange(u8),
    /// INFO with its `Number`.
    Info(u32),
    /// ERROR with `Number`, `State` and `Class`.
    Error { number: u32, state: u8, class: u8 },
    /// LOGINACK with `TDSVersion`, `ProgName` and the four version bytes.
    LoginAck {
        tds_version: u32,
        prog_name: String,
        version: [u8; 4],
    },
    /// DONE with its `Status`.
    Done(u16),
}

/// Walks a token stream ([MS-TDS] 2.2.7): ENVCHANGE, INFO, ERROR and LOGINACK carry a
/// USHORT length; DONE is 12 bytes after its type (TDS 7.2 and later).
fn tokens(payload: &[u8]) -> Vec<Tok> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < payload.len() {
        let kind = payload[pos];
        pos += 1;
        match kind {
            TOKEN_DONE => {
                let status = u16::from_le_bytes([payload[pos], payload[pos + 1]]);
                pos += 12;
                out.push(Tok::Done(status));
            }
            TOKEN_ENVCHANGE | TOKEN_INFO | TOKEN_ERROR | TOKEN_LOGINACK => {
                let len = usize::from(u16::from_le_bytes([payload[pos], payload[pos + 1]]));
                let body = &payload[pos + 2..pos + 2 + len];
                pos += 2 + len;
                out.push(match kind {
                    TOKEN_ENVCHANGE => Tok::EnvChange(body[0]),
                    TOKEN_INFO => Tok::Info(u32::from_le_bytes(body[0..4].try_into().unwrap())),
                    TOKEN_ERROR => Tok::Error {
                        number: u32::from_le_bytes(body[0..4].try_into().unwrap()),
                        state: body[4],
                        class: body[5],
                    },
                    _ => {
                        // Interface (1), TDSVersion (4, most significant byte first),
                        // ProgName (B_VARCHAR), four version bytes.
                        let tds_version = u32::from_be_bytes(body[1..5].try_into().unwrap());
                        let chars = usize::from(body[5]);
                        let name: Vec<u16> = body[6..6 + 2 * chars]
                            .chunks(2)
                            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                            .collect();
                        let rest = &body[6 + 2 * chars..];
                        Tok::LoginAck {
                            tds_version,
                            prog_name: String::from_utf16(&name).unwrap(),
                            version: rest[0..4].try_into().unwrap(),
                        }
                    }
                });
            }
            other => panic!("unexpected token type 0x{other:02X} at offset {}", pos - 1),
        }
    }
    out
}

/// Connects, completes the PRELOGIN exchange and sends the LOGIN7.
async fn connect_and_login(addr: SocketAddr, login: &Login) -> TcpStream {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&prelogin_packet()).await.unwrap();
    let prelogin = timeout(Duration::from_secs(5), read_packet(&mut client))
        .await
        .expect("PRELOGIN response within 5 s");
    assert_eq!(prelogin[0], PACKET_TABULAR_RESULT);
    client.write_all(&login7_packet(login)).await.unwrap();
    client
}

/// Asserts that the server closed the connection: the next read ends (EOF or reset)
/// without delivering a byte.
async fn assert_closed_by_server(client: &mut TcpStream) {
    let mut buf = [0u8; 8];
    let read = timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .expect("the server closes the connection within 5 s");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "expected end of stream, read {read:?}"
    );
}

// ---------------------------------------------------------------------------------------
// Hand-made login-only client (ENCRYPT_OFF against an `Optional` policy)
// ---------------------------------------------------------------------------------------
//
// [MS-TDS] 3.3.5: with ENCRYPT_OFF the TLS handshake still runs, wrapped in PRELOGIN
// packets, but only the LOGIN7 travels inside TLS records; the server takes the raw socket
// back before answering and the client reads that answer in clear text. The client below
// does exactly that, driving `rustls` by hand (no `tokio-rustls`): the test uses only what
// the crate already depends on.

/// Self-signed test certificate and its PKCS#8 key, shared with the `tds` tests
/// (`CN=localhost`).
const CERT_PEM: &[u8] = include_bytes!("../../vauban-tds/tests/fixtures/test-cert.pem");
const KEY_PEM: &[u8] = include_bytes!("../../vauban-tds/tests/fixtures/test-key.pem");

/// Server TLS configuration built from the `tds` fixtures, **TLS 1.2 only**: with TLS 1.3
/// the server sends post-handshake messages that a TDS 7.4 client cannot receive inside a
/// PRELOGIN packet. The PEM is decoded through `rustls::pki_types`, no extra dependency.
fn server_tls_config() -> Arc<rustls::ServerConfig> {
    let certs = CertificateDer::pem_slice_iter(CERT_PEM)
        .collect::<Result<Vec<_>, _>>()
        .expect("test certificate fixture is valid PEM");
    let key = PrivateKeyDer::from_pem_slice(KEY_PEM).expect("test key fixture is valid PEM");
    let config = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("test certificate and key match");
    Arc::new(config)
}

/// Client configuration matching [`server_tls_config`]: TLS 1.2 only, any certificate
/// trusted (the equivalent of `TrustServerCertificate=true`), signatures still verified.
fn client_tls_config() -> Arc<rustls::ClientConfig> {
    let builder = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12]);
    let verifier = trust_any::TrustAnyServerCert {
        provider: builder.crypto_provider().clone(),
    };
    Arc::new(
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth(),
    )
}

/// Test-only certificate verifier: accepts any server certificate, verifies signatures.
mod trust_any {
    use std::sync::Arc;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};

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

/// Reads one server handshake flight: PRELOGIN packets until EOM, payloads concatenated.
async fn read_handshake_flight(client: &mut TcpStream) -> Vec<u8> {
    let mut payload = Vec::new();
    loop {
        let packet = timeout(Duration::from_secs(5), read_packet(client))
            .await
            .expect("handshake flight within 5 s");
        assert_eq!(
            packet[0], PACKET_PRELOGIN,
            "a handshake flight travels in PRELOGIN packets"
        );
        payload.extend_from_slice(&packet[8..]);
        if packet[1] & STATUS_EOM == STATUS_EOM {
            return payload;
        }
    }
}

/// Everything `conn` wants to write, taken out at once.
fn pending_tls(conn: &mut rustls::ClientConnection) -> Vec<u8> {
    let mut out = Vec::new();
    while conn.wants_write() {
        conn.write_tls(&mut out).expect("writing to a Vec succeeds");
    }
    out
}

/// Connects, sends a PRELOGIN with ENCRYPT_OFF, runs the TLS handshake inside PRELOGIN
/// packets, then sends the LOGIN7 as TLS records. The returned socket is back in clear
/// text: what the server sends next must be readable without TLS ([MS-TDS] 3.3.5).
async fn connect_and_login_login_only(addr: SocketAddr, login: &Login) -> TcpStream {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&prelogin_packet_with(ENCRYPT_OFF))
        .await
        .unwrap();
    let prelogin = timeout(Duration::from_secs(5), read_packet(&mut client))
        .await
        .expect("PRELOGIN response within 5 s");
    assert_eq!(prelogin[0], PACKET_TABULAR_RESULT);
    // The last byte of the response payload is the ENCRYPTION option: ENCRYPT_OFF, the
    // server agrees to encrypt the login only.
    assert_eq!(prelogin[prelogin.len() - 1], ENCRYPT_OFF);

    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = rustls::ClientConnection::new(client_tls_config(), server_name).unwrap();
    // One flight out (wrapped in a PRELOGIN message), one flight in, until the client is
    // done: from then on the records go straight to the socket.
    loop {
        let out = pending_tls(&mut conn);
        if !out.is_empty() {
            client
                .write_all(&packet(PACKET_PRELOGIN, &out))
                .await
                .unwrap();
        }
        if !conn.is_handshaking() {
            break;
        }
        let flight = read_handshake_flight(&mut client).await;
        let mut rest = &flight[..];
        while !rest.is_empty() {
            conn.read_tls(&mut rest).expect("reading from a slice");
            conn.process_new_packets().expect("TLS handshake");
        }
    }

    // The LOGIN7 is the only encrypted message; the response comes back in clear text.
    conn.writer().write_all(&login7_packet(login)).unwrap();
    let records = pending_tls(&mut conn);
    client.write_all(&records).await.unwrap();
    client
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn no_auth_login_is_acknowledged_with_the_spid_of_the_connection() {
    let running = start_no_auth().await;

    let mut client = connect_and_login(running.addr, &Login::sa("")).await;
    let response = read_response(&mut client).await;
    let toks = tokens(&response.payload);
    assert_eq!(
        toks,
        vec![
            Tok::EnvChange(1),
            Tok::Info(5701),
            Tok::EnvChange(7),
            Tok::EnvChange(2),
            Tok::Info(5703),
            Tok::EnvChange(4),
            Tok::LoginAck {
                tds_version: TDS_7_4,
                prog_name: "VaubanDB".into(),
                version: [16, 0, 0x10, 0xB3],
            },
            Tok::Done(0),
        ]
    );

    // Every packet of the response carries the SPID of the connection span.
    assert!(!response.spids.is_empty());
    assert!(
        response.spids.iter().all(|spid| *spid == FIRST_SPID),
        "header SPIDs: {:?}",
        response.spids
    );
    // The connection stays open after the login: the client leaves first.
    drop(client);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn header_spid_matches_the_connection_span() {
    let running = start_no_auth().await;

    let mut client = connect_and_login(running.addr, &Login::sa("")).await;
    let response = read_response(&mut client).await;
    drop(client);

    let span_spids = running.spids();
    assert_eq!(span_spids, vec![FIRST_SPID]);
    assert_eq!(response.spids[0], span_spids[0]);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn wrong_password_gets_18456_and_the_connection_is_closed() {
    let running = start_sa().await;

    let mut client = connect_and_login(running.addr, &Login::sa(WRONG)).await;
    let response = read_response(&mut client).await;
    assert_eq!(
        tokens(&response.payload),
        vec![
            Tok::Error {
                number: 18456,
                state: 1,
                class: 14,
            },
            Tok::Done(DONE_ERROR),
        ]
    );
    assert_closed_by_server(&mut client).await;

    running.stop().await.unwrap();
}

#[tokio::test]
async fn right_password_is_accepted_by_the_sa_authenticator() {
    let running = start_sa().await;

    let mut client = connect_and_login(running.addr, &Login::sa(SECRET)).await;
    let response = read_response(&mut client).await;
    let toks = tokens(&response.payload);
    assert!(matches!(toks[6], Tok::LoginAck { .. }), "{toks:?}");
    assert_eq!(toks.last(), Some(&Tok::Done(0)));

    running.stop().await.unwrap();
}

#[tokio::test]
async fn sspi_request_gets_18452_and_the_connection_is_closed() {
    let running = start_no_auth().await;

    let login = Login {
        sspi: true,
        ..Login::sa("")
    };
    let mut client = connect_and_login(running.addr, &login).await;
    let response = read_response(&mut client).await;
    assert_eq!(
        tokens(&response.payload),
        vec![
            Tok::Error {
                number: 18452,
                state: 1,
                class: 14,
            },
            Tok::Done(DONE_ERROR),
        ]
    );
    assert_closed_by_server(&mut client).await;

    running.stop().await.unwrap();
}

#[tokio::test]
async fn unknown_database_gets_4060_then_18456_then_done_error() {
    let running = start_no_auth().await;

    let login = Login {
        database: "nope",
        ..Login::sa("")
    };
    let mut client = connect_and_login(running.addr, &login).await;
    let response = read_response(&mut client).await;
    assert_eq!(
        tokens(&response.payload),
        vec![
            Tok::Error {
                number: 4060,
                state: 1,
                class: 11,
            },
            Tok::Error {
                number: 18456,
                state: 1,
                class: 14,
            },
            Tok::Done(DONE_ERROR),
        ]
    );
    assert_closed_by_server(&mut client).await;

    running.stop().await.unwrap();
}

#[tokio::test]
async fn master_in_any_case_is_accepted() {
    let running = start_no_auth().await;

    let login = Login {
        database: "MASTER",
        ..Login::sa("")
    };
    let mut client = connect_and_login(running.addr, &login).await;
    let response = read_response(&mut client).await;
    assert_eq!(tokens(&response.payload).last(), Some(&Tok::Done(0)));

    running.stop().await.unwrap();
}

#[tokio::test]
async fn requested_packet_size_is_confirmed_by_envchange() {
    let running = start_no_auth().await;

    let login = Login {
        packet_size: 8000,
        ..Login::sa("")
    };
    let mut client = connect_and_login(running.addr, &login).await;
    let response = read_response(&mut client).await;
    // ENVCHANGE type 4: NewValue then OldValue as B_VARCHAR of decimal text.
    let expected_body: Vec<u8> = {
        let mut body = vec![4u8];
        for text in ["8000", "4096"] {
            body.push(text.len() as u8);
            body.extend(utf16le(text));
        }
        body
    };
    let found = response
        .payload
        .windows(expected_body.len())
        .any(|window| window == expected_body.as_slice());
    assert!(found, "ENVCHANGE packet size 4096 -> 8000 not found");

    running.stop().await.unwrap();
}

#[tokio::test]
async fn tds_older_than_7_2_is_closed_without_a_response() {
    let running = start_no_auth().await;

    let login = Login {
        tds_version: 0x7100_0000,
        ..Login::sa("")
    };
    let mut client = connect_and_login(running.addr, &login).await;
    assert_closed_by_server(&mut client).await;

    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_batch_before_the_login_closes_the_connection() {
    let running = start_no_auth().await;

    let mut client = TcpStream::connect(running.addr).await.unwrap();
    client.write_all(&prelogin_packet()).await.unwrap();
    let _ = read_packet(&mut client).await;
    // A SQL batch (type 0x01) where a LOGIN7 is expected.
    client
        .write_all(&packet(0x01, &utf16le("SELECT 1")))
        .await
        .unwrap();
    assert_closed_by_server(&mut client).await;

    running.stop().await.unwrap();
}

#[tokio::test]
async fn journal_never_contains_the_password() {
    let running = start_sa().await;

    let mut ok = connect_and_login(running.addr, &Login::sa(SECRET)).await;
    let response = read_response(&mut ok).await;
    assert_eq!(tokens(&response.payload).last(), Some(&Tok::Done(0)));
    drop(ok);

    let mut refused = connect_and_login(running.addr, &Login::sa(WRONG)).await;
    let response = read_response(&mut refused).await;
    assert_eq!(
        tokens(&response.payload).last(),
        Some(&Tok::Done(DONE_ERROR))
    );
    assert_closed_by_server(&mut refused).await;

    let journal = Arc::clone(&running.journal);
    running.stop().await.unwrap();

    let lines = journal.lock().unwrap().lines.clone();
    assert!(
        lines.iter().any(|line| line.contains("login succeeded")),
        "the successful login is journaled: {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("error=18456") && line.contains("state=8")),
        "the refusal is journaled with its detailed state: {lines:#?}"
    );
    for line in &lines {
        assert!(!line.contains(SECRET), "password leaked: {line}");
        assert!(!line.contains(WRONG), "wrong password leaked: {line}");
        assert!(
            !line.to_ascii_lowercase().contains("password"),
            "the word password appears: {line}"
        );
    }
}

#[tokio::test]
async fn login_only_wrong_password_gets_18456_in_clear_text() {
    let running = start_login_only(Arc::new(SaPasswordAuthenticator(SECRET.into()))).await;

    let mut client = connect_and_login_login_only(running.addr, &Login::sa(WRONG)).await;
    // The refusal must arrive in clear text: written before the downgrade it would reach a
    // client already back in clear text as TLS records ([MS-TDS] 3.3.5).
    let response = read_response(&mut client).await;
    assert_eq!(
        tokens(&response.payload),
        vec![
            Tok::Error {
                number: 18456,
                state: 1,
                class: 14,
            },
            Tok::Done(DONE_ERROR),
        ]
    );
    assert_closed_by_server(&mut client).await;

    running.stop().await.unwrap();
}

#[tokio::test]
async fn login_only_sspi_request_gets_18452_in_clear_text() {
    let running = start_login_only(Arc::new(NoAuth)).await;

    let login = Login {
        sspi: true,
        ..Login::sa("")
    };
    let mut client = connect_and_login_login_only(running.addr, &login).await;
    let response = read_response(&mut client).await;
    assert_eq!(
        tokens(&response.payload),
        vec![
            Tok::Error {
                number: 18452,
                state: 1,
                class: 14,
            },
            Tok::Done(DONE_ERROR),
        ]
    );
    assert_closed_by_server(&mut client).await;

    running.stop().await.unwrap();
}

#[tokio::test]
async fn login_only_accepted_login_is_answered_in_clear_text() {
    let running = start_login_only(Arc::new(SaPasswordAuthenticator(SECRET.into()))).await;

    let mut client = connect_and_login_login_only(running.addr, &Login::sa(SECRET)).await;
    let response = read_response(&mut client).await;
    let toks = tokens(&response.payload);
    assert!(matches!(toks[6], Tok::LoginAck { .. }), "{toks:?}");
    assert_eq!(toks.last(), Some(&Tok::Done(0)));

    drop(client);
    running.stop().await.unwrap();
}

// ---------------------------------------------------------------------------------------
// A `program_name` LOGINACK cannot carry
// ---------------------------------------------------------------------------------------
//
// `ProgName` is a B_VARCHAR ([MS-TDS] 2.2.7.14, 2.2.5.1): one length byte, then that many
// UTF-16 code units. Without the cut, 255 units log in and 256 close the connection without
// an error token, because the encoder refuses the B_VARCHAR and the whole LOGINACK is lost
// with it: the client of this file then reads an early EOF instead of a response.
//
// The `cli` crate refuses a longer name at startup; what is covered here is the defensive
// cut that protects a `ServerConfig` built without going through `cli`.

/// 255 units arrive entire: the cut is not a cut at the boundary itself.
#[tokio::test]
async fn a_program_name_of_255_units_arrives_entire() {
    let name = "x".repeat(255);
    let running = start_server(server_with_program_name(&name)).await;

    let mut client = connect_and_login(running.addr, &Login::sa("")).await;
    let response = read_response(&mut client).await;
    let toks = tokens(&response.payload);
    let Tok::LoginAck { prog_name, .. } = &toks[6] else {
        panic!("token 6 is the LOGINACK: {toks:?}");
    };
    assert_eq!(prog_name, &name);
    assert_eq!(prog_name.encode_utf16().count(), 255);
    assert_eq!(toks.last(), Some(&Tok::Done(0)));

    drop(client);
    running.stop().await.unwrap();
}

/// 300 units still produce a LOGINACK the client reads, cut to 255 units, and the login
/// completes with a final DONE instead of a closed connection.
#[tokio::test]
async fn a_program_name_of_300_units_is_cut_to_255_and_the_login_completes() {
    let running = start_server(server_with_program_name(&"x".repeat(300))).await;

    let mut client = connect_and_login(running.addr, &Login::sa("")).await;
    let response = read_response(&mut client).await;
    let toks = tokens(&response.payload);
    let Tok::LoginAck { prog_name, .. } = &toks[6] else {
        panic!("token 6 is the LOGINACK: {toks:?}");
    };
    assert_eq!(prog_name.encode_utf16().count(), 255);
    assert_eq!(prog_name, &"x".repeat(255));
    assert_eq!(toks.last(), Some(&Tok::Done(0)));

    drop(client);
    running.stop().await.unwrap();
}

/// The cut lands on a character boundary. Two hundred U+1F3F0 CASTLE weigh 400 UTF-16 code
/// units; a cut counted in units alone would stop inside the 128th surrogate pair and the
/// announced length would cover half of it. `tokens` rebuilds `ProgName` with
/// `String::from_utf16`, which fails on an unpaired surrogate, so what this test asserts is
/// that 127 whole characters (254 units) arrive and decode.
#[tokio::test]
async fn the_cut_of_astral_characters_decodes_back_from_utf16() {
    let running = start_server(server_with_program_name(
        &'\u{1F3F0}'.to_string().repeat(200),
    ))
    .await;

    let mut client = connect_and_login(running.addr, &Login::sa("")).await;
    let response = read_response(&mut client).await;
    let toks = tokens(&response.payload);
    let Tok::LoginAck { prog_name, .. } = &toks[6] else {
        panic!("token 6 is the LOGINACK: {toks:?}");
    };
    assert_eq!(prog_name.encode_utf16().count(), 254);
    assert_eq!(prog_name.chars().count(), 127);
    assert!(prog_name.chars().all(|ch| ch == '\u{1F3F0}'));
    assert_eq!(toks.last(), Some(&Tok::Done(0)));

    drop(client);
    running.stop().await.unwrap();
}
