//! `Catalog::bootstrap` seen from outside the crate: what a client-visible start leaves in
//! `storage`.
//!
//! These tests know the public API only. The names of the internal tables are `pub(crate)`,
//! so a test that must reach one finds it by its content (`schemas_table` below) rather than
//! by its name; the tests that need the names are the unit tests of `src/bootstrap.rs`.

use std::collections::BTreeSet;
use std::sync::Arc;

use vauban_catalog::Catalog;
use vauban_storage::{DbId, MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::Value;

/// The four system databases, lowercased, as the tests compare them.
const SYSTEM_DATABASES: [&str; 4] = ["master", "tempdb", "model", "msdb"];

/// The schemas each system database carries.
const SYSTEM_SCHEMAS: [&str; 3] = ["dbo", "sys", "INFORMATION_SCHEMA"];

/// A fresh storage and the transaction manager on top of it.
fn instance() -> (Arc<dyn Storage>, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    (storage, txn)
}

/// The databases of `storage` as `(DbId, lowercased name)`, sorted by `DbId`.
fn databases(storage: &Arc<dyn Storage>) -> Vec<(DbId, String)> {
    storage
        .databases()
        .expect("databases()")
        .into_iter()
        .map(|(id, name)| (id, name.to_lowercase()))
        .collect()
}

/// The rows of `table`, read through a transaction of its own.
fn rows(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    table: TableId,
) -> Vec<Vec<Value>> {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = txn.statement_snapshot(&handle);
    let rows: Vec<Vec<Value>> = storage
        .scan(&snapshot, table)
        .expect("scan")
        .map(|row| row.expect("row").1.0)
        .collect();
    txn.commit(handle).expect("commit of the reading txn");
    rows
}

/// The internal table of the schemas: the table of `master` that holds a row with the text
/// `dbo`.
///
/// Read through `Storage::scan` rather than through a `CatalogSnapshot`, so that the test
/// depends on the storage alone.
fn schemas_table(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    master: DbId,
) -> Vec<Vec<Value>> {
    let mut found = Vec::new();
    for (table, _) in storage.tables(master).expect("tables(master)") {
        let content = rows(storage, txn, table);
        if content.iter().any(|row| row.iter().any(is_dbo)) {
            found.push(content);
        }
    }
    assert_eq!(found.len(), 1, "one table of master holds the text `dbo`");
    found.pop().unwrap_or_default()
}

/// Whether a value is the text `dbo`.
fn is_dbo(value: &Value) -> bool {
    matches!(value, Value::String(s) if s.text == "dbo")
}

/// The texts a row carries.
fn texts(row: &[Value]) -> Vec<String> {
    row.iter()
        .filter_map(|value| match value {
            Value::String(s) => Some(s.text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn bootstrap_creates_four_system_databases() {
    let (storage, txn) = instance();
    Catalog::bootstrap(Arc::clone(&storage), txn).expect("bootstrap");
    let names: Vec<String> = databases(&storage)
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert_eq!(names, SYSTEM_DATABASES);
    // The identifiers a fresh instance hands out, in the order of `sys.databases`: master 1,
    // tempdb 2, model 3, msdb 4.
    let ids: Vec<u32> = databases(&storage)
        .into_iter()
        .map(|(id, _)| id.0)
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 4]);
}

#[test]
fn bootstrap_is_idempotent() {
    let (storage, txn) = instance();
    Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("first bootstrap");
    let first_databases = databases(&storage);
    let first_tables: Vec<TableId> = storage
        .tables(first_databases[0].0)
        .expect("tables(master)")
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let first_rows: Vec<Vec<Vec<Value>>> = first_tables
        .iter()
        .map(|&table| rows(&storage, &txn, table))
        .collect();

    Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("second bootstrap");

    assert_eq!(databases(&storage), first_databases);
    let second_tables: Vec<TableId> = storage
        .tables(first_databases[0].0)
        .expect("tables(master)")
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(second_tables, first_tables);
    let second_rows: Vec<Vec<Vec<Value>>> = first_tables
        .iter()
        .map(|&table| rows(&storage, &txn, table))
        .collect();
    assert_eq!(second_rows, first_rows);
}

#[test]
fn bootstrap_creates_three_schemas_in_master() {
    let (storage, txn) = instance();
    Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    let master = databases(&storage)[0].0;
    let schemas = schemas_table(&storage, &txn, master);

    // Three schemas in each of the four databases.
    assert_eq!(schemas.len(), 12);
    let names: BTreeSet<String> = schemas.iter().flat_map(|row| texts(row)).collect();
    let expected: BTreeSet<String> = SYSTEM_SCHEMAS.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(names, expected);

    // The rows of `master` itself: its `DbId` appears in the row beside the schema name.
    let of_master: BTreeSet<String> = schemas
        .iter()
        .filter(|row| row.contains(&Value::I32(master.0 as i32)))
        .flat_map(|row| texts(row))
        .collect();
    assert_eq!(of_master, expected);
}

#[test]
fn restart_finds_master() {
    let (storage, txn) = instance();
    let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    let before = databases(&storage);
    let tables_before = storage.tables(before[0].0).expect("tables(master)").len();
    drop(catalog);

    // A restart of the instance: the same storage, a catalogue built again on it.
    let restarted = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn));
    if let Err(err) = &restarted {
        panic!("a restart bootstraps again: {err}");
    }
    let after = databases(&storage);
    assert_eq!(after, before);
    assert_eq!(
        after.iter().filter(|(_, name)| name == "master").count(),
        1,
        "a restart adds a second master"
    );
    assert_eq!(
        storage.tables(after[0].0).expect("tables(master)").len(),
        tables_before
    );
}

#[test]
fn the_internal_tables_hold_a_row_per_database() {
    let (storage, txn) = instance();
    Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    let master = databases(&storage)[0].0;
    // The table of `master` that names the four databases, which is not the one that names
    // the schemas.
    let mut named: Vec<Vec<String>> = Vec::new();
    for (table, _) in storage.tables(master).expect("tables(master)") {
        for row in rows(&storage, &txn, table) {
            let texts = texts(&row);
            if texts.iter().any(|text| text == "msdb") {
                named.push(texts);
            }
        }
    }
    assert_eq!(named.len(), 1, "one row names `msdb`");
    assert!(
        named[0].contains(&"SQL_Latin1_General_CP1_CI_AS".to_string()),
        "the row of msdb carries its collation name: {:?}",
        named[0]
    );
}
