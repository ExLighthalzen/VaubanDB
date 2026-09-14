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
    /// Line in the batch that produced the message, 1-based; `0` when unknown.
    pub line: u32,
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
mod tests {
    use super::*;

    #[test]
    fn info_message_is_send_sync_static() {
        fn assert<T: Send + Sync + 'static>() {}
        assert::<InfoMessage>();
    }

    #[test]
    fn info_message_supports_equality_and_clone() {
        let msg = InfoMessage {
            number: 0,
            severity: 0,
            state: 1,
            message: "hello".to_string(),
            line: 1,
        };
        assert_eq!(msg.clone(), msg);
    }

    #[test]
    fn info_null_eliminated_is_8153() {
        let msg = InfoMessage::null_eliminated();
        assert_eq!(msg.number, 8153);
        assert_eq!(msg.severity, 10);
        assert_eq!(msg.state, 1);
        assert_eq!(msg.line, 0);
        assert_eq!(
            msg.message,
            "Warning: an aggregate or SET operation ignored a NULL value."
        );
    }

    #[test]
    fn info_null_eliminated_stays_informational() {
        assert!(InfoMessage::null_eliminated().severity <= 10);
    }
}
