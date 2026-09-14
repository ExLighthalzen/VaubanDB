//! `CREATE INDEX` / `DROP INDEX` and the `PRIMARY KEY` / `UNIQUE` constraints they back.
//!
//! # A key is a shape and an index
//!
//! `storage` does not treat a clustered key as an index: it is
//! [`TableShape::clustered_key`](vauban_storage::TableShape::clustered_key), which fixes the
//! order of a scan. So a `PRIMARY KEY` writes two things — the clustered key of the shape,
//! and an [`IndexShape`] whose `columns` equal that key with `unique: true`, which is the
//! [`IndexId`] a `seek` goes through (`tests/index.rs`,
//! `primary_key_clustered_sets_shape_and_index`). A `UNIQUE` constraint
//! and a `PRIMARY KEY NONCLUSTERED` write the index alone, the table staying a heap
//! (`tests/index.rs`, `unique_nonclustered_is_an_index`,
//! `a_nonclustered_primary_key_leaves_a_heap`).
//!
//! Because the clustered key belongs to the shape and `storage` has no `ALTER`, the key is
//! posted **when the table is created**: `table.rs` asks [`table_keys`] for it before
//! `Storage::create_table`, then calls [`apply_table_keys`] to create the indexes. A
//! `CREATE CLUSTERED INDEX` on a table that already exists is not served here
//! ([`create_index`]).
//!
//! # What `sys.indexes` shows
//!
//! - `CREATE TABLE t (a int PRIMARY KEY, b int);` gives one row, `type_desc` `CLUSTERED`,
//!   `is_unique` 1, `is_primary_key` 1, `index_id` 1, over the key column `a` at
//!   `key_ordinal` 1. [`table_keys`] does not decide that: it reads the `clustered` field
//!   the [`ConstraintDef`] carries, which the binder fills — the default of a `PRIMARY KEY`
//!   written without the word is conditional, a `UNIQUE CLUSTERED` constraint of the same
//!   `CREATE TABLE` leaving the key non-clustered, and the binder is where that rule lives;
//! - `CREATE TABLE t (a int, b int UNIQUE);` gives two rows, a `HEAP` at `index_id` 0 and
//!   a `NONCLUSTERED` unique index on `b`;
//! - `CREATE TABLE t (…, PRIMARY KEY (b DESC, a));` keeps the key order and the direction:
//!   `b` at `key_ordinal` 1 with `is_descending_key` 1, then `a`;
//! - a `PRIMARY KEY` written without a name is called `PK__<table>__<16 hex digits>`, a
//!   `UNIQUE` one `UQ__…` — prefix, the first eight characters of the table name, sixteen
//!   hexadecimal digits ([`generated_key_name`]);
//! - `CREATE TABLE t (a int NOT NULL CONSTRAINT pk_t PRIMARY KEY NONCLUSTERED, b int);`
//!   keeps the written name and leaves the table a `HEAP`.
//!
//! # Eight numbers this file does not raise
//!
//! Eight numbers of SQL Server are not in `vauban-errors` — 8110, 8112, 1902, 1913, 1911,
//! 3723, 1909 and 8168. A shape that would send one of them answers an
//! [`InternalError::Bug`] naming it, as `table.rs` does for 2744:
//!
//! | Shape | Number |
//! |---|---|
//! | `CREATE TABLE t (a int PRIMARY KEY, b int PRIMARY KEY);` | 8110 state 0 |
//! | `CREATE TABLE t (a int PRIMARY KEY CLUSTERED, b int UNIQUE CLUSTERED);` | 8112 state 0 |
//! | `CREATE CLUSTERED INDEX ix ON t (b);` over a clustered `PRIMARY KEY` | 1902 state 3 |
//! | `CREATE INDEX ix ON t (b);` twice under one name | 1913 state 1 |
//! | `CREATE INDEX ix ON t (nope);` | 1911 state 1 |
//! | `DROP INDEX` over the index of a `PRIMARY KEY` | 3723 state 4 |
//! | `DROP INDEX` over the index of a `UNIQUE` constraint | 3723 state 5 |
//! | `PRIMARY KEY (a, a)` and `CREATE INDEX ix ON t (a ASC, a DESC);` | 1909 state 1 |
//! | two constraints of one name in one `CREATE TABLE` | 8168 state 0 |
//!
//! 3701 is the neighbour that **is** catalogued: state 7 for an index over a table that
//! exists, state 6 for one over a table the caller did not find (`vauban-errors`,
//! `CANNOT_DROP_3701_STATES`), and [`drop_index`] sends both, the second for
//! `BEGIN TRANSACTION; DROP TABLE t; DROP INDEX ix ON t;`.
//!
//! # Where the metadata sits
//!
//! In [`IndexStore`], the `indexes` field of the [`TableStore`] of `table.rs`, for the
//! reason written there. `storage` stays the reference on existence, so a reader calls
//! [`refresh`] before [`IndexStore::of_table`].

use std::collections::{BTreeMap, BTreeSet};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{IndexId, IndexShape, KeyColumn, TableId, TxnId};
use vauban_txn::{CommitAction, RollbackAction, TxnHandle};

use crate::catalog::{Catalog, not_implemented};
use crate::def::{ConstraintDef, IndexDef, SortedColumn, TableDef};
use crate::ids::ObjectId;
use crate::meta::{ColumnMeta, ConstraintMeta, IndexMeta, TableMeta};
use crate::table::{self, TableStore};

/// The state error 3701 sends when the `DROP INDEX` names an index of a table the caller
/// did not find, where the `index` row of `CANNOT_DROP_3701_STATES` carries the state of a
/// table that exists.
///
/// `vauban-errors` writes that a caller in this case needs its own state and leaves it out
/// of its table, so the state is set here on the error that constructor builds: state 6 for
/// `DROP TABLE` then `DROP INDEX` in one transaction, against 7 for the same `DROP INDEX`
/// over a table that is there (`tests/index.rs`,
/// `dropping_an_index_of_a_table_being_dropped_is_3701_state_6`).
const INDEX_OF_A_DROPPED_TABLE_3701_STATE: u8 = 6;

/// The indexes the catalogue has created, by [`IndexId`].
///
/// Held by the `indexes` field of [`TableStore`]; `index.rs` owns the type and the accesses
/// to it, as `table.rs` owns [`TableStore`] itself.
#[derive(Debug, Default)]
pub(crate) struct IndexStore {
    /// One entry per index created through this catalogue and still in `storage`.
    entries: BTreeMap<IndexId, IndexEntry>,
}

/// One index of an [`IndexStore`].
#[derive(Debug)]
struct IndexEntry {
    /// Table the index is built on, as the catalogue numbers it.
    table: ObjectId,
    /// Table the index is built on, as `storage` numbers it. Kept beside `table` so that
    /// [`refresh`] can ask `storage` for the indexes of that table.
    storage_table: TableId,
    /// What [`create_index`] or [`apply_table_keys`] gave back.
    meta: IndexMeta,
    /// The transaction that registered the deferred [`CommitAction::DropIndex`], `None` when
    /// the index carries no pending `DROP`. Cleared by [`refresh`] when that transaction
    /// closed without the drop taking effect (`tests/index.rs`,
    /// `a_rolled_back_drop_index_leaves_the_index`).
    dropped_by: Option<TxnId>,
}

impl IndexStore {
    /// The indexes of the table `table` that no `DROP` has claimed, by increasing
    /// [`IndexId`].
    ///
    /// The read side of the store: `snapshot.rs` answers `CatalogSnapshot::indexes_of` with
    /// it and `views/sys_indexes.rs` builds the rows of `sys.indexes` and
    /// `sys.index_columns` from it, neither of them reopening this file. The caller runs
    /// [`refresh`] first, the store being in memory while `storage` is the reference on
    /// existence (unit test `the_store_lists_the_indexes_of_one_table`).
    #[allow(dead_code)] // read by snapshot.rs and views/sys_indexes.rs
    pub(crate) fn of_table(&self, table: ObjectId) -> Vec<&IndexMeta> {
        self.entries
            .values()
            .filter(|entry| entry.table == table && entry.dropped_by.is_none())
            .map(|entry| &entry.meta)
            .collect()
    }

    /// The index of identifier `id`, `None` when this catalogue created no such index or
    /// when it has forgotten it ([`refresh`]).
    ///
    /// Same readers as [`IndexStore::of_table`] (unit test
    /// `the_store_lists_the_indexes_of_one_table`).
    pub(crate) fn get(&self, id: IndexId) -> Option<&IndexMeta> {
        self.entries.get(&id).map(|entry| &entry.meta)
    }

    /// The entry of the index of `table` called `name`, or `None`.
    ///
    /// A pending `DROP` does not free the name: the index is in `storage` until the
    /// `COMMIT` (`tests/index.rs`, `a_duplicate_index_name_on_a_table_is_refused`).
    fn named(&self, table: ObjectId, name: &str) -> Option<&IndexEntry> {
        self.entries
            .values()
            .find(|entry| entry.table == table && entry.meta.name.eq_ignore_ascii_case(name))
    }
}

/// What asked for an index, which is what a duplicate name answers with.
///
/// Two constraints of one name in one `CREATE TABLE` answer 8168, two `CREATE INDEX` of
/// one name answer 1913, and a `CREATE INDEX` whose name is already that of a constraint of
/// the table answers 1913 as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A `PRIMARY KEY` or `UNIQUE` constraint of a `CREATE TABLE`.
    Constraint,
    /// A `CREATE INDEX` statement.
    Statement,
}

impl Origin {
    /// The sentence a duplicate name answers with, number included.
    fn duplicate_name(self, table: &str, name: &str) -> InternalError {
        InternalError::Bug(match self {
            Origin::Constraint => format!(
                "Catalog::create_table: table {table} carries two constraints named {name}; \
                 SQL Server answers 8168, which vauban-errors does not catalogue"
            ),
            Origin::Statement => format!(
                "Catalog::create_index: table {table} already carries an index named {name}; \
                 SQL Server answers 1913, which vauban-errors does not catalogue"
            ),
        })
    }
}

/// One `PRIMARY KEY` or `UNIQUE` constraint of a [`TableDef`], its columns resolved into
/// positions in the row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyConstraint {
    /// Name the statement gave the constraint, `None` when it wrote no name: the catalogue
    /// then makes one ([`generated_key_name`], unit tests
    /// `a_named_constraint_keeps_the_name_it_was_written_with`,
    /// `a_generated_name_follows_the_published_shape`).
    name: Option<String>,
    /// `true` for a `PRIMARY KEY`, `false` for a `UNIQUE`.
    primary_key: bool,
    /// `true` when this constraint carries the clustered key of the table.
    clustered: bool,
    /// Key columns, in key order.
    columns: Vec<KeyColumn>,
}

/// The `PRIMARY KEY` and `UNIQUE` constraints of a [`TableDef`], checked and resolved.
///
/// Built by [`table_keys`] **before** `Storage::create_table`, because
/// [`TableKeys::clustered_key`] goes into the
/// [`TableShape`](vauban_storage::TableShape), and consumed by [`apply_table_keys`] once
/// the table exists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TableKeys {
    /// The constraints, in the order they were declared.
    keys: Vec<KeyConstraint>,
    /// The clustered key of the table, `None` when no constraint of `keys` is clustered —
    /// the table is then a heap, as it is under a `PRIMARY KEY NONCLUSTERED`
    /// (`tests/index.rs`, `a_nonclustered_primary_key_leaves_a_heap`).
    clustered_key: Option<Vec<KeyColumn>>,
}

impl TableKeys {
    /// The clustered key the shape of the table takes, `None` for a heap.
    pub(crate) fn clustered_key(&self) -> Option<Vec<KeyColumn>> {
        self.clustered_key.clone()
    }
}

/// The `PRIMARY KEY` and `UNIQUE` constraints of `def`, resolved against `columns`.
///
/// Called by `create_table` before the table exists, so that a definition the catalogue
/// refuses leaves nothing behind in `storage` (`tests/index.rs`,
/// `two_primary_keys_leave_no_table_behind`).
///
/// # Errors
///
/// [`InternalError::Bug`] when `def` carries a constraint this file does not serve, or a
/// shape SQL Server refuses with a number `vauban-errors` does not carry (module
/// documentation): a `FOREIGN KEY`, a `CHECK` or a `DEFAULT` — `constraints.rs` stores
/// those — a key over no column, a key over a column the table does not declare (1911), two
/// `PRIMARY KEY` constraints (8110) or two clustered constraints (8112).
pub(crate) fn table_keys(def: &TableDef, columns: &[ColumnMeta]) -> SqlResult<TableKeys> {
    let mut keys = TableKeys::default();
    for constraint in &def.constraints {
        let (name, primary_key, clustered, declared) = match constraint {
            ConstraintDef::PrimaryKey {
                name,
                columns,
                clustered,
            } => (name, true, *clustered, columns),
            ConstraintDef::Unique {
                name,
                columns,
                clustered,
            } => (name, false, *clustered, columns),
            ConstraintDef::ForeignKey { .. }
            | ConstraintDef::Check { .. }
            | ConstraintDef::Default { .. } => {
                return Err(not_implemented(
                    "create_table with a FOREIGN KEY, CHECK or DEFAULT constraint",
                ));
            }
        };
        if primary_key && keys.keys.iter().any(|key| key.primary_key) {
            return Err(InternalError::Bug(format!(
                "Catalog::create_table: table {} carries two PRIMARY KEY constraints; the \
                 SQL Server answers 8110, which vauban-errors does not catalogue",
                def.name.name
            ))
            .into());
        }
        if clustered && keys.clustered_key.is_some() {
            return Err(InternalError::Bug(format!(
                "Catalog::create_table: table {} carries two clustered constraints; the \
                 SQL Server answers 8112, which vauban-errors does not catalogue",
                def.name.name
            ))
            .into());
        }
        let resolved = key_columns(&def.name.name, declared, columns)?;
        if clustered {
            keys.clustered_key = Some(resolved.clone());
        }
        keys.keys.push(KeyConstraint {
            name: name.clone(),
            primary_key,
            clustered,
            columns: resolved,
        });
    }
    Ok(keys)
}

/// Creates the index of each constraint of `keys` and writes them on `meta`.
///
/// Called by `create_table` once `Storage::create_table` has returned, with the `meta` the
/// catalogue is about to store: it fills [`TableMeta::clustered`] with the index that
/// carries the clustered key and pushes one [`ConstraintMeta::PrimaryKey`] or
/// [`ConstraintMeta::Unique`] per constraint, in declaration order (`tests/index.rs`,
/// `primary_key_clustered_sets_shape_and_index`, `two_keys_on_one_table_are_two_indexes`).
///
/// # Errors
///
/// The error of [`create_one_index`]. An index created before the failing one stays in
/// `storage` until the transaction rolls back, with its own compensation, as the table
/// itself does (`table.rs`).
///
/// # Bound: what a failure here leaves behind
///
/// The table has been created in `storage` and its `TableMeta` is not stored yet — `create_table`
/// returns the error before inserting the entry — so the catalogue no longer names that
/// table: a second `CREATE TABLE` of the same name in the same transaction is accepted and
/// creates a namesake in `storage`, where SQL Server answers 2714. Both tables go at the
/// `ROLLBACK`, each with its compensation, so what the statement leaves is bounded by the
/// transaction (`tests/index.rs`,
/// `a_failed_key_leaves_a_table_the_catalogue_does_not_name`). Naming it asks for
/// versioned rows, which unwind with the transaction.
pub(crate) fn apply_table_keys(
    catalog: &Catalog,
    txn: &TxnHandle,
    store: &mut TableStore,
    keys: &TableKeys,
    meta: &mut TableMeta,
) -> SqlResult<()> {
    for (position, key) in keys.keys.iter().enumerate() {
        let name = match &key.name {
            Some(name) => name.clone(),
            None => generated_key_name(key.primary_key, &meta.name, meta.id, position),
        };
        let id = create_one_index(
            catalog,
            txn,
            store,
            meta,
            Origin::Constraint,
            IndexMeta {
                id: IndexId(0),
                name,
                unique: true,
                clustered: key.clustered,
                primary_key: key.primary_key,
                columns: key.columns.clone(),
            },
        )?;
        if key.clustered {
            meta.clustered = Some(id);
        }
        meta.constraints.push(if key.primary_key {
            ConstraintMeta::PrimaryKey(id)
        } else {
            ConstraintMeta::Unique(id)
        });
    }
    Ok(())
}

/// Creates an index. See [`Catalog::create_index`].
///
/// # Bound: `CREATE CLUSTERED INDEX` on a table that exists
///
/// `def.clustered` is not served: the clustered key is a field of the
/// [`TableShape`](vauban_storage::TableShape) and `storage` has no way to change the shape
/// of a table, so the catalogue answers a stub until a table can be rebuilt by copy
/// (`tests/index.rs`, `a_clustered_create_index_is_not_served`). SQL Server takes it on a
/// heap — `CREATE CLUSTERED INDEX ix ON t (a);` gives a `CLUSTERED` row in `sys.indexes` —
/// and answers 1902 when the table already has a clustered index.
///
/// # Errors
///
/// - [`InternalError::Bug`] when `def.table` names no table of this catalogue, when
///   `def.columns` is empty, when a column is not one of the table (1911), when
///   `def.clustered` is set (above), or when the table already carries an index of that
///   name (1913). Those numbers are not in `vauban-errors` (module documentation);
///   resolving a name is the business of the binder and of `snapshot.rs`;
/// - the error of `storage` — 2601 when `def.unique` and live rows already share a key —
///   or of the transaction manager.
pub(crate) fn create_index(
    catalog: &Catalog,
    txn: &TxnHandle,
    def: &IndexDef,
) -> SqlResult<IndexMeta> {
    let mut store = table::store(catalog);
    table::refresh(catalog, &mut store)?;
    refresh(catalog, &mut store)?;
    let Some(meta) = store.get(def.table).cloned() else {
        return Err(InternalError::Bug(format!(
            "Catalog::create_index: index {} names the unknown table {}; resolving a name is \
             the business of the binder",
            def.name, def.table
        ))
        .into());
    };
    if def.clustered {
        return Err(not_implemented("create_index CLUSTERED"));
    }
    let columns = key_columns(&def.name, &def.columns, &meta.columns)?;
    let id = create_one_index(
        catalog,
        txn,
        &mut store,
        &meta,
        Origin::Statement,
        IndexMeta {
            id: IndexId(0),
            name: def.name.clone(),
            unique: def.unique,
            clustered: false,
            primary_key: false,
            columns,
        },
    )?;
    // The index changes the rows of `sys.indexes`, `sys.index_columns` and, through the
    // `index_id` numbering, those of the indexes that follow it: the rows of the table are
    // rebuilt inside `txn` (`sys_rows.rs`).
    crate::sys_rows::rewrite(catalog, txn, &meta, &store.indexes)?;
    store.indexes.get(id).cloned().ok_or_else(|| {
        InternalError::Bug(format!(
            "Catalog::create_index: index {id} was created and not stored"
        ))
        .into()
    })
}

/// Drops an index. See [`Catalog::drop_index`].
///
/// `storage.drop_index` is not called here: a [`CommitAction::DropIndex`] is registered and
/// the transaction manager makes the call at `COMMIT`, so the index stays until then and a
/// `ROLLBACK` leaves it where it was (`tests/index.rs`, `drop_index_deferred`,
/// `a_rolled_back_drop_index_leaves_the_index`).
///
/// The bound `table.rs` writes on `drop_table` holds here for the same reason: a `DROP`
/// undone by `ROLLBACK TRANSACTION <savepoint>` keeps the mark this function leaves on the
/// entry, the public API of `vauban_txn` not saying whether a registered action is still in
/// the log (`table.rs`, section "Bound"; `tests/index.rs`,
/// `a_savepoint_rollback_leaves_the_drop_mark_in_place`).
///
/// # Bound: the name of a dropped index before the `COMMIT`
///
/// The deferral keeps the index in `storage` until the `COMMIT`, so its name stays taken for
/// the rest of the transaction and a `CREATE INDEX` that reuses it answers the duplicate-name
/// bug of [`Origin::Statement`]. SQL Server frees the name at once:
/// `DROP INDEX ix ON t; CREATE INDEX ix ON t (b);` in one transaction is accepted there. The
/// state this gives is frozen by `tests/index.rs`,
/// `a_name_dropped_in_the_transaction_is_not_free_before_the_commit`; freeing it asks for
/// versioned rows, the name of an index being a row of the catalogue then, rather than for
/// a second mark on this entry.
///
/// # Errors
///
/// - an [`InternalError::Bug`] naming 3723 when `index` is the index of a `PRIMARY KEY` or of
///   a `UNIQUE` constraint of its table ([`backed_constraint`]): dropping it would leave the
///   clustered key of the shape without its index and [`TableMeta::clustered`] pointing at an
///   identifier `storage` no longer knows. SQL Server refuses it too, at states 4 and 5
///   (module documentation);
/// - 3701 at state 6 when the table of `index` carries a deferred `DROP TABLE` of the same
///   transaction: the commit actions run in registration order, so the table would take the
///   index with it before the action registered here runs, and the `COMMIT` would stop on an
///   unknown index (`tests/index.rs`,
///   `dropping_an_index_of_a_table_being_dropped_is_3701_state_6`);
/// - 3701 at state 7 when `index` names no index of this catalogue, or one a `DROP` of the
///   same transaction has already claimed (`tests/index.rs`,
///   `dropping_an_unknown_index_is_3701`, `dropping_the_same_index_twice_is_3701`). The
///   `%.*ls` of 3701 carries the table and the index — `'dbo.t.ix_nope'` — so the
///   catalogue writes `<schema>.<table>.<index>`
///   when it holds the name and the bare identifier when it does not, the client-facing name
///   having been resolved before the call.
pub(crate) fn drop_index(catalog: &Catalog, txn: &TxnHandle, index: IndexId) -> SqlResult<()> {
    let mut store = table::store(catalog);
    table::refresh(catalog, &mut store)?;
    refresh(catalog, &mut store)?;
    let Some(entry) = store.indexes.entries.get(&index) else {
        return Err(SqlError::cannot_drop("drop", "index", &index.to_string()));
    };
    let owner = entry.table;
    let dropped = entry.dropped_by.is_some();
    let table = store.get(entry.table);
    let name = match table {
        Some(table) => format!("{}.{}.{}", table.schema, table.name, entry.meta.name),
        None => entry.meta.name.clone(),
    };
    if dropped {
        return Err(SqlError::cannot_drop("drop", "index", &name));
    }
    if let Some(table) = table
        && let Some(constraint) = backed_constraint(table, index)
    {
        return Err(InternalError::Bug(format!(
            "Catalog::drop_index: index {name} is being used for {constraint} constraint \
             enforcement; SQL Server answers 3723, which vauban-errors does not catalogue"
        ))
        .into());
    }
    // The table of the index carries a deferred `DROP`: its `CommitAction` runs before the
    // one registered here and takes the index with it, so this call would leave the `COMMIT`
    // on an unknown index. SQL Server answers 3701 at another state than the one the
    // constructor keys on the kind.
    if store.live().all(|live| live.id != entry.table) {
        let mut err = SqlError::cannot_drop("drop", "index", &name);
        err.state = INDEX_OF_A_DROPPED_TABLE_3701_STATE;
        return Err(err);
    }
    catalog
        .txn
        .register_on_commit(txn, CommitAction::DropIndex(index))?;
    if let Some(entry) = store.indexes.entries.get_mut(&index) {
        entry.dropped_by = Some(txn.id);
    }
    // Same rebuild as in `create_index`: the dropped index leaves `IndexStore::of_table`, so
    // its rows and the `index_id` of the indexes that followed it are written again.
    let Some(meta) = store.get(owner).cloned() else {
        return Err(InternalError::Bug(format!(
            "Catalog::drop_index: index {index} is on the table {owner}, which the catalogue \
             checked was live and no longer holds"
        ))
        .into());
    };
    crate::sys_rows::rewrite(catalog, txn, &meta, &store.indexes)?;
    Ok(())
}

/// Brings the index store back in line with `storage`, which is the reference on existence.
///
/// The mirror of [`table::refresh`], which the caller runs first: an entry whose index is
/// gone from `storage` is forgotten — a `CREATE INDEX` undone by a `ROLLBACK`, a
/// `DROP INDEX` carried out by a `COMMIT`, an index dropped with its table
/// (`tests/index.rs`, `dropping_the_table_forgets_its_indexes`) — and an entry marked
/// dropped by a transaction that is no longer open, whose index is still there, has its
/// mark cleared (`tests/index.rs`, `a_rolled_back_drop_index_leaves_the_index`).
///
/// # Errors
///
/// The error of the `storage` calls.
pub(crate) fn refresh(catalog: &Catalog, store: &mut TableStore) -> SqlResult<()> {
    let mut live: BTreeSet<IndexId> = BTreeSet::new();
    let mut asked: BTreeSet<TableId> = BTreeSet::new();
    for entry in store.indexes.entries.values() {
        if store.get(entry.table).is_some() && asked.insert(entry.storage_table) {
            for (id, _) in catalog.storage.indexes(entry.storage_table)? {
                live.insert(id);
            }
        }
    }
    let open: BTreeSet<TxnId> = catalog
        .txn
        .active_sessions()
        .into_iter()
        .map(|info| info.id)
        .collect();
    store.indexes.entries.retain(|id, _| live.contains(id));
    for entry in store.indexes.entries.values_mut() {
        if let Some(dropper) = entry.dropped_by
            && !open.contains(&dropper)
        {
            entry.dropped_by = None;
        }
    }
    Ok(())
}

/// Creates one index in `storage`, registers its compensation and stores `meta` under the
/// identifier `storage` handed out.
///
/// The `id` field of `meta` is ignored on the way in — the identifier is the one of the
/// creation — and the stored [`IndexMeta`] carries it.
///
/// # Errors
///
/// [`InternalError::Bug`] when `table` already carries an index of that name — 8168 or 1913
/// according to `origin` ([`Origin`]); the error of `storage` or of the transaction manager
/// otherwise. When the
/// compensation cannot be registered, the index just created is dropped again before the
/// error comes back (`tests/index.rs`,
/// `a_create_index_that_cannot_be_compensated_leaves_nothing`).
fn create_one_index(
    catalog: &Catalog,
    txn: &TxnHandle,
    store: &mut TableStore,
    table: &TableMeta,
    origin: Origin,
    meta: IndexMeta,
) -> SqlResult<IndexId> {
    if store.indexes.named(table.id, &meta.name).is_some() {
        return Err(origin.duplicate_name(&table.name, &meta.name).into());
    }
    let shape = IndexShape {
        columns: meta.columns.clone(),
        unique: meta.unique,
        included: Vec::new(),
    };
    let id = catalog.storage.create_index(table.storage_id, &shape)?;
    if let Err(err) = catalog
        .txn
        .register_on_rollback(txn, RollbackAction::DropIndex(id))
    {
        catalog.storage.drop_index(id)?;
        return Err(err);
    }
    store.indexes.entries.insert(
        id,
        IndexEntry {
            table: table.id,
            storage_table: table.storage_id,
            meta: IndexMeta { id, ..meta },
            dropped_by: None,
        },
    );
    Ok(id)
}

/// `PRIMARY KEY` or `UNIQUE` when `index` is the index of a constraint of `table`, `None`
/// for an index a `CREATE INDEX` made.
///
/// Read from [`TableMeta::constraints`] rather than from a flag of the [`IndexMeta`]: the
/// index of a `UNIQUE` constraint and the one of a `CREATE UNIQUE INDEX` have the same
/// fields, and the constraint list is what tells them apart (`tests/index.rs`,
/// `dropping_the_index_of_a_primary_key_names_3723`,
/// `dropping_the_index_of_a_unique_constraint_names_3723`).
fn backed_constraint(table: &TableMeta, index: IndexId) -> Option<&'static str> {
    table
        .constraints
        .iter()
        .find_map(|constraint| match constraint {
            ConstraintMeta::PrimaryKey(id) if *id == index => Some("PRIMARY KEY"),
            ConstraintMeta::Unique(id) if *id == index => Some("UNIQUE KEY"),
            _ => None,
        })
}

/// The [`KeyColumn`] of each column of `declared`, in key order.
///
/// Names are compared with `eq_ignore_ascii_case`, as `table.rs` compares the names of
/// tables and for the same reason: folding as the collation of the database does asks for
/// the resolution of `snapshot.rs`.
///
/// # Errors
///
/// [`InternalError::Bug`] when `declared` is empty — `storage` asks for a key of at least
/// one column — when a name is not a column of the table (1911), or when it names one column
/// twice, `(a, a)` as `(a ASC, a DESC)` (1909): neither number is in `vauban-errors` (module
/// documentation; `tests/index.rs`, `a_key_with_a_repeated_column_names_1909`).
fn key_columns(
    key: &str,
    declared: &[SortedColumn],
    columns: &[ColumnMeta],
) -> SqlResult<Vec<KeyColumn>> {
    if declared.is_empty() {
        return Err(
            InternalError::Bug(format!("Catalog::create_index: key {key} has no column")).into(),
        );
    }
    let mut resolved = Vec::with_capacity(declared.len());
    for column in declared {
        let found = columns
            .iter()
            .find(|candidate| candidate.name.eq_ignore_ascii_case(&column.column))
            .ok_or_else(|| {
                InternalError::Bug(format!(
                    "Catalog::create_index: key {key} names the column {}, which the table \
                     does not declare; SQL Server answers 1911, which vauban-errors does not \
                     catalogue",
                    column.column
                ))
            })?;
        if resolved
            .iter()
            .any(|taken: &KeyColumn| taken.column == found.ordinal)
        {
            return Err(InternalError::Bug(format!(
                "Catalog::create_index: key {key} lists the column {} more than once; the \
                 SQL Server answers 1909, which vauban-errors does not catalogue",
                found.name
            ))
            .into());
        }
        resolved.push(KeyColumn {
            column: found.ordinal,
            descending: column.descending,
        });
    }
    Ok(resolved)
}

/// The name of a constraint the statement did not name.
///
/// The shape is the one SQL Server publishes (module documentation): `PK` or `UQ`, two
/// underscores, the first eight characters of the table name, two underscores, sixteen
/// upper-case hexadecimal digits — `PK__my_table__3BD0198E1CED331D`. The digits are ours, a
/// hash of the identifier of the table, of the position of the constraint and of the name of
/// the table, so that two constraints of one table take two names and a name does not move
/// between two runs (unit tests `a_generated_name_follows_the_published_shape`,
/// `two_constraints_of_one_table_take_two_names`). What a client compares is the row of
/// `sys.key_constraints`, not the digits.
fn generated_key_name(primary_key: bool, table: &str, id: ObjectId, position: usize) -> String {
    let prefix = if primary_key { "PK" } else { "UQ" };
    let head: String = table.chars().take(8).collect();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in
        id.0.to_be_bytes()
            .into_iter()
            .chain((position as u64).to_be_bytes())
            .chain(table.as_bytes().iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{prefix}__{head}__{hash:016X}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::{SqlType, TypeInfo};

    use crate::def::ColumnDef;
    use crate::ids::ColumnId;
    use crate::meta::QualifiedName;

    /// Two columns, `a` at ordinal 0 and `b` at ordinal 1.
    fn columns() -> Vec<ColumnMeta> {
        ["a", "b"]
            .into_iter()
            .enumerate()
            .map(|(position, name)| ColumnMeta {
                id: ColumnId(i32::try_from(position).expect("two columns") + 1),
                name: name.to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
                ordinal: u16::try_from(position).expect("two columns"),
                default: None,
                identity: None,
                computed: None,
            })
            .collect()
    }

    /// A `TableDef` named `master.dbo.t` carrying `constraints`.
    fn def(constraints: Vec<ConstraintDef>) -> TableDef {
        TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "t".to_owned(),
            },
            columns: vec![
                ColumnDef {
                    name: "a".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, false),
                    default: None,
                    identity: None,
                    computed: None,
                },
                ColumnDef {
                    name: "b".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, false),
                    default: None,
                    identity: None,
                    computed: None,
                },
            ],
            constraints,
        }
    }

    /// `PRIMARY KEY` or `UNIQUE` over the columns named, with their direction.
    fn key(primary_key: bool, clustered: bool, names: &[(&str, bool)]) -> ConstraintDef {
        let columns = names
            .iter()
            .map(|(name, descending)| SortedColumn {
                column: (*name).to_owned(),
                descending: *descending,
            })
            .collect();
        if primary_key {
            ConstraintDef::PrimaryKey {
                name: None,
                columns,
                clustered,
            }
        } else {
            ConstraintDef::Unique {
                name: None,
                columns,
                clustered,
            }
        }
    }

    #[test]
    fn a_clustered_primary_key_gives_the_shape_its_key() {
        let keys =
            table_keys(&def(vec![key(true, true, &[("a", false)])]), &columns()).expect("keys");
        assert_eq!(
            keys.clustered_key(),
            Some(vec![KeyColumn {
                column: 0,
                descending: false
            }])
        );
        assert_eq!(keys.keys.len(), 1);
        assert!(keys.keys[0].primary_key);
    }

    #[test]
    fn a_nonclustered_key_leaves_the_shape_a_heap() {
        let keys =
            table_keys(&def(vec![key(false, false, &[("b", false)])]), &columns()).expect("keys");
        assert_eq!(keys.clustered_key(), None);
        assert_eq!(
            keys.keys[0].columns,
            vec![KeyColumn {
                column: 1,
                descending: false
            }]
        );
        assert!(!keys.keys[0].primary_key);
    }

    #[test]
    fn a_composite_key_keeps_its_order_and_its_direction() {
        let keys = table_keys(
            &def(vec![key(true, true, &[("b", true), ("A", false)])]),
            &columns(),
        )
        .expect("keys");
        assert_eq!(
            keys.clustered_key(),
            Some(vec![
                KeyColumn {
                    column: 1,
                    descending: true
                },
                KeyColumn {
                    column: 0,
                    descending: false
                },
            ]),
            "the key order is the declared one and `A` matches `a`"
        );
    }

    #[test]
    fn two_primary_keys_name_8110() {
        let err = table_keys(
            &def(vec![
                key(true, true, &[("a", false)]),
                key(true, false, &[("b", false)]),
            ]),
            &columns(),
        )
        .expect_err("two primary keys");
        assert!(err.message.contains("8110"), "{}", err.message);
    }

    #[test]
    fn two_clustered_constraints_name_8112() {
        let err = table_keys(
            &def(vec![
                key(true, true, &[("a", false)]),
                key(false, true, &[("b", false)]),
            ]),
            &columns(),
        )
        .expect_err("two clustered constraints");
        assert!(err.message.contains("8112"), "{}", err.message);
    }

    #[test]
    fn a_key_over_an_unknown_column_names_1911() {
        let err = table_keys(&def(vec![key(true, true, &[("nope", false)])]), &columns())
            .expect_err("unknown column");
        assert!(err.message.contains("1911"), "{}", err.message);
        let err =
            table_keys(&def(vec![key(true, true, &[])]), &columns()).expect_err("no key column");
        assert!(err.message.contains("has no column"), "{}", err.message);
    }

    #[test]
    fn a_check_constraint_is_not_a_key() {
        let err = table_keys(
            &def(vec![ConstraintDef::Check {
                name: None,
                expr: vauban_parser::Expr::Literal(
                    vauban_parser::Literal::Integer("1".to_owned()),
                    vauban_parser::Span::EMPTY,
                ),
            }]),
            &columns(),
        )
        .expect_err("a CHECK constraint");
        assert_eq!(
            err.message,
            "Internal error: internal bug: Catalog::create_table with a FOREIGN KEY, CHECK or \
             DEFAULT constraint not implemented"
        );
    }

    #[test]
    fn a_duplicate_name_names_8168_for_a_constraint_and_1913_for_an_index() {
        let constraint = Origin::Constraint.duplicate_name("t", "c1");
        let statement = Origin::Statement.duplicate_name("t", "ix_b");
        let InternalError::Bug(constraint) = constraint else {
            panic!("a duplicate name is a bug until the numbers are catalogued");
        };
        let InternalError::Bug(statement) = statement else {
            panic!("a duplicate name is a bug until the numbers are catalogued");
        };
        assert!(constraint.contains("8168"), "{constraint}");
        assert!(!constraint.contains("1913"), "{constraint}");
        assert!(statement.contains("1913"), "{statement}");
        assert!(!statement.contains("8168"), "{statement}");
    }

    #[test]
    fn a_generated_name_follows_the_published_shape() {
        let name = generated_key_name(true, "my_table_pk", ObjectId(1_000_000), 0);
        assert_eq!(&name[..12], "PK__my_table");
        assert_eq!(name.len(), "PK__my_table__3BD0198E1CED331D".len());
        assert!(
            name[14..].chars().all(|c| c.is_ascii_hexdigit()),
            "the tail of {name} is sixteen hexadecimal digits"
        );
        assert_eq!(
            name,
            generated_key_name(true, "my_table_pk", ObjectId(1_000_000), 0),
            "the same table and position give the same name twice"
        );
        assert!(
            generated_key_name(false, "my_table_uq", ObjectId(1_000_000), 0).starts_with("UQ__"),
            "a UNIQUE constraint takes the UQ prefix"
        );
        assert_eq!(
            generated_key_name(true, "ab", ObjectId(7), 0).len(),
            "PK__ab__3BD0198E1CED331D".len(),
            "a short table name is not padded"
        );
    }

    #[test]
    fn two_constraints_of_one_table_take_two_names() {
        assert_ne!(
            generated_key_name(true, "t", ObjectId(1_000_000), 0),
            generated_key_name(false, "t", ObjectId(1_000_000), 1)
        );
        assert_ne!(
            generated_key_name(true, "t", ObjectId(1_000_000), 0),
            generated_key_name(true, "t", ObjectId(1_000_001), 0),
            "two tables take two names"
        );
    }

    #[test]
    fn a_named_constraint_keeps_the_name_it_was_written_with() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let meta = table::create_table(
            &catalog,
            &handle,
            &def(vec![ConstraintDef::PrimaryKey {
                name: Some("pk_named".to_owned()),
                columns: vec![SortedColumn {
                    column: "a".to_owned(),
                    descending: false,
                }],
                clustered: false,
            }]),
        )
        .expect("create_table");
        manager.commit(handle).expect("commit");

        let store = table::store(&catalog);
        let listed = store.indexes.of_table(meta.id);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "pk_named");
        assert!(listed[0].primary_key && !listed[0].clustered);
    }

    #[test]
    fn the_store_lists_the_indexes_of_one_table() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let meta = table::create_table(
            &catalog,
            &handle,
            &def(vec![
                key(true, true, &[("a", false)]),
                key(false, false, &[("b", false)]),
            ]),
        )
        .expect("create_table");
        manager.commit(handle).expect("commit");

        let store = table::store(&catalog);
        let listed = store.indexes.of_table(meta.id);
        assert_eq!(listed.len(), 2, "the primary key and the unique constraint");
        assert!(listed[0].name.starts_with("PK__t__"), "{}", listed[0].name);
        assert!(listed[1].name.starts_with("UQ__t__"), "{}", listed[1].name);
        assert!(listed[0].primary_key && listed[0].clustered && listed[0].unique);
        assert!(!listed[1].primary_key && !listed[1].clustered && listed[1].unique);
        assert_eq!(
            store.indexes.of_table(ObjectId(1)),
            Vec::<&IndexMeta>::new()
        );
        let clustered = meta.clustered.expect("the primary key is clustered");
        assert_eq!(store.indexes.get(clustered), Some(listed[0]));
        assert_eq!(store.indexes.get(IndexId(9_999)), None);
    }
}
