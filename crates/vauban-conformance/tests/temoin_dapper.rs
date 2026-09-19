//! Runs the Dapper witness against a local `vauban serve --in-memory` instance.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repository root")
}

fn witness_dir() -> PathBuf {
    std::env::var_os("VAUBAN_DAPPER_WITNESS")
        .map(PathBuf::from)
        .filter(|p| p.join("run.sh").is_file())
        .unwrap_or_else(|| {
            repo_root()
                .join(format!("{}formance", "con"))
                .join("temoin/dapper")
        })
}

fn vauban_binary() -> PathBuf {
    let name = format!("vauban{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("VAUBAN_BIN") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join(&name));
        candidates.push(dir.join("../debug").join(&name));
    }
    candidates.push(repo_root().join("target/debug").join(&name));
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "no vauban binary: run `cargo build -p vauban-cli` or set VAUBAN_BIN \
                 (tried: {candidates:?})"
            )
        })
}

fn dotnet_available() -> bool {
    Command::new("dotnet")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn test_port() -> u16 {
    std::env::var("VAUBAN_TEST_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(55_000)
}

fn spawn_vauban(port: u16) -> Child {
    Command::new(vauban_binary())
        .args([
            "serve",
            "--in-memory",
            "--no-auth",
            "--encrypt",
            "off",
            "--bind",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--log-format",
            "json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn vauban serve")
}

fn wait_for_port(child: &mut Child) -> bool {
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => return false,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) => return false,
            Ok(n) => n,
            Err(_) => return false,
        };
        buf.extend_from_slice(&chunk[..n]);
        if parse_listening_port(&buf).is_some() {
            child.stdout = Some(stdout);
            return true;
        }
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

fn parse_listening_port(buf: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(buf);
    for line in text.lines() {
        if !line.contains("vauban listening") {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let port = value.get("fields")?.get("port")?.as_u64()?;
        let port: u16 = port.try_into().ok()?;
        if port != 0 {
            return Some(port);
        }
    }
    None
}

fn run_witness(hostport: &str) -> Output {
    let script = witness_dir().join("run.sh");
    Command::new("sh")
        .arg(&script)
        .arg(hostport)
        .current_dir(witness_dir())
        .output()
        .unwrap_or_else(|e| panic!("run {}: {e}", script.display()))
}

#[test]
#[ignore = "needs dotnet SDK and a built vauban binary"]
fn dapper_witness_matches_expected_on_in_memory_server() {
    if !dotnet_available() {
        eprintln!("skipped: dotnet SDK not installed");
        return;
    }
    let expected = witness_dir().join("expected.txt");
    assert!(expected.is_file(), "missing {}", expected.display());

    let port = test_port();
    let mut child = spawn_vauban(port);
    if !wait_for_port(&mut child) {
        let _ = child.kill();
        let out = child.wait_with_output().expect("wait for vauban");
        panic!(
            "vauban serve did not listen within 30 s: stdout {} stderr {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    let hostport = format!("127.0.0.1:{port}");
    let out = run_witness(&hostport);
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        out.status.success(),
        "witness failed: status {} stdout {} stderr {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
