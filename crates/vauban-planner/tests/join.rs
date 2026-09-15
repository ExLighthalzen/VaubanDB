//! The join planning rules: nested loop by default, seek on inner when an equality matches
//! an index, and hash join on equality without an index.
//!
//! Each test builds a `LogicalPlan::Join` by hand and plans it against a `FakeCatalog`.
//! The tables have two `int` columns each: `a` has columns `{k, c}` and `b` has `{k, c}`,
//! both at storage positions 0 and 1.

use vauban_planner::{
    KeyRangeExpr, PhysicalJoinKind, PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext,
    plan, testing::FakeCatalog,
};

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundStatement, ColumnBinding, CompareOp, JoinKind, LockHints,
    LogicalOp, LogicalPlan, OutputColumn, OutputSchema,
};
use vauban_catalog::ColumnId;
use vauban_storage::{Direction, IndexId, KeyColumn, TableId};
use vauban_types::{SqlType, TypeInfo};

// Table IDs for the two sides.
const TABLE_A: TableId = TableId(1);
const TABLE_B: TableId = TableId(2);

fn int_type() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

fn bit_type() -> TypeInfo {
    TypeInfo::new(SqlType::Bit, false)
}

fn binding_a(name: &str, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(index as i32 + 1),
        index,
        name: name.to_owned(),
        ty: int_type(),
    }
}

fn binding_b(name: &str, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(index as i32 + 10),
        index: 2 + index,
        name: name.to_owned(),
        ty: int_type(),
    }
}

fn column_a(name: &str, index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding_a(name, index)),
        ty: int_type(),
        line: 1,
    }
}

fn column_b(name: &str, index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding_b(name, index)),
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

fn schema_a() -> OutputSchema {
    OutputSchema {
        columns: vec![
            OutputColumn {
                name: "k".to_owned(),
                ty: int_type(),
            },
            OutputColumn {
                name: "c".to_owned(),
                ty: int_type(),
            },
        ],
    }
}

fn schema_b() -> OutputSchema {
    OutputSchema {
        columns: vec![
            OutputColumn {
                name: "k".to_owned(),
                ty: int_type(),
            },
            OutputColumn {
                name: "c".to_owned(),
                ty: int_type(),
            },
        ],
    }
}

fn scan_a() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE_A,
        columns: vec![binding_a("k", 0), binding_a("c", 1)],
        alias: "a".to_owned(),
        schema: schema_a(),
        hints: LockHints::default(),
    }
}

fn scan_b() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE_B,
        columns: vec![binding_b("k", 0), binding_b("c", 1)],
        alias: "b".to_owned(),
        schema: schema_b(),
        hints: LockHints::default(),
    }
}

/// The join schema of a then b: columns of left then right.
fn join_schema() -> OutputSchema {
    OutputSchema {
        columns: schema_a()
            .columns
            .into_iter()
            .chain(schema_b().columns)
            .collect(),
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

fn join(kind: JoinKind, on: Option<BoundExpr>) -> LogicalPlan {
    LogicalPlan::Join {
        left: Box::new(scan_a()),
        right: Box::new(scan_b()),
        kind,
        on,
        schema: join_schema(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn inner_join_without_index_is_a_hash_join() {
    let catalog = FakeCatalog::new();
    let planned = plan_with(
        &catalog,
        join(
            JoinKind::Inner,
            Some(compare(CompareOp::Eq, column_a("k", 0), column_b("k", 0))),
        ),
    );

    let PhysicalPlan::HashJoin {
        build,
        probe,
        kind,
        keys,
        residual,
        ..
    } = &planned
    else {
        panic!("expected a HashJoin, got {planned:?}")
    };
    assert_eq!(*kind, PhysicalJoinKind::Inner);
    assert!(residual.is_none());
    assert_eq!(keys.len(), 1);
    // build is the right side, probe is the left side.
    assert!(
        matches!(build.as_ref(), PhysicalPlan::TableScan { table, .. } if *table == TABLE_B),
        "build should be the right side, got {build:?}"
    );
    assert!(
        matches!(probe.as_ref(), PhysicalPlan::TableScan { table, .. } if *table == TABLE_A),
        "probe should be the left side, got {probe:?}"
    );
}

#[test]
fn cross_join_is_a_nested_loop() {
    let catalog = FakeCatalog::new();
    let planned = plan_with(&catalog, join(JoinKind::Cross, None));

    let PhysicalPlan::NestedLoopJoin { kind, on, .. } = &planned else {
        panic!("expected a NestedLoopJoin, got {planned:?}")
    };
    assert_eq!(*kind, PhysicalJoinKind::Cross);
    assert!(on.is_none());
}

#[test]
fn equality_on_an_inner_index_becomes_a_loop_with_a_seek() {
    let catalog = FakeCatalog::new().with_index(
        TABLE_B,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        true,
    );
    let planned = plan_with(
        &catalog,
        join(
            JoinKind::Inner,
            Some(compare(CompareOp::Eq, column_a("k", 0), column_b("k", 0))),
        ),
    );

    let PhysicalPlan::NestedLoopJoin { inner, on, .. } = &planned else {
        panic!("expected a NestedLoopJoin, got {planned:?}")
    };
    // The ON is empty: the equality was consumed by the seek.
    assert!(on.is_none(), "expected no remaining ON, got {on:?}");

    let PhysicalPlan::IndexSeek {
        index,
        range,
        direction,
        ..
    } = inner.as_ref()
    else {
        panic!("expected an IndexSeek as inner, got {inner:?}")
    };
    assert_eq!(*index, IndexId(1));
    assert_eq!(*direction, Direction::Forward);
    let KeyRangeExpr::Point(keys) = range else {
        panic!("expected a Point, got {range:?}")
    };
    assert_eq!(keys.len(), 1);
    // The bound is the outer column expression: a.k (column_a("k", 0)).
    assert!(
        matches!(&keys[0].kind, BoundExprKind::ColumnRef(b) if b.name == "k" && b.index == 0),
        "expected a ColumnRef to a.k, got {:?}",
        keys[0]
    );
}

#[test]
fn residual_conditions_stay_on_the_join() {
    let catalog = FakeCatalog::new();
    let planned = plan_with(
        &catalog,
        join(
            JoinKind::Inner,
            Some(and(
                compare(CompareOp::Eq, column_a("k", 0), column_b("k", 0)),
                compare(CompareOp::Gt, column_a("c", 1), column_b("c", 1)),
            )),
        ),
    );

    let PhysicalPlan::HashJoin { keys, residual, .. } = &planned else {
        panic!("expected a HashJoin, got {planned:?}")
    };
    assert_eq!(keys.len(), 1, "expected one equality key");
    let (ref left_key, ref right_key) = keys[0];
    // The equality key is a.k = b.k
    assert!(
        matches!(&left_key.kind, BoundExprKind::ColumnRef(b) if b.name == "k"),
        "left key should be a.k, got {left_key:?}"
    );
    assert!(
        matches!(&right_key.kind, BoundExprKind::ColumnRef(b) if b.name == "k"),
        "right key should be b.k, got {right_key:?}"
    );

    // residual is a.x > b.y
    let Some(residual) = residual else {
        panic!("expected a residual condition, got None")
    };
    assert!(
        matches!(
            &residual.kind,
            BoundExprKind::Compare {
                op: CompareOp::Gt,
                ..
            }
        ),
        "expected a Gt comparison, got {residual:?}"
    );
}

#[test]
fn full_join_is_a_nested_loop() {
    let catalog = FakeCatalog::new();
    let planned = plan_with(
        &catalog,
        join(
            JoinKind::Full,
            Some(compare(CompareOp::Eq, column_a("k", 0), column_b("k", 0))),
        ),
    );

    let PhysicalPlan::NestedLoopJoin { kind, .. } = &planned else {
        panic!("expected a NestedLoopJoin, got {planned:?}")
    };
    assert_eq!(*kind, PhysicalJoinKind::Full);
}

#[test]
fn right_join_is_a_nested_loop() {
    let catalog = FakeCatalog::new();
    let planned = plan_with(
        &catalog,
        join(
            JoinKind::Right,
            Some(compare(CompareOp::Eq, column_a("k", 0), column_b("k", 0))),
        ),
    );

    let PhysicalPlan::NestedLoopJoin { kind, .. } = &planned else {
        panic!("expected a NestedLoopJoin, got {planned:?}")
    };
    assert_eq!(*kind, PhysicalJoinKind::Right);
}

#[test]
fn join_order_is_the_written_order() {
    let catalog = FakeCatalog::new();
    let planned = plan_with(
        &catalog,
        join(
            JoinKind::Inner,
            Some(compare(CompareOp::Eq, column_a("k", 0), column_b("k", 0))),
        ),
    );

    // For a hash join: probe = left (a), build = right (b).
    let PhysicalPlan::HashJoin { probe, build, .. } = &planned else {
        panic!("expected a HashJoin, got {planned:?}")
    };
    assert!(
        matches!(probe.as_ref(), PhysicalPlan::TableScan { table, .. } if *table == TABLE_A),
        "probe should be the left side (a), got {probe:?}"
    );
    assert!(
        matches!(build.as_ref(), PhysicalPlan::TableScan { table, .. } if *table == TABLE_B),
        "build should be the right side (b), got {build:?}"
    );
}

#[test]
fn join_schema_is_left_then_right() {
    let catalog = FakeCatalog::new();
    let planned = plan_with(
        &catalog,
        join(
            JoinKind::Inner,
            Some(compare(CompareOp::Eq, column_a("k", 0), column_b("k", 0))),
        ),
    );

    let schema = planned.schema();
    assert_eq!(
        schema.columns.len(),
        4,
        "expected 4 columns, got {}",
        schema.columns.len()
    );
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["k", "c", "k", "c"]);
}

#[test]
fn a_join_without_equality_falls_back_to_the_loop() {
    let catalog = FakeCatalog::new();
    let planned = plan_with(
        &catalog,
        join(
            JoinKind::Inner,
            Some(compare(CompareOp::Gt, column_a("c", 1), column_b("c", 1))),
        ),
    );

    let PhysicalPlan::NestedLoopJoin { kind, on, .. } = &planned else {
        panic!("expected a NestedLoopJoin, got {planned:?}")
    };
    assert_eq!(*kind, PhysicalJoinKind::Inner);
    assert!(on.is_some(), "expected a remaining ON predicate");
}
