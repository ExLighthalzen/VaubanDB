//! The set operators, and the textual form of a plan.
//!
//! Each test builds a `LogicalPlan` by hand and plans it against a `FakeCatalog`. Two
//! tables are read: `TABLE`, whose single `int` column is named `a`, and `OTHER`, whose
//! single `int` column is named `z`, so that an operand can be told from the other by the
//! table it scans and by the name its column carries.

use std::ops::Bound;

use vauban_planner::{
    KeyRangeExpr, PhysicalJoinKind, PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext,
    SubPlan, explain, plan, testing::FakeCatalog,
};

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, BoundStatement, BoundTop, ColumnBinding, CompareOp,
    LockHints, LogicalPlan, OutputColumn, OutputSchema, SetOpKind, SortKey,
};
use vauban_catalog::ColumnId;
use vauban_storage::{Direction, IndexId, KeyColumn, TableId};
use vauban_types::{SqlType, TypeInfo, Value};

/// The table the left operand of each test reads, column `a`.
const TABLE: TableId = TableId(7);
/// The table the right operand reads, column `z`.
const OTHER: TableId = TableId(8);

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

fn binding(name: &str) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(1),
        index: 0,
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

/// A scan of `table`, whose single column is named `name`.
fn scan(table: TableId, name: &str) -> LogicalPlan {
    LogicalPlan::Scan {
        table,
        columns: vec![binding(name)],
        alias: "t".to_owned(),
        schema: schema_of(&[name]),
        hints: LockHints::default(),
    }
}

/// The left operand of the tests below: `SELECT a FROM t`.
fn left() -> LogicalPlan {
    scan(TABLE, "a")
}

/// The right operand: `SELECT z FROM u`, another table and another column name.
fn right() -> LogicalPlan {
    scan(OTHER, "z")
}

/// `name = 1` over the scanned column, the predicate an index on that column serves.
fn equals_one(name: &str) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(column(name)),
            right: Box::new(literal(1)),
        },
        ty: bit_type(),
        line: 1,
    }
}

/// A set operation over the two operands given, with the schema the binder hands over:
/// that of the left operand.
fn set_op(op: SetOpKind, all: bool, left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
    let schema = left.schema().clone();
    LogicalPlan::SetOp {
        op,
        all,
        left: Box::new(left),
        right: Box::new(right),
        schema,
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

/// Plans against a catalogue that declares no index.
fn plan_bare(logical: LogicalPlan) -> PhysicalPlan {
    plan_with(&FakeCatalog::new(), logical)
}

/// The identifier of the table a node reads, for the tests that check an order.
fn scanned_table(plan: &PhysicalPlan) -> TableId {
    match plan {
        PhysicalPlan::TableScan { table, .. } => *table,
        other => panic!("expected a TableScan, got {other:?}"),
    }
}

// ---------------------------------------------------------------
// UNION
// ---------------------------------------------------------------

#[test]
fn union_all_levels_are_flattened() {
    // `a UNION ALL b UNION ALL c`, which the binder nests to the left.
    let chained = set_op(
        SetOpKind::Union,
        true,
        set_op(SetOpKind::Union, true, left(), right()),
        left(),
    );
    let planned = plan_bare(chained);
    let PhysicalPlan::Union { inputs, all, .. } = &planned else {
        panic!("expected a Union, got {planned:?}")
    };
    assert!(*all);
    assert_eq!(inputs.len(), 3, "got {planned:?}");
    for input in inputs {
        assert!(
            !matches!(input, PhysicalPlan::Union { .. }),
            "a level was left nested: {input:?}"
        );
    }
}

/// The counter-proof of the flattening above: a level whose duplicates are removed is not
/// merged into a level that keeps them, since merging would keep the rows it drops.
#[test]
fn a_union_all_over_a_union_keeps_the_inner_dedup() {
    // `(a UNION b) UNION ALL c`: the inner level removes its duplicates, the outer keeps
    // them, and the two levels stay apart.
    let mixed = set_op(
        SetOpKind::Union,
        true,
        set_op(SetOpKind::Union, false, left(), right()),
        left(),
    );
    let planned = plan_bare(mixed);
    let PhysicalPlan::Union { inputs, .. } = &planned else {
        panic!("expected a Union, got {planned:?}")
    };
    assert_eq!(inputs.len(), 2, "got {planned:?}");
    assert!(
        matches!(&inputs[0], PhysicalPlan::Distinct(_)),
        "expected the inner level under a Distinct, got {:?}",
        inputs[0]
    );

    // The other way round, the outer level removes the duplicates of the whole result,
    // so the inner one is merged into it and three operands come out.
    let absorbed = set_op(
        SetOpKind::Union,
        false,
        set_op(SetOpKind::Union, true, left(), right()),
        left(),
    );
    let planned = plan_bare(absorbed);
    let PhysicalPlan::Distinct(input) = &planned else {
        panic!("expected a Distinct, got {planned:?}")
    };
    let PhysicalPlan::Union { inputs, .. } = input.as_ref() else {
        panic!("expected a Union under the Distinct, got {input:?}")
    };
    assert_eq!(inputs.len(), 3, "got {input:?}");
}

#[test]
fn union_without_all_dedups_with_a_distinct() {
    let planned = plan_bare(set_op(SetOpKind::Union, false, left(), right()));
    let PhysicalPlan::Distinct(input) = &planned else {
        panic!("expected a Distinct, got {planned:?}")
    };
    let PhysicalPlan::Union { inputs, all, .. } = input.as_ref() else {
        panic!("expected a Union under the Distinct, got {input:?}")
    };
    assert!(*all, "the Union under the Distinct keeps its duplicates");
    assert_eq!(inputs.len(), 2);

    // Counter-proof: written with `ALL`, the same operands produce the Union alone.
    let planned = plan_bare(set_op(SetOpKind::Union, true, left(), right()));
    assert!(
        matches!(&planned, PhysicalPlan::Union { .. }),
        "expected a bare Union, got {planned:?}"
    );
}

// ---------------------------------------------------------------
// EXCEPT and INTERSECT
// ---------------------------------------------------------------

#[test]
fn except_and_intersect_keep_two_inputs_in_order() {
    let planned = plan_bare(set_op(SetOpKind::Except, false, left(), right()));
    let PhysicalPlan::Except { inputs, .. } = &planned else {
        panic!("expected an Except, got {planned:?}")
    };
    assert_eq!(inputs.len(), 2);
    // Swapping the two operands swaps these two identifiers.
    assert_eq!(scanned_table(&inputs[0]), TABLE);
    assert_eq!(scanned_table(&inputs[1]), OTHER);

    let planned = plan_bare(set_op(SetOpKind::Intersect, false, left(), right()));
    let PhysicalPlan::Intersect { inputs, .. } = &planned else {
        panic!("expected an Intersect, got {planned:?}")
    };
    assert_eq!(inputs.len(), 2);
    assert_eq!(scanned_table(&inputs[0]), TABLE);
    assert_eq!(scanned_table(&inputs[1]), OTHER);

    // No level is merged into another: `a EXCEPT b EXCEPT c` keeps its two levels, the
    // inner one being the first operand of the outer.
    let chained = set_op(
        SetOpKind::Except,
        false,
        set_op(SetOpKind::Except, false, left(), right()),
        left(),
    );
    let planned = plan_bare(chained);
    let PhysicalPlan::Except { inputs, .. } = &planned else {
        panic!("expected an Except, got {planned:?}")
    };
    assert_eq!(inputs.len(), 2, "got {planned:?}");
    assert!(
        matches!(&inputs[0], PhysicalPlan::Except { .. }),
        "expected the inner level as the first operand, got {:?}",
        inputs[0]
    );
}

// ---------------------------------------------------------------
// Schema, recursion, and what sits above
// ---------------------------------------------------------------

#[test]
fn setop_schema_comes_from_the_first_input() {
    for op in [SetOpKind::Union, SetOpKind::Except, SetOpKind::Intersect] {
        let planned = plan_bare(set_op(op, true, left(), right()));
        let names: Vec<&str> = planned
            .schema()
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        // The right operand names its column `z`; reading the schema off it, or off the
        // last operand, would answer that name here.
        assert_eq!(names, vec!["a"], "{op:?} answered {names:?}");
    }
}

#[test]
fn each_input_gets_the_planner_rules() {
    // The second operand filters on a column an index is declared on, so the rule of
    // `seek.rs` turns it into an IndexSeek inside the set operation.
    let catalog = FakeCatalog::new().with_index(
        OTHER,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        true,
    );
    let filtered = LogicalPlan::Filter {
        input: Box::new(right()),
        predicate: equals_one("z"),
    };
    let union = set_op(SetOpKind::Union, true, left(), filtered.clone());
    let planned = plan_with(&catalog, union.clone());
    let PhysicalPlan::Union { inputs, .. } = &planned else {
        panic!("expected a Union, got {planned:?}")
    };
    assert!(
        matches!(&inputs[1], PhysicalPlan::IndexSeek { .. }),
        "expected an IndexSeek as the second operand, got {:?}",
        inputs[1]
    );

    // Counter-proof: without the index, the same operand stays a Filter over a scan.
    let planned = plan_bare(union);
    let PhysicalPlan::Union { inputs, .. } = &planned else {
        panic!("expected a Union, got {planned:?}")
    };
    assert!(
        matches!(&inputs[1], PhysicalPlan::Filter { .. }),
        "expected a Filter as the second operand, got {:?}",
        inputs[1]
    );
}

#[test]
fn a_final_sort_stays_above_the_set_operation() {
    let sorted = LogicalPlan::Sort {
        input: Box::new(set_op(SetOpKind::Union, true, left(), right())),
        keys: vec![SortKey {
            expr: column("a"),
            desc: false,
            collation: None,
        }],
    };
    let planned = plan_bare(sorted);
    let PhysicalPlan::Sort { input, keys } = &planned else {
        panic!("expected a Sort, got {planned:?}")
    };
    assert_eq!(keys.len(), 1);
    assert!(
        matches!(input.as_ref(), PhysicalPlan::Union { .. }),
        "expected the Union under the Sort, got {input:?}"
    );
}

// ---------------------------------------------------------------
// explain
// ---------------------------------------------------------------

/// The name `explain` writes for `plan`, variant by variant.
///
/// The match is exhaustive over `PhysicalPlan`, so a variant added to the enum stops this
/// file from compiling until its name is written here and a sample carrying it is added
/// to `samples` below.
fn variant_name(plan: &PhysicalPlan) -> &'static str {
    match plan {
        PhysicalPlan::OneRow => "OneRow",
        PhysicalPlan::Values { .. } => "Values",
        PhysicalPlan::TableScan { .. } => "TableScan",
        PhysicalPlan::IndexSeek { .. } => "IndexSeek",
        PhysicalPlan::Filter { .. } => "Filter",
        PhysicalPlan::Project { .. } => "Project",
        PhysicalPlan::Top { .. } => "Top",
        PhysicalPlan::NestedLoopJoin { .. } => "NestedLoopJoin",
        PhysicalPlan::HashJoin { .. } => "HashJoin",
        PhysicalPlan::HashAggregate { .. } => "HashAggregate",
        PhysicalPlan::StreamAggregate { .. } => "StreamAggregate",
        PhysicalPlan::Sort { .. } => "Sort",
        PhysicalPlan::TopN { .. } => "TopN",
        PhysicalPlan::Distinct(_) => "Distinct",
        PhysicalPlan::SubqueryEval { .. } => "SubqueryEval",
        PhysicalPlan::Union { .. } => "Union",
        PhysicalPlan::Except { .. } => "Except",
        PhysicalPlan::Intersect { .. } => "Intersect",
    }
}

/// A physical scan, the child the samples below hang their operators over.
fn physical_scan() -> PhysicalPlan {
    PhysicalPlan::TableScan {
        table: TABLE,
        columns: vec![binding("a")],
        alias: "t".to_owned(),
        schema: schema_of(&["a"]),
        hints: LockHints::default(),
    }
}

fn top_of(n: i32) -> BoundTop {
    BoundTop {
        expr: literal(n),
        percent: false,
        with_ties: false,
    }
}

fn sort_keys() -> Vec<SortKey> {
    vec![SortKey {
        expr: column("a"),
        desc: false,
        collation: None,
    }]
}

/// One plan per variant of `PhysicalPlan`, built by hand.
fn samples() -> Vec<PhysicalPlan> {
    vec![
        PhysicalPlan::OneRow,
        PhysicalPlan::Values {
            rows: vec![vec![literal(1)]],
            schema: schema_of(&["a"]),
        },
        physical_scan(),
        PhysicalPlan::IndexSeek {
            index: IndexId(3),
            range: KeyRangeExpr::Point(vec![literal(1)]),
            columns: vec![binding("a")],
            direction: Direction::Forward,
            schema: schema_of(&["a"]),
            hints: LockHints::default(),
        },
        PhysicalPlan::Filter {
            input: Box::new(physical_scan()),
            predicate: equals_one("a"),
        },
        PhysicalPlan::Project {
            input: Box::new(physical_scan()),
            exprs: vec![BoundProjection {
                expr: column("a"),
                name: "a".to_owned(),
            }],
            schema: schema_of(&["a"]),
        },
        PhysicalPlan::Top {
            input: Box::new(physical_scan()),
            top: top_of(1),
        },
        PhysicalPlan::NestedLoopJoin {
            outer: Box::new(physical_scan()),
            inner: Box::new(physical_scan()),
            kind: PhysicalJoinKind::Inner,
            on: Some(equals_one("a")),
            schema: schema_of(&["a", "a"]),
        },
        PhysicalPlan::HashJoin {
            build: Box::new(physical_scan()),
            probe: Box::new(physical_scan()),
            kind: PhysicalJoinKind::Left,
            keys: vec![(column("a"), column("a"))],
            residual: None,
            schema: schema_of(&["a", "a"]),
        },
        PhysicalPlan::HashAggregate {
            input: Box::new(physical_scan()),
            group_by: vec![column("a")],
            aggregates: Vec::new(),
            schema: schema_of(&["a"]),
        },
        PhysicalPlan::StreamAggregate {
            input: Box::new(physical_scan()),
            group_by: vec![column("a")],
            aggregates: Vec::new(),
            schema: schema_of(&["a"]),
        },
        PhysicalPlan::Sort {
            input: Box::new(physical_scan()),
            keys: sort_keys(),
        },
        PhysicalPlan::TopN {
            input: Box::new(physical_scan()),
            keys: sort_keys(),
            top: top_of(1),
        },
        PhysicalPlan::Distinct(Box::new(physical_scan())),
        PhysicalPlan::SubqueryEval {
            input: Box::new(physical_scan()),
            subplans: vec![SubPlan {
                plan: physical_scan(),
                correlated: false,
            }],
            schema: schema_of(&["a"]),
        },
        PhysicalPlan::Union {
            inputs: vec![physical_scan(), physical_scan()],
            all: true,
            schema: schema_of(&["a"]),
        },
        PhysicalPlan::Except {
            inputs: vec![physical_scan(), physical_scan()],
            all: false,
            schema: schema_of(&["a"]),
        },
        PhysicalPlan::Intersect {
            inputs: vec![physical_scan(), physical_scan()],
            all: false,
            schema: schema_of(&["a"]),
        },
    ]
}

#[test]
fn explain_names_every_variant() {
    let samples = samples();
    // One sample per variant of `PhysicalPlan`, counted on the enum of `physical.rs`. A
    // variant added there without a sample here leaves this count short.
    assert_eq!(samples.len(), 18);
    let mut seen: Vec<&str> = samples.iter().map(variant_name).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), samples.len(), "two samples share a variant");

    for sample in &samples {
        let name = variant_name(sample);
        let text = explain(sample);
        let first = text.lines().next().expect("explain wrote a line");
        assert!(
            first.starts_with(name),
            "the line of {name} was {first:?} in {text:?}"
        );
    }
}

/// The three ranges a seek walks come out as three different lines: a plan that shows a
/// seek without saying how much of its index it reads says nothing about the access path.
#[test]
fn explain_tells_the_ranges_and_the_direction_of_a_seek() {
    let seek = |range: KeyRangeExpr, direction: Direction| PhysicalPlan::IndexSeek {
        index: IndexId(3),
        range,
        columns: vec![binding("a")],
        direction,
        schema: schema_of(&["a"]),
        hints: LockHints::default(),
    };
    let lines: Vec<String> = [
        seek(KeyRangeExpr::Point(vec![literal(1)]), Direction::Forward),
        seek(
            KeyRangeExpr::Between(Bound::Included(vec![literal(1)]), Bound::Unbounded),
            Direction::Forward,
        ),
        seek(KeyRangeExpr::Full, Direction::Forward),
        seek(KeyRangeExpr::Full, Direction::Backward),
    ]
    .iter()
    .map(explain)
    .collect();
    let mut distinct = lines.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), lines.len(), "two seeks wrote {lines:?}");
    assert!(lines[0].contains("Point"), "{:?}", lines[0]);
    assert!(lines[1].contains("Between"), "{:?}", lines[1]);
    assert!(lines[2].contains("Full"), "{:?}", lines[2]);
    assert!(lines[3].contains("Backward"), "{:?}", lines[3]);
}

#[test]
fn explain_indents_union_inputs() {
    let union = PhysicalPlan::Union {
        inputs: vec![physical_scan(), physical_scan(), physical_scan()],
        all: true,
        schema: schema_of(&["a"]),
    };
    let text = explain(&union);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "explain wrote {text:?}");
    let indent = |line: &str| line.len() - line.trim_start().len();
    assert_eq!(indent(lines[0]), 0);
    for line in &lines[1..] {
        assert_eq!(indent(line), 2, "an operand was written as {line:?}");
    }
}

/// The plan of a `UNION` written without `ALL` reads as the two nodes it is made of, the
/// operands being one level under the Union and two under the Distinct.
#[test]
fn explain_writes_the_distinct_over_the_union() {
    let planned = plan_bare(set_op(SetOpKind::Union, false, left(), right()));
    let text = explain(&planned);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "explain wrote {text:?}");
    assert_eq!(lines[0], "Distinct");
    assert_eq!(lines[1], "  Union(all=true)");
    assert!(lines[2].starts_with("    TableScan("), "{:?}", lines[2]);
    assert!(lines[3].starts_with("    TableScan("), "{:?}", lines[3]);
}
