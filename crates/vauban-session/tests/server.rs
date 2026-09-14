//! Integration tests of `Server`: PRELOGIN through the acceptance loop,
//! resilience to garbage, graceful shutdown and SPID assignment.
//!
//! The client side is written by hand from [MS-TDS] 2.2.3.1 (packet header) and
//! 2.2.6.5 (PRELOGIN): no driver is involved.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Instrument, Subscriber, info_span};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use vauban_errors::{InternalError, SqlResult};
use vauban_session::{Authenticator, EncryptPolicy, Engine, Principal, Server, ServerConfig};
use vauban_storage::MemoryStorage;

// ---------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------

fn bug<T>() -> SqlResult<T> {
    Err(InternalError::Bug("NoLogin".into()).into())
}

/// No login is attempted by these tests: authenticating is a bug here.
struct NoLogin;

impl Authenticator for NoLogin {
    fn authenticate(&self, _user: &str, _password: &str) -> SqlResult<Principal> {
        bug()
    }
}

fn new_server() -> Server {
    Server::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(NoLogin),
            server_name: "vauban-test".into(),
            default_packet_size: 4096,
            program_name: None,
            version_banner: None,
            edition: None,
        },
    )
}

// ---------------------------------------------------------------------------------------
// Span collection: the SPID of every `connection` span, per server under test
// ---------------------------------------------------------------------------------------
//
// One subscriber for the whole test binary, installed once as the global default, and a
// `harness{port}` span that each test enters around its own `serve`: the `connection`
// spans of a server are the children of its harness span, which is how the collector
// tells the servers of concurrent tests apart.
//
// Why global and not scoped to one test (`WithSubscriber`): `tracing-core` caches the
// interest of every callsite once for the whole process. While a single dispatcher is
// registered, that interest is computed against the default dispatcher of *the thread
// that first hits the callsite*. The `info_span!("connection", …)` callsite is shared by
// every test of this binary: whenever another test, running with no subscriber, reached
// it first, `Interest::never()` was cached and the SPID test got `Span::none()`, hence an
// empty collection under parallel execution only. A global subscriber makes the interest the
// same on every thread.

/// Every `(harness port, spid)` pair seen since the subscriber was installed.
type SpidLog = Arc<Mutex<Vec<(u16, i64)>>>;

/// Port of the server a `harness` span belongs to, stored in the span's extensions.
struct HarnessPort(u16);

/// Records the `spid` field of every span named `connection` created under a `harness`
/// span, tagged with the `port` field of that harness span.
struct SpidCapture {
    log: SpidLog,
}

impl<S> Layer<S> for SpidCapture
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
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
                    self.log.lock().unwrap().push((port, spid));
                }
            }
            _ => {}
        }
    }
}

/// The two integer fields the collector reads: `spid` of a `connection` span and `port` of
/// a `harness` span.
#[derive(Default)]
struct Fields {
    spid: Option<i64>,
    port: Option<u16>,
}

impl Visit for Fields {
    fn record_i64(&mut self, field: &Field, value: i64) {
        match field.name() {
            "spid" => self.spid = Some(value),
            "port" => self.port = u16::try_from(value).ok(),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "spid" => self.spid = i64::try_from(value).ok(),
            "port" => self.port = u16::try_from(value).ok(),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// Installs the capturing subscriber as the global default, once per process, and returns
/// the shared log. Every test calls it before creating any span or event, so that no
/// callsite is ever registered against the `NoSubscriber` default.
fn install_tracing() -> SpidLog {
    static LOG: OnceLock<SpidLog> = OnceLock::new();
    LOG.get_or_init(|| {
        let log: SpidLog = Arc::default();
        let subscriber = tracing_subscriber::registry().with(SpidCapture {
            log: Arc::clone(&log),
        });
        tracing::subscriber::set_global_default(subscriber)
            .expect("the test binary installs its subscriber once");
        log
    })
    .clone()
}

/// View of the log restricted to one server: the SPIDs of its connections, in order.
struct Spids {
    log: SpidLog,
    port: u16,
}

impl Spids {
    fn collect(&self) -> Vec<i64> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(port, _)| *port == self.port)
            .map(|(_, spid)| *spid)
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------------------

struct Running {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), InternalError>>,
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
}

async fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

async fn start() -> Running {
    install_tracing();
    let (listener, addr) = bind().await;
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(new_server().serve(listener, shutdown.clone()));
    Running {
        addr,
        shutdown,
        task,
    }
}

/// Like `start`, with `serve` run under a `harness{port}` span so that the global collector
/// (see `install_tracing`) can single out the `connection` spans of this server.
async fn start_capturing_spids() -> (Running, Spids) {
    let log = install_tracing();
    let (listener, addr) = bind().await;
    let shutdown = CancellationToken::new();
    let harness = info_span!("harness", port = addr.port());
    let task = tokio::spawn(
        new_server()
            .serve(listener, shutdown.clone())
            .instrument(harness),
    );
    let running = Running {
        addr,
        shutdown,
        task,
    };
    let spids = Spids {
        log,
        port: addr.port(),
    };
    (running, spids)
}

// ---------------------------------------------------------------------------------------
// Hand-made TDS client
// ---------------------------------------------------------------------------------------

/// Packet type PRELOGIN ([MS-TDS] 2.2.3.1.1).
const PACKET_PRELOGIN: u8 = 0x12;
/// Packet type "Tabular result" ([MS-TDS] 2.2.3.1.1): the type of every server response,
/// PRELOGIN response included ([MS-TDS] 2.2.6.5).
const PACKET_TABULAR_RESULT: u8 = 0x04;
/// Status EOM ([MS-TDS] 2.2.3.1.2).
const STATUS_EOM: u8 = 0x01;
/// `PL_OPTION_TOKEN` VERSION, ENCRYPTION and TERMINATOR ([MS-TDS] 2.2.6.5).
const OPTION_VERSION: u8 = 0x00;
const OPTION_ENCRYPTION: u8 = 0x01;
const OPTION_TERMINATOR: u8 = 0xFF;
/// `B_FENCRYPTION` ENCRYPT_NOT_SUP ([MS-TDS] 2.2.6.5).
const ENCRYPT_NOT_SUP: u8 = 0x02;

/// A minimal client PRELOGIN message ([MS-TDS] 2.2.6.5): options VERSION and ENCRYPTION
/// (ENCRYPT_NOT_SUP), in one packet with the EOM status ([MS-TDS] 2.2.3.1).
fn prelogin_packet() -> Vec<u8> {
    // Option headers: PL_OPTION_TOKEN (1) + PL_OFFSET (2, big-endian) + PL_OPTION_LENGTH
    // (2, big-endian), then TERMINATOR, then the option data. Offsets count from the
    // start of the payload.
    let version: [u8; 6] = [0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00]; // 15.0.2000, sub-build 0
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
    payload.push(ENCRYPT_NOT_SUP);

    // Packet header ([MS-TDS] 2.2.3.1): Type, Status, Length (big-endian, header
    // included), SPID (0 before login), PacketID, Window.
    let mut packet = vec![PACKET_PRELOGIN, STATUS_EOM];
    packet.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x00]);
    packet.push(0x01);
    packet.push(0x00);
    packet.extend_from_slice(&payload);
    packet
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

/// Value of a PRELOGIN option in a decoded payload, `None` when absent or empty.
fn prelogin_option(payload: &[u8], token: u8) -> Option<&[u8]> {
    let mut pos = 0;
    while payload[pos] != OPTION_TERMINATOR {
        let header = &payload[pos..pos + 5];
        let offset = usize::from(u16::from_be_bytes([header[1], header[2]]));
        let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if header[0] == token {
            return (len > 0).then(|| &payload[offset..offset + len]);
        }
        pos += 5;
    }
    None
}

/// Connects, sends the PRELOGIN and returns the connection with the response packet.
async fn prelogin(addr: SocketAddr) -> (TcpStream, Vec<u8>) {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&prelogin_packet()).await.unwrap();
    let response = timeout(Duration::from_secs(5), read_packet(&mut client))
        .await
        .expect("PRELOGIN response within 5 s");
    (client, response)
}

/// 16 bytes from a xorshift generator seeded by the clock; the seed is printed so that a
/// failure can be replayed.
fn random_bytes() -> [u8; 16] {
    let seed = Instant::now().elapsed().as_nanos() as u64
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut state = seed | 1;
    println!("random_bytes seed: {seed:#x}");
    let mut out = [0u8; 16];
    for byte in &mut out {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state >> 24) as u8;
    }
    out
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn prelogin_is_answered_with_a_tabular_result_packet() {
    let running = start().await;

    let (_client, response) = prelogin(running.addr).await;
    assert_eq!(response[0], PACKET_TABULAR_RESULT, "response packet type");
    assert_eq!(response[1] & STATUS_EOM, STATUS_EOM, "response carries EOM");
    assert_eq!(
        prelogin_option(&response[8..], OPTION_ENCRYPTION),
        Some(&[ENCRYPT_NOT_SUP][..]),
        "EncryptPolicy::Off answers ENCRYPT_NOT_SUP"
    );
    assert!(
        prelogin_option(&response[8..], OPTION_VERSION).is_some(),
        "the response carries VERSION"
    );

    running.stop().await.unwrap();
}

#[tokio::test]
async fn garbage_does_not_bring_the_server_down() {
    let running = start().await;

    // A client that speaks anything but TDS, then leaves.
    let mut garbage = TcpStream::connect(running.addr).await.unwrap();
    garbage.write_all(&random_bytes()).await.unwrap();
    garbage.shutdown().await.unwrap();
    // The server closes on its side (error or peer close): reading ends, one way or the
    // other, instead of hanging.
    let mut sink = Vec::new();
    let _ = timeout(Duration::from_secs(5), garbage.read_to_end(&mut sink))
        .await
        .expect("the server closes the garbage connection within 5 s");
    drop(garbage);

    // The next client is served normally.
    let (_client, response) = prelogin(running.addr).await;
    assert_eq!(response[0], PACKET_TABULAR_RESULT);

    running.stop().await.unwrap();
}

#[tokio::test]
async fn shutdown_closes_open_connections_and_returns_ok() {
    let running = start().await;

    // A connection accepted by the server (its PRELOGIN was answered) that then stays
    // silent, as a driver waiting between two batches would.
    let (mut idle, response) = prelogin(running.addr).await;
    assert_eq!(response[0], PACKET_TABULAR_RESULT);

    let started = Instant::now();
    let outcome = running.stop().await;
    assert!(outcome.is_ok(), "serve returned {outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "serve took {:?} to return",
        started.elapsed()
    );

    // The server dropped the idle connection: the client reads end-of-stream.
    let mut buf = [0u8; 8];
    let read = timeout(Duration::from_secs(5), idle.read(&mut buf))
        .await
        .expect("the idle connection is closed within 5 s")
        .expect("closed cleanly, not reset");
    assert_eq!(read, 0, "client read after shutdown");
}

#[tokio::test]
async fn successive_connections_get_spids_51_then_52() {
    let (running, spids) = start_capturing_spids().await;

    for _ in 0..2 {
        let (client, response) = prelogin(running.addr).await;
        assert_eq!(response[0], PACKET_TABULAR_RESULT);
        drop(client);
    }

    running.stop().await.unwrap();
    assert_eq!(spids.collect(), vec![51, 52]);
}
