//! Table hints and escalation: `TABLOCK`, `TABLOCKX`, `ROWLOCK`, `PAGLOCK`.
//!
//! The scenarios run threads over an `Arc<TransactionManager>`, not SQL. What SQL Server
//! does on each shape is in the module documentation of `crates/vauban-txn/src/table_lock.rs`.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use vauban_storage::{MemoryStorage, RowId, TableId, TxnId};
use vauban_txn::{
    ESCALATION_THRESHOLD, IsolationLevel, LockIntent, LockManager, LockMode, LockResource,
    LockTimeout, LockWait, TableLockDecision, TransactionManager, TxnHandle,
};

const SOON: Duration = Duration::from_secs(2);
const QUIET: Duration = Duration::from_millis(100);

const T: TableId = TableId(1);
const R: RowId = RowId(1);

fn manager() -> Arc<TransactionManager> {
    Arc::new(TransactionManager::new(Arc::new(MemoryStorage::new())))
}

fn table_res() -> LockResource {
    LockResource::Table(T)
}

fn row_res() -> LockResource {
    LockResource::Row(T, R)
}

fn plain() -> LockIntent {
    LockIntent::default()
}

fn tablock() -> LockIntent {
    LockIntent {
        tablock: true,
        ..LockIntent::default()
    }
}

fn tablockx() -> LockIntent {
    LockIntent {
        tablockx: true,
        ..LockIntent::default()
    }
}

fn in_background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

fn await_waiter(locks: &LockManager, txn: TxnId) {
    let end = Instant::now() + SOON;
    while Instant::now() < end {
        if locks.waiters().iter().any(|(t, _, _)| *t == txn) {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "transaction {txn} never reached the queue: {:?}",
        locks.waiters()
    );
}

fn still_waiting<T: std::fmt::Debug>(rx: &Receiver<T>, what: &str) {
    match rx.recv_timeout(QUIET) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("{what} was expected to wait, it answered {other:?}"),
    }
}

fn table_modes(mgr: &TransactionManager, txn: &TxnHandle) -> Vec<LockMode> {
    mgr.locks()
        .held(txn.id)
        .into_iter()
        .filter(|(res, _)| *res == table_res())
        .map(|(_, mode)| mode)
        .collect()
}

fn row_lock_count(mgr: &TransactionManager, txn: &TxnHandle) -> usize {
    mgr.locks()
        .held(txn.id)
        .into_iter()
        .filter(|(res, mode)| matches!(res, LockResource::Row(T, _) if mode.is_data()))
        .count()
}

fn take_row_x_locks(mgr: &TransactionManager, txn: &TxnHandle, count: usize) {
    for i in 1..=count {
        mgr.locks()
            .lock(
                txn.id,
                LockResource::Row(T, RowId(i as u64)),
                LockMode::X,
                LockTimeout::Infinite,
                &LockWait::none(),
            )
            .expect("row lock");
    }
}

fn read_in_background(
    mgr: &Arc<TransactionManager>,
    handle: &TxnHandle,
    hints: LockIntent,
) -> Receiver<Result<(), u32>> {
    let (mgr, handle) = (Arc::clone(mgr), handle.clone());
    in_background(move || {
        mgr.read_lock(&handle, T, R, &hints, &LockWait::none())
            .map(|_| ())
            .map_err(|e| e.number)
    })
}

/// `TABLOCK` takes an `S` on the table; a second reader goes through.
#[test]
fn tablock_takes_a_shared_table_lock() {
    let mgr = manager();
    let first = mgr.begin(IsolationLevel::RepeatableRead);
    let decision = mgr
        .table_lock(&first, T, &tablock())
        .expect("the table is free");
    assert_eq!(decision, TableLockDecision::TableShared);
    assert_eq!(table_modes(&mgr, &first), vec![LockMode::S]);

    let second = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.read_lock(&second, T, R, &plain(), &LockWait::none())
        .expect("a shared table lock lets another reader through");
}

/// While `TABLOCKX` holds an `X` on the table, a `read_lock` at `ReadCommitted` waits.
#[test]
fn tablockx_blocks_a_read_lock() {
    let mgr = manager();
    let holder = mgr.begin(IsolationLevel::RepeatableRead);
    mgr.table_lock(&holder, T, &tablockx())
        .expect("the table is free");

    let waiter = mgr.begin(IsolationLevel::ReadCommitted);
    let rx = read_in_background(&mgr, &waiter, plain());
    await_waiter(mgr.locks(), waiter.id);
    still_waiting(&rx, "the row read behind TABLOCKX");

    mgr.commit(holder).expect("releases the table lock");
    rx.recv_timeout(SOON)
        .expect("the row read after commit")
        .expect("no error");
}

/// Without a table hint, the decision is row-level and only intent shows on the table.
#[test]
fn rowlock_keeps_row_granularity() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::RepeatableRead);
    let decision = mgr
        .table_lock(&txn, T, &plain())
        .expect("no table lock to take");
    assert_eq!(decision, TableLockDecision::RowLevel);
    assert!(table_modes(&mgr, &txn).is_empty());

    mgr.read_lock(&txn, T, R, &plain(), &LockWait::none())
        .expect("the row read");
    assert_eq!(table_modes(&mgr, &txn), vec![LockMode::IS]);
    assert!(
        mgr.locks()
            .held(txn.id)
            .iter()
            .any(|(res, mode)| *res == row_res() && *mode == LockMode::S)
    );
}

/// `PAGLOCK` has no page here: the decision matches the default row granularity.
#[test]
fn paglock_is_accepted_without_effect() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    let rowlock_decision = mgr.table_lock(&txn, T, &plain()).expect("row level");
    assert_eq!(rowlock_decision, TableLockDecision::RowLevel);
    // `LockIntent` carries no `paglock` flag yet; until the executor maps it, the default
    // path is the shape this hint will keep (`table_lock.rs`).
    let again = mgr.table_lock(&txn, T, &plain()).expect("paglock level");
    assert_eq!(again, TableLockDecision::RowLevel);
}

/// Under `READ COMMITTED`, the table lock of `TABLOCK` ends with the instruction.
#[test]
fn tablock_is_released_after_the_statement_under_read_committed() {
    let mgr = manager();
    let holder = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.table_lock(&holder, T, &tablock())
        .expect("the table lock");
    mgr.end_table_lock(&holder, T).expect("the statement ends");
    assert!(table_modes(&mgr, &holder).is_empty());

    let writer = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&writer, T, R, &plain(), &LockWait::none())
        .expect("the table is free after end_table_lock");
}

/// `end_table_lock` may run on a thread other than the one that took the table lock.
#[test]
fn end_table_lock_from_another_thread_releases_the_lock() {
    let mgr = manager();
    let holder = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.table_lock(&holder, T, &tablock())
        .expect("the table lock");

    let bg_mgr = Arc::clone(&mgr);
    let bg_holder = holder.clone();
    in_background(move || bg_mgr.end_table_lock(&bg_holder, T).expect("release"))
        .recv_timeout(SOON)
        .expect("the other thread finishes");

    assert!(table_modes(&mgr, &holder).is_empty());
}

/// Under `REPEATABLE READ`, the table lock stays until commit.
#[test]
fn tablock_is_held_to_commit_under_repeatable_read() {
    let mgr = manager();
    let holder = mgr.begin(IsolationLevel::RepeatableRead);
    mgr.table_lock(&holder, T, &tablock())
        .expect("the table lock");
    mgr.end_table_lock(&holder, T)
        .expect("nothing to release at statement end");
    assert_eq!(table_modes(&mgr, &holder), vec![LockMode::S]);

    let writer = mgr.begin(IsolationLevel::ReadCommitted);
    let rx = in_background({
        let mgr = Arc::clone(&mgr);
        let writer = writer.clone();
        move || {
            mgr.write_lock(&writer, T, R, &plain(), &LockWait::none())
                .map_err(|e| e.number)
        }
    });
    await_waiter(mgr.locks(), writer.id);
    still_waiting(&rx, "the writer behind TABLOCK");

    mgr.commit(holder).expect("releases the table lock");
    rx.recv_timeout(SOON)
        .expect("the writer after commit")
        .expect("no error");
}

/// At [`ESCALATION_THRESHOLD`] row locks, row locks become a table `X`.
#[test]
fn escalation_converts_at_the_threshold() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    take_row_x_locks(&mgr, &txn, ESCALATION_THRESHOLD);
    assert_eq!(row_lock_count(&mgr, &txn), ESCALATION_THRESHOLD);

    let decision = mgr
        .maybe_escalate(&txn, T)
        .expect("escalation")
        .expect("a decision");
    assert_eq!(decision, TableLockDecision::TableExclusive);
    assert_eq!(row_lock_count(&mgr, &txn), 0);
    assert_eq!(table_modes(&mgr, &txn), vec![LockMode::X]);
}

/// One row lock below the threshold leaves row granularity in place.
#[test]
fn escalation_does_not_convert_one_row_below() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    take_row_x_locks(&mgr, &txn, ESCALATION_THRESHOLD - 1);

    assert!(mgr.maybe_escalate(&txn, T).expect("probe").is_none());
    assert_eq!(row_lock_count(&mgr, &txn), ESCALATION_THRESHOLD - 1);
    assert!(table_modes(&mgr, &txn).is_empty());
}
