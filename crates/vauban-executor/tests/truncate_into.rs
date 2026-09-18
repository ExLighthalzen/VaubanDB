//! Integration tests for `TRUNCATE TABLE` and `SELECT … INTO`.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, DdlStatement, LockHints, OutputColumn, OutputSchema,
    SessionOptions,
};
use vauban_catalog::{Catalog, ColumnDef, IdentitySpec, QualifiedName, TableDef};
use vauban_errors::SqlResult;
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, RowSet, execute_collect};
use vauban_planner::{PhysicalInsert, PhysicalPlan, PhysicalSelectInto, PhysicalStatement};
use vauban_storage::{MemoryStorage, Storage, TableId};
use vauban_sysfn::StaticContext;
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

fn out_col(name: &str, ty: TypeInfo) -> OutputColumn {
    OutputColumn {
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

fn truncate_stmt(name: &str) -> PhysicalStatement {
    PhysicalStatement::Ddl(DdlStatement::TruncateTable {
        name: tbl_name(name),
    })
}

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
        let snap = self.txn_mgr.statement_snapshot(&self.handle);
        let mut ctx = ExecContext::scalar(&self.eval, SessionOptions::default())
            .with_engine(
                self.storage.as_ref() as &dyn vauban_storage::Storage,
                self.txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&self.catalog)
            .with_handle(&self.handle)
            .with_session(&mut self.session);
        execute_collect(stmt, &mut ctx)
    }

    fn scan(&mut self, table: TableId, bindings: &[ColumnBinding]) -> Vec<Vec<Value>> {
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
        self.run(&stmt).expect("scan succeeds").1.rows
    }
}

fn insert_rows(
    fixture: &mut Fixture,
    table: TableId,
    bindings: &[ColumnBinding],
    rows: Vec<Vec<Value>>,
) {
    let types: Vec<TypeInfo> = bindings.iter().map(|b| b.ty.clone()).collect();
    let stmt = PhysicalStatement::Insert(PhysicalInsert {
        table,
        columns: bindings.to_vec(),
        source: values_plan(rows, &types),
        spool: false,
    });
    fixture.run(&stmt).expect("insert succeeds");
}

#[test]
fn truncate_empties_the_table() {
    let mut fixture = Fixture::new();
    let table = fixture.create(
        "truncate_empty",
        vec![ColumnDef {
            name: "a".to_owned(),
            ty: int(false),
            default: None,
            identity: None,
            computed: None,
        }],
    );
    insert_rows(
        &mut fixture,
        table,
        &[col_binding(0, "a", int(false))],
        vec![
            vec![Value::I32(1)],
            vec![Value::I32(2)],
            vec![Value::I32(3)],
        ],
    );
    fixture
        .run(&truncate_stmt("truncate_empty"))
        .expect("truncate succeeds");
    let rows = fixture.scan(table, &[col_binding(0, "a", int(false))]);
    assert!(rows.is_empty());
}

#[test]
fn truncate_reports_no_row_count() {
    let mut fixture = Fixture::new();
    let table = fixture.create(
        "truncate_rowcount",
        vec![ColumnDef {
            name: "a".to_owned(),
            ty: int(false),
            default: None,
            identity: None,
            computed: None,
        }],
    );
    insert_rows(
        &mut fixture,
        table,
        &[col_binding(0, "a", int(false))],
        vec![vec![Value::I32(1)]],
    );
    let (outcome, _) = fixture
        .run(&truncate_stmt("truncate_rowcount"))
        .expect("truncate succeeds");
    assert!(matches!(outcome, ExecOutcome::NoRows));
    // TRUNCATE leaves @@ROWCOUNT at 0 where DELETE reports the rows removed.
    assert_eq!(fixture.session.rowcount, 0);
}

fn next_identity_after_emptying(truncate: bool) -> Value {
    let mut fixture = Fixture::new();
    let table = fixture.create(
        if truncate {
            "truncate_identity"
        } else {
            "delete_identity"
        },
        vec![
            ColumnDef {
                name: "id".to_owned(),
                ty: int(false),
                default: None,
                identity: Some(IdentitySpec {
                    seed: 1,
                    increment: 1,
                }),
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
    );
    let bindings = [col_binding(1, "code", int(false))];
    for code in [10, 20, 30] {
        insert_rows(&mut fixture, table, &bindings, vec![vec![Value::I32(code)]]);
    }
    fixture
        .txn_mgr
        .commit(fixture.handle)
        .expect("seed rows committed");
    fixture.handle = fixture.txn_mgr.begin(IsolationLevel::ReadCommitted);
    fixture
        .run(&if truncate {
            truncate_stmt("truncate_identity")
        } else {
            PhysicalStatement::Delete(vauban_planner::PhysicalDelete {
                table,
                input: PhysicalPlan::TableScan {
                    table,
                    columns: bindings.to_vec(),
                    schema: OutputSchema {
                        columns: vec![out_col("code", int(false))],
                    },
                    alias: "t".to_owned(),
                    hints: LockHints::default(),
                },
                spool: false,
            })
        })
        .expect("emptying succeeds");
    insert_rows(&mut fixture, table, &bindings, vec![vec![Value::I32(99)]]);
    fixture.scan(table, &[col_binding(0, "id", int(false))])[0][0].clone()
}

#[test]
fn truncate_resets_identity_to_the_seed() {
    assert_eq!(
        next_identity_after_emptying(true),
        Value::I32(1),
        "TRUNCATE resets the counter to the seed"
    );
}

#[test]
fn delete_leaves_identity_where_it_was() {
    assert_eq!(
        next_identity_after_emptying(false),
        Value::I32(4),
        "DELETE leaves the counter where it was"
    );
}

#[test]
fn truncate_rollback_restores_identity_counter() {
    let mut fixture = Fixture::new();
    let table = fixture.create(
        "truncate_identity_rollback",
        vec![
            ColumnDef {
                name: "id".to_owned(),
                ty: int(false),
                default: None,
                identity: Some(IdentitySpec {
                    seed: 1,
                    increment: 1,
                }),
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
    );
    let bindings = [col_binding(1, "code", int(false))];
    for code in [10, 20, 30] {
        insert_rows(&mut fixture, table, &bindings, vec![vec![Value::I32(code)]]);
    }
    fixture
        .txn_mgr
        .commit(fixture.handle)
        .expect("seed rows committed");
    let trunc_handle = fixture.txn_mgr.begin(IsolationLevel::ReadCommitted);
    {
        let snap = fixture.txn_mgr.statement_snapshot(&trunc_handle);
        let mut ctx = ExecContext::scalar(&fixture.eval, SessionOptions::default())
            .with_engine(
                fixture.storage.as_ref() as &dyn vauban_storage::Storage,
                fixture.txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&fixture.catalog)
            .with_handle(&trunc_handle)
            .with_session(&mut fixture.session);
        execute_collect(&truncate_stmt("truncate_identity_rollback"), &mut ctx).expect("truncate");
    }
    fixture
        .txn_mgr
        .rollback(trunc_handle)
        .expect("rollback truncate");
    fixture.handle = fixture.txn_mgr.begin(IsolationLevel::ReadCommitted);
    insert_rows(&mut fixture, table, &bindings, vec![vec![Value::I32(99)]]);
    let rows = fixture.scan(
        table,
        &[
            col_binding(0, "id", int(false)),
            col_binding(1, "code", int(false)),
        ],
    );
    let inserted = rows
        .iter()
        .find(|row| row[1] == Value::I32(99))
        .expect("row inserted after rollback");
    assert_eq!(
        inserted[0],
        Value::I32(4),
        "rollback of TRUNCATE restores the identity counter"
    );
}

#[test]
fn truncate_rolls_back() {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let table = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("truncate_rollback"),
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
        .expect("create")
        .storage_id;
    for v in [1, 2, 3] {
        Storage::insert(
            storage.as_ref(),
            handle.id,
            table,
            &vauban_storage::Row(vec![Value::I32(v)]),
        )
        .expect("insert");
    }
    txn_mgr.commit(handle).expect("commit setup");
    let trunc_handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    {
        let snap = txn_mgr.statement_snapshot(&trunc_handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(
                storage.as_ref() as &dyn vauban_storage::Storage,
                txn_mgr.as_ref(),
                &snap,
            )
            .with_catalog(&catalog)
            .with_handle(&trunc_handle)
            .with_session(&mut session);
        execute_collect(&truncate_stmt("truncate_rollback"), &mut ctx).expect("truncate");
    }
    txn_mgr.rollback(trunc_handle).expect("rollback");
    let read_handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    let snap = txn_mgr.statement_snapshot(&read_handle);
    let count = Storage::scan(storage.as_ref(), &snap, table)
        .expect("scan")
        .count();
    assert_eq!(count, 3);
}

#[test]
fn select_into_creates_and_fills() {
    let mut fixture = Fixture::new();
    let ty = int(false);
    let source_types = [ty.clone(), ty.clone()];
    let source = values_plan(
        vec![
            vec![Value::I32(1), Value::I32(10)],
            vec![Value::I32(2), Value::I32(20)],
        ],
        &source_types,
    );
    let def = TableDef {
        name: tbl_name("into_filled"),
        columns: vec![
            ColumnDef {
                name: "a".to_owned(),
                ty: ty.clone(),
                default: None,
                identity: None,
                computed: None,
            },
            ColumnDef {
                name: "b".to_owned(),
                ty,
                default: None,
                identity: None,
                computed: None,
            },
        ],
        constraints: Vec::new(),
    };
    let stmt = PhysicalStatement::SelectInto(PhysicalSelectInto {
        def: def.clone(),
        source,
        spool: false,
    });
    fixture.run(&stmt).expect("select into succeeds");
    let snap = fixture.catalog.snapshot(&fixture.handle);
    let meta = snap
        .resolve_object("master", Some("dbo"), "into_filled", "dbo")
        .expect("table exists");
    let table = snap.table(meta.id).expect("is a table").storage_id;
    let rows = fixture.scan(
        table,
        &[
            col_binding(0, "a", int(false)),
            col_binding(1, "b", int(false)),
        ],
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::I32(1), Value::I32(10)]);
    assert_eq!(def.columns[0].ty, source_types[0]);
    assert_eq!(def.columns[1].ty, source_types[1]);
}

#[test]
fn select_into_of_an_empty_source_creates_the_table() {
    let mut fixture = Fixture::new();
    let ty = int(true);
    let def = TableDef {
        name: tbl_name("into_empty"),
        columns: vec![ColumnDef {
            name: "a".to_owned(),
            ty: ty.clone(),
            default: None,
            identity: None,
            computed: None,
        }],
        constraints: Vec::new(),
    };
    let source = values_plan(vec![], &[ty]);
    let stmt = PhysicalStatement::SelectInto(PhysicalSelectInto {
        def,
        source,
        spool: false,
    });
    fixture.run(&stmt).expect("select into succeeds");
    let snap = fixture.catalog.snapshot(&fixture.handle);
    let meta = snap
        .resolve_object("master", Some("dbo"), "into_empty", "dbo")
        .expect("table exists");
    let table = snap.table(meta.id).expect("is a table").storage_id;
    let rows = fixture.scan(table, &[col_binding(0, "a", int(true))]);
    assert!(rows.is_empty());
}
