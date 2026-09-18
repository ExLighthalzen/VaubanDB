//! Wire-level integration tests for TRANSACTION_MANAGER.
//!
//! The client is hand-written from [MS-TDS] 2.2.3.1, 2.2.5.3.1, 2.2.6.4, 2.2.6.5,
//! 2.2.6.9 and 2.2.7. It checks the exact ENVCHANGE order and the DONE fields,
//! then reuses the same connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tokio_util::sync::CancellationToken;
use vauban_errors::InternalError;
use vauban_session::{EncryptPolicy, Engine, NoAuth, Server, ServerConfig};
use vauban_storage::MemoryStorage;

const PACKET_SQL_BATCH: u8 = 0x01;
const PACKET_TABULAR_RESULT: u8 = 0x04;
const PACKET_TRANSACTION_MANAGER: u8 = 0x0E;
const PACKET_LOGIN7: u8 = 0x10;
const PACKET_PRELOGIN: u8 = 0x12;
const STATUS_EOM: u8 = 0x01;
const TDS_7_4: u32 = 0x7400_0004;
const PACKET_SIZE: u16 = 4096;
const HEADER_TRANSACTION_DESCRIPTOR: u16 = 0x0002;
const TM_GET_DTC_ADDRESS: u16 = 0;
const TM_BEGIN_XACT: u16 = 5;
const TM_COMMIT_XACT: u16 = 7;
const TM_ROLLBACK_XACT: u16 = 8;
const TOKEN_ERROR: u8 = 0xAA;
const TOKEN_INFO: u8 = 0xAB;
const TOKEN_ENVCHANGE: u8 = 0xE3;
const TOKEN_DONE: u8 = 0xFD;
const ENV_BEGIN: u8 = 8;
const ENV_COMMIT: u8 = 9;
const ENV_ROLLBACK: u8 = 10;
const DONE_ERROR: u16 = 0x0002;
const CUR_CMD_TRANSACTION_MANAGER: u16 = 0x00FD;

struct Running {
    addr: SocketAddr,
    engine: Arc<Engine>,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), InternalError>>,
}

impl Running {
    async fn stop(self) {
        self.shutdown.cancel();
        timeout(Duration::from_secs(6), self.task)
            .await
            .expect("serve must stop within six seconds")
            .expect("serve task must not panic")
            .expect("serve returns Ok");
    }
}

async fn start() -> Running {
    vauban_sysfn::register_builtins();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::default())));
    let server = Server::new(
        Arc::clone(&engine),
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(NoAuth),
            server_name: "vauban-transaction-test".into(),
            default_packet_size: PACKET_SIZE,
            program_name: None,
            version_banner: None,
            edition: None,
        },
    );
    let task = tokio::spawn(server.serve(listener, shutdown.clone()));
    Running {
        addr,
        engine,
        shutdown,
        task,
    }
}

fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![kind, STATUS_EOM];
    out.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(payload);
    out
}

fn prelogin_packet() -> Vec<u8> {
    let version = [0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00];
    let data_start = 11u16;
    let mut payload = vec![0x00];
    payload.extend_from_slice(&data_start.to_be_bytes());
    payload.extend_from_slice(&(version.len() as u16).to_be_bytes());
    payload.push(0x01);
    payload.extend_from_slice(&(data_start + version.len() as u16).to_be_bytes());
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.push(0xFF);
    payload.extend_from_slice(&version);
    payload.push(0x02); // ENCRYPT_NOT_SUP
    packet(PACKET_PRELOGIN, &payload)
}

fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn login7_packet() -> Vec<u8> {
    const FIXED_LEN: usize = 94;
    let mut data = Vec::new();
    let mut pairs = Vec::new();
    for field in [
        "testhost",
        "sa",
        "",
        "vauban-transaction-test",
        "localhost",
        "",
        "hand-made",
        "",
        "",
    ] {
        pairs.push(((FIXED_LEN + data.len()) as u16, field.len() as u16));
        data.extend_from_slice(&utf16le(field));
    }

    let mut fixed = Vec::with_capacity(FIXED_LEN);
    fixed.extend_from_slice(&((FIXED_LEN + data.len()) as u32).to_le_bytes());
    fixed.extend_from_slice(&TDS_7_4.to_le_bytes());
    fixed.extend_from_slice(&u32::from(PACKET_SIZE).to_le_bytes());
    fixed.extend_from_slice(&0u32.to_le_bytes());
    fixed.extend_from_slice(&4242u32.to_le_bytes());
    fixed.extend_from_slice(&0u32.to_le_bytes());
    fixed.extend_from_slice(&[0; 4]);
    fixed.extend_from_slice(&0i32.to_le_bytes());
    fixed.extend_from_slice(&0x0409u32.to_le_bytes());
    for (offset, chars) in pairs {
        fixed.extend_from_slice(&offset.to_le_bytes());
        fixed.extend_from_slice(&chars.to_le_bytes());
    }
    fixed.extend_from_slice(&[0; 6]);
    fixed.extend_from_slice(&[0; 4]);
    fixed.extend_from_slice(&[0; 4]);
    fixed.extend_from_slice(&[0; 4]);
    fixed.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(fixed.len(), FIXED_LEN);
    fixed.extend_from_slice(&data);
    packet(PACKET_LOGIN7, &fixed)
}

fn all_headers(descriptor: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(22);
    out.extend_from_slice(&22u32.to_le_bytes());
    out.extend_from_slice(&18u32.to_le_bytes());
    out.extend_from_slice(&HEADER_TRANSACTION_DESCRIPTOR.to_le_bytes());
    out.extend_from_slice(&descriptor.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out
}

fn tm_begin_packet() -> Vec<u8> {
    tm_begin_packet_with_isolation(0)
}

fn tm_begin_packet_with_isolation(isolation: u8) -> Vec<u8> {
    let mut payload = all_headers(0);
    payload.extend_from_slice(&TM_BEGIN_XACT.to_le_bytes());
    payload.extend_from_slice(&[isolation, 0]); // isolation, empty BEGIN_XACT_NAME
    packet(PACKET_TRANSACTION_MANAGER, &payload)
}

fn sql_batch_packet(descriptor: u64, text: &str) -> Vec<u8> {
    let mut payload = all_headers(descriptor);
    payload.extend_from_slice(
        &text
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    packet(PACKET_SQL_BATCH, &payload)
}

fn tm_finish_packet(request_type: u16, descriptor: u64, chain: bool) -> Vec<u8> {
    let mut payload = all_headers(descriptor);
    payload.extend_from_slice(&request_type.to_le_bytes());
    payload.push(0); // empty XACT_NAME
    payload.push(u8::from(chain));
    if chain {
        payload.extend_from_slice(&[0, 0]); // isolation, empty BEGIN_XACT_NAME
    }
    packet(PACKET_TRANSACTION_MANAGER, &payload)
}

fn tm_unsupported_packet() -> Vec<u8> {
    let mut payload = all_headers(0);
    payload.extend_from_slice(&TM_GET_DTC_ADDRESS.to_le_bytes());
    packet(PACKET_TRANSACTION_MANAGER, &payload)
}

fn empty_batch_packet() -> Vec<u8> {
    packet(PACKET_SQL_BATCH, &all_headers(0))
}

async fn read_packet(client: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 8];
    client.read_exact(&mut header).await.unwrap();
    let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
    assert!(len >= 8);
    let mut out = header.to_vec();
    out.resize(len, 0);
    client.read_exact(&mut out[8..]).await.unwrap();
    out
}

async fn read_response(client: &mut TcpStream) -> Vec<u8> {
    let mut payload = Vec::new();
    loop {
        let packet = timeout(Duration::from_secs(5), read_packet(client))
            .await
            .expect("response within five seconds");
        assert_eq!(packet[0], PACKET_TABULAR_RESULT);
        payload.extend_from_slice(&packet[8..]);
        if packet[1] & STATUS_EOM != 0 {
            return payload;
        }
    }
}

async fn connect_and_login(addr: SocketAddr) -> TcpStream {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&prelogin_packet()).await.unwrap();
    read_packet(&mut client).await;
    client.write_all(&login7_packet()).await.unwrap();
    let login = read_response(&mut client).await;
    assert_eq!(login.last().copied(), Some(0));
    client
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    EnvChange { kind: u8, new: u64, old: u64 },
    Info(u32),
    Error(u32),
    Done { status: u16, cur_cmd: u16 },
}

fn u16_at(bytes: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([bytes[pos], bytes[pos + 1]])
}

fn varbyte(body: &[u8], pos: &mut usize) -> u64 {
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

fn transaction_tokens(payload: &[u8]) -> Vec<Token> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < payload.len() {
        let kind = payload[pos];
        pos += 1;
        match kind {
            TOKEN_ENVCHANGE => {
                let len = usize::from(u16_at(payload, pos));
                pos += 2;
                let body = &payload[pos..pos + len];
                pos += len;
                let mut body_pos = 1;
                let new = varbyte(body, &mut body_pos);
                let old = varbyte(body, &mut body_pos);
                assert_eq!(body_pos, body.len());
                out.push(Token::EnvChange {
                    kind: body[0],
                    new,
                    old,
                });
            }
            TOKEN_INFO => {
                let len = usize::from(u16_at(payload, pos));
                let body = &payload[pos + 2..pos + 2 + len];
                out.push(Token::Info(u32::from_le_bytes(
                    body[..4].try_into().unwrap(),
                )));
                pos += 2 + len;
            }
            TOKEN_ERROR => {
                let len = usize::from(u16_at(payload, pos));
                let body = &payload[pos + 2..pos + 2 + len];
                out.push(Token::Error(u32::from_le_bytes(
                    body[..4].try_into().unwrap(),
                )));
                pos += 2 + len;
            }
            TOKEN_DONE => {
                out.push(Token::Done {
                    status: u16_at(payload, pos),
                    cur_cmd: u16_at(payload, pos + 2),
                });
                pos += 12;
            }
            other => panic!("unexpected transaction response token 0x{other:02X}"),
        }
    }
    out
}

async fn request(client: &mut TcpStream, packet: &[u8]) -> Vec<Token> {
    client.write_all(packet).await.unwrap();
    transaction_tokens(&read_response(client).await)
}

async fn begin_descriptor(client: &mut TcpStream) -> u64 {
    begin_descriptor_from(client, &tm_begin_packet()).await
}

const TOKEN_ROW: u8 = 0xD1;
const INTNTYPE: u8 = 0x26;

fn first_int_column(response: &[u8]) -> i32 {
    let payload = &response[8..];
    let row = payload
        .iter()
        .position(|&byte| byte == TOKEN_ROW)
        .expect("ROW token in scalar response");
    let mut pos = row + 1;
    if payload.get(1) == Some(&INTNTYPE) {
        pos += 1; // BYTELEN prefix on INTNTYPE rows
    }
    i32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap())
}

#[tokio::test]
async fn begin_commit_and_reuse_the_connection() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    let begin = request(&mut client, &tm_begin_packet()).await;
    let descriptor = match begin.as_slice() {
        [
            Token::EnvChange {
                kind: ENV_BEGIN,
                new,
                old: 0,
            },
            Token::Done {
                status: 0,
                cur_cmd: CUR_CMD_TRANSACTION_MANAGER,
            },
        ] if *new != 0 => *new,
        other => panic!("unexpected BEGIN response: {other:?}"),
    };

    let commit = request(
        &mut client,
        &tm_finish_packet(TM_COMMIT_XACT, descriptor, false),
    )
    .await;
    assert_eq!(
        commit,
        vec![
            Token::EnvChange {
                kind: ENV_COMMIT,
                new: 0,
                old: descriptor,
            },
            Token::Done {
                status: 0,
                cur_cmd: CUR_CMD_TRANSACTION_MANAGER,
            },
        ]
    );

    client.write_all(&empty_batch_packet()).await.unwrap();
    assert!(!read_response(&mut client).await.is_empty());
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn rollback_with_begin_next_is_chained_in_one_response() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    let begin = request(&mut client, &tm_begin_packet()).await;
    let Token::EnvChange { new: first, .. } = begin[0] else {
        panic!("BEGIN did not return an ENVCHANGE")
    };

    let rollback = request(
        &mut client,
        &tm_finish_packet(TM_ROLLBACK_XACT, first, true),
    )
    .await;
    assert!(matches!(
        rollback.as_slice(),
        [
            Token::EnvChange {
                kind: ENV_ROLLBACK,
                new: 0,
                old,
            },
            Token::EnvChange {
                kind: ENV_BEGIN,
                new,
                old: 0,
            },
            Token::Done {
                status: 0,
                cur_cmd: CUR_CMD_TRANSACTION_MANAGER,
            },
        ] if *old == first && *new != 0 && *new != first
    ));

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn mismatched_and_unsupported_requests_do_not_close() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    let mismatch = request(&mut client, &tm_finish_packet(TM_COMMIT_XACT, 42, false)).await;
    assert_eq!(
        mismatch,
        vec![
            Token::Error(3902),
            Token::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
            },
        ]
    );

    let unsupported = request(&mut client, &tm_unsupported_packet()).await;
    assert!(matches!(
        unsupported.as_slice(),
        [
            Token::Error(50000),
            Token::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
            },
        ]
    ));

    client.write_all(&empty_batch_packet()).await.unwrap();
    assert!(!read_response(&mut client).await.is_empty());
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn driver_begin_then_rollback_undoes_the_insert() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    client
        .write_all(&sql_batch_packet(
            0,
            "CREATE TABLE dbo.txn_driver (id int NOT NULL)",
        ))
        .await
        .unwrap();
    read_response(&mut client).await;

    let descriptor = begin_descriptor(&mut client).await;

    client
        .write_all(&sql_batch_packet(
            descriptor,
            "INSERT INTO dbo.txn_driver (id) VALUES (1)",
        ))
        .await
        .unwrap();
    let insert = read_response(&mut client).await;
    assert!(
        !insert[8..].contains(&TOKEN_ERROR),
        "insert failed: {:?}",
        &insert[8..]
    );

    client
        .write_all(&sql_batch_packet(
            descriptor,
            "SELECT COUNT(*) FROM dbo.txn_driver",
        ))
        .await
        .unwrap();
    let before = read_response(&mut client).await;
    assert!(
        !before[8..].contains(&TOKEN_ERROR),
        "count before rollback failed: {:?}",
        &before[8..]
    );
    assert_eq!(
        first_int_column(&before),
        1,
        "insert is visible before rollback"
    );

    request(
        &mut client,
        &tm_finish_packet(TM_ROLLBACK_XACT, descriptor, false),
    )
    .await;

    client
        .write_all(&sql_batch_packet(0, "SELECT COUNT(*) FROM dbo.txn_driver"))
        .await
        .unwrap();
    let count = first_int_column(&read_response(&mut client).await);
    assert_eq!(count, 0, "rollback must undo the insert");

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn driver_transaction_is_the_session_transaction() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    let descriptor = begin_descriptor(&mut client).await;

    client
        .write_all(&sql_batch_packet(descriptor, "SELECT @@TRANCOUNT"))
        .await
        .unwrap();
    let trancount = first_int_column(&read_response(&mut client).await);
    assert_eq!(trancount, 1);

    client
        .write_all(&sql_batch_packet(descriptor, "COMMIT"))
        .await
        .unwrap();
    read_response(&mut client).await;

    client
        .write_all(&sql_batch_packet(0, "SELECT @@TRANCOUNT"))
        .await
        .unwrap();
    let trancount = first_int_column(&read_response(&mut client).await);
    assert_eq!(trancount, 0);

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn begin_uses_the_isolation_level_of_the_message() {
    use vauban_txn::IsolationLevel;

    let running = start().await;
    let mut client = connect_and_login(running.addr).await;

    let first = begin_descriptor_from(&mut client, &tm_begin_packet_with_isolation(2)).await;
    assert_eq!(
        running.engine.txn.active_sessions()[0].isolation,
        IsolationLevel::ReadCommitted
    );
    request(&mut client, &tm_finish_packet(TM_COMMIT_XACT, first, false)).await;
    assert!(running.engine.txn.active_sessions().is_empty());

    request(&mut client, &tm_begin_packet_with_isolation(5)).await;
    assert_eq!(
        running.engine.txn.active_sessions()[0].isolation,
        IsolationLevel::Snapshot
    );

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn unknown_descriptor() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    let descriptor = begin_descriptor(&mut client).await;

    client
        .write_all(&sql_batch_packet(descriptor + 1, "SELECT 1"))
        .await
        .unwrap();
    let response = transaction_tokens(&read_response(&mut client).await);
    assert_eq!(
        response,
        vec![
            Token::Info(3926),
            Token::Error(3971),
            Token::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
            },
        ]
    );

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn second_tm_begin_returns_3989() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    begin_descriptor(&mut client).await;

    let second = request(&mut client, &tm_begin_packet()).await;
    assert_eq!(
        second,
        vec![
            Token::Error(3989),
            Token::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
            },
        ]
    );

    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn zero_descriptor_with_open_transaction_returns_3989() {
    let running = start().await;
    let mut client = connect_and_login(running.addr).await;
    begin_descriptor(&mut client).await;

    client
        .write_all(&sql_batch_packet(0, "SELECT 1"))
        .await
        .unwrap();
    let response = transaction_tokens(&read_response(&mut client).await);
    assert_eq!(
        response,
        vec![
            Token::Error(3989),
            Token::Done {
                status: DONE_ERROR,
                cur_cmd: 0,
            },
        ]
    );

    drop(client);
    running.stop().await;
}

async fn begin_descriptor_from(client: &mut TcpStream, packet: &[u8]) -> u64 {
    let begin = request(client, packet).await;
    match begin.as_slice() {
        [
            Token::EnvChange {
                kind: ENV_BEGIN,
                new,
                old: 0,
            },
            Token::Done { .. },
        ] if *new != 0 => *new,
        other => panic!("unexpected BEGIN response: {other:?}"),
    }
}

/// Relays one client to VaubanDB and converts the first SQL batch into the
/// TRANSACTION_MANAGER Begin request the ODBC driver sends. `tiberius` has no public
/// transaction-manager method, so the marker batch is adapted at the wire boundary while the
/// real client still consumes the ENVCHANGE response and builds its following request itself.
async fn proxy_tiberius_begin(
    listener: TcpListener,
    upstream_addr: SocketAddr,
    descriptor_tx: oneshot::Sender<u64>,
) {
    let (downstream, _) = listener.accept().await.unwrap();
    let upstream = TcpStream::connect(upstream_addr).await.unwrap();
    let (mut client_read, mut client_write) = downstream.into_split();
    let (mut server_read, mut server_write) = upstream.into_split();

    let client_to_server = async move {
        let mut transformed_begin = false;
        let mut descriptor_tx = Some(descriptor_tx);
        loop {
            let mut header = [0u8; 8];
            match client_read.read_exact(&mut header).await {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return,
                Err(error) => panic!("proxy client read failed: {error}"),
            }
            let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
            assert!(len >= 8);
            let mut payload = vec![0; len - 8];
            client_read.read_exact(&mut payload).await.unwrap();

            if header[0] == PACKET_SQL_BATCH && !transformed_begin {
                server_write.write_all(&tm_begin_packet()).await.unwrap();
                transformed_begin = true;
                continue;
            }

            if header[0] == PACKET_SQL_BATCH && transformed_begin && payload.len() >= 18 {
                let descriptor = u64::from_le_bytes(payload[10..18].try_into().unwrap());
                if let Some(tx) = descriptor_tx.take() {
                    let _ = tx.send(descriptor);
                }
            }
            server_write.write_all(&header).await.unwrap();
            server_write.write_all(&payload).await.unwrap();
        }
    };

    let server_to_client = async move {
        tokio::io::copy(&mut server_read, &mut client_write)
            .await
            .unwrap();
    };
    tokio::join!(client_to_server, server_to_client);
}

#[tokio::test]
#[ignore = "network integration test with a real tiberius client"]
async fn tiberius_keeps_the_connection_after_begin() {
    let running = start().await;
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let (descriptor_tx, descriptor_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(proxy_tiberius_begin(proxy, running.addr, descriptor_tx));

    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(proxy_addr.port());
    config.authentication(AuthMethod::sql_server("sa", "ignored"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();

    let tcp = TcpStream::connect(proxy_addr).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    let mut client = Client::connect(config, tcp.compat_write()).await.unwrap();

    let begin_results = client
        .simple_query("BEGIN")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    assert!(begin_results.is_empty());

    let row = client
        .simple_query("SELECT 1")
        .await
        .unwrap()
        .into_row()
        .await
        .unwrap()
        .expect("SELECT 1 returns one row after BEGIN");
    assert_eq!(row.get::<i32, _>(0), Some(1));
    assert_ne!(
        timeout(Duration::from_secs(5), descriptor_rx)
            .await
            .expect("proxy observes the next SQL batch")
            .expect("proxy reports its transaction descriptor"),
        0,
        "tiberius must reuse the descriptor from BEGIN's ENVCHANGE"
    );

    client.close().await.unwrap();
    timeout(Duration::from_secs(5), proxy_task)
        .await
        .expect("proxy must stop")
        .expect("proxy must not panic");
    running.stop().await;
}

#[tokio::test]
async fn tiberius_rollback_undoes_a_write() {
    async fn proxy(listener: TcpListener, upstream_addr: SocketAddr) {
        let (downstream, _) = listener.accept().await.unwrap();
        let upstream = TcpStream::connect(upstream_addr).await.unwrap();
        let (mut client_read, mut client_write) = downstream.into_split();
        let (mut server_read, mut server_write) = upstream.into_split();
        let mut saw_begin = false;

        let client_to_server = async move {
            loop {
                let mut header = [0u8; 8];
                match client_read.read_exact(&mut header).await {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return,
                    Err(error) => panic!("proxy client read failed: {error}"),
                }
                let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
                let mut payload = vec![0; len - 8];
                client_read.read_exact(&mut payload).await.unwrap();

                if header[0] == PACKET_SQL_BATCH && !saw_begin {
                    let units: Vec<u16> = payload[22..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|chunk| u16::from_le_bytes(*chunk))
                        .collect();
                    let text = String::from_utf16_lossy(&units);
                    if text.contains("CREATE TABLE dbo.txn_tiberius") {
                        server_write.write_all(&header).await.unwrap();
                        server_write.write_all(&payload).await.unwrap();
                        continue;
                    }
                    server_write.write_all(&tm_begin_packet()).await.unwrap();
                    saw_begin = true;
                    continue;
                }
                if header[0] == PACKET_SQL_BATCH && saw_begin && payload.len() >= 18 {
                    let descriptor = u64::from_le_bytes(payload[10..18].try_into().unwrap());
                    let units: Vec<u16> = payload[22..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|chunk| u16::from_le_bytes(*chunk))
                        .collect();
                    let text = String::from_utf16_lossy(&units);
                    if text.trim().eq_ignore_ascii_case("ROLLBACK") {
                        server_write
                            .write_all(&tm_finish_packet(TM_ROLLBACK_XACT, descriptor, false))
                            .await
                            .unwrap();
                        continue;
                    }
                }
                server_write.write_all(&header).await.unwrap();
                server_write.write_all(&payload).await.unwrap();
            }
        };
        let server_to_client = async move {
            tokio::io::copy(&mut server_read, &mut client_write)
                .await
                .unwrap();
        };
        tokio::join!(client_to_server, server_to_client);
    }

    let running = start().await;
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_task = tokio::spawn(proxy(proxy_listener, running.addr));

    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(proxy_addr.port());
    config.authentication(AuthMethod::sql_server("sa", "ignored"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();

    let tcp = TcpStream::connect(proxy_addr).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    let mut client = Client::connect(config, tcp.compat_write()).await.unwrap();

    client
        .simple_query("CREATE TABLE dbo.txn_tiberius (id int NOT NULL)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("BEGIN")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO dbo.txn_tiberius (id) VALUES (1)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("ROLLBACK")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let row = client
        .simple_query("SELECT COUNT(*) FROM dbo.txn_tiberius")
        .await
        .unwrap()
        .into_row()
        .await
        .unwrap()
        .expect("COUNT returns one row");
    assert_eq!(row.get::<i32, _>(0), Some(0));

    client.close().await.unwrap();
    timeout(Duration::from_secs(5), proxy_task)
        .await
        .expect("proxy must stop")
        .expect("proxy must not panic");
    running.stop().await;
}
