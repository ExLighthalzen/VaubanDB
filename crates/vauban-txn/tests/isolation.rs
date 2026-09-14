//! The read policy: which mode a row read takes at each isolation level and under each
//! lock hint, and until when it is held.
//!
//! The scenarios run threads over an `Arc<TransactionManager>`, not SQL: this file knows no
//! T-SQL text. What SQL Server does on each shape is the table in the module documentation
//! of `crates/vauban-txn/src/isolation.rs`.
//!
//! # Axes
//!
//! Crossed here: the five [`IsolationLevel`]s × the hints `NOLOCK`, `HOLDLOCK`, `UPDLOCK`,
//! `XLOCK`, `READPAST`, `NOWAIT` and the per-table level hint × (a reader alone / a reader
//! against a writer / a writer against a reader) × (one row read once / the same row read
//! twice by one transaction). On the second reading of a row, the read whose lock is held to
//! the end of the transaction decides in both orders
//! (`a_held_read_overrules_an_earlier_released_read_of_the_same_row`,
//! `a_plain_repeatable_read_overrules_an_earlier_read_committed_hint`, the two double-read
//! rows of the table in `src/isolation.rs`); two reads that both release and are both still
//! open share one entry, so the first `end_row_read` gives the shared lock back while the
//! other read is going (`two_open_reads_of_one_row_give_the_lock_back_at_the_first_end`),
//! the bound the documentation of `SHARED_UNTIL_END_OF_ROW` carries. Not crossed, and why:
//! `TABLOCK` and `TABLOCKX`, which are not applied — the one test here checks they change
//! nothing; `Sch-S` and `Sch-M`, which `tests/schema_lock.rs` covers; the database options
//! and the `SNAPSHOT` level, not served yet; range locks, which this engine does not model
//! (`serializable_locks_the_rows_it_read_not_the_range`); the numbers 1222 and 1205 beyond
//! the one refusal `nowait_yields_1222` asserts, which `tests/deadlock.rs` covers.
//!
//! A thread that must be waiting before a scenario goes on is attested through
//! [`LockManager::waiters`], as `tests/lock.rs` does, so that no assertion rests on a thread
//! having had the time to start.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use vauban_storage::{MemoryStorage, RowId, TableId, TxnId};
use vauban_txn::{
    IsolationLevel, LockIntent, LockManager, LockMode, LockResource, ReadAccess,
    TransactionManager, TxnHandle,
};

/// How long a test waits for something it expects to happen.
const SOON: Duration = Duration::from_secs(2);

/// How long a test waits before concluding that nothing is coming.
const QUIET: Duration = Duration::from_millis(100);

/// The table every scenario locks rows of.
const T: TableId = TableId(1);

/// The row every scenario that needs one row uses.
const R: RowId = RowId(1);

/// A manager over an empty `MemoryStorage`. No row is written: the manager locks
/// identifiers, `storage` is not consulted by the four methods under test.
fn manager() -> Arc<TransactionManager> {
    Arc::new(TransactionManager::new(Arc::new(MemoryStorage::new())))
}

/// The row resource of `R` in `T`.
fn row_res() -> LockResource {
    LockResource::Row(T, R)
}

/// The table resource of `T`.
fn table_res() -> LockResource {
    LockResource::Table(T)
}

/// No hint at all.
fn plain() -> LockIntent {
    LockIntent::default()
}

/// A hint that asks not to wait, so that a refusal is an error instead of a hang.
fn nowait() -> LockIntent {
    LockIntent {
        nowait: true,
        ..LockIntent::default()
    }
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

/// Asserts that nothing came out of `rx` within [`QUIET`].
fn still_waiting<T: std::fmt::Debug>(rx: &Receiver<T>, what: &str) {
    match rx.recv_timeout(QUIET) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("{what} was expected to wait, it answered {other:?}"),
    }
}

/// `true` when `txn` holds `mode` on `resource` right now.
fn holds(
    mgr: &TransactionManager,
    txn: &TxnHandle,
    resource: LockResource,
    mode: LockMode,
) -> bool {
    mgr.locks().held(txn.id).contains(&(resource, mode))
}

// ------------------------------------------------------------------ The four levels

/// `READ COMMITTED`: the reader waits for the exclusive lock of the writer, passes at its
/// commit, and gives its shared lock back at the end of the row read.
#[test]
fn read_committed_blocks_on_x_then_releases() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&a, T, R, &plain()).expect("A takes the row");

    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let (reader, handle) = (Arc::clone(&mgr), b.clone());
    let (started_tx, started) = channel();
    let ended = in_background(move || {
        let access = reader.read_lock(&handle, T, R, &plain()).expect("B reads");
        started_tx.send(access).expect("the test is listening");
        reader.end_row_read(&handle, T, R).expect("B ends its row")
    });
    await_waiter(mgr.locks(), b.id);
    still_waiting(&started, "the READ COMMITTED reader");

    mgr.commit(a).expect("A commits");
    assert_eq!(
        started.recv_timeout(SOON),
        Ok(ReadAccess::Locked),
        "the reader passes at the commit of the writer"
    );
    ended.recv_timeout(SOON).expect("end_row_read returns");
    assert!(
        !holds(&mgr, &b, row_res(), LockMode::S),
        "the shared lock is gone: {:?}",
        mgr.locks().held(b.id)
    );
}

/// `REPEATABLE READ`: the shared lock survives the row read and makes a writer wait until
/// the reader commits.
#[test]
fn repeatable_read_holds_the_share_lock() {
    let mgr = manager();
    let b = mgr.begin(IsolationLevel::RepeatableRead);
    assert_eq!(
        mgr.read_lock(&b, T, R, &plain()).expect("B reads"),
        ReadAccess::Locked
    );
    mgr.end_row_read(&b, T, R).expect("B ends its row");
    assert!(
        holds(&mgr, &b, row_res(), LockMode::S),
        "the shared lock is still held: {:?}",
        mgr.locks().held(b.id)
    );

    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let (writer, handle) = (Arc::clone(&mgr), a.clone());
    let written = in_background(move || writer.write_lock(&handle, T, R, &plain()));
    await_waiter(mgr.locks(), a.id);
    still_waiting(&written, "the writer against a REPEATABLE READ reader");

    mgr.commit(b).expect("B commits");
    written
        .recv_timeout(SOON)
        .expect("the writer is woken")
        .expect("the writer takes the row");
}

/// The counter-test of `repeatable_read_holds_the_share_lock`: the same scenario under
/// `READ COMMITTED`, where the writer does not wait for the reader.
#[test]
fn read_committed_lets_the_writer_through() {
    let mgr = manager();
    let b = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.read_lock(&b, T, R, &plain()).expect("B reads");
    mgr.end_row_read(&b, T, R).expect("B ends its row");

    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let (writer, handle) = (Arc::clone(&mgr), a.clone());
    let written = in_background(move || writer.write_lock(&handle, T, R, &plain()));
    written
        .recv_timeout(SOON)
        .expect("the writer answers without waiting for a commit")
        .expect("the writer takes the row");
    assert!(holds(&mgr, &a, row_res(), LockMode::X));
}

/// `READ UNCOMMITTED`: no lock is asked for, so the reader neither waits nor holds
/// anything while another transaction holds the exclusive lock.
#[test]
fn read_uncommitted_does_not_block() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&a, T, R, &plain()).expect("A takes the row");

    let b = mgr.begin(IsolationLevel::ReadUncommitted);
    // In the background with a deadline: a read that took a shared lock here would wait for
    // the exclusive lock of A, and this test would hang instead of failing.
    let (reader, handle) = (Arc::clone(&mgr), b.clone());
    let read = in_background(move || reader.read_lock(&handle, T, R, &plain()));
    assert_eq!(
        read.recv_timeout(SOON)
            .expect("the dirty read answers without waiting")
            .expect("B reads"),
        ReadAccess::Dirty
    );
    assert_eq!(
        mgr.locks().held(b.id),
        vec![],
        "a dirty read takes no lock, not even an intent lock"
    );
    mgr.end_row_read(&b, T, R).expect("nothing to give back");
    assert_eq!(mgr.locks().held(b.id), vec![]);
}

/// A `NOLOCK` hint on one table stands in front of the level of the transaction.
#[test]
fn nolock_hint_overrides_the_level() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&a, T, R, &plain()).expect("A takes the row");

    let b = mgr.begin(IsolationLevel::RepeatableRead);
    let hints = LockIntent {
        level: Some(IsolationLevel::ReadUncommitted),
        ..LockIntent::default()
    };
    assert_eq!(
        mgr.effective_level(&b, &hints),
        IsolationLevel::ReadUncommitted
    );
    let (reader, handle) = (Arc::clone(&mgr), b.clone());
    let read = in_background(move || reader.read_lock(&handle, T, R, &hints));
    assert_eq!(
        read.recv_timeout(SOON)
            .expect("the hinted read answers without waiting")
            .expect("B reads"),
        ReadAccess::Dirty
    );
    assert_eq!(mgr.locks().held(b.id), vec![]);
}

/// The four lock-based levels give their locks back at the commit, row locks and intent
/// locks alike.
#[test]
fn commit_releases_every_level() {
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
    ] {
        let mgr = manager();
        let txn = mgr.begin(level);
        mgr.read_lock(&txn, T, R, &plain()).expect("the read");
        mgr.write_lock(&txn, T, RowId(2), &plain())
            .expect("the write");
        assert!(
            !mgr.locks().held(txn.id).is_empty(),
            "{level:?} holds something before the commit"
        );
        mgr.commit(txn.clone()).expect("the commit");
        assert_eq!(
            mgr.locks().held(txn.id),
            vec![],
            "{level:?} holds nothing after the commit"
        );
    }
}

// ------------------------------------------------------------------- The level hints

/// The per-table hint decides in both directions, which is what tells it from the level of
/// the transaction: a `READCOMMITTED` hint gives the shared lock back inside a
/// `REPEATABLE READ` transaction.
#[test]
fn a_read_committed_hint_lowers_a_repeatable_read_transaction() {
    let mgr = manager();
    let b = mgr.begin(IsolationLevel::RepeatableRead);
    let hints = LockIntent {
        level: Some(IsolationLevel::ReadCommitted),
        ..LockIntent::default()
    };
    mgr.read_lock(&b, T, R, &hints).expect("B reads");
    mgr.end_row_read(&b, T, R).expect("B ends its row");
    assert!(
        !holds(&mgr, &b, row_res(), LockMode::S),
        "the hint gave the shared lock back: {:?}",
        mgr.locks().held(b.id)
    );
}

/// The other direction: a `REPEATABLEREAD` hint holds the shared lock inside a
/// `READ COMMITTED` transaction.
#[test]
fn a_repeatable_read_hint_raises_a_read_committed_transaction() {
    let mgr = manager();
    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let hints = LockIntent {
        level: Some(IsolationLevel::RepeatableRead),
        ..LockIntent::default()
    };
    mgr.read_lock(&b, T, R, &hints).expect("B reads");
    mgr.end_row_read(&b, T, R).expect("B ends its row");
    assert!(
        holds(&mgr, &b, row_res(), LockMode::S),
        "the hint held the shared lock: {:?}",
        mgr.locks().held(b.id)
    );
}

/// `HOLDLOCK` is the `SERIALIZABLE` hint: it raises the level of the read and holds the
/// shared lock past the end of the row.
#[test]
fn holdlock_is_the_serializable_hint() {
    let mgr = manager();
    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let hints = LockIntent {
        holdlock: true,
        ..LockIntent::default()
    };
    assert_eq!(
        mgr.effective_level(&b, &hints),
        IsolationLevel::Serializable
    );
    mgr.read_lock(&b, T, R, &hints).expect("B reads");
    mgr.end_row_read(&b, T, R).expect("B ends its row");
    assert!(holds(&mgr, &b, row_res(), LockMode::S));
}

/// The level of a read, hint by hint: the hint when it names one, `HOLDLOCK` when it is the
/// only hint that names one, the level of the transaction otherwise.
#[test]
fn effective_level_prefers_the_hint() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!(
        mgr.effective_level(&txn, &plain()),
        IsolationLevel::ReadCommitted
    );
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
        IsolationLevel::Snapshot,
    ] {
        let hints = LockIntent {
            level: Some(level),
            ..LockIntent::default()
        };
        assert_eq!(mgr.effective_level(&txn, &hints), level);
        let with_holdlock = LockIntent {
            holdlock: true,
            ..hints
        };
        assert_eq!(
            mgr.effective_level(&txn, &with_holdlock),
            level,
            "the level hint is read before HOLDLOCK; the pair is refused with 1047 before \
             the manager is reached"
        );
    }
}

// ------------------------------------------------------- One row read twice in one transaction

/// Plain read then the same row under a `REPEATABLEREAD` hint, inside a `READ COMMITTED`
/// transaction: the read that holds its lock to the end over-rules the one that was to give
/// it back, so `end_row_read` gives nothing back and a writer waits for the commit.
///
/// SQL Server does the same: when B in `READ COMMITTED` reads a row plainly then reads it
/// again `WITH (REPEATABLEREAD)`, the `UPDATE` of A waits for the `COMMIT` of B; with the
/// plain read alone, the `UPDATE` of A goes through.
#[test]
fn a_held_read_overrules_an_earlier_released_read_of_the_same_row() {
    let mgr = manager();
    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let held = LockIntent {
        level: Some(IsolationLevel::RepeatableRead),
        ..LockIntent::default()
    };
    mgr.read_lock(&b, T, R, &plain()).expect("B reads plainly");
    mgr.read_lock(&b, T, R, &held)
        .expect("B reads under a hint");
    mgr.end_row_read(&b, T, R).expect("B ends its row");
    assert!(
        holds(&mgr, &b, row_res(), LockMode::S),
        "the shared lock the hint held is still held: {:?}",
        mgr.locks().held(b.id)
    );

    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let (writer, handle) = (Arc::clone(&mgr), a.clone());
    let written = in_background(move || writer.write_lock(&handle, T, R, &plain()));
    await_waiter(mgr.locks(), a.id);
    still_waiting(&written, "the writer against the row read twice");

    mgr.commit(b).expect("B commits");
    written
        .recv_timeout(SOON)
        .expect("the writer is woken")
        .expect("the writer takes the row");
}

/// The other order: a `READCOMMITTED` hint then a plain read of the same row, inside a
/// `REPEATABLE READ` transaction. The plain read holds its `S` to the end, so the entry the
/// hinted read left goes and `end_row_read` gives nothing back.
///
/// SQL Server does the same: the `UPDATE` of A waits for the `COMMIT` of B; with the hinted
/// read alone, the `UPDATE` of A goes through, which is also what
/// `a_read_committed_hint_lowers_a_repeatable_read_transaction` asserts here.
#[test]
fn a_plain_repeatable_read_overrules_an_earlier_read_committed_hint() {
    let mgr = manager();
    let b = mgr.begin(IsolationLevel::RepeatableRead);
    let lowered = LockIntent {
        level: Some(IsolationLevel::ReadCommitted),
        ..LockIntent::default()
    };
    mgr.read_lock(&b, T, R, &lowered).expect("B reads lowered");
    mgr.read_lock(&b, T, R, &plain()).expect("B reads plainly");
    mgr.end_row_read(&b, T, R).expect("B ends its row");
    assert!(
        holds(&mgr, &b, row_res(), LockMode::S),
        "the shared lock of the plain read is still held: {:?}",
        mgr.locks().held(b.id)
    );

    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let (writer, handle) = (Arc::clone(&mgr), a.clone());
    let written = in_background(move || writer.write_lock(&handle, T, R, &plain()));
    await_waiter(mgr.locks(), a.id);
    still_waiting(&written, "the writer against the row read twice");

    mgr.commit(b).expect("B commits");
    written
        .recv_timeout(SOON)
        .expect("the writer is woken")
        .expect("the writer takes the row");
}

/// The shape the entry does not count: two releasing reads of one row open at once share
/// one entry, so the first `end_row_read` of the two gives the shared lock back while the
/// other read is still going.
///
/// This is the bound written in the documentation of `SHARED_UNTIL_END_OF_ROW`, asserted
/// here so that a change of the entry is a change of this test. It has no SQL
/// counterpart: `end_row_read` is an API of the executor, which brackets one row read at a
/// time on the thread that reads it.
#[test]
fn two_open_reads_of_one_row_give_the_lock_back_at_the_first_end() {
    let mgr = manager();
    let b = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.read_lock(&b, T, R, &plain()).expect("the outer read");
    mgr.read_lock(&b, T, R, &plain()).expect("the inner read");
    mgr.end_row_read(&b, T, R).expect("the inner read ends");
    assert!(
        !holds(&mgr, &b, row_res(), LockMode::S),
        "one entry for two reads: the first end gives the lock back: {:?}",
        mgr.locks().held(b.id)
    );
    mgr.end_row_read(&b, T, R).expect("the outer read ends");
    assert!(
        holds(&mgr, &b, table_res(), LockMode::IS),
        "the intent lock stays to the end of the transaction: {:?}",
        mgr.locks().held(b.id)
    );
}

// ------------------------------------------------------------------- The mode hints

/// `UPDLOCK` takes a `U` in place of the `S`, and that `U` outlives the row read.
#[test]
fn updlock_is_held_to_the_end() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    let hints = LockIntent {
        updlock: true,
        ..LockIntent::default()
    };
    assert_eq!(
        mgr.read_lock(&txn, T, R, &hints).expect("the read"),
        ReadAccess::Locked
    );
    assert!(holds(&mgr, &txn, row_res(), LockMode::U));
    assert!(holds(&mgr, &txn, table_res(), LockMode::IX));
    mgr.end_row_read(&txn, T, R).expect("the end of the row");
    assert!(
        holds(&mgr, &txn, row_res(), LockMode::U),
        "the update lock is held to the end of the transaction: {:?}",
        mgr.locks().held(txn.id)
    );
    mgr.commit(txn.clone()).expect("the commit");
    assert_eq!(mgr.locks().held(txn.id), vec![]);
}

/// A `U` refuses another `U` on the same row and lets a plain read through, the shape the
/// compatibility matrix holds and SQL Server shows.
#[test]
fn updlock_refuses_updlock_and_lets_a_plain_read_through() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let updlock = LockIntent {
        updlock: true,
        ..LockIntent::default()
    };
    mgr.read_lock(&a, T, R, &updlock).expect("A takes the U");

    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let second_u = LockIntent {
        updlock: true,
        nowait: true,
        ..LockIntent::default()
    };
    mgr.read_lock(&b, T, R, &second_u)
        .expect_err("a second UPDLOCK is refused");
    assert_eq!(
        mgr.read_lock(&b, T, R, &nowait()).expect("a plain read"),
        ReadAccess::Locked
    );
}

/// `UPDLOCK` is read before the level, so it takes its `U` under `READ UNCOMMITTED` too.
#[test]
fn updlock_under_read_uncommitted_still_takes_the_u() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadUncommitted);
    let hints = LockIntent {
        updlock: true,
        ..LockIntent::default()
    };
    assert_eq!(
        mgr.read_lock(&txn, T, R, &hints).expect("the read"),
        ReadAccess::Locked
    );
    assert!(holds(&mgr, &txn, row_res(), LockMode::U));
}

/// `XLOCK` takes an `X` on a row it only reads, which makes another reader wait.
#[test]
fn xlock_blocks_a_reader() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    let hints = LockIntent {
        xlock: true,
        ..LockIntent::default()
    };
    mgr.read_lock(&a, T, R, &hints).expect("A reads with XLOCK");
    assert!(holds(&mgr, &a, row_res(), LockMode::X));
    mgr.end_row_read(&a, T, R).expect("the end of the row");
    assert!(
        holds(&mgr, &a, row_res(), LockMode::X),
        "the exclusive lock is held to the end of the transaction"
    );

    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let (reader, handle) = (Arc::clone(&mgr), b.clone());
    let read = in_background(move || reader.read_lock(&handle, T, R, &plain()));
    await_waiter(mgr.locks(), b.id);
    still_waiting(&read, "the reader against an XLOCK");
    mgr.commit(a).expect("A commits");
    assert_eq!(
        read.recv_timeout(SOON)
            .expect("the reader is woken")
            .expect("the reader takes the row"),
        ReadAccess::Locked
    );
}

/// `READPAST` leaves out a row it cannot lock, without an error and without waiting, and
/// keeps reading the rows it can lock.
#[test]
fn readpast_skips_instead_of_waiting() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&a, T, R, &plain()).expect("A takes row 1");

    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let hints = LockIntent {
        readpast: true,
        ..LockIntent::default()
    };
    assert_eq!(
        mgr.read_lock(&b, T, R, &hints).expect("no error"),
        ReadAccess::Skip
    );
    assert_eq!(
        mgr.read_lock(&b, T, RowId(2), &hints).expect("no error"),
        ReadAccess::Locked,
        "the rows it can lock are still read"
    );
    assert!(
        holds(&mgr, &b, table_res(), LockMode::IS),
        "the intent lock of the table is taken before the row and kept: {:?}",
        mgr.locks().held(b.id)
    );
    assert!(!holds(&mgr, &b, row_res(), LockMode::S));
}

/// `READPAST` is a hint of the read path: a write that cannot take its row reports the
/// refusal instead of leaving the row out.
#[test]
fn readpast_does_not_apply_to_a_write() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&a, T, R, &plain()).expect("A takes the row");

    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let hints = LockIntent {
        readpast: true,
        nowait: true,
        ..LockIntent::default()
    };
    mgr.write_lock(&b, T, R, &hints)
        .expect_err("the write reports the refused row");
}

/// `NOWAIT` reports a refused lock instead of waiting for it.
///
/// The number is 1222, state 51: a refused wait on a row is the error SQL Server sends on
/// the same shape (module documentation of `src/isolation.rs`).
#[test]
fn nowait_yields_1222() {
    let mgr = manager();
    let a = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&a, T, R, &plain()).expect("A takes the row");

    let b = mgr.begin(IsolationLevel::ReadCommitted);
    let refused = mgr
        .read_lock(&b, T, R, &nowait())
        .expect_err("the row is taken");
    assert_eq!(
        (refused.number, refused.state),
        (1222, 51),
        "a refused row wait under NOWAIT is 1222/51; message was {:?}",
        refused.message
    );
}

// ------------------------------------------------------------------ Intent and release

/// The hierarchy: `IS` on the table under a shared row read, `IX` under a write.
#[test]
fn intent_locks_are_taken() {
    let mgr = manager();
    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.read_lock(&reader, T, R, &plain()).expect("the read");
    assert!(
        holds(&mgr, &reader, table_res(), LockMode::IS),
        "held after the read: {:?}",
        mgr.locks().held(reader.id)
    );

    let writer = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&writer, T, RowId(2), &plain())
        .expect("the write");
    assert!(
        holds(&mgr, &writer, table_res(), LockMode::IX),
        "held after the write: {:?}",
        mgr.locks().held(writer.id)
    );
}

/// The intent lock of the table outlives the row read that took it.
#[test]
fn end_row_read_keeps_the_intent_lock() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.read_lock(&txn, T, R, &plain()).expect("the read");
    mgr.end_row_read(&txn, T, R).expect("the end of the row");
    assert_eq!(
        mgr.locks().held(txn.id),
        vec![(table_res(), LockMode::IS)],
        "the row lock is gone, the intent lock stays"
    );
}

/// A row read then written keeps the exclusive lock the write took: `end_row_read` gives
/// back a shared lock, not the conversion that replaced it.
#[test]
fn a_row_converted_to_x_keeps_its_lock() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.read_lock(&txn, T, R, &plain()).expect("the read");
    mgr.write_lock(&txn, T, R, &plain()).expect("the write");
    mgr.end_row_read(&txn, T, R).expect("the end of the row");
    assert!(
        holds(&mgr, &txn, row_res(), LockMode::X),
        "held after the end of the row: {:?}",
        mgr.locks().held(txn.id)
    );
}

/// `end_row_read` without a row read of its own gives nothing back and reports no error.
#[test]
fn end_row_read_without_a_read_is_quiet() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.end_row_read(&txn, T, R).expect("nothing to give back");
    mgr.write_lock(&txn, T, R, &plain()).expect("the write");
    mgr.end_row_read(&txn, T, R)
        .expect("a lock this method did not take");
    assert!(
        holds(&mgr, &txn, row_res(), LockMode::X),
        "the write keeps its lock: {:?}",
        mgr.locks().held(txn.id)
    );
}

/// The bound of the per-thread entry: a row read whose two halves run on two threads keeps
/// its shared lock to the end of the transaction.
#[test]
fn a_row_read_split_over_two_threads_holds_its_lock() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    let (reader, handle) = (Arc::clone(&mgr), txn.clone());
    in_background(move || reader.read_lock(&handle, T, R, &plain()))
        .recv_timeout(SOON)
        .expect("the read returns")
        .expect("the read takes the row");
    mgr.end_row_read(&txn, T, R)
        .expect("the other thread knows of no entry");
    assert!(
        holds(&mgr, &txn, row_res(), LockMode::S),
        "the shared lock stays: {:?}",
        mgr.locks().held(txn.id)
    );
}

/// A write takes its `X` and its `IX` at the four lock-based levels, `READ UNCOMMITTED`
/// included.
#[test]
fn write_lock_takes_x_at_every_level() {
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
    ] {
        let mgr = manager();
        let txn = mgr.begin(level);
        mgr.write_lock(&txn, T, R, &plain()).expect("the write");
        assert!(holds(&mgr, &txn, row_res(), LockMode::X), "X at {level:?}");
        assert!(
            holds(&mgr, &txn, table_res(), LockMode::IX),
            "IX at {level:?}"
        );
    }
}

/// `TABLOCK` and `TABLOCKX` are carried by [`LockIntent`] and not applied: the locks taken
/// with them are the locks taken without them.
#[test]
fn tablock_fields_are_carried_not_applied() {
    let with_hints = LockIntent {
        tablock: true,
        tablockx: true,
        ..LockIntent::default()
    };
    let mut held = Vec::new();
    for hints in [with_hints, plain()] {
        let mgr = manager();
        let txn = mgr.begin(IsolationLevel::RepeatableRead);
        mgr.read_lock(&txn, T, R, &hints).expect("the read");
        held.push(mgr.locks().held(txn.id));
    }
    assert_eq!(held[0], held[1], "TABLOCK changed the locks taken");
}

/// `SERIALIZABLE` holds the rows it read and nothing else: a row it did not read is taken
/// by another transaction without waiting. That is a deliberate difference from SQL Server,
/// where a range lock makes that writer wait.
#[test]
fn serializable_locks_the_rows_it_read_not_the_range() {
    let mgr = manager();
    let reader = mgr.begin(IsolationLevel::Serializable);
    mgr.read_lock(&reader, T, R, &plain()).expect("the read");
    mgr.end_row_read(&reader, T, R).expect("the end of the row");
    assert!(holds(&mgr, &reader, row_res(), LockMode::S));

    let writer = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.write_lock(&writer, T, RowId(2), &nowait())
        .expect("a row the reader did not read is free");
    mgr.write_lock(&writer, T, R, &nowait())
        .expect_err("the row the reader read is held");
}
