use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use super::super::buffer::DurableLsn;
use super::super::temp::TempDir;
use super::super::wal::{Wal, WalRecord};
use super::super::{DiskOptions, WAL_FILE_NAME, alloc};
use super::*;

/// The table the tests of this file work on.
const TABLE: TableId = TableId(9);

/// An empty instance in a temporary directory, with the guard that removes it.
fn instance(label: &str) -> (TempDir, DiskStorage) {
    instance_of(label, DiskOptions::default().buffer_pages)
}

/// The same, with a buffer pool of `frames` frames.
fn instance_of(label: &str, frames: usize) -> (TempDir, DiskStorage) {
    let dir = TempDir::created(label);
    let storage = DiskStorage::open(
        dir.path(),
        DiskOptions {
            buffer_pages: frames,
        },
    )
    .expect("create an instance");
    (dir, storage)
}

/// The records the journal of `storage` holds, read through its handle.
fn journal(storage: &DiskStorage) -> Vec<WalRecord> {
    storage.wal.records().expect("read the journal")
}

/// The records the journal **file** holds, read by a handle of its own.
///
/// What this answers went through `write_all` on the file, whether a `sync_all` covered it
/// or not: it is what another process would read, not what a crash would leave.
fn journal_on_disk(dir: &TempDir) -> Vec<WalRecord> {
    let wal = Wal::open(&dir.child(WAL_FILE_NAME)).expect("open the journal file");
    wal.iter()
        .expect("read the journal file")
        .collect::<Result<Vec<_>, _>>()
        .expect("the records of the journal file")
}

/// The kind of each record, in order.
fn kinds(records: &[WalRecord]) -> Vec<WalRecordKind> {
    records.iter().map(|record| record.kind).collect()
}

/// A table of one nullable `int` column, no clustered key, over a fresh heap page.
fn table_of(storage: &DiskStorage) -> HeapTable<'_> {
    table_shaped(storage, int_shape())
}

/// The shape of the tables of one nullable `int` column.
fn int_shape() -> TableShape {
    TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: None,
    }
}

/// A table of the given shape over a fresh heap page.
fn table_shaped(storage: &DiskStorage, shape: TableShape) -> HeapTable<'_> {
    let head = alloc::allocate(storage).expect("allocate the head page");
    HeapTable::create(storage, TABLE, head, shape).expect("create the table")
}

/// A row of one `int`.
fn row(value: i32) -> Row {
    Row(vec![Value::I32(value)])
}

/// A snapshot of `own` seeing what is below `xmax` but the transactions of `active`.
fn snap(own: u64, xmax: u64, active: &[u64]) -> Snapshot {
    Snapshot {
        xmin: TxnId(active.iter().copied().min().unwrap_or(xmax)),
        xmax: TxnId(xmax),
        active: active.iter().map(|t| TxnId(*t)).collect(),
        own: TxnId(own),
    }
}

/// The message of an error the test expects to be a caller bug.
fn bug_message(error: InternalError) -> String {
    match error {
        InternalError::Bug(message) => message,
        other => panic!("expected a Bug, got {other:?}"),
    }
}

/// The `(row, seq, xmin, xmax)` of the versions the heap holds, in chain order.
fn chain(table: &HeapTable<'_>) -> Vec<(u64, u64, u64, Option<u64>)> {
    table
        .stored_versions(None)
        .expect("read the versions")
        .iter()
        .map(|version| {
            (
                version.header.row.0,
                version.header.seq,
                version.header.xmin.0,
                version.header.xmax.map(|x| x.0),
            )
        })
        .collect()
}

// --- Layout of a version ------------------------------------------------------------

#[test]
fn a_version_is_a_thirty_two_byte_prefix_then_the_columns() {
    let header = VersionHeader {
        row: RowId(7),
        xmin: TxnId(3),
        xmax: None,
        seq: 42,
    };
    let bytes = encode_version(&header, &row(11));
    let mut columns = Vec::new();
    encode_row(&row(11), &mut columns);
    assert_eq!(VERSION_PREFIX_LEN, 32);
    assert_eq!(bytes.len(), VERSION_PREFIX_LEN + columns.len());
    assert_eq!(&bytes[VERSION_PREFIX_LEN..], columns.as_slice());
    assert_eq!(&bytes[0..8], &7u64.to_le_bytes());
    assert_eq!(&bytes[8..16], &3u64.to_le_bytes());
    assert_eq!(&bytes[16..24], &NO_XMAX.to_le_bytes());
    assert_eq!(&bytes[24..32], &42u64.to_le_bytes());
    assert_eq!(VersionHeader::read_from(&bytes).expect("read back"), header);
}

#[test]
fn with_xmax_changes_the_prefix_and_keeps_the_columns() {
    let header = VersionHeader {
        row: RowId(7),
        xmin: TxnId(3),
        xmax: None,
        seq: 42,
    };
    let bytes = encode_version(&header, &row(11));
    let patched = with_xmax(&bytes, Some(TxnId(5))).expect("patch the xmax");
    assert_eq!(patched.len(), bytes.len());
    assert_eq!(&patched[VERSION_PREFIX_LEN..], &bytes[VERSION_PREFIX_LEN..]);
    let read = VersionHeader::read_from(&patched).expect("read back");
    assert_eq!(read.xmax, Some(TxnId(5)));
    assert_eq!(read.row, header.row);
    assert_eq!(read.xmin, header.xmin);
    assert_eq!(read.seq, header.seq);
    assert_eq!(with_xmax(&patched, None).expect("clear the xmax"), bytes);
}

#[test]
fn decode_version_refuses_a_record_shorter_than_the_prefix() {
    let short = vec![0u8; VERSION_PREFIX_LEN - 1];
    assert!(matches!(
        VersionHeader::read_from(&short),
        Err(InternalError::Corruption(_))
    ));
    assert!(matches!(
        with_xmax(&short, None),
        Err(InternalError::Corruption(_))
    ));
}

// --- Visibility ---------------------------------------------------------------------

#[test]
fn own_insert_visible_before_commit() {
    let (_dir, storage) = instance("version-own-insert");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");

    let own = snap(4, 5, &[4]);
    assert_eq!(table.get(&own, id).expect("get"), Some(row(1)));
    assert_eq!(table.scan(&own).expect("scan"), vec![(id, row(1))]);

    // Another transaction, with 4 still active: nothing to see.
    let other = snap(7, 8, &[4]);
    assert_eq!(table.get(&other, id).expect("get"), None);
    assert!(table.scan(&other).expect("scan").is_empty());
}

#[test]
fn commit_makes_row_visible_to_others() {
    let (_dir, storage) = instance("version-commit-visible");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    let after = snap(9, 100, &[]);
    assert_eq!(table.get(&after, id).expect("before commit"), None);

    table.commit(TxnId(4)).expect("commit");
    assert_eq!(table.get(&after, id).expect("after commit"), Some(row(1)));
    assert_eq!(table.scan(&after).expect("scan"), vec![(id, row(1))]);
    assert_eq!(table.status(TxnId(4)), TxnStatus::Committed);
}

#[test]
fn rollback_undoes_insert() {
    let (_dir, storage) = instance("version-rollback-insert");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.rollback(TxnId(4)).expect("rollback");

    assert_eq!(table.get(&snap(4, 5, &[4]), id).expect("get"), None);
    assert_eq!(table.get(&snap(9, 100, &[]), id).expect("get"), None);
    assert_eq!(table.latest_version(id).expect("latest"), None);
    assert!(chain(&table).is_empty());
    assert!(table.heap().scan().expect("heap scan").is_empty());

    // The RowId is not handed out again.
    let next = table.insert(TxnId(5), &row(2)).expect("insert");
    assert_eq!(id, RowId(1));
    assert_eq!(next, RowId(2));
}

#[test]
fn seq_is_not_reused_after_a_rollback() {
    let (_dir, storage) = instance("version-seq");
    let mut table = table_of(&storage);
    table.insert(TxnId(4), &row(1)).expect("insert");
    table.rollback(TxnId(4)).expect("rollback");
    let id = table.insert(TxnId(5), &row(2)).expect("insert");
    assert_eq!(chain(&table), vec![(id.0, 2, 5, None)]);
}

#[test]
fn update_keeps_row_id_and_old_version() {
    let (_dir, storage) = instance("version-update");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");

    table.update(TxnId(6), id, &row(2)).expect("update");
    assert_eq!(
        chain(&table),
        vec![(id.0, 1, 4, Some(6)), (id.0, 2, 6, None)]
    );

    // The writer sees the new content, a snapshot that has not settled 6 the old one.
    assert_eq!(table.get(&snap(6, 7, &[6]), id).expect("get"), Some(row(2)));
    let reader = snap(8, 9, &[6]);
    assert_eq!(table.get(&reader, id).expect("get"), Some(row(1)));
    assert_eq!(table.scan(&reader).expect("scan"), vec![(id, row(1))]);
    assert_eq!(
        table.latest_version(id).expect("latest"),
        Some((TxnId(6), row(2)))
    );
}

/// A version is read by its row and its `seq` through the directory of the versions, which
/// follows the record when an `update` or a `delete` moves it, and loses the versions a
/// rollback takes away.
#[test]
fn version_by_row_and_seq_reads_the_directory_of_the_versions() {
    let (_dir, storage) = instance("version-by-seq");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");
    table.update(TxnId(5), id, &row(2)).expect("update");
    table.commit(TxnId(5)).expect("commit");
    let seqs: Vec<u64> = table
        .versions(Some(id))
        .expect("versions")
        .iter()
        .map(|version| version.seq)
        .collect();
    assert_eq!(seqs.len(), 2);

    let old = table.version(id, seqs[0]).expect("old").expect("held");
    assert_eq!(
        (old.xmin, old.xmax, old.data),
        (TxnId(4), Some(TxnId(5)), row(1))
    );
    let new = table.version(id, seqs[1]).expect("new").expect("held");
    assert_eq!((new.xmin, new.xmax, new.data), (TxnId(5), None, row(2)));

    // A delete rewrites the current version elsewhere; the lookup follows it.
    table.delete(TxnId(6), id).expect("delete");
    let deleted = table.version(id, seqs[1]).expect("deleted").expect("held");
    assert_eq!(deleted.xmax, Some(TxnId(6)));
    // An update rolled back takes its version away from the directory.
    table.rollback(TxnId(6)).expect("rollback the delete");
    table.update(TxnId(7), id, &row(3)).expect("update");
    let undone = table.versions(Some(id)).expect("versions")[2].seq;
    table.rollback(TxnId(7)).expect("rollback the update");
    assert_eq!(table.version(id, undone).expect("undone"), None);
    assert_eq!(
        table
            .version(id, seqs[1])
            .expect("restored")
            .expect("held")
            .xmax,
        None
    );
    assert_eq!(
        table.version(RowId(99), seqs[0]).expect("unknown row"),
        None
    );
}

#[test]
fn rid_of_a_row_may_move_on_update() {
    let (_dir, storage) = instance("version-rid-moves");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    let first = table.state.directory[&id];
    table.commit(TxnId(4)).expect("commit");
    table.update(TxnId(6), id, &row(2)).expect("update");

    // The RowId is the one the insert handed out; the address of its current version is
    // not, since the heap writes a new record for the new version.
    assert_eq!(id, RowId(1));
    assert_ne!(table.state.directory[&id], first);
    assert_eq!(table.get(&snap(6, 7, &[6]), id).expect("get"), Some(row(2)));
}

#[test]
fn two_updates_by_same_txn_ok() {
    let (_dir, storage) = instance("version-two-updates");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");

    table.update(TxnId(6), id, &row(2)).expect("first update");
    table.update(TxnId(6), id, &row(3)).expect("second update");
    assert_eq!(
        table.latest_version(id).expect("latest"),
        Some((TxnId(6), row(3)))
    );
    assert_eq!(
        chain(&table),
        vec![
            (id.0, 1, 4, Some(6)),
            (id.0, 2, 6, Some(6)),
            (id.0, 3, 6, None),
        ]
    );
    assert_eq!(table.get(&snap(6, 7, &[6]), id).expect("get"), Some(row(3)));
    assert_eq!(table.get(&snap(8, 9, &[6]), id).expect("get"), Some(row(1)));
}

#[test]
fn rollback_undoes_update() {
    let (_dir, storage) = instance("version-rollback-update");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");
    table.update(TxnId(6), id, &row(2)).expect("update");
    table.rollback(TxnId(6)).expect("rollback");

    assert_eq!(chain(&table), vec![(id.0, 1, 4, None)]);
    assert_eq!(table.get(&snap(8, 9, &[]), id).expect("get"), Some(row(1)));
    assert_eq!(
        table.latest_version(id).expect("latest"),
        Some((TxnId(4), row(1)))
    );
}

#[test]
fn delete_visibility() {
    let (_dir, storage) = instance("version-delete");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");

    table.delete(TxnId(6), id).expect("delete");
    // Hidden from the writer at once, still there for a snapshot that has not settled 6.
    assert_eq!(table.get(&snap(6, 7, &[6]), id).expect("get"), None);
    assert!(table.scan(&snap(6, 7, &[6])).expect("scan").is_empty());
    assert_eq!(table.get(&snap(8, 9, &[6]), id).expect("get"), Some(row(1)));
    assert_eq!(
        table.latest_version(id).expect("latest"),
        Some((TxnId(6), row(1)))
    );

    table.rollback(TxnId(6)).expect("rollback");
    assert_eq!(table.get(&snap(6, 7, &[6]), id).expect("get"), Some(row(1)));
    assert_eq!(chain(&table), vec![(id.0, 1, 4, None)]);

    // Committed this time: hidden from a snapshot taken after it.
    table.delete(TxnId(7), id).expect("delete");
    table.commit(TxnId(7)).expect("commit");
    assert_eq!(table.get(&snap(8, 9, &[]), id).expect("get"), None);
    assert_eq!(chain(&table), vec![(id.0, 1, 4, Some(7))]);
}

#[test]
fn update_on_uncommitted_other_is_bug() {
    let (_dir, storage) = instance("version-conflict");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");
    table.delete(TxnId(6), id).expect("delete by T1");

    let before = chain(&table);
    let message = bug_message(table.update(TxnId(7), id, &row(2)).expect_err("T2 update"));
    assert!(message.contains("is stale"), "{message}");
    assert_eq!(chain(&table), before);

    // The same refusal for a row another transaction inserted and has not committed.
    let fresh = table.insert(TxnId(6), &row(3)).expect("insert by T1");
    let before = chain(&table);
    let message = bug_message(
        table
            .update(TxnId(7), fresh, &row(4))
            .expect_err("T2 update"),
    );
    assert!(
        message.contains("being written by transaction 6"),
        "{message}"
    );
    assert_eq!(chain(&table), before);
}

// --- Scan, latest_version and preconditions -----------------------------------------

#[test]
fn scan_lists_visible_rows_in_row_id_order() {
    let (_dir, storage) = instance("version-scan");
    let mut table = table_of(&storage);
    let mut ids = Vec::new();
    for value in 1..=5 {
        ids.push(table.insert(TxnId(4), &row(value)).expect("insert"));
    }
    table.delete(TxnId(4), ids[1]).expect("delete");
    table.update(TxnId(4), ids[3], &row(40)).expect("update");
    table.commit(TxnId(4)).expect("commit");

    assert_eq!(
        table.scan(&snap(9, 100, &[])).expect("scan"),
        vec![
            (ids[0], row(1)),
            (ids[2], row(3)),
            (ids[3], row(40)),
            (ids[4], row(5)),
        ]
    );
}

#[test]
fn latest_version_is_the_tail_of_the_chain() {
    let (_dir, storage) = instance("version-latest");
    let mut table = table_of(&storage);
    assert_eq!(table.latest_version(RowId(1)).expect("unknown row"), None);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    assert_eq!(
        table.latest_version(id).expect("latest"),
        Some((TxnId(4), row(1)))
    );
    table.commit(TxnId(4)).expect("commit");
    table.delete(TxnId(6), id).expect("delete");
    // The deleter, not the creator: `txn` reads this to find the writer of a conflict.
    assert_eq!(
        table.latest_version(id).expect("latest"),
        Some((TxnId(6), row(1)))
    );
}

#[test]
fn a_long_row_goes_through_overflow_and_reads_back() {
    let (_dir, storage) = instance("version-overflow");
    let shape = TableShape {
        columns: vec![TypeInfo::new(SqlType::VarChar(Len::Max), true)],
        clustered_key: None,
    };
    let mut table = table_shaped(&storage, shape);
    let long = Row(vec![Value::String(SqlString {
        text: "v".repeat(20_000),
    })]);
    let id = table.insert(TxnId(4), &long).expect("insert");
    table.commit(TxnId(4)).expect("commit");
    assert_eq!(table.get(&snap(9, 100, &[]), id).expect("get"), Some(long));
}

#[test]
fn wrong_arity_and_unknown_row_are_bugs() {
    let (_dir, storage) = instance("version-preconditions");
    let mut table = table_of(&storage);
    let wide = Row(vec![Value::I32(1), Value::I32(2)]);
    let message = bug_message(table.insert(TxnId(4), &wide).expect_err("arity"));
    assert!(message.contains("table 9 has 1 columns"), "{message}");
    assert!(chain(&table).is_empty());

    let message = bug_message(
        table
            .update(TxnId(4), RowId(3), &row(1))
            .expect_err("unknown"),
    );
    assert!(message.contains("unknown row 3"), "{message}");
    let message = bug_message(table.delete(TxnId(4), RowId(3)).expect_err("unknown"));
    assert!(message.contains("unknown row 3"), "{message}");
}

#[test]
fn reserved_txn_id_is_a_bug() {
    let (_dir, storage) = instance("version-reserved-txn");
    let mut table = table_of(&storage);
    let reserved = TxnId(NO_XMAX);
    for message in [
        bug_message(table.insert(reserved, &row(1)).expect_err("insert")),
        bug_message(
            table
                .update(reserved, RowId(1), &row(1))
                .expect_err("update"),
        ),
        bug_message(table.delete(reserved, RowId(1)).expect_err("delete")),
        bug_message(table.commit(reserved).expect_err("commit")),
        bug_message(table.rollback(reserved).expect_err("rollback")),
    ] {
        assert!(message.contains("reserved id"), "{message}");
    }
    assert!(chain(&table).is_empty());
}

#[test]
fn a_second_commit_is_a_bug() {
    let (_dir, storage) = instance("version-second-commit");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");
    for message in [
        bug_message(table.commit(TxnId(4)).expect_err("second commit")),
        bug_message(table.rollback(TxnId(4)).expect_err("rollback after commit")),
        bug_message(
            table
                .insert(TxnId(4), &row(2))
                .expect_err("insert after commit"),
        ),
        bug_message(
            table
                .update(TxnId(4), id, &row(2))
                .expect_err("update after commit"),
        ),
        bug_message(table.delete(TxnId(4), id).expect_err("delete after commit")),
    ] {
        assert!(message.contains("is already Committed"), "{message}");
    }
    assert_eq!(chain(&table), vec![(id.0, 1, 4, None)]);
}

#[test]
fn the_overflow_pages_of_an_uncommitted_long_row_are_not_flagged() {
    let (_dir, storage) = instance("version-overflow-no-steal");
    let shape = TableShape {
        columns: vec![TypeInfo::new(SqlType::VarChar(Len::Max), true)],
        clustered_key: None,
    };
    let mut table = table_shaped(&storage, shape);
    let long = Row(vec![Value::String(SqlString {
        text: "v".repeat(20_000),
    })]);
    let id = table.insert(TxnId(4), &long).expect("insert");
    let stub_page = table.state.directory[&id].page;

    storage.pool.flush_all().expect("flush while 4 runs");
    // The heap page holding the stub is flagged, so it stays out of `data`; the pages of
    // the overflow chain are not, so the payload of the uncommitted row is written.
    let file_page = storage
        .data
        .read_page(stub_page)
        .expect("read the stub page");
    assert_eq!(file_page.slot_count(), 0);
    let pages = storage.control().expect("control block").next_page_id.0;
    let with_payload = (0..pages)
        .filter(|id| {
            storage.data.read_page(PageId(*id)).is_ok_and(|page| {
                page.0
                    .windows(1_000)
                    .any(|run| run.iter().all(|b| *b == b'v'))
            })
        })
        .count();
    assert!(with_payload > 0, "the overflow chain stayed out of `data`");
}

#[test]
fn a_page_shared_by_two_txns_loses_its_flag_at_the_first_commit() {
    let (_dir, storage) = instance("version-shared-page");
    let mut table = table_of(&storage);
    storage.pool.flush_all().expect("flush the empty page");
    let first = table.insert(TxnId(4), &row(1)).expect("insert by 4");
    let second = table.insert(TxnId(5), &row(2)).expect("insert by 5");
    let page = table.state.directory[&first].page;
    assert_eq!(page, table.state.directory[&second].page);

    table.commit(TxnId(4)).expect("commit 4");
    storage.pool.flush_all().expect("flush while 5 runs");
    // The flag belongs to the page, not to a transaction: the commit of 4 cleared it and
    // the record of 5, which has not committed, went to `data` with the one of 4. The
    // journal does not hold that page back either: the commit of 4 flushed it up to its
    // own record, and the records of 5 were appended before that one.
    assert_eq!(storage.data.read_page(page).expect("read").slot_count(), 2);
    let records = journal(&storage);
    assert_eq!(
        kinds(&records),
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Commit,
        ]
    );
    // The row of 5 sits in `data` while its transaction has no record that ends it: redo
    // alone would leave it there, which is the shape the recovery answers by the register.
    assert!(!records.iter().any(|record| record.txn == TxnId(5)
        && matches!(record.kind, WalRecordKind::Commit | WalRecordKind::Abort)));
    assert_eq!(storage.wal.durable_lsn(), Lsn(5));
}

#[test]
fn open_serves_the_rows_but_hands_out_row_id_one_again() {
    let (_dir, storage) = instance("version-open");
    let head = alloc::allocate(&storage).expect("allocate the head page");
    let mut table =
        HeapTable::create(&storage, TABLE, head, int_shape()).expect("create the table");
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");
    drop(table);

    let mut reopened = HeapTable::open(&storage, TABLE, head, int_shape());
    let after = snap(9, 100, &[]);
    // The scan walks the heap, so the row is served; the directory is empty, so what goes
    // through it is not.
    assert_eq!(reopened.get(&after, id).expect("get"), None);
    assert_eq!(reopened.scan(&after).expect("scan"), vec![(id, row(1))]);
    assert_eq!(reopened.latest_version(id).expect("latest"), None);
    assert!(matches!(
        reopened.update(TxnId(6), id, &row(2)),
        Err(InternalError::Bug(_))
    ));
    assert!(matches!(
        reopened.delete(TxnId(6), id),
        Err(InternalError::Bug(_))
    ));

    // The counter starts at 1 again: the id of the row already there is handed out twice.
    let again = reopened.insert(TxnId(6), &row(3)).expect("insert");
    reopened.commit(TxnId(6)).expect("commit");
    assert_eq!(again, RowId(1));
    assert_eq!(again, id);
    assert_eq!(
        reopened.scan(&after).expect("scan"),
        vec![(id, row(1)), (id, row(3))]
    );
}

// --- No-steal -----------------------------------------------------------------------

#[test]
fn in_progress_keeps_an_uncommitted_row_out_of_the_data_file() {
    let (_dir, storage) = instance("version-no-steal");
    let mut table = table_of(&storage);
    // The empty page is written first, so what the file holds next comes from the row.
    storage.pool.flush_all().expect("flush the empty page");
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    let page = table.state.directory[&id].page;

    storage.pool.flush_all().expect("flush while 4 runs");
    let before = storage.data.read_page(page).expect("read the page");
    assert_eq!(before.slot_count(), 0);

    table.commit(TxnId(4)).expect("commit");
    storage.pool.flush_all().expect("flush after the commit");
    let after = storage.data.read_page(page).expect("read the page");
    assert_eq!(after.slot_count(), 1);
}

// --- Write-ahead logging ------------------------------------------------------------

#[test]
fn commit_fsyncs_wal() {
    let (dir, storage) = instance("version-commit-fsync");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");

    // The instance is still open and `data` was not synced: what the file holds is what
    // the commit put there.
    let records = journal_on_disk(&dir);
    assert_eq!(
        kinds(&records),
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Commit
        ]
    );
    assert_eq!(
        records.iter().map(|record| record.txn).collect::<Vec<_>>(),
        vec![TxnId(4); 3]
    );
    assert_eq!(
        records.iter().map(|record| record.lsn).collect::<Vec<_>>(),
        vec![Lsn(1), Lsn(2), Lsn(3)]
    );
    // `Begin` and `Commit` carry an empty payload; the `Insert` carries the version.
    assert!(records[0].payload.is_empty());
    assert!(records[2].payload.is_empty());
    let change = RowChange::decode(&records[1].payload).expect("decode the insert payload");
    assert_eq!(change.table, TABLE);
    assert_eq!(change.header.row, id);
    assert_eq!(change.header.xmin, TxnId(4));
    assert_eq!(change.header.xmax, None);
    assert_eq!(change.header.seq, 1);
    assert_eq!(decode_row(&change.columns).expect("decode"), row(1));
    // The commit moved the durable LSN to its own record.
    assert_eq!(storage.wal.durable_lsn(), Lsn(3));
}

#[test]
fn insert_without_commit_not_on_data() {
    let (dir, storage) = instance("version-insert-not-on-data");
    let mut table = table_of(&storage);
    let page = table.heap().first_page();
    storage.pool.flush_all().expect("flush the empty page");
    let before = storage.data.read_page(page).expect("read the page");

    table.insert(TxnId(4), &row(1)).expect("insert");
    storage.pool.flush_all().expect("flush while 4 runs");
    // Byte for byte the image the page had before the insert.
    assert_eq!(
        storage.data.read_page(page).expect("read the page"),
        before,
        "the row of a running transaction reached `data`"
    );
    // The journal file holds the two records of the write, which the heap page does not
    // carry into `data` yet; the commit is what would sync them.
    assert_eq!(
        kinds(&journal_on_disk(&dir)),
        vec![WalRecordKind::Begin, WalRecordKind::Insert]
    );
    assert_eq!(storage.wal.durable_lsn(), Lsn(0));
    assert_eq!(storage.wal.last_lsn().expect("last lsn"), Lsn(2));
}

#[test]
fn committed_page_can_evict() {
    let (_dir, storage) = instance_of("version-evict", 1);
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    let page = table.state.directory[&id].page;
    table.commit(TxnId(4)).expect("commit");

    // One frame: pinning another page asks the clock for a victim, and the page of the
    // committed transaction is written on its way out, its record being durable. The
    // allocator writes the page it hands out through the file, so the pin is what needs
    // the frame.
    let other = alloc::allocate(&storage).expect("allocate another page");
    assert_ne!(other, page);
    let pin = storage
        .pool
        .pin(other)
        .expect("pin the page that was just allocated");
    assert_eq!(
        storage
            .data
            .read_page(page)
            .expect("read the heap page")
            .slot_count(),
        1
    );
    // And `flush_all` after a commit writes it in any case.
    drop(pin);
    storage.pool.flush_all().expect("flush after the commit");
    assert_eq!(
        storage
            .data
            .read_page(page)
            .expect("read the heap page")
            .slot_count(),
        1
    );
}

#[test]
fn flush_page_before_wal_flush_is_bug() {
    let (_dir, storage) = instance("version-flush-before-wal");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    let page = table.state.directory[&id].page;
    // No-steal first: the flag is what makes `flush_all` skip the page, without an error.
    storage.pool.flush_all().expect("flush_all skips the page");

    // The flag cleared by hand, the write-ahead rule is what is left to refuse the page.
    storage
        .pool
        .set_in_progress(page, false)
        .expect("clear the flag by hand");
    let err = storage
        .pool
        .flush(page)
        .expect_err("the record of the insert is not durable");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("ahead of the log"), "{err}");
    assert!(err.to_string().contains("durable up to 0"), "{err}");
    assert_eq!(
        storage
            .data
            .read_page(page)
            .expect("read the heap page")
            .slot_count(),
        0
    );

    // The counter-check: the same page and the same flush, once the commit flushed the
    // journal.
    table.commit(TxnId(4)).expect("commit");
    storage.pool.flush(page).expect("the record is durable now");
    assert_eq!(
        storage
            .data
            .read_page(page)
            .expect("read the heap page")
            .slot_count(),
        1
    );
}

#[test]
fn rollback_appends_abort() {
    let (dir, storage) = instance("version-rollback-abort");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.rollback(TxnId(4)).expect("rollback");

    assert_eq!(
        kinds(&journal_on_disk(&dir)),
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Abort,
            WalRecordKind::Compensation
        ]
    );
    // The `Abort` is flushed like a `Commit`, so a `Begin` left without either is a
    // transaction a crash interrupted, for the recovery; the compensation of the undo is
    // flushed after it.
    assert_eq!(storage.wal.durable_lsn(), Lsn(4));
    assert!(chain(&table).is_empty());
    assert_eq!(table.get(&snap(9, 100, &[]), id).expect("get"), None);
    assert_eq!(table.status(TxnId(4)), TxnStatus::Aborted);
}

#[test]
fn an_update_and_a_delete_are_journalled_with_their_versions() {
    let (_dir, storage) = instance("version-update-delete-journal");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit 4");
    table.update(TxnId(5), id, &row(2)).expect("update");
    table.delete(TxnId(5), id).expect("delete");
    table.commit(TxnId(5)).expect("commit 5");

    let records = journal(&storage);
    assert_eq!(
        kinds(&records),
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Commit,
            WalRecordKind::Begin,
            WalRecordKind::Update,
            WalRecordKind::Delete,
            WalRecordKind::Commit,
        ]
    );
    // The `Update` names the version it creates: seq 2, written by 5, with its content.
    let update = RowChange::decode(&records[4].payload).expect("decode the update payload");
    assert_eq!(update.header.row, id);
    assert_eq!(update.header.xmin, TxnId(5));
    assert_eq!(update.header.xmax, None);
    assert_eq!(update.header.seq, 2);
    assert_eq!(decode_row(&update.columns).expect("decode"), row(2));
    // The `Delete` names the version it hides: seq 2, created by 5, hidden by 5, and it
    // carries no column.
    let delete = RowChange::decode(&records[5].payload).expect("decode the delete payload");
    assert_eq!(delete.header.row, id);
    assert_eq!(delete.header.xmin, TxnId(5));
    assert_eq!(delete.header.xmax, Some(TxnId(5)));
    assert_eq!(delete.header.seq, 2);
    assert!(delete.columns.is_empty());
}

#[test]
fn a_commit_without_a_write_appends_no_record() {
    let (_dir, storage) = instance("version-commit-no-write");
    let mut table = table_of(&storage);
    table
        .commit(TxnId(4))
        .expect("commit a transaction that wrote nothing");
    assert!(journal(&storage).is_empty());
    assert_eq!(storage.wal.last_lsn().expect("last lsn"), Lsn(0));

    // The second commit is the bug the trait names, and it appends nothing either.
    let err = table.commit(TxnId(4)).expect_err("second commit");
    assert!(bug_message(err).contains("is already Committed"));
    assert!(journal(&storage).is_empty());

    // A rollback of a transaction that wrote nothing is the same.
    table
        .rollback(TxnId(7))
        .expect("rollback a transaction that wrote nothing");
    assert!(journal(&storage).is_empty());
}

#[test]
fn a_transaction_over_two_heap_pages_keeps_the_flag_on_both_of_them() {
    let (_dir, storage) = instance("version-two-pages");
    let mut table = table_of(&storage);
    let mut other = table_shaped(&storage, int_shape());

    // 400 rows of one `int` do not fit in one 8 192-byte page: the heap grows and chains
    // a fresh page, which is where `Heap::link_page` used to clear the flag of the page
    // left behind.
    let mut pages = BTreeSet::new();
    for value in 0..400 {
        let id = table.insert(TxnId(4), &row(value)).expect("insert");
        pages.insert(table.state.directory[&id].page);
    }
    assert!(pages.len() >= 2, "one page held the 400 rows: {pages:?}");

    // While 4 runs, `flush_all` walks past the pages of 4 without writing them and
    // without an error: before the fix it answered `Bug` on the page left behind, which
    // carried an LSN the journal had not covered.
    storage.pool.flush_all().expect("flush_all while 4 runs");
    for page in &pages {
        assert_eq!(
            storage.data.read_page(*page).expect("read").slot_count(),
            0,
            "page {page} of the running transaction reached `data`"
        );
    }

    // Another transaction, on another table, commits: that flushes the journal past the
    // records of 4. The pages of 4 stay behind their flag just the same.
    other.insert(TxnId(5), &row(1)).expect("insert by 5");
    other.commit(TxnId(5)).expect("commit 5");
    storage
        .pool
        .flush_all()
        .expect("flush_all after the commit of 5");
    for page in &pages {
        assert_eq!(
            storage.data.read_page(*page).expect("read").slot_count(),
            0,
            "page {page} of the running transaction reached `data` after the commit of 5"
        );
    }

    // The commit of 4 is what lets them out.
    table.commit(TxnId(4)).expect("commit 4");
    storage
        .pool
        .flush_all()
        .expect("flush_all after the commit of 4");
    let written: usize = pages
        .iter()
        .map(|page| usize::from(storage.data.read_page(*page).expect("read").slot_count()))
        .sum();
    assert_eq!(written, 400);
}

#[test]
fn a_row_whose_record_is_past_the_payload_limit_is_a_bug() {
    let (_dir, storage) = instance("version-payload-limit");
    let shape = TableShape {
        columns: vec![TypeInfo::new(SqlType::VarChar(Len::Max), true)],
        clustered_key: None,
    };
    let mut table = table_shaped(&storage, shape);
    let long = Row(vec![Value::String(SqlString {
        text: "v".repeat(2 * 1024 * 1024),
    })]);

    let message = bug_message(table.insert(TxnId(4), &long).expect_err("2 MiB of text"));
    assert!(message.contains("payload bytes"), "{message}");
    // The record is appended before the heap is changed, so the heap is untouched and the
    // next insert goes through.
    assert!(chain(&table).is_empty());
    let short = Row(vec![Value::String(SqlString {
        text: "v".to_string(),
    })]);
    let id = table.insert(TxnId(4), &short).expect("a row that fits");
    assert_eq!(id, RowId(1));
    // The `Begin` of 4 was appended before the refused record, and 4 is still running.
    assert_eq!(
        kinds(&journal(&storage)),
        vec![WalRecordKind::Begin, WalRecordKind::Insert]
    );
    assert_eq!(table.status(TxnId(4)), TxnStatus::InProgress);
}

#[test]
fn the_page_of_a_write_carries_the_lsn_of_its_record() {
    let (_dir, storage) = instance("version-page-lsn");
    let mut table = table_of(&storage);
    let id = table.insert(TxnId(4), &row(1)).expect("insert");
    let page = table.state.directory[&id].page;
    // `Begin` is Lsn(1), the `Insert` that changed the page is Lsn(2).
    let pin = storage.pool.pin(page).expect("pin the heap page");
    assert_eq!(
        pin.with_page(|page| page.lsn()).expect("read the lsn"),
        Lsn(2)
    );
    drop(pin);

    table.update(TxnId(4), id, &row(2)).expect("update");
    let pin = storage.pool.pin(page).expect("pin the heap page");
    assert_eq!(
        pin.with_page(|page| page.lsn()).expect("read the lsn"),
        Lsn(3)
    );
}

// --- Records of the slots an undo or a vacuum frees -----------------------------------

/// The payloads of the records of `kind` the journal holds, in order, with their `txn`.
fn releases(storage: &DiskStorage, kind: WalRecordKind) -> Vec<(TxnId, SlotRelease)> {
    journal(storage)
        .iter()
        .filter(|record| record.kind == kind)
        .map(|record| {
            (
                record.txn,
                SlotRelease::decode(&record.payload).expect("a slot release payload"),
            )
        })
        .collect()
}

/// The `(row, seq, xmax, release)` a release names.
fn named(release: &SlotRelease) -> (u64, u64, Option<u64>, Release) {
    (
        release.header.row.0,
        release.header.seq,
        release.header.xmax.map(|x| x.0),
        release.release,
    )
}

#[test]
fn a_rollback_journals_one_compensation_per_version_it_takes_out() {
    let (_dir, storage) = instance("version-compensation");
    let mut table = table_of(&storage);
    let updated = table.insert(TxnId(4), &row(1)).expect("insert 1");
    let deleted = table.insert(TxnId(4), &row(2)).expect("insert 2");
    table.commit(TxnId(4)).expect("commit 4");

    let inserted = table.insert(TxnId(5), &row(3)).expect("insert 3");
    table.update(TxnId(5), updated, &row(10)).expect("update 1");
    table.delete(TxnId(5), deleted).expect("delete 2");
    // Where each version sits before the undo: the records name these slots.
    let at = |table: &HeapTable<'_>, row: RowId, seq: u64| {
        *table
            .state
            .versions
            .get(&(row, seq))
            .expect("the version is in the directory")
    };
    let slots = [
        at(&table, deleted, 2),
        at(&table, updated, 4),
        at(&table, updated, 1),
        at(&table, inserted, 3),
    ];
    table.rollback(TxnId(5)).expect("rollback 5");

    let compensations = releases(&storage, WalRecordKind::Compensation);
    // The undo walks the writes backwards: the delete, the update (its new version out,
    // then the `xmax` of the version it replaced), the insert.
    assert_eq!(
        compensations
            .iter()
            .map(|(txn, release)| (txn.0, named(release)))
            .collect::<Vec<_>>(),
        vec![
            (5, (2, 2, Some(5), Release::XmaxCleared)),
            (5, (1, 4, None, Release::Removed)),
            (5, (1, 1, Some(5), Release::XmaxCleared)),
            (5, (3, 3, None, Release::Removed)),
        ]
    );
    assert_eq!(
        compensations
            .iter()
            .map(|(_, release)| release.at)
            .collect::<Vec<_>>(),
        slots
    );
    assert!(
        compensations
            .iter()
            .all(|(_, release)| release.table == TABLE)
    );
    // The records are durable once the rollback returns, the flag of the pages being down.
    let last = journal(&storage).last().expect("a record").lsn;
    assert_eq!(storage.wal.durable_lsn(), last);
    let records = journal(&storage);
    assert_eq!(
        records.last().map(|record| record.kind),
        Some(WalRecordKind::Compensation)
    );
    // The heap page carries the LSN of the last compensation, and the checkpoint writes it.
    let pin = storage.pool.pin(slots[3].page).expect("pin the page");
    assert_eq!(
        pin.with_page(super::super::page::Page::lsn).expect("lsn"),
        last
    );
    drop(pin);
    storage.checkpoint().expect("checkpoint");
    assert_eq!(
        chain(&table),
        vec![(1, 1, 4, None), (2, 2, 4, None)],
        "what the committed transaction wrote, nothing of the undone one"
    );
}

#[test]
fn a_rollback_to_a_savepoint_journals_its_compensations_and_syncs_them() {
    let (_dir, storage) = instance("version-compensation-savepoint");
    let mut table = table_of(&storage);
    table.insert(TxnId(4), &row(1)).expect("insert kept");
    let mark = table.writes_len(TxnId(4));
    table.insert(TxnId(4), &row(2)).expect("insert undone");
    table
        .rollback_to_savepoint(TxnId(4), mark)
        .expect("rollback to the savepoint");

    let compensations = releases(&storage, WalRecordKind::Compensation);
    assert_eq!(
        compensations
            .iter()
            .map(|(txn, release)| (txn.0, named(release)))
            .collect::<Vec<_>>(),
        vec![(4, (2, 2, None, Release::Removed))]
    );
    let last = journal(&storage).last().expect("a record").lsn;
    assert_eq!(storage.wal.durable_lsn(), last);
    // Nothing undone, nothing journalled.
    let before = journal(&storage).len();
    table
        .rollback_to_savepoint(TxnId(4), table.writes_len(TxnId(4)))
        .expect("an empty rollback to a savepoint");
    assert_eq!(journal(&storage).len(), before);
}

#[test]
fn a_vacuum_journals_one_purge_per_version_and_syncs_before_the_pages_leave() {
    let (_dir, storage) = instance("version-purge");
    let mut table = table_of(&storage);
    let gone = table.insert(TxnId(4), &row(1)).expect("insert 1");
    let replaced = table.insert(TxnId(4), &row(2)).expect("insert 2");
    table.commit(TxnId(4)).expect("commit 4");
    table.delete(TxnId(5), gone).expect("delete 1");
    table
        .update(TxnId(5), replaced, &row(20))
        .expect("update 2");
    table.commit(TxnId(5)).expect("commit 5");
    let before = journal(&storage).len();

    let removed = table.vacuum(TxnId(6)).expect("vacuum");
    assert_eq!(removed.len(), 2);
    let purges = releases(&storage, WalRecordKind::Purge);
    assert_eq!(
        purges
            .iter()
            .map(|(txn, release)| (txn.0, named(release)))
            .collect::<Vec<_>>(),
        vec![
            (0, (1, 1, Some(5), Release::Removed)),
            (0, (2, 2, Some(5), Release::Removed)),
        ]
    );
    assert_eq!(journal(&storage).len(), before + 2);
    let last = journal(&storage).last().expect("a record").lsn;
    assert_eq!(storage.wal.durable_lsn(), last);
    // The pages carry LSNs the journal covers: the pool writes them without complaint.
    storage
        .pool
        .flush_all()
        .expect("flush the pages of the vacuum");
    assert_eq!(chain(&table), vec![(2, 3, 5, None)]);
    // A vacuum with nothing to take out journals nothing.
    table.vacuum(TxnId(6)).expect("vacuum again");
    assert_eq!(journal(&storage).len(), before + 2);
}
