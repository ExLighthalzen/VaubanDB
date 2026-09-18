//! Which lock mode a read takes, and until when it is held: the **policy** the isolation
//! level and the lock hints decide, over the mechanism of [`crate::lock`].
//!
//! [`crate::lock::LockManager`] knows modes, resources and queues and names no isolation
//! level (`lock.rs`); here a level and a hint choose the mode of a row read, the intent
//! mode of its table, and whether the lock is given back at the end of the row or at the
//! end of the transaction.
//!
//! # The policy
//!
//! `hints.level` is the per-table level of the `NOLOCK`, `READCOMMITTED`, `REPEATABLEREAD`
//! and `SERIALIZABLE` hints and stands in front of the level of the transaction;
//! `HOLDLOCK`, the synonym of the `SERIALIZABLE` hint, is read as that level when no other
//! hint named one ([`TransactionManager::effective_level`]).
//!
//! | effective level | row mode | given back by [`TransactionManager::end_row_read`] |
//! |---|---|---|
//! | `ReadUncommitted` | the read is [`ReadAccess::Dirty`], lock-free (`tests/isolation.rs`, `read_uncommitted_does_not_block`) | — |
//! | `ReadCommitted` | `S` | yes |
//! | `RepeatableRead` | `S` | no, at the end of the transaction |
//! | `Serializable` | `S` | no, at the end of the transaction |
//! | `Snapshot` | `S`, given back, where `ALLOW_SNAPSHOT_ISOLATION` is off | yes |
//!
//! The row of `ReadCommitted` holds where `READ_COMMITTED_SNAPSHOT` is off and the row of
//! `Snapshot` where `ALLOW_SNAPSHOT_ISOLATION` is off. Where the option is on, a read that
//! would take an `S` takes no lock at all and comes back [`ReadAccess::Versioned`]
//! (`snapshot_modes.rs`; `tests/snapshot_modes.rs`, `rcsi_on_does_not`,
//! `snapshot_txn_reads_take_no_lock`); a `U` or an `X` a hint asked for is taken as below
//! (`tests/snapshot_modes.rs`, `updlock_still_waits_under_rcsi`).
//!
//! `UPDLOCK` puts a `U` in place of the `S` and `XLOCK` an `X`, both held to the end of the
//! transaction whatever the level (`tests/isolation.rs`, `updlock_is_held_to_the_end`,
//! `xlock_blocks_a_reader`, `updlock_under_read_uncommitted_still_takes_the_u`). Before an
//! `S` on a row the table takes an `IS`, before a `U` or an `X` an `IX`, and those intent
//! locks are given back at the end of the transaction (`tests/isolation.rs`,
//! `intent_locks_are_taken`, `end_row_read_keeps_the_intent_lock`). `READPAST` turns a row
//! lock that cannot be had at once into [`ReadAccess::Skip`] instead of an error, and
//! `NOWAIT` asks for [`LockTimeout::NoWait`], which `deadlock.rs` reports as 1222.
//!
//! `TABLOCK` and `TABLOCKX` are carried by [`LockIntent`] and not applied here: this file
//! takes row locks and the intent locks of the hierarchy, and a caller that sets those two
//! fields gets the locks a caller that leaves them at `false` gets
//! (`tests/isolation.rs`, `tablock_fields_are_carried_not_applied`).
//!
//! # What SQL Server does
//!
//! The policy above follows SQL Server on these shapes, with two connections A and B on a
//! table of three rows:
//!
//! | scenario | what SQL Server does |
//! |---|---|
//! | A `BEGIN TRAN; UPDATE … WHERE id=1`, B `READ COMMITTED` `SELECT v … WHERE id=1` | B waits for the `COMMIT` of A, then reads the committed value |
//! | B `REPEATABLE READ` `SELECT v … WHERE id=2` then A `UPDATE … WHERE id=2` | A waits for the `COMMIT` of B |
//! | the same with B in `READ COMMITTED` | A goes through, B still open |
//! | B `READ COMMITTED` `SELECT … WITH (REPEATABLEREAD)`, then A `UPDATE` | A waits for the `COMMIT` of B |
//! | B `REPEATABLE READ` `SELECT … WITH (READCOMMITTED)`, then A `UPDATE` | A goes through, B still open |
//! | B `READ COMMITTED` `SELECT … WITH (HOLDLOCK)`, then A `UPDATE` | A waits for the `COMMIT` of B |
//! | B `READ COMMITTED`, plain `SELECT` of a row **then** the same row `WITH (REPEATABLEREAD)`, then A `UPDATE` | A waits for the `COMMIT` of B |
//! | B `REPEATABLE READ`, `SELECT … WITH (READCOMMITTED)` **then** a plain `SELECT` of the same row, then A `UPDATE` | A waits for the `COMMIT` of B |
//! | A `SELECT … WITH (UPDLOCK) WHERE id=3`, B the same | B waits; a plain `SELECT` of that row goes through |
//! | A `BEGIN TRAN; UPDATE … WHERE id=1`, B `SELECT … WITH (READPAST)` | B answers with rows 2 and 3, row 1 left out |
//! | the same with B under `WITH (NOLOCK)` | B answers with the uncommitted value |
//! | the same with B under `WITH (NOWAIT)` | 1222, state 51 |
//! | B `REPEATABLE READ` `SELECT … WITH (NOLOCK)` over the row A holds | B answers with the uncommitted value |
//! | `SELECT … WITH (NOLOCK, HOLDLOCK)` | 1047, conflicting locking hints |
//!
//! The two hint rows tell the per-table hint from the level of the transaction: the hint
//! decides in both directions, so neither level alone explains the pair. The two rows that
//! read one row **twice** answer the other question, which of the two reads decides: the
//! read that holds its `S` to the end of the transaction decides, whichever of the two came
//! first, and the writer waits for the `COMMIT` in both orders. `read_lock` answers them by
//! taking the entry of a releasing read of that row out when it takes a lock held to the
//! end (`tests/isolation.rs`, `a_held_read_overrules_an_earlier_released_read_of_the_same_row`,
//! `a_plain_repeatable_read_overrules_an_earlier_read_committed_hint`). The last row is why
//! [`TransactionManager::effective_level`] needs no rule for two level hints that disagree:
//! such a statement is refused before it reaches the manager, and refusing it is the
//! binder's work.
//!
//! `SERIALIZABLE` is a **deliberate difference from SQL Server**: there, an `INSERT` inside
//! the range a `SERIALIZABLE` transaction has read waits for it, while under
//! `REPEATABLE READ` the same `INSERT` goes through and a second `SELECT COUNT(*)` of the
//! reader counts the phantom row. Without range locks this file serves `Serializable` as it
//! serves `RepeatableRead`.
//!
//! # One row read, one thread
//!
//! [`TransactionManager::read_lock`] and [`TransactionManager::end_row_read`] bracket the
//! read of one row, and what the second gives back is what the first decided: the signature
//! of the second carries no [`LockIntent`], so the decision is kept between the two. It is
//! kept **per thread**, in `SHARED_UNTIL_END_OF_ROW`, because the engine of VaubanDB is
//! synchronous and one statement runs on one blocking thread — the pair belongs to the
//! thread that reads the row, which is how the executor calls it. A caller that splits the
//! pair over two threads gets a shared lock held to the end of the transaction instead of
//! to the end of the row read: it blocks a writer longer than SQL Server does in the table
//! above and it reads the same rows (`tests/isolation.rs`,
//! `a_row_read_split_over_two_threads_holds_its_lock`).

use std::cell::RefCell;
use std::collections::HashSet;

use vauban_errors::SqlResult;
use vauban_storage::{RowId, TableId, TxnId};

use crate::deadlock;
use crate::lock::{LockMode, LockOutcome, LockResource, LockWait};
use crate::{IsolationLevel, LockTimeout, TransactionManager, TxnHandle, VersioningMode};

/// What the plan asked for on one table, as the executor hands it to the manager.
///
/// The type lives in `txn` and not in `binder`: `vauban-txn` does not depend on
/// `vauban-binder`, and the executor translates the `LockHints` the binder attached to a
/// table reference into this structure. Nothing here parses a hint or names one in SQL.
///
/// [`LockIntent::default`] carries no hint: `level` is `None`, so the level of the
/// transaction decides, and the seven flags are `false`. The fields are public and the
/// structure is built by its literal, so a caller that sets one field writes
/// `LockIntent { updlock: true, ..LockIntent::default() }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockIntent {
    /// The level named by a `NOLOCK` / `READUNCOMMITTED`, `READCOMMITTED`,
    /// `REPEATABLEREAD` or `SERIALIZABLE` hint on this table, ahead of the level of the
    /// transaction.
    pub level: Option<IsolationLevel>,
    /// `HOLDLOCK`: read as the `SERIALIZABLE` hint when [`LockIntent::level`] is `None`.
    pub holdlock: bool,
    /// `UPDLOCK`: take a `U` in place of the `S`, held to the end of the transaction.
    pub updlock: bool,
    /// `XLOCK`: take an `X` in place of the `S`, held to the end of the transaction.
    pub xlock: bool,
    /// `READPAST`: a row lock that cannot be had at once gives [`ReadAccess::Skip`].
    pub readpast: bool,
    /// `NOWAIT`: do not wait for a lock ([`LockTimeout::NoWait`]), which `deadlock.rs`
    /// reports as 1222.
    pub nowait: bool,
    /// `TABLOCK`: carried here without effect.
    pub tablock: bool,
    /// `TABLOCKX`: carried here without effect.
    pub tablockx: bool,
}

/// What the caller may do with the row it asked to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadAccess {
    /// The row is locked for this read: read the version the snapshot shows.
    Locked,
    /// No lock was taken: read the latest version, committed or not. Reading it is the
    /// executor's work.
    Dirty,
    /// The row is locked by another transaction and `READPAST` asked to leave it out: the
    /// executor skips it.
    Skip,
    /// No lock was taken and none was waited for: read the version the snapshot of the
    /// statement shows. The answer of a read the database options serve from the row
    /// versions (`snapshot_modes.rs`).
    Versioned,
}

thread_local! {
    /// The row reads of this thread whose shared lock [`TransactionManager::end_row_read`]
    /// gives back, one entry per `(transaction, table, row)`.
    ///
    /// Filled by [`TransactionManager::read_lock`] when the effective level gives the `S`
    /// back at the end of the row read and the transaction held no data mode on that row
    /// beforehand; emptied by [`TransactionManager::end_row_read`], by
    /// [`TransactionManager::write_lock`], which converts the row to `X` and keeps it, and
    /// by a [`TransactionManager::read_lock`] of the same row whose lock is held to the end
    /// of the transaction (the two double-read rows of the module documentation).
    /// A read abandoned without its `end_row_read` leaves its entry behind until the same
    /// thread reads the same row of the same transaction identifier again.
    ///
    /// The entry is a presence, not a count, so it says "this row is to be given back"
    /// and not "how many reads of it are open". What that costs is one shape: two reads
    /// of the same row that are both releasing and both open at once — the inner read finds
    /// the entry already there, and the first [`TransactionManager::end_row_read`] of the
    /// two gives the `S` back while the outer read is still going, so the outer read may see
    /// the row change under it (`tests/isolation.rs`,
    /// `two_open_reads_of_one_row_give_the_lock_back_at_the_first_end`). The executor
    /// brackets one row read at a time on the thread that reads it, which is the shape the pair was
    /// cut for; a caller that nests two reads of one row reads it under `ReadCommitted`
    /// rules from the first `end_row_read` on.
    static SHARED_UNTIL_END_OF_ROW: RefCell<HashSet<(TxnId, TableId, RowId)>> =
        RefCell::new(HashSet::new());
}

/// Notes that the `S` taken on this row is to be given back at the end of the row read.
fn record_row_read(txn: TxnId, table: TableId, row: RowId) {
    SHARED_UNTIL_END_OF_ROW.with_borrow_mut(|rows| {
        rows.insert((txn, table, row));
    });
}

/// Takes the entry of this row read out, reporting whether there was one.
fn take_row_read(txn: TxnId, table: TableId, row: RowId) -> bool {
    SHARED_UNTIL_END_OF_ROW.with_borrow_mut(|rows| rows.remove(&(txn, table, row)))
}

/// The mode a read takes on the row at `level`, or `None` when it takes no lock.
///
/// `XLOCK` and `UPDLOCK` are looked at before the level, so they hold under
/// `ReadUncommitted` too (`tests/isolation.rs`,
/// `updlock_under_read_uncommitted_still_takes_the_u`).
fn read_mode(level: IsolationLevel, hints: &LockIntent) -> Option<LockMode> {
    if hints.xlock {
        return Some(LockMode::X);
    }
    if hints.updlock {
        return Some(LockMode::U);
    }
    match level {
        IsolationLevel::ReadUncommitted => None,
        _ => Some(LockMode::S),
    }
}

/// `true` when the shared lock of a row read at `level` is given back by
/// [`TransactionManager::end_row_read`] rather than at the end of the transaction.
///
/// `Snapshot` answers here as `ReadCommitted` does (`ids.rs`): the `S` of either is taken
/// where the database option is off, see [`TransactionManager::read_lock`].
fn releases_at_end_of_row(level: IsolationLevel) -> bool {
    match level {
        IsolationLevel::RepeatableRead | IsolationLevel::Serializable => false,
        IsolationLevel::ReadUncommitted | IsolationLevel::ReadCommitted => true,
        IsolationLevel::Snapshot => true,
    }
}

/// The mode the table takes before `mode` is taken on one of its rows.
///
/// `IS` under an `S`, `IX` under a `U` or an `X`: the lock hierarchy of SQL Server, not
/// the table hints.
fn intent_of(mode: LockMode) -> LockMode {
    match mode {
        LockMode::S => LockMode::IS,
        _ => LockMode::IX,
    }
}

/// The wait a lock of this transaction is given when the hints carry no `NOWAIT`.
///
/// `SET LOCK_TIMEOUT -1`, [`LockTimeout::Infinite`], is the default of a SQL Server
/// session. [`TxnHandle`] carries an identifier and a level and no timeout (`handle.rs`), so
/// that default is what the locks of this file wait for until the session value is put on
/// the handle; this function is the line that changes then.
fn session_timeout(txn: &TxnHandle) -> LockTimeout {
    let _ = txn;
    LockTimeout::Infinite
}

impl TransactionManager {
    /// The level this read is served at: the level of the hints when they name one, the
    /// level of the transaction otherwise.
    ///
    /// `HOLDLOCK` is the `SERIALIZABLE` hint under another name and is read as that level
    /// when `hints.level` is `None`. Two hints that name two levels are refused with 1047
    /// before the executor is reached (module documentation), so the order between the two
    /// fields decides nothing a client can reach.
    ///
    /// The database options are read after this method, in
    /// [`TransactionManager::read_lock`]: `READ_COMMITTED_SNAPSHOT` turns a `ReadCommitted`
    /// answered here into a versioned read, and `ALLOW_SNAPSHOT_ISOLATION` does the same
    /// for `Snapshot`. Keeping this method to that one job is why it reads no option.
    #[must_use]
    pub fn effective_level(&self, txn: &TxnHandle, hints: &LockIntent) -> IsolationLevel {
        hints
            .level
            .or_else(|| hints.holdlock.then_some(IsolationLevel::Serializable))
            .unwrap_or(txn.isolation)
    }

    /// Takes the locks the level and the hints ask for before `txn` reads one row, and says
    /// what the caller may do with it.
    ///
    /// [`ReadAccess::Dirty`] when the effective level is `ReadUncommitted` and no `UPDLOCK`
    /// or `XLOCK` hint asked for a mode: no row lock, no intent lock, no wait
    /// (`tests/isolation.rs`, `read_uncommitted_does_not_block`,
    /// `nolock_hint_overrides_the_level`). [`ReadAccess::Versioned`] when the read would
    /// take an `S` and the database options serve the effective level from the row
    /// versions — `ReadCommitted` under `READ_COMMITTED_SNAPSHOT`, `Snapshot` under
    /// `ALLOW_SNAPSHOT_ISOLATION` — with the same three absences (`tests/snapshot_modes.rs`,
    /// `rcsi_on_does_not`, `a_read_committed_hint_is_versioned_under_rcsi`).
    /// [`ReadAccess::Skip`] when `READPAST` was asked for and the row lock was refused at
    /// once (`tests/isolation.rs`, `readpast_skips_instead_of_waiting`).
    /// [`ReadAccess::Locked`] otherwise, with the intent lock on the table and the row lock
    /// of the table in the module documentation held.
    ///
    /// The `S` taken for a `ReadCommitted` read is given back by
    /// [`TransactionManager::end_row_read`]; the one taken for a `RepeatableRead` or a
    /// `Serializable` read, and a `U` or an `X` a hint asked for, are given back when the
    /// transaction ends (`tests/isolation.rs`, `read_committed_blocks_on_x_then_releases`,
    /// `repeatable_read_holds_the_share_lock`, `updlock_is_held_to_the_end`,
    /// `commit_releases_every_level`).
    ///
    /// When the same row is read twice by the same transaction, the read whose lock is held
    /// to the end of the transaction decides: it takes out the entry a releasing read of
    /// that row left, so [`TransactionManager::end_row_read`] gives nothing back. That holds
    /// in both orders (`tests/isolation.rs`,
    /// `a_held_read_overrules_an_earlier_released_read_of_the_same_row`,
    /// `a_plain_repeatable_read_overrules_an_earlier_read_committed_hint`), which is the two
    /// double-read rows of the module documentation. Two releasing reads of one row share
    /// one entry, a shape `SHARED_UNTIL_END_OF_ROW` documents.
    ///
    /// # Errors
    ///
    /// The error of a refused lock — 1222 or 1205, from `deadlock::to_error`
    /// (`tests/isolation.rs`, `nowait_yields_1222`). `READPAST` turns the refusal of the
    /// **row** lock into [`ReadAccess::Skip`] instead; the intent lock on the table waits as
    /// the timeout allows, since `READPAST` leaves out rows, not tables.
    pub fn read_lock(
        &self,
        txn: &TxnHandle,
        table: TableId,
        row: RowId,
        hints: &LockIntent,
        wait: &LockWait,
    ) -> SqlResult<ReadAccess> {
        let level = self.effective_level(txn, hints);
        let Some(mode) = read_mode(level, hints) else {
            return Ok(ReadAccess::Dirty);
        };
        if mode == LockMode::S && self.read_versioning(txn, level) != VersioningMode::Locking {
            return Ok(ReadAccess::Versioned);
        }
        let lock_wait = if hints.nowait {
            LockTimeout::NoWait
        } else {
            session_timeout(txn)
        };
        self.take(
            txn.id,
            LockResource::Table(table),
            intent_of(mode),
            lock_wait,
            wait,
        )?;
        let resource = LockResource::Row(table, row);
        let held_before = self.data_mode_held(txn.id, resource);
        let row_timeout = if hints.readpast {
            LockTimeout::NoWait
        } else {
            lock_wait
        };
        if let Err(refused) = self.take(txn.id, resource, mode, row_timeout, wait) {
            if hints.readpast {
                return Ok(ReadAccess::Skip);
            }
            return Err(refused);
        }
        if mode != LockMode::S || !releases_at_end_of_row(level) {
            // This read holds its lock until the transaction ends, so it over-rules a read
            // of the same row that was to give its `S` back at the end of the row: the
            // entry goes, and `end_row_read` gives nothing back. The two double-read rows
            // of the module documentation are the two directions this line answers.
            take_row_read(txn.id, table, row);
        } else if held_before.is_none() {
            record_row_read(txn.id, table, row);
        }
        Ok(ReadAccess::Locked)
    }

    /// Gives back the shared lock of a row read that was to end with the row.
    ///
    /// Does nothing when the row read took no lock, when its lock is held to the end of the
    /// transaction, or when the transaction has since converted the row to a stronger mode
    /// — an `UPDATE` that read the row and then wrote it keeps its `X`
    /// (`tests/isolation.rs`, `a_row_converted_to_x_keeps_its_lock`). The intent lock on the
    /// table stays held in each of these cases (`tests/isolation.rs`,
    /// `end_row_read_keeps_the_intent_lock`).
    ///
    /// # Errors
    ///
    /// The error of [`crate::lock::LockManager::unlock`], which reports a caller bug when
    /// the transaction holds nothing on the row. This method reads the mode held before it
    /// calls `unlock`, so that error is reached by a caller that gave the lock back itself
    /// between the two calls.
    pub fn end_row_read(&self, txn: &TxnHandle, table: TableId, row: RowId) -> SqlResult<()> {
        if !take_row_read(txn.id, table, row) {
            return Ok(());
        }
        let resource = LockResource::Row(table, row);
        if self.data_mode_held(txn.id, resource) != Some(LockMode::S) {
            return Ok(());
        }
        self.locks().unlock(txn.id, resource)
    }

    /// Takes the exclusive lock `txn` needs to change one row, held until it commits or
    /// rolls back.
    ///
    /// An `IX` on the table, then an `X` on the row, at the four lock-based levels —
    /// `ReadUncommitted` included (`tests/isolation.rs`,
    /// `write_lock_takes_x_at_every_level`). A row this transaction was reading under
    /// `ReadCommitted` stops being given back by [`TransactionManager::end_row_read`]: the
    /// `X` this call took is stronger than the `S` the read had (`tests/isolation.rs`,
    /// `a_row_converted_to_x_keeps_its_lock`).
    ///
    /// `READPAST` is not read here: a write that cannot take its row waits or fails, it does
    /// not leave the row out (`tests/isolation.rs`, `readpast_does_not_apply_to_a_write`).
    ///
    /// # Errors
    ///
    /// The error of a refused lock, as [`TransactionManager::read_lock`] reports it.
    pub fn write_lock(
        &self,
        txn: &TxnHandle,
        table: TableId,
        row: RowId,
        hints: &LockIntent,
        wait: &LockWait,
    ) -> SqlResult<()> {
        let lock_wait = if hints.nowait {
            LockTimeout::NoWait
        } else {
            session_timeout(txn)
        };
        self.take(
            txn.id,
            LockResource::Table(table),
            LockMode::IX,
            lock_wait,
            wait,
        )?;
        self.take(
            txn.id,
            LockResource::Row(table, row),
            LockMode::X,
            lock_wait,
            wait,
        )?;
        // Housekeeping: the entry of a `ReadCommitted` read of this row is not needed any
        // more. What keeps the `X` is the mode `end_row_read` reads before it gives a lock
        // back (`tests/isolation.rs`, `a_row_converted_to_x_keeps_its_lock`, which stays
        // green when this line is removed).
        take_row_read(txn.id, table, row);
        Ok(())
    }

    /// Takes one lock through [`crate::lock::LockManager`], reporting a refusal as the error
    /// of `deadlock::to_error`.
    fn take(
        &self,
        txn: TxnId,
        resource: LockResource,
        mode: LockMode,
        timeout: LockTimeout,
        wait: &LockWait,
    ) -> SqlResult<()> {
        match self.locks().try_acquire(txn, resource, mode, timeout, wait) {
            LockOutcome::Granted => Ok(()),
            other => Err(deadlock::to_error(self.locks(), other, resource)),
        }
    }

    /// The data mode `txn` holds on `resource` right now, or `None`.
    fn data_mode_held(&self, txn: TxnId, resource: LockResource) -> Option<LockMode> {
        self.locks()
            .held(txn)
            .into_iter()
            .find(|(res, mode)| *res == resource && mode.is_data())
            .map(|(_, mode)| mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table of the module documentation, held as data: level, then the mode a plain
    /// read takes and whether `end_row_read` gives it back.
    #[test]
    fn the_policy_table_is_the_one_documented() {
        let plain = LockIntent::default();
        let expected = [
            (IsolationLevel::ReadUncommitted, None, true),
            (IsolationLevel::ReadCommitted, Some(LockMode::S), true),
            (IsolationLevel::RepeatableRead, Some(LockMode::S), false),
            (IsolationLevel::Serializable, Some(LockMode::S), false),
            (IsolationLevel::Snapshot, Some(LockMode::S), true),
        ];
        for (level, mode, released) in expected {
            assert_eq!(read_mode(level, &plain), mode, "row mode at {level:?}");
            assert_eq!(
                releases_at_end_of_row(level),
                released,
                "end_row_read at {level:?}"
            );
        }
    }

    /// The hints that name a mode are read before the level, so `UPDLOCK` and `XLOCK` take
    /// theirs under `ReadUncommitted` as under `Serializable`.
    #[test]
    fn updlock_and_xlock_are_read_before_the_level() {
        let updlock = LockIntent {
            updlock: true,
            ..LockIntent::default()
        };
        let xlock = LockIntent {
            xlock: true,
            ..LockIntent::default()
        };
        for level in [
            IsolationLevel::ReadUncommitted,
            IsolationLevel::Serializable,
        ] {
            assert_eq!(read_mode(level, &updlock), Some(LockMode::U));
            assert_eq!(read_mode(level, &xlock), Some(LockMode::X));
        }
        // XLOCK wins over UPDLOCK when both are set, the stronger of the two.
        let both = LockIntent {
            updlock: true,
            xlock: true,
            ..LockIntent::default()
        };
        assert_eq!(
            read_mode(IsolationLevel::ReadCommitted, &both),
            Some(LockMode::X)
        );
    }

    /// The intent mode under each row mode: `IS` under an `S`, `IX` under the four others.
    #[test]
    fn intent_follows_the_row_mode() {
        assert_eq!(intent_of(LockMode::S), LockMode::IS);
        assert_eq!(intent_of(LockMode::U), LockMode::IX);
        assert_eq!(intent_of(LockMode::X), LockMode::IX);
    }
}
