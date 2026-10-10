//! Planning a statement bound at run time under the snapshot of its transaction.

use std::sync::Arc;

use vauban_binder::{DeferredStatement, SessionOptions};
use vauban_catalog::{Catalog, ColumnDef, IndexDef, QualifiedName, SortedColumn, TableDef};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{SqlType, TypeInfo};

use super::*;
use crate::context::ExecSession;

/// A bootstrapped catalogue over an in-memory storage.
struct Fixture {
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
    catalog: Catalog,
}

impl Fixture {
    fn new() -> Self {
        vauban_sysfn::register_builtins();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
            .expect("the catalogue boots on a fresh storage");
        Self {
            storage,
            txn,
            catalog,
        }
    }

    /// Creates and commits `dbo.t (a int, b int)`, and answers its identifier.
    fn table(&self) -> vauban_catalog::ObjectId {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let column = |name: &str| ColumnDef {
            name: name.to_owned(),
            ty: TypeInfo::new(SqlType::Int, true),
            default: None,
            identity: None,
            computed: None,
        };
        let meta = self
            .catalog
            .create_table(
                &handle,
                &TableDef {
                    name: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: "t".to_owned(),
                    },
                    columns: vec![column("a"), column("b")],
                    constraints: Vec::new(),
                },
            )
            .expect("the table is created");
        self.txn.commit(handle).expect("the table commits");
        meta.id
    }

    /// Binds and plans `text` as a statement left unbound, in the transaction `handle`.
    fn plan_deferred(&self, handle: &TxnHandle, text: &str) -> PhysicalStatement {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let deferred = DeferredStatement {
            text: Arc::from(text),
            statement: Box::new(batch.statements[0].clone()),
            database: "master".to_owned(),
            default_schema: "dbo".to_owned(),
            line: 1,
        };
        let snap = self.txn.statement_snapshot(handle);
        let eval = StaticContext::default();
        let mut session = ExecSession::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), &self.txn, &snap)
            .with_catalog(&self.catalog)
            .with_handle(handle)
            .with_session(&mut session);
        bind_and_plan(&deferred, &mut ctx).expect("the statement binds and plans")
    }
}

/// A `CREATE INDEX` transaction A has not committed: a statement of transaction B bound at
/// run time plans a scan, while the same statement of A seeks through the index, which
/// `storage` holds for both.
#[test]
fn a_deferred_statement_does_not_seek_an_index_another_transaction_has_not_committed() {
    let fixture = Fixture::new();
    let table = fixture.table();
    let a = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .catalog
        .create_index(
            &a,
            &IndexDef {
                table,
                name: "ix".to_owned(),
                columns: vec![SortedColumn {
                    column: "a".to_owned(),
                    descending: false,
                }],
                unique: false,
                clustered: false,
            },
        )
        .expect("the index is created");
    let b = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let text = "SELECT b FROM dbo.t WHERE a = 1;";

    let seen_by_b = format!("{:?}", fixture.plan_deferred(&b, text));
    assert!(!seen_by_b.contains("IndexSeek"), "{seen_by_b}");
    let seen_by_a = format!("{:?}", fixture.plan_deferred(&a, text));
    assert!(seen_by_a.contains("IndexSeek"), "{seen_by_a}");

    fixture.txn.rollback(b).expect("B rolls back");
    fixture.txn.rollback(a).expect("A rolls back");
}
