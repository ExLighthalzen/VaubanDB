//! The schema lock: `Sch-S` for the statements that read the shape of a table, `Sch-M` for
//! those that change it.
//!
//! The scenarios run threads over an `Arc<TransactionManager>`, not SQL: this file knows no
//! T-SQL text. What SQL Server does on each shape is the table in the module documentation
//! of `crates/vauban-txn/src/schema_lock.rs`.
//!
//! # Axes
//!
//! Crossed here: the two schema modes × (the same transaction / another one) × (a schema
//! lock against a schema lock / against a row write) × (`ReadUncommitted` with a `NOLOCK`
//! hint / `ReadCommitted`) × (commit / rollback) × (an infinite wait / a bounded one). The
//! counter-test of `drop_waits_for_readers` is `sch_s_readers_do_not_block_each_other`,
//! where three `Sch-S` sit on one table at once, and the one of `nolock_still_takes_sch_s`
//! is the row of that table where a `NOLOCK` read goes past an open `UPDATE` —
//! `sch_s_does_not_block_a_writer` is that row in this file, taken from the other end.
//!
//! Not crossed, and why: the statements that call these two methods, which the executor
//! and the catalogue place; error 3726 of a `DROP TABLE` of a referenced table, which the
//! catalogue reports; the schema lock of a database, a view or a procedure, which this
//! crate does not take; the deadlock a schema lock takes part in, which `tests/deadlock.rs`
//! covers on the mechanism the two methods call.
//!
//! A thread that must be waiting before a scenario goes on is attested through
//! [`LockManager::waiters`], as `tests/lock.rs` and `tests/isolation.rs` do, so that no
//! assertion rests on a thread having had the time to start.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use vauban_storage::testsuite::int_table_shape;
use vauban_storage::{DbId, MemoryStorage, RowId, Storage, TableId, TxnId};
use vauban_txn::{
    CommitAction, IsolationLevel, LockIntent, LockManager, LockMode, LockResource, LockTimeout,
    LockWait, ReadAccess, TransactionManager, TxnHandle,
};

/// How long a test waits for something it expects to happen — the two seconds a blocked
/// `Sch-M` is granted within, once the reader has committed.
const SOON: Duration = Duration::from_secs(2);

/// How long a test waits before concluding that nothing is coming.
const QUIET: Duration = Duration::from_millis(100);

/// The table every scenario locks.
const T: TableId = TableId(1);

/// The row the writer of `sch_s_does_not_block_a_writer` takes.
const R: RowId = RowId(1);

/// A manager over an empty `MemoryStorage`. No row is written: the two methods under test
/// lock identifiers and do not read `storage`.
fn manager() -> Arc<TransactionManager> {
    Arc::new(TransactionManager::new(Arc::new(MemoryStorage::new())))
}

/// The table resource of `T`.
fn table_res() -> LockResource {
    LockResource::Table(T)
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

/// The modes `txn` holds on the table of the scenario.
fn table_modes(mgr: &TransactionManager, txn: &TxnHandle) -> Vec<LockMode> {
    mgr.locks()
        .held(txn.id)
        .into_iter()
        .filter(|(res, _)| *res == table_res())
        .map(|(_, mode)| mode)
        .collect()
}

/// Takes the `Sch-M` of `handle` on `T` in the background, reporting what it got.
fn modify_in_background(
    mgr: &Arc<TransactionManager>,
    handle: &TxnHandle,
) -> Receiver<Result<(), u32>> {
    let (mgr, handle) = (Arc::clone(mgr), handle.clone());
    in_background(move || mgr.schema_modify_lock(&handle, T).map_err(|e| e.number))
}

/// Takes the `Sch-S` of `handle` on `T` in the background, reporting what it got.
fn read_in_background(
    mgr: &Arc<TransactionManager>,
    handle: &TxnHandle,
) -> Receiver<Result<(), u32>> {
    let (mgr, handle) = (Arc::clone(mgr), handle.clone());
    in_background(move || mgr.schema_stability_lock(&handle, T).map_err(|e| e.number))
}

// --------------------------------------------- A DDL waits for the readers, and back

/// A `DROP TABLE` waits for the transaction that is reading the table, and gets its `Sch-M`
/// once that transaction has committed.
#[test]
fn drop_waits_for_readers() {
    let mgr = manager();
    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_stability_lock(&reader, T)
        .expect("the table is free");

    let ddl = mgr.begin(IsolationLevel::ReadCommitted);
    let got = modify_in_background(&mgr, &ddl);
    await_waiter(mgr.locks(), ddl.id);
    still_waiting(&got, "the DROP behind an open reader");
    assert_eq!(
        table_modes(&mgr, &reader),
        vec![LockMode::SchS],
        "the reader still holds its Sch-S"
    );

    mgr.commit(reader).expect("the reader commits");
    assert_eq!(
        got.recv_timeout(SOON),
        Ok(Ok(())),
        "the DROP passes within {SOON:?} of the commit"
    );
    assert_eq!(table_modes(&mgr, &ddl), vec![LockMode::SchM]);
}

/// The other direction: a reader that arrives while a DDL holds the table waits for it.
#[test]
fn reader_waits_for_ddl() {
    let mgr = manager();
    let ddl = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_modify_lock(&ddl, T).expect("the table is free");

    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    let got = read_in_background(&mgr, &reader);
    await_waiter(mgr.locks(), reader.id);
    still_waiting(&got, "the reader behind an open DDL");

    mgr.commit(ddl).expect("the DDL commits");
    assert_eq!(
        got.recv_timeout(SOON),
        Ok(Ok(())),
        "the reader passes at the commit of the DDL"
    );
    assert_eq!(table_modes(&mgr, &reader), vec![LockMode::SchS]);
}

/// Counter-test of the two above: three readers hold their `Sch-S` on one table at the
/// same time, so what the scenarios above check is the `Sch-M`, not the resource.
#[test]
fn sch_s_readers_do_not_block_each_other() {
    let mgr = manager();
    let readers: Vec<TxnHandle> = (0..3)
        .map(|_| mgr.begin(IsolationLevel::ReadCommitted))
        .collect();
    let taken: Vec<Receiver<Result<(), u32>>> = readers
        .iter()
        .map(|r| read_in_background(&mgr, r))
        .collect();

    for (i, rx) in taken.iter().enumerate() {
        assert_eq!(
            rx.recv_timeout(SOON),
            Ok(Ok(())),
            "reader {i} takes its Sch-S"
        );
    }
    for reader in &readers {
        assert_eq!(
            table_modes(&mgr, reader),
            vec![LockMode::SchS],
            "the three Sch-S are held together: {:?}",
            mgr.locks().held(reader.id)
        );
    }
}

// ------------------------------------------------- The level does not decide this lock

/// A read that takes no data lock takes its `Sch-S` all the same, and makes a `Sch-M` wait:
/// the two shapes SQL Server shows, the `NOLOCK` hint over a `READ COMMITTED` transaction
/// and a transaction opened in `READ UNCOMMITTED`.
#[test]
fn nolock_still_takes_sch_s() {
    let nolock = LockIntent {
        level: Some(IsolationLevel::ReadUncommitted),
        ..LockIntent::default()
    };
    let shapes = [
        (IsolationLevel::ReadCommitted, nolock),
        (IsolationLevel::ReadUncommitted, LockIntent::default()),
    ];
    for (level, hints) in shapes {
        let mgr = manager();
        let reader = mgr.begin(level);
        assert_eq!(
            mgr.read_lock(&reader, T, R, &hints, &LockWait::none())
                .expect("the row read"),
            ReadAccess::Dirty,
            "the read takes no data lock at {level:?}"
        );
        assert_eq!(
            mgr.locks().held(reader.id),
            vec![],
            "no data lock is held at {level:?}"
        );

        mgr.schema_stability_lock(&reader, T)
            .expect("the table is free");
        assert_eq!(
            table_modes(&mgr, &reader),
            vec![LockMode::SchS],
            "the dirty read holds a Sch-S at {level:?}"
        );

        let ddl = mgr.begin(IsolationLevel::ReadCommitted);
        let got = modify_in_background(&mgr, &ddl);
        await_waiter(mgr.locks(), ddl.id);
        still_waiting(&got, "the DDL behind a dirty reader");

        mgr.commit(reader).expect("the reader commits");
        assert_eq!(
            got.recv_timeout(SOON),
            Ok(Ok(())),
            "the DDL passes at the commit of the dirty reader at {level:?}"
        );
    }
}

/// The `Sch-S` / `X` line of the matrix: a writer takes its row while another transaction
/// holds the `Sch-S` of the table.
#[test]
fn sch_s_does_not_block_a_writer() {
    let mgr = manager();
    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_stability_lock(&reader, T)
        .expect("the table is free");

    let writer = mgr.begin(IsolationLevel::ReadCommitted);
    let (bg, handle) = (Arc::clone(&mgr), writer.clone());
    let written = in_background(move || {
        bg.write_lock(&handle, T, R, &LockIntent::default(), &LockWait::none())
            .map_err(|e| e.number)
    });
    assert_eq!(
        written.recv_timeout(SOON),
        Ok(Ok(())),
        "the writer takes its row under a Sch-S"
    );
    assert_eq!(
        mgr.locks().held(writer.id),
        vec![
            (LockResource::Row(T, R), LockMode::X),
            (table_res(), LockMode::IX),
        ],
        "the writer holds the IX of the table and the X of the row"
    );
    assert_eq!(
        table_modes(&mgr, &reader),
        vec![LockMode::SchS],
        "the reader kept its Sch-S"
    );
}

// ------------------------------------------------------- Conversion and release

/// The `ALTER TABLE` of a table this transaction has just read: `Sch-S` then `Sch-M`, one
/// lock converted, no wait on itself.
#[test]
fn sch_s_to_sch_m_converts() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_stability_lock(&txn, T)
        .expect("the table is free");
    mgr.schema_modify_lock(&txn, T)
        .expect("its own Sch-S is not in its way");
    assert_eq!(
        mgr.locks().held(txn.id),
        vec![(table_res(), LockMode::SchM)],
        "one lock, converted, not two"
    );
    assert_eq!(mgr.locks().waiters(), vec![], "nothing was queued");
}

/// After the commit, the transaction holds no schema mode, and the table is free for the
/// next one.
#[test]
fn commit_releases_the_schema_lock() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_stability_lock(&txn, T)
        .expect("the table is free");
    mgr.schema_modify_lock(&txn, TableId(2))
        .expect("the other table is free");

    mgr.commit(txn.clone()).expect("commit");
    assert_eq!(
        mgr.locks().held(txn.id),
        vec![],
        "no schema mode is left after the commit"
    );

    let next = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_modify_lock(&next, T)
        .expect("the table is free again");
}

/// The same after a rollback, the other end of the transaction.
#[test]
fn rollback_releases_the_schema_lock() {
    let mgr = manager();
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_modify_lock(&txn, T).expect("the table is free");

    mgr.rollback(txn.clone()).expect("rollback");
    assert_eq!(
        mgr.locks().held(txn.id),
        vec![],
        "no schema mode is left after the rollback"
    );

    let next = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_stability_lock(&next, T)
        .expect("the table is free again");
}

// ------------------------------------------------------- The timeout and the order

/// A schema wait that runs out of time reports 1222 with the state of an object, the number
/// and the state SQL Server sends to a `SELECT … WITH (NOLOCK)` under `SET LOCK_TIMEOUT 300`
/// (module documentation of `src/schema_lock.rs`, row 5).
///
/// The bounded wait is asked of the lock manager directly: `schema_stability_lock` passes
/// `LockTimeout::Infinite` until `SET LOCK_TIMEOUT` is put on the handle.
#[test]
fn a_schema_wait_that_times_out_reports_1222_on_the_object() {
    let mgr = manager();
    let ddl = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_modify_lock(&ddl, T).expect("the table is free");

    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    let err = mgr
        .locks()
        .lock(
            reader.id,
            table_res(),
            LockMode::SchS,
            LockTimeout::Millis(100),
            &LockWait::none(),
        )
        .expect_err("the Sch-M is in the way");
    assert_eq!(
        (err.number, err.severity, err.state),
        (1222, 16, 56),
        "message was {:?}",
        err.message
    );
}

/// The order the deferred DDL needs: the `storage.drop_table` of the commit
/// runs before the `Sch-M` is given back, so the reader that wakes on the table finds it
/// gone instead of reading a table that is about to go.
#[test]
fn the_deferred_drop_runs_before_the_waiter_wakes() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db: DbId = storage
        .create_database("schema_lock")
        .expect("create_database");
    let table = storage
        .create_table(db, &int_table_shape(1))
        .expect("create_table");
    let mgr = Arc::new(TransactionManager::new(Arc::clone(&storage)));

    let ddl = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_modify_lock(&ddl, table)
        .expect("the table is free");
    mgr.register_on_commit(&ddl, CommitAction::DropTable(table))
        .expect("defer the drop to the commit");

    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    let (bg, handle, seen) = (Arc::clone(&mgr), reader.clone(), Arc::clone(&storage));
    let woke = in_background(move || {
        bg.schema_stability_lock(&handle, table)
            .expect("the reader gets its Sch-S at the commit");
        seen.tables(db).expect("tables").len()
    });
    await_waiter(mgr.locks(), reader.id);
    still_waiting(&woke, "the reader behind the deferred drop");
    assert_eq!(
        storage.tables(db).expect("tables").len(),
        1,
        "registering the drop does not apply it"
    );

    mgr.commit(ddl).expect("the DDL commits");
    assert_eq!(
        woke.recv_timeout(SOON),
        Ok(0),
        "the table was already dropped when the reader woke"
    );
}
