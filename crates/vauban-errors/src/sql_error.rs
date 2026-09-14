//! The error type returned to SQL Server clients.

use std::fmt;

/// An error destined to a SQL Server client.
///
/// Carries the four attributes every SQL Server error has (number, severity, state,
/// message) plus the position information the client displays (line, procedure). The
/// module `tds` encodes these fields as-is in the ERROR token (`[MS-TDS]` 2.2.7.10);
/// this type formats nothing for the wire.
///
/// Fields are public on purpose: other crates build `SqlError`s by struct literal in
/// their tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    /// Error number (e.g. `2627` for a unique-key violation). Numbers below 50000 are
    /// reserved to SQL Server's own catalogue;
    /// `50000` is the generic user/internal error.
    pub number: u32,
    /// Severity level, `0..=25`. Levels `0..=10` are informational and are normally
    /// carried by an [`InfoMessage`](crate::InfoMessage) instead; `11..=16` are user
    /// errors, `17..=19` are resource errors, `20..=25` are fatal and end the connection.
    pub severity: u8,
    /// State, `1..=255`: distinguishes the code paths that raise the same number.
    pub state: u8,
    /// Final message text, in English, with all arguments already substituted.
    pub message: String,
    /// Line in the batch that raised the error, 1-based; `0` when unknown.
    pub line: u32,
    /// Name of the stored procedure or trigger that raised the error, if any.
    pub procedure: Option<String>,
}

/// Result alias for every operation that can fail with a client-visible error.
pub type SqlResult<T> = Result<T, SqlError>;

impl SqlError {
    /// Builds an error with `line` set to `0` and no `procedure`.
    ///
    /// Use [`with_line`](Self::with_line) and [`with_procedure`](Self::with_procedure)
    /// to add position information.
    pub fn new(number: u32, severity: u8, state: u8, message: impl Into<String>) -> Self {
        Self {
            number,
            severity,
            state,
            message: message.into(),
            line: 0,
            procedure: None,
        }
    }

    /// Sets the 1-based line in the batch that raised the error.
    #[must_use]
    pub fn with_line(mut self, line: u32) -> Self {
        self.line = line;
        self
    }

    /// Sets the name of the stored procedure or trigger that raised the error.
    #[must_use]
    pub fn with_procedure(mut self, name: impl Into<String>) -> Self {
        self.procedure = Some(name.into());
        self
    }

    /// What this error stops when it is raised during statement execution.
    ///
    /// Errors absent from the SQL Server catalogue, including internal error 50000, default
    /// to stopping the batch. Binding and compilation errors are handled before execution and
    /// do not consult this attribute.
    pub fn batch_scope(&self) -> crate::BatchErrorScope {
        crate::message_template(self.number).map_or(crate::BatchErrorScope::Batch, |definition| {
            definition.batch_scope()
        })
    }
}

/// Log-oriented layout, the one command-line clients print, without the
/// `Server <name>,` segment some of them insert before `Line`:
///
/// ```text
/// Msg 208, Level 16, State 1, Line 0
/// Unknown object name 'dbo.t'.
/// ```
///
/// When `procedure` is set, `Procedure <name>,` is inserted before `Line`. This layout is
/// for internal logs only; clients receive the raw fields through `tds`.
impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Msg {}, Level {}, State {}, ",
            self.number, self.severity, self.state
        )?;
        if let Some(procedure) = &self.procedure {
            write!(f, "Procedure {procedure}, ")?;
        }
        write!(f, "Line {}\n{}", self.line, self.message)
    }
}

impl std::error::Error for SqlError {}

#[cfg(test)]
mod tests {
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
}
