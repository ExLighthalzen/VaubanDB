//! The errors this crate raises, as a thin adapter over `vauban-errors`.
//!
//! No message text lives here: the catalogue (number, severity, template) and the named
//! constructors belong to the `errors` crate. This module turns a [`SqlType`] into the
//! name the message prints and delegates, so that the messages of the crate go through
//! one place.
//!
//! Type spelling depends on the message. With `decimal(18,6)` and `numeric(18,6)`:
//! 402 (`& 1`), 529 (casts to/from the four modern temporal types) and 206 (CASE with a
//! date alternative) preserve the declared spelling. 8114 from `CAST('x' AS ...)`
//! normalizes the target to `numeric`. Adding 'x' to the exact numeric value also produces
//! 8114, not 245; first casting that value to int produces 245, which names varchar and
//! int and cannot discriminate the original exact numeric spelling. Other adapters retain
//! their existing policy (`tests::exact_numeric_error_spellings`).
//!
//! **States.** The state of a conversion overflow is a property of the (source, target)
//! pair, not of the number: 232 towards `int` is state 3 and towards `money` state 2, 8115
//! towards `numeric` is 6 from a `float` and 8 from anything else, 237 is 1, 2 or 3
//! according to the target. The adapters of an overflow therefore delegate to the
//! constructors that take the source, `overflow_for_data_type_from`,
//! `overflow_for_type_from` and `arithmetic_overflow_from`, and not to the flat ones, which
//! send state 2 regardless of the pair. The single exception is [`arithmetic_overflow`],
//! the 8115 of an **operator**, whose state is 2 whatever the target
//! (`tests::overflow_states_follow_the_source_and_the_target`).
//!
//! If a number, a template or a state is missing or wrong, the fix belongs to the `errors`
//! crate; nothing here builds a `SqlError` by hand.

use vauban_errors::{InternalError, SqlError};

use crate::SqlType;

/// Error 245: a conversion of `value` (read as type `from`) to type `to` failed.
///
/// ```text
/// The varchar value 'abc' could not be converted to data type int.
/// ```
#[allow(dead_code)] // kept for the character-to-integer path and its tests
pub(crate) fn conversion_failed(from: &SqlType, value: &str, to: &SqlType) -> SqlError {
    SqlError::conversion_failed(from.error_name(), value, to.error_name())
}

/// Error 8114: a conversion between two types failed; the message does not quote the value.
///
/// ```text
/// Data type varchar could not be converted to numeric.
/// ```
pub(crate) fn error_converting(from: &SqlType, to: &SqlType) -> SqlError {
    SqlError::error_converting_data_type(from.error_name(), to.error_name())
}

/// Error 8115, state 2: the result of an **operator** does not fit in type `to`.
///
/// `from` is a string, not a `&SqlType`, because the message carries there either a type
/// name (`Converting int to data type numeric`) or the literal word `expression` when the
/// value is the result of a computation (`Converting expression to data type int`).
/// Callers pass `from.ty.error_name()` or `"expression"`.
///
/// A **conversion** takes [`arithmetic_overflow_from`] instead, which carries the state
/// of the pair: `SELECT CAST(CAST(123456789 AS numeric(9,0)) * 10 AS ...)` overflowing an
/// operator sends state 2 even towards `numeric`, where a `CAST` towards `numeric` sends 8
/// (`tests::overflow_states_follow_the_source_and_the_target`).
pub(crate) fn arithmetic_overflow(from: &str, to: &SqlType) -> SqlError {
    SqlError::arithmetic_overflow(from, to.error_name())
}

/// Error 8115 of a **conversion**, with the state of the (`from`, `to`) pair: 4, 5, 6, 8,
/// and 2 for the other pairs.
///
/// Same reading of `from` as [`arithmetic_overflow`]. `to` is the type the message prints,
/// which is not necessarily the declared target: a character target prints its
/// variable-length name (`varchar` for a `char(3)`), so the caller passes that spelling.
pub(crate) fn arithmetic_overflow_from(from: &str, to: &SqlType) -> SqlError {
    SqlError::arithmetic_overflow_from(from, to.error_name())
}

/// Error 8134: the divisor of a division or a modulo evaluated to zero.
pub(crate) fn divide_by_zero() -> SqlError {
    SqlError::divide_by_zero()
}

/// Error 242: the value is a valid `from` but falls outside the range of `to`.
///
/// ```text
/// Converting date to datetime produced a value outside the target range.
/// ```
pub(crate) fn out_of_range(from: &SqlType, to: &SqlType) -> SqlError {
    SqlError::out_of_range_conversion(from.error_name(), to.error_name())
}

/// Error 220: an integral value does not fit in type `to`.
///
/// `from` does not appear in the message; it selects the state of the pair, which indexes
/// the conversion routine and not the target: an `int` and a `money` towards the same
/// `smallint` send 1 and 7.
///
/// ```text
/// Value out of range for data type tinyint: 300.
/// ```
pub(crate) fn overflow_for_data_type(from: &SqlType, to: &SqlType, value: i64) -> SqlError {
    SqlError::overflow_for_data_type_from(from.error_name(), to.error_name(), value)
}

/// Error 232: a floating-point value does not fit in type `to`.
///
/// Same reading of `from` as [`overflow_for_data_type`]. The rendering of the value
/// belongs to the `errors` crate (`%f`), not here.
pub(crate) fn overflow_for_type(from: &SqlType, to: &SqlType, value: f64) -> SqlError {
    SqlError::overflow_for_type_from(from.error_name(), to.error_name(), value)
}

/// Error 237: a `money` value has no room in the numeric type `to`.
///
/// ```text
/// A money value does not fit in the result type smallmoney.
/// ```
pub(crate) fn insufficient_result_space_money(to: &SqlType) -> SqlError {
    SqlError::insufficient_result_space_money(to.error_name())
}

/// Error 234: a `money` value has no room in the character type `to`, which the message
/// names by its variable-length spelling (`varchar` for a `char(3)`).
///
/// ```text
/// A money value does not fit in the result type varchar.
/// ```
pub(crate) fn insufficient_result_space_money_to(to: &SqlType) -> SqlError {
    SqlError::insufficient_result_space_money_to(to.error_name())
}

/// Error 292: the `smallmoney` twin of [`insufficient_result_space_money_to`].
pub(crate) fn insufficient_result_space_smallmoney_to(to: &SqlType) -> SqlError {
    SqlError::insufficient_result_space_smallmoney_to(to.error_name())
}

/// Error 8170: a GUID needs 36 characters and the narrow character target is shorter. The
/// message names neither the target nor its length, so there is nothing to adapt.
pub(crate) fn insufficient_result_space_guid() -> SqlError {
    SqlError::insufficient_result_space_guid()
}

/// Error 402: the *pair* of operand types has no meaning for `operator`, which is printed
/// as given (`add`, `subtract`, `'&'`). The neighbouring refusal that names a single
/// operand is error 8117, [`invalid_operand_type`].
///
/// Preserves the declared spelling on `SELECT CAST(1.5 AS decimal(18,6)) & 1;`
/// and its `numeric` counterpart.
///
/// ```text
/// The types bit and bit cannot be combined by the add operator.
/// ```
pub(crate) fn incompatible_types_for_operator(
    left: &SqlType,
    right: &SqlType,
    operator: &str,
) -> SqlError {
    SqlError::incompatible_types_for_operator(left.name(), right.name(), operator)
}

/// Error 1007: a numeric literal needs more than the 38 digits `numeric` offers. `text` is
/// the literal as the user wrote it.
pub(crate) fn number_out_of_numeric_range(text: &str) -> SqlError {
    SqlError::number_out_of_numeric_range(text)
}

/// Error 168: a `float` literal falls outside a double. `text` is the literal as the user
/// wrote it.
pub(crate) fn float_out_of_range(text: &str) -> SqlError {
    SqlError::float_out_of_range(text)
}

/// Error 151: a `money` literal falls outside `money`.
///
/// The message quotes the literal **with** its currency sign, which the payload of a
/// `Money` literal does not carry (README of `types`, convention of `parse_literal`): the
/// `$` is put back here, the one place that adapts a value to a message.
pub(crate) fn invalid_money_value(amount: &str) -> SqlError {
    SqlError::invalid_money_value(&format!("${amount}"))
}

/// Error 248: a character value is a well-formed integer but too large for `int`, the one
/// target this number serves. `text` is the value as the message echoes it, spaces included.
pub(crate) fn conversion_overflowed_int(from: &SqlType, text: &str) -> SqlError {
    SqlError::conversion_overflowed_int(from.error_name(), text)
}

/// Error 244: the `tinyint` and `smallint` twin of [`conversion_overflowed_int`].
///
/// The message names those two targets by their internal column type, `INT1` and `INT2`,
/// and that name drives the state (1 and 2); the mapping belongs here rather than in the
/// caller, which knows just the [`SqlType`].
pub(crate) fn conversion_overflowed_small_int(
    from: &SqlType,
    text: &str,
    to: &SqlType,
) -> SqlError {
    let column = if matches!(to, SqlType::TinyInt) {
        "INT1"
    } else {
        "INT2"
    };
    SqlError::conversion_overflowed_small_int(from.error_name(), text, column)
}

/// Error 235: a character value towards `money` is not a money literal. The message names
/// neither the value nor the source type, so there is nothing to adapt.
pub(crate) fn char_to_money_syntax() -> SqlError {
    SqlError::char_to_money_syntax()
}

/// Error 8152: a string or binary value is too long for its destination. A `CAST` raises it
/// too, on the three scaled types of 2008 towards a binary target narrower than their
/// storage (`tests/convert_datetime.rs`).
pub(crate) fn truncated() -> SqlError {
    SqlError::string_or_binary_truncated()
}

/// Error 448: `name` is not a collation the server knows.
pub(crate) fn invalid_collation(name: &str) -> SqlError {
    SqlError::invalid_collation(name)
}

/// Error 9809: the `CONVERT` style number means nothing for that pair of types.
pub(crate) fn unsupported_style(style: i32, from: &SqlType, to: &SqlType) -> SqlError {
    SqlError::unsupported_convert_style(style, from.error_name(), to.error_name())
}

/// Error 281: `style` is no style number at all for a conversion from the date type
/// `from` towards a character type. The target is not named: the template spells it out
/// as "a character string", `varchar` and `nvarchar` alike, so just the source is
/// adapted here.
pub(crate) fn invalid_style_number(style: i32, from: &SqlType) -> SqlError {
    SqlError::invalid_style_number(style, from.error_name())
}

/// Error 9807: a character string does not have the shape the **strict** style `style`
/// demands. The message names neither the value nor the types, so there is nothing to
/// adapt; the adapter exists so that the messages of the crate go through this module.
///
/// Order matters on the reading path, and it is the caller's job: a style the pair does not
/// support is refused **before** the string is read, by [`unsupported_style`] (9809). See
/// the rustdoc of `SqlError::input_does_not_follow_style`.
pub(crate) fn does_not_follow_style(style: i32) -> SqlError {
    SqlError::input_does_not_follow_style(style)
}

/// Error 8169: a character string is not a GUID. The message does not echo the value.
pub(crate) fn conversion_failed_guid() -> SqlError {
    SqlError::conversion_failed_guid()
}

/// Error 529: that pair of types has no explicit conversion at all, so `CAST` refuses it.
pub(crate) fn explicit_conversion_not_allowed(from: &SqlType, to: &SqlType) -> SqlError {
    SqlError::explicit_conversion_not_allowed(from.name(), to.name())
}

/// Error 257: that pair of types has an explicit conversion but no implicit one.
pub(crate) fn implicit_conversion_not_allowed(from: &SqlType, to: &SqlType) -> SqlError {
    SqlError::implicit_conversion_not_allowed(from.error_name(), to.error_name())
}

/// Error 206: two operands have types that cannot be reconciled.
pub(crate) fn operand_type_clash(left: &SqlType, right: &SqlType) -> SqlError {
    SqlError::operand_type_clash(left.name(), right.name())
}

/// Error 8117: `operator` has no meaning for that operand type. The operator is printed
/// as given: the symbolic ones are quoted (`'~'`), the named ones are not.
pub(crate) fn invalid_operand_type(ty: &SqlType, operator: &str) -> SqlError {
    SqlError::invalid_operand_type(ty.error_name(), operator)
}

/// Error 50000: a broken precondition of this crate, or a rule not implemented yet.
///
/// This is an [`InternalError::Bug`] converted at the boundary; it never describes a
/// mistake the user made.
pub(crate) fn bug(msg: impl Into<String>) -> SqlError {
    InternalError::Bug(msg.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Len, SqlType};

    /// The texts are **not** written in this crate: they come from the catalogue of the
    /// `errors` crate. This test checks that the adapter passes the right arguments, in
    /// the right order, with `error_name()` and not `name()`.
    #[test]
    fn adapters_pass_their_arguments_in_order() {
        let e = out_of_range(&SqlType::Date, &SqlType::DateTime);
        assert_eq!(e.number, 242);
        assert_eq!(e.severity, 16);
        assert_eq!(
            e.message,
            "Converting date to datetime produced a value outside the target range."
        );

        let e = overflow_for_data_type(&SqlType::Int, &SqlType::TinyInt, 300);
        assert_eq!(e.number, 220);
        assert_eq!(e.severity, 16);
        assert_eq!(e.message, "Value out of range for data type tinyint: 300.");

        let e = conversion_failed(&SqlType::VarChar(Len::Fixed(3)), "abc", &SqlType::Int);
        assert_eq!(e.number, 245);
        assert_eq!(
            e.message,
            "The varchar value 'abc' could not be converted to data type int."
        );

        let e = arithmetic_overflow(
            SqlType::Numeric {
                precision: 5,
                scale: 2,
            }
            .error_name(),
            &SqlType::VarChar(Len::Fixed(2)),
        );
        assert_eq!(e.number, 8115);
        assert_eq!(
            e.message,
            "Converting numeric to data type varchar overflowed."
        );

        // The target `decimal(3, 0)` is named `numeric`: `error_name()`, not `name()`.
        let e = arithmetic_overflow(
            "int",
            &SqlType::Decimal {
                precision: 3,
                scale: 0,
            },
        );
        assert_eq!(e.number, 8115);
        assert_eq!(e.message, "Converting int to data type numeric overflowed.");

        // The source is a computation, not a type.
        let e = arithmetic_overflow("expression", &SqlType::Int);
        assert_eq!(e.number, 8115);
        assert_eq!(
            e.message,
            "Converting expression to data type int overflowed."
        );

        assert_eq!(invalid_collation("Klingon_CI_AS").number, 448);
        assert_eq!(
            unsupported_style(112, &SqlType::Date, &SqlType::DateTimeOffset(7)).number,
            9809
        );
        assert_eq!(conversion_failed_guid().number, 8169);
    }

    /// Regression coverage for the adapters retaining the internal numeric spelling.
    #[test]
    fn adapters_retaining_numeric_spelling() {
        let d = SqlType::Decimal {
            precision: 5,
            scale: 2,
        };
        for message in [
            conversion_failed(&d, "1.5", &SqlType::Int).message,
            error_converting(&d, &SqlType::Int).message,
            out_of_range(&d, &SqlType::Int).message,
            unsupported_style(1, &d, &SqlType::Int).message,
            implicit_conversion_not_allowed(&d, &SqlType::Int).message,
            invalid_operand_type(&d, "~").message,
            arithmetic_overflow_from("int", &d).message,
            overflow_for_data_type(&SqlType::Int, &d, 1).message,
            overflow_for_type(&SqlType::Float, &d, 1.0).message,
            insufficient_result_space_money(&d).message,
            conversion_overflowed_int(&d, "1").message,
        ] {
            assert!(message.contains("numeric"), "{message}");
            assert!(!message.contains("decimal"), "{message}");
        }

        // 402 prints the declared spelling
        // (`SELECT CAST(1.5 AS decimal(2,1)) & 1;`).
        let clash = incompatible_types_for_operator(&d, &SqlType::Int, "'&'");
        assert_eq!(clash.number, 402);
        assert_eq!(
            clash.message,
            "The types decimal and int cannot be combined by the '&' operator."
        );
        assert_eq!(
            incompatible_types_for_operator(
                &SqlType::Numeric {
                    precision: 5,
                    scale: 2
                },
                &SqlType::Int,
                "'&'"
            )
            .message,
            "The types numeric and int cannot be combined by the '&' operator."
        );
    }

    #[test]
    fn exact_numeric_error_spellings() {
        for (ty, spelling) in [
            (
                SqlType::Decimal {
                    precision: 18,
                    scale: 6,
                },
                "decimal",
            ),
            (
                SqlType::Numeric {
                    precision: 18,
                    scale: 6,
                },
                "numeric",
            ),
        ] {
            for temporal in [
                SqlType::Date,
                SqlType::Time(3),
                SqlType::DateTime2(3),
                SqlType::DateTimeOffset(3),
            ] {
                for (from, to) in [(ty, temporal), (temporal, ty)] {
                    assert_eq!(
                        explicit_conversion_not_allowed(&from, &to).message,
                        format!(
                            "No explicit conversion exists from {} to {}.",
                            from.name(),
                            to.name()
                        )
                    );
                }
            }
            assert_eq!(
                operand_type_clash(&ty, &SqlType::Date).message,
                format!("Type mismatch: {spelling} cannot be combined with date.")
            );
            assert_eq!(
                error_converting(&SqlType::VarChar(Len::Fixed(1)), &ty).message,
                "Data type varchar could not be converted to numeric."
            );
        }
    }

    /// The numbers of the adapters that have no text assertion above, so that a wrong
    /// delegation is caught here rather than at the first call site.
    #[test]
    fn remaining_adapters_carry_their_number() {
        assert_eq!(
            error_converting(&SqlType::VarChar(Len::Max), &SqlType::Int).number,
            8114
        );
        assert_eq!(divide_by_zero().number, 8134);
        assert_eq!(
            overflow_for_type(&SqlType::Float, &SqlType::Real, 1e40).number,
            232
        );
        assert_eq!(truncated().number, 8152);
        assert_eq!(
            explicit_conversion_not_allowed(&SqlType::DateTime, &SqlType::UniqueIdentifier).number,
            529
        );
        assert_eq!(
            implicit_conversion_not_allowed(&SqlType::Date, &SqlType::Int).number,
            257
        );
        assert_eq!(
            operand_type_clash(&SqlType::Date, &SqlType::Int).number,
            206
        );
        assert_eq!(invalid_operand_type(&SqlType::Float, "~").number, 8117);
    }

    /// The state of a **conversion** overflow follows the (source, target) pair, and the
    /// adapters must therefore delegate to the `_from` constructors, not to the flat ones
    /// which send state 2 regardless of the pair.
    ///
    /// `SELECT CAST(CAST(1e20 AS float) AS <target>);` raises 232 state 1 towards
    /// `tinyint`, state 2 towards `smallint`, state 3 towards `int`, state 2 towards
    /// `money` and `smallmoney`; `SELECT CAST(CAST(1234.5 AS float) AS numeric(5,2));`
    /// raises 8115 state 6.
    #[test]
    fn overflow_states_follow_the_source_and_the_target() {
        let numeric = SqlType::Numeric {
            precision: 5,
            scale: 2,
        };
        for (to, state) in [
            (SqlType::TinyInt, 1),
            (SqlType::SmallInt, 2),
            (SqlType::Int, 3),
            (SqlType::Money, 2),
            (SqlType::SmallMoney, 2),
            (SqlType::Real, 2),
        ] {
            let e = overflow_for_type(&SqlType::Float, &to, 1e20);
            assert_eq!(e.number, 232, "{to:?}");
            assert_eq!(e.state, state, "{to:?}");
        }

        let e = arithmetic_overflow_from(SqlType::Float.error_name(), &numeric);
        assert_eq!(e.number, 8115);
        assert_eq!(e.state, 6);
        assert_eq!(
            e.message,
            "Converting float to data type numeric overflowed."
        );

        // A source that is not approximate takes another state towards `numeric`:
        // `SELECT CAST(CAST(214749 AS money) AS numeric(5,0));` raises 8115 state 8.
        assert_eq!(
            arithmetic_overflow_from(SqlType::Money.error_name(), &numeric).state,
            8
        );

        // 220 carries a state of the pair too: `SELECT CAST(CAST(300 AS int) AS tinyint);`
        // raises state 2, `DECLARE @m money = 40000; SELECT CAST(@m AS smallint);`
        // raises state 7.
        assert_eq!(
            overflow_for_data_type(&SqlType::Int, &SqlType::TinyInt, 300).state,
            2
        );
        assert_eq!(
            overflow_for_data_type(&SqlType::Money, &SqlType::SmallInt, 400_000_000).state,
            7
        );

        // The **operator** overflow keeps state 2 whatever the target, which is why the
        // flat constructor stays: `SELECT CAST(CAST(123456789 AS numeric(9,0)) * 10 AS
        // numeric(9,0));` raises 8115 state 2 where the `CAST` above raises 8.
        assert_eq!(arithmetic_overflow("expression", &numeric).state, 2);
        assert_eq!(arithmetic_overflow("expression", &SqlType::Int).state, 2);
    }

    /// Error 237 follows its **target** alone, the source being `money` in each case:
    /// `DECLARE @m money = 99999999999; SELECT CAST(@m AS int);` raises state 1,
    /// `DECLARE @m money = 214749; SELECT CAST(@m AS smallint);` state 2 and the same
    /// towards `tinyint` state 3, and `DECLARE @m money = 300000; SELECT CAST(@m AS
    /// smallmoney);` state 3.
    #[test]
    fn insufficient_result_space_states_follow_the_target() {
        for (to, state) in [
            (SqlType::Int, 1),
            (SqlType::SmallInt, 2),
            (SqlType::TinyInt, 3),
            (SqlType::SmallMoney, 3),
        ] {
            let e = insufficient_result_space_money(&to);
            assert_eq!(e.number, 237, "{to:?}");
            assert_eq!(e.severity, 16, "{to:?}");
            assert_eq!(e.state, state, "{to:?}");
            assert_eq!(
                e.message,
                format!(
                    "A money value does not fit in the result type {}.",
                    to.error_name()
                ),
                "{to:?}"
            );
        }
    }

    #[test]
    fn bug_is_the_generic_internal_error() {
        let e = bug("convert: numeric not implemented");
        assert_eq!(e.number, 50000);
        assert_eq!(e.severity, 16);
        assert!(e.message.contains("convert: numeric not implemented"));
    }
}
