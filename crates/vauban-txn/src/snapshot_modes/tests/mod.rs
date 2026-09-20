use super::*;

/// The table of the module documentation, held as data: level and options, then the
/// mode.
#[test]
fn the_mode_table_is_the_one_documented() {
    let off = VersioningOptions::default();
    let rcsi = VersioningOptions {
        read_committed_snapshot: true,
        ..off
    };
    let asi = VersioningOptions {
        allow_snapshot_isolation: true,
        ..off
    };
    let both = VersioningOptions {
        read_committed_snapshot: true,
        allow_snapshot_isolation: true,
    };
    use IsolationLevel::*;
    use VersioningMode::*;
    let expected = [
        (ReadCommitted, rcsi, StatementSnapshot),
        (ReadCommitted, both, StatementSnapshot),
        (ReadCommitted, off, Locking),
        (ReadCommitted, asi, Locking),
        (Snapshot, asi, TxnSnapshot),
        (Snapshot, both, TxnSnapshot),
        (Snapshot, off, Locking),
        (Snapshot, rcsi, Locking),
        (ReadUncommitted, both, Locking),
        (RepeatableRead, both, Locking),
        (Serializable, both, Locking),
    ];
    for (level, opts, mode) in expected {
        assert_eq!(opts.mode_for(level), mode, "{level:?} under {opts:?}");
    }
}

/// The refusal names the method and the database, like the other internal errors of
/// the manager.
#[test]
fn the_refusal_names_the_database() {
    let err = snapshot_not_allowed(DbId(7));
    assert_eq!(err.number, 50000);
    assert!(err.message.contains("begin_in"), "{}", err.message);
    assert!(err.message.contains("database 7"), "{}", err.message);
}
