//! What `views/sys_principals.rs` publishes: `sys.database_principals`,
//! `sys.server_principals`, `sys.database_role_members` and `sys.server_role_members` resolve
//! as views in `master` and in a database a client creates, and the two internal tables
//! behind them carry the fixed principals and the fixed memberships.
//!
//! These tests know the public API only, as those of `tests/sys_object_kinds.rs` do: the names
//! of the internal tables are `pub(crate)`, so this file hardcodes the strings
//! (`vauban_sys_principals`, `vauban_sys_role_members`) rather than importing them. `name`
//! being a `CASE` on `principal_id` rather than a stored column (`src/views/sys_principals.rs`,
//! section "Fixed rows, `name` included"), the text of the two `_principals` definitions is
//! read to check every fixed name is there; the unit tests of `src/views/sys_principals.rs`
//! check the exact select list that carries it.

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

/// Whether `value` is the text `expected`.
fn is_text(value: &Value, expected: &str) -> bool {
    matches!(value, Value::String(s) if s.text == expected)
}

/// Whether any cell of `row` is the text `expected`.
fn row_has_text(row: &[Value], expected: &str) -> bool {
    row.iter().any(|v| is_text(v, expected))
}

/// The four views of this file, `sys.<name>`.
const VIEWS: [&str; 4] = [
    "database_principals",
    "server_principals",
    "database_role_members",
    "server_role_members",
];

#[test]
fn the_four_views_resolve_as_views_in_master_and_in_a_new_database() {
    let (catalog, _storage, txn) = instance();
    let creating = begin(&txn);
    catalog
        .create_database(&creating, "d", None)
        .expect("create_database");
    txn.commit(creating).expect("commit of the create");
    let handle = begin(&txn);
    let snapshot = catalog.snapshot(&handle);
    for view in VIEWS {
        for database in ["master", "d"] {
            let object = snapshot
                .resolve_object(database, Some("sys"), view, "dbo")
                .unwrap_or_else(|| panic!("{database}.sys.{view} resolves"));
            assert!(
                snapshot.table(object.id).is_none(),
                "{database}.sys.{view} is a view, not a table"
            );
            assert!(
                snapshot.view_definition(object.id).is_some(),
                "{database}.sys.{view} carries a definition"
            );
        }
    }
}

/// The rows of `master.dbo.vauban_sys_principals`, read through the public API.
fn principal_rows(
    catalog: &Catalog,
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
) -> Vec<Vec<Value>> {
    let handle = begin(txn);
    let snapshot = catalog.snapshot(&handle);
    let object = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_principals", "dbo")
        .expect("master.dbo.vauban_sys_principals resolves");
    let meta = snapshot
        .table(object.id)
        .expect("vauban_sys_principals is a table of the snapshot");
    rows(storage, txn, meta.storage_id)
}

#[test]
fn the_principals_table_carries_thirteen_database_rows_and_ten_server_rows() {
    let (catalog, storage, txn) = instance();
    let all = principal_rows(&catalog, &storage, &txn);
    assert_eq!(all.len(), 23, "{all:?}");
    let database_rows = all.iter().filter(|row| row_has_text(row, "D")).count();
    let server_rows = all.iter().filter(|row| row_has_text(row, "S")).count();
    // `type` also carries `S` (`SQL_USER` / `SQL_LOGIN`), so the two counts below are not a
    // partition of `all`; each is a lower bound the fixed rows of its kind must reach.
    assert!(database_rows >= 13, "{database_rows}");
    assert!(server_rows >= 10, "{server_rows}");
}

#[test]
fn sa_sid_is_one_byte_0x01() {
    let (catalog, storage, txn) = instance();
    let all = principal_rows(&catalog, &storage, &txn);
    let sa_sid = all
        .iter()
        .find(|row| row.contains(&Value::I32(1)) && row_has_text(row, "SQL_LOGIN"))
        .and_then(|row| row.iter().find(|v| matches!(v, Value::Bytes(_))))
        .expect("sa carries a sid");
    assert_eq!(sa_sid, &Value::Bytes(vec![0x01]));
}

#[test]
fn the_role_members_table_carries_dbo_in_db_owner_and_sa_in_sysadmin() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let snapshot = catalog.snapshot(&handle);
    let object = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_role_members", "dbo")
        .expect("master.dbo.vauban_sys_role_members resolves");
    let meta = snapshot
        .table(object.id)
        .expect("vauban_sys_role_members is a table of the snapshot");
    let all = rows(&storage, &txn, meta.storage_id);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(
        all.iter()
            .any(|row| row.contains(&Value::I32(16_384)) && row.contains(&Value::I32(1))),
        "dbo (1) is a member of db_owner (16384): {all:?}"
    );
    assert!(
        all.iter()
            .any(|row| row.contains(&Value::I32(3)) && row.contains(&Value::I32(1))),
        "sa (1) is a member of sysadmin (3): {all:?}"
    );
}

/// Every fixed name of the `_principals` views is a `THEN N'<name>'` branch of the `CASE`
/// their definition carries for `name` (`src/views/sys_principals.rs`).
#[test]
fn the_definitions_name_every_fixed_principal() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let snapshot = catalog.snapshot(&handle);
    let database_names = [
        "public",
        "dbo",
        "guest",
        "INFORMATION_SCHEMA",
        "sys",
        "db_owner",
        "db_accessadmin",
        "db_securityadmin",
        "db_ddladmin",
        "db_datareader",
        "db_datawriter",
        "db_denydatareader",
        "db_denydatawriter",
    ];
    let server_names = [
        "sa",
        "public",
        "sysadmin",
        "securityadmin",
        "serveradmin",
        "setupadmin",
        "processadmin",
        "diskadmin",
        "dbcreator",
        "bulkadmin",
    ];
    let definition_of = |view: &str| -> String {
        let object = snapshot
            .resolve_object("master", Some("sys"), view, "dbo")
            .unwrap_or_else(|| panic!("sys.{view} resolves"));
        snapshot
            .view_definition(object.id)
            .unwrap_or_else(|| panic!("sys.{view} carries a definition"))
            .to_owned()
    };
    let database_principals = definition_of("database_principals");
    let server_principals = definition_of("server_principals");
    for name in database_names {
        assert!(
            database_principals.contains(&format!("N'{name}'")),
            "sys.database_principals: {name}"
        );
    }
    for name in server_names {
        assert!(
            server_principals.contains(&format!("N'{name}'")),
            "sys.server_principals: {name}"
        );
    }
}
