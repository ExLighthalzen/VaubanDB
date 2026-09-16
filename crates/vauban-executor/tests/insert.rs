//! Integration tests for `INSERT`: VALUES, DEFAULT, IDENTITY, NOT NULL, conversion,
//! spool and row counts.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, CompareOp, LockHints, OutputColumn, OutputSchema,
    SessionOptions,
};
use vauban_catalog::{Catalog, ColumnDef, IdentitySpec, QualifiedName, TableDef};
use vauban_executor::{ExecContext, ExecSession, execute_collect};
use vauban_parser::{Expr, Literal, Span};
use vauban_planner::{PhysicalInsert, PhysicalPlan, PhysicalStatement};
use vauban_storage::MemoryStorage;
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

/// Builds an `int` type, nullable or not.
fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

/// Builds a `varchar(n)` type, nullable or not.
fn varchar(len: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(len)), nullable)
}

/// A `BoundExpr` literal.
fn lit(value: Value) -> BoundExpr {
    let ty = match &value {
        Value::I32(_) | Value::I64(_) => int(true),
        Value::String(_) => varchar(100, true),
        _ => int(true),
    };
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 0,
    }
}

/// Wraps a `Vec<Value>` into a `Values` source plan.
fn values_plan(rows: Vec<Vec<Value>>, column_types: &[TypeInfo]) -> PhysicalPlan {
    let bound_rows: Vec<Vec<BoundExpr>> = rows
        .into_iter()
        .map(|row| row.into_iter().map(lit).collect())
        .collect();
    let schema = OutputSchema {
        columns: column_types
            .iter()
            .map(|ty| OutputColumn {
                name: String::new(),
                ty: ty.clone(),
            })
            .collect(),
    };
    PhysicalPlan::Values {
        rows: bound_rows,
        schema,
    }
}

/// Builds a table name on `master.dbo.{name}`.
fn tbl_name(name: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "dbo".to_owned(),
        name: name.to_owned(),
    }
}

/// Builds a `ColumnBinding` for a column of the given ordinal and type.
fn col_binding(index: usize, name: &str, ty: TypeInfo) -> ColumnBinding {
    ColumnBinding {
        column: vauban_catalog::ColumnId(index as i32),
        index,
        name: name.to_owned(),
        ty,
    }
}

/// One `OutputColumn` with the given name and type.
fn out_col(name: &str, ty: TypeInfo) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        ty,
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[test]
fn insert_values_then_scan_returns_the_rows() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("insert_test_1"),
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
        .expect("create_table succeeds");

    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: meta.storage_id,
        columns: vec![
            col_binding(0, "a", int(false)),
            col_binding(1, "b", int(true)),
        ],
        source: values_plan(
            vec![
                vec![Value::I32(10), Value::I32(100)],
                vec![Value::I32(20), Value::I32(200)],
                vec![Value::I32(30), Value::I32(300)],
            ],
            &[int(false), int(true)],
        ),
        spool: false,
    });

    let eval = StaticContext::default();
    let snap = txn_mgr.statement_snapshot(&handle);
    let mut session = ExecSession::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let (outcome, _) = execute_collect(&stmt, &mut ctx).expect("INSERT succeeds");
    assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));

    let scan_plan = PhysicalPlan::TableScan {
        table: meta.storage_id,
        columns: vec![
            col_binding(0, "a", int(false)),
            col_binding(1, "b", int(true)),
        ],
        schema: OutputSchema {
            columns: vec![out_col("a", int(false)), out_col("b", int(true))],
        },
        alias: "".to_owned(),
        hints: LockHints::default(),
    };
    let scan_stmt = PhysicalStatement::Query(scan_plan);
    let (_, set) = execute_collect(&scan_stmt, &mut ctx).expect("scan succeeds");
    assert_eq!(set.rows.len(), 3);
    assert_eq!(set.rows[0], vec![Value::I32(10), Value::I32(100)]);
    assert_eq!(set.rows[1], vec![Value::I32(20), Value::I32(200)]);
    assert_eq!(set.rows[2], vec![Value::I32(30), Value::I32(300)]);
}

#[test]
fn missing_column_takes_its_default() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("insert_test_default"),
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
                        default: Some(Expr::Literal(
                            Literal::Integer("42".to_owned()),
                            Span::EMPTY,
                        )),
                        identity: None,
                        computed: None,
                    },
                ],
                constraints: Vec::new(),
            },
        )
        .expect("create_table succeeds");

    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", int(false))],
        source: values_plan(vec![vec![Value::I32(7)]], &[int(false)]),
        spool: false,
    });

    let eval = StaticContext::default();
    let snap = txn_mgr.statement_snapshot(&handle);
    let mut session = ExecSession::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let (outcome, _) = execute_collect(&stmt, &mut ctx).expect("INSERT succeeds");
    assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));

    let read_plan = PhysicalPlan::TableScan {
        table: meta.storage_id,
        columns: vec![
            col_binding(0, "a", int(false)),
            col_binding(1, "b", int(false)),
        ],
        schema: OutputSchema {
            columns: vec![out_col("a", int(false)), out_col("b", int(false))],
        },
        alias: "".to_owned(),
        hints: LockHints::default(),
    };
    let read_stmt = PhysicalStatement::Query(read_plan);
    let (_, set) = execute_collect(&read_stmt, &mut ctx).expect("scan succeeds");
    assert_eq!(set.rows.len(), 1, "one row was inserted");
    assert_eq!(set.rows[0], vec![Value::I32(7), Value::I64(42)]);
}

#[test]
fn missing_not_null_column_is_515() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("insert_test_515"),
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
        .expect("create_table succeeds");

    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", int(false))],
        source: values_plan(vec![vec![Value::I32(7)]], &[int(false)]),
        spool: false,
    });

    let eval = StaticContext::default();
    let snap = txn_mgr.statement_snapshot(&handle);
    let mut session = ExecSession::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let err = execute_collect(&stmt, &mut ctx).unwrap_err();
    assert_eq!(err.number, 515, "error number is 515");
    assert_eq!(err.severity, 16, "severity is 16");
    assert!(
        err.message.contains("b"),
        "error message cites the column: {}",
        err.message
    );
    assert!(
        err.message.contains("INSERT"),
        "error message cites INSERT: {}",
        err.message
    );
}

#[test]
fn identity_increments_per_row() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("insert_test_id"),
                columns: vec![
                    ColumnDef {
                        name: "id".to_owned(),
                        ty: int(false),
                        default: None,
                        identity: Some(IdentitySpec::default()),
                        computed: None,
                    },
                    ColumnDef {
                        name: "v".to_owned(),
                        ty: int(true),
                        default: None,
                        identity: None,
                        computed: None,
                    },
                ],
                constraints: Vec::new(),
            },
        )
        .expect("create_table succeeds");

    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: meta.storage_id,
        columns: vec![col_binding(1, "v", int(true))],
        source: values_plan(
            vec![
                vec![Value::I32(10)],
                vec![Value::I32(20)],
                vec![Value::I32(30)],
            ],
            &[int(true)],
        ),
        spool: false,
    });

    let eval = StaticContext::default();
    let snap = txn_mgr.statement_snapshot(&handle);
    let mut session = ExecSession::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let (outcome, _) = execute_collect(&stmt, &mut ctx).expect("INSERT succeeds");
    assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));

    let read_plan = PhysicalPlan::TableScan {
        table: meta.storage_id,
        columns: vec![
            col_binding(0, "id", int(false)),
            col_binding(1, "v", int(true)),
        ],
        schema: OutputSchema {
            columns: vec![out_col("id", int(false)), out_col("v", int(true))],
        },
        alias: "".to_owned(),
        hints: LockHints::default(),
    };
    let read_stmt = PhysicalStatement::Query(read_plan);
    let (_, set) = execute_collect(&read_stmt, &mut ctx).expect("scan succeeds");
    assert_eq!(set.rows.len(), 3);
    assert_eq!(set.rows[0], vec![Value::I32(1), Value::I32(10)]);
    assert_eq!(set.rows[1], vec![Value::I32(2), Value::I32(20)]);
    assert_eq!(set.rows[2], vec![Value::I32(3), Value::I32(30)]);
}

#[test]
fn string_too_long_truncates_silently() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("insert_test_trunc"),
                columns: vec![ColumnDef {
                    name: "a".to_owned(),
                    ty: varchar(3, false),
                    default: None,
                    identity: None,
                    computed: None,
                }],
                constraints: Vec::new(),
            },
        )
        .expect("create_table succeeds");

    let long_val = Value::String(SqlString {
        text: "abcd".to_owned(),
    });
    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", varchar(3, false))],
        source: values_plan(vec![vec![long_val]], &[varchar(10, false)]),
        spool: false,
    });

    let eval = StaticContext::default();
    let snap = txn_mgr.statement_snapshot(&handle);
    let mut session = ExecSession::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let (outcome, _) =
        execute_collect(&stmt, &mut ctx).expect("INSERT succeeds with silent truncation");
    assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));

    // Read back: the string was truncated to 3 characters.
    let read_plan = PhysicalPlan::TableScan {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", varchar(3, false))],
        schema: OutputSchema {
            columns: vec![out_col("a", varchar(3, false))],
        },
        alias: "".to_owned(),
        hints: LockHints::default(),
    };
    let read_stmt = PhysicalStatement::Query(read_plan);
    let (_, set) = execute_collect(&read_stmt, &mut ctx).expect("readback works");
    assert_eq!(
        set.rows.len(),
        1,
        "one row was inserted despite the truncation"
    );
    assert_eq!(
        set.rows[0][0],
        Value::String(SqlString {
            text: "abc".to_owned(),
        }),
        "the value was truncated to 3 characters"
    );
}

#[test]
fn insert_from_plan_counts_rows() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("insert_test_count"),
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
        .expect("create_table succeeds");

    let filter_pred = BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Gt,
            left: Box::new(BoundExpr {
                kind: BoundExprKind::ColumnRef(col_binding(0, "a", int(true))),
                ty: int(true),
                line: 0,
            }),
            right: Box::new(BoundExpr {
                kind: BoundExprKind::Literal(Value::I32(10)),
                ty: int(true),
                line: 0,
            }),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 0,
    };
    let source = PhysicalPlan::Filter {
        input: Box::new(values_plan(
            vec![
                vec![Value::I32(5)],
                vec![Value::I32(15)],
                vec![Value::I32(25)],
                vec![Value::I32(3)],
            ],
            &[int(true)],
        )),
        predicate: filter_pred,
    };

    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", int(true))],
        source,
        spool: false,
    });

    let eval = StaticContext::default();
    let snap = txn_mgr.statement_snapshot(&handle);
    let mut session = ExecSession::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let (outcome, _) = execute_collect(&stmt, &mut ctx).expect("INSERT succeeds");
    assert!(
        matches!(outcome, vauban_executor::ExecOutcome::NoRows),
        "INSERT answers NoRows"
    );
    assert_eq!(session.rowcount, 2, "two rows passed the filter");
}

#[test]
fn insert_reading_its_own_target_is_materialized() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("insert_test_spool"),
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
        .expect("create_table succeeds");

    // Insert two rows first.
    let seed_stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", int(true))],
        source: values_plan(vec![vec![Value::I32(1)], vec![Value::I32(2)]], &[int(true)]),
        spool: false,
    });

    let eval = StaticContext::default();
    let snap = txn_mgr.statement_snapshot(&handle);
    let mut session = ExecSession::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(
            storage.as_ref() as &dyn vauban_storage::Storage,
            txn_mgr.as_ref(),
            &snap,
        )
        .with_catalog(&catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    execute_collect(&seed_stmt, &mut ctx).expect("seed INSERT succeeds");

    // INSERT INTO t SELECT a FROM t — spool=true
    let src_scan = PhysicalPlan::TableScan {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", int(true))],
        schema: OutputSchema {
            columns: vec![out_col("a", int(true))],
        },
        alias: "".to_owned(),
        hints: LockHints::default(),
    };
    let copy_stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", int(true))],
        source: src_scan,
        spool: true,
    });

    let (outcome, _) = execute_collect(&copy_stmt, &mut ctx).expect("spooled INSERT succeeds");
    assert!(matches!(outcome, vauban_executor::ExecOutcome::NoRows));

    let read_plan = PhysicalPlan::TableScan {
        table: meta.storage_id,
        columns: vec![col_binding(0, "a", int(true))],
        schema: OutputSchema {
            columns: vec![out_col("a", int(true))],
        },
        alias: "".to_owned(),
        hints: LockHints::default(),
    };
    let read_stmt = PhysicalStatement::Query(read_plan);
    let (_, set) = execute_collect(&read_stmt, &mut ctx).expect("scan succeeds");
    assert_eq!(set.rows.len(), 4, "two originals + two copies");
}
