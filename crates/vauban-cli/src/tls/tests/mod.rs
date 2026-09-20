//! Unit tests of the TLS policy: `EncryptPolicy` per `--encrypt` choice, certificate
//! and key loading from `--cert`/`--key` or generation under `--data`, and the shape
//! handshakes with `tiberius` and the fingerprint helper produce.

use super::*;
use crate::config::{Config, ServeArgs};
use clap::Parser;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme, SupportedProtocolVersion};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Server name used by every test that generates a certificate.
const NAME: &str = "vauban-test";

fn config(list: &[&str]) -> Config {
    let args = ServeArgs::try_parse_from(std::iter::once("serve").chain(list.iter().copied()))
        .expect("valid arguments");
    Config::assemble(&args, None, None)
}

/// A directory under the system temporary directory, removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("vauban-cli-tls-{}-{n}-{label}", std::process::id()));
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

/// The parsed leaf of the certificate file at `path`.
fn cert_at(path: &Path) -> CertificateDer<'static> {
    read_certs(path)
        .expect("readable certificate")
        .into_iter()
        .next()
        .expect("one certificate")
}

/// Accepts any server certificate: the tests only care about the protocol version.
#[derive(Debug)]
struct TrustAny;

impl ServerCertVerifier for TrustAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Moves every pending TLS byte of `from` into `to` and lets `to` process it.
fn pump(from: &mut rustls::Connection, to: &mut rustls::Connection) -> Result<(), rustls::Error> {
    let mut wire = Vec::new();
    while from.wants_write() {
        from.write_tls(&mut wire).expect("write to a Vec");
    }
    let mut cursor = wire.as_slice();
    while !cursor.is_empty() {
        to.read_tls(&mut cursor).expect("read from a slice");
    }
    to.process_new_packets().map(|_| ())
}

/// Runs a full handshake over in-memory buffers between a client restricted to
/// `versions` and `server`; `Err` when either side gives up.
fn handshake(
    server: Arc<rustls::ServerConfig>,
    versions: &[&'static SupportedProtocolVersion],
) -> Result<(), rustls::Error> {
    let client_cfg = rustls::ClientConfig::builder_with_protocol_versions(versions)
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAny))
        .with_no_client_auth();
    let name = ServerName::try_from(LOCALHOST).expect("valid name");
    let mut client =
        rustls::Connection::Client(rustls::ClientConnection::new(Arc::new(client_cfg), name)?);
    let mut server = rustls::Connection::Server(rustls::ServerConnection::new(server)?);
    // A TLS 1.2 handshake takes two round trips; the bound only guards against a loop.
    for _ in 0..8 {
        pump(&mut client, &mut server)?;
        pump(&mut server, &mut client)?;
        if !client.is_handshaking() && !server.is_handshaking() {
            return Ok(());
        }
    }
    panic!("handshake did not complete");
}

#[test]
fn off_yields_no_tls() {
    let tls = build_tls_for(&config(&["--encrypt", "off"]), NAME).expect("off is accepted");
    assert!(tls.is_none());
}

#[test]
fn optional_without_data_yields_an_ephemeral_certificate() {
    let tls = build_tls_for(&config(&["--encrypt", "optional"]), NAME).expect("generated");
    assert!(tls.is_some());
    let tls = build_tls_for(&config(&["--encrypt", "required"]), NAME).expect("generated");
    assert!(tls.is_some());
}

#[test]
fn ephemeral_certificates_differ_between_generations() {
    let a = generate(NAME).expect("generated").into_material();
    let b = generate(NAME).expect("generated").into_material();
    assert_ne!(fingerprint(&a.certs[0]), fingerprint(&b.certs[0]));
}

#[test]
fn data_dir_generates_once_then_reuses() {
    let dir = TempDir::new("data");
    let (cert_path, key_path) = generated_paths(dir.path());
    let cfg = config(&["--encrypt", "optional", "--data", dir.arg()]);

    assert!(!cert_path.exists());
    build_tls_for(&cfg, NAME)
        .expect("first start")
        .expect("tls");
    assert!(cert_path.is_file(), "{}", cert_path.display());
    assert!(key_path.is_file(), "{}", key_path.display());
    let first = fingerprint(&cert_at(&cert_path));
    let cert_text = fs::read_to_string(&cert_path).expect("read cert");
    let key_text = fs::read_to_string(&key_path).expect("read key");
    assert!(
        cert_text.starts_with("-----BEGIN CERTIFICATE-----"),
        "{cert_text}"
    );
    assert!(
        key_text.starts_with("-----BEGIN PRIVATE KEY-----"),
        "{key_text}"
    );

    build_tls_for(&cfg, NAME)
        .expect("second start")
        .expect("tls");
    let second = fingerprint(&cert_at(&cert_path));
    assert_eq!(first, second, "the certificate was regenerated");
    assert_eq!(
        cert_text,
        fs::read_to_string(&cert_path).expect("read cert"),
        "the certificate file was rewritten"
    );
}

#[cfg(unix)]
#[test]
fn generated_key_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("mode");
    let (cert_path, key_path) = generated_paths(dir.path());
    build_tls_for(&config(&["--data", dir.arg()]), NAME)
        .expect("start")
        .expect("tls");
    let key_mode = fs::metadata(&key_path).expect("key").permissions().mode() & 0o777;
    assert_eq!(key_mode, 0o600, "key mode {key_mode:o}");
    let cert_mode = fs::metadata(&cert_path).expect("cert").permissions().mode() & 0o777;
    assert_eq!(cert_mode, 0o644, "cert mode {cert_mode:o}");
}

#[test]
fn data_dir_with_one_file_missing_is_refused() {
    let dir = TempDir::new("half");
    let (cert_path, key_path) = generated_paths(dir.path());
    build_tls_for(&config(&["--data", dir.arg()]), NAME)
        .expect("start")
        .expect("tls");
    fs::remove_file(&key_path).expect("remove key");
    let err = build_tls_for(&config(&["--data", dir.arg()]), NAME).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains(&cert_path.display().to_string()), "{text}");
    assert!(text.contains(&key_path.display().to_string()), "{text}");
    assert!(text.contains("is missing"), "{text}");
}

#[test]
fn generated_certificate_parses_and_names_localhost_and_the_server() {
    let material = generate(NAME).expect("generated").into_material();
    let parsed =
        rustls::server::ParsedCertificate::try_from(&material.certs[0]).expect("x509 parses");
    for name in [LOCALHOST, NAME] {
        let server_name = ServerName::try_from(name).expect("valid name");
        rustls::client::verify_server_name(&parsed, &server_name)
            .unwrap_or_else(|err| panic!("SAN lacks {name}: {err}"));
    }
    let loopback = ServerName::IpAddress(LOOPBACK.into());
    rustls::client::verify_server_name(&parsed, &loopback).expect("SAN lacks 127.0.0.1");
    let other = ServerName::try_from("example.com").expect("valid name");
    assert!(rustls::client::verify_server_name(&parsed, &other).is_err());
}

#[test]
fn generated_certificate_is_valid_for_ten_years() {
    // The certificate is its own trust anchor: a `webpki` verifier then checks the
    // signature, the SAN and the validity dates at the instant we choose.
    let material = generate(NAME).expect("generated").into_material();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(material.certs[0].clone())
        .expect("self-signed certificate as trust anchor");
    let verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .expect("verifier");
    let name = ServerName::try_from(LOCALHOST).expect("valid name");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970");
    let year = Duration::from_secs(365 * 24 * 60 * 60);
    let at = |instant: Duration| {
        verifier.verify_server_cert(
            &material.certs[0],
            &[],
            &name,
            &[],
            UnixTime::since_unix_epoch(instant),
        )
    };
    at(now + Duration::from_secs(60)).expect("valid now");
    at(now + 9 * year).expect("valid after nine years");
    let err = at(now + 11 * year).expect_err("expired after eleven years");
    assert!(
        matches!(
            err,
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::Expired | rustls::CertificateError::ExpiredContext { .. }
            )
        ),
        "{err:?}"
    );
    let err = at(now.saturating_sub(year)).expect_err("not yet valid a year ago");
    assert!(
        matches!(
            err,
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidYet
                    | rustls::CertificateError::NotValidYetContext { .. }
            )
        ),
        "{err:?}"
    );
}

#[test]
fn server_name_that_is_an_ip_or_not_a_dns_name_still_works() {
    let ip = generate("192.0.2.10").expect("generated").into_material();
    let parsed = rustls::server::ParsedCertificate::try_from(&ip.certs[0]).expect("x509");
    let addr = ServerName::IpAddress("192.0.2.10".parse::<IpAddr>().expect("ip").into());
    rustls::client::verify_server_name(&parsed, &addr).expect("SAN lacks the IP");

    let odd = generate("héllo wörld").expect("generated").into_material();
    let parsed = rustls::server::ParsedCertificate::try_from(&odd.certs[0]).expect("x509");
    let name = ServerName::try_from(LOCALHOST).expect("valid name");
    rustls::client::verify_server_name(&parsed, &name).expect("SAN lacks localhost");

    let local = generate("localhost").expect("generated").into_material();
    let parsed = rustls::server::ParsedCertificate::try_from(&local.certs[0]).expect("x509");
    rustls::client::verify_server_name(&parsed, &name).expect("SAN lacks localhost");
    assert_eq!(subject_alt_names("localhost").len(), 2);
}

#[test]
fn missing_cert_file_names_the_path() {
    let err = build_tls_for(
        &config(&[
            "--cert",
            "/nonexistent/server.crt",
            "--key",
            "/nonexistent/server.key",
        ]),
        NAME,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("/nonexistent/server.crt"),
        "{err:#}"
    );
}

#[test]
fn non_pem_cert_is_refused() {
    let dir = TempDir::new("nonpem");
    let cert_path = dir.path().join("server.crt");
    let key_path = dir.path().join("server.key");
    fs::write(&cert_path, "this is not PEM\n").expect("write");
    let generated = generate(NAME).expect("generated");
    fs::write(&key_path, generated.key_pem()).expect("write");
    let err = build_tls_for(
        &config(&[
            "--cert",
            cert_path.to_str().unwrap(),
            "--key",
            key_path.to_str().unwrap(),
        ]),
        NAME,
    )
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains(&cert_path.display().to_string()), "{text}");
    assert!(text.contains("no CERTIFICATE block"), "{text}");
}

#[test]
fn non_pem_key_and_missing_key_are_refused() {
    let dir = TempDir::new("badkey");
    let cert_path = dir.path().join("server.crt");
    let key_path = dir.path().join("server.key");
    let generated = generate(NAME).expect("generated");
    fs::write(&cert_path, generated.cert_pem()).expect("write");
    fs::write(&key_path, "not a key\n").expect("write");
    let err = build_tls_for(
        &config(&[
            "--cert",
            cert_path.to_str().unwrap(),
            "--key",
            key_path.to_str().unwrap(),
        ]),
        NAME,
    )
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("no PRIVATE KEY block"), "{text}");

    let missing = dir.path().join("absent.key");
    let err = build_tls_for(
        &config(&[
            "--cert",
            cert_path.to_str().unwrap(),
            "--key",
            missing.to_str().unwrap(),
        ]),
        NAME,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains(&missing.display().to_string()),
        "{err:#}"
    );
}

#[test]
fn cert_and_key_that_do_not_match_are_refused() {
    let dir = TempDir::new("mismatch");
    let cert_path = dir.path().join("server.crt");
    let key_path = dir.path().join("server.key");
    fs::write(&cert_path, generate(NAME).expect("a").cert_pem()).expect("write");
    fs::write(&key_path, generate(NAME).expect("b").key_pem()).expect("write");
    let err = build_tls_for(
        &config(&[
            "--cert",
            cert_path.to_str().unwrap(),
            "--key",
            key_path.to_str().unwrap(),
        ]),
        NAME,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("not usable together"),
        "{err:#}"
    );
}

#[test]
fn loaded_pem_files_are_accepted() {
    let dir = TempDir::new("pem");
    let cert_path = dir.path().join("server.crt");
    let key_path = dir.path().join("server.key");
    let generated = generate(NAME).expect("generated");
    fs::write(&cert_path, generated.cert_pem()).expect("write");
    fs::write(&key_path, generated.key_pem()).expect("write");
    let tls = build_tls_for(
        &config(&[
            "--cert",
            cert_path.to_str().unwrap(),
            "--key",
            key_path.to_str().unwrap(),
        ]),
        NAME,
    )
    .expect("loaded");
    assert!(tls.is_some());
}

#[test]
fn tls12_client_connects_and_tls13_only_client_is_refused() {
    let server = build_tls_for(&config(&[]), NAME)
        .expect("generated")
        .expect("tls");
    handshake(Arc::clone(&server), &[&rustls::version::TLS12]).expect("TLS 1.2 handshake");
    let err = handshake(Arc::clone(&server), &[&rustls::version::TLS13])
        .expect_err("a TLS 1.3-only client must be refused");
    assert!(
        matches!(
            err,
            rustls::Error::PeerIncompatible(_) | rustls::Error::AlertReceived(_)
        ),
        "{err:?}"
    );
    // A client offering both settles on 1.2.
    handshake(server, &[&rustls::version::TLS13, &rustls::version::TLS12]).expect("TLS 1.2 chosen");
}

#[test]
fn fingerprint_is_sha256_colon_separated() {
    let material = generate(NAME).expect("generated").into_material();
    let fp = fingerprint(&material.certs[0]);
    assert_eq!(fp.len(), 32 * 3 - 1, "{fp}");
    assert!(
        fp.split(':')
            .all(|pair| pair.len() == 2 && u8::from_str_radix(pair, 16).is_ok())
    );
    // Known answer: SHA256 of the empty string.
    assert_eq!(
        fingerprint(&CertificateDer::from(Vec::new())),
        "E3:B0:C4:42:98:FC:1C:14:9A:FB:F4:C8:99:6F:B9:24:27:AE:41:E4:64:9B:93:4C:A4:95:99:1B:78:52:B8:55"
    );
}

/// Paths of the generated certificate and key under a `--data` directory.
fn generated_paths(data: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = data.join(TLS_DIR);
    (dir.join(CERT_FILE), dir.join(KEY_FILE))
}
