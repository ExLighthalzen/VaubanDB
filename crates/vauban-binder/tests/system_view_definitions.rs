//! The T-SQL text of the system views the catalogue installs is read by the parser of the
//! workspace: each definition parses as one `SELECT`, and the parser refuses the same text
//! once a reserved column name loses its brackets or a `CASE` loses its `END`.

use std::sync::Arc;

use vauban_catalog::{Catalog, CatalogSnapshot};
use vauban_parser::{ParseOptions, Statement, parse_batch};
use vauban_storage::{MemoryStorage, Storage};
use vauban_txn::{IsolationLevel, TransactionManager};

/// The system views whose text this file reads, in `master`.
const VIEWS: [&str; 15] = [
    "indexes",
    "index_columns",
    "key_constraints",
    "identity_columns",
    "databases",
    "schemas",
    "types",
    "database_principals",
    "server_principals",
    "database_role_members",
    "server_role_members",
    "foreign_keys",
    "foreign_key_columns",
    "check_constraints",
    "default_constraints",
];

/// The catalogue of a fresh memory storage, as a new transaction sees it.
fn snapshot() -> CatalogSnapshot {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(storage, Arc::clone(&txn)).expect("bootstrap");
    let read = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = catalog.snapshot(&read);
    txn.commit(read).expect("commit read");
    snapshot
}

/// The text of `master.sys.<name>`.
fn definition_of(snapshot: &CatalogSnapshot, name: &str) -> String {
    let object = snapshot
        .resolve_object("master", Some("sys"), name, "dbo")
        .unwrap_or_else(|| panic!("sys.{name} is installed"));
    snapshot
        .view_definition(object.id)
        .unwrap_or_else(|| panic!("sys.{name} carries a definition"))
        .to_owned()
}

#[test]
fn each_definition_parses_as_one_select() {
    let snapshot = snapshot();
    for name in VIEWS {
        let definition = definition_of(&snapshot, name);
        let batch = parse_batch(&definition, &ParseOptions::default())
            .unwrap_or_else(|err| panic!("sys.{name}: {}\n{definition}", err.message));
        assert_eq!(batch.statements.len(), 1, "sys.{name}");
        assert!(
            matches!(batch.statements[0], Statement::Select(_)),
            "sys.{name} is a SELECT"
        );
    }
}

#[test]
fn a_reserved_column_name_is_delimited() {
    // `file` stays a reserved word: in place of the delimited column, it parses bracketed
    // and is refused bare.
    let snapshot = snapshot();
    for name in ["identity_columns", "types"] {
        let definition = definition_of(&snapshot, name);
        assert!(definition.contains("[precision]"), "{definition}");
        let bracketed = definition.replace("[precision]", "[file]");
        assert!(
            parse_batch(&bracketed, &ParseOptions::default()).is_ok(),
            "{bracketed}"
        );
        let bare = definition.replace("[precision]", "file");
        assert!(
            parse_batch(&bare, &ParseOptions::default()).is_err(),
            "a bare `file` in the select list of sys.{name} is read by the parser: {bare}"
        );
    }
}

#[test]
fn the_case_of_sys_databases_is_read_by_the_parser() {
    // The same text with the `END` of its `CASE` taken out is refused, so the parse of
    // `each_definition_parses_as_one_select` is not blind to a broken `CASE`.
    let definition = definition_of(&snapshot(), "databases");
    let broken = definition.replace(" END AS nvarchar(60))", " AS nvarchar(60))");
    assert_ne!(broken, definition);
    assert!(
        parse_batch(&broken, &ParseOptions::default()).is_err(),
        "a `CASE` without its `END` is read by the parser: {broken}"
    );
}
