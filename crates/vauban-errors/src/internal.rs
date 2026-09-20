//! Errors that must never reach a client as-is.

use crate::SqlError;

/// An error internal to the engine: a bug, a storage corruption or an I/O failure.
///
/// These have no SQL Server equivalent and carry no error number. They are converted
/// into a generic [`SqlError`] at the session boundary (see the `From` impl below).
///
/// Not `Clone` nor `PartialEq`: [`std::io::Error`] supports neither.
#[derive(Debug, thiserror::Error)]
pub enum InternalError {
    /// An I/O failure from the operating system (file, socket).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// On-disk or in-memory data that violates a storage invariant.
    #[error("data corruption: {0}")]
    Corruption(String),
    /// A programming error: a state the code assumed impossible.
    #[error("internal bug: {0}")]
    Bug(String),
}

/// Converts an internal error into the generic client-visible error.
///
/// Number `50000`, severity `16`, state `1`, line `0`, no procedure, and message
/// `Internal error: <Display of the InternalError>`. SQL Server has no single equivalent
/// for an internal engine failure, so this mapping is VaubanDB's own and may be revisited.
impl From<InternalError> for SqlError {
    fn from(err: InternalError) -> Self {
        SqlError::new(50000, 16, 1, format!("Internal error: {err}"))
    }
}

#[cfg(test)]
mod tests;
