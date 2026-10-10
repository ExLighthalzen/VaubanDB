//! The row identifier a write statement asks of the reads of its target table, and the
//! order in which a plan lists the row identifiers its rows carry.
//!
//! Each test builds its bound statement by hand and plans it against a `FakeCatalog`.

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, BoundStatement, ColumnBinding, CompareOp,
    DeletePlan, JoinKind, LockHints, LogicalPlan, OutputColumn, OutputSchema, UpdatePlan,
};
use vauban_catalog::ColumnId;
use vauban_planner::{
    PhysicalJoinKind, PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext, ROW_ID_NAME, plan,
    row_id_binding, row_id_table, testing::FakeCatalog,
};
use vauban_storage::{KeyColumn, TableId};
use vauban_types::{SqlType, TypeInfo, Value};

const TABLE_T: TableId = TableId(7);
const TABLE_U: TableId = TableId(8);

fn int_type() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

fn binding(name: &str, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(i32::try_from(index).expect("a small index") + 1),
        index,
        name: name.to_owned(),
        ty: int_type(),
        depth: 0,
    }
}

fn column_ref(name: &str, index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(name, index)),
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

fn equals(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, false),
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

/// `FROM <table>`, columns `pk` and `x`.
fn scan(table: TableId, alias: &str) -> LogicalPlan {
    LogicalPlan::Scan {
        table,
        columns: vec![binding("pk", 0), binding("x", 1)],
        alias: alias.to_owned(),
        schema: schema_of(&["pk", "x"]),
        hints: LockHints::default(),
    }
}

/// `t JOIN u ON t.pk = u.pk`: `t.pk`, `t.x`, `u.pk`, `u.x`.
fn t_join_u() -> LogicalPlan {
    LogicalPlan::Join {
        left: Box::new(scan(TABLE_T, "t")),
        right: Box::new(scan(TABLE_U, "u")),
        kind: JoinKind::Inner,
        on: Some(equals(column_ref("pk", 0), column_ref("pk", 2))),
        schema: schema_of(&["pk", "x", "pk", "x"]),
    }
}

fn plan_with(stmt: BoundStatement, catalog: &dyn PlanCatalog) -> PhysicalStatement {
    plan(stmt, &PlanContext { catalog }).expect("the statement plans")
}

/// The row-identifier bindings of the scans and seeks of `plan`, with the table each one
/// reads, in the order of the tail.
fn marked_reads(plan: &PhysicalPlan, out: &mut Vec<(String, TableId)>) {
    match plan {
        PhysicalPlan::TableScan { columns, alias, .. } => {
            for table in columns.iter().filter_map(row_id_table) {
                out.push((alias.clone(), table));
            }
        }
        PhysicalPlan::IndexSeek { columns, .. } => {
            for table in columns.iter().filter_map(row_id_table) {
                out.push(("seek".to_owned(), table));
            }
        }
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::Sort { input, .. } => marked_reads(input, out),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            marked_reads(outer, out);
            marked_reads(inner, out);
        }
        PhysicalPlan::HashJoin { build, probe, .. } => {
            marked_reads(probe, out);
            marked_reads(build, out);
        }
        _ => {}
    }
}

/// The binding names its table, and the binding of a column names none.
#[test]
fn the_binding_names_its_table() {
    for table in [TableId(0), TableId(7), TableId(i32::MAX as u32)] {
        let binding = row_id_binding(table).expect("the identifier fits");
        assert_eq!(row_id_table(&binding), Some(table));
        assert_eq!(binding.name, ROW_ID_NAME);
        assert!(binding.column.0 < 0, "{binding:?}");
    }
    assert!(row_id_binding(TableId(u32::MAX)).is_none());
    assert_eq!(row_id_table(&binding("pk", 0)), None);
    // The slot identifier the binder gives a computed column is not a row identifier.
    let mut slot = binding("n", 0);
    slot.column = ColumnId(0);
    assert_eq!(row_id_table(&slot), None);
}

/// `UPDATE t SET x = 1 FROM t JOIN u ON t.pk = u.pk`: the scan of `t` carries the row
/// identifier, the scan of `u` does not, and the rows of the input name `t` alone.
#[test]
fn an_update_over_a_join_asks_the_target_side_alone() {
    let stmt = BoundStatement::Update(UpdatePlan {
        target: Default::default(),
        table: TABLE_T,
        input: Box::new(t_join_u()),
        assignments: vec![(binding("x", 1), literal(1))],
    });
    let PhysicalStatement::Update(update) = plan_with(stmt, &FakeCatalog::new()) else {
        panic!("an UPDATE plans to an Update");
    };
    assert_eq!(update.input.row_id_tables(), vec![TABLE_T]);
    let mut marked = Vec::new();
    marked_reads(&update.input, &mut marked);
    assert_eq!(marked, vec![("t".to_owned(), TABLE_T)]);
    // The visible width is the width of the join: the identifier is not a column.
    assert_eq!(update.input.schema().columns.len(), 4);
}

/// `DELETE FROM t WHERE pk = 3` over a unique index on `pk`: the seek that replaced the
/// scan carries the row identifier.
#[test]
fn a_delete_asks_its_seek_for_the_row_id() {
    let catalog = FakeCatalog::new().with_index(
        TABLE_T,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        true,
    );
    let stmt = BoundStatement::Delete(DeletePlan {
        table: TABLE_T,
        input: Box::new(LogicalPlan::Filter {
            input: Box::new(scan(TABLE_T, "t")),
            predicate: equals(column_ref("pk", 0), literal(3)),
        }),
    });
    let PhysicalStatement::Delete(delete) = plan_with(stmt, &catalog) else {
        panic!("a DELETE plans to a Delete");
    };
    let mut marked = Vec::new();
    marked_reads(&delete.input, &mut marked);
    assert_eq!(marked, vec![("seek".to_owned(), TABLE_T)]);
    assert_eq!(delete.input.row_id_tables(), vec![TABLE_T]);
}

/// The same join, read by a `SELECT`, asks no row identifier.
#[test]
fn a_select_asks_no_row_id() {
    let stmt = BoundStatement::Query(Box::new(LogicalPlan::Project {
        input: Box::new(t_join_u()),
        exprs: vec![BoundProjection {
            expr: column_ref("x", 1),
            name: "x".to_owned(),
        }],
        schema: schema_of(&["x"]),
    }));
    let PhysicalStatement::Query(query) = plan_with(stmt, &FakeCatalog::new()) else {
        panic!("a SELECT plans to a Query");
    };
    assert_eq!(query.row_id_count(), 0);
    let mut marked = Vec::new();
    marked_reads(&query, &mut marked);
    assert!(marked.is_empty(), "{marked:?}");
}

/// A scan of `table`, asked for its row identifier.
fn marked_scan(table: TableId) -> PhysicalPlan {
    PhysicalPlan::TableScan {
        table,
        columns: vec![
            binding("pk", 0),
            row_id_binding(table).expect("the identifier fits"),
        ],
        alias: format!("t{table}"),
        schema: schema_of(&["pk"]),
        hints: LockHints::default(),
    }
}

/// A hash join lists the probe side then the build side, a nested-loop join the outer then
/// the inner, a semi-join the outer alone; a `DISTINCT` and an aggregate build rows of
/// their own and list none.
#[test]
fn the_tail_follows_the_join_order() {
    let hash = PhysicalPlan::HashJoin {
        build: Box::new(marked_scan(TABLE_U)),
        probe: Box::new(marked_scan(TABLE_T)),
        kind: PhysicalJoinKind::Inner,
        keys: vec![(column_ref("pk", 0), column_ref("pk", 1))],
        residual: None,
        schema: schema_of(&["pk", "pk"]),
    };
    assert_eq!(hash.row_id_tables(), vec![TABLE_T, TABLE_U]);
    let nested = |kind| PhysicalPlan::NestedLoopJoin {
        outer: Box::new(marked_scan(TABLE_U)),
        inner: Box::new(marked_scan(TABLE_T)),
        kind,
        on: None,
        schema: schema_of(&["pk", "pk"]),
    };
    assert_eq!(
        nested(PhysicalJoinKind::Left).row_id_tables(),
        vec![TABLE_U, TABLE_T]
    );
    assert_eq!(
        nested(PhysicalJoinKind::Semi).row_id_tables(),
        vec![TABLE_U]
    );
    assert_eq!(
        nested(PhysicalJoinKind::AntiSemi).row_id_tables(),
        vec![TABLE_U]
    );
    assert_eq!(
        PhysicalPlan::Distinct(Box::new(hash.clone())).row_id_count(),
        0
    );
    let aggregate = PhysicalPlan::HashAggregate {
        input: Box::new(hash),
        group_by: vec![column_ref("pk", 0)],
        aggregates: Vec::new(),
        schema: schema_of(&["pk"]),
    };
    assert_eq!(aggregate.row_id_count(), 0);
}

/// `UPDATE y SET x = 1 FROM t JOIN t AS y ON t.pk = y.pk`, bound with the row-identifier
/// binding on the scan of `y`, the source the target names: the planner keeps that mark
/// and adds none to the other read of `t`, so the rows of the input name one row of `t`.
/// Without the mark, both reads would carry one.
#[test]
fn a_self_join_marks_the_source_the_target_names() {
    let self_join = |marked: bool| {
        let mut right = scan(TABLE_T, "y");
        if marked && let LogicalPlan::Scan { columns, .. } = &mut right {
            columns.push(row_id_binding(TABLE_T).expect("the identifier fits"));
        }
        LogicalPlan::Join {
            left: Box::new(scan(TABLE_T, "t")),
            right: Box::new(right),
            kind: JoinKind::Inner,
            on: Some(equals(column_ref("pk", 0), column_ref("pk", 2))),
            schema: schema_of(&["pk", "x", "pk", "x"]),
        }
    };
    let planned = |marked: bool| {
        let stmt = BoundStatement::Update(UpdatePlan {
            target: Default::default(),
            table: TABLE_T,
            input: Box::new(self_join(marked)),
            assignments: vec![(binding("x", 1), literal(1))],
        });
        let PhysicalStatement::Update(update) = plan_with(stmt, &FakeCatalog::new()) else {
            panic!("an UPDATE plans to an Update");
        };
        let mut reads = Vec::new();
        marked_reads(&update.input, &mut reads);
        reads
    };
    assert_eq!(planned(true), vec![("y".to_owned(), TABLE_T)]);
    assert_eq!(
        planned(false),
        vec![("t".to_owned(), TABLE_T), ("y".to_owned(), TABLE_T)]
    );
}
