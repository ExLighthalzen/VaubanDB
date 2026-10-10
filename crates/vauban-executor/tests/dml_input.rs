//! `UPDATE` and `DELETE` over an input plan of any shape: a join with the sources of a
//! `FROM`, an `IN (SELECT …)`, an `EXISTS`, the target named by an alias, the spool, and a
//! row another transaction changes while the statement waits for it.
//!
//! The statements go through the whole chain a client's text takes (parse, bind, plan,
//! execute) over one in-memory engine. Each session holds a transaction of its own; the
//! tests of two sessions run the second one on a thread, every wait bounded by [`SOON`].

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use vauban_binder::{
    BindContext, ColumnBinding, LockHints, NoVariables, OutputColumn, OutputSchema, SessionOptions,
    bind,
};
use vauban_catalog::{Catalog, ColumnId};
use vauban_errors::SqlResult;
use vauban_executor::{
    ExecContext, ExecOutcome, ExecSession, RowSet, execute_collect, ops::spool::Spool,
};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{
    PhysicalPlan, PhysicalStatement, PlanContext, StorageIndexes, plan, row_id_binding,
};
use vauban_storage::{DbId, MemoryStorage, RowId, Storage, TableId, TxnId};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, LockManager, TransactionManager, TxnHandle, VersioningOptions};
use vauban_types::{SqlType, TypeInfo, Value};

/// How long a test waits for something it expects to happen.
const SOON: Duration = Duration::from_secs(5);

/// How long a test waits before concluding that nothing is coming.
const QUIET: Duration = Duration::from_millis(150);

/// The engine the sessions share.
#[derive(Clone)]
struct Engine {
    storage: Arc<MemoryStorage>,
    txn: Arc<TransactionManager>,
    catalog: Arc<Catalog>,
}

impl Engine {
    fn new() -> Self {
        let storage = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(storage.clone() as Arc<dyn Storage>));
        let catalog = Arc::new(
            Catalog::bootstrap(storage.clone(), txn.clone()).expect("the catalogue bootstraps"),
        );
        Self {
            storage,
            txn,
            catalog,
        }
    }

    /// Runs `text`, one statement per line, in a transaction committed at the end.
    fn setup(&self, text: &str) {
        let mut session = self.session();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            session.run(line).expect("the setup runs");
        }
        session.commit();
    }

    /// A session with a `READ COMMITTED` transaction of its own.
    fn session(&self) -> Session {
        Session {
            engine: self.clone(),
            handle: self.txn.begin(IsolationLevel::ReadCommitted),
            eval: StaticContext::default(),
            state: ExecSession::default(),
        }
    }

    /// The database the transactions of [`Engine::session`] open on.
    fn database(&self) -> DbId {
        self.storage
            .databases()
            .expect("the databases are listed")
            .into_iter()
            .find(|(_, name)| name == "master")
            .map(|(id, _)| id)
            .expect("master exists")
    }

    /// The table `dbo.<name>` of `master`.
    fn table(&self, name: &str) -> TableId {
        let session = self.session();
        let snap = self.catalog.snapshot(&session.handle);
        let object = snap
            .resolve_object("master", Some("dbo"), name, "dbo")
            .expect("the table exists")
            .id;
        let id = snap
            .table(object)
            .expect("the object is a table")
            .storage_id;
        session.commit();
        id
    }
}

/// One session: its transaction and the state its statements write.
struct Session {
    engine: Engine,
    handle: TxnHandle,
    eval: StaticContext,
    state: ExecSession,
}

impl Session {
    /// Parses, binds, plans and runs the one statement of `text`.
    fn run(&mut self, text: &str) -> SqlResult<(ExecOutcome, RowSet)> {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the statement parses");
        assert_eq!(batch.statements.len(), 1, "one statement: {text}");
        let engine = self.engine.clone();
        let physical = {
            let snap = engine.catalog.snapshot(&self.handle);
            let bind_ctx = BindContext {
                text,
                catalog: Some(&snap),
                database: "master",
                default_schema: "dbo",
                variables: &NoVariables,
                options: SessionOptions::default(),
            };
            let bound = bind(&batch.statements[0], &bind_ctx)?;
            plan(
                bound,
                &PlanContext {
                    catalog: &StorageIndexes(engine.storage.as_ref()),
                },
            )?
        };
        self.execute(&physical)
    }

    fn execute(&mut self, physical: &PhysicalStatement) -> SqlResult<(ExecOutcome, RowSet)> {
        let engine = self.engine.clone();
        let snap = engine.txn.statement_snapshot(&self.handle);
        let mut ctx = ExecContext::scalar(&self.eval, SessionOptions::default())
            .with_engine(
                engine.storage.as_ref() as &dyn Storage,
                engine.txn.as_ref(),
                &snap,
            )
            .with_catalog(engine.catalog.as_ref())
            .with_handle(&self.handle)
            .with_session(&mut self.state);
        execute_collect(physical, &mut ctx)
    }

    /// The rows of `SELECT * FROM dbo.<table> ORDER BY 1`, as integers, `NULL` as `None`.
    fn rows(&mut self, table: &str) -> Vec<Vec<Option<i32>>> {
        let (_, set) = self
            .run(&format!("SELECT * FROM dbo.{table} ORDER BY 1"))
            .expect("the read succeeds");
        set.rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| match value {
                        Value::I32(v) => Some(v),
                        Value::Null => None,
                        other => panic!("an int was expected, got {other:?}"),
                    })
                    .collect()
            })
            .collect()
    }

    fn rowcount(&self) -> i64 {
        self.state.rowcount
    }

    fn commit(self) {
        self.engine
            .txn
            .commit(self.handle)
            .expect("the session commits");
    }
}

/// Rows of `(Option<i32>, …)` literals, for the comparisons.
fn table_of(rows: &[&[Option<i32>]]) -> Vec<Vec<Option<i32>>> {
    rows.iter().map(|row| row.to_vec()).collect()
}

/// Runs `f` on a thread of its own and hands back what it returns.
fn in_background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

/// Waits until transaction `txn` is queued on a lock.
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
fn still_waiting<T>(rx: &Receiver<T>, what: &str) {
    match rx.recv_timeout(QUIET) {
        Err(RecvTimeoutError::Timeout) => {}
        Ok(_) => panic!("{what} was expected to wait, it answered"),
        Err(RecvTimeoutError::Disconnected) => panic!("{what} stopped without an answer"),
    }
}

const B_AND_S: &str = "
CREATE TABLE dbo.b (id int NOT NULL, v int NULL)
CREATE TABLE dbo.s (id int NOT NULL, w int NULL)
INSERT INTO dbo.b VALUES (1, 10), (2, 20), (3, 30)
INSERT INTO dbo.s VALUES (1, 100), (1, 101), (2, 200)
";

/// `DELETE FROM a WHERE id IN (SELECT 1)` deletes the one row of `id` 1.
#[test]
fn a_delete_whose_where_is_an_in_over_a_constant_subquery() {
    let engine = Engine::new();
    engine.setup(
        "CREATE TABLE dbo.a (id int NOT NULL PRIMARY KEY, v int NULL)
         INSERT INTO dbo.a VALUES (1, 10), (2, 20), (3, 30)",
    );
    let mut session = engine.session();
    session
        .run("DELETE FROM a WHERE id IN (SELECT 1)")
        .expect("the delete runs");
    assert_eq!(session.rowcount(), 1);
    assert_eq!(
        session.rows("a"),
        table_of(&[&[Some(2), Some(20)], &[Some(3), Some(30)]])
    );
}

/// `UPDATE b SET v = 0 FROM b JOIN s ON 1 = 1` reaches each row of `b` three times and
/// writes it once: three rows counted, not nine.
#[test]
fn a_target_row_reached_twice_is_written_once() {
    let engine = Engine::new();
    engine.setup(B_AND_S);
    let mut session = engine.session();
    session
        .run("UPDATE b SET v = 0 FROM b JOIN s ON 1 = 1")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 3);
    assert_eq!(
        session.rows("b"),
        table_of(&[
            &[Some(1), Some(0)],
            &[Some(2), Some(0)],
            &[Some(3), Some(0)]
        ])
    );
    // A value computed from the row itself is added once per row, not once per partner.
    session
        .run("UPDATE b SET v = v + 1 FROM b JOIN s ON 1 = 1")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 3);
    assert_eq!(
        session.rows("b"),
        table_of(&[
            &[Some(1), Some(1)],
            &[Some(2), Some(1)],
            &[Some(3), Some(1)]
        ])
    );
}

/// `UPDATE x SET v = s.w FROM b AS x JOIN s ON s.id = x.id`: the rows with a partner take
/// the value of one of them; the row of `id` 1 has two partners, and which one wins is not
/// specified.
#[test]
fn an_update_through_the_alias_of_a_source_of_the_from() {
    let engine = Engine::new();
    engine.setup(B_AND_S);
    let mut session = engine.session();
    session
        .run("UPDATE x SET v = s.w FROM b AS x JOIN s ON s.id = x.id")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 2);
    let rows = session.rows("b");
    assert!(
        rows[0] == [Some(1), Some(100)] || rows[0] == [Some(1), Some(101)],
        "{rows:?}"
    );
    assert_eq!(
        rows[1..],
        table_of(&[&[Some(2), Some(200)], &[Some(3), Some(30)]])
    );
}

/// A `FROM` that reads the target twice: the source the target names is the one written,
/// unaliased in the first statement, by its alias in the second.
#[test]
fn a_self_join_writes_the_source_the_target_names() {
    let engine = Engine::new();
    engine.setup(B_AND_S);
    let mut session = engine.session();
    session
        .run("UPDATE b SET v = y.v + 1 FROM b JOIN b AS y ON y.id = b.id + 1")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 2);
    assert_eq!(
        session.rows("b"),
        table_of(&[
            &[Some(1), Some(21)],
            &[Some(2), Some(31)],
            &[Some(3), Some(30)]
        ])
    );
    session
        .run("UPDATE y SET v = b.v * 10 FROM b JOIN b AS y ON y.id = b.id + 1")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 2);
    assert_eq!(
        session.rows("b"),
        table_of(&[
            &[Some(1), Some(21)],
            &[Some(2), Some(210)],
            &[Some(3), Some(310)]
        ])
    );
    session
        .run("DELETE y FROM b JOIN b AS y ON y.id = b.id + 1 WHERE b.id = 1")
        .expect("the delete runs");
    assert_eq!(session.rowcount(), 1);
    assert_eq!(
        session.rows("b"),
        table_of(&[&[Some(1), Some(21)], &[Some(3), Some(310)]])
    );
}

/// A target no source of the `FROM` names joins it: every row of `b` is written when `s`
/// has a row that passes the `WHERE`, none otherwise, and the columns of the target are in
/// scope.
#[test]
fn a_target_outside_the_from_is_joined_to_it() {
    let engine = Engine::new();
    engine.setup(B_AND_S);
    let mut session = engine.session();
    session
        .run("UPDATE b SET v = 7 FROM s WHERE s.id = 2")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 3);
    session
        .run("UPDATE b SET v = 8 FROM s WHERE s.id = 99")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 0);
    session
        .run("UPDATE b SET v = b.v + s.w FROM s WHERE s.id = b.id AND s.w > 150")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 1);
    assert_eq!(
        session.rows("b"),
        table_of(&[
            &[Some(1), Some(7)],
            &[Some(2), Some(207)],
            &[Some(3), Some(7)]
        ])
    );
}

/// `EXISTS`, `NOT EXISTS` and `NOT IN` over another table, correlated or not.
#[test]
fn exists_and_not_exists_choose_the_rows() {
    let engine = Engine::new();
    engine.setup(B_AND_S);
    let mut session = engine.session();
    session
        .run("UPDATE b SET v = 0 WHERE EXISTS (SELECT 1 FROM s WHERE s.id = b.id AND s.w > 150)")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 1);
    session
        .run("UPDATE b SET v = -1 WHERE NOT EXISTS (SELECT 1 FROM s WHERE s.id = b.id)")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 1);
    assert_eq!(
        session.rows("b"),
        table_of(&[
            &[Some(1), Some(10)],
            &[Some(2), Some(0)],
            &[Some(3), Some(-1)]
        ])
    );
    session
        .run("DELETE FROM b WHERE id NOT IN (SELECT id FROM s)")
        .expect("the delete runs");
    assert_eq!(session.rowcount(), 1);
    session
        .run("DELETE b FROM b JOIN s ON s.id = b.id")
        .expect("the delete runs");
    assert_eq!(session.rowcount(), 2);
    assert!(session.rows("b").is_empty());
}

/// `UPDATE t SET v = v + 1 WHERE k IN (SELECT k FROM t WHERE v < 10)` on (1,1), (2,5),
/// (3,20) writes each of the first two rows once. The second statement makes the spool
/// matter: row 3 qualifies through the value row 2 had before the statement; written as
/// the input is read, row 2 would no longer let row 3 through.
#[test]
fn an_in_over_the_target_writes_each_row_once() {
    let engine = Engine::new();
    engine.setup(
        "CREATE TABLE dbo.t (k int NOT NULL PRIMARY KEY, v int NULL)
         INSERT INTO dbo.t VALUES (1, 1), (2, 5), (3, 20)",
    );
    let mut session = engine.session();
    session
        .run("UPDATE t SET v = v + 1 WHERE k IN (SELECT k FROM t WHERE v < 10)")
        .expect("the update runs");
    assert_eq!(
        session.rows("t"),
        table_of(&[
            &[Some(1), Some(2)],
            &[Some(2), Some(6)],
            &[Some(3), Some(20)]
        ])
    );
    session
        .run("UPDATE t SET v = v + 100 WHERE k IN (SELECT k + 1 FROM t WHERE v < 10)")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 2);
    assert_eq!(
        session.rows("t"),
        table_of(&[
            &[Some(1), Some(2)],
            &[Some(2), Some(106)],
            &[Some(3), Some(120)]
        ])
    );
    session
        .run("DELETE FROM t WHERE EXISTS (SELECT 1 FROM t AS p WHERE p.k = t.k - 1)")
        .expect("the delete runs");
    assert_eq!(session.rowcount(), 2);
    assert_eq!(session.rows("t"), table_of(&[&[Some(1), Some(2)]]));
}

/// A value of the `SET` list that reads the target is computed for every row before the
/// first write: each row takes the value the row before it had when the statement started.
/// Written as the rows are read, row 2 would take the `NULL` row 1 had just received.
/// The second statement reads the row after each one, which the order of the writes leaves
/// untouched either way.
#[test]
fn a_set_value_reading_the_target_is_computed_before_the_writes() {
    let engine = Engine::new();
    engine.setup(
        "CREATE TABLE dbo.t (k int NOT NULL PRIMARY KEY, v int NULL)
         INSERT INTO dbo.t VALUES (1, 10), (2, 20), (3, 30)",
    );
    let mut session = engine.session();
    session
        .run("UPDATE t SET v = (SELECT x.v FROM t AS x WHERE x.k = t.k - 1)")
        .expect("the update runs");
    assert_eq!(session.rowcount(), 3);
    assert_eq!(
        session.rows("t"),
        table_of(&[&[Some(1), None], &[Some(2), Some(10)], &[Some(3), Some(20)]])
    );
    session
        .run("UPDATE t SET v = (SELECT x.v FROM t AS x WHERE x.k = t.k + 1)")
        .expect("the update runs");
    assert_eq!(
        session.rows("t"),
        table_of(&[&[Some(1), Some(10)], &[Some(2), Some(20)], &[Some(3), None]])
    );
}

/// The scenario of a row changed while the statement waits for it. Session A changes row 1
/// out of the `WHERE` of session B and row 2 inside it, without committing; B's `UPDATE`
/// waits; A commits. B leaves row 1 as A left it, and writes row 2 from the value A wrote.
fn a_row_changed_while_waiting(versioned: bool) {
    let engine = Engine::new();
    engine.setup(
        "CREATE TABLE dbo.w (a int NOT NULL, b int NULL)
         INSERT INTO dbo.w VALUES (1, 10), (2, 10), (3, 60)",
    );
    if versioned {
        engine.txn.set_versioning_options(
            engine.database(),
            VersioningOptions {
                read_committed_snapshot: true,
                allow_snapshot_isolation: false,
            },
        );
    }
    let mut first = engine.session();
    first
        .run("UPDATE w SET b = 99 WHERE a = 1")
        .expect("A changes row 1");
    first
        .run("UPDATE w SET b = 20 WHERE a = 2")
        .expect("A changes row 2");

    let mut second = engine.session();
    let waiter = second.handle.id;
    let write = in_background(move || {
        let answer = second.run("UPDATE w SET b = b + 1000 WHERE b < 50");
        (answer.map(|_| ()), second)
    });
    await_waiter(engine.txn.locks(), waiter);
    still_waiting(&write, "the second session");
    first.commit();
    let (answer, mut second) = write.recv_timeout(SOON).expect("B answers once A commits");
    answer.expect("B's update succeeds");
    assert_eq!(second.rowcount(), 1);
    assert_eq!(
        second.rows("w"),
        table_of(&[
            &[Some(1), Some(99)],
            &[Some(2), Some(1020)],
            &[Some(3), Some(60)]
        ])
    );
    second.commit();
}

/// Reads that lock: B's scan waits on the row A holds, and reads what A committed.
#[test]
fn a_row_changed_out_of_the_where_is_not_updated_after_the_wait() {
    a_row_changed_while_waiting(false);
}

/// `READ_COMMITTED_SNAPSHOT`: B's scan reads the versions of its snapshot without waiting,
/// its write waits, and the row is read again once A has committed.
#[test]
fn a_row_changed_out_of_the_where_is_not_updated_under_row_versioning() {
    a_row_changed_while_waiting(true);
}

/// `NOWAIT` on the target of an `UPDATE` answers 1222 on a row another transaction holds,
/// where the same statement without the word waits.
#[test]
fn nowait_on_the_target_answers_1222() {
    let engine = Engine::new();
    engine.setup(
        "CREATE TABLE dbo.n (a int NOT NULL, b int NULL)
         INSERT INTO dbo.n VALUES (1, 10)",
    );
    let mut first = engine.session();
    first
        .run("UPDATE n SET b = 11 WHERE a = 1")
        .expect("A holds the row");

    let mut second = engine.session();
    let err = second
        .run("UPDATE n WITH (NOWAIT) SET b = 12 WHERE a = 1")
        .expect_err("the row is held");
    assert_eq!(err.number, 1222);

    let waiter = second.handle.id;
    let write = in_background(move || second.run("UPDATE n SET b = 12 WHERE a = 1").map(|_| ()));
    await_waiter(engine.txn.locks(), waiter);
    still_waiting(&write, "the update without NOWAIT");
    first.commit();
    write
        .recv_timeout(SOON)
        .expect("the update answers once the row is free")
        .expect("the update succeeds");
}

/// A spool hands each row over whole, the row identifier its scan appended included.
#[test]
fn the_row_identifier_crosses_the_spool() {
    let engine = Engine::new();
    engine.setup("CREATE TABLE dbo.r (a int NOT NULL)");
    let table = engine.table("r");
    let session = engine.session();
    let ids: Vec<RowId> = (0..3)
        .map(|value| {
            engine
                .storage
                .insert(
                    session.handle.id,
                    table,
                    &vauban_storage::Row(vec![Value::I32(value)]),
                )
                .expect("the row is inserted")
        })
        .collect();
    let int = TypeInfo::new(SqlType::Int, false);
    let scan = PhysicalPlan::TableScan {
        table,
        columns: vec![
            ColumnBinding {
                column: ColumnId(1),
                index: 0,
                name: "a".to_owned(),
                ty: int.clone(),
                depth: 0,
            },
            row_id_binding(table).expect("the table identifier is small"),
        ],
        alias: "r".to_owned(),
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: "a".to_owned(),
                ty: int,
            }],
        },
        hints: LockHints::default(),
    };
    let eval = StaticContext::default();
    let snap = engine.txn.statement_snapshot(&session.handle);
    let mut seen = Vec::new();
    {
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                engine.storage.as_ref() as &dyn Storage,
                engine.txn.as_ref(),
                &snap,
            )
            .with_handle(&session.handle);
        let mut spool = Spool::wrap_if(
            true,
            vauban_executor::build_operator(&scan).expect("the scan builds"),
        );
        spool.open(&mut ctx).expect("the spool opens");
        while let Some(row) = spool.next(&mut ctx).expect("a row") {
            seen.push(row);
        }
        spool.close();
    }
    let expected: Vec<Vec<Value>> = ids
        .iter()
        .zip(0..)
        .map(|(id, value)| {
            vec![
                Value::I32(value),
                Value::I64(i64::from_ne_bytes(id.0.to_ne_bytes())),
            ]
        })
        .collect();
    assert_eq!(seen, expected);
    session.commit();
}
