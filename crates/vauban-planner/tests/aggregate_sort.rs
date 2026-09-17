//! The aggregate and sort planning rules: `HashAggregate` / `StreamAggregate`, sort
//! elimination by index order, `TopN` fusion, and `Distinct`.
//!
//! Each test builds a `LogicalPlan` by hand and plans it against a `FakeCatalog`. The
//! table read has three `int` columns, `a`, `b` and `c`, at storage positions 0, 1 and 2.

use vauban_planner::{
    PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext, plan, testing::FakeCatalog,
};

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundStatement, BoundTop, ColumnBinding, CompareOp, LockHints,
    LogicalPlan, OutputColumn, OutputSchema, SortKey,
};
use vauban_catalog::ColumnId;
use vauban_storage::{Direction, KeyColumn, TableId};
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

fn schema_of(names: &[&str]) -> OutputSchema {
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

fn scan() -> LogicalPlan {
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
        hints: LockHints::default(),
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

// ---------------------------------------------------------------
// Aggregate tests
// ---------------------------------------------------------------

#[test]
fn group_by_over_an_unordered_input_is_a_hash_aggregate() {
    let aggregate = LogicalPlan::Aggregate {
        input: Box::new(scan()),
        group_by: vec![column("a")],
        aggregates: Vec::new(),
        schema: schema_of(&["a"]),
    };
    let planned = plan_with(&FakeCatalog::new(), aggregate);
    let PhysicalPlan::HashAggregate {
        group_by,
        aggregates,
        ..
    } = &planned
    else {
        panic!("expected HashAggregate, got {planned:?}")
    };
    assert_eq!(group_by.len(), 1);
    assert!(aggregates.is_empty());
}

#[test]
fn group_by_over_an_index_order_is_a_stream_aggregate() {
    // Aggregate { group_by: [a] } over Filter { a = 1, Scan }
    // With an index on (a, b), the filter becomes an IndexSeek and the aggregate
    // is streamed because the seek delivers (a, b).
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a", "b"]), false);
    let filter = LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: compare(CompareOp::Eq, column("a"), literal(1)),
    };
    let aggregate = LogicalPlan::Aggregate {
        input: Box::new(filter),
        group_by: vec![column("a")],
        aggregates: Vec::new(),
        schema: schema_of(&["a"]),
    };
    let planned = plan_with(&catalog, aggregate.clone());
    assert!(
        matches!(&planned, PhysicalPlan::StreamAggregate { .. }),
        "expected StreamAggregate, got {planned:?}"
    );

    // Counter-proof: without the index, the same plan becomes a HashAggregate.
    let planned = plan_with(&FakeCatalog::new(), aggregate);
    assert!(
        matches!(&planned, PhysicalPlan::HashAggregate { .. }),
        "expected HashAggregate, got {planned:?}"
    );
}

#[test]
fn an_aggregate_without_group_by_keeps_an_empty_group_list() {
    let aggregate = LogicalPlan::Aggregate {
        input: Box::new(scan()),
        group_by: Vec::new(),
        aggregates: Vec::new(),
        schema: schema_of(&[]),
    };
    let planned = plan_with(&FakeCatalog::new(), aggregate);
    let PhysicalPlan::HashAggregate { group_by, .. } = &planned else {
        panic!("expected HashAggregate, got {planned:?}")
    };
    assert!(group_by.is_empty());
}

#[test]
fn a_group_by_on_a_non_prefix_column_is_a_hash_aggregate() {
    // Aggregate { group_by: [b] } over Filter { a = 1, Scan } with index (a).
    // The seek delivers (a), not (b), so HashAggregate.
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), false);
    let filter = LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: compare(CompareOp::Eq, column("a"), literal(1)),
    };
    let aggregate = LogicalPlan::Aggregate {
        input: Box::new(filter),
        group_by: vec![column("b")],
        aggregates: Vec::new(),
        schema: schema_of(&["b"]),
    };
    let planned = plan_with(&catalog, aggregate);
    assert!(
        matches!(&planned, PhysicalPlan::HashAggregate { .. }),
        "expected HashAggregate, got {planned:?}"
    );
}

#[test]
fn a_group_by_in_another_order_than_the_index_is_a_hash_aggregate() {
    // Aggregate { group_by: [b, a] } over Filter { a = 1, Scan } with index (a, b).
    // The seek delivers (a, b), so the keys arrive in the other order than the one the
    // grouping names, and the aggregate is hashed.
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a", "b"]), false);
    let filter = LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: compare(CompareOp::Eq, column("a"), literal(1)),
    };
    let aggregate = LogicalPlan::Aggregate {
        input: Box::new(filter),
        group_by: vec![column("b"), column("a")],
        aggregates: Vec::new(),
        schema: schema_of(&["b", "a"]),
    };
    let planned = plan_with(&catalog, aggregate);
    assert!(
        matches!(&planned, PhysicalPlan::HashAggregate { .. }),
        "expected HashAggregate, got {planned:?}"
    );

    // Counter-proof: the same two keys in the order the seek delivers them do stream.
    let filter = LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: compare(CompareOp::Eq, column("a"), literal(1)),
    };
    let aggregate = LogicalPlan::Aggregate {
        input: Box::new(filter),
        group_by: vec![column("a"), column("b")],
        aggregates: Vec::new(),
        schema: schema_of(&["a", "b"]),
    };
    let planned = plan_with(&catalog, aggregate);
    assert!(
        matches!(&planned, PhysicalPlan::StreamAggregate { .. }),
        "expected StreamAggregate, got {planned:?}"
    );
}

// ---------------------------------------------------------------
// Sort tests
// ---------------------------------------------------------------

#[test]
fn a_sort_the_index_already_delivers_is_dropped() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a", "b"]), false);
    let filter = LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: compare(CompareOp::Eq, column("a"), literal(1)),
    };
    let sort = LogicalPlan::Sort {
        input: Box::new(filter),
        keys: vec![SortKey {
            expr: column("a"),
            desc: false,
            collation: None,
        }],
    };
    let planned = plan_with(&catalog, sort);
    // The sort should be gone; what remains is just the IndexSeek (a Filter if
    // residual, or the bare seek).
    assert!(
        !matches!(&planned, PhysicalPlan::Sort { .. }),
        "expected no Sort node, got {planned:?}"
    );
}

#[test]
fn a_descending_sort_flips_the_seek_direction() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), true);
    let filter = LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: compare(CompareOp::Eq, column("a"), literal(1)),
    };
    let sort = LogicalPlan::Sort {
        input: Box::new(filter),
        keys: vec![SortKey {
            expr: column("a"),
            desc: true,
            collation: None,
        }],
    };
    let planned = plan_with(&catalog, sort);
    let PhysicalPlan::IndexSeek { direction, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    assert_eq!(*direction, Direction::Backward);
}

#[test]
fn a_partial_reversal_keeps_the_sort() {
    // With an index on (a, b), `ORDER BY a DESC, b` is neither the order the seek
    // delivers nor its opposite: reading the index backwards would give `b DESC`.
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a", "b"]), false);
    let sort_over_seek = |second_desc: bool| LogicalPlan::Sort {
        input: Box::new(LogicalPlan::Filter {
            input: Box::new(scan()),
            predicate: compare(CompareOp::Eq, column("a"), literal(1)),
        }),
        keys: vec![
            SortKey {
                expr: column("a"),
                desc: true,
                collation: None,
            },
            SortKey {
                expr: column("b"),
                desc: second_desc,
                collation: None,
            },
        ],
    };
    let planned = plan_with(&catalog, sort_over_seek(false));
    assert!(
        matches!(&planned, PhysicalPlan::Sort { .. }),
        "expected a Sort node, got {planned:?}"
    );

    // Counter-proof: both keys reversed is the opposite of the whole index order, and
    // the seek is turned round instead.
    let planned = plan_with(&catalog, sort_over_seek(true));
    let PhysicalPlan::IndexSeek { direction, .. } = &planned else {
        panic!("expected an IndexSeek, got {planned:?}")
    };
    assert_eq!(*direction, Direction::Backward);
}

#[test]
fn a_sort_on_another_column_is_kept() {
    let catalog = FakeCatalog::new().with_index(TABLE, &ascending(&["a"]), false);
    let filter = LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: compare(CompareOp::Eq, column("a"), literal(1)),
    };
    let sort = LogicalPlan::Sort {
        input: Box::new(filter),
        keys: vec![SortKey {
            expr: column("c"),
            desc: false,
            collation: None,
        }],
    };
    let planned = plan_with(&catalog, sort);
    assert!(
        matches!(&planned, PhysicalPlan::Sort { .. }),
        "expected a Sort node, got {planned:?}"
    );
}

#[test]
fn a_table_scan_delivers_no_order() {
    let sort = LogicalPlan::Sort {
        input: Box::new(scan()),
        keys: vec![SortKey {
            expr: column("a"),
            desc: false,
            collation: None,
        }],
    };
    let planned = plan_with(&FakeCatalog::new(), sort);
    assert!(
        matches!(&planned, PhysicalPlan::Sort { .. }),
        "expected a Sort node, got {planned:?}"
    );
}

// ---------------------------------------------------------------
// TopN / Top tests
// ---------------------------------------------------------------

#[test]
fn top_over_a_sort_becomes_a_top_n() {
    let sort = LogicalPlan::Sort {
        input: Box::new(scan()),
        keys: vec![SortKey {
            expr: column("a"),
            desc: false,
            collation: None,
        }],
    };
    let limit = LogicalPlan::Limit {
        input: Box::new(sort),
        top: BoundTop {
            expr: literal(10),
            percent: false,
            with_ties: false,
        },
    };
    let planned = plan_with(&FakeCatalog::new(), limit);
    let PhysicalPlan::TopN {
        top, keys, input, ..
    } = &planned
    else {
        panic!("expected TopN, got {planned:?}")
    };
    assert!(matches!(
        top.expr.kind,
        BoundExprKind::Literal(Value::I32(10))
    ));
    assert_eq!(keys.len(), 1);
    let PhysicalPlan::TableScan { .. } = input.as_ref() else {
        panic!("expected TableScan under TopN, got {input:?}")
    };
}

#[test]
fn top_without_order_by_stays_a_plain_top() {
    let limit = LogicalPlan::Limit {
        input: Box::new(scan()),
        top: BoundTop {
            expr: literal(10),
            percent: false,
            with_ties: false,
        },
    };
    let planned = plan_with(&FakeCatalog::new(), limit);
    let PhysicalPlan::Top { top, input } = &planned else {
        panic!("expected Top, got {planned:?}")
    };
    assert!(matches!(
        top.expr.kind,
        BoundExprKind::Literal(Value::I32(10))
    ));
    let PhysicalPlan::TableScan { .. } = input.as_ref() else {
        panic!("expected TableScan under Top, got {input:?}")
    };
}

// ---------------------------------------------------------------
// Distinct tests
// ---------------------------------------------------------------

#[test]
fn distinct_is_a_distinct_node() {
    let distinct = LogicalPlan::Distinct(Box::new(scan()));
    let planned = plan_with(&FakeCatalog::new(), distinct);
    let PhysicalPlan::Distinct(input) = &planned else {
        panic!("expected Distinct, got {planned:?}")
    };
    let PhysicalPlan::TableScan { .. } = input.as_ref() else {
        panic!("expected TableScan under Distinct, got {input:?}")
    };
}
