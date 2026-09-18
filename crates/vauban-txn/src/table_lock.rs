//! `TABLOCK`, `TABLOCKX`, `ROWLOCK`, `PAGLOCK` and lock escalation: which granularity one
//! instruction takes on a table, and until when a table lock is held.
//!
//! [`TransactionManager::table_lock`] is called once per table per instruction, before the
//! row loop. It takes an `S` or an `X` on [`LockResource::Table`] when `TABLOCK` or
//! `TABLOCKX` is asked for, and answers [`TableLockDecision`] so the executor knows
//! whether it still needs a row lock per row. The intent locks of the hierarchy stay in
//! `isolation.rs`; this file does not rewrite them.
//!
//! # Until when
//!
//! | isolation level | table lock from `TABLOCK` / `TABLOCKX` |
//! |---|---|
//! | `ReadCommitted`, `ReadUncommitted` | until [`TransactionManager::end_table_lock`] |
//! | `RepeatableRead`, `Serializable`, `Snapshot` | until the transaction commits or rolls back |
//!
//! # What SQL Server does
//!
//! With two connections A and B on one table:
//!
//! | scenario | what SQL Server does |
//! |---|---|
//! | A `READ COMMITTED`, `BEGIN TRAN; SELECT … WITH (TABLOCK); WAITFOR` left open; B `UPDATE` another row | B goes through once the `SELECT` ended |
//! | the same with A in `REPEATABLE READ` | B waits for the `COMMIT` of A |
//!
//! The first row is why [`TransactionManager::end_table_lock`] exists for the two releasing
//! levels (`tests/table_lock.rs`, `tablock_is_released_after_the_statement_under_read_committed`);
//! the second is why the table lock stays through commit there
//! (`tablock_is_held_to_commit_under_repeatable_read`).
//!
//! # `ROWLOCK` and `PAGLOCK`
//!
//! `ROWLOCK` asks for row granularity — the default on `MemoryStorage`, where this file
//! answers [`TableLockDecision::RowLevel`]. `PAGLOCK` asks for page granularity; there are
//! no pages here, so the hint is accepted and the decision is the same as `ROWLOCK`
//! (`tests/table_lock.rs`, `paglock_is_accepted_without_effect`).
//!
//! # Escalation
//!
//! [`TransactionManager::maybe_escalate`] converts row locks on one table into one table
//! lock once [`ESCALATION_THRESHOLD`] row locks are held there
//! (`tests/table_lock.rs`, `escalation_converts_at_the_threshold`,
//! `escalation_does_not_convert_one_row_below`).

use std::sync::Mutex;

use vauban_errors::SqlResult;
use vauban_storage::{RowId, TableId, TxnId};

use crate::lock::{LockMode, LockResource, LockWait};
use crate::{IsolationLevel, LockIntent, LockTimeout, TransactionManager, TxnHandle};

/// Row locks held on one table at or above this count convert to a table lock through
/// [`TransactionManager::maybe_escalate`].
pub const ESCALATION_THRESHOLD: usize = 6235;

/// What granularity one instruction uses on one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableLockDecision {
    /// Row locks are still needed for each row of the instruction.
    RowLevel,
    /// An `S` on the table was taken; row shared locks can be skipped.
    TableShared,
    /// An `X` on the table was taken; another transaction's [`TransactionManager::read_lock`]
    /// at `ReadCommitted` waits behind it (`tests/table_lock.rs`,
    /// `tablockx_blocks_a_read_lock`).
    TableExclusive,
}

/// Table locks [`TransactionManager::end_table_lock`] gives back at the end of one statement
/// under `ReadCommitted` and `ReadUncommitted`. The manager address disambiguates `TxnId`s
/// across [`TransactionManager`] instances in concurrent tests.
static STATEMENT_TABLE_LOCKS: Mutex<Vec<(usize, TxnId, TableId)>> = Mutex::new(Vec::new());

fn manager_key(mgr: &TransactionManager) -> usize {
    std::ptr::from_ref(mgr).addr()
}

fn record_statement_table_lock(mgr: &TransactionManager, txn: TxnId, table: TableId) {
    STATEMENT_TABLE_LOCKS
        .lock()
        .expect("statement table lock list")
        .push((manager_key(mgr), txn, table));
}

fn take_statement_table_lock(mgr: &TransactionManager, txn: TxnId, table: TableId) -> bool {
    let key = manager_key(mgr);
    let mut locks = STATEMENT_TABLE_LOCKS
        .lock()
        .expect("statement table lock list");
    if let Some(pos) = locks
        .iter()
        .position(|&(k, t, tbl)| k == key && t == txn && tbl == table)
    {
        locks.remove(pos);
        true
    } else {
        false
    }
}

/// `true` when a table lock taken for this level is given back by
/// [`TransactionManager::end_table_lock`] rather than at commit.
fn releases_table_lock_at_end_of_statement(level: IsolationLevel) -> bool {
    matches!(
        level,
        IsolationLevel::ReadCommitted | IsolationLevel::ReadUncommitted
    )
}

/// The wait a table lock of this transaction is given when the hints carry no `NOWAIT`.
fn session_timeout(txn: &TxnHandle) -> LockTimeout {
    let _ = txn;
    LockTimeout::Infinite
}

impl TransactionManager {
    /// Takes the table lock the hints ask for before one instruction walks the rows of
    /// `table`, and says whether row locks are still needed.
    ///
    /// `TABLOCK` takes an `S` on the table and answers [`TableLockDecision::TableShared`].
    /// `TABLOCKX` takes an `X` and answers [`TableLockDecision::TableExclusive`]. When a
    /// table lock is taken, row locks of the same table can be omitted for this instruction.
    /// `ROWLOCK`, the default, and `PAGLOCK` answer [`TableLockDecision::RowLevel`].
    ///
    /// Under `ReadCommitted` and `ReadUncommitted`, the table lock is given back by
    /// [`TransactionManager::end_table_lock`]; under the other levels it stays until commit
    /// or rollback (`tests/table_lock.rs`, `tablock_is_released_after_the_statement_under_read_committed`).
    ///
    /// # Errors
    ///
    /// The error of [`crate::lock::LockManager::lock`] when the wait ends without the lock.
    pub fn table_lock(
        &self,
        txn: &TxnHandle,
        table: TableId,
        hints: &LockIntent,
    ) -> SqlResult<TableLockDecision> {
        if hints.tablockx {
            self.acquire_table_lock(txn, table, LockMode::X, hints)?;
            if releases_table_lock_at_end_of_statement(txn.isolation) {
                record_statement_table_lock(self, txn.id, table);
            }
            return Ok(TableLockDecision::TableExclusive);
        }
        if hints.tablock {
            self.acquire_table_lock(txn, table, LockMode::S, hints)?;
            if releases_table_lock_at_end_of_statement(txn.isolation) {
                record_statement_table_lock(self, txn.id, table);
            }
            return Ok(TableLockDecision::TableShared);
        }
        // `PAGLOCK` has no page to lock; `ROWLOCK` and the default keep row granularity.
        Ok(TableLockDecision::RowLevel)
    }

    /// Gives back a table lock that was to end with the instruction.
    ///
    /// Does nothing when this table was not marked for release at the end of the statement,
    /// or when the level holds table locks to the end of the transaction. The mark lives in
    /// a process-wide list, so any thread of the transaction may call this method
    /// (`tests/table_lock.rs`, `end_table_lock_from_another_thread_releases_the_lock`).
    ///
    /// # Errors
    ///
    /// The error of [`crate::lock::LockManager::unlock`] when the transaction holds nothing
    /// on the table.
    pub fn end_table_lock(&self, txn: &TxnHandle, table: TableId) -> SqlResult<()> {
        if !take_statement_table_lock(self, txn.id, table) {
            return Ok(());
        }
        self.locks().unlock(txn.id, LockResource::Table(table))
    }

    /// When `txn` holds at least [`ESCALATION_THRESHOLD`] row locks on `table`, gives those
    /// locks back and takes one table lock instead.
    ///
    /// An `X` on one row escalates to [`TableLockDecision::TableExclusive`]; when each row
    /// lock is an `S`, the table lock is [`TableLockDecision::TableShared`]. Does nothing
    /// when a data mode is already held on the table resource, or when the count sits below
    /// the threshold (`tests/table_lock.rs`, `escalation_converts_at_the_threshold`,
    /// `escalation_does_not_convert_one_row_below`).
    ///
    /// # Errors
    ///
    /// Those of [`TransactionManager::table_lock`] when the table lock cannot be taken.
    pub fn maybe_escalate(
        &self,
        txn: &TxnHandle,
        table: TableId,
    ) -> SqlResult<Option<TableLockDecision>> {
        if self.table_data_mode(txn.id, table).is_some() {
            return Ok(None);
        }
        let rows = self.row_locks_on_table(txn.id, table);
        if rows.len() < ESCALATION_THRESHOLD {
            return Ok(None);
        }
        let table_mode = if rows
            .iter()
            .any(|(_, mode)| matches!(mode, LockMode::X | LockMode::U))
        {
            LockMode::X
        } else {
            LockMode::S
        };
        for (row, _) in &rows {
            self.locks()
                .unlock(txn.id, LockResource::Row(table, *row))?;
        }
        self.acquire_table_lock(txn, table, table_mode, &LockIntent::default())?;
        let decision = if table_mode == LockMode::X {
            TableLockDecision::TableExclusive
        } else {
            TableLockDecision::TableShared
        };
        Ok(Some(decision))
    }

    /// Row data modes `txn` holds on rows of `table`.
    fn row_locks_on_table(&self, txn: TxnId, table: TableId) -> Vec<(RowId, LockMode)> {
        self.locks()
            .held(txn)
            .into_iter()
            .filter_map(|(res, mode)| match res {
                LockResource::Row(t, row) if t == table && mode.is_data() => Some((row, mode)),
                _ => None,
            })
            .collect()
    }

    /// The data mode `txn` holds on the table resource, if any.
    fn table_data_mode(&self, txn: TxnId, table: TableId) -> Option<LockMode> {
        self.locks()
            .held(txn)
            .into_iter()
            .find(|(res, mode)| *res == LockResource::Table(table) && mode.is_data())
            .map(|(_, mode)| mode)
    }

    /// Takes one data mode on the table resource.
    fn acquire_table_lock(
        &self,
        txn: &TxnHandle,
        table: TableId,
        mode: LockMode,
        hints: &LockIntent,
    ) -> SqlResult<()> {
        let timeout = if hints.nowait {
            LockTimeout::NoWait
        } else {
            session_timeout(txn)
        };
        self.locks().lock(
            txn.id,
            LockResource::Table(table),
            mode,
            timeout,
            &LockWait::none(),
        )
    }
}
