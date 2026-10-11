//! Subquery planning: derived-table flattening, decorrelated semi-joins, and
//! [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval) for correlated forms.
//!
//! Each test builds a `LogicalPlan` by hand and plans it against a `FakeCatalog`.

use vauban_planner::{
    PhysicalJoinKind, PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext, SubPlan, plan,
    testing::FakeCatalog,
};

use vauban_binder::{
    AggregateCall, BoundCaseArm, BoundDeclaration, BoundExpr, BoundExprKind, BoundProjection,
    BoundStatement, BoundSubquery, BoundTop, ColumnBinding, CompareOp, DeletePlan, JoinKind,
    LockHints, LogicalOp, LogicalPlan, OutputColumn, OutputSchema, SetOpKind, SortKey,
};
use vauban_catalog::ColumnId;
use vauban_storage::{KeyColumn, TableId};
use vauban_sysfn::{Arity, EvalArgs, EvalContext, FunctionDef, FunctionKind};
use vauban_types::{BinaryOp, SqlType, TypeInfo, Value};

const TABLE_A: TableId = TableId(1);
const TABLE_B: TableId = TableId(2);
const TABLE_INDEXED: TableId = TableId(3);

fn int_type() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

fn bit_type() -> TypeInfo {
    TypeInfo::new(SqlType::Bit, false)
}

fn binding_a(name: &str, column_id: i32, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(column_id),
        index,
        name: name.to_owned(),
        ty: int_type(),
        depth: 0,
    }
}

/// A reference to a column of `a` written inside a subquery of the query over `a`: one
/// query up.
fn outer_a(name: &str, column_id: i32, index: usize) -> ColumnBinding {
    ColumnBinding {
        depth: 1,
        ..binding_a(name, column_id, index)
    }
}

fn binding_b(name: &str, column_id: i32, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(column_id),
        index,
        name: name.to_owned(),
        ty: int_type(),
        depth: 0,
    }
}

fn column(binding: ColumnBinding) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding),
        ty: int_type(),
        line: 1,
    }
}

fn literal(n: i32) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(n)),
        ty: int_type(),
        line: 1,
    }
}

fn compare(op: CompareOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: bit_type(),
        line: 1,
    }
}

fn not(inner: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Not(Box::new(inner)),
        ty: bit_type(),
        line: 1,
    }
}

fn logical(op: LogicalOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Logical {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: bit_type(),
        line: 1,
    }
}

fn and(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    logical(LogicalOp::And, left, right)
}

fn assert_correlated_exists(inner: LogicalPlan) {
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: exists(inner),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::Filter { input, .. } = &planned else {
        panic!("expected Filter over SubqueryEval, got {planned:?}");
    };
    let PhysicalPlan::SubqueryEval { subplans, .. } = input.as_ref() else {
        panic!("expected SubqueryEval, got {input:?}");
    };
    assert_eq!(subplans.len(), 1);
    assert!(
        subplans[0].correlated,
        "expected correlated: true, got {planned:?}"
    );
}

fn assert_uncorrelated_exists(inner: LogicalPlan) {
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: exists(inner),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    assert!(
        matches!(
            planned,
            PhysicalPlan::NestedLoopJoin {
                kind: PhysicalJoinKind::Semi,
                ..
            }
        ),
        "expected Semi join, got {planned:?}"
    );
}

fn test_aggregate_return_type(_args: &[TypeInfo]) -> vauban_errors::SqlResult<TypeInfo> {
    Ok(int_type())
}

fn test_aggregate_eval(
    _args: &EvalArgs<'_>,
    _ctx: &dyn EvalContext,
) -> vauban_errors::SqlResult<Value> {
    Ok(Value::Null)
}

/// A stub aggregate definition: the shape under test is the plan, not the function.
static TEST_AGGREGATE: FunctionDef = FunctionDef {
    name: "TEST_MAX",
    kind: FunctionKind::Aggregate,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: test_aggregate_return_type,
    eval: test_aggregate_eval,
    aggregate: None,
};

fn max_on(binding: ColumnBinding) -> AggregateCall {
    AggregateCall {
        def: &TEST_AGGREGATE,
        arg: Some(column(binding)),
        distinct: false,
    }
}

fn schema(names: &[&str]) -> OutputSchema {
    OutputSchema {
        columns: names
            .iter()
            .map(|name| OutputColumn {
                name: (*name).to_owned(),
                ty: int_type(),
            })
            .collect(),
    }
}

fn scan_a() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE_A,
        columns: vec![binding_a("k", 1, 0), binding_a("c", 2, 1)],
        alias: "a".to_owned(),
        schema: schema(&["k", "c"]),
        hints: LockHints::default(),
    }
}

fn scan_b() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE_B,
        columns: vec![binding_b("k", 11, 0), binding_b("c", 12, 1)],
        alias: "b".to_owned(),
        schema: schema(&["k", "c"]),
        hints: LockHints::default(),
    }
}

fn scan_indexed() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE_INDEXED,
        columns: vec![binding_b("k", 21, 0)],
        alias: "b".to_owned(),
        schema: schema(&["k"]),
        hints: LockHints::default(),
    }
}

fn plan_with(catalog: &dyn PlanCatalog, logical: LogicalPlan) -> PhysicalPlan {
    let ctx = PlanContext { catalog };
    match plan(BoundStatement::Query(Box::new(logical)), &ctx) {
        Ok(PhysicalStatement::Query(physical)) => physical,
        Ok(other) => panic!("expected a query, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

fn exists(inner: LogicalPlan) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Exists(BoundSubquery::new(inner)),
        ty: bit_type(),
        line: 1,
    }
}

fn scalar(inner: LogicalPlan) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ScalarSubquery(BoundSubquery::new(inner)),
        ty: int_type(),
        line: 1,
    }
}

fn in_subquery(tested: BoundExpr, inner: LogicalPlan, negated: bool) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::InSubquery {
            expr: Box::new(tested),
            plan: BoundSubquery::new(inner),
            negated,
        },
        ty: bit_type(),
        line: 1,
    }
}

fn project_one(input: LogicalPlan, binding: ColumnBinding, name: &str) -> LogicalPlan {
    LogicalPlan::Project {
        input: Box::new(input),
        exprs: vec![BoundProjection {
            expr: column(binding),
            name: name.to_owned(),
        }],
        schema: schema(&[name]),
    }
}

fn project_scan(plan: LogicalPlan) -> LogicalPlan {
    let LogicalPlan::Scan { columns, .. } = &plan else {
        panic!("project_scan expects a Scan");
    };
    let columns = columns.clone();
    let schema = plan.schema().clone();
    LogicalPlan::Project {
        input: Box::new(plan),
        exprs: columns
            .into_iter()
            .map(|binding| BoundProjection {
                expr: column(binding.clone()),
                name: binding.name,
            })
            .collect(),
        schema,
    }
}

fn inner_exists_scan() -> LogicalPlan {
    project_scan(scan_b())
}

#[test]
fn a_derived_table_is_flattened() {
    let derived = LogicalPlan::Subquery {
        input: Box::new(project_scan(scan_a())),
        alias: "d".to_owned(),
        schema: schema(&["k", "c"]),
    };
    let planned = plan_with(&FakeCatalog::new(), derived);
    let PhysicalPlan::Project { input, .. } = &planned else {
        panic!("expected Project, got {planned:?}");
    };
    let PhysicalPlan::TableScan { .. } = input.as_ref() else {
        panic!("expected TableScan under Project, got {input:?}");
    };
}

#[test]
fn uncorrelated_exists_becomes_a_semi_join() {
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: exists(inner_exists_scan()),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::NestedLoopJoin { kind, on, .. } = &planned else {
        panic!("expected NestedLoopJoin, got {planned:?}");
    };
    assert_eq!(*kind, PhysicalJoinKind::Semi);
    assert!(on.is_none());
    assert!(
        !matches!(planned, PhysicalPlan::Filter { .. }),
        "expected no Filter, got {planned:?}"
    );
}

#[test]
fn uncorrelated_not_exists_becomes_an_anti_semi_join() {
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: not(exists(inner_exists_scan())),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::NestedLoopJoin { kind, on, .. } = &planned else {
        panic!("expected NestedLoopJoin, got {planned:?}");
    };
    assert_eq!(*kind, PhysicalJoinKind::AntiSemi);
    assert!(on.is_none());
}

#[test]
fn uncorrelated_in_carries_the_equality() {
    let inner = project_one(scan_b(), binding_b("k", 11, 0), "k");
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: in_subquery(column(binding_a("k", 1, 0)), inner, false),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::NestedLoopJoin { kind, on, .. } = &planned else {
        panic!("expected NestedLoopJoin, got {planned:?}");
    };
    assert_eq!(*kind, PhysicalJoinKind::Semi);
    let Some(on) = on else {
        panic!("expected an ON predicate");
    };
    let BoundExprKind::Compare {
        op: CompareOp::Eq,
        left,
        right,
    } = &on.kind
    else {
        panic!("expected equality ON, got {on:?}");
    };
    assert!(matches!(left.kind, BoundExprKind::ColumnRef(_)));
    assert!(matches!(right.kind, BoundExprKind::ColumnRef(_)));
}

/// `a.k IN (SELECT b.c FROM b)`: `b.c` is the second column of `b`, but the inner plan
/// produces it as its only column, so the equality reads it right after the two columns of
/// `a`, at index 2, and not at the index `b.c` has in the row of `b` shifted past `a`.
#[test]
fn in_subquery_compares_the_column_the_inner_plan_produces() {
    let inner = project_one(scan_b(), binding_b("c", 12, 1), "c");
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: in_subquery(column(binding_a("k", 1, 0)), inner, false),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::NestedLoopJoin {
        kind,
        on: Some(on),
        inner,
        ..
    } = &planned
    else {
        panic!("expected a semi-join with an ON, got {planned:?}");
    };
    assert_eq!(*kind, PhysicalJoinKind::Semi);
    assert_eq!(inner.schema().columns.len(), 1);
    let BoundExprKind::Compare { left, right, .. } = &on.kind else {
        panic!("expected equality ON, got {on:?}");
    };
    let (BoundExprKind::ColumnRef(tested), BoundExprKind::ColumnRef(produced)) =
        (&left.kind, &right.kind)
    else {
        panic!("expected two column references, got {on:?}");
    };
    assert_eq!((tested.index, tested.depth), (0, 0));
    assert_eq!((produced.index, produced.depth), (2, 0));
}

/// `b.c / 10`, computed by the inner plan over the row of `b`.
fn c_over_ten() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Arith {
            op: BinaryOp::Div,
            left: Box::new(column(binding_b("c", 12, 1))),
            right: Box::new(literal(10)),
        },
        ty: int_type(),
        line: 1,
    }
}

/// The position of the column the equality of a decorrelated `IN` reads on the inner side,
/// with the inner plan it reads it from.
fn produced_position(planned: &PhysicalPlan) -> (usize, &PhysicalPlan) {
    let PhysicalPlan::NestedLoopJoin {
        kind: PhysicalJoinKind::Semi,
        on: Some(on),
        inner,
        ..
    } = planned
    else {
        panic!("expected a semi-join with an ON, got {planned:?}");
    };
    let BoundExprKind::Compare { right, .. } = &on.kind else {
        panic!("expected equality ON, got {on:?}");
    };
    let BoundExprKind::ColumnRef(produced) = &right.kind else {
        panic!("expected a column reference, got {right:?}");
    };
    assert_eq!(produced.depth, 0);
    (produced.index, inner)
}

/// `a.k IN (SELECT DISTINCT b.c / 10 AS q FROM b)`: the computed, renamed column under
/// `DISTINCT` is the one column of the inner plan, read right after the two columns of `a`.
#[test]
fn in_subquery_reads_a_computed_column_under_distinct_after_the_outer_columns() {
    let inner = LogicalPlan::Distinct(Box::new(LogicalPlan::Project {
        input: Box::new(scan_b()),
        exprs: vec![BoundProjection {
            expr: c_over_ten(),
            name: "q".to_owned(),
        }],
        schema: schema(&["q"]),
    }));
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: in_subquery(column(binding_a("k", 1, 0)), inner, false),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let (index, inner) = produced_position(&planned);
    assert_eq!(index, 2);
    assert_eq!(inner.schema().columns.len(), 1);
    assert_eq!(inner.schema().columns[0].name, "q");
}

/// `DELETE FROM a WHERE a.k IN (SELECT b.c / 10 FROM b)`: the rows of `a` carry their row
/// identifier after their visible values, and the joined row keeps it after the inner
/// column, so the equality still reads the inner column at index 2.
#[test]
fn in_subquery_under_a_delete_reads_past_the_visible_columns_alone() {
    let inner = LogicalPlan::Project {
        input: Box::new(scan_b()),
        exprs: vec![BoundProjection {
            expr: c_over_ten(),
            name: String::new(),
        }],
        schema: schema(&[""]),
    };
    let stmt = BoundStatement::Delete(DeletePlan {
        table: TABLE_A,
        input: Box::new(LogicalPlan::Filter {
            input: Box::new(scan_a()),
            predicate: in_subquery(column(binding_a("k", 1, 0)), inner, false),
        }),
    });
    let catalog = FakeCatalog::new();
    let ctx = PlanContext { catalog: &catalog };
    let Ok(PhysicalStatement::Delete(delete)) = plan(stmt, &ctx) else {
        panic!("a DELETE plans to a Delete");
    };
    let (index, _) = produced_position(&delete.input);
    let PhysicalPlan::NestedLoopJoin { outer, .. } = &delete.input else {
        panic!("expected the semi-join at the root of the input");
    };
    assert_eq!(outer.row_id_count(), 1);
    assert_eq!(outer.schema().columns.len(), 2);
    assert_eq!(index, 2);
}

/// `a.k IN (SELECT b.c / 10 FROM b UNION SELECT b.k FROM b)`: the root of the subquery is
/// the set operation, and the equality reads its one output column after the outer columns.
#[test]
fn in_subquery_over_a_union_compares_its_output_column() {
    let left = LogicalPlan::Project {
        input: Box::new(scan_b()),
        exprs: vec![BoundProjection {
            expr: c_over_ten(),
            name: "q".to_owned(),
        }],
        schema: schema(&["q"]),
    };
    let union = LogicalPlan::SetOp {
        op: SetOpKind::Union,
        all: false,
        left: Box::new(left),
        right: Box::new(project_one(scan_b(), binding_b("k", 11, 0), "k")),
        schema: schema(&["q"]),
    };
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: in_subquery(column(binding_a("k", 1, 0)), union, false),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let (index, inner) = produced_position(&planned);
    assert_eq!(index, 2);
    assert_eq!(inner.schema().columns.len(), 1);
}

/// `a.k IN (SELECT b.k FROM b) AND a.c IN (SELECT b.c FROM b WHERE b.k = a.k)`: the
/// uncorrelated `IN` becomes the semi-join, and the residual correlated `IN` is filtered
/// over a `SubqueryEval` that carries its plan, laid over the semi-join.
#[test]
fn a_residual_correlated_in_is_evaluated_over_the_semi_join() {
    let correlated = LogicalPlan::Filter {
        input: Box::new(scan_b()),
        predicate: compare(
            CompareOp::Eq,
            column(binding_b("k", 11, 0)),
            column(outer_a("k", 1, 0)),
        ),
    };
    let predicate = and(
        in_subquery(
            column(binding_a("k", 1, 0)),
            project_one(scan_b(), binding_b("k", 11, 0), "k"),
            false,
        ),
        in_subquery(
            column(binding_a("c", 2, 1)),
            project_one(correlated, binding_b("c", 12, 1), "c"),
            false,
        ),
    );
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate,
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::Filter { input, .. } = &planned else {
        panic!("expected a Filter for the residual, got {planned:?}");
    };
    let PhysicalPlan::SubqueryEval {
        input: join,
        subplans,
        ..
    } = input.as_ref()
    else {
        panic!("expected a SubqueryEval under the Filter, got {input:?}");
    };
    assert_eq!(subplans.len(), 1);
    assert!(subplans[0].correlated);
    let (index, _) = produced_position(join);
    assert_eq!(index, 2);
}

/// The `on` of an anti-semi-join: the equality between the tested value and the column the
/// inner plan produces, read right after the outer columns.
fn anti_semi_on(planned: &PhysicalPlan) -> (&BoundExpr, usize) {
    let PhysicalPlan::NestedLoopJoin {
        kind: PhysicalJoinKind::AntiSemi,
        on: Some(on),
        schema,
        ..
    } = planned
    else {
        panic!("expected an anti-semi-join with an ON, got {planned:?}");
    };
    assert_eq!(schema.columns.len(), 2, "the join keeps the columns of a");
    let BoundExprKind::Compare {
        op: CompareOp::Eq,
        left,
        right,
    } = &on.kind
    else {
        panic!("expected an equality ON, got {on:?}");
    };
    let BoundExprKind::ColumnRef(produced) = &right.kind else {
        panic!("expected a column reference, got {right:?}");
    };
    (left.as_ref(), produced.index)
}

/// `a.k NOT IN (SELECT b.k FROM b)`, `NOT (a.k IN (SELECT b.k FROM b))` and
/// `a.k <> ALL (SELECT b.k FROM b)`, the last bound as the first: an anti-semi-join whose
/// `on` is the equality of the `IN`.
#[test]
fn uncorrelated_not_in_becomes_an_anti_semi_join() {
    let inner = || project_one(scan_b(), binding_b("k", 11, 0), "k");
    for predicate in [
        in_subquery(column(binding_a("k", 1, 0)), inner(), true),
        not(in_subquery(column(binding_a("k", 1, 0)), inner(), false)),
    ] {
        let filtered = LogicalPlan::Filter {
            input: Box::new(scan_a()),
            predicate,
        };
        let planned = plan_with(&FakeCatalog::new(), filtered);
        let (tested, index) = anti_semi_on(&planned);
        assert_eq!(index, 2);
        assert!(
            matches!(&tested.kind, BoundExprKind::ColumnRef(binding) if binding.index == 0),
            "expected a.k on the left, got {tested:?}"
        );
    }
}

/// `a.k NOT IN (SELECT b.k FROM b WHERE b.c = a.c)` reads `a` inside the subquery: it is
/// evaluated per row, not turned into a join.
#[test]
fn correlated_not_in_stays_a_subquery_eval() {
    let correlated = LogicalPlan::Filter {
        input: Box::new(scan_b()),
        predicate: compare(
            CompareOp::Eq,
            column(binding_b("c", 12, 1)),
            column(outer_a("c", 2, 1)),
        ),
    };
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: in_subquery(
            column(binding_a("k", 1, 0)),
            project_one(correlated, binding_b("k", 11, 0), "k"),
            true,
        ),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::Filter { input, .. } = &planned else {
        panic!("expected Filter over SubqueryEval, got {planned:?}");
    };
    let PhysicalPlan::SubqueryEval {
        input, subplans, ..
    } = input.as_ref()
    else {
        panic!("expected SubqueryEval, got {input:?}");
    };
    assert!(subplans[0].correlated);
    assert!(
        matches!(input.as_ref(), PhysicalPlan::TableScan { .. }),
        "expected the scan of a under the SubqueryEval, got {input:?}"
    );
}

/// `(SELECT MAX(b.k) FROM b) NOT IN (SELECT b.k FROM b)`: the tested value holds a
/// subquery, which an `on` cannot evaluate; the predicate stays a `SubqueryEval`.
#[test]
fn not_in_over_a_scalar_subquery_stays_a_subquery_eval() {
    let max = LogicalPlan::Aggregate {
        input: Box::new(scan_b()),
        group_by: vec![],
        aggregates: vec![max_on(binding_b("k", 11, 0))],
        schema: schema(&["m"]),
    };
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: in_subquery(
            scalar(max),
            project_one(scan_b(), binding_b("k", 11, 0), "k"),
            true,
        ),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::Filter { input, .. } = &planned else {
        panic!("expected Filter over SubqueryEval, got {planned:?}");
    };
    assert!(
        matches!(input.as_ref(), PhysicalPlan::SubqueryEval { .. }),
        "expected SubqueryEval, got {input:?}"
    );
}

/// `EXISTS (SELECT … FROM b) AND a.c NOT IN (SELECT b.c FROM b)`: a semi-join, then an
/// anti-semi-join over it; the anti-semi-join reads `a.c` where the scan of `a` puts it.
#[test]
fn exists_and_not_in_chain_a_semi_and_an_anti_semi_join() {
    let predicate = and(
        exists(inner_exists_scan()),
        in_subquery(
            column(binding_a("c", 2, 1)),
            project_one(scan_b(), binding_b("c", 12, 1), "c"),
            true,
        ),
    );
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate,
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let (tested, index) = anti_semi_on(&planned);
    assert_eq!(index, 2);
    assert!(
        matches!(&tested.kind, BoundExprKind::ColumnRef(binding) if binding.index == 1),
        "expected a.c on the left, got {tested:?}"
    );
    let PhysicalPlan::NestedLoopJoin { outer, .. } = &planned else {
        unreachable!("anti_semi_on matched a join");
    };
    let PhysicalPlan::NestedLoopJoin {
        kind: PhysicalJoinKind::Semi,
        on: None,
        outer: scan,
        ..
    } = outer.as_ref()
    else {
        panic!("expected the semi-join of the EXISTS under the anti-semi-join, got {outer:?}");
    };
    assert!(matches!(scan.as_ref(), PhysicalPlan::TableScan { .. }));
}

/// `a.c > ALL (SELECT k FROM b)` as the binder rewrites it: `0 NOT IN` the outcomes
/// `CASE WHEN a.c > b.k THEN 1 WHEN NOT (a.c > b.k) THEN 0 END`, one per row of `b`. The
/// outcome reads `a.c` one query up, so the subquery is correlated and evaluated per row,
/// not turned into a join.
#[test]
fn a_quantified_rewrite_reads_the_outer_value_per_row() {
    let comparison = compare(
        CompareOp::Gt,
        column(outer_a("c", 2, 1)),
        column(binding_b("k", 11, 0)),
    );
    let outcome = BoundExpr {
        kind: BoundExprKind::Case {
            operand: None,
            arms: vec![
                BoundCaseArm {
                    when: comparison.clone(),
                    then: literal(1),
                },
                BoundCaseArm {
                    when: not(comparison),
                    then: literal(0),
                },
            ],
            else_: None,
        },
        ty: TypeInfo::new(SqlType::Int, true),
        line: 1,
    };
    let outcomes = LogicalPlan::Project {
        input: Box::new(project_one(scan_b(), binding_b("k", 11, 0), "k")),
        exprs: vec![BoundProjection {
            expr: outcome,
            name: String::new(),
        }],
        schema: schema(&[""]),
    };
    for negated in [true, false] {
        let filtered = LogicalPlan::Filter {
            input: Box::new(scan_a()),
            predicate: in_subquery(literal(i32::from(!negated)), outcomes.clone(), negated),
        };
        let planned = plan_with(&FakeCatalog::new(), filtered);
        let PhysicalPlan::Filter { input, .. } = &planned else {
            panic!("expected Filter over SubqueryEval, got {planned:?}");
        };
        let PhysicalPlan::SubqueryEval { subplans, .. } = input.as_ref() else {
            panic!("expected SubqueryEval, got {input:?}");
        };
        assert_eq!(subplans.len(), 1);
        assert!(subplans[0].correlated, "{planned:?}");
    }
}

#[test]
fn exists_grouped_on_an_outer_column_stays_correlated() {
    let inner = LogicalPlan::Aggregate {
        input: Box::new(scan_b()),
        group_by: vec![column(outer_a("k", 1, 0))],
        aggregates: Vec::new(),
        schema: schema(&["k"]),
    };
    assert_correlated_exists(inner);
}

#[test]
fn exists_ordered_on_an_outer_column_stays_correlated() {
    let inner = LogicalPlan::Sort {
        input: Box::new(scan_b()),
        keys: vec![SortKey {
            expr: column(outer_a("k", 1, 0)),
            desc: false,
            collation: None,
        }],
    };
    assert_correlated_exists(inner);
}

#[test]
fn exists_values_over_an_outer_column_stays_correlated() {
    let inner = LogicalPlan::Values {
        rows: vec![vec![column(outer_a("k", 1, 0))]],
        schema: schema(&["k"]),
    };
    assert_correlated_exists(inner);
}

#[test]
fn exists_max_on_an_outer_column_stays_correlated() {
    let inner = LogicalPlan::Aggregate {
        input: Box::new(scan_b()),
        group_by: Vec::new(),
        aggregates: vec![max_on(outer_a("k", 1, 0))],
        schema: schema(&["m"]),
    };
    assert_correlated_exists(inner);
}

#[test]
fn exists_max_without_an_outer_column_stays_a_semi_join() {
    let inner = LogicalPlan::Aggregate {
        input: Box::new(scan_b()),
        group_by: Vec::new(),
        aggregates: vec![max_on(binding_b("k", 11, 0))],
        schema: schema(&["m"]),
    };
    assert_uncorrelated_exists(inner);
}

#[test]
fn exists_top_on_an_outer_column_stays_correlated() {
    let inner = LogicalPlan::Limit {
        input: Box::new(scan_b()),
        top: BoundTop {
            expr: column(outer_a("k", 1, 0)),
            percent: false,
            with_ties: false,
        },
    };
    assert_correlated_exists(inner);
}

#[test]
fn exists_top_without_an_outer_column_stays_a_semi_join() {
    let inner = LogicalPlan::Limit {
        input: Box::new(scan_b()),
        top: BoundTop {
            expr: literal(1),
            percent: false,
            with_ties: false,
        },
    };
    assert_uncorrelated_exists(inner);
}

/// Each [`LogicalPlan`] arm that carries a [`BoundExpr`] must be walked by
/// `plan_holds_external_column`; extend this list when a new holder appears.
#[test]
fn correlation_walk_covers_every_bound_expr_holder() {
    const HOLDERS: &[&str] = &[
        "Filter.predicate",
        "Project.exprs",
        "Limit.top.expr",
        "Join.on",
        "Aggregate.group_by",
        "Aggregate.aggregates[].arg",
        "Sort.keys.expr",
        "Values.rows",
    ];
    assert_eq!(
        HOLDERS.len(),
        8,
        "update the holders when a new arm carries a BoundExpr"
    );

    let cases: Vec<(&str, LogicalPlan)> = vec![
        (
            "Filter.predicate",
            LogicalPlan::Filter {
                input: Box::new(scan_b()),
                predicate: compare(
                    CompareOp::Eq,
                    column(binding_b("k", 11, 0)),
                    column(outer_a("k", 1, 0)),
                ),
            },
        ),
        (
            "Project.exprs",
            LogicalPlan::Project {
                input: Box::new(scan_b()),
                exprs: vec![BoundProjection {
                    expr: column(outer_a("k", 1, 0)),
                    name: "k".to_owned(),
                }],
                schema: schema(&["k"]),
            },
        ),
        (
            "Limit.top.expr",
            LogicalPlan::Limit {
                input: Box::new(scan_b()),
                top: BoundTop {
                    expr: column(outer_a("k", 1, 0)),
                    percent: false,
                    with_ties: false,
                },
            },
        ),
        (
            "Join.on",
            LogicalPlan::Join {
                left: Box::new(scan_b()),
                right: Box::new(scan_b()),
                kind: JoinKind::Inner,
                on: Some(compare(
                    CompareOp::Eq,
                    column(binding_b("k", 11, 0)),
                    column(outer_a("k", 1, 0)),
                )),
                schema: schema(&["k", "k"]),
            },
        ),
        (
            "Aggregate.group_by",
            LogicalPlan::Aggregate {
                input: Box::new(scan_b()),
                group_by: vec![column(outer_a("k", 1, 0))],
                aggregates: Vec::new(),
                schema: schema(&["k"]),
            },
        ),
        (
            "Aggregate.aggregates[].arg",
            LogicalPlan::Aggregate {
                input: Box::new(scan_b()),
                group_by: Vec::new(),
                aggregates: vec![max_on(outer_a("k", 1, 0))],
                schema: schema(&["m"]),
            },
        ),
        (
            "Sort.keys.expr",
            LogicalPlan::Sort {
                input: Box::new(scan_b()),
                keys: vec![SortKey {
                    expr: column(outer_a("k", 1, 0)),
                    desc: false,
                    collation: None,
                }],
            },
        ),
        (
            "Values.rows",
            LogicalPlan::Values {
                rows: vec![vec![column(outer_a("k", 1, 0))]],
                schema: schema(&["k"]),
            },
        ),
    ];
    assert_eq!(
        cases.len(),
        HOLDERS.len(),
        "each holder name needs a correlated inner plan"
    );
    for (holder, inner) in cases {
        assert_correlated_exists(inner);
        assert!(
            HOLDERS.contains(&holder),
            "{holder} is missing from the holder list"
        );
    }
}

#[test]
fn exists_and_a_residual_predicate_keeps_the_filter() {
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: and(
            exists(inner_exists_scan()),
            compare(CompareOp::Eq, column(binding_a("c", 2, 1)), literal(1)),
        ),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::Filter {
        input, predicate, ..
    } = &planned
    else {
        panic!("expected Filter over semi-join, got {planned:?}");
    };
    let PhysicalPlan::NestedLoopJoin {
        kind: PhysicalJoinKind::Semi,
        ..
    } = input.as_ref()
    else {
        panic!("expected Semi join under Filter, got {input:?}");
    };
    let BoundExprKind::Compare {
        op: CompareOp::Eq,
        left,
        right,
    } = &predicate.kind
    else {
        panic!("expected residual c = 1, got {predicate:?}");
    };
    assert!(matches!(left.kind, BoundExprKind::ColumnRef(_)));
    assert!(matches!(right.kind, BoundExprKind::Literal(_)));
}

#[test]
fn correlated_exists_is_evaluated_per_row() {
    let correlated_inner = LogicalPlan::Filter {
        input: Box::new(project_scan(scan_b())),
        predicate: compare(
            CompareOp::Eq,
            column(binding_b("k", 11, 0)),
            column(outer_a("k", 1, 0)),
        ),
    };
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: exists(correlated_inner),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::Filter { input, .. } = &planned else {
        panic!("expected Filter over SubqueryEval, got {planned:?}");
    };
    let PhysicalPlan::SubqueryEval { subplans, .. } = input.as_ref() else {
        panic!("expected SubqueryEval, got {input:?}");
    };
    assert_eq!(subplans.len(), 1);
    assert!(subplans[0].correlated);

    let uncorrelated = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: exists(inner_exists_scan()),
    };
    let planned = plan_with(&FakeCatalog::new(), uncorrelated);
    assert!(
        matches!(
            planned,
            PhysicalPlan::NestedLoopJoin {
                kind: PhysicalJoinKind::Semi,
                ..
            }
        ),
        "expected Semi join, got {planned:?}"
    );
}

/// A scan of `b` whose first column carries the identifier of the first column of `a`:
/// the identifier alone does not tell the two tables apart.
fn scan_b_numbered_like_a() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE_B,
        columns: vec![binding_b("k", 1, 0), binding_b("c", 2, 1)],
        alias: "b".to_owned(),
        schema: schema(&["k", "c"]),
        hints: LockHints::default(),
    }
}

/// `EXISTS (SELECT 1 FROM b WHERE b.k = a.k)` where `a.k` and `b.k` share `column`,
/// `index` and `name`: the depth of the reference to `a.k` is what makes the subquery
/// correlated. With that depth at 0, the same shape is a plain semi-join.
#[test]
fn exists_on_a_second_table_with_the_same_column_id_is_correlated() {
    let correlated = LogicalPlan::Filter {
        input: Box::new(project_scan(scan_b_numbered_like_a())),
        predicate: compare(
            CompareOp::Eq,
            column(binding_b("k", 1, 0)),
            column(outer_a("k", 1, 0)),
        ),
    };
    assert_correlated_exists(correlated);

    let local_only = LogicalPlan::Filter {
        input: Box::new(project_scan(scan_b_numbered_like_a())),
        predicate: compare(
            CompareOp::Eq,
            column(binding_b("k", 1, 0)),
            column(binding_a("k", 1, 0)),
        ),
    };
    assert_uncorrelated_exists(local_only);
}

/// `(SELECT b.c FROM b WHERE b.k = a.k)` where `a.k` and `b.k` share `column`, `index`
/// and `name` is evaluated per row, not cached: `correlated` is true on the subplan.
#[test]
fn a_scalar_subquery_on_a_second_table_with_the_same_column_id_is_not_cached() {
    let inner = LogicalPlan::Project {
        input: Box::new(LogicalPlan::Filter {
            input: Box::new(scan_b_numbered_like_a()),
            predicate: compare(
                CompareOp::Eq,
                column(binding_b("k", 1, 0)),
                column(outer_a("k", 1, 0)),
            ),
        }),
        exprs: vec![BoundProjection {
            expr: column(binding_b("c", 2, 1)),
            name: "c".to_owned(),
        }],
        schema: schema(&["c"]),
    };
    let query = LogicalPlan::Project {
        input: Box::new(scan_a()),
        exprs: vec![BoundProjection {
            expr: scalar(inner),
            name: "x".to_owned(),
        }],
        schema: schema(&["x"]),
    };
    let planned = plan_with(&FakeCatalog::new(), query);
    let subplans = subplans_in(&planned);
    assert_eq!(subplans.len(), 1);
    assert!(
        subplans[0].correlated,
        "expected correlated: true, got {planned:?}"
    );
}

/// A reference met two subqueries down is classified against the subquery it lands in:
/// `depth` 1 from the innermost query names a column of the middle one, which stays
/// uncorrelated with respect to `a`; `depth` 2 reaches `a` and correlates the middle one.
#[test]
fn a_nested_reference_to_the_enclosing_query_is_not_correlated() {
    let middle_over = |depth: u8| LogicalPlan::Filter {
        input: Box::new(project_scan(scan_b())),
        predicate: exists(LogicalPlan::Filter {
            input: Box::new(project_scan(scan_b())),
            predicate: compare(
                CompareOp::Eq,
                column(binding_b("k", 11, 0)),
                column(ColumnBinding {
                    depth,
                    ..binding_b("k", 11, 0)
                }),
            ),
        }),
    };
    assert_uncorrelated_exists(middle_over(1));
    assert_correlated_exists(middle_over(2));
}

#[test]
fn an_uncorrelated_scalar_subquery_is_marked_once() {
    let inner = project_scan(scan_b());
    let query = LogicalPlan::Project {
        input: Box::new(scan_a()),
        exprs: vec![BoundProjection {
            expr: scalar(inner),
            name: "x".to_owned(),
        }],
        schema: schema(&["x"]),
    };
    let planned = plan_with(&FakeCatalog::new(), query);
    let subplans = subplans_in(&planned);
    assert_eq!(subplans.len(), 1);
    assert!(!subplans[0].correlated);
}

#[test]
fn subplans_follow_the_expression_order() {
    let left = scalar(project_scan(scan_a()));
    let right = scalar(project_scan(scan_b()));
    let expr = logical(LogicalOp::And, left, right);
    let query = LogicalPlan::Project {
        input: Box::new(scan_a()),
        exprs: vec![BoundProjection {
            expr,
            name: "both".to_owned(),
        }],
        schema: schema(&["both"]),
    };
    let planned = plan_with(&FakeCatalog::new(), query);
    let subplans = subplans_in(&planned);
    assert_eq!(subplans.len(), 2);
    assert_ne!(subplans[0].id, subplans[1].id);
    assert!(
        contains_table(&subplans[0].plan, TABLE_A),
        "the left subquery must be first in the walk"
    );
    assert!(
        contains_table(&subplans[1].plan, TABLE_B),
        "the right subquery must be second; swapping them fails this test"
    );
}

/// The binder may copy one subquery node into two places of an expression; both copies
/// carry one identity, and one plan is built for it.
#[test]
fn a_subquery_copied_into_two_places_is_planned_once() {
    let once = scalar(project_scan(scan_a()));
    let BoundExprKind::ScalarSubquery(subquery) = &once.kind else {
        panic!("expected a scalar subquery");
    };
    let id = subquery.id;
    let other = scalar(project_scan(scan_b()));
    let expr = logical(
        LogicalOp::And,
        logical(LogicalOp::And, once.clone(), other),
        once,
    );
    let query = LogicalPlan::Project {
        input: Box::new(scan_a()),
        exprs: vec![BoundProjection {
            expr,
            name: "both".to_owned(),
        }],
        schema: schema(&["both"]),
    };
    let planned = plan_with(&FakeCatalog::new(), query);
    let subplans = subplans_in(&planned);
    assert_eq!(subplans.len(), 2);
    assert_eq!(subplans[0].id, id);
    assert!(contains_table(&subplans[1].plan, TABLE_B));
}

fn subplans_in(plan: &PhysicalPlan) -> &[SubPlan] {
    match plan {
        PhysicalPlan::SubqueryEval { subplans, .. } => subplans,
        PhysicalPlan::Project { input, .. } => subplans_in(input),
        _ => panic!("expected SubqueryEval, got {plan:?}"),
    }
}

fn contains_table(plan: &PhysicalPlan, table: TableId) -> bool {
    match plan {
        PhysicalPlan::TableScan { table: read, .. } => *read == table,
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::Distinct(input)
        | PhysicalPlan::HashAggregate { input, .. }
        | PhysicalPlan::StreamAggregate { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. } => contains_table(input, table),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            contains_table(outer, table) || contains_table(inner, table)
        }
        PhysicalPlan::HashJoin { build, probe, .. } => {
            contains_table(build, table) || contains_table(probe, table)
        }
        PhysicalPlan::IndexSeek { .. }
        | PhysicalPlan::OneRow
        | PhysicalPlan::Values { .. }
        | PhysicalPlan::Union { .. }
        | PhysicalPlan::Except { .. }
        | PhysicalPlan::Intersect { .. } => false,
    }
}

#[test]
fn the_inner_plan_gets_the_planner_rules() {
    let catalog = FakeCatalog::new().with_index(
        TABLE_INDEXED,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        false,
    );
    let inner = LogicalPlan::Filter {
        input: Box::new(scan_indexed()),
        predicate: compare(CompareOp::Eq, column(binding_b("k", 21, 0)), literal(1)),
    };
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: exists(inner),
    };
    let planned = plan_with(&catalog, filtered);
    let PhysicalPlan::NestedLoopJoin { inner, .. } = &planned else {
        panic!("expected NestedLoopJoin, got {planned:?}");
    };
    assert!(
        contains_index_seek(inner),
        "inner plan should contain IndexSeek, got {inner:?}"
    );
}

fn contains_index_seek(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::IndexSeek { .. } => true,
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::Distinct(input)
        | PhysicalPlan::HashAggregate { input, .. }
        | PhysicalPlan::StreamAggregate { input, .. } => contains_index_seek(input),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            contains_index_seek(outer) || contains_index_seek(inner)
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------
// Subqueries written outside a plan: the value of a `SET`, the initializer of a `DECLARE`
// ---------------------------------------------------------------------------------------

fn planned_statement(logical: BoundStatement) -> PhysicalStatement {
    let catalog = FakeCatalog::new();
    let ctx = PlanContext { catalog: &catalog };
    match plan(logical, &ctx) {
        Ok(physical) => physical,
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

fn set_variable(value: BoundExpr) -> BoundStatement {
    BoundStatement::SetVariable {
        name: "@x".to_owned(),
        value,
    }
}

fn set_subplans(stmt: &PhysicalStatement) -> &[SubPlan] {
    match stmt {
        PhysicalStatement::SetVariable { subplans, .. } => subplans,
        other => panic!("expected a SET, got {other:?}"),
    }
}

/// `SET @x = (SELECT k FROM a)`: the plan of the subquery travels on the statement, since
/// there is no operator under the value to hang it on.
#[test]
fn a_set_carries_the_plan_of_its_subquery() {
    let planned = planned_statement(set_variable(scalar(project_scan(scan_a()))));
    let subplans = set_subplans(&planned);
    assert_eq!(subplans.len(), 1);
    assert!(contains_table(&subplans[0].plan, TABLE_A));
    assert!(!subplans[0].correlated);
}

/// `SET @x = (SELECT … FROM a) + (SELECT … FROM b)`: one plan per subquery, in the order
/// the expression is walked, left operand first.
#[test]
fn a_set_carries_one_subplan_per_subquery_of_its_value() {
    let left = scalar(project_scan(scan_a()));
    let right = scalar(project_scan(scan_b()));
    let planned = planned_statement(set_variable(logical(LogicalOp::And, left, right)));
    let subplans = set_subplans(&planned);
    assert_eq!(subplans.len(), 2);
    assert!(
        contains_table(&subplans[0].plan, TABLE_A),
        "the left subquery must be first in the walk"
    );
    assert!(
        contains_table(&subplans[1].plan, TABLE_B),
        "the right subquery must be second; swapping them fails this test"
    );
}

/// `SET @x = 1`: no subquery, no plan attached.
#[test]
fn a_set_without_a_subquery_carries_no_subplan() {
    let planned = planned_statement(set_variable(literal(1)));
    assert!(set_subplans(&planned).is_empty());
}

/// `DECLARE @a int = (SELECT k FROM a), @b int`: the plan goes on the declaration that
/// holds the subquery, and the one written without `=` carries none.
#[test]
fn a_declare_carries_the_subplans_of_each_initial_value() {
    let planned = planned_statement(BoundStatement::Declare(vec![
        BoundDeclaration {
            name: "@a".to_owned(),
            ty: int_type(),
            value: Some(scalar(project_scan(scan_a()))),
        },
        BoundDeclaration {
            name: "@b".to_owned(),
            ty: int_type(),
            value: None,
        },
    ]));
    let PhysicalStatement::Declare(declarations) = &planned else {
        panic!("expected a DECLARE, got {planned:?}");
    };
    assert_eq!(declarations[0].subplans.len(), 1);
    assert!(contains_table(&declarations[0].subplans[0].plan, TABLE_A));
    assert!(declarations[1].subplans.is_empty());
}

/// A set operation whose right operand reads `a.k` one query up, the left one reading `b`
/// alone.
fn union_correlated_on_the_right(op: SetOpKind, depth: u8) -> LogicalPlan {
    let right = LogicalPlan::Filter {
        input: Box::new(project_scan(scan_b())),
        predicate: compare(
            CompareOp::Eq,
            column(binding_b("k", 11, 0)),
            column(ColumnBinding {
                depth,
                ..binding_a("k", 1, 0)
            }),
        ),
    };
    LogicalPlan::SetOp {
        op,
        all: false,
        left: Box::new(project_scan(scan_b())),
        right: Box::new(right),
        schema: schema(&["k", "c"]),
    }
}

/// An `EXISTS` over a set operation with an operand that reads the enclosing query stays
/// a correlated `SubqueryEval`, not a semi-join; without the outer reference it becomes
/// the semi-join.
#[test]
fn exists_over_a_correlated_set_operation_is_correlated() {
    for op in [SetOpKind::Union, SetOpKind::Intersect, SetOpKind::Except] {
        assert_correlated_exists(union_correlated_on_the_right(op, 1));
        assert_uncorrelated_exists(union_correlated_on_the_right(op, 0));
    }
}

/// A scalar subquery over a set operation with an operand that reads the enclosing query
/// is evaluated per row: `correlated` is true on its subplan.
#[test]
fn a_scalar_subquery_over_a_correlated_set_operation_is_not_cached() {
    let inner = LogicalPlan::Project {
        input: Box::new(union_correlated_on_the_right(SetOpKind::Intersect, 1)),
        exprs: vec![BoundProjection {
            expr: column(binding_b("k", 11, 0)),
            name: "k".to_owned(),
        }],
        schema: schema(&["k"]),
    };
    let query = LogicalPlan::Project {
        input: Box::new(scan_a()),
        exprs: vec![BoundProjection {
            expr: scalar(inner),
            name: "x".to_owned(),
        }],
        schema: schema(&["x"]),
    };
    let planned = plan_with(&FakeCatalog::new(), query);
    let subplans = subplans_in(&planned);
    assert_eq!(subplans.len(), 1);
    assert!(
        subplans[0].correlated,
        "expected correlated: true, got {planned:?}"
    );
}

/// `EXISTS (SELECT k FROM a WHERE EXISTS (SELECT k FROM b))`: two levels of subquery, the
/// inner one planned inside the plan of the outer one.
fn nested_exists() -> BoundExpr {
    exists(LogicalPlan::Filter {
        input: Box::new(project_scan(scan_a())),
        predicate: exists(project_scan(scan_b())),
    })
}

/// The plan of the one subquery written directly in a statement expression reads both
/// tables: the nested subquery travels inside it, not beside it.
fn assert_nested_plans(planned: &vauban_planner::StatementExpr) {
    assert_eq!(planned.subplans.len(), 1);
    assert!(contains_table(&planned.subplans[0].plan, TABLE_A));
    assert!(contains_table(&planned.subplans[0].plan, TABLE_B));
}

/// `IF EXISTS (SELECT … WHERE EXISTS (…))`: the condition carries the plan of its subquery,
/// as the value of a `SET` does.
#[test]
fn an_if_condition_carries_the_plans_of_its_subqueries() {
    let planned = planned_statement(BoundStatement::If {
        condition: nested_exists(),
        then_: Box::new(BoundStatement::Break),
        else_: None,
    });
    let PhysicalStatement::If { condition, .. } = &planned else {
        panic!("expected an IF, got {planned:?}");
    };
    assert_nested_plans(condition);
}

/// `WHILE EXISTS (SELECT … WHERE EXISTS (…))`: the same for the condition of a loop.
#[test]
fn a_while_condition_carries_the_plans_of_its_subqueries() {
    let planned = planned_statement(BoundStatement::While {
        condition: nested_exists(),
        body: Box::new(BoundStatement::Break),
    });
    let PhysicalStatement::While { condition, .. } = &planned else {
        panic!("expected a WHILE, got {planned:?}");
    };
    assert_nested_plans(condition);
}

/// `RETURN (SELECT …)` carries the plan of its subquery.
#[test]
fn a_return_carries_the_plan_of_its_subquery() {
    let planned = planned_statement(BoundStatement::Return(Some(scalar(project_scan(scan_a())))));
    let PhysicalStatement::Return(Some(returned)) = &planned else {
        panic!("expected a RETURN, got {planned:?}");
    };
    assert_eq!(returned.subplans.len(), 1);
    assert!(contains_table(&returned.subplans[0].plan, TABLE_A));
}

/// `IF 1 = 1`: no subquery, no plan attached.
#[test]
fn an_if_condition_without_a_subquery_carries_no_subplan() {
    let planned = planned_statement(BoundStatement::If {
        condition: compare(CompareOp::Eq, literal(1), literal(1)),
        then_: Box::new(BoundStatement::Break),
        else_: None,
    });
    let PhysicalStatement::If { condition, .. } = &planned else {
        panic!("expected an IF, got {planned:?}");
    };
    assert!(condition.subplans.is_empty());
}
