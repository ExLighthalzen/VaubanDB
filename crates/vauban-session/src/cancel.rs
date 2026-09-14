//! ATTENTION handling: `CancelHandle`, cancellation of the running request, DONE with ATTN.
//! The engine polls the handle between two units of work; the connection
//! task raises it when an ATTENTION arrives ([MS-TDS] 2.2.1.7) and answers the client with
//! a lone DONE carrying `DoneStatus::ATTN`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use vauban_errors::{InternalError, SqlError, SqlResult};

/// A shared flag that asks the running request to stop. Cloning gives another handle on
/// the same flag.
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    cancelled: Arc<AtomicBool>,
}

impl CancelHandle {
    /// A handle whose request is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the running request to stop at its next check.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// `true` once [`cancel`](Self::cancel) was called on any clone of this handle.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Lowers the flag. The connection task calls it before each request: a handle lives
    /// as long as its `Session`, one ATTENTION must not cancel the requests that follow.
    pub(crate) fn reset(&self) {
        self.cancelled.store(false, Ordering::Release);
    }

    /// `Ok(())` while the request may go on, an internal "cancelled" error once the flag
    /// is up. Called between two statements and inside every long loop.
    ///
    /// This error never reaches the client: it only unwinds the blocking task, and the
    /// connection task ignores the result of a cancelled request to send the DONE `ATTN`
    /// instead (`server.rs`).
    pub(crate) fn check(&self) -> SqlResult<()> {
        if self.is_cancelled() {
            // `InternalError` has no `Cancelled` variant: `Bug` carries the text of an
            // error that is discarded before the wire.
            Err(SqlError::from(InternalError::Bug(CANCELLED.to_owned())))
        } else {
            Ok(())
        }
    }
}

/// Text of the internal error [`CancelHandle::check`] raises; logged, never sent.
const CANCELLED: &str = "request cancelled by an ATTENTION";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_the_flag() {
        let handle = CancelHandle::new();
        let clone = handle.clone();
        assert!(!handle.is_cancelled());
        clone.cancel();
        assert!(handle.is_cancelled());
        assert!(clone.is_cancelled());
    }

    #[test]
    fn check_fails_once_cancelled_and_passes_again_after_reset() {
        let handle = CancelHandle::new();
        assert!(handle.check().is_ok());
        handle.cancel();
        let err = handle
            .check()
            .expect_err("a cancelled handle fails its check");
        assert_eq!(err.number, 50000);
        assert!(err.message.contains(CANCELLED), "{}", err.message);
        handle.reset();
        assert!(!handle.is_cancelled());
        assert!(handle.check().is_ok());
    }

    #[test]
    fn reset_reaches_every_clone() {
        let handle = CancelHandle::new();
        let clone = handle.clone();
        clone.cancel();
        handle.reset();
        assert!(!clone.is_cancelled());
    }
}
