//! Integration tests for `UPDATE` assignment checks: 2628 on truncation and 515 on `NULL`.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, LockHints, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_catalog::{Catalog, ColumnDef, QualifiedName, TableDef};
use vauban_executor::{ExecContext, ExecSession, execute_collect};
use vauban_planner::{PhysicalPlan, PhysicalStatement, PhysicalUpdate};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

fn varchar(len: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(len)), nullable)
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

fn tbl_name(name: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "dbo".to_owned(),
        name: name.to_owned(),
    }
}

fn lit(value: Value, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 0,
    }
}

fn scan(table: vauban_storage::TableId, columns: Vec<ColumnBinding>) -> PhysicalPlan {
    PhysicalPlan::TableScan {
        table,
        columns: columns.clone(),
        schema: OutputSchema {
            columns: columns
                .iter()
                .map(|c| out_col(&c.name, c.ty.clone()))
                .collect(),
        },
        alias: "t".to_owned(),
        hints: LockHints::default(),
    }
}

struct Fixture {
    storage: Arc<MemoryStorage>,
    txn_mgr: Arc<TransactionManager>,
    catalog: Catalog,
    handle: vauban_txn::TxnHandle,
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

    fn create(&self, name: &str, columns: Vec<ColumnDef>) -> vauban_storage::TableId {
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

    fn seed(&self, table: vauban_storage::TableId, rows: &[Vec<Value>]) {
        for row in rows {
            self.storage
                .insert(self.handle.id, table, &vauban_storage::Row(row.clone()))
                .expect("seed row");
        }
    }

    fn run(&mut self, stmt: &PhysicalStatement) -> vauban_errors::SqlResult<()> {
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
        execute_collect(stmt, &mut ctx).map(|_| ())
    }

    fn read(
        &mut self,
        table: vauban_storage::TableId,
        bindings: &[ColumnBinding],
    ) -> Vec<Vec<Value>> {
        let stmt = PhysicalStatement::Query(scan(table, bindings.to_vec()));
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
        execute_collect(&stmt, &mut ctx)
            .expect("scan succeeds")
            .1
            .rows
    }
}

fn update_stmt(
    table: vauban_storage::TableId,
    scan_cols: Vec<ColumnBinding>,
    assignments: Vec<(ColumnBinding, BoundExpr)>,
) -> PhysicalStatement {
    PhysicalStatement::Update(PhysicalUpdate {
        table,
        input: scan(table, scan_cols),
        assignments,
        spool: false,
    })
}

#[test]
fn update_string_too_long_for_a_varchar_column_is_2628() {
    let mut f = Fixture::new();
    let table = f.create(
        "update_trunc_varchar",
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
                ty: varchar(10, false),
                default: None,
                identity: None,
                computed: None,
            },
        ],
    );
    f.seed(
        table,
        &[vec![
            Value::I32(1),
            Value::String(SqlString {
                text: "C1".to_owned(),
            }),
        ]],
    );
    let id = col_binding(0, "id", int(false));
    let code = col_binding(1, "code", varchar(10, false));
    let long = Value::String(SqlString {
        text: "ABCDEFGHIJK".to_owned(),
    });

    let err = f
        .run(&update_stmt(
            table,
            vec![id.clone(), code.clone()],
            vec![(code.clone(), lit(long, varchar(20, false)))],
        ))
        .expect_err("eleven characters do not fit varchar(10)");
    assert_eq!(err.number, 2628);
    assert_eq!(err.state, 1);
    assert!(
        err.message.contains("'code'") && err.message.contains("'ABCDEFGHIJ'"),
        "{}",
        err.message
    );
    assert_eq!(
        f.read(table, &[id, code]),
        vec![vec![
            Value::I32(1),
            Value::String(SqlString {
                text: "C1".to_owned(),
            }),
        ]],
        "the row before the failed update is intact"
    );
}

#[test]
fn update_a_not_null_column_to_null_is_515() {
    let mut f = Fixture::new();
    let table = f.create(
        "update_null_not_null",
        vec![
            ColumnDef {
                name: "id".to_owned(),
                ty: int(false),
                default: None,
                identity: None,
                computed: None,
            },
            ColumnDef {
                name: "nom".to_owned(),
                ty: varchar(10, false),
                default: None,
                identity: None,
                computed: None,
            },
        ],
    );
    f.seed(
        table,
        &[vec![
            Value::I32(1),
            Value::String(SqlString {
                text: "x".to_owned(),
            }),
        ]],
    );
    let id = col_binding(0, "id", int(false));
    let nom = col_binding(1, "nom", varchar(10, false));

    let err = f
        .run(&update_stmt(
            table,
            vec![id.clone(), nom.clone()],
            vec![(nom.clone(), lit(Value::Null, varchar(10, true)))],
        ))
        .expect_err("NULL is refused on a NOT NULL column");
    assert_eq!(err.number, 515);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 2);
    assert!(
        err.message.contains("'nom'")
            && err.message.contains("master.dbo.update_null_not_null")
            && err.message.contains("UPDATE"),
        "{}",
        err.message
    );
    assert_eq!(
        f.read(table, &[id, nom]),
        vec![vec![
            Value::I32(1),
            Value::String(SqlString {
                text: "x".to_owned(),
            }),
        ]],
        "the row before the failed update is intact"
    );
}
