//! The handle a caller holds on an open transaction, and the row `sys.dm_tran_*` reads.

use crate::IsolationLevel;
use vauban_storage::TxnId;

/// An open transaction, as seen by the caller.
///
/// Handed out by [`TransactionManager::begin`](crate::TransactionManager::begin), the only
/// way to mint a new one — the derived `Clone` copies an existing handle, it does not open a
/// transaction. `id` and `isolation` are public because the executor and `session` read
/// them.
///
/// The handle carries no state of its own: [`TransactionManager`](crate::TransactionManager)
/// holds the counter, the list of open transactions and the storage, and the handle names the
/// transaction it belongs to, and the savepoint stack lives in the manager too. The type
/// stays `#[non_exhaustive]` so that a later field, public or `pub(crate)`, does not break a
/// caller.
///
/// A handle outlives the transaction it names: after
/// [`TransactionManager::commit`](crate::TransactionManager::commit) the manager no longer
/// holds the transaction open, and a second `commit` of the same handle — or of a clone of it
/// — fails with an internal bug (`tests/txn_basic.rs`, `commit_twice_is_a_bug`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TxnHandle {
    /// Identifier of the transaction, assigned by
    /// [`TransactionManager::begin`](crate::TransactionManager::begin).
    pub id: TxnId,
    /// Isolation level asked for when the transaction started.
    pub isolation: IsolationLevel,
}

/// One open transaction, as published to the `sys.dm_tran_*` views by
/// [`TransactionManager::active_sessions`](crate::TransactionManager::active_sessions).
///
/// Separate from [`TxnHandle`]: a handle lets its owner act on the transaction, a `TxnInfo`
/// describes it without giving that power. `#[non_exhaustive]` for the same reason as
/// [`TxnHandle`]: the columns those views need (start time, state, session) are added when
/// the views are served.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TxnInfo {
    /// Identifier of the transaction.
    pub id: TxnId,
    /// Isolation level asked for when the transaction started.
    pub isolation: IsolationLevel,
}

impl TxnInfo {
    /// The description of the transaction named by `handle`.
    pub(crate) fn of(handle: &TxnHandle) -> Self {
        Self {
            id: handle.id,
            isolation: handle.isolation,
        }
    }
}
