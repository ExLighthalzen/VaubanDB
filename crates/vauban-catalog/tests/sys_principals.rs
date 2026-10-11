//! What `views/sys_principals.rs` publishes: `sys.database_principals`,
//! `sys.server_principals`, `sys.database_role_members` and `sys.server_role_members` resolve
//! as views in `master` and in a database a client creates, and the two internal tables
//! behind them carry the fixed principals and the fixed memberships.
//!
//! These tests know the public API only, as those of `tests/sys_object_kinds.rs` do: the names
//! of the internal tables are `pub(crate)`, so this file hardcodes the strings
//! (`vauban_sys_principals`, `vauban_sys_role_members`) rather than importing them. `name` is
//! a column of the principals table, so the fixed names are read from its rows, and a row
//! written there is read back through the text of the view
//! (`a_row_written_in_the_principals_table_is_a_row_of_the_view`).

use std::sync::Arc;

use vauban_catalog::Catalog;
use vauban_storage::{MemoryStorage, Row, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::SqlString;
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

/// The names the principals table carries for each `kind`: `D` for
/// `sys.database_principals`, `S` for `sys.server_principals`.
fn names_of_kind(rows: &[Vec<Value>], kind: &str) -> Vec<String> {
    rows.iter()
        .filter(|row| is_text(&row[0], kind))
        .map(|row| match &row[2] {
            Value::String(name) => name.text.clone(),
            other => panic!("name is a string: {other:?}"),
        })
        .collect()
}

#[test]
fn the_principals_table_carries_every_fixed_name() {
    let (catalog, storage, txn) = instance();
    let all = principal_rows(&catalog, &storage, &txn);
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
    assert_eq!(names_of_kind(&all, "D"), database_names);
    assert_eq!(names_of_kind(&all, "S"), server_names);
}

/// The rows the view whose text is `definition` reads from `table`, a view of
/// `views/sys_principals.rs`: each item of the select list that is a bare column name is read
/// from the column of that name, the others (literals the view casts) are left out, and the
/// `WHERE <column> = '<text>'` filter keeps the rows it names. Returns `(column, value)` pairs.
fn evaluate(
    definition: &str,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Vec<Vec<(String, Value)>> {
    let select_list = definition
        .strip_prefix("SELECT ")
        .and_then(|rest| rest.split("\n  FROM ").next())
        .expect("SELECT … FROM");
    let read: Vec<(String, usize)> = select_list
        .split(",\n")
        .map(str::trim)
        .filter(|item| !item.contains(' '))
        .map(|item| {
            let position = columns
                .iter()
                .position(|column| column == item)
                .unwrap_or_else(|| panic!("`{item}` is a column of the table"));
            (item.to_owned(), position)
        })
        .collect();
    let filter = definition
        .split("\n WHERE ")
        .nth(1)
        .expect("a WHERE clause");
    let (column, literal) = filter.split_once(" = ").expect("<column> = '<text>'");
    let kind_position = columns
        .iter()
        .position(|name| name == column.trim())
        .expect("the filter column is a column of the table");
    let kind = literal.trim().trim_matches('\'');
    rows.iter()
        .filter(|row| is_text(&row[kind_position], kind))
        .map(|row| {
            read.iter()
                .map(|(name, position)| (name.clone(), row[*position].clone()))
                .collect()
        })
        .collect()
}

#[test]
fn a_row_written_in_the_principals_table_is_a_row_of_the_view() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let snapshot = catalog.snapshot(&handle);
    let object = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_principals", "dbo")
        .expect("master.dbo.vauban_sys_principals resolves");
    let meta = snapshot
        .table(object.id)
        .expect("vauban_sys_principals is a table of the snapshot")
        .clone();
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
    let columns: Vec<String> = meta.columns.iter().map(|c| c.name.clone()).collect();
    let mut written = vec![Value::Null; columns.len()];
    let mut set = |column: &str, value: Value| {
        let position = columns
            .iter()
            .position(|name| name == column)
            .unwrap_or_else(|| panic!("`{column}` is a column of the table"));
        written[position] = value;
    };
    let text = |s: &str| Value::String(SqlString { text: s.to_owned() });
    set("kind", text("D"));
    set("principal_id", Value::I32(5));
    set("name", text("app_reader"));
    set("type", text("S"));
    set("type_desc", text("SQL_USER"));
    set("is_fixed_role", Value::Bit(false));
    set("default_schema_name", text("dbo"));
    storage
        .insert(handle.id, meta.storage_id, &Row(written))
        .expect("insert into the principals table");
    txn.commit(handle).expect("commit of the insert");

    let all = rows(&storage, &txn, meta.storage_id);
    let database_view = evaluate(&database_principals, &columns, &all);
    let added: Vec<&Vec<(String, Value)>> = database_view
        .iter()
        .filter(|row| row.contains(&("principal_id".to_owned(), Value::I32(5))))
        .collect();
    assert_eq!(added.len(), 1, "{database_view:?}");
    assert!(
        added[0].contains(&("name".to_owned(), text("app_reader"))),
        "{added:?}"
    );
    assert!(
        added[0].contains(&("type_desc".to_owned(), text("SQL_USER"))),
        "{added:?}"
    );
    assert_eq!(
        database_view.len(),
        14,
        "thirteen fixed rows and the added one"
    );

    let server_view = evaluate(&server_principals, &columns, &all);
    assert_eq!(server_view.len(), 10, "{server_view:?}");
    assert!(
        server_view
            .iter()
            .all(|row| !row.contains(&("name".to_owned(), text("app_reader")))),
        "a row of kind D is not a row of sys.server_principals"
    );
}

/// `server_principal_sid` answers, for each server row of the principals table, the `sid` that
/// row carries; the name compares under the default collation; a database principal and a name
/// the table does not carry answer `None`.
#[test]
fn server_principal_sid_reads_the_published_rows() {
    let (catalog, storage, txn) = instance();
    let all = principal_rows(&catalog, &storage, &txn);
    let server_rows: Vec<&Vec<Value>> = all.iter().filter(|row| is_text(&row[0], "S")).collect();
    assert_eq!(server_rows.len(), 10, "{all:?}");
    for row in server_rows {
        let Value::String(name) = &row[2] else {
            panic!("a server row carries its name: {row:?}");
        };
        let Value::Bytes(sid) = &row[3] else {
            panic!("a server row carries its sid: {row:?}");
        };
        assert_eq!(
            vauban_catalog::server_principal_sid(&name.text).as_ref(),
            Some(sid),
            "{}",
            name.text
        );
    }
    for (name, expected) in [
        ("sa", Some(vec![0x01])),
        ("SA", Some(vec![0x01])),
        ("sa ", Some(vec![0x01])),
        ("public", Some(vec![0x02])),
        ("sysadmin", Some(vec![0x03])),
        ("bulkadmin", Some(vec![0x0A])),
        (" sa", None),
        ("nosuch", None),
        ("", None),
        ("dbo", None),
        ("db_owner", None),
    ] {
        assert_eq!(
            vauban_catalog::server_principal_sid(name),
            expected,
            "{name:?}"
        );
    }
}
