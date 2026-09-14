//! Integration tests of the batch path: after a hand-made login, SQL_BATCH and
//! TRANSACTION_MANAGER messages through the acceptance loop, the blocking pool and the
//! token channel, and the responses read packet by packet.
//!
//! The client side is written by hand from [MS-TDS] 2.2.3.1 (packet header), 2.2.5.3
//! (ALL_HEADERS), 2.2.6.4 (LOGIN7), 2.2.6.5 (PRELOGIN), 2.2.6.7 (SQL_BATCH), 2.2.6.9
//! (TRANSACTION_MANAGER) and 2.2.7 (tokens): no driver is involved.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use vauban_errors::{InternalError, SqlError, message_template};
use vauban_session::{EncryptPolicy, Engine, NoAuth, Server, ServerConfig};
use vauban_storage::MemoryStorage;

/// Packet size in force before the login; also the `default_packet_size` of the server.
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// `TDSVersion` of TDS 7.4 ([MS-TDS] 2.2.6.4).
const TDS_7_4: u32 = 0x7400_0004;
/// First SPID a server hands out.
const FIRST_SPID: u16 = 51;
/// `CurCmd` of the DONE of a SELECT.
const CUR_CMD_SELECT: u16 = 0xC1;

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
///
/// The built-in functions are registered first: the registry is a process-wide table that
/// the binary fills at start-up, not something `Server::new` does, and without
/// it `@@SPID` is an undeclared variable. `vauban_compat::register_functions()` cannot be
/// called from here (`compat` depends on `session`), so no batch below uses `@@VERSION`.
async fn start() -> Running {
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
const PACKET_TRANSACTION_MANAGER: u8 = 0x0E;
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
/// `RequestType` TM_COMMIT_XACT ([MS-TDS] 2.2.6.9).
const TM_COMMIT_XACT: u16 = 7;
/// Token types ([MS-TDS] 2.2.7).
const TOKEN_COLMETADATA: u8 = 0x81;
const TOKEN_ERROR: u8 = 0xAA;
const TOKEN_INFO: u8 = 0xAB;
const TOKEN_LOGINACK: u8 = 0xAD;
const TOKEN_ROW: u8 = 0xD1;
const TOKEN_ENVCHANGE: u8 = 0xE3;
const TOKEN_DONE: u8 = 0xFD;
/// TYPE_INFO tokens ([MS-TDS] 2.2.5.4.1).
const INT2TYPE: u8 = 0x34;
const INT4TYPE: u8 = 0x38;
const INTNTYPE: u8 = 0x26;
/// `Status` bits of DONE ([MS-TDS] 2.2.7, DONE).
const DONE_MORE: u16 = 0x0001;
const DONE_ERROR: u16 = 0x0002;
const DONE_COUNT: u16 = 0x0010;

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
        "vauban-batch-test",
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

/// ALL_HEADERS with the Transaction Descriptor header alone ([MS-TDS] 2.2.5.3.1):
/// descriptor 0 (no transaction), OutstandingRequestCount 1.
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

/// A TRANSACTION_MANAGER TM_COMMIT_XACT ([MS-TDS] 2.2.6.9): ALL_HEADERS, RequestType,
/// empty XACT_NAME, `fBeginXact` clear.
fn tm_commit_packet() -> Vec<u8> {
    let mut payload = all_headers();
    payload.extend_from_slice(&TM_COMMIT_XACT.to_le_bytes());
    payload.push(0); // XACT_NAME: B_VARCHAR of length 0
    payload.push(0); // fBeginXact
    packet(PACKET_TRANSACTION_MANAGER, &payload)
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

/// One server response: the SPID of every packet header, how many packets carried EOM,
/// and the concatenated payloads.
struct Response {
    spids: Vec<u16>,
    eom_packets: usize,
    payload: Vec<u8>,
}

/// Reads packets until one carries EOM ([MS-TDS] 2.2.3.1.2), then checks that nothing
/// follows within a short delay: one response is exactly one EOM.
async fn read_response(client: &mut TcpStream) -> Response {
    let mut response = Response {
        spids: Vec::new(),
        eom_packets: 0,
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
            response.eom_packets += 1;
            break;
        }
    }
    let mut extra = [0u8; 1];
    let after = timeout(Duration::from_millis(200), client.read(&mut extra)).await;
    assert!(
        after.is_err(),
        "bytes after the EOM packet of the response: {after:?}"
    );
    response
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
    /// ERROR with `Number`, `State`, `Class` and `MsgText`.
    Error {
        number: u32,
        state: u8,
        class: u8,
        message: String,
    },
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

/// Walks a token stream ([MS-TDS] 2.2.7). COLMETADATA is decoded for the integer types
/// the fake engine sends (`int` and `smallint`, fixed or `INTN`); ROW uses the last
/// COLMETADATA to size its values.
fn tokens(payload: &[u8]) -> Vec<Tok> {
    let mut pos = 0;
    let mut out = Vec::new();
    // `(TYPE_INFO token, value length)` of the last COLMETADATA.
    let mut columns: Vec<(u8, usize)> = Vec::new();
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
                    let len = match ty {
                        INT2TYPE => 2,
                        INT4TYPE => 4,
                        INTNTYPE => {
                            let len = usize::from(payload[pos]);
                            pos += 1;
                            len
                        }
                        other => panic!("TYPE_INFO 0x{other:02X} not decoded by this test"),
                    };
                    let chars = usize::from(payload[pos]);
                    pos += 1;
                    let name = utf16_at(payload, pos, chars);
                    pos += 2 * chars;
                    columns.push((ty, len));
                    decoded.push((flags, ty, name));
                }
                out.push(Tok::ColMetaData(decoded));
            }
            TOKEN_ROW => {
                let mut values = Vec::new();
                for (ty, len) in &columns {
                    let len = if *ty == INTNTYPE {
                        let actual = usize::from(payload[pos]);
                        pos += 1;
                        actual
                    } else {
                        *len
                    };
                    let value = match len {
                        2 => i64::from(i16::from_le_bytes([payload[pos], payload[pos + 1]])),
                        4 => i64::from(i32::from_le_bytes(
                            payload[pos..pos + 4].try_into().unwrap(),
                        )),
                        other => panic!("value length {other} not decoded by this test"),
                    };
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
                let body = &payload[pos + 2..pos + 2 + len];
                pos += 2 + len;
                out.push(if kind == TOKEN_ERROR {
                    // Number (4), State (1), Class (1), MsgText (US_VARCHAR).
                    let chars = usize::from(u16_at(body, 6));
                    Tok::Error {
                        number: u32::from_le_bytes(body[0..4].try_into().unwrap()),
                        state: body[4],
                        class: body[5],
                        message: utf16_at(body, 8, chars),
                    }
                } else {
                    Tok::Other(kind)
                });
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
    let response = read_response(&mut client).await;
    let toks = tokens(&response.payload);
    assert_eq!(
        toks.last(),
        Some(&Tok::Done {
            status: 0,
            cur_cmd: 0,
            row_count: 0
        }),
        "login accepted: {toks:?}"
    );
    client
}

/// Sends a SQL_BATCH and reads its response.
async fn batch(client: &mut TcpStream, text: &str) -> Response {
    client.write_all(&sql_batch_packet(text)).await.unwrap();
    read_response(client).await
}

/// The tokens `SELECT 1` must produce: an unnamed non-nullable `int` column with every
/// flag clear, a ROW with 1, a DONE with count 1 and `CurCmd` SELECT.
fn select_1_tokens(more: bool) -> Vec<Tok> {
    vec![
        Tok::ColMetaData(vec![(0, INT4TYPE, String::new())]),
        Tok::Row(vec![1]),
        Tok::Done {
            status: DONE_COUNT | if more { DONE_MORE } else { 0 },
            cur_cmd: CUR_CMD_SELECT,
            row_count: 1,
        },
    ]
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn select_1_gets_colmetadata_row_and_done_count_1_in_one_response() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    let response = batch(&mut client, "SELECT 1").await;
    assert_eq!(tokens(&response.payload), select_1_tokens(false));
    assert_eq!(response.eom_packets, 1);
    // The SPID of the session is in every packet header after the login.
    assert!(
        response.spids.iter().all(|spid| *spid == FIRST_SPID),
        "header SPIDs: {:?}",
        response.spids
    );

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn two_successive_batches_work_on_the_same_connection() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    let first = batch(&mut client, "SELECT 1").await;
    assert_eq!(tokens(&first.payload), select_1_tokens(false));

    // A mixed batch: the SET gets its own DONE with MORE, then the SELECT; nothing of
    // the first response leaks into the second. `NOCOUNT` is honoured, so the DONE of
    // that SELECT carries no `DONE_COUNT`, as on SQL Server, where `SET NOCOUNT ON;
    // SELECT 1; SELECT 1 WHERE 1 = 0;` answers `DONE(status=0x0001[MORE], curcmd=193,
    // rowcount=1)` then `DONE(status=0x0000[FINAL], curcmd=193, rowcount=0)`, and the
    // comparison batch `SELECT 1; SELECT 1 WHERE 1 = 0;` (no `SET` of its own, the option
    // at its default `OFF`) answers `0x0011[MORE|COUNT]` then `0x0010[COUNT]`. SQL Server
    // still writes the raw count with the flag clear; [MS-TDS] 2.2.7.6 declares it
    // invalid then, and VaubanDB writes 0. Deliberate difference: the `CurCmd` of the
    // SET's own DONE is 185 on SQL Server and 0 here.
    let second = batch(&mut client, "SET NOCOUNT ON;\nSELECT 1").await;
    assert_eq!(
        tokens(&second.payload),
        vec![
            Tok::Done {
                status: DONE_MORE,
                cur_cmd: 0,
                row_count: 0,
            },
            Tok::ColMetaData(vec![(0, INT4TYPE, String::new())]),
            Tok::Row(vec![1]),
            Tok::Done {
                status: 0,
                cur_cmd: CUR_CMD_SELECT,
                row_count: 0,
            },
        ]
    );
    assert_eq!(second.eom_packets, 1);

    // `NOCOUNT` is a session option: it survives the batch, so the next SELECT on the
    // same connection loses its `DONE_COUNT` too (batch `SELECT 1; SELECT
    // @@ROWCOUNT;` after `SET NOCOUNT ON`: `0x0001[MORE]` then `0x0000[FINAL]`).
    let third = batch(&mut client, "select @@spid").await;
    assert_eq!(
        tokens(&third.payload),
        vec![
            Tok::ColMetaData(vec![(0, INT2TYPE, String::new())]),
            Tok::Row(vec![i64::from(FIRST_SPID)]),
            Tok::Done {
                status: 0,
                cur_cmd: CUR_CMD_SELECT,
                row_count: 0,
            },
        ]
    );

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn a_syntax_error_gets_102_and_the_connection_stays_usable() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    // The error is the one `SELEC` raises, and the `SELECT 1` before it never runs,
    // because SQL Server compiles no batch partly.
    let response = batch(&mut client, "SELECT 1; SELEC 1").await;
    assert_eq!(
        tokens(&response.payload),
        vec![
            Tok::Error {
                number: 102,
                state: 1,
                class: 15,
                message: SqlError::incorrect_syntax_near("SELEC", 1).message,
            },
            Tok::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
                row_count: 0,
            },
        ]
    );

    let next = batch(&mut client, "SELECT 1").await;
    assert_eq!(tokens(&next.payload), select_1_tokens(false));

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn commit_outside_a_transaction_gets_3902_and_the_connection_stays_usable() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    client.write_all(&tm_commit_packet()).await.unwrap();
    let response = read_response(&mut client).await;
    assert_eq!(
        tokens(&response.payload),
        vec![
            Tok::Error {
                number: 3902,
                state: 1,
                class: 16,
                message: message_template(3902)
                    .expect("3902 is in the catalogue")
                    .template
                    .to_owned(),
            },
            Tok::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
                row_count: 0,
            },
        ]
    );
    assert_eq!(response.eom_packets, 1);

    let next = batch(&mut client, "SELECT 1").await;
    assert_eq!(tokens(&next.payload), select_1_tokens(false));

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn empty_batch_gets_a_single_done() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    let response = batch(&mut client, "-- nothing to run\n").await;
    assert_eq!(
        tokens(&response.payload),
        vec![Tok::Done {
            status: 0,
            cur_cmd: 0,
            row_count: 0,
        }]
    );

    drop(client);
    running.stop().await;
}
