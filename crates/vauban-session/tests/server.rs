//! Integration tests of `Server`: PRELOGIN through the acceptance loop,
//! resilience to garbage, graceful shutdown, SPID assignment, and the answer a response
//! gets when one of its tokens cannot be encoded.
//!
//! The client side is written by hand from [MS-TDS] 2.2.3.1 (packet header), 2.2.5.3
//! (ALL_HEADERS), 2.2.6.4 (LOGIN7), 2.2.6.5 (PRELOGIN), 2.2.6.7 (SQL_BATCH) and 2.2.7
//! (tokens): no driver is involved.

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
use vauban_session::{
    Authenticator, EncryptPolicy, Engine, NoAuth, Principal, Server, ServerConfig,
};
use vauban_storage::MemoryStorage;

/// Packet size in force before the login; also the `default_packet_size` of the servers
/// started here.
const DEFAULT_PACKET_SIZE: u16 = 4096;

// ---------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------

fn bug<T>() -> SqlResult<T> {
    Err(InternalError::Bug("NoLogin".into()).into())
}

/// No login is attempted by the tests that stop at the PRELOGIN: authenticating is a bug
/// there.
struct NoLogin;

impl Authenticator for NoLogin {
    fn authenticate(&self, _user: &str, _password: &str) -> SqlResult<Principal> {
        bug()
    }
}

/// A server whose `Authenticator` is `authenticator`, over a fresh in-memory engine.
fn new_server_with(authenticator: Arc<dyn Authenticator>) -> Server {
    Server::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator,
            server_name: "vauban-test".into(),
            default_packet_size: DEFAULT_PACKET_SIZE,
            program_name: None,
            version_banner: None,
            edition: None,
        },
    )
}

fn new_server() -> Server {
    new_server_with(Arc::new(NoLogin))
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

/// Like `start`, with an authenticator that accepts the login it is handed: what the tests
/// that go past the LOGIN7 and run batches need.
async fn start_accepting_logins() -> Running {
    install_tracing();
    let (listener, addr) = bind().await;
    let shutdown = CancellationToken::new();
    let server = new_server_with(Arc::new(NoAuth));
    let task = tokio::spawn(server.serve(listener, shutdown.clone()));
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

/// Packet types ([MS-TDS] 2.2.3.1.1). "Tabular result" is the type of every server
/// response, PRELOGIN response included ([MS-TDS] 2.2.6.5).
const PACKET_SQL_BATCH: u8 = 0x01;
const PACKET_TABULAR_RESULT: u8 = 0x04;
const PACKET_LOGIN7: u8 = 0x10;
const PACKET_PRELOGIN: u8 = 0x12;
/// Status EOM ([MS-TDS] 2.2.3.1.2).
const STATUS_EOM: u8 = 0x01;
/// `PL_OPTION_TOKEN` VERSION, ENCRYPTION and TERMINATOR ([MS-TDS] 2.2.6.5).
const OPTION_VERSION: u8 = 0x00;
const OPTION_ENCRYPTION: u8 = 0x01;
const OPTION_TERMINATOR: u8 = 0xFF;
/// `B_FENCRYPTION` ENCRYPT_NOT_SUP ([MS-TDS] 2.2.6.5).
const ENCRYPT_NOT_SUP: u8 = 0x02;
/// `TDSVersion` of TDS 7.4 ([MS-TDS] 2.2.6.4).
const TDS_7_4: u32 = 0x7400_0004;
/// ALL_HEADERS Transaction Descriptor header ([MS-TDS] 2.2.5.3.1).
const HEADER_TRANSACTION_DESCRIPTOR: u16 = 0x0002;
/// Token types ([MS-TDS] 2.2.7).
const TOKEN_COLMETADATA: u8 = 0x81;
const TOKEN_ERROR: u8 = 0xAA;
const TOKEN_INFO: u8 = 0xAB;
const TOKEN_LOGINACK: u8 = 0xAD;
const TOKEN_ROW: u8 = 0xD1;
const TOKEN_NBCROW: u8 = 0xD2;
const TOKEN_ENVCHANGE: u8 = 0xE3;
const TOKEN_DONE: u8 = 0xFD;
/// TYPE_INFO tokens of `int` ([MS-TDS] 2.2.5.4.1 and 2.2.5.4.2): INT4TYPE for a column
/// announced as not nullable, INTNTYPE with a length byte for a nullable one.
const INT4TYPE: u8 = 0x38;
const INTNTYPE: u8 = 0x26;
/// `Status` bits of DONE ([MS-TDS] 2.2.7, DONE).
const DONE_MORE: u16 = 0x0001;
const DONE_ERROR: u16 = 0x0002;
const DONE_COUNT: u16 = 0x0010;
/// `CurCmd` of the DONE of a SELECT.
const CUR_CMD_SELECT: u16 = 0xC1;

/// Wraps `payload` in one packet of type `kind` with the EOM status, SPID 0 and
/// PacketID 1 ([MS-TDS] 2.2.3.1): Type, Status, Length (big-endian, header included),
/// SPID, PacketID, Window.
fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![kind, STATUS_EOM];
    packet.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x00]);
    packet.push(0x01);
    packet.push(0x00);
    packet.extend_from_slice(payload);
    packet
}

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
    packet(PACKET_PRELOGIN, &payload)
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// A LOGIN7 message ([MS-TDS] 2.2.6.4) for `sa` with an empty password (`NoAuth`): the
/// 94-byte fixed part (TDS 7.2 and later layout) followed by the data section.
fn login7_packet() -> Vec<u8> {
    const FIXED_LEN: usize = 94;

    let mut data = Vec::new();
    let mut pairs = Vec::new();
    let text_fields = [
        "testhost",
        "sa",
        "", // password (empty: nothing to obfuscate)
        "vauban-server-test",
        "localhost",
        "", // Unused / Extension
        "hand-made",
        "", // Language
        "", // Database
    ];
    for field in text_fields {
        pairs.push(((FIXED_LEN + data.len()) as u16, field.len() as u16));
        data.extend_from_slice(&utf16le(field));
    }

    let mut fixed = Vec::with_capacity(FIXED_LEN);
    fixed.extend_from_slice(&((FIXED_LEN + data.len()) as u32).to_le_bytes()); // Length
    fixed.extend_from_slice(&TDS_7_4.to_le_bytes()); // TDSVersion
    fixed.extend_from_slice(&u32::from(DEFAULT_PACKET_SIZE).to_le_bytes()); // PacketSize
    fixed.extend_from_slice(&0u32.to_le_bytes()); // ClientProgVer
    fixed.extend_from_slice(&4242u32.to_le_bytes()); // ClientPID
    fixed.extend_from_slice(&0u32.to_le_bytes()); // ConnectionID
    fixed.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // OptionFlags1..3, TypeFlags
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

/// ALL_HEADERS with the Transaction Descriptor header alone ([MS-TDS] 2.2.5.3.1).
fn all_headers() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(22);
    bytes.extend_from_slice(&22u32.to_le_bytes()); // TotalLength
    bytes.extend_from_slice(&18u32.to_le_bytes()); // HeaderLength
    bytes.extend_from_slice(&HEADER_TRANSACTION_DESCRIPTOR.to_le_bytes()); // HeaderType
    bytes.extend_from_slice(&0u64.to_le_bytes()); // TransactionDescriptor
    bytes.extend_from_slice(&1u32.to_le_bytes()); // OutstandingRequestCount
    bytes
}

/// A SQL_BATCH message ([MS-TDS] 2.2.6.7): ALL_HEADERS then `SQLText` in UCS-2.
fn sql_batch_packet(text: &str) -> Vec<u8> {
    let mut payload = all_headers();
    payload.extend_from_slice(&utf16le(text));
    packet(PACKET_SQL_BATCH, &payload)
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

/// Reads packets until one carries EOM ([MS-TDS] 2.2.3.1.2) and returns the concatenated
/// payloads, or `None` when the server hung up before the EOM.
///
/// A request can go unanswered in two ways, and this helper separates them: the server
/// hangs up, which at least wakes the client, or it says nothing, in which case `budget`
/// runs out and the panic names the delay instead of blocking the test run.
async fn read_response_or_close(client: &mut TcpStream, budget: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + budget;
    let mut payload = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let packet = timeout(left, read_packet_or_close(client))
            .await
            .unwrap_or_else(|_| panic!("response packet within {budget:?}"))?;
        assert_eq!(packet[0], PACKET_TABULAR_RESULT, "response packet type");
        payload.extend_from_slice(&packet[8..]);
        if packet[1] & STATUS_EOM == STATUS_EOM {
            return Some(payload);
        }
    }
}

/// One packet, or `None` when the peer closed before sending one.
async fn read_packet_or_close(client: &mut TcpStream) -> Option<Vec<u8>> {
    let mut header = [0u8; 8];
    let mut read = 0;
    while read < header.len() {
        match client.read(&mut header[read..]).await {
            Ok(0) | Err(_) if read == 0 => return None,
            Ok(0) => panic!("connection closed in the middle of a packet header"),
            Ok(n) => read += n,
            Err(err) => panic!("read failed: {err}"),
        }
    }
    let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
    assert!(len >= 8, "packet length {len} shorter than its header");
    let mut packet = header.to_vec();
    packet.resize(len, 0);
    client.read_exact(&mut packet[8..]).await.unwrap();
    Some(packet)
}

/// The response to a request that must be answered: the payload up to the EOM.
async fn read_response(client: &mut TcpStream, budget: Duration) -> Vec<u8> {
    read_response_or_close(client, budget)
        .await
        .expect("the server answers instead of closing the connection")
}

/// The tokens of a response, decoded just enough for the assertions.
#[derive(Debug, PartialEq, Eq)]
enum Tok {
    /// COLMETADATA: `(Flags, TYPE_INFO token, ColName)` per column.
    ColMetaData(Vec<(u16, u8, String)>),
    /// ROW or NBCROW: every value as a little-endian integer, `None` for a NULL.
    Row(Vec<Option<i64>>),
    /// DONE with `Status`, `CurCmd` and `DoneRowCount`.
    Done {
        status: u16,
        cur_cmd: u16,
        row_count: u64,
    },
    /// ERROR, by `Number` ([MS-TDS] 2.2.7.9): the text is not asserted on.
    Error(u32),
    /// INFO, ENVCHANGE or LOGINACK, by type: not inspected here.
    Other(u8),
}

fn u16_at(bytes: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([bytes[pos], bytes[pos + 1]])
}

fn utf16_at(bytes: &[u8], pos: usize, units: usize) -> String {
    let units: Vec<u16> = bytes[pos..pos + 2 * units]
        .chunks(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).unwrap()
}

/// Walks a token stream ([MS-TDS] 2.2.7). Only what these tests ask for is decoded: `int`
/// columns, their rows, the ERROR numbers and the DONE tokens.
fn tokens(payload: &[u8]) -> Vec<Tok> {
    let mut pos = 0;
    let mut out = Vec::new();
    // TYPE_INFO token of each column of the last COLMETADATA.
    let mut columns: Vec<u8> = Vec::new();
    while pos < payload.len() {
        let kind = payload[pos];
        pos += 1;
        match kind {
            TOKEN_COLMETADATA => {
                let count = usize::from(u16_at(payload, pos));
                pos += 2;
                columns.clear();
                let mut decoded = Vec::new();
                for _ in 0..count {
                    pos += 4; // UserType
                    let flags = u16_at(payload, pos);
                    pos += 2;
                    let ty = payload[pos];
                    pos += 1;
                    match ty {
                        INT4TYPE => {}
                        // BYTELEN TYPE_INFO: the maximum length follows the token.
                        INTNTYPE => pos += 1,
                        other => panic!("TYPE_INFO 0x{other:02X} not decoded by this test"),
                    }
                    let chars = usize::from(payload[pos]);
                    pos += 1;
                    let name = utf16_at(payload, pos, chars);
                    pos += 2 * chars;
                    columns.push(ty);
                    decoded.push((flags, ty, name));
                }
                out.push(Tok::ColMetaData(decoded));
            }
            TOKEN_ROW | TOKEN_NBCROW => {
                // NBCROW carries a null bitmap of `ceil(n / 8)` bytes, least significant
                // bit first, and writes nothing for its NULL columns ([MS-TDS] 2.2.7.13).
                let bitmap_len = if kind == TOKEN_NBCROW {
                    columns.len().div_ceil(8)
                } else {
                    0
                };
                let bitmap = &payload[pos..pos + bitmap_len];
                pos += bitmap_len;
                let mut values = Vec::new();
                for (i, ty) in columns.iter().enumerate() {
                    if kind == TOKEN_NBCROW && bitmap[i / 8] & (1 << (i % 8)) != 0 {
                        values.push(None);
                        continue;
                    }
                    // INTNTYPE prefixes its value with a length byte, zero for a NULL.
                    if *ty == INTNTYPE {
                        let len = usize::from(payload[pos]);
                        pos += 1;
                        if len == 0 {
                            values.push(None);
                            continue;
                        }
                        assert_eq!(len, 4, "only a 4-byte INTNTYPE is decoded here");
                    }
                    let value = i32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    values.push(Some(i64::from(value)));
                }
                out.push(Tok::Row(values));
            }
            TOKEN_DONE => {
                out.push(Tok::Done {
                    status: u16_at(payload, pos),
                    cur_cmd: u16_at(payload, pos + 2),
                    row_count: u64::from_le_bytes(payload[pos + 4..pos + 12].try_into().unwrap()),
                });
                pos += 12;
            }
            TOKEN_ERROR => {
                let len = usize::from(u16_at(payload, pos));
                let number = u32::from_le_bytes(payload[pos + 2..pos + 6].try_into().unwrap());
                pos += 2 + len;
                out.push(Tok::Error(number));
            }
            TOKEN_INFO | TOKEN_ENVCHANGE | TOKEN_LOGINACK => {
                let len = usize::from(u16_at(payload, pos));
                pos += 2 + len;
                out.push(Tok::Other(kind));
            }
            other => panic!("unexpected token type 0x{other:02X} at offset {}", pos - 1),
        }
    }
    out
}

/// Connects, completes the PRELOGIN exchange, sends the LOGIN7 and reads the login
/// response up to its DONE.
async fn connect_and_login(addr: SocketAddr) -> TcpStream {
    let (mut client, prelogin) = prelogin(addr).await;
    assert_eq!(prelogin[0], PACKET_TABULAR_RESULT);
    client.write_all(&login7_packet()).await.unwrap();
    let payload = read_response(&mut client, Duration::from_secs(5)).await;
    assert_eq!(
        tokens(&payload).last(),
        Some(&Tok::Done {
            status: 0,
            cur_cmd: 0,
            row_count: 0
        }),
        "login accepted"
    );
    client
}

/// Sends `text` as a SQL batch and returns the tokens of its response.
async fn run_batch(client: &mut TcpStream, text: &str, budget: Duration) -> Vec<Tok> {
    client.write_all(&sql_batch_packet(text)).await.unwrap();
    tokens(&read_response(client, budget).await)
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

/// How long a response may take before the client gives up on it. A closed connection is
/// reported at once; only a response that never comes spends the whole budget.
const RESPONSE_BUDGET: Duration = Duration::from_secs(5);

/// Number the engine answers for a state it cannot serve.
const INTERNAL_ERROR: u32 = 50000;

/// Leaves a row whose non-nullable column holds `NULL`, through the write path that still
/// accepts it, and checks that none of the three batches was refused: the read path has to
/// cope with such a row, whichever way it got there.
async fn trap_a_row(client: &mut TcpStream) {
    for text in [
        "CREATE TABLE dbo.t (a int NOT NULL)",
        "INSERT INTO dbo.t (a) VALUES (1)",
        "UPDATE dbo.t SET a = NULL",
    ] {
        let response = run_batch(client, text, RESPONSE_BUDGET).await;
        assert_eq!(
            response,
            vec![Tok::Done {
                status: 0,
                cur_cmd: 0,
                row_count: 0,
            }],
            "`{text}` was answered {response:?}"
        );
    }
}

/// The tokens `SELECT 1` produces: an unnamed non-nullable `int` column, a ROW with 1, a
/// DONE with count 1 and `CurCmd` SELECT.
fn select_1_tokens() -> Vec<Tok> {
    vec![
        Tok::ColMetaData(vec![(0, INT4TYPE, String::new())]),
        Tok::Row(vec![Some(1)]),
        Tok::Done {
            status: DONE_COUNT,
            cur_cmd: CUR_CMD_SELECT,
            row_count: 1,
        },
    ]
}

#[tokio::test]
async fn a_row_the_codec_refuses_is_answered_with_an_error_and_a_done() {
    let running = start_accepting_logins().await;
    let mut client = connect_and_login(running.addr).await;
    trap_a_row(&mut client).await;

    // The read announces its column, then says it cannot send the value. Both ways of
    // leaving the client unanswered fail here under the guard delay: a response that stops
    // after the COLMETADATA runs `RESPONSE_BUDGET` out, and a connection dropped in its
    // place is reported by `read_response`.
    let read = Instant::now();
    let response = run_batch(&mut client, "SELECT a FROM dbo.t", RESPONSE_BUDGET).await;
    assert_eq!(
        response,
        vec![
            Tok::ColMetaData(vec![(0, INT4TYPE, "a".to_owned())]),
            Tok::Error(INTERNAL_ERROR),
            Tok::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
                row_count: 0,
            },
        ]
    );
    assert!(
        read.elapsed() < RESPONSE_BUDGET,
        "answered after {:?}",
        read.elapsed()
    );

    // Same connection, next request: the refusal ended the response, not the conversation.
    let after = run_batch(&mut client, "SELECT 1", RESPONSE_BUDGET).await;
    assert_eq!(after, select_1_tokens());

    drop(client);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn the_refusal_comes_after_the_metadata_and_rows_already_sent() {
    let running = start_accepting_logins().await;
    let mut client = connect_and_login(running.addr).await;
    trap_a_row(&mut client).await;

    // A first statement whose result set is complete, then the read that fails: the ERROR
    // lands after tokens the client has already taken in ([MS-TDS] 2.2.7.9), and the DONE
    // of the failing statement is the one carrying `ERROR`.
    let response = run_batch(
        &mut client,
        "SELECT 1; SELECT a FROM dbo.t",
        RESPONSE_BUDGET,
    )
    .await;
    assert_eq!(
        response,
        vec![
            Tok::ColMetaData(vec![(0, INT4TYPE, String::new())]),
            Tok::Row(vec![Some(1)]),
            Tok::Done {
                status: DONE_COUNT | DONE_MORE,
                cur_cmd: CUR_CMD_SELECT,
                row_count: 1,
            },
            Tok::ColMetaData(vec![(0, INT4TYPE, "a".to_owned())]),
            Tok::Error(INTERNAL_ERROR),
            Tok::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
                row_count: 0,
            },
        ]
    );

    let after = run_batch(&mut client, "SELECT 1", RESPONSE_BUDGET).await;
    assert_eq!(after, select_1_tokens());

    drop(client);
    running.stop().await.unwrap();
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
