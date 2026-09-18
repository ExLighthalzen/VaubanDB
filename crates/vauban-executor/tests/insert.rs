//! Integration tests for `INSERT`: VALUES, DEFAULT, IDENTITY, NOT NULL, conversion,
//! spool and row counts.

use std::slice;
use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, CompareOp, LockHints, OutputColumn, OutputSchema,
    SessionOptions,
};
use vauban_catalog::{Catalog, ColumnDef, ConstraintDef, IdentitySpec, QualifiedName, TableDef};
use vauban_errors::SqlResult;
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, RowSet, execute_collect};
use vauban_parser::{BinaryOp, Expr, Literal, Span, UnaryOp};
use vauban_planner::{PhysicalInsert, PhysicalPlan, PhysicalStatement};
use vauban_storage::{MemoryStorage, TableId};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{Decimal, Len, SqlString, SqlType, TypeInfo, Value};

/// Builds an `int` type, nullable or not.
fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

/// Builds a `varchar(n)` type, nullable or not.
fn varchar(len: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(len)), nullable)
}

/// Builds an `nvarchar(n)` type, nullable or not.
fn nvarchar(len: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Fixed(len)), nullable)
}

/// Builds a `varbinary(n)` type, nullable or not.
fn varbinary(len: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::VarBinary(Len::Fixed(len)), nullable)
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

/// The four pieces an `ExecContext` borrows, plus the session it writes: an engine one
/// test runs several statements on.
///
/// `run` rebuilds the context per statement, because the statement snapshot does, and it
/// destructures `self` field by field so that the catalogue and the session are borrowed
/// separately.
struct Fixture {
    storage: Arc<MemoryStorage>,
    txn_mgr: Arc<TransactionManager>,
    catalog: Catalog,
    handle: TxnHandle,
    eval: StaticContext,
    session: ExecSession,
}

impl Fixture {
    fn new() -> Self {
        let storage = Arc::new(MemoryStorage::new());
        let txn_mgr = Arc::new(TransactionManager::new(
            storage.clone() as Arc<dyn vauban_storage::Storage>
        ));
        let catalog =
            Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
        let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
        Self {
            storage,
            txn_mgr,
            catalog,
            handle,
            eval: StaticContext::default(),
            session: ExecSession::default(),
        }
    }

    /// Creates `name` with `columns` and answers the storage identifier of the table.
    fn create(&self, name: &str, columns: Vec<ColumnDef>) -> TableId {
        self.catalog
            .create_table(
                &self.handle,
                &TableDef {
                    name: tbl_name(name),
                    columns,
                    constraints: Vec::new(),
                },
            )
            .expect("create_table succeeds")
            .storage_id
    }

    fn run(&mut self, stmt: &PhysicalStatement) -> SqlResult<(ExecOutcome, RowSet)> {
        let Self {
            storage,
            txn_mgr,
            catalog,
            handle,
            eval,
            session,
        } = self;
        let snap = txn_mgr.statement_snapshot(handle);
        let mut ctx = ExecContext::scalar(eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(catalog)
            .with_handle(handle)
            .with_session(session);
        execute_collect(stmt, &mut ctx)
    }

    /// Every row of `table`, read through the columns `bindings` names.
    fn read(&mut self, table: TableId, bindings: &[ColumnBinding]) -> Vec<Vec<Value>> {
        let stmt = PhysicalStatement::Query(PhysicalPlan::TableScan {
            table,
            columns: bindings.to_vec(),
            schema: OutputSchema {
                columns: bindings
                    .iter()
                    .map(|b| out_col(&b.name, b.ty.clone()))
                    .collect(),
            },
            alias: String::new(),
            hints: LockHints::default(),
        });
        self.run(&stmt).expect("the scan succeeds").1.rows
    }
}

/// One column, with the `DEFAULT` expression it carries and nothing else.
fn column(name: &str, ty: TypeInfo, default: Option<Expr>) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty,
        default,
        identity: None,
        computed: None,
    }
}

/// An `INSERT` of one `VALUES` row, written for the columns of `bindings`.
fn insert_one(table: TableId, bindings: &[ColumnBinding], row: Vec<Value>) -> PhysicalStatement {
    let types: Vec<TypeInfo> = bindings.iter().map(|b| b.ty.clone()).collect();
    PhysicalStatement::Insert(PhysicalInsert {
        table,
        columns: bindings.to_vec(),
        source: values_plan(vec![row], &types),
        spool: false,
    })
}

/// A literal `Expr` of the AST, as a `DEFAULT` constraint holds it.
fn default_of(literal: Literal) -> Expr {
    Expr::Literal(literal, Span::EMPTY)
}

/// `- expr`, the shape the parser gives `DEFAULT -5`.
fn negated(expr: Expr) -> Expr {
    Expr::Unary {
        op: UnaryOp::Minus,
        expr: Box::new(expr),
        span: Span::EMPTY,
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
    assert_eq!(set.rows[0], vec![Value::I32(7), Value::I32(42)]);
}

#[test]
fn missing_column_takes_its_named_default() {
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
                name: tbl_name("insert_test_named_default"),
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
                constraints: vec![ConstraintDef::Default {
                    name: Some("df_b".to_owned()),
                    column: "b".to_owned(),
                    expr: Expr::Literal(Literal::Integer("42".to_owned()), Span::EMPTY),
                }],
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
    assert_eq!(
        set.rows[0],
        vec![Value::I32(7), Value::I32(42)],
        "the named DEFAULT filled b"
    );
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
fn string_too_long_for_a_varchar_column_is_2628() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_trunc_varchar",
        vec![column("code", varchar(10, false), None)],
    );
    let code = col_binding(0, "code", varchar(10, false));
    let long = Value::String(SqlString {
        text: "ABCDEFGHIJK".to_owned(),
    });

    let err = f
        .run(&insert_one(
            table,
            slice::from_ref(&code),
            vec![long.clone()],
        ))
        .expect_err("eleven characters do not fit varchar(10)");
    assert_eq!(err.number, 2628);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert!(
        err.message.contains("master.dbo.insert_trunc_varchar")
            && err.message.contains("'code'")
            && err.message.contains("'ABCDEFGHIJ'"),
        "{}",
        err.message
    );
    assert!(f.read(table, &[code]).is_empty(), "nothing was written");
}

#[test]
fn string_too_long_for_an_nvarchar_column_is_2628() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_trunc_nvarchar",
        vec![column("nom", nvarchar(5, false), None)],
    );
    let nom = col_binding(0, "nom", nvarchar(5, false));
    let long = Value::String(SqlString {
        text: "aaaaaa".to_owned(),
    });

    let err = f
        .run(&insert_one(table, slice::from_ref(&nom), vec![long]))
        .expect_err("six characters do not fit nvarchar(5)");
    assert_eq!(err.number, 2628);
    assert_eq!(err.state, 1);
    assert!(
        err.message.contains("'nom'") && err.message.contains("'aaaaa'"),
        "{}",
        err.message
    );
}

#[test]
fn binary_too_long_for_a_varbinary_column_is_2628() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_trunc_varbinary",
        vec![column("b", varbinary(3, false), None)],
    );
    let b = col_binding(0, "b", varbinary(3, false));
    let long = Value::Bytes(vec![1, 2, 3, 4]);

    let err = f
        .run(&insert_one(table, slice::from_ref(&b), vec![long]))
        .expect_err("four bytes do not fit varbinary(3)");
    assert_eq!(err.number, 2628);
    assert_eq!(err.state, 1);
    assert!(err.message.contains("'b'"), "{}", err.message);
}

#[test]
fn a_default_literal_too_long_for_the_column_is_2628() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_trunc_default",
        vec![
            column("k", int(false), None),
            column(
                "d",
                varchar(2, false),
                Some(default_of(Literal::Str {
                    value: "abcd".to_owned(),
                    unicode: false,
                })),
            ),
        ],
    );
    let k = col_binding(0, "k", int(false));

    let err = f
        .run(&insert_one(table, slice::from_ref(&k), vec![Value::I32(1)]))
        .expect_err("the default is too long for the column");
    assert_eq!(err.number, 2628);
    assert_eq!(err.state, 1);
}

#[test]
fn a_value_that_fits_still_inserts() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_trunc_fits",
        vec![column("code", varchar(10, false), None)],
    );
    let code = col_binding(0, "code", varchar(10, false));
    let val = Value::String(SqlString {
        text: "ABCDEFGHIJ".to_owned(),
    });

    f.run(&insert_one(
        table,
        slice::from_ref(&code),
        vec![val.clone()],
    ))
    .expect("ten characters fit varchar(10)");
    assert_eq!(f.read(table, &[code]), vec![vec![val]]);
}

#[test]
fn an_explicit_cast_still_truncates_without_error() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_cast_trunc",
        vec![column("a", varchar(2, false), None)],
    );
    let a = col_binding(0, "a", varchar(2, false));
    let inner = BoundExpr {
        kind: BoundExprKind::Literal(Value::String(SqlString {
            text: "abcdef".to_owned(),
        })),
        ty: varchar(10, false),
        line: 0,
    };
    let cast = BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(inner),
            style: None,
            try_: false,
        },
        ty: varchar(2, false),
        line: 0,
    };
    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table,
        columns: vec![a.clone()],
        source: PhysicalPlan::Values {
            rows: vec![vec![cast]],
            schema: OutputSchema {
                columns: vec![out_col("a", varchar(2, false))],
            },
        },
        spool: false,
    });

    f.run(&stmt).expect("CAST already cut the string");
    assert_eq!(
        f.read(table, &[a]),
        vec![vec![Value::String(SqlString {
            text: "ab".to_owned(),
        })]]
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

// ---------------------------------------------------------------
// A NULL written into a column that refuses it
// ---------------------------------------------------------------

/// A `NULL` in the `VALUES` row answers exactly what omitting the column answers: the
/// refusal does not depend on how the `NULL` got there.
#[test]
fn an_explicit_null_in_a_not_null_column_is_the_515_of_the_omission() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_null_written",
        vec![column("a", int(false), None), column("b", int(false), None)],
    );
    let a = col_binding(0, "a", int(false));
    let b = col_binding(1, "b", int(false));

    let written = f
        .run(&insert_one(
            table,
            &[a.clone(), b.clone()],
            vec![Value::I32(7), Value::Null],
        ))
        .expect_err("NULL in a NOT NULL column");
    let omitted = f
        .run(&insert_one(table, &[a], vec![Value::I32(7)]))
        .expect_err("b is missing");

    assert_eq!(written.number, 515);
    assert_eq!(written.severity, 16);
    assert_eq!(written.state, 2);
    assert!(
        written.message.contains("'b'")
            && written.message.contains("master.dbo.insert_null_written"),
        "the column and the three-part table name are in {}",
        written.message
    );
    assert!(written.message.contains("INSERT"), "{}", written.message);
    assert_eq!(
        (
            written.number,
            written.severity,
            written.state,
            written.message
        ),
        (
            omitted.number,
            omitted.severity,
            omitted.state,
            omitted.message
        ),
        "the written NULL and the omission answer the same thing"
    );
}

/// The refused row never reaches the storage, so nothing is left for a later read to trip
/// over: the scan that follows answers zero rows instead of hanging on a value its column
/// metadata says cannot be `NULL`.
#[test]
fn a_refused_null_leaves_nothing_to_read() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_null_nothing_written",
        vec![column("a", int(false), None)],
    );
    let a = col_binding(0, "a", int(false));

    f.run(&insert_one(table, slice::from_ref(&a), vec![Value::Null]))
        .expect_err("NULL in a NOT NULL column");
    assert!(
        f.read(table, &[a]).is_empty(),
        "the refused row was not written"
    );
}

/// The same refusal through an `INSERT … SELECT`: the `NULL` comes from a nullable column
/// of the source, not from a literal of the statement.
#[test]
fn a_null_from_a_select_source_is_515() {
    let mut f = Fixture::new();
    let source = f.create("insert_null_source", vec![column("n", int(true), None)]);
    let target = f.create("insert_null_target", vec![column("a", int(false), None)]);
    let n = col_binding(0, "n", int(true));
    let a = col_binding(0, "a", int(false));

    f.run(&insert_one(source, slice::from_ref(&n), vec![Value::Null]))
        .expect("a nullable column takes a NULL");

    let select = PhysicalPlan::TableScan {
        table: source,
        columns: vec![n.clone()],
        schema: OutputSchema {
            columns: vec![out_col("n", int(true))],
        },
        alias: String::new(),
        hints: LockHints::default(),
    };
    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table: target,
        columns: vec![a.clone()],
        source: select,
        spool: false,
    });
    let err = f.run(&stmt).expect_err("the source row carries a NULL");
    assert_eq!(err.number, 515);
    assert_eq!(err.state, 2);
    assert!(f.read(target, &[a]).is_empty(), "nothing was written");
}

/// And through a variable whose value is `NULL`.
#[test]
fn a_null_from_a_variable_is_515() {
    let mut f = Fixture::new();
    let table = f.create("insert_null_variable", vec![column("a", int(false), None)]);
    let a = col_binding(0, "a", int(false));
    f.session.variables.insert("@v".to_owned(), Value::Null);

    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table,
        columns: vec![a.clone()],
        source: PhysicalPlan::Values {
            rows: vec![vec![BoundExpr {
                kind: BoundExprKind::Variable {
                    name: "@v".to_owned(),
                },
                ty: int(true),
                line: 0,
            }]],
            schema: OutputSchema {
                columns: vec![out_col("a", int(true))],
            },
        },
        spool: false,
    });
    let err = f.run(&stmt).expect_err("@v is NULL");
    assert_eq!(err.number, 515);
    assert_eq!(err.state, 2);
}

/// The counter-proof of the three above: a column that accepts `NULL` still takes one, and
/// the row is there to be read.
#[test]
fn a_nullable_column_still_takes_an_explicit_null() {
    let mut f = Fixture::new();
    let table = f.create("insert_null_allowed", vec![column("a", int(true), None)]);
    let a = col_binding(0, "a", int(true));

    f.run(&insert_one(table, slice::from_ref(&a), vec![Value::Null]))
        .expect("a nullable column takes a NULL");
    assert_eq!(f.read(table, &[a]), vec![vec![Value::Null]]);
}

/// A `DEFAULT` constraint does not rescue a written `NULL`: the column is `NOT NULL` and
/// carries a default, and writing `NULL` refuses instead of applying it. The second half
/// of the test is the same table with the column omitted, which does apply the default —
/// without it the first half would pass on a build that ignored defaults altogether.
#[test]
fn a_written_null_does_not_fall_back_to_the_default() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_null_over_default",
        vec![
            column("k", int(false), None),
            column(
                "d",
                int(false),
                Some(default_of(Literal::Integer("7".to_owned()))),
            ),
        ],
    );
    let k = col_binding(0, "k", int(false));
    let d = col_binding(1, "d", int(false));

    let err = f
        .run(&insert_one(
            table,
            &[k.clone(), d.clone()],
            vec![Value::I32(1), Value::Null],
        ))
        .expect_err("the written NULL is refused");
    assert_eq!(err.number, 515);
    assert!(err.message.contains("'d'"), "{}", err.message);

    f.run(&insert_one(table, slice::from_ref(&k), vec![Value::I32(2)]))
        .expect("the omitted column takes its default");
    assert_eq!(
        f.read(table, &[k, d]),
        vec![vec![Value::I32(2), Value::I32(7)]],
        "the default filled d for the row that omitted it"
    );
}

/// A `DEFAULT NULL` on a `NOT NULL` column is refused where it would be applied: the
/// constraint holds a `NULL` and the column does not take one.
#[test]
fn a_null_default_on_a_not_null_column_is_refused() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_null_default",
        vec![
            column("k", int(true), None),
            column("d", int(false), Some(default_of(Literal::Null))),
        ],
    );
    let k = col_binding(0, "k", int(true));

    let err = f
        .run(&insert_one(table, &[k], vec![Value::I32(1)]))
        .expect_err("the default is NULL and the column is NOT NULL");
    assert_eq!(err.number, 515);
    assert_eq!(err.state, 2);
    assert!(err.message.contains("'d'"), "{}", err.message);
}

// ---------------------------------------------------------------
// What a DEFAULT constraint may hold
// ---------------------------------------------------------------

/// A signed literal, a binary literal and a parenthesised literal are written and read
/// back at the type of their column. Each of the four answered the internal error 50000
/// before this pair of functions read them.
#[test]
fn signed_and_binary_defaults() {
    let mut f = Fixture::new();
    let money = TypeInfo::new(
        SqlType::Decimal {
            precision: 9,
            scale: 2,
        },
        false,
    );
    let one_byte = TypeInfo::new(SqlType::Binary(Len::Fixed(1)), false);
    let table = f.create(
        "insert_defaults_signed",
        vec![
            column("k", int(false), None),
            column(
                "neg",
                int(false),
                Some(negated(default_of(Literal::Integer("5".to_owned())))),
            ),
            column(
                "bin",
                one_byte.clone(),
                Some(default_of(Literal::Binary("01".to_owned()))),
            ),
            column(
                "dec",
                money.clone(),
                Some(negated(default_of(Literal::Decimal("1.25".to_owned())))),
            ),
            column(
                "par",
                int(false),
                Some(Expr::Nested(
                    Box::new(default_of(Literal::Integer("8".to_owned()))),
                    Span::EMPTY,
                )),
            ),
        ],
    );
    let k = col_binding(0, "k", int(false));

    f.run(&insert_one(table, slice::from_ref(&k), vec![Value::I32(1)]))
        .expect("every default is a literal");
    let row = &f.read(
        table,
        &[
            k,
            col_binding(1, "neg", int(false)),
            col_binding(2, "bin", one_byte),
            col_binding(3, "dec", money),
            col_binding(4, "par", int(false)),
        ],
    )[0];
    assert_eq!(row[1], Value::I32(-5), "DEFAULT -5 on an int column");
    assert_eq!(row[2], Value::Bytes(vec![1]), "DEFAULT 0x01 on binary(1)");
    assert_eq!(
        row[3],
        Value::Decimal(Decimal {
            mantissa: -125,
            precision: 9,
            scale: 2,
        }),
        "DEFAULT -1.25 on decimal(9,2)"
    );
    assert_eq!(row[4], Value::I32(8), "DEFAULT (8) on an int column");
}

/// An arithmetic default is an expression and not a literal: it keeps the internal error
/// rather than being computed. The line that separates the two is the sign and the
/// parentheses of `signed_and_binary_defaults`, which are part of the literal.
#[test]
fn an_arithmetic_default_is_not_a_literal() {
    let mut f = Fixture::new();
    let table = f.create(
        "insert_defaults_arith",
        vec![
            column("k", int(true), None),
            column(
                "d",
                int(false),
                Some(Expr::Binary {
                    op: BinaryOp::Add,
                    op_span: Span::EMPTY,
                    left: Box::new(default_of(Literal::Integer("1".to_owned()))),
                    right: Box::new(default_of(Literal::Integer("1".to_owned()))),
                    span: Span::EMPTY,
                }),
            ),
        ],
    );
    let k = col_binding(0, "k", int(true));

    let err = f
        .run(&insert_one(table, &[k], vec![Value::I32(1)]))
        .expect_err("1 + 1 is not a literal");
    assert_eq!(err.number, 50_000);
}
