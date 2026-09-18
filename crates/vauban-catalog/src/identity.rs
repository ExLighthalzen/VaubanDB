//! `IDENTITY` counters: the short autonomous transaction behind
//! [`Catalog::next_identity`](crate::Catalog::next_identity).
//!
//! # Why a transaction of its own
//!
//! An `IDENTITY` value handed out is consumed: a rolled-back `INSERT` leaves a hole in the
//! sequence. On `dbo.t (i int IDENTITY(10,2))`: two inserts give 10 then 12, a third insert
//! rolled back with `ROLLBACK TRAN`, and the insert after it gives 16 — the 14 of the
//! rolled-back statement does not come back. `storage` holds versioned rows and
//! nothing else, so the counter is a row like any other and what keeps it out of the
//! caller's transaction is the transaction it is written in: [`next_identity`] opens one of
//! its own, writes the row, commits, and gives the value back. The caller's [`TxnHandle`] is
//! not the one that carries the write (unit test `caller_rollback_does_not_rewind_identity`).
//!
//! # Where the counter lives
//!
//! In one table of `master`, [`COUNTERS_TABLE`], one row per table that has been asked for a
//! value, plus the marker row described below. It is created at the first call rather than
//! by the bootstrap, so that it stays outside the list of internal tables of `views/mod.rs`.
//!
//! `storage` keeps the shape of a table and not its name ([`TableShape`]), and this table is
//! outside the position-based list
//! [`internal_table_id`](crate::bootstrap::internal_table_id) walks, so it carries its own
//! name: a table of `master` is the counter table when its shape is [`counters_shape`]
//! **and** a visible row holds [`COUNTERS_TABLE`] in its first column. The marker row is
//! written with the table so that the lookup has something to read before the first counter
//! row (unit tests `the_counters_live_in_one_table_of_master` and
//! `a_user_table_of_the_same_shape_is_not_the_counter_table`).
//!
//! Bounds of that scheme: a row stays behind when its table
//! is dropped; a first call whose transaction fails leaves a table of that shape without its
//! marker row, which the next call skips and replaces; and the key of a row is an
//! [`ObjectId`], which a restart does not hand out again (`table.rs`,
//! `TableStore::raise_counter_above`), so a counter is read back within the life of one
//! catalogue.
//!
//! # Serialisation
//!
//! The mutex of the [`Catalog`] — the one `table.rs` holds the [`TableStore`] behind — is
//! taken for the whole call, so two callers are served one after the other and get two
//! values (unit tests `two_sequential_calls_are_distinct` and
//! `four_threads_share_one_sequence`). The metadata is read through it anyway
//! ([`TableStore::get`]), so the same guard covers the read of the [`IdentitySpec`] and the
//! write of the counter.

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{DbId, Row, TableId, TableShape};
use vauban_txn::{IsolationLevel, TxnHandle};
use vauban_types::{Decimal, Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::SYSTEM_DATABASES;
use crate::catalog::Catalog;
use crate::ids::ObjectId;
use crate::meta::IdentitySpec;
use crate::table::{self, TableStore};

/// Name of the table of the counters, written in the first column of each of its rows.
///
/// A `vauban_sys_*` name of our own, like the internal tables of `bootstrap.rs`: it is not a
/// table SQL Server holds. No view is built over this one.
pub(crate) const COUNTERS_TABLE: &str = "vauban_sys_identity_counters";

/// Where each column of [`COUNTERS_TABLE`] sits in a [`Row`], as `bootstrap.rs` does for the
/// tables it declares.
mod counters_columns {
    /// `name nvarchar(128)`: [`super::COUNTERS_TABLE`], the marker the lookup reads.
    pub(super) const NAME: usize = 0;
    /// `object_id int`: the table the counter belongs to.
    pub(super) const OBJECT_ID: usize = 1;
    /// `last_value bigint`: the last value handed out for that table.
    pub(super) const LAST_VALUE: usize = 2;
    /// Width of a row of the table.
    pub(super) const WIDTH: usize = 3;
}

/// The `object_id` of the marker row, which belongs to no table.
///
/// `0` is not an identifier [`TableStore`] hands out: it counts from `FIRST_USER_OBJECT_ID`,
/// which is 1 000 000 (`table.rs`). A call for `ObjectId(0)` stops on the unknown-table
/// error before any row is read (unit test `an_object_id_no_table_carries_is_a_bug`).
const MARKER_OBJECT_ID: i32 = 0;

/// Hands out the next `IDENTITY` value of `table`. See
/// [`Catalog::next_identity`](crate::Catalog::next_identity).
///
/// The steps: take the mutex of the catalogue, read the [`IdentitySpec`] of the identity
/// column of `table`, open a transaction of the catalogue's own, read and write the counter
/// row in it, commit it, and give the value back as a [`Decimal`].
///
/// `caller` names the transaction the value is for. It is not written to: the value stays
/// consumed after a `commit` and after a `rollback` of it (unit test
/// `caller_rollback_does_not_rewind_identity`). The metadata is read from the in-memory
/// [`TableStore`] and not from versioned rows, so no snapshot of `caller` is taken either.
///
/// # Errors
///
/// [`InternalError::Bug`] when `table` is no table of this catalogue, and when the table
/// carries no `IDENTITY` column: neither is a shape a client can send. `IDENT_CURRENT` on a
/// table without an identity column answers `NULL` rather than an error, and the caller of
/// this function is the `INSERT`, which reads the [`IdentitySpec`] before calling.
/// The two messages are frozen by `a_table_without_an_identity_column_is_a_bug` and
/// `an_object_id_no_table_carries_is_a_bug`.
///
/// [`InternalError::Bug`] as well when the counter passes `i64::MAX`: the range of the column
/// is narrower and its enforcement — error 8115 — belongs to the `INSERT`
/// (`an_i64_overflow_is_a_bug`).
///
/// The error of a `storage` or `txn` call otherwise.
pub(crate) fn next_identity(
    catalog: &Catalog,
    caller: &TxnHandle,
    table: ObjectId,
) -> SqlResult<Decimal> {
    let _ = caller;
    let mut store = table::store(catalog);
    table::refresh(catalog, &mut store)?;
    let spec = identity_spec(&store, table)?;
    let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
    let value = match hand_out(catalog, &handle, table, spec) {
        Ok(value) => {
            catalog.txn.commit(handle)?;
            value
        }
        Err(err) => {
            catalog.txn.rollback(handle)?;
            return Err(err);
        }
    };
    drop(store);
    Ok(decimal(value))
}

/// The last identity value handed out for `table`, for `IDENT_CURRENT`.
///
/// `None` when `table` is unknown to this catalogue or carries no `IDENTITY` column. When
/// no value was handed out yet, answers the seed of the column — after
/// `CREATE TABLE dbo.t (id int IDENTITY(1,1))` with no `INSERT` yet,
/// `SELECT IDENT_CURRENT('dbo.t')` is `1` (`tests/session_variables.rs`,
/// `ident_current_before_any_insert_returns_seed`).
///
/// # Errors
///
/// The error of a `storage` or `txn` call otherwise.
pub(crate) fn identity_current(
    catalog: &Catalog,
    caller: &TxnHandle,
    table: ObjectId,
) -> SqlResult<Option<Decimal>> {
    let mut store = table::store(catalog);
    table::refresh(catalog, &mut store)?;
    let Some(meta) = store.get(table) else {
        return Ok(None);
    };
    let Some(spec) = meta.columns.iter().find_map(|column| column.identity) else {
        return Ok(None);
    };
    let snapshot = catalog.txn.statement_snapshot(caller);
    let last = read_last_value(catalog, &snapshot, table)?;
    Ok(Some(decimal(last.unwrap_or(spec.seed))))
}

/// The [`IdentitySpec`] of the identity column of `table`.
///
/// The first identity column of the table: `create_table` refuses a second one (`table.rs`,
/// `two_identity_columns_are_a_bug`), so there is at most one.
///
/// # Errors
///
/// [`InternalError::Bug`] for an unknown table and for a table with no identity column, as
/// [`next_identity`] describes.
fn identity_spec(store: &TableStore, table: ObjectId) -> SqlResult<IdentitySpec> {
    let Some(meta) = store.get(table) else {
        return Err(bug(&format!(
            "no table of object id {table} in this catalogue"
        )));
    };
    meta.columns
        .iter()
        .find_map(|column| column.identity)
        .ok_or_else(|| {
            bug(&format!(
                "table {}.{} (object id {table}) has no IDENTITY column",
                meta.schema, meta.name
            ))
        })
}

/// The `last_value` stored for `table` in [`COUNTERS_TABLE`], if the table and a row exist.
fn read_last_value(
    catalog: &Catalog,
    snapshot: &vauban_storage::Snapshot,
    table: ObjectId,
) -> SqlResult<Option<i64>> {
    let master = master(catalog)?;
    let shape = counters_shape();
    let mut counters_id = None;
    for (id, stored) in catalog.storage.tables(master)? {
        if stored != shape {
            continue;
        }
        for row in catalog.storage.scan(snapshot, id)? {
            let (_, values) = row?;
            if values.0.get(counters_columns::NAME)
                == Some(&Value::String(SqlString {
                    text: COUNTERS_TABLE.to_owned(),
                }))
            {
                counters_id = Some(id);
                break;
            }
        }
        if counters_id.is_some() {
            break;
        }
    }
    let Some(counters) = counters_id else {
        return Ok(None);
    };
    for row in catalog.storage.scan(snapshot, counters)? {
        let (_, values) = row?;
        if values.0.get(counters_columns::OBJECT_ID) != Some(&Value::I32(table.0)) {
            continue;
        }
        let Some(&Value::I64(last)) = values.0.get(counters_columns::LAST_VALUE) else {
            return Err(bug(&format!(
                "{COUNTERS_TABLE} holds a row whose last_value is not a bigint"
            )));
        };
        return Ok(Some(last));
    }
    Ok(None)
}

/// Reads the counter row of `table` inside `handle`, writes the next value back and returns
/// it.
///
/// A table asked for the first time has no row: the value handed out is
/// [`IdentitySpec::seed`] and the row is inserted with it (unit test
/// `first_identity_is_the_seed`). Afterwards the value is the stored one plus
/// [`IdentitySpec::increment`] (`second_call_adds_the_increment`).
///
/// # Errors
///
/// [`InternalError::Bug`] on an `i64` overflow and on a counter row whose `last_value` is not
/// a `bigint`; the error of a `storage` call otherwise.
fn hand_out(
    catalog: &Catalog,
    handle: &TxnHandle,
    table: ObjectId,
    spec: IdentitySpec,
) -> SqlResult<i64> {
    let counters = counters_table(catalog, handle)?;
    let snapshot = catalog.txn.statement_snapshot(handle);
    let mut current = None;
    for row in catalog.storage.scan(&snapshot, counters)? {
        let (id, values) = row?;
        if values.0.get(counters_columns::OBJECT_ID) != Some(&Value::I32(table.0)) {
            continue;
        }
        let Some(&Value::I64(last)) = values.0.get(counters_columns::LAST_VALUE) else {
            return Err(bug(&format!(
                "{COUNTERS_TABLE} holds a row whose last_value is not a bigint"
            )));
        };
        current = Some((id, last));
        break;
    }
    let Some((row, last)) = current else {
        catalog
            .storage
            .insert(handle.id, counters, &counter_row(table, spec.seed))?;
        return Ok(spec.seed);
    };
    let next = last.checked_add(spec.increment).ok_or_else(|| {
        bug(&format!(
            "the identity counter of object id {table} passed the range of a bigint"
        ))
    })?;
    catalog
        .storage
        .update(handle.id, counters, row, &counter_row(table, next))?;
    Ok(next)
}

/// The [`TableId`] of [`COUNTERS_TABLE`] in `master`, created with its marker row when the
/// lookup below comes back empty.
///
/// How the table is recognised, and what that does not cover: module documentation. The
/// marker row is inserted inside `handle`, the transaction [`next_identity`] commits; a
/// table created here whose marker row does not reach the storage is dropped again before
/// the error goes back to the caller.
///
/// # Errors
///
/// [`InternalError::Bug`] when the storage holds no `master`, which means the catalogue was
/// not bootstrapped; the error of a `storage` call otherwise.
fn counters_table(catalog: &Catalog, handle: &TxnHandle) -> SqlResult<TableId> {
    let master = master(catalog)?;
    let shape = counters_shape();
    let snapshot = catalog.txn.statement_snapshot(handle);
    for (id, stored) in catalog.storage.tables(master)? {
        if stored != shape {
            continue;
        }
        for row in catalog.storage.scan(&snapshot, id)? {
            let (_, values) = row?;
            if let Some(Value::String(name)) = values.0.get(counters_columns::NAME)
                && name.text == COUNTERS_TABLE
            {
                return Ok(id);
            }
        }
    }
    let id = catalog.storage.create_table(master, &shape)?;
    let marker = counter_row(ObjectId(MARKER_OBJECT_ID), 0);
    if let Err(err) = catalog.storage.insert(handle.id, id, &marker) {
        catalog.storage.drop_table(id)?;
        return Err(err);
    }
    Ok(id)
}

/// The shape of [`COUNTERS_TABLE`]: `name nvarchar(128)`, `object_id int`,
/// `last_value bigint`, a heap.
///
/// `last_value` is a `bigint` and not an `int`: `bigint` is the widest integer an `IDENTITY`
/// column takes, the one [`IdentitySpec`] holds its seed and its increment in (`meta.rs`).
fn counters_shape() -> TableShape {
    TableShape {
        columns: vec![
            TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
            TypeInfo::new(SqlType::Int, false),
            TypeInfo::new(SqlType::BigInt, false),
        ],
        clustered_key: None,
    }
}

/// The row of [`COUNTERS_TABLE`] that says `table` was last handed `value`.
fn counter_row(table: ObjectId, value: i64) -> Row {
    let mut row = vec![Value::Null; counters_columns::WIDTH];
    row[counters_columns::NAME] = Value::String(SqlString {
        text: COUNTERS_TABLE.to_owned(),
    });
    row[counters_columns::OBJECT_ID] = Value::I32(table.0);
    row[counters_columns::LAST_VALUE] = Value::I64(value);
    Row(row)
}

/// The [`DbId`] of `master`, where the internal tables live.
///
/// The name is compared without regard to case, as `bootstrap.rs` compares database names.
///
/// # Errors
///
/// [`InternalError::Bug`] when no database of that name is in the storage.
fn master(catalog: &Catalog) -> SqlResult<DbId> {
    catalog
        .storage
        .databases()?
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(SYSTEM_DATABASES[0]))
        .map(|(id, _)| id)
        .ok_or_else(|| bug("this storage holds no master database"))
}

/// The value as the caller reads it: `numeric(38, 0)`.
///
/// `SQL_VARIANT_PROPERTY(@@IDENTITY, 'BaseType')` is `numeric`, `'Precision'` 38 and
/// `'Scale'` 0 after an insert into a table with an `int IDENTITY` column — the width of the
/// column is not the width of the value the server hands back (unit test
/// `the_value_is_a_numeric_38_0`).
fn decimal(value: i64) -> Decimal {
    Decimal {
        mantissa: i128::from(value),
        precision: 38,
        scale: 0,
    }
}

/// The internal error this file answers with, prefixed with the name of the method.
fn bug(message: &str) -> SqlError {
    InternalError::Bug(format!("next_identity: {message}")).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::TransactionManager;

    use crate::def::{ColumnDef, TableDef};
    use crate::meta::QualifiedName;

    /// A bootstrapped catalogue over a fresh `MemoryStorage`, with its transaction manager.
    fn instance() -> (Catalog, Arc<TransactionManager>) {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog =
            Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
        (catalog, txn)
    }

    /// An open transaction at `READ COMMITTED`.
    fn begin(txn: &Arc<TransactionManager>) -> TxnHandle {
        txn.begin(IsolationLevel::ReadCommitted)
    }

    /// The description of `master.dbo.<name>` with the columns given.
    fn table_def(name: &str, columns: Vec<ColumnDef>) -> TableDef {
        TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            columns,
            constraints: Vec::new(),
        }
    }

    /// A column with its type, an optional `IDENTITY` and nothing else.
    fn column(name: &str, ty: SqlType, identity: Option<IdentitySpec>) -> ColumnDef {
        ColumnDef {
            name: name.to_owned(),
            ty: TypeInfo::new(ty, identity.is_none()),
            default: None,
            identity,
            computed: None,
        }
    }

    /// `master.dbo.<name> (i int IDENTITY(seed, increment) NOT NULL, a int NULL)`, created
    /// and committed.
    fn identity_table(
        catalog: &Catalog,
        txn: &Arc<TransactionManager>,
        name: &str,
        seed: i64,
        increment: i64,
    ) -> ObjectId {
        let def = table_def(
            name,
            vec![
                column("i", SqlType::Int, Some(IdentitySpec { seed, increment })),
                column("a", SqlType::Int, None),
            ],
        );
        create(catalog, txn, &def)
    }

    /// `master.dbo.<name> (a int NULL)`, created and committed, with no identity column.
    fn plain_table(catalog: &Catalog, txn: &Arc<TransactionManager>, name: &str) -> ObjectId {
        let def = table_def(name, vec![column("a", SqlType::Int, None)]);
        create(catalog, txn, &def)
    }

    /// Creates the table in a transaction of its own and commits it.
    fn create(catalog: &Catalog, txn: &Arc<TransactionManager>, def: &TableDef) -> ObjectId {
        let handle = begin(txn);
        let meta = catalog.create_table(&handle, def).expect("create_table");
        txn.commit(handle).expect("commit");
        meta.id
    }

    /// The value [`next_identity`] hands out, as an `i128`.
    fn next(catalog: &Catalog, handle: &TxnHandle, table: ObjectId) -> i128 {
        catalog
            .next_identity(handle, table)
            .expect("next_identity")
            .mantissa
    }

    /// The number of tables `master` holds.
    fn tables_of_master(catalog: &Catalog) -> usize {
        let master = master(catalog).expect("master");
        catalog.storage.tables(master).expect("tables()").len()
    }

    #[test]
    fn first_identity_is_the_seed() {
        let (catalog, txn) = instance();
        let table = identity_table(&catalog, &txn, "t", 10, 2);
        let handle = begin(&txn);
        assert_eq!(next(&catalog, &handle, table), 10);
    }

    #[test]
    fn second_call_adds_the_increment() {
        let (catalog, txn) = instance();
        let table = identity_table(&catalog, &txn, "t", 10, 2);
        let handle = begin(&txn);
        assert_eq!(next(&catalog, &handle, table), 10);
        assert_eq!(next(&catalog, &handle, table), 12);
    }

    #[test]
    fn the_value_is_a_numeric_38_0() {
        let (catalog, txn) = instance();
        let table = identity_table(&catalog, &txn, "t", 1, 1);
        let handle = begin(&txn);
        assert_eq!(
            catalog.next_identity(&handle, table).expect("next"),
            Decimal {
                mantissa: 1,
                precision: 38,
                scale: 0,
            }
        );
    }

    #[test]
    fn caller_rollback_does_not_rewind_identity() {
        let (catalog, txn) = instance();
        let table = identity_table(&catalog, &txn, "t", 10, 2);
        let first = begin(&txn);
        assert_eq!(next(&catalog, &first, table), 10);
        txn.rollback(first).expect("rollback");
        let second = begin(&txn);
        assert_eq!(next(&catalog, &second, table), 12);
        txn.commit(second).expect("commit");
    }

    #[test]
    fn two_sequential_calls_are_distinct() {
        let (catalog, txn) = instance();
        let table = identity_table(&catalog, &txn, "t", 1, 1);
        let first = begin(&txn);
        let second = begin(&txn);
        let (a, b) = (
            next(&catalog, &first, table),
            next(&catalog, &second, table),
        );
        assert_ne!(a, b);
        assert_eq!((a, b), (1, 2));
    }

    #[test]
    fn four_threads_share_one_sequence() {
        const THREADS: i128 = 4;
        const CALLS: i128 = 25;
        let (catalog, txn) = instance();
        let table = identity_table(&catalog, &txn, "t", 1, 1);
        let catalog = &catalog;
        let txn = &txn;
        let mut handed: Vec<i128> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(move || {
                        let handle = begin(txn);
                        (0..CALLS)
                            .map(|_| next(catalog, &handle, table))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            threads
                .into_iter()
                .flat_map(|thread| thread.join().expect("thread"))
                .collect()
        });
        handed.sort_unstable();
        assert_eq!(handed, (1..=THREADS * CALLS).collect::<Vec<_>>());
    }

    #[test]
    fn two_tables_have_their_own_counter() {
        let (catalog, txn) = instance();
        let left = identity_table(&catalog, &txn, "left", 10, 2);
        let right = identity_table(&catalog, &txn, "right", 100, 5);
        let handle = begin(&txn);
        assert_eq!(next(&catalog, &handle, left), 10);
        assert_eq!(next(&catalog, &handle, right), 100);
        assert_eq!(next(&catalog, &handle, left), 12);
        assert_eq!(next(&catalog, &handle, right), 105);
    }

    #[test]
    fn the_counters_live_in_one_table_of_master() {
        let (catalog, txn) = instance();
        let left = identity_table(&catalog, &txn, "left", 1, 1);
        let right = identity_table(&catalog, &txn, "right", 1, 1);
        let before = tables_of_master(&catalog);
        let handle = begin(&txn);
        for _ in 0..3 {
            next(&catalog, &handle, left);
            next(&catalog, &handle, right);
        }
        assert_eq!(tables_of_master(&catalog), before + 1);
    }

    #[test]
    fn a_user_table_of_the_same_shape_is_not_the_counter_table() {
        let (catalog, txn) = instance();
        // A table of `master` with the shape of the counters, holding a row that is not the
        // marker: the marker is what tells the two apart.
        let master_db = master(&catalog).expect("master");
        let decoy = catalog
            .storage
            .create_table(master_db, &counters_shape())
            .expect("create_table");
        let writing = begin(&txn);
        let mut row = counter_row(ObjectId(1_000_000), 41);
        row.0[counters_columns::NAME] = Value::String(SqlString {
            text: "not the counters".to_owned(),
        });
        catalog
            .storage
            .insert(writing.id, decoy, &row)
            .expect("insert");
        txn.commit(writing).expect("commit");

        let table = identity_table(&catalog, &txn, "t", 7, 1);
        let handle = begin(&txn);
        assert_eq!(next(&catalog, &handle, table), 7);
        assert_ne!(counters_table(&catalog, &handle).expect("counters"), decoy);
    }

    #[test]
    fn a_table_without_an_identity_column_is_a_bug() {
        let (catalog, txn) = instance();
        let table = plain_table(&catalog, &txn, "t");
        let handle = begin(&txn);
        let err = catalog
            .next_identity(&handle, table)
            .expect_err("no identity column");
        assert_eq!(
            err.message,
            format!(
                "Internal error: internal bug: next_identity: table dbo.t (object id {table}) \
                 has no IDENTITY column"
            )
        );
    }

    #[test]
    fn an_object_id_no_table_carries_is_a_bug() {
        let (catalog, txn) = instance();
        let handle = begin(&txn);
        for id in [ObjectId(MARKER_OBJECT_ID), ObjectId(-1), ObjectId(i32::MAX)] {
            let err = catalog.next_identity(&handle, id).expect_err("unknown");
            assert_eq!(
                err.message,
                format!(
                    "Internal error: internal bug: next_identity: no table of object id {id} in \
                     this catalogue"
                )
            );
        }
    }

    #[test]
    fn a_dropped_table_is_unknown_again() {
        let (catalog, txn) = instance();
        let table = identity_table(&catalog, &txn, "t", 1, 1);
        let handle = begin(&txn);
        assert_eq!(next(&catalog, &handle, table), 1);
        catalog.drop_table(&handle, table).expect("drop_table");
        txn.commit(handle).expect("commit");
        let after = begin(&txn);
        let err = catalog.next_identity(&after, table).expect_err("dropped");
        assert!(err.message.contains("no table of object id"), "{err:?}");
    }

    #[test]
    fn an_i64_overflow_is_a_bug() {
        let (catalog, txn) = instance();
        let table = identity_table(&catalog, &txn, "t", i64::MAX, 1);
        let handle = begin(&txn);
        assert_eq!(next(&catalog, &handle, table), i128::from(i64::MAX));
        let err = catalog
            .next_identity(&handle, table)
            .expect_err("overflow of the counter");
        assert_eq!(
            err.message,
            format!(
                "Internal error: internal bug: next_identity: the identity counter of object id \
                 {table} passed the range of a bigint"
            )
        );
    }
}
