//! Integration tests of ATTENTION: a long `WAITFOR DELAY` interrupted by an
//! ATTENTION packet, the acknowledgement expected by every driver (a lone DONE carrying
//! `DONE_ATTN`), the reuse of the connection afterwards, and the protocol violation of a
//! second request sent while one is running.
//!
//! The client side is written by hand from [MS-TDS] 2.2.1.7 (Attention), 2.2.3.1 (packet
//! header), 2.2.5.3 (ALL_HEADERS), 2.2.6.4 (LOGIN7), 2.2.6.5 (PRELOGIN), 2.2.6.7
//! (SQL_BATCH) and 2.2.7 (tokens): no driver is involved. The harness is the one of
//! `tests/batch.rs`, kept to what these tests need.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use vauban_errors::InternalError;
use vauban_session::{EncryptPolicy, Engine, NoAuth, Server, ServerConfig};
use vauban_storage::MemoryStorage;

/// Packet size in force before the login; also the `default_packet_size` of the server.
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// `TDSVersion` of TDS 7.4 ([MS-TDS] 2.2.6.4).
const TDS_7_4: u32 = 0x7400_0004;
/// `CurCmd` of the DONE of a SELECT.
const CUR_CMD_SELECT: u16 = 0xC1;
/// How long a cancelled request may take to be acknowledged: the fake engine looks at the
/// cancellation flag every 50 ms.
const ATTENTION_BUDGET: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------

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

/// Starts a `NoAuth` server on an ephemeral port.
async fn start() -> Running {
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
    );
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

/// Packet types ([MS-TDS] 2.2.3.1.1).
const PACKET_SQL_BATCH: u8 = 0x01;
const PACKET_TABULAR_RESULT: u8 = 0x04;
const PACKET_ATTENTION: u8 = 0x06;
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
/// ALL_HEADERS Transaction Descriptor header ([MS-TDS] 2.2.5.3.1).
const HEADER_TRANSACTION_DESCRIPTOR: u16 = 0x0002;
/// Token types ([MS-TDS] 2.2.7).
const TOKEN_COLMETADATA: u8 = 0x81;
const TOKEN_ERROR: u8 = 0xAA;
const TOKEN_INFO: u8 = 0xAB;
const TOKEN_LOGINACK: u8 = 0xAD;
const TOKEN_ROW: u8 = 0xD1;
const TOKEN_ENVCHANGE: u8 = 0xE3;
const TOKEN_DONE: u8 = 0xFD;
/// TYPE_INFO tokens ([MS-TDS] 2.2.5.4.1).
const INT4TYPE: u8 = 0x38;
/// `Status` bits of DONE ([MS-TDS] 2.2.7, DONE).
const DONE_COUNT: u16 = 0x0010;
const DONE_ATTN: u16 = 0x0020;

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
        "vauban-attention-test",
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

/// An ATTENTION message ([MS-TDS] 2.2.1.7): one packet of type 0x06 with EOM and no
/// payload.
fn attention_packet() -> Vec<u8> {
    packet(PACKET_ATTENTION, &[])
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

/// Reads packets until one carries EOM ([MS-TDS] 2.2.3.1.2), then checks that nothing
/// follows within a short delay: one response is exactly one EOM.
async fn read_response(client: &mut TcpStream, budget: Duration) -> Vec<u8> {
    let mut payload = Vec::new();
    loop {
        let packet = timeout(budget, read_packet(client))
            .await
            .unwrap_or_else(|_| panic!("response packet within {budget:?}"));
        assert_eq!(packet[0], PACKET_TABULAR_RESULT, "response packet type");
        payload.extend_from_slice(&packet[8..]);
        if packet[1] & STATUS_EOM == STATUS_EOM {
            break;
        }
    }
    let mut extra = [0u8; 1];
    let after = timeout(Duration::from_millis(200), client.read(&mut extra)).await;
    assert!(
        after.is_err(),
        "bytes after the EOM packet of the response: {after:?}"
    );
    payload
}

/// The tokens of a response, decoded just enough for the assertions.
#[derive(Debug, PartialEq, Eq)]
enum Tok {
    /// COLMETADATA: `(Flags, TYPE_INFO token, ColName)` per column.
    ColMetaData(Vec<(u16, u8, String)>),
    /// ROW: every value as a little-endian integer.
    Row(Vec<i64>),
    /// DONE with `Status`, `CurCmd` and `DoneRowCount`.
    Done {
        status: u16,
        cur_cmd: u16,
        row_count: u64,
    },
    /// ERROR, INFO, ENVCHANGE or LOGINACK, by type: not inspected here.
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

/// Walks a token stream ([MS-TDS] 2.2.7). Only what these tests send is decoded: the `int`
/// column of `SELECT 1`, its ROW, and the DONE tokens.
fn tokens(payload: &[u8]) -> Vec<Tok> {
    let mut pos = 0;
    let mut out = Vec::new();
    let mut columns: Vec<usize> = Vec::new();
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
                    assert_eq!(
                        ty, INT4TYPE,
                        "TYPE_INFO 0x{ty:02X} not decoded by this test"
                    );
                    let chars = usize::from(payload[pos]);
                    pos += 1;
                    let name = utf16_at(payload, pos, chars);
                    pos += 2 * chars;
                    columns.push(4);
                    decoded.push((flags, ty, name));
                }
                out.push(Tok::ColMetaData(decoded));
            }
            TOKEN_ROW => {
                let mut values = Vec::new();
                for len in &columns {
                    let value = i64::from(i32::from_le_bytes(
                        payload[pos..pos + 4].try_into().unwrap(),
                    ));
                    pos += len;
                    values.push(value);
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
            TOKEN_ERROR | TOKEN_INFO | TOKEN_ENVCHANGE | TOKEN_LOGINACK => {
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
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&prelogin_packet()).await.unwrap();
    let prelogin = timeout(Duration::from_secs(5), read_packet(&mut client))
        .await
        .expect("PRELOGIN response within 5 s");
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

/// The acknowledgement of an ATTENTION: a lone DONE with `DONE_ATTN`, no count.
fn attention_ack() -> Vec<Tok> {
    vec![Tok::Done {
        status: DONE_ATTN,
        cur_cmd: 0,
        row_count: 0,
    }]
}

/// The tokens `SELECT 1` produces: an unnamed non-nullable `int` column, a ROW with 1, a
/// DONE with count 1 and `CurCmd` SELECT.
fn select_1_tokens() -> Vec<Tok> {
    vec![
        Tok::ColMetaData(vec![(0, INT4TYPE, String::new())]),
        Tok::Row(vec![1]),
        Tok::Done {
            status: DONE_COUNT,
            cur_cmd: CUR_CMD_SELECT,
            row_count: 1,
        },
    ]
}

/// Sends `SELECT 1` and checks the response: the connection is still usable.
async fn assert_still_usable(client: &mut TcpStream) {
    client
        .write_all(&sql_batch_packet("SELECT 1"))
        .await
        .unwrap();
    let payload = read_response(client, Duration::from_secs(5)).await;
    assert_eq!(tokens(&payload), select_1_tokens());
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn attention_during_a_batch_gets_a_done_attn_and_the_connection_stays_usable() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    client
        .write_all(&sql_batch_packet("WAITFOR DELAY '00:00:10'"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent = Instant::now();
    client.write_all(&attention_packet()).await.unwrap();

    // The whole answer to the cancelled batch is the acknowledgement ([MS-TDS] 2.2.1.7).
    let payload = read_response(&mut client, ATTENTION_BUDGET).await;
    assert_eq!(tokens(&payload), attention_ack());
    let elapsed = sent.elapsed();
    assert!(
        elapsed < ATTENTION_BUDGET,
        "acknowledged after {elapsed:?}, the batch asked for 10 s"
    );

    assert_still_usable(&mut client).await;

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn attention_outside_a_batch_is_acknowledged_and_the_connection_stays_usable() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    // [MS-TDS] 2.2.1.7: the server always acknowledges, even with nothing to cancel.
    client.write_all(&attention_packet()).await.unwrap();
    let payload = read_response(&mut client, ATTENTION_BUDGET).await;
    assert_eq!(tokens(&payload), attention_ack());

    assert_still_usable(&mut client).await;

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn an_uninterrupted_waitfor_ends_with_a_done_without_attn() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    let start_of_batch = Instant::now();
    client
        .write_all(&sql_batch_packet("WAITFOR DELAY '00:00:00.100'"))
        .await
        .unwrap();
    let payload = read_response(&mut client, ATTENTION_BUDGET).await;
    assert_eq!(
        tokens(&payload),
        vec![Tok::Done {
            status: 0,
            cur_cmd: 0,
            row_count: 0,
        }]
    );
    assert!(
        start_of_batch.elapsed() >= Duration::from_millis(100),
        "the delay was not waited: {:?}",
        start_of_batch.elapsed()
    );

    assert_still_usable(&mut client).await;

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn a_second_batch_during_a_batch_closes_the_connection() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    client
        .write_all(&sql_batch_packet("WAITFOR DELAY '00:00:10'"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // No MARS: a second request while one runs is a protocol violation.
    client
        .write_all(&sql_batch_packet("SELECT 1"))
        .await
        .unwrap();

    let mut buffer = [0u8; 64];
    let closed = timeout(ATTENTION_BUDGET, client.read(&mut buffer))
        .await
        .expect("the connection must be closed within the budget");
    // A closed connection reads as EOF, or as a reset when the peer closed with data
    // still queued: both mean the server hung up, and neither carries a response.
    assert!(
        matches!(closed, Ok(0) | Err(_)),
        "the server answered instead of closing: {closed:?}"
    );

    running.stop().await;
}
