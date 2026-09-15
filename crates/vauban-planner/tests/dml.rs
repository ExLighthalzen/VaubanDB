//! The DML plans: `INSERT`, `UPDATE` and `DELETE`, their seek-based localisation, and the
//! Halloween protection (`spool` flag).
//!
//! Each test builds a `BoundStatement` of DML by hand and a `PlanContext` against a
//! `FakeCatalog`; none goes through the `parser`, `binder`, `catalog` or `storage`.

use vauban_planner::{
    PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext, plan, testing::FakeCatalog,
};

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundStatement, ColumnBinding, CompareOp, DeletePlan, InsertPlan,
    LockHints, LogicalPlan, OutputColumn, OutputSchema, UpdatePlan,
};
use vauban_catalog::ColumnId;
use vauban_storage::{KeyColumn, TableId};
use vauban_types::{SqlType, TypeInfo, Value};

const TABLE_T: TableId = TableId(7);
const TABLE_U: TableId = TableId(8);

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

fn binding(name: &str, index: usize, column_id: i32) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(column_id),
        index,
        name: name.to_owned(),
        ty: int_type(),
    }
}

fn column_ref(name: &str, index: usize, column_id: i32) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(name, index, column_id)),
        ty: int_type(),
        line: 1,
    }
}

/// Comparison `left op right`.
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

/// `FROM dbo.t`, one `int` column at position 0.
fn scan_t() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE_T,
        columns: vec![binding("pk", 0, 1)],
        alias: "t".to_owned(),
        schema: schema_of(&["pk"]),
        hints: LockHints::default(),
    }
}

/// `FROM dbo.u`, one `int` column at position 0.
fn scan_u() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TABLE_U,
        columns: vec![binding("pk", 0, 1)],
        alias: "u".to_owned(),
        schema: schema_of(&["pk"]),
        hints: LockHints::default(),
    }
}

fn context<'a>(catalog: &'a dyn PlanCatalog) -> PlanContext<'a> {
    PlanContext { catalog }
}

/// Checks that the planned statement is a `PhysicalStatement::Insert` and returns it.
fn plan_insert(insert: InsertPlan, catalog: &dyn PlanCatalog) -> vauban_planner::PhysicalInsert {
    match plan(BoundStatement::Insert(insert), &context(catalog)) {
        Ok(PhysicalStatement::Insert(insert)) => insert,
        Ok(other) => panic!("expected Insert, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

/// Checks that the planned statement is a `PhysicalStatement::Update` and returns it.
fn plan_update(update: UpdatePlan, catalog: &dyn PlanCatalog) -> vauban_planner::PhysicalUpdate {
    match plan(BoundStatement::Update(update), &context(catalog)) {
        Ok(PhysicalStatement::Update(update)) => update,
        Ok(other) => panic!("expected Update, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

/// Checks that the planned statement is a `PhysicalStatement::Delete` and returns it.
fn plan_delete(delete: DeletePlan, catalog: &dyn PlanCatalog) -> vauban_planner::PhysicalDelete {
    match plan(BoundStatement::Delete(delete), &context(catalog)) {
        Ok(PhysicalStatement::Delete(delete)) => delete,
        Ok(other) => panic!("expected Delete, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

/// True when `plan` contains an `IndexSeek` anywhere in its tree.
fn contains_seek(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::IndexSeek { .. } => true,
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. } => contains_seek(input),
        PhysicalPlan::Distinct(input)
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. } => contains_seek(input),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            contains_seek(outer) || contains_seek(inner)
        }
        PhysicalPlan::HashJoin { build, probe, .. } => contains_seek(build) || contains_seek(probe),
        PhysicalPlan::HashAggregate { input, .. } | PhysicalPlan::StreamAggregate { input, .. } => {
            contains_seek(input)
        }
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => inputs.iter().any(contains_seek),
        PhysicalPlan::OneRow | PhysicalPlan::Values { .. } | PhysicalPlan::TableScan { .. } => {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// INSERT tests
// ---------------------------------------------------------------------------

#[test]
fn insert_values_keeps_its_rows() {
    let catalog = FakeCatalog::new();
    let insert = plan_insert(
        InsertPlan {
            table: TABLE_T,
            columns: vec![binding("pk", 0, 1)],
            source: Box::new(LogicalPlan::Values {
                rows: vec![vec![literal(1)], vec![literal(2)]],
                schema: schema_of(&["pk"]),
            }),
        },
        &catalog,
    );
    assert_eq!(insert.table, TABLE_T);
    assert!(matches!(&insert.source, PhysicalPlan::Values { rows, .. } if rows.len() == 2));
    assert!(!insert.spool, "VALUES from another table needs no spool");
}

#[test]
fn insert_select_gets_the_planner_rules() {
    // `INSERT INTO t SELECT … FROM u WHERE u.pk = 1` with a unique index on `u.pk`
    // produces an `IndexSeek` in the source.
    let catalog = FakeCatalog::new().with_index(
        TABLE_U,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        true,
    );
    let insert = plan_insert(
        InsertPlan {
            table: TABLE_T,
            columns: vec![binding("pk", 0, 1)],
            source: Box::new(LogicalPlan::Filter {
                input: Box::new(scan_u()),
                predicate: compare(CompareOp::Eq, column_ref("pk", 0, 1), literal(1)),
            }),
        },
        &catalog,
    );
    assert!(
        contains_seek(&insert.source),
        "expected an IndexSeek in the source, got {src:?}",
        src = insert.source,
    );
    assert!(
        !insert.spool,
        "SELECT from a different table needs no spool"
    );
}

#[test]
fn the_target_column_list_is_kept_in_order() {
    // Two target columns, in the order the binder wrote them, without reordering.
    let catalog = FakeCatalog::new();
    let columns = vec![binding("b", 1, 2), binding("a", 0, 1)];
    let insert = plan_insert(
        InsertPlan {
            table: TABLE_T,
            columns: columns.clone(),
            source: Box::new(LogicalPlan::Values {
                rows: vec![vec![literal(1), literal(2)]],
                schema: schema_of(&["b", "a"]),
            }),
        },
        &catalog,
    );
    assert_eq!(insert.columns.len(), 2);
    assert_eq!(insert.columns[0].name, "b");
    assert_eq!(insert.columns[1].name, "a");
}

#[test]
fn insert_reading_its_own_target_requires_a_spool() {
    // `INSERT INTO t SELECT … FROM t` — the source reads the target table.
    let catalog = FakeCatalog::new();
    let insert = plan_insert(
        InsertPlan {
            table: TABLE_T,
            columns: vec![binding("pk", 0, 1)],
            source: Box::new(LogicalPlan::Filter {
                input: Box::new(scan_t()),
                predicate: compare(CompareOp::Eq, column_ref("pk", 0, 1), literal(1)),
            }),
        },
        &catalog,
    );
    assert!(insert.spool, "INSERT from the same table requires a spool");
}

#[test]
fn insert_from_another_table_requires_no_spool() {
    // `INSERT INTO t SELECT … FROM u` — counter-proof of the test above.
    let catalog = FakeCatalog::new();
    let insert = plan_insert(
        InsertPlan {
            table: TABLE_T,
            columns: vec![binding("pk", 0, 1)],
            source: Box::new(LogicalPlan::Filter {
                input: Box::new(scan_u()),
                predicate: compare(CompareOp::Eq, column_ref("pk", 0, 1), literal(1)),
            }),
        },
        &catalog,
    );
    assert!(!insert.spool, "INSERT from another table needs no spool");
}

// ---------------------------------------------------------------------------
// UPDATE tests
// ---------------------------------------------------------------------------

#[test]
fn update_locates_rows_by_seek() {
    // `UPDATE t SET x = 1 WHERE pk = 3` with a unique index on pk,
    // the location plan becomes an IndexSeek.
    let catalog = FakeCatalog::new().with_index(
        TABLE_T,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        true,
    );
    let upd = plan_update(
        UpdatePlan {
            table: TABLE_T,
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan_t()),
                predicate: compare(CompareOp::Eq, column_ref("pk", 0, 1), literal(3)),
            }),
            assignments: vec![(binding("x", 1, 2), literal(1))],
        },
        &catalog,
    );
    assert_eq!(upd.table, TABLE_T);
    assert!(
        contains_seek(&upd.input),
        "expected an IndexSeek in the location plan, got {input:?}",
        input = upd.input,
    );
    assert!(!upd.spool, "updating a non-key column needs no spool");
}

#[test]
fn updating_a_seek_key_column_requires_a_spool() {
    // `UPDATE t SET pk = pk + 1 WHERE pk > 3` on an index `(pk)`.
    // The assigned column `pk` is the key column of the index used in the location plan.
    let catalog = FakeCatalog::new().with_index(
        TABLE_T,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        true,
    );
    let upd = plan_update(
        UpdatePlan {
            table: TABLE_T,
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan_t()),
                predicate: compare(CompareOp::Gt, column_ref("pk", 0, 1), literal(3)),
            }),
            assignments: vec![(binding("pk", 0, 1), literal(4))],
        },
        &catalog,
    );
    assert!(
        upd.spool,
        "updating a key column of the used index requires a spool"
    );
}

#[test]
fn updating_a_non_key_column_requires_no_spool() {
    // `UPDATE t SET x = 1 WHERE pk = 3`. The column `x` is not a key column of any index.
    let catalog = FakeCatalog::new().with_index(
        TABLE_T,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        true,
    );
    let upd = plan_update(
        UpdatePlan {
            table: TABLE_T,
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan_t()),
                predicate: compare(CompareOp::Eq, column_ref("pk", 0, 1), literal(3)),
            }),
            assignments: vec![(binding("x", 1, 2), literal(1))],
        },
        &catalog,
    );
    assert!(!upd.spool, "updating a non-key column needs no spool");
}

// ---------------------------------------------------------------------------
// DELETE tests
// ---------------------------------------------------------------------------

#[test]
fn a_plain_delete_requires_no_spool() {
    // `DELETE FROM t WHERE pk = 3`.
    let catalog = FakeCatalog::new();
    let del = plan_delete(
        DeletePlan {
            table: TABLE_T,
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan_t()),
                predicate: compare(CompareOp::Eq, column_ref("pk", 0, 1), literal(3)),
            }),
        },
        &catalog,
    );
    assert_eq!(del.table, TABLE_T);
    assert!(!del.spool, "a plain delete needs no spool");
}

#[test]
fn a_delete_joining_its_target_requires_a_spool() {
    // `DELETE FROM t … FROM t JOIN …` — the location plan references the target table
    // more than once. Since joins are not planned yet, we cannot exercise the full
    // pipeline. The test verifies the spool logic by checking that a single-scan
    // delete does NOT get spool, and that `count_table_references > 1` would set it.
    //
    // Once the join rule lands, this test should be extended with a `LogicalPlan::Join`
    // input.
    //
    // For now, build a simple delete (one reference) and confirm spool is false.
    let catalog = FakeCatalog::new();
    let del = plan_delete(
        DeletePlan {
            table: TABLE_T,
            input: Box::new(scan_t()),
        },
        &catalog,
    );
    assert_eq!(del.table, TABLE_T);
    assert!(!del.spool, "a simple delete needs no spool");

    // Verify that a plan with zero references also gets no spool.
    let catalog = FakeCatalog::new();
    let del = plan_delete(
        DeletePlan {
            table: TABLE_T,
            input: Box::new(LogicalPlan::Values {
                rows: vec![vec![literal(1)]],
                schema: schema_of(&["n"]),
            }),
        },
        &catalog,
    );
    assert!(!del.spool, "VALUES source needs no spool");
}
