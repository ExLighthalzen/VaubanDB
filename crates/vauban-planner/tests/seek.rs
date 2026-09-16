//! The seek rule: a `Filter` over a scan becomes an `IndexSeek` when its conjunctions
//! cover a prefix of an index of the scanned table.
//!
//! Each test builds a `LogicalPlan` by hand and plans it against a `FakeCatalog`; none goes
//! through the parser or the binder. The table read has three `int` columns, `a`, `b` and
//! `c`, at storage positions 0, 1 and 2.

use std::ops::Bound;

use vauban_planner::{
    KeyRangeExpr, PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext, plan,
    testing::FakeCatalog,
};

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundStatement, ColumnBinding, CompareOp, LockHints, LogicalOp,
    LogicalPlan, OutputColumn, OutputSchema,
};
use vauban_catalog::ColumnId;
use vauban_storage::{Direction, IndexId, KeyColumn, TableId};
use vauban_types::{SqlType, TypeInfo, Value};

const TABLE: TableId = TableId(7);

fn int_type() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

fn bit_type() -> TypeInfo {
    TypeInfo::new(SqlType::Bit, false)
}

fn literal(n: i32) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(n)),
        ty: int_type(),
        line: 1,
    }
}

fn variable(name: &str) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Variable {
            name: name.to_owned(),
        },
        ty: int_type(),
        line: 1,
    }
}

/// The three columns of the table, in storage order.
const COLUMNS: [&str; 3] = ["a", "b", "c"];

fn binding(name: &str) -> ColumnBinding {
    let index = COLUMNS
        .iter()
        .position(|column| *column == name)
        .expect("a column of the table");
    ColumnBinding {
        column: ColumnId(index as i32 + 1),
        index,
        name: name.to_owned(),
        ty: int_type(),
    }
}

fn column(name: &str) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(name)),
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

/// `FROM dbo.t`, the three columns read, with `hints` on the reference.
fn scan_with(hints: LockHints) -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE,
        columns: COLUMNS.iter().map(|name| binding(name)).collect(),
        alias: "t".to_owned(),
        schema: OutputSchema {
            columns: COLUMNS
                .iter()
                .map(|name| OutputColumn {
                    name: (*name).to_owned(),
                    ty: int_type(),
                })
                .collect(),
        },
        hints,
    }
}

/// `FROM dbo.t` written without a hint.
fn scan() -> LogicalPlan {
    scan_with(LockHints::default())
}

fn filter(predicate: BoundExpr) -> LogicalPlan {
    LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate,
    }
}

fn key(name: &str, descending: bool) -> KeyColumn {
    let position = COLUMNS
        .iter()
        .position(|column| *column == name)
        .expect("a column of the table");
    KeyColumn {
        column: position as u16,
        descending,
    }
}

fn ascending(names: &[&str]) -> Vec<KeyColumn> {
    names.iter().map(|name| key(name, false)).collect()
}

fn plan_with(catalog: &dyn PlanCatalog, logical: LogicalPlan) -> PhysicalPlan {
    let ctx = PlanContext { catalog };
    match plan(BoundStatement::Query(Box::new(logical)), &ctx) {
        Ok(PhysicalStatement::Query(physical)) => physical,
        Ok(other) => panic!("expected a query, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

/// The value of a literal bound, which is what the tests compare.
fn literal_value(expr: &BoundExpr) -> i32 {
    match &expr.kind {
        BoundExprKind::Literal(Value::I32(n)) => *n,
        other => panic!("expected an int literal, got {other:?}"),
    }
}

fn literal_values(exprs: &[BoundExpr]) -> Vec<i32> {
    exprs.iter().map(literal_value).collect()
}

/// Checks that `planned` is a `Filter` over a `TableScan` of the table, with no seek
/// anywhere in it.
fn assert_filter_over_scan(planned: &PhysicalPlan) {
    let PhysicalPlan::Filter { input, .. } = planned else {
        panic!("expected a Filter, got {planned:?}")
    };
    assert!(
        matches!(input.as_ref(), PhysicalPlan::TableScan { table, .. } if *table == TABLE),
        "expected a TableScan under the Filter, got {input:?}"
    );
}

#[test]
fn equality_on_a_unique_index_becomes_a_point_seek() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), literal(3))),
    );

    let PhysicalPlan::IndexSeek {
        index,
        range,
        direction,
        ..
    } = &planned
    else {
        panic!("expected an IndexSeek with no Filter above it, got {planned:?}")
    };
    assert_eq!(*index, IndexId(1));
    assert_eq!(*direction, Direction::Forward);
    let KeyRangeExpr::Point(keys) = range else {
        panic!("expected a Point, got {range:?}")
    };
    assert_eq!(literal_values(keys), vec![3]);
}

/// A `Filter` above a `Scan` whose reference carries `UPDLOCK`: the seek that replaces the
/// scan keeps the hints.
#[test]
fn a_seek_keeps_the_hints_of_the_scan_it_replaces() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    let hints = LockHints {
        updlock: true,
        ..LockHints::default()
    };
    let logical = LogicalPlan::Filter {
        input: Box::new(scan_with(hints)),
        predicate: compare(CompareOp::Eq, column("a"), literal(3)),
    };
    let planned = plan_with(&catalog, logical);
    let PhysicalPlan::IndexSeek { hints: carried, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    assert_eq!(*carried, hints);
}

#[test]
fn the_bound_may_be_written_on_the_left() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    // `3 = a` seeks the same point as `a = 3`; `3 < a` is `a > 3`, an exclusive lower bound.
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, literal(3), column("a"))),
    );
    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    let KeyRangeExpr::Point(keys) = range else {
        panic!("expected a Point, got {range:?}")
    };
    assert_eq!(literal_values(keys), vec![3]);

    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Lt, literal(3), column("a"))),
    );
    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    let KeyRangeExpr::Between(Bound::Excluded(low), Bound::Unbounded) = range else {
        panic!("expected a Between open above, got {range:?}")
    };
    assert_eq!(literal_values(low), vec![3]);
}

#[test]
fn prefix_of_a_composite_index_becomes_a_range() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a", "b"]), false);
    // WHERE a = 1 AND b > 2
    let predicate = and(
        compare(CompareOp::Eq, column("a"), literal(1)),
        compare(CompareOp::Gt, column("b"), literal(2)),
    );
    let planned = plan_with(&catalog, filter(predicate));

    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek with no Filter above it, got {planned:?}")
    };
    let KeyRangeExpr::Between(low, high) = range else {
        panic!("expected a Between, got {range:?}")
    };
    let Bound::Excluded(low) = low else {
        panic!("expected an exclusive lower bound, got {low:?}")
    };
    assert_eq!(literal_values(low), vec![1, 2]);
    // No upper bound on `b`: the upper side is the whole set of keys starting with `a = 1`.
    let Bound::Included(high) = high else {
        panic!("expected an inclusive upper bound on the prefix, got {high:?}")
    };
    assert_eq!(literal_values(high), vec![1]);
}

#[test]
fn two_bounds_on_the_range_column_close_the_range() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a", "b"]), false);
    // WHERE a = 1 AND b >= 2 AND b < 5
    let predicate = and(
        and(
            compare(CompareOp::Eq, column("a"), literal(1)),
            compare(CompareOp::Ge, column("b"), literal(2)),
        ),
        compare(CompareOp::Lt, column("b"), literal(5)),
    );
    let planned = plan_with(&catalog, filter(predicate));

    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek with no Filter above it, got {planned:?}")
    };
    let KeyRangeExpr::Between(Bound::Included(low), Bound::Excluded(high)) = range else {
        panic!("expected a Between closed below and open above, got {range:?}")
    };
    assert_eq!(literal_values(low), vec![1, 2]);
    assert_eq!(literal_values(high), vec![1, 5]);
}

#[test]
fn equalities_on_a_strict_prefix_make_a_between_on_the_prefix() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a", "b"]), false);
    // WHERE a = 1, on an index (a, b): a prefix, not a point.
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), literal(1))),
    );

    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    let KeyRangeExpr::Between(Bound::Included(low), Bound::Included(high)) = range else {
        panic!("expected a Between on the prefix, got {range:?}")
    };
    assert_eq!(literal_values(low), vec![1]);
    assert_eq!(literal_values(high), vec![1]);
}

#[test]
fn a_range_on_the_first_column_alone_is_open_on_the_other_side() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    // WHERE a <= 4
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Le, column("a"), literal(4))),
    );

    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    let KeyRangeExpr::Between(Bound::Unbounded, Bound::Included(high)) = range else {
        panic!("expected a Between open below, got {range:?}")
    };
    assert_eq!(literal_values(high), vec![4]);
}

#[test]
fn a_descending_range_column_flips_the_bounds() {
    let catalog = FakeCatalog::new().with_index(TABLE, &[key("a", false), key("b", true)], false);
    // WHERE a = 1 AND b > 2 on an index (a, b DESC): in index order the keys with b > 2
    // come before b = 2, so the bound closes the range from above.
    let predicate = and(
        compare(CompareOp::Eq, column("a"), literal(1)),
        compare(CompareOp::Gt, column("b"), literal(2)),
    );
    let planned = plan_with(&catalog, filter(predicate));

    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    let KeyRangeExpr::Between(Bound::Included(low), Bound::Excluded(high)) = range else {
        panic!("expected a Between open above, got {range:?}")
    };
    assert_eq!(literal_values(low), vec![1]);
    assert_eq!(literal_values(high), vec![1, 2]);
}

#[test]
fn residual_predicate_stays_in_a_filter() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    // WHERE a = 1 AND c = 9
    let predicate = and(
        compare(CompareOp::Eq, column("a"), literal(1)),
        compare(CompareOp::Eq, column("c"), literal(9)),
    );
    let planned = plan_with(&catalog, filter(predicate));

    let PhysicalPlan::Filter { input, predicate } = &planned else {
        panic!("expected a Filter over the seek, got {planned:?}")
    };
    // One conjunction left, `c = 9`: `a = 1` was consumed by the seek and is not
    // evaluated twice.
    let BoundExprKind::Compare {
        op: CompareOp::Eq,
        left,
        right,
    } = &predicate.kind
    else {
        panic!("expected the single conjunction c = 9, got {predicate:?}")
    };
    assert!(matches!(&left.kind, BoundExprKind::ColumnRef(b) if b.name == "c"));
    assert_eq!(literal_value(right), 9);

    let PhysicalPlan::IndexSeek { range, .. } = input.as_ref() else {
        panic!("expected an IndexSeek under the Filter, got {input:?}")
    };
    let KeyRangeExpr::Point(keys) = range else {
        panic!("expected a Point, got {range:?}")
    };
    assert_eq!(literal_values(keys), vec![1]);
}

#[test]
fn two_residual_conjunctions_are_joined_with_and() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    // WHERE c = 9 AND a = 1 AND b = 2: `a = 1` is consumed, `c = 9 AND b = 2` remains.
    let predicate = and(
        and(
            compare(CompareOp::Eq, column("c"), literal(9)),
            compare(CompareOp::Eq, column("a"), literal(1)),
        ),
        compare(CompareOp::Eq, column("b"), literal(2)),
    );
    let planned = plan_with(&catalog, filter(predicate));

    let PhysicalPlan::Filter { input, predicate } = &planned else {
        panic!("expected a Filter over the seek, got {planned:?}")
    };
    let BoundExprKind::Logical {
        op: LogicalOp::And,
        left,
        right,
    } = &predicate.kind
    else {
        panic!("expected c = 9 AND b = 2, got {predicate:?}")
    };
    let name_of = |expr: &BoundExpr| match &expr.kind {
        BoundExprKind::Compare { left, .. } => match &left.kind {
            BoundExprKind::ColumnRef(b) => b.name.clone(),
            other => panic!("expected a column, got {other:?}"),
        },
        other => panic!("expected a comparison, got {other:?}"),
    };
    assert_eq!(name_of(left), "c");
    assert_eq!(name_of(right), "b");
    assert!(matches!(input.as_ref(), PhysicalPlan::IndexSeek { .. }));
}

#[test]
fn a_non_indexed_column_keeps_the_table_scan() {
    // An index on `a`, a predicate on `c`.
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("c"), literal(9))),
    );
    assert_filter_over_scan(&planned);

    // The same predicate with no index at all.
    let empty = FakeCatalog::new();
    let planned = plan_with(
        &empty,
        filter(compare(CompareOp::Eq, column("c"), literal(9))),
    );
    assert_filter_over_scan(&planned);
}

#[test]
fn a_bound_on_the_second_key_column_alone_is_no_seek() {
    // Index (a, b), WHERE b = 2: no equality on the first key column.
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a", "b"]), false);
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("b"), literal(2))),
    );
    assert_filter_over_scan(&planned);
}

#[test]
fn a_not_equal_is_no_range() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Ne, column("a"), literal(3))),
    );
    assert_filter_over_scan(&planned);
}

#[test]
fn a_bound_that_reads_a_column_is_not_a_key() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    // WHERE a = b
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), column("b"))),
    );
    assert_filter_over_scan(&planned);

    // WHERE a = -b: the column is read inside the bound.
    let minus_b = BoundExpr {
        kind: BoundExprKind::Negate(Box::new(column("b"))),
        ty: int_type(),
        line: 1,
    };
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), minus_b)),
    );
    assert_filter_over_scan(&planned);
}

#[test]
fn a_variable_bound_is_kept_as_an_expression() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    // WHERE a = @x
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), variable("@x"))),
    );

    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    let KeyRangeExpr::Point(keys) = range else {
        panic!("expected a Point, got {range:?}")
    };
    assert_eq!(keys.len(), 1);
    assert!(
        matches!(&keys[0].kind, BoundExprKind::Variable { name } if name == "@x"),
        "expected the variable itself as the bound, got {:?}",
        keys[0]
    );
}

#[test]
fn a_converted_and_a_negated_literal_are_bounds() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    // WHERE a = -1
    let minus_one = BoundExpr {
        kind: BoundExprKind::Negate(Box::new(literal(1))),
        ty: int_type(),
        line: 1,
    };
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), minus_one)),
    );
    assert!(
        matches!(&planned, PhysicalPlan::IndexSeek { .. }),
        "expected an IndexSeek, got {planned:?}"
    );

    // WHERE a = CONVERT(int, @x)
    let converted = BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(variable("@x")),
            style: None,
            try_: false,
        },
        ty: int_type(),
        line: 1,
    };
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), converted)),
    );
    assert!(
        matches!(&planned, PhysicalPlan::IndexSeek { .. }),
        "expected an IndexSeek, got {planned:?}"
    );
}

#[test]
fn a_null_literal_is_a_bound_like_another() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    let null = BoundExpr {
        kind: BoundExprKind::Literal(Value::Null),
        ty: TypeInfo::new(SqlType::Int, true),
        line: 1,
    };
    let planned = plan_with(&catalog, filter(compare(CompareOp::Eq, column("a"), null)));
    let PhysicalPlan::IndexSeek { range, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    let KeyRangeExpr::Point(keys) = range else {
        panic!("expected a Point, got {range:?}")
    };
    assert!(matches!(&keys[0].kind, BoundExprKind::Literal(Value::Null)));
}

#[test]
fn an_or_at_the_top_is_not_split() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    // WHERE a = 1 OR a = 2
    let predicate = logical(
        LogicalOp::Or,
        compare(CompareOp::Eq, column("a"), literal(1)),
        compare(CompareOp::Eq, column("a"), literal(2)),
    );
    let planned = plan_with(&catalog, filter(predicate));
    assert_filter_over_scan(&planned);
}

#[test]
fn the_first_applicable_index_wins() {
    // Two indexes usable on `a`: the non-unique one is declared first and gets `IndexId(1)`.
    let catalog = FakeCatalog::new()
        .with_index(TABLE, &ascending(&["a", "b"]), false)
        .with_index(TABLE, &ascending(&["a"]), true);
    assert_eq!(catalog.indexes_of(TABLE)[0].0, IndexId(1));
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), literal(1))),
    );

    let PhysicalPlan::IndexSeek { index, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    assert_eq!(*index, IndexId(1));

    // Counter-proof: an index the predicate cannot use is skipped, whatever its position.
    let catalog = FakeCatalog::new()
        .with_index(TABLE, &ascending(&["c"]), false)
        .with_index(TABLE, &ascending(&["a"]), true);
    let planned = plan_with(
        &catalog,
        filter(compare(CompareOp::Eq, column("a"), literal(1))),
    );
    let PhysicalPlan::IndexSeek { index, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    assert_eq!(*index, IndexId(2));
}

#[test]
fn seek_keeps_the_output_schema() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["b"]), true);
    let logical = filter(compare(CompareOp::Eq, column("b"), literal(1)));
    let expected: Vec<String> = logical
        .schema()
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    let planned = plan_with(&catalog, logical);

    let PhysicalPlan::IndexSeek {
        columns, schema, ..
    } = &planned
    else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    let names: Vec<String> = schema
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    assert_eq!(names, expected);
    assert_eq!(names, vec!["a", "b", "c"]);
    let indexes: Vec<usize> = columns.iter().map(|column| column.index).collect();
    assert_eq!(indexes, vec![0, 1, 2]);
    assert_eq!(planned.schema().columns.len(), 3);
}

#[test]
fn a_seek_under_a_project_keeps_its_columns_in_place() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    let project = LogicalPlan::Project {
        input: Box::new(filter(compare(CompareOp::Eq, column("a"), literal(1)))),
        exprs: vec![vauban_binder::BoundProjection {
            expr: column("c"),
            name: "c".to_owned(),
        }],
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: "c".to_owned(),
                ty: int_type(),
            }],
        },
    };
    let planned = plan_with(&catalog, project);
    let PhysicalPlan::Project { input, .. } = &planned else {
        panic!("expected a Project, got {planned:?}")
    };
    let PhysicalPlan::IndexSeek { columns, .. } = input.as_ref() else {
        panic!("expected an IndexSeek under the Project, got {input:?}")
    };
    assert_eq!(columns[2].name, "c");
    assert_eq!(columns[2].index, 2);
}
