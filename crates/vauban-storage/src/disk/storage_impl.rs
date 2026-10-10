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
//! [`DiskStorage::end_txn_grouped`] appends its `Commit` **or** its `Abort` once
//! (`super::tests::one_begin_one_end_per_txn_over_two_tables`). [`Storage::commit`] and
//! [`Storage::rollback`] then walk the tables the transaction wrote in and let each of them
//! drop its undo log or replay it backwards.
//!
//! # Ending a transaction without holding the register across the sync
//!
//! The end record is appended under the lock of the register. The lock is then dropped, and
//! the call waits for the record to be durable through the group commit of the journal
//! ([`super::wal::WalHandle::wait_durable`]): the transactions that end at the same time
//! append their records while one sync runs, and share the next one
//! (`tests::sixteen_concurrent_commits_share_two_syncs`). [`Storage::commit`] returns once
//! the record is durable, and not before
//! (`tests::no_commit_returns_before_its_record_is_synced`). The tables are walked after that
//! wait, so the pages the transaction wrote keep their `in_progress` flag until its end is on
//! disk. The register says `Committed` or `Aborted` once the wait has returned `Ok`; when the
//! sync fails, the transaction stays `InProgress` with its pages and its undo log, so a
//! `rollback` of it goes through, and the `Abort` record it appends after the `Commit` one
//! decides the replay of the journal
//! (`tests::a_failed_commit_rolls_back_and_stays_rolled_back_after_a_reopen`).
//!
//! What hides a transaction from the readers while its record waits for the sync is the
//! snapshot they read through: the `txn` module keeps it in the `active` list of the
//! snapshots it builds until this call has returned.
//!
//! # Locks
//!
//! Two locks guard the rows: the one of the catalogue ([`DiskStorage::lock_catalogue`]) and
//! the one of the state of each table ([`DiskStorage::table_state`]), taken in that order,
//! which is the order [`DiskStorage::create_index`] takes them in too.
//!
//! - A **write** ([`DiskStorage::with_rows`], and through it `insert`, `update`, `delete`,
//!   `commit` and `rollback`) holds both for the whole call: it reads the entries of the table
//!   and of its indexes, checks the `unique` indexes, writes the version, puts the maintenance
//!   log in the trees of the indexes and writes back a root a split moved, as one section. A
//!   second writer of the key waits for that section and then finds the entry of the first one
//!   (`concurrent_inserts_of_one_key_let_one_through` in `tests/disk_concurrency.rs`), and the
//!   roots it opens are the ones the first one left.
//! - A **read** (`get`, `scan`, `seek`, `latest_version`) holds the catalogue while it reads
//!   the entries and locks the table, then releases it.
//!
//! Inside a call, the pages are protected by the latches of the buffer pool
//! ([`super::buffer`], "Latches").
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
pub(crate) use super::clustered::ClusteredTable;
use super::heap::Heap;
use super::index::{self, DiskIndex, IndexChange, VersionSource};
use super::meta::{self, Catalogue, IndexEntry, TableEntry};
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
    pub(crate) fn take_index_changes(&mut self) -> Vec<IndexChange> {
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

    /// Number of undo entries `txn` has written in this store.
    pub(crate) fn writes_len(&self, txn: TxnId) -> usize {
        match self {
            Self::Heap(table) => table.writes_len(txn),
            Self::Clustered(table) => table.writes_len(txn),
        }
    }

    /// Rolls back the writes of `txn` after `mark`.
    pub(crate) fn rollback_to_savepoint(
        &mut self,
        txn: TxnId,
        mark: usize,
    ) -> Result<(), InternalError> {
        match self {
            Self::Heap(table) => table.rollback_to_savepoint(txn, mark),
            Self::Clustered(table) => table.rollback_to_savepoint(txn, mark),
        }
    }

    /// Removes the versions a `vacuum` below `horizon` discards and answers the index changes.
    pub(crate) fn vacuum(&mut self, horizon: TxnId) -> Result<Vec<IndexChange>, InternalError> {
        match self {
            Self::Heap(table) => table.vacuum(horizon),
            Self::Clustered(table) => table.vacuum(horizon),
        }
    }
}

/// What a call left of the shape of a table: its next [`RowId`] and the roots of its two trees,
/// `(None, None)` on a heap.
type ShapeAfter = (u64, (Option<super::PageId>, Option<super::PageId>));

impl DiskStorage {
    /// Runs `f` on the store of `table` as a write: the lock of the catalogue, then the state
    /// of that table, both held for the whole call.
    ///
    /// The state is moved into the store at the entry and moved back out at the return,
    /// whether `f` answered `Ok` or `Err`: the counters and the undo logs of a refused write
    /// stay where they were, and the identifiers a refused write took are not handed out again
    /// (`super::version::HeapTable::insert`).
    ///
    /// The catalogue entry is read under the lock the call holds to its end, so the roots the
    /// store opens are the ones the previous writer left, and a split that moves one is written
    /// back before the next caller reads it (module documentation, "Locks").
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
        let mut catalogue = self.lock_catalogue()?;
        let entry = self.entry_in(&catalogue, table)?;
        let (outcome, after) = self.with_rows_of(&entry, f)?;
        note_table_shape(&mut catalogue, &entry, after);
        outcome
    }

    /// Runs `f` on the store of the table `pick` names, as a read: the catalogue is held while
    /// `pick` reads it and until the state of the table is locked, then released.
    ///
    /// A writer holds the catalogue for the whole of its call ([`DiskStorage::with_rows`]), so
    /// the entries `pick` reads are the ones the last writer left, and no writer starts between
    /// that read and the lock of the table. Releasing the catalogue once the table is locked is
    /// what lets reads of two tables, and a read of one beside a write of another, go on at the
    /// same time.
    ///
    /// # Errors
    ///
    /// Those of `pick`, of [`DiskStorage::with_rows_of`] and of `f`.
    fn read_rows<X, R>(
        &self,
        pick: impl FnOnce(&Catalogue) -> Result<(TableEntry, X), InternalError>,
        f: impl FnOnce(&mut Store<'_>, X) -> SqlResult<R>,
    ) -> SqlResult<R> {
        let catalogue = self.lock_catalogue()?;
        let (entry, picked) = pick(&catalogue)?;
        let cell = self.table_state(entry.table);
        let mut guard = lock_state(&cell);
        drop(catalogue);
        let (outcome, after) = self.run_on_state(&entry, &mut guard, |store| f(store, picked))?;
        drop(guard);
        if shape_changed(&entry, after) {
            note_table_shape(&mut *self.lock_catalogue()?, &entry, after);
        }
        outcome
    }

    /// Runs `f` on the store of `entry` and answers what it said together with the counters
    /// the call left, without touching the catalogue.
    ///
    /// The caller holds the lock of the catalogue: [`DiskStorage::create_index`] and
    /// [`DiskStorage::with_rows`] do, and [`std::sync::Mutex`] is not re-entrant. The lock of
    /// the table is taken here, under it.
    ///
    /// # Errors
    ///
    /// The errors of [`DiskStorage::store_of`] and of the rebuild; the error of `f` is handed
    /// back inside the answer, so that the state of the table is put back either way.
    pub(crate) fn with_rows_of<R>(
        &self,
        entry: &TableEntry,
        f: impl FnOnce(&mut Store<'_>) -> SqlResult<R>,
    ) -> SqlResult<(SqlResult<R>, ShapeAfter)> {
        let cell = self.table_state(entry.table);
        let mut guard = lock_state(&cell);
        self.run_on_state(entry, &mut guard, f)
    }

    /// Runs `f` on the store of `entry` built around `state`, which the caller has locked, and
    /// puts the state back, on an `Ok` of `f` as on an `Err`.
    fn run_on_state<R>(
        &self,
        entry: &TableEntry,
        state: &mut TableState,
        f: impl FnOnce(&mut Store<'_>) -> SqlResult<R>,
    ) -> SqlResult<(SqlResult<R>, ShapeAfter)> {
        let taken = std::mem::take(state);
        let mut store = match self.store_of(entry, taken) {
            Ok(store) => store,
            // The state was moved into the call: a failure hands it back and it goes into the
            // cell again, rather than leaving a `TableState::default()` there with the undo
            // logs and the `in_progress` pages of the table lost
            // (`super::tests::a_failed_store_of_leaves_the_state_of_the_table_in_place`).
            Err((taken, err)) => {
                *state = taken;
                return Err(err.into());
            }
        };
        let outcome = self.resume(&mut store).and_then(|()| f(&mut store));
        let after = (store.next_row_id(), store.roots());
        *state = store.into_state();
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

    /// The catalogue entry of `table`, cloned out of `catalogue` so that the lock of the
    /// table may be taken without it.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a table the catalogue does not hold.
    fn entry_in(&self, catalogue: &Catalogue, table: TableId) -> Result<TableEntry, InternalError> {
        match catalogue.table(table) {
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
    /// The caller holds the catalogue and the table from the read of `indexes` until the write
    /// has reached the trees of the indexes ([`DiskStorage::write_row`]): a second writer of
    /// the key waits for that lock and then finds the entry of the first one in the tree it
    /// reads (`concurrent_inserts_of_one_key_let_one_through` in `tests/disk_concurrency.rs`).
    fn check_unique(
        &self,
        entry: &TableEntry,
        indexes: &[IndexEntry],
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

    /// Puts `changes` in the trees of the indexes of `table`, under the lock of the catalogue
    /// and then the one of the table, and writes a root a split moved back in the catalogue.
    ///
    /// This is the entry point of a caller that drained the maintenance log in an earlier call
    /// ([`DiskStorage::rollback_to`], [`DiskStorage::vacuum`]); a write of this file applies its
    /// changes inside its own call ([`DiskStorage::apply_index_changes`]).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a table the catalogue does not hold; the errors of the trees.
    pub(crate) fn maintain_indexes(
        &self,
        table: TableId,
        changes: &[IndexChange],
    ) -> SqlResult<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut catalogue = self.lock_catalogue()?;
        let entry = self.entry_in(&catalogue, table)?;
        let indexes = indexes_in(&catalogue, table);
        let cell = self.table_state(table);
        let _table = lock_state(&cell);
        self.apply_index_changes(&mut catalogue, &entry.shape, &indexes, changes)
    }

    /// Puts `changes` in the trees of `indexes`, and writes a root a split moved back in
    /// `catalogue`.
    ///
    /// The caller holds the catalogue and the table; it is what keeps two writers of one table
    /// from walking one tree at the same time, and what keeps the root a split moves from being
    /// read stale by the next writer. An index that shares the tree of a clustered table takes
    /// nothing: the rows **are** its entries.
    fn apply_index_changes(
        &self,
        catalogue: &mut Catalogue,
        shape: &TableShape,
        indexes: &[IndexEntry],
        changes: &[IndexChange],
    ) -> SqlResult<()> {
        if changes.is_empty() {
            return Ok(());
        }
        for entry in indexes {
            let Some(root) = entry.root else {
                continue;
            };
            let mut index = DiskIndex::open(self, entry.index, &entry.shape, root, shape)?;
            index.apply(changes)?;
            if index.root() != root {
                catalogue.add_index(IndexEntry {
                    root: Some(index.root()),
                    ..entry.clone()
                });
            }
        }
        Ok(())
    }

    /// Runs one write on `table` and the index maintenance that follows it, as one section.
    ///
    /// The catalogue and the table are locked from the read of the indexes to the write of the
    /// roots ([`DiskStorage::with_rows`]): the uniqueness of the `unique` indexes is checked
    /// before `write`, and the maintenance log is drained into the trees after it, before a
    /// second writer of the table may check its own key. A 2601 leaves neither a version nor an
    /// entry, and a write that went through reaches the trees of the indexes of its table
    /// before the call returns (`super::tests::index_seek_on_clustered_table` reads a `seek`
    /// after an `insert` and after an `update`).
    fn write_row<R>(
        &self,
        table: TableId,
        check: Check<'_>,
        write: impl FnOnce(&mut Store<'_>) -> SqlResult<R>,
    ) -> SqlResult<R> {
        let mut catalogue = self.lock_catalogue()?;
        let entry = self.entry_in(&catalogue, table)?;
        let indexes = indexes_in(&catalogue, table);
        let (outcome, after) = self.with_rows_of(&entry, |store| {
            check(self, &entry, &indexes, store)?;
            let outcome = write(store)?;
            let changes = store.take_index_changes();
            self.apply_index_changes(&mut catalogue, &entry.shape, &indexes, &changes)?;
            Ok(outcome)
        })?;
        note_table_shape(&mut catalogue, &entry, after);
        outcome
    }

    /// Writes the catalogue to its chain of meta pages at `lsn`, so that the roots a split
    /// moved and the counters the writes advanced are found again after a reopen.
    fn store_catalogue(&self, lsn: super::Lsn) -> SqlResult<()> {
        let catalogue = self.lock_catalogue()?;
        let root = meta::ensure_root(self)?;
        meta::store(self, root, &catalogue, lsn)?;
        Ok(())
    }

    /// Ends `txn` with `kind` and applies that end to each table it wrote in, the index
    /// maintenance of each table inside the section of that table ([`DiskStorage::write_row`]).
    fn end_transaction(&self, txn: TxnId, kind: super::WalRecordKind) -> SqlResult<()> {
        let status = match kind {
            super::WalRecordKind::Abort => TxnStatus::Aborted,
            _ => TxnStatus::Committed,
        };
        let lsn = self.end_txn_grouped(txn, kind, status)?;
        for table in self.txn_tables(txn) {
            let mut catalogue = self.lock_catalogue()?;
            // A table the transaction wrote in and that was dropped since is left alone:
            // `drop_table` frees its pages and takes it out of the catalogue, and its
            // in-flight writes have nothing to finish in — the same answer
            // [`crate::MemoryStorage`] gives (`table_create_drop_and_ids_not_reused`).
            let Some(entry) = catalogue.table(table).cloned() else {
                continue;
            };
            let indexes = indexes_in(&catalogue, table);
            let (outcome, after) = self.with_rows_of(&entry, |store| {
                store.finish(txn, status)?;
                let changes = store.take_index_changes();
                self.apply_index_changes(&mut catalogue, &entry.shape, &indexes, &changes)
            })?;
            note_table_shape(&mut catalogue, &entry, after);
            outcome?;
        }
        if let Some(lsn) = lsn {
            self.store_catalogue(lsn)?;
        }
        Ok(())
    }
}

impl DiskStorage {
    /// Ends `txn` with `kind` as [`DiskStorage::end_txn`] does, but waits for the record to be
    /// durable after the lock of the register is dropped, through the group commit of the
    /// journal (module documentation).
    ///
    /// A transaction that wrote nothing appends no record and waits for nothing; its status is
    /// registered too, so that a second end is reported.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a transaction that already committed or rolled back; the
    /// errors of [`super::wal::WalHandle::append`] and of
    /// [`super::wal::WalHandle::wait_durable`] otherwise, in which case the register keeps
    /// the transaction `InProgress`.
    ///
    /// The status changes after the wait, so two ends of one transaction issued at the same
    /// time may both append their record: a caller bug the `txn` module turns away, since it
    /// refuses a second end of a transaction it is ending.
    fn end_txn_grouped(
        &self,
        txn: TxnId,
        kind: super::WalRecordKind,
        status: TxnStatus,
    ) -> Result<Option<super::Lsn>, InternalError> {
        let lsn = {
            let mut register = super::write_lock(&self.txns);
            let record = register.txns.entry(txn).or_default();
            if record.status != TxnStatus::InProgress {
                return Err(InternalError::Bug(format!(
                    "transaction {txn} is already {:?}",
                    record.status
                )));
            }
            if !record.began {
                record.status = status;
                return Ok(None);
            }
            self.wal.append(kind, txn, &[])?
        };
        self.wal.wait_durable(lsn)?;
        if let Some(record) = super::write_lock(&self.txns).txns.get_mut(&txn) {
            record.status = status;
        }
        Ok(Some(lsn))
    }
}

/// The catalogue entries of the indexes of `table`, cloned out of `catalogue`.
fn indexes_in(catalogue: &Catalogue, table: TableId) -> Vec<IndexEntry> {
    catalogue.indexes_of(table).into_iter().cloned().collect()
}

/// Whether a call changed the next [`RowId`] or a root of the table `entry` describes.
fn shape_changed(entry: &TableEntry, after: ShapeAfter) -> bool {
    let (next_row_id, roots) = after;
    next_row_id != entry.next_row_id
        || (entry.shape.clustered_key.is_some() && roots != (entry.tree_root, entry.directory_root))
}

/// Writes back in `catalogue` what a call changed of the shape of a table: the next [`RowId`]
/// and, when a split moved them, the roots of a clustered table.
///
/// The entry is merged with the one the catalogue holds now rather than written over it: the
/// counter takes the greater of the two, and the roots are taken from the call when that call
/// moved them and from the catalogue otherwise, so a read that wrote its counter back after a writer moved a root leaves that root in
/// place. The chain of meta pages takes the entry at the next `commit` ([`Storage::commit`]),
/// which has a journal record to hang the write on.
fn note_table_shape(catalogue: &mut Catalogue, entry: &TableEntry, after: ShapeAfter) {
    let Some(current) = catalogue.table(entry.table) else {
        return;
    };
    let (next_row_id, roots) = after;
    let next_row_id = next_row_id.max(current.next_row_id);
    let moved =
        entry.shape.clustered_key.is_some() && roots != (entry.tree_root, entry.directory_root);
    let (tree_root, directory_root) = if moved {
        roots
    } else {
        (current.tree_root, current.directory_root)
    };
    if current.next_row_id == next_row_id
        && current.tree_root == tree_root
        && current.directory_root == directory_root
    {
        return;
    }
    let updated = TableEntry {
        next_row_id,
        tree_root,
        directory_root,
        ..current.clone()
    };
    catalogue.add_table(updated);
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
        self.read_rows(
            |catalogue| Ok((self.entry_in(catalogue, table)?, ())),
            |store, ()| Ok(store.get(snap, id)?),
        )
    }

    fn scan(&self, snap: &Snapshot, table: TableId) -> SqlResult<Box<dyn RowIter + '_>> {
        let rows = self.read_rows(
            |catalogue| Ok((self.entry_in(catalogue, table)?, ())),
            |store, ()| Ok(store.scan(snap)?),
        )?;
        Ok(iter(rows))
    }

    fn seek(
        &self,
        snap: &Snapshot,
        index: IndexId,
        range: &KeyRange,
        dir: Direction,
    ) -> SqlResult<Box<dyn RowIter + '_>> {
        // The entry of the index is read under the same lock of the catalogue as the one of
        // its table, so its root is the one the last writer left.
        let pick = |catalogue: &Catalogue| match catalogue.index(index) {
            Some(entry) => {
                let table = self.entry_in(catalogue, entry.table)?;
                let shape = table.shape.clone();
                Ok((table, (entry.clone(), shape)))
            }
            None => Err(InternalError::Bug(format!(
                "index {index} is unknown to instance {}",
                self.dir.display()
            ))),
        };
        let rows = self.read_rows(pick, |store, (entry, shape)| match entry.root {
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
        self.read_rows(
            |catalogue| Ok((self.entry_in(catalogue, table)?, ())),
            |store, ()| Ok(store.latest_version(id)?),
        )
    }

    fn commit(&self, txn: TxnId) -> SqlResult<()> {
        self.end_transaction(txn, super::WalRecordKind::Commit)
    }

    fn rollback(&self, txn: TxnId) -> SqlResult<()> {
        self.end_transaction(txn, super::WalRecordKind::Abort)
    }

    fn savepoint(&self, txn: TxnId) -> SqlResult<SavepointId> {
        DiskStorage::savepoint(self, txn)
    }

    fn rollback_to(&self, txn: TxnId, sp: SavepointId) -> SqlResult<()> {
        DiskStorage::rollback_to(self, txn, sp)
    }

    fn checkpoint(&self) -> SqlResult<()> {
        DiskStorage::checkpoint(self)?;
        Ok(())
    }

    fn vacuum(&self, horizon: TxnId) -> SqlResult<()> {
        DiskStorage::vacuum(self, horizon)
    }

    fn next_txn_id(&self) -> u64 {
        DiskStorage::next_txn_id(self)
    }

    fn committed_txn_high_water(&self) -> u64 {
        DiskStorage::next_txn_id(self).saturating_sub(1)
    }
}

#[cfg(test)]
mod tests;
