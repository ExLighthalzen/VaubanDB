//! Integration tests for `FOREIGN KEY` and `CHECK` at write time.

use std::sync::Arc;

use vauban_binder::{
    BindContext, BoundExpr, BoundExprKind, BoundStatement, ColumnBinding, LockHints, OutputColumn,
    OutputSchema, SessionOptions, TargetExpressions, bind,
};
use vauban_catalog::{Catalog, ColumnDef, ConstraintDef, QualifiedName, SortedColumn, TableDef};
use vauban_errors::SqlError;
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, execute_collect};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{
    NoIndexes, PhysicalDelete, PhysicalInsert, PhysicalPlan, PhysicalStatement, PhysicalUpdate,
    PlanContext, plan,
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
        depth: 0,
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
        on_delete: vauban_catalog::RefAction::NoAction,
        on_update: vauban_catalog::RefAction::NoAction,
    }
}

fn check_positive(column: &str) -> ConstraintDef {
    ConstraintDef::Check {
        name: Some("CK_positive".to_owned()),
        expr: format!("([{column}]>(0))"),
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
            target: self.checks_of(table),
        });
        self.run(&stmt).expect("insert succeeds");
    }

    /// The `CHECK` predicates the binder puts in the plan of an `INSERT` into `table`.
    fn checks_of(&self, table: TableId) -> TargetExpressions {
        let snap = self.catalog.snapshot(&self.handle);
        let meta = snap.table_by_storage(table).expect("the table is known");
        let text = format!("INSERT INTO dbo.{} DEFAULT VALUES;", meta.name);
        let batch = parse_batch(&text, &ParseOptions::default()).expect("the text parses");
        let mut bind_ctx = BindContext::scalar(&text, SessionOptions::default());
        bind_ctx.catalog = Some(&snap);
        match bind(&batch.statements[0], &bind_ctx).expect("the INSERT binds") {
            BoundStatement::Insert(insert) => TargetExpressions {
                defaults: Vec::new(),
                ..insert.target
            },
            _ => panic!("not an INSERT"),
        }
    }

    /// Parses, binds, plans and runs one statement of `text`, and answers how many
    /// catalogue snapshots its execution built, the binding left out.
    fn run_sql(&mut self, text: &str) -> Result<(ExecOutcome, u64), SqlError> {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let physical = {
            let snap = self.catalog.snapshot(&self.handle);
            let mut bind_ctx = BindContext::scalar(text, SessionOptions::default());
            bind_ctx.catalog = Some(&snap);
            let bound = bind(&batch.statements[0], &bind_ctx)?;
            plan(
                bound,
                &PlanContext {
                    catalog: &NoIndexes,
                },
            )?
        };
        let before = Catalog::snapshots_built_on_this_thread();
        let outcome = self.run(&physical)?;
        Ok((outcome, Catalog::snapshots_built_on_this_thread() - before))
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
            target: Default::default(),
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
            target: env.checks_of(table),
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
        target: Default::default(),
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
        target: Default::default(),
    });
    let err = env
        .run(&parent_update)
        .expect_err("parent key update fails");
    assert_eq!(err.number, 547);
    assert!(err.message.contains("FK_child_parent"));
    assert!(err.message.contains("dbo.fk_check_child"));
}

/// An `INSERT … SELECT` of 2 000 rows into a table with a `FOREIGN KEY` and a `CHECK` reads
/// the catalogue once: the constraints are resolved before the first row, not per row.
#[test]
fn an_insert_select_builds_one_catalogue_snapshot() {
    let mut env = Env::new();
    for text in [
        "CREATE TABLE dbo.snap_parent (id int NOT NULL PRIMARY KEY);",
        "CREATE TABLE dbo.snap_source (id int NOT NULL, pid int NOT NULL, v int NOT NULL);",
        "CREATE TABLE dbo.snap_child (id int NOT NULL PRIMARY KEY, \
         pid int NULL CONSTRAINT fk_snap REFERENCES dbo.snap_parent (id), \
         v int NULL CONSTRAINT ck_snap CHECK (v >= 0));",
        "INSERT INTO dbo.snap_parent VALUES (1), (2), (3);",
    ] {
        env.run_sql(text).expect("the setup runs");
    }
    let source = {
        let snap = env.catalog.snapshot(&env.handle);
        let meta = snap
            .resolve_object("master", Some("dbo"), "snap_source", "dbo")
            .expect("the source resolves");
        snap.table(meta.id)
            .expect("the source is a table")
            .storage_id
    };
    for id in 0..2_000 {
        env.storage
            .insert(
                env.handle.id,
                source,
                &vauban_storage::Row(vec![
                    Value::I32(id),
                    Value::I32(1 + id % 3),
                    Value::I32(id % 10),
                ]),
            )
            .expect("a source row");
    }

    let (_, snapshots) = env
        .run_sql("INSERT INTO dbo.snap_child (id, pid, v) SELECT id, pid, v FROM dbo.snap_source;")
        .expect("every row satisfies both constraints");
    assert_eq!(env.session.rowcount, 2_000);
    assert_eq!(
        snapshots, 1,
        "one catalogue snapshot for the whole statement"
    );

    // The constraints are still checked on each row: the last row breaks the CHECK, the
    // one before it the FOREIGN KEY.
    let err = env
        .run_sql("INSERT INTO dbo.snap_child (id, pid, v) SELECT id + 5000, pid, v - 9 FROM dbo.snap_source;")
        .expect_err("v - 9 is negative for most rows");
    assert_eq!(err.number, 547);
    assert!(err.message.contains("ck_snap"), "{}", err.message);
    let err = env
        .run_sql("INSERT INTO dbo.snap_child (id, pid, v) SELECT id + 9000, pid + 3, v FROM dbo.snap_source;")
        .expect_err("no parent has an id above 3");
    assert_eq!(err.number, 547);
    assert!(err.message.contains("fk_snap"), "{}", err.message);
}

/// A `CHECK` the plan carries no predicate for is not skipped: the row is refused with the
/// internal error, which names the constraint.
#[test]
fn a_check_missing_from_the_plan_refuses_the_row() {
    let mut env = Env::new();
    let table = env.create(
        "fk_check_unbound",
        vec![ColumnDef {
            name: "a".to_owned(),
            ty: int(true),
            default: None,
            identity: None,
            computed: None,
        }],
        vec![check_positive("a")],
    );
    let err = env
        .run(&PhysicalStatement::Insert(PhysicalInsert {
            table,
            columns: vec![col_binding(0, "a", int(true))],
            source: values_plan(vec![vec![Value::I32(1)]], &[int(true)]),
            spool: false,
            target: TargetExpressions::default(),
        }))
        .expect_err("the CHECK has no predicate in the plan");
    assert_eq!(err.number, 50_000);
    assert!(err.message.contains("CK_positive"), "{}", err.message);
    assert_eq!(env.row_count(table), 0);
}
