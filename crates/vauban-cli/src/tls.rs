//! TLS material for the server: the certificate and key given with
//! `--cert`/`--key`, or a self-signed certificate generated on the first start in
//! `--data`, or an ephemeral one when neither is available; and the `rustls` server
//! configuration built from them, **TLS 1.2 only**.
//!
//! TLS 1.2 is a constraint of the TDS 7.4 encapsulation of the handshake in PRELOGIN
//! packets: TLS 1.3 arrives with TDS 8.0, in a later version. Do not add it here.
//!
//! The private key is not written to the log; the SHA256 fingerprint of the certificate
//! is, so that an operator can compare it with what the client sees.

use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use rustls::SupportedCipherSuite;
use rustls::crypto::hash::HashAlgorithm;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tracing::{info, warn};
use vauban_session::EncryptPolicy;

use crate::config::Config;

/// Sub-directory of `--data` holding the generated material.
const TLS_DIR: &str = "tls";
/// Certificate file inside [`TLS_DIR`], PEM.
const CERT_FILE: &str = "server.crt";
/// Private key file inside [`TLS_DIR`], PEM, mode 0600 on Unix.
const KEY_FILE: &str = "server.key";
/// Validity of a generated certificate: ten years.
const VALIDITY: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);
/// Name always present in the SAN of a generated certificate, whatever the server name.
const LOCALHOST: &str = "localhost";
/// Address always present in the SAN of a generated certificate.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Certificate chain and private key, as `rustls` takes them.
struct Material {
    /// Leaf first, then the intermediates, if any.
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

/// A freshly generated self-signed certificate with its key.
struct Generated {
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
}

impl Generated {
    /// The certificate, PEM.
    fn cert_pem(&self) -> String {
        self.cert.pem()
    }

    /// The private key, PKCS#8 PEM.
    fn key_pem(&self) -> String {
        self.key.serialize_pem()
    }

    /// The material as `rustls` takes it.
    fn into_material(self) -> Material {
        Material {
            certs: vec![self.cert.der().clone()],
            key: PrivatePkcs8KeyDer::from(self.key.serialize_der()).into(),
        }
    }
}

/// Builds the `rustls` server configuration for `cfg`, `None` when no TLS is needed
/// (`--encrypt off`). The server name of the generated certificate is the one announced
/// to clients.
pub(crate) fn build_tls(cfg: &Config) -> anyhow::Result<Option<Arc<rustls::ServerConfig>>> {
    build_tls_for(cfg, &crate::server_name())
}

/// [`build_tls`] with an explicit server name (subject and SAN of a generated certificate).
fn build_tls_for(
    cfg: &Config,
    server_name: &str,
) -> anyhow::Result<Option<Arc<rustls::ServerConfig>>> {
    match cfg.encrypt {
        EncryptPolicy::Off => return Ok(None),
        EncryptPolicy::Optional | EncryptPolicy::Required => {}
    }
    let material = match (&cfg.cert, &cfg.key) {
        (Some(cert), Some(key)) => {
            let material = load_pem(cert, key)?;
            info!(
                cert = %cert.display(),
                fingerprint = %fingerprint(&material.certs[0]),
                "TLS certificate loaded"
            );
            material
        }
        (None, None) => match &cfg.data {
            Some(data) => load_or_generate(&data.join(TLS_DIR), server_name)?,
            None => {
                warn!(
                    "no --data and no --cert: using an ephemeral self-signed certificate \
                     that changes at every start"
                );
                let generated = generate(server_name)?;
                let material = generated.into_material();
                info!(
                    fingerprint = %fingerprint(&material.certs[0]),
                    "ephemeral self-signed TLS certificate generated"
                );
                material
            }
        },
        // `Config::validate` refuses one without the other before we get here.
        _ => bail!("--cert and --key must be given together"),
    };
    Ok(Some(Arc::new(server_config(material)?)))
}

/// Loads a PEM certificate chain and a PEM private key (PKCS#8, PKCS#1 RSA or SEC1 EC).
fn load_pem(cert_path: &Path, key_path: &Path) -> anyhow::Result<Material> {
    Ok(Material {
        certs: read_certs(cert_path)?,
        key: read_key(key_path)?,
    })
}

/// Reads every `CERTIFICATE` block of a PEM file; an error names the path.
fn read_certs(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let pem = fs::read(path)
        .with_context(|| format!("cannot read certificate file {}", path.display()))?;
    let certs = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("{} is not a PEM certificate", path.display()))?;
    if certs.is_empty() {
        bail!(
            "{}: no CERTIFICATE block found; a PEM certificate is expected",
            path.display()
        );
    }
    Ok(certs)
}

/// Reads the first private key of a PEM file; an error names the path.
fn read_key(path: &Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    let pem = fs::read(path)
        .with_context(|| format!("cannot read private key file {}", path.display()))?;
    rustls_pemfile::private_key(&mut pem.as_slice())
        .with_context(|| format!("{} is not a PEM private key", path.display()))?
        .ok_or_else(|| {
            anyhow!(
                "{}: no PRIVATE KEY block found; a PKCS#8, RSA or EC PEM key is expected",
                path.display()
            )
        })
}

/// Reads `<dir>/server.crt` and `<dir>/server.key` when both exist; generates and writes
/// them when neither does. One without the other is refused rather than overwritten.
fn load_or_generate(dir: &Path, server_name: &str) -> anyhow::Result<Material> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    match (cert_path.is_file(), key_path.is_file()) {
        (true, true) => {
            let material = load_pem(&cert_path, &key_path)?;
            info!(
                cert = %cert_path.display(),
                fingerprint = %fingerprint(&material.certs[0]),
                "TLS certificate loaded from the data directory"
            );
            Ok(material)
        }
        (false, false) => {
            let generated = generate(server_name)?;
            write_material(dir, &cert_path, &key_path, &generated)?;
            let material = generated.into_material();
            info!(
                cert = %cert_path.display(),
                fingerprint = %fingerprint(&material.certs[0]),
                "self-signed TLS certificate generated; clients must trust it \
                 (TrustServerCertificate=true, sqlcmd -C)"
            );
            Ok(material)
        }
        (cert_exists, _) => {
            let (present, missing) = if cert_exists {
                (&cert_path, &key_path)
            } else {
                (&key_path, &cert_path)
            };
            bail!(
                "{} exists but {} is missing; remove it to generate a new certificate",
                present.display(),
                missing.display()
            )
        }
    }
}

/// Generates a self-signed certificate: subject `CN=<server_name>`, SAN
/// `DNS:<server_name>`, `DNS:localhost`, `IP:127.0.0.1`, ten years, ECDSA P-256.
///
/// A server name that is not a valid DNS name (non-ASCII, for instance) is left out of
/// the SAN with a warning: `localhost` and `127.0.0.1` are always there, so clients that
/// check the name still have something to match.
fn generate(server_name: &str) -> anyhow::Result<Generated> {
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, server_name);
    params.subject_alt_names = subject_alt_names(server_name);
    let now = rcgen::date_time_ymd(1970, 1, 1)
        + SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before 1970")?;
    params.not_before = now;
    params.not_after = now + VALIDITY;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];

    // `KeyPair::generate` is ECDSA P-256 (`PKCS_ECDSA_P256_SHA256`).
    let key = rcgen::KeyPair::generate().context("cannot generate the TLS private key")?;
    let cert = params
        .self_signed(&key)
        .context("cannot generate the self-signed TLS certificate")?;
    Ok(Generated { cert, key })
}

/// SAN of a generated certificate: the server name (as an IP or a DNS name, if valid),
/// then `localhost` and `127.0.0.1`, without duplicates.
fn subject_alt_names(server_name: &str) -> Vec<rcgen::SanType> {
    let mut sans = Vec::with_capacity(3);
    if let Ok(ip) = server_name.parse::<IpAddr>() {
        sans.push(rcgen::SanType::IpAddress(ip));
    } else {
        match rcgen::string::Ia5String::try_from(server_name) {
            Ok(name) => sans.push(rcgen::SanType::DnsName(name)),
            Err(_) => warn!(
                server_name,
                "server name is not a valid DNS name; left out of the certificate SAN"
            ),
        }
    }
    if !server_name.eq_ignore_ascii_case(LOCALHOST) {
        // `localhost` is ASCII by construction.
        if let Ok(name) = rcgen::string::Ia5String::try_from(LOCALHOST) {
            sans.push(rcgen::SanType::DnsName(name));
        }
    }
    if !sans.contains(&rcgen::SanType::IpAddress(LOOPBACK)) {
        sans.push(rcgen::SanType::IpAddress(LOOPBACK));
    }
    sans
}

/// Writes the generated certificate and key in `dir`, creating it. The files are created
/// exclusively (never overwritten); the key is mode 0600 on Unix.
fn write_material(
    dir: &Path,
    cert_path: &Path,
    key_path: &Path,
    generated: &Generated,
) -> anyhow::Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("cannot create TLS directory {}", dir.display()))?;
    write_new(cert_path, &generated.cert_pem(), false)?;
    write_new(key_path, &generated.key_pem(), true)
}

/// Creates `path` with `content`, failing if it exists. `private` restricts the file to
/// its owner (0600) on Unix.
fn write_new(path: &Path, content: &str, private: bool) -> anyhow::Result<()> {
    let mut file = open_options(private)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", path.display()))
}

/// Exclusive creation, owner-only when `private`.
#[cfg(unix)]
fn open_options(private: bool) -> fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(if private { 0o600 } else { 0o644 });
    options
}

/// Exclusive creation; file modes do not exist on this platform.
#[cfg(not(unix))]
fn open_options(_private: bool) -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    options
}

/// The `rustls` server configuration: no client authentication, TLS 1.2 only, default
/// cipher suites of that version.
fn server_config(material: Material) -> anyhow::Result<rustls::ServerConfig> {
    // TLS 1.2 only (`tests::tls12_client_connects_and_tls13_only_client_is_refused`): the
    // PRELOGIN encapsulation of the handshake does not carry the post-handshake messages of
    // TLS 1.3; TLS 1.3 comes with TDS 8.0, in a later version.
    rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_no_client_auth()
        .with_single_cert(material.certs, material.key)
        .context("the TLS certificate and private key are not usable together")
}

/// SHA256 fingerprint of a DER certificate, `AB:CD:...` as `openssl x509 -fingerprint
/// -sha256` prints it.
///
/// `rustls` exposes no hash function on its own; the SHA256 of one of its cipher suites
/// is the same primitive, and the workspace has no dedicated hashing crate.
fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let hash = match rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256 {
        SupportedCipherSuite::Tls13(suite) => suite.common.hash_provider,
        // The constant above is a TLS 1.3 suite by definition.
        _ => unreachable!("TLS13_AES_128_GCM_SHA256 is a TLS 1.3 suite"),
    };
    debug_assert_eq!(hash.algorithm(), HashAlgorithm::SHA256);
    let digest = hash.hash(cert.as_ref());
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Paths of the generated certificate and key under a `--data` directory.
#[cfg(test)]
fn generated_paths(data: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = data.join(TLS_DIR);
    (dir.join(CERT_FILE), dir.join(KEY_FILE))
}

#[cfg(test)]
mod tests {
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
            let path = std::env::temp_dir()
                .join(format!("vauban-cli-tls-{}-{n}-{label}", std::process::id()));
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
    fn pump(
        from: &mut rustls::Connection,
        to: &mut rustls::Connection,
    ) -> Result<(), rustls::Error> {
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
                    rustls::CertificateError::Expired
                        | rustls::CertificateError::ExpiredContext { .. }
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
        handshake(server, &[&rustls::version::TLS13, &rustls::version::TLS12])
            .expect("TLS 1.2 chosen");
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
}
