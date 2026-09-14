//! Snapshots and the MVCC visibility rule shared by every [`crate::Storage`] implementation.

use crate::TxnId;

/// The fate of a transaction, as known by the storage layer.
///
/// An implementation learns a transaction at its first write (`InProgress`) and its fate
/// through [`crate::Storage::commit`] (`Committed`) or [`crate::Storage::rollback`]
/// (`Aborted`). Transactions forgotten by [`crate::Storage::vacuum`] are all `Committed`
/// or `Aborted` and older than the horizon: whatever an implementation answers for them
/// is never consulted by a usable snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStatus {
    /// The transaction has written and has neither committed nor rolled back.
    InProgress,
    /// [`crate::Storage::commit`] returned `Ok` for this transaction.
    Committed,
    /// [`crate::Storage::rollback`] returned `Ok` for this transaction.
    Aborted,
}

/// What a reading transaction may see. Built by the `txn` module, never by `storage`.
///
/// - `xmin`: the smallest [`TxnId`] still active when the snapshot was taken. Every
///   `TxnId < xmin` is finished (committed or aborted).
/// - `xmax`: the next `TxnId` to be assigned when the snapshot was taken. Every
///   `TxnId >= xmax` is invisible (except `own`).
/// - `active`: the transactions in progress within `[xmin, xmax)`, sorted in increasing
///   order. A transaction listed here is invisible even if it has committed since.
/// - `own`: the reading transaction. It may be `>= xmax` when the snapshot was taken before
///   the transaction started; the rule handles `own` before anything else.
///
/// The visibility rule lives here, in [`Snapshot::is_settled`] and [`Snapshot::is_visible`],
/// so that both implementations and the `txn` module (`check_write_conflict`) share it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Smallest transaction still active at snapshot time; every `TxnId < xmin` is finished.
    pub xmin: TxnId,
    /// Next transaction to be assigned at snapshot time; every `TxnId >= xmax` is invisible.
    pub xmax: TxnId,
    /// Transactions in progress within `[xmin, xmax)` at snapshot time, sorted.
    pub active: Vec<TxnId>,
    /// The reading transaction.
    pub own: TxnId,
}

impl Snapshot {
    /// Whether the effects of transaction `t` count as done for this snapshot.
    ///
    /// `true` when `t == self.own`, or when `t < self.xmax`, `t` is not listed in
    /// `self.active` and `status(t) == TxnStatus::Committed`. `status` is consulted even for
    /// `t < self.xmin`: such a transaction is finished, but it may have been aborted.
    ///
    /// `own` is settled unconditionally, which is what makes a transaction see its own
    /// uncommitted writes and its own deletes. The `txn` module reuses this function in
    /// `check_write_conflict`.
    pub fn is_settled(&self, t: TxnId, status: &dyn Fn(TxnId) -> TxnStatus) -> bool {
        t == self.own
            || (t < self.xmax && !self.active.contains(&t) && status(t) == TxnStatus::Committed)
    }

    /// Whether the version `(xmin, xmax)` is visible to this snapshot.
    ///
    /// `true` when `xmin` is settled ([`Snapshot::is_settled`]) and `xmax` is not `Some(x)`
    /// with `x` settled. Consequences: a transaction sees its own uncommitted writes; a
    /// delete by another uncommitted transaction does not hide the row; a version whose
    /// creator rolled back is never visible; a delete by a rolled-back transaction hides
    /// nothing.
    ///
    /// Because the versions of a logical row form a chain (the `xmax` of one is the `xmin`
    /// of the next), at most one version of a row is visible to a given snapshot.
    pub fn is_visible(
        &self,
        xmin: TxnId,
        xmax: Option<TxnId>,
        status: &dyn Fn(TxnId) -> TxnStatus,
    ) -> bool {
        self.is_settled(xmin, status) && !xmax.is_some_and(|x| self.is_settled(x, status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot taken by transaction 5 while 3 and 5 are in progress and 6 is the next id.
    /// Transactions 1, 2 and 4 are finished; their fate is given by the status function.
    fn snap() -> Snapshot {
        Snapshot {
            xmin: TxnId(3),
            xmax: TxnId(6),
            active: vec![TxnId(3), TxnId(5)],
            own: TxnId(5),
        }
    }

    /// Statuses consistent with `snap()`: 1, 2 and 4 committed, 3 and 5 in progress,
    /// everything at or after 6 in progress.
    fn status(t: TxnId) -> TxnStatus {
        match t.0 {
            1 | 2 | 4 => TxnStatus::Committed,
            _ => TxnStatus::InProgress,
        }
    }

    /// A status function that marks `aborted` as `Aborted` and defers to `status` otherwise.
    fn aborting(aborted: u64) -> impl Fn(TxnId) -> TxnStatus {
        move |t| {
            if t.0 == aborted {
                TxnStatus::Aborted
            } else {
                status(t)
            }
        }
    }

    /// A status function that says `Committed` for `t` and defers to `status` otherwise
    /// (a transaction that committed after the snapshot was taken).
    fn committed_later(later: u64) -> impl Fn(TxnId) -> TxnStatus {
        move |t| {
            if t.0 == later {
                TxnStatus::Committed
            } else {
                status(t)
            }
        }
    }

    // --- is_settled -------------------------------------------------------------------

    #[test]
    fn own_is_always_settled() {
        assert!(snap().is_settled(TxnId(5), &status));
        assert!(snap().is_settled(TxnId(5), &aborting(5)));
    }

    #[test]
    fn committed_not_active_below_xmax_is_settled() {
        assert!(snap().is_settled(TxnId(4), &status));
        assert!(snap().is_settled(TxnId(1), &status));
    }

    #[test]
    fn active_in_progress_aborted_or_at_xmax_is_not_settled() {
        assert!(!snap().is_settled(TxnId(3), &status));
        assert!(!snap().is_settled(TxnId(3), &committed_later(3)));
        assert!(!snap().is_settled(TxnId(4), &aborting(4)));
        assert!(!snap().is_settled(TxnId(6), &committed_later(6)));
        assert!(!snap().is_settled(TxnId(7), &committed_later(7)));
    }

    // --- is_visible: creator (xmin) ---------------------------------------------------

    #[test]
    fn own_uncommitted_insert_is_visible_to_itself() {
        assert!(snap().is_visible(TxnId(5), None, &status));
    }

    #[test]
    fn own_delete_hides_row_from_itself() {
        assert!(!snap().is_visible(TxnId(2), Some(TxnId(5)), &status));
        // Inserted then deleted by the same transaction: invisible to it too.
        assert!(!snap().is_visible(TxnId(5), Some(TxnId(5)), &status));
    }

    #[test]
    fn own_is_visible_even_when_own_ge_xmax() {
        let snap = Snapshot {
            xmin: TxnId(3),
            xmax: TxnId(6),
            active: vec![TxnId(3)],
            own: TxnId(8),
        };
        assert!(snap.is_visible(TxnId(8), None, &status));
        assert!(!snap.is_visible(TxnId(2), Some(TxnId(8)), &status));
    }

    #[test]
    fn committed_before_snapshot_is_visible() {
        assert!(snap().is_visible(TxnId(4), None, &status));
    }

    #[test]
    fn committed_below_snapshot_xmin_is_visible() {
        assert!(snap().is_visible(TxnId(1), None, &status));
        assert!(snap().is_visible(TxnId(2), None, &status));
    }

    #[test]
    fn committed_but_listed_active_is_invisible() {
        // Transaction 3 committed after the snapshot was taken: still invisible.
        assert!(!snap().is_visible(TxnId(3), None, &committed_later(3)));
    }

    #[test]
    fn committed_at_or_after_xmax_is_invisible() {
        assert!(!snap().is_visible(TxnId(6), None, &committed_later(6)));
        assert!(!snap().is_visible(TxnId(9), None, &committed_later(9)));
    }

    #[test]
    fn in_progress_other_txn_is_invisible() {
        assert!(!snap().is_visible(TxnId(3), None, &status));
    }

    #[test]
    fn aborted_creator_is_invisible() {
        assert!(!snap().is_visible(TxnId(4), None, &aborting(4)));
    }

    #[test]
    fn aborted_below_snapshot_xmin_is_invisible() {
        assert!(!snap().is_visible(TxnId(1), None, &aborting(1)));
    }

    // --- is_visible: deleter (xmax) ---------------------------------------------------

    #[test]
    fn deleted_by_committed_settled_txn_is_invisible() {
        assert!(!snap().is_visible(TxnId(1), Some(TxnId(4)), &status));
        assert!(!snap().is_visible(TxnId(1), Some(TxnId(2)), &status));
    }

    #[test]
    fn deleted_by_committed_but_listed_active_is_still_visible() {
        assert!(snap().is_visible(TxnId(1), Some(TxnId(3)), &committed_later(3)));
    }

    #[test]
    fn deleted_by_in_progress_other_txn_is_still_visible() {
        assert!(snap().is_visible(TxnId(1), Some(TxnId(3)), &status));
        assert!(snap().is_visible(TxnId(1), Some(TxnId(7)), &status));
    }

    #[test]
    fn deleted_by_aborted_txn_is_still_visible() {
        assert!(snap().is_visible(TxnId(1), Some(TxnId(4)), &aborting(4)));
    }

    #[test]
    fn deleted_version_with_invisible_creator_is_invisible() {
        // The deleter alone never makes a version visible.
        assert!(!snap().is_visible(TxnId(3), Some(TxnId(4)), &status));
    }
}
