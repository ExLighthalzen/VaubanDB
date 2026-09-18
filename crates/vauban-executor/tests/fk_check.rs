//! Integration tests for `FOREIGN KEY` and `CHECK` at write time.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, LockHints, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_catalog::{Catalog, ColumnDef, ConstraintDef, QualifiedName, SortedColumn, TableDef};
use vauban_errors::SqlError;
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, execute_collect};
use vauban_parser::{BinaryOp, ColumnRef, Expr, Ident, Literal, Span};
use vauban_planner::{
    PhysicalDelete, PhysicalInsert, PhysicalPlan, PhysicalStatement, PhysicalUpdate,
};
use vauban_storage::Snapshot;
use vauban_storage::{MemoryStorage, Storage, TableId};
use vauban_sysfn::{StaticContext, register_builtins};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{SqlType, TypeInfo, Value};

fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

fn tbl_name(name: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "dbo".to_owned(),
        name: name.to_owned(),
    }
}

fn col_binding(index: usize, name: &str, ty: TypeInfo) -> ColumnBinding {
    ColumnBinding {
        column: vauban_catalog::ColumnId(index as i32),
        index,
        name: name.to_owned(),
        ty,
    }
}

fn lit(value: Value) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty: int(true),
        line: 0,
    }
}

fn values_plan(rows: Vec<Vec<Value>>, column_types: &[TypeInfo]) -> PhysicalPlan {
    let bound_rows: Vec<Vec<BoundExpr>> = rows
        .into_iter()
        .map(|row| row.into_iter().map(lit).collect())
        .collect();
    PhysicalPlan::Values {
        rows: bound_rows,
        schema: OutputSchema {
            columns: column_types
                .iter()
                .map(|ty| OutputColumn {
                    name: String::new(),
                    ty: ty.clone(),
                })
                .collect(),
        },
    }
}

fn primary_key(columns: &[&str]) -> ConstraintDef {
    ConstraintDef::PrimaryKey {
        name: Some("pk".to_owned()),
        columns: columns
            .iter()
            .map(|column| SortedColumn {
                column: (*column).to_owned(),
                descending: false,
            })
            .collect(),
        clustered: true,
    }
}

fn foreign_key(
    name: &str,
    columns: &[&str],
    referenced: &str,
    referenced_columns: &[&str],
) -> ConstraintDef {
    ConstraintDef::ForeignKey {
        name: Some(name.to_owned()),
        columns: columns.iter().map(|column| (*column).to_owned()).collect(),
        referenced: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: referenced.to_owned(),
        },
        referenced_columns: referenced_columns
            .iter()
            .map(|column| (*column).to_owned())
            .collect(),
        on_delete: vauban_parser::RefAction::NoAction,
        on_update: vauban_parser::RefAction::NoAction,
    }
}

fn check_positive(column: &str) -> ConstraintDef {
    ConstraintDef::Check {
        name: Some("CK_positive".to_owned()),
        expr: Expr::Binary {
            op: BinaryOp::Gt,
            op_span: Span::EMPTY,
            left: Box::new(Expr::Column(ColumnRef {
                qualifier: None,
                name: Ident {
                    value: column.to_owned(),
                    quoted: false,
                },
                span: Span::EMPTY,
            })),
            right: Box::new(Expr::Literal(Literal::Integer("0".to_owned()), Span::EMPTY)),
            span: Span::EMPTY,
        },
    }
}

struct Env {
    storage: Arc<MemoryStorage>,
    txn_mgr: Arc<TransactionManager>,
    catalog: Catalog,
    handle: TxnHandle,
    snap: Snapshot,
    eval: StaticContext,
    session: ExecSession,
}

impl Env {
    fn new() -> Self {
        register_builtins();
        let storage = Arc::new(MemoryStorage::new());
        let txn_mgr = Arc::new(TransactionManager::new(storage.clone() as Arc<dyn Storage>));
        let catalog =
            Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap succeeds");
        let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
        let snap = txn_mgr.statement_snapshot(&handle);
        Self {
            storage,
            txn_mgr,
            catalog,
            handle,
            snap,
            eval: StaticContext::default(),
            session: ExecSession::default(),
        }
    }

    fn ctx(&mut self) -> ExecContext<'_> {
        self.snap = self.txn_mgr.statement_snapshot(&self.handle);
        ExecContext::scalar(&self.eval, SessionOptions::default())
            .with_engine(
                self.storage.as_ref() as &dyn Storage,
                self.txn_mgr.as_ref(),
                &self.snap,
            )
            .with_catalog(&self.catalog)
            .with_handle(&self.handle)
            .with_session(&mut self.session)
    }

    fn run(&mut self, stmt: &PhysicalStatement) -> Result<ExecOutcome, SqlError> {
        execute_collect(stmt, &mut self.ctx()).map(|(outcome, _)| outcome)
    }

    fn create(
        &self,
        name: &str,
        columns: Vec<ColumnDef>,
        constraints: Vec<ConstraintDef>,
    ) -> TableId {
        self.catalog
            .create_table(
                &self.handle,
                &TableDef {
                    name: tbl_name(name),
                    columns,
                    constraints,
                },
            )
            .expect("create_table succeeds")
            .storage_id
    }

    fn insert(&mut self, table: TableId, bindings: &[ColumnBinding], row: Vec<Value>) {
        let types: Vec<TypeInfo> = bindings.iter().map(|b| b.ty.clone()).collect();
        let stmt = PhysicalStatement::Insert(PhysicalInsert {
            table,
            columns: bindings.to_vec(),
            source: values_plan(vec![row], &types),
            spool: false,
        });
        self.run(&stmt).expect("insert succeeds");
    }

    fn row_count(&mut self, table: TableId) -> usize {
        let stmt = PhysicalStatement::Query(PhysicalPlan::TableScan {
            table,
            columns: vec![col_binding(0, "a", int(false))],
            schema: OutputSchema {
                columns: vec![OutputColumn {
                    name: "a".to_owned(),
                    ty: int(false),
                }],
            },
            alias: String::new(),
            hints: LockHints::default(),
        });
        execute_collect(&stmt, &mut self.ctx())
            .expect("scan succeeds")
            .1
            .rows
            .len()
    }
}

fn parent_child_tables(env: &Env) -> (TableId, TableId) {
    let parent = env.create(
        "fk_check_parent",
        vec![
            ColumnDef {
                name: "id".to_owned(),
                ty: int(false),
                default: None,
                identity: None,
                computed: None,
            },
            ColumnDef {
                name: "code".to_owned(),
                ty: int(false),
                default: None,
                identity: None,
                computed: None,
            },
        ],
        vec![primary_key(&["id"])],
    );
    let child = env.create(
        "fk_check_child",
        vec![
            ColumnDef {
                name: "id".to_owned(),
                ty: int(false),
                default: None,
                identity: None,
                computed: None,
            },
            ColumnDef {
                name: "pid".to_owned(),
                ty: int(true),
                default: None,
                identity: None,
                computed: None,
            },
        ],
        vec![
            primary_key(&["id"]),
            foreign_key("FK_child_parent", &["pid"], "fk_check_parent", &["id"]),
        ],
    );
    (parent, child)
}

#[test]
fn insert_without_parent_is_547() {
    let mut env = Env::new();
    let (_parent, child) = parent_child_tables(&env);
    let bindings = vec![
        col_binding(0, "id", int(false)),
        col_binding(1, "pid", int(true)),
    ];
    let err = env
        .run(&PhysicalStatement::Insert(PhysicalInsert {
            table: child,
            columns: bindings.clone(),
            source: values_plan(
                vec![vec![Value::I32(1), Value::I32(99)]],
                &[int(false), int(true)],
            ),
            spool: false,
        }))
        .expect_err("orphan insert fails");
    assert_eq!(err.number, 547);
    assert_eq!(err.severity, 16);
    assert!(err.message.contains("FK_child_parent"));
    assert!(err.message.contains("master"));
    assert!(err.message.contains("dbo.fk_check_parent"));
    assert!(err.message.contains("'id'"));
}

#[test]
fn insert_with_parent_succeeds() {
    let mut env = Env::new();
    let (parent, child) = parent_child_tables(&env);
    let bindings = vec![
        col_binding(0, "id", int(false)),
        col_binding(1, "pid", int(true)),
    ];
    env.insert(
        parent,
        &[
            col_binding(0, "id", int(false)),
            col_binding(1, "code", int(false)),
        ],
        vec![Value::I32(1), Value::I32(10)],
    );
    env.insert(child, &bindings, vec![Value::I32(1), Value::I32(1)]);
    assert_eq!(env.row_count(child), 1);
}

#[test]
fn delete_referenced_parent_is_547() {
    let mut env = Env::new();
    let (parent, child) = parent_child_tables(&env);
    env.insert(
        parent,
        &[
            col_binding(0, "id", int(false)),
            col_binding(1, "code", int(false)),
        ],
        vec![Value::I32(1), Value::I32(10)],
    );
    env.insert(
        child,
        &[
            col_binding(0, "id", int(false)),
            col_binding(1, "pid", int(true)),
        ],
        vec![Value::I32(1), Value::I32(1)],
    );
    let err = env
        .run(&PhysicalStatement::Delete(PhysicalDelete {
            table: parent,
            spool: false,
            input: PhysicalPlan::TableScan {
                table: parent,
                columns: vec![
                    col_binding(0, "id", int(false)),
                    col_binding(1, "code", int(false)),
                ],
                schema: OutputSchema {
                    columns: vec![
                        OutputColumn {
                            name: "id".to_owned(),
                            ty: int(false),
                        },
                        OutputColumn {
                            name: "code".to_owned(),
                            ty: int(false),
                        },
                    ],
                },
                alias: String::new(),
                hints: LockHints::default(),
            },
        }))
        .expect_err("delete parent fails");
    assert_eq!(err.number, 547);
    assert!(err.message.contains("FK_child_parent"));
    assert!(err.message.contains("dbo.fk_check_child"));

    env.run(&PhysicalStatement::Delete(PhysicalDelete {
        table: child,
        spool: false,
        input: PhysicalPlan::TableScan {
            table: child,
            columns: vec![
                col_binding(0, "id", int(false)),
                col_binding(1, "pid", int(true)),
            ],
            schema: OutputSchema {
                columns: vec![
                    OutputColumn {
                        name: "id".to_owned(),
                        ty: int(false),
                    },
                    OutputColumn {
                        name: "pid".to_owned(),
                        ty: int(true),
                    },
                ],
            },
            alias: String::new(),
            hints: LockHints::default(),
        },
    }))
    .expect("delete child succeeds");
    env.run(&PhysicalStatement::Delete(PhysicalDelete {
        table: parent,
        spool: false,
        input: PhysicalPlan::TableScan {
            table: parent,
            columns: vec![
                col_binding(0, "id", int(false)),
                col_binding(1, "code", int(false)),
            ],
            schema: OutputSchema {
                columns: vec![
                    OutputColumn {
                        name: "id".to_owned(),
                        ty: int(false),
                    },
                    OutputColumn {
                        name: "code".to_owned(),
                        ty: int(false),
                    },
                ],
            },
            alias: String::new(),
            hints: LockHints::default(),
        },
    }))
    .expect("delete parent succeeds after child is gone");
}

#[test]
fn null_fk_column_is_not_checked() {
    let mut env = Env::new();
    let (_parent, child) = parent_child_tables(&env);
    env.insert(
        child,
        &[
            col_binding(0, "id", int(false)),
            col_binding(1, "pid", int(true)),
        ],
        vec![Value::I32(1), Value::Null],
    );
    assert_eq!(env.row_count(child), 1);
}

#[test]
fn check_constraint_rejects_false_only() {
    // `CHECK (a > 0)` with `a = NULL` passes on SQL Server 2022 (three-valued logic).
    let mut env = Env::new();
    let table = env.create(
        "fk_check_chk",
        vec![ColumnDef {
            name: "a".to_owned(),
            ty: int(true),
            default: None,
            identity: None,
            computed: None,
        }],
        vec![check_positive("a")],
    );
    let bindings = vec![col_binding(0, "a", int(true))];
    env.insert(table, &bindings, vec![Value::I32(1)]);
    let err = env
        .run(&PhysicalStatement::Insert(PhysicalInsert {
            table,
            columns: bindings.clone(),
            source: values_plan(vec![vec![Value::I32(-1)]], &[int(true)]),
            spool: false,
        }))
        .expect_err("negative value fails");
    assert_eq!(err.number, 547);
    assert!(err.message.contains("CK_positive"));
    env.insert(table, &bindings, vec![Value::Null]);
    assert_eq!(env.row_count(table), 2);
}

#[test]
fn update_rechecks_both_directions() {
    let mut env = Env::new();
    let (parent, child) = parent_child_tables(&env);
    env.insert(
        parent,
        &[
            col_binding(0, "id", int(false)),
            col_binding(1, "code", int(false)),
        ],
        vec![Value::I32(1), Value::I32(10)],
    );
    env.insert(
        child,
        &[
            col_binding(0, "id", int(false)),
            col_binding(1, "pid", int(true)),
        ],
        vec![Value::I32(1), Value::I32(1)],
    );

    let child_update = PhysicalStatement::Update(PhysicalUpdate {
        table: child,
        input: PhysicalPlan::TableScan {
            table: child,
            columns: vec![
                col_binding(0, "id", int(false)),
                col_binding(1, "pid", int(true)),
            ],
            schema: OutputSchema {
                columns: vec![
                    OutputColumn {
                        name: "id".to_owned(),
                        ty: int(false),
                    },
                    OutputColumn {
                        name: "pid".to_owned(),
                        ty: int(true),
                    },
                ],
            },
            alias: String::new(),
            hints: LockHints::default(),
        },
        assignments: vec![(col_binding(1, "pid", int(true)), lit(Value::I32(99)))],
        spool: false,
    });
    let err = env
        .run(&child_update)
        .expect_err("child update to orphan key fails");
    assert_eq!(err.number, 547);
    assert!(err.message.contains("FK_child_parent"));

    let parent_update = PhysicalStatement::Update(PhysicalUpdate {
        table: parent,
        input: PhysicalPlan::TableScan {
            table: parent,
            columns: vec![
                col_binding(0, "id", int(false)),
                col_binding(1, "code", int(false)),
            ],
            schema: OutputSchema {
                columns: vec![
                    OutputColumn {
                        name: "id".to_owned(),
                        ty: int(false),
                    },
                    OutputColumn {
                        name: "code".to_owned(),
                        ty: int(false),
                    },
                ],
            },
            alias: String::new(),
            hints: LockHints::default(),
        },
        assignments: vec![(col_binding(0, "id", int(false)), lit(Value::I32(5)))],
        spool: false,
    });
    let err = env
        .run(&parent_update)
        .expect_err("parent key update fails");
    assert_eq!(err.number, 547);
    assert!(err.message.contains("FK_child_parent"));
    assert!(err.message.contains("dbo.fk_check_child"));
}
