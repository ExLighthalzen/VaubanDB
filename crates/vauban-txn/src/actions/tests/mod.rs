use super::*;
use vauban_storage::MemoryStorage;

/// A log that never saw `sp` reports no position: this is what turns an invalid savepoint
/// into a caller bug in
/// [`TransactionManager::rollback_to`](crate::TransactionManager::rollback_to).
#[test]
fn an_unknown_savepoint_has_no_position() {
    let mut log = ActionLog::default();
    assert_eq!(log.position_of(SavepointId(1)), None);
    log.push_on_commit(CommitAction::DropTable(TableId(7)));
    log.mark(SavepointId(1));
    assert_eq!(log.position_of(SavepointId(1)), Some(1));
    assert_eq!(log.position_of(SavepointId(2)), None);
}

/// `unwind_after` keeps the mark it cuts at, so the same savepoint can be rolled back to
/// twice, and forgets what follows it.
#[test]
fn unwinding_keeps_its_own_mark() {
    let storage = MemoryStorage::new();
    let mut log = ActionLog::default();
    log.mark(SavepointId(1));
    log.push_on_commit(CommitAction::DropTable(TableId(7)));
    log.mark(SavepointId(2));
    log.unwind_after(0, &storage)
        .expect("no compensation to run");
    assert_eq!(log.position_of(SavepointId(1)), Some(0));
    assert_eq!(log.position_of(SavepointId(2)), None, "invalidated");
    assert_eq!(log.entries.len(), 1, "the deferred drop is forgotten");
    log.unwind_after(0, &storage).expect("the same mark twice");
    assert_eq!(log.position_of(SavepointId(1)), Some(0));
}
