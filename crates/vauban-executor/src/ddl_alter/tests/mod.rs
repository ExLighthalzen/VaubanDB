use super::*;
use std::sync::Arc;
use vauban_binder::{BindContext, SessionOptions, bind};
use vauban_catalog::TableDef;
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{NoIndexes, PhysicalStatement, PlanContext, plan};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo};

/// A bootstrapped catalogue over an in-memory storage.
struct Fixture {
    txn: Arc<TransactionManager>,
    catalog: Catalog,
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
            .expect("the catalogue boots on a fresh storage");
        Self { txn, catalog }
    }
}

/// The single statement of `text`, bound against a catalogue view and planned.
fn bound(fixture: &Fixture, handle: &TxnHandle, text: &str) -> PhysicalStatement {
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    assert_eq!(batch.statements.len(), 1, "one statement per call");
    let snapshot = fixture.catalog.snapshot(handle);
    let mut bind_ctx = BindContext::scalar(text, SessionOptions::default());
    bind_ctx.catalog = Some(&snapshot);
    let bound = bind(&batch.statements[0], &bind_ctx).expect("the statement binds");
    plan(
        bound,
        &PlanContext {
            catalog: &NoIndexes,
        },
    )
    .expect("the statement plans")
}

/// A `TableDef` of one `int` column named `a` in `master.dbo`.
fn table_def(name: &str) -> TableDef {
    TableDef {
        name: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: name.to_owned(),
        },
        columns: vec![vauban_catalog::ColumnDef {
            name: "a".to_owned(),
            ty: TypeInfo::new(SqlType::Int, true),
            default: None,
            identity: None,
            computed: None,
        }],
        constraints: Vec::new(),
    }
}

/// Each variant reaching this file through `execute_ddl` is routed here and answers
/// [`ExecOutcome::NoRows`], rather than an internal 50000 from the stub in `ddl.rs`.
#[test]
fn alter_table_reaches_ddl_alter() {
    let fixture = Fixture::new();
    let eval = StaticContext::default();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .catalog
        .create_table(&handle, &table_def("t"))
        .expect("the table is created");
    let ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_catalog(&fixture.catalog)
        .with_handle(&handle);
    let stmt = bound(&fixture, &handle, "ALTER TABLE dbo.t ADD c int;");
    let PhysicalStatement::Ddl(ddl) = stmt else {
        panic!("expected Ddl");
    };
    assert!(matches!(
        execute_alter_table(&ddl, &ctx).expect("the column is added"),
        ExecOutcome::NoRows
    ));
    fixture
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// A statement that is not an `ALTER TABLE` one is the internal 50000 of this file.
#[test]
fn a_statement_that_is_not_an_alter_table_one_is_a_bug() {
    let fixture = Fixture::new();
    let eval = StaticContext::default();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_catalog(&fixture.catalog)
        .with_handle(&handle);
    let stmt = DdlStatement::CreateTable {
        def: table_def("t"),
    };
    assert_eq!(
        execute_alter_table(&stmt, &ctx)
            .expect_err("not ALTER TABLE")
            .number,
        50000
    );
    fixture
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}
