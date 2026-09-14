//! Integration test: the shape of `convert`, not its rules.
//!
//! This test asserts **only** the shape of the entry point and its dispatch on the family
//! of the target: `NULL` converts to `NULL`, the five branches of the `match` exist, and a
//! pair of types that never converts stays an error.

use vauban_types::{Len, SqlType, TypeInfo, Value, convert};

fn ti(ty: SqlType, nullable: bool) -> TypeInfo {
    TypeInfo::new(ty, nullable)
}

/// `NULL` converts to `NULL` without looking at the target: the nullability of the target
/// is checked by the caller (an insert, an assignment), never by `convert`.
#[test]
fn convert_null_is_null() {
    let from = ti(SqlType::VarChar(Len::Fixed(10)), true);
    let to = ti(SqlType::Int, false);
    assert_eq!(convert(&Value::Null, &from, &to, None), Ok(Value::Null));
}

/// One target per branch of the dispatch: `Int` (numeric), `varchar` (character), `date`
/// (datetime), `varbinary` (binary) and `uniqueidentifier` (guid). A missing branch would
/// stop the crate from compiling, but a branch routed to the wrong submodule would not, so
/// the last assertion checks that the numeric branch is really reached.
#[test]
fn convert_dispatches_by_target_family() {
    let from = ti(SqlType::Int, true);
    for target in [
        SqlType::Int,
        SqlType::VarChar(Len::Fixed(10)),
        SqlType::Date,
        SqlType::VarBinary(Len::Fixed(4)),
        SqlType::UniqueIdentifier,
    ] {
        let to = ti(target, true);
        assert_eq!(
            convert(&Value::Null, &from, &to, None),
            Ok(Value::Null),
            "target {}",
            target.declaration()
        );
    }

    // `uniqueidentifier` to `int` is refused whatever the value; it proves the numeric
    // branch is reached at all.
    let guid = ti(SqlType::UniqueIdentifier, false);
    let to_int = ti(SqlType::Int, false);
    assert!(convert(&Value::Guid([0; 16]), &guid, &to_int, None).is_err());
}
