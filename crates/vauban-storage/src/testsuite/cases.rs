//! The scenarios of the contract suite, one `case_*` function each. See the module
//! documentation of [`crate::testsuite`] for the list and what each one checks.
//!
//! Every function takes a factory of fresh storages and uses nothing but the public API of
//! the [`Storage`] trait and its types: whatever is asserted here is the contract written in
//! the rustdoc of the trait, not a property of one implementation. `TxnId`s are small
//! integers in order of start; snapshots come from [`snap`].

use std::ops::Bound;

use crate::testsuite::{
    assert_bug, assert_duplicate_key, clustered_int_table_shape, collect, index_shape,
    int_table_shape, key, key_columns, nullable_key, nullable_row, row, row_ids, snap,
};
use crate::{
    DbId, Direction, IndexId, IndexShape, KeyRange, Row, RowId, SavepointId, Snapshot, Storage,
    TableId, TableShape, TxnId,
};

// ------------------------------------------------------------------ helpers

/// A fresh storage with one database and one table of `n_cols` `int` columns.
fn one_table(
    make: &dyn Fn() -> Box<dyn Storage>,
    n_cols: usize,
) -> (Box<dyn Storage>, DbId, TableId) {
    let s = make();
    let db = s.create_database("db").expect("create_database");
    let t = s
        .create_table(db, &int_table_shape(n_cols))
        .expect("create_table");
    (s, db, t)
}

/// The rows of `scan`, sorted by `RowId`: the order of a table without clustered key is not
/// part of the contract.
fn scan_sorted(s: &dyn Storage, sn: &Snapshot, t: TableId) -> Vec<(RowId, Row)> {
    let mut rows = collect(s.scan(sn, t).expect("scan"));
    rows.sort_by_key(|(id, _)| *id);
    rows
}

/// The rows of `scan` in the order the storage yields them (tables with a clustered key).
fn scan_ordered(s: &dyn Storage, sn: &Snapshot, t: TableId) -> Vec<(RowId, Row)> {
    collect(s.scan(sn, t).expect("scan"))
}

fn seek_rows(
    s: &dyn Storage,
    sn: &Snapshot,
    i: IndexId,
    range: &KeyRange,
    dir: Direction,
) -> Vec<(RowId, Row)> {
    collect(s.seek(sn, i, range, dir).expect("seek"))
}

/// The `RowId`s of a forward `seek`.
fn seek_ids(s: &dyn Storage, sn: &Snapshot, i: IndexId, range: &KeyRange) -> Vec<RowId> {
    row_ids(&seek_rows(s, sn, i, range, Direction::Forward))
}

/// The `RowId`s of a backward `seek`.
fn seek_ids_back(s: &dyn Storage, sn: &Snapshot, i: IndexId, range: &KeyRange) -> Vec<RowId> {
    row_ids(&seek_rows(s, sn, i, range, Direction::Backward))
}

fn get(s: &dyn Storage, sn: &Snapshot, t: TableId, id: RowId) -> Option<Row> {
    s.get(sn, t, id).expect("get")
}

fn latest(s: &dyn Storage, t: TableId, id: RowId) -> Option<(TxnId, Row)> {
    s.latest_version(t, id).expect("latest_version")
}

fn point(values: &[i32]) -> KeyRange {
    KeyRange::Point(key(values))
}

fn between(lo: Bound<&[i32]>, hi: Bound<&[i32]>) -> KeyRange {
    KeyRange::Between(lo.map(key), hi.map(key))
}

/// Inserts every row on behalf of `txn` and returns the ids, in order.
fn insert_all(s: &dyn Storage, txn: u64, t: TableId, rows: &[Row]) -> Vec<RowId> {
    rows.iter()
        .map(|r| s.insert(TxnId(txn), t, r).expect("insert"))
        .collect()
}

/// `ids[k]` for every `k` of `order`.
fn pick(ids: &[RowId], order: &[usize]) -> Vec<RowId> {
    order.iter().map(|&k| ids[k]).collect()
}

// ---------------------------------------------------------------- databases

/// See [`crate::testsuite`]: `databases_create_list_drop`.
pub fn case_databases_create_list_drop(make: &dyn Fn() -> Box<dyn Storage>) {
    let s = make();
    assert_eq!(s.databases().expect("databases"), vec![]);
    let a = s.create_database("alpha").expect("create alpha");
    // Names are stored verbatim: spaces and case are kept.
    let b = s.create_database(" Beta ").expect("create beta");
    assert!(a < b, "DbIds must increase: {a:?} then {b:?}");
    assert_eq!(
        s.databases().expect("databases"),
        vec![(a, "alpha".to_owned()), (b, " Beta ".to_owned())]
    );
    // A database starts empty.
    assert_eq!(s.tables(a).expect("tables"), vec![]);
    // Its tables and indexes go with it, in-flight writes included.
    let t = s
        .create_table(a, &int_table_shape(1))
        .expect("create_table");
    let i = s
        .create_index(t, &index_shape(&[(0, false)], false))
        .expect("create_index");
    s.insert(TxnId(1), t, &row(&[1])).expect("insert");
    s.drop_database(a).expect("drop_database");
    assert_eq!(
        s.databases().expect("databases"),
        vec![(b, " Beta ".to_owned())]
    );
    assert_bug(s.drop_database(a));
    assert_bug(s.tables(a));
    assert_bug(s.create_table(a, &int_table_shape(1)));
    assert_bug(s.drop_table(t));
    assert_bug(s.indexes(t));
    assert_bug(s.drop_index(i));
    assert_bug(s.insert(TxnId(1), t, &row(&[2])));
    assert_bug(s.scan(&snap(1, &[]), t).map(|_| ()));
    // The write of 1 on the dropped table is silently ignored by its commit.
    s.commit(TxnId(1)).expect("commit after drop_database");
    // Ids are never reused, even for the same name.
    let c = s.create_database("alpha").expect("create alpha again");
    assert!(c > b, "DbId reused after drop: {c:?}");
    assert_eq!(
        s.databases().expect("databases"),
        vec![(b, " Beta ".to_owned()), (c, "alpha".to_owned())]
    );
}

/// See [`crate::testsuite`]: `table_create_drop_and_ids_not_reused`.
pub fn case_table_create_drop_and_ids_not_reused(make: &dyn Fn() -> Box<dyn Storage>) {
    let s = make();
    let db1 = s.create_database("one").expect("create one");
    let db2 = s.create_database("two").expect("create two");
    let t1 = s.create_table(db1, &int_table_shape(1)).expect("t1");
    let t2 = s.create_table(db1, &int_table_shape(2)).expect("t2");
    let t3 = s.create_table(db2, &int_table_shape(1)).expect("t3");
    // Unique within the whole instance, increasing.
    assert!(
        t1 < t2 && t2 < t3,
        "TableIds must increase: {t1:?} {t2:?} {t3:?}"
    );
    assert_eq!(ids_of_tables(&*s, db1), vec![t1, t2]);
    assert_eq!(ids_of_tables(&*s, db2), vec![t3]);
    // Rows and an index on t1, one write committed, one in flight.
    let i = s
        .create_index(t1, &index_shape(&[(0, false)], false))
        .expect("create_index");
    s.insert(TxnId(1), t1, &row(&[1])).expect("insert");
    s.insert(TxnId(2), t1, &row(&[2])).expect("insert");
    s.drop_table(t1).expect("drop_table");
    assert_eq!(ids_of_tables(&*s, db1), vec![t2]);
    assert_eq!(ids_of_tables(&*s, db2), vec![t3]);
    assert_bug(s.drop_table(t1));
    assert_bug(s.indexes(t1));
    assert_bug(s.drop_index(i));
    assert_bug(s.insert(TxnId(1), t1, &row(&[3])));
    assert_bug(s.scan(&snap(1, &[]), t1).map(|_| ()));
    assert_bug(
        s.seek(&snap(1, &[]), i, &KeyRange::Full, Direction::Forward)
            .map(|_| ()),
    );
    // In-flight writes on the dropped table are ignored by commit and rollback alike.
    s.commit(TxnId(1)).expect("commit after drop_table");
    s.rollback(TxnId(2)).expect("rollback after drop_table");
    // Fresh ids, never the dropped ones.
    let t4 = s.create_table(db1, &int_table_shape(1)).expect("t4");
    assert!(t4 > t3, "TableId reused after drop: {t4:?}");
    assert_eq!(ids_of_tables(&*s, db1), vec![t2, t4]);
    let j = s
        .create_index(t4, &index_shape(&[(0, false)], false))
        .expect("create_index");
    assert!(j > i, "IndexId reused after drop_table: {j:?}");
    // The other tables are untouched.
    let id = s.insert(TxnId(3), t2, &row(&[1, 2])).expect("insert");
    assert_eq!(get(&*s, &snap(3, &[]), t2, id), Some(row(&[1, 2])));
}

fn ids_of_tables(s: &dyn Storage, db: DbId) -> Vec<TableId> {
    s.tables(db)
        .expect("tables")
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// See [`crate::testsuite`]: `tables_and_indexes_introspection`.
pub fn case_tables_and_indexes_introspection(make: &dyn Fn() -> Box<dyn Storage>) {
    let s = make();
    let db = s.create_database("db").expect("create db");
    let other = s.create_database("other").expect("create other");
    let plain = int_table_shape(1);
    let clustered = clustered_int_table_shape(3, &[(2, true), (0, false)]);
    let t1 = s.create_table(db, &clustered).expect("t1");
    let t2 = s.create_table(db, &plain).expect("t2");
    let t3 = s.create_table(other, &plain).expect("t3");
    assert_eq!(
        s.tables(db).expect("tables"),
        vec![(t1, clustered.clone()), (t2, plain.clone())]
    );
    assert_eq!(s.tables(other).expect("tables"), vec![(t3, plain.clone())]);
    // Index shapes are returned verbatim, `unique` and `included` included.
    let ix_a = IndexShape {
        columns: key_columns(&[(0, false), (1, true)]),
        unique: true,
        included: vec![2],
    };
    let ix_b = index_shape(&[(2, false)], false);
    let ix_c = index_shape(&[(0, false)], true);
    let i1 = s.create_index(t1, &ix_a).expect("i1");
    let i2 = s.create_index(t1, &ix_b).expect("i2");
    let i3 = s.create_index(t2, &ix_c).expect("i3");
    assert!(
        i1 < i2 && i2 < i3,
        "IndexIds must increase: {i1:?} {i2:?} {i3:?}"
    );
    assert_eq!(
        s.indexes(t1).expect("indexes"),
        vec![(i1, ix_a.clone()), (i2, ix_b.clone())]
    );
    assert_eq!(s.indexes(t2).expect("indexes"), vec![(i3, ix_c)]);
    assert_eq!(s.indexes(t3).expect("indexes"), vec![]);
    // Introspection has no MVCC: a pending write changes nothing to the listings.
    s.insert(TxnId(1), t2, &row(&[1])).expect("insert");
    assert_eq!(ids_of_tables(&*s, db), vec![t1, t2]);
    // Dropped objects disappear at once.
    s.drop_index(i1).expect("drop_index");
    assert_eq!(s.indexes(t1).expect("indexes"), vec![(i2, ix_b)]);
    assert_bug(s.drop_index(i1));
    s.drop_table(t1).expect("drop_table");
    assert_eq!(s.tables(db).expect("tables"), vec![(t2, plain)]);
    assert_bug(s.indexes(t1));
    assert_bug(s.drop_index(i2));
    s.rollback(TxnId(1)).expect("rollback");
}

// --------------------------------------------------------------- visibility

/// See [`crate::testsuite`]: `insert_get_scan_visibility_before_and_after_commit`.
pub fn case_insert_get_scan_visibility_before_and_after_commit(
    make: &dyn Fn() -> Box<dyn Storage>,
) {
    let (s, _, t) = one_table(make, 1);
    let own = snap(1, &[]);
    assert!(scan_sorted(&*s, &own, t).is_empty());
    // An unknown row is `None`, never an error.
    assert_eq!(get(&*s, &own, t, RowId(1)), None);
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    let b = s.insert(TxnId(1), t, &row(&[2])).expect("insert b");
    assert_ne!(a, b, "RowIds must be distinct within a table");
    // The writer sees its own uncommitted rows.
    assert_eq!(get(&*s, &own, t, a), Some(row(&[1])));
    assert_eq!(get(&*s, &own, t, b), Some(row(&[2])));
    assert_eq!(
        scan_sorted(&*s, &own, t),
        vec![(a, row(&[1])), (b, row(&[2]))]
    );
    // Nobody else does: 2 reads while 1 is in progress.
    let concurrent = snap(2, &[1]);
    assert_eq!(get(&*s, &concurrent, t, a), None);
    assert!(scan_sorted(&*s, &concurrent, t).is_empty());
    // A snapshot whose `xmax` does not cover 1 does not see it either.
    let too_old = Snapshot {
        xmin: TxnId(1),
        xmax: TxnId(1),
        active: vec![],
        own: TxnId(3),
    };
    assert_eq!(get(&*s, &too_old, t, a), None);
    s.commit(TxnId(1)).expect("commit");
    // A snapshot taken after the commit sees both rows, exactly once each.
    let after = snap(2, &[]);
    assert_eq!(get(&*s, &after, t, a), Some(row(&[1])));
    assert_eq!(get(&*s, &after, t, b), Some(row(&[2])));
    assert_eq!(
        scan_sorted(&*s, &after, t),
        vec![(a, row(&[1])), (b, row(&[2]))]
    );
    // Snapshots taken before the commit keep not seeing them.
    assert_eq!(get(&*s, &concurrent, t, a), None);
    assert!(scan_sorted(&*s, &concurrent, t).is_empty());
    assert_eq!(get(&*s, &too_old, t, b), None);
    assert!(scan_sorted(&*s, &too_old, t).is_empty());
}

/// See [`crate::testsuite`]: `rollback_undoes_insert`.
pub fn case_rollback_undoes_insert(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    let b = s.insert(TxnId(1), t, &row(&[2])).expect("insert b");
    s.rollback(TxnId(1)).expect("rollback");
    for sn in [snap(1, &[]), snap(2, &[]), snap(2, &[1])] {
        assert_eq!(get(&*s, &sn, t, a), None, "snapshot {sn:?}");
        assert_eq!(get(&*s, &sn, t, b), None, "snapshot {sn:?}");
        assert!(scan_sorted(&*s, &sn, t).is_empty(), "snapshot {sn:?}");
    }
    assert_eq!(latest(&*s, t, a), None);
    assert_eq!(latest(&*s, t, b), None);
    // 1 is finished.
    assert_bug(s.insert(TxnId(1), t, &row(&[3])));
    assert_bug(s.rollback(TxnId(1)));
    assert_bug(s.commit(TxnId(1)));
    assert_bug(s.savepoint(TxnId(1)));
    // RowIds are never reused.
    let c = s.insert(TxnId(2), t, &row(&[3])).expect("insert c");
    assert!(c > b, "RowId reused after rollback: {c:?}");
    s.commit(TxnId(2)).expect("commit");
    assert_eq!(scan_sorted(&*s, &snap(3, &[]), t), vec![(c, row(&[3]))]);
}

/// See [`crate::testsuite`]: `update_versions_and_snapshot_isolation`.
pub fn case_update_versions_and_snapshot_isolation(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let id = s.insert(TxnId(1), t, &row(&[1])).expect("insert");
    s.commit(TxnId(1)).expect("commit 1");
    s.update(TxnId(2), t, id, &row(&[2])).expect("update");
    // The writer sees the new version under the same RowId, others keep the old one.
    assert_eq!(get(&*s, &snap(2, &[]), t, id), Some(row(&[2])));
    assert_eq!(scan_sorted(&*s, &snap(2, &[]), t), vec![(id, row(&[2]))]);
    let frozen = snap(3, &[2]);
    assert_eq!(get(&*s, &frozen, t, id), Some(row(&[1])));
    assert_eq!(scan_sorted(&*s, &frozen, t), vec![(id, row(&[1]))]);
    s.commit(TxnId(2)).expect("commit 2");
    assert_eq!(get(&*s, &snap(3, &[]), t, id), Some(row(&[2])));
    assert_eq!(scan_sorted(&*s, &snap(3, &[]), t), vec![(id, row(&[2]))]);
    // A snapshot that listed 2 as active keeps the old version.
    assert_eq!(get(&*s, &frozen, t, id), Some(row(&[1])));
    // Two successive updates by the same transaction, RowId kept, one visible version.
    s.update(TxnId(3), t, id, &row(&[3])).expect("update 3a");
    s.update(TxnId(3), t, id, &row(&[4])).expect("update 3b");
    assert_eq!(get(&*s, &snap(3, &[]), t, id), Some(row(&[4])));
    assert_eq!(scan_sorted(&*s, &snap(3, &[]), t), vec![(id, row(&[4]))]);
    assert_eq!(get(&*s, &snap(4, &[3]), t, id), Some(row(&[2])));
    s.rollback(TxnId(3)).expect("rollback 3");
    assert_eq!(get(&*s, &snap(4, &[]), t, id), Some(row(&[2])));
    assert_eq!(scan_sorted(&*s, &snap(4, &[]), t), vec![(id, row(&[2]))]);
}

/// See [`crate::testsuite`]: `delete_visibility`.
pub fn case_delete_visibility(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    let b = s.insert(TxnId(1), t, &row(&[2])).expect("insert b");
    s.commit(TxnId(1)).expect("commit 1");
    s.delete(TxnId(2), t, a).expect("delete");
    // Gone for the writer, still there for a concurrent snapshot.
    assert_eq!(get(&*s, &snap(2, &[]), t, a), None);
    assert_eq!(scan_sorted(&*s, &snap(2, &[]), t), vec![(b, row(&[2]))]);
    let frozen = snap(3, &[2]);
    assert_eq!(get(&*s, &frozen, t, a), Some(row(&[1])));
    assert_eq!(
        scan_sorted(&*s, &frozen, t),
        vec![(a, row(&[1])), (b, row(&[2]))]
    );
    s.commit(TxnId(2)).expect("commit 2");
    assert_eq!(get(&*s, &snap(3, &[]), t, a), None);
    assert_eq!(scan_sorted(&*s, &snap(3, &[]), t), vec![(b, row(&[2]))]);
    assert_eq!(get(&*s, &frozen, t, a), Some(row(&[1])));
    // A rolled-back delete hides nothing.
    s.delete(TxnId(4), t, b).expect("delete b");
    assert_eq!(get(&*s, &snap(4, &[]), t, b), None);
    s.rollback(TxnId(4)).expect("rollback 4");
    assert_eq!(get(&*s, &snap(5, &[]), t, b), Some(row(&[2])));
    assert_eq!(scan_sorted(&*s, &snap(5, &[]), t), vec![(b, row(&[2]))]);
}

/// See [`crate::testsuite`]: `own_insert_then_delete_invisible_to_self`.
pub fn case_own_insert_then_delete_invisible_to_self(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert");
    s.delete(TxnId(1), t, a).expect("delete");
    assert_eq!(get(&*s, &snap(1, &[]), t, a), None);
    assert!(scan_sorted(&*s, &snap(1, &[]), t).is_empty());
    assert_eq!(get(&*s, &snap(2, &[1]), t, a), None);
    // The deleter is the writer of the most recent state.
    assert_eq!(latest(&*s, t, a), Some((TxnId(1), row(&[1]))));
    s.commit(TxnId(1)).expect("commit");
    assert_eq!(get(&*s, &snap(2, &[]), t, a), None);
    assert!(scan_sorted(&*s, &snap(2, &[]), t).is_empty());
    // The row is stale for everyone.
    assert_bug(s.update(TxnId(2), t, a, &row(&[2])));
    assert_bug(s.delete(TxnId(2), t, a));
    let b = s.insert(TxnId(2), t, &row(&[1])).expect("insert again");
    assert!(b > a, "RowId reused: {b:?}");
}

/// See [`crate::testsuite`]: `latest_version_semantics`.
pub fn case_latest_version_semantics(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    assert_eq!(latest(&*s, t, RowId(1)), None);
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert");
    // Snapshots are ignored: an uncommitted version is reported.
    assert_eq!(latest(&*s, t, a), Some((TxnId(1), row(&[1]))));
    s.commit(TxnId(1)).expect("commit 1");
    assert_eq!(latest(&*s, t, a), Some((TxnId(1), row(&[1]))));
    s.update(TxnId(2), t, a, &row(&[2])).expect("update 2");
    assert_eq!(latest(&*s, t, a), Some((TxnId(2), row(&[2]))));
    s.rollback(TxnId(2)).expect("rollback 2");
    assert_eq!(latest(&*s, t, a), Some((TxnId(1), row(&[1]))));
    s.update(TxnId(3), t, a, &row(&[3])).expect("update 3");
    s.commit(TxnId(3)).expect("commit 3");
    assert_eq!(latest(&*s, t, a), Some((TxnId(3), row(&[3]))));
    // A deleted row reports its deleter and the deleted content.
    s.delete(TxnId(4), t, a).expect("delete 4");
    assert_eq!(latest(&*s, t, a), Some((TxnId(4), row(&[3]))));
    s.rollback(TxnId(4)).expect("rollback 4");
    assert_eq!(latest(&*s, t, a), Some((TxnId(3), row(&[3]))));
    s.delete(TxnId(5), t, a).expect("delete 5");
    s.commit(TxnId(5)).expect("commit 5");
    assert_eq!(latest(&*s, t, a), Some((TxnId(5), row(&[3]))));
    // Vacuumed: `None`.
    s.vacuum(TxnId(6)).expect("vacuum");
    assert_eq!(latest(&*s, t, a), None);
    // Rolled-back insert: `None`.
    let b = s.insert(TxnId(7), t, &row(&[7])).expect("insert 7");
    s.rollback(TxnId(7)).expect("rollback 7");
    assert_eq!(latest(&*s, t, b), None);
}

// ---------------------------------------------------------------- savepoints

/// See [`crate::testsuite`]: `savepoint_rollback_to_partial_and_repeated`.
pub fn case_savepoint_rollback_to_partial_and_repeated(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let c = s.insert(TxnId(1), t, &row(&[1])).expect("insert c");
    s.commit(TxnId(1)).expect("commit 1");
    let a = s.insert(TxnId(2), t, &row(&[10])).expect("insert a");
    let sp = s.savepoint(TxnId(2)).expect("savepoint");
    let b = s.insert(TxnId(2), t, &row(&[20])).expect("insert b");
    s.update(TxnId(2), t, a, &row(&[11])).expect("update a");
    s.delete(TxnId(2), t, c).expect("delete c");
    let own = snap(2, &[]);
    assert_eq!(
        scan_sorted(&*s, &own, t),
        vec![(a, row(&[11])), (b, row(&[20]))]
    );
    s.rollback_to(TxnId(2), sp).expect("rollback_to");
    // `a` is back to its content before the savepoint, `b` is gone, `c` is undeleted.
    assert_eq!(
        scan_sorted(&*s, &own, t),
        vec![(c, row(&[1])), (a, row(&[10]))]
    );
    assert_eq!(get(&*s, &own, t, b), None);
    assert_eq!(latest(&*s, t, b), None);
    assert_eq!(latest(&*s, t, a), Some((TxnId(2), row(&[10]))));
    assert_eq!(latest(&*s, t, c), Some((TxnId(1), row(&[1]))));
    // Still in progress: it can write again, with fresh RowIds.
    let d = s.insert(TxnId(2), t, &row(&[30])).expect("insert d");
    assert!(d > b, "RowId reused after rollback_to: {d:?}");
    // The savepoint stays valid and can be returned to again and again.
    s.rollback_to(TxnId(2), sp).expect("rollback_to again");
    assert_eq!(
        scan_sorted(&*s, &own, t),
        vec![(c, row(&[1])), (a, row(&[10]))]
    );
    s.insert(TxnId(2), t, &row(&[40])).expect("insert e");
    s.delete(TxnId(2), t, c).expect("delete c again");
    s.rollback_to(TxnId(2), sp).expect("rollback_to third");
    assert_eq!(
        scan_sorted(&*s, &own, t),
        vec![(c, row(&[1])), (a, row(&[10]))]
    );
    s.rollback_to(TxnId(2), sp)
        .expect("rollback_to with nothing to undo");
    let f = s.insert(TxnId(2), t, &row(&[50])).expect("insert f");
    s.commit(TxnId(2)).expect("commit 2");
    assert_eq!(
        scan_sorted(&*s, &snap(3, &[]), t),
        vec![(c, row(&[1])), (a, row(&[10])), (f, row(&[50]))]
    );
    // Savepoint ids increase within a transaction.
    let sp1 = s.savepoint(TxnId(4)).expect("sp1");
    let sp2 = s.savepoint(TxnId(4)).expect("sp2");
    assert!(
        sp1 < sp2,
        "savepoint ids must increase: {sp1:?} then {sp2:?}"
    );
    s.rollback(TxnId(4)).expect("rollback 4");
}

/// See [`crate::testsuite`]: `savepoint_invalidated_after_rollback_to`.
pub fn case_savepoint_invalidated_after_rollback_to(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let sp1 = s.savepoint(TxnId(1)).expect("sp1");
    s.insert(TxnId(1), t, &row(&[1])).expect("insert 1");
    let sp2 = s.savepoint(TxnId(1)).expect("sp2");
    assert!(sp1 < sp2);
    s.insert(TxnId(1), t, &row(&[2])).expect("insert 2");
    s.rollback_to(TxnId(1), sp1).expect("rollback_to sp1");
    // Savepoints taken after the target are invalid; the target survives.
    assert_bug(s.rollback_to(TxnId(1), sp2));
    assert!(scan_sorted(&*s, &snap(1, &[]), t).is_empty());
    s.rollback_to(TxnId(1), sp1).expect("rollback_to sp1 again");
    let sp3 = s.savepoint(TxnId(1)).expect("sp3");
    assert!(sp3 > sp2, "savepoint id reused: {sp3:?}");
    // Another transaction's savepoint, a made-up one, an unknown transaction.
    s.insert(TxnId(2), t, &row(&[3])).expect("insert 3");
    assert_bug(s.rollback_to(TxnId(2), sp1));
    assert_bug(s.rollback_to(TxnId(1), SavepointId(u64::MAX)));
    assert_bug(s.rollback_to(TxnId(7), sp1));
    assert_eq!(scan_sorted(&*s, &snap(2, &[]), t).len(), 1);
    // Savepoints die with their transaction.
    s.commit(TxnId(1)).expect("commit 1");
    assert_bug(s.savepoint(TxnId(1)));
    assert_bug(s.rollback_to(TxnId(1), sp1));
    assert_bug(s.rollback_to(TxnId(1), sp3));
    let sp4 = s.savepoint(TxnId(2)).expect("sp4");
    s.rollback(TxnId(2)).expect("rollback 2");
    assert_bug(s.savepoint(TxnId(2)));
    assert_bug(s.rollback_to(TxnId(2), sp4));
    // A savepoint before any write registers the transaction, which then finishes once.
    s.savepoint(TxnId(5)).expect("sp before write");
    s.commit(TxnId(5)).expect("commit 5");
    assert_bug(s.savepoint(TxnId(5)));
    assert_bug(s.commit(TxnId(5)));
}

/// See [`crate::testsuite`]: `update_on_superseded_version_is_bug`.
pub fn case_update_on_superseded_version_is_bug(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    let b = s.insert(TxnId(1), t, &row(&[2])).expect("insert b");
    s.commit(TxnId(1)).expect("commit 1");
    // A pending update by 2: 3 may neither update nor delete, and nothing changes.
    s.update(TxnId(2), t, a, &row(&[20])).expect("update 2");
    assert_bug(s.update(TxnId(3), t, a, &row(&[30])));
    assert_bug(s.delete(TxnId(3), t, a));
    assert_eq!(get(&*s, &snap(2, &[]), t, a), Some(row(&[20])));
    assert_eq!(get(&*s, &snap(3, &[2]), t, a), Some(row(&[1])));
    assert_eq!(latest(&*s, t, a), Some((TxnId(2), row(&[20]))));
    // Once 2 is committed, the latest version is current again: the storage accepts.
    s.commit(TxnId(2)).expect("commit 2");
    s.update(TxnId(3), t, a, &row(&[30])).expect("update 3");
    assert_eq!(get(&*s, &snap(3, &[]), t, a), Some(row(&[30])));
    // A row deleted by the writer itself is stale for it too.
    s.delete(TxnId(3), t, a).expect("delete 3");
    assert_bug(s.update(TxnId(3), t, a, &row(&[31])));
    assert_bug(s.delete(TxnId(3), t, a));
    s.rollback(TxnId(3)).expect("rollback 3");
    assert_eq!(get(&*s, &snap(4, &[]), t, a), Some(row(&[20])));
    assert_eq!(latest(&*s, t, a), Some((TxnId(2), row(&[20]))));
    // An uncommitted delete by another transaction blocks; its rollback unblocks.
    s.delete(TxnId(4), t, b).expect("delete 4");
    assert_bug(s.update(TxnId(5), t, b, &row(&[50])));
    assert_bug(s.delete(TxnId(5), t, b));
    assert_eq!(latest(&*s, t, b), Some((TxnId(4), row(&[2]))));
    s.rollback(TxnId(4)).expect("rollback 4");
    s.delete(TxnId(5), t, b).expect("delete 5");
    s.commit(TxnId(5)).expect("commit 5");
    assert_eq!(get(&*s, &snap(6, &[]), t, b), None);
    // A version whose creator rolled back is gone: the row is unknown.
    let c = s.insert(TxnId(6), t, &row(&[6])).expect("insert c");
    s.rollback(TxnId(6)).expect("rollback 6");
    assert_bug(s.update(TxnId(7), t, c, &row(&[7])));
    assert_bug(s.delete(TxnId(7), t, c));
}

// ------------------------------------------------------------------- indexes

/// See [`crate::testsuite`]: `index_seek_point_between_full_both_directions`.
pub fn case_index_seek_point_between_full_both_directions(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 2);
    let i = s
        .create_index(t, &index_shape(&[(0, false), (1, false)], false))
        .expect("create_index");
    let rows = [
        row(&[1, 5]),
        row(&[1, 0]),
        row(&[2, 3]),
        row(&[0, 9]),
        row(&[1, 5]),
        row(&[3, 1]),
    ];
    let ids = insert_all(&*s, 1, t, &rows);
    s.commit(TxnId(1)).expect("commit");
    let sn = snap(2, &[]);
    // Index order: (0,9) (1,0) (1,5)#0 (1,5)#4 (2,3) (3,1); ties by RowId.
    let forward = pick(&ids, &[3, 1, 0, 4, 2, 5]);
    let full = seek_rows(&*s, &sn, i, &KeyRange::Full, Direction::Forward);
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
        seek_ids_back(&*s, &sn, i, &KeyRange::Full),
        pick(&ids, &[5, 2, 4, 0, 1, 3])
    );
    // Point: full key, prefix, empty (= Full), absent.
    assert_eq!(seek_ids(&*s, &sn, i, &point(&[1, 5])), pick(&ids, &[0, 4]));
    assert_eq!(
        seek_ids_back(&*s, &sn, i, &point(&[1, 5])),
        pick(&ids, &[4, 0])
    );
    assert_eq!(seek_ids(&*s, &sn, i, &point(&[1])), pick(&ids, &[1, 0, 4]));
    assert_eq!(
        seek_ids_back(&*s, &sn, i, &point(&[1])),
        pick(&ids, &[4, 0, 1])
    );
    assert_eq!(seek_ids(&*s, &sn, i, &point(&[])), forward);
    assert!(seek_ids(&*s, &sn, i, &point(&[7])).is_empty());
    assert!(seek_ids(&*s, &sn, i, &point(&[1, 7])).is_empty());
    // Between: prefix bounds cover the whole prefix group.
    assert_eq!(
        seek_ids(
            &*s,
            &sn,
            i,
            &between(Bound::Included(&[1]), Bound::Excluded(&[3]))
        ),
        pick(&ids, &[1, 0, 4, 2])
    );
    assert_eq!(
        seek_ids(
            &*s,
            &sn,
            i,
            &between(Bound::Excluded(&[1]), Bound::Unbounded)
        ),
        pick(&ids, &[2, 5])
    );
    assert_eq!(
        seek_ids(
            &*s,
            &sn,
            i,
            &between(Bound::Unbounded, Bound::Included(&[1, 0]))
        ),
        pick(&ids, &[3, 1])
    );
    assert_eq!(
        seek_ids(
            &*s,
            &sn,
            i,
            &between(Bound::Included(&[1, 0]), Bound::Excluded(&[1, 5]))
        ),
        pick(&ids, &[1])
    );
    assert_eq!(
        seek_ids(
            &*s,
            &sn,
            i,
            &between(Bound::Excluded(&[1, 0]), Bound::Included(&[2]))
        ),
        pick(&ids, &[0, 4, 2])
    );
    assert_eq!(
        seek_ids(&*s, &sn, i, &between(Bound::Unbounded, Bound::Unbounded)),
        forward
    );
    assert_eq!(
        seek_ids(
            &*s,
            &sn,
            i,
            &between(Bound::Included(&[0, 0]), Bound::Included(&[9]))
        ),
        forward
    );
    assert_eq!(
        seek_ids_back(
            &*s,
            &sn,
            i,
            &between(Bound::Included(&[1]), Bound::Included(&[2]))
        ),
        pick(&ids, &[2, 4, 0, 1])
    );
    // Empty and inverted ranges yield nothing, never an error.
    assert!(
        seek_ids(
            &*s,
            &sn,
            i,
            &between(Bound::Included(&[2]), Bound::Included(&[1]))
        )
        .is_empty()
    );
    assert!(
        seek_ids(
            &*s,
            &sn,
            i,
            &between(Bound::Excluded(&[1]), Bound::Excluded(&[1]))
        )
        .is_empty()
    );
    // Visibility applies to seek: own uncommitted rows show, others' do not.
    let x = s.insert(TxnId(3), t, &row(&[1, 5])).expect("insert x");
    assert_eq!(
        seek_ids(&*s, &snap(3, &[]), i, &point(&[1, 5])),
        vec![ids[0], ids[4], x]
    );
    assert_eq!(
        seek_ids(&*s, &snap(4, &[3]), i, &point(&[1, 5])),
        pick(&ids, &[0, 4])
    );
}

/// See [`crate::testsuite`]: `index_nulls_first_ascending_last_descending`.
pub fn case_index_nulls_first_ascending_last_descending(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let asc = s
        .create_index(t, &index_shape(&[(0, false)], false))
        .expect("asc");
    let desc = s
        .create_index(t, &index_shape(&[(0, true)], false))
        .expect("desc");
    let ids = insert_all(
        &*s,
        1,
        t,
        &[
            row(&[2]),
            nullable_row(&[None]),
            row(&[1]),
            nullable_row(&[None]),
        ],
    );
    s.commit(TxnId(1)).expect("commit");
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
        seek_ids(&*s, &sn, asc, &KeyRange::Full),
        pick(&ids, &[1, 3, 2, 0])
    );
    assert_eq!(
        seek_ids_back(&*s, &sn, asc, &KeyRange::Full),
        pick(&ids, &[0, 2, 3, 1])
    );
    assert_eq!(
        seek_ids(&*s, &sn, desc, &KeyRange::Full),
        pick(&ids, &[0, 2, 1, 3])
    );
    assert_eq!(
        seek_ids_back(&*s, &sn, desc, &KeyRange::Full),
        pick(&ids, &[3, 1, 2, 0])
    );
    // NULL is a key like any other: a point on it, bounds around it, in both orders.
    assert_eq!(seek_ids(&*s, &sn, asc, &null), pick(&ids, &[1, 3]));
    assert_eq!(seek_ids(&*s, &sn, desc, &null), pick(&ids, &[1, 3]));
    assert_eq!(
        seek_ids(&*s, &sn, asc, &after_null_asc),
        pick(&ids, &[2, 0])
    );
    assert_eq!(
        seek_ids(&*s, &sn, desc, &before_null_desc),
        pick(&ids, &[0, 2])
    );
    assert_eq!(
        seek_ids(&*s, &sn, asc, &null_to_one_asc),
        pick(&ids, &[1, 3, 2])
    );
    // Descending bounds are given in index order: (2) comes before (1).
    assert_eq!(
        seek_ids(
            &*s,
            &sn,
            desc,
            &between(Bound::Included(&[2]), Bound::Included(&[1]))
        ),
        pick(&ids, &[0, 2])
    );
    assert!(
        seek_ids(
            &*s,
            &sn,
            desc,
            &between(Bound::Included(&[1]), Bound::Included(&[2]))
        )
        .is_empty()
    );
}

/// See [`crate::testsuite`]: `unique_index_violation_2601_mvcc_aware`.
pub fn case_unique_index_violation_2601_mvcc_aware(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, db, t) = one_table(make, 1);
    let u = s
        .create_index(t, &index_shape(&[(0, false)], true))
        .expect("unique index");
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    s.commit(TxnId(1)).expect("commit 1");
    // A committed live row blocks; nothing is inserted on failure.
    assert_duplicate_key(s.insert(TxnId(2), t, &row(&[1])), t, u);
    assert_eq!(scan_sorted(&*s, &snap(2, &[]), t), vec![(a, row(&[1]))]);
    // An uncommitted insert of another transaction blocks; its rollback frees the key.
    s.insert(TxnId(3), t, &row(&[2])).expect("insert 3");
    assert_duplicate_key(s.insert(TxnId(4), t, &row(&[2])), t, u);
    s.rollback(TxnId(3)).expect("rollback 3");
    let b = s
        .insert(TxnId(4), t, &row(&[2]))
        .expect("insert 4 after rollback");
    s.commit(TxnId(4)).expect("commit 4");
    // A committed delete frees the key.
    s.delete(TxnId(5), t, a).expect("delete a");
    s.commit(TxnId(5)).expect("commit 5");
    let c = s
        .insert(TxnId(6), t, &row(&[1]))
        .expect("insert 6 after delete");
    s.commit(TxnId(6)).expect("commit 6");
    // An uncommitted delete by another transaction does not free it.
    s.delete(TxnId(7), t, c).expect("delete 7");
    assert_duplicate_key(s.insert(TxnId(8), t, &row(&[1])), t, u);
    s.rollback(TxnId(7)).expect("rollback 7");
    // On update, the row is left unchanged.
    assert_duplicate_key(s.update(TxnId(8), t, b, &row(&[1])), t, u);
    assert_eq!(get(&*s, &snap(8, &[]), t, b), Some(row(&[2])));
    assert_eq!(latest(&*s, t, b), Some((TxnId(4), row(&[2]))));
    assert_eq!(seek_ids(&*s, &snap(8, &[]), u, &point(&[1])), vec![c]);
    // `create_index` refuses existing duplicates and creates nothing.
    let t2 = s.create_table(db, &int_table_shape(1)).expect("t2");
    insert_all(&*s, 9, t2, &[row(&[1]), row(&[1])]);
    s.commit(TxnId(9)).expect("commit 9");
    let err = s
        .create_index(t2, &index_shape(&[(0, false)], true))
        .expect_err("create_index on duplicates");
    assert_eq!(err.number, 2601, "unexpected error: {}", err.message);
    assert!(
        err.message.contains(&format!("'{t2}'")),
        "2601 must name table {t2}: {}",
        err.message
    );
    assert_eq!(s.indexes(t2).expect("indexes"), vec![]);
    // Without uniqueness the same definition passes.
    s.create_index(t2, &index_shape(&[(0, false)], false))
        .expect("non-unique index on duplicates");
}

/// See [`crate::testsuite`]: `unique_index_null_counts_as_value`.
pub fn case_unique_index_null_counts_as_value(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 2);
    let u = s
        .create_index(t, &index_shape(&[(0, false), (1, false)], true))
        .expect("unique index");
    s.insert(TxnId(1), t, &nullable_row(&[Some(1), None]))
        .expect("(1, NULL)");
    assert_duplicate_key(s.insert(TxnId(1), t, &nullable_row(&[Some(1), None])), t, u);
    s.insert(TxnId(1), t, &nullable_row(&[None, None]))
        .expect("(NULL, NULL)");
    assert_duplicate_key(s.insert(TxnId(1), t, &nullable_row(&[None, None])), t, u);
    let b = s
        .insert(TxnId(1), t, &nullable_row(&[Some(2), None]))
        .expect("(2, NULL)");
    assert_duplicate_key(
        s.update(TxnId(1), t, b, &nullable_row(&[Some(1), None])),
        t,
        u,
    );
    s.commit(TxnId(1)).expect("commit");
    assert_duplicate_key(s.insert(TxnId(2), t, &nullable_row(&[Some(1), None])), t, u);
    assert_eq!(scan_sorted(&*s, &snap(2, &[]), t).len(), 3);
}

/// See [`crate::testsuite`]: `unique_index_ignores_versions_superseded_by_writer`.
pub fn case_unique_index_ignores_versions_superseded_by_writer(
    make: &dyn Fn() -> Box<dyn Storage>,
) {
    let (s, _, t) = one_table(make, 2);
    let u = s
        .create_index(t, &index_shape(&[(0, false)], true))
        .expect("unique index");
    let a = s.insert(TxnId(1), t, &row(&[1, 0])).expect("insert a");
    s.commit(TxnId(1)).expect("commit 1");
    // Updates keeping the key pass, twice.
    s.update(TxnId(2), t, a, &row(&[1, 1])).expect("update 2a");
    s.update(TxnId(2), t, a, &row(&[1, 2])).expect("update 2b");
    assert_eq!(
        seek_rows(&*s, &snap(2, &[]), u, &point(&[1]), Direction::Forward),
        vec![(a, row(&[1, 2]))]
    );
    assert_eq!(
        seek_rows(&*s, &snap(3, &[2]), u, &point(&[1]), Direction::Forward),
        vec![(a, row(&[1, 0]))]
    );
    // Others are still blocked by the key.
    assert_duplicate_key(s.insert(TxnId(3), t, &row(&[1, 9])), t, u);
    s.commit(TxnId(2)).expect("commit 2");
    // Delete then insert of the same key in the same transaction.
    s.delete(TxnId(3), t, a).expect("delete 3");
    let b = s.insert(TxnId(3), t, &row(&[1, 9])).expect("insert 3");
    assert_eq!(
        seek_rows(&*s, &snap(3, &[]), u, &point(&[1]), Direction::Forward),
        vec![(b, row(&[1, 9]))]
    );
    assert_eq!(
        seek_rows(&*s, &snap(4, &[3]), u, &point(&[1]), Direction::Forward),
        vec![(a, row(&[1, 2]))]
    );
    assert_duplicate_key(s.insert(TxnId(4), t, &row(&[1, 4])), t, u);
    s.commit(TxnId(3)).expect("commit 3");
    assert_eq!(
        seek_rows(&*s, &snap(4, &[]), u, &point(&[1]), Direction::Forward),
        vec![(b, row(&[1, 9]))]
    );
    // Insert, delete, insert in one transaction.
    let c = s.insert(TxnId(5), t, &row(&[5, 0])).expect("insert c");
    s.delete(TxnId(5), t, c).expect("delete c");
    let d = s.insert(TxnId(5), t, &row(&[5, 1])).expect("insert d");
    assert_eq!(seek_ids(&*s, &snap(5, &[]), u, &point(&[5])), vec![d]);
}

/// See [`crate::testsuite`]: `index_maintained_on_update_rollback_vacuum`.
pub fn case_index_maintained_on_update_rollback_vacuum(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let i = s
        .create_index(t, &index_shape(&[(0, false)], false))
        .expect("index");
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    let b = s.insert(TxnId(1), t, &row(&[2])).expect("insert b");
    s.commit(TxnId(1)).expect("commit 1");
    // A pending update moves the row for its writer only.
    s.update(TxnId(2), t, a, &row(&[3])).expect("update 2");
    assert_eq!(seek_ids(&*s, &snap(2, &[]), i, &point(&[3])), vec![a]);
    assert!(seek_ids(&*s, &snap(2, &[]), i, &point(&[1])).is_empty());
    assert_eq!(seek_ids(&*s, &snap(3, &[2]), i, &point(&[1])), vec![a]);
    assert!(seek_ids(&*s, &snap(3, &[2]), i, &point(&[3])).is_empty());
    // Rollback undoes the move.
    s.rollback(TxnId(2)).expect("rollback 2");
    assert!(seek_ids(&*s, &snap(3, &[]), i, &point(&[3])).is_empty());
    assert_eq!(seek_ids(&*s, &snap(3, &[]), i, &point(&[1])), vec![a]);
    assert_eq!(seek_ids(&*s, &snap(3, &[]), i, &KeyRange::Full), vec![a, b]);
    // A committed update moves it for later snapshots.
    s.update(TxnId(3), t, a, &row(&[3])).expect("update 3");
    s.commit(TxnId(3)).expect("commit 3");
    assert!(seek_ids(&*s, &snap(4, &[]), i, &point(&[1])).is_empty());
    assert_eq!(seek_ids(&*s, &snap(4, &[]), i, &point(&[3])), vec![a]);
    assert_eq!(seek_ids(&*s, &snap(4, &[]), i, &KeyRange::Full), vec![b, a]);
    // rollback_to removes the entries of the later writes only.
    let c = s.insert(TxnId(4), t, &row(&[7])).expect("insert c");
    let sp = s.savepoint(TxnId(4)).expect("savepoint");
    s.insert(TxnId(4), t, &row(&[8])).expect("insert d");
    s.update(TxnId(4), t, c, &row(&[9])).expect("update c");
    s.rollback_to(TxnId(4), sp).expect("rollback_to");
    assert!(seek_ids(&*s, &snap(4, &[]), i, &point(&[8])).is_empty());
    assert!(seek_ids(&*s, &snap(4, &[]), i, &point(&[9])).is_empty());
    assert_eq!(seek_ids(&*s, &snap(4, &[]), i, &point(&[7])), vec![c]);
    s.commit(TxnId(4)).expect("commit 4");
    // A committed delete hides the entry; vacuum removes it for good.
    s.delete(TxnId(5), t, b).expect("delete b");
    s.commit(TxnId(5)).expect("commit 5");
    assert!(seek_ids(&*s, &snap(6, &[]), i, &point(&[2])).is_empty());
    s.vacuum(TxnId(6)).expect("vacuum");
    assert!(seek_ids(&*s, &snap(6, &[]), i, &point(&[2])).is_empty());
    assert_eq!(seek_ids(&*s, &snap(6, &[]), i, &KeyRange::Full), vec![a, c]);
    assert_eq!(
        seek_rows(&*s, &snap(6, &[]), i, &KeyRange::Full, Direction::Forward),
        vec![(a, row(&[3])), (c, row(&[7]))]
    );
    // An index created afterwards indexes the existing rows.
    let j = s
        .create_index(t, &index_shape(&[(0, true)], false))
        .expect("late index");
    assert_eq!(seek_ids(&*s, &snap(6, &[]), j, &KeyRange::Full), vec![c, a]);
    // A dropped index is unknown.
    s.drop_index(i).expect("drop_index");
    assert_bug(
        s.seek(&snap(6, &[]), i, &KeyRange::Full, Direction::Forward)
            .map(|_| ()),
    );
    assert_eq!(seek_ids(&*s, &snap(6, &[]), j, &KeyRange::Full), vec![c, a]);
}

/// See [`crate::testsuite`]: `clustered_key_orders_scan`.
pub fn case_clustered_key_orders_scan(make: &dyn Fn() -> Box<dyn Storage>) {
    let s = make();
    let db = s.create_database("db").expect("create_database");
    let t = s
        .create_table(db, &clustered_int_table_shape(2, &[(0, false), (1, true)]))
        .expect("clustered table");
    let ids = insert_all(
        &*s,
        1,
        t,
        &[
            row(&[2, 1]),
            nullable_row(&[Some(1), None]),
            row(&[1, 5]),
            nullable_row(&[None, Some(0)]),
            row(&[1, 5]),
            row(&[1, 9]),
        ],
    );
    s.commit(TxnId(1)).expect("commit 1");
    // Column 0 ascending (NULL first), then column 1 descending (NULL last), then RowId.
    let expected = pick(&ids, &[3, 5, 2, 4, 1, 0]);
    assert_eq!(row_ids(&scan_ordered(&*s, &snap(2, &[]), t)), expected);
    // An updated row moves to its new place and keeps its RowId; a snapshot that does not
    // settle the writer keeps the old order.
    s.update(TxnId(2), t, ids[3], &row(&[3, 0]))
        .expect("update");
    assert_eq!(
        row_ids(&scan_ordered(&*s, &snap(2, &[]), t)),
        pick(&ids, &[5, 2, 4, 1, 0, 3])
    );
    assert_eq!(get(&*s, &snap(2, &[]), t, ids[3]), Some(row(&[3, 0])));
    assert_eq!(row_ids(&scan_ordered(&*s, &snap(3, &[2]), t)), expected);
    s.commit(TxnId(2)).expect("commit 2");
    assert_eq!(
        row_ids(&scan_ordered(&*s, &snap(3, &[]), t)),
        pick(&ids, &[5, 2, 4, 1, 0, 3])
    );
    // A table without clustered key yields every visible row exactly once.
    let u = s
        .create_table(db, &int_table_shape(1))
        .expect("plain table");
    let mut plain_ids = insert_all(&*s, 3, u, &[row(&[3]), row(&[1]), row(&[2])]);
    plain_ids.sort();
    assert_eq!(row_ids(&scan_sorted(&*s, &snap(3, &[]), u)), plain_ids);
}

/// See [`crate::testsuite`]: `scan_and_seek_isolated_from_later_writes_of_other_txns`.
pub fn case_scan_and_seek_isolated_from_later_writes_of_other_txns(
    make: &dyn Fn() -> Box<dyn Storage>,
) {
    let (s, _, t) = one_table(make, 1);
    let i = s
        .create_index(t, &index_shape(&[(0, false)], false))
        .expect("index");
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    s.commit(TxnId(1)).expect("commit 1");
    // 2 creates its iterators, then 3 writes and commits, then 2 iterates.
    let reader = snap(2, &[]);
    let scan_iter = s.scan(&reader, t).expect("scan");
    let seek_iter = s
        .seek(&reader, i, &KeyRange::Full, Direction::Forward)
        .expect("seek");
    let b = s.insert(TxnId(3), t, &row(&[2])).expect("insert b");
    s.update(TxnId(3), t, a, &row(&[10])).expect("update a");
    s.commit(TxnId(3)).expect("commit 3");
    // A snapshot taken now sees the new state…
    assert_eq!(
        scan_sorted(&*s, &snap(4, &[]), t),
        vec![(a, row(&[10])), (b, row(&[2]))]
    );
    // …the iterators created before do not.
    assert_eq!(collect(scan_iter), vec![(a, row(&[1]))]);
    assert_eq!(collect(seek_iter), vec![(a, row(&[1]))]);
}

// -------------------------------------------------------------------- vacuum

/// See [`crate::testsuite`]: `vacuum_removes_dead_versions_keeps_visible_ones`.
pub fn case_vacuum_removes_dead_versions_keeps_visible_ones(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    let i = s
        .create_index(t, &index_shape(&[(0, false)], false))
        .expect("index");
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    let b = s.insert(TxnId(1), t, &row(&[2])).expect("insert b");
    let c = s.insert(TxnId(1), t, &row(&[3])).expect("insert c");
    s.commit(TxnId(1)).expect("commit 1");
    s.delete(TxnId(2), t, a).expect("delete a");
    s.commit(TxnId(2)).expect("commit 2");
    s.update(TxnId(3), t, b, &row(&[20])).expect("update b");
    // 5 takes its snapshot while 3 is in progress: the oldest usable snapshot has xmin 3.
    let held = snap(5, &[3]);
    assert_eq!(held.xmin, TxnId(3));
    s.commit(TxnId(3)).expect("commit 3");
    s.vacuum(TxnId(3)).expect("vacuum 3");
    // `a`, deleted by 2 < 3: gone for good.
    assert_eq!(latest(&*s, t, a), None);
    assert_eq!(get(&*s, &snap(6, &[]), t, a), None);
    assert!(seek_ids(&*s, &snap(6, &[]), i, &point(&[1])).is_empty());
    // `b`: its old version was replaced by 3, not below the horizon: `held` still sees it.
    assert_eq!(get(&*s, &held, t, b), Some(row(&[2])));
    assert_eq!(
        seek_rows(&*s, &held, i, &point(&[2]), Direction::Forward),
        vec![(b, row(&[2]))]
    );
    assert_eq!(get(&*s, &snap(6, &[]), t, b), Some(row(&[20])));
    assert_eq!(latest(&*s, t, b), Some((TxnId(3), row(&[20]))));
    // `c`, never touched: untouched even though its creator is below the horizon.
    assert_eq!(get(&*s, &held, t, c), Some(row(&[3])));
    assert_eq!(get(&*s, &snap(6, &[]), t, c), Some(row(&[3])));
    // `held` is released: horizon 4 lets `b`'s old version go, nothing else changes.
    drop(held);
    s.vacuum(TxnId(4)).expect("vacuum 4");
    assert_eq!(latest(&*s, t, b), Some((TxnId(3), row(&[20]))));
    assert_eq!(get(&*s, &snap(6, &[]), t, b), Some(row(&[20])));
    assert!(seek_ids(&*s, &snap(6, &[]), i, &point(&[2])).is_empty());
    assert_eq!(seek_ids(&*s, &snap(6, &[]), i, &point(&[20])), vec![b]);
    assert_eq!(
        scan_sorted(&*s, &snap(6, &[]), t),
        vec![(b, row(&[20])), (c, row(&[3]))]
    );
    // Twice with the same horizon is harmless.
    s.vacuum(TxnId(4)).expect("vacuum 4 again");
    assert_eq!(
        scan_sorted(&*s, &snap(6, &[]), t),
        vec![(b, row(&[20])), (c, row(&[3]))]
    );
    assert_eq!(seek_ids(&*s, &snap(6, &[]), i, &KeyRange::Full), vec![c, b]);
    // Versions of an aborted creator are gone; RowIds are never reused.
    let d = s.insert(TxnId(7), t, &row(&[7])).expect("insert d");
    s.rollback(TxnId(7)).expect("rollback 7");
    s.vacuum(TxnId(8)).expect("vacuum 8");
    assert_eq!(latest(&*s, t, d), None);
    let e = s.insert(TxnId(8), t, &row(&[8])).expect("insert e");
    assert!(e > d, "RowId reused after vacuum: {e:?}");
    assert_eq!(get(&*s, &snap(8, &[]), t, a), None);
    assert_eq!(
        scan_sorted(&*s, &snap(8, &[]), t),
        vec![(b, row(&[20])), (c, row(&[3])), (e, row(&[8]))]
    );
}

/// See [`crate::testsuite`]: `checkpoint_is_ok`.
pub fn case_checkpoint_is_ok(make: &dyn Fn() -> Box<dyn Storage>) {
    let s = make();
    s.checkpoint().expect("checkpoint on empty storage");
    let db = s.create_database("db").expect("create_database");
    let t = s
        .create_table(db, &int_table_shape(1))
        .expect("create_table");
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    s.commit(TxnId(1)).expect("commit 1");
    let b = s.insert(TxnId(2), t, &row(&[2])).expect("insert b");
    s.checkpoint().expect("checkpoint with a pending write");
    s.checkpoint().expect("checkpoint twice");
    // Nothing changed for anyone.
    assert_eq!(get(&*s, &snap(3, &[2]), t, a), Some(row(&[1])));
    assert_eq!(get(&*s, &snap(3, &[2]), t, b), None);
    assert_eq!(get(&*s, &snap(2, &[]), t, b), Some(row(&[2])));
    s.commit(TxnId(2)).expect("commit 2 after checkpoint");
    assert_eq!(
        scan_sorted(&*s, &snap(3, &[]), t),
        vec![(a, row(&[1])), (b, row(&[2]))]
    );
    s.checkpoint().expect("checkpoint after commit");
}

/// See [`crate::testsuite`]: `commit_of_read_only_txn_is_ok_and_twice_is_bug`.
pub fn case_commit_of_read_only_txn_is_ok_and_twice_is_bug(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, _, t) = one_table(make, 1);
    // A transaction that wrote nothing commits once.
    s.commit(TxnId(42)).expect("commit of a read-only txn");
    assert_bug(s.commit(TxnId(42)));
    assert_bug(s.rollback(TxnId(42)));
    assert_bug(s.insert(TxnId(42), t, &row(&[1])));
    assert_bug(s.savepoint(TxnId(42)));
    // Or rolls back once.
    s.rollback(TxnId(43)).expect("rollback of a read-only txn");
    assert_bug(s.rollback(TxnId(43)));
    assert_bug(s.commit(TxnId(43)));
    assert_bug(s.insert(TxnId(43), t, &row(&[1])));
    // Same for transactions that wrote: their writes are untouched by the refused calls.
    let a = s.insert(TxnId(1), t, &row(&[1])).expect("insert a");
    s.commit(TxnId(1)).expect("commit 1");
    assert_bug(s.commit(TxnId(1)));
    assert_bug(s.rollback(TxnId(1)));
    assert_eq!(get(&*s, &snap(2, &[]), t, a), Some(row(&[1])));
    let b = s.insert(TxnId(2), t, &row(&[2])).expect("insert b");
    s.rollback(TxnId(2)).expect("rollback 2");
    assert_bug(s.rollback(TxnId(2)));
    assert_bug(s.commit(TxnId(2)));
    assert_eq!(get(&*s, &snap(3, &[]), t, b), None);
    assert_eq!(scan_sorted(&*s, &snap(3, &[]), t), vec![(a, row(&[1]))]);
}

/// See [`crate::testsuite`]: `arity_and_unknown_id_preconditions_are_bug_errors`.
pub fn case_arity_and_unknown_id_preconditions_are_bug_errors(make: &dyn Fn() -> Box<dyn Storage>) {
    let (s, db, t) = one_table(make, 2);
    let i = s
        .create_index(t, &index_shape(&[(0, false)], false))
        .expect("index");
    let no_db = DbId(u32::MAX);
    let no_table = TableId(u32::MAX);
    let no_index = IndexId(u32::MAX);
    let no_row = RowId(u64::MAX);
    let sn = snap(1, &[]);
    let two = row(&[1, 2]);
    // Arity: nothing is written by a refused call.
    assert_bug(s.insert(TxnId(1), t, &row(&[])));
    assert_bug(s.insert(TxnId(1), t, &row(&[1])));
    assert_bug(s.insert(TxnId(1), t, &row(&[1, 2, 3])));
    assert!(scan_sorted(&*s, &sn, t).is_empty());
    let id = s.insert(TxnId(1), t, &two).expect("insert");
    assert_bug(s.update(TxnId(1), t, id, &row(&[1])));
    assert_bug(s.update(TxnId(1), t, id, &row(&[])));
    assert_bug(s.update(TxnId(1), t, id, &row(&[1, 2, 3])));
    assert_eq!(get(&*s, &sn, t, id), Some(two.clone()));
    // Unknown database.
    assert_bug(s.drop_database(no_db));
    assert_bug(s.tables(no_db));
    assert_bug(s.create_table(no_db, &int_table_shape(1)));
    // Unknown table.
    assert_bug(s.drop_table(no_table));
    assert_bug(s.indexes(no_table));
    assert_bug(s.create_index(no_table, &index_shape(&[(0, false)], false)));
    assert_bug(s.insert(TxnId(1), no_table, &two));
    assert_bug(s.update(TxnId(1), no_table, id, &two));
    assert_bug(s.delete(TxnId(1), no_table, id));
    assert_bug(s.get(&sn, no_table, id));
    assert_bug(s.scan(&sn, no_table).map(|_| ()));
    assert_bug(s.latest_version(no_table, id));
    // Unknown row (for writes; reads answer `None`).
    assert_bug(s.update(TxnId(1), t, no_row, &two));
    assert_bug(s.delete(TxnId(1), t, no_row));
    assert_eq!(get(&*s, &sn, t, no_row), None);
    assert_eq!(latest(&*s, t, no_row), None);
    // Unknown index, prefix longer than the key.
    assert_bug(s.drop_index(no_index));
    assert_bug(
        s.seek(&sn, no_index, &KeyRange::Full, Direction::Forward)
            .map(|_| ()),
    );
    assert_bug(
        s.seek(&sn, i, &point(&[1, 2]), Direction::Forward)
            .map(|_| ()),
    );
    assert_bug(
        s.seek(
            &sn,
            i,
            &between(Bound::Included(&[1, 2]), Bound::Unbounded),
            Direction::Backward,
        )
        .map(|_| ()),
    );
    // Invalid shapes.
    assert_bug(s.create_table(
        db,
        &TableShape {
            columns: vec![],
            clustered_key: None,
        },
    ));
    assert_bug(s.create_table(
        db,
        &TableShape {
            columns: int_table_shape(1).columns,
            clustered_key: Some(vec![]),
        },
    ));
    assert_bug(s.create_table(db, &clustered_int_table_shape(1, &[(1, false)])));
    assert_bug(s.create_index(t, &index_shape(&[], false)));
    assert_bug(s.create_index(t, &index_shape(&[(2, false)], false)));
    assert_bug(s.create_index(
        t,
        &IndexShape {
            columns: key_columns(&[(0, false)]),
            unique: false,
            included: vec![2],
        },
    ));
    // Unknown savepoint.
    assert_bug(s.rollback_to(TxnId(1), SavepointId(u64::MAX)));
    // Nothing leaked: catalogue and row are as before.
    assert_eq!(ids_of_tables(&*s, db), vec![t]);
    assert_eq!(s.indexes(t).expect("indexes").len(), 1);
    assert_eq!(scan_sorted(&*s, &sn, t), vec![(id, two.clone())]);
    s.commit(TxnId(1)).expect("commit 1");
    assert_eq!(scan_sorted(&*s, &snap(2, &[]), t), vec![(id, two)]);
}

// --------------------------------------------------------------- concurrency

/// See [`crate::testsuite`]: `concurrent_writers_on_distinct_tables`.
pub fn case_concurrent_writers_on_distinct_tables(make: &dyn Fn() -> Box<dyn Storage>) {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send_sync::<dyn Storage>();
    assert_send_sync::<Box<dyn Storage>>();

    const THREADS: u64 = 8;
    const ROWS: u64 = 100;
    let s = make();
    let db = s.create_database("db").expect("create_database");
    let tables: Vec<TableId> = (0..THREADS)
        .map(|_| {
            s.create_table(db, &int_table_shape(1))
                .expect("create_table")
        })
        .collect();
    let storage: &dyn Storage = &*s;
    std::thread::scope(|scope| {
        for (i, &t) in (0..THREADS).zip(&tables) {
            scope.spawn(move || {
                for k in 0..ROWS {
                    let txn = TxnId(1000 * (i + 1) + k);
                    let value = i32::try_from(k).expect("row value fits in i32");
                    storage
                        .insert(txn, t, &row(&[value]))
                        .expect("concurrent insert");
                    storage.commit(txn).expect("concurrent commit");
                }
            });
        }
    });
    // Everything committed: a snapshot that settles every transaction sees 100 rows per
    // table, with the expected values.
    let all = Snapshot {
        xmin: TxnId(u64::MAX),
        xmax: TxnId(u64::MAX),
        active: vec![],
        own: TxnId(u64::MAX),
    };
    let expected: Vec<Row> = (0..ROWS)
        .map(|k| row(&[i32::try_from(k).expect("fits")]))
        .collect();
    for &t in &tables {
        let rows = scan_sorted(storage, &all, t);
        assert_eq!(
            rows.len(),
            usize::try_from(ROWS).expect("fits"),
            "table {t}"
        );
        let mut values: Vec<Row> = rows.into_iter().map(|(_, r)| r).collect();
        values.sort_by_key(|r| match r.0[0] {
            vauban_types::Value::I32(v) => v,
            ref other => panic!("unexpected value {other:?}"),
        });
        assert_eq!(values, expected, "table {t}");
    }
}

/// See [`crate::testsuite`]:
/// `vacuum_during_pending_update_then_rollback_restores_committed_version`.
pub fn case_vacuum_during_pending_update_then_rollback_restores_committed_version(
    make: &dyn Fn() -> Box<dyn Storage>,
) {
    let (s, _, t) = one_table(make, 1);
    let i = s
        .create_index(t, &index_shape(&[(0, false)], false))
        .expect("index");
    let id = s.insert(TxnId(1), t, &row(&[1])).expect("insert");
    s.commit(TxnId(1)).expect("commit 1");
    s.update(TxnId(2), t, id, &row(&[2])).expect("update 2");
    s.commit(TxnId(2)).expect("commit 2");
    // 3 is a reader whose snapshot is the oldest still usable; 4 updates without committing.
    let held = snap(3, &[]);
    s.update(TxnId(4), t, id, &row(&[4])).expect("update 4");
    s.vacuum(TxnId(3)).expect("vacuum 3");
    // 1's version is gone, 2's stays (replaced by an in-progress transaction), 4's stays.
    assert!(seek_ids(&*s, &held, i, &point(&[1])).is_empty());
    assert_eq!(get(&*s, &held, t, id), Some(row(&[2])));
    assert_eq!(get(&*s, &snap(5, &[4]), t, id), Some(row(&[2])));
    assert_eq!(seek_ids(&*s, &snap(5, &[4]), i, &point(&[2])), vec![id]);
    assert_eq!(get(&*s, &snap(4, &[]), t, id), Some(row(&[4])));
    assert_eq!(seek_ids(&*s, &snap(4, &[]), i, &point(&[4])), vec![id]);
    assert_eq!(latest(&*s, t, id), Some((TxnId(4), row(&[4]))));
    // 4 rolls back: 2's version is current again, for everyone and in the index.
    s.rollback(TxnId(4)).expect("rollback 4");
    assert_eq!(get(&*s, &held, t, id), Some(row(&[2])));
    assert_eq!(get(&*s, &snap(5, &[]), t, id), Some(row(&[2])));
    assert_eq!(scan_sorted(&*s, &snap(5, &[]), t), vec![(id, row(&[2]))]);
    assert_eq!(latest(&*s, t, id), Some((TxnId(2), row(&[2]))));
    assert_eq!(seek_ids(&*s, &snap(5, &[]), i, &point(&[2])), vec![id]);
    assert!(seek_ids(&*s, &snap(5, &[]), i, &point(&[4])).is_empty());
    assert_eq!(seek_ids(&*s, &snap(5, &[]), i, &KeyRange::Full), vec![id]);
    // The row is writable again.
    s.update(TxnId(5), t, id, &row(&[5])).expect("update 5");
    assert_eq!(get(&*s, &snap(5, &[]), t, id), Some(row(&[5])));
    s.commit(TxnId(5)).expect("commit 5");
    assert_eq!(get(&*s, &snap(6, &[]), t, id), Some(row(&[5])));
    assert_eq!(seek_ids(&*s, &snap(6, &[]), i, &point(&[5])), vec![id]);
    s.delete(TxnId(6), t, id).expect("delete 6");
    assert_eq!(get(&*s, &snap(6, &[]), t, id), None);
    s.rollback(TxnId(6)).expect("rollback 6");
    assert_eq!(get(&*s, &snap(7, &[]), t, id), Some(row(&[5])));
}
