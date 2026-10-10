//! The `Spool` operator, and the write statement whose source reads its own target inside
//! a subquery.
//!
//! The operator tests put a `Spool` over a hand-written operator that records what it was
//! asked. The statement tests go through the parser, the binder, the planner and the
//! executor, on an in-memory catalogue.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use vauban_binder::{BindContext, NoVariables, OutputColumn, OutputSchema, SessionOptions, bind};
use vauban_catalog::{Catalog, ColumnDef, QualifiedName, TableDef};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_executor::{
    CancelToken, ExecContext, ExecOutcome, ExecSession, Operator, RowSet, execute_collect,
    ops::spool::Spool,
};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{NoIndexes, PlanContext, plan};
use vauban_storage::MemoryStorage;
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{SqlType, TypeInfo, Value};

fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

// ---------------------------------------------------------------------------------------
// The operator
// ---------------------------------------------------------------------------------------

/// What a [`Live`] operator was asked, shared with the test that built it.
#[derive(Default)]
struct Trace {
    opens: Cell<usize>,
    nexts: Cell<usize>,
    closes: Cell<usize>,
}

/// An operator that reads a shared list of `int` values at the time of each `next`, as a
/// scan reads a table a statement writes to: a value appended while the operator streams
/// is read too. `fail_at` makes the `next` of that position answer an error.
struct Live {
    table: Rc<RefCell<Vec<i32>>>,
    position: usize,
    trace: Rc<Trace>,
    fail_at: Option<usize>,
    schema: OutputSchema,
}

impl Live {
    fn new(table: &Rc<RefCell<Vec<i32>>>, trace: &Rc<Trace>) -> Self {
        Self {
            table: Rc::clone(table),
            position: 0,
            trace: Rc::clone(trace),
            fail_at: None,
            schema: OutputSchema {
                columns: vec![OutputColumn {
                    name: "k".to_owned(),
                    ty: int(),
                }],
            },
        }
    }
}

impl<'a> Operator<'a> for Live {
    fn open(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.trace.opens.set(self.trace.opens.get() + 1);
        self.position = 0;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<Option<Vec<Value>>> {
        self.trace.nexts.set(self.trace.nexts.get() + 1);
        if self.fail_at == Some(self.position) {
            return Err(SqlError::from(InternalError::Bug("read failed".to_owned())));
        }
        let value = self.table.borrow().get(self.position).copied();
        self.position += 1;
        Ok(value.map(|k| vec![Value::I32(k)]))
    }

    fn close(&mut self) {
        self.trace.closes.set(self.trace.closes.get() + 1);
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

fn table(values: &[i32]) -> Rc<RefCell<Vec<i32>>> {
    Rc::new(RefCell::new(values.to_vec()))
}

/// Copies each row `root` hands out back into `table`, plus ten, as
/// `INSERT t SELECT k + 10 FROM t` does, and answers how many rows were copied. Stops at
/// `cap` rows, which only a source that reads its own writes reaches.
fn copy_into<'a>(
    root: &mut (dyn Operator<'a> + 'a),
    table: &Rc<RefCell<Vec<i32>>>,
    ctx: &mut ExecContext<'a>,
    cap: usize,
) -> usize {
    root.open(ctx).expect("the source opens");
    let mut copied = 0;
    while copied < cap {
        let Some(row) = root.next(ctx).expect("a row is read") else {
            break;
        };
        let Value::I32(k) = row[0] else {
            panic!("an int row, got {row:?}");
        };
        table.borrow_mut().push(k + 10);
        copied += 1;
    }
    root.close();
    copied
}

#[test]
fn a_spooled_source_does_not_read_the_rows_written_from_it() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());

    let spooled = table(&[1, 2, 3]);
    let trace = Rc::new(Trace::default());
    let mut spool = Spool::new(Box::new(Live::new(&spooled, &trace)));
    assert_eq!(copy_into(&mut spool, &spooled, &mut ctx, 100), 3);
    assert_eq!(*spooled.borrow(), vec![1, 2, 3, 11, 12, 13]);

    // The counter-proof: the same copy, streamed, reads its own writes until the cap.
    let streamed = table(&[1, 2, 3]);
    let trace = Rc::new(Trace::default());
    let mut live = Live::new(&streamed, &trace);
    assert_eq!(copy_into(&mut live, &streamed, &mut ctx, 100), 100);
}

#[test]
fn the_input_is_drained_and_closed_when_the_spool_opens() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let rows = table(&[4, 5, 6]);
    let trace = Rc::new(Trace::default());
    let mut spool = Spool::new(Box::new(Live::new(&rows, &trace)));

    spool.open(&mut ctx).expect("the spool opens");
    assert_eq!(trace.opens.get(), 1);
    assert_eq!(trace.nexts.get(), 4, "three rows and the end of the input");
    assert_eq!(
        trace.closes.get(),
        1,
        "the input is closed before any row is handed out"
    );

    let mut out = Vec::new();
    while let Some(row) = spool.next(&mut ctx).expect("a row") {
        out.push(row);
    }
    assert_eq!(
        out,
        vec![
            vec![Value::I32(4)],
            vec![Value::I32(5)],
            vec![Value::I32(6)]
        ],
        "the rows come out in the order they were read"
    );
    assert_eq!(
        trace.nexts.get(),
        4,
        "handing the rows out reads nothing more"
    );
    spool.close();
    assert_eq!(
        trace.closes.get(),
        1,
        "closing the spool closes nothing twice"
    );
    assert_eq!(spool.schema().columns.len(), 1);
    assert_eq!(spool.schema().columns[0].name, "k");
}

#[test]
fn a_spool_opened_twice_reads_its_input_again() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let rows = table(&[1]);
    let trace = Rc::new(Trace::default());
    let mut spool = Spool::new(Box::new(Live::new(&rows, &trace)));
    spool.open(&mut ctx).expect("first open");
    spool.close();
    rows.borrow_mut().push(2);
    spool.open(&mut ctx).expect("second open");
    let mut out = Vec::new();
    while let Some(row) = spool.next(&mut ctx).expect("a row") {
        out.push(row);
    }
    assert_eq!(out, vec![vec![Value::I32(1)], vec![Value::I32(2)]]);
}

#[test]
fn an_error_of_the_input_closes_it_and_reaches_the_caller() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let rows = table(&[1, 2, 3]);
    let trace = Rc::new(Trace::default());
    let mut live = Live::new(&rows, &trace);
    live.fail_at = Some(1);
    let mut spool = Spool::new(Box::new(live));
    let err = spool
        .open(&mut ctx)
        .expect_err("the read error reaches the caller");
    assert!(err.message.contains("read failed"), "{}", err.message);
    assert_eq!(trace.closes.get(), 1);
}

#[test]
fn a_cancelled_spool_hands_out_nothing() {
    let eval = StaticContext::default();
    let token = CancelToken::new();
    token.cancel();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_cancel(&token);
    // More rows than the interval at which the token is read.
    let rows = table(&(0..5000).collect::<Vec<_>>());
    let trace = Rc::new(Trace::default());
    let mut spool = Spool::new(Box::new(Live::new(&rows, &trace)));
    spool
        .open(&mut ctx)
        .expect("a cancellation is not an error");
    assert!(trace.nexts.get() < 5000, "the read stops early");
    assert_eq!(trace.closes.get(), 1);
    assert!(spool.next(&mut ctx).expect("no error").is_none());
}

#[test]
fn without_the_flag_the_input_is_handed_back_as_it_is() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let rows = table(&[1, 2]);

    let trace = Rc::new(Trace::default());
    let mut streamed = Spool::wrap_if(false, Box::new(Live::new(&rows, &trace)));
    streamed.open(&mut ctx).expect("opens");
    assert_eq!(trace.nexts.get(), 0, "an unspooled input is read on demand");
    streamed.close();

    let trace = Rc::new(Trace::default());
    let mut spooled = Spool::wrap_if(true, Box::new(Live::new(&rows, &trace)));
    spooled.open(&mut ctx).expect("opens");
    assert_eq!(
        trace.nexts.get(),
        3,
        "a spooled input is read when it opens"
    );
    spooled.close();
}

// ---------------------------------------------------------------------------------------
// The statement
// ---------------------------------------------------------------------------------------

/// An in-memory engine, one open transaction, and the session the statements write.
struct Fixture {
    storage: Arc<MemoryStorage>,
    txn_mgr: Arc<TransactionManager>,
    catalog: Catalog,
    handle: TxnHandle,
    eval: StaticContext,
    session: ExecSession,
}

impl Fixture {
    fn new() -> Self {
        let storage = Arc::new(MemoryStorage::new());
        let txn_mgr = Arc::new(TransactionManager::new(
            storage.clone() as Arc<dyn vauban_storage::Storage>
        ));
        let catalog =
            Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
        let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
        Self {
            storage,
            txn_mgr,
            catalog,
            handle,
            eval: StaticContext::default(),
            session: ExecSession::default(),
        }
    }

    /// Creates `dbo.<name>` with one nullable `int` column `k`.
    fn create(&self, name: &str) {
        self.catalog
            .create_table(
                &self.handle,
                &TableDef {
                    name: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: name.to_owned(),
                    },
                    columns: vec![ColumnDef {
                        name: "k".to_owned(),
                        ty: int(),
                        default: None,
                        identity: None,
                        computed: None,
                    }],
                    constraints: Vec::new(),
                },
            )
            .expect("create_table succeeds");
    }

    /// Parses, binds, plans and runs the one statement of `text`.
    fn run_sql(&mut self, text: &str) -> SqlResult<(ExecOutcome, RowSet)> {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the statement parses");
        assert_eq!(batch.statements.len(), 1, "one statement");
        let physical = {
            let snap = self.catalog.snapshot(&self.handle);
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
                    catalog: &NoIndexes,
                },
            )?
        };
        let Self {
            storage,
            txn_mgr,
            catalog,
            handle,
            eval,
            session,
        } = self;
        let snap = txn_mgr.statement_snapshot(handle);
        let mut ctx = ExecContext::scalar(eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(catalog)
            .with_handle(handle)
            .with_session(session);
        execute_collect(&physical, &mut ctx)
    }

    fn values_of(&mut self, table: &str) -> Vec<Vec<Value>> {
        let (_, set) = self
            .run_sql(&format!("SELECT k FROM dbo.{table} ORDER BY k"))
            .expect("the read succeeds");
        set.rows
    }
}

/// `INSERT h SELECT u.k FROM u WHERE NOT EXISTS (SELECT 1 FROM h WHERE h.k = u.k)` with
/// the value 5 twice in `u`: the source is read before the first row is written, so both
/// rows pass the `NOT EXISTS`. On a source streamed into the writes, the second 5 would see
/// the first and be dropped.
#[test]
fn an_insert_whose_not_exists_reads_the_target_reads_its_source_first() {
    let mut fx = Fixture::new();
    fx.create("h");
    fx.create("u");
    fx.run_sql("INSERT u VALUES (5), (5)").expect("seed");
    fx.run_sql("INSERT h SELECT u.k FROM u WHERE NOT EXISTS (SELECT 1 FROM h WHERE h.k = u.k)")
        .expect("the insert runs");
    assert_eq!(
        fx.values_of("h"),
        vec![vec![Value::I32(5)], vec![Value::I32(5)]]
    );
    assert_eq!(fx.session.rowcount, 2);
}

/// The same shape, reading another table in the subquery: the result does not depend on
/// the spool, and both rows go in.
#[test]
fn an_insert_whose_not_exists_reads_another_table_inserts_each_row() {
    let mut fx = Fixture::new();
    fx.create("h");
    fx.create("u");
    fx.create("w");
    fx.run_sql("INSERT u VALUES (5), (6)").expect("seed");
    fx.run_sql("INSERT w VALUES (6)").expect("seed");
    fx.run_sql("INSERT h SELECT u.k FROM u WHERE NOT EXISTS (SELECT 1 FROM w WHERE w.k = u.k)")
        .expect("the insert runs");
    assert_eq!(fx.values_of("h"), vec![vec![Value::I32(5)]]);
}
