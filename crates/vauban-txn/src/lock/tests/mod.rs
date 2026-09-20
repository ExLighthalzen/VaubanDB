use super::*;

/// The queue of a resource is forgotten once nothing is held and nobody waits, so a
/// server that took and gave back many row locks does not keep one entry per row.
#[test]
fn an_emptied_resource_leaves_no_entry() {
    let mgr = LockManager::new();
    let res = LockResource::Row(TableId(1), RowId(7));
    let wait = LockWait::none();
    assert_eq!(
        mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait),
        LockOutcome::Granted
    );
    assert_eq!(mgr.state().entries.len(), 1);
    mgr.release_all(TxnId(1));
    assert!(mgr.state().entries.is_empty(), "entry dropped when empty");
}

/// A refused `NoWait` leaves nothing behind either: the candidate took itself out of
/// the queue before returning.
#[test]
fn a_refused_nowait_leaves_the_queue_empty() {
    let mgr = LockManager::new();
    let res = LockResource::Table(TableId(3));
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);
    assert_eq!(
        mgr.try_acquire(TxnId(2), res, LockMode::S, LockTimeout::NoWait, &wait),
        LockOutcome::TimedOut
    );
    assert!(mgr.waiters().is_empty());
    assert_eq!(mgr.held(TxnId(1)), vec![(res, LockMode::X)]);
}

/// The ticket counter saturates instead of wrapping round to a ticket in use, the
/// bound `TransactionManager::begin` already takes on its identifiers.
#[test]
fn the_ticket_counter_saturates() {
    let mgr = LockManager::new();
    mgr.state().next_ticket = u64::MAX;
    let res = LockResource::Table(TableId(1));
    let wait = LockWait::none();
    mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);
    mgr.try_acquire(TxnId(2), res, LockMode::X, LockTimeout::NoWait, &wait);
    assert_eq!(mgr.state().next_ticket, u64::MAX);
}
