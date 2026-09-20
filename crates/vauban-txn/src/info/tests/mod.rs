use super::*;
use vauban_storage::{RowId, TableId};

/// `Display` on a status writes the upper-case word of the three.
#[test]
fn status_strings_are_upper_case() {
    assert_eq!(LockStatus::Grant.to_string(), "GRANT");
    assert_eq!(LockStatus::Wait.to_string(), "WAIT");
    assert_eq!(LockStatus::Convert.to_string(), "CONVERT");
}

/// `Display` on a mode writes what `short_name` returns, for the seven modes.
#[test]
fn display_of_a_mode_is_its_short_name() {
    for mode in [
        LockMode::S,
        LockMode::X,
        LockMode::U,
        LockMode::IS,
        LockMode::IX,
        LockMode::SchS,
        LockMode::SchM,
    ] {
        assert_eq!(mode.to_string(), mode.short_name());
    }
}

/// A row is a `ROW`, a table is an `OBJECT`.
#[test]
fn resource_kind_tells_row_from_table() {
    assert_eq!(
        LockResource::Row(TableId(1), RowId(2)).resource_kind(),
        "ROW"
    );
    assert_eq!(LockResource::Table(TableId(1)).resource_kind(), "OBJECT");
}
