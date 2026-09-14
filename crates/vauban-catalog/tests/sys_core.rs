//! What the bootstrap leaves in `storage` for `views/sys_core.rs`: the rows behind
//! `sys.databases`, `sys.schemas` and `sys.types`, read through the public API.
//!
//! These tests know the public API only, as those of `tests/bootstrap.rs` do: the names of
//! the internal tables are `pub(crate)`, so a test that must reach one finds it by its
//! content. The shape of the three views and the text of their definitions are checked by
//! the unit tests of `src/views/sys_core.rs`, which can read what this file cannot.
//!
//! The views are checked here through the internal rows they read.

use std::collections::BTreeSet;
use std::sync::Arc;

use vauban_catalog::Catalog;
use vauban_storage::{MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::Value;

/// A bootstrapped catalogue on a fresh storage, with the pieces the tests read it through.
fn instance() -> (Arc<dyn Storage>, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    (storage, txn)
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

/// The rows of the table of `master` that holds a row carrying the text `text`.
///
/// How these tests reach an internal table without knowing its name, the names being
/// `pub(crate)`: the types table holds the row of `int`, the table of the databases the row
/// of `msdb`, the table of the schemas the row of `dbo`. The assertion below fails if a
/// second table of `master` ever carries the same text, rather than reading the wrong one.
fn table_holding(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    text: &str,
) -> Vec<Vec<Value>> {
    let master = storage.databases().expect("databases()")[0].0;
    let mut found = Vec::new();
    for (table, _) in storage.tables(master).expect("tables(master)") {
        let content = rows(storage, txn, table);
        if content
            .iter()
            .any(|row| row.iter().any(|value| is_text(value, text)))
        {
            found.push(content);
        }
    }
    assert_eq!(
        found.len(),
        1,
        "one table of master carries the text `{text}`"
    );
    found.pop().unwrap_or_default()
}

/// Whether a value is that piece of text, compared without regard to case as
/// `SQL_Latin1_General_CP1_CI_AS` compares a `sysname`.
fn is_text(value: &Value, text: &str) -> bool {
    matches!(value, Value::String(stored) if stored.text.eq_ignore_ascii_case(text))
}

/// The row of the types table whose `name` column is `name`.
fn type_row(rows: &[Vec<Value>], name: &str) -> Vec<Value> {
    rows.iter()
        .find(|row| is_text(&row[0], name))
        .unwrap_or_else(|| panic!("the type `{name}` has a row"))
        .clone()
}

#[test]
fn bootstrap_writes_the_system_types_in_master() {
    let (storage, txn) = instance();
    let types = table_holding(&storage, &txn, "int");
    // The 34 system types, each row as wide as the 15 columns of `sys.types`.
    assert_eq!(types.len(), 34);
    for row in &types {
        assert_eq!(row.len(), 15);
    }
}

#[test]
fn sys_types_contains_int_and_nvarchar() {
    let (storage, txn) = instance();
    let types = table_holding(&storage, &txn, "int");
    // `name` is matched without regard to case; `system_type_id` and `user_type_id` are the
    // ones of `sys.types`.
    let int = type_row(&types, "INT");
    assert_eq!(int[1], Value::I8(56));
    assert_eq!(int[2], Value::I32(56));
    let nvarchar = type_row(&types, "NVarChar");
    assert_eq!(nvarchar[1], Value::I8(231));
    assert_eq!(nvarchar[2], Value::I32(231));
    // `schema_id` 4 is `sys`, the schema the rows of `sys.types` belong to.
    assert_eq!(int[3], Value::I32(4));
    assert_eq!(nvarchar[3], Value::I32(4));
}

#[test]
fn the_rows_sys_databases_reads_name_the_four_system_databases() {
    let (storage, txn) = instance();
    let databases = table_holding(&storage, &txn, "msdb");
    let names: BTreeSet<String> = databases
        .iter()
        .filter_map(|row| match &row[1] {
            Value::String(name) => Some(name.text.clone()),
            _ => None,
        })
        .collect();
    let expected: BTreeSet<String> = ["master", "tempdb", "model", "msdb"]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(names, expected);
}

#[test]
fn the_rows_sys_schemas_reads_carry_the_three_schemas_of_each_database() {
    let (storage, txn) = instance();
    let schemas = table_holding(&storage, &txn, "dbo");
    // Three schemas in each of the four system databases: the view filters this table on
    // `database_id = DB_ID()`.
    assert_eq!(schemas.len(), 12);
    let pairs: BTreeSet<(i32, String)> = schemas
        .iter()
        .filter_map(|row| match (&row[0], &row[2]) {
            (Value::I32(database), Value::String(name)) => Some((*database, name.text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(pairs.len(), 12);
    let of_master: BTreeSet<String> = pairs
        .iter()
        .filter(|(database, _)| *database == 1)
        .map(|(_, name)| name.clone())
        .collect();
    let expected: BTreeSet<String> = ["dbo", "sys", "INFORMATION_SCHEMA"]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(of_master, expected);
}
