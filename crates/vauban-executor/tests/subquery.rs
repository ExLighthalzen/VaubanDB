//! Subquery execution: scalar, `EXISTS`, `IN`, caching and semi-joins.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, ColumnBinding, CompareOp, LockHints, OutputColumn,
    OutputSchema, SessionOptions,
};
use vauban_catalog::ColumnId;
use vauban_executor::{
    CollectSink, ExecContext, ExecOutcome, ExecSession, Row, execute, execute_collect,
};
use vauban_planner::{
    PhysicalDeclaration, PhysicalJoinKind, PhysicalPlan, PhysicalStatement, SubPlan,
};
use vauban_storage::{MemoryStorage, Row as StorageRow, Snapshot, Storage, TableId, TableShape};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo, Value};

struct Fixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    outer: TableId,
    inner: TableId,
}

fn int_ty(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

fn bit_ty() -> TypeInfo {
    TypeInfo::new(SqlType::Bit, true)
}

fn binding(table_offset: i32, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(table_offset + i32::try_from(index).expect("small index")),
        index,
        name: format!("c{index}"),
        ty: int_ty(true),
        depth: 0,
    }
}

fn col(index: usize) -> BoundExpr {
    outer_col(index)
}

fn outer_col(index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(1, index)),
        ty: int_ty(true),
        line: 7,
    }
}

fn inner_col(index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(10, index)),
        ty: int_ty(true),
        line: 7,
    }
}

fn join_inner_col(outer_width: usize, index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(10, outer_width + index)),
        ty: int_ty(true),
        line: 7,
    }
}

fn lit(n: i32) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(n)),
        ty: int_ty(false),
        line: 7,
    }
}

fn eq(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: bit_ty(),
        line: 7,
    }
}

fn exists(inner: PhysicalPlan) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Exists(Box::new(logical_plan_from_physical(&inner))),
        ty: bit_ty(),
        line: 7,
    }
}

fn scalar(inner: PhysicalPlan) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ScalarSubquery(Box::new(logical_plan_from_physical(&inner))),
        ty: int_ty(true),
        line: 7,
    }
}

fn in_subquery(tested: BoundExpr, inner: PhysicalPlan, negated: bool) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::InSubquery {
            expr: Box::new(tested),
            plan: Box::new(logical_plan_from_physical(&inner)),
            negated,
        },
        ty: bit_ty(),
        line: 7,
    }
}

fn logical_plan_from_physical(plan: &PhysicalPlan) -> vauban_binder::LogicalPlan {
    match plan {
        PhysicalPlan::TableScan {
            table,
            columns,
            alias,
            schema,
            hints,
        } => vauban_binder::LogicalPlan::Scan {
            table: *table,
            columns: columns.clone(),
            alias: alias.clone(),
            schema: schema.clone(),
            hints: *hints,
        },
        PhysicalPlan::Project {
            input,
            exprs,
            schema,
        } => vauban_binder::LogicalPlan::Project {
            input: Box::new(logical_plan_from_physical(input)),
            exprs: exprs.clone(),
            schema: schema.clone(),
        },
        PhysicalPlan::Values { rows, schema } => vauban_binder::LogicalPlan::Values {
            rows: rows.clone(),
            schema: schema.clone(),
        },
        PhysicalPlan::Filter { input, predicate } => vauban_binder::LogicalPlan::Filter {
            input: Box::new(logical_plan_from_physical(input)),
            predicate: predicate.clone(),
        },
        _ => panic!("unsupported test plan shape"),
    }
}

fn schema_of(names: &[&str], ty: TypeInfo) -> OutputSchema {
    OutputSchema {
        columns: names
            .iter()
            .map(|name| OutputColumn {
                name: (*name).to_owned(),
                ty: ty.clone(),
            })
            .collect(),
    }
}

fn scan(table: TableId, offset: i32) -> PhysicalPlan {
    PhysicalPlan::TableScan {
        table,
        columns: vec![binding(offset, 0)],
        alias: "t".to_owned(),
        schema: schema_of(&["c0"], int_ty(true)),
        hints: LockHints::default(),
    }
}

fn values_int(rows: &[i32]) -> PhysicalPlan {
    PhysicalPlan::Values {
        rows: rows
            .iter()
            .map(|v| {
                vec![BoundExpr {
                    kind: BoundExprKind::Literal(Value::I32(*v)),
                    ty: int_ty(false),
                    line: 1,
                }]
            })
            .collect(),
        schema: schema_of(&["v"], int_ty(false)),
    }
}

fn subquery_eval(input: PhysicalPlan, _expr: BoundExpr, subplans: Vec<SubPlan>) -> PhysicalPlan {
    let mut schema = input.schema().clone();
    for subplan in &subplans {
        schema.columns.push(OutputColumn {
            name: String::new(),
            ty: subplan.plan.schema().columns[0].ty.clone(),
        });
    }
    PhysicalPlan::SubqueryEval {
        input: Box::new(input),
        subplans,
        schema,
    }
}

fn subplan(plan: PhysicalPlan, correlated: bool) -> SubPlan {
    SubPlan { plan, correlated }
}

fn filter(input: PhysicalPlan, predicate: BoundExpr) -> PhysicalPlan {
    PhysicalPlan::Filter {
        input: Box::new(input),
        predicate,
    }
}

fn project(input: PhysicalPlan, expr: BoundExpr, name: &str) -> PhysicalPlan {
    let ty = expr.ty.clone();
    PhysicalPlan::Project {
        input: Box::new(input),
        exprs: vec![BoundProjection {
            expr,
            name: name.to_owned(),
        }],
        schema: schema_of(&[name], ty),
    }
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let shape = TableShape {
            columns: vec![int_ty(true)],
            clustered_key: None,
        };
        let outer = storage.create_table(db, &shape).expect("outer table");
        let inner = storage.create_table(db, &shape).expect("inner table");
        let txn = TransactionManager::new(Arc::clone(&storage));
        Self {
            storage,
            txn,
            outer,
            inner,
        }
    }

    fn insert(&self, table: TableId, values: &[i32]) {
        for v in values {
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            self.storage
                .insert(handle.id, table, &StorageRow(vec![Value::I32(*v)]))
                .expect("insert");
            self.txn.commit(handle).expect("commit");
        }
    }

    fn insert_null(&self, table: TableId) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.storage
            .insert(handle.id, table, &StorageRow(vec![Value::Null]))
            .expect("insert");
        self.txn.commit(handle).expect("commit");
    }

    fn snapshot(&self) -> Snapshot {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.txn.statement_snapshot(&handle)
    }

    fn run(&self, plan: PhysicalPlan) -> vauban_errors::SqlResult<(ExecOutcome, Vec<Row>)> {
        let eval = StaticContext::default();
        let snap = self.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            &self.txn,
            &snap,
        );
        let stmt = PhysicalStatement::Query(plan);
        let (outcome, set) = execute_collect(&stmt, &mut ctx)?;
        Ok((outcome, set.rows))
    }
}

#[test]
fn scalar_subquery_returns_the_value() {
    let f = Fixture::new();
    f.insert(f.outer, &[1]);
    f.insert(f.inner, &[42]);
    let inner = scan(f.inner, 10);
    let sq = subquery_eval(
        scan(f.outer, 1),
        scalar(inner.clone()),
        vec![subplan(inner.clone(), false)],
    );
    let plan = project(sq, scalar(inner), "v");
    let (_outcome, rows) = f.run(plan).expect("the plan runs");
    assert_eq!(rows[0][0], Value::I32(42));
}

#[test]
fn scalar_subquery_on_empty_input_is_null() {
    let f = Fixture::new();
    f.insert(f.outer, &[1]);
    let inner = scan(f.inner, 10);
    let sq = subquery_eval(
        scan(f.outer, 1),
        scalar(inner.clone()),
        vec![subplan(inner.clone(), false)],
    );
    let plan = project(sq, scalar(inner), "v");
    let (_outcome, rows) = f.run(plan).expect("the plan runs");
    assert_eq!(rows[0][0], Value::Null);
}

#[test]
fn scalar_subquery_with_two_rows_is_512() {
    let f = Fixture::new();
    f.insert(f.outer, &[1]);
    f.insert(f.inner, &[1, 2]);
    let inner = scan(f.inner, 10);
    let sq = subquery_eval(
        scan(f.outer, 1),
        scalar(inner.clone()),
        vec![subplan(inner.clone(), false)],
    );
    let plan = project(sq, scalar(inner), "v");
    let error = f.run(plan).expect_err("512");
    assert_eq!(error.number, 512);
    assert_eq!(
        error.message,
        vauban_errors::SqlError::subquery_returned_more_than_one_value().message
    );
    assert_eq!(error.line, 7);
}

#[test]
fn exists_on_empty_input_is_false() {
    let f = Fixture::new();
    f.insert(f.outer, &[1]);
    let inner = scan(f.inner, 10);
    let sq = subquery_eval(
        scan(f.outer, 1),
        exists(inner.clone()),
        vec![subplan(inner.clone(), false)],
    );
    let plan = filter(sq, exists(inner));
    let (_outcome, rows) = f.run(plan).expect("the plan runs");
    assert!(rows.is_empty());
}

#[test]
fn exists_stops_at_the_first_row() {
    let f = Fixture::new();
    let inner = values_int(&[10, 20, 30]);
    let eval = StaticContext::default();
    let snap = f.snapshot();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        f.storage.as_ref(),
        &f.txn,
        &snap,
    );
    let nexts = Rc::new(Cell::new(0));
    ctx.set_subquery_next_count(Rc::clone(&nexts));
    let exists = ctx.test_eval_exists_plan(&inner).expect("exists");
    assert_eq!(exists, Value::Bit(true));
    assert_eq!(nexts.get(), 1);

    let eval = StaticContext::default();
    let snap = f.snapshot();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        f.storage.as_ref(),
        &f.txn,
        &snap,
    );
    let nexts_in = Rc::new(Cell::new(0));
    ctx.set_subquery_next_count(Rc::clone(&nexts_in));
    let matched = ctx
        .test_eval_in_subquery_plan(&inner, &lit(99), None, 7)
        .expect("in");
    assert_eq!(matched, Value::Bit(false));
    assert!(nexts_in.get() > nexts.get());
}

#[test]
fn in_with_a_null_and_no_match_is_unknown() {
    let f = Fixture::new();
    f.insert(f.outer, &[1, 2]);
    f.insert(f.inner, &[5]);
    f.insert_null(f.inner);
    let inner = scan(f.inner, 10);
    let tested = col(0);
    let sq = subquery_eval(
        scan(f.outer, 1),
        in_subquery(tested.clone(), inner.clone(), false),
        vec![subplan(inner.clone(), false)],
    );
    let plan = filter(sq, in_subquery(tested, inner.clone(), false));
    let (_outcome, rows) = f.run(plan).expect("in filters unknown");
    assert!(rows.is_empty());

    let sq_not = subquery_eval(
        scan(f.outer, 1),
        in_subquery(col(0), inner.clone(), true),
        vec![subplan(inner.clone(), false)],
    );
    let not_plan = filter(sq_not, in_subquery(col(0), inner, true));
    let (_outcome, not_rows) = f.run(not_plan).expect("not in filters unknown");
    assert!(not_rows.is_empty());

    let g = Fixture::new();
    g.insert(g.outer, &[1, 2]);
    g.insert(g.inner, &[9]);
    let inner2 = scan(g.inner, 10);
    let sq2 = subquery_eval(
        scan(g.outer, 1),
        in_subquery(col(0), inner2.clone(), true),
        vec![subplan(inner2.clone(), false)],
    );
    let not_plan2 = filter(sq2, in_subquery(col(0), inner2, true));
    let (_outcome, kept) = g.run(not_plan2).expect("not in without null");
    assert!(!kept.is_empty());
}

#[test]
fn uncorrelated_subquery_is_evaluated_once() {
    let f = Fixture::new();
    f.insert(f.outer, &[1, 2, 3, 4, 5]);
    f.insert(f.inner, &[99]);
    let inner = scan(f.inner, 10);
    let sq = subquery_eval(
        scan(f.outer, 1),
        scalar(inner.clone()),
        vec![subplan(inner.clone(), false)],
    );
    let eval = StaticContext::default();
    let snap = f.snapshot();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        f.storage.as_ref(),
        &f.txn,
        &snap,
    );
    let opens = Rc::new(Cell::new(0));
    ctx.set_subquery_open_count(Rc::clone(&opens));
    let stmt = PhysicalStatement::Query(project(sq, scalar(inner.clone()), "v"));
    let (_outcome, set) = execute_collect(&stmt, &mut ctx).expect("uncorrelated scalar");
    assert_eq!(set.rows.len(), 5);
    assert!(set.rows.iter().all(|row| row[0] == Value::I32(99)));
    assert_eq!(opens.get(), 1);

    f.insert(f.inner, &[1, 2, 3, 4, 5]);
    let correlated_inner = filter(scan(f.inner, 10), eq(inner_col(0), outer_col(0)));
    let sq_corr = subquery_eval(
        scan(f.outer, 1),
        exists(correlated_inner.clone()),
        vec![subplan(correlated_inner.clone(), true)],
    );
    let eval = StaticContext::default();
    let snap = f.snapshot();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        f.storage.as_ref(),
        &f.txn,
        &snap,
    );
    let opens_corr = Rc::new(Cell::new(0));
    ctx.set_subquery_open_count(Rc::clone(&opens_corr));
    let stmt = PhysicalStatement::Query(filter(sq_corr, exists(correlated_inner)));
    let (_outcome, set) = execute_collect(&stmt, &mut ctx).expect("correlated exists");
    assert_eq!(set.rows.len(), 5);
    assert_eq!(opens_corr.get(), 5);
}

#[test]
fn correlated_subquery_reads_the_outer_row() {
    let f = Fixture::new();
    f.insert(f.outer, &[10, 20]);
    f.insert(f.inner, &[10]);
    f.insert(f.inner, &[20]);
    let inner = filter(scan(f.inner, 10), eq(inner_col(0), outer_col(0)));
    let sq = subquery_eval(
        scan(f.outer, 1),
        scalar(inner.clone()),
        vec![subplan(inner.clone(), true)],
    );
    let plan = project(sq, scalar(inner), "v");
    let (_outcome, rows) = f.run(plan).expect("correlated scalar");
    assert_eq!(rows[0][0], Value::I32(10));
    assert_eq!(rows[1][0], Value::I32(20));
}

#[test]
fn semi_join_emits_the_outer_row_once() {
    let f = Fixture::new();
    f.insert(f.outer, &[1]);
    f.insert(f.inner, &[1, 1]);
    let outer_width = 1;
    let join = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan(f.outer, 1)),
        inner: Box::new(scan(f.inner, 10)),
        kind: PhysicalJoinKind::Semi,
        on: Some(eq(outer_col(0), join_inner_col(outer_width, 0))),
        schema: schema_of(&["c0"], int_ty(true)),
    };
    let (_outcome, rows) = f.run(join).expect("semi join");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Value::I32(1)]);

    let anti = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan(f.outer, 1)),
        inner: Box::new(scan(f.inner, 10)),
        kind: PhysicalJoinKind::AntiSemi,
        on: Some(eq(outer_col(0), lit(99))),
        schema: schema_of(&["c0"], int_ty(true)),
    };
    let (_outcome, anti_rows) = f.run(anti).expect("anti semi join");
    assert_eq!(anti_rows.len(), 1);
}

/// `SELECT o.c, i.c FROM o CROSS JOIN i WHERE EXISTS (SELECT 1 FROM t WHERE t.c = o.c)`:
/// the correlated subquery in the `WHERE` reads the outer side of the cross join, and the
/// rows that pass still carry the inner side of the concatenated row.
#[test]
fn correlated_exists_over_a_cross_join_keeps_both_sides() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage
        .create_database("mydb")
        .expect("the database is new");
    let shape = TableShape {
        columns: vec![int_ty(true)],
        clustered_key: None,
    };
    let outer_table = storage.create_table(db, &shape).expect("outer");
    let inner_table = storage.create_table(db, &shape).expect("inner");
    let third_table = storage.create_table(db, &shape).expect("third");
    let txn = TransactionManager::new(Arc::clone(&storage));
    let insert = |table: TableId, value: i32| {
        let h = txn.begin(IsolationLevel::ReadCommitted);
        storage
            .insert(h.id, table, &StorageRow(vec![Value::I32(value)]))
            .expect("insert");
        txn.commit(h).expect("commit");
    };
    insert(outer_table, 1);
    insert(outer_table, 2);
    insert(inner_table, 10);
    insert(inner_table, 20);
    insert(third_table, 2);
    insert(third_table, 20);

    let cross = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan(outer_table, 1)),
        inner: Box::new(scan(inner_table, 10)),
        kind: PhysicalJoinKind::Cross,
        on: None,
        schema: schema_of(&["c0", "c0"], int_ty(true)),
    };
    let third = BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(30, 0)),
        ty: int_ty(true),
        line: 7,
    };
    let subquery = filter(scan(third_table, 30), eq(third, outer_col(0)));
    let plan = filter(
        subquery_eval(
            cross,
            exists(subquery.clone()),
            vec![subplan(subquery.clone(), true)],
        ),
        exists(subquery),
    );

    let eval = StaticContext::default();
    let snap = txn.statement_snapshot(&txn.begin(IsolationLevel::ReadCommitted));
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        storage.as_ref(),
        &txn,
        &snap,
    );
    let (_, set) = execute_collect(&PhysicalStatement::Query(plan), &mut ctx)
        .expect("the correlated exists over the cross join runs");
    let pairs: Vec<(Value, Value)> = set
        .rows
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            (Value::I32(2), Value::I32(10)),
            (Value::I32(2), Value::I32(20)),
        ],
        "the outer row the third table holds pairs with each inner row"
    );
}

// ---------------------------------------------------------------------------------------
// SQL text through the whole chain: parse, bind, plan, execute
// ---------------------------------------------------------------------------------------

/// A bootstrapped catalogue over an in-memory storage, for tests written as SQL text.
struct SqlFixture {
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
    catalog: vauban_catalog::Catalog,
}

impl SqlFixture {
    fn new() -> Self {
        vauban_sysfn::register_builtins();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = vauban_catalog::Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
            .expect("the catalogue boots on a fresh storage");
        Self {
            storage,
            txn,
            catalog,
        }
    }

    /// Parses, binds, plans and executes each statement of `text`, each in a transaction
    /// of its own, and hands back the rows of the last one.
    fn run(&self, text: &str) -> Vec<Row> {
        let batch = vauban_parser::parse_batch(text, &vauban_parser::ParseOptions::default())
            .expect("the text parses");
        let mut last = Vec::new();
        for statement in &batch.statements {
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            let snapshot = self.catalog.snapshot(&handle);
            let mut bind_ctx = vauban_binder::BindContext::scalar(text, SessionOptions::default());
            bind_ctx.catalog = Some(&snapshot);
            let bound = vauban_binder::bind(statement, &bind_ctx).expect("the statement binds");
            let planned = vauban_planner::plan(
                bound,
                &vauban_planner::PlanContext {
                    catalog: &vauban_planner::NoIndexes,
                },
            )
            .expect("the statement plans");
            let snap = self.txn.statement_snapshot(&handle);
            let eval = StaticContext::default();
            let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
                .with_engine(self.storage.as_ref(), &self.txn, &snap)
                .with_catalog(&self.catalog)
                .with_handle(&handle);
            let (_, set) = execute_collect(&planned, &mut ctx).expect("the statement runs");
            self.txn.commit(handle).expect("the transaction commits");
            last = set.rows;
        }
        last
    }
}

fn i32s(rows: &[&[i32]]) -> Vec<Row> {
    rows.iter()
        .map(|row| row.iter().map(|v| Value::I32(*v)).collect())
        .collect()
}

fn seeded() -> SqlFixture {
    let f = SqlFixture::new();
    for ddl in [
        "CREATE TABLE t1 (a int);",
        "CREATE TABLE t2 (a int);",
        "CREATE TABLE t3 (k int, v int);",
        "CREATE TABLE t (k int);",
        "CREATE TABLE i (k int);",
        "INSERT INTO t1 VALUES (1), (2), (3);",
        "INSERT INTO t2 VALUES (1), (2);",
        "INSERT INTO t3 VALUES (1, 10), (2, 20), (3, 30);",
        "INSERT INTO t VALUES (1), (2);",
        "INSERT INTO i VALUES (1), (3);",
    ] {
        f.run(ddl);
    }
    f
}

/// `x.a` and `z.a` are the same column of the same table under two aliases: the
/// subquery counts the rows of `z` above the row of `x`, not above itself.
#[test]
fn correlated_scalar_over_the_same_table_reads_the_enclosing_row() {
    let f = seeded();
    let rows =
        f.run("SELECT x.a, (SELECT COUNT(*) FROM t1 z WHERE z.a > x.a) FROM t1 x ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1, 2], &[2, 1], &[3, 0]]));
}

/// The inner `t t` hides the outer `t`; `i.k` is the column of the enclosing row.
#[test]
fn correlated_exists_over_the_same_table_under_a_cross_join() {
    let f = seeded();
    let rows = f.run("SELECT t.k, i.k FROM t CROSS JOIN i WHERE EXISTS (SELECT 1 FROM t t WHERE t.k = i.k) ORDER BY t.k, i.k;");
    assert_eq!(rows, i32s(&[&[1, 1], &[2, 1]]));
}

/// The outer reference names the second alias of a cross self join.
#[test]
fn correlated_scalar_reads_the_second_alias_of_a_cross_self_join() {
    let f = seeded();
    let rows = f.run("SELECT x.a, y.a, (SELECT COUNT(*) FROM t3 w WHERE w.k > y.a) FROM t1 x CROSS JOIN t1 y ORDER BY x.a, y.a;");
    assert_eq!(
        rows,
        i32s(&[
            &[1, 1, 2],
            &[1, 2, 1],
            &[1, 3, 0],
            &[2, 1, 2],
            &[2, 2, 1],
            &[2, 3, 0],
            &[3, 1, 2],
            &[3, 2, 1],
            &[3, 3, 0]
        ])
    );
}

/// Two tables whose first column carries the same identifier and name: the outer
/// reference is the second one.
#[test]
fn correlated_scalar_reads_the_second_table_with_the_same_column_id() {
    let f = seeded();
    let rows = f.run("SELECT x.a, o.a, (SELECT w.v FROM t3 w WHERE w.k = o.a) FROM t1 x CROSS JOIN t2 o ORDER BY x.a, o.a;");
    assert_eq!(
        rows,
        i32s(&[
            &[1, 1, 10],
            &[1, 2, 20],
            &[2, 1, 10],
            &[2, 2, 20],
            &[3, 1, 10],
            &[3, 2, 20]
        ])
    );
}

/// A cross self join inside a correlated subquery keeps `p` and `q` apart.
#[test]
fn cross_self_join_inside_a_correlated_scalar_keeps_both_aliases() {
    let f = seeded();
    let rows = f.run("SELECT x.a, (SELECT COUNT(*) FROM t1 p CROSS JOIN t1 q WHERE p.a < q.a AND q.a >= x.a) FROM t1 x ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1, 3], &[2, 3], &[3, 2]]));
}

/// Two levels: `y.a` reads the middle query, `x.a` the outermost one.
#[test]
fn correlated_exists_two_levels_deep_over_the_same_table() {
    let f = seeded();
    let rows = f.run("SELECT x.a FROM t1 x WHERE EXISTS (SELECT 1 FROM t1 y WHERE y.a = x.a AND EXISTS (SELECT 1 FROM t1 z WHERE z.a > y.a)) ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1], &[2]]));
}

/// `IN` over the same table with an outer reference in its `WHERE`.
#[test]
fn correlated_in_over_the_same_table_reads_the_enclosing_row() {
    let f = seeded();
    let rows = f.run("SELECT x.a FROM t1 x WHERE x.a IN (SELECT z.a + 1 FROM t1 z WHERE z.a < x.a) ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[2], &[3]]));
}

/// Two tables whose first column carries the same identifier under two names, crossed
/// inside an uncorrelated scalar subquery: `q.k` is a column of the subquery, not of the
/// enclosing row.
#[test]
fn uncorrelated_cross_join_of_two_tables_inside_a_scalar_counts_its_pairs() {
    let f = seeded();
    let rows = f.run("SELECT x.a, (SELECT COUNT(*) FROM t1 p CROSS JOIN t3 q WHERE p.a < q.k) FROM t1 x ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1, 3], &[2, 3], &[3, 3]]));
}

/// The same cross join with an outer reference on the second table.
#[test]
fn cross_join_of_two_tables_inside_a_correlated_scalar_reads_each_side() {
    let f = seeded();
    let rows = f.run("SELECT x.a, (SELECT COUNT(*) FROM t1 p CROSS JOIN t3 q WHERE q.k >= x.a) FROM t1 x ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1, 9], &[2, 6], &[3, 3]]));
    let rows = f.run("SELECT x.a, (SELECT COUNT(*) FROM t1 p CROSS JOIN t3 q WHERE p.a < q.k AND q.v >= x.a) FROM t1 x ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1, 3], &[2, 3], &[3, 3]]));
}

/// A reference two queries up while the query in between holds no outer reference of
/// its own: the middle evaluation still counts as one level.
#[test]
fn a_reference_two_queries_up_skips_a_middle_query_without_one() {
    let f = seeded();
    let rows = f.run("SELECT x.a FROM t1 x WHERE EXISTS (SELECT 1 FROM t1 y WHERE EXISTS (SELECT 1 FROM t1 z WHERE z.a = x.a AND y.a = 2)) ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1], &[2], &[3]]));
}

/// An `EXISTS` that stops at its first row leaves the `SubqueryEval` of its plan
/// unexhausted: the second `EXISTS` of the predicate still takes its own plan.
#[test]
fn a_second_subquery_after_a_nested_exists_takes_its_own_plan() {
    let f = seeded();
    let rows = f.run("SELECT x.a FROM t1 x WHERE EXISTS (SELECT 1 FROM t1 y WHERE y.a = x.a AND EXISTS (SELECT 1 FROM t1 z WHERE z.a > y.a)) AND EXISTS (SELECT 1 FROM t1 w WHERE w.a = x.a + 1) ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1], &[2]]));
}

/// A derived table crossed with a table inside a correlated scalar: the derived
/// column and the outer column are told apart.
#[test]
fn derived_table_crossed_inside_a_correlated_scalar_reads_the_enclosing_row() {
    let f = seeded();
    let rows = f.run("SELECT x.a, (SELECT MAX(d.a) FROM t1 p CROSS JOIN (SELECT a FROM t1) d WHERE d.a < x.a AND p.a = x.a) FROM t1 x ORDER BY x.a;");
    assert_eq!(
        rows,
        vec![
            vec![Value::I32(1), Value::Null],
            vec![Value::I32(2), Value::I32(1)],
            vec![Value::I32(3), Value::I32(2)],
        ]
    );
}

/// A hash join's row is laid out probe-then-build: a scalar subquery correlated to the
/// build side reads the right value, not zero on every row.
#[test]
fn hash_join_row_read_by_a_correlated_scalar_on_the_build_side() {
    let f = seeded();
    let rows = f.run("SELECT x.a, w.v, (SELECT COUNT(*) FROM t3 q WHERE q.v < w.v) AS n FROM t1 x JOIN t3 w ON x.a = w.k ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1, 10, 0], &[2, 20, 1], &[3, 30, 2]]));
}

/// Same hash join, the correlated scalar reads the probe side instead.
#[test]
fn hash_join_row_read_by_a_correlated_scalar_on_the_probe_side() {
    let f = seeded();
    let rows = f.run("SELECT x.a, w.v, (SELECT COUNT(*) FROM t3 q WHERE q.k > x.a) AS n FROM t1 x JOIN t3 w ON x.a = w.k ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[1, 10, 2], &[2, 20, 1], &[3, 30, 0]]));
}

/// A correlated `EXISTS` reads the build side of the enclosing hash join.
#[test]
fn hash_join_row_read_by_a_correlated_exists_on_the_build_side() {
    let f = seeded();
    let rows = f.run("SELECT x.a, w.v FROM t1 x JOIN t3 w ON x.a = w.k WHERE EXISTS (SELECT 1 FROM t3 q WHERE q.v = w.v AND q.k = 2) ORDER BY x.a;");
    assert_eq!(rows, i32s(&[&[2, 20]]));
}

/// A hash join built and probed **inside** a correlated scalar subquery: its own row
/// layout must not be swapped either.
#[test]
fn hash_join_inside_a_correlated_scalar_filters_on_its_build_side() {
    let f = seeded();
    let rows = f.run(
        "SELECT x.a, (SELECT COUNT(*) FROM t1 p JOIN t3 q ON p.a = q.k WHERE q.v > x.a * 10) AS n FROM t1 x ORDER BY x.a;",
    );
    assert_eq!(rows, i32s(&[&[1, 2], &[2, 1], &[3, 0]]));
}

/// Same inner hash join, the correlated scalar projects the build side instead of
/// filtering on it.
#[test]
fn hash_join_inside_a_correlated_scalar_projects_its_build_side() {
    let f = seeded();
    let rows = f.run(
        "SELECT x.a, (SELECT MAX(q.v) FROM t1 p JOIN t3 q ON p.a = q.k WHERE p.a <= x.a) AS m FROM t1 x ORDER BY x.a;",
    );
    assert_eq!(rows, i32s(&[&[1, 10], &[2, 20], &[3, 30]]));
}

/// A derived table's own column, read by a scalar subquery that encloses it: the
/// reference is at depth 1 but still carries `SLOT_COLUMN_ID`, and must be read from the
/// enclosing row, not the local one.
#[test]
fn derived_table_column_read_from_an_enclosing_scalar() {
    let f = seeded();
    let rows = f.run(
        "SELECT d.a, (SELECT COUNT(*) FROM t1 z WHERE z.a < d.a) AS m FROM (SELECT x.a FROM t1 x) d ORDER BY d.a;",
    );
    assert_eq!(rows, i32s(&[&[1, 0], &[2, 1], &[3, 2]]));
}

/// Same derived table, read from an enclosing `EXISTS` instead of a scalar subquery.
#[test]
fn derived_table_column_read_from_an_enclosing_exists() {
    let f = seeded();
    let rows =
        f.run("SELECT d.a FROM (SELECT x.a FROM t1 x) d WHERE EXISTS (SELECT 1 FROM t2 o WHERE o.a = d.a) ORDER BY d.a;");
    assert_eq!(rows, i32s(&[&[1], &[2]]));
}

/// Two levels down: the innermost `EXISTS` reads the derived table's column two queries
/// up.
#[test]
fn derived_table_column_read_two_levels_down() {
    let f = seeded();
    let rows = f.run(
        "SELECT d.a FROM (SELECT x.a FROM t1 x) d WHERE EXISTS (SELECT 1 FROM t1 y WHERE y.a = d.a AND EXISTS (SELECT 1 FROM t1 z WHERE z.a > y.a AND z.a > d.a)) ORDER BY d.a;",
    );
    assert_eq!(rows, i32s(&[&[1], &[2]]));
}

/// The derived table sits under a join: its `SLOT_COLUMN_ID` column keeps the position
/// the join gave it, and a correlated scalar still reads the enclosing row through it.
#[test]
fn derived_table_under_a_join_column_read_from_an_enclosing_scalar() {
    let f = seeded();
    let rows = f.run(
        "SELECT d.a, o.a, (SELECT COUNT(*) FROM t1 z WHERE z.a < d.a) AS m FROM (SELECT x.a FROM t1 x) d CROSS JOIN t2 o ORDER BY d.a, o.a;",
    );
    assert_eq!(
        rows,
        i32s(&[
            &[1, 1, 0],
            &[1, 2, 0],
            &[2, 1, 1],
            &[2, 2, 1],
            &[3, 1, 2],
            &[3, 2, 2]
        ])
    );
}

/// A derived table whose own projection reorders its source's columns, joined on its
/// second column: the join key must read the position the derived table gives it, not
/// the position its source has underneath.
#[test]
fn hash_join_key_on_a_derived_table_that_reorders_its_source() {
    let f = seeded();
    let rows =
        f.run("SELECT d.k, d.dv FROM (SELECT q.v AS dv, q.k FROM t3 q) d JOIN t1 w ON d.k = w.a ORDER BY d.k;");
    assert_eq!(rows, i32s(&[&[1, 10], &[2, 20], &[3, 30]]));
}

// ---------------------------------------------------------------------------------------
// Subqueries written outside a plan: the value of a `SET`, the initializer of a `DECLARE`
// ---------------------------------------------------------------------------------------

/// `SET @x = (SELECT v FROM (VALUES …) AS t(v))`, the plan of the subquery carried by the
/// statement as the planner attaches it.
fn set_from_subquery(rows: &[i32]) -> PhysicalStatement {
    let inner = values_int(rows);
    PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: scalar(inner.clone()),
        subplans: vec![subplan(inner, false)],
    }
}

/// `DECLARE @x int = (SELECT v FROM (VALUES …) AS t(v))`, the same attachment.
fn declare_from_subquery(rows: &[i32]) -> PhysicalStatement {
    let inner = values_int(rows);
    PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@x".to_owned(),
        ty: int_ty(true),
        value: Some(scalar(inner.clone())),
        subplans: vec![subplan(inner, false)],
    }])
}

/// A session where `@x` is an `int` holding `NULL`, as a `DECLARE` leaves it.
fn session_with_x() -> ExecSession {
    let mut session = ExecSession::default();
    session.variables.insert("@x".to_owned(), Value::Null);
    session.variable_types.insert("@x".to_owned(), int_ty(true));
    session
}

fn run_statement(
    session: &mut ExecSession,
    stmt: &PhysicalStatement,
) -> vauban_errors::SqlResult<ExecOutcome> {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(session);
    let mut sink = CollectSink::new();
    execute(stmt, &mut ctx, &mut sink)
}

/// A subquery over no row assigns `NULL`.
#[test]
fn a_set_from_an_empty_subquery_is_null() {
    let mut session = session_with_x();
    run_statement(&mut session, &set_from_subquery(&[])).expect("the SET runs");
    assert_eq!(session.variables.get("@x"), Some(&Value::Null));
}

/// A subquery over one row assigns the value of its single column.
#[test]
fn a_set_from_a_subquery_of_one_row_takes_its_value() {
    let mut session = session_with_x();
    run_statement(&mut session, &set_from_subquery(&[42])).expect("the SET runs");
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(42)));
}

/// A subquery over two rows answers 512 on the line of the expression, and the variable
/// keeps the value it had.
#[test]
fn a_set_whose_subquery_returns_two_rows_answers_512() {
    let mut session = session_with_x();
    let err = run_statement(&mut session, &set_from_subquery(&[1, 2])).expect_err("512");
    assert_eq!(
        (err.number, err.severity, err.state, err.line),
        (512, 16, 1, 7)
    );
    assert_eq!(session.variables.get("@x"), Some(&Value::Null));
}

/// The initializer of a `DECLARE` reads its subquery the same way.
#[test]
fn a_declare_from_a_subquery_of_one_row_takes_its_value() {
    let mut session = ExecSession::default();
    run_statement(&mut session, &declare_from_subquery(&[7])).expect("the DECLARE runs");
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(7)));
}

/// The initializer of a `DECLARE` over two rows answers 512 as the value of a `SET` does,
/// and the variable it declares is entered at `NULL`, for the statements that follow.
#[test]
fn a_declare_whose_subquery_returns_two_rows_answers_512() {
    let mut session = ExecSession::default();
    let err = run_statement(&mut session, &declare_from_subquery(&[1, 2])).expect_err("512");
    assert_eq!(
        (err.number, err.severity, err.state, err.line),
        (512, 16, 1, 7)
    );
    assert_eq!(session.variables.get("@x"), Some(&Value::Null));
}

/// The plans are installed for one evaluation and dropped with it: running the same `SET`
/// twice opens its subquery plan twice, instead of reading what the first run cached.
#[test]
fn a_set_run_twice_evaluates_its_subquery_twice() {
    let mut session = session_with_x();
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
    let opens = Rc::new(Cell::new(0));
    ctx.set_subquery_open_count(Rc::clone(&opens));
    let mut sink = CollectSink::new();
    let stmt = set_from_subquery(&[9]);
    execute(&stmt, &mut ctx, &mut sink).expect("the first SET runs");
    execute(&stmt, &mut ctx, &mut sink).expect("the second SET runs");
    assert_eq!(opens.get(), 2);
}

/// `SET @x = EXISTS (…)` and `SET @x = 2 IN (SELECT …)`: the other two kinds of expression
/// that hold a plan travel on the statement the same way as a scalar subquery.
#[test]
fn a_set_reads_an_exists_and_an_in_subquery_in_the_same_position() {
    let mut session = session_with_x();
    let rows = values_int(&[1, 2]);
    let exists_value = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: exists(rows.clone()),
        subplans: vec![subplan(rows.clone(), false)],
    };
    run_statement(&mut session, &exists_value).expect("the EXISTS runs");
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(1)));

    let in_value = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: in_subquery(lit(2), rows.clone(), false),
        subplans: vec![subplan(rows, false)],
    };
    run_statement(&mut session, &in_value).expect("the IN runs");
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(1)));
}
