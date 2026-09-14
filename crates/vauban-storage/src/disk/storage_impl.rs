//! The rows of [`DiskStorage`] behind [`Storage`]: the `impl` block itself, the store one
//! call is dispatched to ([`Store`]), and the index maintenance that goes with each write.
//!
//! # What this file does and what the two stores do
//!
//! [`super::version::HeapTable`] and [`super::clustered::ClusteredTable`] hold the pages and
//! the versions; neither of them is a field of [`DiskStorage`], which carries no lifetime
//! parameter. What the instance keeps between
//! two calls is [`super::version::TableState`] — the counters, the directory, the undo logs
//! and the index maintenance log — and [`super::TxnRegistry`], the status of each
//! transaction. [`DiskStorage::with_rows`] locks the state of one table, builds the store
//! around `&self` and that state, runs the call, and hands the state back.
//!
//! # One `Begin` and one end record per transaction
//!
//! The register of the transactions is per instance: [`DiskStorage::begin_txn`] appends the
//! `Begin` of the first write of a transaction, whichever table it lands in, and
//! [`DiskStorage::end_txn`] appends its `Commit` **or** its `Abort` once
//! (`super::tests::one_begin_one_end_per_txn_over_two_tables`). [`Storage::commit`] and
//! [`Storage::rollback`] then walk the tables the transaction wrote in and let each of them
//! drop its undo log or replay it backwards.
//!
//! # Order of the checks of a write
//!
//! The preconditions of the store are reported **before** the uniqueness of an index, which is
//! the order [`crate::MemoryStorage`] follows: [`super::version::HeapTable::check_update`] and
//! [`super::version::HeapTable::check_insert`] run first, the `unique` indexes after them
//! (`super::tests::stale_version_is_refused_before_uniqueness`,
//! `super::tests::a_finished_txn_is_refused_before_uniqueness`,
//! `super::tests::a_row_of_the_wrong_arity_is_refused_before_uniqueness`). The other order
//! would answer 2601 for a finished transaction where [`crate::MemoryStorage`] answers
//! `Bug("transaction 2 is already Committed")`, and [`InternalError::Corruption`] for a row of
//! the wrong arity where it answers a `Bug`.
//!
//! # Not implemented yet
//!
//! - `savepoint`, `rollback_to` and `vacuum` answer [`InternalError::Bug`]
//!   (`super::tests::savepoint_rollback_to_and_vacuum_are_not_implemented_on_disk`);
//! - the redo of the journal writes the rows of a **heap** back (`super::recover`); the
//!   entries of a clustered table and of an index tree come back through the pages
//!   themselves, which a checkpoint is what puts in `data`. With one row inserted and
//!   committed in each of the two stores, then reopened without a checkpoint: the heap serves
//!   its row, and the clustered table answers the [`InternalError::Corruption`] of a tree root
//!   read back as a free page
//!   (`super::tests::a_clustered_table_reopened_without_a_checkpoint_is_corruption`). The
//!   clustered half of `super::tests::insert_commit_reopen_get` therefore takes a checkpoint
//!   and the heap half does not. Replaying a tree, or rebuilding the indexes at `open`, is
//!   not implemented yet.

use std::sync::{Arc, Mutex};

use vauban_errors::{InternalError, SqlError, SqlResult};

use super::DiskStorage;
use super::clustered::ClusteredTable;
use super::heap::Heap;
use super::index::{self, DiskIndex, IndexChange, VersionSource};
use super::meta::{self, TableEntry};
use super::version::{HeapTable, TableState};
use crate::{
    DbId, Direction, IndexId, IndexShape, KeyRange, Row, RowId, RowIter, SavepointId, Snapshot,
    Storage, TableId, TableShape, TxnId, TxnStatus,
};

/// The store one call works on: the heap of a table without clustered key, the tree of a table
/// with one.
///
/// The two types answer the same row methods and are both a
/// [`super::index::VersionSource`], which is what lets one index serve either of them.
#[derive(Debug)]
pub(crate) enum Store<'storage> {
    /// A table without clustered key ([`super::version::HeapTable`]).
    Heap(HeapTable<'storage>),
    /// A table with a clustered key ([`super::clustered::ClusteredTable`]).
    Clustered(ClusteredTable<'storage>),
}

impl Store<'_> {
    /// Adds a logical row holding `row`, created by `txn`, and answers its fresh [`RowId`].
    fn insert(&mut self, txn: TxnId, row: &Row) -> Result<RowId, InternalError> {
        match self {
            Self::Heap(table) => table.insert(txn, row),
            Self::Clustered(table) => table.insert(txn, row),
        }
    }

    /// Replaces the content of `id` with `row`, keeping the [`RowId`].
    fn update(&mut self, txn: TxnId, id: RowId, row: &Row) -> Result<(), InternalError> {
        match self {
            Self::Heap(table) => table.update(txn, id, row),
            Self::Clustered(table) => table.update(txn, id, row),
        }
    }

    /// Checks the preconditions of an `insert` without writing anything.
    fn check_insert(&self, txn: TxnId, row: &Row) -> Result<(), InternalError> {
        match self {
            Self::Heap(table) => table.check_insert(txn, row),
            Self::Clustered(table) => table.check_insert(txn, row),
        }
    }

    /// Checks the preconditions of an `update` without writing anything.
    fn check_update(&self, txn: TxnId, id: RowId, row: &Row) -> Result<(), InternalError> {
        match self {
            Self::Heap(table) => table.check_update(txn, id, row),
            Self::Clustered(table) => table.check_update(txn, id, row),
        }
    }

    /// Deletes the logical row `id`.
    fn delete(&mut self, txn: TxnId, id: RowId) -> Result<(), InternalError> {
        match self {
            Self::Heap(table) => table.delete(txn, id),
            Self::Clustered(table) => table.delete(txn, id),
        }
    }

    /// The content of `id` as `snap` sees it.
    fn get(&self, snap: &Snapshot, id: RowId) -> Result<Option<Row>, InternalError> {
        match self {
            Self::Heap(table) => table.get(snap, id),
            Self::Clustered(table) => table.get(snap, id),
        }
    }

    /// The rows `snap` sees: in [`RowId`] order on a heap, in clustered-key order on a tree.
    fn scan(&self, snap: &Snapshot) -> Result<Vec<(RowId, Row)>, InternalError> {
        match self {
            Self::Heap(table) => table.scan(snap),
            Self::Clustered(table) => table.scan(snap),
        }
    }

    /// The tail of the chain of `id`, regardless of the statuses.
    fn latest_version(&self, id: RowId) -> Result<Option<(TxnId, Row)>, InternalError> {
        match self {
            Self::Heap(table) => table.latest_version(id),
            Self::Clustered(table) => table.latest_version(id),
        }
    }

    /// Applies an end of transaction the instance has already journalled.
    fn finish(&mut self, txn: TxnId, status: TxnStatus) -> Result<(), InternalError> {
        match self {
            Self::Heap(table) => table.finish(txn, status),
            Self::Clustered(table) => table.finish(txn, status),
        }
    }

    /// Takes the maintenance log of the writes made since the previous call.
    fn take_index_changes(&mut self) -> Vec<IndexChange> {
        match self {
            Self::Heap(table) => table.take_index_changes(),
            Self::Clustered(table) => table.take_index_changes(),
        }
    }

    /// The store as the index layer reads it.
    pub(crate) fn source(&self) -> &dyn VersionSource {
        match self {
            Self::Heap(table) => table,
            Self::Clustered(table) => table,
        }
    }

    /// The next [`RowId`] the store hands out.
    fn next_row_id(&self) -> u64 {
        match self {
            Self::Heap(table) => table.next_row_id(),
            Self::Clustered(table) => table.next_row_id(),
        }
    }

    /// The roots the catalogue keeps for this table: the tree of the versions and the
    /// directory, `None` on a heap.
    fn roots(&self) -> (Option<super::PageId>, Option<super::PageId>) {
        match self {
            Self::Heap(_) => (None, None),
            Self::Clustered(table) => (Some(table.tree_root()), Some(table.directory_root())),
        }
    }

    /// Hands the state of the table back to the instance.
    fn into_state(self) -> TableState {
        match self {
            Self::Heap(table) => table.into_state(),
            Self::Clustered(table) => table.into_state(),
        }
    }
}

impl DiskStorage {
    /// Runs `f` on the store of `table`, the state of that table locked for the call.
    ///
    /// The state is moved into the store at the entry and moved back out at the return,
    /// whether `f` answered `Ok` or `Err`: the counters and the undo logs of a refused write
    /// stay where they were, and the identifiers a refused write took are not handed out again
    /// (`super::version::HeapTable::insert`). The lock of the cell is what serialises two
    /// calls on one table; two calls on two tables take two different cells.
    ///
    /// The state of a table is rebuilt from what the recovery of the `open` found the first
    /// time it is reached ([`super::recover::Recovery`]).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a table the catalogue does not hold; the errors of `f`
    /// otherwise.
    pub(crate) fn with_rows<R>(
        &self,
        table: TableId,
        f: impl FnOnce(&mut Store<'_>) -> SqlResult<R>,
    ) -> SqlResult<R> {
        let entry = self.table_entry(table)?;
        let (outcome, after) = self.with_rows_of(&entry, f)?;
        self.note_table_shape(&entry, after)?;
        outcome
    }

    /// Runs `f` on the store of `entry` and answers what it said together with the counters
    /// the call left, without touching the catalogue.
    ///
    /// The caller that already holds the lock of the catalogue goes through this one:
    /// [`DiskStorage::create_index`] does, the lock of the catalogue being what serialises two
    /// DDL statements, and [`std::sync::Mutex`] not being re-entrant.
    ///
    /// # Errors
    ///
    /// The errors of [`DiskStorage::store_of`] and of the rebuild; the error of `f` is handed
    /// back inside the answer, so that the state of the table is put back either way.
    #[allow(clippy::type_complexity)]
    pub(crate) fn with_rows_of<R>(
        &self,
        entry: &TableEntry,
        f: impl FnOnce(&mut Store<'_>) -> SqlResult<R>,
    ) -> SqlResult<(
        SqlResult<R>,
        (u64, (Option<super::PageId>, Option<super::PageId>)),
    )> {
        let cell = self.table_state(entry.table);
        let mut guard = lock_state(&cell);
        let state = std::mem::take(&mut *guard);
        let mut store = match self.store_of(entry, state) {
            Ok(store) => store,
            // The state was moved into the call: a failure hands it back and it goes into the
            // cell again, rather than leaving a `TableState::default()` there with the undo
            // logs and the `in_progress` pages of the table lost
            // (`super::tests::a_failed_store_of_leaves_the_state_of_the_table_in_place`).
            Err((state, err)) => {
                *guard = state;
                return Err(err.into());
            }
        };
        let outcome = self.resume(&mut store).and_then(|()| f(&mut store));
        let after = (store.next_row_id(), store.roots());
        *guard = store.into_state();
        Ok((outcome, after))
    }

    /// The store of `entry`, built around `&self` and the state the instance kept.
    ///
    /// # Errors
    ///
    /// The state is handed back with the error: the caller took it out of the cell of the
    /// table before the call, and a failure here must not leave that cell empty.
    /// [`InternalError::Corruption`] for a clustered entry the catalogue holds without its two
    /// roots; the errors of [`ClusteredTable::open`] otherwise.
    // The `Err` variant carries the state back to its cell, which is the point of this
    // signature; boxing it would allocate on a path that must not lose it.
    #[allow(clippy::result_large_err)]
    fn store_of<'a>(
        &'a self,
        entry: &TableEntry,
        state: TableState,
    ) -> Result<Store<'a>, (TableState, InternalError)> {
        if entry.shape.clustered_key.is_none() {
            let heap = Heap::open(self, entry.table, entry.first_page);
            return Ok(Store::Heap(HeapTable::resume(
                self,
                heap,
                entry.shape.clone(),
                state,
            )));
        }
        let (Some(tree), Some(directory)) = (entry.tree_root, entry.directory_root) else {
            return Err((
                state,
                InternalError::Corruption(format!(
                    "table {} carries a clustered key and no tree in the catalogue of instance {}",
                    entry.table,
                    self.dir.display()
                )),
            ));
        };
        let table = match ClusteredTable::open(
            self,
            entry.table,
            entry.first_page,
            (tree, directory),
            entry.shape.clone(),
        ) {
            Ok(table) => table,
            Err(err) => return Err((state, err)),
        };
        Ok(Store::Clustered(table.with_state(state)))
    }

    /// Rebuilds the state of `store` from what the recovery of the `open` found, once per
    /// table and per instance.
    ///
    /// A heap walks its pages to rebuild its directory; a clustered table keeps its directory
    /// in a tree of the instance, so it takes the two counters and nothing else. Both take the counters of
    /// the instance, one past the largest identifier the journal names, so a `RowId` an
    /// interrupted insert took is not handed out a second time.
    fn resume(&self, store: &mut Store<'_>) -> SqlResult<()> {
        let recovery = self.recovery();
        match store {
            Store::Heap(table) => {
                if !table.state_needs_resume() {
                    return Ok(());
                }
                table.resume_after_recovery(
                    recovery.next_row_id,
                    recovery.next_seq,
                    &recovery.winners,
                    &recovery.losers,
                )?;
            }
            Store::Clustered(table) => {
                if !table.state_needs_resume() {
                    return Ok(());
                }
                self.register_recovered(&recovery.winners, &recovery.losers)?;
                table.resume_after_recovery(recovery.next_row_id, recovery.next_seq);
            }
        }
        Ok(())
    }

    /// Writes back in the catalogue what a call changed of the shape of a table: the next
    /// [`RowId`] and, when a split moved them, the roots of a clustered table.
    ///
    /// The entry in memory is the one the next call reads; the chain of meta pages takes it at
    /// the next `commit` ([`Storage::commit`]), which has a journal record to hang the write
    /// on.
    fn note_table_shape(
        &self,
        entry: &TableEntry,
        after: (u64, (Option<super::PageId>, Option<super::PageId>)),
    ) -> SqlResult<()> {
        let (next_row_id, (tree_root, directory_root)) = after;
        let clustered = entry.shape.clustered_key.is_some();
        let roots = if clustered {
            (tree_root, directory_root)
        } else {
            (entry.tree_root, entry.directory_root)
        };
        if entry.next_row_id == next_row_id
            && entry.tree_root == roots.0
            && entry.directory_root == roots.1
        {
            return Ok(());
        }
        let mut catalogue = self.lock_catalogue()?;
        if catalogue.table(entry.table).is_none() {
            return Ok(());
        }
        catalogue.add_table(TableEntry {
            next_row_id,
            tree_root: roots.0,
            directory_root: roots.1,
            ..entry.clone()
        });
        Ok(())
    }

    /// The catalogue entry of `table`, cloned so that the lock of the catalogue is not held
    /// while the rows are read or written.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a table the catalogue does not hold.
    fn table_entry(&self, table: TableId) -> Result<TableEntry, InternalError> {
        match self.lock_catalogue()?.table(table) {
            Some(entry) => Ok(entry.clone()),
            None => Err(InternalError::Bug(format!(
                "table {table} is unknown to instance {}",
                self.dir.display()
            ))),
        }
    }

    /// Refuses `row` when a `unique` index of `table` already holds its key on another live
    /// row, and answers 2601 with the identifier of that index.
    ///
    /// Called **before** the write it guards, so a 2601 leaves neither a version nor an entry.
    /// An index that shares the tree of a clustered table has no tree of its own and is
    /// checked over the versions ([`index::check_duplicate_without_tree`]).
    ///
    /// `indexes` is read by the caller before it takes the state of the table, so that the
    /// lock of the catalogue is not taken under the lock of a table: [`DiskStorage::create_index`]
    /// takes them the other way round, and taking both in both orders is how two threads stop.
    fn check_unique(
        &self,
        entry: &TableEntry,
        indexes: &[super::meta::IndexEntry],
        store: &Store<'_>,
        txn: TxnId,
        row: &Row,
        replacing: Option<RowId>,
    ) -> SqlResult<()> {
        for index in indexes {
            if !index.shape.unique {
                continue;
            }
            match index.root {
                Some(root) => {
                    DiskIndex::open(self, index.index, &index.shape, root, &entry.shape)?
                        .check_duplicate(store.source(), txn, row, replacing)?;
                }
                None => index::check_duplicate_without_tree(
                    store.source(),
                    index.index,
                    &index.shape,
                    txn,
                    row,
                    replacing,
                )?,
            }
        }
        Ok(())
    }

    /// Puts `changes` in the trees of the indexes of `table`, and writes a root a split moved
    /// back in the catalogue.
    ///
    /// An index that shares the tree of a clustered table takes nothing: the rows **are** its
    /// entries.
    fn maintain_indexes(&self, table: TableId, changes: &[IndexChange]) -> SqlResult<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let shape = self.table_entry(table)?.shape;
        for entry in self.index_entries(table)? {
            let Some(root) = entry.root else {
                continue;
            };
            let mut index = DiskIndex::open(self, entry.index, &entry.shape, root, &shape)?;
            index.apply(changes)?;
            if index.root() != root {
                let mut catalogue = self.lock_catalogue()?;
                catalogue.add_index(super::meta::IndexEntry {
                    root: Some(index.root()),
                    ..entry.clone()
                });
            }
        }
        Ok(())
    }

    /// The catalogue entries of the indexes of `table`, cloned as [`DiskStorage::table_entry`]
    /// clones its own.
    fn index_entries(&self, table: TableId) -> Result<Vec<super::meta::IndexEntry>, InternalError> {
        Ok(self
            .lock_catalogue()?
            .indexes_of(table)
            .into_iter()
            .cloned()
            .collect())
    }

    /// Runs one write on `table` and the index maintenance that follows it.
    ///
    /// The uniqueness of the `unique` indexes is checked before `write`, the maintenance log
    /// drained after it: a 2601 leaves neither a version nor an entry, and a write that went
    /// through reaches the trees of the indexes of its table before the call returns
    /// (`super::tests::index_seek_on_clustered_table` reads a `seek` after an `insert` and
    /// after an `update`).
    fn write_row<R>(
        &self,
        table: TableId,
        check: Check<'_>,
        write: impl FnOnce(&mut Store<'_>) -> SqlResult<R>,
    ) -> SqlResult<R> {
        let entry = self.table_entry(table)?;
        let indexes = self.index_entries(table)?;
        let (outcome, changes) = self.with_rows(table, |store| {
            check(self, &entry, &indexes, store)?;
            let outcome = write(store)?;
            Ok((outcome, store.take_index_changes()))
        })?;
        self.maintain_indexes(table, &changes)?;
        Ok(outcome)
    }

    /// Writes the catalogue to its chain of meta pages at `lsn`, so that the roots a split
    /// moved and the counters the writes advanced are found again after a reopen.
    fn store_catalogue(&self, lsn: super::Lsn) -> SqlResult<()> {
        let catalogue = self.lock_catalogue()?;
        let root = meta::ensure_root(self)?;
        meta::store(self, root, &catalogue, lsn)?;
        Ok(())
    }

    /// Ends `txn` with `kind` and applies that end to each table it wrote in.
    fn end_transaction(&self, txn: TxnId, kind: super::WalRecordKind) -> SqlResult<()> {
        let status = match kind {
            super::WalRecordKind::Abort => TxnStatus::Aborted,
            _ => TxnStatus::Committed,
        };
        let lsn = self.end_txn(txn, kind)?;
        let mut changed: Vec<(TableId, Vec<IndexChange>)> = Vec::new();
        for table in self.txn_tables(txn) {
            let changes = self.with_rows(table, |store| {
                store.finish(txn, status)?;
                Ok(store.take_index_changes())
            })?;
            changed.push((table, changes));
        }
        for (table, changes) in changed {
            self.maintain_indexes(table, &changes)?;
        }
        if let Some(lsn) = lsn {
            self.store_catalogue(lsn)?;
        }
        Ok(())
    }
}

/// What [`DiskStorage::write_row`] runs before the write it guards: the preconditions of the
/// call and the uniqueness of the indexes of the table, in that order.
type Check<'a> =
    &'a dyn Fn(&DiskStorage, &TableEntry, &[super::meta::IndexEntry], &Store<'_>) -> SqlResult<()>;

/// Takes the state of one table, taking the contents back from a thread that panicked while it
/// held the guard, as the other locks of this module do.
fn lock_state(cell: &Arc<Mutex<TableState>>) -> std::sync::MutexGuard<'_, TableState> {
    cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The rows of `rows` as [`Storage::scan`] and [`Storage::seek`] hand them back.
///
/// The rows are read and copied before the call returns, which is the isolation the trait asks
/// for: a write made after the call does not reach the caller.
fn iter(rows: Vec<(RowId, Row)>) -> Box<dyn RowIter + 'static> {
    Box::new(rows.into_iter().map(Ok::<_, SqlError>))
}

impl Storage for DiskStorage {
    fn create_database(&self, name: &str) -> SqlResult<DbId> {
        DiskStorage::create_database(self, name)
    }

    fn drop_database(&self, db: DbId) -> SqlResult<()> {
        DiskStorage::drop_database(self, db)
    }

    fn databases(&self) -> SqlResult<Vec<(DbId, String)>> {
        DiskStorage::databases(self)
    }

    fn create_table(&self, db: DbId, shape: &TableShape) -> SqlResult<TableId> {
        DiskStorage::create_table(self, db, shape)
    }

    fn drop_table(&self, table: TableId) -> SqlResult<()> {
        DiskStorage::drop_table(self, table)
    }

    fn tables(&self, db: DbId) -> SqlResult<Vec<(TableId, TableShape)>> {
        DiskStorage::tables(self, db)
    }

    fn create_index(&self, table: TableId, def: &IndexShape) -> SqlResult<IndexId> {
        DiskStorage::create_index(self, table, def)
    }

    fn drop_index(&self, index: IndexId) -> SqlResult<()> {
        DiskStorage::drop_index(self, index)
    }

    fn indexes(&self, table: TableId) -> SqlResult<Vec<(IndexId, IndexShape)>> {
        DiskStorage::indexes(self, table)
    }

    fn insert(&self, txn: TxnId, table: TableId, row: &Row) -> SqlResult<RowId> {
        self.write_row(
            table,
            &|storage, entry, indexes, store| {
                // Preconditions of the store first, uniqueness second, as on an `update` and
                // as `MemoryStorage::insert` does.
                store.check_insert(txn, row)?;
                storage.check_unique(entry, indexes, store, txn, row, None)
            },
            |store| Ok(store.insert(txn, row)?),
        )
    }

    fn update(&self, txn: TxnId, table: TableId, id: RowId, row: &Row) -> SqlResult<()> {
        self.write_row(
            table,
            &|storage, entry, indexes, store| {
                // Stale version first, uniqueness second: the order of `MemoryStorage`.
                store.check_update(txn, id, row)?;
                storage.check_unique(entry, indexes, store, txn, row, Some(id))
            },
            |store| Ok(store.update(txn, id, row)?),
        )
    }

    fn delete(&self, txn: TxnId, table: TableId, id: RowId) -> SqlResult<()> {
        self.write_row(table, &|_, _, _, _| Ok(()), |store| {
            Ok(store.delete(txn, id)?)
        })
    }

    fn get(&self, snap: &Snapshot, table: TableId, id: RowId) -> SqlResult<Option<Row>> {
        self.with_rows(table, |store| Ok(store.get(snap, id)?))
    }

    fn scan(&self, snap: &Snapshot, table: TableId) -> SqlResult<Box<dyn RowIter + '_>> {
        let rows = self.with_rows(table, |store| Ok(store.scan(snap)?))?;
        Ok(iter(rows))
    }

    fn seek(
        &self,
        snap: &Snapshot,
        index: IndexId,
        range: &KeyRange,
        dir: Direction,
    ) -> SqlResult<Box<dyn RowIter + '_>> {
        let entry = match self.lock_catalogue()?.index(index) {
            Some(entry) => entry.clone(),
            None => {
                return Err(InternalError::Bug(format!(
                    "index {index} is unknown to instance {}",
                    self.dir.display()
                ))
                .into());
            }
        };
        let shape = self.table_entry(entry.table)?.shape;
        let rows = self.with_rows(entry.table, |store| match entry.root {
            Some(root) => {
                let tree = DiskIndex::open(self, index, &entry.shape, root, &shape)?;
                tree.seek(store.source(), snap, range, dir)
            }
            // An index that shares the tree of its clustered table is served by that tree.
            None => match store {
                Store::Clustered(table) => Ok(table.seek(snap, range, dir)?),
                Store::Heap(_) => Err(InternalError::Corruption(format!(
                    "index {index} carries no tree and table {} is a heap",
                    entry.table
                ))
                .into()),
            },
        })?;
        Ok(iter(rows))
    }

    fn latest_version(&self, table: TableId, id: RowId) -> SqlResult<Option<(TxnId, Row)>> {
        self.with_rows(table, |store| Ok(store.latest_version(id)?))
    }

    fn commit(&self, txn: TxnId) -> SqlResult<()> {
        self.end_transaction(txn, super::WalRecordKind::Commit)
    }

    fn rollback(&self, txn: TxnId) -> SqlResult<()> {
        self.end_transaction(txn, super::WalRecordKind::Abort)
    }

    fn savepoint(&self, txn: TxnId) -> SqlResult<SavepointId> {
        Err(InternalError::Bug(format!(
            "savepoint of transaction {txn} on an on-disk instance is not implemented yet"
        ))
        .into())
    }

    fn rollback_to(&self, txn: TxnId, sp: SavepointId) -> SqlResult<()> {
        Err(InternalError::Bug(format!(
            "rollback of transaction {txn} to savepoint {sp} on an on-disk instance is not implemented yet"
        ))
        .into())
    }

    fn checkpoint(&self) -> SqlResult<()> {
        DiskStorage::checkpoint(self)?;
        Ok(())
    }

    fn vacuum(&self, horizon: TxnId) -> SqlResult<()> {
        Err(InternalError::Bug(format!(
            "vacuum below horizon {horizon} on an on-disk instance is not implemented yet"
        ))
        .into())
    }
}
