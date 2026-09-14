//! Integration tests of the `vauban` binary: subcommands, configuration errors, and
//! (on Unix) a real start, a TCP connection and a SIGINT shutdown.
//!
//! # Ports
//!
//! With no override, each server binds port zero: the operating system allocates its
//! listening socket atomically. The test reads the assigned port from that child's
//! startup log. `VAUBAN_TEST_PORT` retains explicit base-plus-offset allocation (20..=24).

use std::net::TcpStream;
use std::process::{Command, Output, Stdio};

/// The binary with a clean environment and a working directory without `vauban.toml`.
fn vauban() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vauban"));
    cmd.env_remove("VAUBAN_SA_PASSWORD")
        .env_remove("VAUBAN_PROGRAM_NAME")
        .env_remove("VAUBAN_VERSION_BANNER")
        .env_remove("VAUBAN_EDITION")
        .env_remove("RUST_LOG")
        .current_dir(env!("CARGO_TARGET_TMPDIR"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn run(args: &[&str]) -> Output {
    vauban().args(args).output().expect("binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Explicit base-plus-offset allocation, or atomic OS allocation via port zero.
fn port(offset: u16) -> u16 {
    configured_port(std::env::var("VAUBAN_TEST_PORT").ok().as_deref(), offset)
}

fn configured_port(base: Option<&str>, offset: u16) -> u16 {
    base.map(|text| {
        text.trim()
            .parse::<u16>()
            .expect("VAUBAN_TEST_PORT must be a u16")
            .checked_add(offset)
            .expect("VAUBAN_TEST_PORT plus test offset exceeds 65535")
    })
    .unwrap_or(0)
}

#[test]
fn version_prints_the_crate_version() {
    let output = run(&["version"]);
    assert_eq!(output.status.code(), Some(0));
    let out = stdout(&output);
    assert!(out.starts_with("vauban "), "{out}");
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");
}

#[test]
fn serve_help_lists_every_option() {
    let output = run(&["serve", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let out = stdout(&output);
    for option in [
        "--bind",
        "--port",
        "--data",
        "--in-memory",
        "--sa-password",
        "--no-auth",
        "--encrypt",
        "--cert",
        "--key",
        "--log-level",
        "--log-format",
        "--config",
    ] {
        assert!(out.contains(option), "missing {option} in:\n{out}");
    }
}

#[test]
fn serve_without_password_or_no_auth_exits_2() {
    let output = run(&["serve", "--in-memory", "--encrypt", "off"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains(
            "error: a sa password is required (--sa-password or VAUBAN_SA_PASSWORD), or pass --no-auth"
        ),
        "{}",
        stderr(&output)
    );
}

#[test]
fn serve_without_in_memory_exits_2() {
    let output = run(&["serve", "--no-auth", "--encrypt", "off"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("error: disk storage is not implemented yet; pass --in-memory"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn serve_with_cert_without_key_exits_2() {
    let output = run(&[
        "serve",
        "--in-memory",
        "--no-auth",
        "--encrypt",
        "off",
        "--cert",
        "server.pem",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("error: --cert and --key must be given together"));
}

#[test]
fn serve_with_missing_config_file_exits_2() {
    let output = run(&[
        "serve",
        "--in-memory",
        "--no-auth",
        "--encrypt",
        "off",
        "--config",
        "/nonexistent/vauban.toml",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("error: invalid configuration file"));
}

#[test]
fn serve_with_unknown_key_in_config_file_exits_2() {
    let dir = std::env::temp_dir().join(format!("vauban-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("bad.toml");
    std::fs::write(&file, "prot = 1500\n").expect("write config");
    let output = run(&[
        "serve",
        "--in-memory",
        "--no-auth",
        "--encrypt",
        "off",
        "--config",
        file.to_str().expect("utf-8 path"),
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("error: invalid configuration file"));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::io::Read;
    use std::process::Child;
    use std::sync::{Arc, Mutex};
    use std::thread::{JoinHandle, sleep, spawn};
    use std::time::{Duration, Instant};

    /// The line the server logs once its listening socket is bound. Waiting for it is
    /// waiting for an event of *our* child, where an open port could belong to any other
    /// process that took the same number.
    const LISTENING: &str = "vauban listening";

    /// Cap on every wait below, in seconds, when [`TIMEOUT_ENV`] is unset. Deliberately
    /// generous: each wait ends on the event it observes, so a large cap costs nothing on
    /// an idle machine and only decides how long a genuinely stuck server is given.
    const DEFAULT_TIMEOUT_SECS: u64 = 30;

    /// Raises (or lowers) that cap without touching this file, for a loaded CI runner.
    const TIMEOUT_ENV: &str = "VAUBAN_TEST_TIMEOUT_SECS";

    /// Rest between two attempts of a polling loop. It is not a delay we wait *for*: every
    /// loop below exits on an observed event (the log line, the accepted connection, the
    /// exit of the process) and never after a number of rests. It only keeps a loop that
    /// would otherwise spin on `ECONNREFUSED` from eating a core.
    const POLL_REST: Duration = Duration::from_millis(20);

    /// The cap on the waits of one test.
    fn timeout() -> Duration {
        let secs = std::env::var(TIMEOUT_ENV)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        Duration::from_secs(secs)
    }

    /// The output of a child, drained by two threads from the moment it is spawned.
    /// Draining as it goes serves two ends: a test may read the log while the server still
    /// runs, and a chatty server (`--log-level trace`) never blocks on a full pipe, which
    /// would make its shutdown depend on when we read.
    struct Drained {
        stdout: Arc<Mutex<Vec<u8>>>,
        stderr: Arc<Mutex<Vec<u8>>>,
        readers: Vec<JoinHandle<()>>,
    }

    impl Drained {
        fn new(child: &mut Child) -> Self {
            let out = child.stdout.take().expect("piped stdout");
            let err = child.stderr.take().expect("piped stderr");
            let stdout = Arc::new(Mutex::new(Vec::new()));
            let stderr = Arc::new(Mutex::new(Vec::new()));
            let readers = vec![
                drain(out, Arc::clone(&stdout)),
                drain(err, Arc::clone(&stderr)),
            ];
            Self {
                stdout,
                stderr,
                readers,
            }
        }

        /// What the child has written on stdout so far.
        fn out(&self) -> String {
            text(&self.stdout)
        }

        /// What the child has written on stderr so far.
        fn err(&self) -> String {
            text(&self.stderr)
        }

        /// Waits for both readers to reach end of file (the pipes close when the child
        /// exits) and returns everything the child wrote.
        fn finish(self) -> (Vec<u8>, Vec<u8>) {
            for reader in self.readers {
                let _ = reader.join();
            }
            let out = self.stdout.lock().expect("stdout buffer").clone();
            let err = self.stderr.lock().expect("stderr buffer").clone();
            (out, err)
        }
    }

    /// Copies `source` into `sink` until end of file, in a thread.
    fn drain<R: Read + Send + 'static>(mut source: R, sink: Arc<Mutex<Vec<u8>>>) -> JoinHandle<()> {
        spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match source.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => sink
                        .lock()
                        .expect("output buffer")
                        .extend_from_slice(&buffer[..read]),
                }
            }
        })
    }

    fn text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&buffer.lock().expect("output buffer")).into_owned()
    }

    fn listening_port(output: &str) -> Option<u16> {
        output
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .find_map(|line| {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    let fields = value.get("fields")?;
                    if fields.get("message")?.as_str()? != LISTENING {
                        return None;
                    }
                    return fields.get("port")?.as_u64()?.try_into().ok();
                }
                if !line.contains(LISTENING) {
                    return None;
                }
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix("port=")?.parse().ok())
            })
    }

    /// A running server. Killed on drop if a test fails before stopping it.
    struct Running {
        child: Option<Child>,
        drained: Option<Drained>,
        port: u16,
    }

    impl Running {
        /// Starts `serve --in-memory --encrypt off --port <port(offset)>` plus `extra`,
        /// and waits for the server to listen.
        fn start(offset: u16, extra: &[&str]) -> Self {
            let port = port(offset);
            let mut command = vauban();
            command
                .args([
                    "serve",
                    "--in-memory",
                    "--encrypt",
                    "off",
                    "--port",
                    &port.to_string(),
                ])
                .args(extra);
            Self::spawn(command, port)
        }

        /// Spawns `command`, drains its output, and waits for two events, never for a
        /// delay: the [`LISTENING`] line on its stdout, then a connection accepted on
        /// `port`. The line proves the socket of *this* child is bound; the connection
        /// proves the tests may now use it.
        fn spawn(mut command: Command, port: u16) -> Self {
            let child = command.spawn().expect("spawn the server");
            Self::wait_for_start(child, port, timeout())
        }

        fn wait_for_start(mut child: Child, requested_port: u16, cap: Duration) -> Self {
            let drained = Drained::new(&mut child);
            // Install cleanup before any assertion or polling can fail.
            let mut running = Self {
                child: Some(child),
                drained: Some(drained),
                port: requested_port,
            };
            let started = Instant::now();
            loop {
                let drained = running.drained.as_ref().expect("draining");
                if let Some(bound_port) = listening_port(&drained.out()) {
                    assert!(bound_port != 0, "server reported port zero");
                    assert!(
                        requested_port == 0 || requested_port == bound_port,
                        "server bound {bound_port}, requested {requested_port}"
                    );
                    if TcpStream::connect(("127.0.0.1", bound_port)).is_ok() {
                        running.port = bound_port;
                        return running;
                    }
                }
                if let Some(status) = running
                    .child
                    .as_mut()
                    .expect("running")
                    .try_wait()
                    .expect("try_wait")
                {
                    let (out, err) = running.drained.take().expect("draining").finish();
                    panic!(
                        "the server exited with {status} after {:?}, before listening on port {requested_port}\nstdout:\n{}\nstderr:\n{}",
                        started.elapsed(),
                        String::from_utf8_lossy(&out),
                        String::from_utf8_lossy(&err)
                    );
                }
                assert!(
                    started.elapsed() < cap,
                    "the server never listened on port {requested_port} after {:?} (cap {cap:?}, raise it with {TIMEOUT_ENV}); startup requires an info-level listening log\nstdout:\n{}\nstderr:\n{}",
                    started.elapsed(),
                    drained.out(),
                    drained.err()
                );
                sleep(POLL_REST);
            }
        }

        /// Sends SIGINT through `kill` and waits for the exit of the process; returns the
        /// output and the time the shutdown took.
        fn interrupt(mut self) -> (Output, Duration) {
            let mut child = self.child.take().expect("still running");
            let drained = self.drained.take().expect("still draining");
            let port = self.port;
            let pid = child.id().to_string();
            let sent = Command::new("kill")
                .args(["-INT", &pid])
                .status()
                .expect("run kill");
            assert!(sent.success(), "kill -INT {pid} failed: {sent}");

            let cap = timeout();
            let started = Instant::now();
            let status = loop {
                if let Some(status) = child.try_wait().expect("try_wait") {
                    break status;
                }
                if started.elapsed() >= cap {
                    let elapsed = started.elapsed();
                    let _ = child.kill();
                    let _ = child.wait();
                    let (out, err) = drained.finish();
                    panic!(
                        "the server on port {port} (pid {pid}) never exited after SIGINT: \
                         still waiting for the end of the process after {elapsed:?} \
                         (cap {cap:?}, raise it with {TIMEOUT_ENV})\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&out),
                        String::from_utf8_lossy(&err)
                    );
                }
                sleep(POLL_REST);
            };
            let elapsed = started.elapsed();
            let (stdout, stderr) = drained.finish();
            (
                Output {
                    status,
                    stdout,
                    stderr,
                },
                elapsed,
            )
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Some(drained) = self.drained.take() {
                let _ = drained.finish();
            }
        }
    }

    #[test]
    fn listening_port_requires_a_complete_startup_line() {
        assert_eq!(listening_port("INFO vauban listening port=5"), None);
        assert_eq!(
            listening_port("INFO config port=1234\nINFO vauban listening port=56789\n"),
            Some(56789)
        );
        assert_eq!(
            listening_port("{\"fields\":{\"message\":\"vauban listening\",\"port\":56789}}\n"),
            Some(56789)
        );
    }

    #[test]
    fn explicit_port_and_overflow_are_checked() {
        assert_eq!(configured_port(None, 24), 0);
        assert_eq!(configured_port(Some(" 25200 "), 20), 25220);
        assert_eq!(configured_port(Some("65511"), 24), 65535);
        assert!(std::panic::catch_unwind(|| configured_port(Some("65512"), 24)).is_err());
    }

    #[test]
    fn startup_timeout_reaps_the_child() {
        let mut command = vauban();
        command.args([
            "serve",
            "--in-memory",
            "--no-auth",
            "--encrypt",
            "off",
            "--port",
            "0",
            "--log-level",
            "warn",
        ]);
        let child = command.spawn().expect("spawn server");
        let pid = child.id();
        let result = std::panic::catch_unwind(|| {
            Running::wait_for_start(child, 0, Duration::from_millis(100))
        });
        assert!(
            result.is_err(),
            "warn-level child should time out without its startup log"
        );
        let status = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .expect("check process");
        assert!(!status.success(), "timed-out child {pid} survived");
    }

    #[test]
    fn occupied_explicit_port_is_not_mistaken_for_our_child() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve port");
        let port = listener.local_addr().expect("address").port();
        let result = std::panic::catch_unwind(|| {
            let mut command = vauban();
            command.args([
                "serve",
                "--bind",
                "127.0.0.1",
                "--in-memory",
                "--no-auth",
                "--encrypt",
                "off",
                "--port",
                &port.to_string(),
            ]);
            Running::spawn(command, port)
        });
        let panic = match result {
            Ok(_) => panic!("neighbor must not count as our child"),
            Err(panic) => panic,
        };
        let message = panic.downcast_ref::<String>().expect("panic diagnostic");
        assert!(message.contains("cannot listen on 127.0.0.1:"), "{message}");
        assert!(TcpStream::connect(listener.local_addr().expect("address")).is_ok());
    }

    #[test]
    fn allocated_sockets_are_distinct() {
        let mut first_command = vauban();
        first_command.args([
            "serve",
            "--in-memory",
            "--no-auth",
            "--encrypt",
            "off",
            "--port",
            "0",
        ]);
        let first = Running::spawn(first_command, 0);
        let mut second_command = vauban();
        second_command.args([
            "serve",
            "--in-memory",
            "--no-auth",
            "--encrypt",
            "off",
            "--port",
            "0",
        ]);
        let second = Running::spawn(second_command, 0);
        assert_ne!(first.port, second.port);
        assert!(first.interrupt().0.status.success());
        assert!(second.interrupt().0.status.success());
    }

    #[test]
    fn serve_accepts_connections_and_stops_on_sigint() {
        let server = Running::start(20, &["--no-auth"]);
        assert!(TcpStream::connect(("127.0.0.1", server.port)).is_ok());
        let (output, elapsed) = server.interrupt();
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        assert!(elapsed < timeout(), "shutdown took {elapsed:?}");
        let out = stdout(&output);
        assert!(out.contains("vauban listening"), "{out}");
        assert!(out.contains("shutdown complete"), "{out}");
    }

    #[test]
    fn json_log_format_writes_one_object_per_line() {
        let server = Running::start(21, &["--no-auth", "--log-format", "json"]);
        let (output, _) = server.interrupt();
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        let out = stdout(&output);
        let mut lines = 0;
        for line in out.lines().filter(|line| !line.trim().is_empty()) {
            let value: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|err| panic!("{err}: {line}"));
            assert!(value.is_object(), "not an object: {line}");
            lines += 1;
        }
        assert!(
            lines >= 2,
            "expected the startup and shutdown lines:\n{out}"
        );
    }

    #[test]
    fn password_never_reaches_the_output_even_at_trace() {
        let server = Running::start(22, &["--sa-password", "Secret1!", "--log-level", "trace"]);
        let (output, _) = server.interrupt();
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        let out = stdout(&output);
        let err = stderr(&output);
        assert!(
            !out.contains("Secret1!"),
            "password leaked on stdout:\n{out}"
        );
        assert!(
            !err.contains("Secret1!"),
            "password leaked on stderr:\n{err}"
        );
        assert!(
            out.contains("sa password passed on the command line is visible in the process list"),
            "warning missing:\n{out}"
        );
        assert!(
            out.contains("<redacted>"),
            "config not logged redacted:\n{out}"
        );
        assert!(
            out.contains("auth=\"sa\"") || out.contains("auth=sa"),
            "{out}"
        );
    }

    #[test]
    fn password_from_environment_starts_the_server_without_warning() {
        let port = port(23);
        let mut command = vauban();
        command
            .args([
                "serve",
                "--in-memory",
                "--encrypt",
                "off",
                "--port",
                &port.to_string(),
            ])
            .env("VAUBAN_SA_PASSWORD", "Secret1!");
        let server = Running::spawn(command, port);
        let (output, _) = server.interrupt();
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        let out = stdout(&output);
        assert!(!out.contains("Secret1!"), "{out}");
        assert!(!out.contains("visible in the process list"), "{out}");
    }

    #[test]
    fn config_file_sets_the_port_and_the_flags() {
        let port = port(24);
        let dir = std::env::temp_dir().join(format!("vauban-cli-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("vauban.toml");
        std::fs::write(
            &file,
            format!("port = {port}\nin_memory = true\nno_auth = true\nencrypt = \"off\"\n"),
        )
        .expect("write config");

        let mut command = vauban();
        command.args(["serve", "--config", file.to_str().expect("utf-8 path")]);
        let server = Running::spawn(command, port);
        let port = server.port;
        let (output, _) = server.interrupt();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(output.status.code(), Some(0));
        assert!(
            stdout(&output).contains(&format!("port={port}")),
            "{}",
            stdout(&output)
        );
    }
}
