//! Integration tests for `UPDATE` and `DELETE`: row counts, column assignments, index
//! seeks, spool for Halloween protection and write-conflict handling under SNAPSHOT
//! isolation.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, CompareOp, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_catalog::{Catalog, ColumnDef, IndexDef, QualifiedName, SortedColumn, TableDef};
use vauban_executor::{ExecContext, ExecSession, execute_collect};
use vauban_planner::{
    KeyRangeExpr, PhysicalDelete, PhysicalPlan, PhysicalStatement, PhysicalUpdate,
};
use vauban_storage::{Direction, MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager, VersioningOptions};
use vauban_types::{BinaryOp, SqlType, TypeInfo, Value};

fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

fn col_binding(index: usize, name: &str, ty: TypeInfo) -> ColumnBinding {
    ColumnBinding {
        column: vauban_catalog::ColumnId(index as i32),
        index,
        name: name.to_owned(),
        ty,
    }
}

fn out_col(name: &str, ty: TypeInfo) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        ty,
    }
}

fn lit(value: Value) -> BoundExpr {
    let ty = match &value {
        Value::I32(_) | Value::I64(_) => int(true),
        _ => int(true),
    };
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 0,
    }
}

fn tbl_name(name: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "dbo".to_owned(),
        name: name.to_owned(),
    }
}

fn scan_plan(
    table: vauban_storage::TableId,
    columns: Vec<ColumnBinding>,
    schema: OutputSchema,
) -> PhysicalPlan {
    PhysicalPlan::TableScan {
        table,
        columns,
        schema,
        alias: "t".to_owned(),
    }
}

/// Three rows inserted by `storage.insert`, updated through a `TableScan` with no filter:
/// the three are updated, the count is 3, and a re-scan shows the new values.
#[test]
fn update_all_rows_counts_them() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).unwrap();
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("update_all"),
                columns: vec![
                    ColumnDef {
                        name: "a".to_owned(),
                        ty: int(false),
                        default: None,
                        identity: None,
                        computed: None,
                    },
                    ColumnDef {
                        name: "b".to_owned(),
                        ty: int(true),
                        default: None,
                        identity: None,
                        computed: None,
                    },
                ],
                constraints: Vec::new(),
            },
        )
        .unwrap();

    for row in &[
        vec![Value::I32(10), Value::I32(100)],
        vec![Value::I32(20), Value::I32(200)],
        vec![Value::I32(30), Value::I32(300)],
    ] {
        storage
            .insert(
                handle.id,
                meta.storage_id,
                &vauban_storage::Row(row.clone()),
            )
            .unwrap();
    }

    let stmt = PhysicalStatement::Update(PhysicalUpdate {
        table: meta.storage_id,
        input: scan_plan(
            meta.storage_id,
            vec![
                col_binding(0, "a", int(false)),
                col_binding(1, "b", int(true)),
            ],
            OutputSchema {
                columns: vec![out_col("a", int(false)), out_col("b", int(true))],
            },
        ),
        assignments: vec![(
            col_binding(0, "a", int(false)),
            BoundExpr {
                kind: BoundExprKind::Arith {
                    op: BinaryOp::Add,
                    left: Box::new(BoundExpr {
                        kind: BoundExprKind::ColumnRef(col_binding(0, "a", int(false))),
                        ty: int(true),
                        line: 0,
                    }),
                    right: Box::new(lit(Value::I32(1))),
                },
                ty: int(true),
                line: 0,
            },
        )],
        spool: false,
    });

    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    let rowcount = {
        let snap = txn_mgr.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&catalog)
            .with_handle(&handle)
            .with_session(&mut session);
        let (outcome, _) = execute_collect(&stmt, &mut ctx).unwrap();
        assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));
        session.rowcount
    };
    assert_eq!(rowcount, 3);

    let snap = txn_mgr.statement_snapshot(&handle);
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let scan = PhysicalStatement::Query(scan_plan(
        meta.storage_id,
        vec![
            col_binding(0, "a", int(false)),
            col_binding(1, "b", int(true)),
        ],
        OutputSchema {
            columns: vec![out_col("a", int(false)), out_col("b", int(true))],
        },
    ));
    let (_, set) = execute_collect(&scan, &mut ctx).unwrap();
    assert_eq!(set.rows.len(), 3);
    assert_eq!(set.rows[0], vec![Value::I32(11), Value::I32(100)]);
    assert_eq!(set.rows[1], vec![Value::I32(21), Value::I32(200)]);
    assert_eq!(set.rows[2], vec![Value::I32(31), Value::I32(300)]);
}

/// `SET a = a + 1, b = a` reads the old `a` twice: `a = 1, b = 0` becomes `a = 2, b = 1`,
/// not `a = 2, b = 2`.
#[test]
fn update_reads_the_old_row() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).unwrap();
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("update_old_row"),
                columns: vec![
                    ColumnDef {
                        name: "a".to_owned(),
                        ty: int(false),
                        default: None,
                        identity: None,
                        computed: None,
                    },
                    ColumnDef {
                        name: "b".to_owned(),
                        ty: int(false),
                        default: None,
                        identity: None,
                        computed: None,
                    },
                ],
                constraints: Vec::new(),
            },
        )
        .unwrap();

    storage
        .insert(
            handle.id,
            meta.storage_id,
            &vauban_storage::Row(vec![Value::I32(1), Value::I32(0)]),
        )
        .unwrap();

    let col_a = col_binding(0, "a", int(false));
    let col_b = col_binding(1, "b", int(false));

    let stmt = PhysicalStatement::Update(PhysicalUpdate {
        table: meta.storage_id,
        input: scan_plan(
            meta.storage_id,
            vec![col_a.clone(), col_b.clone()],
            OutputSchema {
                columns: vec![out_col("a", int(false)), out_col("b", int(false))],
            },
        ),
        assignments: vec![
            (
                col_a.clone(),
                BoundExpr {
                    kind: BoundExprKind::Arith {
                        op: BinaryOp::Add,
                        left: Box::new(BoundExpr {
                            kind: BoundExprKind::ColumnRef(col_a.clone()),
                            ty: int(true),
                            line: 0,
                        }),
                        right: Box::new(lit(Value::I32(1))),
                    },
                    ty: int(true),
                    line: 0,
                },
            ),
            (
                col_b.clone(),
                BoundExpr {
                    kind: BoundExprKind::ColumnRef(col_a),
                    ty: int(true),
                    line: 0,
                },
            ),
        ],
        spool: false,
    });

    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    {
        let snap = txn_mgr.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&catalog)
            .with_handle(&handle)
            .with_session(&mut session);
        let (outcome, _) = execute_collect(&stmt, &mut ctx).unwrap();
        assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));
    }

    let snap = txn_mgr.statement_snapshot(&handle);
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let scan = PhysicalStatement::Query(scan_plan(
        meta.storage_id,
        vec![
            col_binding(0, "a", int(false)),
            col_binding(1, "b", int(false)),
        ],
        OutputSchema {
            columns: vec![out_col("a", int(false)), out_col("b", int(false))],
        },
    ));
    let (_, set) = execute_collect(&scan, &mut ctx).unwrap();
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.rows[0], vec![Value::I32(2), Value::I32(1)]);
}

/// An `IndexSeek` on a point key touches one row; the other rows are unchanged.
#[test]
fn update_via_seek_touches_one_row() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).unwrap();
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("update_seek"),
                columns: vec![
                    ColumnDef {
                        name: "a".to_owned(),
                        ty: int(false),
                        default: None,
                        identity: None,
                        computed: None,
                    },
                    ColumnDef {
                        name: "b".to_owned(),
                        ty: int(true),
                        default: None,
                        identity: None,
                        computed: None,
                    },
                ],
                constraints: Vec::new(),
            },
        )
        .unwrap();

    let idx = catalog
        .create_index(
            &handle,
            &IndexDef {
                table: meta.id,
                name: "ix_a".to_owned(),
                columns: vec![SortedColumn {
                    column: "a".to_owned(),
                    descending: false,
                }],
                unique: false,
                clustered: false,
            },
        )
        .unwrap();

    for row in &[
        vec![Value::I32(10), Value::I32(100)],
        vec![Value::I32(20), Value::I32(200)],
        vec![Value::I32(30), Value::I32(300)],
    ] {
        storage
            .insert(
                handle.id,
                meta.storage_id,
                &vauban_storage::Row(row.clone()),
            )
            .unwrap();
    }

    let key_col_binding = col_binding(0, "a", int(false));

    let stmt = PhysicalStatement::Update(PhysicalUpdate {
        table: meta.storage_id,
        input: PhysicalPlan::IndexSeek {
            index: idx.id,
            range: KeyRangeExpr::Point(vec![BoundExpr {
                kind: BoundExprKind::Literal(Value::I32(20)),
                ty: int(true),
                line: 0,
            }]),
            columns: vec![key_col_binding.clone()],
            direction: Direction::Forward,
            schema: OutputSchema {
                columns: vec![out_col("a", int(false))],
            },
        },
        assignments: vec![(
            col_binding(0, "a", int(false)),
            BoundExpr {
                kind: BoundExprKind::Arith {
                    op: BinaryOp::Add,
                    left: Box::new(BoundExpr {
                        kind: BoundExprKind::ColumnRef(key_col_binding),
                        ty: int(true),
                        line: 0,
                    }),
                    right: Box::new(lit(Value::I32(1))),
                },
                ty: int(true),
                line: 0,
            },
        )],
        spool: false,
    });

    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    let rowcount = {
        let snap = txn_mgr.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&catalog)
            .with_handle(&handle)
            .with_session(&mut session);
        let (outcome, _) = execute_collect(&stmt, &mut ctx).unwrap();
        assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));
        session.rowcount
    };
    assert_eq!(rowcount, 1);

    let snap = txn_mgr.statement_snapshot(&handle);
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let scan = PhysicalStatement::Query(scan_plan(
        meta.storage_id,
        vec![
            col_binding(0, "a", int(false)),
            col_binding(1, "b", int(true)),
        ],
        OutputSchema {
            columns: vec![out_col("a", int(false)), out_col("b", int(true))],
        },
    ));
    let (_, set) = execute_collect(&scan, &mut ctx).unwrap();
    assert_eq!(set.rows.len(), 3);
    assert_eq!(set.rows[0], vec![Value::I32(10), Value::I32(100)]);
    assert_eq!(set.rows[1], vec![Value::I32(21), Value::I32(200)]);
    assert_eq!(set.rows[2], vec![Value::I32(30), Value::I32(300)]);
}

/// A `DELETE` on a filtered scan removes one row and the count is 1.
#[test]
fn delete_counts_and_disappears() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).unwrap();
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("delete_test"),
                columns: vec![ColumnDef {
                    name: "a".to_owned(),
                    ty: int(true),
                    default: None,
                    identity: None,
                    computed: None,
                }],
                constraints: Vec::new(),
            },
        )
        .unwrap();

    for val in &[Value::I32(5), Value::I32(15), Value::I32(25)] {
        storage
            .insert(
                handle.id,
                meta.storage_id,
                &vauban_storage::Row(vec![val.clone()]),
            )
            .unwrap();
    }

    let col_a = col_binding(0, "a", int(true));
    let predicate = BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Gt,
            left: Box::new(BoundExpr {
                kind: BoundExprKind::ColumnRef(col_a.clone()),
                ty: int(true),
                line: 0,
            }),
            right: Box::new(lit(Value::I32(20))),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 0,
    };

    let stmt = PhysicalStatement::Delete(PhysicalDelete {
        table: meta.storage_id,
        input: PhysicalPlan::Filter {
            input: Box::new(scan_plan(
                meta.storage_id,
                vec![col_a.clone()],
                OutputSchema {
                    columns: vec![out_col("a", int(true))],
                },
            )),
            predicate,
        },
        spool: false,
    });

    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    let rowcount = {
        let snap = txn_mgr.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&catalog)
            .with_handle(&handle)
            .with_session(&mut session);
        let (outcome, _) = execute_collect(&stmt, &mut ctx).unwrap();
        assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));
        session.rowcount
    };
    assert_eq!(rowcount, 1);

    let snap = txn_mgr.statement_snapshot(&handle);
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let scan = PhysicalStatement::Query(scan_plan(
        meta.storage_id,
        vec![col_binding(0, "a", int(true))],
        OutputSchema {
            columns: vec![out_col("a", int(true))],
        },
    ));
    let (_, set) = execute_collect(&scan, &mut ctx).unwrap();
    assert_eq!(set.rows.len(), 2);
    assert_eq!(set.rows[0], vec![Value::I32(5)]);
    assert_eq!(set.rows[1], vec![Value::I32(15)]);
}

/// An `UPDATE` whose filter matches nothing answers 0 rows, no error.
#[test]
fn update_matching_nothing_is_zero() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).unwrap();
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("update_zero"),
                columns: vec![ColumnDef {
                    name: "a".to_owned(),
                    ty: int(true),
                    default: None,
                    identity: None,
                    computed: None,
                }],
                constraints: Vec::new(),
            },
        )
        .unwrap();

    storage
        .insert(
            handle.id,
            meta.storage_id,
            &vauban_storage::Row(vec![Value::I32(5)]),
        )
        .unwrap();

    let col_a = col_binding(0, "a", int(true));
    let predicate = BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Gt,
            left: Box::new(BoundExpr {
                kind: BoundExprKind::ColumnRef(col_a.clone()),
                ty: int(true),
                line: 0,
            }),
            right: Box::new(lit(Value::I32(100))),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 0,
    };

    let stmt = PhysicalStatement::Update(PhysicalUpdate {
        table: meta.storage_id,
        input: PhysicalPlan::Filter {
            input: Box::new(scan_plan(
                meta.storage_id,
                vec![col_a.clone()],
                OutputSchema {
                    columns: vec![out_col("a", int(true))],
                },
            )),
            predicate,
        },
        assignments: vec![(
            col_binding(0, "a", int(true)),
            BoundExpr {
                kind: BoundExprKind::Arith {
                    op: BinaryOp::Add,
                    left: Box::new(BoundExpr {
                        kind: BoundExprKind::ColumnRef(col_a),
                        ty: int(true),
                        line: 0,
                    }),
                    right: Box::new(lit(Value::I32(1))),
                },
                ty: int(true),
                line: 0,
            },
        )],
        spool: false,
    });

    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    let rowcount = {
        let snap = txn_mgr.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&catalog)
            .with_handle(&handle)
            .with_session(&mut session);
        let (outcome, _) = execute_collect(&stmt, &mut ctx).unwrap();
        assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));
        session.rowcount
    };
    assert_eq!(rowcount, 0);
}

/// An `UPDATE` of an indexed column with `spool: true` updates each row once.
#[test]
fn halloween_spool_updates_each_row_once() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).unwrap();
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("halloween_test"),
                columns: vec![ColumnDef {
                    name: "a".to_owned(),
                    ty: int(false),
                    default: None,
                    identity: None,
                    computed: None,
                }],
                constraints: Vec::new(),
            },
        )
        .unwrap();

    let idx = catalog
        .create_index(
            &handle,
            &IndexDef {
                table: meta.id,
                name: "ix_halloween".to_owned(),
                columns: vec![SortedColumn {
                    column: "a".to_owned(),
                    descending: false,
                }],
                unique: false,
                clustered: false,
            },
        )
        .unwrap();

    storage
        .insert(
            handle.id,
            meta.storage_id,
            &vauban_storage::Row(vec![Value::I32(10)]),
        )
        .unwrap();
    storage
        .insert(
            handle.id,
            meta.storage_id,
            &vauban_storage::Row(vec![Value::I32(20)]),
        )
        .unwrap();

    let col_a = col_binding(0, "a", int(false));

    let stmt = PhysicalStatement::Update(PhysicalUpdate {
        table: meta.storage_id,
        input: PhysicalPlan::IndexSeek {
            index: idx.id,
            range: KeyRangeExpr::Full,
            columns: vec![col_a.clone()],
            direction: Direction::Forward,
            schema: OutputSchema {
                columns: vec![out_col("a", int(false))],
            },
        },
        assignments: vec![(
            col_a.clone(),
            BoundExpr {
                kind: BoundExprKind::Arith {
                    op: BinaryOp::Add,
                    left: Box::new(BoundExpr {
                        kind: BoundExprKind::ColumnRef(col_a.clone()),
                        ty: int(false),
                        line: 0,
                    }),
                    right: Box::new(lit(Value::I32(1))),
                },
                ty: int(false),
                line: 0,
            },
        )],
        spool: true,
    });

    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    let rowcount = {
        let snap = txn_mgr.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&catalog)
            .with_handle(&handle)
            .with_session(&mut session);
        let (outcome, _) = execute_collect(&stmt, &mut ctx).unwrap();
        assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));
        session.rowcount
    };
    assert_eq!(rowcount, 2);

    let snap = txn_mgr.statement_snapshot(&handle);
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let scan = PhysicalStatement::Query(scan_plan(
        meta.storage_id,
        vec![col_binding(0, "a", int(false))],
        OutputSchema {
            columns: vec![out_col("a", int(false))],
        },
    ));
    let (_, set) = execute_collect(&scan, &mut ctx).unwrap();
    assert_eq!(set.rows.len(), 2);
    assert_eq!(set.rows[0], vec![Value::I32(11)]);
    assert_eq!(set.rows[1], vec![Value::I32(21)]);
}

/// A write conflict on a SNAPSHOT isolation update raises 3960.
#[test]
fn write_conflict_in_snapshot_is_3960() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));

    let db = vauban_storage::DbId(1);
    txn_mgr.set_versioning_options(
        db,
        VersioningOptions {
            allow_snapshot_isolation: true,
            read_committed_snapshot: false,
        },
    );

    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).unwrap();
    let create_handle = txn_mgr.begin(IsolationLevel::Snapshot);
    let meta = catalog
        .create_table(
            &create_handle,
            &TableDef {
                name: tbl_name("conflict_test"),
                columns: vec![ColumnDef {
                    name: "a".to_owned(),
                    ty: int(true),
                    default: None,
                    identity: None,
                    computed: None,
                }],
                constraints: Vec::new(),
            },
        )
        .unwrap();
    txn_mgr.commit(create_handle).unwrap();

    let insert_handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    storage
        .insert(
            insert_handle.id,
            meta.storage_id,
            &vauban_storage::Row(vec![Value::I32(42)]),
        )
        .unwrap();
    txn_mgr.commit(insert_handle).unwrap();

    let txn1 = txn_mgr.begin(IsolationLevel::Snapshot);
    let snap1 = txn_mgr.statement_snapshot(&txn1);

    let txn2 = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let snap2 = txn_mgr.statement_snapshot(&txn2);
    {
        let col_a = col_binding(0, "a", int(true));
        let stmt2 = PhysicalStatement::Update(PhysicalUpdate {
            table: meta.storage_id,
            input: scan_plan(
                meta.storage_id,
                vec![col_a.clone()],
                OutputSchema {
                    columns: vec![out_col("a", int(true))],
                },
            ),
            assignments: vec![(
                col_binding(0, "a", int(true)),
                BoundExpr {
                    kind: BoundExprKind::Literal(Value::I32(99)),
                    ty: int(true),
                    line: 0,
                },
            )],
            spool: false,
        });
        let eval = StaticContext::default();
        let mut session2 = ExecSession::default();
        let mut ctx2 = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap2,
            )
            .with_catalog(&catalog)
            .with_handle(&txn2)
            .with_session(&mut session2);
        execute_collect(&stmt2, &mut ctx2).unwrap();
    }
    txn_mgr.commit(txn2).unwrap();

    let col_a = col_binding(0, "a", int(true));
    let stmt1 = PhysicalStatement::Update(PhysicalUpdate {
        table: meta.storage_id,
        input: scan_plan(
            meta.storage_id,
            vec![col_a.clone()],
            OutputSchema {
                columns: vec![out_col("a", int(true))],
            },
        ),
        assignments: vec![(
            col_binding(0, "a", int(true)),
            BoundExpr {
                kind: BoundExprKind::Literal(Value::I32(100)),
                ty: int(true),
                line: 0,
            },
        )],
        spool: false,
    });

    let eval = StaticContext::default();
    let mut session1 = ExecSession::default();
    let err = {
        let mut ctx1 = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap1,
            )
            .with_catalog(&catalog)
            .with_handle(&txn1)
            .with_session(&mut session1);
        execute_collect(&stmt1, &mut ctx1).unwrap_err()
    };
    assert_eq!(err.number, 3960);
    assert_eq!(err.severity, 16);
}
