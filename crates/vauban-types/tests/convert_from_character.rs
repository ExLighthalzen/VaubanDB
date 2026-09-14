//! Integration test: what a string-to-number conversion looks like from outside the crate.
//!
//! `string_to_numeric` is `pub(crate)`: the only public way in is `convert`, which
//! reaches it through `convert::numeric::to_numeric`. The grammar vectors
//! (`string_to_integer`, `string_to_decimal_and_money`,
//! `string_to_bit_follows_the_integer_grammar`, …) live in the unit tests of
//! `src/convert/from_character.rs`, where they can call the function.
//!
//! What this file locks is the public part of the contract: a `NULL` string converts to
//! `NULL` towards **every** numeric target, without the conversion rules ever being
//! consulted.

use vauban_types::{Len, SqlType, TypeInfo, Value, convert};

/// The ten numeric targets `string_to_numeric` accepts, one per family.
fn numeric_targets() -> Vec<SqlType> {
    vec![
        SqlType::Bit,
        SqlType::TinyInt,
        SqlType::SmallInt,
        SqlType::Int,
        SqlType::BigInt,
        SqlType::Decimal {
            precision: 5,
            scale: 2,
        },
        SqlType::Numeric {
            precision: 5,
            scale: 2,
        },
        SqlType::Float,
        SqlType::Real,
        SqlType::Money,
    ]
}

/// `CAST(NULL AS int)` is `NULL`, and so is every other numeric target: `convert` answers
/// before any string is read, so no error number can depend on the target here.
#[test]
fn null_string_converts_to_null_for_every_numeric_target() {
    for source in [
        SqlType::Char(Len::Fixed(3)),
        SqlType::VarChar(Len::Fixed(30)),
        SqlType::NChar(Len::Fixed(3)),
        SqlType::NVarChar(Len::Max),
    ] {
        let from = TypeInfo::new(source, true);
        for target in numeric_targets() {
            let to = TypeInfo::new(target, true);
            assert_eq!(
                convert(&Value::Null, &from, &to, None),
                Ok(Value::Null),
                "{} to {}",
                source.declaration(),
                target.declaration()
            );
        }
    }
}

/// A character source keeps its collation in its `TypeInfo`, and a numeric target has
/// none: the pair `string_to_numeric` receives is well formed whatever the target.
#[test]
fn character_source_carries_a_collation_and_numeric_targets_do_not() {
    let from = TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), true);
    assert!(from.collation.is_some());
    for target in numeric_targets() {
        assert_eq!(
            TypeInfo::new(target, true).collation,
            None,
            "{}",
            target.declaration()
        );
    }
}
