//! `READ_COMMITTED_SNAPSHOT` and `ALLOW_SNAPSHOT_ISOLATION`: the two database options that
//! put the **versioning** engine in front of the **locking** one for a read, and the
//! `SNAPSHOT` level they make available.
//!
//! The lock manager (`lock.rs`) and the read policy (`isolation.rs`) serve a database whose
//! two options are off: a `READ COMMITTED` read takes an `S` and waits for the `X` of a
//! writer. This file holds the options, decides the [`VersioningMode`] a read is served at,
//! opens a transaction on a database ([`TransactionManager::begin_in`]) and refuses
//! `SNAPSHOT` where the option is off. What the mode changes is placed where each effect
//! lives: `read_lock` takes no `S` for a versioned read (`isolation.rs`),
//! `statement_snapshot` pins one snapshot per transaction under `TxnSnapshot` and
//! `check_write_conflict` answers [`WriteDecision::Conflict`] under it (`manager.rs`).
//!
//! # Wiring
//!
//! `vauban-txn` reads no catalogue: `vauban-catalog` depends on this crate, not the
//! reverse. The catalogue is the source of truth for the two options and the executor pushes
//! them here with [`TransactionManager::set_versioning_options`] once an
//! `ALTER DATABASE ... SET` has changed them. A database nothing was pushed for has both
//! options off ([`VersioningOptions::default`], `tests/snapshot_modes.rs`,
//! `options_default_to_off`). The options are read at each decision rather than kept on
//! the transaction when it opens: a hint that names a level (`READCOMMITTED` inside a
//! `REPEATABLE READ` transaction) is served at the mode of that level under the options of
//! the database at that moment (`tests/snapshot_modes.rs`,
//! `a_read_committed_hint_is_versioned_under_rcsi`).
//!
//! # The mode
//!
//! | level of the read | `READ_COMMITTED_SNAPSHOT` | `ALLOW_SNAPSHOT_ISOLATION` | [`VersioningMode`] |
//! |---|---|---|---|
//! | `ReadCommitted` | on | either | `StatementSnapshot`: no `S`, one snapshot per statement |
//! | `ReadCommitted` | off | either | `Locking` |
//! | `Snapshot` | either | on | `TxnSnapshot`: no `S`, one snapshot per transaction, 3960 on a write conflict |
//! | `Snapshot` | either | off | `Locking`, served as `ReadCommitted` by [`TransactionManager::begin`]; refused by [`TransactionManager::begin_in`] |
//! | `ReadUncommitted`, `RepeatableRead`, `Serializable` | either | either | `Locking` |
//!
//! The table is held as data by the unit test `the_mode_table_is_the_one_documented`. A
//! write keeps its `X` under the three modes: the options versioned the reads, not the
//! writes (`tests/snapshot_modes.rs`, `write_locks_are_still_taken_under_rcsi`), and so
//! does a `U` or an `X` a hint asked for (`tests/snapshot_modes.rs`,
//! `updlock_still_waits_under_rcsi`).
//!
//! # What SQL Server does
//!
//! With two connections A and B on a table of three rows, the row `id = 1` at `v = 10`:
//!
//! | scenario | what SQL Server does |
//! |---|---|
//! | options off; A `BEGIN TRAN; UPDATE ... WHERE id = 1`, B `SELECT v ... WHERE id = 1` | B waits for the `COMMIT` of A, then reads the committed value |
//! | `READ_COMMITTED_SNAPSHOT ON`; the same | B answers `10` at once; its next `SELECT` after the `COMMIT` reads `11` |
//! | `READ_COMMITTED_SNAPSHOT ON`; B `BEGIN TRAN; SELECT` reads `10`, A `UPDATE` committed, B `SELECT` again | `11`: one snapshot per statement |
//! | `READ_COMMITTED_SNAPSHOT ON`; B in `REPEATABLE READ`, `SELECT ... WITH (READCOMMITTED)` while A holds the row | B answers `10` at once |
//! | `READ_COMMITTED_SNAPSHOT ON`; B `SELECT ... WITH (REPEATABLEREAD)`, `WITH (UPDLOCK)` or `WITH (READCOMMITTEDLOCK)`, or B in `REPEATABLE READ` | B waits for the `COMMIT` of A |
//! | `READ_COMMITTED_SNAPSHOT ON`; A `BEGIN TRAN; UPDATE`, B `UPDATE` of the same row | B waits for the `COMMIT` of A |
//! | `ALLOW_SNAPSHOT_ISOLATION ON`; B `SNAPSHOT`, `BEGIN TRAN; SELECT` reads `10`, A `UPDATE` committed, B `SELECT` again | `10`; after its `COMMIT`, B reads `11` |
//! | the same, but B reads nothing before the `UPDATE` of A (`BEGIN TRAN` alone, or `SELECT 1`) | B reads `11`: the snapshot is pinned by the first read of a table, not by `BEGIN TRAN` |
//! | `ALLOW_SNAPSHOT_ISOLATION ON`; A and B `SNAPSHOT`, both read the row, A `UPDATE` committed, B `UPDATE` of the same row | 3960, severity 16, state 2; `@@TRANCOUNT` is `0` afterwards |
//! | the same, B `UPDATE` of another row | goes through |
//! | the same, B `DELETE` of the row, or A `DELETE` committed then B `UPDATE` | 3960 |
//! | the same, but the `UPDATE` of A rolled back instead of committed | B goes through |
//! | `ALLOW_SNAPSHOT_ISOLATION ON`; A `SNAPSHOT` holds the row updated, B `SNAPSHOT` `UPDATE` of it | B waits for the `COMMIT` of A, then 3960 |
//! | `ALLOW_SNAPSHOT_ISOLATION ON`; B `SNAPSHOT` read the row, A `UPDATE` committed, B `UPDATE ... WITH (READCOMMITTED)` | goes through |
//! | options off; `SET TRANSACTION ISOLATION LEVEL SNAPSHOT; BEGIN TRAN; SELECT` | 3952, severity 16, state 1 |
//!
//! The first two rows are what tells the two engines apart, and
//! `tests/snapshot_modes.rs` holds them as `rcsi_off_blocks_the_reader` and
//! `rcsi_on_does_not`. The row with the `READCOMMITTED` hint inside a `SNAPSHOT` write is
//! not served: [`TransactionManager::check_write_conflict`] takes no hint and decides at
//! the level of the transaction.
//!
//! # The option flip
//!
//! On SQL Server, `ALTER DATABASE ... SET READ_COMMITTED_SNAPSHOT ON` waits while another
//! connection is open on the database, an idle one included; `WITH NO_WAIT` answers 5070,
//! severity 16, state 2, and `WITH ROLLBACK IMMEDIATE` closes the other connections.
//! `SET ALLOW_SNAPSHOT_ISOLATION OFF` waits for the open `SNAPSHOT` transactions to end.
//! So no transaction sees the option it runs under change:
//! [`TransactionManager::set_versioning_options`] just replaces the entry, and refusing or
//! delaying the `ALTER` belongs to its executor.
//!
//! # 3952 and 3960
//!
//! Neither number is built here. `begin_in` refuses `SNAPSHOT` where the option is off with
//! an internal error (`snapshot_not_allowed`); the session turns the refusal into 3952,
//! which names the database. `check_write_conflict` answers [`WriteDecision::Conflict`];
//! the executor turns it into 3960, which names the table and the database.
//!
//! [`WriteDecision::Conflict`]: crate::WriteDecision::Conflict

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::DbId;

use crate::{IsolationLevel, TransactionManager, TxnHandle};

/// The two database options that turn the versioning engine on, as `sys.databases` shows
/// them (`is_read_committed_snapshot_on`, `snapshot_isolation_state`).
///
/// Built by its literal or by [`VersioningOptions::default`], which is both off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VersioningOptions {
    /// `READ_COMMITTED_SNAPSHOT ON`: a `READ COMMITTED` read takes no shared lock and reads
    /// through a snapshot taken for its statement.
    pub read_committed_snapshot: bool,
    /// `ALLOW_SNAPSHOT_ISOLATION ON`: the `SNAPSHOT` level may be asked for; its snapshot
    /// is pinned for the transaction and a write on a row changed since is a conflict.
    pub allow_snapshot_isolation: bool,
}

impl VersioningOptions {
    /// The mode a read at `level` is served at under these options, the table of the module
    /// documentation.
    pub(crate) fn mode_for(self, level: IsolationLevel) -> VersioningMode {
        match level {
            IsolationLevel::Snapshot if self.allow_snapshot_isolation => {
                VersioningMode::TxnSnapshot
            }
            IsolationLevel::ReadCommitted if self.read_committed_snapshot => {
                VersioningMode::StatementSnapshot
            }
            IsolationLevel::ReadUncommitted
            | IsolationLevel::ReadCommitted
            | IsolationLevel::RepeatableRead
            | IsolationLevel::Serializable
            | IsolationLevel::Snapshot => VersioningMode::Locking,
        }
    }
}

/// Which engine serves a read: the shared lock of `isolation.rs`, or a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersioningMode {
    /// The read takes the lock its level asks for. The mode of a database whose options
    /// are off, and of the levels other than `ReadCommitted` and `Snapshot`.
    Locking,
    /// `READ_COMMITTED_SNAPSHOT`: no shared lock, a snapshot taken at each
    /// [`TransactionManager::statement_snapshot`] call.
    StatementSnapshot,
    /// `SNAPSHOT` under `ALLOW_SNAPSHOT_ISOLATION`: no shared lock, one snapshot pinned by
    /// the first [`TransactionManager::statement_snapshot`] call and handed out unchanged
    /// until the transaction ends, and a conflict on a row another transaction changed
    /// after that snapshot.
    TxnSnapshot,
}

/// The error [`TransactionManager::begin_in`] returns for `SNAPSHOT` on a database whose
/// `ALLOW_SNAPSHOT_ISOLATION` is off.
///
/// An internal error on purpose: the session that catches it builds 3952, which names the
/// database by its name, something this crate does not hold.
pub(crate) fn snapshot_not_allowed(db: DbId) -> SqlError {
    InternalError::Bug(format!(
        "TransactionManager::begin_in: SNAPSHOT needs ALLOW_SNAPSHOT_ISOLATION on database {db}"
    ))
    .into()
}

/// The options of each database the executor pushed, behind their own lock.
///
/// Taken after the state lock of `manager.rs` when both are needed, and never held while
/// that one is taken.
pub(crate) type OptionsMap = Mutex<HashMap<DbId, VersioningOptions>>;

impl TransactionManager {
    /// The options map, recovering from a poisoned lock as `manager.rs` does.
    fn options(&self) -> MutexGuard<'_, HashMap<DbId, VersioningOptions>> {
        self.versioning
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the options of `db`, the call the executor makes after
    /// `ALTER DATABASE ... SET READ_COMMITTED_SNAPSHOT` or `... SET ALLOW_SNAPSHOT_ISOLATION`.
    ///
    /// The transactions already open on `db` are served at the new options from their next
    /// decision on; a snapshot already pinned stays pinned (`tests/snapshot_modes.rs`,
    /// `a_flip_does_not_unpin_the_snapshot`). Whether the `ALTER` may run while they are
    /// open is decided by its executor, see the module documentation.
    pub fn set_versioning_options(&self, db: DbId, opts: VersioningOptions) {
        self.options().insert(db, opts);
    }

    /// The options of `db`: what was last pushed, both off otherwise
    /// (`tests/snapshot_modes.rs`, `options_default_to_off`).
    #[must_use]
    pub fn versioning_options(&self, db: DbId) -> VersioningOptions {
        self.options().get(&db).copied().unwrap_or_default()
    }

    /// Opens a transaction on `db` at `isolation` and returns its handle.
    ///
    /// [`TransactionManager::begin`] with the database made explicit: the identifier, the
    /// entry in [`TransactionManager::active_sessions`] and the absence of a `storage` call
    /// are the ones documented there. The database is kept by the manager, not on the
    /// handle, and is what [`TransactionManager::versioning_mode`] reads the options of.
    ///
    /// Under `TxnSnapshot`, no snapshot is taken here: the first
    /// [`TransactionManager::statement_snapshot`] call pins it, which is when SQL Server
    /// pins its own (module documentation; `tests/snapshot_modes.rs`,
    /// `the_snapshot_is_pinned_by_the_first_statement_not_by_begin`).
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` (`snapshot_not_allowed`) when `isolation` is [`IsolationLevel::Snapshot`] and the
    /// `allow_snapshot_isolation` option of `db` is off (`tests/snapshot_modes.rs`,
    /// `snapshot_requires_the_option`); nothing is opened then. The session answers 3952 in
    /// its place. Nothing else.
    pub fn begin_in(&self, db: DbId, isolation: IsolationLevel) -> SqlResult<TxnHandle> {
        if isolation == IsolationLevel::Snapshot
            && !self.versioning_options(db).allow_snapshot_isolation
        {
            return Err(snapshot_not_allowed(db));
        }
        Ok(self.open(db, isolation))
    }

    /// The mode the reads of `txn` are served at, at its own level: the table of the module
    /// documentation, on the options its database has now.
    ///
    /// `Locking` for a handle the manager no longer holds open.
    #[must_use]
    pub fn versioning_mode(&self, txn: &TxnHandle) -> VersioningMode {
        self.read_versioning(txn, txn.isolation)
    }

    /// The mode a read of `txn` at `level` is served at, where `level` may be the one a
    /// hint named rather than the level of the transaction.
    pub(crate) fn read_versioning(&self, txn: &TxnHandle, level: IsolationLevel) -> VersioningMode {
        match self.database_of(txn.id) {
            Some(db) => self.versioning_options(db).mode_for(level),
            None => VersioningMode::Locking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table of the module documentation, held as data: level and options, then the
    /// mode.
    #[test]
    fn the_mode_table_is_the_one_documented() {
        let off = VersioningOptions::default();
        let rcsi = VersioningOptions {
            read_committed_snapshot: true,
            ..off
        };
        let asi = VersioningOptions {
            allow_snapshot_isolation: true,
            ..off
        };
        let both = VersioningOptions {
            read_committed_snapshot: true,
            allow_snapshot_isolation: true,
        };
        use IsolationLevel::*;
        use VersioningMode::*;
        let expected = [
            (ReadCommitted, rcsi, StatementSnapshot),
            (ReadCommitted, both, StatementSnapshot),
            (ReadCommitted, off, Locking),
            (ReadCommitted, asi, Locking),
            (Snapshot, asi, TxnSnapshot),
            (Snapshot, both, TxnSnapshot),
            (Snapshot, off, Locking),
            (Snapshot, rcsi, Locking),
            (ReadUncommitted, both, Locking),
            (RepeatableRead, both, Locking),
            (Serializable, both, Locking),
        ];
        for (level, opts, mode) in expected {
            assert_eq!(opts.mode_for(level), mode, "{level:?} under {opts:?}");
        }
    }

    /// The refusal names the method and the database, like the other internal errors of
    /// the manager.
    #[test]
    fn the_refusal_names_the_database() {
        let err = snapshot_not_allowed(DbId(7));
        assert_eq!(err.number, 50000);
        assert!(err.message.contains("begin_in"), "{}", err.message);
        assert!(err.message.contains("database 7"), "{}", err.message);
    }
}
