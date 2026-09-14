//! Savepoints and deferred DDL over a `MemoryStorage`: a `CREATE` compensated at
//! rollback, a `DROP` deferred to the commit, and what a `rollback_to` a savepoint undoes,
//! runs and forgets.
//!
//! Tables are built with the helpers of `vauban_storage::testsuite`, one nullable `int`
//! column each; the identifiers in the assertions are the small integers `storage` and
//! `TransactionManager::begin` hand out.
//!
//! # How the order of the actions is told apart
//!
//! Dropping a table drops its indexes (documentation of `Storage::drop_table`), so a
//! `drop_index` after the `drop_table` of the same table is a caller bug. Each of the two
//! order scenarios therefore registers its pair both ways: the order under test succeeds and
//! the opposite one fails with number 50000 — an `is_ok()` on one order alone would not tell
//! them apart.

use std::sync::Arc;

use vauban_errors::SqlError;
use vauban_storage::testsuite::{collect, index_shape, int_table_shape, row};
use vauban_storage::{DbId, IndexId, MemoryStorage, Row, SavepointId, Storage, TableId, TxnId};
use vauban_txn::{CommitAction, IsolationLevel, RollbackAction, TransactionManager, TxnHandle};

/// A storage with one empty database, and a manager over it.
struct Fixture {
    storage: Arc<dyn Storage>,
    mgr: TransactionManager,
    db: DbId,
}

fn fixture() -> Fixture {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage.create_database("txn").expect("create_database");
    let mgr = TransactionManager::new(Arc::clone(&storage));
    Fixture { storage, mgr, db }
}

impl Fixture {
    /// A new table of one `int` column in the database of the fixture.
    fn create_table(&self) -> TableId {
        self.storage
            .create_table(self.db, &int_table_shape(1))
            .expect("create_table")
    }

    /// A new index on the first column of `table`.
    fn create_index(&self, table: TableId) -> IndexId {
        self.storage
            .create_index(table, &index_shape(&[(0, true)], false))
            .expect("create_index")
    }

    /// The tables the database holds now, by identifier. `storage` applies DDL at once, so
    /// this reads what the deferred actions have done, not what a snapshot shows.
    fn table_ids(&self) -> Vec<TableId> {
        self.storage
            .tables(self.db)
            .expect("tables")
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// The rows of `table` visible to `txn` right now, in the order `scan` yields them.
    fn rows(&self, txn: &TxnHandle, table: TableId) -> Vec<Row> {
        let snap = self.mgr.statement_snapshot(txn);
        let iter = self.storage.scan(&snap, table).expect("scan");
        collect(iter).into_iter().map(|(_, r)| r).collect()
    }

    /// A transaction that inserts `values` into `table` and commits.
    fn seed(&self, table: TableId, values: &[i32]) {
        let txn = self.mgr.begin(IsolationLevel::ReadCommitted);
        for v in values {
            self.storage
                .insert(txn.id, table, &row(&[*v]))
                .expect("insert");
        }
        self.mgr.commit(txn).expect("commit the seeder");
    }
}

/// An internal bug reaches the client as number 50000, severity 16 (crate `errors`).
fn assert_bug(err: &SqlError, expected: &str) {
    assert_eq!(err.number, 50000, "number");
    assert_eq!(err.severity, 16, "severity");
    assert!(
        err.message.contains(expected),
        "message was {:?}",
        err.message
    );
}

// ------------------------------------------------- Compensation of a CREATE

#[test]
fn create_compensated_on_rollback() {
    let f = fixture();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    let table = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(table))
        .expect("register the compensation");
    let id = f
        .storage
        .insert(txn.id, table, &row(&[1]))
        .expect("insert a row in the new table");
    assert_eq!(f.table_ids(), vec![table], "the table exists at once");

    f.mgr.rollback(txn).expect("rollback");
    assert_eq!(f.table_ids(), Vec::new(), "the compensation dropped it");

    let reader = f.mgr.begin(IsolationLevel::ReadCommitted);
    let snap = f.mgr.statement_snapshot(&reader);
    let err = f
        .storage
        .get(&snap, table, id)
        .expect_err("the table is unknown to storage now");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);
}

#[test]
fn a_committed_compensation_leaves_the_table() {
    let f = fixture();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    let table = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(table))
        .expect("register the compensation");
    f.mgr.commit(txn).expect("commit");
    assert_eq!(
        f.table_ids(),
        vec![table],
        "a commit does not run the compensations"
    );
}

// ----------------------------------------------------- Deferral of a DROP

#[test]
fn drop_deferred_until_commit() {
    let f = fixture();
    let table = f.create_table();
    f.seed(table, &[10, 20]);

    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    f.mgr
        .register_on_commit(&txn, CommitAction::DropTable(table))
        .expect("register the deferred drop");
    assert_eq!(
        f.table_ids(),
        vec![table],
        "registering does not call storage.drop_table"
    );
    let seen = f.rows(&txn, table);
    assert_eq!(seen.len(), 2, "the rows are still readable");
    assert!(seen.contains(&row(&[10])) && seen.contains(&row(&[20])));

    f.mgr.commit(txn).expect("commit");
    assert_eq!(f.table_ids(), Vec::new(), "the commit ran the drop");
}

#[test]
fn a_rolled_back_deferred_drop_keeps_the_table() {
    let f = fixture();
    let table = f.create_table();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    f.mgr
        .register_on_commit(&txn, CommitAction::DropTable(table))
        .expect("register the deferred drop");
    f.mgr.rollback(txn).expect("rollback");
    assert_eq!(
        f.table_ids(),
        vec![table],
        "a rollback does not run the deferred drops"
    );
}

#[test]
fn indexes_and_databases_are_deferred_and_compensated_too() {
    let f = fixture();
    let table = f.create_table();
    let index = f.create_index(table);

    let dropper = f.mgr.begin(IsolationLevel::ReadCommitted);
    f.mgr
        .register_on_commit(&dropper, CommitAction::DropIndex(index))
        .expect("register");
    assert_eq!(f.storage.indexes(table).expect("indexes").len(), 1);
    f.mgr.commit(dropper).expect("commit");
    assert_eq!(f.storage.indexes(table).expect("indexes").len(), 0);
    assert_eq!(f.table_ids(), vec![table], "the table itself stays");

    let creator = f.mgr.begin(IsolationLevel::ReadCommitted);
    let second = f
        .storage
        .create_database("second")
        .expect("create_database");
    f.mgr
        .register_on_rollback(&creator, RollbackAction::DropDatabase(second))
        .expect("register");
    f.mgr.rollback(creator).expect("rollback");
    let names: Vec<String> = f
        .storage
        .databases()
        .expect("databases")
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert_eq!(names, vec!["txn".to_owned()], "the second one was dropped");
}

// ------------------------------------------------------------ Order of the actions

#[test]
fn commit_actions_run_in_registration_order() {
    // The index is registered first, so it is dropped before the table that holds it.
    let f = fixture();
    let table = f.create_table();
    let index = f.create_index(table);
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    f.mgr
        .register_on_commit(&txn, CommitAction::DropIndex(index))
        .expect("register the index drop first");
    f.mgr
        .register_on_commit(&txn, CommitAction::DropTable(table))
        .expect("register the table drop second");
    f.mgr.commit(txn).expect("both drops run, in that order");
    assert_eq!(f.table_ids(), Vec::new());

    // Counter-test: the opposite registration order drops the table first, and the index
    // it took away is then unknown.
    let g = fixture();
    let table = g.create_table();
    let index = g.create_index(table);
    let txn = g.mgr.begin(IsolationLevel::ReadCommitted);
    g.mgr
        .register_on_commit(&txn, CommitAction::DropTable(table))
        .expect("register the table drop first");
    g.mgr
        .register_on_commit(&txn, CommitAction::DropIndex(index))
        .expect("register the index drop second");
    let err = g
        .mgr
        .commit(txn.clone())
        .expect_err("the second drop names an index storage no longer knows");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);
    let open: Vec<TxnId> = g.mgr.active_sessions().iter().map(|i| i.id).collect();
    assert_eq!(open, vec![txn.id], "a refused commit closes nothing");
}

#[test]
fn rollback_actions_run_in_reverse_order() {
    // Registered as the objects are created: table first, index second. Undoing them last
    // registered first drops the index before its table.
    let f = fixture();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    let table = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(table))
        .expect("register the table compensation first");
    let index = f.create_index(table);
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropIndex(index))
        .expect("register the index compensation second");
    f.mgr
        .rollback(txn)
        .expect("the index compensation runs before the table one");
    assert_eq!(f.table_ids(), Vec::new());

    // Counter-test: registered the other way round, the reverse run drops the table first
    // and the index compensation then names an unknown index.
    let g = fixture();
    let txn = g.mgr.begin(IsolationLevel::ReadCommitted);
    let table = g.create_table();
    let index = g.create_index(table);
    g.mgr
        .register_on_rollback(&txn, RollbackAction::DropIndex(index))
        .expect("register the index compensation first");
    g.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(table))
        .expect("register the table compensation second");
    let err = g
        .mgr
        .rollback(txn.clone())
        .expect_err("the second compensation names an index storage no longer knows");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);
    let open: Vec<TxnId> = g.mgr.active_sessions().iter().map(|i| i.id).collect();
    assert_eq!(open, vec![txn.id], "a refused rollback closes nothing");
}

// ------------------------------------------------------------------- Savepoints

#[test]
fn savepoint_undoes_later_creates() {
    let f = fixture();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    // Two tables before the savepoint, so that a `rollback_to` compensating from the start
    // of the log instead of from the mark drops one of them and fails this test.
    let before = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(before))
        .expect("compensate the first create");
    let also_before = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(also_before))
        .expect("compensate the second create");
    f.storage
        .insert(txn.id, before, &row(&[1]))
        .expect("a row written before the savepoint");

    let sp = f.mgr.savepoint(&txn).expect("savepoint");

    let after = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(after))
        .expect("compensate the third create");
    f.storage
        .insert(txn.id, before, &row(&[2]))
        .expect("a row written after the savepoint");
    assert_eq!(f.table_ids(), vec![before, also_before, after]);

    f.mgr.rollback_to(&txn, sp).expect("rollback to sp");
    assert_eq!(
        f.table_ids(),
        vec![before, also_before],
        "the table created after the savepoint is gone, those created before stay"
    );
    assert_eq!(
        f.rows(&txn, before),
        vec![row(&[1])],
        "storage.rollback_to undid the row written after the savepoint"
    );

    // The compensation registered before the savepoint is still armed.
    f.mgr.rollback(txn).expect("rollback the whole thing");
    assert_eq!(f.table_ids(), Vec::new());
}

#[test]
fn nested_savepoints_are_ordered() {
    let f = fixture();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    let first = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(first))
        .expect("compensate the first create");

    let sp1 = f.mgr.savepoint(&txn).expect("first savepoint");
    let middle = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(middle))
        .expect("compensate the second create");

    let sp2 = f.mgr.savepoint(&txn).expect("second savepoint");
    assert!(sp1 < sp2, "savepoint ids increase within a transaction");
    let last = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(last))
        .expect("compensate the third create");
    assert_eq!(f.table_ids(), vec![first, middle, last]);

    f.mgr.rollback_to(&txn, sp2).expect("rollback to sp2");
    assert_eq!(
        f.table_ids(),
        vec![first, middle],
        "the compensations registered before sp2 did not run"
    );
    f.mgr.rollback_to(&txn, sp2).expect("sp2 is still usable");
    assert_eq!(f.table_ids(), vec![first, middle]);

    f.mgr.rollback_to(&txn, sp1).expect("rollback to sp1");
    assert_eq!(f.table_ids(), vec![first]);
    let err = f
        .mgr
        .rollback_to(&txn, sp2)
        .expect_err("sp2 was invalidated by the rollback to sp1");
    assert_bug(
        &err,
        &format!("TransactionManager::rollback_to: savepoint {sp2} is not held by transaction 1"),
    );
}

#[test]
fn a_savepoint_forgets_the_deferred_drops_registered_after_it() {
    let f = fixture();
    let table = f.create_table();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    let sp = f.mgr.savepoint(&txn).expect("savepoint");
    f.mgr
        .register_on_commit(&txn, CommitAction::DropTable(table))
        .expect("defer the drop after the savepoint");
    f.mgr.rollback_to(&txn, sp).expect("rollback to sp");
    f.mgr.commit(txn).expect("commit");
    assert_eq!(
        f.table_ids(),
        vec![table],
        "the deferred drop was forgotten with the savepoint"
    );
}

// ------------------------------------------- What a failure in the middle leaves behind

/// `rollback_to` truncates the log **before** calling
/// `Storage::rollback_to`, so an error from `storage` leaves the registrations made after the
/// mark already run and already forgotten.
#[test]
fn a_refused_storage_rollback_to_leaves_the_log_truncated() {
    let f = fixture();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    let kept = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(kept))
        .expect("compensate the create made before the savepoint");
    let sp = f.mgr.savepoint(&txn).expect("savepoint");
    let later = f.create_table();
    f.mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(later))
        .expect("compensate the create made after the savepoint");

    // Out of band, the transaction is finished in `storage`: its `rollback_to` refuses.
    f.storage.rollback(txn.id).expect("rollback out of band");
    let err = f
        .mgr
        .rollback_to(&txn, sp)
        .expect_err("storage refuses the rollback_to");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);
    assert_eq!(
        f.table_ids(),
        vec![kept],
        "the compensation had already run when storage refused"
    );

    let again = f
        .mgr
        .rollback_to(&txn, sp)
        .expect_err("the same call, once more");
    assert_eq!(
        again.message, err.message,
        "the same error, so no compensation was replayed"
    );
    assert_eq!(f.table_ids(), vec![kept], "and nothing else was dropped");
}

/// When a compensation fails in the middle of the reverse run, the ones
/// already run stay run. A second `rollback` replays them and fails again, so the
/// transaction is closed by a `commit` — which leaves the objects created behind.
#[test]
fn a_failed_compensation_leaves_the_transaction_to_a_commit() {
    let f = fixture();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    let first = f.create_table();
    let second = f.create_table();
    let third = f.create_table();
    for table in [first, second, third] {
        f.mgr
            .register_on_rollback(&txn, RollbackAction::DropTable(table))
            .expect("compensate each create");
    }
    // The middle table disappears out of band: its compensation will fail.
    f.storage.drop_table(second).expect("drop out of band");

    let err = f
        .mgr
        .rollback(txn.clone())
        .expect_err("the compensation of the second table fails");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);
    assert_eq!(
        f.table_ids(),
        vec![first],
        "the third compensation ran before the failure, the first did not run"
    );
    let open: Vec<TxnId> = f.mgr.active_sessions().iter().map(|i| i.id).collect();
    assert_eq!(open, vec![txn.id], "the transaction stays open");

    let again = f
        .mgr
        .rollback(txn.clone())
        .expect_err("a second rollback replays the compensation of the third table");
    assert_eq!(again.number, 50000, "message was {:?}", again.message);
    assert_eq!(f.table_ids(), vec![first]);

    f.mgr.commit(txn).expect("a commit closes the transaction");
    assert_eq!(f.mgr.active_sessions(), Vec::new());
    assert_eq!(
        f.table_ids(),
        vec![first],
        "the table created by the transaction is left behind"
    );
}

// ------------------------------------------------------------- Caller bugs

#[test]
fn rollback_to_an_unknown_savepoint_is_a_bug() {
    let f = fixture();
    let a = f.mgr.begin(IsolationLevel::ReadCommitted);
    let b = f.mgr.begin(IsolationLevel::ReadCommitted);
    let sp_b = f.mgr.savepoint(&b).expect("savepoint of B");

    let err = f
        .mgr
        .rollback_to(&a, sp_b)
        .expect_err("a savepoint of another transaction");
    assert_bug(
        &err,
        &format!("TransactionManager::rollback_to: savepoint {sp_b} is not held by transaction 1"),
    );
    let err = f
        .mgr
        .rollback_to(&a, SavepointId(999))
        .expect_err("a made-up savepoint");
    assert_bug(&err, "savepoint 999 is not held by transaction 1");
    f.mgr.rollback_to(&b, sp_b).expect("B still holds its own");
}

#[test]
fn a_deferred_drop_of_a_gone_table_is_a_bug() {
    let f = fixture();
    let table = f.create_table();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    f.mgr
        .register_on_commit(&txn, CommitAction::DropTable(table))
        .expect("register the deferred drop");
    // Something else dropped the table in the meantime: the precondition of the action is
    // broken and the commit reports a caller bug.
    f.storage.drop_table(table).expect("drop_table out of band");
    let err = f
        .mgr
        .commit(txn.clone())
        .expect_err("the id is unknown now");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);
    let open: Vec<TxnId> = f.mgr.active_sessions().iter().map(|i| i.id).collect();
    assert_eq!(open, vec![txn.id], "the transaction stays open");
}

#[test]
fn using_a_closed_transaction_is_a_bug() {
    let f = fixture();
    let table = f.create_table();
    let txn = f.mgr.begin(IsolationLevel::ReadCommitted);
    f.mgr.commit(txn.clone()).expect("commit");

    let err = f
        .mgr
        .register_on_commit(&txn, CommitAction::DropTable(table))
        .expect_err("register_on_commit after the commit");
    assert_bug(
        &err,
        "TransactionManager::register_on_commit: transaction 1 is not open",
    );
    let err = f
        .mgr
        .register_on_rollback(&txn, RollbackAction::DropTable(table))
        .expect_err("register_on_rollback after the commit");
    assert_bug(
        &err,
        "TransactionManager::register_on_rollback: transaction 1 is not open",
    );
    let err = f
        .mgr
        .savepoint(&txn)
        .expect_err("savepoint after the commit");
    assert_bug(
        &err,
        "TransactionManager::savepoint: transaction 1 is not open",
    );
    let err = f
        .mgr
        .rollback_to(&txn, SavepointId(1))
        .expect_err("rollback_to after the commit");
    assert_bug(
        &err,
        "TransactionManager::rollback_to: transaction 1 is not open",
    );
    assert_eq!(f.table_ids(), vec![table], "nothing was dropped");
}
