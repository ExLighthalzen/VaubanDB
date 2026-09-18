//! Subquery planning: derived-table flattening, decorrelated semi-joins, and
//! [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval) for correlated forms.
//!
//! Each test builds a `LogicalPlan` by hand and plans it against a `FakeCatalog`.

use vauban_planner::{
    PhysicalJoinKind, PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext, SubPlan, plan,
    testing::FakeCatalog,
};

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, BoundStatement, ColumnBinding, CompareOp, LockHints,
    LogicalOp, LogicalPlan, OutputColumn, OutputSchema, SortKey,
};
use vauban_catalog::ColumnId;
use vauban_storage::{KeyColumn, TableId};
use vauban_types::{SqlType, TypeInfo, Value};

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
    }
}

fn binding_b(name: &str, column_id: i32, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(column_id),
        index,
        name: name.to_owned(),
        ty: int_type(),
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
    let PhysicalPlan::SubqueryEval { subplans, .. } = &planned else {
        panic!("expected SubqueryEval, got {planned:?}");
    };
    assert_eq!(subplans.len(), 1);
    assert!(
        subplans[0].correlated,
        "expected correlated: true, got {planned:?}"
    );
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
        kind: BoundExprKind::Exists(Box::new(inner)),
        ty: bit_type(),
        line: 1,
    }
}

fn scalar(inner: LogicalPlan) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ScalarSubquery(Box::new(inner)),
        ty: int_type(),
        line: 1,
    }
}

fn in_subquery(tested: BoundExpr, inner: LogicalPlan, negated: bool) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::InSubquery {
            expr: Box::new(tested),
            plan: Box::new(inner),
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

#[test]
fn not_in_stays_a_subquery_eval() {
    let inner = project_one(scan_b(), binding_b("k", 11, 0), "k");
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: in_subquery(column(binding_a("k", 1, 0)), inner, true),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    assert!(
        matches!(planned, PhysicalPlan::SubqueryEval { .. }),
        "expected SubqueryEval, got {planned:?}"
    );
    assert!(
        !matches!(
            planned,
            PhysicalPlan::NestedLoopJoin {
                kind: PhysicalJoinKind::AntiSemi,
                ..
            }
        ),
        "NOT IN must not become AntiSemi"
    );
}

#[test]
fn exists_grouped_on_an_outer_column_stays_correlated() {
    let inner = LogicalPlan::Aggregate {
        input: Box::new(scan_b()),
        group_by: vec![column(binding_a("k", 1, 0))],
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
            expr: column(binding_a("k", 1, 0)),
            desc: false,
            collation: None,
        }],
    };
    assert_correlated_exists(inner);
}

#[test]
fn exists_values_over_an_outer_column_stays_correlated() {
    let inner = LogicalPlan::Values {
        rows: vec![vec![column(binding_a("k", 1, 0))]],
        schema: schema(&["k"]),
    };
    assert_correlated_exists(inner);
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
            column(binding_a("k", 1, 0)),
        ),
    };
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan_a()),
        predicate: exists(correlated_inner),
    };
    let planned = plan_with(&FakeCatalog::new(), filtered);
    let PhysicalPlan::SubqueryEval { subplans, .. } = &planned else {
        panic!("expected SubqueryEval, got {planned:?}");
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
    assert!(
        contains_table(&subplans[0].plan, TABLE_A),
        "the left subquery must be first in the walk"
    );
    assert!(
        contains_table(&subplans[1].plan, TABLE_B),
        "the right subquery must be second; swapping them fails this test"
    );
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
