//! Non-clustered index of the on-disk layout: one [`BTree`] per index, whose key is the
//! values of the index columns of **one version** of a row and whose payload names that
//! version, plus the uniqueness barrier that answers 2601.
//!
//! # One entry per version, not per row
//!
//! The payload of an entry is a [`RowId`] and the `seq` of a version, not the row alone. That
//! is what lets an old snapshot read a row under its old key while a newer one reads it under
//! the new one: [`DiskIndex::seek`] filters each entry through [`Snapshot::is_visible`], and
//! the chain invariant of the crate root leaves at most one version of a row visible.
//! `memory/index.rs` holds its entries the same way.
//!
//! The maintenance follows from that shape:
//!
//! | Write | What the index does |
//! |---|---|
//! | `insert` | adds the entry of the version created |
//! | `update` | adds the entry of the version created; the entry of the replaced one stays |
//! | `delete` | nothing: the version stays, visibility hides it |
//! | `rollback` | removes the entries of the versions the undo takes away |
//!
//! The entry of a version an `update` replaced stays because a snapshot that has not settled
//! the writer still reads the row under the old key
//! (`tests::update_leaves_the_old_key_to_an_older_snapshot`, and
//! `case_index_maintained_on_update_rollback_vacuum` of the generic suite). Taking those
//! entries away is the business of `vacuum`, not implemented on disk.
//!
//! # How a caller drives it
//!
//! [`super::version::HeapTable`] writes the rows and records what each write means for an
//! index in its maintenance log; the caller drains that log with
//! [`super::version::HeapTable::take_index_changes`] and hands it to [`DiskIndex::apply`].
//! A unique index is asked **before** the write ([`DiskIndex::check_duplicate`]), so a refused
//! call leaves neither a version nor an entry (`tests::unique_2601_names_ids`).
//! [`super::storage_impl`] ties the two together behind [`crate::Storage`]; the tests of this
//! file drive them the same way, through `insert_row`, `update_row`, `delete_row` and
//! `rollback_txn`.
//!
//! # What this module does not do
//!
//! No journal record is written for an index page: a crash loses the tree, and rebuilding the
//! indexes from the versions after the redo of the rows is not implemented.
//! [`IndexShape::included`] is kept in the shape and read by nothing here.

use std::collections::HashMap;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::Value;

use super::DiskStorage;
use super::alloc::free;
use super::btree::{BTree, internal_child, internal_len, leftmost_child};
use super::page::{PageId, PageKind};
use super::version::VersionView;
use crate::{
    Direction, IndexId, IndexShape, KeyRange, Row, RowId, Snapshot, TableId, TableShape, TxnId,
    TxnStatus,
};

/// Number of bytes an entry carries as its payload: the [`RowId`] then the `seq`.
pub(crate) const PAYLOAD_LEN: usize = 16;

/// Largest number of pages [`free_tree`] walks before it calls the tree corrupt: a child
/// pointer that leads back up the tree is an error rather than a walk without an end.
const MAX_TREE_PAGES: usize = 1 << 20;

/// What one write of [`HeapTable`] means for the indexes of its table.
///
/// The variants name a version, by its row and its `seq`, and carry the columns of that
/// version so that each index computes its own key from them: the log does not know the key
/// columns of the indexes, and a table may carry several.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IndexChange {
    /// A version was created: each index takes an entry for it.
    Added {
        /// The logical row the version belongs to.
        row: RowId,
        /// The serial number of the version within its table.
        seq: u64,
        /// The columns of the version, which the key is extracted from.
        data: Row,
    },
    /// A version was taken away by the undo of a rollback: each index drops its entry.
    Removed {
        /// The logical row the version belonged to.
        row: RowId,
        /// The serial number of the version within its table.
        seq: u64,
        /// The columns of the version, which the key is extracted from.
        data: Row,
    },
}

/// The bytes an entry of a tree carries as its payload: the [`RowId`] then the `seq`, both
/// **big-endian**.
///
/// The order of two entries of one key is the order of their payload bytes ([`BTree::seek`]
/// compares them with `Ord` on `[u8]`), so big-endian is what makes it the order of the pair
/// `(RowId, seq)` — the tie-break "equal keys are ordered by increasing `RowId`" of
/// [`crate::TableShape`]. Asserted on the pairs of
/// `tests::payload_round_trips_and_sorts_by_row_then_seq`, and read end to end by
/// `tests::seek_point_between_full_both_directions`, whose two rows of the key `(1, 5)` come
/// back in `RowId` order.
fn payload_of(row: RowId, seq: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(PAYLOAD_LEN);
    out.extend_from_slice(&row.0.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out
}

/// Reads back what [`payload_of`] wrote.
///
/// # Errors
///
/// [`InternalError::Corruption`] for a payload that is not [`PAYLOAD_LEN`] bytes long.
fn read_payload(bytes: &[u8]) -> Result<(RowId, u64), InternalError> {
    let (Some(row), Some(seq)) = (
        bytes.get(0..8).and_then(|s| <[u8; 8]>::try_from(s).ok()),
        bytes
            .get(8..PAYLOAD_LEN)
            .and_then(|s| <[u8; 8]>::try_from(s).ok()),
    ) else {
        return Err(payload_corruption(bytes.len()));
    };
    if bytes.len() != PAYLOAD_LEN {
        return Err(payload_corruption(bytes.len()));
    }
    Ok((RowId(u64::from_be_bytes(row)), u64::from_be_bytes(seq)))
}

/// The error of a payload whose length is not [`PAYLOAD_LEN`].
fn payload_corruption(len: usize) -> InternalError {
    InternalError::Corruption(format!(
        "an index entry carries a payload of {len} bytes where the {PAYLOAD_LEN} bytes of a row \
         id and a version serial were expected"
    ))
}

/// Answers 2601 when a live version of another logical row already holds the key `row` gives
/// the shape `def`, reading the versions rather than a tree.
///
/// This is the uniqueness of an index that **shares** the tree of a clustered table
/// ([`super::DiskStorage::create_index`]): such an index has no tree of its own, so there is
/// no entry to seek. The walk is over the versions of the table, which is what
/// [`DiskIndex::check_duplicate`] avoids by seeking one key; it is the price of an index that
/// takes no page. `replacing` is the row an `update` supersedes, left out of the comparison
/// for the reason [`DiskIndex::check_duplicate`] gives.
///
/// # Errors
///
/// [`SqlError`] 2601 naming the table, the index and the key, with the decimal identifiers in
/// place of names; the errors of the store otherwise.
pub(crate) fn check_duplicate_without_tree(
    table: &dyn VersionSource,
    index: IndexId,
    def: &IndexShape,
    txn: TxnId,
    row: &Row,
    replacing: Option<RowId>,
) -> SqlResult<()> {
    if !def.unique {
        return Ok(());
    }
    let key = key_of(row, def, index)?;
    let status = |t| table.status(t);
    for version in table.versions(None)? {
        if Some(version.row) == replacing || !is_live(&version, Some(txn), &status) {
            continue;
        }
        if key_of(&version.data, def, index)? == key {
            return Err(SqlError::duplicate_key_index(
                &table.table().to_string(),
                &index.to_string(),
                &key_text(&key),
            ));
        }
    }
    Ok(())
}

/// The key of `row` for the shape `def`: its values at the key columns, in key order.
///
/// # Errors
///
/// [`InternalError::Corruption`] for a row that has no value at a key column.
fn key_of(row: &Row, def: &IndexShape, index: IndexId) -> Result<Vec<Value>, InternalError> {
    def.columns
        .iter()
        .map(|column| {
            row.0
                .get(usize::from(column.column))
                .cloned()
                .ok_or_else(|| {
                    InternalError::Corruption(format!(
                        "a row of {} values has no key column {} for index {index}",
                        row.0.len(),
                        column.column
                    ))
                })
        })
        .collect()
}

/// The key as the 2601 message shows it: `(v1, v2)`.
///
/// Same form as `key_text` of `memory/index.rs`: `vauban_types` has no display function yet,
/// so the `Debug` form of each value is used, and the executor rephrases the error anyway.
/// `pub(crate)`: the duplicate an index on a clustered key finds is named with the same text
/// ([`super::DiskStorage::create_index`]).
pub(crate) fn key_text(key: &[Value]) -> String {
    let values: Vec<String> = key.iter().map(|v| format!("{v:?}")).collect();
    format!("({})", values.join(", "))
}

/// Whether `version` is *live* for the uniqueness test, seen from the writing transaction
/// `txn` ("Duplicate keys" on [`crate::Storage`]): its creator has not aborted, and it is
/// either current or deleted/replaced by another transaction still in progress. A version
/// `txn` itself deleted or replaced is not live for `txn`. With `txn = None`
/// ([`DiskIndex::create`], no writer) an in-progress deleter counts.
///
/// Same rule as `is_live` of `memory/index.rs`, written again here because `disk` does not
/// import `memory`.
fn is_live(version: &VersionView, txn: Option<TxnId>, status: &dyn Fn(TxnId) -> TxnStatus) -> bool {
    if status(version.xmin) == TxnStatus::Aborted {
        return false;
    }
    match version.xmax {
        None => true,
        Some(x) => Some(x) != txn && status(x) == TxnStatus::InProgress,
    }
}

/// The versions of a table by the pair an index payload names.
fn by_payload(versions: Vec<VersionView>) -> HashMap<(RowId, u64), VersionView> {
    versions
        .into_iter()
        .map(|version| ((version.row, version.seq), version))
        .collect()
}

/// Puts the pages of the tree rooted at `root` back in the free list.
///
/// `pub(crate)`: `drop_index` and `drop_table` hand the tree of an index and the two trees of
/// a clustered table back ([`super::DiskStorage::drop_index`]).
///
/// The walk reads the children of an internal page (`P0` and the pointer of each key) before
/// it lets the pin of that page go, then frees the pages it collected: [`free`] refuses a page
/// that carries a pin. It is what makes a [`DiskIndex::create`] refused for a duplicate key
/// leave the instance as it found it (`tests::create_index_refuses_existing_duplicate` reads
/// the free list and the page counter back).
///
/// # Errors
///
/// [`InternalError::Corruption`] when the walk reaches more than [`MAX_TREE_PAGES`] pages; the
/// errors of the buffer pool and of [`free`].
pub(crate) fn free_tree(storage: &DiskStorage, root: PageId) -> Result<(), InternalError> {
    let mut pending = vec![root];
    let mut pages: Vec<PageId> = Vec::new();
    while let Some(id) = pending.pop() {
        if pages.len() >= MAX_TREE_PAGES {
            return Err(InternalError::Corruption(format!(
                "the B+tree rooted at {root} reaches more than {MAX_TREE_PAGES} pages: a child \
                 pointer leads back up the tree"
            )));
        }
        let pin = storage.pool.pin(id)?;
        let children = pin.with_page(|page| {
            if page.kind()? != PageKind::BTreeInternal {
                return Ok(Vec::new());
            }
            let mut children = vec![leftmost_child(page)];
            for index in 0..internal_len(page) {
                children.push(internal_child(page, index)?);
            }
            Ok::<_, InternalError>(children)
        })??;
        drop(pin);
        pages.push(id);
        pending.extend(children);
    }
    for id in pages {
        free(storage, id)?;
    }
    Ok(())
}

/// What an index reads of the table it belongs to: the identifier, the shape, the versions
/// and the status of a transaction.
///
/// This trait sits between [`DiskIndex`] and the stores so that one index serves a heap and a
/// clustered table alike (`super::tests::index_seek_on_clustered_table`). Implemented by
/// [`HeapTable`] and by [`super::clustered::ClusteredTable`].
pub(crate) trait VersionSource {
    /// The table the versions belong to; the 2601 message names it.
    fn table(&self) -> TableId;
    /// The shape of the table, whose columns carry the type of each key column.
    fn shape(&self) -> &TableShape;
    /// The versions of the row `only`, or of the whole table, by increasing `(RowId, seq)`.
    ///
    /// # Errors
    ///
    /// Those of the store the versions are read from.
    fn versions(&self, only: Option<RowId>) -> Result<Vec<VersionView>, InternalError>;
    /// The status of `t` as [`Snapshot::is_visible`] must see it.
    fn status(&self, t: TxnId) -> TxnStatus;
}

/// One non-clustered index of a table: its identifier, the shape it was created with and the
/// [`BTree`] holding one entry per indexed version.
#[derive(Debug)]
pub(crate) struct DiskIndex<'storage> {
    /// The identifier the caller gave the index; it is what the 2601 message names.
    id: IndexId,
    /// The shape handed to [`DiskIndex::create`], kept verbatim, [`IndexShape::included`]
    /// included, which nothing here reads.
    shape: IndexShape,
    /// The entries, keyed by the values of [`IndexShape::columns`].
    tree: BTree<'storage>,
}

impl<'storage> DiskIndex<'storage> {
    /// Creates the index `id` of shape `shape` on `table`, indexing the versions already
    /// there.
    ///
    /// A version whose creator has not aborted takes an entry, as `create_index` of
    /// [`crate::MemoryStorage`] does: a snapshot older than the index reads the same rows
    /// through it as through a scan. A `unique` shape is then checked, and a key held by live
    /// versions of two rows is 2601; the pages the tree took go back to the free list before
    /// the error leaves ([`free_tree`]), so the refusal creates nothing
    /// (`tests::create_index_refuses_existing_duplicate`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for an [`IndexShape::included`] column outside the table and for
    /// the key shapes `super::btree::KeyOrder::new` refuses (no column, a column outside the
    /// table). [`SqlError`] 2601 for the duplicate above. The errors of the buffer pool, of the
    /// allocator and of [`BTree::insert`] otherwise — among them the [`InternalError::Bug`] of
    /// a key and payload longer than the ceiling of an entry.
    pub(crate) fn create(
        storage: &'storage DiskStorage,
        id: IndexId,
        shape: &IndexShape,
        table: &dyn VersionSource,
    ) -> SqlResult<Self> {
        let arity = table.shape().columns.len();
        if let Some(inc) = shape
            .included
            .iter()
            .find(|inc| usize::from(**inc) >= arity)
        {
            return Err(InternalError::Bug(format!(
                "included column {inc} of index {id} is outside the {arity} columns of table {}",
                table.table()
            ))
            .into());
        }
        let tree = BTree::create(storage, &shape.columns, &table.shape().columns)?;
        let mut index = Self {
            id,
            shape: shape.clone(),
            tree,
        };
        let versions = table.versions(None)?;
        for version in &versions {
            if table.status(version.xmin) == TxnStatus::Aborted {
                continue;
            }
            let key = index.extract(&version.data)?;
            index
                .tree
                .insert(&key, &payload_of(version.row, version.seq))?;
        }
        if shape.unique
            && let Some(key) = index.first_duplicate(table, versions)?
        {
            free_tree(storage, index.tree.root())?;
            return Err(SqlError::duplicate_key_index(
                &table.table().to_string(),
                &id.to_string(),
                &key_text(&key),
            ));
        }
        Ok(index)
    }

    /// Attaches to the index `id` whose tree is rooted at `root`, leaving the pages as they
    /// stand.
    ///
    /// This is how [`super::DiskStorage`] reaches an index between two calls, the catalogue
    /// keeping the root ([`super::meta::IndexEntry`]): the tree is opened, used and dropped,
    /// and the root is written back when a split moved it.
    ///
    /// # Errors
    ///
    /// The errors of `super::btree::KeyOrder::new` for a key shape it refuses.
    pub(crate) fn open(
        storage: &'storage DiskStorage,
        id: IndexId,
        shape: &IndexShape,
        root: PageId,
        table: &TableShape,
    ) -> Result<Self, InternalError> {
        let tree = BTree::open(storage, root, &shape.columns, &table.columns)?;
        Ok(Self {
            id,
            shape: shape.clone(),
            tree,
        })
    }

    /// The identifier of the index.
    pub(crate) fn id(&self) -> IndexId {
        self.id
    }

    /// The shape the index was created with, as [`crate::Storage::indexes`] hands it back.
    pub(crate) fn shape(&self) -> &IndexShape {
        &self.shape
    }

    /// The root of the B+tree of the index.
    ///
    /// The catalogue of the instance writes it so that the tree is found again after a reopen
    /// and handed back by `drop_index` ([`free_tree`]).
    pub(crate) fn root(&self) -> PageId {
        self.tree.root()
    }

    /// Applies the maintenance log of one write of [`HeapTable`].
    ///
    /// An [`IndexChange::Removed`] whose entry the tree does not hold leaves the tree as it
    /// stands: a version created before the index existed and undone after it has no entry
    /// here, which is the `false` of [`BTree::delete`].
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a version whose columns do not reach a key column;
    /// the errors of [`BTree::insert`] and [`BTree::delete`] otherwise.
    pub(crate) fn apply(&mut self, changes: &[IndexChange]) -> Result<(), InternalError> {
        for change in changes {
            match change {
                IndexChange::Added { row, seq, data } => {
                    let key = self.extract(data)?;
                    self.tree.insert(&key, &payload_of(*row, *seq))?;
                }
                IndexChange::Removed { row, seq, data } => {
                    let key = self.extract(data)?;
                    self.tree.delete(&key, &payload_of(*row, *seq))?;
                }
            }
        }
        Ok(())
    }

    /// Refuses the key of `row` when a live version of another logical row already holds it.
    ///
    /// Called **before** the write it guards, so a 2601 leaves neither a version nor an entry.
    /// `replacing` is the row an `update` supersedes: its current version is about to take
    /// `xmax = txn`, which makes it not live for `txn`, and its older versions carry an `xmax`
    /// that is either `txn` or committed, so none of them is live either — an update that keeps
    /// the key passes (`tests::unique_allows_update_same_key`).
    ///
    /// A non-`unique` index answers `Ok(())` without reading the tree.
    ///
    /// # Errors
    ///
    /// [`SqlError`] 2601 naming the table, the index and the key, with the decimal identifiers
    /// in place of names (`tests::unique_2601_names_ids`); [`InternalError::Corruption`] for an
    /// entry that names a version the heap does not hold; the errors of [`BTree::seek`] and of
    /// the heap otherwise.
    pub(crate) fn check_duplicate(
        &self,
        table: &dyn VersionSource,
        txn: TxnId,
        row: &Row,
        replacing: Option<RowId>,
    ) -> SqlResult<()> {
        if !self.shape.unique {
            return Ok(());
        }
        let key = self.extract(row)?;
        let status = |t| table.status(t);
        let entries = self
            .tree
            .seek(&KeyRange::Point(key.clone()), Direction::Forward)?;
        for (_, payload) in &entries {
            let (other, seq) = read_payload(payload)?;
            if Some(other) == replacing {
                continue;
            }
            let version = self.version_of(table, other, seq)?;
            if is_live(&version, Some(txn), &status) {
                return Err(SqlError::duplicate_key_index(
                    &table.table().to_string(),
                    &self.id.to_string(),
                    &key_text(&key),
                ));
            }
        }
        Ok(())
    }

    /// The keys of `range` this index holds, as [`crate::Storage::seek`] hands them back.
    ///
    /// The rows of `range` that `snap` sees, in the order `dir` asks for.
    ///
    /// The entries of the range are read out of the tree, then each one is looked up among the
    /// versions of the table and kept when [`Snapshot::is_visible`] answers `true`; the row
    /// handed back is the content of **that** version, not of the current one, so a snapshot
    /// older than an update reads the old columns
    /// (`tests::update_leaves_the_old_key_to_an_older_snapshot`). The answer is materialised
    /// here, which is the isolation [`crate::Storage::seek`] asks for.
    ///
    /// Bounds are read in index order, [`crate::KeyColumn::descending`] already applied, and a
    /// bound of fewer values than the key has columns is a prefix ([`BTree::seek`]).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a bound of more values than the key has columns;
    /// [`InternalError::Corruption`] for an entry that names a version the heap does not hold;
    /// the errors of the buffer pool otherwise.
    pub(crate) fn seek(
        &self,
        table: &dyn VersionSource,
        snap: &Snapshot,
        range: &KeyRange,
        dir: Direction,
    ) -> SqlResult<Vec<(RowId, Row)>> {
        let entries = self.tree.seek(range, dir)?;
        let versions = by_payload(table.versions(None)?);
        let status = |t| table.status(t);
        let mut out = Vec::with_capacity(entries.len());
        for (_, payload) in &entries {
            let (row, seq) = read_payload(payload)?;
            let Some(version) = versions.get(&(row, seq)) else {
                return Err(self.missing_version(table, row, seq).into());
            };
            if snap.is_visible(version.xmin, version.xmax, &status) {
                out.push((row, version.data.clone()));
            }
        }
        Ok(out)
    }

    /// The key of `row`: its values at the index columns, in index order.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a row that has no value at a key column; the arity of
    /// a row is checked by [`HeapTable`] before it is stored.
    fn extract(&self, row: &Row) -> Result<Vec<Value>, InternalError> {
        self.shape
            .columns
            .iter()
            .map(|column| {
                row.0
                    .get(usize::from(column.column))
                    .cloned()
                    .ok_or_else(|| {
                        InternalError::Corruption(format!(
                            "a row of {} values has no key column {} for index {}",
                            row.0.len(),
                            column.column,
                            self.id
                        ))
                    })
            })
            .collect()
    }

    /// The first key held by live versions of two distinct rows, for the uniqueness check of
    /// [`DiskIndex::create`], `None` when the table holds no such key.
    ///
    /// The versions are walked in `(RowId, seq)` order and the key reported is the one of the
    /// first of them that meets a live twin of another row. `memory/index.rs` walks its entries
    /// in key order instead, so the two implementations may name different keys on a table that
    /// holds several duplicates; both name a key two live rows share.
    fn first_duplicate(
        &self,
        table: &dyn VersionSource,
        versions: Vec<VersionView>,
    ) -> SqlResult<Option<Vec<Value>>> {
        let status = |t| table.status(t);
        let versions = by_payload(versions);
        let mut ordered: Vec<&VersionView> = versions.values().collect();
        ordered.sort_by_key(|version| (version.row, version.seq));
        for version in ordered {
            if !is_live(version, None, &status) {
                continue;
            }
            let key = self.extract(&version.data)?;
            let entries = self
                .tree
                .seek(&KeyRange::Point(key.clone()), Direction::Forward)?;
            for (_, payload) in &entries {
                let (other, seq) = read_payload(payload)?;
                if other == version.row {
                    continue;
                }
                let Some(twin) = versions.get(&(other, seq)) else {
                    return Err(self.missing_version(table, other, seq).into());
                };
                if is_live(twin, None, &status) {
                    return Ok(Some(key));
                }
            }
        }
        Ok(None)
    }

    /// The version an entry points at, read from the heap.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the heap holds no such version, which is a broken
    /// invariant of this module; the errors of the heap otherwise.
    fn version_of(
        &self,
        table: &dyn VersionSource,
        row: RowId,
        seq: u64,
    ) -> Result<VersionView, InternalError> {
        table
            .versions(Some(row))?
            .into_iter()
            .find(|version| version.seq == seq)
            .ok_or_else(|| self.missing_version(table, row, seq))
    }

    /// The error of an entry that names a version the heap does not hold.
    fn missing_version(&self, table: &dyn VersionSource, row: RowId, seq: u64) -> InternalError {
        InternalError::Corruption(format!(
            "index {} of table {} references version {seq} of row {row}, which does not exist",
            self.id,
            table.table()
        ))
    }
}

#[cfg(test)]
mod tests {
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
        let mut index =
            DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], false), &table)
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
        let mut index =
            DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], true), &table)
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
        let b = insert_row(&mut table, &mut index, 1, &nullable_row(&[Some(2), None]))
            .expect("(2, NULL)");
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
        let mut index =
            DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], true), &table)
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
        let mut index =
            DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], true), &table)
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
        let mut index =
            DiskIndex::create(&storage, INDEX, &index_shape(&[(0, false)], false), &table)
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
}
