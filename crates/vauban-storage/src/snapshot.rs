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
mod tests;
