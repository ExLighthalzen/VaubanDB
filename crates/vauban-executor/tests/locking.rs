//! Reading and writing under locks, seen from the statements: which read waits, which one
//! goes through, which row is left out, and which write blocks another.
//!
//! # Two threads, and a bound on every wait
//!
//! A block needs two threads: each scenario runs the second statement on a thread of its
//! own, over one `MemoryStorage` and one `TransactionManager` shared through an `Arc`, each
//! statement in its own transaction. Every wait a test performs is bounded
//! by [`SOON`] or [`QUIET`], so a lock that is never given back fails the test instead of
//! hanging it. A statement that must be waiting before the scenario goes on is attested
//! through the queue of the lock manager ([`await_waiter`]), never through a sleep, and a
//! statement that must **not** wait is attested by an answer arriving within [`SOON`].
//!
//! # Axes
//!
//! Crossed: the isolation level of the reader (`READ COMMITTED`, `REPEATABLE READ`) × the
//! words written on the table reference (none, `NOLOCK`, `READPAST`, `NOWAIT`, `UPDLOCK`) ×
//! what the other transaction holds (an uncommitted `UPDATE`, an uncommitted `UPDATE` later
//! rolled back, an exclusive lock on one row of three) × which of the two waits (the reader
//! on a writer, a writer on a reader, a writer on a writer). Each scenario carries the
//! counter-proof that its word is what changes the answer: the same shape without the word
//! behaves the other way, in the same test.
//!
//! Not crossed, and why: `SERIALIZABLE`, which this engine serves as `REPEATABLE READ`
//! without range locks; the `SNAPSHOT` level and the two database options, which the reads
//! of a versioned level do not lock for; `TABLOCK`, `TABLOCKX`, `ROWLOCK` and `PAGLOCK`,
//! which change no lock; the numbers a queue produces beyond the 1222 of
//! `nowait_is_1222`; `DELETE`, whose lock is taken on the same line of the same file as the
//! one `UPDATE` takes.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, CompareOp, LockHints, OutputColumn, OutputSchema,
    SessionOptions,
};
use vauban_catalog::{Catalog, ColumnDef, QualifiedName, TableDef};
use vauban_errors::SqlResult;
use vauban_executor::{ExecContext, execute_collect};
use vauban_planner::{PhysicalPlan, PhysicalStatement, PhysicalUpdate};
use vauban_storage::{MemoryStorage, RowId, Storage, TableId, TxnId};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, LockManager, TransactionManager, TxnHandle};
use vauban_types::{SqlType, TypeInfo, Value};

/// How long a test waits for something it expects to happen.
const SOON: Duration = Duration::from_secs(5);

/// How long a test waits before concluding that nothing is coming.
const QUIET: Duration = Duration::from_millis(150);

fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

fn binding(index: usize, name: &str) -> ColumnBinding {
    ColumnBinding {
        column: vauban_catalog::ColumnId(index as i32 + 1),
        index,
        name: name.to_owned(),
        ty: int(index == 1),
    }
}

fn schema() -> OutputSchema {
    OutputSchema {
        columns: vec![
            OutputColumn {
                name: "a".to_owned(),
                ty: int(false),
            },
            OutputColumn {
                name: "b".to_owned(),
                ty: int(true),
            },
        ],
    }
}

fn lit(value: i32) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(value)),
        ty: int(true),
        line: 0,
    }
}

/// A two-column table of `rows`, committed, and the engine three statements share.
struct Fixture {
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
    catalog: Arc<Catalog>,
    table: TableId,
    /// The identifier the storage gave each row of `rows`, in that order.
    ids: Vec<RowId>,
}

impl Fixture {
    fn new(name: &str, rows: &[(i32, i32)]) -> Self {
        let memory = Arc::new(MemoryStorage::new());
        let storage: Arc<dyn Storage> = memory.clone();
        let txn = Arc::new(TransactionManager::new(storage.clone()));
        let catalog =
            Arc::new(Catalog::bootstrap(memory, txn.clone()).expect("the catalogue bootstraps"));
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let meta = catalog
            .create_table(
                &handle,
                &TableDef {
                    name: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: name.to_owned(),
                    },
                    columns: vec![
                        ColumnDef {
                            name: "a".to_owned(),
                            ty: int(false),
                            default: None,
                            identity: None,
                            computed: None,
                        },
                        ColumnDef {
                            name: "b".to_owned(),
                            ty: int(true),
                            default: None,
                            identity: None,
                            computed: None,
                        },
                    ],
                    constraints: Vec::new(),
                },
            )
            .expect("the table is new");
        let mut ids = Vec::new();
        for (a, b) in rows {
            ids.push(
                storage
                    .insert(
                        handle.id,
                        meta.storage_id,
                        &vauban_storage::Row(vec![Value::I32(*a), Value::I32(*b)]),
                    )
                    .expect("the row is inserted"),
            );
        }
        txn.commit(handle).expect("the setup commits");
        Self {
            storage,
            txn,
            catalog,
            table: meta.storage_id,
            ids,
        }
    }

    /// The handles and the shared engine, cloned for a thread of its own.
    fn share(&self) -> Self {
        Self {
            storage: Arc::clone(&self.storage),
            txn: Arc::clone(&self.txn),
            catalog: Arc::clone(&self.catalog),
            table: self.table,
            ids: self.ids.clone(),
        }
    }

    /// `SELECT a, b FROM t WITH (<hints>)`, run in `handle`.
    fn select(&self, handle: &TxnHandle, hints: LockHints) -> SqlResult<Vec<Vec<Value>>> {
        let eval = StaticContext::default();
        let snap = self.txn.statement_snapshot(handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), self.txn.as_ref(), &snap)
            .with_catalog(self.catalog.as_ref())
            .with_handle(handle);
        let stmt = PhysicalStatement::Query(PhysicalPlan::TableScan {
            table: self.table,
            columns: vec![binding(0, "a"), binding(1, "b")],
            alias: "t".to_owned(),
            schema: schema(),
            hints,
        });
        execute_collect(&stmt, &mut ctx).map(|(_, set)| set.rows)
    }

    /// `UPDATE t SET b = <value> WHERE a = <key>`, run in `handle`; the whole table when
    /// `key` is `None`.
    fn update(&self, handle: &TxnHandle, key: Option<i32>, value: i32) -> SqlResult<()> {
        let eval = StaticContext::default();
        let snap = self.txn.statement_snapshot(handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), self.txn.as_ref(), &snap)
            .with_catalog(self.catalog.as_ref())
            .with_handle(handle);
        let scan = PhysicalPlan::TableScan {
            table: self.table,
            columns: vec![binding(0, "a"), binding(1, "b")],
            alias: "t".to_owned(),
            schema: schema(),
            hints: LockHints::default(),
        };
        let input = match key {
            None => scan,
            Some(key) => PhysicalPlan::Filter {
                input: Box::new(scan),
                predicate: BoundExpr {
                    kind: BoundExprKind::Compare {
                        op: CompareOp::Eq,
                        left: Box::new(BoundExpr {
                            kind: BoundExprKind::ColumnRef(binding(0, "a")),
                            ty: int(false),
                            line: 0,
                        }),
                        right: Box::new(lit(key)),
                    },
                    ty: TypeInfo::new(SqlType::Bit, true),
                    line: 0,
                },
            },
        };
        let stmt = PhysicalStatement::Update(PhysicalUpdate {
            table: self.table,
            input,
            assignments: vec![(binding(1, "b"), lit(value))],
            spool: false,
        });
        execute_collect(&stmt, &mut ctx).map(|_| ())
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

/// Blocks until `txn` appears in the queue of `locks`, at most [`SOON`]; panics otherwise,
/// so that a scenario whose second statement never reached the queue asserts nothing by
/// accident.
fn await_waiter(locks: &LockManager, txn: TxnId) {
    let end = Instant::now() + SOON;
    while Instant::now() < end {
        if locks.waiters().iter().any(|(t, _, _)| *t == txn) {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("transaction {txn} never reached the queue");
}

/// Asserts that nothing came out of `rx` within [`QUIET`].
fn still_waiting<T: std::fmt::Debug>(rx: &Receiver<T>, what: &str) {
    match rx.recv_timeout(QUIET) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("{what} was expected to wait, it answered {other:?}"),
    }
}

/// The value of column `b` in the one row `rows` holds.
fn only_b(rows: &[Vec<Value>]) -> Value {
    assert_eq!(rows.len(), 1, "one row was expected: {rows:?}");
    rows[0][1].clone()
}

/// `READ COMMITTED`: a `SELECT` of a row an uncommitted `UPDATE` holds waits, and passes at
/// the `COMMIT` of that `UPDATE` with the value it wrote.
#[test]
fn read_committed_blocks_on_a_written_row() {
    let fixture = Fixture::new("blocks_on_a_written_row", &[(1, 10)]);
    let writer = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .update(&writer, None, 100)
        .expect("the writer updates");

    let reader = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), reader.clone());
    let read = in_background(move || shared.select(&handle, LockHints::default()));
    await_waiter(fixture.txn.locks(), reader.id);
    still_waiting(&read, "the reader");

    fixture.txn.commit(writer).expect("the writer commits");
    let rows = read
        .recv_timeout(SOON)
        .expect("the reader answers once the writer has committed")
        .expect("the read succeeds");
    assert_eq!(only_b(&rows), Value::I32(100));
    fixture.txn.commit(reader).expect("the reader commits");
}

/// `NOLOCK` reads the uncommitted value of the row without waiting, where the same read
/// without the word waits.
#[test]
fn nolock_reads_the_uncommitted_row() {
    let fixture = Fixture::new("nolock_reads_uncommitted", &[(1, 10)]);
    let writer = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .update(&writer, None, 100)
        .expect("the writer updates");

    let nolock = LockHints {
        nolock: true,
        ..LockHints::default()
    };
    let dirty = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let rows = fixture
        .select(&dirty, nolock)
        .expect("the read takes no lock");
    assert_eq!(only_b(&rows), Value::I32(100));
    fixture.txn.commit(dirty).expect("the dirty reader commits");

    // The counter-proof: without the word, the same read of the same row waits.
    let plain = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), plain.clone());
    let read = in_background(move || shared.select(&handle, LockHints::default()));
    await_waiter(fixture.txn.locks(), plain.id);
    still_waiting(&read, "the reader without the word");

    fixture.txn.commit(writer).expect("the writer commits");
    read.recv_timeout(SOON)
        .expect("the reader answers")
        .expect("the read succeeds");
    fixture.txn.commit(plain).expect("the reader commits");
}

/// A dirty read of a write that is rolled back afterwards has read a value the table never
/// keeps: the reader answers `100`, and the row is back to `10` once the writer is gone.
#[test]
fn nolock_sees_a_rolled_back_write() {
    let fixture = Fixture::new("nolock_sees_a_rollback", &[(1, 10)]);
    let writer = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .update(&writer, None, 100)
        .expect("the writer updates");

    let nolock = LockHints {
        nolock: true,
        ..LockHints::default()
    };
    let reader = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), reader.clone());
    let read = in_background(move || shared.select(&handle, nolock));
    let during = read
        .recv_timeout(SOON)
        .expect("the dirty read does not wait")
        .expect("the read succeeds");
    assert_eq!(only_b(&during), Value::I32(100));
    fixture.txn.commit(reader).expect("the reader commits");

    fixture.txn.rollback(writer).expect("the writer rolls back");
    let after = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let rows = fixture
        .select(&after, LockHints::default())
        .expect("the row is free");
    assert_eq!(only_b(&rows), Value::I32(10));
    assert_ne!(only_b(&during), only_b(&rows));
    fixture.txn.commit(after).expect("the later reader commits");
}

/// `REPEATABLE READ` keeps the shared lock of a row it has read until its transaction ends,
/// so a writer of that row waits; the same reader at `READ COMMITTED` does not hold it back.
#[test]
fn repeatable_read_holds_its_share_lock() {
    let fixture = Fixture::new("repeatable_read_holds", &[(1, 10)]);
    let reader = fixture.txn.begin(IsolationLevel::RepeatableRead);
    fixture
        .select(&reader, LockHints::default())
        .expect("the reader reads");

    let writer = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), writer.clone());
    let write = in_background(move || shared.update(&handle, None, 100));
    await_waiter(fixture.txn.locks(), writer.id);
    still_waiting(&write, "the writer");

    fixture.txn.commit(reader).expect("the reader commits");
    write
        .recv_timeout(SOON)
        .expect("the writer answers once the reader has committed")
        .expect("the update succeeds");
    fixture.txn.commit(writer).expect("the writer commits");

    // The counter-proof: the same read at `READ COMMITTED` gives its shared lock back with
    // the row, and the writer of that row goes through while the reader is still open.
    let released = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .select(&released, LockHints::default())
        .expect("the reader reads");
    let second = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), second.clone());
    let write = in_background(move || shared.update(&handle, None, 200));
    write
        .recv_timeout(SOON)
        .expect("the writer does not wait for a released shared lock")
        .expect("the update succeeds");
    fixture.txn.commit(second).expect("the writer commits");
    fixture.txn.commit(released).expect("the reader commits");
}

/// `READPAST` leaves out the row another transaction holds and reads the others, without
/// waiting; the same read without the word waits for that row.
#[test]
fn readpast_skips_the_locked_row() {
    let fixture = Fixture::new("readpast_skips", &[(1, 10), (2, 20), (3, 30)]);
    let writer = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .update(&writer, Some(2), 200)
        .expect("the writer updates one row");

    let readpast = LockHints {
        readpast: true,
        ..LockHints::default()
    };
    let reader = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), reader.clone());
    let read = in_background(move || shared.select(&handle, readpast));
    let rows = read
        .recv_timeout(SOON)
        .expect("the read does not wait")
        .expect("the read succeeds");
    let keys: Vec<Value> = rows.iter().map(|row| row[0].clone()).collect();
    assert_eq!(keys, vec![Value::I32(1), Value::I32(3)]);
    fixture.txn.commit(reader).expect("the reader commits");

    // The counter-proof: without the word, the same scan waits on the row that is held.
    let plain = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), plain.clone());
    let read = in_background(move || shared.select(&handle, LockHints::default()));
    await_waiter(fixture.txn.locks(), plain.id);
    still_waiting(&read, "the reader without the word");

    fixture.txn.commit(writer).expect("the writer commits");
    let rows = read
        .recv_timeout(SOON)
        .expect("the reader answers")
        .expect("the read succeeds");
    assert_eq!(rows.len(), 3);
    fixture.txn.commit(plain).expect("the reader commits");
}

/// `NOWAIT` answers 1222 instead of waiting for a row another transaction holds, and the
/// same read without the word is still waiting when the error has already come back.
#[test]
fn nowait_is_1222() {
    let fixture = Fixture::new("nowait_is_1222", &[(1, 10)]);
    let writer = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .update(&writer, None, 100)
        .expect("the writer updates");

    let nowait = LockHints {
        nowait: true,
        ..LockHints::default()
    };
    let refused = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), refused.clone());
    let read = in_background(move || shared.select(&handle, nowait));
    let error = read
        .recv_timeout(SOON)
        .expect("the read does not wait")
        .expect_err("the row is held by another transaction");
    assert_eq!(error.number, 1222);
    fixture
        .txn
        .rollback(refused)
        .expect("the reader rolls back");

    // The counter-proof: without the word, the same read of the same row is still in the
    // queue rather than holding an error.
    let plain = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), plain.clone());
    let read = in_background(move || shared.select(&handle, LockHints::default()));
    await_waiter(fixture.txn.locks(), plain.id);
    still_waiting(&read, "the reader without the word");

    fixture.txn.commit(writer).expect("the writer commits");
    read.recv_timeout(SOON)
        .expect("the reader answers")
        .expect("the read succeeds");
    fixture.txn.commit(plain).expect("the reader commits");
}

/// Two transactions that read one row under `UPDLOCK` and then write it: the second waits
/// for the first to commit and then goes through, and neither is chosen as a deadlock
/// victim.
#[test]
fn updlock_then_write_does_not_deadlock() {
    let fixture = Fixture::new("updlock_then_write", &[(1, 10)]);
    let updlock = LockHints {
        updlock: true,
        ..LockHints::default()
    };

    // The first transaction reads under `UPDLOCK`, says so, and waits to be told to write.
    let first = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), first.clone());
    let (read_tx, first_read) = channel();
    let (go_tx, go) = channel::<()>();
    let first_done = in_background(move || {
        let read = shared.select(&handle, updlock);
        read_tx
            .send(read.map(|rows| only_b(&rows)))
            .expect("listening");
        go.recv_timeout(SOON).expect("the test says when to write");
        shared.update(&handle, None, 100)
    });
    assert_eq!(
        first_read
            .recv_timeout(SOON)
            .expect("the first read answers")
            .expect("the read succeeds"),
        Value::I32(10)
    );

    // The second asks for the same update lock and waits: `UPDLOCK` is held to the end of
    // the transaction, so nothing is given back at the end of the row read.
    let second = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), second.clone());
    let second_done = in_background(move || {
        let read = shared.select(&handle, updlock)?;
        shared.update(&handle, None, 200).map(|()| only_b(&read))
    });
    await_waiter(fixture.txn.locks(), second.id);
    still_waiting(&second_done, "the second reader");

    go_tx.send(()).expect("the first thread is listening");
    first_done
        .recv_timeout(SOON)
        .expect("the first transaction answers")
        .expect("the first update succeeds");
    fixture.txn.commit(first).expect("the first commits");

    let read_by_second = second_done
        .recv_timeout(SOON)
        .expect("the second transaction answers once the first has committed")
        .expect("the second update succeeds");
    assert_eq!(read_by_second, Value::I32(100));
    fixture.txn.commit(second).expect("the second commits");

    let after = fixture.txn.begin(IsolationLevel::ReadCommitted);
    assert_eq!(
        only_b(
            &fixture
                .select(&after, LockHints::default())
                .expect("the row is free")
        ),
        Value::I32(200)
    );
    fixture.txn.commit(after).expect("the reader commits");
}

/// An uncommitted `UPDATE` holds the row exclusively: a second `UPDATE` of that row from
/// another transaction waits, and goes through at the `COMMIT` of the first.
#[test]
fn write_takes_an_exclusive_lock() {
    let fixture = Fixture::new("write_takes_x", &[(1, 10), (2, 20)]);
    let first = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .update(&first, Some(1), 100)
        .expect("the first writer updates");

    let second = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), second.clone());
    let write = in_background(move || shared.update(&handle, Some(1), 300));
    await_waiter(fixture.txn.locks(), second.id);
    still_waiting(&write, "the second writer");

    // The counter-proof: the other row is not held, and an update of it goes through while
    // the first transaction is still open.
    let other = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let (shared, handle) = (fixture.share(), other.clone());
    let free = in_background(move || shared.update(&handle, Some(2), 400));
    free.recv_timeout(SOON)
        .expect("the update of a free row does not wait")
        .expect("the update succeeds");
    fixture.txn.commit(other).expect("the third writer commits");

    fixture.txn.commit(first).expect("the first writer commits");
    write
        .recv_timeout(SOON)
        .expect("the second writer answers once the first has committed")
        .expect("the update succeeds");
    fixture
        .txn
        .commit(second)
        .expect("the second writer commits");

    let after = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let rows = fixture
        .select(&after, LockHints::default())
        .expect("every row is free");
    let values: Vec<Value> = rows.iter().map(|row| row[1].clone()).collect();
    assert_eq!(values, vec![Value::I32(300), Value::I32(400)]);
    fixture.txn.commit(after).expect("the reader commits");
    assert_eq!(fixture.ids.len(), 2);
}
