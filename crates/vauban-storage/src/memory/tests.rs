//! Tests of `MemoryStorage`: databases, tables, versioned insert/get/scan, commit/rollback
//! and the MVCC visibility they produce; update/delete, `latest_version`, savepoints,
//! vacuum; indexes, seek, uniqueness under MVCC and the clustered-key order of scan.
//!
//! # Snapshot convention
//!
//! Snapshots are built by hand, without the `txn` module. `TxnId`s are 1, 2,
//! 3… in order of start. The snapshot "of transaction `t`, each earlier one finished" is
//! `Snapshot { xmin: t, xmax: t + 1, active: [], own: t }`; the snapshot "`t` reads while
//! `u < t` is still in progress" is `Snapshot { xmin: u, xmax: t + 1, active: [u], own: t }`.
//! [`snap`] builds both: `xmin` is the smallest of `own` and `active`, `xmax` is `own + 1`.

use std::fmt::Debug;
use std::ops::Bound;

use vauban_errors::SqlResult;
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use super::MemoryStorage;
use crate::{
    DbId, Direction, IndexId, IndexShape, KeyColumn, KeyRange, Row, RowId, SavepointId, Snapshot,
    Storage, TableId, TableShape, TxnId, TxnStatus,
};

/// See the module documentation ("Snapshot convention").
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

/// A table of `n` nullable `int` columns, no clustered key.
fn shape(n: usize) -> TableShape {
    TableShape {
        columns: (0..n).map(|_| TypeInfo::new(SqlType::Int, true)).collect(),
        clustered_key: None,
    }
}

fn row(v: i32) -> Row {
    Row(vec![Value::I32(v)])
}

/// A storage with one database and one single-column table.
fn one_table() -> (MemoryStorage, DbId, TableId) {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let t = s.create_table(db, &shape(1)).unwrap();
    (s, db, t)
}

/// Asserts that `r` is the generic error produced by `InternalError::Bug`.
fn assert_bug<T: Debug>(r: SqlResult<T>) {
    let err = r.expect_err("expected an InternalError::Bug");
    assert_eq!(err.number, 50000);
    assert!(
        err.message.starts_with("Internal error: internal bug: "),
        "unexpected message: {}",
        err.message
    );
}

fn collect(s: &MemoryStorage, snap: &Snapshot, t: TableId) -> Vec<(RowId, Row)> {
    s.scan(snap, t).unwrap().map(|item| item.unwrap()).collect()
}

/// The `(xmin, xmax)` of every version of `id`, oldest first, read straight from the table
/// without any MVCC filtering. `None` if the logical row is gone.
fn chain(s: &MemoryStorage, t: TableId, id: RowId) -> Option<Vec<(u64, Option<u64>)>> {
    let inner = s.inner.read().unwrap();
    inner
        .tables
        .get(&t)
        .and_then(|table| table.rows.get(&id))
        .map(|versions| {
            versions
                .iter()
                .map(|v| (v.xmin.0, v.xmax.map(|x| x.0)))
                .collect()
        })
}

/// The transactions the registry still knows, sorted.
fn known_txns(s: &MemoryStorage) -> Vec<u64> {
    let mut ids: Vec<u64> = s.inner.read().unwrap().txns.keys().map(|t| t.0).collect();
    ids.sort_unstable();
    ids
}

// ---------------------------------------------------------------- Visibility

#[test]
fn insert_is_visible_to_own_txn_before_commit() {
    let (s, _, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(7)).unwrap();
    assert_eq!(s.get(&snap(1, &[]), t, id).unwrap(), Some(row(7)));
    assert_eq!(collect(&s, &snap(1, &[]), t), vec![(id, row(7))]);
}

#[test]
fn insert_is_invisible_to_concurrent_snapshot_until_commit() {
    let (s, _, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(7)).unwrap();
    // 2 reads while 1 is still in progress: nothing.
    let concurrent = snap(2, &[1]);
    assert_eq!(s.get(&concurrent, t, id).unwrap(), None);
    assert!(collect(&s, &concurrent, t).is_empty());
    // 1 commits; a snapshot taken afterwards by 2 sees the row.
    s.commit(TxnId(1)).unwrap();
    let after = snap(2, &[]);
    assert_eq!(s.get(&after, t, id).unwrap(), Some(row(7)));
    assert_eq!(collect(&s, &after, t), vec![(id, row(7))]);
}

#[test]
fn committed_insert_is_visible_to_later_snapshot() {
    let (s, _, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    assert_eq!(s.get(&snap(2, &[]), t, id).unwrap(), Some(row(1)));
    assert_eq!(s.get(&snap(9, &[]), t, id).unwrap(), Some(row(1)));
}

#[test]
fn snapshot_taken_before_commit_never_sees_it() {
    let (s, _, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    let frozen = snap(2, &[1]);
    s.commit(TxnId(1)).unwrap();
    // 1 is listed in `active`: still invisible, even though it has committed since.
    assert_eq!(s.get(&frozen, t, id).unwrap(), None);
    assert!(collect(&s, &frozen, t).is_empty());
    // A transaction started after 1 finished, but whose snapshot has xmax <= 1, is the
    // same story: `xmin >= xmax` is never settled.
    let too_old = Snapshot {
        xmin: TxnId(1),
        xmax: TxnId(1),
        active: vec![],
        own: TxnId(3),
    };
    assert_eq!(s.get(&too_old, t, id).unwrap(), None);
}

#[test]
fn rollback_removes_inserted_rows_for_everyone() {
    let (s, _, t) = one_table();
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    let b = s.insert(TxnId(1), t, &row(2)).unwrap();
    s.rollback(TxnId(1)).unwrap();
    for sn in [snap(1, &[]), snap(2, &[]), snap(2, &[1])] {
        assert_eq!(s.get(&sn, t, a).unwrap(), None);
        assert_eq!(s.get(&sn, t, b).unwrap(), None);
        assert!(collect(&s, &sn, t).is_empty());
    }
}

#[test]
fn aborted_creator_is_invisible_even_without_physical_removal() {
    // Same as above but checks the status path: a version whose creator rolled back would be
    // invisible through `is_visible` alone. Here we insert with 1, roll back, and insert
    // with 2: only 2's row shows to a snapshot that settles both.
    let (s, _, t) = one_table();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.rollback(TxnId(1)).unwrap();
    let b = s.insert(TxnId(2), t, &row(2)).unwrap();
    s.commit(TxnId(2)).unwrap();
    assert_eq!(collect(&s, &snap(3, &[]), t), vec![(b, row(2))]);
}

#[test]
fn two_writers_see_only_their_own_uncommitted_rows() {
    let (s, _, t) = one_table();
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    let b = s.insert(TxnId(2), t, &row(2)).unwrap();
    assert_eq!(collect(&s, &snap(1, &[2]), t), vec![(a, row(1))]);
    assert_eq!(collect(&s, &snap(2, &[1]), t), vec![(b, row(2))]);
    assert!(collect(&s, &snap(3, &[1, 2]), t).is_empty());
}

// --------------------------------------------------------- Transaction life cycle

#[test]
fn commit_of_unknown_txn_is_ok() {
    let s = MemoryStorage::new();
    s.commit(TxnId(42)).unwrap();
}

#[test]
fn rollback_of_unknown_txn_is_ok() {
    let s = MemoryStorage::new();
    s.rollback(TxnId(42)).unwrap();
}

#[test]
fn commit_twice_is_bug_error() {
    let (s, _, t) = one_table();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    assert_bug(s.commit(TxnId(1)));
    assert_bug(s.rollback(TxnId(1)));
    // A read-only transaction committed once is finished too.
    s.commit(TxnId(2)).unwrap();
    assert_bug(s.commit(TxnId(2)));
}

#[test]
fn rollback_twice_is_bug_error() {
    let (s, _, t) = one_table();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.rollback(TxnId(1)).unwrap();
    assert_bug(s.rollback(TxnId(1)));
    assert_bug(s.commit(TxnId(1)));
}

#[test]
fn insert_by_finished_txn_is_bug_error() {
    let (s, _, t) = one_table();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    assert_bug(s.insert(TxnId(1), t, &row(2)));
    s.insert(TxnId(2), t, &row(1)).unwrap();
    s.rollback(TxnId(2)).unwrap();
    assert_bug(s.insert(TxnId(2), t, &row(2)));
    // Nothing leaked from the refused inserts.
    assert!(collect(&s, &snap(3, &[]), t).len() == 1);
}

#[test]
fn checkpoint_is_ok_and_changes_nothing() {
    let (s, _, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.checkpoint().unwrap();
    assert_eq!(s.get(&snap(1, &[]), t, id).unwrap(), Some(row(1)));
    assert_eq!(s.get(&snap(2, &[1]), t, id).unwrap(), None);
}

// ------------------------------------------------------------------------ Scan

#[test]
fn scan_is_isolated_from_later_inserts_of_other_txns() {
    let (s, _, t) = one_table();
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    // `t` = 2 creates its iterator, then `u` = 3 inserts and commits, then 2 iterates.
    let iter = s.scan(&snap(2, &[]), t).unwrap();
    s.insert(TxnId(3), t, &row(3)).unwrap();
    s.commit(TxnId(3)).unwrap();
    let seen: Vec<(RowId, Row)> = iter.map(|item| item.unwrap()).collect();
    assert_eq!(seen, vec![(a, row(1))]);
}

#[test]
fn scan_yields_each_row_once_in_row_id_order() {
    let (s, _, t) = one_table();
    let mut ids = Vec::new();
    for v in 0..10 {
        ids.push(s.insert(TxnId(1), t, &row(v)).unwrap());
    }
    s.commit(TxnId(1)).unwrap();
    let seen = collect(&s, &snap(2, &[]), t);
    assert_eq!(seen.len(), 10);
    let seen_ids: Vec<RowId> = seen.iter().map(|(id, _)| *id).collect();
    assert_eq!(seen_ids, ids);
    assert!(seen_ids.windows(2).all(|w| w[0] < w[1]));
    for (i, (_, r)) in seen.iter().enumerate() {
        assert_eq!(*r, row(i as i32));
    }
}

#[test]
fn scan_of_empty_table_is_empty() {
    let (s, _, t) = one_table();
    assert!(collect(&s, &snap(1, &[]), t).is_empty());
}

#[test]
fn get_of_unknown_row_is_none_not_error() {
    let (s, _, t) = one_table();
    assert_eq!(s.get(&snap(1, &[]), t, RowId(12345)).unwrap(), None);
}

// ------------------------------------------------------------ Databases and tables

#[test]
fn databases_are_listed_in_db_id_order_with_verbatim_names() {
    let s = MemoryStorage::new();
    assert!(s.databases().unwrap().is_empty());
    let b = s.create_database("  B ").unwrap();
    let a = s.create_database("a").unwrap();
    assert!(b < a);
    assert_eq!(
        s.databases().unwrap(),
        vec![(b, "  B ".to_owned()), (a, "a".to_owned())]
    );
    s.drop_database(b).unwrap();
    assert_eq!(s.databases().unwrap(), vec![(a, "a".to_owned())]);
    assert_bug(s.drop_database(b));
    assert_bug(s.drop_database(DbId(999)));
}

#[test]
fn tables_lists_shapes_in_table_id_order_and_forgets_dropped() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let other = s.create_database("other").unwrap();
    let shape1 = shape(1);
    let shape2 = TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, false),
            TypeInfo::new(SqlType::Int, true),
        ],
        clustered_key: Some(vec![KeyColumn {
            column: 1,
            descending: true,
        }]),
    };
    let t1 = s.create_table(db, &shape1).unwrap();
    let t2 = s.create_table(db, &shape2).unwrap();
    let _elsewhere = s.create_table(other, &shape1).unwrap();
    assert!(t1 < t2);
    assert_eq!(
        s.tables(db).unwrap(),
        vec![(t1, shape1.clone()), (t2, shape2.clone())]
    );
    s.drop_table(t1).unwrap();
    assert_eq!(s.tables(db).unwrap(), vec![(t2, shape2)]);
    assert_bug(s.drop_table(t1));
    assert_bug(s.tables(DbId(999)));
}

#[test]
fn create_table_precondition_violations_are_bug_errors() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    assert_bug(s.create_table(DbId(999), &shape(1)));
    assert_bug(s.create_table(db, &shape(0)));
    assert_bug(s.create_table(
        db,
        &TableShape {
            columns: shape(1).columns,
            clustered_key: Some(vec![]),
        },
    ));
    assert_bug(s.create_table(
        db,
        &TableShape {
            columns: shape(2).columns,
            clustered_key: Some(vec![KeyColumn {
                column: 2,
                descending: false,
            }]),
        },
    ));
    assert!(s.tables(db).unwrap().is_empty());
}

#[test]
fn rollback_ignores_writes_on_dropped_table() {
    let (s, db, t) = one_table();
    let keep = s.create_table(db, &shape(1)).unwrap();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    let kept = s.insert(TxnId(1), keep, &row(2)).unwrap();
    s.drop_table(t).unwrap();
    s.rollback(TxnId(1)).unwrap();
    // The surviving table was rolled back normally.
    assert_eq!(s.get(&snap(1, &[]), keep, kept).unwrap(), None);
}

#[test]
fn commit_ignores_writes_on_dropped_table() {
    let (s, _, t) = one_table();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.drop_table(t).unwrap();
    s.commit(TxnId(1)).unwrap();
}

#[test]
fn drop_database_removes_its_tables() {
    let (s, db, t) = one_table();
    let t2 = s.create_table(db, &shape(1)).unwrap();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.drop_database(db).unwrap();
    assert_bug(s.insert(TxnId(1), t, &row(2)));
    assert_bug(s.insert(TxnId(1), t2, &row(2)));
    assert_bug(s.get(&snap(1, &[]), t, RowId(1)));
    assert_bug(s.scan(&snap(1, &[]), t).map(|_| ()));
    assert_bug(s.drop_table(t));
    assert_bug(s.tables(db));
    // The in-flight write on the dropped table is ignored by commit.
    s.commit(TxnId(1)).unwrap();
}

#[test]
fn row_arity_mismatch_is_bug_error() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let t = s.create_table(db, &shape(2)).unwrap();
    assert_bug(s.insert(TxnId(1), t, &Row(vec![])));
    assert_bug(s.insert(TxnId(1), t, &row(1)));
    assert_bug(s.insert(
        TxnId(1),
        t,
        &Row(vec![Value::I32(1), Value::Null, Value::I32(3)]),
    ));
    s.insert(TxnId(1), t, &Row(vec![Value::I32(1), Value::Null]))
        .unwrap();
    assert_eq!(collect(&s, &snap(1, &[]), t).len(), 1);
}

#[test]
fn unknown_table_id_is_bug_error() {
    let s = MemoryStorage::new();
    let t = TableId(7);
    assert_bug(s.insert(TxnId(1), t, &row(1)));
    assert_bug(s.get(&snap(1, &[]), t, RowId(1)));
    assert_bug(s.scan(&snap(1, &[]), t).map(|_| ()));
    assert_bug(s.drop_table(t));
}

#[test]
fn ids_are_never_reused_after_drop() {
    let s = MemoryStorage::new();
    let db1 = s.create_database("one").unwrap();
    let t1 = s.create_table(db1, &shape(1)).unwrap();
    let r1 = s.insert(TxnId(1), t1, &row(1)).unwrap();
    s.rollback(TxnId(1)).unwrap();
    let r2 = s.insert(TxnId(2), t1, &row(2)).unwrap();
    assert!(r2 > r1, "RowId reused after rollback");
    s.drop_table(t1).unwrap();
    let t2 = s.create_table(db1, &shape(1)).unwrap();
    assert!(t2 > t1, "TableId reused after drop_table");
    s.drop_database(db1).unwrap();
    let db2 = s.create_database("two").unwrap();
    assert!(db2 > db1, "DbId reused after drop_database");
    let t3 = s.create_table(db2, &shape(1)).unwrap();
    assert!(t3 > t2, "TableId reused after drop_database");
}

#[test]
fn default_is_an_empty_storage() {
    let s = MemoryStorage::default();
    assert!(s.databases().unwrap().is_empty());
}

// --------------------------------------------------------------------------- Misc

#[test]
fn memory_storage_is_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<MemoryStorage>();
    fn g<T: Storage + 'static>() {}
    g::<MemoryStorage>();
}

#[test]
fn poisoned_lock_is_reported_as_corruption() {
    let s = MemoryStorage::new();
    // Panic while holding the write lock, on another thread, to poison it.
    let outcome = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _guard = s.inner.write().unwrap();
                panic!("poisoning the storage lock on purpose");
            })
            .join()
    });
    assert!(outcome.is_err());
    let err = s.databases().unwrap_err();
    assert_eq!(err.number, 50000);
    assert_eq!(
        err.message,
        "Internal error: data corruption: storage lock poisoned"
    );
    assert!(s.create_database("x").is_err());
}

// ------------------------------------------------------------ Update and delete

/// A committed row `(id, row(1))` written by transaction 1, ready for later transactions.
fn one_committed_row() -> (MemoryStorage, DbId, TableId, RowId) {
    let (s, db, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    (s, db, t, id)
}

#[test]
fn update_creates_version_visible_only_to_writer_until_commit() {
    let (s, _, t, id) = one_committed_row();
    s.update(TxnId(2), t, id, &row(2)).unwrap();
    // The writer sees the new content, exactly once.
    assert_eq!(s.get(&snap(2, &[]), t, id).unwrap(), Some(row(2)));
    assert_eq!(collect(&s, &snap(2, &[]), t), vec![(id, row(2))]);
    // A concurrent reader still sees the old content, exactly once.
    assert_eq!(s.get(&snap(3, &[2]), t, id).unwrap(), Some(row(1)));
    assert_eq!(collect(&s, &snap(3, &[2]), t), vec![(id, row(1))]);
    // Two versions chained on the same logical row.
    assert_eq!(chain(&s, t, id), Some(vec![(1, Some(2)), (2, None)]));
    s.commit(TxnId(2)).unwrap();
    assert_eq!(s.get(&snap(3, &[]), t, id).unwrap(), Some(row(2)));
}

#[test]
fn reader_snapshot_keeps_old_version_during_concurrent_update() {
    let (s, _, t, id) = one_committed_row();
    s.update(TxnId(2), t, id, &row(2)).unwrap();
    // 3 takes its snapshot while 2 is in progress, then 2 commits.
    let before = snap(3, &[2]);
    s.commit(TxnId(2)).unwrap();
    let after = snap(3, &[]);
    assert_eq!(s.get(&before, t, id).unwrap(), Some(row(1)));
    assert_eq!(s.get(&after, t, id).unwrap(), Some(row(2)));
    assert_eq!(collect(&s, &before, t), vec![(id, row(1))]);
    assert_eq!(collect(&s, &after, t), vec![(id, row(2))]);
}

#[test]
fn update_then_commit_replaces_row_for_later_snapshots() {
    let (s, _, t, id) = one_committed_row();
    s.update(TxnId(2), t, id, &row(2)).unwrap();
    s.commit(TxnId(2)).unwrap();
    for own in [3, 4, 100] {
        // One row only, the new content, under the unchanged RowId.
        assert_eq!(collect(&s, &snap(own, &[]), t), vec![(id, row(2))]);
        assert_eq!(s.get(&snap(own, &[]), t, id).unwrap(), Some(row(2)));
    }
}

#[test]
fn two_updates_by_same_txn_are_allowed_and_keep_the_row_id() {
    let (s, _, t, id) = one_committed_row();
    s.update(TxnId(2), t, id, &row(2)).unwrap();
    s.update(TxnId(2), t, id, &row(3)).unwrap();
    assert_eq!(collect(&s, &snap(2, &[]), t), vec![(id, row(3))]);
    assert_eq!(collect(&s, &snap(3, &[2]), t), vec![(id, row(1))]);
    assert_eq!(
        chain(&s, t, id),
        Some(vec![(1, Some(2)), (2, Some(2)), (2, None)])
    );
    s.commit(TxnId(2)).unwrap();
    assert_eq!(collect(&s, &snap(3, &[]), t), vec![(id, row(3))]);
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(2), row(3))));
}

#[test]
fn delete_hides_row_from_others_only_after_commit() {
    let (s, _, t, id) = one_committed_row();
    s.delete(TxnId(2), t, id).unwrap();
    // Gone at once for the deleter.
    assert_eq!(s.get(&snap(2, &[]), t, id).unwrap(), None);
    assert!(collect(&s, &snap(2, &[]), t).is_empty());
    // Still there for a concurrent reader.
    let concurrent = snap(3, &[2]);
    assert_eq!(s.get(&concurrent, t, id).unwrap(), Some(row(1)));
    assert_eq!(collect(&s, &concurrent, t), vec![(id, row(1))]);
    s.commit(TxnId(2)).unwrap();
    // The frozen snapshot keeps seeing it; a new one does not.
    assert_eq!(s.get(&concurrent, t, id).unwrap(), Some(row(1)));
    assert_eq!(s.get(&snap(3, &[]), t, id).unwrap(), None);
    assert!(collect(&s, &snap(3, &[]), t).is_empty());
    // Nothing was removed physically.
    assert_eq!(chain(&s, t, id), Some(vec![(1, Some(2))]));
}

#[test]
fn insert_then_delete_in_same_txn_is_invisible_to_itself() {
    let (s, _, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.delete(TxnId(1), t, id).unwrap();
    assert_eq!(s.get(&snap(1, &[]), t, id).unwrap(), None);
    assert!(collect(&s, &snap(1, &[]), t).is_empty());
    s.commit(TxnId(1)).unwrap();
    assert_eq!(s.get(&snap(2, &[]), t, id).unwrap(), None);
    assert_eq!(chain(&s, t, id), Some(vec![(1, Some(1))]));
}

#[test]
fn update_on_superseded_version_is_bug_error() {
    let (s, _, t, id) = one_committed_row();
    s.update(TxnId(2), t, id, &row(2)).unwrap();
    // 3 tries to write the same row while 2's write is pending: refused, nothing changes.
    assert_bug(s.update(TxnId(3), t, id, &row(3)));
    assert_bug(s.delete(TxnId(3), t, id));
    assert_eq!(chain(&s, t, id), Some(vec![(1, Some(2)), (2, None)]));
    assert_eq!(s.get(&snap(2, &[]), t, id).unwrap(), Some(row(2)));
    assert_eq!(s.get(&snap(3, &[2]), t, id).unwrap(), Some(row(1)));
    // The refused writes did not register 3.
    assert_eq!(known_txns(&s), vec![1, 2]);
    // Once 2 has committed, the latest version has no `xmax` again: the storage accepts the
    // write (deciding whether it is a conflict is the job of `txn`).
    s.commit(TxnId(2)).unwrap();
    s.update(TxnId(3), t, id, &row(3)).unwrap();
    assert_eq!(s.get(&snap(3, &[]), t, id).unwrap(), Some(row(3)));
}

#[test]
fn write_after_uncommitted_delete_by_other_txn_is_bug_error() {
    let (s, _, t, id) = one_committed_row();
    s.delete(TxnId(2), t, id).unwrap();
    assert_bug(s.update(TxnId(3), t, id, &row(3)));
    assert_bug(s.delete(TxnId(3), t, id));
    assert_eq!(chain(&s, t, id), Some(vec![(1, Some(2))]));
    assert_eq!(known_txns(&s), vec![1, 2]);
    // Once 2 rolls back, the row is writable again.
    s.rollback(TxnId(2)).unwrap();
    s.delete(TxnId(3), t, id).unwrap();
    assert_eq!(chain(&s, t, id), Some(vec![(1, Some(3))]));
}

#[test]
fn update_after_delete_by_same_txn_is_bug_error() {
    let (s, _, t, id) = one_committed_row();
    s.delete(TxnId(2), t, id).unwrap();
    assert_bug(s.update(TxnId(2), t, id, &row(2)));
    assert_bug(s.delete(TxnId(2), t, id));
    assert_eq!(chain(&s, t, id), Some(vec![(1, Some(2))]));
}

#[test]
fn update_and_delete_precondition_violations_are_bug_errors() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let t = s.create_table(db, &shape(2)).unwrap();
    let two = Row(vec![Value::I32(1), Value::I32(2)]);
    let id = s.insert(TxnId(1), t, &two).unwrap();
    s.commit(TxnId(1)).unwrap();
    // Unknown table, unknown row, wrong arity.
    assert_bug(s.update(TxnId(2), TableId(99), id, &two));
    assert_bug(s.delete(TxnId(2), TableId(99), id));
    assert_bug(s.update(TxnId(2), t, RowId(99), &two));
    assert_bug(s.delete(TxnId(2), t, RowId(99)));
    assert_bug(s.update(TxnId(2), t, id, &row(1)));
    assert_bug(s.update(TxnId(2), t, id, &Row(vec![])));
    // Finished transactions.
    assert_bug(s.update(TxnId(1), t, id, &two));
    assert_bug(s.delete(TxnId(1), t, id));
    s.insert(TxnId(3), t, &two).unwrap();
    s.rollback(TxnId(3)).unwrap();
    assert_bug(s.update(TxnId(3), t, id, &two));
    assert_bug(s.delete(TxnId(3), t, id));
    // Nothing leaked and 2 was never registered.
    assert_eq!(chain(&s, t, id), Some(vec![(1, None)]));
    assert_eq!(known_txns(&s), vec![1, 3]);
    assert_eq!(collect(&s, &snap(4, &[]), t), vec![(id, two)]);
}

// ---------------------------------------------------------------- latest_version

#[test]
fn latest_version_reports_last_writer_and_content() {
    let (s, _, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    // Snapshots are ignored: an uncommitted version is reported.
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(1), row(1))));
    s.commit(TxnId(1)).unwrap();
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(1), row(1))));
    s.update(TxnId(2), t, id, &row(2)).unwrap();
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(2), row(2))));
    s.rollback(TxnId(2)).unwrap();
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(1), row(1))));
    s.update(TxnId(3), t, id, &row(3)).unwrap();
    s.commit(TxnId(3)).unwrap();
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(3), row(3))));
}

#[test]
fn latest_version_of_deleted_row_reports_deleter() {
    let (s, _, t, id) = one_committed_row();
    s.delete(TxnId(2), t, id).unwrap();
    // The deleter, with the content of the deleted version, committed or not.
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(2), row(1))));
    s.rollback(TxnId(2)).unwrap();
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(1), row(1))));
    s.delete(TxnId(3), t, id).unwrap();
    s.commit(TxnId(3)).unwrap();
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(3), row(1))));
}

#[test]
fn latest_version_unknown_row_is_none() {
    let (s, _, t) = one_table();
    assert_eq!(s.latest_version(t, RowId(1)).unwrap(), None);
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.rollback(TxnId(1)).unwrap();
    assert_eq!(s.latest_version(t, id).unwrap(), None);
    assert_bug(s.latest_version(TableId(99), id));
}

// -------------------------------------------------------------------- Savepoints

#[test]
fn rollback_to_savepoint_undoes_only_later_writes() {
    let (s, _, t, c) = one_committed_row();
    let a = s.insert(TxnId(2), t, &row(10)).unwrap();
    let sp = s.savepoint(TxnId(2)).unwrap();
    let b = s.insert(TxnId(2), t, &row(20)).unwrap();
    s.update(TxnId(2), t, a, &row(11)).unwrap();
    s.delete(TxnId(2), t, c).unwrap();
    assert_eq!(
        collect(&s, &snap(2, &[]), t),
        vec![(a, row(11)), (b, row(20))]
    );
    s.rollback_to(TxnId(2), sp).unwrap();
    // `a` is back to its content before the savepoint, `b` is gone, `c` is undeleted.
    assert_eq!(
        collect(&s, &snap(2, &[]), t),
        vec![(c, row(1)), (a, row(10))]
    );
    assert_eq!(chain(&s, t, a), Some(vec![(2, None)]));
    assert_eq!(chain(&s, t, b), None);
    assert_eq!(chain(&s, t, c), Some(vec![(1, None)]));
    assert_eq!(s.latest_version(t, c).unwrap(), Some((TxnId(1), row(1))));
    // The transaction is still in progress and can go on writing.
    let d = s.insert(TxnId(2), t, &row(30)).unwrap();
    assert!(d > b, "RowId reused after rollback_to");
    s.commit(TxnId(2)).unwrap();
    assert_eq!(
        collect(&s, &snap(3, &[]), t),
        vec![(c, row(1)), (a, row(10)), (d, row(30))]
    );
}

#[test]
fn rollback_to_same_savepoint_twice_is_ok() {
    let (s, _, t, c) = one_committed_row();
    let sp = s.savepoint(TxnId(2)).unwrap();
    let b1 = s.insert(TxnId(2), t, &row(20)).unwrap();
    s.rollback_to(TxnId(2), sp).unwrap();
    let b2 = s.insert(TxnId(2), t, &row(21)).unwrap();
    s.delete(TxnId(2), t, c).unwrap();
    s.rollback_to(TxnId(2), sp).unwrap();
    assert_eq!(collect(&s, &snap(2, &[]), t), vec![(c, row(1))]);
    assert_eq!(chain(&s, t, b1), None);
    assert_eq!(chain(&s, t, b2), None);
    // Still valid a third time, with nothing to undo.
    s.rollback_to(TxnId(2), sp).unwrap();
    s.commit(TxnId(2)).unwrap();
}

#[test]
fn savepoint_after_rollback_to_is_invalid() {
    let (s, _, t) = one_table();
    let sp1 = s.savepoint(TxnId(1)).unwrap();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    let sp2 = s.savepoint(TxnId(1)).unwrap();
    assert!(
        sp1 < sp2,
        "savepoint ids must increase within a transaction"
    );
    s.insert(TxnId(1), t, &row(2)).unwrap();
    s.rollback_to(TxnId(1), sp1).unwrap();
    assert_bug(s.rollback_to(TxnId(1), sp2));
    // `sp1` survives and a new savepoint gets a fresh id.
    s.rollback_to(TxnId(1), sp1).unwrap();
    let sp3 = s.savepoint(TxnId(1)).unwrap();
    assert!(sp3 > sp2);
    assert!(collect(&s, &snap(1, &[]), t).is_empty());
}

#[test]
fn savepoint_of_other_or_unknown_txn_is_bug_error() {
    let (s, _, t) = one_table();
    let sp = s.savepoint(TxnId(1)).unwrap();
    s.insert(TxnId(2), t, &row(2)).unwrap();
    // Another transaction's savepoint, a made-up one, an unknown transaction.
    assert_bug(s.rollback_to(TxnId(2), sp));
    assert_bug(s.rollback_to(TxnId(1), SavepointId(999)));
    assert_bug(s.rollback_to(TxnId(7), sp));
    // Nothing was undone and 7 was not registered.
    assert_eq!(collect(&s, &snap(2, &[]), t).len(), 1);
    assert_eq!(known_txns(&s), vec![1, 2]);
}

#[test]
fn savepoint_on_finished_txn_is_bug_error() {
    let (s, _, t) = one_table();
    let sp = s.savepoint(TxnId(1)).unwrap();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    assert_bug(s.savepoint(TxnId(1)));
    assert_bug(s.rollback_to(TxnId(1), sp));
    let sp2 = s.savepoint(TxnId(2)).unwrap();
    s.rollback(TxnId(2)).unwrap();
    assert_bug(s.savepoint(TxnId(2)));
    assert_bug(s.rollback_to(TxnId(2), sp2));
    // The committed row is untouched by the refused calls.
    assert_eq!(collect(&s, &snap(3, &[]), t).len(), 1);
}

#[test]
fn savepoint_before_first_write_registers_txn_in_progress() {
    let (s, _, t) = one_table();
    let sp = s.savepoint(TxnId(5)).unwrap();
    assert_eq!(
        s.inner
            .read()
            .unwrap()
            .txns
            .get(&TxnId(5))
            .map(|st| st.status),
        Some(TxnStatus::InProgress)
    );
    let id = s.insert(TxnId(5), t, &row(1)).unwrap();
    s.rollback_to(TxnId(5), sp).unwrap();
    assert_eq!(chain(&s, t, id), None);
    s.commit(TxnId(5)).unwrap();
    assert!(collect(&s, &snap(6, &[]), t).is_empty());
}

#[test]
fn rollback_after_update_restores_previous_version_for_everyone() {
    let (s, _, t, id) = one_committed_row();
    s.update(TxnId(2), t, id, &row(2)).unwrap();
    s.update(TxnId(2), t, id, &row(3)).unwrap();
    s.rollback(TxnId(2)).unwrap();
    for sn in [snap(2, &[]), snap(3, &[]), snap(3, &[2])] {
        assert_eq!(s.get(&sn, t, id).unwrap(), Some(row(1)));
        assert_eq!(collect(&s, &sn, t), vec![(id, row(1))]);
    }
    assert_eq!(chain(&s, t, id), Some(vec![(1, None)]));
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(1), row(1))));
    // The `xmax` was reset: the row can be written again.
    s.update(TxnId(3), t, id, &row(3)).unwrap();
    assert_eq!(s.get(&snap(3, &[]), t, id).unwrap(), Some(row(3)));
}

#[test]
fn rollback_after_delete_restores_row_for_everyone() {
    let (s, _, t, id) = one_committed_row();
    s.delete(TxnId(2), t, id).unwrap();
    s.rollback(TxnId(2)).unwrap();
    for sn in [snap(2, &[]), snap(3, &[]), snap(3, &[2])] {
        assert_eq!(s.get(&sn, t, id).unwrap(), Some(row(1)));
    }
    assert_eq!(chain(&s, t, id), Some(vec![(1, None)]));
    s.delete(TxnId(3), t, id).unwrap();
    assert_eq!(s.get(&snap(3, &[]), t, id).unwrap(), None);
}

#[test]
fn rollback_to_ignores_writes_on_dropped_table() {
    let (s, db, t) = one_table();
    let keep = s.create_table(db, &shape(1)).unwrap();
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    let kept = s.insert(TxnId(1), keep, &row(2)).unwrap();
    s.commit(TxnId(1)).unwrap();
    let sp = s.savepoint(TxnId(2)).unwrap();
    s.update(TxnId(2), t, id, &row(10)).unwrap();
    s.insert(TxnId(2), t, &row(11)).unwrap();
    s.delete(TxnId(2), keep, kept).unwrap();
    s.drop_table(t).unwrap();
    s.rollback_to(TxnId(2), sp).unwrap();
    // The surviving table was rolled back to the savepoint normally.
    assert_eq!(collect(&s, &snap(2, &[]), keep), vec![(kept, row(2))]);
    s.commit(TxnId(2)).unwrap();
}

// ------------------------------------------------------------------------ Vacuum

#[test]
fn vacuum_removes_versions_deleted_before_horizon() {
    let (s, _, t) = one_table();
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    let b = s.insert(TxnId(1), t, &row(2)).unwrap();
    let c = s.insert(TxnId(1), t, &row(3)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.delete(TxnId(2), t, a).unwrap();
    s.commit(TxnId(2)).unwrap();
    s.update(TxnId(3), t, b, &row(20)).unwrap();
    s.commit(TxnId(3)).unwrap();
    s.vacuum(TxnId(4)).unwrap();
    // `a` has no version left: the logical row is gone. `b` keeps only its live version.
    // `c`, committed and never touched, is untouched even though its `xmin < horizon`.
    assert_eq!(chain(&s, t, a), None);
    assert_eq!(s.latest_version(t, a).unwrap(), None);
    assert_eq!(chain(&s, t, b), Some(vec![(3, None)]));
    assert_eq!(chain(&s, t, c), Some(vec![(1, None)]));
    assert_eq!(
        collect(&s, &snap(4, &[]), t),
        vec![(b, row(20)), (c, row(3))]
    );
    // Twice with the same horizon is harmless.
    s.vacuum(TxnId(4)).unwrap();
    assert_eq!(
        collect(&s, &snap(4, &[]), t),
        vec![(b, row(20)), (c, row(3))]
    );
}

#[test]
fn vacuum_keeps_versions_a_current_snapshot_can_see() {
    let (s, _, t, id) = one_committed_row();
    // T3 deletes the row; T5 takes its snapshot while T3 is still in progress; T3 commits.
    s.delete(TxnId(3), t, id).unwrap();
    let t5 = snap(5, &[3]);
    assert_eq!(
        t5,
        Snapshot {
            xmin: TxnId(3),
            xmax: TxnId(6),
            active: vec![TxnId(3)],
            own: TxnId(5),
        }
    );
    s.commit(TxnId(3)).unwrap();
    // T5's snapshot is the oldest usable one, so `txn` provides `horizon = 3`: T3's delete
    // is not below the horizon and must stay.
    s.vacuum(TxnId(3)).unwrap();
    assert_eq!(s.get(&t5, t, id).unwrap(), Some(row(1)));
    assert_eq!(collect(&s, &t5, t), vec![(id, row(1))]);
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(3), row(1))));
    // Once T5 is gone, `horizon = 4` lets the version go.
    s.vacuum(TxnId(4)).unwrap();
    assert_eq!(s.get(&snap(6, &[]), t, id).unwrap(), None);
    assert!(collect(&s, &snap(6, &[]), t).is_empty());
    assert_eq!(s.latest_version(t, id).unwrap(), None);
}

#[test]
fn vacuum_does_not_reuse_row_ids() {
    let (s, _, t, a) = one_committed_row();
    s.delete(TxnId(2), t, a).unwrap();
    s.commit(TxnId(2)).unwrap();
    s.vacuum(TxnId(3)).unwrap();
    assert_eq!(chain(&s, t, a), None);
    let b = s.insert(TxnId(3), t, &row(2)).unwrap();
    assert!(b > a, "RowId reused after vacuum");
    assert_eq!(s.get(&snap(3, &[]), t, a).unwrap(), None);
}

#[test]
fn vacuum_leaves_in_progress_and_recent_deletes_alone() {
    let (s, _, t) = one_table();
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    let b = s.insert(TxnId(1), t, &row(2)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.delete(TxnId(2), t, a).unwrap();
    s.delete(TxnId(3), t, b).unwrap();
    s.commit(TxnId(3)).unwrap();
    // 2 is in progress and 3 is not below the horizon: both versions stay.
    s.vacuum(TxnId(3)).unwrap();
    assert_eq!(chain(&s, t, a), Some(vec![(1, Some(2))]));
    assert_eq!(chain(&s, t, b), Some(vec![(1, Some(3))]));
    assert_eq!(collect(&s, &snap(4, &[2]), t), vec![(a, row(1))]);
    // 2 rolls back: its delete never happened and `a` outlives any horizon.
    s.rollback(TxnId(2)).unwrap();
    s.vacuum(TxnId(4)).unwrap();
    assert_eq!(chain(&s, t, a), Some(vec![(1, None)]));
    assert_eq!(chain(&s, t, b), None);
    assert_eq!(collect(&s, &snap(4, &[]), t), vec![(a, row(1))]);
}

#[test]
fn vacuum_forgets_finished_txns_below_horizon_only() {
    let (s, _, t) = one_table();
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.insert(TxnId(2), t, &row(2)).unwrap();
    s.rollback(TxnId(2)).unwrap();
    let c = s.insert(TxnId(3), t, &row(3)).unwrap();
    s.commit(TxnId(4)).unwrap();
    s.insert(TxnId(5), t, &row(5)).unwrap();
    s.commit(TxnId(5)).unwrap();
    assert_eq!(known_txns(&s), vec![1, 2, 3, 4, 5]);
    s.vacuum(TxnId(4)).unwrap();
    // 1 and 2 are finished and below the horizon; 3 is in progress; 4 and 5 are not below.
    assert_eq!(known_txns(&s), vec![3, 4, 5]);
    // The forgotten 1 is now reported as `Committed`, which is exactly what it is: its row
    // is still visible, and 3's uncommitted row still is not.
    assert_eq!(s.get(&snap(6, &[3]), t, a).unwrap(), Some(row(1)));
    assert_eq!(s.get(&snap(6, &[3]), t, c).unwrap(), None);
    assert_eq!(s.get(&snap(3, &[]), t, c).unwrap(), Some(row(3)));
    // Vacuuming with the same horizon changes nothing more.
    s.vacuum(TxnId(4)).unwrap();
    assert_eq!(known_txns(&s), vec![3, 4, 5]);
}

#[test]
fn vacuum_removes_versions_of_aborted_creator_defensively() {
    // `rollback` removes the versions of an aborted transaction itself; the second pass of
    // `vacuum` is a safety net. Forge the state it guards against: a version whose creator
    // is registered `Aborted` but still present.
    let (s, _, t) = one_table();
    let id = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.inner
        .write()
        .unwrap()
        .txns
        .get_mut(&TxnId(1))
        .unwrap()
        .status = TxnStatus::Aborted;
    assert_eq!(s.latest_version(t, id).unwrap(), Some((TxnId(1), row(1))));
    // The horizon does not matter for this pass: 1 is not below it and stays registered.
    s.vacuum(TxnId(1)).unwrap();
    assert_eq!(chain(&s, t, id), None);
    assert_eq!(s.latest_version(t, id).unwrap(), None);
    assert_eq!(known_txns(&s), vec![1]);
}

#[test]
fn vacuum_of_empty_storage_is_ok() {
    let s = MemoryStorage::new();
    s.vacuum(TxnId(1)).unwrap();
    s.vacuum(TxnId(u64::MAX)).unwrap();
    assert!(s.databases().unwrap().is_empty());
}

// ----------------------------------------------------------------------- Indexes

fn col(column: u16, descending: bool) -> KeyColumn {
    KeyColumn { column, descending }
}

/// An index shape on the given `(column, descending)` pairs, no included column.
fn index(columns: &[(u16, bool)], unique: bool) -> IndexShape {
    IndexShape {
        columns: columns.iter().map(|&(c, d)| col(c, d)).collect(),
        unique,
        included: vec![],
    }
}

fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

fn ints(values: &[Option<i32>]) -> Row {
    Row(values
        .iter()
        .map(|v| v.map_or(Value::Null, Value::I32))
        .collect())
}

/// A table of one nullable `varchar(10)` column, no clustered key.
fn text_shape() -> TableShape {
    TableShape {
        columns: vec![TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true)],
        clustered_key: None,
    }
}

/// A storage with one database, one table of `n` int columns and one index on the given
/// columns.
fn indexed_table(
    n: usize,
    columns: &[(u16, bool)],
    unique: bool,
) -> (MemoryStorage, TableId, IndexId) {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let t = s.create_table(db, &shape(n)).unwrap();
    let i = s.create_index(t, &index(columns, unique)).unwrap();
    (s, t, i)
}

/// Inserts every row on behalf of `txn` and returns the ids.
fn insert_all(s: &MemoryStorage, txn: u64, t: TableId, rows: &[Row]) -> Vec<RowId> {
    rows.iter()
        .map(|r| s.insert(TxnId(txn), t, r).unwrap())
        .collect()
}

fn seek_rows(
    s: &MemoryStorage,
    snap: &Snapshot,
    i: IndexId,
    range: &KeyRange,
    dir: Direction,
) -> Vec<(RowId, Row)> {
    s.seek(snap, i, range, dir)
        .unwrap()
        .map(|item| item.unwrap())
        .collect()
}

fn seek_ids(s: &MemoryStorage, snap: &Snapshot, i: IndexId, range: &KeyRange) -> Vec<RowId> {
    seek_rows(s, snap, i, range, Direction::Forward)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

fn point(values: &[Option<i32>]) -> KeyRange {
    KeyRange::Point(ints(values).0)
}

fn between(lo: Bound<&[Option<i32>]>, hi: Bound<&[Option<i32>]>) -> KeyRange {
    KeyRange::Between(lo.map(|v| ints(v).0), hi.map(|v| ints(v).0))
}

/// Asserts that `r` is the 2601 error naming `t` and `i` by their bare decimal ids.
fn assert_duplicate<T: Debug>(r: SqlResult<T>, t: TableId, i: IndexId) {
    let err = r.expect_err("expected error 2601");
    assert_eq!(err.number, 2601, "unexpected error: {}", err.message);
    assert!(
        err.message.contains(&format!("'{t}'")) && err.message.contains(&format!("'{i}'")),
        "unexpected message: {}",
        err.message
    );
}

/// The number of entries an index holds, read straight from the storage.
fn entry_count(s: &MemoryStorage, i: IndexId) -> usize {
    s.inner
        .read()
        .unwrap()
        .indexes
        .get(&i)
        .map_or(0, |index| index.entries.len())
}

// ------------------------------------------------------------------- seek

#[test]
fn seek_point_returns_only_matching_visible_rows() {
    let (s, t, i) = indexed_table(1, &[(0, false)], false);
    let ids = insert_all(&s, 1, t, &[row(5), row(7), row(5)]);
    s.commit(TxnId(1)).unwrap();
    let pending = s.insert(TxnId(2), t, &row(5)).unwrap();
    // 3 does not see 2's row; 2 sees it.
    assert_eq!(
        seek_ids(&s, &snap(3, &[2]), i, &point(&[Some(5)])),
        vec![ids[0], ids[2]]
    );
    assert_eq!(
        seek_ids(&s, &snap(2, &[]), i, &point(&[Some(5)])),
        vec![ids[0], ids[2], pending]
    );
    assert_eq!(
        seek_ids(&s, &snap(3, &[2]), i, &point(&[Some(7)])),
        vec![ids[1]]
    );
    assert!(seek_ids(&s, &snap(3, &[2]), i, &point(&[Some(6)])).is_empty());
    // The content is that of the visible version.
    let rows = seek_rows(
        &s,
        &snap(3, &[2]),
        i,
        &point(&[Some(7)]),
        Direction::Forward,
    );
    assert_eq!(rows, vec![(ids[1], row(7))]);
}

#[test]
fn seek_point_on_prefix_of_composite_key() {
    let (s, t, i) = indexed_table(2, &[(0, false), (1, false)], false);
    let ids = insert_all(
        &s,
        1,
        t,
        &[
            ints(&[Some(1), Some(2)]),
            ints(&[Some(2), Some(1)]),
            ints(&[Some(1), Some(1)]),
        ],
    );
    s.commit(TxnId(1)).unwrap();
    let sn = snap(2, &[]);
    assert_eq!(
        seek_ids(&s, &sn, i, &point(&[Some(1)])),
        vec![ids[2], ids[0]]
    );
    assert_eq!(
        seek_ids(&s, &sn, i, &point(&[Some(1), Some(2)])),
        vec![ids[0]]
    );
    assert_eq!(seek_ids(&s, &sn, i, &point(&[Some(2)])), vec![ids[1]]);
    assert!(seek_ids(&s, &sn, i, &point(&[Some(2), Some(2)])).is_empty());
}

#[test]
fn seek_empty_point_equals_full() {
    let (s, t, i) = indexed_table(1, &[(0, false)], false);
    insert_all(&s, 1, t, &[row(3), row(1), row(2)]);
    s.commit(TxnId(1)).unwrap();
    let sn = snap(2, &[]);
    let full = seek_rows(&s, &sn, i, &KeyRange::Full, Direction::Forward);
    assert_eq!(full.len(), 3);
    assert_eq!(
        seek_rows(&s, &sn, i, &KeyRange::Point(vec![]), Direction::Forward),
        full
    );
    let full_back = seek_rows(&s, &sn, i, &KeyRange::Full, Direction::Backward);
    assert_eq!(
        seek_rows(&s, &sn, i, &KeyRange::Point(vec![]), Direction::Backward),
        full_back
    );
}

#[test]
fn seek_between_respects_included_and_excluded_bounds() {
    let (s, t, i) = indexed_table(1, &[(0, false)], false);
    let ids = insert_all(&s, 1, t, &[row(1), row(2), row(3), row(4), row(5)]);
    s.commit(TxnId(1)).unwrap();
    let sn = snap(2, &[]);
    let two = &[Some(2)][..];
    let four = &[Some(4)][..];
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Included(two), Bound::Excluded(four))
        ),
        vec![ids[1], ids[2]]
    );
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Excluded(two), Bound::Included(four))
        ),
        vec![ids[2], ids[3]]
    );
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Included(two), Bound::Included(four))
        ),
        vec![ids[1], ids[2], ids[3]]
    );
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Excluded(two), Bound::Excluded(four))
        ),
        vec![ids[2]]
    );
    assert_eq!(
        seek_ids(&s, &sn, i, &between(Bound::Unbounded, Bound::Excluded(two))),
        vec![ids[0]]
    );
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Included(four), Bound::Unbounded)
        ),
        vec![ids[3], ids[4]]
    );
    assert_eq!(
        seek_ids(&s, &sn, i, &between(Bound::Unbounded, Bound::Unbounded)),
        ids
    );
    // An empty or inverted range yields nothing, never an error.
    assert!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Excluded(two), Bound::Excluded(two))
        )
        .is_empty()
    );
    assert!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Included(four), Bound::Included(two))
        )
        .is_empty()
    );
    // Bounds outside the stored keys.
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Included(&[Some(0)]), Bound::Included(&[Some(9)]))
        ),
        ids
    );
}

#[test]
fn seek_between_prefix_bounds_cover_whole_prefix_group() {
    let (s, t, i) = indexed_table(2, &[(0, false), (1, false)], false);
    let ids = insert_all(
        &s,
        1,
        t,
        &[
            ints(&[Some(0), Some(9)]),
            ints(&[Some(1), Some(0)]),
            ints(&[Some(1), Some(5)]),
            ints(&[Some(2), Some(0)]),
        ],
    );
    s.commit(TxnId(1)).unwrap();
    let sn = snap(2, &[]);
    let one = &[Some(1)][..];
    // `Included((1))` as the high bound includes `(1, 5)`.
    assert_eq!(
        seek_ids(&s, &sn, i, &between(Bound::Unbounded, Bound::Included(one))),
        vec![ids[0], ids[1], ids[2]]
    );
    // `Excluded((1))` as the high bound excludes `(1, 0)`.
    assert_eq!(
        seek_ids(&s, &sn, i, &between(Bound::Unbounded, Bound::Excluded(one))),
        vec![ids[0]]
    );
    // Same on the low side.
    assert_eq!(
        seek_ids(&s, &sn, i, &between(Bound::Included(one), Bound::Unbounded)),
        vec![ids[1], ids[2], ids[3]]
    );
    assert_eq!(
        seek_ids(&s, &sn, i, &between(Bound::Excluded(one), Bound::Unbounded)),
        vec![ids[3]]
    );
    // Both bounds on the same prefix: the whole group, or nothing.
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Included(one), Bound::Included(one))
        ),
        vec![ids[1], ids[2]]
    );
    assert!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Included(one), Bound::Excluded(one))
        )
        .is_empty()
    );
    // A full-length bound inside the group.
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &between(Bound::Excluded(&[Some(1), Some(0)]), Bound::Included(one))
        ),
        vec![ids[2]]
    );
}

#[test]
fn seek_backward_reverses_order() {
    let (s, t, i) = indexed_table(1, &[(0, false)], false);
    let ids = insert_all(&s, 1, t, &[row(2), row(1), row(2), row(3)]);
    s.commit(TxnId(1)).unwrap();
    let sn = snap(2, &[]);
    let forward = seek_ids(&s, &sn, i, &KeyRange::Full);
    assert_eq!(forward, vec![ids[1], ids[0], ids[2], ids[3]]);
    let backward: Vec<RowId> = seek_rows(&s, &sn, i, &KeyRange::Full, Direction::Backward)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    // Ties (the two `2`s) come by decreasing `RowId` too.
    assert_eq!(backward, vec![ids[3], ids[2], ids[0], ids[1]]);
    let range = between(Bound::Included(&[Some(2)]), Bound::Unbounded);
    let backward: Vec<RowId> = seek_rows(&s, &sn, i, &range, Direction::Backward)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(backward, vec![ids[3], ids[2], ids[0]]);
}

#[test]
fn seek_full_equals_sorted_scan() {
    let (s, t, i) = indexed_table(1, &[(0, false)], false);
    insert_all(&s, 1, t, &[row(30), row(10), row(20), row(10)]);
    s.commit(TxnId(1)).unwrap();
    s.insert(TxnId(2), t, &row(5)).unwrap();
    s.commit(TxnId(2)).unwrap();
    let sn = snap(3, &[]);
    let mut scanned = collect(&s, &sn, t);
    scanned.sort_by_key(|(id, r)| {
        let Value::I32(v) = r.0[0] else {
            panic!("int expected")
        };
        (v, *id)
    });
    assert_eq!(
        seek_rows(&s, &sn, i, &KeyRange::Full, Direction::Forward),
        scanned
    );
}

#[test]
fn seek_with_too_long_prefix_is_bug_error() {
    let (s, t, i) = indexed_table(1, &[(0, false)], false);
    insert_all(&s, 1, t, &[row(1)]);
    s.commit(TxnId(1)).unwrap();
    let sn = snap(2, &[]);
    assert_bug(
        s.seek(&sn, i, &point(&[Some(1), Some(2)]), Direction::Forward)
            .map(|_| ()),
    );
    let too_long = between(Bound::Unbounded, Bound::Included(&[Some(1), Some(2)]));
    assert_bug(s.seek(&sn, i, &too_long, Direction::Forward).map(|_| ()));
    let too_long = between(Bound::Excluded(&[Some(1), Some(2)]), Bound::Unbounded);
    assert_bug(s.seek(&sn, i, &too_long, Direction::Forward).map(|_| ()));
    // A value whose variant does not match the column type is a bug too; `NULL` fits.
    assert_bug(
        s.seek(
            &sn,
            i,
            &KeyRange::Point(vec![text("1")]),
            Direction::Forward,
        )
        .map(|_| ()),
    );
    assert!(seek_ids(&s, &sn, i, &point(&[None])).is_empty());
    // The exact-length prefix is fine, and an unknown index is a bug.
    assert_eq!(seek_ids(&s, &sn, i, &point(&[Some(1)])).len(), 1);
    assert_bug(
        s.seek(&sn, IndexId(99), &KeyRange::Full, Direction::Forward)
            .map(|_| ()),
    );
}

#[test]
fn seek_is_isolated_from_later_inserts_of_other_txns() {
    let (s, t, i) = indexed_table(1, &[(0, false)], false);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    // 2 creates its iterator, then 3 inserts and commits, then 2 iterates.
    let iter = s
        .seek(&snap(2, &[]), i, &KeyRange::Full, Direction::Forward)
        .unwrap();
    s.insert(TxnId(3), t, &row(0)).unwrap();
    s.commit(TxnId(3)).unwrap();
    let seen: Vec<(RowId, Row)> = iter.map(|item| item.unwrap()).collect();
    assert_eq!(seen, vec![(a, row(1))]);
}

#[test]
fn nulls_sort_first_ascending_and_last_descending() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let t = s.create_table(db, &shape(1)).unwrap();
    let asc = s.create_index(t, &index(&[(0, false)], false)).unwrap();
    let desc = s.create_index(t, &index(&[(0, true)], false)).unwrap();
    let ids = insert_all(&s, 1, t, &[row(2), ints(&[None]), row(1), ints(&[None])]);
    s.commit(TxnId(1)).unwrap();
    let sn = snap(2, &[]);
    assert_eq!(
        seek_ids(&s, &sn, asc, &KeyRange::Full),
        vec![ids[1], ids[3], ids[2], ids[0]]
    );
    assert_eq!(
        seek_ids(&s, &sn, desc, &KeyRange::Full),
        vec![ids[0], ids[2], ids[1], ids[3]]
    );
    // `NULL` is a key like any other: a point on it, and bounds around it, in both orders.
    assert_eq!(
        seek_ids(&s, &sn, asc, &point(&[None])),
        vec![ids[1], ids[3]]
    );
    assert_eq!(
        seek_ids(&s, &sn, desc, &point(&[None])),
        vec![ids[1], ids[3]]
    );
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            asc,
            &between(Bound::Excluded(&[None]), Bound::Unbounded)
        ),
        vec![ids[2], ids[0]]
    );
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            desc,
            &between(Bound::Unbounded, Bound::Excluded(&[None]))
        ),
        vec![ids[0], ids[2]]
    );
    // Descending bounds are given in index order: `(2)` comes before `(1)`.
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            desc,
            &between(Bound::Included(&[Some(2)]), Bound::Included(&[Some(1)]))
        ),
        vec![ids[0], ids[2]]
    );
}

#[test]
fn string_keys_compare_case_insensitively_under_default_collation() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let t = s.create_table(db, &text_shape()).unwrap();
    let i = s.create_index(t, &index(&[(0, false)], true)).unwrap();
    let abc = s.insert(TxnId(1), t, &Row(vec![text("abc")])).unwrap();
    s.commit(TxnId(1)).unwrap();
    assert_duplicate(s.insert(TxnId(2), t, &Row(vec![text("ABC")])), t, i);
    assert_duplicate(s.insert(TxnId(2), t, &Row(vec![text("abc  ")])), t, i);
    let b = s.insert(TxnId(2), t, &Row(vec![text("B")])).unwrap();
    let a = s.insert(TxnId(2), t, &Row(vec![text("a")])).unwrap();
    s.commit(TxnId(2)).unwrap();
    let sn = snap(3, &[]);
    // Order: `a` < `abc` < `B` (case-insensitive), and a point matches whatever the case.
    assert_eq!(seek_ids(&s, &sn, i, &KeyRange::Full), vec![a, abc, b]);
    assert_eq!(
        seek_ids(&s, &sn, i, &KeyRange::Point(vec![text("ABC ")])),
        vec![abc]
    );
    assert_eq!(
        seek_ids(
            &s,
            &sn,
            i,
            &KeyRange::Between(
                Bound::Excluded(vec![text("A")]),
                Bound::Excluded(vec![text("b")])
            )
        ),
        vec![abc]
    );
}

// ------------------------------------------------------------- Uniqueness

#[test]
fn unique_index_rejects_duplicate_with_2601() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    let err = s.insert(TxnId(2), t, &row(1)).unwrap_err();
    assert_eq!(err.number, 2601);
    assert_eq!(err.state, 1, "{}", err.message);
    for part in [format!("'{t}'"), format!("'{i}'"), "(I32(1))".to_string()] {
        assert!(
            err.message.contains(&part),
            "2601 must spell out {part}: {}",
            err.message
        );
    }
    // Nothing was inserted, no trace of 2 was kept, and the storage is still usable.
    assert_eq!(collect(&s, &snap(2, &[]), t), vec![(a, row(1))]);
    assert_eq!(entry_count(&s, i), 1);
    assert!(!known_txns(&s).contains(&2));
    let b = s.insert(TxnId(2), t, &row(2)).unwrap();
    assert_eq!(seek_ids(&s, &snap(2, &[]), i, &KeyRange::Full), vec![a, b]);
    // A non-unique index accepts duplicates.
    let (s, t, _) = indexed_table(1, &[(0, false)], false);
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.insert(TxnId(1), t, &row(1)).unwrap();
}

#[test]
fn unique_index_on_update_rejects_key_of_other_live_row_and_leaves_row_unchanged() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    let ids = insert_all(&s, 1, t, &[row(1), row(2)]);
    s.commit(TxnId(1)).unwrap();
    assert_duplicate(s.update(TxnId(2), t, ids[1], &row(1)), t, i);
    assert_eq!(s.get(&snap(2, &[]), t, ids[1]).unwrap(), Some(row(2)));
    assert_eq!(chain(&s, t, ids[1]), Some(vec![(1, None)]));
    assert!(!known_txns(&s).contains(&2));
    assert_eq!(entry_count(&s, i), 2);
    // A key change to a free value passes.
    s.update(TxnId(2), t, ids[1], &row(3)).unwrap();
    assert_eq!(
        seek_ids(&s, &snap(2, &[]), i, &point(&[Some(3)])),
        vec![ids[1]]
    );
}

#[test]
fn unique_index_treats_null_as_a_value() {
    let (s, t, i) = indexed_table(2, &[(0, false), (1, false)], true);
    s.insert(TxnId(1), t, &ints(&[Some(1), None])).unwrap();
    assert_duplicate(s.insert(TxnId(1), t, &ints(&[Some(1), None])), t, i);
    // `(1, 2)` and `(2, NULL)` are other keys.
    s.insert(TxnId(1), t, &ints(&[Some(1), Some(2)])).unwrap();
    s.insert(TxnId(1), t, &ints(&[Some(2), None])).unwrap();
    s.insert(TxnId(1), t, &ints(&[None, None])).unwrap();
    assert_duplicate(s.insert(TxnId(1), t, &ints(&[None, None])), t, i);
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    s.insert(TxnId(1), t, &ints(&[None])).unwrap();
    assert_duplicate(s.insert(TxnId(1), t, &ints(&[None])), t, i);
}

#[test]
fn unique_check_sees_uncommitted_insert_of_other_txn() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    s.insert(TxnId(1), t, &row(1)).unwrap();
    // 2 would have to wait in SQL Server; here it gets the error at once.
    assert_duplicate(s.insert(TxnId(2), t, &row(1)), t, i);
    // The same holds after 1 committed, whatever 2's snapshot says.
    s.commit(TxnId(1)).unwrap();
    assert_duplicate(s.insert(TxnId(2), t, &row(1)), t, i);
}

#[test]
fn unique_check_ignores_rows_deleted_by_committed_txn() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.delete(TxnId(2), t, a).unwrap();
    // While 2 is in progress, the deleted row is still live for 3.
    assert_duplicate(s.insert(TxnId(3), t, &row(1)), t, i);
    s.commit(TxnId(2)).unwrap();
    let b = s.insert(TxnId(3), t, &row(1)).unwrap();
    s.commit(TxnId(3)).unwrap();
    assert_eq!(seek_ids(&s, &snap(4, &[]), i, &point(&[Some(1)])), vec![b]);
    // Both versions are still indexed until vacuum; visibility does the filtering.
    assert_eq!(entry_count(&s, i), 2);
    assert_eq!(seek_ids(&s, &snap(4, &[3]), i, &point(&[Some(1)])), vec![]);
}

#[test]
fn unique_check_ignores_rows_of_rolled_back_txn() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.rollback(TxnId(1)).unwrap();
    let b = s.insert(TxnId(2), t, &row(1)).unwrap();
    assert_eq!(seek_ids(&s, &snap(2, &[]), i, &point(&[Some(1)])), vec![b]);
    // An update rolled back frees its new key as well.
    s.commit(TxnId(2)).unwrap();
    s.update(TxnId(3), t, b, &row(5)).unwrap();
    s.rollback(TxnId(3)).unwrap();
    let c = s.insert(TxnId(4), t, &row(5)).unwrap();
    assert_eq!(seek_ids(&s, &snap(4, &[]), i, &point(&[Some(5)])), vec![c]);
    assert_duplicate(s.insert(TxnId(4), t, &row(1)), t, i);
}

#[test]
fn update_without_key_change_on_unique_index_is_ok() {
    let (s, t, i) = indexed_table(2, &[(0, false)], true);
    let a = s.insert(TxnId(1), t, &ints(&[Some(1), Some(10)])).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.update(TxnId(2), t, a, &ints(&[Some(1), Some(20)]))
        .unwrap();
    // Twice in the same transaction too.
    s.update(TxnId(2), t, a, &ints(&[Some(1), Some(30)]))
        .unwrap();
    assert_eq!(
        seek_rows(&s, &snap(2, &[]), i, &point(&[Some(1)]), Direction::Forward),
        vec![(a, ints(&[Some(1), Some(30)]))]
    );
    assert_eq!(
        seek_rows(
            &s,
            &snap(3, &[2]),
            i,
            &point(&[Some(1)]),
            Direction::Forward
        ),
        vec![(a, ints(&[Some(1), Some(10)]))]
    );
    // Another transaction still cannot take the key.
    assert_duplicate(s.insert(TxnId(3), t, &ints(&[Some(1), Some(0)])), t, i);
    s.commit(TxnId(2)).unwrap();
    assert_eq!(
        seek_rows(&s, &snap(3, &[]), i, &point(&[Some(1)]), Direction::Forward),
        vec![(a, ints(&[Some(1), Some(30)]))]
    );
    // One entry per version: three of them.
    assert_eq!(entry_count(&s, i), 3);
}

#[test]
fn delete_then_insert_same_unique_key_in_same_txn_is_ok() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.delete(TxnId(2), t, a).unwrap();
    let b = s.insert(TxnId(2), t, &row(1)).unwrap();
    assert_eq!(seek_ids(&s, &snap(2, &[]), i, &point(&[Some(1)])), vec![b]);
    // Others still see `a` under that key, and cannot take it.
    assert_eq!(seek_ids(&s, &snap(3, &[2]), i, &point(&[Some(1)])), vec![a]);
    assert_duplicate(s.insert(TxnId(3), t, &row(1)), t, i);
    s.commit(TxnId(2)).unwrap();
    assert_eq!(seek_ids(&s, &snap(3, &[]), i, &point(&[Some(1)])), vec![b]);
    // Same story with a row inserted and deleted by the same transaction.
    let c = s.insert(TxnId(4), t, &row(7)).unwrap();
    s.delete(TxnId(4), t, c).unwrap();
    s.insert(TxnId(4), t, &row(7)).unwrap();
}

#[test]
fn updated_key_moves_row_in_index_for_later_snapshots_only() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.update(TxnId(2), t, a, &row(5)).unwrap();
    // 2 sees the row under 5 only; 3, with 2 in progress, under 1 only.
    let own = snap(2, &[]);
    assert_eq!(seek_ids(&s, &own, i, &point(&[Some(5)])), vec![a]);
    assert!(seek_ids(&s, &own, i, &point(&[Some(1)])).is_empty());
    let concurrent = snap(3, &[2]);
    assert_eq!(seek_ids(&s, &concurrent, i, &point(&[Some(1)])), vec![a]);
    assert!(seek_ids(&s, &concurrent, i, &point(&[Some(5)])).is_empty());
    // A full seek yields the row once, under its visible key.
    assert_eq!(seek_ids(&s, &own, i, &KeyRange::Full), vec![a]);
    assert_eq!(seek_ids(&s, &concurrent, i, &KeyRange::Full), vec![a]);
    // Meanwhile 3 cannot take 1 (still live for it) nor 5 (2 holds it).
    assert_duplicate(s.insert(TxnId(3), t, &row(1)), t, i);
    assert_duplicate(s.insert(TxnId(3), t, &row(5)), t, i);
    s.commit(TxnId(2)).unwrap();
    let after = snap(3, &[]);
    assert_eq!(seek_ids(&s, &after, i, &point(&[Some(5)])), vec![a]);
    assert!(seek_ids(&s, &after, i, &point(&[Some(1)])).is_empty());
    // The frozen snapshot keeps its view, and the old key is free again for writers.
    assert_eq!(seek_ids(&s, &concurrent, i, &point(&[Some(1)])), vec![a]);
    s.insert(TxnId(3), t, &row(1)).unwrap();
}

// ------------------------------------------------------ create_index / drop_index

#[test]
fn create_index_indexes_existing_versions_and_future_ones() {
    let (s, _, t) = one_table();
    let ids = insert_all(&s, 1, t, &[row(2), row(1)]);
    s.commit(TxnId(1)).unwrap();
    s.update(TxnId(2), t, ids[0], &row(3)).unwrap();
    let pending = s.insert(TxnId(3), t, &row(0)).unwrap();
    let i = s.create_index(t, &index(&[(0, false)], false)).unwrap();
    // Every version that is not aborted got an entry: old and new version of `ids[0]`, 1, 0.
    assert_eq!(entry_count(&s, i), 4);
    assert_eq!(
        seek_ids(&s, &snap(4, &[2, 3]), i, &KeyRange::Full),
        vec![ids[1], ids[0]]
    );
    assert_eq!(
        seek_ids(&s, &snap(2, &[3]), i, &KeyRange::Full),
        vec![ids[1], ids[0]]
    );
    assert_eq!(
        seek_rows(
            &s,
            &snap(2, &[3]),
            i,
            &point(&[Some(3)]),
            Direction::Forward
        ),
        vec![(ids[0], row(3))]
    );
    assert_eq!(
        seek_ids(&s, &snap(3, &[2]), i, &KeyRange::Full),
        vec![pending, ids[1], ids[0]]
    );
    // Future writes are indexed too.
    let later = s.insert(TxnId(4), t, &row(9)).unwrap();
    assert_eq!(
        seek_ids(&s, &snap(4, &[2, 3]), i, &point(&[Some(9)])),
        vec![later]
    );
    assert_eq!(entry_count(&s, i), 5);
}

#[test]
fn create_unique_index_on_table_with_duplicates_fails_and_creates_nothing() {
    let (s, _, t) = one_table();
    insert_all(&s, 1, t, &[row(1), row(2), row(1)]);
    s.commit(TxnId(1)).unwrap();
    let err = s.create_index(t, &index(&[(0, false)], true)).unwrap_err();
    assert_eq!(err.number, 2601);
    assert!(err.message.contains(&format!("'{t}'")), "{}", err.message);
    assert!(err.message.contains("'1'"), "{}", err.message);
    assert!(err.message.contains("(I32(1))"), "{}", err.message);
    assert!(s.indexes(t).unwrap().is_empty());
    assert!(s.inner.read().unwrap().indexes.is_empty());
    // The id named in the message was not consumed: nothing was created.
    let i = s.create_index(t, &index(&[(0, false)], false)).unwrap();
    assert_eq!(i, IndexId(1));
    // An uncommitted duplicate counts as live too.
    let (s, _, t) = one_table();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.insert(TxnId(2), t, &row(1)).unwrap();
    assert_eq!(
        s.create_index(t, &index(&[(0, false)], true))
            .unwrap_err()
            .number,
        2601
    );
    // A duplicate of a rolled-back or committed-deleted row does not.
    s.rollback(TxnId(2)).unwrap();
    let a = s.insert(TxnId(3), t, &row(5)).unwrap();
    s.commit(TxnId(3)).unwrap();
    s.delete(TxnId(4), t, a).unwrap();
    s.commit(TxnId(4)).unwrap();
    s.insert(TxnId(5), t, &row(5)).unwrap();
    s.commit(TxnId(5)).unwrap();
    s.create_index(t, &index(&[(0, false)], true)).unwrap();
}

#[test]
fn create_unique_index_accepts_two_live_versions_of_the_same_row() {
    // An update in progress that keeps the key: both versions are live for the check, but
    // they belong to the same row.
    let (s, _, t) = one_table();
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.update(TxnId(2), t, a, &row(1)).unwrap();
    let i = s.create_index(t, &index(&[(0, false)], true)).unwrap();
    assert_eq!(entry_count(&s, i), 2);
    assert_duplicate(s.insert(TxnId(3), t, &row(1)), t, i);
}

#[test]
fn create_index_precondition_violations_are_bug_errors() {
    let (s, _, t) = one_table();
    assert_bug(s.create_index(TableId(99), &index(&[(0, false)], false)));
    assert_bug(s.create_index(t, &index(&[], false)));
    assert_bug(s.create_index(t, &index(&[(1, false)], false)));
    assert_bug(s.create_index(t, &index(&[(0, false), (7, true)], true)));
    let mut with_included = index(&[(0, false)], false);
    with_included.included = vec![1];
    assert_bug(s.create_index(t, &with_included));
    assert!(s.indexes(t).unwrap().is_empty());
    assert_bug(s.indexes(TableId(99)));
    assert_bug(s.drop_index(IndexId(1)));
}

#[test]
fn drop_index_makes_index_id_invalid() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.drop_index(i).unwrap();
    assert_bug(s.drop_index(i));
    assert_bug(
        s.seek(&snap(1, &[]), i, &KeyRange::Full, Direction::Forward)
            .map(|_| ()),
    );
    assert!(s.indexes(t).unwrap().is_empty());
    // The rows are untouched and the uniqueness constraint is gone with the index.
    assert_eq!(collect(&s, &snap(1, &[]), t), vec![(a, row(1))]);
    s.insert(TxnId(1), t, &row(1)).unwrap();
    // The undo log still mentions the dropped index's table: rollback ignores it.
    s.rollback(TxnId(1)).unwrap();
    // Ids are never reused.
    let j = s.create_index(t, &index(&[(0, false)], false)).unwrap();
    assert!(j > i);
}

#[test]
fn drop_table_drops_its_indexes() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let t = s.create_table(db, &shape(1)).unwrap();
    let u = s.create_table(db, &shape(1)).unwrap();
    let i = s.create_index(t, &index(&[(0, false)], false)).unwrap();
    let j = s.create_index(u, &index(&[(0, false)], false)).unwrap();
    s.insert(TxnId(1), t, &row(1)).unwrap();
    s.drop_table(t).unwrap();
    assert_bug(s.drop_index(i));
    assert_bug(
        s.seek(&snap(1, &[]), i, &KeyRange::Full, Direction::Forward)
            .map(|_| ()),
    );
    assert_eq!(s.indexes(u).unwrap().len(), 1);
    // The write on the dropped table is ignored by the rollback, index included.
    s.rollback(TxnId(1)).unwrap();
    // `drop_database` does the same for every table.
    s.drop_database(db).unwrap();
    assert_bug(s.drop_index(j));
    assert!(s.inner.read().unwrap().indexes.is_empty());
}

#[test]
fn indexes_lists_shapes_in_index_id_order_and_forgets_dropped() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let t = s.create_table(db, &shape(3)).unwrap();
    let u = s.create_table(db, &shape(1)).unwrap();
    let first = IndexShape {
        columns: vec![col(2, true), col(0, false)],
        unique: true,
        included: vec![1],
    };
    let second = index(&[(1, false)], false);
    let i = s.create_index(t, &first).unwrap();
    let other = s.create_index(u, &index(&[(0, false)], false)).unwrap();
    let j = s.create_index(t, &second).unwrap();
    assert!(i < other && other < j);
    assert_eq!(
        s.indexes(t).unwrap(),
        vec![(i, first.clone()), (j, second.clone())]
    );
    assert_eq!(s.indexes(u).unwrap().len(), 1);
    s.drop_index(i).unwrap();
    assert_eq!(s.indexes(t).unwrap(), vec![(j, second)]);
    assert_bug(s.indexes(TableId(99)));
    // `included` has no effect: a key on column 2 then 0 works as declared.
    let k = s.create_index(t, &first).unwrap();
    let a = s
        .insert(TxnId(1), t, &ints(&[Some(1), Some(0), Some(1)]))
        .unwrap();
    let b = s
        .insert(TxnId(1), t, &ints(&[Some(0), Some(0), Some(2)]))
        .unwrap();
    assert_eq!(seek_ids(&s, &snap(1, &[]), k, &KeyRange::Full), vec![b, a]);
}

// ------------------------------------------------------- Rollback and vacuum

#[test]
fn rollback_removes_index_entries() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    s.insert(TxnId(1), t, &row(1)).unwrap();
    assert_eq!(entry_count(&s, i), 1);
    s.rollback(TxnId(1)).unwrap();
    assert_eq!(entry_count(&s, i), 0);
    assert!(seek_ids(&s, &snap(2, &[]), i, &point(&[Some(1)])).is_empty());
    let b = s.insert(TxnId(2), t, &row(1)).unwrap();
    s.commit(TxnId(2)).unwrap();
    // A rolled-back update: the new key's entry goes, the old one serves again.
    s.update(TxnId(3), t, b, &row(5)).unwrap();
    assert_eq!(entry_count(&s, i), 2);
    s.rollback(TxnId(3)).unwrap();
    assert_eq!(entry_count(&s, i), 1);
    let sn = snap(4, &[]);
    assert_eq!(seek_ids(&s, &sn, i, &point(&[Some(1)])), vec![b]);
    assert!(seek_ids(&s, &sn, i, &point(&[Some(5)])).is_empty());
    s.insert(TxnId(4), t, &row(5)).unwrap();
    assert_duplicate(s.insert(TxnId(4), t, &row(1)), t, i);
    // A rolled-back delete: the entry was never touched, the row is back.
    s.delete(TxnId(4), t, b).unwrap();
    s.rollback(TxnId(4)).unwrap();
    assert_eq!(seek_ids(&s, &snap(5, &[]), i, &point(&[Some(1)])), vec![b]);
    assert_eq!(entry_count(&s, i), 1);
}

#[test]
fn rollback_to_savepoint_removes_entries_of_later_writes_only() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    let sp = s.savepoint(TxnId(1)).unwrap();
    s.insert(TxnId(1), t, &row(2)).unwrap();
    s.update(TxnId(1), t, a, &row(3)).unwrap();
    assert_eq!(entry_count(&s, i), 3);
    s.rollback_to(TxnId(1), sp).unwrap();
    assert_eq!(entry_count(&s, i), 1);
    let own = snap(1, &[]);
    assert_eq!(seek_ids(&s, &own, i, &KeyRange::Full), vec![a]);
    assert_eq!(seek_ids(&s, &own, i, &point(&[Some(1)])), vec![a]);
    assert!(seek_ids(&s, &own, i, &point(&[Some(3)])).is_empty());
    s.insert(TxnId(1), t, &row(2)).unwrap();
    s.insert(TxnId(1), t, &row(3)).unwrap();
    assert_duplicate(s.insert(TxnId(1), t, &row(1)), t, i);
}

#[test]
fn rollback_ignores_entries_of_index_dropped_in_the_meantime() {
    let (s, t, i) = indexed_table(1, &[(0, false)], false);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.insert(TxnId(2), t, &row(2)).unwrap();
    s.update(TxnId(2), t, a, &row(3)).unwrap();
    s.drop_index(i).unwrap();
    s.rollback(TxnId(2)).unwrap();
    assert_eq!(collect(&s, &snap(3, &[]), t), vec![(a, row(1))]);
}

#[test]
fn vacuum_removes_index_entries_of_discarded_versions() {
    let (s, t, i) = indexed_table(1, &[(0, false)], true);
    let a = s.insert(TxnId(1), t, &row(1)).unwrap();
    s.commit(TxnId(1)).unwrap();
    s.update(TxnId(2), t, a, &row(2)).unwrap();
    s.commit(TxnId(2)).unwrap();
    let b = s.insert(TxnId(3), t, &row(3)).unwrap();
    s.delete(TxnId(3), t, b).unwrap();
    s.commit(TxnId(3)).unwrap();
    assert_eq!(entry_count(&s, i), 3);
    // Horizon 3: the version replaced by 2 goes, the one deleted by 3 stays.
    s.vacuum(TxnId(3)).unwrap();
    assert_eq!(entry_count(&s, i), 2);
    s.vacuum(TxnId(4)).unwrap();
    assert_eq!(entry_count(&s, i), 1);
    let sn = snap(4, &[]);
    assert_eq!(seek_ids(&s, &sn, i, &KeyRange::Full), vec![a]);
    assert!(seek_ids(&s, &sn, i, &point(&[Some(1)])).is_empty());
    s.insert(TxnId(4), t, &row(1)).unwrap();
    s.insert(TxnId(4), t, &row(3)).unwrap();
    assert_duplicate(s.insert(TxnId(4), t, &row(2)), t, i);
    // Vacuuming again changes nothing.
    s.vacuum(TxnId(4)).unwrap();
    assert_eq!(entry_count(&s, i), 3);
}

// ---------------------------------------------------------- Clustered key

#[test]
fn scan_follows_clustered_key_order() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let clustered = TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true),
        ],
        clustered_key: Some(vec![col(1, false), col(0, true)]),
    };
    let t = s.create_table(db, &clustered).unwrap();
    let r = |n: Option<i32>, s: Option<&str>| {
        Row(vec![
            n.map_or(Value::Null, Value::I32),
            s.map_or(Value::Null, text),
        ])
    };
    let ids = insert_all(
        &s,
        1,
        t,
        &[
            r(Some(1), Some("b")),
            r(Some(2), Some("a")),
            r(Some(3), Some("B")),
            r(None, Some("a")),
            r(Some(1), None),
            r(Some(1), Some("b")),
        ],
    );
    s.commit(TxnId(1)).unwrap();
    // Column 1 ascending (`NULL` first, case-insensitive), then column 0 descending (`NULL`
    // last), then `RowId` for equal keys.
    let expected: Vec<RowId> = [4, 1, 3, 2, 0, 5].iter().map(|&k| ids[k]).collect();
    let scanned: Vec<RowId> = collect(&s, &snap(2, &[]), t)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(scanned, expected);
    // No uniqueness is imposed by the clustered key, and an update follows the new key.
    s.update(TxnId(2), t, ids[4], &r(Some(9), Some("z")))
        .unwrap();
    let scanned: Vec<RowId> = collect(&s, &snap(2, &[]), t)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        scanned,
        [1, 3, 2, 0, 5, 4]
            .iter()
            .map(|&k| ids[k])
            .collect::<Vec<_>>()
    );
    // A table without clustered key keeps the `RowId` order.
    let u = s.create_table(db, &shape(1)).unwrap();
    let ids = insert_all(&s, 3, u, &[row(3), row(1), row(2)]);
    let scanned: Vec<RowId> = collect(&s, &snap(3, &[]), u)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(scanned, ids);
}

#[test]
fn index_on_clustered_key_columns_is_an_ordinary_index() {
    let s = MemoryStorage::new();
    let db = s.create_database("db").unwrap();
    let clustered = TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, false)],
        clustered_key: Some(vec![col(0, false)]),
    };
    let t = s.create_table(db, &clustered).unwrap();
    let pk = s.create_index(t, &index(&[(0, false)], true)).unwrap();
    let ids = insert_all(&s, 1, t, &[row(2), row(1)]);
    assert_duplicate(s.insert(TxnId(1), t, &row(2)), t, pk);
    let sn = snap(1, &[]);
    assert_eq!(seek_ids(&s, &sn, pk, &KeyRange::Full), vec![ids[1], ids[0]]);
    let scanned: Vec<RowId> = collect(&s, &sn, t).into_iter().map(|(id, _)| id).collect();
    assert_eq!(scanned, vec![ids[1], ids[0]]);
}
