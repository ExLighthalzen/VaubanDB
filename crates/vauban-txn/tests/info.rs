//! What `active_sessions` and `active_locks` publish: the open transactions with their
//! state, the locks held and the requests waiting, and the strings the lock modes and
//! statuses render to.
//!
//! The scenarios run threads over an `Arc<TransactionManager>`, not SQL: no view and no
//! column is read here. A thread that must be waiting before a scenario goes on is attested
//! through [`LockManager::waiters`], as `tests/lock.rs` does, so that no assertion rests on
//! a thread having had the time to start.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use vauban_errors::SqlResult;
use vauban_storage::{MemoryStorage, RowId, TableId, TxnId};
use vauban_txn::{
    IsolationLevel, LockInfo, LockIntent, LockManager, LockMode, LockResource, LockStatus,
    LockTimeout, LockWait, TransactionManager, TxnState,
};

/// How long a test waits for something it expects to happen.
const SOON: Duration = Duration::from_secs(2);

/// The table the scenarios lock rows of.
const T: TableId = TableId(1);

/// The row the scenarios that need one row use.
const R: RowId = RowId(1);

/// A manager over an empty `MemoryStorage`: the methods under test lock identifiers and
/// list transactions, `storage` is not consulted.
fn manager() -> Arc<TransactionManager> {
    Arc::new(TransactionManager::new(Arc::new(MemoryStorage::new())))
}

/// The row resource of `R` in `T`.
fn row_res() -> LockResource {
    LockResource::Row(T, R)
}

/// Runs `f` on a thread of its own and hands back what it returns.
fn in_background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

/// Blocks until `txn` appears in the queue of `locks`, at most [`SOON`].
///
/// Panics otherwise: a scenario whose second thread never reached the queue would assert
/// nothing.
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

/// The lines of `active_locks` in the name of `txn`.
fn lines_of(mgr: &TransactionManager, txn: TxnId) -> Vec<LockInfo> {
    mgr.active_locks()
        .into_iter()
        .filter(|line| line.txn == txn)
        .collect()
}

/// The state `active_sessions` reports for `txn`; panics when the transaction is closed.
fn state_of(mgr: &TransactionManager, txn: TxnId) -> TxnState {
    mgr.active_sessions()
        .into_iter()
        .find(|info| info.id == txn)
        .unwrap_or_else(|| panic!("transaction {txn} is not open"))
        .state
}

// ------------------------------------------------------------------ Transactions

/// Three `begin`s give three `TxnInfo`s, oldest first, each at the level it asked for and
/// in no queue; one `commit` leaves two.
#[test]
fn active_sessions_lists_open_transactions() {
    let mgr = manager();
    let before = SystemTime::now();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let b = mgr.begin(IsolationLevel::RepeatableRead);
    let c = mgr.begin(IsolationLevel::Snapshot);
    let after = SystemTime::now();

    let active = mgr.active_sessions();
    assert_eq!(active.len(), 3);
    let ids: Vec<TxnId> = active.iter().map(|info| info.id).collect();
    assert_eq!(ids, vec![a.id, b.id, c.id], "oldest first");
    let levels: Vec<IsolationLevel> = active.iter().map(|info| info.isolation).collect();
    assert_eq!(
        levels,
        vec![
            IsolationLevel::ReadCommitted,
            IsolationLevel::RepeatableRead,
            IsolationLevel::Snapshot,
        ]
    );
    for info in &active {
        assert_eq!(info.state, TxnState::Active, "{}", info.id);
        assert_eq!(info.locks_held, 0, "{}", info.id);
        assert_eq!(info.deadlock_priority, 0, "{}", info.id);
        assert!(
            before <= info.began_at && info.began_at <= after,
            "{} began at {:?}, outside [{before:?}, {after:?}]",
            info.id,
            info.began_at
        );
    }

    mgr.commit(b).expect("commit B");
    let left: Vec<TxnId> = mgr.active_sessions().iter().map(|info| info.id).collect();
    assert_eq!(left, vec![a.id, c.id], "B is closed, A and C are not");
}

/// The `DEADLOCK_PRIORITY` a transaction set is the one its `TxnInfo` carries; the others
/// stay at `0`.
#[test]
fn deadlock_priority_is_reported() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let b = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.set_deadlock_priority(&a, -5);
    let priorities: Vec<(TxnId, i16)> = mgr
        .active_sessions()
        .iter()
        .map(|info| (info.id, info.deadlock_priority))
        .collect();
    assert_eq!(priorities, vec![(a.id, -5), (b.id, 0)]);
}

// ------------------------------------------------------------------ Locks held

/// A `REPEATABLE READ` read holds its `S` on the row: `active_locks` lists it at `GRANT`,
/// with the `IS` the read took on the table, and `locks_held` counts the two.
#[test]
fn held_locks_are_reported_as_grant() -> SqlResult<()> {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::RepeatableRead);
    mgr.read_lock(&a, T, R, &LockIntent::default(), &LockWait::none())?;

    let mine = lines_of(&mgr, a.id);
    let shape: Vec<(LockResource, LockMode, LockStatus, u64)> = mine
        .iter()
        .map(|line| (line.resource, line.mode, line.status, line.waiting_ms))
        .collect();
    assert!(
        shape.contains(&(row_res(), LockMode::S, LockStatus::Grant, 0)),
        "no S at GRANT on the row in {mine:?}"
    );
    assert!(
        shape.contains(&(LockResource::Table(T), LockMode::IS, LockStatus::Grant, 0)),
        "no IS at GRANT on the table in {mine:?}"
    );
    assert_eq!(mine.len(), 2, "{mine:?}");
    assert_eq!(mine[0].resource.resource_kind(), "ROW");
    assert_eq!(mine[1].resource.resource_kind(), "OBJECT");

    let info = mgr
        .active_sessions()
        .into_iter()
        .find(|info| info.id == a.id)
        .expect("A is open");
    assert_eq!(info.locks_held, 2);
    assert_eq!(info.state, TxnState::Active);

    mgr.commit(a)?;
    assert!(mgr.active_locks().is_empty(), "commit gave the locks back");
    Ok(())
}

// ------------------------------------------------------------------ Waiting

/// While B waits for the `X` A holds, `active_locks` lists B at `WAIT` on that row, its
/// `TxnInfo` is `Waiting` on it, and `waiting_ms` grows between two calls.
#[test]
fn a_waiting_txn_is_visible() -> SqlResult<()> {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let b = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.lock_row(&a, T, R, LockTimeout::NoWait)?;

    let b_id = b.id;
    let rx = {
        let mgr = Arc::clone(&mgr);
        in_background(move || mgr.lock_row(&b, T, R, LockTimeout::Infinite).map(|()| b))
    };
    await_waiter(mgr.locks(), b_id);

    let first = lines_of(&mgr, b_id);
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].resource, row_res());
    assert_eq!(first[0].mode, LockMode::X);
    assert_eq!(first[0].status, LockStatus::Wait);
    assert_eq!(
        state_of(&mgr, b_id),
        TxnState::Waiting { on: row_res() },
        "B waits on the row"
    );
    assert_eq!(
        state_of(&mgr, a.id),
        TxnState::Active,
        "A holds, it waits on nothing"
    );
    let holder = lines_of(&mgr, a.id);
    assert_eq!(holder.len(), 1, "{holder:?}");
    assert_eq!(holder[0].status, LockStatus::Grant);
    assert_eq!(holder[0].waiting_ms, 0, "a holder is not waiting");

    thread::sleep(Duration::from_millis(20));
    let second = lines_of(&mgr, b_id);
    assert_eq!(second.len(), 1, "{second:?}");
    assert!(
        second[0].waiting_ms > first[0].waiting_ms,
        "waiting_ms went from {} to {}",
        first[0].waiting_ms,
        second[0].waiting_ms
    );

    mgr.commit(a)?;
    let b = rx.recv_timeout(SOON).expect("B answered")?;
    assert_eq!(state_of(&mgr, b_id), TxnState::Active, "B holds the X now");
    assert_eq!(lines_of(&mgr, b_id)[0].status, LockStatus::Grant);
    mgr.commit(b)?;
    assert!(mgr.active_locks().is_empty());
    Ok(())
}

/// A transaction asking for a stronger mode on a row it already holds an `S` on, while
/// another `S` stands in its way, is listed at `CONVERT` towards `X`, its `S` still at
/// `GRANT`, and its `TxnInfo` is `Waiting` on that row.
#[test]
fn a_conversion_is_reported_as_convert() -> SqlResult<()> {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::RepeatableRead);
    let b = mgr.begin(IsolationLevel::RepeatableRead);
    mgr.read_lock(&a, T, R, &LockIntent::default(), &LockWait::none())?;
    mgr.read_lock(&b, T, R, &LockIntent::default(), &LockWait::none())?;

    let a_id = a.id;
    let rx = {
        let mgr = Arc::clone(&mgr);
        in_background(move || mgr.lock_row(&a, T, R, LockTimeout::Infinite).map(|()| a))
    };
    await_waiter(mgr.locks(), a_id);

    let on_row: Vec<LockInfo> = lines_of(&mgr, a_id)
        .into_iter()
        .filter(|line| line.resource == row_res())
        .collect();
    let statuses: Vec<(LockMode, LockStatus)> =
        on_row.iter().map(|line| (line.mode, line.status)).collect();
    assert_eq!(
        statuses,
        vec![
            (LockMode::S, LockStatus::Grant),
            (LockMode::X, LockStatus::Convert)
        ],
        "{on_row:?}"
    );
    assert_eq!(state_of(&mgr, a_id), TxnState::Waiting { on: row_res() });

    mgr.commit(b)?;
    let a = rx.recv_timeout(SOON).expect("A answered")?;
    mgr.commit(a)?;
    Ok(())
}

// ------------------------------------------------------------------ Strings

/// The seven modes render to the short names of the lock modes table, held here as data.
#[test]
fn mode_strings_match_the_learn_names() {
    let table = [
        (LockMode::S, "S"),
        (LockMode::X, "X"),
        (LockMode::U, "U"),
        (LockMode::IS, "IS"),
        (LockMode::IX, "IX"),
        (LockMode::SchS, "Sch-S"),
        (LockMode::SchM, "Sch-M"),
    ];
    for (mode, name) in table {
        assert_eq!(mode.to_string(), name, "{mode:?}");
        assert_eq!(mode.short_name(), name, "{mode:?}");
    }
    assert_eq!(LockStatus::Grant.to_string(), "GRANT");
    assert_eq!(LockStatus::Wait.to_string(), "WAIT");
    assert_eq!(LockStatus::Convert.to_string(), "CONVERT");
}

// ------------------------------------------------------------------ Consistency

/// While two threads take and give back locks on one row in a loop, a hundred calls to
/// `active_locks` complete and each of them shows, per resource, holders whose modes are
/// pairwise compatible when they belong to different transactions, and a request at
/// `CONVERT` next to a lock its transaction holds on the same resource.
#[test]
fn snapshot_of_locks_is_consistent() {
    let mgr = manager();
    let stop = Arc::new(AtomicBool::new(false));

    let spin = |txn: TxnId, first: LockMode, then: LockMode| {
        let mgr = Arc::clone(&mgr);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let wait = LockWait::none();
            let slice = LockTimeout::Millis(50);
            while !stop.load(Ordering::SeqCst) {
                let locks = mgr.locks();
                locks.try_acquire(txn, row_res(), first, slice, &wait);
                locks.try_acquire(txn, row_res(), then, slice, &wait);
                locks.try_acquire(txn, LockResource::Table(T), LockMode::IX, slice, &wait);
                locks.release_all(txn);
            }
        })
    };
    // Two transactions that both read the row and then convert to X: the conversions
    // stand against each other, so holders, waiters and conversions are seen together.
    let one = spin(TxnId(1), LockMode::S, LockMode::X);
    let two = spin(TxnId(2), LockMode::S, LockMode::X);

    for round in 0..100 {
        let lines = mgr.active_locks();
        for (i, line) in lines.iter().enumerate() {
            if line.status == LockStatus::Grant {
                for other in &lines[i + 1..] {
                    if other.resource == line.resource
                        && other.status == LockStatus::Grant
                        && other.txn != line.txn
                    {
                        assert!(
                            line.mode.compatible_with(other.mode)
                                && other.mode.compatible_with(line.mode),
                            "round {round}: {line:?} and {other:?} are both held"
                        );
                    }
                }
            }
            if line.status == LockStatus::Convert {
                assert!(
                    lines.iter().any(|held| held.txn == line.txn
                        && held.resource == line.resource
                        && held.status == LockStatus::Grant
                        && held.mode.is_data() == line.mode.is_data()),
                    "round {round}: {line:?} converts from nothing in {lines:?}"
                );
            }
        }
        thread::sleep(Duration::from_millis(1));
    }

    stop.store(true, Ordering::SeqCst);
    one.join().expect("thread one");
    two.join().expect("thread two");
    assert!(
        mgr.active_locks().is_empty(),
        "both threads gave back what they held"
    );
}
