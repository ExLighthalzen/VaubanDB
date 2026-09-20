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
mod tests;
