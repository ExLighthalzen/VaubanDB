//! The transaction life cycle over a `MemoryStorage`: increasing identifiers,
//! what a snapshot shows before and after a commit, what a rollback hides, and the horizon
//! `Storage::vacuum` takes.
//!
//! Each scenario uses one database and one table of a single nullable `int` column, built
//! with the helpers of `vauban_storage::testsuite`. Identifiers in the assertions are the
//! small integers `TransactionManager::begin` hands out, `TxnId(1)` first.

use std::sync::Arc;

use vauban_errors::SqlError;
use vauban_storage::testsuite::{int_table_shape, row};
use vauban_storage::{MemoryStorage, Row, RowId, Storage, TableId, TxnId};
use vauban_txn::{IsolationLevel, LockTimeout, TransactionManager, TxnHandle, WriteDecision};

/// A storage with one table of one `int` column, and a manager over it.
struct Fixture {
    storage: Arc<dyn Storage>,
    mgr: TransactionManager,
    table: TableId,
}

fn fixture() -> Fixture {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage.create_database("txn").expect("create_database");
    let table = storage
        .create_table(db, &int_table_shape(1))
        .expect("create_table");
    let mgr = TransactionManager::new(Arc::clone(&storage));
    Fixture {
        storage,
        mgr,
        table,
    }
}

impl Fixture {
    /// Row `id` as `txn` sees it through a snapshot taken now, `None` when no version is
    /// visible to that snapshot.
    fn read(&self, txn: &TxnHandle, id: RowId) -> Option<Row> {
        let snap = self.mgr.statement_snapshot(txn);
        self.storage.get(&snap, self.table, id).expect("get")
    }
}

// --------------------------------------------------------------- Identifiers

#[test]
fn txn_ids_are_strictly_increasing() {
    let f = fixture();
    let first = f.mgr.begin(IsolationLevel::ReadCommitted);
    let second = f.mgr.begin(IsolationLevel::ReadCommitted);
    let third = f.mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!(
        [first.id, second.id, third.id],
        [TxnId(1), TxnId(2), TxnId(3)]
    );
    assert!(first.id < second.id && second.id < third.id);
}

#[test]
fn commit_and_rollback_close_the_transaction() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    let b = f.mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!(f.mgr.active_sessions().len(), 2);
    f.mgr.commit(a).expect("commit A");
    let left: Vec<TxnId> = f.mgr.active_sessions().iter().map(|i| i.id).collect();
    assert_eq!(left, vec![TxnId(2)], "A is closed, B is not");
    f.mgr.rollback(b).expect("rollback B");
    assert_eq!(f.mgr.active_sessions(), Vec::new());
    // A closed transaction does not give its identifier back.
    assert_eq!(f.mgr.begin(IsolationLevel::ReadCommitted).id, TxnId(3));
}

#[test]
fn commit_twice_is_a_bug() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    f.mgr.commit(a.clone()).expect("first commit");
    let err = f.mgr.commit(a.clone()).expect_err("second commit");
    assert_bug(
        &err,
        "TransactionManager::commit: transaction 1 is not open",
    );
    let err = f.mgr.rollback(a).expect_err("rollback after commit");
    assert_bug(
        &err,
        "TransactionManager::rollback: transaction 1 is not open",
    );
}

/// An internal bug reaches the client as number 50000, severity 16 (crate `errors`).
fn assert_bug(err: &SqlError, expected: &str) {
    assert_eq!(err.number, 50000, "number");
    assert_eq!(err.severity, 16, "severity");
    assert!(
        err.message.contains(expected),
        "message was {:?}",
        err.message
    );
}

#[test]
fn a_refused_storage_commit_keeps_the_transaction_open() {
    let f = fixture();
    // A second manager over the same storage hands out `TxnId(1)` again; storage has already
    // committed that identifier, so its own precondition refuses the call.
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    f.storage.insert(a.id, f.table, &row(&[1])).expect("insert");
    f.mgr.commit(a).expect("commit through the first manager");

    let other = TransactionManager::new(Arc::clone(&f.storage));
    let clash = other.begin(IsolationLevel::ReadCommitted);
    assert_eq!(clash.id, TxnId(1));
    let err = other.commit(clash).expect_err("storage refuses the commit");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);
    let left: Vec<TxnId> = other.active_sessions().iter().map(|i| i.id).collect();
    assert_eq!(left, vec![TxnId(1)], "a refused commit closes nothing");
}

// ------------------------------------------------------------------ Snapshots

#[test]
fn own_writes_are_visible() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    let id = f
        .storage
        .insert(a.id, f.table, &row(&[42]))
        .expect("insert");
    let snap = f.mgr.statement_snapshot(&a);
    assert_eq!(snap.own, a.id);
    assert_eq!(
        f.storage.get(&snap, f.table, id).expect("get"),
        Some(row(&[42])),
        "a transaction reads its own insert before it commits"
    );
}

#[test]
fn other_in_progress_is_invisible() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    let b = f.mgr.begin(IsolationLevel::ReadCommitted);
    let id = f.storage.insert(a.id, f.table, &row(&[7])).expect("insert");

    let before = f.mgr.statement_snapshot(&b);
    assert_eq!(before.own, TxnId(2));
    assert_eq!(before.xmin, TxnId(1));
    assert_eq!(before.xmax, TxnId(3));
    assert_eq!(before.active, vec![TxnId(1)], "A is listed, B is not");
    assert_eq!(f.storage.get(&before, f.table, id).expect("get"), None);

    f.mgr.commit(a).expect("commit A");
    assert_eq!(
        f.storage.get(&before, f.table, id).expect("get"),
        None,
        "the snapshot taken before the commit keeps hiding the row"
    );
    let after = f.mgr.statement_snapshot(&b);
    assert_eq!(after.active, Vec::new());
    assert_eq!(
        f.storage.get(&after, f.table, id).expect("get"),
        Some(row(&[7])),
        "a snapshot taken after the commit shows the row"
    );
}

#[test]
fn rollback_hides_the_row() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    let id = f.storage.insert(a.id, f.table, &row(&[5])).expect("insert");
    assert_eq!(
        f.read(&a, id),
        Some(row(&[5])),
        "visible to its writer first"
    );
    f.mgr.rollback(a).expect("rollback A");

    let b = f.mgr.begin(IsolationLevel::ReadCommitted);
    let snap = f.mgr.statement_snapshot(&b);
    assert_eq!(
        f.storage.get(&snap, f.table, id).expect("get"),
        None,
        "a snapshot taken after the rollback finds no version"
    );
}

#[test]
fn active_ids_of_a_snapshot_are_sorted() {
    let f = fixture();
    let opened: Vec<_> = (0..5)
        .map(|_| f.mgr.begin(IsolationLevel::ReadCommitted))
        .collect();
    f.mgr.commit(opened[1].clone()).expect("commit 2");
    f.mgr.rollback(opened[3].clone()).expect("rollback 4");
    let reader = f.mgr.begin(IsolationLevel::ReadCommitted);
    let snap = f.mgr.statement_snapshot(&reader);
    assert_eq!(snap.active, vec![TxnId(1), TxnId(3), TxnId(5)]);
    assert_eq!(snap.xmin, TxnId(1));
    assert_eq!(snap.xmax, TxnId(7));
    assert_eq!(snap.own, TxnId(6));
}

#[test]
fn the_five_levels_are_served_as_read_committed() {
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
        IsolationLevel::Snapshot,
    ] {
        let f = fixture();
        let writer = f.mgr.begin(IsolationLevel::ReadCommitted);
        let reader = f.mgr.begin(level);
        assert_eq!(reader.isolation, level, "the level is kept as given");
        let id = f
            .storage
            .insert(writer.id, f.table, &row(&[3]))
            .expect("insert");
        assert_eq!(
            f.read(&reader, id),
            None,
            "{level:?}: no dirty read, no snapshot pinned at begin"
        );
        f.mgr.commit(writer).expect("commit the writer");
        assert_eq!(
            f.read(&reader, id),
            Some(row(&[3])),
            "{level:?}: the next statement sees the committed row"
        );
    }
}

// -------------------------------------------------------------------- Horizon

#[test]
fn snapshot_horizon_follows_the_oldest_open_transaction() {
    let f = fixture();
    assert_eq!(f.mgr.snapshot_horizon(), TxnId(1), "nothing open yet");
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    let b = f.mgr.begin(IsolationLevel::ReadCommitted);
    let c = f.mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!(f.mgr.snapshot_horizon(), TxnId(1));
    f.mgr.commit(a).expect("commit A");
    assert_eq!(f.mgr.snapshot_horizon(), TxnId(2));
    f.mgr.rollback(c).expect("rollback C");
    assert_eq!(f.mgr.snapshot_horizon(), TxnId(2), "B is still the oldest");
    f.mgr.commit(b).expect("commit B");
    assert_eq!(f.mgr.snapshot_horizon(), TxnId(4), "the next identifier");
}

#[test]
fn horizon_matches_the_xmin_of_a_live_snapshot() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    let id = f.storage.insert(a.id, f.table, &row(&[9])).expect("insert");
    f.mgr.commit(a).expect("commit A");
    let b = f.mgr.begin(IsolationLevel::ReadCommitted);
    let snap = f.mgr.statement_snapshot(&b);
    let horizon = f.mgr.snapshot_horizon();
    assert_eq!(horizon, snap.xmin, "the horizon is the xmin of B");
    f.storage.vacuum(horizon).expect("vacuum");
    assert_eq!(
        f.storage.get(&snap, f.table, id).expect("get"),
        Some(row(&[9])),
        "a vacuum at that horizon leaves the visible row alone"
    );
}

/// The limit of `snapshot_horizon`: the horizon is read off the open transactions, so it
/// can sit above the `xmin` of a snapshot a transaction still open is still
/// reading through, and a `vacuum` at that horizon takes a visible version away.
#[test]
fn snapshot_horizon_can_pass_the_xmin_of_a_snapshot_in_use() {
    let f = fixture();
    let seeder = f.mgr.begin(IsolationLevel::ReadCommitted);
    let id = f
        .storage
        .insert(seeder.id, f.table, &row(&[11]))
        .expect("insert");
    f.mgr.commit(seeder).expect("commit the seeder");

    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    let b = f.mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!([a.id, b.id], [TxnId(2), TxnId(3)]);
    f.storage.delete(a.id, f.table, id).expect("delete by A");

    // B takes its snapshot while A is open: the delete of A is not settled, the row shows.
    let held = f.mgr.statement_snapshot(&b);
    assert_eq!(held.xmin, TxnId(2));
    assert_eq!(held.active, vec![TxnId(2)]);
    assert_eq!(
        f.storage.get(&held, f.table, id).expect("get"),
        Some(row(&[11]))
    );

    f.mgr.commit(a).expect("commit A");
    let horizon = f.mgr.snapshot_horizon();
    assert_eq!(horizon, TxnId(3), "B is the oldest open transaction");
    assert!(
        horizon > held.xmin,
        "the horizon passed the xmin of a snapshot B is still reading through"
    );

    f.storage.vacuum(horizon).expect("vacuum");
    assert_eq!(
        f.storage.get(&held, f.table, id).expect("get"),
        None,
        "the version the held snapshot was showing is gone"
    );
}

// ----------------------------------------------------- Locks and conflicts without contention

#[test]
fn lock_row_is_a_noop() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::Serializable);
    for timeout in [
        LockTimeout::Infinite,
        LockTimeout::Millis(0),
        LockTimeout::Millis(100),
        LockTimeout::NoWait,
    ] {
        f.mgr
            .lock_row(&a, f.table, RowId(1), timeout)
            .unwrap_or_else(|e| panic!("{timeout:?}: {}", e.message));
    }
    // No state moved: the transaction is still the one open, at the same identifier.
    let open: Vec<TxnId> = f.mgr.active_sessions().iter().map(|i| i.id).collect();
    assert_eq!(open, vec![TxnId(1)]);
    assert_eq!(f.mgr.snapshot_horizon(), TxnId(1));
}

#[test]
fn check_write_conflict_lets_the_writer_proceed() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::Snapshot);
    let b = f.mgr.begin(IsolationLevel::Snapshot);
    let id = f.storage.insert(a.id, f.table, &row(&[1])).expect("insert");
    // Even on a row another open transaction has just written.
    assert_eq!(
        f.mgr.check_write_conflict(&b, f.table, id).expect("check"),
        WriteDecision::Proceed
    );
    assert_eq!(
        f.mgr.check_write_conflict(&a, f.table, id).expect("check"),
        WriteDecision::Proceed
    );
}
