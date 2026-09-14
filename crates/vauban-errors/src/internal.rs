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
mod tests {
    use super::*;

    #[test]
    fn internal_error_converts_to_50000_severity_16() {
        let err = SqlError::from(InternalError::Bug("x".into()));
        assert_eq!(err.number, 50000);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
        assert!(err.message.starts_with("Internal error: "));
        assert_eq!(err.message, "Internal error: internal bug: x");
    }

    #[test]
    fn io_error_converts_via_from() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err: InternalError = io.into();
        assert!(matches!(err, InternalError::Io(_)));
        assert_eq!(err.to_string(), "I/O error: missing");
    }

    #[test]
    fn display_matches_thiserror_attributes() {
        assert_eq!(
            InternalError::Corruption("page 7".into()).to_string(),
            "data corruption: page 7"
        );
        assert_eq!(
            InternalError::Bug("x".into()).to_string(),
            "internal bug: x"
        );
    }

    #[test]
    fn io_source_is_exposed_through_std_error() {
        use std::error::Error;
        let err = InternalError::from(std::io::Error::other("disk"));
        assert!(err.source().is_some());
    }

    #[test]
    fn internal_error_is_send_sync_static() {
        fn assert<T: Send + Sync + 'static>() {}
        assert::<InternalError>();
    }
}
