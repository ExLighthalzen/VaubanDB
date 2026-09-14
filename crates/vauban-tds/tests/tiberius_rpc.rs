//! A real client (`tiberius`, driven through its public API) sends a
//! parameterized query to a fake server built on `vauban-tds` alone. The client turns
//! `query("SELECT @P1", &[&42i32])` into an RPC to `sp_executesql` (ProcID 10,
//! [MS-TDS] 2.2.6.6) whose three parameters are the text, the declaration `@P1 int` and the
//! typed value; the fake server decodes it with `rpc::decode` and answers with the value of
//! the third parameter.
//!
//! The fake server lives here and nowhere else (the login sequence is a copy of the one in
//! `tests/tiberius_select1.rs`, the two files do not share a module). It sends every
//! decoded RPC on a channel so that the test can inspect what the client actually sent.
//! A TCP relay copies the raw bytes too: the RPC packet is printed on stderr so that the
//! exact format a real driver uses can be read from the test output.
//!
//! Everything runs on `127.0.0.1:0`, without encryption: no external network.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use vauban_errors::InfoMessage;
use vauban_tds::{
    ClientMessage, ColumnFlags, ColumnMeta, DoneStatus, EncryptPolicy, EnvChange, Login7, Rpc,
    RpcProc, TdsError, TdsStream, Token,
};
use vauban_types::{Collation, SqlString, SqlType, TypeInfo, Value};

/// Credentials configured on the `tiberius` side; the fake server does not check them.
const USERNAME: &str = "sa";
const PASSWORD: &str = "Sup3r-Secret!é";
/// TDS 7.4 as written in LOGINACK ([MS-TDS] 2.2.7 LOGINACK: `74 00 00 04`).
const TDS_VERSION_7_4: u32 = 0x7400_0004;
/// `MajorVer`, `MinorVer`, `BuildNumHi`, `BuildNumLow` announced in LOGINACK (16.0.1000).
const SERVER_VERSION: [u8; 4] = [0x10, 0x00, 0x03, 0xE8];
/// `CurCmd` of the DONEPROC token of a SELECT.
const CUR_CMD_SELECT: u16 = 0xC1;
/// Packet size in force before the LOGIN7 ([MS-TDS] 2.2.6.4 PacketSize).
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// Packet type of an RPC message ([MS-TDS] 2.2.3.1.1).
const PACKET_TYPE_RPC: u8 = 0x03;
/// Upper bound of any single test, so that a protocol misunderstanding fails instead of hanging.
const TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Server TLS configuration built from the fixtures (TLS 1.2 only). `accept` wants
/// one even when the policy is `Off`; it is never used by these tests.
fn tls_config() -> Arc<rustls::ServerConfig> {
    const CERT_PEM: &[u8] = include_bytes!("fixtures/test-cert.pem");
    const KEY_PEM: &[u8] = include_bytes!("fixtures/test-key.pem");

    let certs = rustls_pemfile::certs(&mut &CERT_PEM[..])
        .collect::<Result<Vec<_>, _>>()
        .expect("test certificate fixture is valid PEM");
    let key = rustls_pemfile::private_key(&mut &KEY_PEM[..])
        .expect("test key fixture is valid PEM")
        .expect("test key fixture holds a private key");
    let config = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("test certificate and key match");
    Arc::new(config)
}

/// The login response, in the order a client expects it (same as `tiberius_select1.rs`).
fn login_response(login: &Login7, packet_size: u16) -> Vec<Token> {
    let database = login.database.clone().unwrap_or_else(|| "master".into());
    vec![
        Token::EnvChange(EnvChange::Database {
            old: "master".into(),
            new: database.clone(),
        }),
        Token::Info(InfoMessage {
            number: 5701,
            severity: 0,
            state: 2,
            message: format!("Changed database context to '{database}'."),
            line: 1,
        }),
        Token::EnvChange(EnvChange::Collation {
            old: None,
            new: Collation::DEFAULT,
        }),
        Token::EnvChange(EnvChange::Language {
            old: String::new(),
            new: "us_english".into(),
        }),
        Token::Info(InfoMessage {
            number: 5703,
            severity: 0,
            state: 1,
            message: "Changed language setting to us_english.".into(),
            line: 1,
        }),
        Token::EnvChange(EnvChange::PacketSize {
            old: DEFAULT_PACKET_SIZE,
            new: packet_size,
        }),
        Token::LoginAck {
            tds_version: TDS_VERSION_7_4,
            program_name: "VaubanDB".into(),
            version: SERVER_VERSION,
        },
        Token::Done {
            status: DoneStatus::FINAL,
            cur_cmd: 0,
            row_count: None,
        },
    ]
}

/// DONE closing a request that produced nothing.
fn done_final() -> Token {
    Token::Done {
        status: DoneStatus::FINAL,
        cur_cmd: 0,
        row_count: None,
    }
}

/// The response to an RPC: to `sp_executesql` by ProcID, one
/// anonymous column typed like the third parameter, one row holding its value, DONEPROC
/// with a count of 1. Anything else, including an `sp_executesql` with fewer than three
/// parameters, gets a DONE `FINAL`.
fn answer(rpc: &Rpc) -> Vec<Token> {
    if rpc.proc != RpcProc::Id(10) {
        return vec![done_final()];
    }
    let Some(param) = rpc.params.get(2) else {
        return vec![done_final()];
    };
    let ty = TypeInfo {
        nullable: true,
        ..param.ty.clone()
    };
    vec![
        Token::ColMetaData(vec![ColumnMeta {
            name: String::new(),
            ty,
            flags: ColumnFlags {
                nullable: true,
                ..ColumnFlags::default()
            },
        }]),
        Token::Row(vec![param.value.clone()]),
        Token::DoneProc {
            status: DoneStatus::COUNT,
            cur_cmd: CUR_CMD_SELECT,
            row_count: Some(1),
        },
    ]
}

/// Serves one client connection until it closes, delivering every decoded RPC on `rpcs`.
async fn serve_connection(
    tcp: TcpStream,
    rpcs: mpsc::UnboundedSender<Rpc>,
) -> Result<(), TdsError> {
    let mut stream = TdsStream::accept(tcp, Some(tls_config()), EncryptPolicy::Off).await?;

    let login = match stream.read_message().await? {
        ClientMessage::Login7(login) => login,
        _ => return Err(TdsError::Unsupported("expected a LOGIN7 after PRELOGIN")),
    };
    // [MS-TDS] 2.2.6.4 PacketSize: the client's request, bounded to what the server allows.
    let packet_size = login.packet_size.clamp(512, 32767) as u16;

    let mut stream = stream.downgrade_encryption().await;
    stream.set_packet_size(packet_size);
    stream.set_spid(51);
    stream
        .write_tokens(&login_response(&login, packet_size))
        .await?;
    stream.flush().await?;

    loop {
        let message = match stream.read_message().await {
            Ok(message) => message,
            Err(TdsError::ConnectionClosed) => return Ok(()),
            Err(err) => return Err(err),
        };
        let tokens = match message {
            ClientMessage::Rpc(rpc) => {
                let tokens = answer(&rpc);
                // The test inspects the RPC; the receiver may already be gone, which is fine.
                let _ = rpcs.send(rpc);
                tokens
            }
            ClientMessage::Attention => vec![Token::Done {
                status: DoneStatus::ATTN,
                cur_cmd: 0,
                row_count: None,
            }],
            // SQL_BATCH, TRANSACTION_MANAGER, unsupported packets: nothing to run.
            _ => vec![done_final()],
        };
        stream.write_tokens(&tokens).await?;
        stream.flush().await?;
    }
}

/// Starts the fake server on an ephemeral port. Returns its address and the channel on
/// which every decoded RPC is delivered.
async fn spawn_server() -> (SocketAddr, mpsc::UnboundedReceiver<Rpc>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(err) = serve_connection(tcp, tx).await {
                    eprintln!("fake server: connection ended with {err}");
                }
            });
        }
    });
    (addr, rx)
}

/// Every byte the client sent to the fake server, copied by a TCP relay: the exact RPC
/// format a real driver uses.
#[derive(Clone, Default)]
struct WireTap {
    client_to_server: Arc<Mutex<Vec<u8>>>,
}

impl WireTap {
    fn client_to_server(&self) -> Vec<u8> {
        self.client_to_server.lock().unwrap().clone()
    }
}

/// Copies `reader` into `writer` until EOF, appending everything to `log` when given.
async fn relay<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    log: Option<Arc<Mutex<Vec<u8>>>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Some(log) = &log {
            log.lock().unwrap().extend_from_slice(&buf[..n]);
        }
        if writer.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// Starts a relay on an ephemeral port that forwards its first connection to `upstream`
/// and copies the client-to-server direction into the tap.
async fn spawn_tap(upstream: SocketAddr) -> (SocketAddr, WireTap) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tap = WireTap::default();
    let up_log = tap.client_to_server.clone();
    tokio::spawn(async move {
        let Ok((client, _)) = listener.accept().await else {
            return;
        };
        let Ok(server) = TcpStream::connect(upstream).await else {
            return;
        };
        let (client_read, client_write) = client.into_split();
        let (server_read, server_write) = server.into_split();
        let up = tokio::spawn(relay(client_read, server_write, Some(up_log)));
        let down = tokio::spawn(relay(server_read, client_write, None));
        let _ = tokio::join!(up, down);
    });
    (addr, tap)
}

/// The payloads of every packet of type `kind` in a clear-text byte stream, concatenated
/// per message ([MS-TDS] 2.2.3.1: 8-byte header with the type, the status whose bit 0 is
/// EOM, and the big-endian total length).
fn payloads_of(stream: &[u8], kind: u8) -> Vec<Vec<u8>> {
    let mut messages = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut at = 0;
    while at + 8 <= stream.len() {
        let packet_type = stream[at];
        let eom = stream[at + 1] & 0x01 != 0;
        let length = usize::from(u16::from_be_bytes([stream[at + 2], stream[at + 3]]));
        assert!(length >= 8 && at + length <= stream.len(), "packet header");
        if packet_type == kind {
            current
                .get_or_insert_with(Vec::new)
                .extend_from_slice(&stream[at + 8..at + length]);
            if eom {
                messages.extend(current.take());
            }
        }
        at += length;
    }
    messages
}

/// `Value::String` of `s`.
fn s(text: &str) -> Value {
    Value::String(SqlString { text: text.into() })
}

/// Connects a `tiberius` client with SQL authentication and no encryption.
async fn connect(addr: SocketAddr) -> Client<Compat<TcpStream>> {
    let mut config = Config::new();
    config.host(addr.ip().to_string());
    config.port(addr.port());
    config.authentication(AuthMethod::sql_server(USERNAME, PASSWORD));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();

    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    Client::connect(config, tcp.compat_write())
        .await
        .expect("tiberius connects to the fake server")
}

/// Runs `test` under the per-test timeout.
async fn with_timeout<F: std::future::Future<Output = ()>>(test: F) {
    tokio::time::timeout(TEST_TIMEOUT, test)
        .await
        .expect("test finished before the timeout");
}

#[tokio::test]
async fn tiberius_parameterized_query_roundtrip() {
    with_timeout(async {
        let (server, mut rpcs) = spawn_server().await;
        let (addr, tap) = spawn_tap(server).await;
        let mut client = connect(addr).await;

        let rows = client
            .query("SELECT @P1", &[&42i32])
            .await
            .expect("the parameterized query is accepted")
            .into_first_result()
            .await
            .expect("the query yields a result set");
        assert_eq!(rows.len(), 1, "exactly one row");
        assert_eq!(rows[0].get::<i32, _>(0), Some(42));

        // What the fake server decoded: sp_executesql by ProcID with the text, the
        // declaration and the unnamed typed value.
        let rpc = rpcs.recv().await.expect("the fake server decoded an RPC");
        assert_eq!(rpc.proc, RpcProc::Id(10));
        assert_eq!(rpc.transaction_descriptor, 0);
        assert_eq!(rpc.params.len(), 3, "{rpc:?}");
        assert_eq!(rpc.params[0].value, s("SELECT @P1"));
        assert_eq!(rpc.params[1].value, s("@P1 int"));
        assert_eq!(rpc.params[2].value, Value::I32(42));
        assert_eq!(rpc.params[2].ty.ty, SqlType::Int);
        for param in &rpc.params {
            assert!(!param.output, "{param:?}");
            assert!(!param.default, "{param:?}");
        }

        // What travelled on the wire: one RPC message in clear text, printed on stderr.
        let messages = payloads_of(&tap.client_to_server(), PACKET_TYPE_RPC);
        assert_eq!(messages.len(), 1, "one RPC message");
        let payload = &messages[0];
        eprintln!(
            "RPC payload sent by tiberius ({} bytes): {payload:02X?}",
            payload.len()
        );
        eprintln!("decoded as: {rpc:?}");
        // ALL_HEADERS (22 bytes) then ProcIDSwitch and ProcID 10.
        assert_eq!(&payload[..4], &[0x16, 0x00, 0x00, 0x00], "TotalLength 22");
        assert_eq!(&payload[22..26], &[0xFF, 0xFF, 0x0A, 0x00], "ProcID 10");
    })
    .await;
}
