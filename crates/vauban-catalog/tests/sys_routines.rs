//! What `views/sys_routines.rs` publishes: `sys.procedures`, `sql_modules` and
//! `sys.parameters` resolve in `master`, carry no join and read no row, present or in a
//! database a client creates. The column names and their order are checked here from the
//! text `CatalogSnapshot::view_definition` hands back — the same surface a client reads
//! through `sys.dm_exec_describe_first_result_set` — since the internal tables of
//! `views/sys_routines.rs` are `pub(crate)` and are covered from inside the crate instead
//! (`src/views/sys_routines.rs`).

use std::sync::Arc;

use vauban_catalog::Catalog;
use vauban_storage::{MemoryStorage, Storage};
use vauban_txn::{IsolationLevel, TransactionManager};

/// A bootstrapped catalogue over a fresh `MemoryStorage`, with its transaction manager.
fn instance() -> (Catalog, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(storage, Arc::clone(&txn)).expect("bootstrap");
    (catalog, txn)
}

/// The column names a `SELECT` text of `views/` publishes, in order: the last identifier of
/// each comma-separated item, unbracketed.
fn published_columns(definition: &str) -> Vec<String> {
    definition
        .split("\n  FROM ")
        .next()
        .expect("a FROM clause")
        .trim_start_matches("SELECT ")
        .split(",\n       ")
        .map(|item| {
            let name = item
                .rsplit(' ')
                .next()
                .unwrap_or_else(|| panic!("{item} names its column"));
            name.trim_matches(['[', ']']).to_owned()
        })
        .collect()
}

/// The ordered column names of `sys.procedures` in SQL Server 2022.
const PUBLISHED_PROCEDURES_COLUMNS: [&str; 16] = [
    "name",
    "object_id",
    "principal_id",
    "schema_id",
    "parent_object_id",
    "type",
    "type_desc",
    "create_date",
    "modify_date",
    "is_ms_shipped",
    "is_published",
    "is_schema_published",
    "is_auto_executed",
    "is_execution_replicated",
    "is_repl_serializable_only",
    "skips_repl_constraints",
];

/// The ordered column names of `sql_modules`.
const PUBLISHED_SQL_MODULES_COLUMNS: [&str; 12] = [
    "object_id",
    "definition",
    "uses_ansi_nulls",
    "uses_quoted_identifier",
    "is_schema_bound",
    "uses_database_collation",
    "is_recompiled",
    "null_on_null_input",
    "execute_as_principal_id",
    "uses_native_compilation",
    "inline_type",
    "is_inlineable",
];

/// The ordered column names of `sys.parameters`.
const PUBLISHED_PARAMETERS_COLUMNS: [&str; 21] = [
    "object_id",
    "name",
    "parameter_id",
    "system_type_id",
    "user_type_id",
    "max_length",
    "precision",
    "scale",
    "is_output",
    "is_cursor_ref",
    "has_default_value",
    "is_xml_document",
    "default_value",
    "xml_collection_id",
    "is_readonly",
    "is_nullable",
    "encryption_type",
    "encryption_type_desc",
    "encryption_algorithm_name",
    "column_encryption_key_id",
    "column_encryption_key_database_name",
];

#[test]
fn sys_routines_shape_matches_the_measure() {
    let (catalog, txn) = instance();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = catalog.snapshot(&handle);
    for (name, published) in [
        ("procedures", &PUBLISHED_PROCEDURES_COLUMNS[..]),
        ("sql_modules", &PUBLISHED_SQL_MODULES_COLUMNS[..]),
        ("parameters", &PUBLISHED_PARAMETERS_COLUMNS[..]),
    ] {
        let object = snapshot
            .resolve_object("master", Some("sys"), name, "dbo")
            .unwrap_or_else(|| panic!("sys.{name} resolves"));
        assert!(
            snapshot.table(object.id).is_none(),
            "sys.{name} is a view, not a table"
        );
        let definition = snapshot
            .view_definition(object.id)
            .unwrap_or_else(|| panic!("sys.{name} carries a definition"));
        assert!(
            !definition.to_ascii_uppercase().contains("JOIN"),
            "sys.{name}: {definition}"
        );
        assert_eq!(published_columns(definition), published, "sys.{name}");
    }
}

#[test]
fn sys_routines_resolve_from_a_database_a_client_creates() {
    // `OBJECT_ID('d.sys.procedures')` and `OBJECT_ID('master.sys.procedures')` are one number
    // in SQL Server, whichever database names the view (`a_system_view_is_visible_from_a_user_database`
    // of `tests/snapshot.rs` proves the general rule; this checks it holds for the three
    // views of this file too).
    let (catalog, txn) = instance();
    let creating = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .create_database(&creating, "d", None)
        .expect("create_database");
    txn.commit(creating).expect("commit of the create");
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = catalog.snapshot(&handle);
    for name in ["procedures", "sql_modules", "parameters"] {
        let from_master = snapshot
            .resolve_object("master", Some("sys"), name, "dbo")
            .unwrap_or_else(|| panic!("sys.{name} resolves from master"))
            .id;
        let from_d = snapshot
            .resolve_object("d", Some("sys"), name, "dbo")
            .unwrap_or_else(|| panic!("sys.{name} resolves from d"))
            .id;
        assert_eq!(from_master, from_d, "sys.{name}");
    }
}
