//! Integration tests of the client messages a server reads whole but does not handle: an
//! RPC whose parameter has an `xml`, table-valued or `sql_variant` type, a batch of RPCs,
//! an encrypted parameter, a BULK_LOAD message. Each one gets an ERROR and a DONE carrying
//! `DONE_ERROR`, and the next `SELECT 1` on the same connection answers its row. A
//! malformed RPC, by contrast, closes the connection. The LONGLEN parameters (`text`,
//! `ntext`, `image`) are read, and the tests at the end of the file follow them through
//! `sp_executesql`.
//!
//! The client side is written by hand from [MS-TDS] 2.2.3.1 (packet header), 2.2.5.3
//! (ALL_HEADERS), 2.2.5.4 (TYPE_INFO), 2.2.6.4 (LOGIN7), 2.2.6.5 (PRELOGIN), 2.2.6.6 (RPC
//! Request), 2.2.6.7 (SQL_BATCH) and 2.2.7 (tokens): no driver is involved. The harness is
//! the one of `tests/attention.rs`, kept to what these tests need.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use vauban_errors::{InternalError, SqlError};
use vauban_session::{
    EncryptPolicy, Engine, NoAuth, ProcAction, ProcArg, ProcParam, Server, ServerConfig,
};
use vauban_storage::MemoryStorage;
use vauban_types::{SqlType, Value};

/// Packet size in force before the login; also the `default_packet_size` of the server.
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// `TDSVersion` of TDS 7.4 ([MS-TDS] 2.2.6.4).
const TDS_7_4: u32 = 0x7400_0004;
/// `CurCmd` of the DONE of a SELECT.
const CUR_CMD_SELECT: u16 = 0xC1;
/// How long the server may take to answer or to hang up.
const BUDGET: Duration = Duration::from_secs(5);

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
    start_with(Engine::new(Arc::new(MemoryStorage::new()))).await
}

/// A stand-in for the `sp_executesql` of the system procedure layer, which this crate does
/// not depend on: the statement is the first argument and must be `nvarchar` or `nchar`
/// (error 214, state 2 otherwise); the arguments after the declaration are bound with the
/// type and value the RPC decoder delivered.
fn executesql_stand_in(name: &str, args: &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>> {
    if name != "sp_executesql" {
        return None;
    }
    let statement = args.first()?;
    let text = match (statement.ty.ty, statement.value) {
        (SqlType::NVarChar(_) | SqlType::NChar(_), Value::String(text)) => text.text.clone(),
        _ => {
            return Some(Err(SqlError::procedure_expects_type(
                "@statement",
                "ntext/nchar/nvarchar",
            )));
        }
    };
    let params = args
        .iter()
        .skip(2)
        .map(|arg| ProcParam {
            name: arg.name.unwrap_or_default().to_owned(),
            ty: arg.ty.clone(),
            value: arg.value.clone(),
            output: arg.output,
        })
        .collect();
    Some(Ok(ProcAction::ExecuteSql {
        statement: text,
        params,
    }))
}

/// Starts a `NoAuth` server whose engine resolves `sp_executesql` by [`executesql_stand_in`].
async fn start_with_executesql() -> Running {
    start_with(Engine::with_system_procedures(
        Arc::new(MemoryStorage::new()),
        Arc::new(executesql_stand_in),
    ))
    .await
}

/// Starts a `NoAuth` server on `engine`, on an ephemeral port.
async fn start_with(engine: Engine) -> Running {
    vauban_sysfn::register_builtins();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = Server::new(
        Arc::new(engine),
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
const PACKET_RPC: u8 = 0x03;
const PACKET_TABULAR_RESULT: u8 = 0x04;
const PACKET_BULK_LOAD: u8 = 0x07;
const PACKET_LOGIN7: u8 = 0x10;
const PACKET_PRELOGIN: u8 = 0x12;
/// Status EOM ([MS-TDS] 2.2.3.1.2).
const STATUS_EOM: u8 = 0x01;
/// Status NORMAL: more packets of the same message follow.
const STATUS_NORMAL: u8 = 0x00;
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
/// RETURNSTATUS token ([MS-TDS] 2.2.7.18).
const TOKEN_RETURNSTATUS: u8 = 0x79;
/// DONEPROC token ([MS-TDS] 2.2.7.7).
const TOKEN_DONEPROC: u8 = 0xFE;
/// DONEINPROC token ([MS-TDS] 2.2.7.6).
const TOKEN_DONEINPROC: u8 = 0xFF;
/// BIGVARBINTYPE ([MS-TDS] 2.2.5.4.2): `varbinary(n)`.
const BIGVARBINTYPE: u8 = 0xA5;
/// TYPE_INFO tokens ([MS-TDS] 2.2.5.4.1).
const INT4TYPE: u8 = 0x38;
/// INTNTYPE ([MS-TDS] 2.2.5.4.2): the nullable `int` of a column such as `@@TRANCOUNT`.
const INTNTYPE: u8 = 0x26;
/// `Status` bits of DONE ([MS-TDS] 2.2.7, DONE).
const DONE_ERROR: u16 = 0x0002;
const DONE_COUNT: u16 = 0x0010;

/// Wraps `payload` in one packet of type `kind` with the EOM status, SPID 0 and
/// PacketID 1 ([MS-TDS] 2.2.3.1).
fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
    packet_with_status(kind, STATUS_EOM, payload)
}

/// Wraps `payload` in one packet of type `kind` with the given status.
fn packet_with_status(kind: u8, status: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![kind, status];
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
        "vauban-refusal-test",
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
    all_headers_in(0)
}

/// ALL_HEADERS carrying the descriptor of the transaction the request runs in.
fn all_headers_in(descriptor: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(22);
    bytes.extend_from_slice(&22u32.to_le_bytes()); // TotalLength
    bytes.extend_from_slice(&18u32.to_le_bytes()); // HeaderLength
    bytes.extend_from_slice(&HEADER_TRANSACTION_DESCRIPTOR.to_le_bytes()); // HeaderType
    bytes.extend_from_slice(&descriptor.to_le_bytes()); // TransactionDescriptor
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
    /// ROW of `varbinary` columns: the bytes of each value.
    BinaryRow(Vec<Vec<u8>>),
    /// DONEPROC or DONEINPROC: token, `Status`, `CurCmd`.
    DoneInProc { kind: u8, status: u16, cur_cmd: u16 },
    /// RETURNSTATUS: the value.
    ReturnStatus(i32),
    /// DONE with `Status`, `CurCmd` and `DoneRowCount`.
    Done {
        status: u16,
        cur_cmd: u16,
        row_count: u64,
    },
    /// ERROR: `Number`, `State`, `Class`.
    Error { number: u32, state: u8, class: u8 },
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

/// The wire form of a column, for reading its ROW values.
#[derive(Clone, Copy, PartialEq)]
enum Column {
    /// INT4TYPE: four bytes, no length.
    Int,
    /// INTNTYPE of width 4: a length byte then four bytes.
    IntN,
    /// BIGVARBINTYPE: a two-byte length then the bytes.
    VarBinary,
}

/// Walks a token stream ([MS-TDS] 2.2.7). Only what these tests send is decoded: the `int`,
/// nullable `int` and `varbinary(n)` columns, their ROW, RETURNSTATUS and the DONE tokens.
fn tokens(payload: &[u8]) -> Vec<Tok> {
    let mut pos = 0;
    let mut out = Vec::new();
    let mut columns: Vec<Column> = Vec::new();
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
                    let column = match ty {
                        INT4TYPE => Column::Int,
                        INTNTYPE => {
                            assert_eq!(payload[pos], 4, "INTNTYPE of another width");
                            pos += 1;
                            Column::IntN
                        }
                        BIGVARBINTYPE => {
                            pos += 2; // maximum length
                            Column::VarBinary
                        }
                        _ => panic!("TYPE_INFO 0x{ty:02X} not decoded by this test"),
                    };
                    let chars = usize::from(payload[pos]);
                    pos += 1;
                    let name = utf16_at(payload, pos, chars);
                    pos += 2 * chars;
                    columns.push(column);
                    decoded.push((flags, ty, name));
                }
                out.push(Tok::ColMetaData(decoded));
            }
            TOKEN_ROW if columns.iter().all(|c| *c == Column::VarBinary) => {
                let mut values = Vec::new();
                for _ in &columns {
                    let len = usize::from(u16_at(payload, pos));
                    pos += 2;
                    values.push(payload[pos..pos + len].to_vec());
                    pos += len;
                }
                out.push(Tok::BinaryRow(values));
            }
            TOKEN_ROW => {
                let mut values = Vec::new();
                for &column in &columns {
                    assert!(column != Column::VarBinary, "mixed ROW not decoded");
                    if column == Column::IntN {
                        assert_eq!(payload[pos], 4, "NULL or odd-width int");
                        pos += 1;
                    }
                    let value = i64::from(i32::from_le_bytes(
                        payload[pos..pos + 4].try_into().unwrap(),
                    ));
                    pos += 4;
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
            TOKEN_DONEPROC | TOKEN_DONEINPROC => {
                out.push(Tok::DoneInProc {
                    kind,
                    status: u16_at(payload, pos),
                    cur_cmd: u16_at(payload, pos + 2),
                });
                pos += 12;
            }
            TOKEN_RETURNSTATUS => {
                out.push(Tok::ReturnStatus(i32::from_le_bytes(
                    payload[pos..pos + 4].try_into().unwrap(),
                )));
                pos += 4;
            }
            TOKEN_ERROR => {
                let len = usize::from(u16_at(payload, pos));
                out.push(Tok::Error {
                    number: u32::from_le_bytes(payload[pos + 2..pos + 6].try_into().unwrap()),
                    state: payload[pos + 6],
                    class: payload[pos + 7],
                });
                pos += 2 + len;
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

/// `B_VARCHAR` parameter name, `StatusFlags` 0, then `type_info` and `value`
/// ([MS-TDS] 2.2.6.6 ParameterData).
fn param(name: &str, type_info: &[u8], value: &[u8]) -> Vec<u8> {
    let mut bytes = vec![name.encode_utf16().count() as u8];
    bytes.extend_from_slice(&utf16le(name));
    bytes.push(0x00);
    bytes.extend_from_slice(type_info);
    bytes.extend_from_slice(value);
    bytes
}

/// Collation `Latin1_General_CI_AS` as five bytes ([MS-TDS] 2.2.5.1.2).
const COLLATION: [u8; 5] = [0x09, 0x04, 0xD0, 0x00, 0x34];

/// An `nvarchar(4000)` parameter.
fn nvarchar_param(name: &str, text: &str) -> Vec<u8> {
    let data = utf16le(text);
    let mut type_info = vec![0xE7];
    type_info.extend_from_slice(&8000u16.to_le_bytes());
    type_info.extend_from_slice(&COLLATION);
    let mut value = (data.len() as u16).to_le_bytes().to_vec();
    value.extend_from_slice(&data);
    param(name, &type_info, &value)
}

/// An `xml` parameter without schema (XMLTYPE, [MS-TDS] 2.2.5.4.3), value as one PLP chunk.
fn xml_param(name: &str, text: &str) -> Vec<u8> {
    let data = utf16le(text);
    let mut value = (data.len() as u64).to_le_bytes().to_vec();
    value.extend_from_slice(&(data.len() as u32).to_le_bytes());
    value.extend_from_slice(&data);
    value.extend_from_slice(&0u32.to_le_bytes());
    param(name, &[0xF1, 0x00], &value)
}

/// A `text` (TEXTTYPE) or `ntext` (NTEXTTYPE) parameter: LONGLEN maximum, collation,
/// then a four-byte length and the bytes ([MS-TDS] 2.2.5.4.3, 2.2.5.6, 2.2.5.2.3).
fn long_text_param(name: &str, token: u8, data: &[u8]) -> Vec<u8> {
    let mut type_info = vec![token];
    type_info.extend_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    type_info.extend_from_slice(&COLLATION);
    let mut value = (data.len() as u32).to_le_bytes().to_vec();
    value.extend_from_slice(data);
    param(name, &type_info, &value)
}

/// An `image` (IMAGETYPE) parameter: LONGLEN maximum, no collation, then a four-byte
/// length and the bytes ([MS-TDS] 2.2.5.4.3, 2.2.5.6, 2.2.5.2.3).
fn image_param(name: &str, data: &[u8]) -> Vec<u8> {
    let mut type_info = vec![0x22];
    type_info.extend_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    let mut value = (data.len() as u32).to_le_bytes().to_vec();
    value.extend_from_slice(data);
    param(name, &type_info, &value)
}

/// A `sql_variant` parameter (SSVARIANTTYPE) holding the `int` 7: LONGLEN maximum, then
/// the length, the base type INT4TYPE, no property byte, the four bytes.
fn sql_variant_param(name: &str) -> Vec<u8> {
    let mut type_info = vec![0x62];
    type_info.extend_from_slice(&8016u32.to_le_bytes());
    let mut value = 6u32.to_le_bytes().to_vec();
    value.extend_from_slice(&[INT4TYPE, 0x00]);
    value.extend_from_slice(&7i32.to_le_bytes());
    param(name, &type_info, &value)
}

/// A table-valued parameter of type `dbo.T (a int)` with one row (TVPTYPE,
/// [MS-TDS] 2.2.5.5.5): type name, one column, TVP_END_TOKEN, one TVP_ROW, TVP_END_TOKEN.
fn tvp_param(name: &str) -> Vec<u8> {
    let mut bytes = vec![name.encode_utf16().count() as u8];
    bytes.extend_from_slice(&utf16le(name));
    bytes.push(0x00);
    bytes.push(0xF3);
    bytes.push(0x00); // DbName
    bytes.push(3);
    bytes.extend_from_slice(&utf16le("dbo"));
    bytes.push(1);
    bytes.extend_from_slice(&utf16le("T"));
    bytes.extend_from_slice(&1u16.to_le_bytes()); // column count
    bytes.extend_from_slice(&0u32.to_le_bytes()); // UserType
    bytes.extend_from_slice(&0u16.to_le_bytes()); // Flags
    bytes.extend_from_slice(&[0x26, 0x04]); // INTNTYPE, 4 bytes
    bytes.push(0x00); // ColName, empty
    bytes.push(0x00); // TVP_END_TOKEN
    bytes.push(0x01); // TVP_ROW_TOKEN
    bytes.extend_from_slice(&[0x04, 0x05, 0x00, 0x00, 0x00]);
    bytes.push(0x00); // TVP_END_TOKEN
    bytes
}

/// The body of one RPC to `sp_executesql` by ProcID ([MS-TDS] 2.2.6.6): `NameLenProcID`,
/// `OptionFlags`, then the parameters.
fn executesql_body(params: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFF, 0x0A, 0x00, 0x00, 0x00];
    for param in params {
        bytes.extend_from_slice(param);
    }
    bytes
}

/// `sp_executesql` of `statement`, declared with `declaration`, with `value` as `@x`.
fn executesql_payload(statement: &str, declaration: &str, value: Vec<u8>) -> Vec<u8> {
    let mut payload = all_headers();
    payload.extend_from_slice(&executesql_body(&[
        nvarchar_param("", statement),
        nvarchar_param("", declaration),
        value,
    ]));
    payload
}

/// The RPC messages this version reads whole and refuses, by name.
fn refused_rpcs() -> Vec<(&'static str, Vec<u8>)> {
    let mut batch = all_headers();
    batch.extend_from_slice(&executesql_body(&[nvarchar_param("", "SELECT 1")]));
    batch.push(0xFF); // BatchFlag
    batch.extend_from_slice(&executesql_body(&[nvarchar_param("", "SELECT 2")]));

    let mut encrypted = executesql_payload(
        "SELECT @x",
        "@x int",
        param("@x", &[0x26, 0x04], &[0x04, 0x01, 0x00, 0x00, 0x00]),
    );
    let flags = encrypted.len() - 8; // StatusFlags, before INTNTYPE (2) and the value (5)
    encrypted[flags] = 0x08; // fEncrypted

    vec![
        (
            "xml",
            executesql_payload("SELECT @x", "@x xml", xml_param("@x", "<a/>")),
        ),
        (
            "table-valued",
            executesql_payload("SELECT 1", "@x dbo.T READONLY", tvp_param("@x")),
        ),
        (
            "sql_variant",
            executesql_payload("SELECT @x", "@x sql_variant", sql_variant_param("@x")),
        ),
        ("RPC batch", batch),
        ("encrypted", encrypted),
    ]
}

/// The answer to a refused message: error 50000, severity 16, state 1, then a DONE with
/// `DONE_ERROR` and no count.
fn refusal_tokens() -> Vec<Tok> {
    vec![
        Tok::Error {
            number: 50000,
            state: 1,
            class: 16,
        },
        Tok::Done {
            status: DONE_ERROR,
            cur_cmd: 0,
            row_count: 0,
        },
    ]
}

/// Reads until the server hangs up: EOF, or a reset when it closed with data queued.
async fn assert_closed(client: &mut TcpStream, what: &str) {
    let mut buffer = [0u8; 64];
    let closed = timeout(BUDGET, client.read(&mut buffer))
        .await
        .unwrap_or_else(|_| panic!("{what}: the connection must be closed within {BUDGET:?}"));
    assert!(
        matches!(closed, Ok(0) | Err(_)),
        "{what}: the server answered instead of closing: {closed:?}"
    );
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn refused_rpc_gets_an_error_and_the_connection_stays_usable() {
    let running = start().await;
    for (what, payload) in refused_rpcs() {
        let mut client = connect_and_login(running.addr).await;
        client
            .write_all(&packet(PACKET_RPC, &payload))
            .await
            .unwrap();
        let answer = read_response(&mut client, BUDGET).await;
        assert_eq!(tokens(&answer), refusal_tokens(), "{what}");
        assert_still_usable(&mut client).await;
        // Twice in a row on the same connection: the first refusal left nothing behind.
        client
            .write_all(&packet(PACKET_RPC, &payload))
            .await
            .unwrap();
        let answer = read_response(&mut client, BUDGET).await;
        assert_eq!(tokens(&answer), refusal_tokens(), "{what}, second time");
        assert_still_usable(&mut client).await;
    }
    running.stop().await;
}

#[tokio::test]
async fn refused_rpc_over_several_packets_is_read_to_its_end() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    // An xml value of 6000 UTF-16 units: the message needs four packets of 4096 bytes.
    let payload = executesql_payload("SELECT @x", "@x xml", xml_param("@x", &"a".repeat(6000)));
    let chunks: Vec<&[u8]> = payload
        .chunks(usize::from(DEFAULT_PACKET_SIZE) - 8)
        .collect();
    assert!(chunks.len() > 1);
    for (i, chunk) in chunks.iter().enumerate() {
        let status = if i + 1 == chunks.len() {
            STATUS_EOM
        } else {
            STATUS_NORMAL
        };
        client
            .write_all(&packet_with_status(PACKET_RPC, status, chunk))
            .await
            .unwrap();
    }
    let answer = read_response(&mut client, BUDGET).await;
    assert_eq!(tokens(&answer), refusal_tokens());
    assert_still_usable(&mut client).await;
    running.stop().await;
}

#[tokio::test]
async fn bulk_load_gets_an_error_and_the_connection_stays_usable() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    // A row of data with no INSERT BULK before it: TDS_ROW, one int, TDS_DONE.
    let mut payload = vec![0xD1, 0x05, 0x00, 0x00, 0x00, 0xFD];
    payload.extend_from_slice(&[0u8; 12]);
    client
        .write_all(&packet(PACKET_BULK_LOAD, &payload))
        .await
        .unwrap();
    let answer = read_response(&mut client, BUDGET).await;
    assert_eq!(tokens(&answer), refusal_tokens());
    assert_still_usable(&mut client).await;
    running.stop().await;
}

#[tokio::test]
async fn malformed_rpc_closes_the_connection() {
    let running = start().await;
    let mut unknown_type = all_headers();
    unknown_type.extend_from_slice(&executesql_body(&[
        nvarchar_param("", "SELECT 1"),
        param("", &[0x99], &[]),
    ]));
    let mut truncated = executesql_payload("SELECT @x", "@x int", nvarchar_param("@x", "1"));
    truncated.truncate(truncated.len() - 3);
    // An unknown type token before an xml parameter: the stream is broken before the
    // refused part is reached.
    let mut broken_then_xml = all_headers();
    broken_then_xml.extend_from_slice(&executesql_body(&[
        nvarchar_param("", "SELECT @x"),
        param("", &[0x99], &[]),
        xml_param("@x", "<a/>"),
    ]));
    for (what, payload) in [
        ("unknown type token", unknown_type),
        ("truncated value", truncated),
        ("unknown type before xml", broken_then_xml),
    ] {
        let mut client = connect_and_login(running.addr).await;
        client
            .write_all(&packet(PACKET_RPC, &payload))
            .await
            .unwrap();
        assert_closed(&mut client, what).await;
    }
    running.stop().await;
}

/// Sends `text` as a SQL_BATCH under `descriptor` and returns its raw response.
async fn batch_raw(client: &mut TcpStream, text: &str, descriptor: u64) -> Vec<u8> {
    let mut payload = all_headers_in(descriptor);
    payload.extend_from_slice(&utf16le(text));
    client
        .write_all(&packet(PACKET_SQL_BATCH, &payload))
        .await
        .unwrap();
    read_response(client, BUDGET).await
}

/// Sends `text` as a SQL_BATCH under `descriptor` and returns the tokens of its response.
async fn batch(client: &mut TcpStream, text: &str, descriptor: u64) -> Vec<Tok> {
    tokens(&batch_raw(client, text, descriptor).await)
}

/// The descriptor of the ENVCHANGE Begin Transaction (type 8, [MS-TDS] 2.2.7.9) of a
/// response.
fn begun_descriptor(payload: &[u8]) -> u64 {
    let at = payload
        .windows(4)
        .position(|w| w[0] == TOKEN_ENVCHANGE && w[3] == 0x08)
        .expect("an ENVCHANGE Begin Transaction");
    assert_eq!(payload[at + 4], 8, "new value of 8 bytes");
    u64::from_le_bytes(payload[at + 5..at + 13].try_into().unwrap())
}

/// The single `int` a one-row, one-column response carries.
fn single_int(answer: &[Tok]) -> i64 {
    let rows: Vec<&Vec<i64>> = answer
        .iter()
        .filter_map(|tok| match tok {
            Tok::Row(values) => Some(values),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "{answer:?}");
    assert_eq!(rows[0].len(), 1, "{answer:?}");
    rows[0][0]
}

#[tokio::test]
async fn refused_rpc_leaves_the_open_transaction_as_it_was() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    batch(&mut client, "CREATE TABLE dbo.kept (a int NOT NULL)", 0).await;
    let begun = batch_raw(
        &mut client,
        "BEGIN TRAN; INSERT INTO dbo.kept VALUES (5)",
        0,
    )
    .await;
    let descriptor = begun_descriptor(&begun);
    assert_ne!(descriptor, 0);

    let mut payload = all_headers_in(descriptor);
    payload.extend_from_slice(&executesql_body(&[
        nvarchar_param("", "SELECT @x"),
        nvarchar_param("", "@x xml"),
        xml_param("@x", "<a/>"),
    ]));
    client
        .write_all(&packet(PACKET_RPC, &payload))
        .await
        .unwrap();
    let answer = read_response(&mut client, BUDGET).await;
    assert_eq!(tokens(&answer), refusal_tokens());

    let trancount = "SELECT @@TRANCOUNT";
    let visible = "SELECT COUNT(*) FROM dbo.kept WHERE a = 5";
    assert_eq!(
        single_int(&batch(&mut client, trancount, descriptor).await),
        1
    );
    assert_eq!(
        single_int(&batch(&mut client, visible, descriptor).await),
        1
    );
    batch(&mut client, "COMMIT", descriptor).await;
    assert_eq!(single_int(&batch(&mut client, trancount, 0).await), 0);
    assert_eq!(single_int(&batch(&mut client, visible, 0).await), 1);
    running.stop().await;
}

/// Sends one RPC to `sp_executesql` by ProcID with `params` and returns the tokens of its
/// response.
async fn executesql(client: &mut TcpStream, params: &[Vec<u8>]) -> Vec<Tok> {
    let mut payload = all_headers();
    payload.extend_from_slice(&executesql_body(params));
    client
        .write_all(&packet(PACKET_RPC, &payload))
        .await
        .unwrap();
    tokens(&read_response(client, BUDGET).await)
}

/// The rows of a response, whatever their form.
fn rows(answer: &[Tok]) -> Vec<&Tok> {
    answer
        .iter()
        .filter(|tok| matches!(tok, Tok::Row(_) | Tok::BinaryRow(_)))
        .collect()
}

/// The errors of a response, as `(number, state)`.
fn errors(answer: &[Tok]) -> Vec<(u32, u8)> {
    answer
        .iter()
        .filter_map(|tok| match tok {
            Tok::Error { number, state, .. } => Some((*number, *state)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn executesql_statement_as_ntext_runs() {
    let running = start_with_executesql().await;
    let mut client = connect_and_login(running.addr).await;
    let answer = executesql(
        &mut client,
        &[long_text_param("", 0x63, &utf16le("SELECT 1"))],
    )
    .await;
    assert_eq!(errors(&answer), [], "{answer:?}");
    assert_eq!(rows(&answer), [&Tok::Row(vec![1])], "{answer:?}");
    assert!(
        matches!(
            answer.last(),
            Some(Tok::DoneInProc {
                kind: TOKEN_DONEPROC,
                ..
            })
        ),
        "{answer:?}"
    );
    assert_still_usable(&mut client).await;
    running.stop().await;
}

#[tokio::test]
async fn executesql_statement_as_text_is_error_214() {
    let running = start_with_executesql().await;
    let mut client = connect_and_login(running.addr).await;
    let answer = executesql(&mut client, &[long_text_param("", 0x23, b"SELECT 1")]).await;
    assert_eq!(errors(&answer), [(214, 2)], "{answer:?}");
    assert_eq!(rows(&answer), Vec::<&Tok>::new(), "{answer:?}");
    assert_still_usable(&mut client).await;
    running.stop().await;
}

#[tokio::test]
async fn executesql_image_value_reaches_a_varbinary_parameter() {
    let running = start_with_executesql().await;
    let mut client = connect_and_login(running.addr).await;
    let answer = executesql(
        &mut client,
        &[
            nvarchar_param("", "SELECT CAST(@a AS varbinary(10))"),
            nvarchar_param("", "@a varbinary(10)"),
            image_param("@a", &[0xAB, 0xCD]),
        ],
    )
    .await;
    assert_eq!(errors(&answer), [], "{answer:?}");
    assert_eq!(
        rows(&answer),
        [&Tok::BinaryRow(vec![vec![0xAB, 0xCD]])],
        "{answer:?}"
    );
    assert_still_usable(&mut client).await;
    running.stop().await;
}
