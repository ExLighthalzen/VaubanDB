//! A real client (`tiberius`, driven through its public API) connects to a
//! fake server built on `vauban-tds` alone and reads `SELECT 1` in the three encryption
//! modes of `EncryptPolicy`.
//!
//! The fake server lives here and nowhere else: it accepts one TCP connection per test,
//! runs `TdsStream::accept` with the policy of the test, reads the LOGIN7 (credentials are
//! kept for the test, never checked), calls `downgrade_encryption` unconditionally, answers
//! the login sequence, then answers every SQL_BATCH according to its text (`answer`).
//!
//! Everything runs on `127.0.0.1:0`: no external network.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use vauban_errors::{InfoMessage, SqlError};
use vauban_tds::{
    ClientMessage, ColumnFlags, ColumnMeta, DoneStatus, EncryptPolicy, EnvChange, Login7, TdsError,
    TdsStream, Token,
};
use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value};

/// Credentials configured on the `tiberius` side; the fake server must decode them.
const USERNAME: &str = "sa";
const PASSWORD: &str = "Sup3r-Secret!é";
/// TDS 7.4 as written in LOGINACK ([MS-TDS] 2.2.7 LOGINACK: `74 00 00 04`).
const TDS_VERSION_7_4: u32 = 0x7400_0004;
/// `MajorVer`, `MinorVer`, `BuildNumHi`, `BuildNumLow` announced in LOGINACK (16.0.1000).
const SERVER_VERSION: [u8; 4] = [0x10, 0x00, 0x03, 0xE8];
/// `CurCmd` of the DONE token of a SELECT.
const CUR_CMD_SELECT: u16 = 0xC1;
/// Packet size in force before the LOGIN7 ([MS-TDS] 2.2.6.4 PacketSize).
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// Number of rows answered to `SELECT MANY`.
const MANY_ROWS: i32 = 2000;
/// Upper bound of any single test, so that the whole file stays under 30 s.
const TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Username and password decoded from the LOGIN7, as the fake server saw them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Credentials {
    username: String,
    password: String,
}

/// Server TLS configuration built from the fixtures, TLS 1.2 only: with TLS 1.3 the
/// server sends post-handshake messages that a TDS 7.4 client cannot receive inside a
/// PRELOGIN packet. Same lines as `tls::test_tls_configs`, which `tests/` cannot see.
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

/// The login response, in the order a client expects it: ENVCHANGE database, INFO
/// 5701, ENVCHANGE collation, ENVCHANGE language, INFO 5703, ENVCHANGE packet size,
/// LOGINACK, DONE. `packet_size` is the one taken from the LOGIN7, already clamped and
/// applied to the stream: the ENVCHANGE confirms the very size the next packets use.
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

/// A column of the fake result sets. `updatable` is set for the columns that look like
/// table columns (`n`, `a`, `b`, `c`) and cleared for the anonymous expression of
/// `SELECT 1`, which exercises both values of `usUpdateable` against the client.
fn column(name: &str, ty: SqlType, nullable: bool) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        ty: TypeInfo::new(ty, nullable),
        flags: ColumnFlags {
            nullable,
            updatable: !name.is_empty(),
            ..ColumnFlags::default()
        },
    }
}

/// DONE closing a result set of `rows` rows.
fn done_count(rows: u64) -> Token {
    Token::Done {
        status: DoneStatus::COUNT,
        cur_cmd: CUR_CMD_SELECT,
        row_count: Some(rows),
    }
}

/// The response to a SQL batch, by its trimmed text.
fn answer(text: &str) -> Vec<Token> {
    match text {
        "SELECT 1" => vec![
            Token::ColMetaData(vec![column("", SqlType::Int, false)]),
            Token::Row(vec![Value::I32(1)]),
            done_count(1),
        ],
        "SELECT MANY" => {
            let mut tokens = Vec::with_capacity(MANY_ROWS as usize + 2);
            tokens.push(Token::ColMetaData(vec![column("n", SqlType::Int, false)]));
            tokens.extend((0..MANY_ROWS).map(|n| Token::Row(vec![Value::I32(n)])));
            tokens.push(done_count(MANY_ROWS as u64));
            tokens
        }
        "SELECT MIXED" => vec![
            Token::ColMetaData(vec![
                column("a", SqlType::Int, true),
                column("b", SqlType::NVarChar(Len::Max), true),
                column("c", SqlType::Bit, false),
            ]),
            Token::Row(vec![
                Value::Null,
                Value::String(SqlString {
                    text: "héllo wörld".into(),
                }),
                Value::Bit(true),
            ]),
            done_count(1),
        ],
        "SELECT ERROR" => vec![
            Token::Error(SqlError::new(208, 16, 1, "Invalid object name 'dbo.t'.")),
            Token::Done {
                status: DoneStatus::ERROR,
                cur_cmd: CUR_CMD_SELECT,
                row_count: None,
            },
        ],
        _ => vec![Token::Done {
            status: DoneStatus::FINAL,
            cur_cmd: 0,
            row_count: None,
        }],
    }
}

/// Serves one client connection until it closes.
async fn serve_connection(
    tcp: TcpStream,
    policy: EncryptPolicy,
    credentials: mpsc::UnboundedSender<Credentials>,
) -> Result<(), TdsError> {
    let mut stream = TdsStream::accept(tcp, Some(tls_config()), policy).await?;

    let login = match stream.read_message().await? {
        ClientMessage::Login7(login) => login,
        _ => return Err(TdsError::Unsupported("expected a LOGIN7 after PRELOGIN")),
    };
    // The test inspects the credentials; the receiver may already be gone, which is fine.
    let _ = credentials.send(Credentials {
        username: login.username.clone(),
        password: login.password.clone(),
    });

    // [MS-TDS] 2.2.6.4 PacketSize: the client's request, bounded to what the server allows.
    let packet_size = login.packet_size.clamp(512, 32767) as u16;

    // No-op unless ENCRYPT_OFF was negotiated: then the login response
    // already goes out in clear text.
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
        match message {
            ClientMessage::SqlBatch(batch) => {
                stream.write_tokens(&answer(batch.text.trim())).await?;
                stream.flush().await?;
            }
            ClientMessage::Attention => {
                stream
                    .write_tokens(&[Token::Done {
                        status: DoneStatus::ATTN,
                        cur_cmd: 0,
                        row_count: None,
                    }])
                    .await?;
                stream.flush().await?;
            }
            // Anything else (RPC, TRANSACTION_MANAGER, unsupported packets) is ignored.
            _ => {}
        }
    }
}

/// Starts a fake server with `policy` on an ephemeral port. Returns its address and the
/// channel on which every decoded LOGIN7 credential pair is delivered.
async fn spawn_server(policy: EncryptPolicy) -> (SocketAddr, mpsc::UnboundedReceiver<Credentials>) {
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
                if let Err(err) = serve_connection(tcp, policy, tx).await {
                    eprintln!("fake server: connection ended with {err}");
                }
            });
        }
    });
    (addr, rx)
}

/// Every byte exchanged between `tiberius` and the fake server, in both directions, copied
/// by a TCP relay. This is how the tests check what travels in clear text.
#[derive(Clone, Default)]
struct WireTap {
    client_to_server: Arc<Mutex<Vec<u8>>>,
    server_to_client: Arc<Mutex<Vec<u8>>>,
}

impl WireTap {
    fn client_to_server(&self) -> Vec<u8> {
        self.client_to_server.lock().unwrap().clone()
    }

    fn server_to_client(&self) -> Vec<u8> {
        self.server_to_client.lock().unwrap().clone()
    }
}

/// Copies `reader` into `writer` until EOF, appending everything to `log`.
async fn relay<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    log: Arc<Mutex<Vec<u8>>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        log.lock().unwrap().extend_from_slice(&buf[..n]);
        if writer.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// Starts a relay on an ephemeral port that forwards its first connection to `upstream`
/// and copies both directions into the tap.
async fn spawn_tap(upstream: SocketAddr) -> (SocketAddr, WireTap) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tap = WireTap::default();
    let (up_log, down_log) = (tap.client_to_server.clone(), tap.server_to_client.clone());
    tokio::spawn(async move {
        let Ok((client, _)) = listener.accept().await else {
            return;
        };
        let Ok(server) = TcpStream::connect(upstream).await else {
            return;
        };
        let (client_read, client_write) = client.into_split();
        let (server_read, server_write) = server.into_split();
        let up = tokio::spawn(relay(client_read, server_write, up_log));
        let down = tokio::spawn(relay(server_read, client_write, down_log));
        let _ = tokio::join!(up, down);
    });
    (addr, tap)
}

/// `true` when `needle` occurs anywhere in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// UTF-16LE bytes of `s`, the form every string takes on the TDS wire.
fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// The password as LOGIN7 carries it ([MS-TDS] 2.2.6.4 Password: every byte of the
/// UTF-16LE text has its nibbles swapped, then is XORed with 0xA5). Its presence in the
/// tap proves the LOGIN7 went in clear text; its absence, that it was encrypted.
fn obfuscated_password() -> Vec<u8> {
    utf16le(PASSWORD)
        .into_iter()
        .map(|b| b.rotate_left(4) ^ 0xA5)
        .collect()
}

/// `ENCRYPTION` byte of the client PRELOGIN that opens `client_to_server`
/// ([MS-TDS] 2.2.6.5: packet type 0x12, then an option table of `(token, offset, length)`
/// entries, offsets relative to the payload, terminated by 0xFF).
fn client_prelogin_encryption(client_to_server: &[u8]) -> u8 {
    const PRELOGIN: u8 = 0x12;
    const ENCRYPTION: u8 = 0x01;
    const TERMINATOR: u8 = 0xFF;
    assert_eq!(
        client_to_server[0], PRELOGIN,
        "the first packet is a PRELOGIN"
    );
    let payload = &client_to_server[8..];
    let mut at = 0;
    while payload[at] != TERMINATOR {
        let token = payload[at];
        let offset = u16::from_be_bytes([payload[at + 1], payload[at + 2]]) as usize;
        let length = u16::from_be_bytes([payload[at + 3], payload[at + 4]]) as usize;
        if token == ENCRYPTION {
            assert_eq!(length, 1);
            return payload[offset];
        }
        at += 5;
    }
    panic!("client PRELOGIN without ENCRYPTION option");
}

/// Connects a `tiberius` client with SQL authentication, the given encryption level and
/// `TrustServerCertificate` (the fixture certificate is self-signed).
async fn connect(addr: SocketAddr, encryption: EncryptionLevel) -> Client<Compat<TcpStream>> {
    let mut config = Config::new();
    config.host(addr.ip().to_string());
    config.port(addr.port());
    config.authentication(AuthMethod::sql_server(USERNAME, PASSWORD));
    config.encryption(encryption);
    config.trust_cert();

    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    Client::connect(config, tcp.compat_write())
        .await
        .expect("tiberius connects to the fake server")
}

/// `SELECT 1` through `client`: exactly one row whose column 0 reads as `i32 == 1`.
async fn assert_select_1(client: &mut Client<Compat<TcpStream>>) {
    let rows = client
        .simple_query("SELECT 1")
        .await
        .expect("SELECT 1 is accepted")
        .into_first_result()
        .await
        .expect("SELECT 1 yields a result set");
    assert_eq!(rows.len(), 1, "exactly one row");
    assert_eq!(rows[0].get::<i32, _>(0), Some(1));
}

/// Runs `test` under the per-test timeout so that a protocol misunderstanding shows up as
/// a failure instead of a hang.
async fn with_timeout<F: std::future::Future<Output = ()>>(test: F) {
    tokio::time::timeout(TEST_TIMEOUT, test)
        .await
        .expect("test finished before the timeout");
}

#[tokio::test]
async fn select_1_without_encryption() {
    with_timeout(async {
        let (server, _credentials) = spawn_server(EncryptPolicy::Off).await;
        let (addr, tap) = spawn_tap(server).await;
        let mut client = connect(addr, EncryptionLevel::NotSupported).await;
        assert_select_1(&mut client).await;

        // On the wire: ENCRYPT_NOT_SUP requested, and everything in clear text.
        let up = tap.client_to_server();
        let down = tap.server_to_client();
        assert_eq!(client_prelogin_encryption(&up), 0x02, "ENCRYPT_NOT_SUP");
        assert!(
            contains(&up, &obfuscated_password()),
            "LOGIN7 in clear text"
        );
        assert!(
            contains(&down, &utf16le("VaubanDB")),
            "LOGINACK in clear text"
        );
        assert!(
            contains(&up, &utf16le("SELECT 1")),
            "SQL_BATCH in clear text"
        );
    })
    .await;
}

#[tokio::test]
async fn select_1_login_only_encryption() {
    with_timeout(async {
        let (server, _credentials) = spawn_server(EncryptPolicy::Optional).await;
        let (addr, tap) = spawn_tap(server).await;
        // `Off`: only the login is encrypted (ENCRYPT_OFF, [MS-TDS] 2.2.6.5).
        let mut client = connect(addr, EncryptionLevel::Off).await;
        assert_select_1(&mut client).await;

        // On the wire: ENCRYPT_OFF requested, LOGIN7 encrypted, and both the login
        // response and the batch in clear text. This is what `downgrade_encryption`
        // assumes; the client accepts it.
        let up = tap.client_to_server();
        let down = tap.server_to_client();
        assert_eq!(client_prelogin_encryption(&up), 0x00, "ENCRYPT_OFF");
        assert!(!contains(&up, &obfuscated_password()), "LOGIN7 encrypted");
        assert!(
            contains(&down, &utf16le("VaubanDB")),
            "login response in clear text"
        );
        assert!(
            contains(&up, &utf16le("SELECT 1")),
            "SQL_BATCH in clear text"
        );
    })
    .await;
}

#[tokio::test]
async fn select_1_full_encryption() {
    with_timeout(async {
        let (server, _credentials) = spawn_server(EncryptPolicy::Required).await;
        let (addr, tap) = spawn_tap(server).await;
        let mut client = connect(addr, EncryptionLevel::Required).await;
        assert_select_1(&mut client).await;

        // On the wire: `tiberius` asks for ENCRYPT_REQ (0x03) rather than
        // ENCRYPT_ON (0x01); [MS-TDS] 2.2.6.5 allows both and `accept` answers ENCRYPT_ON to
        // either. Nothing after PRELOGIN travels in clear text.
        let up = tap.client_to_server();
        let down = tap.server_to_client();
        let encryption = client_prelogin_encryption(&up);
        assert!(
            encryption == 0x01 || encryption == 0x03,
            "ENCRYPT_ON or ENCRYPT_REQ, got 0x{encryption:02X}"
        );
        assert!(!contains(&up, &obfuscated_password()), "LOGIN7 encrypted");
        assert!(
            !contains(&down, &utf16le("VaubanDB")),
            "login response encrypted"
        );
        assert!(!contains(&up, &utf16le("SELECT 1")), "SQL_BATCH encrypted");
    })
    .await;
}

#[tokio::test]
async fn login7_credentials_are_decoded() {
    with_timeout(async {
        let (addr, mut credentials) = spawn_server(EncryptPolicy::Off).await;
        let _client = connect(addr, EncryptionLevel::NotSupported).await;
        let received = credentials
            .recv()
            .await
            .expect("the fake server decoded a LOGIN7");
        assert_eq!(
            received,
            Credentials {
                username: USERNAME.into(),
                password: PASSWORD.into(),
            }
        );
    })
    .await;
}

#[tokio::test]
async fn many_rows_span_packets() {
    with_timeout(async {
        let (addr, _credentials) = spawn_server(EncryptPolicy::Off).await;
        let mut client = connect(addr, EncryptionLevel::NotSupported).await;
        let rows = client
            .simple_query("SELECT MANY")
            .await
            .unwrap()
            .into_first_result()
            .await
            .unwrap();
        assert_eq!(rows.len(), MANY_ROWS as usize);
        let values: Vec<i32> = rows
            .iter()
            .map(|row| row.get::<i32, _>("n").unwrap())
            .collect();
        let expected: Vec<i32> = (0..MANY_ROWS).collect();
        assert_eq!(values, expected);
    })
    .await;
}

#[tokio::test]
async fn nvarchar_max_and_null() {
    with_timeout(async {
        let (addr, _credentials) = spawn_server(EncryptPolicy::Off).await;
        let mut client = connect(addr, EncryptionLevel::NotSupported).await;
        let rows = client
            .simple_query("SELECT MIXED")
            .await
            .unwrap()
            .into_first_result()
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.get::<i32, _>("a"), None);
        assert_eq!(row.get::<&str, _>("b"), Some("héllo wörld"));
        assert_eq!(row.get::<bool, _>("c"), Some(true));
    })
    .await;
}

#[tokio::test]
async fn error_token_is_surfaced() {
    with_timeout(async {
        let (addr, _credentials) = spawn_server(EncryptPolicy::Off).await;
        let mut client = connect(addr, EncryptionLevel::NotSupported).await;
        // The client may report the error when the batch is sent or when the result is
        // collected: both are legitimate places, only the content matters.
        let err = match client.simple_query("SELECT ERROR").await {
            Err(err) => err,
            Ok(stream) => stream
                .into_first_result()
                .await
                .expect_err("SELECT ERROR yields a server error"),
        };
        match err {
            tiberius::error::Error::Server(server) => {
                assert_eq!(server.code(), 208);
                assert_eq!(server.message(), "Invalid object name 'dbo.t'.");
            }
            other => panic!("expected a server error, got {other:?}"),
        }
    })
    .await;
}
