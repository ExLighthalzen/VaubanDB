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
