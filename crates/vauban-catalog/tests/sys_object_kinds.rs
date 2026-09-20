//! What `views/sys_object_kinds.rs` publishes: `sys.triggers`, `sys.synonyms`,
//! `sys.sequences` and `sys.table_types` resolve in `master`, their four internal tables are
//! part of the bootstrap, and each view answers zero row, under a two-part name in a database
//! a client creates.

use std::sync::Arc;

use vauban_catalog::Catalog;
use vauban_storage::{MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::Value;

/// A bootstrapped catalogue over a fresh `MemoryStorage`, with its storage and transaction
/// manager.
fn instance() -> (Catalog, Arc<dyn Storage>, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    (catalog, storage, txn)
}

/// Opens a transaction of `txn`.
fn begin(txn: &Arc<TransactionManager>) -> TxnHandle {
    txn.begin(IsolationLevel::ReadCommitted)
}

/// The rows of the storage table `table`, read in a transaction of its own.
fn rows(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    table: TableId,
) -> Vec<Vec<Value>> {
    let handle = begin(txn);
    let snapshot = txn.statement_snapshot(&handle);
    let read: Vec<Vec<Value>> = storage
        .scan(&snapshot, table)
        .expect("scan")
        .map(|row| row.expect("row").1.0)
        .collect();
    txn.commit(handle).expect("commit of the reading txn");
    read
}

/// The internal tables of this file, `master.dbo.<name>`.
const INTERNAL_TABLES: [&str; 4] = [
    "vauban_sys_triggers",
    "vauban_sys_synonyms",
    "vauban_sys_sequences",
    "vauban_sys_table_types",
];

/// The views of this file, `sys.<name>`.
const VIEWS: [&str; 4] = ["triggers", "synonyms", "sequences", "table_types"];

#[test]
fn the_bootstrap_creates_the_four_internal_tables_of_this_file() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let snapshot = catalog.snapshot(&handle);
    for name in INTERNAL_TABLES {
        let object = snapshot
            .resolve_object("master", Some("dbo"), name, "dbo")
            .unwrap_or_else(|| panic!("master.dbo.{name} resolves"));
        let meta = snapshot
            .table(object.id)
            .unwrap_or_else(|| panic!("{name} is a table of the snapshot"));
        assert!(!meta.columns.is_empty(), "{name} carries its columns");
        assert_eq!(rows(&storage, &txn, meta.storage_id).len(), 0, "{name}");
    }
}

#[test]
fn the_four_views_answer_zero_row_in_a_new_database() {
    let (catalog, storage, txn) = instance();
    let creating = begin(&txn);
    catalog
        .create_database(&creating, "d", None)
        .expect("create_database");
    txn.commit(creating).expect("commit of the create");
    let handle = begin(&txn);
    let snapshot = catalog.snapshot(&handle);
    for view in VIEWS {
        // Two-part name: `sys.<view>`, resolved from within database `d`.
        let object = snapshot
            .resolve_object("d", Some("sys"), view, "dbo")
            .unwrap_or_else(|| panic!("sys.{view} resolves from d"));
        assert!(
            snapshot.table(object.id).is_none(),
            "sys.{view} is a view, not a table"
        );
        let definition = snapshot
            .view_definition(object.id)
            .unwrap_or_else(|| panic!("sys.{view} carries a definition"));
        // The internal table the view reads carries no row, whichever database applies the
        // `database_id` filter of the definition text above.
        let table_name = format!("vauban_sys_{view}");
        let table_object = snapshot
            .resolve_object("master", Some("dbo"), &table_name, "dbo")
            .unwrap_or_else(|| panic!("master.dbo.{table_name} resolves"));
        let table_meta = snapshot
            .table(table_object.id)
            .unwrap_or_else(|| panic!("{table_name} is a table of the snapshot"));
        assert_eq!(
            rows(&storage, &txn, table_meta.storage_id).len(),
            0,
            "sys.{view}: {definition}"
        );
    }
}
