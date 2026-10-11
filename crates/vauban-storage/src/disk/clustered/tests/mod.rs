//! Tests of `ClusteredTable`: scan and seek order on the clustered key (`NULL` first,
//! descending columns last), an update that moves a row to keep that order while its
//! `RowId` stays put, MVCC visibility of inserts, deletes and rollbacks, and the WAL
//! records a transaction's writes produce.
//!
//! Tree pages follow a no-steal rule: a leaf an uncommitted transaction wrote, or split,
//! stays out of the data file until that transaction commits, and a rollback releases it
//! early. A row too long for a leaf goes through the overflow heap and reads back whole.
//! Other tests cover the preconditions `ClusteredTable::create` and `open` enforce, and
//! the on-disk shape of a version payload (prefix, overflow link, column bytes).

use std::path::Path;

use vauban_types::{SqlString, SqlType};

use super::super::page::{PAGE_SIZE, Page, PageKind};
use super::super::temp::TempDir;
use super::super::wal::{PageWrite, WalRecord, WalRecordKind};
use super::super::{DiskOptions, alloc};
use super::*;

/// The table the tests of this file work on.
const TABLE: TableId = TableId(4);

/// An empty instance in a temporary directory, with the guard that removes it.
fn instance(label: &str) -> (TempDir, DiskStorage) {
    let dir = TempDir::created(label);
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("an instance");
    (dir, storage)
}

/// An ascending key on the column 0.
fn first_column() -> Vec<KeyColumn> {
    vec![KeyColumn {
        column: 0,
        descending: false,
    }]
}

/// A table of two nullable `int` columns, clustered on the first one, ascending.
fn pair_shape() -> TableShape {
    TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::Int, true),
        ],
        clustered_key: Some(first_column()),
    }
}

/// A table of the given shape over a fresh page.
fn table_of(storage: &DiskStorage, shape: TableShape) -> ClusteredTable<'_> {
    let head = alloc::allocate(storage).expect("allocate the head page");
    ClusteredTable::create(storage, TABLE, head, shape).expect("create the table")
}

/// A row of two `int`s.
fn pair(first: i32, second: i32) -> Row {
    Row(vec![Value::I32(first), Value::I32(second)])
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

/// The `RowId` of each row a scan answered, in order.
fn ids(rows: &[(RowId, Row)]) -> Vec<u64> {
    rows.iter().map(|(id, _)| id.0).collect()
}

/// The column `at` of each row a scan answered, `None` for a `NULL`.
fn column(rows: &[(RowId, Row)], at: usize) -> Vec<Option<i32>> {
    rows.iter()
        .map(|(_, row)| match row.0.get(at) {
            Some(Value::I32(number)) => Some(*number),
            Some(Value::Null) => None,
            other => panic!("a row of the scan carries {other:?} at {at}"),
        })
        .collect()
}

/// The payload of each entry of the tree of the versions, in key order.
fn entries(table: &ClusteredTable<'_>) -> Vec<VersionPayload> {
    table
        .tree
        .seek(&KeyRange::Full, Direction::Forward)
        .expect("seek the whole tree")
        .iter()
        .map(|(_, payload)| VersionPayload::decode(payload).expect("decode a payload"))
        .collect()
}

/// The records the journal of `storage` holds.
fn journal(storage: &DiskStorage) -> Vec<WalRecord> {
    storage.wal.records().expect("read the journal")
}

/// The number of slots the page `id` holds **in the data file**, which is what a flush of
/// the pool leaves there.
fn slots_in_data(storage: &DiskStorage, id: PageId) -> u16 {
    storage
        .data
        .read_page(id)
        .expect("read the page from data")
        .slot_count()
}

/// The kind of each record but the page records, in order: what the transactions wrote, the
/// pages of the trees apart.
fn kinds(records: &[WalRecord]) -> Vec<WalRecordKind> {
    records
        .iter()
        .map(|record| record.kind)
        .filter(|kind| *kind != WalRecordKind::PageWrite)
        .collect()
}

/// Makes the journal durable, then writes each page the pool may write: what a checkpoint
/// does with the pages.
fn settle(storage: &DiskStorage) {
    storage.wal.flush().expect("sync the journal");
    storage.pool.flush_all().expect("flush the pool");
}

/// The message of an error the test expects to be a caller bug.
fn bug_message(error: InternalError) -> String {
    match error {
        InternalError::Bug(message) => message,
        other => panic!("expected a Bug, got {other:?}"),
    }
}

// --- Placement and order ---------------------------------------------------------------

#[test]
fn scan_follows_clustered_key() {
    let (_dir, storage) = instance("clustered-scan");
    let mut table = table_of(&storage, pair_shape());
    let two = table
        .insert(TxnId(1), &pair(2, 20))
        .expect("insert (2, 20)");
    let first_one = table
        .insert(TxnId(1), &pair(1, 11))
        .expect("insert (1, 11)");
    let second_one = table
        .insert(TxnId(1), &pair(1, 12))
        .expect("insert (1, 12)");
    table.commit(TxnId(1)).expect("commit");

    let rows = table.scan(&snap(9, 9, &[])).expect("scan");
    assert_eq!(column(&rows, 0), vec![Some(1), Some(1), Some(2)]);
    assert_eq!(column(&rows, 1), vec![Some(11), Some(12), Some(20)]);
    // The two rows of key 1 come back in `RowId` order, and the row inserted first comes
    // last: the order is the key's, not the one of the writes.
    assert_eq!(ids(&rows), vec![first_one.0, second_one.0, two.0]);
    assert_eq!((two.0, first_one.0, second_one.0), (1, 2, 3));
}

#[test]
fn nulls_first_descending_last() {
    let (_dir, storage) = instance("clustered-nulls");
    let ascending = TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: Some(first_column()),
    };
    let descending = TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: Some(vec![KeyColumn {
            column: 0,
            descending: true,
        }]),
    };
    let mut up = table_of(&storage, ascending);
    let mut down = table_of(&storage, descending);
    // One transaction per table: the register of the transactions is per instance, so
    // one identifier takes one `Commit`, whichever table wrote under it
    // (`super::super::tests::one_begin_one_end_per_txn_over_two_tables`).
    for value in [Value::I32(1), Value::Null, Value::I32(2)] {
        let row = Row(vec![value]);
        up.insert(TxnId(1), &row)
            .expect("insert in the ascending table");
        down.insert(TxnId(2), &row)
            .expect("insert in the descending table");
    }
    up.commit(TxnId(1)).expect("commit the ascending table");
    down.commit(TxnId(2)).expect("commit the descending table");

    let reader = snap(9, 9, &[]);
    // `NULL` first on the ascending column, last on the descending one: the flag of the
    // key column is what moves it.
    assert_eq!(
        column(&up.scan(&reader).expect("scan"), 0),
        vec![None, Some(1), Some(2)]
    );
    assert_eq!(
        column(&down.scan(&reader).expect("scan"), 0),
        vec![Some(2), Some(1), None]
    );
}

// --- An update that moves a row --------------------------------------------------------

#[test]
fn update_key_moves_row_same_row_id() {
    let (_dir, storage) = instance("clustered-move");
    let mut table = table_of(&storage, pair_shape());
    let id = table
        .insert(TxnId(1), &pair(1, 10))
        .expect("insert (1, 10)");
    table
        .insert(TxnId(1), &pair(5, 50))
        .expect("insert (5, 50)");
    table
        .commit(TxnId(1))
        .expect("commit the writer of the rows");

    table
        .update(TxnId(2), id, &pair(9, 10))
        .expect("move the row from key 1 to key 9");
    // A snapshot that does not settle transaction 2 reads the row under its old key.
    let older = table.scan(&snap(9, 9, &[2])).expect("scan");
    assert_eq!(column(&older, 0), vec![Some(1), Some(5)]);
    assert_eq!(ids(&older), vec![id.0, 2]);

    table.commit(TxnId(2)).expect("commit the update");
    let newer = table.scan(&snap(9, 9, &[])).expect("scan");
    assert_eq!(column(&newer, 0), vec![Some(5), Some(9)]);
    // The row kept its `RowId` while its place changed.
    assert_eq!(ids(&newer), vec![2, id.0]);
    assert_eq!(
        table.get(&snap(9, 9, &[]), id).expect("get"),
        Some(pair(9, 10))
    );
}

#[test]
fn get_by_row_id_after_move() {
    let (_dir, storage) = instance("clustered-get-after-move");
    let mut table = table_of(&storage, pair_shape());
    let id = table
        .insert(TxnId(1), &pair(1, 10))
        .expect("insert (1, 10)");
    table.commit(TxnId(1)).expect("commit the insert");
    table
        .update(TxnId(2), id, &pair(9, 10))
        .expect("move the row to key 9");
    table.commit(TxnId(2)).expect("commit the update");

    // The directory names the version of the greatest `seq`, wherever the new key put it.
    assert_eq!(
        table.get(&snap(9, 9, &[]), id).expect("get"),
        Some(pair(9, 10))
    );
    // A snapshot that does not settle transaction 2 walks the link back to the version
    // under the old key.
    assert_eq!(
        table.get(&snap(9, 9, &[2]), id).expect("get"),
        Some(pair(1, 10))
    );
    // The two versions are both in the tree, under the two keys.
    assert_eq!(entries(&table).len(), 2);
    assert_eq!(
        table.get(&snap(9, 9, &[]), RowId(7)).expect("get"),
        None,
        "a row the directory does not hold"
    );
}

/// A version is found by its row and its `seq` by following the chain from the directory,
/// under whichever key each version holds; a `seq` the chain does not hold answers `None`, and
/// the versions of one row come back oldest first.
#[test]
fn version_by_row_and_seq_follows_the_chain() {
    let (_dir, storage) = instance("clustered-version-by-seq");
    let mut table = table_of(&storage, pair_shape());
    let id = table.insert(TxnId(1), &pair(1, 10)).expect("insert");
    table.commit(TxnId(1)).expect("commit the insert");
    table.update(TxnId(2), id, &pair(9, 10)).expect("move to 9");
    table.commit(TxnId(2)).expect("commit the first update");
    table.update(TxnId(3), id, &pair(5, 11)).expect("move to 5");
    table.commit(TxnId(3)).expect("commit the second update");
    let other = table
        .insert(TxnId(4), &pair(3, 30))
        .expect("insert another row");
    table.commit(TxnId(4)).expect("commit the other insert");

    let seqs: Vec<u64> = table
        .versions(Some(id))
        .expect("versions of the row")
        .iter()
        .map(|version| version.seq)
        .collect();
    assert_eq!(seqs.len(), 3);
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    let expected = [pair(1, 10), pair(9, 10), pair(5, 11)];
    for (seq, data) in seqs.iter().zip(expected) {
        let version = table
            .version(id, *seq)
            .expect("version")
            .expect("the chain holds it");
        assert_eq!((version.row, version.seq, version.data), (id, *seq, data));
    }
    let oldest = table.version(id, seqs[0]).expect("oldest").expect("held");
    assert_eq!((oldest.xmin, oldest.xmax), (TxnId(1), Some(TxnId(2))));
    assert_eq!(table.version(id, 99).expect("unknown seq"), None);
    assert_eq!(table.version(RowId(77), 1).expect("unknown row"), None);
    // The `seq` of another row is not a version of this one.
    let other_seq = table.versions(Some(other)).expect("other")[0].seq;
    assert_eq!(
        table.version(id, other_seq).expect("seq of another row"),
        None
    );
}

#[test]
fn splits_keep_the_order_and_the_directory() {
    let (_dir, storage) = instance("clustered-splits");
    let mut table = table_of(&storage, pair_shape());
    let root = table.tree_root();
    // 400 rows whose keys arrive in a scattered order: enough entries for the leaves to
    // split, which moves them from one page to another.
    let keys: Vec<i32> = (0..400).map(|step| (step * 97) % 400).collect();
    let mut ids = Vec::new();
    for key in &keys {
        ids.push(
            table
                .insert(TxnId(1), &pair(*key, key * 2))
                .expect("insert"),
        );
    }
    table.commit(TxnId(1)).expect("commit");
    assert_ne!(
        table.tree_root(),
        root,
        "the root moved, so a split reached it"
    );

    let reader = snap(9, 9, &[]);
    let rows = table.scan(&reader).expect("scan");
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(
        column(&rows, 0),
        sorted.iter().map(|key| Some(*key)).collect::<Vec<_>>()
    );
    // The directory still names each row, the entries having moved between pages.
    for (key, id) in keys.iter().zip(&ids) {
        assert_eq!(
            table.get(&reader, *id).expect("get"),
            Some(pair(*key, key * 2)),
            "row {id}"
        );
    }
}

// --- Transactions ----------------------------------------------------------------------

#[test]
fn an_insert_is_visible_to_its_writer_then_to_a_later_snapshot() {
    let (_dir, storage) = instance("clustered-insert-visibility");
    let mut table = table_of(&storage, pair_shape());
    let id = table.insert(TxnId(3), &pair(1, 10)).expect("insert");

    let own = snap(3, 3, &[]);
    let other = snap(9, 9, &[3]);
    assert_eq!(table.get(&own, id).expect("get"), Some(pair(1, 10)));
    assert_eq!(table.get(&other, id).expect("get"), None);
    assert_eq!(ids(&table.scan(&own).expect("scan")), vec![id.0]);
    assert!(table.scan(&other).expect("scan").is_empty());

    table.commit(TxnId(3)).expect("commit");
    let later = snap(9, 9, &[]);
    assert_eq!(table.get(&later, id).expect("get"), Some(pair(1, 10)));
}

#[test]
fn a_delete_hides_the_row_from_the_snapshot_that_settles_it() {
    let (_dir, storage) = instance("clustered-delete");
    let mut table = table_of(&storage, pair_shape());
    let id = table.insert(TxnId(1), &pair(1, 10)).expect("insert");
    table.commit(TxnId(1)).expect("commit the insert");
    table.delete(TxnId(2), id).expect("delete");
    table.commit(TxnId(2)).expect("commit the delete");

    assert_eq!(table.get(&snap(9, 9, &[]), id).expect("get"), None);
    assert!(table.scan(&snap(9, 9, &[])).expect("scan").is_empty());
    // The entry stays where it was: a snapshot that does not settle the deleter reads it.
    assert_eq!(entries(&table).len(), 1);
    assert_eq!(
        table.get(&snap(9, 9, &[2]), id).expect("get"),
        Some(pair(1, 10))
    );
}

#[test]
fn rollback_restore_old_key() {
    let (_dir, storage) = instance("clustered-rollback-update");
    let mut table = table_of(&storage, pair_shape());
    let id = table
        .insert(TxnId(1), &pair(1, 10))
        .expect("insert (1, 10)");
    table.commit(TxnId(1)).expect("commit the insert");
    table
        .update(TxnId(2), id, &pair(9, 10))
        .expect("move the row to key 9");
    assert_eq!(entries(&table).len(), 2);

    table.rollback(TxnId(2)).expect("roll the update back");
    let rows = table.scan(&snap(9, 9, &[])).expect("scan");
    assert_eq!(column(&rows, 0), vec![Some(1)]);
    assert_eq!(ids(&rows), vec![id.0]);
    // The version the update created is gone and the one it replaced carries no `xmax`.
    let left = entries(&table);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].header.xmax, None);
    assert_eq!(
        table.get(&snap(9, 9, &[]), id).expect("get"),
        Some(pair(1, 10))
    );
}

#[test]
fn rollback_undoes_an_insert_and_keeps_the_row_id_it_took() {
    let (_dir, storage) = instance("clustered-rollback-insert");
    let mut table = table_of(&storage, pair_shape());
    let id = table.insert(TxnId(2), &pair(1, 10)).expect("insert");
    table.rollback(TxnId(2)).expect("roll the insert back");

    assert!(entries(&table).is_empty());
    assert_eq!(table.get(&snap(9, 9, &[]), id).expect("get"), None);
    let next = table.insert(TxnId(3), &pair(2, 20)).expect("insert again");
    assert_eq!((id.0, next.0), (1, 2));
}

#[test]
fn the_writes_of_a_transaction_are_journalled_in_order() {
    let (_dir, storage) = instance("clustered-journal");
    let mut table = table_of(&storage, pair_shape());
    let id = table.insert(TxnId(1), &pair(1, 10)).expect("insert");
    table.update(TxnId(1), id, &pair(9, 10)).expect("update");
    table.delete(TxnId(1), id).expect("delete");
    table.commit(TxnId(1)).expect("commit");

    assert_eq!(
        kinds(&journal(&storage)),
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Update,
            WalRecordKind::Delete,
            WalRecordKind::Commit,
        ]
    );
    // A transaction that writes nothing leaves nothing behind.
    table
        .commit(TxnId(5))
        .expect("commit a transaction of no write");
    assert_eq!(kinds(&journal(&storage)).len(), 5);
}

// --- A row too long for a leaf ---------------------------------------------------------

// --- No-steal on the pages of the trees ---------------------------------------------

#[test]
fn clustered_leaf_stays_out_of_data_until_commit() {
    let (_dir, storage) = instance("clustered-no-steal");
    let mut table = table_of(&storage, pair_shape());
    let (leaf, directory) = (table.tree_root(), table.directory_root());
    // The pages the creation left are written first, so what `data` holds next comes from
    // the row.
    settle(&storage);
    assert_eq!(slots_in_data(&storage, leaf), 0);
    assert_eq!(slots_in_data(&storage, directory), 0);

    table.insert(TxnId(4), &pair(1, 2)).expect("insert");
    storage.pool.flush_all().expect("flush while 4 runs");
    assert_eq!(
        slots_in_data(&storage, leaf),
        0,
        "the leaf of a row transaction 4 has not committed reached data"
    );
    assert_eq!(
        slots_in_data(&storage, directory),
        0,
        "the directory entry of that row reached data"
    );

    table.commit(TxnId(4)).expect("commit");
    storage.pool.flush_all().expect("flush after the commit");
    assert_eq!(slots_in_data(&storage, leaf), 1);
    assert_eq!(slots_in_data(&storage, directory), 1);
    assert_eq!(
        (table.tree_root(), table.directory_root()),
        (leaf, directory),
        "a root moved: the count is not on the page the entry landed on"
    );
}

#[test]
fn an_updated_leaf_stays_out_of_data_until_commit() {
    let (_dir, storage) = instance("clustered-no-steal-update");
    let mut table = table_of(&storage, pair_shape());
    let leaf = table.tree_root();
    let id = table.insert(TxnId(4), &pair(1, 2)).expect("insert");
    table.commit(TxnId(4)).expect("commit the insert");
    storage
        .pool
        .flush_all()
        .expect("flush the committed insert");
    assert_eq!(slots_in_data(&storage, leaf), 1);

    // The update rewrites the version it hides and adds the new one: two entries in the
    // leaf held in the pool, one entry in the leaf `data` holds.
    table.update(TxnId(5), id, &pair(1, 3)).expect("update");
    storage.pool.flush_all().expect("flush while 5 runs");
    assert_eq!(
        slots_in_data(&storage, leaf),
        1,
        "the leaf of a version transaction 5 has not committed reached data"
    );

    table.commit(TxnId(5)).expect("commit the update");
    storage.pool.flush_all().expect("flush after the commit");
    assert_eq!(slots_in_data(&storage, leaf), 2);
    assert_eq!(table.tree_root(), leaf, "the root moved");
}

#[test]
fn a_rolled_back_write_lets_its_pages_reach_data() {
    let (_dir, storage) = instance("clustered-no-steal-rollback");
    let mut table = table_of(&storage, pair_shape());
    let (leaf, directory) = (table.tree_root(), table.directory_root());
    table.insert(TxnId(4), &pair(1, 2)).expect("insert");
    table.rollback(TxnId(4)).expect("roll back");

    // The undo took the entry away and the rollback lifted the flag, so the pages go out.
    // What says they went out is the LSN they carry in `data`: the last page record that
    // named the leaf, the one of the undo, made durable by the rollback.
    storage.pool.flush_all().expect("flush after the rollback");
    let records = journal(&storage);
    let undo = records
        .iter()
        .rev()
        .find(|record| {
            record.kind == WalRecordKind::PageWrite
                && PageWrite::decode(&record.payload)
                    .expect("a page record reads back")
                    .pages
                    .iter()
                    .any(|delta| delta.page == leaf)
        })
        .expect("the page record of the undo");
    let abort = records
        .iter()
        .find(|record| record.kind == WalRecordKind::Abort)
        .expect("the record of the rollback");
    assert!(undo.lsn > abort.lsn, "the undo follows the rollback");
    let page = storage
        .data
        .read_page(leaf)
        .expect("read the leaf from data");
    assert_eq!(page.lsn(), undo.lsn);
    assert_eq!(
        page.slot_count(),
        0,
        "the entry the undo took away is in data"
    );
    assert_eq!(slots_in_data(&storage, directory), 0);
}

#[test]
fn a_leaf_shared_by_two_txns_loses_its_flag_at_the_first_commit() {
    let (_dir, storage) = instance("clustered-shared-leaf");
    let mut table = table_of(&storage, pair_shape());
    let leaf = table.tree_root();
    settle(&storage);

    table.insert(TxnId(4), &pair(1, 2)).expect("insert by 4");
    table.insert(TxnId(5), &pair(2, 2)).expect("insert by 5");
    table.commit(TxnId(4)).expect("commit 4");
    storage.pool.flush_all().expect("flush while 5 runs");

    // The flag belongs to the page, not to a transaction: the commit of 4 cleared it on the
    // leaf the two share, and the entry of 5 went to `data` with the one of 4. The journal
    // holds it back no longer either, the commit of 4 having made it durable past the
    // record of 5.
    assert_eq!(slots_in_data(&storage, leaf), 2);
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
    assert!(
        !records.iter().any(|record| record.txn == TxnId(5)
            && matches!(record.kind, WalRecordKind::Commit | WalRecordKind::Abort)),
        "transaction 5 is ended by a record: the entry in `data` is not the shape asserted"
    );
}

#[test]
fn a_rollback_lets_the_leaf_of_a_running_txn_reach_data() {
    let (_dir, storage) = instance("clustered-shared-leaf-rollback");
    let mut table = table_of(&storage, pair_shape());
    let leaf = table.tree_root();
    settle(&storage);

    table.insert(TxnId(4), &pair(1, 2)).expect("insert by 4");
    table.insert(TxnId(5), &pair(2, 2)).expect("insert by 5");
    table.rollback(TxnId(5)).expect("roll 5 back");
    storage.pool.flush_all().expect("flush while 4 runs");

    // The undo of 5 took its own entry away and the rollback lifted the flag of the shared
    // leaf, so what `data` holds is the entry of 4, which is still running.
    assert_eq!(slots_in_data(&storage, leaf), 1);
    let records = journal(&storage);
    assert!(
        !records.iter().any(|record| record.txn == TxnId(4)
            && matches!(record.kind, WalRecordKind::Commit | WalRecordKind::Abort)),
        "transaction 4 is ended by a record: the entry in `data` is not the shape asserted"
    );
}

#[test]
fn the_overflow_chain_of_an_uncommitted_long_row_is_not_flagged() {
    let (_dir, storage) = instance("clustered-overflow-no-steal");
    let shape = TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::VarChar(Len::Max), true),
        ],
        clustered_key: Some(first_column()),
    };
    let mut table = table_of(&storage, shape);
    let leaf = table.tree_root();
    settle(&storage);
    let long = Row(vec![
        Value::I32(1),
        Value::String(SqlString {
            text: "Z".repeat(40_000),
        }),
    ]);
    table.insert(TxnId(4), &long).expect("insert the long row");

    storage.pool.flush_all().expect("flush while 4 runs");
    // The leaf carries the flag, so the entry that names the chain stays in the pool; the
    // pages of the chain are left unflagged, so the 40 000 characters are written.
    assert_eq!(slots_in_data(&storage, leaf), 0);
    let pages = storage.control().expect("control block").next_page_id.0;
    let with_payload = (0..pages)
        .filter(|id| {
            storage.data.read_page(PageId(*id)).is_ok_and(|page| {
                page.0
                    .windows(1_000)
                    .any(|run| run.iter().all(|byte| *byte == b'Z'))
            })
        })
        .count();
    assert!(
        with_payload > 0,
        "the payload of the uncommitted long row stayed out of `data`"
    );
}

#[test]
fn a_long_row_goes_through_the_overflow_heap_and_reads_back() {
    let (_dir, storage) = instance("clustered-overflow");
    let shape = TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::VarChar(Len::Max), true),
        ],
        clustered_key: Some(first_column()),
    };
    let mut table = table_of(&storage, shape);
    let text = |len: usize| {
        Row(vec![
            Value::I32(1),
            Value::String(SqlString {
                text: "v".repeat(len),
            }),
        ])
    };
    let (short, long) = (text(16), text(4_000));
    let mut columns = Vec::new();
    encode_row(&long, &mut columns);
    assert!(
        columns.len() > MAX_INSERT_BYTES,
        "the long row weighs {} bytes, which the entry ceiling of {MAX_INSERT_BYTES} has \
         to turn away",
        columns.len()
    );

    let small = table
        .insert(TxnId(1), &short)
        .expect("insert the short row");
    let big = table.insert(TxnId(1), &long).expect("insert the long row");
    table.commit(TxnId(1)).expect("commit");

    let bodies: Vec<bool> = entries(&table)
        .iter()
        .map(|payload| matches!(payload.body, Body::Overflow(_)))
        .collect();
    assert_eq!(bodies, vec![false, true]);
    let reader = snap(9, 9, &[]);
    assert_eq!(table.get(&reader, small).expect("get"), Some(short));
    assert_eq!(table.get(&reader, big).expect("get"), Some(long.clone()));
    assert_eq!(table.scan(&reader).expect("scan").len(), 2);

    // The record of the overflow heap goes away with the version that named it.
    table.delete(TxnId(2), big).expect("delete the long row");
    table.rollback(TxnId(2)).expect("roll the delete back");
    assert_eq!(table.get(&reader, big).expect("get"), Some(long));
}

// --- Preconditions and reopening -------------------------------------------------------

#[test]
fn a_table_without_a_clustered_key_is_a_bug() {
    let (_dir, storage) = instance("clustered-no-key");
    let head = alloc::allocate(&storage).expect("allocate the head page");
    let shape = TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: None,
    };
    let message = bug_message(
        ClusteredTable::create(&storage, TABLE, head, shape).expect_err("no clustered key"),
    );
    assert!(message.contains("no clustered key"), "{message}");

    let outside = TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: Some(vec![KeyColumn {
            column: 3,
            descending: false,
        }]),
    };
    let message =
        bug_message(ClusteredTable::create(&storage, TABLE, head, outside).expect_err("column 3"));
    assert!(message.contains("outside"), "{message}");
}

#[test]
fn a_wrong_arity_and_an_unknown_row_are_bugs() {
    let (_dir, storage) = instance("clustered-preconditions");
    let mut table = table_of(&storage, pair_shape());
    let narrow = Row(vec![Value::I32(1)]);
    let message = bug_message(table.insert(TxnId(1), &narrow).expect_err("arity"));
    assert!(message.contains("2 columns"), "{message}");
    let message = bug_message(
        table
            .update(TxnId(1), RowId(8), &pair(1, 1))
            .expect_err("unknown row"),
    );
    assert!(message.contains("unknown row 8"), "{message}");
    let message = bug_message(
        table
            .insert(TxnId(u64::MAX), &pair(1, 1))
            .expect_err("the reserved transaction id"),
    );
    assert!(message.contains("reserved"), "{message}");
}

#[test]
fn open_attaches_to_the_trees_that_create_left() {
    let (_dir, storage) = instance("clustered-open");
    let (roots, head) = {
        let mut table = table_of(&storage, pair_shape());
        table
            .insert(TxnId(1), &pair(2, 20))
            .expect("insert (2, 20)");
        table
            .insert(TxnId(1), &pair(1, 10))
            .expect("insert (1, 10)");
        table.commit(TxnId(1)).expect("commit");
        (
            (table.tree_root(), table.directory_root()),
            table.first_page(),
        )
    };

    let mut again = ClusteredTable::open(&storage, TABLE, head, roots, pair_shape())
        .expect("open the table again");
    let rows = again.scan(&snap(9, 9, &[])).expect("scan");
    assert_eq!(column(&rows, 0), vec![Some(1), Some(2)]);
    assert_eq!(ids(&rows), vec![2, 1]);
    // The counters start again at 1, the table holding them in memory: a table reopened
    // by hand hands out `RowId(1)` a second time.
    assert_eq!(
        again.insert(TxnId(2), &pair(3, 30)).expect("insert"),
        RowId(1)
    );
}

// --- The payload of an entry -----------------------------------------------------------

#[test]
fn a_payload_carries_its_prefix_its_link_and_its_columns() {
    let header = VersionHeader {
        row: RowId(7),
        xmin: TxnId(3),
        xmax: None,
        seq: 5,
    };
    let payload = VersionPayload {
        header,
        prev: vec![Value::I32(1), tie_value(7), tie_value(2)],
        body: Body::Overflow(Rid {
            page: PageId(12),
            slot: 3,
        }),
    };
    let bytes = payload.encode();
    assert_eq!(VersionPayload::decode(&bytes).expect("decode"), payload);
    assert_eq!(seq_of(&payload.prev).expect("the seq of the link"), 2);
}

#[test]
fn a_payload_cut_short_of_its_body_is_corruption() {
    let payload = VersionPayload {
        header: VersionHeader {
            row: RowId(1),
            xmin: TxnId(1),
            xmax: None,
            seq: 1,
        },
        prev: Vec::new(),
        body: Body::Inline(Vec::new()),
    };
    let bytes = payload.encode();
    for cut in [VERSION_PREFIX_LEN - 1, VERSION_PREFIX_LEN + 1] {
        let error = VersionPayload::decode(&bytes[..cut]).expect_err("a payload cut short");
        assert!(
            matches!(error, InternalError::Corruption(_)),
            "cut at {cut}: {error:?}"
        );
    }
}

/// A table of one nullable `nvarchar(10)` column under the default collation, clustered on
/// it, and one nullable `int` column.
fn text_shape() -> TableShape {
    TableShape {
        columns: vec![
            TypeInfo::new(SqlType::NVarChar(Len::Fixed(10)), true),
            TypeInfo::new(SqlType::Int, true),
        ],
        clustered_key: Some(first_column()),
    }
}

/// A row of [`text_shape`].
fn text_pair(text: &str, n: i32) -> Row {
    Row(vec![
        Value::String(SqlString {
            text: text.to_owned(),
        }),
        Value::I32(n),
    ])
}

#[test]
fn versions_of_key_compares_under_collation() {
    let (_dir, storage) = instance("clustered-versions-of-key");
    let mut table = table_of(&storage, text_shape());
    let a = table.insert(TxnId(1), &text_pair("a", 1)).expect("a");
    table.insert(TxnId(1), &text_pair("b", 2)).expect("b");
    let padded = table.insert(TxnId(1), &text_pair("A ", 3)).expect("A ");
    table
        .insert(TxnId(1), &text_pair("\u{e1}", 4))
        .expect("a acute");
    table.commit(TxnId(1)).expect("commit");
    table
        .update(TxnId(2), a, &text_pair("A", 5))
        .expect("update a");
    table.commit(TxnId(2)).expect("commit the update");

    let found = table
        .versions_of_key(&text_pair("a", 0).0[..1])
        .expect("versions of 'a'");
    let mut rows: Vec<(RowId, Option<TxnId>)> = found
        .iter()
        .map(|version| (version.row, version.xmax))
        .collect();
    rows.sort();
    // Both versions of the first row, the old one replaced by 2, and the padded row; `b`
    // and the accented row are other keys.
    assert_eq!(rows, vec![(a, None), (a, Some(TxnId(2))), (padded, None)]);
    assert!(
        table
            .versions_of_key(&text_pair("c", 0).0[..1])
            .expect("versions of 'c'")
            .is_empty()
    );
    let message = bug_message(
        table
            .versions_of_key(&text_pair("a", 0).0)
            .expect_err("two values for one clustered column"),
    );
    assert!(message.contains("clustered columns"), "{message}");
}

// --- Losers at recovery -----------------------------------------------------------------

/// A snapshot that settles each transaction below `u64::MAX - 1`.
fn settled() -> Snapshot {
    snap(0, u64::MAX - 1, &[])
}

/// What a scan of `table` answers through the trait, as `(RowId, first, second)`.
fn rows_of(storage: &DiskStorage, table: TableId) -> Vec<(RowId, Option<i32>, Option<i32>)> {
    let rows: Vec<(RowId, Row)> = crate::Storage::scan(storage, &settled(), table)
        .expect("scan")
        .map(|row| row.expect("a row of the scan"))
        .collect();
    let (first, second) = (column(&rows, 0), column(&rows, 1));
    rows.iter()
        .zip(first.into_iter().zip(second))
        .map(|((id, _), (a, b))| (*id, a, b))
        .collect()
}

/// An instance in `dir` with a heap table and a clustered table of five committed rows each,
/// then a transaction that updates row 0 of both (and deletes it when `delete` says so) and
/// updates row 1, a second one that inserts a row in both and commits, a checkpoint, and a
/// drop with the first one still running. Answers the two tables and the `RowId`s of the
/// five rows of each.
fn a_loser_wrote_both_tables(dir: &TempDir, delete: bool) -> Vec<(TableId, Vec<RowId>)> {
    use crate::Storage;
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
    let db = Storage::create_database(&storage, "a").expect("database");
    let heap_shape = TableShape {
        clustered_key: None,
        ..pair_shape()
    };
    let tables = [
        Storage::create_table(&storage, db, &heap_shape).expect("heap table"),
        Storage::create_table(&storage, db, &pair_shape()).expect("clustered table"),
    ];
    Storage::checkpoint(&storage).expect("checkpoint");
    let first = TxnId(Storage::next_txn_id(&storage));
    let rows: Vec<(TableId, Vec<RowId>)> = tables
        .iter()
        .map(|&table| {
            let ids = (0..5)
                .map(|k| {
                    Storage::insert(&storage, first, table, &pair(k, 100 + k)).expect("insert")
                })
                .collect();
            (table, ids)
        })
        .collect();
    Storage::commit(&storage, first).expect("commit the rows");
    let loser = TxnId(Storage::next_txn_id(&storage));
    for (table, ids) in &rows {
        Storage::update(&storage, loser, *table, ids[0], &pair(0, 999)).expect("update");
        if delete {
            Storage::delete(&storage, loser, *table, ids[0]).expect("delete");
        }
        Storage::update(&storage, loser, *table, ids[1], &pair(1, 998)).expect("update");
    }
    let winner = TxnId(Storage::next_txn_id(&storage));
    for &table in &tables {
        Storage::insert(&storage, winner, table, &pair(500, 50)).expect("insert");
    }
    Storage::commit(&storage, winner).expect("commit the winner");
    Storage::checkpoint(&storage).expect("checkpoint with the loser running");
    rows
}

/// The versions the tree of `table` holds whose `xmin` or `xmax` is `txn`.
fn versions_of_txn(storage: &DiskStorage, table: TableId, txn: TxnId) -> usize {
    let entry = storage
        .lock_catalogue()
        .expect("catalogue")
        .table(table)
        .cloned()
        .expect("the table");
    let roots = (
        entry.tree_root.expect("tree"),
        entry.directory_root.expect("dir"),
    );
    let clustered = ClusteredTable::open(storage, table, entry.first_page, roots, entry.shape)
        .expect("open the table");
    entries(&clustered)
        .iter()
        .filter(|payload| payload.header.xmin == txn || payload.header.xmax == Some(txn))
        .count()
}

#[test]
fn a_clustered_row_a_loser_wrote_takes_writes_and_rollbacks_after_recovery() {
    use crate::Storage;
    for delete in [false, true] {
        let dir = TempDir::created("clustered-loser");
        let rows = a_loser_wrote_both_tables(&dir, delete);
        let (table, ids) = (rows[1].0, rows[1].1.clone());
        let mut expected: Vec<(RowId, Option<i32>, Option<i32>)> = ids
            .iter()
            .zip(0..)
            .map(|(id, k)| (*id, Some(k), Some(100 + k)))
            .collect();
        let mut loser = None;
        for life in 0..3 {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
            // The transactions the later lives roll back are losers too.
            let loser = *loser.get_or_insert(storage.recovery().losers[0]);
            assert!(storage.recovery().losers.contains(&loser), "life {life}");
            assert_eq!(versions_of_txn(&storage, table, loser), 0, "life {life}");
            if life == 1 {
                // The heap table first, the clustered one on a later call.
                assert_eq!(rows_of(&storage, rows[0].0).len(), 6, "delete {delete}");
            }
            let got = rows_of(&storage, table);
            let committed: Vec<_> = got
                .iter()
                .filter(|row| row.1 != Some(500))
                .copied()
                .collect();
            assert_eq!(committed, expected, "delete {delete}, life {life}");
            assert_eq!(got.len(), 6, "delete {delete}, life {life}");
            for &id in &ids[..2] {
                let txn = TxnId(Storage::next_txn_id(&storage));
                Storage::update(&storage, txn, table, id, &pair(7, 7))
                    .unwrap_or_else(|e| panic!("delete {delete}, life {life}: update {id}: {e:?}"));
                Storage::rollback(&storage, txn).expect("rollback the update");
                let txn = TxnId(Storage::next_txn_id(&storage));
                Storage::delete(&storage, txn, table, id)
                    .unwrap_or_else(|e| panic!("delete {delete}, life {life}: delete {id}: {e:?}"));
                Storage::rollback(&storage, txn).expect("rollback the delete");
            }
            assert_eq!(
                rows_of(&storage, table),
                got,
                "delete {delete}, life {life}"
            );
            let txn = TxnId(Storage::next_txn_id(&storage));
            Storage::update(&storage, txn, table, ids[0], &pair(0, life))
                .unwrap_or_else(|e| panic!("delete {delete}, life {life}: update: {e:?}"));
            Storage::commit(&storage, txn).expect("commit the update");
            expected[0].2 = Some(life);
            if life == 1 {
                Storage::checkpoint(&storage).expect("checkpoint");
            }
        }
    }
}

#[test]
fn a_clustered_take_back_lost_with_the_pool_is_made_again_under_its_records() {
    let dir = TempDir::created("clustered-loser-twice");
    let rows = a_loser_wrote_both_tables(&dir, true);
    let table = rows[1].0;
    let compensations = |storage: &DiskStorage| {
        journal(storage)
            .iter()
            .filter(|record| record.kind == WalRecordKind::Compensation)
            .count()
    };
    let first = {
        // No checkpoint: the pages the recovery changed stay in the pool and are lost.
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
        compensations(&storage)
    };
    // Two versions the loser created, rows 0 and 1, and the two `xmax` it set on the versions
    // they replaced; the heap table takes back its own at its first call, which none makes
    // here.
    assert_eq!(first, 4);
    for pass in 0..2 {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        assert_eq!(compensations(&storage), first, "reopen {pass}");
        let loser = storage.recovery().losers[0];
        assert_eq!(versions_of_txn(&storage, table, loser), 0, "reopen {pass}");
        assert_eq!(rows_of(&storage, table).len(), 6, "reopen {pass}");
        if pass == 0 {
            crate::Storage::checkpoint(&storage).expect("checkpoint");
        }
    }
}

#[test]
fn a_clustered_row_whose_old_versions_a_vacuum_took_reopens() {
    use crate::Storage;
    let dir = TempDir::created("clustered-vacuum-reopen");
    let (id, kept) = {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
        let db = Storage::create_database(&storage, "a").expect("database");
        let table = Storage::create_table(&storage, db, &pair_shape()).expect("table");
        let id = Storage::insert(&storage, TxnId(10), table, &pair(1, 1)).expect("insert");
        let kept = Storage::insert(&storage, TxnId(10), table, &pair(2, 2)).expect("insert");
        Storage::commit(&storage, TxnId(10)).expect("commit");
        Storage::update(&storage, TxnId(11), table, id, &pair(3, 3)).expect("update");
        Storage::commit(&storage, TxnId(11)).expect("commit");
        Storage::delete(&storage, TxnId(12), table, kept).expect("delete");
        Storage::commit(&storage, TxnId(12)).expect("commit");
        Storage::vacuum(&storage, TxnId(13)).expect("vacuum");
        Storage::checkpoint(&storage).expect("checkpoint");
        (id, kept)
    };
    for pass in 0..2 {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let table = Storage::tables(&storage, Storage::databases(&storage).expect("dbs")[0].0)
            .expect("tables")[0]
            .0;
        assert_eq!(
            rows_of(&storage, table),
            vec![(id, Some(3), Some(3))],
            "reopen {pass}: row {kept} stays deleted"
        );
        let txn = TxnId(Storage::next_txn_id(&storage));
        Storage::update(&storage, txn, table, id, &pair(4, 4)).expect("update");
        Storage::rollback(&storage, txn).expect("rollback");
    }
}

// --- Writes a savepoint took back ------------------------------------------------------

/// One row as [`rows_of`] answers it.
type ScannedRow = (RowId, Option<i32>, Option<i32>);

/// What a committed transaction does between its savepoint and the `ROLLBACK TO` in
/// [`a_clustered_write_a_savepoint_took_back_stays_out_after_a_crash`].
#[derive(Debug, Clone, Copy)]
enum TakenBackWrite {
    /// Updates row 0, its key kept.
    Update,
    /// Updates row 0 to another key.
    MoveKey,
    /// Deletes row 1.
    Delete,
    /// Inserts a row of key 77.
    Insert,
    /// Updates row 0, and after the `ROLLBACK TO` updates it again to `(0, 555)`.
    UpdateAgain,
}

/// An instance in `dir` with a clustered table of five committed rows `(k, 100 + k)`, then a
/// transaction that takes a savepoint, makes `write`, lets a second transaction share the
/// leaf, commit and checkpoint (so the leaf reaches `data` holding the write), rolls back to
/// the savepoint and commits. Dropped without a checkpoint. Answers the table, the rows
/// expected, and the number of compensations the `ROLLBACK TO` journalled.
fn a_savepoint_took_back(
    dir: &TempDir,
    write: TakenBackWrite,
) -> (TableId, Vec<ScannedRow>, usize) {
    use crate::Storage;
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
    let db = Storage::create_database(&storage, "a").expect("database");
    let table = Storage::create_table(&storage, db, &pair_shape()).expect("table");
    Storage::checkpoint(&storage).expect("checkpoint");
    let first = TxnId(Storage::next_txn_id(&storage));
    let ids: Vec<RowId> = (0..5)
        .map(|k| Storage::insert(&storage, first, table, &pair(k, 100 + k)).expect("insert"))
        .collect();
    Storage::commit(&storage, first).expect("commit the rows");
    let mut expected: Vec<_> = ids
        .iter()
        .zip(0..)
        .map(|(id, k)| (*id, Some(k), Some(100 + k)))
        .collect();
    let txn = TxnId(Storage::next_txn_id(&storage));
    let sp = Storage::savepoint(&storage, txn).expect("savepoint");
    match write {
        TakenBackWrite::Update | TakenBackWrite::UpdateAgain => {
            Storage::update(&storage, txn, table, ids[0], &pair(0, 999)).expect("update");
        }
        TakenBackWrite::MoveKey => {
            Storage::update(&storage, txn, table, ids[0], &pair(900, 999)).expect("update");
        }
        TakenBackWrite::Delete => Storage::delete(&storage, txn, table, ids[1]).expect("delete"),
        TakenBackWrite::Insert => {
            Storage::insert(&storage, txn, table, &pair(77, 999)).expect("insert");
        }
    }
    let other = TxnId(Storage::next_txn_id(&storage));
    let shared = Storage::insert(&storage, other, table, &pair(50, 50)).expect("insert");
    Storage::commit(&storage, other).expect("commit the other");
    Storage::checkpoint(&storage).expect("checkpoint with the write on the leaf");
    expected.push((shared, Some(50), Some(50)));
    let before = journal(&storage)
        .iter()
        .filter(|record| record.kind == WalRecordKind::Compensation)
        .count();
    Storage::rollback_to(&storage, txn, sp).expect("rollback to");
    let compensations = journal(&storage)
        .iter()
        .filter(|record| record.kind == WalRecordKind::Compensation)
        .count()
        - before;
    if let TakenBackWrite::UpdateAgain = write {
        Storage::update(&storage, txn, table, ids[0], &pair(0, 555)).expect("update again");
        expected[0].2 = Some(555);
    }
    Storage::commit(&storage, txn).expect("commit");
    expected.sort_by_key(|row| row.1);
    (table, expected, compensations)
}

#[test]
fn a_clustered_write_a_savepoint_took_back_stays_out_after_a_crash() {
    use crate::Storage;
    for (write, journalled) in [
        (TakenBackWrite::Update, 2),
        (TakenBackWrite::MoveKey, 2),
        (TakenBackWrite::Delete, 1),
        (TakenBackWrite::Insert, 1),
        (TakenBackWrite::UpdateAgain, 2),
    ] {
        let dir = TempDir::created("clustered-rollback-to");
        let (table, mut expected, compensations) = a_savepoint_took_back(&dir, write);
        // One compensation per version taken out and per `xmax` unset.
        assert_eq!(compensations, journalled, "{write:?}");
        for life in 0..3 {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
            let got = rows_of(&storage, table);
            assert_eq!(got, expected, "{write:?}, life {life}");
            for &(id, _, _) in &got {
                let txn = TxnId(Storage::next_txn_id(&storage));
                Storage::update(&storage, txn, table, id, &pair(7, 7))
                    .unwrap_or_else(|e| panic!("{write:?}, life {life}: update {id}: {e:?}"));
                Storage::rollback(&storage, txn).expect("rollback the update");
                let txn = TxnId(Storage::next_txn_id(&storage));
                Storage::delete(&storage, txn, table, id)
                    .unwrap_or_else(|e| panic!("{write:?}, life {life}: delete {id}: {e:?}"));
                Storage::rollback(&storage, txn).expect("rollback the delete");
            }
            assert_eq!(rows_of(&storage, table), got, "{write:?}, life {life}");
            let txn = TxnId(Storage::next_txn_id(&storage));
            let (id, key, _) = expected[0];
            Storage::update(&storage, txn, table, id, &pair(key.expect("a key"), life))
                .unwrap_or_else(|e| panic!("{write:?}, life {life}: update: {e:?}"));
            Storage::commit(&storage, txn).expect("commit the update");
            expected[0].2 = Some(life);
            if life == 1 {
                Storage::checkpoint(&storage).expect("checkpoint");
            }
        }
    }
}

#[test]
fn a_second_rollback_to_the_same_savepoint_cancels_the_write_between() {
    use crate::Storage;
    for (write, checkpoint) in [
        (TakenBackWrite::Update, false),
        (TakenBackWrite::Delete, false),
        (TakenBackWrite::Insert, false),
        (TakenBackWrite::Update, true),
        (TakenBackWrite::Delete, true),
        (TakenBackWrite::Insert, true),
    ] {
        let dir = TempDir::created("clustered-rollback-to-twice");
        let (table, expected) = {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
            let db = Storage::create_database(&storage, "a").expect("database");
            let table = Storage::create_table(&storage, db, &pair_shape()).expect("table");
            Storage::checkpoint(&storage).expect("checkpoint");
            let first = TxnId(Storage::next_txn_id(&storage));
            let ids: Vec<RowId> = (0..5)
                .map(|k| {
                    Storage::insert(&storage, first, table, &pair(k, 100 + k)).expect("insert")
                })
                .collect();
            Storage::commit(&storage, first).expect("commit the rows");
            let expected = rows_of(&storage, table);
            let txn = TxnId(Storage::next_txn_id(&storage));
            let sp = Storage::savepoint(&storage, txn).expect("savepoint");
            for (round, &id) in ids.iter().enumerate().take(2) {
                match write {
                    TakenBackWrite::Delete => {
                        Storage::delete(&storage, txn, table, id).expect("delete");
                    }
                    TakenBackWrite::Insert => {
                        Storage::insert(&storage, txn, table, &pair(70 + key_of(round), 999))
                            .expect("insert");
                    }
                    _ => {
                        Storage::update(&storage, txn, table, id, &pair(key_of(round), 999))
                            .expect("update");
                    }
                }
                if checkpoint {
                    let other = TxnId(Storage::next_txn_id(&storage));
                    Storage::insert(&storage, other, table, &pair(50 + 10 * key_of(round), 50))
                        .expect("insert");
                    Storage::commit(&storage, other).expect("commit the other");
                    Storage::checkpoint(&storage).expect("checkpoint");
                }
                Storage::rollback_to(&storage, txn, sp).expect("rollback to");
            }
            Storage::commit(&storage, txn).expect("commit");
            let mut expected = expected;
            if checkpoint {
                expected = rows_of(&storage, table);
            }
            (table, expected)
        };
        for life in 0..2 {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
            assert_eq!(
                rows_of(&storage, table),
                expected,
                "{write:?}, checkpoint {checkpoint}, life {life}"
            );
        }
    }
}

/// The round as a key offset.
fn key_of(round: usize) -> i32 {
    i32::try_from(round).expect("a round")
}

// --- A crash at each write of a split -----------------------------------------------------

/// A table of a `varchar(900)` clustered key and an `int`: a leaf holds four rows of a key of
/// 846 characters, and an internal page four separators, so a few rows split a leaf, then
/// the root, then an internal page.
fn wide_shape() -> TableShape {
    TableShape {
        columns: vec![
            TypeInfo::new(SqlType::VarChar(Len::Fixed(900)), true),
            TypeInfo::new(SqlType::Int, true),
        ],
        clustered_key: Some(first_column()),
    }
}

/// The row of number `n`: a key of about 846 characters that sorts by `n` scattered, and
/// `value`.
fn wide(n: i32, value: i32) -> Row {
    let scattered = (n * 37) % 101;
    Row(vec![
        Value::String(SqlString {
            text: format!("{scattered:03}-{n:02}{}", "w".repeat(840)),
        }),
        Value::I32(value),
    ])
}

/// The three files of the instance in `dir`, as they stand.
fn files_of(dir: &Path) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let read = |name: &str| std::fs::read(dir.join(name)).expect("read a file of the instance");
    (
        read(super::super::DATA_FILE_NAME),
        read(super::super::CONTROL_FILE_NAME),
        read(super::super::WAL_FILE_NAME),
    )
}

/// The number and the end offset of each record of the journal `wal`.
fn record_ends(wal: &[u8]) -> Vec<(Lsn, usize)> {
    let mut ends = Vec::new();
    let mut at = 0;
    while at + 4 <= wal.len() {
        let len = u32::from_le_bytes(wal[at..at + 4].try_into().expect("four bytes")) as usize;
        if len < 14 || at + 4 + len > wal.len() {
            break;
        }
        let lsn = u64::from_le_bytes(wal[at + 6..at + 14].try_into().expect("eight bytes"));
        at += 4 + len;
        ends.push((Lsn(lsn), at));
    }
    ends
}

/// Each content the pages the page records past `from` name went through, the record that
/// left it first: the page as `data` holds it is the content before the first of them.
fn page_versions(
    data: &[u8],
    records: &[WalRecord],
    from: Lsn,
) -> BTreeMap<PageId, Vec<(Lsn, Page)>> {
    let mut versions: BTreeMap<PageId, Vec<(Lsn, Page)>> = BTreeMap::new();
    for record in records.iter().filter(|record| record.lsn > from) {
        if record.kind != WalRecordKind::PageWrite {
            continue;
        }
        let write = PageWrite::decode(&record.payload).expect("a page record reads back");
        for delta in write.pages {
            let at = usize::try_from(delta.page.0).expect("a page number") * PAGE_SIZE;
            let mut page = match versions.get(&delta.page).and_then(|done| done.last()) {
                Some((_, page)) => page.clone(),
                None if delta.whole => Page::empty(PageKind::Free, delta.page),
                None => {
                    let mut bytes = [0u8; PAGE_SIZE];
                    bytes.copy_from_slice(&data[at..at + PAGE_SIZE]);
                    Page(bytes)
                }
            };
            for (offset, bytes) in &delta.ranges {
                let offset = usize::from(*offset);
                page.0[offset..offset + bytes.len()].copy_from_slice(bytes);
            }
            page.set_lsn(record.lsn);
            page.seal();
            versions
                .entry(delta.page)
                .or_default()
                .push((record.lsn, page));
        }
    }
    versions
}

/// The rows of [`wide`] a reader sees, as `(RowId, n, value)`.
type WideRows = [(RowId, i32, i32)];

/// The rows a settled reader sees, as `(RowId, n, value)`.
fn wide_rows(storage: &DiskStorage, table: TableId) -> Vec<(RowId, i32, i32)> {
    crate::Storage::scan(storage, &settled(), table)
        .expect("scan")
        .map(|row| {
            let (id, row) = row.expect("a row of the scan");
            match (row.0.first(), row.0.get(1)) {
                (Some(Value::String(key)), Some(Value::I32(value))) => {
                    let n = key.text[4..]
                        .split('w')
                        .next()
                        .and_then(|n| n.parse().ok())
                        .expect("the number of the key");
                    (id, n, *value)
                }
                other => panic!("row {id} carries {other:?}"),
            }
        })
        .collect()
}

/// Opens the instance in `dir` twice, and checks each time that both trees of `table` keep
/// their invariants and that the rows read are `expected`, each one found by its `RowId` too.
fn reopen_and_check(dir: &Path, table: TableId, expected: &WideRows, label: &str) {
    use super::super::btree::KeyOrder;
    use super::super::btree::concurrency::assert_tree_valid;
    for round in 0..2 {
        let storage = DiskStorage::open(dir, DiskOptions::default())
            .unwrap_or_else(|e| panic!("{label}, round {round}: open: {e:?}"));
        let entry = storage
            .lock_catalogue()
            .expect("the catalogue")
            .table(table)
            .cloned()
            .expect("the table");
        let (columns, types) = ClusteredTable::tree_key(&entry.shape, table).expect("the key");
        let order = KeyOrder::new(&columns, &types).expect("the order of the tree");
        let directory = KeyOrder::new(&ClusteredTable::directory_key(), &[tie_type()])
            .expect("the order of the directory");
        let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let versions = assert_tree_valid(&storage, entry.tree_root.expect("a tree"), &order);
            let rows = assert_tree_valid(
                &storage,
                entry.directory_root.expect("a directory"),
                &directory,
            );
            (versions, rows)
        }));
        let (versions, rows) =
            checked.unwrap_or_else(|_| panic!("{label}, round {round}: a tree is broken"));
        assert_eq!(
            rows,
            expected.len(),
            "{label}, round {round}: the directory"
        );
        assert!(
            versions >= expected.len(),
            "{label}, round {round}: {versions} versions"
        );
        let mut read = wide_rows(&storage, table);
        read.sort_unstable();
        assert_eq!(read, expected, "{label}, round {round}");
        for (id, n, value) in expected {
            let got = crate::Storage::get(&storage, &settled(), table, *id)
                .unwrap_or_else(|e| panic!("{label}, round {round}: get {id}: {e:?}"));
            assert_eq!(
                got,
                Some(wide(*n, *value)),
                "{label}, round {round}: get {id}"
            );
        }
    }
}

/// Plays each crash an insert left behind: the journal cut after each of its records, and
/// `data` holding, for each page the insert wrote, the page before it or one of the contents
/// the records up to the cut gave it, up to `limit` combinations per cut. Answers the number
/// of crashes played.
fn play_crashes(
    files: &(Vec<u8>, Vec<u8>, Vec<u8>),
    from: Lsn,
    table: TableId,
    (before, after): (&WideRows, &WideRows),
    commit: Lsn,
    label: &str,
) -> usize {
    const LIMIT: usize = 24;
    let (data, ctl, wal) = files;
    let ends = record_ends(wal);
    let records: Vec<WalRecord> = {
        let dir = TempDir::created("clustered-split-journal");
        std::fs::write(dir.child(super::super::WAL_FILE_NAME), wal).expect("write");
        let journal = super::super::wal::Wal::open(&dir.child(super::super::WAL_FILE_NAME))
            .expect("read the journal back");
        journal
            .iter()
            .expect("iterate")
            .collect::<Result<_, _>>()
            .expect("records")
    };
    let versions = page_versions(data, &records, from);
    let mut played = 0;
    let cuts: Vec<(Lsn, usize)> = ends
        .iter()
        .copied()
        .filter(|(lsn, _)| *lsn >= from)
        .collect();
    for (cut, end) in cuts {
        let pages: Vec<(PageId, Vec<&Page>)> = versions
            .iter()
            .map(|(id, all)| {
                let mut options: Vec<&Page> = Vec::new();
                options.extend(
                    all.iter()
                        .filter(|(lsn, _)| *lsn <= cut)
                        .map(|(_, page)| page),
                );
                (*id, options)
            })
            .collect();
        let combinations: usize = pages.iter().map(|(_, options)| options.len() + 1).product();
        let step = combinations.div_ceil(LIMIT).max(1);
        let expected = if cut >= commit { after } else { before };
        for combination in (0..combinations).step_by(step) {
            let mut image = data.clone();
            let mut rest = combination;
            let mut written = Vec::new();
            for (id, options) in &pages {
                let choice = rest % (options.len() + 1);
                rest /= options.len() + 1;
                if choice == 0 {
                    continue;
                }
                let page = options[choice - 1];
                let at = usize::try_from(id.0).expect("a page number") * PAGE_SIZE;
                image[at..at + PAGE_SIZE].copy_from_slice(&page.0);
                written.push((*id, page.lsn()));
            }
            let dir = TempDir::created("clustered-split-image");
            std::fs::write(dir.child(super::super::DATA_FILE_NAME), &image).expect("data");
            std::fs::write(dir.child(super::super::CONTROL_FILE_NAME), ctl).expect("ctl");
            std::fs::write(dir.child(super::super::WAL_FILE_NAME), &wal[..end]).expect("wal");
            reopen_and_check(
                dir.path(),
                table,
                expected,
                &format!("{label}: journal cut at {cut}, pages written {written:?}"),
            );
            played += 1;
        }
    }
    played
}

/// The height of the tree rooted at `root`, its leaves counted.
fn height(storage: &DiskStorage, root: PageId) -> usize {
    let mut levels = 1;
    let mut page = root;
    loop {
        let pin = storage.pool.pin(page).expect("pin");
        let child = pin
            .with_page(|page| match page.kind() {
                Ok(PageKind::BTreeInternal) => Some(super::super::btree::leftmost_child(page)),
                _ => None,
            })
            .expect("read");
        match child {
            Some(child) => {
                levels += 1;
                page = child;
            }
            None => return levels,
        }
    }
}

/// The pages of the trees the page records past `from` name.
fn tree_pages(records: &[WalRecord], from: Lsn) -> BTreeSet<PageId> {
    records
        .iter()
        .filter(|record| record.lsn > from && record.kind == WalRecordKind::PageWrite)
        .filter_map(|record| {
            let write = PageWrite::decode(&record.payload).expect("a page record");
            let tree = write.pages.iter().any(|delta| {
                delta.kind == PageKind::BTreeLeaf.as_byte()
                    || delta.kind == PageKind::BTreeInternal.as_byte()
            });
            tree.then(|| {
                write
                    .pages
                    .iter()
                    .map(|delta| delta.page)
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect()
}

#[test]
fn a_crash_at_each_write_of_a_split_reopens_a_valid_tree() {
    use crate::Storage;
    let dir = TempDir::created("clustered-split-crash");
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
    let db = storage.create_database("a").expect("database");
    let table = storage.create_table(db, &wide_shape()).expect("table");
    let root = |storage: &DiskStorage| {
        storage
            .lock_catalogue()
            .expect("the catalogue")
            .table(table)
            .and_then(|entry| entry.tree_root)
            .expect("the root")
    };
    let mut rows: Vec<(RowId, i32, i32)> = Vec::new();
    let mut shapes = BTreeSet::new();
    let mut played = 0;
    // Inserts, one per transaction, then updates of the rows they left; the crashes of the
    // first write of each shape are played.
    for step in 0..240usize {
        if shapes.len() == 5 {
            break;
        }
        storage.checkpoint().expect("checkpoint");
        let from = storage.wal.last_lsn().expect("the last record");
        let levels = height(&storage, root(&storage));
        let txn = TxnId(storage.next_txn_id());
        let before = rows.clone();
        let updating = shapes.len() >= 3 || step >= 120;
        if updating {
            let at = (step * 7) % rows.len();
            let (id, n, value) = rows[at];
            storage
                .update(txn, table, id, &wide(n, value + 1_000))
                .expect("update");
            rows[at].2 = value + 1_000;
        } else {
            let n = i32::try_from(step).expect("a step");
            let id = storage.insert(txn, table, &wide(n, n)).expect("insert");
            rows.push((id, n, n));
            rows.sort_unstable();
        }
        storage.commit(txn).expect("commit");
        let records = journal(&storage);
        let commit = records
            .iter()
            .find(|record| record.kind == WalRecordKind::Commit && record.txn == txn)
            .expect("the commit")
            .lsn;
        let split = tree_pages(&records, from).len() >= 3;
        let grown = height(&storage, root(&storage)) > levels;
        let shape = match (updating, grown, levels, split) {
            (false, true, 1, _) => "the root leaf splits",
            (false, true, _, _) => "an internal root splits",
            (false, false, _, true) => "a leaf under the root splits",
            (true, _, _, true) => "an update splits a leaf",
            (true, _, _, false) => "an update rewrites the version it replaces",
            _ => continue,
        };
        if !shapes.insert(shape) {
            continue;
        }
        played += play_crashes(
            &files_of(dir.path()),
            from,
            table,
            (&before, &rows),
            commit,
            &format!("{shape} (step {step})"),
        );
    }
    assert_eq!(shapes.len(), 5, "{shapes:?}");
    assert!(played > 100, "{played} crashes played");
}

/// A table of an `int` clustered key and a `varchar(max)`.
fn long_text_shape() -> TableShape {
    TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::VarChar(Len::Max), true),
        ],
        clustered_key: Some(first_column()),
    }
}

/// A row of [`long_text_shape`]: `key`, then `text`.
fn key_text(key: i32, text: &str) -> Row {
    Row(vec![
        Value::I32(key),
        Value::String(SqlString {
            text: text.to_string(),
        }),
    ])
}

#[test]
fn a_loser_that_rewrote_a_long_row_during_a_checkpoint_leaves_the_row_readable() {
    use crate::Storage;
    let dir = TempDir::created("clustered-loser-long-row");
    let long = "L".repeat(9_000);
    let (table, expected) = {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
        let db = storage.create_database("a").expect("database");
        let table = storage.create_table(db, &long_text_shape()).expect("table");
        storage.checkpoint().expect("checkpoint");
        let first = TxnId(storage.next_txn_id());
        let mut ids = Vec::new();
        for key in 0..5 {
            let text = if key == 2 {
                long.clone()
            } else {
                format!("v{key}")
            };
            ids.push((
                storage
                    .insert(first, table, &key_text(key, &text))
                    .expect("insert"),
                key,
                text,
            ));
        }
        storage.commit(first).expect("commit");
        // A loser rewrites the long row with a longer one; a winner that shares the leaf
        // commits and a checkpoint writes what the two left.
        let loser = TxnId(storage.next_txn_id());
        storage
            .update(loser, table, ids[2].0, &key_text(2, &"M".repeat(12_000)))
            .expect("update by the loser");
        let winner = TxnId(storage.next_txn_id());
        storage
            .insert(winner, table, &key_text(50_000, "w"))
            .expect("insert by the winner");
        storage.commit(winner).expect("commit the winner");
        storage.checkpoint().expect("checkpoint");
        let mut expected: Vec<(i32, String)> =
            ids.into_iter().map(|(_, key, text)| (key, text)).collect();
        expected.push((50_000, "w".to_string()));
        (table, expected)
    };
    for life in 0..3 {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let rows: Vec<(RowId, Row)> = storage
            .scan(&settled(), table)
            .expect("scan")
            .map(|row| row.expect("a row of the scan"))
            .collect();
        let read: Vec<(i32, String)> = rows
            .iter()
            .map(|(_, row)| match (&row.0[0], &row.0[1]) {
                (Value::I32(key), Value::String(text)) => (*key, text.text.clone()),
                other => panic!("life {life}: a row carries {other:?}"),
            })
            .collect();
        assert_eq!(read, expected, "life {life}");
        for (id, _) in &rows {
            let txn = TxnId(storage.next_txn_id());
            storage
                .update(txn, table, *id, &key_text(-1, "u"))
                .unwrap_or_else(|e| panic!("life {life}: update {id}: {e:?}"));
            storage.rollback(txn).expect("rollback the update");
        }
        if life == 1 {
            storage.checkpoint().expect("checkpoint");
        }
    }
}
