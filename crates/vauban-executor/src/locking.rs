//! Reading and writing under locks: which lock a row read takes, when it is given back,
//! and the exclusive lock a row write takes.
//!
//! # Where the decision is made
//!
//! `txn` owns the policy — the mode a level and a [`LockIntent`] ask for, the queue, the
//! wait and the numbers a refusal carries. This file owns the **translation**: the
//! [`LockHints`] the plan carries on a `TableScan` or an `IndexSeek` become a
//! [`LockIntent`], and the [`ReadAccess`] that comes back becomes the [`RowVisibility`] an
//! operator acts on. Nothing here queues, waits or picks a mode.
//!
//! | word on the table reference | what it becomes |
//! |---|---|
//! | `NOLOCK`, `READUNCOMMITTED` | [`IsolationLevel::ReadUncommitted`] |
//! | `READCOMMITTED`, `READCOMMITTEDLOCK` | [`IsolationLevel::ReadCommitted`] |
//! | `REPEATABLEREAD` | [`IsolationLevel::RepeatableRead`] |
//! | `SERIALIZABLE`, `HOLDLOCK` | [`IsolationLevel::Serializable`] |
//! | `SNAPSHOT` | [`IsolationLevel::Snapshot`] |
//! | `UPDLOCK` | [`LockIntent::updlock`] |
//! | `XLOCK` | [`LockIntent::xlock`] |
//! | `READPAST` | [`LockIntent::readpast`] |
//! | `NOWAIT` | [`LockIntent::nowait`] |
//! | `TABLOCK`, `TABLOCKX` | [`LockIntent::tablock`], [`LockIntent::tablockx`], carried without effect |
//! | `ROWLOCK`, `PAGLOCK` | read by nothing: granularity is not chosen here |
//!
//! `HOLDLOCK` and `SERIALIZABLE` reach this file as one field, `LockHints::serializable`,
//! because the binder reads the two words into it; the level covers both, and
//! [`LockIntent::holdlock`], which exists for a caller that has the word and not the level,
//! stays `false` (`translate_the_five_level_words`). The two words a level hint cannot be
//! written with at once are refused before the plan is built, so the order the five are
//! read in decides nothing a client can reach.
//!
//! # A locked read is served per row, not by the snapshot of the statement
//!
//! A level that locks is not a level that versions: what it answers is the state of the row
//! **when the lock was granted**, not the state the statement started on. A reader that
//! waited for an exclusive lock and got it therefore answers the value the writer committed,
//! not the one it would have read had it not waited (`tests/locking.rs`,
//! `read_committed_blocks_on_a_written_row`). So a [`ReadAccess::Locked`] row is served from
//! [`Storage::latest_version`](vauban_storage::Storage::latest_version), which ignores
//! visibility: [`RowVisibility::Latest`]. That version is a settled one, because writing the
//! row takes an exclusive lock this read now holds against.
//!
//! [`ReadAccess::Dirty`] takes the same path and no lock, which is what makes it dirty: the
//! latest version of a row another transaction has written without committing is that
//! uncommitted version (`tests/locking.rs`, `nolock_reads_the_uncommitted_row`,
//! `nolock_sees_a_rolled_back_write`). Nothing else could serve it: a
//! [`Snapshot`](vauban_storage::Snapshot) cannot show an uncommitted version, since
//! `Snapshot::is_settled` reads the status of the writer and an in-progress writer is
//! unsettled whatever `xmin`, `xmax` and `active` say. The iteration is therefore the same
//! one under every word ([`snapshot_for_scan`]) and it is the **content** of the row that
//! the level and the words decide.
//!
//! [`ReadAccess::Versioned`], the answer of a level the database options serve from the row
//! versions, is the one that keeps the version the iteration produced:
//! [`RowVisibility::Visible`].
//!
//! A row whose sole stored version is an uncommitted `INSERT` of another transaction is **not**
//! served by either path: no iterator hands its identifier out, so there is nothing to read
//! the latest version of and nothing to lock (gap `uncommitted-insert-invisible-to-a-reader`).
//!
//! # A context without a transaction takes no lock
//!
//! A lock belongs to a transaction: [`ExecContext::handle`] is what `read_lock` and
//! `write_lock` name it by. A context built without a handle has no transaction to lock in,
//! so both answer as if no word had been written and no level applied
//! (`locking::tests::a_context_without_a_transaction_takes_no_lock`). `session` builds every
//! statement context with a handle (`batch.rs`).

use vauban_binder::LockHints;
use vauban_errors::SqlResult;
use vauban_storage::{RowId, Snapshot, TableId};
use vauban_txn::{IsolationLevel, LockIntent, ReadAccess};

use crate::context::ExecContext;

/// What the caller may do with the row it asked to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowVisibility {
    /// Hand over the version the iterator produced: the answer of a level served from the
    /// row versions.
    Visible,
    /// Hand over the latest version of the row instead: the state it is in now that the
    /// lock is held, or the uncommitted state a dirty read asked for (module
    /// documentation).
    Latest,
    /// Leave the row out: `READPAST` on a row another transaction holds.
    Skip,
}

/// The [`LockIntent`] of `hints`, the shape `txn` takes.
fn intent_of(hints: &LockHints) -> LockIntent {
    let level = if hints.nolock {
        Some(IsolationLevel::ReadUncommitted)
    } else if hints.readcommitted {
        Some(IsolationLevel::ReadCommitted)
    } else if hints.repeatableread {
        Some(IsolationLevel::RepeatableRead)
    } else if hints.serializable {
        Some(IsolationLevel::Serializable)
    } else if hints.snapshot {
        Some(IsolationLevel::Snapshot)
    } else {
        None
    };
    LockIntent {
        level,
        holdlock: false,
        updlock: hints.updlock,
        xlock: hints.xlock,
        readpast: hints.readpast,
        nowait: hints.nowait,
        tablock: hints.tablock,
        tablockx: hints.tablockx,
    }
}

/// Takes the lock the level of the transaction and `hints` ask for on one row, and says
/// what the caller may do with it.
///
/// Paired with [`end_row_read`] on the same thread, which gives back what a
/// `READ COMMITTED` read took: `txn` keeps the decision between the two per thread, and the
/// engine runs one statement on one thread.
///
/// A context with no transaction answers [`RowVisibility::Visible`] without a lock (module
/// documentation).
///
/// # Errors
///
/// What `txn` raises for a lock it refused: 1222 for a `NOWAIT` on a row another
/// transaction holds (`tests/locking.rs`, `nowait_is_1222`), 1205 for a deadlock. The
/// internal error 50000 of a context with no transaction manager.
pub(crate) fn read_lock(
    ctx: &mut ExecContext<'_>,
    table: TableId,
    id: RowId,
    hints: &LockHints,
) -> SqlResult<RowVisibility> {
    let (Some(manager), Some(handle)) = (ctx.txn, ctx.handle) else {
        return Ok(RowVisibility::Visible);
    };
    Ok(
        match manager.read_lock(handle, table, id, &intent_of(hints))? {
            ReadAccess::Locked | ReadAccess::Dirty => RowVisibility::Latest,
            ReadAccess::Versioned => RowVisibility::Visible,
            ReadAccess::Skip => RowVisibility::Skip,
        },
    )
}

/// Gives back the lock of the row read [`read_lock`] opened, for the levels that hold it to
/// the end of the row rather than to the end of the transaction.
///
/// Called once per row handed over or left out, on the thread that read it. A row read at
/// `REPEATABLE READ`, under `UPDLOCK` or under `XLOCK` keeps its lock: `txn` decides that,
/// not this call (`tests/locking.rs`, `repeatable_read_holds_its_share_lock`).
///
/// # Errors
///
/// What `txn` raises when it is asked to give back a lock the transaction does not hold.
pub(crate) fn end_row_read(ctx: &mut ExecContext<'_>, table: TableId, id: RowId) -> SqlResult<()> {
    let (Some(manager), Some(handle)) = (ctx.txn, ctx.handle) else {
        return Ok(());
    };
    manager.end_row_read(handle, table, id)
}

/// Takes the exclusive lock one row write needs, held until the transaction ends.
///
/// A context with no transaction takes no lock, as for a read
/// (`locking::tests::a_context_without_a_transaction_takes_no_lock`). The hints of the table
/// reference are not read here: a write waits or fails, it does not leave a row out, and
/// `READPAST` and the level words apply to reads.
///
/// # Errors
///
/// What `txn` raises for a lock it refused: 1222 under a lock timeout, 1205 for a deadlock.
pub(crate) fn write_lock(ctx: &mut ExecContext<'_>, table: TableId, id: RowId) -> SqlResult<()> {
    let (Some(manager), Some(handle)) = (ctx.txn, ctx.handle) else {
        return Ok(());
    };
    manager.write_lock(handle, table, id, &LockIntent::default())
}

/// The snapshot a scan or a seek iterates under `hints`.
///
/// The same one at every level and under every word: the hints change the lock a row read
/// takes and, for a dirty read, the version its content comes from, and neither of those is
/// a property of the iteration (module documentation;
/// `locking::tests::every_hint_iterates_the_statement_snapshot`). Kept as the one place
/// that answers the question, so that a level served from the row versions has a line to
/// change rather than a call site to find.
///
/// # Errors
///
/// The internal error 50000 of a context with no snapshot.
pub(crate) fn snapshot_for_scan(
    ctx: &mut ExecContext<'_>,
    hints: &LockHints,
) -> SqlResult<Snapshot> {
    let _ = hints;
    ctx.snapshot().cloned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vauban_binder::SessionOptions;
    use vauban_storage::{MemoryStorage, Row, Storage, TableShape};
    use vauban_sysfn::StaticContext;
    use vauban_txn::{TransactionManager, VersioningOptions};
    use vauban_types::{SqlType, TypeInfo, Value};

    use super::*;

    /// The five words that name a level become the five levels; `translate_the_five_level_words`
    /// keeps `holdlock` false for each, because the binder folds it into `serializable`.
    #[test]
    fn translate_the_five_level_words() {
        let with = |set: fn(&mut LockHints)| {
            let mut hints = LockHints::default();
            set(&mut hints);
            intent_of(&hints)
        };
        assert_eq!(intent_of(&LockHints::default()).level, None);
        assert_eq!(
            with(|h| h.nolock = true).level,
            Some(IsolationLevel::ReadUncommitted)
        );
        assert_eq!(
            with(|h| h.readcommitted = true).level,
            Some(IsolationLevel::ReadCommitted)
        );
        assert_eq!(
            with(|h| h.repeatableread = true).level,
            Some(IsolationLevel::RepeatableRead)
        );
        assert_eq!(
            with(|h| h.serializable = true).level,
            Some(IsolationLevel::Serializable)
        );
        assert_eq!(
            with(|h| h.snapshot = true).level,
            Some(IsolationLevel::Snapshot)
        );
        assert!(!with(|h| h.serializable = true).holdlock);
    }

    /// The four words that ask for a mode or a refusal are carried one to one, and the two
    /// that ask for a granularity are carried by nothing.
    #[test]
    fn translate_the_mode_words() {
        let hints = LockHints {
            updlock: true,
            xlock: true,
            readpast: true,
            nowait: true,
            tablock: true,
            tablockx: true,
            rowlock: true,
            paglock: true,
            ..LockHints::default()
        };
        let intent = intent_of(&hints);
        assert!(intent.updlock);
        assert!(intent.xlock);
        assert!(intent.readpast);
        assert!(intent.nowait);
        assert!(intent.tablock);
        assert!(intent.tablockx);
        // Counter-proof in `translate_the_mode_words`: a default `LockHints` matches `LockIntent::default()`.
        let plain = intent_of(&LockHints::default());
        assert_eq!(plain, LockIntent::default());
    }

    /// A context with no transaction reads and writes without a lock, and says the row is
    /// visible: the accessors of the context are not consulted for a lock that has no
    /// owner.
    #[test]
    fn a_context_without_a_transaction_takes_no_lock() {
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        let table = TableId(1);
        let id = RowId(1);
        let nolock = LockHints {
            nolock: true,
            ..LockHints::default()
        };
        assert_eq!(
            read_lock(&mut ctx, table, id, &nolock).expect("no lock to take"),
            RowVisibility::Visible
        );
        write_lock(&mut ctx, table, id).expect("no lock to take");
        end_row_read(&mut ctx, table, id).expect("no lock to give back");
    }

    /// The four level words plus `READPAST` each pick the statement snapshot
    /// (`every_hint_iterates_the_statement_snapshot`).
    #[test]
    fn every_hint_iterates_the_statement_snapshot() {
        let eval = StaticContext::default();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = TransactionManager::new(Arc::clone(&storage));
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let snap = manager.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            storage.as_ref(),
            &manager,
            &snap,
        );
        let words: [fn(&mut LockHints); 5] = [
            |h| h.nolock = true,
            |h| h.readcommitted = true,
            |h| h.repeatableread = true,
            |h| h.serializable = true,
            |h| h.readpast = true,
        ];
        for set in words {
            let mut hints = LockHints::default();
            set(&mut hints);
            let chosen = snapshot_for_scan(&mut ctx, &hints).expect("the context has a snapshot");
            assert_eq!(chosen, snap);
        }
    }

    /// A locking read and a dirty read both ask for the latest version of the row; the read
    /// of a level the database options serve from the row versions is the one that keeps the
    /// version the iteration produced.
    ///
    /// The vector separates the two answers on one row: with
    /// `ALLOW_SNAPSHOT_ISOLATION` off, a `READ COMMITTED` read answers
    /// [`RowVisibility::Latest`]; with it on and the transaction opened at `SNAPSHOT`, the
    /// same row answers [`RowVisibility::Visible`].
    #[test]
    fn a_locked_read_asks_for_the_latest_version_and_a_versioned_read_does_not() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage.create_database("mydb").expect("a new database");
        let shape = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, true)],
            clustered_key: None,
        };
        let table = storage.create_table(db, &shape).expect("a new table");
        let manager = TransactionManager::new(Arc::clone(&storage));
        let writer = manager.begin(IsolationLevel::ReadCommitted);
        let id = storage
            .insert(writer.id, table, &Row(vec![Value::I64(1)]))
            .expect("the row is inserted");
        manager.commit(writer).expect("the writer commits");

        let eval = StaticContext::default();
        let nolock = LockHints {
            nolock: true,
            ..LockHints::default()
        };
        let reader = manager.begin(IsolationLevel::ReadCommitted);
        let snap = manager.statement_snapshot(&reader);
        {
            let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
                .with_engine(storage.as_ref(), &manager, &snap)
                .with_handle(&reader);
            assert_eq!(
                read_lock(&mut ctx, table, id, &nolock).expect("no lock to take"),
                RowVisibility::Latest
            );
            assert_eq!(
                read_lock(&mut ctx, table, id, &LockHints::default()).expect("the row is free"),
                RowVisibility::Latest
            );
            end_row_read(&mut ctx, table, id).expect("the shared lock goes back");
        }
        manager.commit(reader).expect("the reader commits");

        manager.set_versioning_options(
            db,
            VersioningOptions {
                read_committed_snapshot: false,
                allow_snapshot_isolation: true,
            },
        );
        let versioned = manager
            .begin_in(db, IsolationLevel::Snapshot)
            .expect("the option is on");
        let snap = manager.statement_snapshot(&versioned);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(storage.as_ref(), &manager, &snap)
            .with_handle(&versioned);
        assert_eq!(
            read_lock(&mut ctx, table, id, &LockHints::default()).expect("no lock to take"),
            RowVisibility::Visible
        );
        manager
            .commit(versioned)
            .expect("the versioned reader commits");
    }
}
