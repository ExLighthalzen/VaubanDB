use super::*;
use crate::IsolationLevel;
use std::sync::Arc;
use vauban_storage::MemoryStorage;

/// A schema lock waits without a deadline until `SET LOCK_TIMEOUT` is put on the handle,
/// at the level the transaction was opened at.
#[test]
fn the_schema_lock_waits_for_ever_by_default() {
    let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
        IsolationLevel::Snapshot,
    ] {
        let txn = mgr.begin(level);
        assert_eq!(
            session_timeout(&txn),
            LockTimeout::Infinite,
            "the wait of a schema lock at {level:?}"
        );
    }
}

/// The two methods name the two modes of the schema family, on the table resource.
#[test]
fn the_two_methods_take_the_two_schema_modes() {
    let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_stability_lock(&reader, TableId(7))
        .expect("the table is free");
    assert_eq!(
        mgr.locks().held(reader.id),
        vec![(LockResource::Table(TableId(7)), LockMode::SchS)]
    );

    let ddl = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.schema_modify_lock(&ddl, TableId(8))
        .expect("the other table is free");
    assert_eq!(
        mgr.locks().held(ddl.id),
        vec![(LockResource::Table(TableId(8)), LockMode::SchM)]
    );
}
