//! In-memory representation of an index: a sorted `Vec` of entries, one per row
//! **version**, searched by binary search. No tree: this is enough for the in-memory
//! engine, and the on-disk one has its own B+tree.
//!
//! One entry per version, not per logical row, is what lets an old snapshot see a row under
//! its old key while a newer snapshot sees it under the new one: [`crate::Storage::seek`]
//! filters each entry by the visibility of its version, and the chain invariant guarantees
//! that at most one version of a row qualifies.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::{Bound, Range};

use vauban_errors::{InternalError, SqlResult};
use vauban_types::Value;

use super::key_order::{KeyOrder, partition_point};
use super::table::Version;
use crate::{IndexShape, KeyRange, RowId, TableId, TxnId, TxnStatus};

/// One entry of an index: the key of one version of one row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexEntry {
    /// The key values of the version, in key order.
    pub(crate) key: Vec<Value>,
    /// The logical row the version belongs to.
    pub(crate) row: RowId,
    /// The serial number of the version within its table ([`Version::seq`]): stable across
    /// `vacuum`, unlike a position in the row's chain.
    pub(crate) version: usize,
}

/// An index on one table: its shape, its comparator and its entries, kept sorted by
/// `(key in index order, RowId, version)`.
#[derive(Debug)]
pub(crate) struct Index {
    /// The table the index belongs to.
    pub(crate) table: TableId,
    /// The shape given to [`crate::Storage::create_index`], returned verbatim by
    /// [`crate::Storage::indexes`].
    pub(crate) shape: IndexShape,
    /// The comparator built from the key columns and the table's column types.
    pub(crate) order: KeyOrder,
    /// The entries, sorted as described on the type.
    pub(crate) entries: Vec<IndexEntry>,
}

/// Whether `version` is *live* for the uniqueness test, from the point of view of the
/// writing transaction `txn` ("Duplicate keys" on [`crate::Storage`]): its creator has
/// not aborted and it is current, or deleted/replaced by another transaction still in
/// progress. A version deleted or replaced by `txn` itself is never live for `txn`. With
/// `txn = None` (`create_index`, no writer) every in-progress deleter counts.
pub(crate) fn is_live(
    version: &Version,
    txn: Option<TxnId>,
    status: &dyn Fn(TxnId) -> TxnStatus,
) -> bool {
    if status(version.xmin) == TxnStatus::Aborted {
        return false;
    }
    match version.xmax {
        None => true,
        Some(x) => Some(x) != txn && status(x) == TxnStatus::InProgress,
    }
}

/// The version `seq` of row `row`, if both still exist.
pub(crate) fn find_version(
    rows: &BTreeMap<RowId, Vec<Version>>,
    row: RowId,
    seq: usize,
) -> Option<&Version> {
    rows.get(&row)
        .and_then(|versions| versions.iter().find(|v| v.seq == seq))
}

/// The key as the 2601 message shows it: `(v1, v2)`. `vauban_types` has no display
/// function yet, so the `Debug` form is used; the executor rephrases the error anyway.
pub(crate) fn key_text(key: &[Value]) -> String {
    let values: Vec<String> = key.iter().map(|v| format!("{v:?}")).collect();
    format!("({})", values.join(", "))
}

impl Index {
    /// An empty index of the given shape on `table`.
    pub(crate) fn new(table: TableId, shape: IndexShape, order: KeyOrder) -> Self {
        Self {
            table,
            shape,
            order,
            entries: Vec::new(),
        }
    }

    /// The first position whose key is not below `prefix` (all keys starting with `prefix`
    /// are at or after it).
    fn lower_bound(&self, prefix: &[Value]) -> SqlResult<usize> {
        partition_point(&self.entries, |e| {
            Ok(self.order.compare_prefix(&e.key, prefix)? == Ordering::Less)
        })
    }

    /// The first position whose key is above `prefix` (all keys starting with `prefix` are
    /// before it).
    fn upper_bound(&self, prefix: &[Value]) -> SqlResult<usize> {
        partition_point(&self.entries, |e| {
            Ok(self.order.compare_prefix(&e.key, prefix)? != Ordering::Greater)
        })
    }

    /// The position where an entry `(key, row, version)` belongs: after every entry that
    /// sorts before it, before every entry that sorts after it or is equal to it.
    pub(crate) fn position_for(
        &self,
        key: &[Value],
        row: RowId,
        version: usize,
    ) -> SqlResult<usize> {
        partition_point(&self.entries, |e| {
            Ok(match self.order.compare_prefix(&e.key, key)? {
                Ordering::Less => true,
                Ordering::Equal => (e.row, e.version) < (row, version),
                Ordering::Greater => false,
            })
        })
    }

    /// Removes the entry of `version` of row `row`, if the index has one. A version the
    /// index never held (created by an aborted transaction before the index existed) is
    /// ignored silently.
    pub(crate) fn remove_version(&mut self, row: RowId, version: &Version) -> SqlResult<()> {
        let key = self.order.extract(&version.data)?;
        let pos = self.position_for(&key, row, version.seq)?;
        let found = match self.entries.get(pos) {
            Some(e) => {
                e.row == row
                    && e.version == version.seq
                    && self.order.compare_prefix(&e.key, &key)? == Ordering::Equal
            }
            None => false,
        };
        if found {
            self.entries.remove(pos);
        }
        Ok(())
    }

    /// The positions of the entries selected by `range`, after checking the preconditions
    /// of [`crate::Storage::seek`] on every key it holds (length, variants).
    pub(crate) fn positions(&self, range: &KeyRange) -> SqlResult<Range<usize>> {
        let (start, end) = match range {
            KeyRange::Full => (0, self.entries.len()),
            KeyRange::Point(k) => {
                self.order.check_prefix(k)?;
                (self.lower_bound(k)?, self.upper_bound(k)?)
            }
            KeyRange::Between(lo, hi) => {
                for bound in [lo, hi] {
                    if let Bound::Included(p) | Bound::Excluded(p) = bound {
                        self.order.check_prefix(p)?;
                    }
                }
                let start = match lo {
                    Bound::Unbounded => 0,
                    Bound::Included(p) => self.lower_bound(p)?,
                    Bound::Excluded(p) => self.upper_bound(p)?,
                };
                let end = match hi {
                    Bound::Unbounded => self.entries.len(),
                    Bound::Included(p) => self.upper_bound(p)?,
                    Bound::Excluded(p) => self.lower_bound(p)?,
                };
                (start, end)
            }
        };
        Ok(start..end.max(start))
    }

    /// Whether a live version other than those of `skip_row` already holds `key`
    /// ("Duplicate keys" on [`crate::Storage`]). `skip_row` is the row an `update` is
    /// replacing: its current version is about to receive `xmax = txn`, which makes it not
    /// live for `txn`, and its older versions carry an `xmax` that is either `txn` or
    /// committed (precondition of `update`), so none of them is live either.
    pub(crate) fn has_live_duplicate(
        &self,
        key: &[Value],
        rows: &BTreeMap<RowId, Vec<Version>>,
        txn: Option<TxnId>,
        status: &dyn Fn(TxnId) -> TxnStatus,
        skip_row: Option<RowId>,
    ) -> SqlResult<bool> {
        let range = self.lower_bound(key)?..self.upper_bound(key)?;
        for entry in self.entries.get(range).unwrap_or_default() {
            if Some(entry.row) == skip_row {
                continue;
            }
            let version = self.version_of(rows, entry)?;
            if is_live(version, txn, status) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The first key held by live versions of two distinct rows, for the uniqueness check
    /// of [`crate::Storage::create_index`]. `None` if there is none.
    pub(crate) fn first_duplicate_key(
        &self,
        rows: &BTreeMap<RowId, Vec<Version>>,
        status: &dyn Fn(TxnId) -> TxnStatus,
    ) -> SqlResult<Option<Vec<Value>>> {
        // Entries are sorted by key: a group of equal keys is contiguous.
        let mut group_start = 0;
        while group_start < self.entries.len() {
            let Some(first) = self.entries.get(group_start) else {
                break;
            };
            let group_end = self.upper_bound(&first.key)?.max(group_start + 1);
            let mut live_row: Option<RowId> = None;
            for entry in self.entries.get(group_start..group_end).unwrap_or_default() {
                if !is_live(self.version_of(rows, entry)?, None, status) {
                    continue;
                }
                match live_row {
                    Some(row) if row != entry.row => return Ok(Some(first.key.clone())),
                    _ => live_row = Some(entry.row),
                }
            }
            group_start = group_end;
        }
        Ok(None)
    }

    /// The version an entry points to; an entry whose version is gone is a broken
    /// invariant of this module.
    fn version_of<'r>(
        &self,
        rows: &'r BTreeMap<RowId, Vec<Version>>,
        entry: &IndexEntry,
    ) -> SqlResult<&'r Version> {
        find_version(rows, entry.row, entry.version).ok_or_else(|| {
            InternalError::Corruption(format!(
                "index on table {} references version {} of row {}, which does not exist",
                self.table, entry.version, entry.row
            ))
            .into()
        })
    }
}
