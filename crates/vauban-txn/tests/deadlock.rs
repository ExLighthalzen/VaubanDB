//! The two errors a refused wait reports — 1222 when the wait ran out of time, 1205 when it
//! would close a cycle — and the choice of the victim.
//!
//! The scenarios run threads over an `Arc<LockManager>` or an `Arc<TransactionManager>`,
//! not SQL: this file knows no isolation level and no statement. A thread that must be
//! waiting before the test goes on is attested through [`LockManager::waiters`] rather than
//! through a sleep.
//!
//! The numbers, severities and states are those of the error constructors; what this file
//! asserts is which constructor a given wait reaches.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant};

use vauban_errors::SqlResult;
use vauban_storage::{MemoryStorage, RowId, TableId, TxnId};
use vauban_txn::{
    IsolationLevel, LockManager, LockMode, LockResource, LockTimeout, LockWait, TransactionManager,
    TxnHandle,
};

/// Longest a test waits for a thread to reach the queue, or for an outcome to come back.
const SOON: Duration = Duration::from_secs(5);

/// The table the row resources of this file belong to.
const TABLE: TableId = TableId(1);

/// Row `n` of [`TABLE`].
fn row(n: u64) -> LockResource {
    LockResource::Row(TABLE, RowId(n))
}

/// Blocks until `txn` is queued somewhere, at most [`SOON`].
fn await_waiter(mgr: &LockManager, txn: TxnId) {
    let end = Instant::now() + SOON;
    while Instant::now() < end {
        if mgr.waiters().iter().any(|(t, _, _)| *t == txn) {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "transaction {txn} never reached the queue: {:?}",
        mgr.waiters()
    );
}

/// Takes an `X` on `resource` for `txn` in a thread of its own, waiting for as long as it
/// takes, and gives the locks of `txn` back once it is granted — the end of a transaction
/// that got what it asked for.
fn wait_for(mgr: &Arc<LockManager>, txn: TxnId, resource: LockResource) -> Receiver<SqlResult<()>> {
    wait_for_mode(mgr, txn, resource, LockMode::X)
}

/// The same wait in the mode asked for: a waiter the holders would let through, held back
/// by the queue alone, asks for an `S`.
fn wait_for_mode(
    mgr: &Arc<LockManager>,
    txn: TxnId,
    resource: LockResource,
    mode: LockMode,
) -> Receiver<SqlResult<()>> {
    let (tx, rx) = channel();
    let mgr = Arc::clone(mgr);
    thread::spawn(move || {
        let outcome = mgr.lock(
            txn,
            resource,
            mode,
            LockTimeout::Infinite,
            &LockWait::none(),
        );
        if outcome.is_ok() {
            mgr.release_all(txn);
        }
        let _ = tx.send(outcome);
    });
    rx
}

/// The outcome of a thread, or a failure naming the thread that did not answer.
fn outcome(rx: &Receiver<SqlResult<()>>, who: &str) -> SqlResult<()> {
    match rx.recv_timeout(SOON) {
        Ok(outcome) => outcome,
        Err(e) => panic!("{who} did not answer within {SOON:?}: {e}"),
    }
}

/// Takes the `X` of `txn` on `resource` at once, as the set-up of a scenario.
fn hold(mgr: &LockManager, txn: TxnId, resource: LockResource) {
    hold_mode(mgr, txn, resource, LockMode::X);
}

/// The same set-up in the mode asked for, for a scenario that starts on a shared lock.
fn hold_mode(mgr: &LockManager, txn: TxnId, resource: LockResource, mode: LockMode) {
    mgr.lock(txn, resource, mode, LockTimeout::NoWait, &LockWait::none())
        .expect("the resource is free");
}

// ------------------------------------------------------------------ 1222, the timeout

/// A wait that reaches the end of its `LOCK_TIMEOUT` reports 1222, and it did wait.
#[test]
fn timeout_yields_1222() {
    let mgr = LockManager::new();
    hold(&mgr, TxnId(1), row(1));

    let started = Instant::now();
    let err = mgr
        .lock(
            TxnId(2),
            row(1),
            LockMode::S,
            LockTimeout::Millis(100),
            &LockWait::none(),
        )
        .expect_err("the row is held in X");
    let waited = started.elapsed();

    assert_eq!(err.number, 1222, "message was {:?}", err.message);
    assert_eq!((err.severity, err.state), (16, 51), "a row lock");
    assert!(
        waited >= Duration::from_millis(100),
        "waited {waited:?}, less than the timeout asked for"
    );
}

/// `NoWait` reports the same 1222 without parking the thread.
#[test]
fn nowait_yields_1222_without_waiting() {
    let mgr = LockManager::new();
    hold(&mgr, TxnId(1), row(1));

    let started = Instant::now();
    let err = mgr
        .lock(
            TxnId(2),
            row(1),
            LockMode::S,
            LockTimeout::NoWait,
            &LockWait::none(),
        )
        .expect_err("the row is held in X");
    let waited = started.elapsed();

    assert_eq!(err.number, 1222, "message was {:?}", err.message);
    assert!(waited < Duration::from_millis(50), "waited {waited:?}");
}

/// The state of 1222 follows the granularity of the resource waited for: 51 on a row, 56 on
/// the object — two constructors, and the shape that separates them.
#[test]
fn timeout_state_follows_the_granularity() {
    let mgr = LockManager::new();
    let table = LockResource::Table(TABLE);
    hold(&mgr, TxnId(1), row(1));
    hold(&mgr, TxnId(1), table);

    let on_row = mgr
        .lock(
            TxnId(2),
            row(1),
            LockMode::S,
            LockTimeout::NoWait,
            &LockWait::none(),
        )
        .expect_err("the row is held");
    let on_object = mgr
        .lock(
            TxnId(2),
            table,
            LockMode::S,
            LockTimeout::NoWait,
            &LockWait::none(),
        )
        .expect_err("the table is held");

    assert_eq!((on_row.number, on_row.state), (1222, 51));
    assert_eq!((on_object.number, on_object.state), (1222, 56));
    assert_eq!(
        on_row.message, on_object.message,
        "one sentence for the two states"
    );
}

// ------------------------------------------------------------------ 1205, the cycle

/// Two transactions that cross their row locks: one of them is rolled back with 1205, the
/// other is granted, and both threads answer.
#[test]
fn two_way_cycle_picks_one_victim() {
    let mgr = Arc::new(LockManager::new());
    hold(&mgr, TxnId(1), row(1));
    hold(&mgr, TxnId(2), row(2));

    let started = Instant::now();
    let one = wait_for(&mgr, TxnId(1), row(2));
    let two = wait_for(&mgr, TxnId(2), row(1));
    let outcomes = [
        outcome(&one, "transaction 1"),
        outcome(&two, "transaction 2"),
    ];
    let elapsed = started.elapsed();

    let refused: Vec<u32> = outcomes
        .iter()
        .filter_map(|o| o.as_ref().err().map(|e| e.number))
        .collect();
    assert_eq!(refused, vec![1205], "outcomes were {outcomes:?}");
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        1,
        "the other branch went through: {outcomes:?}"
    );
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

/// Three transactions in a ring: one victim, and the two others are granted.
#[test]
fn three_way_cycle_is_detected() {
    let mgr = Arc::new(LockManager::new());
    hold(&mgr, TxnId(1), row(1));
    hold(&mgr, TxnId(2), row(2));
    hold(&mgr, TxnId(3), row(3));

    let started = Instant::now();
    let one = wait_for(&mgr, TxnId(1), row(2));
    let two = wait_for(&mgr, TxnId(2), row(3));
    let three = wait_for(&mgr, TxnId(3), row(1));
    let outcomes = [
        outcome(&one, "transaction 1"),
        outcome(&two, "transaction 2"),
        outcome(&three, "transaction 3"),
    ];
    let elapsed = started.elapsed();

    let refused: Vec<u32> = outcomes
        .iter()
        .filter_map(|o| o.as_ref().err().map(|e| e.number))
        .collect();
    assert_eq!(refused, vec![1205], "outcomes were {outcomes:?}");
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        2,
        "the two other branches went through: {outcomes:?}"
    );
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

/// A cycle one branch of which is a wait the holders would allow: `T1` holds `S` on row 1;
/// `T2` asks `X` on it and is refused; `T3` asks `S` on it and is compatible with `T1` but
/// stands behind `T2`; `T3` holds `X` on row 2, which `T1` then asks for. The ring
/// `T1 → T3 → T2 → T1` closes through the edge of the fair queue, `T3 → T2`: without that
/// edge in `wait_for_graph`, the three threads would wait for the 5 s of [`SOON`].
///
/// SQL Server rolls `T2` back on this shape with 1205, level 13, state 51, and lets the two
/// others through. The three ranks of the victim rule land on `T2` here as well: `T3` holds
/// the write lock of the ring, and of the two remaining transactions `T2` is the younger.
#[test]
fn a_wait_behind_a_blocked_candidate_closes_the_cycle() {
    let mgr = Arc::new(LockManager::new());
    hold_mode(&mgr, TxnId(1), row(1), LockMode::S);
    hold(&mgr, TxnId(3), row(2));

    let started = Instant::now();
    let two = wait_for(&mgr, TxnId(2), row(1));
    await_waiter(&mgr, TxnId(2));
    let three = wait_for_mode(&mgr, TxnId(3), row(1), LockMode::S);
    await_waiter(&mgr, TxnId(3));
    let one = wait_for(&mgr, TxnId(1), row(2));

    let outcomes = [
        outcome(&one, "transaction 1"),
        outcome(&two, "transaction 2"),
        outcome(&three, "transaction 3"),
    ];
    let elapsed = started.elapsed();

    match &outcomes {
        [Ok(()), Err(refusal), Ok(())] => assert_eq!(
            (refusal.number, refusal.severity, refusal.state),
            (1205, 13, 51),
            "message was {:?}",
            refusal.message
        ),
        other => panic!("expected transaction 2 alone to be rolled back, got {other:?}"),
    }
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

/// The victim hands its locks back before its 1205 reaches the caller: the transaction that
/// reads the error holds nothing.
#[test]
fn victim_releases_before_returning() {
    let mgr = Arc::new(LockManager::new());
    hold(&mgr, TxnId(1), row(1));
    hold(&mgr, TxnId(2), row(2));

    let one = wait_for(&mgr, TxnId(1), row(2));
    let two = wait_for(&mgr, TxnId(2), row(1));
    let outcomes = [
        outcome(&one, "transaction 1"),
        outcome(&two, "transaction 2"),
    ];

    let victim = match &outcomes {
        [Err(_), Ok(())] => TxnId(1),
        [Ok(()), Err(_)] => TxnId(2),
        other => panic!("expected one victim, got {other:?}"),
    };
    assert_eq!(
        mgr.held(victim),
        vec![],
        "the victim gave its locks back before 1205 went up"
    );
}

// ------------------------------------------------------- The victim, by priority

/// A manager over an empty `MemoryStorage`: `lock_row` locks an identifier, `storage` is
/// not consulted.
fn manager() -> Arc<TransactionManager> {
    Arc::new(TransactionManager::new(Arc::new(MemoryStorage::new())))
}

/// Takes the `X` of `txn` on row `n` in a thread of its own, through the transaction
/// manager, and gives the locks back once granted.
fn lock_row_in_thread(
    mgr: &Arc<TransactionManager>,
    txn: &TxnHandle,
    n: u64,
) -> Receiver<SqlResult<()>> {
    let (tx, rx) = channel();
    let mgr = Arc::clone(mgr);
    let txn = txn.clone();
    thread::spawn(move || {
        let outcome = mgr.lock_row(&txn, TABLE, RowId(n), LockTimeout::Infinite);
        if outcome.is_ok() {
            mgr.locks().release_all(txn.id);
        }
        let _ = tx.send(outcome);
    });
    rx
}

/// Runs the two-way cycle through the transaction manager with `a` at priority `-5` and `b`
/// at the default, `a` crossing first when `a_first`. Answers the transaction the cycle
/// rolled back.
fn victim_of_a_priority_cycle(a_first: bool) -> TxnId {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let b = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.set_deadlock_priority(&a, -5);

    mgr.lock_row(&a, TABLE, RowId(1), LockTimeout::NoWait)
        .expect("row 1 is free");
    mgr.lock_row(&b, TABLE, RowId(2), LockTimeout::NoWait)
        .expect("row 2 is free");

    let (first, second) = if a_first { (&a, &b) } else { (&b, &a) };
    let (first_row, second_row) = if a_first { (2, 1) } else { (1, 2) };
    let started = lock_row_in_thread(&mgr, first, first_row);
    await_waiter(mgr.locks(), first.id);
    let closed = lock_row_in_thread(&mgr, second, second_row);

    let outcomes = [outcome(&started, "first"), outcome(&closed, "second")];
    let refused: Vec<TxnId> = outcomes
        .iter()
        .zip([first.id, second.id])
        .filter_map(|(o, id)| o.as_ref().err().map(|e| (id, e.number)))
        .map(|(id, number)| {
            assert_eq!(number, 1205, "the refusal of {id} was {number}");
            id
        })
        .collect();
    assert_eq!(refused.len(), 1, "outcomes were {outcomes:?}");
    refused[0]
}

/// The lowest `DEADLOCK_PRIORITY` gives way, whichever side closes the cycle — where the
/// tie-break of equal priorities would have picked the younger transaction, `b`.
#[test]
fn priority_decides_the_victim() {
    let a_crossed_first = victim_of_a_priority_cycle(true);
    let b_crossed_first = victim_of_a_priority_cycle(false);
    assert_eq!(
        (a_crossed_first, b_crossed_first),
        (TxnId(1), TxnId(1)),
        "the transaction at -5 is the victim in both arrival orders"
    );
}

// ------------------------------------------------------------------ No cycle, no error

/// A wait that is long but closes no cycle is not a deadlock: the holder gives the row back
/// and the waiter is granted, with neither 1205 nor 1222.
#[test]
fn waiting_without_cycle_is_not_a_deadlock() {
    let mgr = Arc::new(LockManager::new());
    hold(&mgr, TxnId(1), row(1));

    let waiter = wait_for(&mgr, TxnId(2), row(1));
    await_waiter(&mgr, TxnId(2));
    thread::sleep(Duration::from_millis(300));
    mgr.release_all(TxnId(1));

    let granted = outcome(&waiter, "transaction 2");
    assert_eq!(granted, Ok(()), "an honest wait ends on its lock");
}
