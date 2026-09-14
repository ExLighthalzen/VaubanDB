//! The small public enumerations of the module: isolation level, lock timeout, outcome of a
//! write-conflict check. Their variants are the contract the other modules compile against.
//!
//! [`WriteDecision`] is still a declaration: `TransactionManager::check_write_conflict`
//! answers [`WriteDecision::Proceed`] (`tests/txn_basic.rs`,
//! `check_write_conflict_lets_the_writer_proceed`). [`LockTimeout`] bounds the waits of the
//! lock manager (`tests/lock.rs`).

use vauban_storage::RowId;

/// The isolation level of a transaction, as named by `SET TRANSACTION ISOLATION LEVEL`.
///
/// The five variants are the five levels of the T-SQL statement. The behaviour attached to
/// each level — shared locks held or released, snapshot taken per statement or per
/// transaction — is decided by the read policy of `isolation.rs`.
///
/// # What the snapshot serves
///
/// `TransactionManager::begin` accepts the five variants and keeps the one it was given on
/// the handle. The snapshot served is that of [`IsolationLevel::ReadCommitted`]: a fresh one
/// per statement, for the five of them alike (`tests/txn_basic.rs`,
/// `the_five_levels_are_served_as_read_committed`). What tells them apart is the lock policy
/// — no shared lock for `ReadUncommitted`, shared locks held to the end for `RepeatableRead`
/// and `Serializable` (`tests/isolation.rs`) — and, once the database options are served, a
/// snapshot pinned at `begin` for `Snapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IsolationLevel {
    /// `READ UNCOMMITTED`: no shared lock, dirty reads allowed.
    ReadUncommitted,
    /// `READ COMMITTED`: the default level of a SQL Server session.
    ReadCommitted,
    /// `REPEATABLE READ`: shared locks held until the end of the transaction.
    RepeatableRead,
    /// `SERIALIZABLE`: `REPEATABLE READ` plus the range locks that forbid phantom rows.
    Serializable,
    /// `SNAPSHOT`: one snapshot taken when the transaction starts; needs the database option
    /// `ALLOW_SNAPSHOT_ISOLATION`.
    Snapshot,
}

/// How long a writer waits for a row lock before giving up, the value of `SET LOCK_TIMEOUT`.
///
/// `Millis(0)` and [`LockTimeout::NoWait`] are kept apart because they come from two
/// different places: the session setting `SET LOCK_TIMEOUT` and the `NOWAIT` table hint.
/// What each of them reports is decided by the lock manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockTimeout {
    /// Wait forever, the default of a session (`SET LOCK_TIMEOUT -1`).
    Infinite,
    /// Wait at most this many milliseconds, then fail.
    Millis(u32),
    /// Do not wait, from the `NOWAIT` table hint.
    NoWait,
}

/// What a writer must do with a row, once the manager has checked it for conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteDecision {
    /// The row may be written as it was read.
    Proceed,
    /// The row changed since it was read: read this [`RowId`] again, then decide again.
    Reread(RowId),
    /// The write cannot proceed under this isolation level.
    Conflict,
}
