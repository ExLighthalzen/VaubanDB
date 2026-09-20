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

#[cfg(test)]
mod tests;
