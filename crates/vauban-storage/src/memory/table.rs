//! In-memory representation of a table and of its row versions.

use std::collections::{BTreeMap, BTreeSet};

use super::key_order::KeyOrder;
use crate::{DbId, IndexId, Row, RowId, TableShape, TxnId, TxnStatus};

/// One version of a logical row: who created it, who (if anyone) deleted or replaced it,
/// and its content. See the crate documentation ("Model") for the chain invariant.
#[derive(Debug, Clone)]
pub(crate) struct Version {
    /// The transaction that created this version.
    pub(crate) xmin: TxnId,
    /// The transaction that deleted or replaced this version, `None` while it is current.
    pub(crate) xmax: Option<TxnId>,
    /// The content of the version, one [`vauban_types::Value`] per column.
    pub(crate) data: Row,
    /// Serial number of the version within its table, unique and never reused. Index
    /// entries name a version by `(RowId, seq)`: a position in the chain would shift when
    /// `vacuum` removes older versions.
    pub(crate) seq: usize,
}

/// A table: its owning database, its shape and every version of every logical row, keyed
/// by [`RowId`] so that a scan without clustered key runs in increasing `RowId` order.
#[derive(Debug)]
pub(crate) struct Table {
    /// The database the table belongs to.
    pub(crate) db: DbId,
    /// The shape given to [`crate::Storage::create_table`], returned verbatim by
    /// [`crate::Storage::tables`].
    pub(crate) shape: TableShape,
    /// The comparator of `shape.clustered_key`, if any: the order of
    /// [`crate::Storage::scan`].
    pub(crate) clustered: Option<KeyOrder>,
    /// The indexes of the table, kept in increasing [`IndexId`] order. The entries live in
    /// the storage's index map.
    pub(crate) indexes: BTreeSet<IndexId>,
    /// The versions of each logical row, oldest first (the chain of the crate model). A row
    /// present in the map always has at least one version: `vacuum` removes the rows whose
    /// last version it discards.
    pub(crate) rows: BTreeMap<RowId, Vec<Version>>,
    /// The next [`RowId`] to hand out. Only ever grows: ids are never reused, even after a
    /// rollback or a vacuum.
    pub(crate) next_row_id: u64,
    /// The next [`Version::seq`] to hand out. Only ever grows.
    pub(crate) next_seq: usize,
}

impl Table {
    /// An empty table of the given shape in database `db`; `clustered` is the comparator
    /// of `shape.clustered_key`, built by the caller.
    pub(crate) fn new(db: DbId, shape: TableShape, clustered: Option<KeyOrder>) -> Self {
        Self {
            db,
            shape,
            clustered,
            indexes: BTreeSet::new(),
            rows: BTreeMap::new(),
            next_row_id: 1,
            next_seq: 1,
        }
    }

    /// Number of columns a [`Row`] of this table must have.
    pub(crate) fn arity(&self) -> usize {
        self.shape.columns.len()
    }

    /// The [`RowId`] the next [`Table::insert`] will hand out, so that the caller can
    /// prepare the index entries before touching anything.
    pub(crate) fn peek_row_id(&self) -> RowId {
        RowId(self.next_row_id)
    }

    /// The [`Version::seq`] the next created version will receive.
    pub(crate) fn peek_seq(&self) -> usize {
        self.next_seq
    }

    /// Builds the next version `(xmin = txn, xmax = None)` with a fresh serial number.
    fn new_version(&mut self, txn: TxnId, data: Row) -> Version {
        let seq = self.next_seq;
        self.next_seq += 1;
        Version {
            xmin: txn,
            xmax: None,
            data,
            seq,
        }
    }

    /// Adds a new logical row with a single version `(xmin = txn, xmax = None)` and returns
    /// its fresh [`RowId`]. The caller has checked the arity.
    pub(crate) fn insert(&mut self, txn: TxnId, data: Row) -> RowId {
        let id = RowId(self.next_row_id);
        self.next_row_id += 1;
        let version = self.new_version(txn, data);
        self.rows.insert(id, vec![version]);
        id
    }

    /// Sets `xmax = txn` on the current version of row `id` and, for an `update`, chains a
    /// new version `(xmin = txn, xmax = None)` holding `data` after it. The caller has
    /// checked every precondition ([`super::Inner::check_current`]). Returns `false`
    /// without touching anything if the row has no version.
    pub(crate) fn supersede(&mut self, txn: TxnId, id: RowId, data: Option<Row>) -> bool {
        let new_version = data.map(|d| self.new_version(txn, d));
        let Some(versions) = self.rows.get_mut(&id) else {
            return false;
        };
        let Some(current) = versions.last_mut() else {
            return false;
        };
        current.xmax = Some(txn);
        if let Some(v) = new_version {
            versions.push(v);
        }
        true
    }

    /// Removes the logical row `id` and all its versions: the undo of [`Table::insert`].
    /// Returns the removed versions so that the caller drops their index entries. A row
    /// that is already gone (a `vacuum`, or a table re-created) is a no-op.
    pub(crate) fn remove_row(&mut self, id: RowId) -> Option<Vec<Version>> {
        self.rows.remove(&id)
    }

    /// The most recent version of logical row `id`, `None` if the row is unknown or has
    /// been vacuumed away.
    pub(crate) fn latest(&self, id: RowId) -> Option<&Version> {
        self.rows.get(&id).and_then(|versions| versions.last())
    }

    /// Undoes an `update` by `txn` on row `id`: removes the last version, which `txn`
    /// created, and resets `xmax` to `None` on the version before it, which `txn` had
    /// replaced. Returns the removed version so that the caller drops its index entries,
    /// or `None` without touching anything when the chain does not end that way (see
    /// [`super::txn_log::UndoEntry`] for why it must): the caller reports a corruption.
    pub(crate) fn undo_update(&mut self, txn: TxnId, id: RowId) -> Option<Version> {
        let versions = self.rows.get_mut(&id)?;
        let tail_is_txn_update = match versions.as_slice() {
            [.., replaced, created] => {
                replaced.xmax == Some(txn) && created.xmin == txn && created.xmax.is_none()
            }
            _ => false,
        };
        if !tail_is_txn_update {
            return None;
        }
        let created = versions.pop()?;
        if let Some(replaced) = versions.last_mut() {
            replaced.xmax = None;
        }
        Some(created)
    }

    /// Undoes a `delete` by `txn` on row `id`: resets `xmax` to `None` on the last version,
    /// which `txn` had deleted. Returns `false` without touching anything when the last
    /// version does not carry `xmax = txn`: the caller reports a corruption.
    pub(crate) fn undo_delete(&mut self, txn: TxnId, id: RowId) -> bool {
        match self
            .rows
            .get_mut(&id)
            .and_then(|versions| versions.last_mut())
        {
            Some(deleted) if deleted.xmax == Some(txn) => {
                deleted.xmax = None;
                true
            }
            _ => false,
        }
    }

    /// The version-level passes of [`crate::Storage::vacuum`], in the order of the contract:
    /// first the versions deleted or replaced by a `Committed` transaction `< horizon`, then
    /// the versions created by an `Aborted` transaction, then the logical rows left with no
    /// version. `status` is consulted for every `TxnId` met; the registry is pruned by the
    /// caller afterwards. Returns the removed versions with their row so that the caller
    /// drops their index entries.
    pub(crate) fn vacuum(
        &mut self,
        horizon: TxnId,
        status: &dyn Fn(TxnId) -> TxnStatus,
    ) -> Vec<(RowId, Version)> {
        let mut removed = Vec::new();
        for (id, versions) in self.rows.iter_mut() {
            let (dead, alive): (Vec<Version>, Vec<Version>) = versions.drain(..).partition(|v| {
                v.xmax
                    .is_some_and(|x| x < horizon && status(x) == TxnStatus::Committed)
            });
            *versions = alive;
            removed.extend(dead.into_iter().map(|v| (*id, v)));
        }
        for (id, versions) in self.rows.iter_mut() {
            let (dead, alive): (Vec<Version>, Vec<Version>) = versions
                .drain(..)
                .partition(|v| status(v.xmin) == TxnStatus::Aborted);
            *versions = alive;
            removed.extend(dead.into_iter().map(|v| (*id, v)));
        }
        self.rows.retain(|_, versions| !versions.is_empty());
        removed
    }
}
