//! The two database options over a `MemoryStorage`: what a read takes, what a snapshot
//! shows and what a write is told, with `READ_COMMITTED_SNAPSHOT` and
//! `ALLOW_SNAPSHOT_ISOLATION` off and on.
//!
//! The scenarios run threads over an `Arc<TransactionManager>`, not SQL: this file knows no
//! T-SQL text. What SQL Server does on each shape is the table in the module documentation
//! of `crates/vauban-txn/src/snapshot_modes.rs`.
//!
//! # Axes
//!
//! Crossed here: the two options (off / `READ_COMMITTED_SNAPSHOT` / `ALLOW_SNAPSHOT_ISOLATION`)
//! × the level of the transaction (`ReadCommitted`, `RepeatableRead`, `Snapshot`) × the
//! hint (none, `READCOMMITTED`, `UPDLOCK`) × the writer (none, committed before the
//! snapshot, committed after it, rolled back, still open) × the row (the one read, another
//! one, a deleted one). Not crossed, and why: `ReadUncommitted` and `Serializable`, whose
//! mode is `Locking` under both options (unit test `the_mode_table_is_the_one_documented`
//! in `src/snapshot_modes.rs`) and whose locks `tests/isolation.rs` covers; the schema
//! locks (`tests/schema_lock.rs`); the numbers 3952 and 3960 themselves, built by the
//! session and the executor, not here.
//!
//! A thread that must be waiting before a scenario goes on is attested through
//! [`LockManager::waiters`], as `tests/isolation.rs` does.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use vauban_storage::testsuite::{int_table_shape, row};
use vauban_storage::{DbId, MemoryStorage, Row, RowId, Snapshot, Storage, TableId, TxnId};
use vauban_txn::{
    IsolationLevel, LockIntent, LockManager, LockMode, LockResource, LockWait, ReadAccess,
    TransactionManager, TxnHandle, VersioningMode, VersioningOptions, WriteDecision,
};

/// How long a test waits for something it expects to happen.
const SOON: Duration = Duration::from_secs(2);

/// How long a test waits before concluding that nothing is coming.
const QUIET: Duration = Duration::from_millis(100);

/// Both options off.
const OFF: VersioningOptions = VersioningOptions {
    read_committed_snapshot: false,
    allow_snapshot_isolation: false,
};

/// `READ_COMMITTED_SNAPSHOT ON`.
const RCSI: VersioningOptions = VersioningOptions {
    read_committed_snapshot: true,
    allow_snapshot_isolation: false,
};

/// `ALLOW_SNAPSHOT_ISOLATION ON`.
const ASI: VersioningOptions = VersioningOptions {
    read_committed_snapshot: false,
    allow_snapshot_isolation: true,
};

/// One database with one table of one `int` column, holding one committed row, and a
/// manager over it.
struct Fixture {
    storage: Arc<dyn Storage>,
    mgr: Arc<TransactionManager>,
    db: DbId,
    table: TableId,
    /// The committed row, value `10`.
    id: RowId,
}

/// A fixture whose database runs under `opts`. The row is written by the first
/// transaction of the manager, `TxnId(1)`, and committed.
fn fixture(opts: VersioningOptions) -> Fixture {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage
        .create_database("versioned")
        .expect("create_database");
    let table = storage
        .create_table(db, &int_table_shape(1))
        .expect("create_table");
    let mgr = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    mgr.set_versioning_options(db, opts);
    let seed = mgr
        .begin_in(db, IsolationLevel::ReadCommitted)
        .expect("seed");
    let id = storage.insert(seed.id, table, &row(&[10])).expect("insert");
    mgr.commit(seed).expect("commit the seed");
    Fixture {
        storage,
        mgr,
        db,
        table,
        id,
    }
}

impl Fixture {
    /// A transaction on the database of the fixture, at `level`.
    fn begin(&self, level: IsolationLevel) -> TxnHandle {
        self.mgr
            .begin_in(self.db, level)
            .unwrap_or_else(|e| panic!("begin_in at {level:?}: {}", e.message))
    }

    /// Row `id` as `snap` shows it.
    fn get(&self, snap: &Snapshot, id: RowId) -> Option<Row> {
        self.storage.get(snap, self.table, id).expect("get")
    }

    /// The row of the fixture through a snapshot `txn` asks for now.
    fn read(&self, txn: &TxnHandle) -> Option<Row> {
        let snap = self.mgr.statement_snapshot(txn);
        self.get(&snap, self.id)
    }

    /// Sets row `id` to `value` on behalf of `txn`, `X` taken first.
    fn update(&self, txn: &TxnHandle, id: RowId, value: i32) {
        self.mgr
            .write_lock(txn, self.table, id, &plain(), &LockWait::none())
            .expect("write_lock");
        self.storage
            .update(txn.id, self.table, id, &row(&[value]))
            .expect("update");
    }

    /// Sets the row of the fixture to `value` in a transaction of its own, committed.
    fn commit_update(&self, value: i32) {
        let writer = self.begin(IsolationLevel::ReadCommitted);
        self.update(&writer, self.id, value);
        self.mgr.commit(writer).expect("commit the writer");
    }

    /// What `txn` is told about writing row `id`.
    fn check(&self, txn: &TxnHandle, id: RowId) -> WriteDecision {
        self.mgr
            .check_write_conflict(txn, self.table, id)
            .expect("check_write_conflict")
    }
}

/// No hint at all.
fn plain() -> LockIntent {
    LockIntent::default()
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

/// `true` when `txn` holds `mode` on the row of the fixture.
fn holds_row(f: &Fixture, txn: &TxnHandle, mode: LockMode) -> bool {
    f.mgr
        .locks()
        .held(txn.id)
        .contains(&(LockResource::Row(f.table, f.id), mode))
}

// ------------------------------------------------------------------ The options

#[test]
fn options_default_to_off() {
    let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
    assert_eq!(mgr.versioning_options(DbId(1)), OFF);
    assert_eq!(
        mgr.versioning_options(DbId(1)),
        VersioningOptions::default()
    );
    mgr.set_versioning_options(DbId(1), RCSI);
    assert_eq!(mgr.versioning_options(DbId(1)), RCSI);
    assert_eq!(mgr.versioning_options(DbId(2)), OFF, "per database");
}

#[test]
fn begin_opens_on_the_first_database() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let first = storage.create_database("first").expect("create_database");
    assert_eq!(first, DbId(1));
    let mgr = TransactionManager::new(storage);
    let before = mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!(mgr.versioning_mode(&before), VersioningMode::Locking);
    mgr.set_versioning_options(DbId(2), RCSI);
    let elsewhere = mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!(
        mgr.versioning_mode(&elsewhere),
        VersioningMode::Locking,
        "the options of another database do not reach begin"
    );
    mgr.set_versioning_options(DbId(1), RCSI);
    let after = mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!(
        mgr.versioning_mode(&after),
        VersioningMode::StatementSnapshot
    );
    assert_eq!(
        mgr.versioning_mode(&before),
        VersioningMode::StatementSnapshot,
        "the options are read at each decision"
    );
}

#[test]
fn snapshot_requires_the_option() {
    let f = fixture(OFF);
    let refused = f.mgr.begin_in(f.db, IsolationLevel::Snapshot);
    let err = refused.expect_err("SNAPSHOT without ALLOW_SNAPSHOT_ISOLATION");
    assert_eq!(
        err.number, 50000,
        "an internal error, 3952 is the session's"
    );
    assert!(
        err.message.contains("ALLOW_SNAPSHOT_ISOLATION"),
        "{}",
        err.message
    );
    assert_eq!(f.mgr.active_sessions().len(), 0, "nothing was opened");
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
    ] {
        let txn = f.begin(level);
        assert_eq!(f.mgr.versioning_mode(&txn), VersioningMode::Locking);
    }
    // The plain `begin` keeps the level and serves it at `Locking` instead of refusing.
    let kept = f.mgr.begin(IsolationLevel::Snapshot);
    assert_eq!(kept.isolation, IsolationLevel::Snapshot);
    assert_eq!(f.mgr.versioning_mode(&kept), VersioningMode::Locking);

    f.mgr.set_versioning_options(f.db, ASI);
    let txn = f.begin(IsolationLevel::Snapshot);
    assert_eq!(f.mgr.versioning_mode(&txn), VersioningMode::TxnSnapshot);
    let rc = f.begin(IsolationLevel::ReadCommitted);
    assert_eq!(
        f.mgr.versioning_mode(&rc),
        VersioningMode::Locking,
        "ALLOW_SNAPSHOT_ISOLATION alone leaves READ COMMITTED to the locks"
    );
}

// ------------------------------------------------------- READ_COMMITTED_SNAPSHOT

/// With the options off, a `READ COMMITTED` reader waits for the `X` of the writer.
#[test]
fn rcsi_off_blocks_the_reader() {
    let f = fixture(OFF);
    let a = f.begin(IsolationLevel::ReadCommitted);
    f.update(&a, f.id, 11);

    let b = f.begin(IsolationLevel::ReadCommitted);
    let (mgr, handle, table, id) = (Arc::clone(&f.mgr), b.clone(), f.table, f.id);
    let read =
        in_background(move || mgr.read_lock(&handle, table, id, &plain(), &LockWait::none()));
    await_waiter(f.mgr.locks(), b.id);
    still_waiting(&read, "the READ COMMITTED reader");

    f.mgr.commit(a).expect("A commits");
    assert_eq!(
        read.recv_timeout(SOON).expect("the reader is woken"),
        Ok(ReadAccess::Locked)
    );
    assert!(holds_row(&f, &b, LockMode::S));
    assert_eq!(f.read(&b), Some(row(&[11])));
}

/// The counter-test of `rcsi_off_blocks_the_reader`: the same scenario with
/// `READ_COMMITTED_SNAPSHOT` on, where the reader takes no lock, waits for nothing and
/// reads the committed value through its snapshot.
#[test]
fn rcsi_on_does_not() {
    let f = fixture(RCSI);
    let a = f.begin(IsolationLevel::ReadCommitted);
    f.update(&a, f.id, 11);

    let b = f.begin(IsolationLevel::ReadCommitted);
    assert_eq!(f.mgr.versioning_mode(&b), VersioningMode::StatementSnapshot);
    let (mgr, handle, table, id) = (Arc::clone(&f.mgr), b.clone(), f.table, f.id);
    let read =
        in_background(move || mgr.read_lock(&handle, table, id, &plain(), &LockWait::none()));
    assert_eq!(
        read.recv_timeout(SOON).expect("the reader answers at once"),
        Ok(ReadAccess::Versioned)
    );
    assert!(
        f.mgr.locks().held(b.id).is_empty(),
        "no lock at all: {:?}",
        f.mgr.locks().held(b.id)
    );
    assert!(f.mgr.locks().waiters().is_empty());
    assert_eq!(
        f.read(&b),
        Some(row(&[10])),
        "the committed value, not the held one"
    );
    f.mgr
        .end_row_read(&b, f.table, f.id)
        .expect("nothing to give back");

    f.mgr.commit(a).expect("A commits");
    assert_eq!(
        f.read(&b),
        Some(row(&[11])),
        "the next statement sees the commit"
    );
}

#[test]
fn statement_snapshot_is_frozen_for_the_statement() {
    let f = fixture(RCSI);
    let b = f.begin(IsolationLevel::ReadCommitted);
    let statement = f.mgr.statement_snapshot(&b);
    assert_eq!(f.get(&statement, f.id), Some(row(&[10])));
    f.commit_update(11);
    assert_eq!(
        f.get(&statement, f.id),
        Some(row(&[10])),
        "the same snapshot hides the commit"
    );
    let next = f.mgr.statement_snapshot(&b);
    assert_ne!(next, statement);
    assert_eq!(
        f.get(&next, f.id),
        Some(row(&[11])),
        "the next one shows it"
    );
}

/// A `REPEATABLE READ` transaction whose statement carries the `READCOMMITTED` hint is
/// served from the row versions where `READ_COMMITTED_SNAPSHOT` is on; its plain read is
/// not.
#[test]
fn a_read_committed_hint_is_versioned_under_rcsi() {
    let f = fixture(RCSI);
    let a = f.begin(IsolationLevel::ReadCommitted);
    f.update(&a, f.id, 11);

    let b = f.begin(IsolationLevel::RepeatableRead);
    assert_eq!(f.mgr.versioning_mode(&b), VersioningMode::Locking);
    let hint = LockIntent {
        level: Some(IsolationLevel::ReadCommitted),
        ..LockIntent::default()
    };
    assert_eq!(
        f.mgr
            .read_lock(&b, f.table, f.id, &hint, &LockWait::none())
            .expect("B reads"),
        ReadAccess::Versioned
    );
    assert!(f.mgr.locks().held(b.id).is_empty());

    let (mgr, handle, table, id) = (Arc::clone(&f.mgr), b.clone(), f.table, f.id);
    let read =
        in_background(move || mgr.read_lock(&handle, table, id, &plain(), &LockWait::none()));
    await_waiter(f.mgr.locks(), b.id);
    still_waiting(&read, "the plain REPEATABLE READ read");
    f.mgr.commit(a).expect("A commits");
    assert_eq!(
        read.recv_timeout(SOON).expect("the reader is woken"),
        Ok(ReadAccess::Locked)
    );
}

/// `UPDLOCK` takes its `U` where `READ_COMMITTED_SNAPSHOT` is on, and waits for the `X`.
#[test]
fn updlock_still_waits_under_rcsi() {
    let f = fixture(RCSI);
    let a = f.begin(IsolationLevel::ReadCommitted);
    f.update(&a, f.id, 11);

    let b = f.begin(IsolationLevel::ReadCommitted);
    let updlock = LockIntent {
        updlock: true,
        ..LockIntent::default()
    };
    let (mgr, handle, table, id) = (Arc::clone(&f.mgr), b.clone(), f.table, f.id);
    let read =
        in_background(move || mgr.read_lock(&handle, table, id, &updlock, &LockWait::none()));
    await_waiter(f.mgr.locks(), b.id);
    still_waiting(&read, "the UPDLOCK read");
    f.mgr.commit(a).expect("A commits");
    assert_eq!(
        read.recv_timeout(SOON).expect("the reader is woken"),
        Ok(ReadAccess::Locked)
    );
    assert!(holds_row(&f, &b, LockMode::U));
}

#[test]
fn write_locks_are_still_taken_under_rcsi() {
    let f = fixture(RCSI);
    let a = f.begin(IsolationLevel::ReadCommitted);
    f.update(&a, f.id, 11);
    assert!(holds_row(&f, &a, LockMode::X));

    let b = f.begin(IsolationLevel::ReadCommitted);
    let (mgr, handle, table, id) = (Arc::clone(&f.mgr), b.clone(), f.table, f.id);
    let written =
        in_background(move || mgr.write_lock(&handle, table, id, &plain(), &LockWait::none()));
    await_waiter(f.mgr.locks(), b.id);
    still_waiting(&written, "the second writer");

    f.mgr.commit(a).expect("A commits");
    written
        .recv_timeout(SOON)
        .expect("the writer is woken")
        .expect("the writer takes the row");
    assert!(holds_row(&f, &b, LockMode::X));
    assert_eq!(
        f.check(&b, f.id),
        WriteDecision::Proceed,
        "no snapshot handed out to B yet"
    );
}

/// Under `Locking` and `StatementSnapshot`, a row another transaction committed after the
/// snapshot of the statement is to be read again, not refused.
#[test]
fn a_row_changed_since_the_statement_is_reread() {
    for opts in [OFF, RCSI] {
        let f = fixture(opts);
        let b = f.begin(IsolationLevel::ReadCommitted);
        assert_eq!(f.read(&b), Some(row(&[10])));
        f.commit_update(11);
        f.mgr
            .write_lock(&b, f.table, f.id, &plain(), &LockWait::none())
            .expect("B takes the row");
        assert_eq!(
            f.check(&b, f.id),
            WriteDecision::Reread(f.id),
            "{opts:?}: the statement read a version that is no longer the latest"
        );
        assert_eq!(f.read(&b), Some(row(&[11])), "{opts:?}: the re-read");
        assert_eq!(
            f.check(&b, f.id),
            WriteDecision::Proceed,
            "{opts:?}: the fresh snapshot showed the latest version"
        );
    }
}

// ------------------------------------------------------ ALLOW_SNAPSHOT_ISOLATION

#[test]
fn snapshot_txn_is_frozen_until_commit() {
    let f = fixture(ASI);
    let b = f.begin(IsolationLevel::Snapshot);
    let other = f.begin(IsolationLevel::ReadCommitted);
    let first = f.mgr.statement_snapshot(&b);
    assert_eq!(first.active, vec![other.id]);
    f.update(&other, f.id, 11);
    f.mgr.commit(other).expect("the other commits");
    let later = f.begin(IsolationLevel::ReadCommitted);
    let second = f.mgr.statement_snapshot(&b);
    let third = f.mgr.statement_snapshot(&b);
    assert_eq!(second, first, "same xmax, same active list");
    assert_eq!(third, first);
    assert_eq!(second.xmax, first.xmax);
    assert_eq!(
        second.active, first.active,
        "{later:?} is not listed either"
    );
    assert_eq!(
        f.get(&third, f.id),
        Some(row(&[10])),
        "the commit of the other transaction is hidden"
    );
    f.mgr.commit(b.clone()).expect("B commits");
    assert_eq!(
        f.read(&b),
        Some(row(&[11])),
        "a closed handle reads through a fresh snapshot"
    );
}

#[test]
fn the_snapshot_is_pinned_by_the_first_statement_not_by_begin() {
    let f = fixture(ASI);
    let b = f.begin(IsolationLevel::Snapshot);
    f.commit_update(11);
    assert_eq!(
        f.read(&b),
        Some(row(&[11])),
        "a commit between begin and the first statement is shown"
    );
    f.commit_update(12);
    assert_eq!(
        f.read(&b),
        Some(row(&[11])),
        "the first statement pinned it"
    );
}

#[test]
fn a_flip_does_not_unpin_the_snapshot() {
    let f = fixture(ASI);
    let b = f.begin(IsolationLevel::Snapshot);
    let pinned = f.mgr.statement_snapshot(&b);
    f.mgr.set_versioning_options(f.db, OFF);
    assert_eq!(f.mgr.versioning_mode(&b), VersioningMode::Locking);
    f.commit_update(11);
    assert_eq!(f.mgr.statement_snapshot(&b), pinned);
    assert_eq!(f.read(&b), Some(row(&[10])));
}

/// `SNAPSHOT` reads take no lock and wait for no writer.
#[test]
fn snapshot_txn_reads_take_no_lock() {
    let f = fixture(ASI);
    let a = f.begin(IsolationLevel::ReadCommitted);
    f.update(&a, f.id, 11);
    let b = f.begin(IsolationLevel::Snapshot);
    let (mgr, handle, table, id) = (Arc::clone(&f.mgr), b.clone(), f.table, f.id);
    let read =
        in_background(move || mgr.read_lock(&handle, table, id, &plain(), &LockWait::none()));
    assert_eq!(
        read.recv_timeout(SOON).expect("the reader answers at once"),
        Ok(ReadAccess::Versioned)
    );
    assert!(f.mgr.locks().held(b.id).is_empty());
    assert_eq!(f.read(&b), Some(row(&[10])));
}

#[test]
fn update_conflict_is_reported() {
    let f = fixture(ASI);
    let a = f.begin(IsolationLevel::Snapshot);
    let b = f.begin(IsolationLevel::Snapshot);
    assert_eq!(f.read(&a), Some(row(&[10])));
    assert_eq!(f.read(&b), Some(row(&[10])));
    f.update(&a, f.id, 11);
    assert_eq!(f.check(&a, f.id), WriteDecision::Proceed, "its own write");
    f.mgr.commit(a).expect("A commits");
    f.mgr
        .write_lock(&b, f.table, f.id, &plain(), &LockWait::none())
        .expect("B takes the row");
    assert_eq!(f.check(&b, f.id), WriteDecision::Conflict);
    assert_eq!(
        f.read(&b),
        Some(row(&[10])),
        "the pinned snapshot still hides the commit of A"
    );
}

/// The counter-test of `update_conflict_is_reported`: a second row, untouched by A.
#[test]
fn no_conflict_when_rows_differ() {
    let f = fixture(ASI);
    let seed = f.begin(IsolationLevel::ReadCommitted);
    let other = f
        .storage
        .insert(seed.id, f.table, &row(&[20]))
        .expect("insert");
    f.mgr.commit(seed).expect("commit the seed");
    let a = f.begin(IsolationLevel::Snapshot);
    let b = f.begin(IsolationLevel::Snapshot);
    assert_eq!(f.read(&a), Some(row(&[10])));
    assert_eq!(
        f.get(&f.mgr.statement_snapshot(&b), other),
        Some(row(&[20]))
    );
    f.update(&a, f.id, 11);
    f.mgr.commit(a).expect("A commits");
    f.mgr
        .write_lock(&b, f.table, other, &plain(), &LockWait::none())
        .expect("B takes the other row");
    assert_eq!(f.check(&b, other), WriteDecision::Proceed);
    assert_eq!(
        f.check(&b, f.id),
        WriteDecision::Conflict,
        "the row A changed"
    );
}

#[test]
fn a_delete_after_the_snapshot_is_a_conflict() {
    let f = fixture(ASI);
    let b = f.begin(IsolationLevel::Snapshot);
    assert_eq!(f.read(&b), Some(row(&[10])));
    let a = f.begin(IsolationLevel::ReadCommitted);
    f.mgr
        .write_lock(&a, f.table, f.id, &plain(), &LockWait::none())
        .expect("A takes the row");
    f.storage.delete(a.id, f.table, f.id).expect("delete");
    f.mgr.commit(a).expect("A commits");
    assert_eq!(f.check(&b, f.id), WriteDecision::Conflict);
}

#[test]
fn a_rolled_back_update_is_no_conflict() {
    let f = fixture(ASI);
    let b = f.begin(IsolationLevel::Snapshot);
    assert_eq!(f.read(&b), Some(row(&[10])));
    let a = f.begin(IsolationLevel::ReadCommitted);
    f.update(&a, f.id, 11);
    f.mgr.rollback(a).expect("A rolls back");
    f.mgr
        .write_lock(&b, f.table, f.id, &plain(), &LockWait::none())
        .expect("B takes the row");
    assert_eq!(f.check(&b, f.id), WriteDecision::Proceed);
}

/// A row committed before the snapshot is no conflict: the snapshot showed that version.
#[test]
fn a_write_committed_before_the_snapshot_is_no_conflict() {
    let f = fixture(ASI);
    f.commit_update(11);
    let b = f.begin(IsolationLevel::Snapshot);
    assert_eq!(f.read(&b), Some(row(&[11])));
    f.mgr
        .write_lock(&b, f.table, f.id, &plain(), &LockWait::none())
        .expect("B takes the row");
    assert_eq!(f.check(&b, f.id), WriteDecision::Proceed);
}

/// A `SNAPSHOT` writer waits for the `X` of another one, and is told the conflict once
/// that one has committed.
#[test]
fn a_snapshot_writer_waits_then_conflicts() {
    let f = fixture(ASI);
    let a = f.begin(IsolationLevel::Snapshot);
    assert_eq!(f.read(&a), Some(row(&[10])));
    f.update(&a, f.id, 11);

    let b = f.begin(IsolationLevel::Snapshot);
    assert_eq!(f.read(&b), Some(row(&[10])));
    let (mgr, handle, table, id) = (Arc::clone(&f.mgr), b.clone(), f.table, f.id);
    let written = in_background(move || {
        mgr.write_lock(&handle, table, id, &plain(), &LockWait::none())?;
        mgr.check_write_conflict(&handle, table, id)
    });
    await_waiter(f.mgr.locks(), b.id);
    still_waiting(&written, "the SNAPSHOT writer");

    f.mgr.commit(a).expect("A commits");
    assert_eq!(
        written.recv_timeout(SOON).expect("the writer is woken"),
        Ok(WriteDecision::Conflict)
    );
}

#[test]
fn check_on_a_closed_handle_is_a_bug() {
    let f = fixture(ASI);
    let b = f.begin(IsolationLevel::Snapshot);
    f.mgr.commit(b.clone()).expect("B commits");
    let err = f
        .mgr
        .check_write_conflict(&b, f.table, f.id)
        .expect_err("a closed handle");
    assert_eq!(err.number, 50000);
    assert!(err.message.contains("not open"), "{}", err.message);
}
