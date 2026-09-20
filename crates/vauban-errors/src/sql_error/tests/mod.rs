use super::*;

#[test]
fn new_sets_line_zero_and_no_procedure() {
    let err = SqlError::new(208, 16, 1, "Invalid object name 'dbo.t'.");
    assert_eq!(
        err,
        SqlError {
            number: 208,
            severity: 16,
            state: 1,
            message: "Invalid object name 'dbo.t'.".to_string(),
            line: 0,
            procedure: None,
        }
    );
}

#[test]
fn with_line_and_with_procedure_chain() {
    let err = SqlError::new(208, 16, 1, "Invalid object name 'dbo.t'.")
        .with_line(3)
        .with_procedure("dbo.p");
    assert_eq!(err.line, 3);
    assert_eq!(err.procedure.as_deref(), Some("dbo.p"));
}

#[test]
fn batch_scope_is_catalogued_by_number() {
    assert_eq!(
        SqlError::divide_by_zero().batch_scope(),
        crate::BatchErrorScope::Statement
    );
    assert_eq!(
        SqlError::conversion_failed("varchar", "abc", "int").batch_scope(),
        crate::BatchErrorScope::Batch
    );
    assert_eq!(
        SqlError::new(50_000, 16, 1, "internal").batch_scope(),
        crate::BatchErrorScope::Batch
    );
}

#[test]
fn sql_error_display_uses_log_layout() {
    let err = SqlError::new(208, 16, 1, "Invalid object name 'dbo.t'.");
    assert_eq!(
        err.to_string(),
        "Msg 208, Level 16, State 1, Line 0\nInvalid object name 'dbo.t'."
    );
}

#[test]
fn sql_error_display_includes_procedure_when_set() {
    let err = SqlError::new(208, 16, 1, "Invalid object name 'dbo.t'.")
        .with_line(3)
        .with_procedure("dbo.p");
    assert_eq!(
        err.to_string(),
        "Msg 208, Level 16, State 1, Procedure dbo.p, Line 3\nInvalid object name 'dbo.t'."
    );
}

#[test]
fn sql_error_is_send_sync_static() {
    fn assert<T: Send + Sync + 'static>() {}
    assert::<SqlError>();
}

#[test]
fn sql_error_implements_std_error() {
    fn assert<T: std::error::Error>() {}
    assert::<SqlError>();
}
