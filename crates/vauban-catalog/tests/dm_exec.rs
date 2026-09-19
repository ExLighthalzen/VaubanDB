//! Bootstrap leaves the internal tables of `views/dm_exec.rs` in `storage`: the
//! configuration options and the single row of `sys.dm_os_sys_info`.

use std::sync::Arc;

use vauban_catalog::Catalog;
use vauban_storage::{MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Len, SqlType, TypeInfo, Value};

fn instance() -> (Catalog, Arc<dyn Storage>, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    (catalog, storage, txn)
}

fn master(storage: &Arc<dyn Storage>) -> vauban_storage::DbId {
    storage
        .databases()
        .expect("databases()")
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case("master"))
        .expect("master is there after a bootstrap")
        .0
}

fn table_shaped(storage: &Arc<dyn Storage>, columns: &[TypeInfo]) -> TableId {
    let mut found: Vec<TableId> = storage
        .tables(master(storage))
        .expect("tables(master)")
        .into_iter()
        .filter(|(_, shape)| shape.columns == columns)
        .map(|(id, _)| id)
        .collect();
    assert_eq!(found.len(), 1, "one table of master has that shape");
    found.pop().expect("the shape was found")
}

fn rows(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    table: TableId,
) -> Vec<Vec<Value>> {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = txn.statement_snapshot(&handle);
    let read: Vec<Vec<Value>> = storage
        .scan(&snapshot, table)
        .expect("scan")
        .map(|row| row.expect("row").1.0)
        .collect();
    txn.commit(handle).expect("commit of the reading txn");
    read
}

fn configurations_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(35)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(255)), true),
        TypeInfo::new(SqlType::Bit, false),
        TypeInfo::new(SqlType::Bit, false),
    ]
}

fn os_info_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::DateTime, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::NVarChar(Len::Max), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
    ]
}

#[test]
fn bootstrap_creates_configuration_and_os_info_tables() {
    let (_, storage, txn) = instance();
    let configurations = table_shaped(&storage, &configurations_shape());
    let os_info = table_shaped(&storage, &os_info_shape());
    assert_eq!(rows(&storage, &txn, configurations).len(), 97);
    assert_eq!(rows(&storage, &txn, os_info).len(), 1);
}
