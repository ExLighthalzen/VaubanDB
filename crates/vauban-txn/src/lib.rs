#![deny(missing_docs)]
//! Crate `vauban-txn`: transactions — identifiers, isolation levels, MVCC snapshots, write
//! locks, conflict detection, savepoints.
//!
//! # Life cycle
//!
//! The public types and the shape of the API are what `catalog`, `binder`, `executor` and
//! `session` name in their signatures. The life cycle of a transaction runs over the
//! `vauban_storage::Storage` contract:
//!
//! - [`TransactionManager::begin`] hands out [`TxnId`](vauban_storage::TxnId)s from `1`, one
//!   more at each call up to the bound documented on that method, and keeps the list of open
//!   transactions returned by [`TransactionManager::active_sessions`];
//! - [`TransactionManager::commit`] and [`TransactionManager::rollback`] call `storage` and
//!   then close the transaction, in that order;
//! - [`TransactionManager::statement_snapshot`] builds the
//!   [`Snapshot`](vauban_storage::Snapshot) a statement reads through, and
//!   [`TransactionManager::snapshot_horizon`] the identifier `Storage::vacuum` takes.
//!
//! Savepoints and deferred DDL sit on top of it (`tests/txn_actions.rs`):
//!
//! - [`TransactionManager::savepoint`] and [`TransactionManager::rollback_to`] wrap
//!   `Storage::savepoint` / `Storage::rollback_to`;
//! - [`TransactionManager::register_on_commit`] defers a [`CommitAction`] — a `DROP` the
//!   caller does not apply yet — to the commit, in registration order;
//! - [`TransactionManager::register_on_rollback`] keeps the [`RollbackAction`] that
//!   compensates a `CREATE` already applied to `storage`, run in reverse order at rollback;
//! - `rollback_to` runs the compensations registered after the savepoint and forgets the
//!   registrations made after it.
//!
//! The **versioning** engine is carried by `storage`: a transaction sees its own writes
//! before it commits and not those of another open one (`tests/txn_basic.rs`).
//!
//! # The lock manager
//!
//! [`LockManager`] is the **locking** engine, the part that carries no SQL meaning: the seven
//! [`LockMode`]s and their compatibility matrix, [`LockResource::Row`] and
//! [`LockResource::Table`], a FIFO queue per resource, conversion of a mode already held, and
//! a wait bounded by [`LockTimeout`] or cut short by a [`LockWait`] token (`tests/lock.rs`).
//! [`TransactionManager::lock_row`] takes an `X` on the row and `commit` / `rollback` give the
//! locks back (`tests/lock.rs`, `commit_releases_locks`, `rollback_releases_locks`).
//!
//! What that manager does **not** decide is filled in around it: the numbers 1222 and 1205
//! and the wait-for graph (`deadlock.rs`), the two database options and the `SNAPSHOT`
//! conflict 3960 (`snapshot_modes.rs`), the table hints and escalation (`table_lock.rs`), and
//! the `sys.dm_tran_*` views (`info.rs`). Until the latter three are served, the
//! [`Snapshot`](vauban_storage::Snapshot) [`TransactionManager::statement_snapshot`] hands
//! out is rebuilt at each call for the five [`IsolationLevel`]s alike
//! (`tests/txn_basic.rs`, `the_five_levels_are_served_as_read_committed`) and
//! [`TransactionManager::check_write_conflict`] answers [`WriteDecision::Proceed`].
//!
//! The levels are a **policy** over that manager, in `isolation.rs`:
//! [`TransactionManager::read_lock`] takes the mode a level and a [`LockIntent`] ask for and
//! answers [`ReadAccess`], [`TransactionManager::end_row_read`] gives back what a
//! `READ COMMITTED` read took, [`TransactionManager::write_lock`] takes the `X` of a write,
//! and [`TransactionManager::effective_level`] says which level a read is served at
//! (`tests/isolation.rs`).
//!
//! The **schema** modes are at work in `schema_lock.rs`:
//! [`TransactionManager::schema_stability_lock`] takes the `Sch-S` of a statement that reads
//! the shape of a table and [`TransactionManager::schema_modify_lock`] the `Sch-M` of one
//! that changes it, both on [`LockResource::Table`] and both held until the transaction
//! commits or rolls back, whichever level it was opened at (`tests/schema_lock.rs`,
//! `nolock_still_takes_sch_s`, `commit_releases_the_schema_lock`). Which statement calls
//! which is the table in the documentation of that module; placing the calls belongs to the
//! executor.
//!
//! # File map
//!
//! | File | Content |
//! |---|---|
//! | `ids.rs` | [`IsolationLevel`], [`LockTimeout`], [`WriteDecision`] |
//! | `handle.rs` | [`TxnHandle`], [`TxnInfo`] |
//! | `manager.rs` | [`TransactionManager`]: life cycle, snapshots and horizon; savepoints, registrations and deferred actions; the [`LockManager`] behind `lock_row` and the release at commit |
//! | `actions.rs` | [`CommitAction`], [`RollbackAction`] and the log that holds them by savepoint |
//! | `lock.rs` | [`LockManager`], [`LockMode`], [`LockResource`], [`LockOutcome`], [`LockWait`] |
//! | `deadlock.rs` | wait-for graph, victim, 1222 and 1205 |
//! | `isolation.rs` | [`LockIntent`], [`ReadAccess`], and the four methods that decide which mode a read takes and when it is released |
//! | `snapshot_modes.rs` | `READ_COMMITTED_SNAPSHOT`, `ALLOW_SNAPSHOT_ISOLATION`, 3960 — empty |
//! | `table_lock.rs` | `TABLOCK`, `TABLOCKX`, `ROWLOCK`, escalation — empty |
//! | `schema_lock.rs` | [`TransactionManager::schema_stability_lock`] and [`TransactionManager::schema_modify_lock`], and the table of who takes which |
//! | `info.rs` | `TxnInfo` and `LockInfo` for `sys.dm_tran_*` — empty |
//!
//! Visibility is served by two engines: MVCC snapshots for what a statement reads, and locks
//! for what a transaction holds against others.

mod actions;
mod deadlock;
mod handle;
mod ids;
mod info;
mod isolation;
mod lock;
mod manager;
mod schema_lock;
mod snapshot_modes;
mod table_lock;

pub use actions::{CommitAction, RollbackAction};
pub use handle::{TxnHandle, TxnInfo};
pub use ids::{IsolationLevel, LockTimeout, WriteDecision};
pub use isolation::{LockIntent, ReadAccess};
pub use lock::{LockManager, LockMode, LockOutcome, LockResource, LockWait};
pub use manager::TransactionManager;
