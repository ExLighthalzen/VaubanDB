//! `CREATE TABLE` / `ALTER TABLE` / `DROP TABLE` in the catalogue.
//!
//! # What a `CREATE TABLE` does here
//!
//! 1. the [`TableDef`] becomes a [`TableShape`]: the [`TypeInfo`] of each column, in the
//!    order they were declared, and the clustered key its `PRIMARY KEY` or its
//!    `UNIQUE CLUSTERED` constraint asks for, which `index.rs` reads from the definition
//!    before the table exists ([`crate::index::table_keys`]); `None` for a
//!    definition that carries neither;
//! 2. `Storage::create_table` creates the table, which exists at once, outside the
//!    transaction (documentation of the trait);
//! 3. `TransactionManager::register_on_rollback` records the [`RollbackAction::DropTable`]
//!    that undoes it, which is what makes the statement transactional at the SQL level
//!    (`tests/table.rs`, `create_table_rollback_drops_storage`);
//! 4. the catalogue hands out an [`ObjectId`], `index.rs` creates the index of each
//!    `PRIMARY KEY` and `UNIQUE` constraint ([`crate::index::apply_table_keys`]), and the
//!    [`TableMeta`] goes into [`TableStore`].
//!
//! A `DROP TABLE` is the mirror image: no `storage.drop_table` call, a
//! [`CommitAction::DropTable`] registered instead, so the table stays readable until the
//! `COMMIT` (`tests/table.rs`, `drop_table_deferred`).
//!
//! # Where the metadata sits
//!
//! In [`TableStore`], behind the `tables` field of [`Catalog`]; the internal tables of
//! `storage` that the `sys.objects` and `sys.columns` views read are written from it
//! (`sys_rows.rs`). What follows from that: the map is in memory and is not versioned, so
//! `storage` — which is versioned — stays the reference on the question "does this table
//! still exist" ([`refresh`]). What that costs is written where it bites: a `DROP` undone by
//! a `ROLLBACK TRANSACTION <savepoint>` keeps its mark ([`drop_table`]), and two names that
//! differ outside ASCII are two tables ([`TableStore::live_named`]).
//!
//! The read side is [`store`] plus [`TableStore::get`] and [`TableStore::live`], which is
//! what `snapshot.rs` and `identity.rs` use without reopening this file.
//!
//! # Identifiers
//!
//! [`ObjectId`] and [`TableId`] are two different numbers for one table: `alter_table`
//! rebuilds the storage table and switches [`TableMeta::storage_id`] while
//! [`TableMeta::id`] stays put. The catalogue hands out
//! its own, from [`FIRST_USER_OBJECT_ID`], one more at each table (`tests/table.rs`,
//! `the_object_id_is_not_the_table_id`).
//!
//! A second [`Catalog`] built over the same storage starts with an empty map, so its counter
//! is raised past the identifiers the first one handed out before it gives any
//! ([`TableStore::raise_counter_above`], `tests/table.rs`,
//! `a_second_catalogue_hands_out_other_object_ids`).
//!
//! An identifier freed by a `DROP` or by a rolled-back `CREATE` is not handed out again: the
//! counter moves forward, as `OBJECT_ID` does in SQL Server after a `DROP TABLE` or a rolled
//! back `CREATE TABLE` (`tests/table.rs`, `create_table_assigns_stable_object_id`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{MutexGuard, PoisonError};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{DbId, TableId, TableShape, TxnId};
use vauban_txn::{CommitAction, RollbackAction, TxnHandle};
use vauban_types::TypeInfo;

use crate::catalog::{Catalog, not_implemented};
use crate::def::{ConstraintDef, TableDef};
use crate::ids::{ColumnId, ObjectId};
use crate::meta::{AlterTable, ColumnMeta, TableMeta};

/// The [`ObjectId`] of the first table a catalogue creates.
///
/// The value is ours: a client reads an `object_id` through `sys.objects` and compares
/// identifiers with one another, it does not read a range into them. Starting the user
/// objects high leaves the small numbers to the system objects the files of `views/`
/// number, and keeps the identifier apart from the [`TableId`] of the same table on a
/// storage that has handed out fewer than a million of them (`tests/table.rs`,
/// `the_object_id_is_not_the_table_id`).
pub(crate) const FIRST_USER_OBJECT_ID: i32 = 1_000_000;

/// The tables the catalogue has created, by [`ObjectId`], and the counter that numbers them.
///
/// Held by the `tables` field of [`Catalog`]; `table.rs` owns the type and the accesses to
/// it. See the module documentation for why the rows are here rather than in an internal
/// table of `storage`.
#[derive(Debug)]
pub(crate) struct TableStore {
    /// Identifier the next [`create_table`] hands out.
    next_object_id: i32,
    /// One entry per table created through this catalogue and still in `storage`.
    entries: BTreeMap<ObjectId, TableEntry>,
    /// The indexes of those tables. `index.rs` owns the type and the accesses to it, as
    /// this file owns [`TableStore`]: the store sits here because the two are read
    /// together, a `DROP TABLE` taking the indexes of the table with it.
    pub(crate) indexes: crate::index::IndexStore,
    /// The `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints of those tables, as objects.
    /// `constraints.rs` owns the type and the accesses to it, for the reason written on
    /// `indexes`.
    pub(crate) constraints: crate::constraints::ConstraintStore,
}

impl Default for TableStore {
    /// An empty store whose first identifier is [`FIRST_USER_OBJECT_ID`].
    fn default() -> Self {
        TableStore {
            next_object_id: FIRST_USER_OBJECT_ID,
            entries: BTreeMap::new(),
            indexes: crate::index::IndexStore::default(),
            constraints: crate::constraints::ConstraintStore::default(),
        }
    }
}

impl TableStore {
    /// Hands out the next [`ObjectId`] and moves the counter forward.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] once the counter has reached `i32::MAX`, rather than wrapping
    /// round to an identifier already handed out: the last value a store gives is
    /// `i32::MAX - 1` (unit test `an_exhausted_counter_is_a_bug`).
    ///
    /// `pub(crate)` for `constraints.rs`, which numbers a constraint object from this
    /// counter so that a table and a constraint of one catalogue do not share an
    /// identifier (`tests/constraints.rs`, `named_constraints_become_objects`).
    pub(crate) fn take_object_id(&mut self) -> SqlResult<ObjectId> {
        let id = self.next_object_id;
        self.next_object_id = id.checked_add(1).ok_or_else(|| {
            InternalError::Bug("Catalog::create_table: object identifiers exhausted".to_owned())
        })?;
        Ok(ObjectId(id))
    }

    /// The entry of the table called `schema.name` in database `db` that no `DROP` has
    /// claimed, or `None`.
    ///
    /// Compared with `eq_ignore_ascii_case`: two names that differ on a letter outside ASCII
    /// are two tables here, where SQL Server answers 2714 state 6 (`CREATE TABLE dbo.Été`
    /// then `CREATE TABLE dbo.été`). Folding as `SQL_Latin1_General_CP1_CI_AS` does asks
    /// for the collation of the database, which the resolution of `snapshot.rs` carries;
    /// the state this comparison gives is frozen by `tests/table.rs`,
    /// `two_names_that_differ_outside_ascii_are_two_tables_here`.
    fn live_named(&self, db: DbId, schema: &str, name: &str) -> Option<&TableEntry> {
        self.entries.values().find(|entry| {
            entry.dropped_by.is_none()
                && entry.meta.database == db
                && entry.meta.schema.eq_ignore_ascii_case(schema)
                && entry.meta.name.eq_ignore_ascii_case(name)
        })
    }

    /// The metadata of the table of identifier `id`, `None` when this catalogue created no
    /// such table or when it has forgotten it ([`refresh`]).
    ///
    /// The read side of the store: `snapshot.rs` answers `CatalogSnapshot::table` with it
    /// and `identity.rs` reads the identity columns of a table through it, neither of them
    /// reopening this file (unit test `the_store_reads_back_what_create_table_wrote`).
    #[allow(dead_code)] // read by snapshot.rs and identity.rs
    pub(crate) fn get(&self, id: ObjectId) -> Option<&TableMeta> {
        self.entries.get(&id).map(|entry| &entry.meta)
    }

    /// The tables of this catalogue that no `DROP` has claimed, by increasing [`ObjectId`].
    ///
    /// Same readers as [`TableStore::get`] (unit test
    /// `the_store_lists_the_tables_that_no_drop_has_claimed`).
    #[allow(dead_code)] // read by snapshot.rs and identity.rs
    pub(crate) fn live(&self) -> impl Iterator<Item = &TableMeta> {
        self.entries
            .values()
            .filter(|entry| entry.dropped_by.is_none())
            .map(|entry| &entry.meta)
    }

    /// Moves the counter past the identifiers a previous catalogue on this storage may have
    /// handed out, `live` being the tables `storage` holds.
    ///
    /// A [`Catalog`] built a second time over the same storage starts with an empty map (the
    /// store is not reloaded), so its counter would start again at
    /// [`FIRST_USER_OBJECT_ID`] and give a new table the identifier of a table the first
    /// catalogue created. Each table costs one [`TableId`] and one [`ObjectId`], and a
    /// `TableId` is not reused, so keeping the counter above
    /// `FIRST_USER_OBJECT_ID + largest TableId` keeps the new identifiers clear of the old
    /// ones (`tests/table.rs`, `a_second_catalogue_hands_out_other_object_ids`).
    ///
    /// What this does not cover: an identifier whose table was dropped before the second
    /// catalogue started can be handed out again, since a dropped table leaves no `TableId`
    /// behind for the count to see.
    fn raise_counter_above(&mut self, live: &BTreeSet<TableId>) {
        let Some(largest) = live.iter().next_back() else {
            return;
        };
        let floor = i32::try_from(largest.0)
            .unwrap_or(i32::MAX)
            .saturating_add(FIRST_USER_OBJECT_ID)
            .saturating_add(1);
        self.next_object_id = self.next_object_id.max(floor);
    }
}

/// One table of a [`TableStore`].
#[derive(Debug)]
struct TableEntry {
    /// What [`create_table`] gave back to its caller.
    meta: TableMeta,
    /// The transaction that registered the deferred [`CommitAction::DropTable`], `None` when
    /// the table carries no pending `DROP`. Cleared by [`refresh`] when that transaction
    /// closed without the drop taking effect, which is what a rolled-back `DROP TABLE` looks
    /// like from here (`tests/table.rs`, `a_rolled_back_drop_leaves_the_table_droppable`).
    dropped_by: Option<TxnId>,
}

/// Creates a table. See [`Catalog::create_table`].
///
/// The steps are listed in the module documentation. The table is created in the database
/// named by `def.name`, with the columns of `def` in the order they were declared; the
/// `DEFAULT`, `IDENTITY` and computed clauses are stored on the [`ColumnMeta`] and are not
/// evaluated — `INSERT` reads them and `identity.rs` serves the identity counter.
///
/// # Errors
///
/// - 2714 when the database already holds a table of that schema and name (`tests/table.rs`,
///   `a_second_table_of_the_same_name_is_2714`);
/// - [`InternalError::Bug`] when `def` carries no column, more than one `IDENTITY` column —
///   2744 is raised before the catalogue, by the binder (`tests/table.rs`,
///   `two_identity_columns_are_a_bug`) — or a database name that resolves to nothing, name
///   resolution being the business of the binder and of `snapshot.rs`;
/// - what [`crate::index::table_keys`] and [`crate::index::apply_table_keys`] answer on the
///   `PRIMARY KEY` and `UNIQUE` constraints of `def`, and what
///   [`crate::constraints::apply_table_constraints`] answers on its `FOREIGN KEY`, `CHECK`
///   and `DEFAULT` constraints — 2714, 1769, 1776 and the numbers `constraints.rs` names
///   (`tests/constraints.rs`, `duplicate_constraint_name_is_2714`);
/// - the error of `storage` or of the transaction manager otherwise. When the compensation
///   cannot be registered, the table just created is dropped again before the error comes
///   back (`tests/table.rs`, `a_create_that_cannot_be_compensated_leaves_nothing`).
pub(crate) fn create_table(
    catalog: &Catalog,
    txn: &TxnHandle,
    def: &TableDef,
) -> SqlResult<TableMeta> {
    if def.columns.is_empty() {
        return Err(InternalError::Bug(format!(
            "Catalog::create_table: table {} has no column",
            def.name.name
        ))
        .into());
    }
    let identities = def
        .columns
        .iter()
        .filter(|column| column.identity.is_some())
        .count();
    if identities > 1 {
        return Err(InternalError::Bug(format!(
            "Catalog::create_table: table {} carries {identities} identity columns, at most \
             one is allowed; 2744 is raised before the catalogue by the binder",
            def.name.name
        ))
        .into());
    }
    let columns = column_metas(def)?;
    // Before `storage.create_table`: the clustered key is a field of the shape, and a
    // definition the catalogue refuses must leave nothing behind. `table_keys` owns the
    // `PRIMARY KEY` and `UNIQUE` constraints and answers a stub on the other three, which
    // `constraints.rs` stores: it reads the copy of `def` that carries the keys alone.
    let keys = crate::index::table_keys(&crate::constraints::keys_only(def), &columns)?;
    let db = database_of(catalog, &def.name.database)?;
    let mut store = store(catalog);
    refresh(catalog, &mut store)?;
    if store
        .live_named(db, &def.name.schema, &def.name.name)
        .is_some()
    {
        return Err(SqlError::object_already_exists(&def.name.name));
    }
    let shape = TableShape {
        columns: columns
            .iter()
            .map(|column| column.ty.clone())
            .collect::<Vec<TypeInfo>>(),
        clustered_key: keys.clustered_key(),
    };
    let storage_id = catalog.storage.create_table(db, &shape)?;
    if let Err(err) = catalog
        .txn
        .register_on_rollback(txn, RollbackAction::DropTable(storage_id))
    {
        catalog.storage.drop_table(storage_id)?;
        return Err(err);
    }
    let id = store.take_object_id()?;
    let mut meta = TableMeta {
        id,
        storage_id,
        database: db,
        schema: def.name.schema.clone(),
        name: def.name.name.clone(),
        columns,
        clustered: None,
        constraints: Vec::new(),
    };
    crate::index::apply_table_keys(catalog, txn, &mut store, &keys, &mut meta)?;
    // The single point where the `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints become
    // objects of the catalogue, after the keys so that a foreign key pointing at the table
    // being created finds its index (`constraints.rs`).
    crate::constraints::apply_table_constraints(&mut store, def, &mut meta)?;
    // A named `DEFAULT` constraint fills the default of its column when the column does
    // not carry one: the executor reads `ColumnMeta::default`, and the constraint keeps its
    // name in `meta.constraints` for the catalogue views.
    for constraint in &def.constraints {
        if let ConstraintDef::Default { column, expr, .. } = constraint
            && let Some(meta_column) = meta
                .columns
                .iter_mut()
                .find(|candidate| candidate.name.eq_ignore_ascii_case(column))
            && meta_column.default.is_none()
        {
            meta_column.default = Some(expr.clone());
        }
    }
    // The metadata is final here — the keys of `apply_table_keys` have filled
    // `meta.clustered` and `meta.constraints` — so this is where the rows the internal tables
    // carry about the table are written, inside `txn` (`sys_rows.rs`).
    crate::sys_rows::write(catalog, txn, &meta, &store.indexes)?;
    store.entries.insert(
        id,
        TableEntry {
            meta: meta.clone(),
            dropped_by: None,
        },
    );
    Ok(meta)
}

/// Changes the shape of a table. See [`Catalog::alter_table`].
///
/// # Errors
///
/// For now, on each call: `Catalog::alter_table not implemented`.
pub(crate) fn alter_table(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: ObjectId,
    change: &AlterTable,
) -> SqlResult<TableMeta> {
    let _ = (&catalog.storage, &catalog.txn, txn, table, change);
    Err(not_implemented("alter_table"))
}

/// Drops a table. See [`Catalog::drop_table`].
///
/// `storage.drop_table` is not called here: a [`CommitAction::DropTable`] is registered and
/// the transaction manager makes the call at `COMMIT`, so the rows stay readable until then
/// and a `ROLLBACK` leaves the table where it was (`tests/table.rs`, `drop_table_deferred`,
/// `a_rolled_back_drop_leaves_the_table_droppable`).
///
/// # Bound: a `DROP` undone by `ROLLBACK TRANSACTION <savepoint>`
///
/// `TransactionManager::rollback_to` forgets the [`CommitAction`] registered after the
/// savepoint, and the mark this function left on the entry stays: the catalogue goes on
/// believing the table is dropped while `storage` keeps it, so a second `DROP TABLE` of that
/// table answers 3701 and the `COMMIT` leaves the table in place. The state of that
/// sequence — `savepoint`, `drop_table`, `rollback_to`, `drop_table` — is frozen by
/// `tests/table.rs`, `a_savepoint_rollback_leaves_the_drop_mark_in_place`. Lifting the mark
/// asks for something the public API of `vauban_txn` does not give: whether an action
/// registered earlier is still in the log. `refresh` reads that indirectly for a whole
/// `ROLLBACK` (the transaction is closed) and cannot for a partial one; the fix needs
/// either a way to read or cancel a registered action, or versioned rows, which unwind with
/// the transaction.
///
/// # Errors
///
/// - 3701 when `table` names no table of this catalogue, or one a `DROP` of the same
///   transaction has already claimed (`tests/table.rs`, `dropping_an_unknown_table_is_3701`,
///   `dropping_the_same_table_twice_is_3701`). The catalogue holds an [`ObjectId`] and the
///   name that goes with it; when it holds neither, the identifier is printed in place of
///   the name, the client-facing name having been resolved before the call;
/// - 3726 when a `FOREIGN KEY` of another table points at `table`, the name printed with its
///   schema (`crate::constraints`; `tests/constraints.rs`, `drop_table_referenced_is_3726`);
/// - the error of `storage` or of the transaction manager otherwise.
pub(crate) fn drop_table(catalog: &Catalog, txn: &TxnHandle, table: ObjectId) -> SqlResult<()> {
    let mut store = store(catalog);
    refresh(catalog, &mut store)?;
    crate::constraints::forget_dropped(&mut store);
    let Some(entry) = store.entries.get(&table) else {
        return Err(SqlError::cannot_drop("drop", "table", &table.to_string()));
    };
    if entry.dropped_by.is_some() {
        return Err(SqlError::cannot_drop("drop", "table", &entry.meta.name));
    }
    let dropped = entry.meta.clone();
    // A `FOREIGN KEY` of another table points at this one: the `DROP` is refused with 3726
    // and accepted once that table is gone; accepted as well is the `DROP` of a table
    // whose own foreign key points at itself, which is why the key of `table` is left out
    // here (`constraints.rs`, module documentation; `tests/constraints.rs`,
    // `drop_table_referenced_is_3726`, `drop_referencing_table_first_succeeds`).
    let live: Vec<TableMeta> = store.live().cloned().collect();
    if crate::constraints::referencing_foreign_keys(&live, table)
        .iter()
        .any(|key| key.table != table)
    {
        return Err(SqlError::cannot_drop_referenced_object(&format!(
            "{}.{}",
            dropped.schema, dropped.name
        )));
    }
    catalog
        .txn
        .register_on_commit(txn, CommitAction::DropTable(dropped.storage_id))?;
    if let Some(entry) = store.entries.get_mut(&table) {
        entry.dropped_by = Some(txn.id);
    }
    // The rows of the internal tables go inside `txn`, not at the commit: they are versioned,
    // so the other transactions read them until the `COMMIT` and a `ROLLBACK` puts them back,
    // which is the deferral the `CommitAction` above gives the table itself
    // (`sys_rows.rs`).
    crate::sys_rows::remove(catalog, txn, &dropped, &store.indexes)?;
    Ok(())
}

/// The [`ColumnMeta`] of each column of `def`, in the order they were declared.
///
/// [`ColumnMeta::id`] is one-based, as `sys.columns.column_id` is, and
/// [`ColumnMeta::ordinal`] is the position of the column in the
/// [`Row`](vauban_storage::Row) of `storage`, from `0`, as `meta.rs` documents it: the first
/// column of a two-column table is `ColumnId(1)` at ordinal `0` (`tests/table.rs`,
/// `column_id_is_one_based_and_ordinal_is_the_row_position`). The one-based count is what
/// a client reads, the ordinal is what a reader of a row indexes with —
/// `binder/catalog_view.rs` does `usize::from(column.ordinal)`.
///
/// # Errors
///
/// [`InternalError::Bug`] when the column count does not fit in the `u16` of
/// [`ColumnMeta::ordinal`]. SQL Server refuses a table of more than 1024 columns with error
/// 1702, which is not this bound and is raised before the catalogue.
fn column_metas(def: &TableDef) -> SqlResult<Vec<ColumnMeta>> {
    let mut columns = Vec::with_capacity(def.columns.len());
    for (position, column) in def.columns.iter().enumerate() {
        let ordinal = u16::try_from(position).map_err(|_| {
            InternalError::Bug(format!(
                "Catalog::create_table: table {} has more columns than a u16 ordinal holds",
                def.name.name
            ))
        })?;
        columns.push(ColumnMeta {
            id: ColumnId(i32::from(ordinal) + 1),
            name: column.name.clone(),
            ty: column.ty.clone(),
            ordinal,
            default: column.default.clone(),
            identity: column.identity,
            computed: column.computed.clone(),
        });
    }
    Ok(columns)
}

/// The store of `catalog`, recovering from a poisoned lock instead of panicking: the
/// catalogue sits on the execution path of a query, where a panic is forbidden.
///
/// A reader outside this file — `snapshot.rs`, `identity.rs` — takes the guard here, calls
/// [`refresh`] and reads through [`TableStore::get`] or
/// [`TableStore::live`].
pub(crate) fn store(catalog: &Catalog) -> MutexGuard<'_, TableStore> {
    catalog
        .tables
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Brings the store back in line with `storage`, which is the reference on existence.
///
/// The map is in memory and is not versioned (module documentation), so two things it cannot
/// see by itself are read from `storage` and from the transaction manager:
///
/// - an entry whose table is gone from `storage` is forgotten. That covers a `CREATE` undone
///   by a `ROLLBACK` — the compensation dropped the table — and a `DROP` carried out by a
///   `COMMIT`. This is what frees the name for a later `CREATE TABLE` (`tests/table.rs`,
///   `a_name_freed_by_a_rollback_can_be_taken_again`);
/// - an entry marked dropped by a transaction that is no longer open, whose table is still
///   in `storage`, has its mark cleared: the deferred drop did not run, the `DROP` was
///   rolled back (`tests/table.rs`, `a_rolled_back_drop_leaves_the_table_droppable`).
///
/// It also moves the counter of the store past the identifiers a previous catalogue on this
/// storage handed out ([`TableStore::raise_counter_above`]).
///
/// The cost is one `databases()` and one `tables()` per database, per `create_table` or
/// `drop_table` call.
///
/// # Errors
///
/// The error of the `storage` calls.
pub(crate) fn refresh(catalog: &Catalog, store: &mut TableStore) -> SqlResult<()> {
    let live = live_tables(catalog)?;
    store.raise_counter_above(&live);
    let open: BTreeSet<TxnId> = catalog
        .txn
        .active_sessions()
        .into_iter()
        .map(|info| info.id)
        .collect();
    store
        .entries
        .retain(|_, entry| live.contains(&entry.meta.storage_id));
    for entry in store.entries.values_mut() {
        if let Some(dropper) = entry.dropped_by
            && !open.contains(&dropper)
        {
            entry.dropped_by = None;
        }
    }
    Ok(())
}

/// The tables `storage` holds, across the databases of the instance.
///
/// Read database by database because `Storage::tables` takes one: a [`TableId`] is unique
/// within the instance, so the union answers "is this table still there".
///
/// # Errors
///
/// The error of the `storage` calls.
fn live_tables(catalog: &Catalog) -> SqlResult<BTreeSet<TableId>> {
    let mut live = BTreeSet::new();
    for (db, _) in catalog.storage.databases()? {
        for (table, _) in catalog.storage.tables(db)? {
            live.insert(table);
        }
    }
    Ok(live)
}

/// The [`DbId`] of the database called `name`.
///
/// Compared without regard to case, as `bootstrap.rs` compares database names.
///
/// # Errors
///
/// [`InternalError::Bug`] when the name matches no database: a [`TableDef`] reaching the
/// catalogue is already resolved, and the client-facing error of an unknown database is
/// raised before, by the binder and `snapshot.rs`.
fn database_of(catalog: &Catalog, name: &str) -> SqlResult<DbId> {
    catalog
        .storage
        .databases()?
        .into_iter()
        .find(|(_, stored)| stored.eq_ignore_ascii_case(name))
        .map(|(id, _)| id)
        .ok_or_else(|| {
            InternalError::Bug(format!(
                "Catalog::create_table: no database named {name}; resolving a name is the \
                 business of the binder"
            ))
            .into()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::SqlType;

    use crate::def::ColumnDef;
    use crate::meta::QualifiedName;

    #[test]
    fn the_counter_starts_at_the_first_user_object_id() {
        let mut store = TableStore::default();
        assert_eq!(
            store.take_object_id().unwrap(),
            ObjectId(FIRST_USER_OBJECT_ID)
        );
        assert_eq!(
            store.take_object_id().unwrap(),
            ObjectId(FIRST_USER_OBJECT_ID + 1)
        );
    }

    #[test]
    fn an_exhausted_counter_is_a_bug() {
        let mut store = TableStore {
            next_object_id: i32::MAX - 1,
            entries: BTreeMap::new(),
            indexes: crate::index::IndexStore::default(),
            constraints: crate::constraints::ConstraintStore::default(),
        };
        assert_eq!(store.take_object_id().unwrap(), ObjectId(i32::MAX - 1));
        let err = store
            .take_object_id()
            .expect_err("the counter is exhausted");
        assert_eq!(
            err.message,
            "Internal error: internal bug: Catalog::create_table: object identifiers exhausted"
        );
    }

    #[test]
    fn the_counter_is_raised_above_the_tables_of_the_storage() {
        let mut store = TableStore::default();
        store.raise_counter_above(&BTreeSet::from([TableId(2), TableId(9)]));
        assert_eq!(
            store.take_object_id().unwrap(),
            ObjectId(FIRST_USER_OBJECT_ID + 10)
        );
        // Raising takes the higher of the two: a smaller storage does not pull it back.
        store.raise_counter_above(&BTreeSet::from([TableId(0)]));
        assert_eq!(
            store.take_object_id().unwrap(),
            ObjectId(FIRST_USER_OBJECT_ID + 11)
        );
        // An empty storage leaves it where it was.
        store.raise_counter_above(&BTreeSet::new());
        assert_eq!(
            store.take_object_id().unwrap(),
            ObjectId(FIRST_USER_OBJECT_ID + 12)
        );
    }

    /// A bootstrapped catalogue over a fresh `MemoryStorage` and a one-column table created
    /// in `master`: the read surface `snapshot.rs` and `identity.rs` use.
    fn catalogue_with_one_table() -> (Catalog, TableMeta) {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let def = TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "t".to_owned(),
            },
            columns: vec![ColumnDef {
                name: "a".to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
                default: None,
                identity: None,
                computed: None,
            }],
            constraints: Vec::new(),
        };
        let meta = create_table(&catalog, &handle, &def).expect("create_table");
        manager.commit(handle).expect("commit");
        (catalog, meta)
    }

    #[test]
    fn the_store_reads_back_what_create_table_wrote() {
        let (catalog, meta) = catalogue_with_one_table();
        let store = store(&catalog);
        assert_eq!(store.get(meta.id), Some(&meta));
        assert_eq!(store.get(ObjectId(1)), None);
    }

    #[test]
    fn the_store_lists_the_tables_that_no_drop_has_claimed() {
        let (catalog, meta) = catalogue_with_one_table();
        {
            let store = store(&catalog);
            let listed: Vec<&TableMeta> = store.live().collect();
            assert_eq!(listed, vec![&meta]);
        }
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        drop_table(&catalog, &handle, meta.id).expect("drop_table");
        let store = store(&catalog);
        assert_eq!(store.live().count(), 0, "the drop is registered");
        assert!(store.get(meta.id).is_some(), "the metadata is still read");
    }
}
