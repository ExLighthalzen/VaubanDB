//! Shape of the public API of `vauban-txn`: the public signatures are callable, `begin`
//! hands out increasing identifiers and the lock methods answer as a manager that met no
//! contention would.
//!
//! What the life cycle does is tested in `tests/txn_basic.rs`, what the savepoints and the
//! deferred actions do in `tests/txn_actions.rs`.

use std::sync::Arc;

use vauban_storage::{DbId, IndexId, MemoryStorage, RowId, Storage, TableId, TxnId, TxnStatus};
use vauban_txn::{
    CommitAction, IsolationLevel, LockTimeout, RollbackAction, TransactionManager, TxnHandle,
    WriteDecision,
};

fn manager() -> TransactionManager {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    TransactionManager::new(storage)
}

#[test]
fn begin_hands_out_increasing_ids_from_one() {
    let mgr = manager();
    let first = mgr.begin(IsolationLevel::ReadCommitted);
    let second = mgr.begin(IsolationLevel::Snapshot);
    assert_eq!(first.id, TxnId(1));
    assert_eq!(second.id, TxnId(2));
    assert!(first.id < second.id);
    assert_eq!(first.isolation, IsolationLevel::ReadCommitted);
    assert_eq!(second.isolation, IsolationLevel::Snapshot);
}

#[test]
fn begin_keeps_each_transaction_it_opens() {
    let mgr = manager();
    assert_eq!(mgr.active_sessions().len(), 0);
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
        IsolationLevel::Snapshot,
    ] {
        mgr.begin(level);
    }
    let active = mgr.active_sessions();
    assert_eq!(active.len(), 5);
    let ids: Vec<TxnId> = active.iter().map(|info| info.id).collect();
    assert_eq!(
        ids,
        vec![TxnId(1), TxnId(2), TxnId(3), TxnId(4), TxnId(5)],
        "oldest first, one entry per begin"
    );
    assert_eq!(active[4].isolation, IsolationLevel::Snapshot);
}

#[test]
fn commit_and_rollback_close_the_transaction_they_are_given() {
    let mgr = manager();
    let first = mgr.begin(IsolationLevel::ReadCommitted);
    let second = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.commit(first).expect("commit");
    assert_eq!(mgr.active_sessions().len(), 1);
    mgr.rollback(second).expect("rollback");
    assert_eq!(mgr.active_sessions().len(), 0);
}

#[test]
fn savepoints_and_actions_are_callable() {
    let mgr = manager();
    let txn: TxnHandle = mgr.begin(IsolationLevel::RepeatableRead);
    let first = mgr.savepoint(&txn).expect("savepoint");
    let second = mgr.savepoint(&txn).expect("a second savepoint");
    assert!(first < second, "increasing within the transaction");
    mgr.rollback_to(&txn, first).expect("rollback_to");
    mgr.register_on_commit(&txn, CommitAction::DropTable(TableId(1)))
        .expect("register_on_commit");
    mgr.register_on_rollback(&txn, RollbackAction::DropIndex(IndexId(1)))
        .expect("register_on_rollback");
    // The actions of the two enumerations are nameable by a caller compiled against this
    // crate; what they do to `storage` is `tests/txn_actions.rs`.
    assert_ne!(
        CommitAction::DropTable(TableId(1)),
        CommitAction::DropTable(TableId(2))
    );
    assert_ne!(
        RollbackAction::DropDatabase(DbId(1)),
        RollbackAction::DropIndex(IndexId(1))
    );
}

#[test]
fn lock_row_and_check_write_conflict_answer_without_contention() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::Serializable);
    mgr.lock_row(&txn, TableId(1), RowId(1), LockTimeout::Millis(100))
        .expect("lock_row on a free row");
    assert_eq!(
        mgr.check_write_conflict(&txn, TableId(1), RowId(1))
            .expect("check_write_conflict"),
        WriteDecision::Proceed
    );
    // The decision and timeout types are nameable by a caller compiled against this crate.
    assert_ne!(WriteDecision::Reread(RowId(1)), WriteDecision::Proceed);
    assert_ne!(LockTimeout::NoWait, LockTimeout::Infinite);
}

#[test]
fn snapshot_and_horizon_describe_the_open_transactions() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::Snapshot);
    let other = mgr.begin(IsolationLevel::ReadCommitted);
    let snap = mgr.statement_snapshot(&txn);
    assert_eq!(snap.own, txn.id);
    assert_eq!(snap.xmin, TxnId(1));
    assert_eq!(snap.xmax, TxnId(3));
    assert_eq!(snap.active, vec![other.id]);
    // The reader is settled for itself; the transaction still open beside it is not.
    let committed = |_| TxnStatus::Committed;
    assert!(snap.is_settled(txn.id, &committed));
    assert!(!snap.is_settled(other.id, &committed));
    assert_eq!(mgr.snapshot_horizon(), TxnId(1));
}
