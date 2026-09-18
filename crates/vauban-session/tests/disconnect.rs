//! Integration tests of connection release: transactions and locks at disconnect, and
//! ATTENTION during a lock wait.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult};
use vauban_session::{Engine, NoAuth, ResultSink, Server, ServerConfig, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EncryptPolicy, EnvChange};
use vauban_types::{TypeInfo, Value};

const DEFAULT_PACKET_SIZE: u16 = 4096;
const TDS_7_4: u32 = 0x7400_0004;
const ATTENTION_BUDGET: Duration = Duration::from_secs(1);
const LOCK_TIMEOUT_MS: u32 = 500;

/// ATTENTION during a lock wait: lone DONE `ATTN` (status 0x0020), no 1222 before it.
const ATTENTION_DURING_LOCK_WAIT_RESPONSE: AttentionDuringLockWait =
    AttentionDuringLockWait::DoneAttnOnly;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttentionDuringLockWait {
    DoneAttnOnly,
    #[allow(dead_code)]
    Error1222ThenDoneAttn,
}

#[derive(Default)]
struct Recording(Vec<Event>);

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    Error(SqlError),
    EnvChange(EnvChange),
}

impl ResultSink for Recording {
    fn columns(&mut self, _: &[ColumnMeta]) -> SqlResult<()> {
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.0.push(Event::Row(row.to_vec()));
        Ok(())
    }
    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.0.push(Event::Done(rowcount, more));
        Ok(())
    }
    fn info(&mut self, _: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.clone()));
        Ok(())
    }
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.0.push(Event::EnvChange(change.clone()));
        Ok(())
    }
    fn return_value(&mut self, _: &str, _: &TypeInfo, _: &Value) -> SqlResult<()> {
        Ok(())
    }
    fn return_status(&mut self, _: i32) -> SqlResult<()> {
        Ok(())
    }
}

fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
}

fn session(engine: &Arc<Engine>) -> Session {
    Session::new(Arc::clone(engine), SessionState::new(91))
}

fn run(session: &mut Session, text: &str) -> Vec<Event> {
    let events = run_on(session, text);
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error(_))),
        "`{text}` raised an error: {events:#?}"
    );
    events
}

fn run_on(session: &mut Session, text: &str) -> Vec<Event> {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("errors go through the sink");
    sink.0
}

fn one_int(events: &[Event]) -> i32 {
    for event in events {
        if let Event::Row(row) = event
            && let [Value::I32(n)] = row.as_slice()
        {
            return *n;
        }
    }
    panic!("no int row in {events:#?}");
}

struct Running {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), InternalError>>,
}

async fn start(engine: Arc<Engine>) -> Running {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = Server::new(
        engine,
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(NoAuth),
            server_name: "vauban-disconnect-test".into(),
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

impl Running {
    async fn stop(self) {
        self.shutdown.cancel();
        timeout(Duration::from_secs(6), self.task)
            .await
            .expect("serve returns")
            .expect("serve task")
            .expect("serve ok");
    }
}

#[test]
fn disconnect_releases_the_transaction() {
    let engine = engine();
    {
        let mut a = session(&engine);
        run(&mut a, "CREATE TABLE dbo.t (a int NOT NULL)");
        run(&mut a, "BEGIN TRAN");
        run(&mut a, "INSERT INTO dbo.t (a) VALUES (1)");
        assert_eq!(engine.txn.active_sessions().len(), 1);
    }
    assert!(engine.txn.active_sessions().is_empty());
    let mut b = session(&engine);
    assert!(
        run(&mut b, "SELECT a FROM dbo.t")
            .iter()
            .all(|e| !matches!(e, Event::Row(_)))
    );
}

#[test]
fn begin_tran_on_reader_still_sees_1222_under_nowait() {
    let engine = engine();
    let mut holder = session(&engine);
    run(
        &mut holder,
        "CREATE TABLE dbo.t (id int NOT NULL PRIMARY KEY, v int NOT NULL);",
    );
    run(&mut holder, "INSERT INTO dbo.t (id, v) VALUES (1, 10);");
    run(
        &mut holder,
        "BEGIN TRAN; UPDATE dbo.t SET v = 20 WHERE id = 1;",
    );
    let mut reader = session(&engine);
    run(&mut reader, "BEGIN TRAN;");
    let blocked = run_on(
        &mut reader,
        "SELECT v FROM dbo.t WITH (NOWAIT) WHERE id = 1;",
    );
    assert!(
        blocked
            .iter()
            .any(|e| matches!(e, Event::Error(err) if err.number == 1222)),
        "expected 1222 while the lock is held: {blocked:#?}"
    );
}

#[test]
fn disconnect_releases_the_locks() {
    let engine = engine();
    let mut holder = session(&engine);
    run(
        &mut holder,
        "CREATE TABLE dbo.t (id int NOT NULL PRIMARY KEY, v int NOT NULL);",
    );
    run(&mut holder, "INSERT INTO dbo.t (id, v) VALUES (1, 10);");
    run(
        &mut holder,
        "BEGIN TRAN; UPDATE dbo.t SET v = 20 WHERE id = 1;",
    );
    assert_eq!(engine.txn.active_sessions().len(), 1);

    let mut reader = session(&engine);
    let blocked = run_on(
        &mut reader,
        "SELECT v FROM dbo.t WITH (NOWAIT) WHERE id = 1;",
    );
    assert!(
        blocked
            .iter()
            .any(|e| matches!(e, Event::Error(err) if err.number == 1222)),
        "expected 1222 while the lock is held: {blocked:#?}"
    );

    drop(holder);

    let mut after = session(&engine);
    run(&mut after, &format!("SET LOCK_TIMEOUT {LOCK_TIMEOUT_MS};"));
    assert_eq!(
        one_int(&run(&mut after, "SELECT v FROM dbo.t WHERE id = 1;")),
        10
    );
}

// TDS helpers copied from `tests/attention.rs`, kept local so this file stands alone.

const PACKET_SQL_BATCH: u8 = 0x01;
const PACKET_TABULAR_RESULT: u8 = 0x04;
const PACKET_ATTENTION: u8 = 0x06;
const PACKET_LOGIN7: u8 = 0x10;
const PACKET_PRELOGIN: u8 = 0x12;
const STATUS_EOM: u8 = 0x01;
const OPTION_VERSION: u8 = 0x00;
const OPTION_ENCRYPTION: u8 = 0x01;
const OPTION_TERMINATOR: u8 = 0xFF;
const ENCRYPT_NOT_SUP: u8 = 0x02;
const HEADER_TRANSACTION_DESCRIPTOR: u16 = 0x0002;
const ENV_BEGIN: u8 = 8;
const TOKEN_DONE: u8 = 0xFD;
const TOKEN_ERROR: u8 = 0xAA;
const TOKEN_INFO: u8 = 0xAB;
const TOKEN_ENVCHANGE: u8 = 0xE3;
const TOKEN_LOGINACK: u8 = 0xAD;
const TOKEN_COLMETADATA: u8 = 0x81;
const TOKEN_ROW: u8 = 0xD1;
const DONE_ATTN: u16 = 0x0020;

fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![kind, STATUS_EOM];
    out.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00, 0x01, 0x00]);
    out.extend_from_slice(payload);
    out
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

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

fn login7_packet() -> Vec<u8> {
    const FIXED_LEN: usize = 94;
    let mut data = Vec::new();
    let mut pairs = Vec::new();
    for field in [
        "testhost",
        "sa",
        "",
        "vauban-disconnect-test",
        "localhost",
        "",
        "hand-made",
        "",
        "",
    ] {
        pairs.push(((FIXED_LEN + data.len()) as u16, field.len() as u16));
        data.extend_from_slice(&utf16le(field));
    }
    let mut fixed = vec![0u8; FIXED_LEN];
    fixed[0..4].copy_from_slice(&((FIXED_LEN + data.len()) as u32).to_le_bytes());
    fixed[4..8].copy_from_slice(&TDS_7_4.to_le_bytes());
    fixed[8..12].copy_from_slice(&u32::from(DEFAULT_PACKET_SIZE).to_le_bytes());
    for (idx, (ib, cch)) in pairs.iter().enumerate() {
        let off = 36 + idx * 4;
        fixed[off..off + 2].copy_from_slice(&ib.to_le_bytes());
        fixed[off + 2..off + 4].copy_from_slice(&cch.to_le_bytes());
    }
    fixed.extend_from_slice(&data);
    packet(PACKET_LOGIN7, &fixed)
}

fn all_headers(descriptor: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(22);
    bytes.extend_from_slice(&22u32.to_le_bytes());
    bytes.extend_from_slice(&18u32.to_le_bytes());
    bytes.extend_from_slice(&HEADER_TRANSACTION_DESCRIPTOR.to_le_bytes());
    bytes.extend_from_slice(&descriptor.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes
}

fn sql_batch_packet(text: &str) -> Vec<u8> {
    sql_batch_packet_with_descriptor(0, text)
}

fn sql_batch_packet_with_descriptor(descriptor: u64, text: &str) -> Vec<u8> {
    let mut payload = all_headers(descriptor);
    payload.extend_from_slice(&utf16le(text));
    packet(PACKET_SQL_BATCH, &payload)
}

fn varbyte_u64(body: &[u8], pos: &mut usize) -> u64 {
    let len = usize::from(body[*pos]);
    *pos += 1;
    let value = if len == 0 {
        0
    } else {
        assert_eq!(len, 8);
        u64::from_le_bytes(body[*pos..*pos + 8].try_into().unwrap())
    };
    *pos += len;
    value
}

fn transaction_descriptor(payload: &[u8]) -> Option<u64> {
    let mut pos = 0;
    while pos < payload.len() {
        let kind = payload[pos];
        pos += 1;
        match kind {
            TOKEN_ENVCHANGE => {
                let len = usize::from(u16_at(payload, pos));
                pos += 2;
                let body = &payload[pos..pos + len];
                pos += len;
                if body.first() == Some(&ENV_BEGIN) {
                    let mut body_pos = 1;
                    return Some(varbyte_u64(body, &mut body_pos));
                }
            }
            TOKEN_DONE => return None,
            TOKEN_ERROR | TOKEN_INFO | TOKEN_LOGINACK => {
                let len = usize::from(u16_at(payload, pos));
                pos += 2 + len;
            }
            _ => return None,
        }
    }
    None
}

fn attention_packet() -> Vec<u8> {
    packet(PACKET_ATTENTION, &[])
}

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
    payload
}

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    Error,
    Done { status: u16 },
    Other(u8),
}

fn u16_at(bytes: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([bytes[pos], bytes[pos + 1]])
}

fn tokens(payload: &[u8]) -> Vec<Tok> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < payload.len() {
        let kind = payload[pos];
        pos += 1;
        match kind {
            TOKEN_DONE => {
                out.push(Tok::Done {
                    status: u16_at(payload, pos),
                });
                break;
            }
            TOKEN_ERROR => {
                let len = usize::from(u16_at(payload, pos));
                pos += 2 + len;
                out.push(Tok::Error);
            }
            TOKEN_INFO | TOKEN_ENVCHANGE | TOKEN_LOGINACK => {
                let len = usize::from(u16_at(payload, pos));
                pos += 2 + len;
                out.push(Tok::Other(kind));
            }
            TOKEN_COLMETADATA => {
                let count = usize::from(u16_at(payload, pos));
                pos += 2;
                for _ in 0..count {
                    pos += 4;
                    pos += 2;
                    pos += 1;
                    let chars = usize::from(payload[pos]);
                    pos += 1 + 2 * chars;
                }
            }
            TOKEN_ROW => {
                pos += 4;
            }
            other => panic!("unexpected token type 0x{other:02X} at offset {}", pos - 1),
        }
    }
    out
}

fn has_attention_ack(tokens: &[Tok]) -> bool {
    tokens
        .iter()
        .any(|t| matches!(t, Tok::Done { status } if *status & DONE_ATTN != 0))
}

fn int4_from_payload(payload: &[u8]) -> Option<i32> {
    let mut pos = 0;
    while pos < payload.len() {
        let kind = payload[pos];
        pos += 1;
        match kind {
            TOKEN_ROW if pos + 4 <= payload.len() => {
                let bytes = [
                    payload[pos],
                    payload[pos + 1],
                    payload[pos + 2],
                    payload[pos + 3],
                ];
                return Some(i32::from_le_bytes(bytes));
            }
            TOKEN_DONE => return None,
            TOKEN_ERROR | TOKEN_INFO | TOKEN_ENVCHANGE | TOKEN_LOGINACK => {
                let len = usize::from(u16_at(payload, pos));
                pos += 2 + len;
            }
            TOKEN_COLMETADATA => {
                let count = usize::from(u16_at(payload, pos));
                pos += 2;
                for _ in 0..count {
                    pos += 4;
                    pos += 2;
                    pos += 1;
                    let chars = usize::from(payload[pos]);
                    pos += 1 + 2 * chars;
                }
            }
            _ => return None,
        }
    }
    None
}

async fn assert_still_waiting(client: &mut TcpStream) {
    assert!(
        timeout(Duration::from_millis(150), read_packet(client))
            .await
            .is_err(),
        "the client answered before the lock wait was interrupted"
    );
}

async fn session_a_holds_exclusive_row_lock(engine: &Arc<Engine>, a: &mut TcpStream) {
    a.write_all(&sql_batch_packet(
        "BEGIN TRAN; UPDATE dbo.t SET v = 20 WHERE id = 1;",
    ))
    .await
    .unwrap();
    let held = read_response(a, Duration::from_secs(2)).await;
    assert!(
        !tokens(&held).iter().any(|t| matches!(t, Tok::Error)),
        "session A failed to take the lock: {held:?}"
    );
    let blocked = run_on(
        &mut session(engine),
        "SELECT v FROM dbo.t WITH (NOWAIT) WHERE id = 1;",
    );
    assert!(
        blocked
            .iter()
            .any(|e| matches!(e, Event::Error(err) if err.number == 1222)),
        "expected 1222 while A holds the lock: {blocked:#?}"
    );
}

async fn start_blocking_row_read(b: &mut TcpStream, descriptor: u64, sql: &str) {
    b.write_all(&sql_batch_packet_with_descriptor(descriptor, sql))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_still_waiting(b).await;
}

async fn hold_exclusive_row_lock(engine: &Arc<Engine>, a: &mut TcpStream, b: &mut TcpStream) {
    session_a_holds_exclusive_row_lock(engine, a).await;
    b.write_all(&sql_batch_packet("SET LOCK_TIMEOUT -1;"))
        .await
        .unwrap();
    let _ = read_response(b, Duration::from_secs(2)).await;
    start_blocking_row_read(b, 0, "SELECT v FROM dbo.t WHERE id = 1;").await;
}

async fn connect_and_login(addr: SocketAddr) -> TcpStream {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&prelogin_packet()).await.unwrap();
    let prelogin = timeout(Duration::from_secs(5), read_packet(&mut client))
        .await
        .expect("PRELOGIN response within 5 s");
    assert_eq!(prelogin[0], PACKET_TABULAR_RESULT);
    client.write_all(&login7_packet()).await.unwrap();
    let payload = read_response(&mut client, Duration::from_secs(5)).await;
    assert_eq!(tokens(&payload).last(), Some(&Tok::Done { status: 0 }));
    client
}

#[tokio::test]
async fn attention_during_a_lock_wait_returns_in_under_a_second() {
    let engine = engine();
    run(
        &mut session(&engine),
        "CREATE TABLE dbo.t (id int NOT NULL PRIMARY KEY, v int NOT NULL);",
    );
    run(
        &mut session(&engine),
        "INSERT INTO dbo.t (id, v) VALUES (1, 10);",
    );
    let running = start(Arc::clone(&engine)).await;
    let mut a = connect_and_login(running.addr).await;
    let mut b = connect_and_login(running.addr).await;

    hold_exclusive_row_lock(&engine, &mut a, &mut b).await;

    let sent = Instant::now();
    b.write_all(&attention_packet()).await.unwrap();
    let payload = read_response(&mut b, ATTENTION_BUDGET).await;
    assert!(
        sent.elapsed() < ATTENTION_BUDGET,
        "answered after {:?}, expected under {:?}",
        sent.elapsed(),
        ATTENTION_BUDGET
    );

    match ATTENTION_DURING_LOCK_WAIT_RESPONSE {
        AttentionDuringLockWait::DoneAttnOnly => {
            assert!(
                has_attention_ack(&tokens(&payload)),
                "expected DONE ATTN, got {:?}",
                tokens(&payload)
            );
        }
        AttentionDuringLockWait::Error1222ThenDoneAttn => {
            let seen = tokens(&payload);
            assert!(seen.contains(&Tok::Error), "expected 1222 before DONE ATTN");
            assert!(has_attention_ack(&seen), "expected DONE ATTN in {seen:?}");
        }
    }

    drop(a);
    drop(b);
    running.stop().await;
}

#[tokio::test]
async fn attention_keeps_the_transaction_open() {
    let engine = engine();
    run(
        &mut session(&engine),
        "CREATE TABLE dbo.t (id int NOT NULL PRIMARY KEY, v int NOT NULL);",
    );
    run(
        &mut session(&engine),
        "INSERT INTO dbo.t (id, v) VALUES (1, 10);",
    );
    let running = start(Arc::clone(&engine)).await;
    let mut a = connect_and_login(running.addr).await;
    let mut b = connect_and_login(running.addr).await;

    b.write_all(&sql_batch_packet("BEGIN TRAN;")).await.unwrap();
    let begin_payload = read_response(&mut b, Duration::from_secs(2)).await;
    let descriptor = transaction_descriptor(&begin_payload)
        .expect("BEGIN TRAN must announce a transaction descriptor");

    session_a_holds_exclusive_row_lock(&engine, &mut a).await;
    start_blocking_row_read(&mut b, descriptor, "SELECT v FROM dbo.t WHERE id = 1;").await;
    b.write_all(&attention_packet()).await.unwrap();
    let _ = read_response(&mut b, ATTENTION_BUDGET).await;

    b.write_all(&sql_batch_packet_with_descriptor(
        descriptor,
        "SELECT @@TRANCOUNT;",
    ))
    .await
    .unwrap();
    let payload = read_response(&mut b, Duration::from_secs(2)).await;
    // After ATTENTION during a lock wait, @@TRANCOUNT stays at 1.
    assert_eq!(
        int4_from_payload(&payload),
        Some(1),
        "expected @@TRANCOUNT = 1 after ATTENTION, payload len {}",
        payload.len()
    );

    drop(a);
    drop(b);
    running.stop().await;
}
