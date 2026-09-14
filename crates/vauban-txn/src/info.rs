//! What the `sys.dm_tran_*` views will read: one [`TxnInfo`] per open transaction and one
//! [`LockInfo`] per lock held or waited for.
//!
//! This file fixes the **shape** of that information while the lock manager is the one
//! place that knows the holders, the queues and the conversions. It builds no view, no
//! row and no column: turning a `TxnInfo` or a `LockInfo` into a line of
//! `sys.dm_tran_locks` or `sys.dm_tran_active_transactions` belongs to the catalog.
//!
//! [`TransactionManager::active_sessions`] (`manager.rs`) fills the [`TxnInfo`]s from the
//! list of open transactions and from one [`TransactionManager::active_locks`], which reads
//! holders and waiters of the [`LockManager`](crate::LockManager) under one guard of its
//! table (`tests/info.rs`, `snapshot_of_locks_is_consistent`).
//!
//! What is **not** here: the session (`request_session_id`, `@@SPID`) that runs a
//! transaction. This crate does not know sessions; the session layer holds the handle and
//! makes that correspondence.

use std::fmt;
use std::time::SystemTime;

use vauban_storage::TxnId;

use crate::{IsolationLevel, LockMode, LockResource, TransactionManager};

/// One open transaction, as published by [`TransactionManager::active_sessions`].
///
/// Separate from [`TxnHandle`](crate::TxnHandle): a handle lets its owner act on the
/// transaction, a `TxnInfo` describes it without giving that power. `#[non_exhaustive]` so
/// that a later field does not break a caller that builds none.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TxnInfo {
    /// Identifier of the transaction.
    pub id: TxnId,
    /// Isolation level asked for when the transaction started.
    pub isolation: IsolationLevel,
    /// Whether the transaction runs or sits in the queue of a lock.
    pub state: TxnState,
    /// When [`TransactionManager::begin`] opened the transaction, on the wall clock.
    pub began_at: SystemTime,
    /// How many locks the transaction holds at the instant of the call: the lines of
    /// [`TransactionManager::active_locks`] at [`LockStatus::Grant`] in its name
    /// (`tests/info.rs`, `held_locks_are_reported_as_grant`).
    pub locks_held: u32,
    /// Its `DEADLOCK_PRIORITY`, `-10..=10`; `0` when
    /// [`TransactionManager::set_deadlock_priority`] was not called for it.
    pub deadlock_priority: i16,
}

/// What a transaction is doing at the instant [`TransactionManager::active_sessions`] looked.
///
/// A transaction that is committing does not appear here as a state of its own: `commit`
/// holds the list of open transactions while it runs, and a caller of `active_sessions`
/// sees the transaction either open or gone (`tests/txn_basic.rs`,
/// `a_refused_storage_commit_keeps_the_transaction_open`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TxnState {
    /// Running, in no lock queue.
    Active,
    /// Parked in the queue of one resource, holder or not of a weaker mode on it
    /// (`tests/info.rs`, `a_waiting_txn_is_visible`).
    Waiting {
        /// The resource whose queue the transaction sits in.
        on: LockResource,
    },
}

/// One line of the lock table: a lock held, or a request waiting for one, as published by
/// [`TransactionManager::active_locks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LockInfo {
    /// The transaction that holds the lock or asked for it.
    pub txn: TxnId,
    /// What the lock is on; [`LockResource::resource_kind`] names its kind.
    pub resource: LockResource,
    /// The mode held, or the mode asked for. For a conversion, the mode the transaction
    /// converts **to**.
    pub mode: LockMode,
    /// Held, waiting, or waiting to convert a mode already held.
    pub status: LockStatus,
    /// Milliseconds spent in the queue so far; `0` for a lock held.
    pub waiting_ms: u64,
}

/// The status of one [`LockInfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockStatus {
    /// The lock is held.
    Grant,
    /// The request waits in the queue of the resource.
    Wait,
    /// The request waits to convert a mode the transaction already holds on the resource
    /// into a stronger one of the same family.
    Convert,
}

impl LockMode {
    /// The short name of the mode: `S`, `X`, `U`, `IS`, `IX`, `Sch-S`, `Sch-M`, with the
    /// spelling of the lock modes table of the transaction locking and row versioning guide
    /// of the SQL Server documentation (`tests/info.rs`,
    /// `mode_strings_match_the_learn_names`). What `Display` writes.
    #[must_use]
    pub fn short_name(self) -> &'static str {
        match self {
            LockMode::S => "S",
            LockMode::X => "X",
            LockMode::U => "U",
            LockMode::IS => "IS",
            LockMode::IX => "IX",
            LockMode::SchS => "Sch-S",
            LockMode::SchM => "Sch-M",
        }
    }
}

impl fmt::Display for LockMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.short_name())
    }
}

impl fmt::Display for LockStatus {
    /// `GRANT`, `WAIT` or `CONVERT`, upper case.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LockStatus::Grant => "GRANT",
            LockStatus::Wait => "WAIT",
            LockStatus::Convert => "CONVERT",
        })
    }
}

impl LockResource {
    /// The kind of resource, as the `resource_type` of a lock names it: `ROW` for
    /// [`LockResource::Row`], `OBJECT` for [`LockResource::Table`]. The identifiers that
    /// describe the resource stay on the variant; no line is formatted here.
    #[must_use]
    pub fn resource_kind(self) -> &'static str {
        match self {
            LockResource::Row(_, _) => "ROW",
            LockResource::Table(_) => "OBJECT",
        }
    }
}

impl TransactionManager {
    /// The locks held and the requests waiting, resource by resource in the order of
    /// [`LockManager::held`](crate::LockManager::held): the holders of a resource, then its
    /// queue in grant order.
    ///
    /// Holders and waiters are read under one guard of the lock table, so one call cannot
    /// show a resource with a holder its queue already replaced or the reverse
    /// (`tests/info.rs`, `snapshot_of_locks_is_consistent`). The guard is held for the copy
    /// and released before the call returns; a thread parked on the lock manager is not
    /// kept waiting for longer than that copy.
    #[must_use]
    pub fn active_locks(&self) -> Vec<LockInfo> {
        self.locks().lines()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_storage::{RowId, TableId};

    /// `Display` on a status writes the upper-case word of the three.
    #[test]
    fn status_strings_are_upper_case() {
        assert_eq!(LockStatus::Grant.to_string(), "GRANT");
        assert_eq!(LockStatus::Wait.to_string(), "WAIT");
        assert_eq!(LockStatus::Convert.to_string(), "CONVERT");
    }

    /// `Display` on a mode writes what `short_name` returns, for the seven modes.
    #[test]
    fn display_of_a_mode_is_its_short_name() {
        for mode in [
            LockMode::S,
            LockMode::X,
            LockMode::U,
            LockMode::IS,
            LockMode::IX,
            LockMode::SchS,
            LockMode::SchM,
        ] {
            assert_eq!(mode.to_string(), mode.short_name());
        }
    }

    /// A row is a `ROW`, a table is an `OBJECT`.
    #[test]
    fn resource_kind_tells_row_from_table() {
        assert_eq!(
            LockResource::Row(TableId(1), RowId(2)).resource_kind(),
            "ROW"
        );
        assert_eq!(LockResource::Table(TableId(1)).resource_kind(), "OBJECT");
    }
}
