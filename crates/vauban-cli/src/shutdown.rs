//! Graceful shutdown: a `CancellationToken` cancelled on SIGINT or SIGTERM. A second
//! signal while the server drains its connections ends the process at once with exit
//! code 130 (the conventional "terminated by SIGINT" code).

use std::io;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Exit code when a second signal cuts the shutdown short.
pub(crate) const SECOND_SIGNAL_EXIT_CODE: i32 = 130;

/// Registers the signal handlers and returns the token they cancel.
///
/// Must be called inside a tokio runtime: the handlers are registered synchronously
/// (a signal that arrives right after this call is not lost), the wait runs in a task.
pub(crate) fn install() -> io::Result<CancellationToken> {
    let mut signals = Signals::new()?;
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        let first = signals.recv().await;
        info!(signal = first, "shutdown signal received");
        cancel.cancel();
        let second = signals.recv().await;
        warn!(
            signal = second,
            "second signal received, exiting immediately"
        );
        std::process::exit(SECOND_SIGNAL_EXIT_CODE);
    });
    Ok(token)
}

/// The two streams of signals the server listens to.
#[cfg(unix)]
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Waits for the next SIGINT or SIGTERM and names it for the log.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

/// Outside Unix only Ctrl+C is available.
#[cfg(not(unix))]
struct Signals;

#[cfg(not(unix))]
impl Signals {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> &'static str {
        // An error here means the handler could not be registered; there is nothing
        // better to do than to treat it as a request to stop.
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl+C"
    }
}
