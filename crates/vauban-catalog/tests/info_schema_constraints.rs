//! What `views/info_schema_constraints.rs` publishes: `INFORMATION_SCHEMA.TABLE_CONSTRAINTS`,
//! `KEY_COLUMN_USAGE`, `REFERENTIAL_CONSTRAINTS` and `VIEWS` resolve in `master` and in a
//! database a client creates, with no join. The column names and their order are checked here
//! from the text `CatalogSnapshot::view_definition` hands back, as `tests/sys_routines.rs`
//! does: the internal tables of `views/info_schema_constraints.rs` are `pub(crate)` and are
//! covered from inside the crate instead (`src/views/info_schema_constraints.rs`).
//!
//! The content of the three tables — one row per constraint, one row per key column, one row
//! per `FOREIGN KEY` — is checked from inside the crate as well (`table.constraints` and the
//! `IndexStore` are not part of the public API); this file reaches what a client sees through
//! the four views' own text and, for `VIEWS`, the one storage fact its filter depends on
//! (`views_reads_a_table_where_a_system_view_row_carries_no_database`).

use std::sync::Arc;

use vauban_catalog::Catalog;
use vauban_storage::{DbId, MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Len, SqlType, TypeInfo, Value};

/// A bootstrapped catalogue over a fresh `MemoryStorage`, with its storage and its manager.
fn instance() -> (Catalog, Arc<dyn Storage>, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    (catalog, storage, txn)
}

/// The `DbId` of `master`, where the internal tables live.
fn master(storage: &Arc<dyn Storage>) -> DbId {
    storage
        .databases()
        .expect("databases()")
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case("master"))
        .expect("master is there after a bootstrap")
        .0
}

/// The identifier of the one table of `master` whose shape is `columns`, as
/// `tests/sys_extra.rs` reaches the table `sys.views` reads.
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

/// The rows of `table`, read through a transaction of its own.
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

/// The shape of the copy of the objects table `sys.all_objects` and `sys.views` read
/// (`tests/sys_extra.rs`, `all_objects_shape`): the columns of the table of
/// `views/sys_tables.rs` with a nullable `database_id`.
fn all_objects_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, true),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Char(Len::Fixed(2)), false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::Bit, false),
        TypeInfo::new(SqlType::DateTime, false),
        TypeInfo::new(SqlType::DateTime, false),
        TypeInfo::new(SqlType::Int, false),
    ]
}

/// The column names a `SELECT` text of `views/` publishes, in order: the last identifier of
/// each comma-separated item.
fn published_columns(definition: &str) -> Vec<String> {
    definition
        .split("\n  FROM ")
        .next()
        .expect("a FROM clause")
        .trim_start_matches("SELECT ")
        .split(",\n       ")
        .map(|item| {
            item.rsplit(' ')
                .next()
                .unwrap_or_else(|| panic!("{item} names its column"))
                .to_owned()
        })
        .collect()
}

/// The ordered column names of `INFORMATION_SCHEMA.TABLE_CONSTRAINTS` in SQL Server 2022.
const PUBLISHED_TABLE_CONSTRAINTS_COLUMNS: [&str; 9] = [
    "CONSTRAINT_CATALOG",
    "CONSTRAINT_SCHEMA",
    "CONSTRAINT_NAME",
    "TABLE_CATALOG",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "CONSTRAINT_TYPE",
    "IS_DEFERRABLE",
    "INITIALLY_DEFERRED",
];

/// The ordered column names of `INFORMATION_SCHEMA.KEY_COLUMN_USAGE`.
const PUBLISHED_KEY_COLUMN_USAGE_COLUMNS: [&str; 8] = [
    "CONSTRAINT_CATALOG",
    "CONSTRAINT_SCHEMA",
    "CONSTRAINT_NAME",
    "TABLE_CATALOG",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "COLUMN_NAME",
    "ORDINAL_POSITION",
];

/// The ordered column names of `INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS`.
const PUBLISHED_REFERENTIAL_CONSTRAINTS_COLUMNS: [&str; 9] = [
    "CONSTRAINT_CATALOG",
    "CONSTRAINT_SCHEMA",
    "CONSTRAINT_NAME",
    "UNIQUE_CONSTRAINT_CATALOG",
    "UNIQUE_CONSTRAINT_SCHEMA",
    "UNIQUE_CONSTRAINT_NAME",
    "MATCH_OPTION",
    "UPDATE_RULE",
    "DELETE_RULE",
];

/// The ordered column names of `INFORMATION_SCHEMA.VIEWS`.
const PUBLISHED_VIEWS_COLUMNS: [&str; 6] = [
    "TABLE_CATALOG",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "VIEW_DEFINITION",
    "CHECK_OPTION",
    "IS_UPDATABLE",
];

#[test]
fn the_four_views_resolve_in_master_with_their_published_shape() {
    let (catalog, _storage, txn) = instance();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = catalog.snapshot(&handle);
    for (name, published) in [
        (
            "TABLE_CONSTRAINTS",
            &PUBLISHED_TABLE_CONSTRAINTS_COLUMNS[..],
        ),
        ("KEY_COLUMN_USAGE", &PUBLISHED_KEY_COLUMN_USAGE_COLUMNS[..]),
        (
            "REFERENTIAL_CONSTRAINTS",
            &PUBLISHED_REFERENTIAL_CONSTRAINTS_COLUMNS[..],
        ),
        ("VIEWS", &PUBLISHED_VIEWS_COLUMNS[..]),
    ] {
        let object = snapshot
            .resolve_object("master", Some("INFORMATION_SCHEMA"), name, "dbo")
            .unwrap_or_else(|| panic!("INFORMATION_SCHEMA.{name} resolves, no 208"));
        assert!(
            snapshot.table(object.id).is_none(),
            "INFORMATION_SCHEMA.{name} is a view, not a table"
        );
        let definition = snapshot
            .view_definition(object.id)
            .unwrap_or_else(|| panic!("INFORMATION_SCHEMA.{name} carries a definition"));
        assert!(
            !definition.to_ascii_uppercase().contains("JOIN"),
            "INFORMATION_SCHEMA.{name}: {definition}"
        );
        assert_eq!(published_columns(definition), published, "{name}");
    }
}

#[test]
fn the_four_views_resolve_from_a_database_a_client_creates() {
    // `OBJECT_ID('d.INFORMATION_SCHEMA.VIEWS')` and
    // `OBJECT_ID('master.INFORMATION_SCHEMA.VIEWS')` are one number in SQL Server, whichever
    // database names the view (`a_system_view_is_visible_from_a_user_database` of
    // `tests/snapshot.rs` proves the general rule for the catalogue; this checks it holds for
    // the four views of this file too, so a `SELECT *` against a created database does not
    // send 208).
    let (catalog, _storage, txn) = instance();
    let creating = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .create_database(&creating, "d", None)
        .expect("create_database");
    txn.commit(creating).expect("commit of the create");
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = catalog.snapshot(&handle);
    for name in [
        "TABLE_CONSTRAINTS",
        "KEY_COLUMN_USAGE",
        "REFERENTIAL_CONSTRAINTS",
        "VIEWS",
    ] {
        let from_master = snapshot
            .resolve_object("master", Some("INFORMATION_SCHEMA"), name, "dbo")
            .unwrap_or_else(|| panic!("INFORMATION_SCHEMA.{name} resolves from master"))
            .id;
        let from_d = snapshot
            .resolve_object("d", Some("INFORMATION_SCHEMA"), name, "dbo")
            .unwrap_or_else(|| panic!("INFORMATION_SCHEMA.{name} resolves from d"))
            .id;
        assert_eq!(from_master, from_d, "INFORMATION_SCHEMA.{name}");
    }
}

#[test]
fn views_reads_a_table_where_a_system_view_row_carries_no_database() {
    // `INFORMATION_SCHEMA.VIEWS` reads `vauban_sys_all_objects` with the same filter
    // `sys.views` uses (`views/info_schema_constraints.rs`, section "`VIEWS`"): a system-view
    // row of that table carries `database_id NULL`, which is what lets the `OR database_id IS
    // NULL` half of the filter answer it under any `DB_ID()`. Had the filter this file wrote
    // been `database_id = DB_ID()` alone, a database a client creates would see none of them
    // and `SELECT *` would pass on an empty set without erroring — the failure this test's
    // neighbours cannot catch, since they only ask whether the name resolves, not what the
    // rows underneath carry.
    let (_catalog, storage, txn) = instance();
    let table = table_shaped(&storage, &all_objects_shape());
    let system_view_rows = rows(&storage, &txn, table)
        .into_iter()
        .filter(|row| matches!(&row[0], Value::Null))
        .count();
    assert!(
        system_view_rows > 0,
        "no system-view row carries database_id NULL"
    );
}
