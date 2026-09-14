//! `CREATE DATABASE` / `DROP DATABASE` seen from outside the crate: what a caller of
//! [`Catalog::create_database`] and [`Catalog::drop_database`] observes in `storage` and in
//! the errors it gets back.
//!
//! The rows of the internal tables are checked by the unit tests of `src/database.rs`, the
//! only place they are reachable; what is checked here is the public API, the numbers of the
//! errors and the state of `storage`.
//!
//! The two deferred actions the catalogue registers — the deferred drop and the compensated
//! create — are checked through `storage` by `drop_is_deferred_until_commit` and
//! `create_compensated_on_rollback`: what `Storage::databases` reports before the end of the
//! transaction, and what it reports after.

use std::sync::Arc;

use vauban_catalog::Catalog;
use vauban_storage::{MemoryStorage, Storage};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};

/// A catalogue bootstrapped on a fresh `MemoryStorage`, with that storage and the
/// transaction manager it was given: `Catalog` keeps its own reference `pub(crate)`, so a
/// test outside the crate holds the manager it built.
fn bootstrapped() -> (Arc<dyn Storage>, Arc<TransactionManager>, Catalog) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    (storage, txn, catalog)
}

/// Runs `body` in a transaction of `txn` and commits it.
fn committed<T>(txn: &Arc<TransactionManager>, body: impl FnOnce(&TxnHandle) -> T) -> T {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let out = body(&handle);
    txn.commit(handle).expect("commit");
    out
}

/// The names `storage` holds, in the order [`Storage::databases`] sorts them.
fn names(storage: &Arc<dyn Storage>) -> Vec<String> {
    storage
        .databases()
        .expect("databases")
        .into_iter()
        .map(|(_, name)| name)
        .collect()
}

#[test]
fn create_database_then_list() {
    let (storage, txn, catalog) = bootstrapped();
    let id = committed(&txn, |handle| {
        catalog
            .create_database(handle, "d", None)
            .expect("create_database")
    });
    assert_eq!(
        names(&storage),
        vec!["master", "tempdb", "model", "msdb", "d"]
    );
    // The four system databases took 1 to 4 at the bootstrap, so the first user database of
    // this instance is 5 (`MemoryStorage` numbers in creation order).
    assert_eq!(id.0, 5);
}

#[test]
fn create_database_duplicate_is_1801() {
    let (_storage, txn, catalog) = bootstrapped();
    committed(&txn, |handle| {
        catalog
            .create_database(handle, "d", None)
            .expect("create_database")
    });
    let err = committed(&txn, |handle| {
        catalog
            .create_database(handle, "d", None)
            .expect_err("the second create is refused")
    });
    assert_eq!(err.number, 1801);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 3);
    assert!(err.message.contains("'d'"), "{}", err.message);
}

#[test]
fn create_database_is_case_insensitive() {
    let (storage, txn, catalog) = bootstrapped();
    committed(&txn, |handle| {
        catalog
            .create_database(handle, "D", None)
            .expect("create_database")
    });
    let err = committed(&txn, |handle| {
        catalog
            .create_database(handle, "d", None)
            .expect_err("`d` is the name `D`")
    });
    assert_eq!(err.number, 1801);
    // The message quotes the name as the caller wrote it, not the stored spelling.
    assert!(err.message.contains("'d'"), "{}", err.message);
    assert!(!err.message.contains("'D'"), "{}", err.message);
    // And no second database was created for the second spelling.
    assert_eq!(
        names(&storage),
        vec!["master", "tempdb", "model", "msdb", "D"]
    );
    // The same holds in a single transaction, before any commit.
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .create_database(&handle, "e", None)
        .expect("create_database");
    let err = catalog
        .create_database(&handle, "E", None)
        .expect_err("`E` is the name `e` the same transaction just created");
    assert_eq!(err.number, 1801);
    txn.rollback(handle).expect("rollback");
}

#[test]
fn drop_master_is_refused() {
    let (storage, txn, catalog) = bootstrapped();
    for name in ["master", "TEMPDB", "model", "msdb"] {
        let err = committed(&txn, |handle| {
            catalog
                .drop_database(handle, name)
                .expect_err("a system database is not dropped")
        });
        // 3708 severity 16 state 4; the name is printed as the statement wrote it, so the
        // `TEMPDB` turn prints `TEMPDB` and not the `tempdb` of the catalogue.
        assert_eq!(err.number, 3708, "number of the {name} refusal");
        assert_eq!(err.severity, 16, "severity of the {name} refusal");
        assert_eq!(err.state, 4, "state of the {name} refusal");
        assert!(
            err.message.contains(&format!("'{name}'")),
            "{}",
            err.message
        );
    }
    assert_eq!(names(&storage), vec!["master", "tempdb", "model", "msdb"]);
}

#[test]
fn drop_unknown_is_3701() {
    let (_storage, txn, catalog) = bootstrapped();
    let err = committed(&txn, |handle| {
        catalog
            .drop_database(handle, "nope")
            .expect_err("no database of that name")
    });
    assert_eq!(err.number, 3701);
    assert_eq!(err.severity, 11);
    assert_eq!(err.state, 1);
    assert!(err.message.contains("'nope'"), "{}", err.message);
}

#[test]
fn drop_is_deferred_until_commit() {
    let (storage, txn, catalog) = bootstrapped();
    committed(&txn, |handle| {
        catalog
            .create_database(handle, "d", None)
            .expect("create_database")
    });
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog.drop_database(&handle, "d").expect("drop_database");
    // Nothing was asked of `storage` during the statement: the catalogue registers
    // `CommitAction::DropDatabase` instead, so a reader keeps seeing the database.
    assert!(names(&storage).contains(&"d".to_owned()));
    txn.commit(handle).expect("commit");
    // The commit runs the action, and the database leaves `storage` with its rows.
    assert_eq!(names(&storage), vec!["master", "tempdb", "model", "msdb"]);
    // The name is free for a new database, which gets an identifier of its own:
    // `Storage::drop_database` never reuses the one it took back.
    let id = committed(&txn, |handle| {
        catalog
            .create_database(handle, "d", None)
            .expect("the name is free again")
    });
    assert_eq!(id.0, 6);
    assert_eq!(
        names(&storage),
        vec!["master", "tempdb", "model", "msdb", "d"]
    );
}

#[test]
fn create_compensated_on_rollback() {
    let (storage, txn, catalog) = bootstrapped();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let first = catalog
        .create_database(&handle, "d", None)
        .expect("create_database");
    assert!(names(&storage).contains(&"d".to_owned()));
    txn.rollback(handle).expect("rollback");
    // `RollbackAction::DropDatabase` ran: the row went away with the transaction and the
    // database went with it.
    assert_eq!(names(&storage), vec!["master", "tempdb", "model", "msdb"]);
    // The name is free, and the create that follows builds a database of its own rather
    // than finding the one the rolled back transaction had made.
    let second = committed(&txn, |handle| {
        catalog
            .create_database(handle, "d", None)
            .expect("the name is free after the rollback")
    });
    assert_ne!(first, second);
    assert_eq!(
        names(&storage),
        vec!["master", "tempdb", "model", "msdb", "d"]
    );
}

#[test]
fn a_drop_is_invisible_to_another_transaction_until_the_commit() {
    let (_storage, txn, catalog) = bootstrapped();
    committed(&txn, |handle| {
        catalog
            .create_database(handle, "d", None)
            .expect("create_database")
    });
    let writer = txn.begin(IsolationLevel::ReadCommitted);
    catalog.drop_database(&writer, "d").expect("drop_database");
    let reader = txn.begin(IsolationLevel::ReadCommitted);
    // For the reader, `d` is still there: a create of that name is refused with 1801.
    assert_eq!(
        catalog
            .create_database(&reader, "d", None)
            .expect_err("`d` is still there for the reader")
            .number,
        1801
    );
    // For the writer, the row is gone: a second drop of that name answers 3701. The name is
    // not free for it either — `storage` carries `d` until the commit runs the deferred drop
    // — so its own create answers 1801 (unit test
    // `a_create_after_a_drop_of_the_same_name_in_one_transaction_is_1801`).
    assert_eq!(
        catalog
            .drop_database(&writer, "d")
            .expect_err("the row is gone for the writer that deleted it")
            .number,
        3701
    );
    assert_eq!(
        catalog
            .create_database(&writer, "d", None)
            .expect_err("`storage` carries `d` until the commit")
            .number,
        1801
    );
    txn.rollback(writer).expect("rollback");
    txn.rollback(reader).expect("rollback");
}
