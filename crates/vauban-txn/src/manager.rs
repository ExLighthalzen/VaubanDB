//! The transaction manager: identifier assignment, list of open transactions, MVCC
//! snapshots and the commit / rollback life cycle.
//!
//! This file implements `new`, `begin`, `commit`, `rollback`, `statement_snapshot`,
//! `snapshot_horizon` and `active_sessions` over the [`Storage`] contract; `savepoint` and
//! `rollback_to`, `register_on_commit` and `register_on_rollback`, with `commit` and
//! `rollback` running the deferred actions of [`crate::actions`]; and the [`LockManager`]:
//! `lock_row` takes an `X` on the row through it, and `commit` and `rollback` give the locks
//! of the transaction back once `storage` has accepted the outcome (`tests/lock.rs`).
//! `statement_snapshot` and `check_write_conflict` are the two methods the database options
//! of `snapshot_modes.rs` change: one snapshot pinned per transaction under
//! [`VersioningMode::TxnSnapshot`], and a [`WriteDecision`] read off the snapshot the
//! transaction last got.
//!
//! # Three locks, one order
//!
//! The [`Mutex`] below guards the list of open transactions; the [`LockManager`] guards the
//! lock table and is the one a thread parks on; the options map of `snapshot_modes.rs`
//! guards the two database options. They are taken in that order, state first: `finish`
//! calls [`LockManager::release_all`], which parks on nothing, while it holds the state
//! lock, and `lock_row`, which may park for as long as its [`LockTimeout`], takes the state
//! lock neither before nor while it parks. `statement_snapshot` and `check_write_conflict`
//! read the options while they hold the state lock; `set_versioning_options` holds the
//! options lock alone.
//!
//! # One lock, one order
//!
//! [`State`] holds the counter and the list of open transactions — each with its action log —
//! behind a single [`Mutex`]. `commit` and `rollback` hold that lock across the deferred
//! actions and the call to [`Storage::commit`] / [`Storage::rollback`], and they call
//! `storage` **before** taking the transaction out of the list of open ones. Since
//! [`TransactionManager::statement_snapshot`] takes the same
//! lock, a [`Snapshot`] built by another thread reads a state where the two steps have both
//! happened or neither has. The test that tells the two orders apart is
//! `a_refused_storage_commit_keeps_the_transaction_open` (`tests/txn_basic.rs`): swapping the
//! two statements of `finish` makes it fail, on one thread, on a `storage` that refuses the
//! commit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{DbId, RowId, SavepointId, Snapshot, Storage, TableId, TxnId, TxnStatus};

use crate::actions::ActionLog;
use crate::lock::{LockManager, LockMode, LockResource, LockWait};
use crate::snapshot_modes::OptionsMap;
use crate::{
    CommitAction, IsolationLevel, LockStatus, LockTimeout, RollbackAction, TxnHandle, TxnInfo,
    TxnState, VersioningMode, WriteDecision,
};

/// The database [`TransactionManager::begin`] opens on: the identifier a fresh
/// `MemoryStorage` hands to its first database (`tests/snapshot_modes.rs`,
/// `begin_opens_on_the_first_database`). A caller that knows its database uses
/// [`TransactionManager::begin_in`].
pub(crate) const DEFAULT_DB: DbId = DbId(1);

/// The error returned when a handle names a transaction the manager does not hold open.
///
/// Reached by a second `commit` or `rollback` of the same handle, or by a handle built by
/// another manager. A caller bug, like the equivalent precondition of [`Storage::commit`],
/// so number `50000` (`tests/txn_basic.rs`, `commit_twice_is_a_bug`).
fn not_open(method: &str, id: TxnId) -> SqlError {
    InternalError::Bug(format!(
        "TransactionManager::{method}: transaction {id} is not open"
    ))
    .into()
}

/// The error returned when a savepoint is not one the manager holds for this transaction.
///
/// Reached by a savepoint of another transaction, one the caller made up, or one invalidated
/// by an earlier `rollback_to` to a savepoint older than it — the precondition of
/// [`Storage::rollback_to`], checked here before `storage` is called (`tests/txn_actions.rs`,
/// `rollback_to_an_unknown_savepoint_is_a_bug`).
fn unknown_savepoint(sp: SavepointId, id: TxnId) -> SqlError {
    InternalError::Bug(format!(
        "TransactionManager::rollback_to: savepoint {sp} is not held by transaction {id}"
    ))
    .into()
}

/// One open transaction: what the `sys.dm_tran_*` views read, and the actions it deferred.
struct Open {
    /// Identifier and isolation level, as published by
    /// [`TransactionManager::active_sessions`].
    handle: TxnHandle,
    /// When [`TransactionManager::begin`] opened it, the `began_at` of its [`TxnInfo`].
    began_at: SystemTime,
    /// The deferred drops, the compensations and the savepoints of this transaction, in
    /// registration order.
    log: ActionLog,
    /// The database the transaction was opened on, whose options decide its
    /// [`VersioningMode`].
    db: DbId,
    /// The snapshot pinned by the first [`TransactionManager::statement_snapshot`] call
    /// under [`VersioningMode::TxnSnapshot`]; `None` before that call and under the two
    /// other modes.
    pinned: Option<Snapshot>,
    /// The snapshot the last [`TransactionManager::statement_snapshot`] call handed out
    /// under the two other modes, what [`TransactionManager::check_write_conflict`]
    /// compares a row against.
    last: Option<Snapshot>,
}

/// What the manager knows about the transactions it started, behind one lock.
struct State {
    /// Identifier the next [`TransactionManager::begin`] hands out; starts at `1`.
    next_id: u64,
    /// The transactions started and not yet closed, in the order they started, which is
    /// also increasing identifier order: [`TransactionManager::begin`] appends and
    /// [`TransactionManager::commit`] / [`TransactionManager::rollback`] remove one entry
    /// in place, so the order survives both (`tests/txn_basic.rs`,
    /// `active_ids_of_a_snapshot_are_sorted`).
    open: Vec<Open>,
}

impl State {
    /// Position of `id` in [`State::open`], or `None` when the transaction is closed.
    fn position(&self, id: TxnId) -> Option<usize> {
        self.open.iter().position(|txn| txn.handle.id == id)
    }

    /// The action log of `id`, or `None` when the transaction is closed.
    fn log_mut(&mut self, id: TxnId) -> Option<&mut ActionLog> {
        let pos = self.position(id)?;
        Some(&mut self.open[pos].log)
    }

    /// The smallest identifier still open, or `None` when the list is empty.
    fn oldest_open(&self) -> Option<TxnId> {
        self.open.iter().map(|txn| txn.handle.id).min()
    }

    /// A snapshot for `own` built from this state, the shape
    /// [`TransactionManager::statement_snapshot`] documents.
    fn snapshot_for(&self, own: TxnId) -> Snapshot {
        let xmax = TxnId(self.next_id);
        let active: Vec<TxnId> = self
            .open
            .iter()
            .map(|open| open.handle.id)
            .filter(|&id| id != own)
            .collect();
        Snapshot {
            xmin: self.oldest_open().unwrap_or(xmax),
            xmax,
            active,
            own,
        }
    }

    /// The fate of `t` as this state knows it: in progress while the manager holds it
    /// open, committed otherwise. A rolled-back transaction is not told from a committed
    /// one, and need not be: `Storage::rollback` removes the versions it wrote, so
    /// `Storage::latest_version` never names it (`tests/snapshot_modes.rs`,
    /// `a_rolled_back_update_is_no_conflict`).
    fn status(&self, t: TxnId) -> TxnStatus {
        if self.position(t).is_some() {
            TxnStatus::InProgress
        } else {
            TxnStatus::Committed
        }
    }
}

/// Assigns transaction identifiers, tracks open transactions, builds the [`Snapshot`] a
/// statement reads through and drives commit and rollback down to [`Storage`].
///
/// Two visibility engines meet here: the versioning engine carried by `storage`, and the
/// lock manager (shared locks held or released, 1222, 1205).
pub struct TransactionManager {
    /// Storage the manager commits to and rolls back to.
    storage: Arc<dyn Storage>,
    state: Mutex<State>,
    /// The locks the transactions of this manager hold and wait for.
    locks: LockManager,
    /// The two database options of each database, as pushed by
    /// [`TransactionManager::set_versioning_options`].
    pub(crate) versioning: OptionsMap,
}

impl TransactionManager {
    /// A manager over `storage`, with an empty open-transaction list, the next identifier from
    /// [`Storage::next_txn_id`] and an empty lock table.
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage: Arc::clone(&storage),
            state: Mutex::new(State {
                next_id: storage.next_txn_id(),
                open: Vec::new(),
            }),
            locks: LockManager::new(),
            versioning: Mutex::new(HashMap::new()),
        }
    }

    /// Raises the next identifier above [`Storage::committed_txn_high_water`] when a second
    /// engine shares the storage and registry writes must not reuse a committed id.
    pub fn bump_next_id_above_storage(&self) {
        let floor = self.storage.committed_txn_high_water().saturating_add(1);
        let mut state = self.state.lock().expect("transaction manager lock");
        if state.next_id < floor {
            state.next_id = floor;
        }
    }

    /// The lock manager of this server, for the callers that name a mode or a resource
    /// themselves: the table hints, the schema locks, the `sys.dm_tran_*` views and the
    /// tests.
    ///
    /// [`TransactionManager::lock_row`] is the short path for one case: an `X` on one row.
    #[must_use]
    pub fn locks(&self) -> &LockManager {
        &self.locks
    }

    /// The state, recovering from a poisoned lock instead of panicking: the manager sits on
    /// the execution path of a query, which must not panic.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens a transaction at `isolation` and returns its handle.
    ///
    /// Identifiers are handed out from `TxnId(1)`, one more at each call, so that a
    /// transaction started later carries the larger identifier — what the snapshot rule of
    /// `storage` compares (`vauban_storage::Snapshot`). The first `u64::MAX` calls on one
    /// manager hand out `TxnId(1)` up to `TxnId(u64::MAX)`; from the call after those, the
    /// counter is saturated and `begin` hands out `TxnId(u64::MAX)` again instead of wrapping
    /// round to an identifier already used (unit test `saturated_counter_repeats_its_bound`).
    ///
    /// The transaction is added to the list returned by
    /// [`TransactionManager::active_sessions`]. `storage` is not called: it discovers the
    /// transaction at its first write (crate documentation of `vauban_storage`).
    ///
    /// # Isolation
    ///
    /// `isolation` is kept on the handle and the five variants are accepted. The
    /// transaction is opened on the database `DbId(1)` (`DEFAULT_DB`), the identifier a
    /// fresh `MemoryStorage` hands to its first database (`tests/snapshot_modes.rs`,
    /// `begin_opens_on_the_first_database`), and the options of that database decide
    /// the [`VersioningMode`] of its reads (`snapshot_modes.rs`). With both options off, the
    /// snapshot served is that of [`IsolationLevel::ReadCommitted`]:
    /// [`TransactionManager::statement_snapshot`] rebuilds one at each call, for the five of
    /// them alike (`tests/txn_basic.rs`, `the_five_levels_are_served_as_read_committed`),
    /// and what tells the levels apart is the lock policy of `isolation.rs`. Unlike
    /// [`TransactionManager::begin_in`], this method does not refuse
    /// [`IsolationLevel::Snapshot`] where the option is off: the level is kept on the handle
    /// and served at [`VersioningMode::Locking`].
    pub fn begin(&self, isolation: IsolationLevel) -> TxnHandle {
        self.open(DEFAULT_DB, isolation)
    }

    /// Opens a transaction on `db`: the body of [`TransactionManager::begin`] and of
    /// [`TransactionManager::begin_in`], once the latter has checked the option.
    pub(crate) fn open(&self, db: DbId, isolation: IsolationLevel) -> TxnHandle {
        let mut state = self.lock();
        let handle = TxnHandle {
            id: TxnId(state.next_id),
            isolation,
        };
        state.next_id = state.next_id.saturating_add(1);
        state.open.push(Open {
            handle: handle.clone(),
            began_at: SystemTime::now(),
            log: ActionLog::default(),
            db,
            pinned: None,
            last: None,
        });
        handle
    }

    /// The database the open transaction `id` was opened on, `None` once it is closed.
    pub(crate) fn database_of(&self, id: TxnId) -> Option<DbId> {
        let state = self.lock();
        state.position(id).map(|pos| state.open[pos].db)
    }

    /// Commits `txn`: runs the drops it deferred, makes its writes durable in `storage`, then
    /// closes it.
    ///
    /// The [`CommitAction`]s registered with
    /// [`TransactionManager::register_on_commit`] run first, in registration order
    /// (`tests/txn_actions.rs`, `drop_deferred_until_commit`,
    /// `commit_actions_run_in_registration_order`), and the [`RollbackAction`]s are dropped
    /// unrun (`tests/txn_actions.rs`, `a_committed_compensation_leaves_the_table`).
    ///
    /// [`Storage::commit`] is called next and the transaction leaves
    /// [`TransactionManager::active_sessions`] afterwards, under the same lock (module
    /// documentation above). A transaction that wrote nothing commits fine: `storage` accepts
    /// it (unit test `horizon_and_xmin_fall_back_on_the_next_id`). A snapshot another
    /// transaction took while `txn` was open keeps hiding the writes of `txn` after the
    /// commit; the snapshot it takes next shows them (`tests/txn_basic.rs`,
    /// `other_in_progress_is_invisible`).
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `txn` is not open here — a second `commit` of the same
    /// handle, or a handle from another manager (`tests/txn_basic.rs`,
    /// `commit_twice_is_a_bug`) — or when a deferred drop names an object `storage` no longer
    /// knows (`tests/txn_actions.rs`, `a_deferred_drop_of_a_gone_table_is_a_bug`). The error
    /// of [`Storage::commit`] otherwise. In each of these cases the transaction stays open
    /// (`tests/txn_basic.rs`, `a_refused_storage_commit_keeps_the_transaction_open`), and the
    /// deferred drops already run are not undone.
    pub fn commit(&self, txn: TxnHandle) -> SqlResult<()> {
        self.finish("commit", &txn, true)
    }

    /// Rolls `txn` back: compensates the objects it created, undoes its writes in `storage`,
    /// then closes it.
    ///
    /// The [`RollbackAction`]s registered with
    /// [`TransactionManager::register_on_rollback`] run first, in **reverse** registration
    /// order (`tests/txn_actions.rs`, `create_compensated_on_rollback`,
    /// `rollback_actions_run_in_reverse_order`), and the [`CommitAction`]s are dropped unrun
    /// (`tests/txn_actions.rs`, `a_rolled_back_deferred_drop_keeps_the_table`). Then the same
    /// order and the same lock as [`TransactionManager::commit`]: after the call, a row `txn`
    /// inserted is hidden from a later snapshot and a row it deleted is back
    /// (`tests/txn_basic.rs`, `rollback_hides_the_row`).
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `txn` is not open here or when a compensation names an
    /// object `storage` no longer knows; the error of [`Storage::rollback`] otherwise. In
    /// each of these cases the transaction stays open.
    ///
    /// A compensation that fails in the middle of the reverse run stops it there: the ones
    /// already run stay run, the ones registered before it have not run, and the log keeps
    /// them all. A second `rollback` therefore replays the compensations already run and
    /// fails again on them; what closes such a transaction is a `commit`, which runs no
    /// compensation and leaves behind the objects the transaction created
    /// (`tests/txn_actions.rs`, `a_failed_compensation_leaves_the_transaction_to_a_commit`:
    /// three compensations, the second one refused, the first table still there after the
    /// commit). Resuming the run where it stopped would need the log to keep what it has
    /// run, which it does not do.
    pub fn rollback(&self, txn: TxnHandle) -> SqlResult<()> {
        self.finish("rollback", &txn, false)
    }

    /// Runs the deferred actions of `txn` and closes it through `storage`, committing it when
    /// `commit` is true.
    ///
    /// The lock is held across the `storage` call on purpose, see the module documentation.
    fn finish(&self, method: &str, txn: &TxnHandle, commit: bool) -> SqlResult<()> {
        let mut state = self.lock();
        let Some(pos) = state.position(txn.id) else {
            return Err(not_open(method, txn.id));
        };
        if commit {
            state.open[pos].log.run_on_commit(self.storage.as_ref())?;
            self.storage.commit(txn.id)?;
        } else {
            state.open[pos].log.run_on_rollback(self.storage.as_ref())?;
            self.storage.rollback(txn.id)?;
        }
        // After `storage`: a waiter woken here finds a transaction whose outcome `storage`
        // has already taken. An error above returns before this line,
        // so a transaction that stays open keeps its locks (`tests/lock.rs`,
        // `a_refused_commit_keeps_the_locks`).
        self.locks.release_all(txn.id);
        state.open.remove(pos);
        Ok(())
    }

    /// Marks a savepoint in `txn` and returns its identifier, the one `SAVE TRANSACTION`
    /// names.
    ///
    /// Calls [`Storage::savepoint`], which marks the writes made so far, and keeps the
    /// identifier it returns in the action log of `txn` at its current end. From there,
    /// [`TransactionManager::rollback_to`] of this savepoint undoes the writes **and**
    /// compensates the objects created after this call (`tests/txn_actions.rs`,
    /// `savepoint_undoes_later_creates`).
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `txn` is not open here; the error of [`Storage::savepoint`]
    /// otherwise, in which case no mark is kept.
    pub fn savepoint(&self, txn: &TxnHandle) -> SqlResult<SavepointId> {
        let mut state = self.lock();
        if state.position(txn.id).is_none() {
            return Err(not_open("savepoint", txn.id));
        }
        let sp = self.storage.savepoint(txn.id)?;
        if let Some(log) = state.log_mut(txn.id) {
            log.mark(sp);
        }
        Ok(sp)
    }

    /// Rolls `txn` back to the savepoint `sp`, which stays valid.
    ///
    /// Two halves, in this order: the compensations registered after `sp` run, last
    /// registered first, then [`Storage::rollback_to`] undoes the writes made after `sp`.
    /// The registrations made after `sp` — compensations and deferred drops alike — are then
    /// forgotten, so a later `commit` or `rollback` of `txn` does not run them
    /// (`tests/txn_actions.rs`, `nested_savepoints_are_ordered`). What was registered
    /// **before** `sp` is untouched and still runs when `txn` ends
    /// (`tests/txn_actions.rs`, `savepoint_undoes_later_creates`).
    ///
    /// `sp` itself stays usable, so a caller can return to it several times, as
    /// [`Storage::rollback_to`] allows; the savepoints taken after it become unknown here.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `txn` is not open here, when `sp` is not a savepoint this
    /// manager holds for `txn` — another transaction's, a made-up one, or one invalidated by
    /// an earlier call (`tests/txn_actions.rs`, `rollback_to_an_unknown_savepoint_is_a_bug`) —
    /// or when a compensation names an object `storage` no longer knows; the error of
    /// [`Storage::rollback_to`] otherwise.
    ///
    /// That last error arrives **after** the log has been cut: the compensations of the
    /// entries after `sp` have run and those entries are already forgotten, so the objects
    /// created after `sp` are gone while `storage` refused to undo the writes, and calling
    /// `rollback_to(sp)` again returns the same error and compensates nothing more
    /// (`tests/txn_actions.rs`, `a_refused_storage_rollback_to_leaves_the_log_truncated`).
    /// A compensation that fails stops the run, in which case nothing is forgotten, with the
    /// consequences described on [`TransactionManager::rollback`].
    pub fn rollback_to(&self, txn: &TxnHandle, sp: SavepointId) -> SqlResult<()> {
        let mut state = self.lock();
        let Some(pos) = state.position(txn.id) else {
            return Err(not_open("rollback_to", txn.id));
        };
        let Some(at) = state.open[pos].log.position_of(sp) else {
            return Err(unknown_savepoint(sp, txn.id));
        };
        state.open[pos]
            .log
            .unwind_after(at, self.storage.as_ref())?;
        self.storage.rollback_to(txn.id, sp)
    }

    /// Registers an action for `txn` to run when it commits, in place of a `storage.drop_*`
    /// call the caller does not make now.
    ///
    /// This is how `DROP` becomes transactional at the SQL level while
    /// [`Storage::drop_table`] and its siblings take effect at once: the object keeps
    /// answering reads until the commit (`tests/txn_actions.rs`,
    /// `drop_deferred_until_commit`), and a `ROLLBACK` leaves it alone.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `txn` is not open here.
    pub fn register_on_commit(&self, txn: &TxnHandle, action: CommitAction) -> SqlResult<()> {
        let mut state = self.lock();
        match state.log_mut(txn.id) {
            Some(log) => {
                log.push_on_commit(action);
                Ok(())
            }
            None => Err(not_open("register_on_commit", txn.id)),
        }
    }

    /// Registers the action that compensates, at rollback, an object `txn` has just created in
    /// `storage`.
    ///
    /// This is how `CREATE` becomes transactional at the SQL level: the object exists at
    /// once, and a `ROLLBACK` — whole or down to a savepoint taken before this call — drops
    /// it again (`tests/txn_actions.rs`, `create_compensated_on_rollback`). A `COMMIT` leaves
    /// it alone.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `txn` is not open here.
    pub fn register_on_rollback(&self, txn: &TxnHandle, action: RollbackAction) -> SqlResult<()> {
        let mut state = self.lock();
        match state.log_mut(txn.id) {
            Some(log) => {
                log.push_on_rollback(action);
                Ok(())
            }
            None => Err(not_open("register_on_rollback", txn.id)),
        }
    }

    /// The snapshot the next statement of `txn` reads through.
    ///
    /// Built from the state at the moment of the call:
    ///
    /// - `own` is `txn.id`, so the transaction sees its own writes before it commits
    ///   (`vauban_storage::Snapshot::is_settled`, `tests/txn_basic.rs`,
    ///   `own_writes_are_visible`);
    /// - `xmax` is the identifier the next [`TransactionManager::begin`] would hand out, so a
    ///   transaction that starts after the call is hidden;
    /// - `active` lists the open transactions other than `txn`, in increasing order, so their
    ///   uncommitted writes are hidden (`tests/txn_basic.rs`,
    ///   `other_in_progress_is_invisible`);
    /// - `xmin` is the smallest open identifier, `txn` included, or `xmax` when the manager
    ///   holds nothing open. Transactions below `xmin` are finished, the invariant
    ///   `vauban_storage::Snapshot` documents and what
    ///   [`TransactionManager::snapshot_horizon`] feeds to `Storage::vacuum`.
    ///
    /// # One per statement, or one per transaction
    ///
    /// Under [`VersioningMode::Locking`] and [`VersioningMode::StatementSnapshot`] the
    /// snapshot is rebuilt at each call, which is the `READ COMMITTED` behaviour: a
    /// transaction committed between two calls is hidden from the first snapshot and shown
    /// by the second (`tests/txn_basic.rs`, `other_in_progress_is_invisible`;
    /// `tests/snapshot_modes.rs`, `statement_snapshot_is_frozen_for_the_statement`). The
    /// caller reads its whole statement through the one value it got, and asks for the next
    /// at the start of the next statement. `REPEATABLE READ` and `SERIALIZABLE` hold their
    /// shared locks instead (`isolation.rs`).
    ///
    /// Under [`VersioningMode::TxnSnapshot`] the first call builds the snapshot and pins it,
    /// and the later calls hand out the same value, `xmax` and `active` included, whatever
    /// committed in between (`tests/snapshot_modes.rs`,
    /// `snapshot_txn_is_frozen_until_commit`). The pin is placed by this call and not by
    /// [`TransactionManager::begin_in`]: a transaction that has read nothing yet sees what
    /// committed since it opened (`tests/snapshot_modes.rs`,
    /// `the_snapshot_is_pinned_by_the_first_statement_not_by_begin`). The mode is read at
    /// each call, so a transaction opened at `SNAPSHOT` on a database whose option was off
    /// gets a fresh snapshot per call (`tests/txn_basic.rs`,
    /// `the_five_levels_are_served_as_read_committed`); a snapshot once pinned stays pinned
    /// (`tests/snapshot_modes.rs`, `a_flip_does_not_unpin_the_snapshot`).
    ///
    /// A handle the manager no longer holds open gets a snapshot built from the state, as
    /// under `Locking` (unit test `horizon_and_xmin_fall_back_on_the_next_id`).
    pub fn statement_snapshot(&self, txn: &TxnHandle) -> Snapshot {
        let mut state = self.lock();
        let Some(pos) = state.position(txn.id) else {
            return state.snapshot_for(txn.id);
        };
        if let Some(pinned) = &state.open[pos].pinned {
            return pinned.clone();
        }
        let mode = self
            .versioning_options(state.open[pos].db)
            .mode_for(txn.isolation);
        if mode == VersioningMode::TxnSnapshot {
            let pinned = state.snapshot_for(txn.id);
            state.open[pos].pinned = Some(pinned.clone());
            return pinned;
        }
        let snap = state.snapshot_for(txn.id);
        state.open[pos].last = Some(snap.clone());
        snap
    }

    /// Takes an exclusive lock on one row for `txn`, waiting as `timeout` allows.
    ///
    /// The short path over [`LockManager::lock`]: `LockResource::Row(table, id)`,
    /// [`LockMode::X`], and a fresh [`LockWait`] token this method keeps to itself — its
    /// signature has no parameter for one, so a caller that wants its wait interruptible
    /// goes through [`TransactionManager::locks`] with a token of its own
    /// (`tests/lock.rs`, `a_cancelled_wait_returns_cancelled`).
    ///
    /// Re-entrant: a transaction that already holds the row does not wait for itself
    /// (`tests/lock.rs`, `reentrant_lock_is_free`). The lock is given back by
    /// [`TransactionManager::commit`] and [`TransactionManager::rollback`]
    /// (`tests/lock.rs`, `commit_releases_locks`, `rollback_releases_locks`), not at the end
    /// of the statement: releasing a shared lock earlier is the `READ COMMITTED` rule of
    /// `isolation.rs`.
    ///
    /// # Errors
    ///
    /// The error of [`LockManager::lock`] when the lock was refused: the timeout ran out,
    /// a cycle was found, or the wait was cut short (`deadlock::to_error`).
    pub fn lock_row(
        &self,
        txn: &TxnHandle,
        table: TableId,
        id: RowId,
        timeout: LockTimeout,
    ) -> SqlResult<()> {
        self.locks.lock(
            txn.id,
            LockResource::Row(table, id),
            LockMode::X,
            timeout,
            &LockWait::none(),
        )
    }

    /// Decides what `txn` may do with a row it wants to write, from the snapshot it last got
    /// and the latest version of the row.
    ///
    /// Called after [`TransactionManager::write_lock`] took the `X`, so the writer of the
    /// latest version has committed or is `txn` itself. The snapshot compared against is
    /// the one pinned under [`VersioningMode::TxnSnapshot`], or the one the last
    /// [`TransactionManager::statement_snapshot`] call handed out under the two other
    /// modes; `writer` is the transaction [`Storage::latest_version`] names, and settled
    /// means [`Snapshot::is_settled`] on that snapshot.
    ///
    /// | shape | `TxnSnapshot` | `StatementSnapshot`, `Locking` |
    /// |---|---|---|
    /// | no snapshot handed out to `txn` yet | `Proceed` | `Proceed` |
    /// | the row is unknown to `storage` | `Proceed` | `Proceed` |
    /// | `writer` is `txn` | `Proceed` | `Proceed` |
    /// | `writer` is settled: the snapshot showed this version | `Proceed` | `Proceed` |
    /// | `writer` is not settled: the row changed after the snapshot | `Conflict` | `Reread(id)` |
    ///
    /// The first row is what SQL Server does for a `SNAPSHOT` transaction that has read
    /// nothing before another transaction changed the row (`snapshot_modes.rs`, the table
    /// of what SQL Server does), and it is also why a transaction that never asked for a
    /// snapshot is left to the barrier of `storage`: a write on a version another
    /// transaction has already superseded is refused by [`Storage::update`] /
    /// [`Storage::delete`] as a caller bug (`tests/txn_basic.rs`,
    /// `check_write_conflict_lets_the_writer_proceed`). The last row is the `SNAPSHOT`
    /// conflict, which the executor reports as 3960 (`tests/snapshot_modes.rs`,
    /// `update_conflict_is_reported`, `no_conflict_when_rows_differ`,
    /// `a_delete_after_the_snapshot_is_a_conflict`), and the re-read of `READ COMMITTED`,
    /// where the caller reads the row again through a fresh snapshot and decides again
    /// (`tests/snapshot_modes.rs`, `a_row_changed_since_the_statement_is_reread`).
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `txn` is not open here; the error of
    /// [`Storage::latest_version`] otherwise, which is reached only once a snapshot has been
    /// handed out (`tests/txn_shape.rs`,
    /// `lock_row_and_check_write_conflict_answer_without_contention`, on a table `storage`
    /// does not know).
    pub fn check_write_conflict(
        &self,
        txn: &TxnHandle,
        table: TableId,
        id: RowId,
    ) -> SqlResult<WriteDecision> {
        let state = self.lock();
        let Some(pos) = state.position(txn.id) else {
            return Err(not_open("check_write_conflict", txn.id));
        };
        let open = &state.open[pos];
        let Some(snap) = open.pinned.as_ref().or(open.last.as_ref()) else {
            return Ok(WriteDecision::Proceed);
        };
        let Some((writer, _)) = self.storage.latest_version(table, id)? else {
            return Ok(WriteDecision::Proceed);
        };
        if writer == txn.id || snap.is_settled(writer, &|t| state.status(t)) {
            return Ok(WriteDecision::Proceed);
        }
        let mode = self.versioning_options(open.db).mode_for(txn.isolation);
        Ok(match mode {
            VersioningMode::TxnSnapshot => WriteDecision::Conflict,
            VersioningMode::StatementSnapshot | VersioningMode::Locking => {
                WriteDecision::Reread(id)
            }
        })
    }

    /// The horizon to pass to [`Storage::vacuum`]: the smallest identifier still open, or the
    /// identifier the next [`TransactionManager::begin`] would hand out when the manager
    /// holds nothing open (`tests/txn_basic.rs`,
    /// `snapshot_horizon_follows_the_oldest_open_transaction`).
    ///
    /// # What this value does not cover
    ///
    /// `Storage::vacuum` asks for a horizon at or below the `xmin` of the snapshots still in
    /// use. The manager keeps no list of handed-out snapshots, so this value is read off the
    /// open transactions instead, and it can sit above the `xmin` of a snapshot a caller is
    /// still reading through — the transaction that took that snapshot need not have closed.
    /// Two shapes are asserted:
    ///
    /// - the horizon equals the `xmin` of the snapshot of a transaction still open, when
    ///   nothing older than the reader closed in between (`tests/txn_basic.rs`,
    ///   `horizon_matches_the_xmin_of_a_live_snapshot`);
    /// - it passes that `xmin` when the **oldest** transaction open at the time of the
    ///   snapshot closes after it was taken: the snapshot keeps listing that transaction in
    ///   `active`, the horizon stops counting it, and a `vacuum` at that horizon takes away
    ///   a version the snapshot was showing (`tests/txn_basic.rs`,
    ///   `snapshot_horizon_can_pass_the_xmin_of_a_snapshot_in_use`). Closing a transaction
    ///   older than the reader but not the oldest one leaves the horizon at that `xmin`
    ///   (unit test `horizon_holds_when_a_middle_transaction_closes`).
    ///
    /// A snapshot a caller keeps after its own transaction closed falls outside it too.
    /// Calling `Storage::vacuum` with this value therefore waits for a snapshot registry;
    /// the method is here so that its callers can name it.
    pub fn snapshot_horizon(&self) -> TxnId {
        let state = self.lock();
        state.oldest_open().unwrap_or(TxnId(state.next_id))
    }

    /// The transactions started and not yet closed, oldest first, for the `sys.dm_tran_*`
    /// views.
    ///
    /// [`TransactionManager::begin`] appends to the list;
    /// [`TransactionManager::commit`] and [`TransactionManager::rollback`] remove their entry
    /// once `storage` has accepted the outcome (`tests/txn_basic.rs`,
    /// `commit_and_rollback_close_the_transaction`). `locks_held` and `state` are read off
    /// one [`TransactionManager::active_locks`] taken while the list is held, so the two
    /// fields of one call describe the lock table at one instant (`tests/info.rs`,
    /// `a_waiting_txn_is_visible`); `deadlock_priority` is the value
    /// [`TransactionManager::set_deadlock_priority`] kept, `0` without a call.
    pub fn active_sessions(&self) -> Vec<TxnInfo> {
        let state = self.lock();
        let lines = self.active_locks();
        let monitor = self.locks().monitor();
        state
            .open
            .iter()
            .map(|open| {
                let id = open.handle.id;
                let mine = lines.iter().filter(|line| line.txn == id);
                let locks_held = mine
                    .clone()
                    .filter(|line| line.status == LockStatus::Grant)
                    .count();
                let txn_state = mine
                    .clone()
                    .find(|line| line.status != LockStatus::Grant)
                    .map_or(TxnState::Active, |line| TxnState::Waiting {
                        on: line.resource,
                    });
                TxnInfo {
                    id,
                    isolation: open.handle.isolation,
                    state: txn_state,
                    began_at: open.began_at,
                    locks_held: u32::try_from(locks_held).unwrap_or(u32::MAX),
                    deadlock_priority: monitor.priority(id),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_storage::MemoryStorage;

    /// The documented bound of [`TransactionManager::begin`]: a saturated counter repeats
    /// `TxnId(u64::MAX)` instead of wrapping back to an identifier already handed out.
    #[test]
    fn saturated_counter_repeats_its_bound() {
        let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
        mgr.state.lock().expect("fresh mutex").next_id = u64::MAX;
        let last = mgr.begin(IsolationLevel::ReadCommitted);
        let beyond = mgr.begin(IsolationLevel::ReadCommitted);
        assert_eq!(last.id, TxnId(u64::MAX));
        assert_eq!(beyond.id, TxnId(u64::MAX), "saturation, not wrap-around");
    }

    /// `xmin` and the horizon fall back on the next identifier once the last transaction
    /// closed, so that `Storage::vacuum` may prune what it left behind.
    #[test]
    fn horizon_and_xmin_fall_back_on_the_next_id() {
        let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
        let txn = mgr.begin(IsolationLevel::ReadCommitted);
        mgr.commit(txn.clone()).expect("commit of an empty txn");
        assert_eq!(mgr.snapshot_horizon(), TxnId(2));
        let snap = mgr.statement_snapshot(&txn);
        assert_eq!(snap.xmin, TxnId(2));
        assert_eq!(snap.xmax, TxnId(2));
    }

    /// The bound of the second bullet of [`TransactionManager::snapshot_horizon`]: what
    /// makes the horizon pass the `xmin` of a snapshot in use is the **oldest** open
    /// transaction closing, not any transaction older than the reader.
    #[test]
    fn horizon_holds_when_a_middle_transaction_closes() {
        let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
        let first = mgr.begin(IsolationLevel::ReadCommitted);
        let middle = mgr.begin(IsolationLevel::ReadCommitted);
        let reader = mgr.begin(IsolationLevel::ReadCommitted);
        let held = mgr.statement_snapshot(&reader);
        assert_eq!(held.xmin, TxnId(1));
        mgr.commit(middle).expect("commit the middle transaction");
        assert_eq!(
            mgr.snapshot_horizon(),
            held.xmin,
            "the oldest open transaction still holds the horizon down"
        );
        mgr.commit(first).expect("commit the oldest transaction");
        assert!(
            mgr.snapshot_horizon() > held.xmin,
            "closing the oldest one is what moves the horizon past the snapshot"
        );
    }
}
