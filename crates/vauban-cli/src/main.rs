//! Crate vauban-cli: the `vauban` binary: configuration, startup, logging,
//! graceful shutdown.
//!
//! Two subcommands: `serve` starts the server, `version` prints the version. Exit codes
//! of `serve`: 0 after a clean shutdown, 1 when the server cannot start or fails, 2 on a
//! configuration error, 130 when a second signal cuts the shutdown short.
//!
//! The `sa` password never reaches the log: the effective configuration is logged through
//! the hand-written `Debug` of [`config::Config`], and the startup line only names the
//! authentication mode.

mod config;
mod shutdown;
mod tls;

use std::fmt::Display;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use vauban_session::{
    Authenticator, Engine, NoAuth, REQUEST_THREAD_STACK_SIZE, SaPasswordAuthenticator, Server,
    ServerConfig,
};
use vauban_storage::{DiskOptions, DiskStorage, MemoryStorage, Storage};

use crate::config::{
    Config, ConfigError, ConfigLayer, DEFAULT_CONFIG_FILE, EDITION_ENV, LogFormat,
    PROGRAM_NAME_ENV, SA_PASSWORD_ENV, ServeArgs, VERSION_BANNER_ENV,
};

/// Exit code of a clean shutdown.
const EXIT_OK: u8 = 0;
/// Exit code when the server cannot start or fails while running.
const EXIT_FAILURE: u8 = 1;
/// Exit code of a configuration error.
const EXIT_USAGE: u8 = 2;
/// Packet size granted when the client requests none.
const DEFAULT_PACKET_SIZE: u16 = 4096;
/// Server name when the host name cannot be read.
const FALLBACK_SERVER_NAME: &str = "vauban";
/// Environment variable read for the host name (`std` does not expose it and the
/// workspace has no crate for it).
const HOSTNAME_ENV: &str = "HOSTNAME";

/// VaubanDB: a SQL Server-compatible alternative.
#[derive(Parser)]
#[command(name = "vauban", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Subcommands of the binary.
#[derive(Subcommand)]
enum Command {
    /// Start the server
    Serve(ServeArgs),
    /// Print the version and the build target
    Version,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Version => {
            println!("vauban {} ({})", env!("CARGO_PKG_VERSION"), build_target());
            ExitCode::from(EXIT_OK)
        }
        Command::Serve(args) => serve(&args),
    }
}

/// Architecture and operating system this binary was built for.
fn build_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// `vauban serve`: configuration, logging, then the server until a signal.
fn serve(args: &ServeArgs) -> ExitCode {
    let cfg = match load_config(args) {
        Ok(cfg) => cfg,
        Err(err) => return usage_error(err),
    };
    if let Err(err) = cfg.validate() {
        return usage_error(err);
    }

    init_tracing(&cfg);
    for warning in cfg.warnings() {
        warn!("{warning}");
    }
    debug!(config = ?cfg, "effective configuration");

    let tls = match tls::build_tls(&cfg) {
        Ok(tls) => tls,
        Err(err) => return usage_error(err),
    };

    match run(cfg, tls) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(err) => {
            error!(error = format!("{err:#}"), "server failed");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// Prints a configuration error the way `clap` does and returns exit code 2.
fn usage_error(err: impl Display) -> ExitCode {
    eprintln!("error: {err}");
    ExitCode::from(EXIT_USAGE)
}

/// Reads the configuration file (`--config`, or `./vauban.toml` if it exists), the
/// environment variables and the command line, and merges them.
///
/// An empty `VAUBAN_SA_PASSWORD` (and empty identity overrides) count as unset: a
/// shell that exports an empty variable must not start a server with that value.
fn load_config(args: &ServeArgs) -> Result<Config, ConfigError> {
    let file = match &args.config {
        Some(path) => Some(ConfigLayer::from_file(path)?),
        None => {
            let default = Path::new(DEFAULT_CONFIG_FILE);
            if default.is_file() {
                Some(ConfigLayer::from_file(default)?)
            } else {
                None
            }
        }
    };
    let env_password = std::env::var(SA_PASSWORD_ENV)
        .ok()
        .filter(|value| !value.is_empty());
    let mut env = ConfigLayer::from_env_password(env_password);
    env.program_name = nonempty_env(PROGRAM_NAME_ENV);
    env.version_banner = nonempty_env(VERSION_BANNER_ENV);
    env.edition = nonempty_env(EDITION_ENV);
    Ok(Config::assemble_from_layers(args, env, file))
}

/// Empty environment values count as unset, like [`SA_PASSWORD_ENV`].
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Installs the global `tracing` subscriber. `RUST_LOG`, when set, wins over
/// `--log-level`. Colours only when stdout is a terminal: a pipe or a file gets plain
/// text.
fn init_tracing(cfg: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.log_level.as_directive()));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal());
    match cfg.log_format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

/// Starts the tokio runtime and runs the server on it.
fn run(cfg: Config, tls: Option<Arc<rustls::ServerConfig>>) -> anyhow::Result<()> {
    // The stack of every runtime thread, workers and blocking pool alike: a batch is
    // parsed, bound and executed on one thread of that pool, and the depth guards of
    // `parser` and `binder` are sized for this stack.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(REQUEST_THREAD_STACK_SIZE)
        .build()
        .context("cannot start the tokio runtime")?;
    runtime.block_on(run_server(cfg, tls))
}

/// Assembles the engine and the server, listens, and serves until a signal.
async fn run_server(cfg: Config, tls: Option<Arc<rustls::ServerConfig>>) -> anyhow::Result<()> {
    // The registry is a process-wide table; both calls are idempotent. `compat` depends on
    // `session`, so `session` cannot make these calls itself: the binary does it.
    vauban_sysfn::register_builtins();
    vauban_compat::register_functions();

    // `Config::validate` refuses a configuration where both flags or neither is set, so
    // the `else` branch has the directory the caller asked for (`StorageBoth` and
    // `StorageUnset`).
    let storage: Arc<dyn Storage> = if cfg.in_memory {
        Arc::new(MemoryStorage::new())
    } else {
        let dir = cfg
            .data
            .as_ref()
            .expect("validate refused the empty --data branch");
        Arc::new(
            DiskStorage::open(dir, DiskOptions::default())
                .map_err(|err| anyhow::anyhow!("cannot open instance {}: {err}", dir.display()))?,
        )
    };
    let on_disk = !cfg.in_memory;
    let engine = Arc::new(Engine::new(Arc::clone(&storage)));
    let authenticator: Arc<dyn Authenticator> = match (cfg.no_auth, &cfg.sa_password) {
        (true, _) => Arc::new(NoAuth),
        (false, Some(password)) => Arc::new(SaPasswordAuthenticator(password.clone())),
        // `Config::validate` refuses this combination before we get here.
        (false, None) => anyhow::bail!("no sa password and --no-auth not given"),
    };
    let (program_name, version_banner, edition) = cfg.product_identity();
    let server_cfg = ServerConfig {
        encrypt: cfg.encrypt,
        tls,
        authenticator,
        server_name: server_name(),
        default_packet_size: DEFAULT_PACKET_SIZE,
        program_name,
        version_banner,
        edition,
    };

    // Signals first, socket second: once a client can connect, a SIGINT is already a
    // graceful shutdown and not the default action (process killed by the signal).
    let shutdown = shutdown::install().context("cannot install the signal handlers")?;
    let addr = SocketAddr::new(cfg.bind, cfg.port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    let local = listener
        .local_addr()
        .context("cannot read the listening address")?;

    info!(
        bind = %local.ip(),
        port = local.port(),
        encrypt = ?cfg.encrypt,
        auth = cfg.auth_mode(),
        storage = if on_disk { "disk" } else { "in-memory" },
        server_name = %server_cfg.server_name,
        "vauban listening"
    );

    Server::new(engine, server_cfg)
        .serve(listener, shutdown)
        .await
        .context("server failed")?;
    // A disk instance gets its last checkpoint before the process returns, so what the
    // server committed while running reaches `data` and the next `open` finds it without
    // replaying a journal. An in-memory instance answers `Ok(())` on `checkpoint` and has
    // nothing to flush; the call is made on the trait, which both implementations answer.
    if on_disk {
        storage
            .checkpoint()
            .context("cannot checkpoint the instance before exit")?;
    }
    info!("shutdown complete");
    Ok(())
}

/// Name announced to clients: the host name when the environment gives it, otherwise
/// [`FALLBACK_SERVER_NAME`].
fn server_name() -> String {
    std::env::var(HOSTNAME_ENV)
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| FALLBACK_SERVER_NAME.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_target_is_arch_and_os() {
        assert_eq!(
            build_target(),
            format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
        );
    }

    #[test]
    fn server_name_falls_back() {
        // Whatever the environment, the name is never empty.
        assert!(!server_name().is_empty());
    }
}
