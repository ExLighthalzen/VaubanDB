use vauban_types::{SqlType, TypeInfo, Value};

use super::super::temp::TempDir;
use super::super::version::HeapTable;
use super::super::{DiskOptions, alloc};
use super::*;
use crate::{Row, Snapshot, TableShape, TxnStatus};

/// The table the tests of this file work on.
const TABLE: TableId = TableId(7);

/// The instance in `dir`, created or reopened.
fn open(dir: &TempDir) -> DiskStorage {
    DiskStorage::open(dir.path(), DiskOptions::default()).expect("open the instance")
}

/// A table of one nullable `int`.
fn int_shape() -> TableShape {
    TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: None,
    }
}

/// A row of one `int`.
fn row(value: i32) -> Row {
    Row(vec![Value::I32(value)])
}

/// A snapshot whose `xmin` and `xmax` are both `xmax`, with an empty list of running
/// transactions.
fn snap(own: u64, xmax: u64) -> Snapshot {
    Snapshot {
        xmin: TxnId(xmax),
        xmax: TxnId(xmax),
        active: Vec::new(),
        own: TxnId(own),
    }
}

/// Creates the heap of [`TABLE`] in a fresh instance and tells the recovery about it.
fn install(storage: &DiskStorage) -> PageId {
    let head = alloc::allocate(storage).expect("allocate the head page");
    let table = HeapTable::create(storage, TABLE, head, int_shape()).expect("create the table");
    drop(table);
    install_single_heap(storage, TABLE, head, int_shape()).expect("install the heap");
    head
}

/// The table of a reopened instance, its counters and its register taken from the recovery.
fn resumed(storage: &DiskStorage) -> HeapTable<'_> {
    let recovery = storage.recovery();
    let head = recovery
        .heap_of(TABLE)
        .expect("the journal names the heap of the table");
    let mut table = HeapTable::open(storage, TABLE, head, int_shape());
    table
        .resume_after_recovery(
            recovery.next_row_id,
            recovery.next_seq,
            &recovery.winners,
            &recovery.losers,
        )
        .expect("resume the table");
    table
}

#[test]
fn reopen_after_commit_without_checkpoint() {
    let dir = TempDir::created("recover-commit");
    let id = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let id = table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        // No checkpoint: the page of the insert is in the pool, which this drop throws
        // away, and the row is in the journal.
        id
    };

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4)]);
    assert_eq!(recovery.losers, Vec::new());
    assert_eq!((recovery.applied, recovery.skipped), (1, 0));
    let table = resumed(&storage);
    assert_eq!(table.get(&snap(9, 100), id).expect("get"), Some(row(11)));
    assert_eq!(
        table.scan(&snap(9, 100)).expect("scan"),
        vec![(id, row(11))]
    );
}

#[test]
fn uncommitted_insert_gone_after_reopen() {
    let dir = TempDir::created("recover-loser");
    let id = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        table.insert(TxnId(4), &row(11)).expect("insert")
    };
    assert_eq!(id, RowId(1));

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, Vec::new());
    assert_eq!(recovery.losers, vec![TxnId(4)]);
    assert_eq!((recovery.applied, recovery.skipped), (0, 0));
    let mut table = resumed(&storage);
    assert_eq!(table.get(&snap(9, 100), id).expect("get"), None);
    assert_eq!(table.scan(&snap(9, 100)).expect("scan"), Vec::new());
    // The identifier the interrupted insert took is not handed out again.
    let again = table.insert(TxnId(6), &row(12)).expect("insert");
    assert_eq!(again, RowId(2));
}

#[test]
fn a_row_id_taken_by_a_loser_is_not_handed_out_again() {
    let dir = TempDir::created("recover-row-id");
    {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        table.insert(TxnId(5), &row(12)).expect("insert");
        table.rollback(TxnId(5)).expect("rollback");
        table.insert(TxnId(6), &row(13)).expect("insert");
    }

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    // Three inserts, one committed: the counter is past the three of them.
    assert_eq!(recovery.next_row_id, 4);
    assert_eq!(recovery.next_seq, 4);
    assert_eq!(
        storage.control().expect("control block").next_row_id,
        4,
        "the counter is persisted in vauban.ctl"
    );
    let mut table = resumed(&storage);
    assert_eq!(table.insert(TxnId(7), &row(14)).expect("insert"), RowId(4));
}

#[test]
fn checkpoint_then_commit_then_reopen() {
    let dir = TempDir::created("recover-checkpoint");
    let (first, second) = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let first = table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        storage.checkpoint().expect("checkpoint");
        let second = table.insert(TxnId(5), &row(12)).expect("insert");
        table.commit(TxnId(5)).expect("commit");
        (first, second)
    };

    let storage = open(&dir);
    let checkpoint = storage
        .control()
        .expect("control block")
        .latest_checkpoint_lsn;
    assert!(checkpoint.0 > 0, "the checkpoint reached vauban.ctl");
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4), TxnId(5)]);
    // The redo starts at the first record, so the insert of the first transaction is
    // replayed too; the heap already holding it, that record is skipped.
    assert_eq!(recovery.applied + recovery.skipped, 2);
    assert_eq!(
        recovery.skipped, 1,
        "the checkpointed insert is already there"
    );
    let table = resumed(&storage);
    assert_eq!(
        table.scan(&snap(9, 100)).expect("scan"),
        vec![(first, row(11)), (second, row(12))]
    );
}

#[test]
fn txn_straddling_checkpoint_is_redone() {
    let dir = TempDir::created("recover-straddle");
    let id = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let id = table.insert(TxnId(4), &row(11)).expect("insert");
        // The checkpoint runs while the transaction of the insert is in progress: its page
        // is held back, and the commit that follows syncs the journal, not `data`.
        storage.checkpoint().expect("checkpoint");
        table.commit(TxnId(4)).expect("commit");
        id
    };

    let storage = open(&dir);
    let checkpoint = storage
        .control()
        .expect("control block")
        .latest_checkpoint_lsn;
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4)]);
    assert_eq!(
        (recovery.applied, recovery.skipped),
        (1, 0),
        "the insert sits before checkpoint {checkpoint} and is redone"
    );
    let table = resumed(&storage);
    assert_eq!(table.get(&snap(9, 100), id).expect("get"), Some(row(11)));
}

#[test]
fn abort_is_not_redone() {
    let dir = TempDir::created("recover-abort");
    let id = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let id = table.insert(TxnId(4), &row(11)).expect("insert");
        table.rollback(TxnId(4)).expect("rollback");
        // The `Abort` is in the journal, and a checkpoint puts the pages of the undo on
        // `data`: what the recovery sees is a heap without the version.
        storage.checkpoint().expect("checkpoint");
        id
    };

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, Vec::new());
    assert_eq!(recovery.losers, vec![TxnId(4)]);
    assert_eq!((recovery.applied, recovery.skipped), (0, 0));
    let table = resumed(&storage);
    assert_eq!(table.get(&snap(9, 100), id).expect("get"), None);
    assert_eq!(table.status(TxnId(4)), TxnStatus::Aborted);
}

#[test]
fn double_open_is_idempotent() {
    let dir = TempDir::created("recover-twice");
    let id = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let id = table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        id
    };

    let first = open(&dir);
    assert_eq!(first.recovery().applied, 1);
    drop(first);

    let second = open(&dir);
    let recovery = second.recovery().clone();
    // The pool of the first reopen was dropped without a checkpoint, so the redo writes
    // again; what it writes is one version, not two.
    assert_eq!(recovery.applied + recovery.skipped, 1);
    let table = resumed(&second);
    assert_eq!(
        table.scan(&snap(9, 100)).expect("scan"),
        vec![(id, row(11))]
    );
}

#[test]
fn a_checkpoint_between_two_opens_turns_the_redo_into_a_skip() {
    let dir = TempDir::created("recover-twice-checkpoint");
    {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
    }
    {
        let storage = open(&dir);
        assert_eq!(storage.recovery().applied, 1);
        storage.checkpoint().expect("checkpoint");
    }

    let storage = open(&dir);
    // The version reached `data`, so the second redo finds it under its `(RowId, seq)`.
    assert_eq!(
        (storage.recovery().applied, storage.recovery().skipped),
        (0, 1)
    );
    assert_eq!(
        resumed(&storage).scan(&snap(9, 100)).expect("scan"),
        vec![(RowId(1), row(11))]
    );
}

#[test]
fn an_update_and_a_delete_of_a_winner_are_redone() {
    let dir = TempDir::created("recover-update");
    let (kept, gone) = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let kept = table.insert(TxnId(4), &row(11)).expect("insert");
        let gone = table.insert(TxnId(4), &row(12)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        table.update(TxnId(5), kept, &row(21)).expect("update");
        table.delete(TxnId(5), gone).expect("delete");
        table.commit(TxnId(5)).expect("commit");
        (kept, gone)
    };

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4), TxnId(5)]);
    // Two inserts, one update and one delete.
    assert_eq!((recovery.applied, recovery.skipped), (4, 0));
    let mut table = resumed(&storage);
    let now = snap(9, 100);
    assert_eq!(table.get(&now, kept).expect("get"), Some(row(21)));
    assert_eq!(table.get(&now, gone).expect("get"), None);
    // A snapshot that has not settled the second transaction reads what it replaced.
    let before = Snapshot {
        xmin: TxnId(5),
        xmax: TxnId(5),
        active: vec![TxnId(5)],
        own: TxnId(9),
    };
    assert_eq!(table.get(&before, kept).expect("get"), Some(row(11)));
    assert_eq!(table.get(&before, gone).expect("get"), Some(row(12)));
    // The directory is rebuilt, so a write on a row of the rebuilt table goes through.
    table.update(TxnId(7), kept, &row(31)).expect("update");
    table.commit(TxnId(7)).expect("commit");
    assert_eq!(table.get(&snap(9, 100), kept).expect("get"), Some(row(31)));
}

#[test]
fn a_winner_is_registered_committed_and_may_not_write_again() {
    let dir = TempDir::created("recover-winner-register");
    {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
    }

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4)]);
    let mut table = resumed(&storage);
    assert_eq!(table.status(TxnId(4)), TxnStatus::Committed);
    // The identifier of a transaction the journal ended is refused as a writer, as it is
    // on a table that did not close: `HeapTable::check_writable` reads the register.
    let err = table
        .insert(TxnId(4), &row(12))
        .expect_err("the identifier of a winner is refused");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("already Committed"), "{err}");

    // What distinguishes "refused because the journal ended it" from "refused for some
    // other reason":
    // an identifier the journal does not name writes on the same table, through the same
    // call. The register is per instance, so attaching a second `HeapTable` does not
    // escape it.
    let head = recovery.heap_of(TABLE).expect("the journal names the heap");
    let mut same_heap = HeapTable::open(&storage, TABLE, head, int_shape());
    assert!(
        same_heap.insert(TxnId(4), &row(12)).is_err(),
        "a second table of the same instance reads the same register"
    );
    assert_eq!(
        same_heap.insert(TxnId(5), &row(12)).expect("insert"),
        RowId(1)
    );
}

#[test]
fn a_loser_whose_page_reached_data_is_invisible() {
    let dir = TempDir::created("recover-shared-page");
    let (committed, hidden) = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let committed = table.insert(TxnId(4), &row(11)).expect("insert");
        // The second transaction writes the same page; the commit of the first clears the
        // `in_progress` flag of that page, after which the checkpoint writes it to `data`
        // with the version of the second one on it.
        let hidden = table.insert(TxnId(5), &row(12)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        storage.checkpoint().expect("checkpoint");
        (committed, hidden)
    };

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4)]);
    assert_eq!(recovery.losers, vec![TxnId(5)]);
    // The version of the interrupted transaction did reach `data`: a table attached
    // without the register of the recovery takes its writer for a committed one — the rule
    // of `HeapTable::status` for a transaction it has not seen — and serves the row. This
    // is what tells a page that was flushed apart from one that stayed in the pool.
    let head = recovery.heap_of(TABLE).expect("the journal names the heap");
    let unregistered = HeapTable::open(&storage, TABLE, head, int_shape());
    assert_eq!(
        unregistered.scan(&snap(9, 100)).expect("scan"),
        vec![(committed, row(11)), (hidden, row(12))]
    );
    drop(unregistered);

    let table = resumed(&storage);
    assert_eq!(table.status(TxnId(5)), TxnStatus::Aborted);
    assert_eq!(
        table.scan(&snap(9, 100)).expect("scan"),
        vec![(committed, row(11))],
        "the version of the interrupted transaction is on the heap and out of the scan"
    );
    assert_eq!(table.get(&snap(9, 100), hidden).expect("get"), None);
}

#[test]
fn the_redo_appends_no_record() {
    let dir = TempDir::created("recover-no-record");
    let before = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        table.insert(TxnId(4), &row(11)).expect("insert");
        table.update(TxnId(4), RowId(1), &row(12)).expect("update");
        table.commit(TxnId(4)).expect("commit");
        storage.wal.records().expect("read the journal").len()
    };

    let storage = open(&dir);
    assert_eq!(storage.recovery().applied, 2);
    // The redo writes through the heap, which appends nothing: a replay that journalled
    // its own writes would grow the file at each open.
    assert_eq!(
        storage.wal.records().expect("read the journal").len(),
        before
    );
    assert_eq!(
        resumed(&storage).scan(&snap(9, 100)).expect("scan"),
        vec![(RowId(1), row(12))]
    );
}

#[test]
fn a_begin_alone_changes_nothing() {
    let dir = TempDir::created("recover-begin");
    {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        table.insert(TxnId(4), &row(11)).expect("insert");
        // The undo of the rollback takes the version away; its `Begin` and its `Abort`
        // stay in the journal, and the checkpoint puts the empty heap page on `data`.
        table.rollback(TxnId(4)).expect("rollback");
        storage.checkpoint().expect("checkpoint");
    }

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(
        (recovery.applied, recovery.skipped, recovery.unplaced),
        (0, 0, 0)
    );
    assert_eq!(
        resumed(&storage).scan(&snap(9, 100)).expect("scan"),
        Vec::new()
    );
}

#[test]
fn a_row_record_without_its_table_is_counted_unplaced() {
    let dir = TempDir::created("recover-unplaced");
    {
        let storage = open(&dir);
        // No `install_single_heap`: the journal names no heap for the table.
        let head = alloc::allocate(&storage).expect("allocate the head page");
        let mut table =
            HeapTable::create(&storage, TABLE, head, int_shape()).expect("create the table");
        table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
    }

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert!(recovery.tables.is_empty(), "{:?}", recovery.tables);
    assert_eq!((recovery.applied, recovery.unplaced), (0, 1));
    // The counter of the journal is still read: the insert took `RowId(1)`.
    assert_eq!(recovery.next_row_id, 2);
}

#[test]
fn an_empty_instance_recovers_to_the_default() {
    let dir = TempDir::reserved("recover-empty");
    let storage = open(&dir);
    assert_eq!(*storage.recovery(), Recovery::default());
}

#[test]
fn a_create_table_record_of_a_dropped_table_leaves_no_heap() {
    let dir = TempDir::created("recover-dropped");
    let head = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        storage
            .wal
            .append_durable(
                WalRecordKind::DropTable,
                TxnId(0),
                &super::super::meta::identifier_payload(TABLE.0),
            )
            .expect("journal the drop");
        head
    };

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert!(recovery.tables.is_empty(), "{:?}", recovery.tables);
    assert_eq!((recovery.applied, recovery.unplaced), (0, 1));
    // The heap of the dropped table is not read: its head page is left as it stands.
    assert!(recovery.heap_of(TABLE).is_none());
    assert_eq!(head, PageId(0));
}
