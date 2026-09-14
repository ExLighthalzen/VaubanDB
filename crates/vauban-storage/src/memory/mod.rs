//! [`MemoryStorage`]: the in-memory implementation of [`Storage`], used by every test of
//! the engine and by the `--in-memory` mode of the binary. No durability, safe Rust only, no
//! optimisation: `BTreeMap`s and sorted `Vec`s behind a single `RwLock`.
//!
//! It covers databases, tables, `insert`/`get`/`scan`, `commit`/`rollback` and
//! `checkpoint`; `update`/`delete`/`latest_version`, savepoints and `vacuum`; the indexes
//! (`create_index`/`drop_index`/`indexes`/`seek`, uniqueness under MVCC, maintenance on
//! each write) and the clustered-key order of `scan`.

mod index;
mod key_order;
mod table;
#[cfg(test)]
mod tests;
mod txn_log;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::{
    DbId, Direction, IndexId, IndexShape, KeyRange, Row, RowId, RowIter, SavepointId, Snapshot,
    Storage, TableId, TableShape, TxnId, TxnStatus,
};
use index::{Index, IndexEntry, find_version, key_text};
use key_order::{KeyOrder, sort_rows};
use table::{Table, Version};
use txn_log::{TxnState, UndoEntry};

/// In-memory [`Storage`]. Cheap to create, forgets everything when dropped.
///
/// All methods take `&self`: the state lives behind a [`RwLock`], taken for writing by DDL,
/// `insert`, `update`, `delete`, `savepoint`, `rollback_to`, `commit`, `rollback` and
/// `vacuum`, for reading by `get`, `scan`, `seek`, `latest_version`, `databases`, `tables`
/// and `indexes`. No method ever returns something that keeps the lock: `scan` and `seek`
/// copy the visible rows while holding it, then release it before handing out the
/// iterator.
///
/// # Indexes
///
/// An index is a `Vec` of entries sorted by `(key, RowId, version)`, one entry per row
/// **version** (`index::Index`); `seek` filters the entries of the requested range by the
/// visibility of their version. Every `insert`/`update` adds entries (after the uniqueness
/// check of "Duplicate keys" on [`Storage`]), `rollback`/`rollback_to` remove the entries
/// of the versions they undo, `vacuum` those of the versions it discards; `delete` leaves
/// the index alone, visibility does the filtering. The clustered key is not an index:
/// `scan` sorts the visible rows with the same comparator (`key_order::KeyOrder`).
///
/// # Transaction statuses
///
/// The registry only knows the transactions that wrote (`insert`, `update`, `delete`),
/// took a `savepoint`, or were explicitly committed or rolled back, and `vacuum` forgets
/// the finished ones below its horizon. Any other transaction is reported as `Committed` to
/// [`Snapshot::is_visible`]. This closure is exact in both cases:
///
/// - a transaction the storage never saw wrote nothing, so no version carries its id and
///   its status influences nothing;
/// - a transaction forgotten by `vacuum` had finished below the horizon. If it had aborted,
///   `rollback` had removed its versions and reset the `xmax`s it set, and the second pass
///   of `vacuum` discards any version its `xmin` still named before the registry is pruned.
///   If it had committed, the first pass discarded every version whose `xmax` names it; the
///   only surviving references are `xmin`s of committed versions, for which `Committed` is
///   the truth. A usable snapshot (`xmin >= horizon`) therefore gets the same answers as
///   before the pruning, which is what the contract of [`Storage::vacuum`] demands.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    inner: RwLock<Inner>,
}

/// A database: its name, as given, and the tables it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Database {
    /// The name given to [`Storage::create_database`], stored verbatim.
    pub(crate) name: String,
    /// The tables of the database, kept in increasing [`TableId`] order.
    pub(crate) tables: BTreeSet<TableId>,
}

/// Everything behind the lock.
#[derive(Debug, Default)]
struct Inner {
    databases: BTreeMap<DbId, Database>,
    tables: BTreeMap<TableId, Table>,
    /// The indexes of every table; each table also lists its own ids
    /// ([`Table::indexes`]).
    indexes: BTreeMap<IndexId, Index>,
    txns: HashMap<TxnId, TxnState>,
    /// Counters for fresh identifiers. They only grow: an id is never reused, even after
    /// `drop_database`/`drop_table`/`drop_index`.
    next_db_id: u32,
    next_table_id: u32,
    next_index_id: u32,
    /// Counter for [`SavepointId`]s. Global to the instance, which makes every savepoint
    /// unique and increasing within its transaction; ownership is checked by looking the
    /// id up in the transaction's own list.
    next_savepoint_id: u64,
}

/// A caller bug: a violated precondition of the [`Storage`] contract.
fn bug<T>(msg: impl Into<String>) -> SqlResult<T> {
    Err(InternalError::Bug(msg.into()).into())
}

/// A broken invariant of this module: never repaired blindly, always reported.
fn corruption<T>(msg: impl Into<String>) -> SqlResult<T> {
    Err(InternalError::Corruption(msg.into()).into())
}

/// The error reported when another thread panicked while holding the lock.
fn poisoned() -> SqlError {
    InternalError::Corruption("storage lock poisoned".into()).into()
}

impl MemoryStorage {
    /// An empty storage: no database, no table, no index, no transaction known.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                databases: BTreeMap::new(),
                tables: BTreeMap::new(),
                indexes: BTreeMap::new(),
                txns: HashMap::new(),
                next_db_id: 1,
                next_table_id: 1,
                next_index_id: 1,
                next_savepoint_id: 1,
            }),
        }
    }

    fn read(&self) -> SqlResult<RwLockReadGuard<'_, Inner>> {
        self.inner.read().map_err(|_| poisoned())
    }

    fn write(&self) -> SqlResult<RwLockWriteGuard<'_, Inner>> {
        self.inner.write().map_err(|_| poisoned())
    }
}

/// The status of `t` as [`Snapshot::is_visible`] must see it: the registered status if the
/// registry knows `t`, `Committed` otherwise ("Transaction statuses" on [`MemoryStorage`]).
fn status_in(txns: &HashMap<TxnId, TxnState>, t: TxnId) -> TxnStatus {
    txns.get(&t)
        .map_or(TxnStatus::Committed, |state| state.status)
}

/// An index entry prepared before any mutation: the index it goes to, its position there
/// and the entry itself.
type PlannedEntry = (IndexId, usize, IndexEntry);

/// Inserts entries prepared by [`Inner::plan_entries`] under the same write lock, so that
/// their positions are still exact. An index dropped in between cannot exist here; the
/// position is clamped anyway rather than trusted blindly.
fn apply_entries(indexes: &mut BTreeMap<IndexId, Index>, planned: Vec<PlannedEntry>) {
    for (id, pos, entry) in planned {
        if let Some(index) = indexes.get_mut(&id) {
            let pos = pos.min(index.entries.len());
            index.entries.insert(pos, entry);
        }
    }
}

impl Inner {
    /// [`status_in`] on this registry.
    fn status(&self, t: TxnId) -> TxnStatus {
        status_in(&self.txns, t)
    }

    fn database_mut(&mut self, db: DbId) -> SqlResult<&mut Database> {
        match self.databases.get_mut(&db) {
            Some(database) => Ok(database),
            None => bug(format!("unknown database {db}")),
        }
    }

    fn table(&self, table: TableId) -> SqlResult<&Table> {
        match self.tables.get(&table) {
            Some(t) => Ok(t),
            None => bug(format!("unknown table {table}")),
        }
    }

    fn index(&self, index: IndexId) -> SqlResult<&Index> {
        match self.indexes.get(&index) {
            Some(i) => Ok(i),
            None => bug(format!("unknown index {index}")),
        }
    }

    /// Refuses a transaction that already committed or rolled back, without registering
    /// anything: the check of [`Inner::writing_txn`] for a call that may still be refused
    /// for another reason (a duplicate key) and must then leave no trace.
    fn check_writable(&self, txn: TxnId) -> SqlResult<()> {
        match self.txns.get(&txn) {
            Some(state) if state.is_finished() => {
                bug(format!("transaction {txn} is already {:?}", state.status))
            }
            _ => Ok(()),
        }
    }

    /// The state of a transaction about to write: registers it as `InProgress` on its first
    /// write, refuses a transaction that already committed or rolled back.
    fn writing_txn(&mut self, txn: TxnId) -> SqlResult<&mut TxnState> {
        let state = self.txns.entry(txn).or_insert_with(TxnState::in_progress);
        if state.is_finished() {
            return bug(format!("transaction {txn} is already {:?}", state.status));
        }
        Ok(state)
    }

    /// Marks `txn` as finished with `status` and hands back its undo log; its savepoints
    /// become invalid. A transaction the storage never saw wrote nothing: it is registered
    /// with `status` and an empty log so that a second `commit`/`rollback` is reported as a
    /// bug like for any other.
    fn finish_txn(&mut self, txn: TxnId, status: TxnStatus) -> SqlResult<Vec<UndoEntry>> {
        match self.txns.get_mut(&txn) {
            Some(state) if state.is_finished() => {
                bug(format!("transaction {txn} is already {:?}", state.status))
            }
            Some(state) => {
                state.status = status;
                state.savepoints.clear();
                Ok(std::mem::take(&mut state.writes))
            }
            None => {
                self.txns.insert(txn, TxnState::finished(status));
                Ok(Vec::new())
            }
        }
    }

    /// Applies one undo entry of `txn`. A table dropped since the write is ignored silently,
    /// as the contracts of [`Storage::rollback`] and [`Storage::rollback_to`] require. A row
    /// whose chain no longer ends with the write being undone breaks the invariant described
    /// on [`UndoEntry`]: reported as `InternalError::Corruption`, never repaired blindly.
    /// The index entries of the versions taken away are removed.
    fn undo(&mut self, txn: TxnId, entry: UndoEntry) -> SqlResult<()> {
        match entry {
            UndoEntry::Insert { table, row } => {
                let removed = self.tables.get_mut(&table).and_then(|t| t.remove_row(row));
                if let Some(versions) = removed {
                    let removed: Vec<(RowId, Version)> =
                        versions.into_iter().map(|v| (row, v)).collect();
                    self.remove_entries(table, &removed)?;
                }
                Ok(())
            }
            UndoEntry::Update { table, row } => {
                let Some(t) = self.tables.get_mut(&table) else {
                    return Ok(());
                };
                match t.undo_update(txn, row) {
                    Some(created) => self.remove_entries(table, &[(row, created)]),
                    None => corruption(format!(
                        "row {row} of table {table} does not end with the update of \
                         transaction {txn} being undone"
                    )),
                }
            }
            UndoEntry::Delete { table, row } => {
                if self
                    .tables
                    .get_mut(&table)
                    .is_none_or(|t| t.undo_delete(txn, row))
                {
                    Ok(())
                } else {
                    corruption(format!(
                        "row {row} of table {table} does not end with the delete of \
                         transaction {txn} being undone"
                    ))
                }
            }
        }
    }

    /// Removes the entries of `removed` versions from every index of `table`. A table or an
    /// index dropped in the meantime is ignored silently.
    fn remove_entries(&mut self, table: TableId, removed: &[(RowId, Version)]) -> SqlResult<()> {
        let Inner {
            tables, indexes, ..
        } = self;
        let Some(t) = tables.get(&table) else {
            return Ok(());
        };
        for id in &t.indexes {
            let Some(index) = indexes.get_mut(id) else {
                continue;
            };
            for (row, version) in removed {
                index.remove_version(*row, version)?;
            }
        }
        Ok(())
    }

    /// Prepares the index entries of a version about to be created for row `id` of `table`
    /// with content `row` and serial `seq`, on behalf of `txn`: computes each key, checks
    /// uniqueness ("Duplicate keys" on [`Storage`]) and the position of each entry.
    /// Nothing is modified: a 2601 leaves no trace. `replacing` is the row an `update`
    /// supersedes, whose versions are not live for `txn`
    /// ([`Index::has_live_duplicate`]).
    fn plan_entries(
        &self,
        txn: TxnId,
        table: TableId,
        row: &Row,
        id: RowId,
        seq: usize,
        replacing: Option<RowId>,
    ) -> SqlResult<Vec<PlannedEntry>> {
        let t = self.table(table)?;
        let status = |x| self.status(x);
        let mut planned = Vec::with_capacity(t.indexes.len());
        for index_id in &t.indexes {
            let Some(index) = self.indexes.get(index_id) else {
                return corruption(format!("table {table} references unknown index {index_id}"));
            };
            let key = index.order.extract(row)?;
            if index.shape.unique
                && index.has_live_duplicate(&key, &t.rows, Some(txn), &status, replacing)?
            {
                return Err(SqlError::duplicate_key_index(
                    &table.to_string(),
                    &index_id.to_string(),
                    &key_text(&key),
                ));
            }
            let pos = index.position_for(&key, id, seq)?;
            planned.push((
                *index_id,
                pos,
                IndexEntry {
                    key,
                    row: id,
                    version: seq,
                },
            ));
        }
        Ok(planned)
    }

    /// The precondition shared by [`Storage::update`] and [`Storage::delete`]: `table` is
    /// known, `id` exists in it and its most recent version has no `xmax`.
    ///
    /// The creator of that version must also be `txn` itself or a `Committed` transaction.
    /// A latest version created by another transaction still in progress means that
    /// transaction holds the row lock of `id` in `txn`: `check_write_conflict` never answers
    /// `Proceed` on an unsettled writer, so a second writer is a caller bug, the same one
    /// the `xmax` check catches after a `delete`. Refusing it is also what keeps the undo
    /// log sound ([`UndoEntry`]): the writes of a transaction on a row stay the tail of the
    /// row's chain until that transaction finishes.
    fn check_current(&self, txn: TxnId, table: TableId, id: RowId) -> SqlResult<()> {
        match self.table(table)?.latest(id) {
            None => bug(format!("unknown row {id} in table {table}")),
            Some(Version { xmax: Some(x), .. }) => bug(format!(
                "row {id} of table {table} is stale: its latest version was replaced or \
                 deleted by transaction {x}"
            )),
            Some(Version { xmin, .. })
                if *xmin != txn && self.status(*xmin) != TxnStatus::Committed =>
            {
                bug(format!(
                    "row {id} of table {table} is being written by transaction {xmin}, \
                     which is {:?}",
                    self.status(*xmin)
                ))
            }
            Some(_) => Ok(()),
        }
    }

    /// Checks the arity of `row` against `table`.
    fn check_arity(&self, table: TableId, row: &Row) -> SqlResult<()> {
        let arity = self.table(table)?.arity();
        if row.0.len() != arity {
            return bug(format!(
                "row has {} values, table {table} has {arity} columns",
                row.0.len()
            ));
        }
        Ok(())
    }

    /// The version of `versions` visible to `snap`, if any (at most one by the chain
    /// invariant).
    fn visible<'v>(&self, snap: &Snapshot, versions: &'v [Version]) -> Option<&'v Version> {
        let status = |t| self.status(t);
        versions
            .iter()
            .find(|v| snap.is_visible(v.xmin, v.xmax, &status))
    }

    /// Checks the preconditions of [`Storage::create_table`] on `shape` and builds the
    /// comparator of its clustered key, if any.
    fn check_shape(shape: &TableShape) -> SqlResult<Option<KeyOrder>> {
        if shape.columns.is_empty() {
            return bug("table shape has no column");
        }
        match &shape.clustered_key {
            Some(key) => Ok(Some(KeyOrder::new("clustered key", key, shape)?)),
            None => Ok(None),
        }
    }
}

/// Hands out the next value of a `u32` id counter without ever wrapping around.
fn next_id(counter: &mut u32, what: &str) -> SqlResult<u32> {
    let id = *counter;
    match id.checked_add(1) {
        Some(next) => {
            *counter = next;
            Ok(id)
        }
        None => bug(format!("{what} id space exhausted")),
    }
}

impl Storage for MemoryStorage {
    // ---------------------------------------------------------------- Databases

    fn create_database(&self, name: &str) -> SqlResult<DbId> {
        let mut inner = self.write()?;
        let id = DbId(next_id(&mut inner.next_db_id, "database")?);
        inner.databases.insert(
            id,
            Database {
                name: name.to_owned(),
                tables: BTreeSet::new(),
            },
        );
        Ok(id)
    }

    fn drop_database(&self, db: DbId) -> SqlResult<()> {
        let mut inner = self.write()?;
        let Some(database) = inner.databases.remove(&db) else {
            return bug(format!("unknown database {db}"));
        };
        for table in database.tables {
            if let Some(dropped) = inner.tables.remove(&table) {
                for index in dropped.indexes {
                    inner.indexes.remove(&index);
                }
            }
        }
        Ok(())
    }

    fn databases(&self) -> SqlResult<Vec<(DbId, String)>> {
        let inner = self.read()?;
        Ok(inner
            .databases
            .iter()
            .map(|(id, database)| (*id, database.name.clone()))
            .collect())
    }

    // ------------------------------------------------------- Tables and indexes

    fn create_table(&self, db: DbId, shape: &TableShape) -> SqlResult<TableId> {
        let clustered = Inner::check_shape(shape)?;
        let mut inner = self.write()?;
        if !inner.databases.contains_key(&db) {
            return bug(format!("unknown database {db}"));
        }
        let id = TableId(next_id(&mut inner.next_table_id, "table")?);
        inner.database_mut(db)?.tables.insert(id);
        inner
            .tables
            .insert(id, Table::new(db, shape.clone(), clustered));
        Ok(id)
    }

    fn drop_table(&self, table: TableId) -> SqlResult<()> {
        let mut inner = self.write()?;
        let Some(dropped) = inner.tables.remove(&table) else {
            return bug(format!("unknown table {table}"));
        };
        if let Some(database) = inner.databases.get_mut(&dropped.db) {
            database.tables.remove(&table);
        }
        for index in dropped.indexes {
            inner.indexes.remove(&index);
        }
        Ok(())
    }

    fn tables(&self, db: DbId) -> SqlResult<Vec<(TableId, TableShape)>> {
        let inner = self.read()?;
        let Some(database) = inner.databases.get(&db) else {
            return bug(format!("unknown database {db}"));
        };
        let mut out = Vec::with_capacity(database.tables.len());
        for id in &database.tables {
            let Some(table) = inner.tables.get(id) else {
                return corruption(format!("database {db} references unknown table {id}"));
            };
            out.push((*id, table.shape.clone()));
        }
        Ok(out)
    }

    fn create_index(&self, table: TableId, def: &IndexShape) -> SqlResult<IndexId> {
        let mut inner = self.write()?;
        let (id, next, index) = {
            let t = inner.table(table)?;
            let order = KeyOrder::new("index key", &def.columns, &t.shape)?;
            if let Some(inc) = def
                .included
                .iter()
                .find(|inc| usize::from(**inc) >= t.arity())
            {
                return bug(format!(
                    "included column {inc} out of range ({} columns)",
                    t.arity()
                ));
            }
            // The id the index receives, or would have received in the 2601 message.
            let Some(next) = inner.next_index_id.checked_add(1) else {
                return bug("index id space exhausted");
            };
            let id = IndexId(inner.next_index_id);
            // Index every existing version whose creator has not aborted, one entry each.
            let mut index = Index::new(table, def.clone(), order);
            let status = |x| inner.status(x);
            for (row, versions) in &t.rows {
                for v in versions {
                    if status(v.xmin) == TxnStatus::Aborted {
                        continue;
                    }
                    let key = index.order.extract(&v.data)?;
                    let pos = index.position_for(&key, *row, v.seq)?;
                    index.entries.insert(
                        pos,
                        IndexEntry {
                            key,
                            row: *row,
                            version: v.seq,
                        },
                    );
                }
            }
            if def.unique
                && let Some(key) = index.first_duplicate_key(&t.rows, &status)?
            {
                return Err(SqlError::duplicate_key_index(
                    &table.to_string(),
                    &id.to_string(),
                    &key_text(&key),
                ));
            }
            (id, next, index)
        };
        let Some(t) = inner.tables.get_mut(&table) else {
            return corruption(format!("table {table} vanished under the write lock"));
        };
        t.indexes.insert(id);
        inner.next_index_id = next;
        inner.indexes.insert(id, index);
        Ok(id)
    }

    fn drop_index(&self, index: IndexId) -> SqlResult<()> {
        let mut inner = self.write()?;
        let Some(dropped) = inner.indexes.remove(&index) else {
            return bug(format!("unknown index {index}"));
        };
        if let Some(t) = inner.tables.get_mut(&dropped.table) {
            t.indexes.remove(&index);
        }
        Ok(())
    }

    fn indexes(&self, table: TableId) -> SqlResult<Vec<(IndexId, IndexShape)>> {
        let inner = self.read()?;
        let t = inner.table(table)?;
        let mut out = Vec::with_capacity(t.indexes.len());
        for id in &t.indexes {
            let Some(index) = inner.indexes.get(id) else {
                return corruption(format!("table {table} references unknown index {id}"));
            };
            out.push((*id, index.shape.clone()));
        }
        Ok(out)
    }

    // ---------------------------------------------------------------------- Rows

    fn insert(&self, txn: TxnId, table: TableId, row: &Row) -> SqlResult<RowId> {
        let mut inner = self.write()?;
        inner.check_arity(table, row)?;
        // Check the transaction before anything else: a finished `txn` writes nothing, and
        // a refused call registers nothing.
        inner.check_writable(txn)?;
        let (id, seq) = {
            let t = inner.table(table)?;
            (t.peek_row_id(), t.peek_seq())
        };
        let planned = inner.plan_entries(txn, table, row, id, seq, None)?;
        inner.writing_txn(txn)?;
        let Inner {
            tables,
            indexes,
            txns,
            ..
        } = &mut *inner;
        let (Some(t), Some(state)) = (tables.get_mut(&table), txns.get_mut(&txn)) else {
            return corruption(format!(
                "table {table} or transaction {txn} vanished under the write lock"
            ));
        };
        let inserted = t.insert(txn, row.clone());
        if inserted != id {
            return corruption(format!(
                "table {table} handed out row {inserted} instead of the announced {id}"
            ));
        }
        apply_entries(indexes, planned);
        state.writes.push(UndoEntry::Insert { table, row: id });
        Ok(id)
    }

    fn update(&self, txn: TxnId, table: TableId, id: RowId, row: &Row) -> SqlResult<()> {
        let mut inner = self.write()?;
        inner.check_arity(table, row)?;
        inner.check_current(txn, table, id)?;
        inner.check_writable(txn)?;
        let seq = inner.table(table)?.peek_seq();
        // The replaced version is not live for `txn`: an update that keeps the key passes.
        let planned = inner.plan_entries(txn, table, row, id, seq, Some(id))?;
        inner.writing_txn(txn)?;
        let Inner {
            tables,
            indexes,
            txns,
            ..
        } = &mut *inner;
        let (Some(t), Some(state)) = (tables.get_mut(&table), txns.get_mut(&txn)) else {
            return corruption(format!(
                "table {table} or transaction {txn} vanished under the write lock"
            ));
        };
        if !t.supersede(txn, id, Some(row.clone())) {
            return corruption(format!(
                "row {id} of table {table} vanished under the write lock"
            ));
        }
        apply_entries(indexes, planned);
        state.writes.push(UndoEntry::Update { table, row: id });
        Ok(())
    }

    fn delete(&self, txn: TxnId, table: TableId, id: RowId) -> SqlResult<()> {
        let mut inner = self.write()?;
        inner.check_current(txn, table, id)?;
        inner.writing_txn(txn)?;
        let Inner { tables, txns, .. } = &mut *inner;
        let (Some(t), Some(state)) = (tables.get_mut(&table), txns.get_mut(&txn)) else {
            return corruption(format!(
                "table {table} or transaction {txn} vanished under the write lock"
            ));
        };
        // No index maintenance: the entries of the deleted version stay until `vacuum`,
        // visibility filters them out.
        if !t.supersede(txn, id, None) {
            return corruption(format!(
                "row {id} of table {table} vanished under the write lock"
            ));
        }
        state.writes.push(UndoEntry::Delete { table, row: id });
        Ok(())
    }

    fn get(&self, snap: &Snapshot, table: TableId, id: RowId) -> SqlResult<Option<Row>> {
        let inner = self.read()?;
        let t = inner.table(table)?;
        Ok(t.rows
            .get(&id)
            .and_then(|versions| inner.visible(snap, versions))
            .map(|v| v.data.clone()))
    }

    fn scan(&self, snap: &Snapshot, table: TableId) -> SqlResult<Box<dyn RowIter + '_>> {
        // Copy the visible rows under the lock, then release it: the iterator must not hold
        // the lock, and the copy gives the isolation the contract requires from other
        // transactions' later writes.
        let rows: Vec<(RowId, Row)> = {
            let inner = self.read()?;
            let t = inner.table(table)?;
            let visible: Vec<(RowId, Row)> = t
                .rows
                .iter()
                .filter_map(|(id, versions)| {
                    inner.visible(snap, versions).map(|v| (*id, v.data.clone()))
                })
                .collect();
            match &t.clustered {
                Some(order) => sort_rows(order, visible)?,
                None => visible,
            }
        };
        Ok(Box::new(rows.into_iter().map(Ok::<_, SqlError>)))
    }

    fn seek(
        &self,
        snap: &Snapshot,
        index: IndexId,
        range: &KeyRange,
        dir: Direction,
    ) -> SqlResult<Box<dyn RowIter + '_>> {
        // Same discipline as `scan`: select and copy under the lock, iterate without it.
        let rows: Vec<(RowId, Row)> = {
            let inner = self.read()?;
            let idx = inner.index(index)?;
            let Some(t) = inner.tables.get(&idx.table) else {
                return corruption(format!(
                    "index {index} belongs to unknown table {}",
                    idx.table
                ));
            };
            let positions = idx.positions(range)?;
            let status = |x| inner.status(x);
            let mut out = Vec::new();
            for entry in idx.entries.get(positions).unwrap_or_default() {
                let Some(v) = find_version(&t.rows, entry.row, entry.version) else {
                    return corruption(format!(
                        "index {index} references version {} of row {}, which does not exist",
                        entry.version, entry.row
                    ));
                };
                if snap.is_visible(v.xmin, v.xmax, &status) {
                    out.push((entry.row, v.data.clone()));
                }
            }
            if dir == Direction::Backward {
                out.reverse();
            }
            out
        };
        Ok(Box::new(rows.into_iter().map(Ok::<_, SqlError>)))
    }

    fn latest_version(&self, table: TableId, id: RowId) -> SqlResult<Option<(TxnId, Row)>> {
        let inner = self.read()?;
        // No snapshot involved: the last version of the chain, whatever its statuses. The
        // writer of the most recent state is the deleter if there is one, else the creator.
        Ok(inner
            .table(table)?
            .latest(id)
            .map(|v| (v.xmax.unwrap_or(v.xmin), v.data.clone())))
    }

    // ------------------------------------------------- Transaction life cycle

    fn commit(&self, txn: TxnId) -> SqlResult<()> {
        let mut inner = self.write()?;
        // The undo log is simply dropped: the versions and their index entries stay, with
        // `txn` now `Committed`. Entries on tables or indexes dropped in the meantime need
        // no attention either.
        inner.finish_txn(txn, TxnStatus::Committed)?;
        Ok(())
    }

    fn rollback(&self, txn: TxnId) -> SqlResult<()> {
        let mut inner = self.write()?;
        let writes = inner.finish_txn(txn, TxnStatus::Aborted)?;
        for entry in writes.into_iter().rev() {
            inner.undo(txn, entry)?;
        }
        Ok(())
    }

    fn savepoint(&self, txn: TxnId) -> SqlResult<SavepointId> {
        let mut inner = self.write()?;
        // Register an unknown transaction before its first write: `SAVE TRANSACTION` may
        // come first. A finished one is refused.
        inner.writing_txn(txn)?;
        let id = inner.next_savepoint_id;
        let Some(next) = id.checked_add(1) else {
            return bug("savepoint id space exhausted");
        };
        inner.next_savepoint_id = next;
        let Some(state) = inner.txns.get_mut(&txn) else {
            return corruption(format!("transaction {txn} vanished under the write lock"));
        };
        let sp = SavepointId(id);
        state.savepoints.push((sp, state.writes.len()));
        Ok(sp)
    }

    fn rollback_to(&self, txn: TxnId, sp: SavepointId) -> SqlResult<()> {
        let mut inner = self.write()?;
        let undone = {
            // A transaction the storage never saw has no savepoint: `sp` cannot be valid,
            // so there is nothing to register.
            let Some(state) = inner.txns.get_mut(&txn) else {
                return bug(format!("unknown transaction {txn} for savepoint {sp}"));
            };
            if state.is_finished() {
                return bug(format!("transaction {txn} is already {:?}", state.status));
            }
            // Ownership check: a savepoint of another transaction, or one invalidated by an
            // earlier `rollback_to`, is not in this list.
            let found = state
                .savepoints
                .iter()
                .enumerate()
                .find(|(_, (id, _))| *id == sp)
                .map(|(pos, (_, mark))| (pos, *mark));
            let Some((pos, mark)) = found else {
                return bug(format!(
                    "savepoint {sp} is unknown, invalidated or not owned by transaction {txn}"
                ));
            };
            if mark > state.writes.len() {
                return corruption(format!(
                    "savepoint {sp} of transaction {txn} points past its undo log"
                ));
            }
            // `sp` stays valid (kept at `pos`), the later ones are invalidated.
            state.savepoints.truncate(pos + 1);
            state.writes.split_off(mark)
        };
        for entry in undone.into_iter().rev() {
            inner.undo(txn, entry)?;
        }
        Ok(())
    }

    fn checkpoint(&self) -> SqlResult<()> {
        // Nothing to flush: memory is the only medium.
        Ok(())
    }

    fn vacuum(&self, horizon: TxnId) -> SqlResult<()> {
        let mut inner = self.write()?;
        let Inner {
            tables,
            indexes,
            txns,
            ..
        } = &mut *inner;
        // Passes 1 and 2 (versions) consult the registry, so they run before pass 3 prunes
        // it. Versions with `xmax = None`, with an `InProgress` `xmax`, or with an `xmax >=
        // horizon` are untouched by construction; freed `RowId`s are never reused because
        // `Table::next_row_id` only grows. The index entries of every discarded version go
        // with it.
        let status = |t| status_in(txns, t);
        for table in tables.values_mut() {
            let removed = table.vacuum(horizon, &status);
            for id in &table.indexes {
                let Some(index) = indexes.get_mut(id) else {
                    continue;
                };
                for (row, version) in &removed {
                    index.remove_version(*row, version)?;
                }
            }
        }
        // Pass 3: forget the finished transactions below the horizon. From now on they are
        // reported as `Committed`, which is exact ("Transaction statuses" on the type).
        txns.retain(|t, state| !(state.is_finished() && *t < horizon));
        Ok(())
    }
}
