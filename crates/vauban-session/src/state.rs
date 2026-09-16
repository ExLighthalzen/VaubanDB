//! `SessionState`: SPID, current database, `SET` options, isolation level, transaction
//! descriptor and the batch-scoped functions (`@@ROWCOUNT`, `@@ERROR`, `@@TRANCOUNT`).

use crate::login::{EDITION, VERSION_BANNER};
use crate::set_options::{IsolationLevel, SetOptions};
use crate::txn_session::SessionTxn;

/// Per-connection state, built by `server.rs` once the login is accepted:
/// `SessionState::new(spid)` then the fields of the LOGIN7.
///
/// Not held here yet: the batch variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    /// `@@SPID`, also written in every packet header ([MS-TDS] 2.2.3.1.4).
    pub spid: i16,
    /// Current database.
    pub database: String,
    /// Login name as accepted by the `Authenticator`.
    pub login: String,
    /// `AppName` of the LOGIN7 ([MS-TDS] 2.2.6.4).
    pub app_name: String,
    /// `HostName` of the LOGIN7 ([MS-TDS] 2.2.6.4).
    pub hostname: String,
    /// Packet size negotiated at login, in force after the login response.
    pub packet_size: u16,
    /// `SET` options of the session, at their client-connection defaults until a `SET`
    /// changes them.
    pub options: SetOptions,
    /// `SET TRANSACTION ISOLATION LEVEL`, `READ COMMITTED` by default.
    pub isolation: IsolationLevel,
    /// `@@ROWCOUNT`: rows produced or affected by the **previous** statement of the
    /// session, `0` before the first one. `batch.rs` writes it after each statement and
    /// `SessionEvalContext` reads it (`eval_context.rs`); the count is kept in an `i64`
    /// while `@@ROWCOUNT` is an `int`, as `sysfn` documents.
    pub rowcount: i64,
    /// `@@ERROR`: number of the error the last statement raised, `0` when it succeeded.
    ///
    /// `batch.rs` sets it on the error that stops a batch; putting it back to `0` after a
    /// statement that succeeds is not implemented (it comes with `TRY ... CATCH`).
    pub last_error: u32,
    /// `@@TRANCOUNT`: how many `BEGIN TRANSACTION` are open. Kept in step with
    /// [`SessionState::txn`] by `txn_session.rs`; a driver TRANSACTION_MANAGER request still
    /// writes it directly (`txn_request.rs`).
    pub trancount: i32,
    /// The storage transaction a `BEGIN TRANSACTION` opened, held across the statements of a
    /// batch and the batches that follow, `None` outside one. The session's authoritative
    /// transaction; [`SessionState::trancount`] and the wire are read from it by
    /// `txn_session.rs`.
    pub(crate) txn: Option<SessionTxn>,
    /// Opaque descriptor handed to the client for its current transaction, or `0` when
    /// no TRANSACTION_MANAGER transaction is open ([MS-TDS] 2.2.5.3.1).
    pub transaction_descriptor: u64,
    /// `@@VERSION` for this instance.
    pub version_banner: String,
    /// `SERVERPROPERTY('Edition')` for this instance.
    pub edition: String,
    /// Name this instance answers to `@@SERVERNAME` and `SERVERPROPERTY('ServerName')`,
    /// written by `server.rs` from `ServerConfig::server_name` at login.
    pub server_name: String,
    /// Next descriptor to allocate. `0` means the `u64` space was exhausted.
    next_transaction_descriptor: u64,
}

impl SessionState {
    /// Database a session starts in when the LOGIN7 names no database.
    const DEFAULT_DATABASE: &str = "master";
    /// Packet size before any negotiation ([MS-TDS] 2.2.6.4, `PacketSize`).
    const DEFAULT_PACKET_SIZE: u16 = 4096;
    /// Server name of a state built outside a login, when no `ServerConfig` has been read
    /// yet. The value the binary falls back to when the host name is unreadable
    /// (`crates/vauban-cli/src/main.rs`, `FALLBACK_SERVER_NAME`), so a session that
    /// `server.rs` did fill answers the configured name and one built by hand in a test
    /// answers this one (unit test `new_has_the_documented_defaults`).
    const DEFAULT_SERVER_NAME: &str = "vauban";

    /// A fresh state for `spid`: database `master`, default `SET` options and isolation
    /// level, empty `login`/`app_name`/`hostname`, packet size 4096. Enough on its own for
    /// the unit tests; `server.rs` fills the login fields after it.
    pub fn new(spid: i16) -> Self {
        Self {
            spid,
            database: Self::DEFAULT_DATABASE.into(),
            login: String::new(),
            app_name: String::new(),
            hostname: String::new(),
            packet_size: Self::DEFAULT_PACKET_SIZE,
            options: SetOptions::default(),
            isolation: IsolationLevel::default(),
            rowcount: 0,
            last_error: 0,
            trancount: 0,
            txn: None,
            transaction_descriptor: 0,
            version_banner: VERSION_BANNER.into(),
            edition: EDITION.into(),
            server_name: Self::DEFAULT_SERVER_NAME.into(),
            next_transaction_descriptor: 1,
        }
    }

    /// Allocates a connection-local, non-zero transaction descriptor. Exhausting all
    /// `u64` values is reported by `None` instead of reusing a descriptor.
    pub(crate) fn allocate_transaction_descriptor(&mut self) -> Option<u64> {
        let descriptor = self.next_transaction_descriptor;
        if descriptor == 0 {
            return None;
        }
        self.next_transaction_descriptor = descriptor.checked_add(1).unwrap_or(0);
        Some(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_the_documented_defaults() {
        let state = SessionState::new(51);
        assert_eq!(state.spid, 51);
        assert_eq!(state.database, "master");
        assert!(state.login.is_empty());
        assert!(state.app_name.is_empty());
        assert!(state.hostname.is_empty());
        assert_eq!(state.packet_size, 4096);
        assert_eq!(state.options, SetOptions::default());
        assert_eq!(state.isolation, IsolationLevel::ReadCommitted);
        assert_eq!(state.rowcount, 0);
        assert_eq!(state.last_error, 0);
        assert_eq!(state.trancount, 0);
        assert_eq!(state.transaction_descriptor, 0);
        assert_eq!(state.version_banner, VERSION_BANNER);
        assert_eq!(state.edition, EDITION);
        assert_eq!(state.server_name, "vauban");
    }

    #[test]
    fn transaction_descriptors_are_non_zero_and_never_reused() {
        let mut state = SessionState::new(51);
        assert_eq!(state.allocate_transaction_descriptor(), Some(1));
        assert_eq!(state.allocate_transaction_descriptor(), Some(2));

        state.next_transaction_descriptor = u64::MAX;
        assert_eq!(state.allocate_transaction_descriptor(), Some(u64::MAX));
        assert_eq!(state.allocate_transaction_descriptor(), None);
    }
}
