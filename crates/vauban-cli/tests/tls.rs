//! `vauban serve` with the default encryption policy starts without any TLS
//! option, answers PRELOGIN with the negotiated ENCRYPTION byte, writes its generated
//! certificate under `--data`, and completes the TLS handshake with a real TDS client
//! (`tiberius`, trusting any certificate) in the two modes that encrypt the login.
//!
//! Everything runs on `127.0.0.1`; the server is a child process killed at the end of
//! each test.
//!
//! # Ports
//!
//! Every server listens on `VAUBAN_TEST_PORT` (`1433` when unset) plus a fixed offset,
//! one per test, so that `cargo test` may run them in parallel threads. An ephemeral port
//! obtained by binding and closing a listener would not do: another test could take it
//! between the close and the start of the server. The offsets are disjoint across the
//! test files of this crate: this file uses `10..=19`, `cli.rs` `20..=24`, `registry.rs`
//! `30..=39`; `0..=4` is left to other end-to-end tests.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio_util::compat::TokioAsyncWriteCompatExt;

/// How long the port has to accept a connection after the start.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one client exchange may take.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// PRELOGIN packet type ([MS-TDS] 2.2.3.1.1).
const PACKET_PRELOGIN: u8 = 0x12;
/// Tabular result packet type ([MS-TDS] 2.2.3.1.1): the type of every server response,
/// PRELOGIN included.
const PACKET_TABULAR_RESULT: u8 = 0x04;
/// `EOM` status bit ([MS-TDS] 2.2.3.1.2).
const STATUS_EOM: u8 = 0x01;
/// Packet header length ([MS-TDS] 2.2.3.1).
const HEADER_LEN: usize = 8;
/// PRELOGIN option tokens ([MS-TDS] 2.2.6.5).
const OPT_VERSION: u8 = 0x00;
const OPT_ENCRYPTION: u8 = 0x01;
const OPT_INSTOPT: u8 = 0x02;
const OPT_THREADID: u8 = 0x03;
const OPT_MARS: u8 = 0x04;
const OPT_TERMINATOR: u8 = 0xFF;
/// `B_FENCRYPTION` values ([MS-TDS] 2.2.6.5).
const ENCRYPT_OFF: u8 = 0x00;
const ENCRYPT_REQ: u8 = 0x03;
const ENCRYPT_NOT_SUP: u8 = 0x02;

/// The port of the server of one test: `VAUBAN_TEST_PORT` when set, else `1433`, plus
/// `offset`. Offsets `10..=19` belong to this file (see the module documentation).
fn port(offset: u16) -> u16 {
    let base = std::env::var("VAUBAN_TEST_PORT")
        .ok()
        .and_then(|text| text.trim().parse::<u16>().ok())
        .unwrap_or(1433);
    base + offset
}

/// A directory under the system temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vauban-cli-tls-it-{}-{n}-{label}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn arg(&self) -> &str {
        self.0.to_str().expect("utf-8 temp path")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A running server, killed on drop. `stop` returns its output.
struct Running {
    child: Option<Child>,
    port: u16,
}

impl Running {
    /// Starts `serve --in-memory --no-auth --bind 127.0.0.1 --port <port(offset)>
    /// --log-format json` plus `extra`, and waits for the port to accept a connection.
    fn start(offset: u16, extra: &[&str]) -> Self {
        let port = port(offset);
        let port_text = port.to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_vauban"))
            .env_remove("VAUBAN_SA_PASSWORD")
            .env_remove("VAUBAN_PROGRAM_NAME")
            .env_remove("VAUBAN_VERSION_BANNER")
            .env_remove("VAUBAN_EDITION")
            .env_remove("RUST_LOG")
            .current_dir(env!("CARGO_TARGET_TMPDIR"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args([
                "serve",
                "--in-memory",
                "--no-auth",
                "--bind",
                "127.0.0.1",
                "--port",
                &port_text,
                "--log-format",
                "json",
            ])
            .args(extra)
            .spawn()
            .expect("spawn the server");

        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                drop(stream);
                break;
            }
            if let Some(status) = child.try_wait().expect("try_wait") {
                let output = child.wait_with_output().expect("output");
                panic!(
                    "server exited early with {status}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                Instant::now() < deadline,
                "port {port} did not accept a connection within {START_TIMEOUT:?}"
            );
            sleep(Duration::from_millis(50));
        }
        Self {
            child: Some(child),
            port,
        }
    }

    /// Kills the server and returns what it wrote.
    fn stop(mut self) -> Output {
        let mut child = self.child.take().expect("still running");
        let _ = child.kill();
        child.wait_with_output().expect("output")
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A client PRELOGIN message ([MS-TDS] 2.2.6.5): VERSION, ENCRYPTION = `encryption`,
/// INSTOPT (empty name), THREADID, MARS off, TERMINATOR; then the option data.
fn prelogin_packet(encryption: u8) -> Vec<u8> {
    let version: [u8; 6] = [0x10, 0x00, 0x03, 0xE8, 0x00, 0x00];
    let encryption = [encryption];
    let instance = [0x00];
    let thread_id = [0x00, 0x00, 0x00, 0x00];
    let mars = [0x00];
    let options: [(u8, &[u8]); 5] = [
        (OPT_VERSION, &version),
        (OPT_ENCRYPTION, &encryption),
        (OPT_INSTOPT, &instance),
        (OPT_THREADID, &thread_id),
        (OPT_MARS, &mars),
    ];

    let mut payload = Vec::new();
    let mut offset = u16::try_from(options.len() * 5 + 1).expect("small");
    for (token, data) in &options {
        let len = u16::try_from(data.len()).expect("small");
        payload.push(*token);
        payload.extend_from_slice(&offset.to_be_bytes());
        payload.extend_from_slice(&len.to_be_bytes());
        offset += len;
    }
    payload.push(OPT_TERMINATOR);
    for (_, data) in &options {
        payload.extend_from_slice(data);
    }

    let total = u16::try_from(HEADER_LEN + payload.len()).expect("small");
    let mut packet = vec![PACKET_PRELOGIN, STATUS_EOM];
    packet.extend_from_slice(&total.to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x00, 0x01, 0x00]);
    packet.extend_from_slice(&payload);
    packet
}

/// Sends a PRELOGIN announcing `client_encryption` and returns the ENCRYPTION byte of the
/// server's response.
fn negotiate_encryption(port: u16, client_encryption: u8) -> u8 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(IO_TIMEOUT)).expect("timeout");
    stream
        .write_all(&prelogin_packet(client_encryption))
        .expect("send PRELOGIN");

    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).expect("response header");
    assert_eq!(
        header[0], PACKET_TABULAR_RESULT,
        "response type {:#04x}",
        header[0]
    );
    let total = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let mut payload = vec![0u8; total - HEADER_LEN];
    stream.read_exact(&mut payload).expect("response payload");

    let mut pos = 0;
    loop {
        let token = payload[pos];
        if token == OPT_TERMINATOR {
            panic!("no ENCRYPTION option in the PRELOGIN response: {payload:02x?}");
        }
        let offset = usize::from(u16::from_be_bytes([payload[pos + 1], payload[pos + 2]]));
        let len = usize::from(u16::from_be_bytes([payload[pos + 3], payload[pos + 4]]));
        if token == OPT_ENCRYPTION {
            assert_eq!(len, 1, "ENCRYPTION length");
            return payload[offset];
        }
        pos += 5;
    }
}

#[test]
fn default_policy_starts_without_tls_options_and_answers_encrypt_off() {
    let server = Running::start(10, &[]);
    assert_eq!(negotiate_encryption(server.port, ENCRYPT_OFF), ENCRYPT_OFF);
    let output = server.stop();
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(
        log.contains("ephemeral self-signed TLS certificate generated"),
        "{log}"
    );
    assert!(log.contains("changes at every start"), "{log}");
    assert!(log.contains("\"fingerprint\":\""), "{log}");
}

#[test]
fn encrypt_optional_answers_encrypt_off() {
    let server = Running::start(11, &["--encrypt", "optional"]);
    assert_eq!(negotiate_encryption(server.port, ENCRYPT_OFF), ENCRYPT_OFF);
}

#[test]
fn encrypt_required_answers_encrypt_req() {
    let server = Running::start(12, &["--encrypt", "required"]);
    assert_eq!(negotiate_encryption(server.port, ENCRYPT_OFF), ENCRYPT_REQ);
}

#[test]
fn encrypt_off_answers_encrypt_not_sup() {
    let server = Running::start(13, &["--encrypt", "off"]);
    assert_eq!(
        negotiate_encryption(server.port, ENCRYPT_OFF),
        ENCRYPT_NOT_SUP
    );
}

#[test]
fn data_dir_gets_the_generated_certificate_and_key() {
    let data = TempDir::new("data");
    let cert_path = data.path().join("tls").join("server.crt");
    let key_path = data.path().join("tls").join("server.key");

    let server = Running::start(14, &["--data", data.arg()]);
    assert_eq!(negotiate_encryption(server.port, ENCRYPT_OFF), ENCRYPT_OFF);
    let output = server.stop();
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(
        log.contains("self-signed TLS certificate generated"),
        "{log}"
    );

    let cert = fs::read_to_string(&cert_path).expect("server.crt written");
    assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"), "{cert}");
    let key = fs::read_to_string(&key_path).expect("server.key written");
    assert!(key.starts_with("-----BEGIN PRIVATE KEY-----"), "{key}");
    assert!(
        !log.contains(key.trim()),
        "the private key must not be logged"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&key_path).expect("key").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key mode {mode:o}");
    }

    // A second start reloads the same files instead of generating new ones. The same
    // offset is safe: the first server is stopped and waited for before this line.
    let server = Running::start(14, &["--data", data.arg()]);
    assert_eq!(negotiate_encryption(server.port, ENCRYPT_OFF), ENCRYPT_OFF);
    let output = server.stop();
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(
        log.contains("TLS certificate loaded from the data directory"),
        "{log}"
    );
    assert_eq!(cert, fs::read_to_string(&cert_path).expect("server.crt"));
}

#[test]
fn given_cert_and_key_are_loaded() {
    // Generate the material through a first server, then hand it to a second one.
    let data = TempDir::new("given");
    let server = Running::start(15, &["--data", data.arg()]);
    server.stop();
    let cert = data.path().join("tls").join("server.crt");
    let key = data.path().join("tls").join("server.key");

    let server = Running::start(
        15,
        &[
            "--cert",
            cert.to_str().expect("utf-8"),
            "--key",
            key.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(negotiate_encryption(server.port, ENCRYPT_OFF), ENCRYPT_OFF);
    let output = server.stop();
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(
        log.contains("\"message\":\"TLS certificate loaded\""),
        "{log}"
    );
}

#[test]
fn missing_cert_file_exits_2_naming_the_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_vauban"))
        .env_remove("VAUBAN_SA_PASSWORD")
        .env_remove("VAUBAN_PROGRAM_NAME")
        .env_remove("VAUBAN_VERSION_BANNER")
        .env_remove("VAUBAN_EDITION")
        .current_dir(env!("CARGO_TARGET_TMPDIR"))
        .args([
            "serve",
            "--in-memory",
            "--no-auth",
            "--cert",
            "/nonexistent/server.crt",
            "--key",
            "/nonexistent/server.key",
        ])
        .output()
        .expect("binary runs");
    assert_eq!(output.status.code(), Some(2));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.starts_with("error: "), "{err}");
    assert!(err.contains("/nonexistent/server.crt"), "{err}");
}

/// Connects with `tiberius` at `level`, trusting any certificate, and returns the result
/// of the login.
async fn tiberius_login(port: u16, level: EncryptionLevel) -> Result<(), tiberius::error::Error> {
    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(port);
    config.authentication(AuthMethod::sql_server("sa", "any"));
    config.encryption(level);
    config.trust_cert();

    let tcp = tokio::net::TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    let client = tokio::time::timeout(IO_TIMEOUT, Client::connect(config, tcp.compat_write()))
        .await
        .expect("login within the timeout")?;
    client.close().await?;
    Ok(())
}

/// The TLS handshake with a real client, in the two modes that encrypt the login.
///
/// In both modes the LOGIN7 travels inside TLS: the server can only log it once the
/// handshake, encapsulated in PRELOGIN packets, has completed. A login the client reports
/// as failed is accepted here as long as the server logged the LOGIN7 it received over
/// TLS: the handshake is what this checks, not the login.
fn tls_handshake_with_tiberius(offset: u16, level: EncryptionLevel, extra: &[&str]) {
    let server = Running::start(offset, extra);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = runtime.block_on(tiberius_login(server.port, level));
    let output = server.stop();
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(log.contains("PRELOGIN negotiated"), "{log}");
    if let Err(err) = result {
        assert!(
            log.contains("\"message\":\"LOGIN7\""),
            "login failed ({err}) and the server never received the LOGIN7 over TLS:\n{log}"
        );
    }
    assert!(
        !log.contains("\"level\":\"WARN\"") || log.contains("changes at every start"),
        "unexpected warning:\n{log}"
    );
}

#[test]
fn tiberius_encrypts_the_login_with_the_ephemeral_certificate() {
    tls_handshake_with_tiberius(16, EncryptionLevel::Off, &["--encrypt", "optional"]);
}

#[test]
fn tiberius_encrypts_everything_with_the_ephemeral_certificate() {
    tls_handshake_with_tiberius(17, EncryptionLevel::Required, &["--encrypt", "optional"]);
}

#[test]
fn tiberius_encrypts_everything_against_encrypt_required() {
    tls_handshake_with_tiberius(18, EncryptionLevel::Required, &["--encrypt", "required"]);
}

#[test]
fn tiberius_encrypts_the_login_with_the_data_dir_certificate() {
    let data = TempDir::new("tiberius");
    tls_handshake_with_tiberius(19, EncryptionLevel::Off, &["--data", data.arg()]);
}
