//! Integration tests of the connection limits of `Server::with_limits`: the time a
//! connection has to log in, the number of connections served at once, and the size of a
//! client message after the login.
//!
//! The client side is written by hand from [MS-TDS] 2.2.3.1 (packet header), 2.2.5.3
//! (ALL_HEADERS), 2.2.6.4 (LOGIN7), 2.2.6.5 (PRELOGIN), 2.2.6.7 (SQL_BATCH) and 2.2.7
//! (tokens): no driver is involved. Every server listens on an ephemeral port, and no test
//! opens more than four connections.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use vauban_errors::InternalError;
use vauban_session::{ConnectionLimits, EncryptPolicy, Engine, NoAuth, Server, ServerConfig};
use vauban_storage::MemoryStorage;

/// Packet size in force before the login; also the `default_packet_size` of the server.
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// `TDSVersion` of TDS 7.4 ([MS-TDS] 2.2.6.4).
const TDS_7_4: u32 = 0x7400_0004;
/// How long the server may take to answer or to hang up.
const BUDGET: Duration = Duration::from_secs(5);
/// Login timeout of the tests that exercise it.
const SHORT_LOGIN_TIMEOUT: Duration = Duration::from_millis(300);

/// Packet types ([MS-TDS] 2.2.3.1.1).
const PACKET_SQL_BATCH: u8 = 0x01;
const PACKET_TABULAR_RESULT: u8 = 0x04;
const PACKET_LOGIN7: u8 = 0x10;
const PACKET_PRELOGIN: u8 = 0x12;
/// Status bits ([MS-TDS] 2.2.3.1.2).
const STATUS_NORMAL: u8 = 0x00;
const STATUS_EOM: u8 = 0x01;
/// Token types ([MS-TDS] 2.2.7).
const TOKEN_COLMETADATA: u8 = 0x81;
const TOKEN_ERROR: u8 = 0xAA;
const TOKEN_ROW: u8 = 0xD1;
const TOKEN_DONE: u8 = 0xFD;
/// TYPE_INFO tokens ([MS-TDS] 2.2.5.4.1, 2.2.5.4.2).
const INT4TYPE: u8 = 0x38;
const INTNTYPE: u8 = 0x26;
/// RESETCONNECTION ([MS-TDS] 2.2.3.1.2).
const STATUS_RESET_CONNECTION: u8 = 0x08;
/// `Status` bit ERROR of DONE ([MS-TDS] 2.2.7, DONE).
const DONE_ERROR: u16 = 0x0002;

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
    async fn stop(self) {
        self.shutdown.cancel();
        timeout(Duration::from_secs(6), self.task)
            .await
            .expect("serve must return within 6 s of shutdown")
            .expect("serve task must not panic")
            .expect("serve returns Ok");
    }
}

/// Starts a `NoAuth` server on an ephemeral port with `limits`.
async fn start(limits: ConnectionLimits) -> Running {
    vauban_sysfn::register_builtins();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = Server::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(NoAuth),
            server_name: "vauban-test".into(),
            default_packet_size: DEFAULT_PACKET_SIZE,
            program_name: None,
            version_banner: None,
            edition: None,
        },
    )
    .with_limits(limits);
    let task = tokio::spawn(server.serve(listener, shutdown.clone()));
    Running {
        addr,
        shutdown,
        task,
    }
}

// ---------------------------------------------------------------------------------------
// Hand-made TDS client
// ---------------------------------------------------------------------------------------

/// One packet of type `kind` with `status`, SPID 0 and PacketID 1 ([MS-TDS] 2.2.3.1).
fn packet_with_status(kind: u8, status: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![kind, status];
    packet.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x00, 0x01, 0x00]);
    packet.extend_from_slice(payload);
    packet
}

/// A minimal client PRELOGIN ([MS-TDS] 2.2.6.5): VERSION and ENCRYPTION (ENCRYPT_NOT_SUP).
fn prelogin_packet() -> Vec<u8> {
    let payload = [
        0x00, 0x00, 0x0B, 0x00, 0x06, // VERSION @11, 6
        0x01, 0x00, 0x11, 0x00, 0x01, // ENCRYPTION @17, 1
        0xFF, // TERMINATOR
        0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00, // VERSION data
        0x02, // ENCRYPT_NOT_SUP
    ];
    packet_with_status(PACKET_PRELOGIN, STATUS_EOM, &payload)
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// A LOGIN7 message ([MS-TDS] 2.2.6.4) for `sa` with an empty password (`NoAuth`): the
/// 94-byte fixed part followed by the data section.
fn login7_packet() -> Vec<u8> {
    const FIXED_LEN: usize = 94;
    let mut data = Vec::new();
    let mut pairs = Vec::new();
    let text_fields = [
        "testhost",
        "sa",
        "",
        "vauban-limits-test",
        "localhost",
        "",
        "hand-made",
        "",
        "",
    ];
    for field in text_fields {
        pairs.push(((FIXED_LEN + data.len()) as u16, field.len() as u16));
        data.extend_from_slice(&utf16le(field));
    }
    let mut fixed = Vec::with_capacity(FIXED_LEN);
    fixed.extend_from_slice(&((FIXED_LEN + data.len()) as u32).to_le_bytes());
    fixed.extend_from_slice(&TDS_7_4.to_le_bytes());
    fixed.extend_from_slice(&u32::from(DEFAULT_PACKET_SIZE).to_le_bytes());
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
    fixed.extend_from_slice(&[0u8; 12]); // SSPI, AtchDBFile, ChangePassword
    fixed.extend_from_slice(&0u32.to_le_bytes()); // cbSSPILong
    assert_eq!(fixed.len(), FIXED_LEN);
    fixed.extend_from_slice(&data);
    packet_with_status(PACKET_LOGIN7, STATUS_EOM, &fixed)
}

/// SQL_BATCH payload ([MS-TDS] 2.2.6.7): ALL_HEADERS with a Transaction Descriptor
/// header, then `SQLText` in UCS-2.
fn batch_payload(text: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&22u32.to_le_bytes()); // TotalLength
    payload.extend_from_slice(&18u32.to_le_bytes()); // HeaderLength
    payload.extend_from_slice(&0x0002u16.to_le_bytes()); // Transaction Descriptor
    payload.extend_from_slice(&0u64.to_le_bytes());
    payload.extend_from_slice(&1u32.to_le_bytes()); // OutstandingRequestCount
    payload.extend_from_slice(&utf16le(text));
    payload
}

/// Sends `text` as one SQL_BATCH message cut into packets of at most `chunk` body bytes.
async fn send_batch(client: &mut TcpStream, text: &str, chunk: usize) {
    send_batch_with_reset(client, text, chunk, STATUS_NORMAL).await;
}

/// As [`send_batch`], with `reset` (RESETCONNECTION or RESETCONNECTIONSKIPTRAN) in the
/// status of the first packet ([MS-TDS] 2.2.3.1.2).
async fn send_batch_with_reset(client: &mut TcpStream, text: &str, chunk: usize, reset: u8) {
    let payload = batch_payload(text);
    let chunks: Vec<&[u8]> = payload.chunks(chunk).collect();
    let mut bytes = Vec::new();
    for (i, body) in chunks.iter().enumerate() {
        let mut status = if i + 1 == chunks.len() {
            STATUS_EOM
        } else {
            STATUS_NORMAL
        };
        if i == 0 {
            status |= reset;
        }
        bytes.extend_from_slice(&packet_with_status(PACKET_SQL_BATCH, status, body));
    }
    client.write_all(&bytes).await.unwrap();
}

/// Reads one packet: the 8-byte header, then the rest as announced by its Length field.
async fn read_packet(client: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 8];
    client.read_exact(&mut header).await.unwrap();
    let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let mut packet = header.to_vec();
    packet.resize(len, 0);
    client.read_exact(&mut packet[8..]).await.unwrap();
    packet
}

/// Reads packets until one carries EOM and returns their concatenated payloads.
async fn read_response(client: &mut TcpStream) -> Vec<u8> {
    let mut payload = Vec::new();
    loop {
        let packet = timeout(BUDGET, read_packet(client))
            .await
            .expect("response packet within the budget");
        assert_eq!(packet[0], PACKET_TABULAR_RESULT, "response packet type");
        payload.extend_from_slice(&packet[8..]);
        if packet[1] & STATUS_EOM == STATUS_EOM {
            return payload;
        }
    }
}

/// What these tests look at in a response.
#[derive(Debug, PartialEq, Eq)]
enum Tok {
    /// ERROR: `Number`, `State`, `Class`.
    Error { number: u32, state: u8, class: u8 },
    /// DONE: `Status`.
    Done { status: u16 },
    /// ROW of one non-nullable `int`.
    Row(i32),
    /// Any other token with a two-byte length (COLMETADATA of one column is skipped whole).
    Other(u8),
}

fn u16_at(bytes: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([bytes[pos], bytes[pos + 1]])
}

/// Walks a token stream: ERROR, DONE, the COLMETADATA and ROW of one unnamed `int`
/// column (nullable or not), and the tokens with a two-byte length of a login response.
fn tokens(payload: &[u8]) -> Vec<Tok> {
    let mut pos = 0;
    let mut out = Vec::new();
    // Whether the `int` column of the last COLMETADATA carries a length byte (INTNTYPE).
    let mut nullable = false;
    while pos < payload.len() {
        let kind = payload[pos];
        pos += 1;
        match kind {
            TOKEN_ERROR => {
                let len = usize::from(u16_at(payload, pos));
                out.push(Tok::Error {
                    number: u32::from_le_bytes(payload[pos + 2..pos + 6].try_into().unwrap()),
                    state: payload[pos + 6],
                    class: payload[pos + 7],
                });
                pos += 2 + len;
            }
            TOKEN_DONE => {
                out.push(Tok::Done {
                    status: u16_at(payload, pos),
                });
                pos += 12;
            }
            TOKEN_COLMETADATA => {
                // One unnamed `int`: count, UserType, Flags, TYPE_INFO, empty name.
                assert_eq!(u16_at(payload, pos), 1, "one column");
                pos += 2 + 4 + 2;
                nullable = match payload[pos] {
                    INT4TYPE => false,
                    INTNTYPE => {
                        pos += 1;
                        true
                    }
                    other => panic!("TYPE_INFO 0x{other:02X} not decoded by this test"),
                };
                pos += 1;
                assert_eq!(payload[pos], 0, "unnamed column");
                pos += 1;
                out.push(Tok::Other(kind));
            }
            TOKEN_ROW => {
                if nullable {
                    assert_eq!(payload[pos], 4, "NULL or odd-width int");
                    pos += 1;
                }
                out.push(Tok::Row(i32::from_le_bytes(
                    payload[pos..pos + 4].try_into().unwrap(),
                )));
                pos += 4;
            }
            _ => {
                let len = usize::from(u16_at(payload, pos));
                pos += 2 + len;
                out.push(Tok::Other(kind));
            }
        }
    }
    out
}

/// Connects and completes the PRELOGIN exchange.
async fn connect_prelogin(addr: SocketAddr) -> TcpStream {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&prelogin_packet()).await.unwrap();
    let prelogin = timeout(BUDGET, read_packet(&mut client))
        .await
        .expect("PRELOGIN response within the budget");
    assert_eq!(prelogin[0], PACKET_TABULAR_RESULT);
    client
}

/// Connects, sends the LOGIN7 and returns the tokens of the login response.
async fn login(addr: SocketAddr) -> (TcpStream, Vec<Tok>) {
    let mut client = connect_prelogin(addr).await;
    client.write_all(&login7_packet()).await.unwrap();
    let response = tokens(&read_response(&mut client).await);
    (client, response)
}

/// Connects and logs in, asserting the login was accepted.
async fn logged_in(addr: SocketAddr) -> TcpStream {
    let (client, response) = login(addr).await;
    assert_eq!(
        response.last(),
        Some(&Tok::Done { status: 0 }),
        "login accepted: {response:?}"
    );
    client
}

/// Runs `SELECT 1` and asserts its row.
async fn assert_select_1(client: &mut TcpStream) {
    send_batch(client, "SELECT 1", 4096).await;
    let response = tokens(&read_response(client).await);
    assert!(response.contains(&Tok::Row(1)), "SELECT 1: {response:?}");
}

/// Waits for the server to close `client`: `read` returns 0 bytes (or a reset).
async fn assert_closed_within(client: &mut TcpStream, budget: Duration) {
    let mut buf = [0u8; 64];
    let read = timeout(budget, client.read(&mut buf))
        .await
        .expect("connection closed within the budget");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "connection still sending: {read:?}"
    );
}

fn limits() -> ConnectionLimits {
    ConnectionLimits::default()
}

// ---------------------------------------------------------------------------------------
// Login timeout
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_silent_connection_is_closed_after_the_login_timeout() {
    let server = start(ConnectionLimits {
        login_timeout: SHORT_LOGIN_TIMEOUT,
        ..limits()
    })
    .await;
    let started = Instant::now();
    let mut client = TcpStream::connect(server.addr).await.unwrap();
    assert_closed_within(&mut client, BUDGET).await;
    assert!(
        started.elapsed() >= SHORT_LOGIN_TIMEOUT,
        "closed before the timeout: {:?}",
        started.elapsed()
    );
    server.stop().await;
}

#[tokio::test]
async fn a_connection_silent_after_prelogin_is_closed_after_the_login_timeout() {
    let server = start(ConnectionLimits {
        login_timeout: SHORT_LOGIN_TIMEOUT,
        ..limits()
    })
    .await;
    let mut client = connect_prelogin(server.addr).await;
    assert_closed_within(&mut client, BUDGET).await;
    server.stop().await;
}

#[tokio::test]
async fn the_login_timeout_does_not_apply_after_the_login() {
    let server = start(ConnectionLimits {
        login_timeout: SHORT_LOGIN_TIMEOUT,
        ..limits()
    })
    .await;
    let mut client = logged_in(server.addr).await;
    tokio::time::sleep(SHORT_LOGIN_TIMEOUT * 2).await;
    assert_select_1(&mut client).await;
    server.stop().await;
}

// ---------------------------------------------------------------------------------------
// Connection limit
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn the_connection_beyond_the_limit_is_refused_with_17809() {
    let server = start(ConnectionLimits {
        max_connections: 2,
        ..limits()
    })
    .await;
    let mut first = logged_in(server.addr).await;
    let mut second = logged_in(server.addr).await;

    let (mut third, response) = login(server.addr).await;
    assert_eq!(
        response,
        vec![
            Tok::Error {
                number: 17809,
                state: 1,
                class: 20
            },
            Tok::Done { status: DONE_ERROR },
        ]
    );
    assert_closed_within(&mut third, BUDGET).await;

    // The two served connections go on.
    assert_select_1(&mut first).await;
    assert_select_1(&mut second).await;
    server.stop().await;
}

#[tokio::test]
async fn a_closed_connection_frees_its_slot() {
    let server = start(ConnectionLimits {
        max_connections: 1,
        ..limits()
    })
    .await;
    let first = logged_in(server.addr).await;
    drop(first);
    // The slot comes back once the server has seen the peer leave.
    let deadline = Instant::now() + BUDGET;
    loop {
        let (mut client, response) = login(server.addr).await;
        if response.last() == Some(&Tok::Done { status: 0 }) {
            assert_select_1(&mut client).await;
            break;
        }
        assert!(Instant::now() < deadline, "slot not freed: {response:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.stop().await;
}

// ---------------------------------------------------------------------------------------
// Message size
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_batch_gets_an_error_and_the_connection_goes_on() {
    let server = start(ConnectionLimits {
        max_message_size: Some(1000),
        ..limits()
    })
    .await;
    let mut client = logged_in(server.addr).await;
    // Sent in packets of 512 body bytes, so the limit is crossed in the middle of it.
    let text = format!("SELECT '{}'", "x".repeat(1990));
    assert!(batch_payload(&text).len() > 3 * 1000);
    send_batch(&mut client, &text, 512).await;
    let response = tokens(&read_response(&mut client).await);
    assert_eq!(
        response,
        vec![
            Tok::Error {
                number: 50000,
                state: 1,
                class: 16
            },
            Tok::Done { status: DONE_ERROR },
        ]
    );
    assert_select_1(&mut client).await;
    server.stop().await;
}

#[tokio::test]
async fn a_batch_under_the_size_limit_runs() {
    let server = start(ConnectionLimits {
        max_message_size: Some(1000),
        ..limits()
    })
    .await;
    let mut client = logged_in(server.addr).await;
    assert_select_1(&mut client).await;
    server.stop().await;
}

/// Runs `text` and returns the tokens of its response.
async fn run(client: &mut TcpStream, text: &str) -> Vec<Tok> {
    send_batch(client, text, 4096).await;
    tokens(&read_response(client).await)
}

/// `1` when the current database is `master`, `0` otherwise.
async fn in_master(client: &mut TcpStream) -> i32 {
    let response = run(
        client,
        "SELECT CASE WHEN DB_NAME() = N'master' THEN 1 ELSE 0 END",
    )
    .await;
    match response.iter().find(|tok| matches!(tok, Tok::Row(_))) {
        Some(Tok::Row(value)) => *value,
        _ => panic!("no row: {response:?}"),
    }
}

#[tokio::test]
async fn an_oversized_batch_marked_reset_still_resets_the_session() {
    let server = start(ConnectionLimits {
        max_message_size: Some(1000),
        ..limits()
    })
    .await;
    let mut client = logged_in(server.addr).await;
    let created = run(&mut client, "CREATE DATABASE rdb").await;
    assert!(
        !created.iter().any(|tok| matches!(tok, Tok::Error { .. })),
        "{created:?}"
    );
    let text = format!("SELECT '{}'", "x".repeat(1990));

    // Without the reset bit the session keeps its database: the refusal changes nothing.
    run(&mut client, "USE rdb").await;
    assert_eq!(in_master(&mut client).await, 0);
    send_batch(&mut client, &text, 512).await;
    let response = tokens(&read_response(&mut client).await);
    assert!(response.contains(&Tok::Error {
        number: 50000,
        state: 1,
        class: 16
    }));
    assert_eq!(in_master(&mut client).await, 0);

    // With it, the reset is applied before the refusal: back to the login database.
    send_batch_with_reset(&mut client, &text, 512, STATUS_RESET_CONNECTION).await;
    let response = tokens(&read_response(&mut client).await);
    assert!(
        response.contains(&Tok::Error {
            number: 50000,
            state: 1,
            class: 16
        }),
        "{response:?}"
    );
    assert_eq!(response.last(), Some(&Tok::Done { status: DONE_ERROR }));
    assert_eq!(in_master(&mut client).await, 1);
    assert_select_1(&mut client).await;
    server.stop().await;
}
