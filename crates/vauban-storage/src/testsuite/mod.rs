//! Generic contract suite of the [`Storage`](crate::Storage) trait, runnable on any
//! implementation. Behind the `testsuite` Cargo feature; never compiled into the engine.
//!
//! The suite is written against the **contract** of the trait (its rustdoc), not against
//! `MemoryStorage`: it only uses the public API of this crate, so the on-disk implementation
//! runs it unchanged. Each scenario starts from a fresh instance obtained from the
//! factory, creates its own database and tables, and shares nothing with the others.
//!
//! # How an implementation runs it
//!
//! In an integration test of the implementing crate, with the `testsuite` feature enabled:
//! `use vauban_storage::{storage_contract_suite, MemoryStorage};` then
//! `storage_contract_suite!(MemoryStorage::new());` — the expression is evaluated once per
//! scenario and must yield a fresh, empty storage (for disk: `DiskStorage::open(tmp, opts)
//! .expect("open")`). The macro expands to one `#[test]` function per scenario, named after
//! it, so `cargo test -- <name>` isolates one.
//!
//! # Scenarios
//!
//! - `databases_create_list_drop`: `create_database` returns fresh increasing ids, `databases`
//!   lists verbatim names sorted by id, `drop_database` removes the database and its tables,
//!   ids are never reused, a dropped id is a `Bug`.
//! - `table_create_drop_and_ids_not_reused`: `TableId`s are unique across the instance and
//!   never reused, `drop_table` removes rows and indexes, in-flight writes on a dropped table
//!   are ignored by `commit`/`rollback`.
//! - `tables_and_indexes_introspection`: `tables(db)` and `indexes(table)` return the shapes
//!   exactly as given, sorted by id, without the dropped objects, immediately (no MVCC).
//! - `insert_get_scan_visibility_before_and_after_commit`: an insert is visible to its
//!   writer at once, to others only once committed and settled by their snapshot; a snapshot
//!   listing the writer as active never sees it; `get` of an unknown row is `None`.
//! - `rollback_undoes_insert`: after `rollback` nothing of the transaction is visible to
//!   anyone, `latest_version` is `None`, `RowId`s are not reused, the transaction is finished.
//! - `update_versions_and_snapshot_isolation`: `update` keeps the `RowId`, the old version
//!   stays visible to snapshots that do not settle the writer, two updates by one
//!   transaction are allowed.
//! - `delete_visibility`: a delete hides the row from its writer at once, from others only
//!   once committed and settled; a rolled-back delete hides nothing.
//! - `own_insert_then_delete_invisible_to_self`: a row inserted then deleted by one
//!   transaction is invisible to it, before and after commit.
//! - `latest_version_semantics`: reports the most recent version whatever its status, the
//!   deleter as writer for a deleted row, `None` for unknown or vacuumed rows.
//! - `savepoint_rollback_to_partial_and_repeated`: `rollback_to` undoes only the writes made
//!   after the savepoint, can be repeated, and leaves the transaction in progress.
//! - `savepoint_invalidated_after_rollback_to`: savepoints taken after the target are
//!   invalid, foreign or made-up ones are a `Bug`, savepoints die with their transaction.
//! - `update_on_superseded_version_is_bug`: writing over a version pending, deleted or
//!   created by another in-progress transaction is a `Bug` that leaves no trace; the row
//!   becomes writable again once that transaction is settled or rolled back.
//! - `index_seek_point_between_full_both_directions`: `Point` (full key, prefix, empty),
//!   `Between` (included/excluded/unbounded, prefix bounds, inverted), `Full`, in both
//!   directions, ties by `RowId`.
//! - `index_nulls_first_ascending_last_descending`: `NULL` sorts first on an ascending
//!   column and last on a descending one, and is a key like any other for `seek`.
//! - `unique_index_violation_2601_mvcc_aware`: 2601 names the table and the index by their
//!   bare ids, sees uncommitted inserts and uncommitted deletes of other transactions,
//!   ignores rolled-back rows and committed deletes, leaves the row unchanged on `update`,
//!   and `create_index` refuses existing duplicates without creating anything.
//! - `unique_index_null_counts_as_value`: `(1, NULL)` twice is a duplicate.
//! - `unique_index_ignores_versions_superseded_by_writer`: an `update` keeping the key and a
//!   `delete` followed by an `insert` of the same key in the same transaction pass.
//! - `index_maintained_on_update_rollback_vacuum`: `seek` follows every write, undo and
//!   vacuum without any caller intervention, and `create_index` indexes existing rows.
//! - `clustered_key_orders_scan`: `scan` follows the clustered key (`NULL` first ascending,
//!   last descending, ties by `RowId`), an updated row moves, a table without clustered key
//!   yields every row once.
//! - `scan_and_seek_isolated_from_later_writes_of_other_txns`: an iterator created before
//!   another transaction writes and commits does not see those writes.
//! - `vacuum_removes_dead_versions_keeps_visible_ones`: versions deleted or replaced by a
//!   transaction below the horizon go, those a held snapshot can still see stay, aborted
//!   versions go, `RowId`s are not reused, vacuuming twice is harmless.
//! - `checkpoint_is_ok`: `Ok(())` at any time, changes what nobody sees.
//! - `commit_of_read_only_txn_is_ok_and_twice_is_bug`: a transaction that wrote nothing can
//!   commit or roll back once; any later use of its id is a `Bug`.
//! - `arity_and_unknown_id_preconditions_are_bug_errors`: every precondition violation of
//!   the 22 methods answers `Err(Bug)` and never panics or leaves a trace.
//! - `concurrent_writers_on_distinct_tables`: eight threads write and commit on their own
//!   table through a shared `&dyn Storage`; every table ends with 100 visible rows.
//! - `vacuum_during_pending_update_then_rollback_restores_committed_version`: a vacuum run
//!   while an update is pending removes only the versions below the horizon; the rollback of
//!   that update restores the committed version and the row becomes writable again.

mod cases;

use std::fmt::Debug;

use vauban_errors::SqlResult;
use vauban_types::{SqlType, TypeInfo, Value};

pub use cases::*;

use crate::{IndexId, IndexShape, KeyColumn, Row, RowId, RowIter, Snapshot, TableId, TableShape};

/// The snapshot of transaction `own` while the transactions of `active` are in progress and
/// every other earlier transaction is finished: `xmin` is the smallest of `own` and
/// `active`, `xmax` is `own + 1`, `active` is sorted.
///
/// `TxnId`s in the scenarios are small integers assigned in order of start, as the `txn`
/// module does.
pub fn snap(own: u64, active: &[u64]) -> Snapshot {
    let mut active: Vec<crate::TxnId> = active.iter().map(|&t| crate::TxnId(t)).collect();
    active.sort();
    let xmin = active.first().map_or(own, |t| t.0.min(own));
    Snapshot {
        xmin: crate::TxnId(xmin),
        xmax: crate::TxnId(own + 1),
        active,
        own: crate::TxnId(own),
    }
}

/// A table of `n_cols` nullable `int` columns and no clustered key.
pub fn int_table_shape(n_cols: usize) -> TableShape {
    TableShape {
        columns: (0..n_cols)
            .map(|_| TypeInfo::new(SqlType::Int, true))
            .collect(),
        clustered_key: None,
    }
}

/// A table of `n_cols` nullable `int` columns with a clustered key on the given
/// `(column, descending)` pairs.
pub fn clustered_int_table_shape(n_cols: usize, key: &[(u16, bool)]) -> TableShape {
    TableShape {
        columns: int_table_shape(n_cols).columns,
        clustered_key: Some(key_columns(key)),
    }
}

/// An index on the given `(column, descending)` pairs, without included column.
pub fn index_shape(columns: &[(u16, bool)], unique: bool) -> IndexShape {
    IndexShape {
        columns: key_columns(columns),
        unique,
        included: vec![],
    }
}

/// [`KeyColumn`]s from `(column, descending)` pairs.
pub fn key_columns(columns: &[(u16, bool)]) -> Vec<KeyColumn> {
    columns
        .iter()
        .map(|&(column, descending)| KeyColumn { column, descending })
        .collect()
}

/// A row of `int` values.
pub fn row(values: &[i32]) -> Row {
    Row(key(values))
}

/// A row of `int` values where `None` is `NULL`.
pub fn nullable_row(values: &[Option<i32>]) -> Row {
    Row(nullable_key(values))
}

/// A key (or key prefix) of `int` values, for [`crate::KeyRange`].
pub fn key(values: &[i32]) -> Vec<Value> {
    values.iter().map(|&v| Value::I32(v)).collect()
}

/// A key (or key prefix) of `int` values where `None` is `NULL`.
pub fn nullable_key(values: &[Option<i32>]) -> Vec<Value> {
    values
        .iter()
        .map(|v| v.map_or(Value::Null, Value::I32))
        .collect()
}

/// Drains an iterator returned by [`crate::Storage::scan`] or [`crate::Storage::seek`],
/// failing on the first `Err` item.
pub fn collect(iter: Box<dyn RowIter + '_>) -> Vec<(RowId, Row)> {
    iter.map(|item| item.expect("scan/seek yielded an error item"))
        .collect()
}

/// The `RowId`s of collected rows, in order.
pub fn row_ids(rows: &[(RowId, Row)]) -> Vec<RowId> {
    rows.iter().map(|(id, _)| *id).collect()
}

/// Asserts that `r` is the generic error produced by `InternalError::Bug`: number `50000`
/// and message `Internal error: internal bug: …` (mapping of the `errors` crate).
pub fn assert_bug<T: Debug>(r: SqlResult<T>) {
    match r {
        Ok(v) => panic!("expected an InternalError::Bug, got Ok({v:?})"),
        Err(err) => {
            assert_eq!(err.number, 50000, "expected a Bug, got: {}", err.message);
            assert!(
                err.message.starts_with("Internal error: internal bug: "),
                "expected a Bug, got: {}",
                err.message
            );
        }
    }
}

/// Asserts that `r` is the duplicate-key error 2601 naming `table` and `index` by their
/// bare decimal ids ("Duplicate keys" in the [`crate::Storage`] documentation).
pub fn assert_duplicate_key<T: Debug>(r: SqlResult<T>, table: TableId, index: IndexId) {
    match r {
        Ok(v) => panic!("expected error 2601, got Ok({v:?})"),
        Err(err) => {
            assert_eq!(err.number, 2601, "expected 2601, got: {}", err.message);
            assert!(
                err.message.contains(&format!("'{table}'"))
                    && err.message.contains(&format!("'{index}'")),
                "2601 must name table {table} and index {index}: {}",
                err.message
            );
        }
    }
}

/// Generates one `#[test]` per scenario of the contract suite, each calling the matching
/// `case_*` function of [`crate::testsuite`] with a factory built from `$make`.
///
/// `$make` is an expression evaluated once per scenario that yields a fresh, empty storage
/// (any type implementing [`crate::Storage`] and `'static`). See the module documentation.
#[macro_export]
macro_rules! storage_contract_suite {
    ($make:expr) => {
        $crate::storage_contract_suite!(@each $make;
            databases_create_list_drop => case_databases_create_list_drop,
            table_create_drop_and_ids_not_reused => case_table_create_drop_and_ids_not_reused,
            tables_and_indexes_introspection => case_tables_and_indexes_introspection,
            insert_get_scan_visibility_before_and_after_commit
                => case_insert_get_scan_visibility_before_and_after_commit,
            rollback_undoes_insert => case_rollback_undoes_insert,
            update_versions_and_snapshot_isolation => case_update_versions_and_snapshot_isolation,
            delete_visibility => case_delete_visibility,
            own_insert_then_delete_invisible_to_self => case_own_insert_then_delete_invisible_to_self,
            latest_version_semantics => case_latest_version_semantics,
            savepoint_rollback_to_partial_and_repeated => case_savepoint_rollback_to_partial_and_repeated,
            savepoint_invalidated_after_rollback_to => case_savepoint_invalidated_after_rollback_to,
            update_on_superseded_version_is_bug => case_update_on_superseded_version_is_bug,
            index_seek_point_between_full_both_directions
                => case_index_seek_point_between_full_both_directions,
            index_nulls_first_ascending_last_descending => case_index_nulls_first_ascending_last_descending,
            unique_index_violation_2601_mvcc_aware => case_unique_index_violation_2601_mvcc_aware,
            unique_index_null_counts_as_value => case_unique_index_null_counts_as_value,
            unique_index_ignores_versions_superseded_by_writer
                => case_unique_index_ignores_versions_superseded_by_writer,
            index_maintained_on_update_rollback_vacuum => case_index_maintained_on_update_rollback_vacuum,
            clustered_key_orders_scan => case_clustered_key_orders_scan,
            scan_and_seek_isolated_from_later_writes_of_other_txns
                => case_scan_and_seek_isolated_from_later_writes_of_other_txns,
            vacuum_removes_dead_versions_keeps_visible_ones
                => case_vacuum_removes_dead_versions_keeps_visible_ones,
            checkpoint_is_ok => case_checkpoint_is_ok,
            commit_of_read_only_txn_is_ok_and_twice_is_bug => case_commit_of_read_only_txn_is_ok_and_twice_is_bug,
            arity_and_unknown_id_preconditions_are_bug_errors
                => case_arity_and_unknown_id_preconditions_are_bug_errors,
            concurrent_writers_on_distinct_tables => case_concurrent_writers_on_distinct_tables,
            vacuum_during_pending_update_then_rollback_restores_committed_version
                => case_vacuum_during_pending_update_then_rollback_restores_committed_version,
        );
    };
    (@each $make:expr; $($test:ident => $case:ident),* $(,)?) => {
        $(
            #[test]
            fn $test() {
                $crate::testsuite::$case(&|| ::std::boxed::Box::new($make));
            }
        )*
    };
}
