//! Deferred commit and compensation actions.
//!
//! DDL is transactional at the SQL level, but `create_*` and `drop_*` of
//! [`Storage`] take effect at once, outside any transaction (documentation of the
//! trait). The protocol this file carries has two halves:
//!
//! - a `DROP` is **deferred**: the caller registers a [`CommitAction`] instead of calling
//!   `storage.drop_*`, and the manager runs it at commit time;
//! - a `CREATE` is **compensated**: the caller calls `storage.create_*` at once and registers
//!   the [`RollbackAction`] that undoes it, which the manager runs at rollback time.
//!
//! [`ActionLog`] holds both lists of one transaction in a single vector, with the savepoints
//! as marks inside it, so that a `ROLLBACK TRANSACTION <name>` compensates what was created
//! after the savepoint and forgets what was registered after it
//! (`tests/txn_actions.rs`, `savepoint_undoes_later_creates`).
//!
//! # Order
//!
//! Commit actions run in registration order and rollback actions in reverse registration
//! order — the order that undoes a creation before the object it was created in. Dropping a
//! table drops its indexes (documentation of [`Storage::drop_table`]), so an index dropped
//! after its table is an `InternalError::Bug`: that is what tells the two orders apart in
//! `tests/txn_actions.rs` (`commit_actions_run_in_registration_order`,
//! `rollback_actions_run_in_reverse_order`).

use vauban_errors::SqlResult;
use vauban_storage::{DbId, IndexId, SavepointId, Storage, TableId};

/// An action a transaction defers to its `COMMIT`, so that the object it names stays readable
/// until then.
///
/// Registered with
/// [`TransactionManager::register_on_commit`](crate::TransactionManager::register_on_commit)
/// in place of the `storage.drop_*` call, which the manager makes at commit time
/// (`tests/txn_actions.rs`, `drop_deferred_until_commit`). A transaction that rolls back does
/// not run them, so the object stays (`tests/txn_actions.rs`,
/// `a_rolled_back_deferred_drop_keeps_the_table`).
///
/// The three variants are the three droppable objects of [`Storage`].
/// [`CommitAction::DropDatabase`] serves the bootstrap and the unit tests: `DROP DATABASE`
/// inside a user transaction is refused by the executor with error 226, so the catalogue
/// registers no database action in that case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommitAction {
    /// Call [`Storage::drop_table`] for this table.
    DropTable(TableId),
    /// Call [`Storage::drop_index`] for this index.
    DropIndex(IndexId),
    /// Call [`Storage::drop_database`] for this database.
    DropDatabase(DbId),
}

/// An action that compensates, at `ROLLBACK`, an object created immediately in `storage`.
///
/// Registered with
/// [`TransactionManager::register_on_rollback`](crate::TransactionManager::register_on_rollback)
/// right after the `storage.create_*` that it undoes. The manager runs it when the
/// transaction rolls back, wholly or down to a savepoint taken before the registration
/// (`tests/txn_actions.rs`, `create_compensated_on_rollback`,
/// `savepoint_undoes_later_creates`). A transaction that commits does not run them
/// (`tests/txn_actions.rs`, `a_committed_compensation_leaves_the_table`).
///
/// Same three variants as [`CommitAction`], for the same objects; the two enumerations are
/// kept apart so that a caller cannot register a deferred drop where a compensation was
/// meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RollbackAction {
    /// Call [`Storage::drop_table`] to undo a `create_table`.
    DropTable(TableId),
    /// Call [`Storage::drop_index`] to undo a `create_index`.
    DropIndex(IndexId),
    /// Call [`Storage::drop_database`] to undo a `create_database`.
    DropDatabase(DbId),
}

impl CommitAction {
    /// Runs the drop this action names.
    ///
    /// # Errors
    ///
    /// The error of the `storage.drop_*` called, an `InternalError::Bug` when the identifier
    /// is unknown — the precondition of the action is that the object still exists
    /// (`tests/txn_actions.rs`, `a_deferred_drop_of_a_gone_table_is_a_bug`).
    pub(crate) fn run(self, storage: &dyn Storage) -> SqlResult<()> {
        match self {
            CommitAction::DropTable(table) => storage.drop_table(table),
            CommitAction::DropIndex(index) => storage.drop_index(index),
            CommitAction::DropDatabase(db) => storage.drop_database(db),
        }
    }
}

impl RollbackAction {
    /// Runs the drop this action names.
    ///
    /// # Errors
    ///
    /// The error of the `storage.drop_*` called, an `InternalError::Bug` when the identifier
    /// is unknown.
    pub(crate) fn run(self, storage: &dyn Storage) -> SqlResult<()> {
        match self {
            RollbackAction::DropTable(table) => storage.drop_table(table),
            RollbackAction::DropIndex(index) => storage.drop_index(index),
            RollbackAction::DropDatabase(db) => storage.drop_database(db),
        }
    }
}

/// One entry of an [`ActionLog`], in registration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// Run at commit time, in registration order.
    OnCommit(CommitAction),
    /// Run at rollback time, in reverse registration order.
    OnRollback(RollbackAction),
    /// A savepoint of the transaction: the boundary [`ActionLog::unwind_after`] cuts at.
    Mark(SavepointId),
}

/// The actions registered by one transaction, with its savepoints as marks.
///
/// A single vector holds the two kinds and the marks, which is what keeps them ordered
/// relative to one another: a savepoint splits the log at the position it occupies, so
/// [`ActionLog::unwind_after`] knows which registrations came after it
/// (`tests/txn_actions.rs`, `nested_savepoints_are_ordered`).
#[derive(Debug, Default)]
pub(crate) struct ActionLog {
    entries: Vec<Entry>,
}

impl ActionLog {
    /// Adds an action to run at commit time.
    pub(crate) fn push_on_commit(&mut self, action: CommitAction) {
        self.entries.push(Entry::OnCommit(action));
    }

    /// Adds an action to run at rollback time.
    pub(crate) fn push_on_rollback(&mut self, action: RollbackAction) {
        self.entries.push(Entry::OnRollback(action));
    }

    /// Marks the savepoint `sp` at the current end of the log.
    pub(crate) fn mark(&mut self, sp: SavepointId) {
        self.entries.push(Entry::Mark(sp));
    }

    /// Position of the mark of `sp`, or `None` when the log holds no such savepoint — a
    /// savepoint of another transaction, one made up by the caller, or one invalidated by an
    /// earlier [`ActionLog::unwind_after`] (unit test `an_unknown_savepoint_has_no_position`).
    pub(crate) fn position_of(&self, sp: SavepointId) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| *entry == Entry::Mark(sp))
    }

    /// Runs the commit actions, in registration order.
    ///
    /// # Errors
    ///
    /// The error of the first action that fails; the actions before it have run, those after
    /// it have not, and the log is left as it was.
    pub(crate) fn run_on_commit(&self, storage: &dyn Storage) -> SqlResult<()> {
        for entry in &self.entries {
            if let Entry::OnCommit(action) = entry {
                action.run(storage)?;
            }
        }
        Ok(())
    }

    /// Runs the compensations of the whole log, in reverse registration order.
    ///
    /// # Errors
    ///
    /// The error of the first compensation that fails, with the same partial effect as
    /// [`ActionLog::run_on_commit`].
    pub(crate) fn run_on_rollback(&self, storage: &dyn Storage) -> SqlResult<()> {
        Self::compensate(&self.entries, storage)
    }

    /// Runs the compensations registered after position `at`, in reverse registration order,
    /// then forgets the entries after `at` — commit actions included, so a deferred drop
    /// registered after the savepoint is not run by a later commit (`tests/txn_actions.rs`,
    /// `a_savepoint_forgets_the_deferred_drops_registered_after_it`).
    ///
    /// The entry at `at` is the mark of the savepoint itself and stays in the log: the same
    /// savepoint can be rolled back to again, as [`Storage::rollback_to`] allows (unit test
    /// `unwinding_keeps_its_own_mark`).
    ///
    /// # Errors
    ///
    /// The error of the first compensation that fails, in which case nothing is forgotten.
    pub(crate) fn unwind_after(&mut self, at: usize, storage: &dyn Storage) -> SqlResult<()> {
        Self::compensate(&self.entries[at + 1..], storage)?;
        self.entries.truncate(at + 1);
        Ok(())
    }

    /// Runs the [`Entry::OnRollback`] of `entries`, last registered first.
    fn compensate(entries: &[Entry], storage: &dyn Storage) -> SqlResult<()> {
        for entry in entries.iter().rev() {
            if let Entry::OnRollback(action) = entry {
                action.run(storage)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_storage::MemoryStorage;

    /// A log that never saw `sp` reports no position: this is what turns an invalid savepoint
    /// into a caller bug in
    /// [`TransactionManager::rollback_to`](crate::TransactionManager::rollback_to).
    #[test]
    fn an_unknown_savepoint_has_no_position() {
        let mut log = ActionLog::default();
        assert_eq!(log.position_of(SavepointId(1)), None);
        log.push_on_commit(CommitAction::DropTable(TableId(7)));
        log.mark(SavepointId(1));
        assert_eq!(log.position_of(SavepointId(1)), Some(1));
        assert_eq!(log.position_of(SavepointId(2)), None);
    }

    /// `unwind_after` keeps the mark it cuts at, so the same savepoint can be rolled back to
    /// twice, and forgets what follows it.
    #[test]
    fn unwinding_keeps_its_own_mark() {
        let storage = MemoryStorage::new();
        let mut log = ActionLog::default();
        log.mark(SavepointId(1));
        log.push_on_commit(CommitAction::DropTable(TableId(7)));
        log.mark(SavepointId(2));
        log.unwind_after(0, &storage)
            .expect("no compensation to run");
        assert_eq!(log.position_of(SavepointId(1)), Some(0));
        assert_eq!(log.position_of(SavepointId(2)), None, "invalidated");
        assert_eq!(log.entries.len(), 1, "the deferred drop is forgotten");
        log.unwind_after(0, &storage).expect("the same mark twice");
        assert_eq!(log.position_of(SavepointId(1)), Some(0));
    }
}
