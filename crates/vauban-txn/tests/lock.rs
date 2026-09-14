//! The lock manager: the compatibility matrix, the strength order, the fair queue,
//! conversion, the three timeouts, cancellation, and the release at the end of a
//! transaction.
//!
//! The scenarios run threads over an `Arc<LockManager>`, not SQL: this file knows no
//! isolation level and no table hint. A thread that must be waiting before the test goes on is
//! attested through [`LockManager::waiters`] by `await_waiter`, so that no assertion rests
//! on a thread having had the time to start.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use vauban_storage::testsuite::{int_table_shape, row};
use vauban_storage::{MemoryStorage, RowId, Storage, TableId, TxnId};
use vauban_txn::{
    IsolationLevel, LockManager, LockMode, LockOutcome, LockResource, LockTimeout, LockWait,
    TransactionManager,
};

/// How long a test waits for something it expects to happen.
const SOON: Duration = Duration::from_secs(2);

/// How long a test waits before concluding that nothing is coming.
const QUIET: Duration = Duration::from_millis(100);

/// The row every scenario that needs one lock uses.
fn row_res() -> LockResource {
    LockResource::Row(TableId(1), RowId(1))
}

/// Blocks until `txn` appears in the queue of the manager, at most [`SOON`].
///
/// Panics otherwise: a scenario whose second thread never reached the queue would assert
/// nothing.
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

/// Blocks until the manager holds `n` waiters, at most [`SOON`].
fn await_waiter_count(mgr: &LockManager, n: usize) {
    let end = Instant::now() + SOON;
    while Instant::now() < end {
        if mgr.waiters().len() == n {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("expected {n} waiters, found {:?}", mgr.waiters());
}

/// Asserts that nothing arrives on the channel within [`QUIET`].
fn assert_quiet<T: std::fmt::Debug>(rx: &Receiver<T>, what: &str) {
    match rx.recv_timeout(QUIET) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("{what}: expected silence, got {other:?}"),
    }
}

// ------------------------------------------------------- The compatibility matrix

/// The seven modes, in the order of the compatibility table below.
const MODES: [LockMode; 7] = [
    LockMode::IS,
    LockMode::S,
    LockMode::U,
    LockMode::IX,
    LockMode::X,
    LockMode::SchS,
    LockMode::SchM,
];

/// The lock compatibility matrix of SQL Server, `true` where the two modes may be held
/// together: line = mode requested, column = mode another transaction holds.
const COMPATIBILITY: [[bool; 7]; 7] = [
    //        IS     S      U      IX     X      SchS   SchM
    /* IS */
    [true, true, true, true, false, true, false],
    /* S */ [true, true, true, false, false, true, false],
    /* U */ [true, true, false, false, false, true, false],
    /* IX */ [true, false, false, true, false, true, false],
    /* X */ [false, false, false, false, false, true, false],
    /* SchS */ [true, true, true, true, true, true, false],
    /* SchM */ [false, false, false, false, false, false, false],
];

#[test]
fn compatibility_matrix_matches_the_table() {
    let mut couples = 0;
    for (i, &asked) in MODES.iter().enumerate() {
        for (j, &held) in MODES.iter().enumerate() {
            assert_eq!(
                asked.compatible_with(held),
                COMPATIBILITY[i][j],
                "{asked:?} requested while {held:?} is held"
            );
            assert_eq!(
                COMPATIBILITY[i][j], COMPATIBILITY[j][i],
                "the table is symmetric: {asked:?} / {held:?}"
            );
            assert_eq!(
                asked.compatible_with(held),
                held.compatible_with(asked),
                "compatible_with is symmetric: {asked:?} / {held:?}"
            );
            couples += 1;
        }
    }
    assert_eq!(couples, 49, "seven modes, both directions of each couple");
}

#[test]
fn strength_order_is_the_one_documented() {
    // IS < S, IS < IX, S < U < X, IX < X, Sch-S < Sch-M.
    for (strong, weak) in [
        (LockMode::S, LockMode::IS),
        (LockMode::IX, LockMode::IS),
        (LockMode::U, LockMode::S),
        (LockMode::X, LockMode::U),
        (LockMode::X, LockMode::IX),
        (LockMode::SchM, LockMode::SchS),
    ] {
        assert!(strong.covers(weak), "{strong:?} covers {weak:?}");
        assert!(!weak.covers(strong), "{weak:?} does not cover {strong:?}");
        assert_eq!(strong.join(weak), strong);
        assert_eq!(weak.join(strong), strong);
    }
    // S and IX do not compare, so they join on X, and so do U and IX.
    assert!(!LockMode::S.covers(LockMode::IX));
    assert!(!LockMode::IX.covers(LockMode::S));
    assert_eq!(LockMode::S.join(LockMode::IX), LockMode::X);
    assert_eq!(LockMode::IX.join(LockMode::U), LockMode::X);
    // A data mode and a schema mode are two families, not two strengths.
    assert!(!LockMode::X.covers(LockMode::SchS));
    assert!(!LockMode::SchM.covers(LockMode::X));
}

// ----------------------------------------------------------- Two threads, one row

#[test]
fn x_blocks_s_until_release() {
    let mgr = Arc::new(LockManager::new());
    let res = row_res();
    let wait = LockWait::none();
    assert_eq!(
        mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait),
        LockOutcome::Granted
    );

    let (tx, rx) = channel();
    let reader = Arc::clone(&mgr);
    let handle = thread::spawn(move || {
        let outcome = reader.try_acquire(
            TxnId(2),
            res,
            LockMode::S,
            LockTimeout::Infinite,
            &LockWait::none(),
        );
        tx.send(outcome).expect("the test still listens");
    });

    await_waiter(&mgr, TxnId(2));
    assert_quiet(&rx, "B while A holds the X");
    assert_eq!(mgr.held(TxnId(2)), vec![], "B holds nothing while it waits");

    mgr.release_all(TxnId(1));
    assert_eq!(
        rx.recv_timeout(SOON)
            .expect("B is granted after the release"),
        LockOutcome::Granted
    );
    assert_eq!(mgr.held(TxnId(2)), vec![(res, LockMode::S)]);
    handle.join().expect("the reader thread ends");
}

/// Counter-test of `x_blocks_s_until_release`: two shared locks are granted together, so
/// what the first test checks is the mode, not the fact of asking twice.
#[test]
fn s_and_s_run_together() {
    let mgr = Arc::new(LockManager::new());
    let res = row_res();
    assert_eq!(
        mgr.try_acquire(
            TxnId(1),
            res,
            LockMode::S,
            LockTimeout::NoWait,
            &LockWait::none()
        ),
        LockOutcome::Granted
    );

    let (tx, rx) = channel();
    let reader = Arc::clone(&mgr);
    let handle = thread::spawn(move || {
        let outcome = reader.try_acquire(
            TxnId(2),
            res,
            LockMode::S,
            LockTimeout::Infinite,
            &LockWait::none(),
        );
        tx.send(outcome).expect("the test still listens");
    });

    assert_eq!(
        rx.recv_timeout(SOON)
            .expect("B is granted while A still holds its S"),
        LockOutcome::Granted
    );
    assert_eq!(mgr.held(TxnId(1)), vec![(res, LockMode::S)], "A kept its S");
    assert_eq!(mgr.held(TxnId(2)), vec![(res, LockMode::S)]);
    handle.join().expect("the reader thread ends");
}

// --------------------------------------------------------------- The fair queue

#[test]
fn fifo_has_no_overtaking() {
    let mgr = Arc::new(LockManager::new());
    let res = row_res();
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);

    let (got_tx, got_rx) = channel();
    let (go_tx, go_rx) = channel();

    // B asks for the X second in line; it holds it until the test lets it go.
    let b_mgr = Arc::clone(&mgr);
    let b_got = got_tx.clone();
    let b = thread::spawn(move || {
        let outcome = b_mgr.try_acquire(
            TxnId(2),
            res,
            LockMode::X,
            LockTimeout::Infinite,
            &LockWait::none(),
        );
        b_got.send(("B", outcome)).expect("the test still listens");
        go_rx.recv().expect("the test tells B when to let go");
        b_mgr.release_all(TxnId(2));
    });
    await_waiter(&mgr, TxnId(2));

    // C asks for an S third in line.
    let c_mgr = Arc::clone(&mgr);
    let c = thread::spawn(move || {
        let outcome = c_mgr.try_acquire(
            TxnId(3),
            res,
            LockMode::S,
            LockTimeout::Infinite,
            &LockWait::none(),
        );
        got_tx.send(("C", outcome)).expect("the test still listens");
    });
    await_waiter(&mgr, TxnId(3));

    assert_eq!(
        mgr.waiters(),
        vec![(TxnId(2), res, LockMode::X), (TxnId(3), res, LockMode::S),],
        "the queue is in arrival order"
    );
    assert_quiet(&got_rx, "B and C while A holds the X");

    mgr.release_all(TxnId(1));
    // B is at the front, so B is served first; C cannot overtake it, since an unfair
    // manager could have handed the row to C's compatible-looking S instead.
    assert_eq!(
        got_rx.recv_timeout(SOON).expect("the first obtention"),
        ("B", LockOutcome::Granted)
    );
    assert_quiet(&got_rx, "C while B holds the X");
    go_tx.send(()).expect("B is still there");
    assert_eq!(
        got_rx.recv_timeout(SOON).expect("the second obtention"),
        ("C", LockOutcome::Granted)
    );

    b.join().expect("B ends");
    c.join().expect("C ends");
}

/// The shape that tells the fair queue apart from a manager that only looks at the
/// holders: C's mode is compatible with what A holds, and incompatible with what B, older
/// in the queue, is waiting for. Without the queue check, C would be granted here.
#[test]
fn a_compatible_latecomer_waits_behind_an_older_waiter() {
    let mgr = Arc::new(LockManager::new());
    let res = row_res();
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::S, LockTimeout::NoWait, &wait);

    let writer = Arc::clone(&mgr);
    let b = thread::spawn(move || {
        writer.try_acquire(
            TxnId(2),
            res,
            LockMode::X,
            LockTimeout::Infinite,
            &LockWait::none(),
        )
    });
    await_waiter(&mgr, TxnId(2));

    assert_eq!(
        mgr.try_acquire(TxnId(3), res, LockMode::S, LockTimeout::NoWait, &wait),
        LockOutcome::TimedOut,
        "S is compatible with the S held, but B asked for the X first"
    );
    assert_eq!(mgr.held(TxnId(3)), vec![]);

    mgr.release_all(TxnId(1));
    assert_eq!(b.join().expect("B ends"), LockOutcome::Granted);
}

// ----------------------------------------------------------------- Conversion

#[test]
fn conversion_jumps_the_queue() {
    let mgr = Arc::new(LockManager::new());
    let res = row_res();
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::S, LockTimeout::NoWait, &wait);

    let writer = Arc::clone(&mgr);
    let b = thread::spawn(move || {
        writer.try_acquire(
            TxnId(2),
            res,
            LockMode::X,
            LockTimeout::Infinite,
            &LockWait::none(),
        )
    });
    await_waiter(&mgr, TxnId(2));

    // A, which holds the S, asks for the X: it waits for the other holders — there are
    // none — and not for B, which is waiting for A itself. Queued behind B instead, this
    // call would wait for a lock B cannot get and return `TimedOut` at 500 ms.
    assert_eq!(
        mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::Millis(500), &wait),
        LockOutcome::Granted,
        "the conversion of A passes ahead of B"
    );
    assert_eq!(mgr.held(TxnId(1)), vec![(res, LockMode::X)]);
    assert_eq!(
        mgr.waiters(),
        vec![(TxnId(2), res, LockMode::X)],
        "B is still waiting"
    );

    mgr.release_all(TxnId(1));
    assert_eq!(b.join().expect("B ends"), LockOutcome::Granted);
}

#[test]
fn reentrant_lock_is_free() {
    let mgr = LockManager::new();
    let res = row_res();
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);

    for weaker in [
        LockMode::X,
        LockMode::U,
        LockMode::S,
        LockMode::IS,
        LockMode::IX,
    ] {
        assert_eq!(
            mgr.try_acquire(TxnId(1), res, weaker, LockTimeout::NoWait, &wait),
            LockOutcome::Granted,
            "{weaker:?} is covered by the X already held"
        );
    }
    assert_eq!(
        mgr.held(TxnId(1)),
        vec![(res, LockMode::X)],
        "a covered request does not weaken the lock, and queues nothing"
    );
    assert!(mgr.waiters().is_empty());

    // The other family is a second lock, not a conversion.
    let table = LockResource::Table(TableId(1));
    mgr.try_acquire(TxnId(1), table, LockMode::IX, LockTimeout::NoWait, &wait);
    mgr.try_acquire(TxnId(1), table, LockMode::SchS, LockTimeout::NoWait, &wait);
    let mut held = mgr.held(TxnId(1));
    held.sort();
    assert_eq!(
        held,
        vec![
            (row_res(), LockMode::X),
            (table, LockMode::IX),
            (table, LockMode::SchS),
        ]
    );
}

// ------------------------------------------------------------------ The timeouts

#[test]
fn nowait_returns_at_once() {
    let mgr = LockManager::new();
    let res = row_res();
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);

    let started = Instant::now();
    let outcome = mgr.try_acquire(TxnId(2), res, LockMode::S, LockTimeout::NoWait, &wait);
    let elapsed = started.elapsed();
    assert_eq!(outcome, LockOutcome::TimedOut);
    assert!(
        elapsed < Duration::from_millis(250),
        "NoWait parks no thread; took {elapsed:?}"
    );
    assert!(
        mgr.waiters().is_empty(),
        "the refused candidate left the queue"
    );
}

#[test]
fn millis_times_out() {
    let mgr = LockManager::new();
    let res = row_res();
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);

    let started = Instant::now();
    let outcome = mgr.try_acquire(TxnId(2), res, LockMode::S, LockTimeout::Millis(50), &wait);
    let elapsed = started.elapsed();
    assert_eq!(outcome, LockOutcome::TimedOut);
    assert!(
        elapsed >= Duration::from_millis(50),
        "Millis(50) gives up at 50 ms at the earliest; took {elapsed:?}"
    );
    assert!(mgr.waiters().is_empty());
    assert_eq!(mgr.held(TxnId(1)), vec![(res, LockMode::X)]);
}

#[test]
fn a_cancelled_wait_returns_cancelled() {
    let mgr = Arc::new(LockManager::new());
    let res = row_res();
    mgr.try_acquire(
        TxnId(1),
        res,
        LockMode::X,
        LockTimeout::NoWait,
        &LockWait::none(),
    );

    let token = LockWait::none();
    let (tx, rx) = channel();
    let reader = Arc::clone(&mgr);
    let theirs = token.clone();
    let handle = thread::spawn(move || {
        let outcome =
            reader.try_acquire(TxnId(2), res, LockMode::S, LockTimeout::Infinite, &theirs);
        tx.send(outcome).expect("the test still listens");
    });
    await_waiter(&mgr, TxnId(2));
    assert_quiet(&rx, "the waiter before the cancellation");

    token.cancel();
    assert_eq!(
        rx.recv_timeout(SOON).expect("the wait gives up"),
        LockOutcome::Cancelled,
        "an interrupted wait is told apart from a timeout"
    );
    assert!(mgr.waiters().is_empty());
    assert_eq!(mgr.held(TxnId(1)), vec![(res, LockMode::X)], "A kept its X");
    handle.join().expect("the reader thread ends");
}

// --------------------------------------------------------- Resources and release

#[test]
fn row_and_table_are_distinct_resources() {
    let mgr = LockManager::new();
    let wait = LockWait::none();
    let first = LockResource::Row(TableId(7), RowId(1));
    let second = LockResource::Row(TableId(7), RowId(2));
    let table = LockResource::Table(TableId(7));
    mgr.try_acquire(TxnId(1), first, LockMode::X, LockTimeout::NoWait, &wait);

    assert_eq!(
        mgr.try_acquire(TxnId(2), second, LockMode::X, LockTimeout::NoWait, &wait),
        LockOutcome::Granted,
        "another row of the same table is another resource"
    );
    assert_eq!(
        mgr.try_acquire(TxnId(2), table, LockMode::SchS, LockTimeout::NoWait, &wait),
        LockOutcome::Granted,
        "the table itself is another resource"
    );
    // Counter-test: the same row is the same resource.
    assert_eq!(
        mgr.try_acquire(TxnId(2), first, LockMode::X, LockTimeout::NoWait, &wait),
        LockOutcome::TimedOut
    );
}

#[test]
fn release_all_wakes_every_waiter() {
    let mgr = Arc::new(LockManager::new());
    let res = row_res();
    mgr.try_acquire(
        TxnId(1),
        res,
        LockMode::X,
        LockTimeout::NoWait,
        &LockWait::none(),
    );

    let (tx, rx) = channel();
    let mut handles = Vec::new();
    for id in [TxnId(2), TxnId(3), TxnId(4)] {
        let mine = Arc::clone(&mgr);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            let outcome = mine.try_acquire(
                id,
                res,
                LockMode::S,
                LockTimeout::Infinite,
                &LockWait::none(),
            );
            tx.send((id, outcome)).expect("the test still listens");
        }));
    }
    drop(tx);
    await_waiter_count(&mgr, 3);
    assert_quiet(&rx, "the three waiters while A holds the X");

    mgr.release_all(TxnId(1));
    let mut woken: Vec<TxnId> = Vec::new();
    for _ in 0..3 {
        let (id, outcome) = rx.recv_timeout(SOON).expect("a waiter was woken");
        assert_eq!(outcome, LockOutcome::Granted);
        woken.push(id);
    }
    woken.sort();
    assert_eq!(
        woken,
        vec![TxnId(2), TxnId(3), TxnId(4)],
        "one release, three waiters served"
    );
    assert!(mgr.waiters().is_empty());
    for handle in handles {
        handle.join().expect("a waiter thread ends");
    }
}

#[test]
fn release_all_of_an_unknown_txn_is_quiet() {
    let mgr = LockManager::new();
    let res = row_res();
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);
    mgr.release_all(TxnId(99));
    mgr.release_all(TxnId(1));
    mgr.release_all(TxnId(1));
    assert_eq!(mgr.held(TxnId(1)), vec![]);
}

#[test]
fn unlock_hands_the_lock_to_the_next_waiter() {
    let mgr = Arc::new(LockManager::new());
    let res = row_res();
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);

    let reader = Arc::clone(&mgr);
    let b = thread::spawn(move || {
        reader.try_acquire(
            TxnId(2),
            res,
            LockMode::S,
            LockTimeout::Infinite,
            &LockWait::none(),
        )
    });
    await_waiter(&mgr, TxnId(2));

    mgr.unlock(TxnId(1), res).expect("A holds this row");
    assert_eq!(b.join().expect("B ends"), LockOutcome::Granted);
    assert_eq!(mgr.held(TxnId(1)), vec![]);
    assert_eq!(mgr.held(TxnId(2)), vec![(res, LockMode::S)]);
}

#[test]
fn unlock_without_a_lock_is_a_bug() {
    let mgr = LockManager::new();
    let res = row_res();
    let wait = LockWait::none();
    let unknown = mgr
        .unlock(TxnId(1), res)
        .expect_err("nothing is held anywhere");
    assert_eq!(unknown.number, 50000, "message was {:?}", unknown.message);

    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);
    let other = mgr
        .unlock(TxnId(2), res)
        .expect_err("the row is held by another transaction");
    assert_eq!(other.number, 50000, "message was {:?}", other.message);
    assert_eq!(mgr.held(TxnId(1)), vec![(res, LockMode::X)]);
}

#[test]
fn lock_reports_a_refusal_as_an_error() {
    let mgr = LockManager::new();
    let res = row_res();
    let wait = LockWait::none();
    mgr.lock(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait)
        .expect("the first X is free");
    let err = mgr
        .lock(TxnId(2), res, LockMode::S, LockTimeout::NoWait, &wait)
        .expect_err("the row is taken");
    assert_eq!(
        (err.number, err.state),
        (1222, 51),
        "a refused row lock is 1222, in its row state"
    );
}

// ------------------------------------------------- Through the transaction manager

/// A manager over an empty `MemoryStorage`, and the table identifier the tests lock rows
/// of. No row is written: `lock_row` locks an identifier, `storage` is not consulted.
fn manager() -> (TransactionManager, TableId) {
    (
        TransactionManager::new(Arc::new(MemoryStorage::new())),
        TableId(1),
    )
}

#[test]
fn commit_releases_locks() {
    let (mgr, table) = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.lock_row(&a, table, RowId(1), LockTimeout::NoWait)
        .expect("the row is free");
    assert_eq!(
        mgr.locks().held(a.id),
        vec![(LockResource::Row(table, RowId(1)), LockMode::X)],
        "lock_row takes an X on the row"
    );

    let b = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.lock_row(&b, table, RowId(1), LockTimeout::NoWait)
        .expect_err("A holds the row");

    mgr.commit(a).expect("commit A");
    assert_eq!(
        mgr.locks().held(TxnId(1)),
        vec![],
        "the commit gave it back"
    );
    mgr.lock_row(&b, table, RowId(1), LockTimeout::NoWait)
        .expect("B passes once A has committed");
}

#[test]
fn rollback_releases_locks() {
    let (mgr, table) = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.lock_row(&a, table, RowId(1), LockTimeout::NoWait)
        .expect("the row is free");
    mgr.rollback(a).expect("rollback A");
    assert_eq!(mgr.locks().held(TxnId(1)), vec![]);

    let b = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.lock_row(&b, table, RowId(1), LockTimeout::NoWait)
        .expect("B passes once A has rolled back");
}

/// The release is after `storage`, so a transaction `storage` refused to close keeps its
/// locks. The refusal is the one of `tests/txn_basic.rs`,
/// `a_refused_storage_commit_keeps_the_transaction_open`: a second manager over the same
/// storage hands out an identifier storage has already committed.
#[test]
fn a_refused_commit_keeps_the_locks() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage.create_database("txn").expect("create_database");
    let table = storage
        .create_table(db, &int_table_shape(1))
        .expect("create_table");

    let first = TransactionManager::new(Arc::clone(&storage));
    let done = first.begin(IsolationLevel::ReadCommitted);
    storage
        .insert(done.id, table, &row(&[1]))
        .expect("insert so that storage knows the identifier");
    first
        .commit(done)
        .expect("commit through the first manager");

    let other = TransactionManager::new(Arc::clone(&storage));
    let clash = other.begin(IsolationLevel::ReadCommitted);
    assert_eq!(clash.id, TxnId(1));
    other
        .lock_row(&clash, table, RowId(1), LockTimeout::NoWait)
        .expect("the row is free in the second manager");
    other.commit(clash).expect_err("storage refuses the commit");
    assert_eq!(
        other.locks().held(TxnId(1)),
        vec![(LockResource::Row(table, RowId(1)), LockMode::X)],
        "a transaction that stays open keeps its locks"
    );
}
