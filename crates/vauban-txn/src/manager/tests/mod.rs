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
