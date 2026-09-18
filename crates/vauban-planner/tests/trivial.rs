//! The contract of the crate before its planning rules: the public types, the translation
//! that holds no rule, and the internal error each unfilled form answers.
//!
//! The tests named `…_is_not_implemented_yet` pin the unfilled forms: the rule that fills
//! one deletes or inverts its test, and the others keep answering until their turn.

use vauban_planner::{KeyRangeExpr, PhysicalJoinKind, explain, testing::FakeCatalog};
use vauban_planner::{NoIndexes, PhysicalPlan, PhysicalStatement, PlanCatalog, PlanContext, plan};

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, BoundStatement, BoundTop, ColumnBinding, CompareOp,
    DeletePlan, InsertPlan, JoinKind, LockHints, LogicalPlan, OutputColumn, OutputSchema,
    SetOpKind, UpdatePlan,
};
use vauban_catalog::ColumnId;
use vauban_errors::SqlError;
use vauban_storage::{IndexShape, KeyColumn, MemoryStorage, Storage, TableId, TableShape};
use vauban_types::{SqlType, TypeInfo, Value};

/// The context every test plans against, over the catalogue it is handed.
fn context<'a>(catalog: &'a dyn PlanCatalog) -> PlanContext<'a> {
    PlanContext { catalog }
}

fn int_type() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

fn literal(n: i32) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(n)),
        ty: int_type(),
        line: 1,
    }
}

fn id_binding() -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(1),
        index: 0,
        name: "id".to_owned(),
        ty: int_type(),
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

/// `FROM dbo.t`, one `int` column named `id`.
fn scan() -> LogicalPlan {
    LogicalPlan::Scan {
        table: TableId(7),
        columns: vec![id_binding()],
        alias: "t".to_owned(),
        schema: schema_of(&["id"]),
        hints: LockHints::default(),
    }
}

/// `id = 1`, the predicate an index on `id` could serve.
fn id_equals_one() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(BoundExpr {
                kind: BoundExprKind::ColumnRef(id_binding()),
                ty: int_type(),
                line: 1,
            }),
            right: Box::new(literal(1)),
        },
        ty: TypeInfo::new(SqlType::Bit, false),
        line: 1,
    }
}

/// `SELECT id FROM dbo.t WHERE id = 1`, as the binder would hand it over.
fn project_filter_scan() -> LogicalPlan {
    LogicalPlan::Project {
        input: Box::new(LogicalPlan::Filter {
            input: Box::new(scan()),
            predicate: id_equals_one(),
        }),
        exprs: vec![BoundProjection {
            expr: BoundExpr {
                kind: BoundExprKind::ColumnRef(id_binding()),
                ty: int_type(),
                line: 1,
            },
            name: "id".to_owned(),
        }],
        schema: schema_of(&["id"]),
    }
}

fn plan_query(logical: LogicalPlan) -> PhysicalPlan {
    let catalog = NoIndexes;
    match plan(BoundStatement::Query(Box::new(logical)), &context(&catalog)) {
        Ok(PhysicalStatement::Query(physical)) => physical,
        Ok(other) => panic!("expected a query, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

/// The error a statement answers, or the shape it planned to.
#[expect(dead_code)]
fn plan_error(stmt: BoundStatement) -> SqlError {
    let catalog = NoIndexes;
    match plan(stmt, &context(&catalog)) {
        Ok(ok) => panic!("expected an error, got {ok:?}"),
        Err(err) => err,
    }
}

/// Checks that `err` is the internal error 50000 of a form not implemented yet.
#[expect(dead_code)]
fn assert_not_implemented(err: &SqlError) {
    assert_eq!(err.number, 50000, "error was {err:?}");
    assert_eq!(err.state, 1, "error was {err:?}");
    assert!(
        err.message.contains("is not implemented yet"),
        "message was {:?}",
        err.message
    );
}

#[test]
fn one_row_plans_to_one_row() {
    let catalog = NoIndexes;
    let planned = plan(
        BoundStatement::Query(Box::new(LogicalPlan::OneRow)),
        &context(&catalog),
    )
    .expect("OneRow plans");
    assert!(matches!(
        planned,
        PhysicalStatement::Query(PhysicalPlan::OneRow)
    ));
}

/// A `Scan` whose `LockHints` carry `nolock` plans into a `TableScan` that carries those
/// hints: the whole struct is compared, so a translation that dropped them for
/// `LockHints::default()` fails here.
#[test]
fn hints_of_a_scan_reach_the_physical_scan() {
    let hints = LockHints {
        nolock: true,
        ..LockHints::default()
    };
    let logical = LogicalPlan::Scan {
        table: TableId(7),
        columns: vec![id_binding()],
        alias: "t".to_owned(),
        schema: schema_of(&["id"]),
        hints,
    };
    let planned = plan_query(logical);
    let PhysicalPlan::TableScan { hints: carried, .. } = &planned else {
        panic!("expected a TableScan, got {planned:?}")
    };
    assert_eq!(*carried, hints);
}

/// A reference written without a hint carries `LockHints::default()`; the default does not
/// turn into an indicator on the way.
#[test]
fn a_reference_without_a_hint_carries_the_default() {
    let planned = plan_query(scan());
    let PhysicalPlan::TableScan { hints, .. } = &planned else {
        panic!("expected a TableScan, got {planned:?}")
    };
    assert_eq!(*hints, LockHints::default());
}

#[test]
fn filter_project_over_scan_becomes_a_seek_under_the_project() {
    // A unique index on the filtered column, declared to the planner: the seek rule
    // replaces the Filter over the TableScan with an IndexSeek, and the Project above it
    // keeps its columns (`tests/seek.rs`).
    let catalog = FakeCatalog::new().with_index(
        TableId(7),
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        true,
    );
    assert_eq!(catalog.indexes_of(TableId(7)).len(), 1);
    let planned = match plan(
        BoundStatement::Query(Box::new(project_filter_scan())),
        &context(&catalog),
    )
    .expect("the query plans")
    {
        PhysicalStatement::Query(physical) => physical,
        other => panic!("expected a query, got {other:?}"),
    };

    let PhysicalPlan::Project { input, exprs, .. } = &planned else {
        panic!("expected a Project, got {planned:?}")
    };
    assert_eq!(exprs.len(), 1);
    assert!(
        matches!(input.as_ref(), PhysicalPlan::IndexSeek { .. }),
        "expected an IndexSeek under the Project, got {input:?}"
    );
}

#[test]
fn values_and_limit_are_translated() {
    let values = LogicalPlan::Values {
        rows: vec![vec![literal(1)], vec![literal(2)]],
        schema: schema_of(&["n"]),
    };
    let limited = LogicalPlan::Limit {
        input: Box::new(values),
        top: BoundTop {
            expr: literal(1),
            percent: false,
            with_ties: false,
        },
    };
    let planned = plan_query(limited);
    let PhysicalPlan::Top { input, top } = &planned else {
        panic!("expected a Top, got {planned:?}")
    };
    assert!(!top.percent && !top.with_ties);
    let PhysicalPlan::Values { rows, .. } = input.as_ref() else {
        panic!("expected Values under the Top, got {input:?}")
    };
    assert_eq!(rows.len(), 2);
}

/// A self-join on `id = 1` produces a HashJoin: the equality triggers the hash
/// algorithm when no index is available. With the join rule written, the old
/// `join_is_not_implemented_yet` would fail; this test verifies it now plans.
#[test]
fn a_join_with_an_equality_on_a_literal_produces_a_hash_join() {
    let join = LogicalPlan::Join {
        left: Box::new(scan()),
        right: Box::new(scan()),
        kind: JoinKind::Inner,
        on: Some(id_equals_one()),
        schema: schema_of(&["id", "id"]),
    };
    let catalog = NoIndexes;
    let ctx = PlanContext { catalog: &catalog };
    let planned = plan(BoundStatement::Query(Box::new(join)), &ctx)
        .expect("a join with an equality plans to a HashJoin");
    assert!(
        matches!(
            &planned,
            PhysicalStatement::Query(PhysicalPlan::HashJoin { .. })
        ),
        "expected a HashJoin, got {planned:?}"
    );
}

/// An aggregate over a scan produces a HashAggregate: the scan delivers no order, so the
/// grouping goes through a hash table. With the aggregate rule written, the form that
/// used to answer an internal error now plans (`tests/aggregate_sort.rs`).
#[test]
fn an_aggregate_over_a_scan_produces_a_hash_aggregate() {
    let aggregate = LogicalPlan::Aggregate {
        input: Box::new(scan()),
        group_by: vec![literal(1)],
        aggregates: Vec::new(),
        schema: schema_of(&["n"]),
    };
    let planned = plan_query(aggregate);
    assert!(
        matches!(&planned, PhysicalPlan::HashAggregate { .. }),
        "expected a HashAggregate, got {planned:?}"
    );
}

#[test]
fn a_subquery_expression_plans() {
    let exists = BoundExpr {
        kind: BoundExprKind::Exists(Box::new(LogicalPlan::OneRow)),
        ty: TypeInfo::new(SqlType::Bit, false),
        line: 1,
    };
    let filtered = LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: exists,
    };
    let planned = plan_query(filtered);
    assert!(matches!(
        planned,
        PhysicalPlan::NestedLoopJoin {
            kind: PhysicalJoinKind::Semi,
            ..
        }
    ));

    let derived = LogicalPlan::Subquery {
        input: Box::new(scan()),
        alias: "d".to_owned(),
        schema: schema_of(&["id"]),
    };
    let planned = plan_query(derived);
    assert!(matches!(planned, PhysicalPlan::TableScan { .. }));
}

/// `EXISTS (SELECT …)`, the subquery expression the holders below carry.
fn exists_expr() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Exists(Box::new(LogicalPlan::OneRow)),
        ty: TypeInfo::new(SqlType::Bit, false),
        line: 1,
    }
}

#[test]
fn subquery_holders_outside_the_two_call_sites_go_through() {
    // `plan.rs` hands `subquery::plan_expr_subqueries` two expressions: the predicate of a
    // `Filter` and each expression of a `Project`. The seven holders below are copied
    // across without being read, so the same `EXISTS` plans to `Ok` there. The rule that
    // plans them inverts this test.
    let catalog = NoIndexes;
    let holders: Vec<(&str, BoundStatement)> = vec![
        (
            "BoundTop.expr",
            BoundStatement::Query(Box::new(LogicalPlan::Limit {
                input: Box::new(scan()),
                top: BoundTop {
                    expr: exists_expr(),
                    percent: false,
                    with_ties: false,
                },
            })),
        ),
        (
            "a row of Values",
            BoundStatement::Query(Box::new(LogicalPlan::Values {
                rows: vec![vec![exists_expr()]],
                schema: schema_of(&["b"]),
            })),
        ),
        (
            "the condition of If",
            BoundStatement::If {
                condition: exists_expr(),
                then_: Box::new(BoundStatement::Break),
                else_: None,
            },
        ),
        (
            "the condition of While",
            BoundStatement::While {
                condition: exists_expr(),
                body: Box::new(BoundStatement::Break),
            },
        ),
        ("Print", BoundStatement::Print(exists_expr())),
        (
            "SetVariable",
            BoundStatement::SetVariable {
                name: "@x".to_owned(),
                value: exists_expr(),
            },
        ),
        ("Return", BoundStatement::Return(Some(exists_expr()))),
    ];
    assert_eq!(holders.len(), 7);
    for (holder, stmt) in holders {
        assert!(
            plan(stmt, &context(&catalog)).is_ok(),
            "{holder} carried the EXISTS to an error; a rule changed this state"
        );
    }

    // Counter-proof on the two sites `plan.rs` does hand over: the same `EXISTS` in the
    // predicate of a `Filter` and in an expression of a `Project` is planned there too.
    for logical in [
        LogicalPlan::Filter {
            input: Box::new(scan()),
            predicate: exists_expr(),
        },
        LogicalPlan::Project {
            input: Box::new(scan()),
            exprs: vec![BoundProjection {
                expr: exists_expr(),
                name: "b".to_owned(),
            }],
            schema: schema_of(&["b"]),
        },
    ] {
        assert!(
            plan(BoundStatement::Query(Box::new(logical)), &context(&catalog)).is_ok(),
            "the two call sites must plan a subquery expression"
        );
    }
}

#[test]
fn an_expression_without_a_subquery_goes_through() {
    // Counter-proof of the test above: the same `Filter`, with a predicate that holds no
    // subquery, plans without an error.
    let planned = plan_query(LogicalPlan::Filter {
        input: Box::new(scan()),
        predicate: id_equals_one(),
    });
    assert!(matches!(planned, PhysicalPlan::Filter { .. }));
}

#[test]
fn dml_plans_to_physical_insert() {
    let insert = InsertPlan {
        table: TableId(7),
        columns: vec![id_binding()],
        source: Box::new(LogicalPlan::Values {
            rows: vec![vec![literal(1)]],
            schema: schema_of(&["id"]),
        }),
    };
    let catalog = NoIndexes;
    match plan(BoundStatement::Insert(insert), &context(&catalog)) {
        Ok(PhysicalStatement::Insert(_)) => {}
        Ok(other) => panic!("expected Insert, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

#[test]
fn dml_plans_to_physical_update() {
    let update = UpdatePlan {
        table: TableId(7),
        input: Box::new(scan()),
        assignments: vec![(id_binding(), literal(2))],
    };
    let catalog = NoIndexes;
    match plan(BoundStatement::Update(update), &context(&catalog)) {
        Ok(PhysicalStatement::Update(_)) => {}
        Ok(other) => panic!("expected Update, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

#[test]
fn dml_plans_to_physical_delete() {
    let delete = DeletePlan {
        table: TableId(7),
        input: Box::new(scan()),
    };
    let catalog = NoIndexes;
    match plan(BoundStatement::Delete(delete), &context(&catalog)) {
        Ok(PhysicalStatement::Delete(_)) => {}
        Ok(other) => panic!("expected Delete, got {other:?}"),
        Err(err) => panic!("planning failed: {}", err.message),
    }
}

#[test]
fn a_set_operator_plans_to_union() {
    let set_op = LogicalPlan::SetOp {
        op: SetOpKind::Union,
        all: true,
        left: Box::new(scan()),
        right: Box::new(scan()),
        schema: schema_of(&["id"]),
    };
    let planned = plan_query(set_op);
    assert!(
        matches!(&planned, PhysicalPlan::Union { .. }),
        "got {planned:?}"
    );
}

#[test]
fn schema_follows_the_node() {
    let planned = plan_query(project_filter_scan());
    assert_eq!(planned.schema().columns.len(), 1);
    assert_eq!(planned.schema().columns[0].name, "id");

    let PhysicalPlan::Project { input, .. } = &planned else {
        panic!("expected a Project, got {planned:?}")
    };
    // The `Filter` publishes the columns of its input, not its own.
    assert_eq!(input.schema().columns.len(), 1);
    assert_eq!(input.schema().columns[0].name, "id");
    assert_eq!(PhysicalPlan::OneRow.schema().columns.len(), 0);

    // A `Project` that drops a column publishes what it projects.
    let widened = LogicalPlan::Project {
        input: Box::new(scan()),
        exprs: vec![
            BoundProjection {
                expr: literal(1),
                name: "a".to_owned(),
            },
            BoundProjection {
                expr: literal(2),
                name: "b".to_owned(),
            },
        ],
        schema: schema_of(&["a", "b"]),
    };
    let planned = plan_query(widened);
    let names: Vec<&str> = planned
        .schema()
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn explain_writes_one_line_per_node() {
    let planned = plan_query(project_filter_scan());
    let text = explain(&planned);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "explain wrote {text:?}");
    assert_eq!(lines[0], "Project(columns=1)");
    assert_eq!(lines[1], "  Filter");
    assert_eq!(lines[2], "    TableScan(table=7, alias=t)");
    for (level, line) in lines.iter().enumerate() {
        let indent = line.len() - line.trim_start().len();
        assert_eq!(indent, level * 2, "line {level} was {line:?}");
    }
}

#[test]
fn control_flow_is_planned_branch_by_branch() {
    let block = BoundStatement::Block(vec![
        BoundStatement::Break,
        BoundStatement::If {
            condition: id_equals_one(),
            then_: Box::new(BoundStatement::Query(Box::new(LogicalPlan::OneRow))),
            else_: Some(Box::new(BoundStatement::Continue)),
        },
    ]);
    let catalog = NoIndexes;
    let planned = plan(block, &context(&catalog)).expect("the block plans");
    let PhysicalStatement::Block(statements) = &planned else {
        panic!("expected a Block, got {planned:?}")
    };
    assert_eq!(statements.len(), 2);
    let PhysicalStatement::If { then_, else_, .. } = &statements[1] else {
        panic!("expected an If, got {:?}", statements[1])
    };
    assert!(matches!(
        then_.as_ref(),
        PhysicalStatement::Query(PhysicalPlan::OneRow)
    ));
    assert!(matches!(
        else_.as_deref(),
        Some(PhysicalStatement::Continue)
    ));

    // A branch whose body is a set operation is planned like any other query.
    let planned = plan(
        BoundStatement::While {
            condition: id_equals_one(),
            body: Box::new(BoundStatement::Query(Box::new(LogicalPlan::SetOp {
                op: SetOpKind::Union,
                all: true,
                left: Box::new(scan()),
                right: Box::new(scan()),
                schema: schema_of(&["id"]),
            }))),
        },
        &context(&catalog),
    )
    .expect("the While plans");
    let PhysicalStatement::While { body, .. } = &planned else {
        panic!("expected a While, got {planned:?}")
    };
    assert!(
        matches!(
            body.as_ref(),
            PhysicalStatement::Query(PhysicalPlan::Union { .. })
        ),
        "expected a Union in the body, got {body:?}"
    );
}

#[test]
fn storage_indexes_falls_back_to_a_scan_on_error() {
    let storage = MemoryStorage::new();
    let db = storage.create_database("db").expect("database");
    let shape = TableShape {
        columns: vec![int_type()],
        clustered_key: None,
    };
    let table = storage.create_table(db, &shape).expect("table");
    storage
        .create_index(
            table,
            &IndexShape {
                columns: vec![KeyColumn {
                    column: 0,
                    descending: false,
                }],
                unique: true,
                included: Vec::new(),
            },
        )
        .expect("index");

    let indexes = vauban_planner::StorageIndexes(&storage);
    // The counter-proof: the same call on a table that exists answers its index, so the
    // empty vector below comes from the error and not from the implementation.
    assert_eq!(indexes.indexes_of(table).len(), 1);
    assert!(storage.indexes(TableId(4_242)).is_err());
    assert!(indexes.indexes_of(TableId(4_242)).is_empty());
}

#[test]
fn the_declared_types_are_reachable_from_outside() {
    // Callers build on these names; the test fails to compile if one moves.
    assert_eq!(
        PhysicalJoinKind::from(JoinKind::Cross),
        PhysicalJoinKind::Cross
    );
    assert_eq!(
        PhysicalJoinKind::from(JoinKind::Left),
        PhysicalJoinKind::Left
    );
    assert!(matches!(KeyRangeExpr::Full, KeyRangeExpr::Full));
    let point = KeyRangeExpr::Point(vec![literal(1)]);
    assert!(matches!(point, KeyRangeExpr::Point(ref keys) if keys.len() == 1));
}
