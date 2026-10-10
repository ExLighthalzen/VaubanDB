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

use vauban_types::{SqlString, SqlType};

use super::super::temp::TempDir;
use super::super::wal::{WalRecord, WalRecordKind};
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

/// The kind of each record, in order.
fn kinds(records: &[WalRecord]) -> Vec<WalRecordKind> {
    records.iter().map(|record| record.kind).collect()
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
    storage.pool.flush_all().expect("flush what create left");
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
    // What says they went out is the LSN they carry in `data`: the record of the insert
    // named the leaf, and the undo left that LSN on the page
    // (`BufferPool::mark_dirty` does not move one backwards).
    storage.pool.flush_all().expect("flush after the rollback");
    let records = journal(&storage);
    let insert = records
        .iter()
        .find(|record| record.kind == WalRecordKind::Insert)
        .expect("the record of the insert");
    let page = storage
        .data
        .read_page(leaf)
        .expect("read the leaf from data");
    assert_eq!(page.lsn(), insert.lsn);
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
    storage.pool.flush_all().expect("flush what create left");

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
    storage.pool.flush_all().expect("flush what create left");

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
    storage.pool.flush_all().expect("flush what create left");
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
