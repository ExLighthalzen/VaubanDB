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
