//! The operator tree over a hand-built physical plan: a `TableScan` or a `Values` under
//! `Filter`, `Project` and `Top`, driven by [`execute`] into a sink, and stopped by the
//! cancellation token.
//!
//! Each plan is built by hand over an in-memory storage filled through `storage.insert`;
//! no SQL text is parsed, bound or planned here. What the same operators answer for a
//! `SELECT` written as text is `tests/execute_select.rs`.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, BoundTop, ColumnBinding, CompareOp, OutputColumn,
    OutputSchema, SessionOptions,
};
use vauban_catalog::ColumnId;
use vauban_errors::{InfoMessage, SqlResult};
use vauban_executor::{
    CancelToken, CollectSink, ExecContext, ExecOutcome, Operator, Row, RowSink, build_operator,
    execute, ops::limit::Limit,
};
use vauban_planner::{KeyRangeExpr, PhysicalJoinKind, PhysicalPlan, PhysicalStatement};
use vauban_storage::{
    Direction, IndexId, MemoryStorage, Row as StorageRow, Snapshot, Storage, TableId, TableShape,
};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{BinaryOp, SqlType, TypeInfo, Value};

// ---------------------------------------------------------------------------------------
// The storage and the context
// ---------------------------------------------------------------------------------------

/// An `int` table of `width` columns in a fresh in-memory storage, and the manager that
/// opens transactions on it.
struct Fixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    table: TableId,
}

impl Fixture {
    fn new(width: usize) -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let shape = TableShape {
            columns: vec![int(); width],
            clustered_key: None,
        };
        let table = storage.create_table(db, &shape).expect("the table is new");
        let txn = TransactionManager::new(Arc::clone(&storage));
        Self {
            storage,
            txn,
            table,
        }
    }

    /// Inserts one row of `int` values with a transaction of its own, committed.
    fn insert(&self, values: &[i32]) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let row = StorageRow(values.iter().map(|v| Value::I32(*v)).collect());
        self.storage
            .insert(handle.id, self.table, &row)
            .expect("the row is inserted");
        self.txn.commit(handle).expect("the transaction commits");
    }

    /// A snapshot of a freshly opened transaction, as one is taken per statement.
    fn snapshot(&self) -> Snapshot {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.txn.statement_snapshot(&handle)
    }

    /// Runs `plan` into `sink` with a context that carries the engine and `token`.
    fn run(
        &self,
        plan: &PhysicalPlan,
        token: &CancelToken,
        sink: &mut dyn RowSink,
    ) -> SqlResult<ExecOutcome> {
        let eval = StaticContext::default();
        let snap = self.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), &self.txn, &snap)
            .with_cancel(token);
        execute(&PhysicalStatement::Query(plan.clone()), &mut ctx, sink)
    }
}

/// Runs `plan` into `sink` with a scalar context: no storage at all, and `token`.
fn run_scalar(
    plan: &PhysicalPlan,
    token: &CancelToken,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_cancel(token);
    execute(&PhysicalStatement::Query(plan.clone()), &mut ctx, sink)
}

// ---------------------------------------------------------------------------------------
// The sinks
// ---------------------------------------------------------------------------------------

/// What a sink was handed, in order.
#[derive(Debug, PartialEq)]
enum Event {
    Columns(Vec<String>),
    Row(Row),
    Info(u32),
}

/// A sink that records the order of the calls, and raises a token after a given number
/// of rows when asked to.
struct EventSink {
    events: Vec<Event>,
    cancel_after: Option<(usize, CancelToken)>,
}

impl EventSink {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            cancel_after: None,
        }
    }

    fn cancelling_after(rows: usize, token: &CancelToken) -> Self {
        Self {
            events: Vec::new(),
            cancel_after: Some((rows, token.clone())),
        }
    }

    fn rows(&self) -> Vec<&Row> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Row(row) => Some(row),
                _ => None,
            })
            .collect()
    }
}

impl RowSink for EventSink {
    fn columns(&mut self, schema: &OutputSchema) -> SqlResult<()> {
        self.events.push(Event::Columns(
            schema.columns.iter().map(|c| c.name.clone()).collect(),
        ));
        Ok(())
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.events.push(Event::Row(row.to_vec()));
        if let Some((after, token)) = &self.cancel_after
            && self.rows().len() == *after
        {
            token.cancel();
        }
        Ok(())
    }

    fn info(&mut self, message: &InfoMessage) -> SqlResult<()> {
        self.events.push(Event::Info(message.number));
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Building the plan by hand
// ---------------------------------------------------------------------------------------

fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn bigint() -> TypeInfo {
    TypeInfo::new(SqlType::BigInt, false)
}

fn lit(value: i32) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(value)),
        ty: int(),
        line: 1,
    }
}

/// A reference to the column at position `index` of the row below, named `c<index>`.
fn col(index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(index)),
        ty: int(),
        line: 1,
    }
}

fn binding(index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(i32::try_from(index).expect("a small index") + 1),
        index,
        name: format!("c{index}"),
        ty: int(),
    }
}

fn gt(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Gt,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    }
}

fn add(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Arith {
            op: BinaryOp::Add,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: int(),
        line: 1,
    }
}

fn schema_of(names: &[&str]) -> OutputSchema {
    OutputSchema {
        columns: names
            .iter()
            .map(|name| OutputColumn {
                name: (*name).to_owned(),
                ty: int(),
            })
            .collect(),
    }
}

/// A `TableScan` reading the first `width` columns of `table`, in storage order.
fn scan(table: TableId, width: usize) -> PhysicalPlan {
    let columns: Vec<ColumnBinding> = (0..width).map(binding).collect();
    let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    PhysicalPlan::TableScan {
        table,
        columns,
        alias: "t".to_owned(),
        schema: schema_of(&names.iter().map(String::as_str).collect::<Vec<_>>()),
    }
}

fn filter(input: PhysicalPlan, predicate: BoundExpr) -> PhysicalPlan {
    PhysicalPlan::Filter {
        input: Box::new(input),
        predicate,
    }
}

fn project(input: PhysicalPlan, exprs: Vec<(&str, BoundExpr)>) -> PhysicalPlan {
    let names: Vec<&str> = exprs.iter().map(|(name, _)| *name).collect();
    PhysicalPlan::Project {
        input: Box::new(input),
        schema: schema_of(&names),
        exprs: exprs
            .into_iter()
            .map(|(name, expr)| BoundProjection {
                expr,
                name: name.to_owned(),
            })
            .collect(),
    }
}

fn top_n(n: i64) -> BoundTop {
    BoundTop {
        expr: BoundExpr {
            kind: BoundExprKind::Literal(Value::I64(n)),
            ty: bigint(),
            line: 1,
        },
        percent: false,
        with_ties: false,
    }
}

fn top(input: PhysicalPlan, n: i64) -> PhysicalPlan {
    PhysicalPlan::Top {
        input: Box::new(input),
        top: top_n(n),
    }
}

/// A `Values` of one `int` column holding `values`, one row each.
fn values(values: &[i32]) -> PhysicalPlan {
    PhysicalPlan::Values {
        rows: values.iter().map(|v| vec![lit(*v)]).collect(),
        schema: schema_of(&["v"]),
    }
}

/// The pipeline the tests share: `TableScan(c0, c1) -> Filter(c0 > 1) -> Project(c1,
/// c0 + 10) -> Top 2`.
fn pipeline(table: TableId) -> PhysicalPlan {
    top(
        project(
            filter(scan(table, 2), gt(col(0), lit(1))),
            vec![("b", col(1)), ("a_plus_ten", add(col(0), lit(10)))],
        ),
        2,
    )
}

/// The rows the pipeline is fed: `c0 > 1` keeps the last three, `Top 2` keeps two.
fn fill_pipeline_table(fixture: &Fixture) {
    for row in [[1, 10], [2, 20], [3, 30], [4, 40]] {
        fixture.insert(&row);
    }
}

// ---------------------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------------------

#[test]
fn table_scan_streams_to_the_sink() {
    let fixture = Fixture::new(1);
    for v in [1, 2, 3] {
        fixture.insert(&[v]);
    }
    let mut sink = EventSink::new();
    let outcome = fixture
        .run(&scan(fixture.table, 1), &CancelToken::never(), &mut sink)
        .expect("the scan runs");
    assert!(matches!(outcome, ExecOutcome::Rows(3)), "{outcome:?}");
    assert_eq!(
        sink.events,
        vec![
            Event::Columns(vec!["c0".to_owned()]),
            Event::Row(vec![Value::I32(1)]),
            Event::Row(vec![Value::I32(2)]),
            Event::Row(vec![Value::I32(3)]),
        ]
    );
}

#[test]
fn filter_project_limit_pipeline() {
    let fixture = Fixture::new(2);
    fill_pipeline_table(&fixture);
    let mut sink = EventSink::new();
    let outcome = fixture
        .run(&pipeline(fixture.table), &CancelToken::never(), &mut sink)
        .expect("the pipeline runs");
    assert!(matches!(outcome, ExecOutcome::Rows(2)), "{outcome:?}");
    assert_eq!(
        sink.events,
        vec![
            Event::Columns(vec!["b".to_owned(), "a_plus_ten".to_owned()]),
            Event::Row(vec![Value::I32(20), Value::I32(12)]),
            Event::Row(vec![Value::I32(30), Value::I32(13)]),
        ]
    );
}

/// A `Values` produces its rows with no storage at all: the context is the scalar one,
/// in which reading a table is the internal error 50000 (`tests/exec_shape.rs` and the
/// unit tests of the context), so the two rows below came from the plan alone.
#[test]
fn values_operator() {
    let mut sink = CollectSink::new();
    let outcome =
        run_scalar(&values(&[7, 8]), &CancelToken::never(), &mut sink).expect("the values run");
    assert!(matches!(outcome, ExecOutcome::Rows(2)), "{outcome:?}");
    assert_eq!(sink.rows, vec![vec![Value::I32(7)], vec![Value::I32(8)]]);
    let schema = sink.schema.expect("the columns were announced");
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].name, "v");
}

// ---------------------------------------------------------------------------------------
// `Top` pulls what it keeps
// ---------------------------------------------------------------------------------------

/// An operator that counts the `next` calls it forwards to its input.
struct Counter<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    calls: Rc<Cell<usize>>,
}

impl<'a> Operator<'a> for Counter<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        self.calls.set(self.calls.get() + 1);
        self.input.next(ctx)
    }

    fn close(&mut self) {
        self.input.close();
    }

    fn schema(&self) -> &OutputSchema {
        self.input.schema()
    }
}

/// Opens `root`, pulls it dry, closes it, and answers the rows it produced.
fn drain<'a>(root: &mut (dyn Operator<'a> + 'a), ctx: &mut ExecContext<'a>) -> Vec<Row> {
    root.open(ctx).expect("the tree opens");
    let mut rows = Vec::new();
    while let Some(row) = root.next(ctx).expect("a row is produced") {
        rows.push(row);
    }
    root.close();
    rows
}

#[test]
fn limit_stops_early() {
    let eval = StaticContext::default();
    let three = values(&[1, 2, 3]);

    // `Top 1` over the counter over three rows: one `next` reaches the input.
    let calls = Rc::new(Cell::new(0));
    let mut limited = Limit::new(
        Box::new(Counter {
            input: build_operator(&three).expect("the values build"),
            calls: Rc::clone(&calls),
        }),
        top_n(1),
    );
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let rows = drain(&mut limited, &mut ctx);
    assert_eq!(rows, vec![vec![Value::I32(1)]]);
    assert_eq!(calls.get(), 1, "one row kept, one row pulled");

    // The counter-proof: the same counter without a `Top` above it sees the three rows
    // and the `None` that ends them.
    let calls = Rc::new(Cell::new(0));
    let mut counter = Counter {
        input: build_operator(&three).expect("the values build"),
        calls: Rc::clone(&calls),
    };
    let rows = drain(&mut counter, &mut ctx);
    assert_eq!(rows.len(), 3);
    assert_eq!(calls.get(), 4, "three rows and the end of the input");
}

// ---------------------------------------------------------------------------------------
// The nodes whose operator is not written
// ---------------------------------------------------------------------------------------

#[test]
fn unserved_variant_is_a_bug() {
    let input = || Box::new(values(&[1]));
    let unserved: [(&str, PhysicalPlan); 4] = [
        (
            "HashJoin",
            PhysicalPlan::HashJoin {
                build: input(),
                probe: input(),
                kind: PhysicalJoinKind::Inner,
                keys: vec![(col(0), col(0))],
                residual: None,
                schema: schema_of(&["v", "v"]),
            },
        ),
        (
            "IndexSeek",
            PhysicalPlan::IndexSeek {
                index: IndexId(1),
                range: KeyRangeExpr::Full,
                columns: vec![binding(0)],
                direction: Direction::Forward,
                schema: schema_of(&["c0"]),
            },
        ),
        (
            "Sort",
            PhysicalPlan::Sort {
                input: input(),
                keys: Vec::new(),
            },
        ),
        (
            "HashAggregate",
            PhysicalPlan::HashAggregate {
                input: input(),
                group_by: Vec::new(),
                aggregates: Vec::new(),
                schema: schema_of(&[]),
            },
        ),
    ];
    for (name, plan) in &unserved {
        let error = build_operator(plan)
            .err()
            .expect("the node has no operator");
        assert_eq!(error.number, 50000, "{name}: {error:?}");
        assert!(
            error.message.contains(name) && error.message.contains("not implemented"),
            "{name}: {}",
            error.message
        );
    }
}

// ---------------------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------------------

#[test]
fn cancel_before_open_emits_no_row() {
    let fixture = Fixture::new(1);
    fixture.insert(&[1]);
    let token = CancelToken::new();
    token.cancel();
    let mut sink = EventSink::new();
    let outcome = fixture
        .run(&scan(fixture.table, 1), &token, &mut sink)
        .expect("a cancellation is not an error");
    assert!(matches!(outcome, ExecOutcome::Cancelled), "{outcome:?}");
    assert!(sink.events.is_empty(), "{:?}", sink.events);
}

/// Two thousand and forty-eight rows, the token raised by the sink on the tenth: the
/// statement answers `Cancelled` and the sink saw fewer rows than the plan holds. The
/// read of the token happens between two batches of rows, so the sink does see rows
/// past the tenth.
#[test]
fn cancel_mid_stream() {
    let token = CancelToken::new();
    let all: Vec<i32> = (0..2048).collect();
    let mut sink = EventSink::cancelling_after(10, &token);
    let outcome =
        run_scalar(&values(&all), &token, &mut sink).expect("a cancellation is not an error");
    assert!(matches!(outcome, ExecOutcome::Cancelled), "{outcome:?}");
    let seen = sink.rows().len();
    assert!(seen >= 10, "{seen}");
    assert!(seen < 2048, "{seen}");
    // The driving loop reads the token once every 1024 rows.
    assert_eq!(seen, 1024);
    // The counter-proof: the same plan with a token nobody raises streams to the end.
    let mut sink = EventSink::new();
    let outcome =
        run_scalar(&values(&all), &CancelToken::new(), &mut sink).expect("the values run");
    assert!(matches!(outcome, ExecOutcome::Rows(2048)), "{outcome:?}");
    assert_eq!(sink.rows().len(), 2048);
}

// ---------------------------------------------------------------------------------------
// The count
// ---------------------------------------------------------------------------------------

#[test]
fn row_count_is_what_the_sink_saw() {
    let fixture = Fixture::new(2);
    fill_pipeline_table(&fixture);
    let plans = [
        scan(fixture.table, 2),
        filter(scan(fixture.table, 2), gt(col(0), lit(2))),
        pipeline(fixture.table),
        top(scan(fixture.table, 2), 0),
        values(&[5, 6, 7]),
        PhysicalPlan::OneRow,
    ];
    for plan in &plans {
        let mut sink = CollectSink::new();
        let outcome = fixture
            .run(plan, &CancelToken::never(), &mut sink)
            .expect("the plan runs");
        let ExecOutcome::Rows(count) = outcome else {
            panic!("a query answers Rows, not {outcome:?}");
        };
        assert_eq!(count, sink.rows.len() as u64, "{plan:?}");
    }
    // The four table plans answer 4, 2, 2 and 0 rows, `Values` 3 and `OneRow` 1: the
    // counts differ, so the equality above is not read on one shape alone.
    let counts: Vec<u64> = plans
        .iter()
        .map(|plan| {
            let mut sink = CollectSink::new();
            match fixture.run(plan, &CancelToken::never(), &mut sink) {
                Ok(ExecOutcome::Rows(count)) => count,
                other => panic!("{other:?}"),
            }
        })
        .collect();
    assert_eq!(counts, vec![4, 2, 2, 0, 3, 1]);
}
