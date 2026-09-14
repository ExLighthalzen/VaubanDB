//! The binary fills the `sysfn` registry before it listens, and refuses a `program_name`
//! LOGINACK cannot carry instead of listening with it.
//!
//! Two calls at the top of `run_server`, before the `TcpListener` is bound:
//! `vauban_sysfn::register_builtins()` then `vauban_compat::register_functions()`.
//! `compat` depends on `session`, never the other way round, so `session` cannot make them
//! itself; the binary is the only place that can. This file checks the two properties that
//! matter: a client that connects to a freshly started server already talks to a filled
//! registry, and calling the two registrations twice in one process changes nothing.
//!
//! # Ports
//!
//! Like the other integration tests of this crate, every server listens on
//! `VAUBAN_TEST_PORT` (`1433` when unset) plus a fixed offset, one per test, so that
//! `cargo test` may run them in parallel threads. The offsets are disjoint across the
//! test files: `tls.rs` uses `10..=19`, `cli.rs` `20..=24`, this file `30..=39`; `0..=4`
//! is left to other end-to-end tests. The two tests of a refused startup keep an offset of their own even
//! though their server gives up before it binds anything.

use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio_util::compat::TokioAsyncWriteCompatExt;
use vauban_session::VERSION_BANNER;

/// How long the port has to accept a connection after the start.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one client exchange may take.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// The port of the server of one test: `VAUBAN_TEST_PORT` when set, else `1433`, plus
/// `offset`. Offsets `30..=39` belong to this file (see the module documentation).
fn port(offset: u16) -> u16 {
    let base = std::env::var("VAUBAN_TEST_PORT")
        .ok()
        .and_then(|text| text.trim().parse::<u16>().ok())
        .unwrap_or(1433);
    base + offset
}

/// A running `vauban serve`, killed on drop whatever the test does.
struct Running {
    child: Option<Child>,
    port: u16,
}

impl Running {
    /// Starts `serve --in-memory --no-auth --encrypt optional --bind 127.0.0.1 --port
    /// <port(offset)>` and waits for the port to accept a connection.
    fn start(offset: u16) -> Self {
        Self::start_with_env(offset, &[])
    }

    /// Like [`Running::start`], with extra environment variables applied after the
    /// identity variables of the surrounding shell have been cleared.
    fn start_with_env(offset: u16, extra_env: &[(&str, &str)]) -> Self {
        let port = port(offset);
        let mut child = spawn_serve(port, extra_env);

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
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawns `serve --in-memory --no-auth --encrypt optional --bind 127.0.0.1 --port <port>`
/// with `extra_env` applied after the identity variables of the surrounding shell have
/// been cleared, stdout and stderr piped.
fn spawn_serve(port: u16, extra_env: &[(&str, &str)]) -> Child {
    let port_text = port.to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_vauban"));
    command
        .env_remove("VAUBAN_SA_PASSWORD")
        .env_remove("VAUBAN_PROGRAM_NAME")
        .env_remove("VAUBAN_VERSION_BANNER")
        .env_remove("VAUBAN_EDITION")
        .env_remove("RUST_LOG");
    for (name, value) in extra_env {
        command.env(name, value);
    }
    command
        // No `vauban.toml` of the working directory may reach the server.
        .current_dir(env!("CARGO_TARGET_TMPDIR"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args([
            "serve",
            "--in-memory",
            "--no-auth",
            "--encrypt",
            "optional",
            "--bind",
            "127.0.0.1",
            "--port",
            &port_text,
        ])
        .spawn()
        .expect("spawn the server")
}

/// The output of a `serve` that was expected to give up: exit code, stdout and stderr.
struct Refused {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Spawns `serve` with `extra_env` and waits for it to exit, at most [`START_TIMEOUT`].
///
/// The port is passed but not bound when the configuration is refused: the check runs before
/// the listener. A server still alive at the deadline is killed and the test fails — that is
/// exactly the bug this covers, a faulty identity accepted at startup.
fn refused_startup(offset: u16, extra_env: &[(&str, &str)]) -> Refused {
    let mut child = spawn_serve(port(offset), extra_env);
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let output = child.wait_with_output().expect("output");
                return Refused {
                    code: status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                };
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("the server was still running {START_TIMEOUT:?} after the start");
                }
                sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Runs `query` through `tiberius`, trusting the self-signed certificate, and returns the
/// first column of the first row as text.
async fn query_first_string(port: u16, query: &str) -> Result<String, tiberius::error::Error> {
    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(port);
    // `--no-auth` accepts any login and password.
    config.authentication(AuthMethod::sql_server("sa", "x"));
    config.encryption(EncryptionLevel::Off);
    config.trust_cert();

    let tcp = tokio::net::TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    let mut client = tokio::time::timeout(IO_TIMEOUT, Client::connect(config, tcp.compat_write()))
        .await
        .expect("login within the timeout")?;
    let row = client
        .simple_query(query)
        .await?
        .into_row()
        .await?
        .expect("one row");
    let value: &str = row.get(0).expect("a non-null string in column 0");
    let value = value.to_owned();
    client.close().await?;
    Ok(value)
}

/// A client that connects to a freshly started server already talks to a filled registry:
/// `@@VERSION` comes from `compat::register_functions`. What guarantees the order is
/// structural: the two calls sit at the top of `run_server`, before `TcpListener::bind`,
/// so no client can arrive before them.
#[test]
#[ignore = "needs a TDS client and a free port; network test, like the rest of the module"]
fn registry_is_filled_before_listening() {
    let server = Running::start(30);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let banner = runtime
        .block_on(query_first_string(server.port, "SELECT @@VERSION"))
        .expect("SELECT @@VERSION");
    assert_eq!(banner, VERSION_BANNER);
}

/// `VAUBAN_VERSION_BANNER` replaces the default `@@VERSION` text for connected clients.
#[test]
#[ignore = "needs a TDS client and a free port; network test, like the rest of the module"]
fn version_banner_override_is_visible_to_clients() {
    let server = Running::start_with_env(31, &[("VAUBAN_VERSION_BANNER", "CustomBanner")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let banner = runtime
        .block_on(query_first_string(server.port, "SELECT @@VERSION"))
        .expect("SELECT @@VERSION");
    assert_eq!(banner, "CustomBanner");
}

/// `VAUBAN_EDITION` without a banner rewrites the last line of `@@VERSION`.
#[test]
#[ignore = "needs a TDS client and a free port; network test, like the rest of the module"]
fn edition_override_rewrites_the_banner_last_line() {
    let server = Running::start_with_env(32, &[("VAUBAN_EDITION", "Custom Edition")]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let banner = runtime
        .block_on(query_first_string(server.port, "SELECT @@VERSION"))
        .expect("SELECT @@VERSION");
    assert!(
        banner.ends_with("\tCustom Edition"),
        "last line of {banner:?}"
    );
    let edition = runtime
        .block_on(query_first_string(
            server.port,
            "SELECT CONVERT(varchar(128), SERVERPROPERTY('Edition'))",
        ))
        .expect("SERVERPROPERTY Edition");
    assert_eq!(edition, "Custom Edition");
}

/// A `program_name` of 255 UTF-16 code units starts a server that logs a client
/// in. The name itself is checked on the wire by `vauban-session`'s `tests/login.rs`
/// (`a_program_name_of_255_units_arrives_entire`), which decodes `ProgName`; what this test
/// adds is the path from the variable to a working login, since a LOGINACK that cannot be
/// encoded leaves the client with a closed connection and nothing to read.
#[test]
#[ignore = "needs a TDS client and a free port; network test, like the rest of the module"]
fn program_name_of_255_units_starts_and_logs_a_client_in() {
    let name = "x".repeat(255);
    let server = Running::start_with_env(33, &[("VAUBAN_PROGRAM_NAME", name.as_str())]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let banner = runtime
        .block_on(query_first_string(server.port, "SELECT @@VERSION"))
        .expect("SELECT @@VERSION");
    assert_eq!(banner, VERSION_BANNER);
}

/// 256 UTF-16 code units stop the startup with exit code 2 and a message that names both
/// lengths.
///
/// The operator must learn it here and not at the first client: without this check the
/// server would start, log `product identity overridden by the operator`, and cut the
/// login without an error token. The absence of that warning from stdout is what tells
/// the refusal came first: the warning is written after the configuration is validated.
#[test]
fn program_name_of_256_units_stops_the_startup() {
    let refused = refused_startup(34, &[("VAUBAN_PROGRAM_NAME", "x".repeat(256).as_str())]);
    assert_eq!(refused.code, Some(2), "stderr:\n{}", refused.stderr);
    assert_eq!(
        refused.stderr.trim_end(),
        "error: program_name is 256 UTF-16 code units long; LOGINACK carries at most 255"
    );
    assert!(
        !refused.stdout.contains("product identity overridden"),
        "the server logged its startup warning before giving up:\n{}",
        refused.stdout
    );
}

/// The length the binary counts is in UTF-16 code units, through the environment
/// variable as well. `x` 254 times followed by U+1F3F0 CASTLE is 255 characters and 258
/// UTF-8 bytes, but 256 UTF-16 code units, and the message names 256.
#[test]
fn program_name_with_an_astral_character_is_counted_in_utf16_units() {
    let name = format!("{}{}", "x".repeat(254), '\u{1F3F0}');
    assert_eq!(name.chars().count(), 255);
    assert_eq!(name.len(), 258);
    let refused = refused_startup(35, &[("VAUBAN_PROGRAM_NAME", name.as_str())]);
    assert_eq!(refused.code, Some(2), "stderr:\n{}", refused.stderr);
    assert!(
        refused
            .stderr
            .contains("program_name is 256 UTF-16 code units long"),
        "stderr:\n{}",
        refused.stderr
    );
}

/// The two registrations are idempotent: the binary calls them once, but the integration
/// tests of the workspace call them too, and a second registration of the same name would
/// panic in `sysfn::register`. Both guard their body with a `std::sync::Once`, so the
/// second round is a no-op and the registry keeps exactly the same entries.
///
/// The count is compared, not asserted against a fixed number: built-ins keep being added,
/// and this test must not have to be edited each time.
#[test]
fn both_registrations_are_idempotent() {
    vauban_sysfn::register_builtins();
    vauban_compat::register_functions();
    let after_first = vauban_sysfn::all();

    vauban_sysfn::register_builtins();
    vauban_compat::register_functions();
    for definition in after_first {
        assert!(std::ptr::eq(
            vauban_sysfn::lookup(definition.name).expect("registered"),
            definition
        ));
    }

    // `compat` alone registers `@@VERSION` and `SERVERPROPERTY`: an empty registry here
    // would mean the calls above did nothing at all.
    let names: Vec<&str> = vauban_sysfn::all()
        .into_iter()
        .map(|def| def.name)
        .collect();
    assert!(names.contains(&"@@VERSION"), "registry holds {names:?}");
    assert!(
        names.contains(&"SERVERPROPERTY"),
        "registry holds {names:?}"
    );
}
