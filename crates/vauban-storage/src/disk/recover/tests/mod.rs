use vauban_types::{SqlString, SqlType, TypeInfo, Value};

use super::super::page::Page;
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
    // read too; its page reached `data` carrying its number, so that record is skipped.
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
    // The page reached `data` carrying the number of the insert, so the second redo skips it.
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

/// A loser at the largest identifier of the journal, a reopen, a new transaction that takes
/// the identifier the reopened instance hands out and commits on a second table, a second
/// reopen: the write of the loser stays out.
#[test]
fn the_identifier_of_a_loser_is_not_handed_out_again() {
    const OTHER: TableId = TableId(8);
    let dir = TempDir::created("recover-loser-txn-id");
    {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        // The crash comes before the end of transaction 5.
        table.insert(TxnId(5), &row(12)).expect("insert");
    }

    let next = {
        let storage = open(&dir);
        let recovery = storage.recovery().clone();
        assert_eq!(recovery.winners, vec![TxnId(4)]);
        assert_eq!(recovery.losers, vec![TxnId(5)]);
        let next = storage.next_txn_id();
        assert_eq!(next, 6, "one past the loser, not one past the winner");
        assert_eq!(crate::Storage::committed_txn_high_water(&storage), 5);
        // The new transaction writes on a table of its own, so that no resume of `TABLE`
        // registers transaction 5 before it.
        let head = alloc::allocate(&storage).expect("allocate the head page");
        let mut other = HeapTable::create(&storage, OTHER, head, int_shape()).expect("create");
        install_single_heap(&storage, OTHER, head, int_shape()).expect("install the heap");
        other.insert(TxnId(next), &row(21)).expect("insert");
        other.commit(TxnId(next)).expect("commit");
        next
    };

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4), TxnId(next)]);
    assert_eq!(recovery.losers, vec![TxnId(5)]);
    assert_eq!(storage.next_txn_id(), next + 1);
    let table = resumed(&storage);
    assert_eq!(table.status(TxnId(5)), TxnStatus::Aborted);
    assert_eq!(
        table.scan(&snap(99, 100)).expect("scan"),
        vec![(RowId(1), row(11))],
        "the insert of the loser is not replayed"
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

/// The bytes of a page but its checksum field, which a write to `data` fills in and a change in
/// the pool leaves stale.
fn body(page: &Page) -> Vec<u8> {
    let mut bytes = page.0.to_vec();
    bytes[24..28].fill(0);
    bytes
}

/// The heap pages of [`TABLE`] that hold a record, each as the pool holds it now.
fn pool_pages(storage: &DiskStorage, head: PageId) -> BTreeMap<PageId, Vec<u8>> {
    let heap = Heap::open(storage, TABLE, head);
    let mut pages: BTreeSet<PageId> = heap
        .scan()
        .expect("scan the heap")
        .into_iter()
        .map(|rid| rid.page)
        .collect();
    pages.insert(head);
    pages
        .into_iter()
        .map(|id| {
            let pin = storage.pool.pin(id).expect("pin the page");
            (id, pin.with_page(body).expect("read the page"))
        })
        .collect()
}

/// Inserts `count` rows of `txn` and commits them, answering their identifiers.
fn insert_rows(table: &mut HeapTable<'_>, txn: TxnId, count: i32) -> Vec<RowId> {
    let ids = (0..count)
        .map(|value| table.insert(txn, &row(value)).expect("insert"))
        .collect();
    table.commit(txn).expect("commit");
    ids
}

#[test]
fn a_page_on_data_before_the_crash_is_left_as_it_stands_by_the_redo() {
    let dir = TempDir::created("recover-page-on-data");
    let (head, on_data, kept) = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let gone = table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        table.delete(TxnId(5), gone).expect("delete");
        table.commit(TxnId(5)).expect("commit");
        let kept = table.insert(TxnId(6), &row(12)).expect("insert");
        table.commit(TxnId(6)).expect("commit");
        // The vacuum takes the deleted version out of the page, which the checkpoint writes:
        // `data` holds a page newer than the insert and the delete of the journal.
        assert_eq!(table.vacuum(TxnId(10)).expect("vacuum").len(), 1);
        storage.checkpoint().expect("checkpoint");
        let on_data = storage.data.read_page(head).expect("read the page on data");
        (head, on_data, kept)
    };

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4), TxnId(5), TxnId(6)]);
    assert_eq!((recovery.applied, recovery.skipped), (0, 3));
    let pin = storage.pool.pin(head).expect("pin the head page");
    assert_eq!(
        pin.with_page(body).expect("read the page"),
        body(&on_data),
        "the redo does not touch a page whose number covers the records"
    );
    drop(pin);
    let table = resumed(&storage);
    // The vacuumed version is not put back by the insert record that created it.
    let versions = table.versions(None).expect("versions");
    assert_eq!(versions.len(), 1, "{versions:?}");
    assert_eq!(
        table.scan(&snap(9, 100)).expect("scan"),
        vec![(kept, row(12))]
    );
}

#[test]
fn records_behind_the_page_on_data_are_skipped_and_the_ones_past_it_applied() {
    let dir = TempDir::created("recover-page-between");
    let (first, second) = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let first = table.insert(TxnId(4), &row(11)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        // The page reaches `data` between the two writes: the crash comes after the write of
        // the page for the first record and before it for the second.
        storage.pool.flush(head).expect("flush the head page");
        let second = table.insert(TxnId(5), &row(12)).expect("insert");
        table.update(TxnId(5), first, &row(13)).expect("update");
        table.commit(TxnId(5)).expect("commit");
        (first, second)
    };

    let storage = open(&dir);
    assert_eq!(
        (storage.recovery().applied, storage.recovery().skipped),
        (2, 1)
    );
    let table = resumed(&storage);
    assert_eq!(
        table.scan(&snap(9, 100)).expect("scan"),
        vec![(first, row(13)), (second, row(12))]
    );
    // The update set the `xmax` of the first version in place and added the second one.
    assert_eq!(table.versions(Some(first)).expect("versions").len(), 2);
}

#[test]
fn a_redo_played_twice_gives_the_same_pages() {
    let dir = TempDir::created("recover-twice-pages");
    let head = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let ids = insert_rows(&mut table, TxnId(4), 600);
        for id in ids.iter().step_by(7) {
            table.update(TxnId(5), *id, &row(-1)).expect("update");
        }
        table.commit(TxnId(5)).expect("commit");
        for id in ids.iter().skip(3).step_by(11) {
            table.delete(TxnId(6), *id).expect("delete");
        }
        table.commit(TxnId(6)).expect("commit");
        // A loser, whose records the redo leaves out.
        table.insert(TxnId(7), &row(99)).expect("insert");
        head
    };

    let (first, pages, rows) = {
        let storage = open(&dir);
        let recovery = storage.recovery().clone();
        assert!(recovery.applied > 0, "{recovery:?}");
        let pages = pool_pages(&storage, head);
        assert!(pages.len() >= 3, "the heap spans {} pages", pages.len());
        let rows = resumed(&storage).scan(&snap(9, 100)).expect("scan");
        (recovery, pages, rows)
    };
    // Dropped without a checkpoint: the second redo starts again from the same `data`.
    let second = {
        let storage = open(&dir);
        let recovery = storage.recovery().clone();
        assert_eq!(pool_pages(&storage, head), pages);
        assert_eq!(resumed(&storage).scan(&snap(9, 100)).expect("scan"), rows);
        // The pages of this redo reach `data` this time.
        storage.checkpoint().expect("checkpoint");
        recovery
    };
    assert_eq!(
        (second.applied, second.skipped),
        (first.applied, first.skipped)
    );

    let storage = open(&dir);
    let third = storage.recovery().clone();
    assert_eq!(
        (third.applied, third.skipped),
        (0, first.applied + first.skipped),
        "the pages on data carry the numbers of the records"
    );
    assert_eq!(resumed(&storage).scan(&snap(9, 100)).expect("scan"), rows);
}

#[test]
fn a_chained_page_whose_formatting_missed_data_is_formatted_again() {
    let dir = TempDir::created("recover-lost-format");
    let (ids, head) = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let ids = insert_rows(&mut table, TxnId(4), 600);
        // The head page reaches `data` with its link to the second page; the second page
        // does not, and reads back as the free page the allocator wrote.
        storage.pool.flush(head).expect("flush the head page");
        (ids, head)
    };
    let storage = open(&dir);
    let second = Heap::open(&storage, TABLE, head)
        .redo_chain()
        .expect("chain")[1];
    assert_ne!(second, head);
    let table = resumed(&storage);
    let rows = table.scan(&snap(9, 100)).expect("scan");
    assert_eq!(rows.len(), ids.len());
    assert_eq!(
        rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        ids,
        "each row of the second page is put back"
    );
}

#[test]
fn a_page_on_data_out_of_the_chain_on_data_is_chained_back() {
    let dir = TempDir::created("recover-lost-link");
    let ids = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let ids = insert_rows(&mut table, TxnId(4), 600);
        // The pages but the head reach `data`; the head page, which holds the link, does not.
        let rest: BTreeSet<PageId> = Heap::open(&storage, TABLE, head)
            .scan()
            .expect("scan")
            .into_iter()
            .map(|rid| rid.page)
            .filter(|page| *page != head)
            .collect();
        assert!(!rest.is_empty());
        for page in rest {
            storage.pool.flush(page).expect("flush a page");
        }
        ids
    };
    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert!(
        recovery.applied > 0 && recovery.skipped > 0,
        "the head page is redone, the others skipped: {recovery:?}"
    );
    assert_eq!(recovery.applied + recovery.skipped, ids.len());
    let table = resumed(&storage);
    let rows = table.scan(&snap(9, 100)).expect("scan");
    assert_eq!(rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids);
}

/// A table of one `varchar(max)`, whose rows of 20 000 characters go to overflow pages.
fn text_shape() -> TableShape {
    TableShape {
        columns: vec![TypeInfo::new(
            SqlType::VarChar(vauban_types::Len::Max),
            true,
        )],
        clustered_key: None,
    }
}

/// A row of 20 000 copies of `c`.
fn long_row(c: char) -> Row {
    Row(vec![Value::String(vauban_types::SqlString {
        text: c.to_string().repeat(20_000),
    })])
}

#[test]
fn long_rows_are_redone_with_their_xmax_and_twice_alike() {
    let dir = TempDir::created("recover-long-rows");
    let (kept, replaced) = {
        let storage = open(&dir);
        let head = alloc::allocate(&storage).expect("allocate the head page");
        drop(HeapTable::create(&storage, TABLE, head, text_shape()).expect("create"));
        install_single_heap(&storage, TABLE, head, text_shape()).expect("install");
        let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
        let kept = table.insert(TxnId(4), &long_row('a')).expect("insert");
        let replaced = table.insert(TxnId(4), &long_row('b')).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        // The two rows reach `data`; the update and the delete that follow do not.
        storage.checkpoint().expect("checkpoint");
        table
            .update(TxnId(5), replaced, &long_row('c'))
            .expect("update");
        table.delete(TxnId(5), kept).expect("delete");
        table.commit(TxnId(5)).expect("commit");
        (kept, replaced)
    };

    for _ in 0..2 {
        let storage = open(&dir);
        let recovery = storage.recovery().clone();
        assert_eq!((recovery.applied, recovery.skipped), (2, 2));
        let head = recovery.heap_of(TABLE).expect("heap of the table");
        let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
        table
            .resume_after_recovery(
                recovery.next_row_id,
                recovery.next_seq,
                &recovery.winners,
                &recovery.losers,
            )
            .expect("resume");
        assert_eq!(
            table.scan(&snap(9, 100)).expect("scan"),
            vec![(replaced, long_row('c'))]
        );
        let versions = table.versions(None).expect("versions");
        assert_eq!(
            versions.iter().map(|v| (v.row, v.xmax)).collect::<Vec<_>>(),
            vec![
                (kept, Some(TxnId(5))),
                (replaced, Some(TxnId(5))),
                (replaced, None)
            ]
        );
        // Dropped without a checkpoint: the second pass redoes the same records.
    }
}

/// A row of `len` copies of `c`.
fn text_row(c: char, len: usize) -> Row {
    Row(vec![Value::String(vauban_types::SqlString {
        text: c.to_string().repeat(len),
    })])
}

#[test]
fn a_put_on_a_page_older_than_a_vacuum_takes_the_room_its_purges_give_back() {
    let dir = TempDir::created("recover-put-relocated");
    let (head, survivors, fresh) = {
        let storage = open(&dir);
        let head = alloc::allocate(&storage).expect("allocate the head page");
        drop(HeapTable::create(&storage, TABLE, head, text_shape()).expect("create"));
        install_single_heap(&storage, TABLE, head, text_shape()).expect("install");
        let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
        let ids: Vec<RowId> = (0..50)
            .map(|_| table.insert(TxnId(4), &text_row('a', 100)).expect("insert"))
            .collect();
        table.commit(TxnId(4)).expect("commit");
        // The full page reaches `data`; the deletes, the vacuum and the inserts that reuse
        // the room it freed stay in the pool.
        storage.checkpoint().expect("checkpoint");
        for id in ids.iter().step_by(2) {
            table.delete(TxnId(5), *id).expect("delete");
        }
        table.commit(TxnId(5)).expect("commit");
        assert_eq!(table.vacuum(TxnId(10)).expect("vacuum").len(), 25);
        let fresh: Vec<RowId> = (0..8)
            .map(|_| table.insert(TxnId(6), &text_row('b', 300)).expect("insert"))
            .collect();
        table.commit(TxnId(6)).expect("commit");
        let pages: BTreeSet<PageId> = Heap::open(&storage, TABLE, head)
            .scan()
            .expect("scan")
            .into_iter()
            .map(|rid| rid.page)
            .collect();
        assert_eq!(
            pages,
            BTreeSet::from([head]),
            "the inserts reused the head page"
        );
        let survivors: Vec<RowId> = ids.iter().skip(1).step_by(2).copied().collect();
        (head, survivors, fresh)
    };

    for _ in 0..2 {
        let storage = open(&dir);
        let recovery = storage.recovery().clone();
        let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
        table
            .resume_after_recovery(
                recovery.next_row_id,
                recovery.next_seq,
                &recovery.winners,
                &recovery.losers,
            )
            .expect("resume");
        let rows = table.scan(&snap(9, 100)).expect("scan");
        let mut expected: Vec<(RowId, Row)> = survivors
            .iter()
            .map(|id| (*id, text_row('a', 100)))
            .chain(fresh.iter().map(|id| (*id, text_row('b', 300))))
            .collect();
        expected.sort_by_key(|(id, _)| *id);
        assert_eq!(rows, expected);
        // The page on `data` still holds the deleted versions; the purges of the journal take
        // them out before the inserts that reused their room are replayed at their slots.
        assert!(storage.recovery().released >= 25);
        assert_eq!(record_pages(&storage, head), BTreeSet::from([head]));
        // Dropped without a checkpoint: the second pass starts again from the same `data`.
    }
}

/// The heap pages of [`TABLE`] that hold a record.
fn record_pages(storage: &DiskStorage, head: PageId) -> BTreeSet<PageId> {
    Heap::open(storage, TABLE, head)
        .scan()
        .expect("scan the heap")
        .into_iter()
        .map(|rid| rid.page)
        .collect()
}

/// Checks that the heap holds each version `(RowId, seq)` once.
fn assert_each_version_once(storage: &DiskStorage, head: PageId, at: &str) {
    let heap = Heap::open(storage, TABLE, head);
    let mut seen: BTreeMap<(RowId, u64), Rid> = BTreeMap::new();
    for rid in heap.scan().expect("scan") {
        let bytes = heap.get(rid).expect("get").expect("a record");
        let header = VersionHeader::read_from(&bytes).expect("prefix");
        if let Some(first) = seen.insert((header.row, header.seq), rid) {
            panic!(
                "{at}: version {} of row {} at {first} and at {rid}",
                header.seq, header.row
            );
        }
    }
}

/// The table of an instance reopened over `head`, resumed from its recovery.
fn resumed_text(storage: &DiskStorage, head: PageId) -> HeapTable<'_> {
    let recovery = storage.recovery().clone();
    let mut table = HeapTable::open(storage, TABLE, head, text_shape());
    table
        .resume_after_recovery(
            recovery.next_row_id,
            recovery.next_seq,
            &recovery.winners,
            &recovery.losers,
        )
        .expect("resume");
    table
}

/// A heap of two pages on `data`, then deletes, a vacuum and inserts that reuse the room of
/// the head page and fill the second one, all left in the pool; `delete_one` adds a committed
/// delete of one of the rows the head page took back. Answers the head page and the rows a
/// reader sees.
fn vacuum_then_reuse(dir: &TempDir, delete_one: bool) -> (PageId, Vec<(RowId, Row)>) {
    let storage = open(dir);
    let head = alloc::allocate(&storage).expect("allocate the head page");
    drop(HeapTable::create(&storage, TABLE, head, text_shape()).expect("create"));
    install_single_heap(&storage, TABLE, head, text_shape()).expect("install");
    let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
    let mut ids = Vec::new();
    while record_pages(&storage, head).len() < 2 {
        ids.push(table.insert(TxnId(4), &text_row('a', 100)).expect("insert"));
    }
    let on_head: Vec<RowId> = ids[..ids.len() - 1].to_vec();
    for _ in 0..3 {
        ids.push(table.insert(TxnId(4), &text_row('a', 100)).expect("insert"));
    }
    table.commit(TxnId(4)).expect("commit");
    storage.checkpoint().expect("checkpoint");
    let mut expected: BTreeMap<RowId, Row> =
        ids.iter().map(|id| (*id, text_row('a', 100))).collect();
    for id in on_head.iter().step_by(2) {
        table.delete(TxnId(5), *id).expect("delete");
        expected.remove(id);
    }
    table.commit(TxnId(5)).expect("commit");
    table.vacuum(TxnId(10)).expect("vacuum");
    let mut reused = Vec::new();
    for _ in 0..8 {
        let id = table.insert(TxnId(6), &text_row('b', 300)).expect("insert");
        expected.insert(id, text_row('b', 300));
        reused.push(id);
    }
    for _ in 0..20 {
        let id = table.insert(TxnId(6), &text_row('c', 100)).expect("insert");
        expected.insert(id, text_row('c', 100));
    }
    table.commit(TxnId(6)).expect("commit");
    if delete_one {
        let id = reused[reused.len() - 1];
        table.delete(TxnId(7), id).expect("delete");
        table.commit(TxnId(7)).expect("commit");
        expected.remove(&id);
    }
    let expected: Vec<(RowId, Row)> = expected.into_iter().collect();
    assert_eq!(table.scan(&snap(9, 100)).expect("scan"), expected);
    (head, expected)
}

/// Reopens `dir` twice without a checkpoint and checks the rows and the addresses each time.
fn reopen_twice(dir: &TempDir, head: PageId, expected: &[(RowId, Row)]) {
    for pass in 0..2 {
        let storage = open(dir);
        assert_each_version_once(&storage, head, &format!("pass {pass}"));
        let rows = resumed_text(&storage, head)
            .scan(&snap(9, 100))
            .expect("scan");
        assert_eq!(rows, expected, "pass {pass}");
    }
}

#[test]
fn a_version_in_room_a_vacuum_gave_back_survives_the_later_puts_on_its_page() {
    let dir = TempDir::created("recover-relocated-later-put");
    let (head, expected) = vacuum_then_reuse(&dir, false);
    reopen_twice(&dir, head, &expected);
}

#[test]
fn a_version_in_room_a_vacuum_gave_back_deleted_later_is_hidden_after_the_redo() {
    let dir = TempDir::created("recover-relocated-deleted");
    let (head, expected) = vacuum_then_reuse(&dir, true);
    reopen_twice(&dir, head, &expected);
}

/// A generator of numbers for the crash test, the same sequence for the same seed.
struct Lcg(u64);

impl Lcg {
    /// A number below `bound`.
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }
}

/// Copies the files of the instance in `from` to a fresh directory: what `data`, the journal
/// and `vauban.ctl` hold at that point, the pool left out.
fn crash_image(from: &TempDir, label: &str) -> TempDir {
    let image = TempDir::created(label);
    for entry in std::fs::read_dir(from.path()).expect("list the instance") {
        let entry = entry.expect("an entry");
        std::fs::copy(entry.path(), image.path().join(entry.file_name())).expect("copy a file");
    }
    image
}

/// Opens `image` twice and checks that each recovery serves `committed`, each version once.
///
/// With `life`, the instance then lives again over the image: rows of 100 to 800 bytes are
/// inserted and committed, one committed row is deleted, a transaction of inserts and an update
/// is rolled back, a vacuum runs, more rows are inserted and committed, some pages reach
/// `data`, and the process goes away without a checkpoint; two more recoveries then serve the
/// rows of both lives, each version once.
fn check_image(
    image: &TempDir,
    head: PageId,
    committed: &BTreeMap<RowId, Row>,
    at: &str,
    life: Option<u64>,
) {
    let mut committed = committed.clone();
    let check = |committed: &BTreeMap<RowId, Row>, when: &str| {
        let expected: Vec<(RowId, Row)> = committed
            .iter()
            .map(|(id, row)| (*id, row.clone()))
            .collect();
        for pass in 0..2 {
            let storage = open(image);
            assert_each_version_once(&storage, head, &format!("{at}, {when}, pass {pass}"));
            let rows = resumed_text(&storage, head)
                .scan(&snap(999_999, 1_000_000))
                .expect("scan");
            assert_eq!(rows, expected, "{at}, {when}, pass {pass}");
        }
    };
    check(&committed, "first life");
    let Some(seed) = life else {
        return;
    };
    {
        let storage = open(image);
        let mut table = resumed_text(&storage, head);
        let mut rng = Lcg(seed);
        let txn = TxnId(900_000);
        for _ in 0..12 {
            let value = text_row('z', 100 + rng.below(700) as usize);
            let id = table.insert(txn, &value).expect("insert after the redo");
            committed.insert(id, value);
        }
        if let Some(&id) = committed.keys().next() {
            table.delete(txn, id).expect("delete after the redo");
            committed.remove(&id);
        }
        table.commit(txn).expect("commit after the redo");
        // A rolled-back transaction, a vacuum, then inserts that take the room both gave back.
        let loser = TxnId(900_001);
        for _ in 0..3 {
            let value = text_row('y', 300 + rng.below(500) as usize);
            table.insert(loser, &value).expect("insert to roll back");
        }
        if let Some(&id) = committed.keys().last() {
            table
                .update(loser, id, &text_row('y', 700))
                .expect("update to roll back");
        }
        table.rollback(loser).expect("rollback after the redo");
        table.vacuum(TxnId(900_002)).expect("vacuum after the redo");
        let txn = TxnId(900_003);
        for _ in 0..4 {
            let value = text_row('x', 100 + rng.below(700) as usize);
            let id = table.insert(txn, &value).expect("insert after the vacuum");
            committed.insert(id, value);
        }
        table.commit(txn).expect("commit after the vacuum");
        for page in record_pages(&storage, head) {
            if rng.below(2) == 0 {
                storage
                    .pool
                    .flush(page)
                    .expect("flush a page after the redo");
            }
        }
    }
    check(&committed, "second life");
}

/// What one step of [`crash_at_each_write`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Insert(usize),
    Update,
    Delete,
    Commit,
    Rollback,
}

/// Runs a seeded sequence of inserts, updates, deletes, commits, rollbacks and vacuums, with a
/// crash image after each write of the journal and after each write of a page, and checks
/// each image twice. Answers the number of images.
///
/// The sequence goes by phases of 30 steps: inserts of rows of 100 to 300 bytes, then deletes
/// and updates, then inserts of rows of 300 to 800 bytes. Between two phases the open
/// transaction commits, a vacuum runs, and with `checkpoints` a checkpoint writes the pool
/// before the deletes, so that `data` holds pages older than the vacuum whose room the large
/// rows take back. One page of the heap is written after a step one time in three. With
/// `lives`, one image in five also lives again after its first recovery ([`check_image`]).
fn crash_at_each_write(seed: u64, checkpoints: bool, lives: bool) -> usize {
    let dir = TempDir::created(&format!("recover-crash-{seed}-{checkpoints}-{lives}"));
    let storage = open(&dir);
    let head = alloc::allocate(&storage).expect("allocate the head page");
    drop(HeapTable::create(&storage, TABLE, head, text_shape()).expect("create"));
    install_single_heap(&storage, TABLE, head, text_shape()).expect("install");
    let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
    let mut rng = Lcg(seed);
    let mut committed: BTreeMap<RowId, Row> = BTreeMap::new();
    let mut pending: Option<(TxnId, BTreeMap<RowId, Option<Row>>)> = None;
    let mut next_txn = 10u64;
    let mut images = 0usize;
    let mut flushed = 0usize;
    let label = format!("recover-crash-image-{seed}-{checkpoints}-{lives}");
    for step in 0..180usize {
        let phase = (step / 30) % 3;
        if step % 30 == 0 && step > 0 {
            if let Some((txn, changes)) = pending.take() {
                table.commit(txn).expect("commit");
                apply_changes(&mut committed, changes);
            }
            next_txn += 1;
            table.vacuum(TxnId(next_txn)).expect("vacuum");
            if checkpoints && phase == 1 {
                storage.checkpoint().expect("checkpoint");
            }
        }
        let roll = rng.below(20);
        let chosen = match phase {
            0 => match roll {
                0..=16 => Step::Insert(100 + rng.below(200) as usize),
                17..=18 => Step::Commit,
                _ => Step::Rollback,
            },
            1 => match roll {
                0..=9 => Step::Delete,
                10..=13 => Step::Update,
                14..=17 => Step::Commit,
                18 => Step::Rollback,
                _ => Step::Insert(100),
            },
            _ => match roll {
                0..=15 => Step::Insert(300 + rng.below(500) as usize),
                16..=18 => Step::Commit,
                _ => Step::Rollback,
            },
        };
        let (txn, changes) = pending.get_or_insert_with(|| {
            next_txn += 1;
            (TxnId(next_txn), BTreeMap::new())
        });
        let txn = *txn;
        let visible: Vec<RowId> = committed
            .keys()
            .chain(changes.keys())
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|id| changes.get(id).is_none_or(Option::is_some))
            .collect();
        let letter = char::from(b'a' + rng.below(26) as u8);
        let picked =
            (!visible.is_empty()).then(|| visible[rng.below(visible.len() as u64) as usize]);
        match (chosen, picked) {
            (Step::Insert(len), _) => {
                let value = text_row(letter, len);
                let id = table.insert(txn, &value).expect("insert");
                changes.insert(id, Some(value));
            }
            (Step::Update, Some(id)) => {
                let value = text_row(letter, 100 + rng.below(200) as usize);
                table.update(txn, id, &value).expect("update");
                changes.insert(id, Some(value));
            }
            (Step::Delete, Some(id)) => {
                table.delete(txn, id).expect("delete");
                changes.insert(id, None);
            }
            (Step::Commit, _) => {
                table.commit(txn).expect("commit");
                if let Some((_, changes)) = pending.take() {
                    apply_changes(&mut committed, changes);
                }
            }
            (Step::Rollback, _) => {
                table.rollback(txn).expect("rollback");
                pending = None;
            }
            _ => continue,
        }
        let image = crash_image(&dir, &label);
        let life = (lives && step % 5 == 0).then_some(seed * 1_000 + step as u64);
        check_image(
            &image,
            head,
            &committed,
            &format!("seed {seed}, after step {step}"),
            life,
        );
        images += 1;
        if rng.below(3) != 0 {
            continue;
        }
        // One page of the heap goes to `data`, when the pool may write it.
        let pages: Vec<PageId> = Heap::open(&storage, TABLE, head)
            .redo_chain()
            .expect("chain");
        let page = pages[rng.below(pages.len() as u64) as usize];
        if storage.pool.flush(page).is_ok() {
            flushed += 1;
            let image = crash_image(&dir, &label);
            check_image(
                &image,
                head,
                &committed,
                &format!("seed {seed}, page {page} written after step {step}"),
                None,
            );
            images += 1;
        }
    }
    assert!(flushed > 0, "seed {seed}: no page was written");
    images
}

/// Folds the changes of a committed transaction into the rows a reader sees.
fn apply_changes(committed: &mut BTreeMap<RowId, Row>, changes: BTreeMap<RowId, Option<Row>>) {
    for (id, change) in changes {
        match change {
            Some(row) => committed.insert(id, row),
            None => committed.remove(&id),
        };
    }
}

#[test]
fn a_crash_at_each_write_of_the_journal_or_of_a_page_recovers_the_committed_rows() {
    let mut images = 0;
    for seed in 1..=3 {
        images += crash_at_each_write(seed, false, false);
        images += crash_at_each_write(seed, true, false);
    }
    assert!(images > 6 * 150, "{images} crash images");
}

#[test]
fn a_life_after_a_redo_over_room_a_vacuum_gave_back_recovers_each_version_once() {
    let dir = TempDir::created("recover-zz-second-life");
    let (head, expected) = vacuum_then_reuse(&dir, false);
    let mut expected: BTreeMap<RowId, Row> = expected.into_iter().collect();
    {
        // The pages but the head reach `data` after the first redo, and the life after it
        // writes on them; the head page on `data` is still the one older than the vacuum.
        let storage = open(&dir);
        let mut table = resumed_text(&storage, head);
        let got: BTreeMap<RowId, Row> = table
            .scan(&snap(9, 100))
            .expect("scan")
            .into_iter()
            .collect();
        assert_eq!(got, expected, "after the first recovery");
        for page in record_pages(&storage, head) {
            if page != head {
                storage.pool.flush(page).expect("flush one page");
            }
        }
        for _ in 0..150 {
            let id = table
                .insert(TxnId(500), &text_row('d', 100))
                .expect("insert");
            expected.insert(id, text_row('d', 100));
        }
        table.commit(TxnId(500)).expect("commit");
    }
    let expected: Vec<(RowId, Row)> = expected.into_iter().collect();
    for pass in 0..2 {
        let storage = open(&dir);
        assert_each_version_once(&storage, head, &format!("pass {pass}"));
        let rows = resumed_text(&storage, head)
            .scan(&snap(999, 1000))
            .expect("scan");
        assert_eq!(rows, expected, "pass {pass}");
    }
}

#[test]
fn a_crash_after_a_second_life_over_each_image_recovers_the_committed_rows() {
    for seed in 1..=3 {
        crash_at_each_write(seed, false, true);
        crash_at_each_write(seed, true, true);
    }
}

#[test]
fn a_put_without_room_at_its_slot_is_corruption() {
    let dir = TempDir::created("recover-put-without-room");
    {
        let storage = open(&dir);
        let head = alloc::allocate(&storage).expect("allocate the head page");
        drop(HeapTable::create(&storage, TABLE, head, text_shape()).expect("create"));
        install_single_heap(&storage, TABLE, head, text_shape()).expect("install");
        let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
        while record_pages(&storage, head).len() < 2 {
            table
                .insert(TxnId(4), &text_row('a', 1000))
                .expect("insert");
        }
        table.commit(TxnId(4)).expect("commit");
        storage.checkpoint().expect("checkpoint");
        // A committed insert whose record names the full head page.
        let mut columns = Vec::new();
        super::super::encode::encode_row(&text_row('b', 3000), &mut columns);
        let payload = RowChange {
            table: TABLE,
            header: VersionHeader {
                row: RowId(500),
                xmin: TxnId(9),
                xmax: None,
                seq: 500,
            },
            columns,
        }
        .encode_with(&[PageOp::Put {
            rid: Rid {
                page: head,
                slot: 40,
            },
        }]);
        storage
            .wal
            .append(WalRecordKind::Insert, TxnId(9), &payload)
            .expect("append the insert");
        storage
            .wal
            .append_durable(WalRecordKind::Commit, TxnId(9), &[])
            .expect("append the commit");
    }
    for _ in 0..2 {
        match DiskStorage::open(dir.path(), DiskOptions::default()) {
            Err(InternalError::Corruption(message)) => {
                assert!(message.contains("has no room"), "{message}");
            }
            other => panic!("expected a corruption, got {other:?}"),
        }
    }
}

// --- Records that free a heap slot ---------------------------------------------------

/// A row holding `len` times `fill`.
fn text(len: usize, fill: char) -> Row {
    Row(vec![Value::String(SqlString {
        text: fill.to_string().repeat(len),
    })])
}

/// Creates the heap of [`TABLE`], rows of [`text_shape`], and tells the recovery about it.
fn install_text(storage: &DiskStorage) -> PageId {
    let head = alloc::allocate(storage).expect("allocate the head page");
    drop(HeapTable::create(storage, TABLE, head, text_shape()).expect("create the table"));
    install_single_heap(storage, TABLE, head, text_shape()).expect("install the heap");
    head
}

/// The table of rows of [`text_shape`] of a reopened instance, resumed from the recovery.
fn resumed_text_from_recovery(storage: &DiskStorage) -> HeapTable<'_> {
    let recovery = storage.recovery();
    let head = recovery.heap_of(TABLE).expect("the journal names the heap");
    let mut table = HeapTable::open(storage, TABLE, head, text_shape());
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

/// Each version the heap of [`TABLE`] holds, with its address, page by page.
fn placed(storage: &DiskStorage) -> Vec<(Rid, VersionHeader)> {
    let head = storage.recovery().heap_of(TABLE).expect("the heap");
    placed_from(storage, head)
}

/// The same, for the heap whose head page is `head`.
fn placed_from(storage: &DiskStorage, head: PageId) -> Vec<(Rid, VersionHeader)> {
    let heap = Heap::open(storage, TABLE, head);
    heap.scan()
        .expect("scan the heap")
        .into_iter()
        .map(|rid| {
            let bytes = heap.get(rid).expect("read").expect("a record");
            (rid, VersionHeader::read_from(&bytes).expect("a prefix"))
        })
        .collect()
}

/// What a settled snapshot reads in the table of rows of [`text_shape`].
fn settled_text(storage: &DiskStorage) -> Vec<(RowId, Row)> {
    resumed_text_from_recovery(storage)
        .scan(&snap(99, 100))
        .expect("scan")
}

#[test]
fn a_rollback_on_a_page_already_on_data_gives_its_room_back_at_the_redo() {
    let dir = TempDir::created("recover-compensation-room");
    let (head, expected, last) = {
        let storage = open(&dir);
        let head = install_text(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
        let mut expected = Vec::new();
        for i in 0..10u8 {
            let row = text(500, char::from(b'a' + i));
            expected.push((table.insert(TxnId(4), &row).expect("insert"), row));
        }
        table.commit(TxnId(4)).expect("commit 4");
        let undone = [
            table.insert(TxnId(5), &text(1000, 'x')).expect("insert"),
            table.insert(TxnId(5), &text(1000, 'x')).expect("insert"),
        ];
        let small = text(10, 'y');
        expected.push((table.insert(TxnId(6), &small).expect("insert"), small));
        // The commit of 6 takes the flag off the page both wrote and makes the records of 5
        // durable with its own: the page reaches `data` holding the two versions of 5.
        table.commit(TxnId(6)).expect("commit 6");
        storage.pool.flush(head).expect("write the shared page");
        table.rollback(TxnId(5)).expect("rollback 5");
        let large = text(1900, 'z');
        let last = table.insert(TxnId(7), &large).expect("insert");
        expected.push((last, large));
        table.commit(TxnId(7)).expect("commit 7");
        // Before the crash, each version is on the head page, the last one in the room the
        // rollback gave back.
        let before = placed_from(&storage, head);
        assert!(before.iter().all(|(rid, _)| rid.page == head), "{before:?}");
        assert!(
            before
                .iter()
                .all(|(_, header)| !undone.contains(&header.row))
        );
        (head, expected, last)
    };

    for _ in 0..2 {
        let storage = open(&dir);
        let recovery = storage.recovery().clone();
        assert_eq!(recovery.winners, vec![TxnId(4), TxnId(6), TxnId(7)]);
        assert_eq!(recovery.losers, vec![TxnId(5)]);
        assert_eq!(settled_text(&storage), expected);
        let versions = placed(&storage);
        assert!(
            versions.iter().all(|(_, header)| header.xmin != TxnId(5)),
            "the versions of the rolled-back transaction are out of the heap: {versions:?}"
        );
        assert_eq!(versions.len(), 12);
        let (at, _) = versions
            .iter()
            .find(|(_, header)| header.row == last)
            .expect("the version of 7");
        assert_eq!(
            at.page, head,
            "the version of 7 took the room the rollback gave back"
        );
    }
    // The first reopen took the two versions out of the page `data` held; the second reads
    // the same page, the pool of the first having been dropped, and takes them out again.
    assert_eq!(open(&dir).recovery().released, 2);
}

#[test]
fn a_rollback_then_a_second_life_then_a_crash_recovers_the_committed_rows() {
    let dir = TempDir::created("recover-compensation-life");
    let mut expected = {
        let storage = open(&dir);
        let head = install_text(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
        let mut expected = Vec::new();
        for i in 0..10u8 {
            let row = text(500, char::from(b'a' + i));
            expected.push((table.insert(TxnId(4), &row).expect("insert"), row));
        }
        table.commit(TxnId(4)).expect("commit 4");
        table.insert(TxnId(5), &text(1000, 'x')).expect("insert");
        table.insert(TxnId(5), &text(1000, 'x')).expect("insert");
        let small = text(10, 'y');
        expected.push((table.insert(TxnId(6), &small).expect("insert"), small));
        table.commit(TxnId(6)).expect("commit 6");
        storage.pool.flush(head).expect("write the shared page");
        table.rollback(TxnId(5)).expect("rollback 5");
        expected
    };
    {
        // A life after the recovery: a row in the room given back, its page written, then
        // a crash.
        let storage = open(&dir);
        assert_eq!(storage.recovery().released, 2);
        let mut table = resumed_text_from_recovery(&storage);
        let large = text(1900, 'z');
        expected.push((table.insert(TxnId(8), &large).expect("insert"), large));
        table.commit(TxnId(8)).expect("commit 8");
        let head = storage.recovery().heap_of(TABLE).expect("the heap");
        storage.pool.flush(head).expect("write the head page");
    }
    for _ in 0..2 {
        let storage = open(&dir);
        assert_eq!(settled_text(&storage), expected);
        assert_eq!(placed(&storage).len(), 12);
    }
}

#[test]
fn a_compensation_inside_an_outer_rolled_back_range_is_replayed() {
    let dir = TempDir::created("recover-compensation-nested");
    let expected = {
        let storage = open(&dir);
        let head = install(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, int_shape());
        let kept = table.insert(TxnId(4), &row(1)).expect("insert kept");
        // The records `DiskStorage::savepoint` and `DiskStorage::rollback_to` append, the marks
        // they take per table being the length of the undo log.
        let savepoint = |id: u64| {
            storage
                .wal
                .append(WalRecordKind::Savepoint, TxnId(4), &id.to_le_bytes())
                .expect("savepoint")
        };
        savepoint(1);
        let outer = table.writes_len(TxnId(4));
        table.insert(TxnId(4), &row(2)).expect("insert 2");
        savepoint(2);
        let inner = table.writes_len(TxnId(4));
        table.insert(TxnId(4), &row(3)).expect("insert 3");
        // Another transaction shares the page and commits: the page reaches `data` holding
        // the three versions of 4.
        let other = table.insert(TxnId(6), &row(6)).expect("insert 6");
        table.commit(TxnId(6)).expect("commit 6");
        storage.pool.flush(head).expect("write the shared page");
        let rollback_to = |id: u64| {
            storage
                .wal
                .append_durable(WalRecordKind::RollbackTo, TxnId(4), &id.to_le_bytes())
                .expect("rollback to")
        };
        rollback_to(2);
        table
            .rollback_to_savepoint(TxnId(4), inner)
            .expect("undo the insert of 3");
        table.insert(TxnId(4), &row(4)).expect("insert 4");
        rollback_to(1);
        table
            .rollback_to_savepoint(TxnId(4), outer)
            .expect("undo the inserts of 2 and 4");
        table.commit(TxnId(4)).expect("commit 4");
        vec![(kept, row(1)), (other, row(6))]
    };

    let storage = open(&dir);
    let recovery = storage.recovery().clone();
    assert_eq!(recovery.winners, vec![TxnId(4), TxnId(6)]);
    // The compensation of 3 sits between the first savepoint and the second rollback, the
    // range the scan cancels; it is replayed all the same and takes 3 out of the page `data`
    // held, where its committed writer would make it visible. 2 was on that page too; 4
    // never reached it.
    assert_eq!(
        resumed(&storage).scan(&snap(9, 100)).expect("scan"),
        expected
    );
    assert_eq!(recovery.released, 2);
    assert_eq!(placed(&storage).len(), 2);
}

#[test]
fn a_vacuum_then_a_crash_recovers_the_committed_rows_without_the_purged_versions() {
    let dir = TempDir::created("recover-purge");
    let expected = {
        let storage = open(&dir);
        let head = install_text(&storage);
        let mut table = HeapTable::open(&storage, TABLE, head, text_shape());
        let mut rows = Vec::new();
        for i in 0..10u8 {
            let row = text(500, char::from(b'a' + i));
            rows.push((table.insert(TxnId(4), &row).expect("insert"), row));
        }
        table.commit(TxnId(4)).expect("commit 4");
        storage.checkpoint().expect("checkpoint");
        for (id, _) in rows.iter().step_by(2) {
            table.delete(TxnId(5), *id).expect("delete");
        }
        let changed = text(400, 'u');
        table.update(TxnId(5), rows[1].0, &changed).expect("update");
        rows[1].1 = changed;
        table.commit(TxnId(5)).expect("commit 5");
        let removed = table.vacuum(TxnId(6)).expect("vacuum");
        assert_eq!(removed.len(), 6);
        let mut expected: Vec<(RowId, Row)> = rows.into_iter().skip(1).step_by(2).collect();
        for i in 0..3u8 {
            let row = text(500, char::from(b'p' + i));
            expected.push((table.insert(TxnId(7), &row).expect("insert"), row));
        }
        table.commit(TxnId(7)).expect("commit 7");
        expected
    };

    for pass in 0..2 {
        let storage = open(&dir);
        let recovery = storage.recovery().clone();
        assert_eq!(recovery.winners, vec![TxnId(4), TxnId(5), TxnId(7)]);
        assert_eq!(settled_text(&storage), expected, "reopen {pass}");
        // The heap holds the live versions and nothing else: the six the vacuum took out were
        // replayed from their `Insert` and left again at their `Purge`.
        let versions = placed(&storage);
        assert_eq!(versions.len(), 8, "reopen {pass}: {versions:?}");
        assert!(versions.iter().all(|(_, header)| header.xmax.is_none()));
        assert_eq!(recovery.released, 6, "reopen {pass}");
    }
}
