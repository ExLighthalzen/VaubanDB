use super::*;

/// The table of the module documentation, held as data: level, then the mode a plain
/// read takes and whether `end_row_read` gives it back.
#[test]
fn the_policy_table_is_the_one_documented() {
    let plain = LockIntent::default();
    let expected = [
        (IsolationLevel::ReadUncommitted, None, true),
        (IsolationLevel::ReadCommitted, Some(LockMode::S), true),
        (IsolationLevel::RepeatableRead, Some(LockMode::S), false),
        (IsolationLevel::Serializable, Some(LockMode::S), false),
        (IsolationLevel::Snapshot, Some(LockMode::S), true),
    ];
    for (level, mode, released) in expected {
        assert_eq!(read_mode(level, &plain), mode, "row mode at {level:?}");
        assert_eq!(
            releases_at_end_of_row(level),
            released,
            "end_row_read at {level:?}"
        );
    }
}

/// The hints that name a mode are read before the level, so `UPDLOCK` and `XLOCK` take
/// theirs under `ReadUncommitted` as under `Serializable`.
#[test]
fn updlock_and_xlock_are_read_before_the_level() {
    let updlock = LockIntent {
        updlock: true,
        ..LockIntent::default()
    };
    let xlock = LockIntent {
        xlock: true,
        ..LockIntent::default()
    };
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::Serializable,
    ] {
        assert_eq!(read_mode(level, &updlock), Some(LockMode::U));
        assert_eq!(read_mode(level, &xlock), Some(LockMode::X));
    }
    // XLOCK wins over UPDLOCK when both are set, the stronger of the two.
    let both = LockIntent {
        updlock: true,
        xlock: true,
        ..LockIntent::default()
    };
    assert_eq!(
        read_mode(IsolationLevel::ReadCommitted, &both),
        Some(LockMode::X)
    );
}

/// The intent mode under each row mode: `IS` under an `S`, `IX` under the four others.
#[test]
fn intent_follows_the_row_mode() {
    assert_eq!(intent_of(LockMode::S), LockMode::IS);
    assert_eq!(intent_of(LockMode::U), LockMode::IX);
    assert_eq!(intent_of(LockMode::X), LockMode::IX);
}
