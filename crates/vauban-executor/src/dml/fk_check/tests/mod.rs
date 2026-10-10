//! What a statement checks, read under the snapshot of its transaction.

use std::sync::Arc;

use vauban_binder::SessionOptions;
use vauban_catalog::{
    AlterTable, Catalog, ColumnDef, ConstraintDef, QualifiedName, SortedColumn, TableDef,
};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{SqlType, TypeInfo};

use super::*;

fn name(table: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "dbo".to_owned(),
        name: table.to_owned(),
    }
}

fn column(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, false),
        default: None,
        identity: None,
        computed: None,
    }
}

/// A bootstrapped catalogue holding `dbo.p (a PRIMARY KEY)` and `dbo.c (a)`, committed.
struct Fixture {
    txn: Arc<TransactionManager>,
    catalog: Catalog,
    parent: ObjectId,
    child: ObjectId,
}

impl Fixture {
    fn new() -> Self {
        vauban_sysfn::register_builtins();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
            .expect("the catalogue boots on a fresh storage");
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let parent = catalog
            .create_table(
                &handle,
                &TableDef {
                    name: name("p"),
                    columns: vec![column("a")],
                    constraints: vec![ConstraintDef::PrimaryKey {
                        name: Some("pk_p".to_owned()),
                        columns: vec![SortedColumn {
                            column: "a".to_owned(),
                            descending: false,
                        }],
                        clustered: true,
                    }],
                },
            )
            .expect("p is created")
            .id;
        let child = catalog
            .create_table(
                &handle,
                &TableDef {
                    name: name("c"),
                    columns: vec![column("a")],
                    constraints: Vec::new(),
                },
            )
            .expect("c is created")
            .id;
        txn.commit(handle).expect("the tables commit");
        Self {
            txn,
            catalog,
            parent,
            child,
        }
    }

    /// What a `DELETE` of `table` in the transaction `handle` checks.
    fn checks(&self, handle: &TxnHandle, table: ObjectId) -> WriteChecks {
        let snap = self.catalog.snapshot(handle);
        let meta = snap.table(table).expect("the table is in the snapshot");
        let eval = StaticContext::default();
        let ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_catalog(&self.catalog);
        WriteChecks::resolve(meta, &snap, &[], "DELETE", true, &ctx)
            .expect("the constraints resolve")
    }
}

fn rule_names(checks: &WriteChecks) -> Vec<&str> {
    checks
        .rules
        .iter()
        .map(|rule| match rule {
            Rule::Check { name, .. } | Rule::ForeignKey { name, .. } => name.as_str(),
        })
        .collect()
}

fn incoming_names(checks: &WriteChecks) -> Vec<&str> {
    checks
        .incoming
        .iter()
        .map(|reference| reference.name.as_str())
        .collect()
}

/// A `FOREIGN KEY` of `c` towards `p` transaction A has added and not committed: the checks
/// of a statement of transaction B leave it out on both tables, those of A hold it on both,
/// by its name.
#[test]
fn a_foreign_key_another_transaction_has_not_committed_is_not_checked() {
    let fixture = Fixture::new();
    let a = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .catalog
        .alter_table(
            &a,
            fixture.child,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::ForeignKey {
                    name: Some("fk_c_p".to_owned()),
                    columns: vec!["a".to_owned()],
                    referenced: name("p"),
                    referenced_columns: vec!["a".to_owned()],
                    on_delete: vauban_parser::RefAction::NoAction,
                    on_update: vauban_parser::RefAction::NoAction,
                }),
            },
        )
        .expect("A adds the foreign key");
    let b = fixture.txn.begin(IsolationLevel::ReadCommitted);

    assert!(incoming_names(&fixture.checks(&b, fixture.parent)).is_empty());
    assert!(rule_names(&fixture.checks(&b, fixture.child)).is_empty());
    assert_eq!(
        incoming_names(&fixture.checks(&a, fixture.parent)),
        ["fk_c_p"]
    );
    assert_eq!(rule_names(&fixture.checks(&a, fixture.child)), ["fk_c_p"]);

    fixture.txn.rollback(b).expect("B rolls back");
    fixture.txn.rollback(a).expect("A rolls back");
}
