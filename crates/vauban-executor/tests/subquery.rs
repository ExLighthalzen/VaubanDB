//! Subquery execution: scalar, `EXISTS`, `IN`, caching and semi-joins.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, ColumnBinding, CompareOp, LockHints, OutputColumn,
    OutputSchema, SessionOptions,
};
use vauban_catalog::ColumnId;
use vauban_executor::{ExecContext, ExecOutcome, Row, execute_collect};
use vauban_planner::{PhysicalJoinKind, PhysicalPlan, PhysicalStatement, SubPlan};
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
