use std::ops::Bound;

use super::super::version::HeapTable;

use vauban_types::{SqlType, TypeInfo};

use super::super::temp::TempDir;
use super::super::{DiskOptions, alloc};
use super::*;
use crate::{KeyColumn, TableId, TableShape};

/// The table the tests of this file work on: its identifier is the `'1'` the 2601 message
/// carries in place of a name.
const TABLE: TableId = TableId(1);

/// The identifier given to the index under test.
const INDEX: IndexId = IndexId(7);

/// An empty instance in a temporary directory, with the guard that removes it.
fn instance(label: &str) -> (TempDir, DiskStorage) {
    let dir = TempDir::created(label);
    let storage =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    (dir, storage)
}

/// A table of `columns` nullable `int` columns and no clustered key, over a fresh heap
/// page.
fn table_of(storage: &DiskStorage, columns: usize) -> HeapTable<'_> {
    let head = alloc::allocate(storage).expect("allocate the head page");
    let shape = TableShape {
        columns: (0..columns)
            .map(|_| TypeInfo::new(SqlType::Int, true))
            .collect(),
        clustered_key: None,
    };
    HeapTable::create(storage, TABLE, head, shape).expect("create the table")
}

/// The shape of an index on the `(column, descending)` pairs.
fn index_shape(columns: &[(u16, bool)], unique: bool) -> IndexShape {
    IndexShape {
        columns: columns
            .iter()
            .map(|&(column, descending)| KeyColumn { column, descending })
            .collect(),
        unique,
        included: vec![],
    }
}

/// A row of `int` values.
fn row(values: &[i32]) -> Row {
    Row(key(values))
}

/// A row of `int` values where `None` is `NULL`.
fn nullable_row(values: &[Option<i32>]) -> Row {
    Row(nullable_key(values))
}

/// A key, or key prefix, of `int` values.
fn key(values: &[i32]) -> Vec<Value> {
    values.iter().map(|&v| Value::I32(v)).collect()
}

/// A key, or key prefix, of `int` values where `None` is `NULL`.
fn nullable_key(values: &[Option<i32>]) -> Vec<Value> {
    values
        .iter()
        .map(|v| v.map_or(Value::Null, Value::I32))
        .collect()
}

/// A [`KeyRange::Point`] on `values`.
fn point(values: &[i32]) -> KeyRange {
    KeyRange::Point(key(values))
}

/// A [`KeyRange::Between`] of two `int` bounds.
fn between(lo: Bound<&[i32]>, hi: Bound<&[i32]>) -> KeyRange {
    KeyRange::Between(lo.map(key), hi.map(key))
}

/// The snapshot of `own` with the transactions of `active` still in progress, the
/// convention of `memory/tests.rs`: `xmin` is the smallest of `own` and `active`, `xmax` is
/// `own + 1`.
fn snap(own: u64, active: &[u64]) -> Snapshot {
    let mut active: Vec<TxnId> = active.iter().map(|&t| TxnId(t)).collect();
    active.sort();
    let xmin = active.first().map_or(own, |t| t.0.min(own));
    Snapshot {
        xmin: TxnId(xmin),
        xmax: TxnId(own + 1),
        active,
        own: TxnId(own),
    }
}

/// What the store does around an `insert`: ask the unique index first, write the row,
/// then hand the maintenance log to the index.
fn insert_row(
    table: &mut HeapTable<'_>,
    index: &mut DiskIndex<'_>,
    txn: u64,
    values: &Row,
) -> SqlResult<RowId> {
    index.check_duplicate(table, TxnId(txn), values, None)?;
    let id = table.insert(TxnId(txn), values)?;
    index.apply(&table.take_index_changes())?;
    Ok(id)
}

/// The same around an `update`: the row being replaced is not live for its own writer.
fn update_row(
    table: &mut HeapTable<'_>,
    index: &mut DiskIndex<'_>,
    txn: u64,
    id: RowId,
    values: &Row,
) -> SqlResult<()> {
    index.check_duplicate(table, TxnId(txn), values, Some(id))?;
    table.update(TxnId(txn), id, values)?;
    index.apply(&table.take_index_changes())?;
    Ok(())
}

/// The same around a `delete`, which adds and removes no entry.
fn delete_row(
    table: &mut HeapTable<'_>,
    index: &mut DiskIndex<'_>,
    txn: u64,
    id: RowId,
) -> SqlResult<()> {
    table.delete(TxnId(txn), id)?;
    index.apply(&table.take_index_changes())?;
    Ok(())
}

/// The same around a `rollback`, whose undo takes entries away.
fn rollback_txn(table: &mut HeapTable<'_>, index: &mut DiskIndex<'_>, txn: u64) {
    table.rollback(TxnId(txn)).expect("rollback");
    index
        .apply(&table.take_index_changes())
        .expect("index maintenance of the rollback");
}

/// Inserts the rows on behalf of `txn` and answers the ids, in order.
fn insert_all(
    table: &mut HeapTable<'_>,
    index: &mut DiskIndex<'_>,
    txn: u64,
    rows: &[Row],
) -> Vec<RowId> {
    rows.iter()
        .map(|r| insert_row(table, index, txn, r).expect("insert"))
        .collect()
}

/// The rows of a `seek` in the given direction.
fn seek_rows(
    index: &DiskIndex<'_>,
    table: &dyn VersionSource,
    sn: &Snapshot,
    range: &KeyRange,
    dir: Direction,
) -> Vec<(RowId, Row)> {
    index.seek(table, sn, range, dir).expect("seek")
}

/// The `RowId`s of a forward `seek`.
fn seek_ids(
    index: &DiskIndex<'_>,
    table: &dyn VersionSource,
    sn: &Snapshot,
    range: &KeyRange,
) -> Vec<RowId> {
    row_ids(&seek_rows(index, table, sn, range, Direction::Forward))
}

/// The `RowId`s of a backward `seek`.
fn seek_ids_back(
    index: &DiskIndex<'_>,
    table: &dyn VersionSource,
    sn: &Snapshot,
    range: &KeyRange,
) -> Vec<RowId> {
    row_ids(&seek_rows(index, table, sn, range, Direction::Backward))
}

/// The `RowId`s of collected rows, in order.
fn row_ids(rows: &[(RowId, Row)]) -> Vec<RowId> {
    rows.iter().map(|(id, _)| *id).collect()
}

/// `ids[k]` for each `k` of `order`.
fn pick(ids: &[RowId], order: &[usize]) -> Vec<RowId> {
    order.iter().map(|&k| ids[k]).collect()
}

/// Asserts that `result` is the 2601 of `index` on [`TABLE`], with `key` spelt out.
fn assert_duplicate_key<T: std::fmt::Debug>(result: SqlResult<T>, index: IndexId, key: &str) {
    match result {
        Ok(value) => panic!("expected error 2601, got Ok({value:?})"),
        Err(err) => {
            assert_eq!(err.number, 2601, "expected 2601, got: {}", err.message);
            assert_eq!(err.state, 1, "{}", err.message);
            for part in [format!("'{TABLE}'"), format!("'{index}'"), key.to_string()] {
                assert!(
                    err.message.contains(&part),
                    "2601 must spell out {part}: {}",
                    err.message
                );
            }
        }
    }
}

// --- seek ---------------------------------------------------------------------------

/// The scenario of `case_index_seek_point_between_full_both_directions` on one
/// on-disk index: `Point`, `Between` and `Full`, forward and backward, prefixes, empty and
/// inverted ranges, then the visibility filter.
#[test]
fn seek_point_between_full_both_directions() {
    let (_dir, storage) = instance("index-seek-point-between-full");
    let mut table = table_of(&storage, 2);
    let mut index = DiskIndex::create(
        &storage,
        INDEX,
        &index_shape(&[(0, false), (1, false)], false),
        &table,
    )
    .expect("create the index");
    let rows = [
        row(&[1, 5]),
        row(&[1, 0]),
        row(&[2, 3]),
        row(&[0, 9]),
        row(&[1, 5]),
        row(&[3, 1]),
    ];
    let ids = insert_all(&mut table, &mut index, 1, &rows);
    table.commit(TxnId(1)).expect("commit");
    let sn = snap(2, &[]);
    // Index order: (0,9) (1,0) (1,5)#0 (1,5)#4 (2,3) (3,1); ties by RowId.
    let forward = pick(&ids, &[3, 1, 0, 4, 2, 5]);
    let full = seek_rows(&index, &table, &sn, &KeyRange::Full, Direction::Forward);
    assert_eq!(row_ids(&full), forward);
    assert_eq!(
        full.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
        vec![
            row(&[0, 9]),
            row(&[1, 0]),
            row(&[1, 5]),
            row(&[1, 5]),
            row(&[2, 3]),
            row(&[3, 1])
        ]
    );
    assert_eq!(
        seek_ids_back(&index, &table, &sn, &KeyRange::Full),
        pick(&ids, &[5, 2, 4, 0, 1, 3])
    );
    // Point: full key, prefix, empty (= Full), absent.
    assert_eq!(
        seek_ids(&index, &table, &sn, &point(&[1, 5])),
        pick(&ids, &[0, 4])
    );
    assert_eq!(
        seek_ids_back(&index, &table, &sn, &point(&[1, 5])),
        pick(&ids, &[4, 0])
    );
    assert_eq!(
        seek_ids(&index, &table, &sn, &point(&[1])),
        pick(&ids, &[1, 0, 4])
    );
    assert_eq!(
        seek_ids_back(&index, &table, &sn, &point(&[1])),
        pick(&ids, &[4, 0, 1])
    );
    assert_eq!(seek_ids(&index, &table, &sn, &point(&[])), forward);
    assert!(seek_ids(&index, &table, &sn, &point(&[7])).is_empty());
    assert!(seek_ids(&index, &table, &sn, &point(&[1, 7])).is_empty());
    // Between: prefix bounds cover the whole prefix group.
    assert_eq!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Included(&[1]), Bound::Excluded(&[3]))
        ),
        pick(&ids, &[1, 0, 4, 2])
    );
    assert_eq!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Excluded(&[1]), Bound::Unbounded)
        ),
        pick(&ids, &[2, 5])
    );
    assert_eq!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Unbounded, Bound::Included(&[1, 0]))
        ),
        pick(&ids, &[3, 1])
    );
    assert_eq!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Included(&[1, 0]), Bound::Excluded(&[1, 5]))
        ),
        pick(&ids, &[1])
    );
    assert_eq!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Excluded(&[1, 0]), Bound::Included(&[2]))
        ),
        pick(&ids, &[0, 4, 2])
    );
    assert_eq!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Unbounded, Bound::Unbounded)
        ),
        forward
    );
    assert_eq!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Included(&[0, 0]), Bound::Included(&[9]))
        ),
        forward
    );
    assert_eq!(
        seek_ids_back(
            &index,
            &table,
            &sn,
            &between(Bound::Included(&[1]), Bound::Included(&[2]))
        ),
        pick(&ids, &[2, 4, 0, 1])
    );
    // Empty and inverted ranges yield an empty answer rather than an error.
    assert!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Included(&[2]), Bound::Included(&[1]))
        )
        .is_empty()
    );
    assert!(
        seek_ids(
            &index,
            &table,
            &sn,
            &between(Bound::Excluded(&[1]), Bound::Excluded(&[1]))
        )
        .is_empty()
    );
    // Visibility applies to seek: own uncommitted rows show, others' do not.
    let x = insert_row(&mut table, &mut index, 3, &row(&[1, 5])).expect("insert x");
    assert_eq!(
        seek_ids(&index, &table, &snap(3, &[]), &point(&[1, 5])),
        vec![ids[0], ids[4], x]
    );
    assert_eq!(
        seek_ids(&index, &table, &snap(4, &[3]), &point(&[1, 5])),
        pick(&ids, &[0, 4])
    );
}

/// `NULL` first on an ascending index and last on a descending one, over the same rows:
/// the scenario of `case_index_nulls_first_ascending_last_descending`.
#[test]
fn nulls_first_ascending_last_descending() {
    let (_dir, storage) = instance("index-nulls-first-last");
    let mut table = table_of(&storage, 1);
    let mut asc = DiskIndex::create(
        &storage,
        IndexId(1),
        &index_shape(&[(0, false)], false),
        &table,
    )
    .expect("ascending index");
    let mut desc = DiskIndex::create(
        &storage,
        IndexId(2),
        &index_shape(&[(0, true)], false),
        &table,
    )
    .expect("descending index");
    let rows = [
        row(&[2]),
        nullable_row(&[None]),
        row(&[1]),
        nullable_row(&[None]),
    ];
    let mut ids = Vec::new();
    for r in &rows {
        ids.push(table.insert(TxnId(1), r).expect("insert"));
        let changes = table.take_index_changes();
        asc.apply(&changes).expect("maintain the ascending index");
        desc.apply(&changes).expect("maintain the descending index");
    }
    table.commit(TxnId(1)).expect("commit");
    let sn = snap(2, &[]);
    let null = KeyRange::Point(nullable_key(&[None]));
    let after_null_asc =
        KeyRange::Between(Bound::Excluded(nullable_key(&[None])), Bound::Unbounded);
    let before_null_desc =
        KeyRange::Between(Bound::Unbounded, Bound::Excluded(nullable_key(&[None])));
    let null_to_one_asc = KeyRange::Between(
        Bound::Included(nullable_key(&[None])),
        Bound::Included(key(&[1])),
    );
    // Ascending: NULL, NULL, 1, 2. Descending: 2, 1, NULL, NULL. Ties by RowId.
    assert_eq!(
        seek_ids(&asc, &table, &sn, &KeyRange::Full),
        pick(&ids, &[1, 3, 2, 0])
    );
    assert_eq!(
        seek_ids_back(&asc, &table, &sn, &KeyRange::Full),
        pick(&ids, &[0, 2, 3, 1])
    );
    assert_eq!(
        seek_ids(&desc, &table, &sn, &KeyRange::Full),
        pick(&ids, &[0, 2, 1, 3])
    );
    assert_eq!(
        seek_ids_back(&desc, &table, &sn, &KeyRange::Full),
        pick(&ids, &[3, 1, 2, 0])
    );
    // NULL is a key like any other: a point on it, bounds around it, in both orders.
    assert_eq!(seek_ids(&asc, &table, &sn, &null), pick(&ids, &[1, 3]));
    assert_eq!(seek_ids(&desc, &table, &sn, &null), pick(&ids, &[1, 3]));
    assert_eq!(
        seek_ids(&asc, &table, &sn, &after_null_asc),
        pick(&ids, &[2, 0])
    );
    assert_eq!(
        seek_ids(&desc, &table, &sn, &before_null_desc),
        pick(&ids, &[0, 2])
    );
    assert_eq!(
        seek_ids(&asc, &table, &sn, &null_to_one_asc),
        pick(&ids, &[1, 3, 2])
    );
    // Descending bounds are given in index order: (2) comes before (1).
    assert_eq!(
        seek_ids(
            &desc,
            &table,
            &sn,
            &between(Bound::Included(&[2]), Bound::Included(&[1]))
        ),
        pick(&ids, &[0, 2])
    );
    assert!(
        seek_ids(
            &desc,
            &table,
            &sn,
            &between(Bound::Included(&[1]), Bound::Included(&[2]))
        )
        .is_empty()
    );
}

/// A snapshot that has not settled the writer of an update reads the row under its old key
/// and with its old columns; the writer reads the new ones.
#[test]
fn update_leaves_the_old_key_to_an_older_snapshot() {
    let (_dir, storage) = instance("index-update-old-key");
    let mut table = table_of(&storage, 1);
    let mut index = DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], false), &table)
        .expect("create the index");
    let a = insert_row(&mut table, &mut index, 1, &row(&[1])).expect("insert a");
    table.commit(TxnId(1)).expect("commit 1");
    update_row(&mut table, &mut index, 2, a, &row(&[3])).expect("update 2");
    assert_eq!(
        seek_rows(
            &index,
            &table,
            &snap(2, &[]),
            &point(&[3]),
            Direction::Forward
        ),
        vec![(a, row(&[3]))]
    );
    assert!(seek_ids(&index, &table, &snap(2, &[]), &point(&[1])).is_empty());
    assert_eq!(
        seek_rows(
            &index,
            &table,
            &snap(3, &[2]),
            &point(&[1]),
            Direction::Forward
        ),
        vec![(a, row(&[1]))]
    );
    assert!(seek_ids(&index, &table, &snap(3, &[2]), &point(&[3])).is_empty());
}

// --- uniqueness ---------------------------------------------------------------------

/// Two inserts of one key on a unique index: the second is 2601, naming the table and the
/// index by their decimal identifiers, and leaves neither a version nor an entry. Then the
/// MVCC cases: a rolled-back insert and a committed delete free the key, an uncommitted
/// delete by another transaction does not.
#[test]
fn unique_2601_names_ids() {
    let (_dir, storage) = instance("index-unique-2601");
    let mut table = table_of(&storage, 1);
    let mut index = DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], true), &table)
        .expect("create the index");
    let a = insert_row(&mut table, &mut index, 1, &row(&[1])).expect("insert a");
    table.commit(TxnId(1)).expect("commit 1");
    assert_duplicate_key(
        insert_row(&mut table, &mut index, 2, &row(&[1])),
        INDEX,
        "(I32(1))",
    );
    // Nothing was written: one row, one entry.
    assert_eq!(
        table.scan(&snap(2, &[])).expect("scan"),
        vec![(a, row(&[1]))]
    );
    assert_eq!(
        seek_ids(&index, &table, &snap(2, &[]), &KeyRange::Full),
        vec![a]
    );
    // An uncommitted insert of another transaction blocks; its rollback frees the key.
    insert_row(&mut table, &mut index, 3, &row(&[2])).expect("insert 3");
    assert_duplicate_key(
        insert_row(&mut table, &mut index, 4, &row(&[2])),
        INDEX,
        "(I32(2))",
    );
    rollback_txn(&mut table, &mut index, 3);
    let b = insert_row(&mut table, &mut index, 4, &row(&[2])).expect("insert 4");
    table.commit(TxnId(4)).expect("commit 4");
    // A committed delete frees the key.
    delete_row(&mut table, &mut index, 5, a).expect("delete a");
    table.commit(TxnId(5)).expect("commit 5");
    let c = insert_row(&mut table, &mut index, 6, &row(&[1])).expect("insert 6");
    table.commit(TxnId(6)).expect("commit 6");
    // An uncommitted delete by another transaction does not free it.
    delete_row(&mut table, &mut index, 7, c).expect("delete 7");
    assert_duplicate_key(
        insert_row(&mut table, &mut index, 8, &row(&[1])),
        INDEX,
        "(I32(1))",
    );
    rollback_txn(&mut table, &mut index, 7);
    assert_eq!(
        seek_ids(&index, &table, &snap(8, &[]), &KeyRange::Full),
        vec![c, b]
    );
}

/// `NULL` equals `NULL` in a unique index, in one column of a key and in both of them.
#[test]
fn unique_null_equals_null() {
    let (_dir, storage) = instance("index-unique-null");
    let mut table = table_of(&storage, 2);
    let mut index = DiskIndex::create(
        &storage,
        INDEX,
        &index_shape(&[(0, false), (1, false)], true),
        &table,
    )
    .expect("create the index");
    insert_row(&mut table, &mut index, 1, &nullable_row(&[Some(1), None])).expect("(1, NULL)");
    assert_duplicate_key(
        insert_row(&mut table, &mut index, 1, &nullable_row(&[Some(1), None])),
        INDEX,
        "(I32(1), Null)",
    );
    insert_row(&mut table, &mut index, 1, &nullable_row(&[None, None])).expect("(NULL, NULL)");
    assert_duplicate_key(
        insert_row(&mut table, &mut index, 1, &nullable_row(&[None, None])),
        INDEX,
        "(Null, Null)",
    );
    let b =
        insert_row(&mut table, &mut index, 1, &nullable_row(&[Some(2), None])).expect("(2, NULL)");
    assert_duplicate_key(
        update_row(
            &mut table,
            &mut index,
            1,
            b,
            &nullable_row(&[Some(1), None]),
        ),
        INDEX,
        "(I32(1), Null)",
    );
    table.commit(TxnId(1)).expect("commit");
    assert_duplicate_key(
        insert_row(&mut table, &mut index, 2, &nullable_row(&[Some(1), None])),
        INDEX,
        "(I32(1), Null)",
    );
    assert_eq!(table.scan(&snap(2, &[])).expect("scan").len(), 3);
}

/// A unique index lets a row keep its key across two updates, and still blocks another row
/// on it.
#[test]
fn unique_allows_update_same_key() {
    let (_dir, storage) = instance("index-unique-update-same-key");
    let mut table = table_of(&storage, 2);
    let mut index = DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], true), &table)
        .expect("create the index");
    let a = insert_row(&mut table, &mut index, 1, &row(&[1, 0])).expect("insert a");
    table.commit(TxnId(1)).expect("commit 1");
    update_row(&mut table, &mut index, 2, a, &row(&[1, 1])).expect("update 2a");
    update_row(&mut table, &mut index, 2, a, &row(&[1, 2])).expect("update 2b");
    assert_eq!(
        seek_rows(
            &index,
            &table,
            &snap(2, &[]),
            &point(&[1]),
            Direction::Forward
        ),
        vec![(a, row(&[1, 2]))]
    );
    assert_eq!(
        seek_rows(
            &index,
            &table,
            &snap(3, &[2]),
            &point(&[1]),
            Direction::Forward
        ),
        vec![(a, row(&[1, 0]))]
    );
    assert_duplicate_key(
        insert_row(&mut table, &mut index, 3, &row(&[1, 9])),
        INDEX,
        "(I32(1))",
    );
}

/// A delete then an insert of one key in one transaction: the key its own delete freed is
/// available to the transaction that freed it, and to nobody else until it commits.
#[test]
fn delete_then_insert_same_txn() {
    let (_dir, storage) = instance("index-delete-then-insert");
    let mut table = table_of(&storage, 2);
    let mut index = DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], true), &table)
        .expect("create the index");
    let a = insert_row(&mut table, &mut index, 1, &row(&[1, 0])).expect("insert a");
    table.commit(TxnId(1)).expect("commit 1");
    delete_row(&mut table, &mut index, 2, a).expect("delete 2");
    let b = insert_row(&mut table, &mut index, 2, &row(&[1, 9])).expect("insert 2");
    assert_eq!(
        seek_rows(
            &index,
            &table,
            &snap(2, &[]),
            &point(&[1]),
            Direction::Forward
        ),
        vec![(b, row(&[1, 9]))]
    );
    assert_eq!(
        seek_rows(
            &index,
            &table,
            &snap(3, &[2]),
            &point(&[1]),
            Direction::Forward
        ),
        vec![(a, row(&[1, 0]))]
    );
    // Another transaction is still blocked: the delete has not committed.
    assert_duplicate_key(
        insert_row(&mut table, &mut index, 3, &row(&[1, 4])),
        INDEX,
        "(I32(1))",
    );
    table.commit(TxnId(2)).expect("commit 2");
    assert_eq!(
        seek_rows(
            &index,
            &table,
            &snap(3, &[]),
            &point(&[1]),
            Direction::Forward
        ),
        vec![(b, row(&[1, 9]))]
    );
    // Insert, delete, insert in one transaction.
    let c = insert_row(&mut table, &mut index, 5, &row(&[5, 0])).expect("insert c");
    delete_row(&mut table, &mut index, 5, c).expect("delete c");
    let d = insert_row(&mut table, &mut index, 5, &row(&[5, 1])).expect("insert d");
    assert_eq!(
        seek_ids(&index, &table, &snap(5, &[]), &point(&[5])),
        vec![d]
    );
}

// --- create -------------------------------------------------------------------------

/// A unique index over two committed rows of one key is 2601, and the pages the refused
/// tree took go back to the free list; the same shape without `unique` passes and indexes
/// both rows.
#[test]
fn create_index_refuses_existing_duplicate() {
    let (_dir, storage) = instance("index-create-refuses-duplicate");
    let mut table = table_of(&storage, 1);
    // A first index, non-unique, so that the writes below go through the same path.
    let mut plain = DiskIndex::create(
        &storage,
        IndexId(1),
        &index_shape(&[(0, false)], false),
        &table,
    )
    .expect("plain index");
    let a = insert_row(&mut table, &mut plain, 1, &row(&[1])).expect("insert a");
    let b = insert_row(&mut table, &mut plain, 1, &row(&[1])).expect("insert b");
    table.commit(TxnId(1)).expect("commit 1");
    let before = storage.control().expect("control").next_page_id;
    let err = DiskIndex::create(
        &storage,
        IndexId(2),
        &index_shape(&[(0, false)], true),
        &table,
    )
    .expect_err("create a unique index over a duplicate");
    assert_eq!(err.number, 2601, "unexpected error: {}", err.message);
    assert_eq!(err.state, 1, "{}", err.message);
    for part in [
        format!("'{TABLE}'"),
        "'2'".to_string(),
        "(I32(1))".to_string(),
    ] {
        assert!(
            err.message.contains(&part),
            "2601 must spell out {part}: {}",
            err.message
        );
    }
    // The refusal keeps no page: the one leaf of the refused tree heads the free list.
    // `next_page_id` grows and does not come back, so it stays one past that leaf.
    let control = storage.control().expect("control");
    assert_eq!(control.next_page_id, PageId(before.0 + 1));
    assert_eq!(control.free_head, Some(before));
    // Without uniqueness the same definition passes and indexes both rows; it takes the
    // freed page back rather than growing the file.
    let late = DiskIndex::create(
        &storage,
        IndexId(3),
        &index_shape(&[(0, false)], false),
        &table,
    )
    .expect("non-unique index over the duplicate");
    assert_eq!(
        seek_ids(&late, &table, &snap(2, &[]), &KeyRange::Full),
        vec![a, b]
    );
    let control = storage.control().expect("control");
    assert_eq!(
        control.next_page_id,
        PageId(before.0 + 1),
        "the freed page was not handed out again"
    );
    assert_eq!(control.free_head, None);
}

/// A version whose creator rolled back is not indexed, and does not count as a duplicate.
#[test]
fn create_index_ignores_aborted_versions() {
    let (_dir, storage) = instance("index-create-ignores-aborted");
    let mut table = table_of(&storage, 1);
    let mut plain = DiskIndex::create(
        &storage,
        IndexId(1),
        &index_shape(&[(0, false)], false),
        &table,
    )
    .expect("plain index");
    let a = insert_row(&mut table, &mut plain, 1, &row(&[1])).expect("insert a");
    table.commit(TxnId(1)).expect("commit 1");
    // A second row of the same key, rolled back before the index is created.
    insert_row(&mut table, &mut plain, 2, &row(&[1])).expect("insert b");
    rollback_txn(&mut table, &mut plain, 2);
    let unique = DiskIndex::create(
        &storage,
        IndexId(2),
        &index_shape(&[(0, false)], true),
        &table,
    )
    .expect("unique index over one live row");
    assert_eq!(
        seek_ids(&unique, &table, &snap(3, &[]), &KeyRange::Full),
        vec![a]
    );
}

/// The shape is kept verbatim, `included` included, and an `included` column outside the
/// table is a caller bug.
#[test]
fn shape_is_kept_and_included_is_checked() {
    let (_dir, storage) = instance("index-shape-included");
    let table = table_of(&storage, 2);
    let shape = IndexShape {
        columns: vec![KeyColumn {
            column: 0,
            descending: false,
        }],
        unique: false,
        included: vec![1],
    };
    let index = DiskIndex::create(&storage, INDEX, &shape, &table).expect("create the index");
    assert_eq!(index.id(), INDEX);
    assert_eq!(index.shape(), &shape);
    let out_of_range = IndexShape {
        included: vec![2],
        ..shape
    };
    let err = DiskIndex::create(&storage, INDEX, &out_of_range, &table)
        .expect_err("included column 2 of a two-column table");
    assert!(
        err.message.contains("included column 2"),
        "unexpected error: {}",
        err.message
    );
}

// --- rollback -----------------------------------------------------------------------

/// A rollback takes back the entries of the versions its undo removes, for an update and
/// for an insert; the entry of the version an update replaced comes back into view.
#[test]
fn rollback_removes_index_entry() {
    let (_dir, storage) = instance("index-rollback-removes-entry");
    let mut table = table_of(&storage, 1);
    let mut index = DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], false), &table)
        .expect("create the index");
    let a = insert_row(&mut table, &mut index, 1, &row(&[1])).expect("insert a");
    let b = insert_row(&mut table, &mut index, 1, &row(&[2])).expect("insert b");
    table.commit(TxnId(1)).expect("commit 1");
    // The undo of an update takes the new entry away and shows the old one again.
    update_row(&mut table, &mut index, 2, a, &row(&[3])).expect("update 2");
    rollback_txn(&mut table, &mut index, 2);
    assert!(seek_ids(&index, &table, &snap(3, &[]), &point(&[3])).is_empty());
    assert_eq!(
        seek_ids(&index, &table, &snap(3, &[]), &point(&[1])),
        vec![a]
    );
    assert_eq!(
        seek_ids(&index, &table, &snap(3, &[]), &KeyRange::Full),
        vec![a, b]
    );
    // The undo of an insert takes its entry away.
    let c = insert_row(&mut table, &mut index, 3, &row(&[7])).expect("insert c");
    assert_eq!(
        seek_ids(&index, &table, &snap(3, &[]), &point(&[7])),
        vec![c]
    );
    rollback_txn(&mut table, &mut index, 3);
    assert!(seek_ids(&index, &table, &snap(4, &[]), &point(&[7])).is_empty());
    assert_eq!(
        seek_ids(&index, &table, &snap(4, &[]), &KeyRange::Full),
        vec![a, b]
    );
    // The key the rolled-back insert held is free for a unique index created afterwards.
    let late = DiskIndex::create(
        &storage,
        IndexId(8),
        &index_shape(&[(0, false)], true),
        &table,
    )
    .expect("unique index after the rollbacks");
    assert_eq!(
        seek_ids(&late, &table, &snap(4, &[]), &KeyRange::Full),
        vec![a, b]
    );
}

// --- payload ------------------------------------------------------------------------

/// The payload reads back, and its big-endian order is the order of `(RowId, seq)`: a row
/// id of 256 sorts above one of 255, which little-endian bytes would have reversed.
#[test]
fn payload_round_trips_and_sorts_by_row_then_seq() {
    assert_eq!(
        read_payload(&payload_of(RowId(258), 3)).expect("read back"),
        (RowId(258), 3)
    );
    assert!(payload_of(RowId(255), 0) < payload_of(RowId(256), 0));
    assert!(payload_of(RowId(1), 1) < payload_of(RowId(1), 2));
    assert!(payload_of(RowId(1), 9) < payload_of(RowId(2), 0));
    let short = read_payload(&[0; 8]).expect_err("a payload of 8 bytes");
    assert!(
        matches!(&short, InternalError::Corruption(message) if message.contains("8 bytes")),
        "unexpected error: {short:?}"
    );
}

// --- pages read by a point seek -------------------------------------------------------

/// Number of pages the buffer pool of `storage` holds.
///
/// A pool fills its frames in order and reuses one only once it holds its capacity, so below
/// that capacity this is the number of distinct pages read since the pool was built. The count
/// is taken from the `Debug` form of the pool, which prints one `Frame { .. }` per frame and
/// none of the bytes of a page.
fn resident_pages(storage: &DiskStorage) -> usize {
    format!("{:?}", storage.pool).matches("Frame {").count()
}

/// Levels of the tree rooted at `root`, the root and the leaf both counted.
fn tree_height(storage: &DiskStorage, root: PageId) -> usize {
    let mut height = 1;
    let mut page = root;
    loop {
        let pin = storage.pool.pin(page).expect("pin a page of the tree");
        let child = pin
            .with_page(|page| match page.kind().expect("page kind") {
                PageKind::BTreeInternal => Some(leftmost_child(page)),
                _ => None,
            })
            .expect("read a page of the tree");
        match child {
            Some(child) => {
                page = child;
                height += 1;
            }
            None => return height,
        }
    }
}

/// A point seek on an index of 100 000 rows reads the pages of one descent of the index tree,
/// at most one more leaf, and the heap page of the version it names: `height + 2` at most,
/// where reading the versions of the whole table would read each heap page.
///
/// The instance is reopened before the seek so that its pool starts empty, and the state of
/// the table is carried over, as the instance carries it between two calls. An insert looks
/// for room from the head page of the heap on; to keep that walk short, the load attaches the
/// table at the page of the last row written every [`REATTACH`] rows, the pages staying
/// chained from the head.
#[test]
fn point_seek_on_100000_rows_reads_at_most_height_plus_two_pages() {
    const ROWS: i32 = 100_000;
    const BATCH: i32 = 5_000;
    const REATTACH: i32 = 100;
    let dir = TempDir::created("index-seek-pages");
    let opts = DiskOptions { buffer_pages: 8192 };
    let shape = index_shape(&[(0, false)], false);
    let (state, table_shape, head, root, height) = {
        let storage = DiskStorage::open(dir.path(), opts).expect("create an instance");
        let mut table = table_of(&storage, 2);
        let head = table.heap().first_page();
        let table_shape = table.shape().clone();
        let mut index =
            DiskIndex::create(&storage, INDEX, &shape, &table).expect("create the index");
        let mut txn = 10;
        let mut last = None;
        for value in 0..ROWS {
            if value % REATTACH == 0
                && let Some(id) = last
            {
                let state = table.into_state();
                let tail = state.directory[&id].page;
                let heap = super::super::heap::Heap::open(&storage, TABLE, tail);
                table = HeapTable::resume(&storage, heap, table_shape.clone(), state);
            }
            last = Some(
                insert_row(&mut table, &mut index, txn, &row(&[value, value])).expect("insert"),
            );
            if (value + 1) % BATCH == 0 {
                table.commit(TxnId(txn)).expect("commit");
                txn += 1;
            }
        }
        storage.checkpoint().expect("checkpoint");
        let height = tree_height(&storage, index.root());
        (table.into_state(), table_shape, head, index.root(), height)
    };
    assert!(height >= 2, "a tree of 100 000 entries has internal pages");

    let storage = DiskStorage::open(dir.path(), opts).expect("reopen the instance");
    let heap = super::super::heap::Heap::open(&storage, TABLE, head);
    let table = HeapTable::resume(&storage, heap, table_shape.clone(), state);
    let index = DiskIndex::open(&storage, INDEX, &shape, root, &table_shape).expect("open");
    let before = resident_pages(&storage);
    let rows = index
        .seek(
            &table,
            &snap(1_000, &[]),
            &point(&[54_321]),
            Direction::Forward,
        )
        .expect("seek");
    let read = resident_pages(&storage) - before;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, row(&[54_321, 54_321]));
    assert!(
        read <= height + 2,
        "a point seek read {read} pages, the index tree has {height} levels"
    );
    // The whole table is still reachable from the head page.
    assert_eq!(
        table.scan(&snap(1_000, &[])).expect("scan").len(),
        usize::try_from(ROWS).expect("row count")
    );
}

/// The uniqueness of an index that shares the tree of a clustered table seeks its key: on a
/// table of 20 000 rows attached on a cold pool, refusing a duplicate and accepting a fresh key
/// each read at most the height of the tree plus two pages, where a walk of the versions of
/// the table reads several times as many.
#[test]
fn shared_tree_uniqueness_reads_a_bounded_number_of_pages() {
    use super::super::clustered::ClusteredTable;
    const ROWS: i32 = 20_000;
    const BATCH: i32 = 5_000;
    let dir = TempDir::created("index-shared-tree-pages");
    let opts = DiskOptions { buffer_pages: 8192 };
    let shape = TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::Int, true),
        ],
        clustered_key: Some(vec![KeyColumn {
            column: 0,
            descending: false,
        }]),
    };
    let unique = index_shape(&[(0, false)], true);
    let (head, roots) = {
        let storage = DiskStorage::open(dir.path(), opts).expect("create an instance");
        let head = alloc::allocate(&storage).expect("allocate the head page");
        let mut table =
            ClusteredTable::create(&storage, TABLE, head, shape.clone()).expect("create");
        let mut txn = 10;
        for value in 0..ROWS {
            table
                .insert(TxnId(txn), &row(&[value, value]))
                .expect("insert");
            if (value + 1) % BATCH == 0 {
                table.commit(TxnId(txn)).expect("commit");
                txn += 1;
            }
        }
        storage.checkpoint().expect("checkpoint");
        (head, (table.tree_root(), table.directory_root()))
    };

    let storage = DiskStorage::open(dir.path(), opts).expect("reopen the instance");
    let height = tree_height(&storage, roots.0);
    assert!(height >= 2, "a tree of 20 000 entries has internal pages");
    let bound = height + 2;
    let table = ClusteredTable::open(&storage, TABLE, head, roots, shape).expect("attach");
    let before = resident_pages(&storage);
    let refused =
        check_duplicate_without_tree(&table, INDEX, &unique, TxnId(20), &row(&[12_345, 0]), None);
    let duplicate = resident_pages(&storage) - before;
    assert_eq!(refused.expect_err("a duplicate").number, 2601);
    let before = resident_pages(&storage);
    check_duplicate_without_tree(
        &table,
        INDEX,
        &unique,
        TxnId(20),
        &row(&[ROWS + 1, 0]),
        None,
    )
    .expect("a fresh key");
    let fresh = resident_pages(&storage) - before;
    let before = resident_pages(&storage);
    let all = table.versions(None).expect("versions");
    let walk = resident_pages(&storage) - before;
    assert_eq!(all.len(), usize::try_from(ROWS).expect("row count"));
    assert!(
        duplicate <= bound,
        "refusing a duplicate read {duplicate} pages, the tree has {height} levels"
    );
    assert!(
        fresh <= bound,
        "accepting a fresh key read {fresh} pages, the tree has {height} levels"
    );
    assert!(
        walk > 4 * bound,
        "a walk of the versions read {walk} pages, too few for the bound to tell it apart"
    );
}
