//! Configuration of `vauban serve`: command-line options (`clap`), the optional
//! `vauban.toml`, the environment variables (`VAUBAN_SA_PASSWORD` and the product
//! identity overrides), and their merge.
//!
//! Precedence, highest first: command line > environment variable > file > default.
//!
//! Every command-line option is an `Option<T>` **without** `default_value`: a default
//! applied by `clap` would look like an explicit choice and win over the file. The three
//! sources are folded into a [`ConfigLayer`] each, merged, and the defaults are applied
//! last by [`Config::finalize`].

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};
use serde::Deserialize;
use vauban_session::EncryptPolicy;

/// Name of the environment variable that carries the `sa` password.
pub(crate) const SA_PASSWORD_ENV: &str = "VAUBAN_SA_PASSWORD";
/// LOGINACK `ProgName` override. Empty is unset.
pub(crate) const PROGRAM_NAME_ENV: &str = "VAUBAN_PROGRAM_NAME";
/// `@@VERSION` override. Empty is unset.
pub(crate) const VERSION_BANNER_ENV: &str = "VAUBAN_VERSION_BANNER";
/// `SERVERPROPERTY('Edition')` override. Empty is unset.
pub(crate) const EDITION_ENV: &str = "VAUBAN_EDITION";
/// File read when `--config` is not given, if it exists in the working directory.
pub(crate) const DEFAULT_CONFIG_FILE: &str = "vauban.toml";

/// Longest `program_name` LOGINACK can carry. Its `ProgName` is a B_VARCHAR
/// ([MS-TDS] 2.2.7.14): one length byte, then that many UTF-16 code units
/// ([MS-TDS] 2.2.5.1). A longer name cannot be encoded at all, so it is refused at startup
/// instead of cutting the first login without an error token.
pub(crate) const PROGRAM_NAME_MAX_UNITS: usize = 255;

/// Default listening address.
const DEFAULT_BIND: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
/// Default TCP port (the SQL Server default instance port).
const DEFAULT_PORT: u16 = 1433;

/// Options of `vauban serve`. All optional: see the module documentation.
/// No `Debug`: `sa_password` must not reach the log through the raw arguments.
#[derive(Parser, Default, Clone)]
pub(crate) struct ServeArgs {
    /// Address to listen on [default: 0.0.0.0]
    #[arg(long, value_name = "ADDR")]
    pub(crate) bind: Option<IpAddr>,

    /// TCP port to listen on [default: 1433]
    #[arg(long, value_name = "N")]
    pub(crate) port: Option<u16>,

    /// Data directory (holds tls/server.crt and tls/server.key; databases come with disk storage)
    /// Instance directory on disk (vauban.ctl, data, wal); mutually exclusive with --in-memory
    #[arg(long, value_name = "DIR")]
    pub(crate) data: Option<PathBuf>,

    /// Keep the databases in memory; mutually exclusive with --data
    #[arg(long)]
    pub(crate) in_memory: bool,

    /// Password of the `sa` login; prefer the VAUBAN_SA_PASSWORD variable
    #[arg(long, value_name = "PASSWORD")]
    pub(crate) sa_password: Option<String>,

    /// Accept any login and password (development only)
    #[arg(long)]
    pub(crate) no_auth: bool,

    /// Encryption policy announced at PRELOGIN [default: optional]
    #[arg(long, value_name = "POLICY")]
    pub(crate) encrypt: Option<Encrypt>,

    /// Server certificate, PEM
    #[arg(long, value_name = "PEM")]
    pub(crate) cert: Option<PathBuf>,

    /// Private key of the certificate, PEM
    #[arg(long, value_name = "PEM")]
    pub(crate) key: Option<PathBuf>,

    /// Minimum level written to the log (RUST_LOG wins if set) [default: info]
    #[arg(long, value_name = "LEVEL")]
    pub(crate) log_level: Option<LogLevel>,

    /// Log format [default: text]
    #[arg(long, value_name = "FORMAT")]
    pub(crate) log_format: Option<LogFormat>,

    /// Configuration file [default: ./vauban.toml if it exists]
    #[arg(long, value_name = "FILE")]
    pub(crate) config: Option<PathBuf>,
}

/// `--encrypt` values.
#[derive(ValueEnum, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Encrypt {
    /// No TLS at all: the server announces ENCRYPT_NOT_SUP.
    Off,
    /// The login is encrypted when the client asks for it: ENCRYPT_OFF.
    Optional,
    /// Everything is encrypted: ENCRYPT_REQ.
    Required,
}

impl From<Encrypt> for EncryptPolicy {
    fn from(value: Encrypt) -> Self {
        match value {
            Encrypt::Off => EncryptPolicy::Off,
            Encrypt::Optional => EncryptPolicy::Optional,
            Encrypt::Required => EncryptPolicy::Required,
        }
    }
}

/// `--log-level` values, in increasing severity.
#[derive(ValueEnum, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    /// Everything, including the wire-level detail.
    Trace,
    /// Diagnostic detail.
    Debug,
    /// Life-cycle events: startup, connections, shutdown.
    Info,
    /// Recoverable problems.
    Warn,
    /// Failures only.
    Error,
}

impl LogLevel {
    /// The level as a `tracing_subscriber::EnvFilter` directive.
    pub(crate) fn as_directive(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// `--log-format` values.
#[derive(ValueEnum, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogFormat {
    /// Human-readable lines; colour when the output stream supports it.
    Text,
    /// Human-readable lines without ANSI styling.
    Plain,
    /// One JSON object per line.
    Json,
}

/// Where the effective `sa` password came from; drives the command-line warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasswordSource {
    /// `--sa-password`.
    CommandLine,
    /// `VAUBAN_SA_PASSWORD`.
    Environment,
    /// `sa_password` in the configuration file.
    File,
}

/// One source of configuration, every field optional. Also the `serde` shape of
/// `vauban.toml` (keys are the option names with underscores).
#[derive(Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigLayer {
    /// `--bind`.
    pub(crate) bind: Option<IpAddr>,
    /// `--port`.
    pub(crate) port: Option<u16>,
    /// `--data`.
    pub(crate) data: Option<PathBuf>,
    /// `--in-memory`.
    pub(crate) in_memory: Option<bool>,
    /// `--sa-password`.
    pub(crate) sa_password: Option<String>,
    /// `--no-auth`.
    pub(crate) no_auth: Option<bool>,
    /// `--encrypt`.
    pub(crate) encrypt: Option<Encrypt>,
    /// `--cert`.
    pub(crate) cert: Option<PathBuf>,
    /// `--key`.
    pub(crate) key: Option<PathBuf>,
    /// `--log-level`.
    pub(crate) log_level: Option<LogLevel>,
    /// `--log-format`.
    pub(crate) log_format: Option<LogFormat>,
    /// `VAUBAN_PROGRAM_NAME` / `program_name`.
    pub(crate) program_name: Option<String>,
    /// `VAUBAN_VERSION_BANNER` / `version_banner`.
    pub(crate) version_banner: Option<String>,
    /// `VAUBAN_EDITION` / `edition`.
    pub(crate) edition: Option<String>,
}

impl ConfigLayer {
    /// The command line as a layer. The two flags are `Some(true)` only when passed:
    /// `clap` cannot say "absent" for a flag, and an absent flag must not hide the file.
    pub(crate) fn from_args(args: &ServeArgs) -> Self {
        Self {
            bind: args.bind,
            port: args.port,
            data: args.data.clone(),
            in_memory: args.in_memory.then_some(true),
            sa_password: args.sa_password.clone(),
            no_auth: args.no_auth.then_some(true),
            encrypt: args.encrypt,
            cert: args.cert.clone(),
            key: args.key.clone(),
            log_level: args.log_level,
            log_format: args.log_format,
            program_name: None,
            version_banner: None,
            edition: None,
        }
    }

    /// The environment as a layer: only `VAUBAN_SA_PASSWORD`, passed in by the caller
    /// so that tests do not touch the process environment.
    pub(crate) fn from_env_password(sa_password: Option<String>) -> Self {
        Self {
            sa_password,
            ..Self::default()
        }
    }

    /// Parses the content of a `vauban.toml`. Unknown keys are refused.
    pub(crate) fn from_toml(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|err| ConfigError::File(err.to_string()))
    }

    /// Reads and parses `path`.
    pub(crate) fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| ConfigError::File(format!("cannot read {}: {err}", path.display())))?;
        Self::from_toml(&text)
            .map_err(|err| ConfigError::File(format!("{}: {err}", path.display())))
    }

    /// Fills the holes of `self` with `lower`: `self` wins on every field it sets.
    pub(crate) fn over(self, lower: Self) -> Self {
        Self {
            bind: self.bind.or(lower.bind),
            port: self.port.or(lower.port),
            data: self.data.or(lower.data),
            in_memory: self.in_memory.or(lower.in_memory),
            sa_password: self.sa_password.or(lower.sa_password),
            no_auth: self.no_auth.or(lower.no_auth),
            encrypt: self.encrypt.or(lower.encrypt),
            cert: self.cert.or(lower.cert),
            key: self.key.or(lower.key),
            log_level: self.log_level.or(lower.log_level),
            log_format: self.log_format.or(lower.log_format),
            program_name: self.program_name.or(lower.program_name),
            version_banner: self.version_banner.or(lower.version_banner),
            edition: self.edition.or(lower.edition),
        }
    }
}

/// The effective configuration of `serve`, defaults applied.
///
/// `Debug` is written by hand: the password is never printed.
#[derive(Clone)]
pub(crate) struct Config {
    /// Listening address.
    pub(crate) bind: IpAddr,
    /// Listening port.
    pub(crate) port: u16,
    /// Data directory, if any.
    pub(crate) data: Option<PathBuf>,
    /// In-memory storage requested.
    pub(crate) in_memory: bool,
    /// `sa` password, if any.
    pub(crate) sa_password: Option<String>,
    /// Origin of [`Config::sa_password`], `None` when there is no password.
    pub(crate) sa_password_source: Option<PasswordSource>,
    /// Accept any login.
    pub(crate) no_auth: bool,
    /// Encryption policy.
    pub(crate) encrypt: EncryptPolicy,
    /// Certificate path.
    pub(crate) cert: Option<PathBuf>,
    /// Private key path.
    pub(crate) key: Option<PathBuf>,
    /// Log level.
    pub(crate) log_level: LogLevel,
    /// Log format.
    pub(crate) log_format: LogFormat,
    /// LOGINACK `ProgName` override.
    pub(crate) program_name: Option<String>,
    /// `@@VERSION` override.
    pub(crate) version_banner: Option<String>,
    /// `SERVERPROPERTY('Edition')` override.
    pub(crate) edition: Option<String>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("data", &self.data)
            .field("in_memory", &self.in_memory)
            .field(
                "sa_password",
                &self.sa_password.as_ref().map(|_| "<redacted>"),
            )
            .field("sa_password_source", &self.sa_password_source)
            .field("no_auth", &self.no_auth)
            .field("encrypt", &self.encrypt)
            .field("cert", &self.cert)
            .field("key", &self.key)
            .field("log_level", &self.log_level)
            .field("log_format", &self.log_format)
            .field("program_name", &self.program_name)
            .field("version_banner", &self.version_banner)
            .field("edition", &self.edition)
            .finish()
    }
}

/// A configuration the server cannot start with. Every variant maps to exit code 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigError {
    /// The configuration file cannot be read or parsed.
    File(String),
    /// Neither a `sa` password nor `--no-auth`.
    MissingPassword,
    /// `--cert` without `--key` or the reverse.
    CertWithoutKey,
    /// Neither `--in-memory` nor `--data <dir>` given: the server has to be told which of
    /// the two storage implementations to open.
    StorageUnset,
    /// Both `--in-memory` and `--data <dir>` given: the two are exclusive.
    StorageBoth,
    /// `program_name` longer than [`PROGRAM_NAME_MAX_UNITS`]; holds its length in UTF-16
    /// code units.
    ProgramNameTooLong(usize),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::File(msg) => write!(f, "invalid configuration file: {msg}"),
            ConfigError::MissingPassword => write!(
                f,
                "a sa password is required (--sa-password or {SA_PASSWORD_ENV}), or pass --no-auth"
            ),
            ConfigError::CertWithoutKey => {
                write!(f, "--cert and --key must be given together")
            }
            ConfigError::StorageUnset => {
                write!(f, "disk storage requires --data <dir>, or pass --in-memory")
            }
            ConfigError::StorageBoth => {
                write!(f, "--data and --in-memory are mutually exclusive")
            }
            ConfigError::ProgramNameTooLong(units) => write!(
                f,
                "program_name is {units} UTF-16 code units long; \
                 LOGINACK carries at most {PROGRAM_NAME_MAX_UNITS}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Same merge as [`Config::assemble`], with a full environment layer (identity included).
    pub(crate) fn assemble_from_layers(
        args: &ServeArgs,
        env: ConfigLayer,
        file: Option<ConfigLayer>,
    ) -> Self {
        let cli = ConfigLayer::from_args(args);
        let file = file.unwrap_or_default();

        let sa_password_source = if cli.sa_password.is_some() {
            Some(PasswordSource::CommandLine)
        } else if env.sa_password.is_some() {
            Some(PasswordSource::Environment)
        } else if file.sa_password.is_some() {
            Some(PasswordSource::File)
        } else {
            None
        };

        Self::finalize(cli.over(env).over(file), sa_password_source)
    }

    /// Applies the defaults to a merged layer.
    pub(crate) fn finalize(layer: ConfigLayer, sa_password_source: Option<PasswordSource>) -> Self {
        Self {
            bind: layer.bind.unwrap_or(DEFAULT_BIND),
            port: layer.port.unwrap_or(DEFAULT_PORT),
            data: layer.data,
            in_memory: layer.in_memory.unwrap_or(false),
            sa_password: layer.sa_password,
            sa_password_source,
            no_auth: layer.no_auth.unwrap_or(false),
            encrypt: layer.encrypt.unwrap_or(Encrypt::Optional).into(),
            cert: layer.cert,
            key: layer.key,
            log_level: layer.log_level.unwrap_or(LogLevel::Info),
            log_format: layer.log_format.unwrap_or(LogFormat::Text),
            program_name: nonempty_string(layer.program_name),
            version_banner: nonempty_string(layer.version_banner),
            edition: nonempty_string(layer.edition),
        }
    }

    /// Refuses a configuration the server cannot start with. The `--encrypt` rule lives
    /// in `tls.rs`, which owns everything TLS.
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if !self.no_auth && self.sa_password.is_none() {
            return Err(ConfigError::MissingPassword);
        }
        if self.cert.is_some() != self.key.is_some() {
            return Err(ConfigError::CertWithoutKey);
        }
        if self.in_memory && self.data.is_some() {
            return Err(ConfigError::StorageBoth);
        }
        if !self.in_memory && self.data.is_none() {
            return Err(ConfigError::StorageUnset);
        }
        if let Some(name) = &self.program_name {
            // UTF-16 code units, not bytes and not characters: the length byte of a
            // B_VARCHAR counts units, so one astral character weighs two.
            let units = name.encode_utf16().count();
            if units > PROGRAM_NAME_MAX_UNITS {
                return Err(ConfigError::ProgramNameTooLong(units));
            }
        }
        Ok(())
    }

    /// Warnings to log once the subscriber is up. Never contain the password.
    pub(crate) fn warnings(&self) -> Vec<&'static str> {
        let mut warnings = Vec::new();
        if self.sa_password_source == Some(PasswordSource::CommandLine) {
            warnings.push(
                "sa password passed on the command line is visible in the process list; \
                 prefer VAUBAN_SA_PASSWORD",
            );
        }
        if self.no_auth && self.sa_password.is_some() {
            warnings.push("--no-auth ignores the sa password");
        }
        if self.program_name.is_some() || self.version_banner.is_some() || self.edition.is_some() {
            warnings.push("product identity overridden by the operator");
        }
        warnings
    }

    /// LOGINACK name, `@@VERSION` and edition to hand to [`vauban_session::ServerConfig`].
    ///
    /// When only the edition is set, the last line of the default banner is rewritten so
    /// the two stay in agreement.
    pub(crate) fn product_identity(&self) -> (Option<String>, Option<String>, Option<String>) {
        let edition = self.edition.clone();
        let version_banner = match (&self.version_banner, &edition) {
            (Some(banner), _) => Some(banner.clone()),
            (None, Some(edition)) => Some(vauban_session::banner_with_edition(edition)),
            (None, None) => None,
        };
        (self.program_name.clone(), version_banner, edition)
    }

    /// `"sa"` or `"no-auth"`, for the startup log line.
    pub(crate) fn auth_mode(&self) -> &'static str {
        if self.no_auth { "no-auth" } else { "sa" }
    }
}

/// Empty strings count as unset, like [`SA_PASSWORD_ENV`].
fn nonempty_string(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests;
