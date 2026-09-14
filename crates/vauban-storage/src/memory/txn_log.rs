//! The transaction registry of `MemoryStorage`: the status of every transaction seen so far,
//! the undo log used by `rollback` and `rollback_to`, and the savepoints of each transaction.

use crate::{RowId, SavepointId, TableId, TxnStatus};

/// One write logged for a transaction, to be undone in reverse order on rollback.
///
/// An entry names the logical row but never a position in its version chain: `vacuum` may
/// remove older versions of that row while the transaction is still in progress (their
/// `xmax` belongs to a committed transaction below the horizon), which would shift any
/// stored index. Undoing in reverse order is enough to locate the versions without an
/// index: while a transaction is in progress, its writes on a row are the **tail** of the
/// row's chain (`update`/`delete` refuse a row whose latest version carries an `xmax` or was
/// created by another transaction still in progress, and `vacuum` never touches a version
/// whose `xmax` is in progress), so each undo acts on the last version of the row and
/// leaves the chain as it was before the write.
///
/// Index entries are not logged: an entry names a version by `(RowId, seq)`, so undoing an
/// `Insert` or an `Update` removes the entries of the versions it takes away from every
/// index the table still has (an index dropped since the write is ignored silently), and
/// undoing a `Delete` touches no index, since `delete` added no entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UndoEntry {
    /// A logical row created by the transaction; undoing removes the whole row.
    Insert {
        /// The table the row was inserted into.
        table: TableId,
        /// The row that was created.
        row: RowId,
    },
    /// A version replaced by the transaction (`xmax` set on it) followed by a new version it
    /// created; undoing removes the new version and resets the `xmax` of the replaced one.
    Update {
        /// The table the row belongs to.
        table: TableId,
        /// The logical row that was updated.
        row: RowId,
    },
    /// A version deleted by the transaction (`xmax` set on it); undoing resets that `xmax`.
    Delete {
        /// The table the row belongs to.
        table: TableId,
        /// The logical row that was deleted.
        row: RowId,
    },
}

/// What the storage knows about one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TxnState {
    /// `InProgress` from the first write until `commit` or `rollback`.
    pub(crate) status: TxnStatus,
    /// The writes of the transaction in chronological order, emptied by `commit`, consumed
    /// by `rollback` and truncated by `rollback_to`. Always empty once the transaction is
    /// finished.
    pub(crate) writes: Vec<UndoEntry>,
    /// The savepoints of the transaction in creation order, each with the length `writes`
    /// had when it was taken. `rollback_to` truncates the list after the target
    /// savepoint. Always empty once the transaction is finished.
    pub(crate) savepoints: Vec<(SavepointId, usize)>,
}

impl TxnState {
    /// A freshly discovered transaction: in progress, nothing written yet.
    pub(crate) fn in_progress() -> Self {
        Self {
            status: TxnStatus::InProgress,
            writes: Vec::new(),
            savepoints: Vec::new(),
        }
    }

    /// A transaction that finished without the storage ever seeing a write from it.
    pub(crate) fn finished(status: TxnStatus) -> Self {
        Self {
            status,
            writes: Vec::new(),
            savepoints: Vec::new(),
        }
    }

    /// Whether the transaction has committed or rolled back.
    pub(crate) fn is_finished(&self) -> bool {
        self.status != TxnStatus::InProgress
    }
}
