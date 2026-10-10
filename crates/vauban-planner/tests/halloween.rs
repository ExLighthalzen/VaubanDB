//! Halloween protection through subqueries: the `spool` flag of a write statement whose
//! source reads the target table inside a subquery the planner keeps correlated, and so
//! evaluates through a `SubqueryEval`.
//!
//! Each test builds a bound statement by hand and plans it against a `FakeCatalog`. Each
//! test that expects a spool has a twin that reads another table and expects none.

use vauban_planner::{
    PhysicalDelete, PhysicalInsert, PhysicalPlan, PhysicalStatement, PhysicalUpdate, PlanContext,
    plan, testing::FakeCatalog,
};

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, BoundStatement, BoundSubquery, ColumnBinding,
    CompareOp, DeletePlan, InsertPlan, LockHints, LogicalPlan, OutputColumn, OutputSchema,
    UpdatePlan,
};
use vauban_catalog::ColumnId;
use vauban_storage::TableId;
use vauban_types::{SqlType, TypeInfo, Value};

/// The target of every statement.
const TARGET: TableId = TableId(7);
/// Another table, read where the twin of a test reads the target.
const OTHER: TableId = TableId(8);

fn int_type() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

fn bit_type() -> TypeInfo {
    TypeInfo::new(SqlType::Bit, false)
}

/// Column `k` (identifier 1, position 0), read `depth` queries up.
fn k(depth: u8) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(1),
        index: 0,
        name: "k".to_owned(),
        ty: int_type(),
        depth,
    }
}

/// Column `v` (identifier 2, position 1) of the current query.
fn v() -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(2),
        index: 1,
        name: "v".to_owned(),
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

fn exists(inner: LogicalPlan, negated: bool) -> BoundExpr {
    let exists = BoundExpr {
        kind: BoundExprKind::Exists(BoundSubquery::new(inner)),
        ty: bit_type(),
        line: 1,
    };
    if negated {
        BoundExpr {
            kind: BoundExprKind::Not(Box::new(exists)),
            ty: bit_type(),
            line: 1,
        }
    } else {
        exists
    }
}

fn scalar(inner: LogicalPlan) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ScalarSubquery(BoundSubquery::new(inner)),
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

/// `FROM <table>`, two `int` columns `k` and `v`.
fn scan(table: TableId) -> LogicalPlan {
    LogicalPlan::Scan {
        table,
        columns: vec![k(0), v()],
        alias: format!("t{}", table.0),
        schema: schema_of(&["k", "v"]),
        hints: LockHints::default(),
    }
}

fn filter(input: LogicalPlan, predicate: BoundExpr) -> LogicalPlan {
    LogicalPlan::Filter {
        input: Box::new(input),
        predicate,
    }
}

fn project(input: LogicalPlan, expr: BoundExpr) -> LogicalPlan {
    LogicalPlan::Project {
        input: Box::new(input),
        exprs: vec![BoundProjection {
            expr,
            name: "x".to_owned(),
        }],
        schema: schema_of(&["x"]),
    }
}

/// `SELECT 1 FROM <table> WHERE <table>.k = outer.k`: a subquery correlated on the `k`
/// of the query one level up, which the planner evaluates through a `SubqueryEval`.
fn correlated_on_k(table: TableId) -> LogicalPlan {
    project(
        filter(
            scan(table),
            compare(CompareOp::Eq, column(k(0)), column(k(1))),
        ),
        literal(1),
    )
}

fn context(catalog: &FakeCatalog) -> PlanContext<'_> {
    PlanContext { catalog }
}

fn plan_insert(source: LogicalPlan) -> PhysicalInsert {
    let catalog = FakeCatalog::new();
    let stmt = BoundStatement::Insert(InsertPlan {
        target: Default::default(),
        table: TARGET,
        columns: vec![k(0)],
        source: Box::new(source),
    });
    match plan(stmt, &context(&catalog)) {
        Ok(PhysicalStatement::Insert(insert)) => insert,
        Ok(other) => panic!("expected Insert, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

fn plan_update(input: LogicalPlan, value: BoundExpr) -> PhysicalUpdate {
    let catalog = FakeCatalog::new();
    let stmt = BoundStatement::Update(UpdatePlan {
        target: Default::default(),
        table: TARGET,
        input: Box::new(input),
        assignments: vec![(v(), value)],
    });
    match plan(stmt, &context(&catalog)) {
        Ok(PhysicalStatement::Update(update)) => update,
        Ok(other) => panic!("expected Update, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

fn plan_delete(input: LogicalPlan) -> PhysicalDelete {
    let catalog = FakeCatalog::new();
    let stmt = BoundStatement::Delete(DeletePlan {
        table: TARGET,
        input: Box::new(input),
    });
    match plan(stmt, &context(&catalog)) {
        Ok(PhysicalStatement::Delete(delete)) => delete,
        Ok(other) => panic!("expected Delete, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

/// True when a `SubqueryEval` sits somewhere under the projections and filters of `plan`:
/// the shape these tests are about, rather than a decorrelated semi-join.
fn holds_subquery_eval(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::SubqueryEval { .. } => true,
        PhysicalPlan::Filter { input, .. } | PhysicalPlan::Project { input, .. } => {
            holds_subquery_eval(input)
        }
        _ => false,
    }
}

/// `INSERT target SELECT u.k FROM other AS u WHERE NOT EXISTS (SELECT 1 FROM <read> AS h
/// WHERE h.k = u.k)`.
fn insert_unless_present(read: TableId) -> PhysicalInsert {
    plan_insert(project(
        filter(scan(OTHER), exists(correlated_on_k(read), true)),
        column(k(0)),
    ))
}

#[test]
fn an_insert_whose_not_exists_reads_the_target_requires_a_spool() {
    let insert = insert_unless_present(TARGET);
    assert!(
        holds_subquery_eval(&insert.source),
        "expected a SubqueryEval in the source, got {:?}",
        insert.source
    );
    assert!(insert.spool, "the NOT EXISTS reads the target");
}

#[test]
fn an_insert_whose_not_exists_reads_another_table_requires_no_spool() {
    let insert = insert_unless_present(OTHER);
    assert!(holds_subquery_eval(&insert.source));
    assert!(!insert.spool, "nothing in the source reads the target");
}

/// `INSERT target SELECT u.k FROM other AS u WHERE EXISTS (SELECT 1 FROM other AS m
/// WHERE m.k = u.k AND EXISTS (SELECT 1 FROM <read> AS h WHERE h.k = m.k))`: `read` is
/// reached through the plan of a subquery inside the plan of a subquery.
fn insert_two_deep(read: TableId) -> PhysicalInsert {
    let deepest = exists(correlated_on_k(read), false);
    let middle = project(
        filter(
            scan(OTHER),
            BoundExpr {
                kind: BoundExprKind::Logical {
                    op: vauban_binder::LogicalOp::And,
                    left: Box::new(compare(CompareOp::Eq, column(k(0)), column(k(1)))),
                    right: Box::new(deepest),
                },
                ty: bit_type(),
                line: 1,
            },
        ),
        literal(1),
    );
    plan_insert(project(
        filter(scan(OTHER), exists(middle, false)),
        column(k(0)),
    ))
}

#[test]
fn a_target_read_two_subqueries_deep_requires_a_spool() {
    assert!(insert_two_deep(TARGET).spool);
    assert!(!insert_two_deep(OTHER).spool);
}

/// `UPDATE target SET v = v + 1 WHERE EXISTS (SELECT 1 FROM <read> AS s WHERE s.k =
/// target.k)`, the correlated form of `WHERE k IN (SELECT k FROM target …)`.
fn update_where_exists(read: TableId) -> PhysicalUpdate {
    plan_update(
        filter(scan(TARGET), exists(correlated_on_k(read), false)),
        literal(1),
    )
}

#[test]
fn an_update_whose_where_reads_the_target_in_a_subquery_requires_a_spool() {
    let update = update_where_exists(TARGET);
    assert!(holds_subquery_eval(&update.input), "{:?}", update.input);
    assert!(update.spool, "the EXISTS reads the target");
}

#[test]
fn an_update_whose_where_reads_another_table_requires_no_spool() {
    assert!(!update_where_exists(OTHER).spool);
}

/// `UPDATE target SET v = (SELECT k FROM <read> …)`: the subquery lives in the value, not
/// in the rows to update.
fn update_set_from(read: TableId) -> PhysicalUpdate {
    plan_update(scan(TARGET), scalar(project(scan(read), column(k(0)))))
}

#[test]
fn a_set_value_reading_the_target_requires_a_spool() {
    assert!(update_set_from(TARGET).spool);
}

#[test]
fn a_set_value_reading_another_table_requires_no_spool() {
    assert!(!update_set_from(OTHER).spool);
}

/// The subquery of a value of the `SET` list is planned under a `SubqueryEval` at the root of
/// the input, over the read of the target, which keeps its row identifier.
#[test]
fn the_subqueries_of_a_set_are_planned_over_the_input() {
    let update = update_set_from(OTHER);
    let PhysicalPlan::SubqueryEval {
        input, subplans, ..
    } = &update.input
    else {
        panic!("a SubqueryEval at the root: {:?}", update.input);
    };
    assert_eq!(subplans.len(), 1);
    assert!(
        matches!(input.as_ref(), PhysicalPlan::TableScan { table, .. } if *table == TARGET),
        "{input:?}"
    );
    assert_eq!(update.input.row_id_tables(), vec![TARGET]);
}

/// `DELETE target WHERE EXISTS (SELECT 1 FROM <read> AS s WHERE s.k = target.k)`.
fn delete_where_exists(read: TableId) -> PhysicalDelete {
    plan_delete(filter(scan(TARGET), exists(correlated_on_k(read), false)))
}

#[test]
fn a_delete_whose_where_reads_the_target_in_a_subquery_requires_a_spool() {
    let delete = delete_where_exists(TARGET);
    assert!(holds_subquery_eval(&delete.input), "{:?}", delete.input);
    assert!(delete.spool, "the EXISTS reads the target");
}

#[test]
fn a_delete_whose_where_reads_another_table_requires_no_spool() {
    assert!(!delete_where_exists(OTHER).spool);
}

/// `UPDATE target SET v = v + 1 WHERE k IN (SELECT k FROM <read> WHERE v < 10)`: an
/// uncorrelated `IN`, which the planner turns into a semi-join rather than a
/// `SubqueryEval`.
fn update_where_in(read: TableId) -> PhysicalUpdate {
    let inner = project(
        filter(scan(read), compare(CompareOp::Lt, column(v()), literal(10))),
        column(k(0)),
    );
    let predicate = BoundExpr {
        kind: BoundExprKind::InSubquery {
            expr: Box::new(column(k(0))),
            plan: BoundSubquery::new(inner),
            negated: false,
        },
        ty: bit_type(),
        line: 1,
    };
    plan_update(filter(scan(TARGET), predicate), literal(1))
}

#[test]
fn an_update_whose_in_reads_the_target_requires_a_spool() {
    assert!(update_where_in(TARGET).spool);
    assert!(!update_where_in(OTHER).spool);
}
