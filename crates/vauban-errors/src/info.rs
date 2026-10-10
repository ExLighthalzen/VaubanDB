//! Informational messages (severity `0..=10`) sent to clients.

use crate::format::from_catalog_info;

/// A low-severity message sent to a client without failing the batch: `PRINT` output,
/// textual ENVCHANGE notices, `RAISERROR` with severity `<= 10`.
///
/// The module `tds` encodes it in the INFO token (`[MS-TDS]` 2.2.7.12), whose `Class`
/// field carries [`severity`](Self::severity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoMessage {
    /// Message number (e.g. `0` for `PRINT`, `5701` for a database context change).
    pub number: u32,
    /// Severity, `0..=10`. Anything above `10` is an error and must be a
    /// [`SqlError`](crate::SqlError) instead.
    pub severity: u8,
    /// State, `1..=255`.
    pub state: u8,
    /// Final message text, in English, with all arguments already substituted.
    pub message: String,
    /// Line that produced the message, 1-based; `0` when unknown. Inside a procedure, the
    /// line counts from the start of the procedure's definition, as for
    /// [`SqlError::line`](crate::SqlError::line).
    pub line: u32,
    /// Name of the procedure that sent the message, `None` outside a procedure, including
    /// in a dynamic batch run by `EXEC('...')` or `sp_executesql`. The INFO token carries
    /// it in its `ProcName` field, empty for `None`.
    pub procedure: Option<String>,
}

impl InfoMessage {
    /// Message 8153, severity 10, state 1: an aggregate skipped at least one `NULL`.
    ///
    /// Severity comes from the catalogue, 10, not 0. Emitted while `ANSI_WARNINGS` is
    /// `ON`:
    /// `SELECT SUM(c) FROM (VALUES (1),(NULL)) v(c);` returns the row *and* the warning.
    ///
    /// ```text
    /// Warning: an aggregate or SET operation ignored a NULL value.
    /// ```
    pub fn null_eliminated() -> Self {
        from_catalog_info(8153, 1, &[])
    }
}

#[cfg(test)]
mod tests;
