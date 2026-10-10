//! The schema lock: `Sch-S` while an object is read, `Sch-M` while its shape changes.
//!
//! [`crate::lock::LockManager`] already carries the two modes and their line in the
//! compatibility matrix (`tests/lock.rs`,
//! `compatibility_matrix_matches_the_table`): `Sch-S` stands in the way of `Sch-M` and of
//! nothing else, `Sch-M` stands in the way of the seven modes, itself included. What this
//! file adds is **who takes them, on what, and until when**.
//!
//! # Who takes what
//!
//! | statement | mode | placed by |
//! |---|---|---|
//! | a statement that reads the shape of a table, the `NOLOCK` hint included | `Sch-S` | the executor |
//! | `DROP TABLE`, `TRUNCATE TABLE`, `ALTER TABLE` | `Sch-M` | the executor |
//! | the rebuild-and-copy an `ALTER TABLE` needs | `Sch-M` | the catalogue |
//! | `CREATE INDEX`, `DROP INDEX` | `Sch-M` | the executor |
//!
//! The calls themselves are **not** placed here: this file ships
//! [`TransactionManager::schema_stability_lock`],
//! [`TransactionManager::schema_modify_lock`] and the scenarios they answer
//! (`tests/schema_lock.rs`), and the executor and the catalogue call them where the table
//! above says. A database, a view or a procedure is not a resource this file locks:
//! [`LockResource::Table`] is the one it names, and `DROP DATABASE` takes no schema lock.
//!
//! # Until when
//!
//! Both are taken through [`crate::lock::LockManager::lock`] on [`LockResource::Table`],
//! waiting as `session_timeout` allows, and both are given back by
//! `LockManager::release_all` when the transaction commits or rolls back — at the five
//! [`crate::IsolationLevel`]s, `ReadUncommitted` included (`tests/schema_lock.rs`,
//! `nolock_still_takes_sch_s`, `commit_releases_the_schema_lock`,
//! `rollback_releases_the_schema_lock`). That is what tells a schema lock from a data lock:
//! `TransactionManager::read_lock` takes no `S` for a `ReadUncommitted` read
//! (`isolation.rs`), while the `Sch-S` of that same read is taken and held.
//!
//! # Order with the deferred DDL
//!
//! A `DROP TABLE` takes its `Sch-M` **before** it calls
//! [`TransactionManager::register_on_commit`] with [`crate::CommitAction::DropTable`], and
//! `TransactionManager::commit` runs the deferred `storage.drop_table` before
//! `release_all` hands the table to the candidates (`manager.rs`, `finish`). Reversing
//! either half loses the property the lock was taken for: a reader that still held its
//! `Sch-S` would see the table go under it. The thread that wakes on the `Sch-S` the commit
//! released finds the table already gone from `storage` (`tests/schema_lock.rs`,
//! `the_deferred_drop_runs_before_the_waiter_wakes`).
//!
//! # What SQL Server does
//!
//! With two connections A and B on a table of three rows:
//!
//! | scenario | what SQL Server does |
//! |---|---|
//! | A open in `READ COMMITTED`, one row read; B `DROP TABLE` | B goes through, A still open |
//! | the same with A in `REPEATABLE READ` | B waits for the `COMMIT` of A |
//! | A `BEGIN TRAN; ALTER TABLE … ADD c int` left open; B `SELECT … WITH (NOLOCK)` | B waits for the `COMMIT` of A |
//! | the same with B under `SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED` | B waits |
//! | the same with `SET LOCK_TIMEOUT 300` on B | 1222, level 16, state 56 |
//! | A open on an `UPDATE` of the row; B `SELECT … WITH (NOLOCK)` | B answers with the uncommitted value |
//! | A running a long `SELECT … WITH (NOLOCK)`; B `DROP TABLE` | B waits for the end of that statement |
//! | A open, its `WITH (NOLOCK)` read **ended**; B `DROP TABLE` | B goes through |
//!
//! Rows 3 and 4 against row 6 are the point: a `NOLOCK` read waits behind the `ALTER` and
//! goes past the `UPDATE`, so what it waits for is not a data lock and does not follow the
//! isolation level. `nolock_still_takes_sch_s` is that pair. Row 5 says the wait reports the
//! number and the state a wait on an object already reports
//! (`SqlError::lock_request_timeout_on_object`, state 56), which
//! `a_schema_wait_that_times_out_reports_1222_on_the_object` holds. Row 7 against row 8 is
//! the other direction, `Sch-S` in the way of `Sch-M`.
//!
//! Rows 1, 2 and 8 also say how long SQL Server keeps a `Sch-S`: for the statement, not for
//! the transaction. Row 2 blocks on the shared lock a `REPEATABLE READ` keeps on the row,
//! not on a schema lock — row 8, the same transaction with its read finished and no data
//! lock left, lets the `DROP TABLE` through. This file holds its `Sch-S` to the end of the
//! transaction, which is a **deliberate difference from SQL Server** and the stronger of the
//! two: a `DROP TABLE` SQL Server lets through waits here.
//!
//! # What the schema lock does not settle
//!
//! Which tables a transaction sees is not decided here: the catalogue reads it from its
//! versioned rows, so a table whose `CREATE` is not committed is out of the snapshots of the
//! other transactions (`crates/vauban-catalog/tests/snapshot.rs`,
//! `an_uncommitted_create_is_seen_by_its_transaction_alone`). The catalogue takes the `Sch-M`
//! of the storage table a `CREATE`, `DROP` or `ALTER TABLE` works on; a reader that resolves a
//! name in the catalogue asks for no `Sch-S`, so it meets that `Sch-M` only when it locks a
//! row of the table.

use vauban_errors::SqlResult;
use vauban_storage::TableId;

use crate::lock::{LockMode, LockResource, LockWait};
use crate::{LockTimeout, TransactionManager, TxnHandle};

/// The wait a schema lock of this transaction is given.
///
/// `SET LOCK_TIMEOUT -1`, [`LockTimeout::Infinite`], is the default of a SQL Server
/// session. [`TxnHandle`] carries an identifier and a level and no timeout (`handle.rs`), so
/// that default is what the two methods below wait for until the session value is put on
/// the handle; this function is the line that changes then, as its twin in `isolation.rs` is for the data
/// locks (unit test `the_schema_lock_waits_for_ever_by_default`).
fn session_timeout(txn: &TxnHandle) -> LockTimeout {
    let _ = txn;
    LockTimeout::Infinite
}

impl TransactionManager {
    /// Takes the `Sch-S` of a statement that reads the shape of `table`, held until `txn`
    /// commits or rolls back.
    ///
    /// Waits for a `Sch-M` another transaction holds on that table, and not for a data mode:
    /// a writer of a row of `table` holds an `IX`, which `Sch-S` is compatible with
    /// (`tests/schema_lock.rs`, `sch_s_does_not_block_a_writer`), and two readers hold their
    /// `Sch-S` together (`tests/schema_lock.rs`, `sch_s_readers_do_not_block_each_other`,
    /// three threads). The isolation level of `txn` is not read: a `NOLOCK` read takes this
    /// lock as a `SERIALIZABLE` one does (`tests/schema_lock.rs`,
    /// `nolock_still_takes_sch_s`), and the module documentation says why.
    ///
    /// Calling it twice on one table costs nothing: the second call is re-entrant in the
    /// lock manager (`tests/lock.rs`, `reentrant_lock_is_free`).
    ///
    /// # Errors
    ///
    /// The error of [`crate::lock::LockManager::lock`] when the wait ends without the lock:
    /// 1222 for a timeout on an object, 1205 for a transaction chosen as a deadlock victim
    /// (`deadlock.rs`). With `session_timeout` at [`LockTimeout::Infinite`] the timeout is
    /// out of reach from here (`tests/schema_lock.rs`,
    /// `a_schema_wait_that_times_out_reports_1222_on_the_object`, which asks the lock
    /// manager itself for the bounded wait the session will pass).
    pub fn schema_stability_lock(&self, txn: &TxnHandle, table: TableId) -> SqlResult<()> {
        self.schema_lock(txn, table, LockMode::SchS)
    }

    /// Takes the `Sch-M` of a statement that changes the shape of `table`, held until `txn`
    /// commits or rolls back.
    ///
    /// Waits for the other transactions that hold anything on that table — a reader under
    /// `Sch-S`, a writer under `IX`, a `TABLOCK` under `S` — because `Sch-M` is compatible
    /// with no mode of the seven (`tests/lock.rs`,
    /// `compatibility_matrix_matches_the_table`; `tests/schema_lock.rs`,
    /// `drop_waits_for_readers`, `reader_waits_for_ddl`).
    ///
    /// A transaction that already holds the `Sch-S` of a read it has just made converts to
    /// `Sch-M` without waiting for itself — the shape of an `ALTER TABLE` on a table the
    /// same transaction has read (`tests/schema_lock.rs`, `sch_s_to_sch_m_converts`).
    ///
    /// A `DROP TABLE` calls this **before** [`TransactionManager::register_on_commit`]; the
    /// module documentation says what that order buys.
    ///
    /// # Errors
    ///
    /// Those of [`TransactionManager::schema_stability_lock`].
    pub fn schema_modify_lock(&self, txn: &TxnHandle, table: TableId) -> SqlResult<()> {
        self.schema_lock(txn, table, LockMode::SchM)
    }

    /// Takes one schema mode on the table resource, the body the two methods above share.
    fn schema_lock(&self, txn: &TxnHandle, table: TableId, mode: LockMode) -> SqlResult<()> {
        self.locks().lock(
            txn.id,
            LockResource::Table(table),
            mode,
            session_timeout(txn),
            &LockWait::none(),
        )
    }
}

#[cfg(test)]
mod tests;
