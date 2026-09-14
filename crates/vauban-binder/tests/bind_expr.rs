//! The contract `expr.rs` leans on, checked from outside the crate.
//!
//! # Why the binding tests themselves are not here
//!
//! `bind_expr` and `bind_condition` are `pub(crate)`: `pub` is for what the crate
//! exposes. An integration test sees the public surface and nothing else, so the tests of
//! the two functions live next to them, in `src/expr.rs`, and build the AST node by hand.
//!
//! # What this file does check
//!
//! The two contracts the binder reads as given, so that a change in `types` or in `errors`
//! breaks the crate that depends on them and not the one that changed alone: the answers
//! of the typing functions the binder relays, and the exact texts of the messages it
//! raises.

use vauban_binder::{BoundExpr, BoundExprKind, CompareOp};
use vauban_errors::SqlError;
use vauban_types::{
    BinaryOp, Collation, Len, SqlType, TypeInfo, Value, binary_op_type, implicit_result_type,
};

/// A `numeric(p, s)`, not nullable.
fn numeric(precision: u8, scale: u8) -> TypeInfo {
    TypeInfo::new(SqlType::Numeric { precision, scale }, false)
}

/// The typing answers `bind_arith`, `bind_comparison`, `bind_case` and `bind_in` relay
/// without recomputing them.
///
/// The first two assertions are the two halves of the rule that makes the binder
/// substitute a type before it calls in: an integer **literal** enters the precision table
/// with its own number of digits (`SELECT SQL_VARIANT_PROPERTY(1 + 1.5, 'Precision');`
/// answers 3), an `int` with the precision of its type (the same query over
/// `CAST(1 AS int)` answers 12).
#[test]
fn types_answers_the_questions_the_binder_asks_it() {
    let sum = binary_op_type(BinaryOp::Add, &numeric(1, 0), &numeric(2, 1)).expect("1 + 1.5");
    assert_eq!(
        sum.ty,
        SqlType::Numeric {
            precision: 3,
            scale: 1
        }
    );
    let sum = binary_op_type(
        BinaryOp::Add,
        &TypeInfo::new(SqlType::Int, false),
        &numeric(2, 1),
    )
    .expect("int + numeric(2,1)");
    assert_eq!(
        sum.ty,
        SqlType::Numeric {
            precision: 12,
            scale: 1
        }
    );

    // The common type of a comparison: `int` outranks `varchar`, so `1 = '1'` converts the
    // string. The binder inserts the `Convert` on that side and on no other.
    let common = implicit_result_type(
        &TypeInfo::new(SqlType::Int, false),
        &TypeInfo::new(SqlType::VarChar(Len::Fixed(1)), false),
    )
    .expect("int and varchar have a common type");
    assert_eq!(common.ty, SqlType::Int);

    // The common type of the branches of a `CASE`, with the same literal rule.
    let branches = implicit_result_type(&numeric(1, 0), &numeric(2, 1)).expect("1 and 2.5");
    assert_eq!(
        branches.ty,
        SqlType::Numeric {
            precision: 2,
            scale: 1
        }
    );

    // A pair with no common type at all is error 206, operands in the order they were
    // written.
    let clash = implicit_result_type(
        &TypeInfo::new(SqlType::UniqueIdentifier, false),
        &TypeInfo::new(SqlType::Int, false),
    )
    .expect_err("a guid and a number have no common type");
    assert_eq!(clash.number, 206);
    assert_eq!(
        clash.message,
        "Type mismatch: uniqueidentifier cannot be combined with int."
    );

    assert!(Collation::parse("Latin1_General_CS_AS").is_ok());
    assert_eq!(
        Collation::parse("Klingon_CI_AS")
            .expect_err("no such collation")
            .number,
        448
    );
}

/// The two answers `bind_arith` reads apart.
///
/// `binary_op_type` gives the type of the **result** and `implicit_result_type` the
/// **common type** of the pair. Neither is *systematically* the target of a conversion of
/// the operands: an operand that carries a precision of its own goes to a third type again,
/// and converting it to the common type would answer 8115 where SQL Server answers a
/// value. The operands that carry none — a `bit`, a character, a binary — do go to the
/// common type, and so do the operands of a pair whose common type is not an exact
/// numeric.
#[test]
fn an_operand_that_carries_a_precision_is_not_brought_to_the_common_type() {
    let int = TypeInfo::new(SqlType::Int, false);
    let tiny_scale = TypeInfo::new(
        SqlType::Decimal {
            precision: 38,
            scale: 38,
        },
        false,
    );

    // `CAST(12345 AS int) + CAST(0.1 AS decimal(38,38))`: the common type holds no value of
    // 1 or more, while SQL Server answers `12345.1000000000000000000000000000`.
    let common = implicit_result_type(&int, &tiny_scale).expect("int and decimal(38,38)");
    assert_eq!(
        common.ty,
        SqlType::Decimal {
            precision: 38,
            scale: 38
        }
    );
    // The type of the result is a third one again.
    let sum = binary_op_type(BinaryOp::Add, &int, &tiny_scale).expect("int + decimal(38,38)");
    assert_eq!(
        sum.ty,
        SqlType::Decimal {
            precision: 38,
            scale: 28
        }
    );

    // What the binder converts an operand that carries a precision to instead: the type it
    // enters the precision table with. It reads it off `types` rather than holding a copy
    // of the table — `numeric(1, 0)` is the neutral element of the widening of two exact
    // numerics, so merging it with a type yields that type seen as an exact numeric. Each
    // line is what SQL Server answers: the precision of `CAST(1.5 AS decimal(2,1)) +
    // CAST(1 AS t)` is 5, 7, 12, 21, 11 and 20 down the list below.
    let one_digit = numeric(1, 0);
    for (ty, precision, scale) in [
        (SqlType::TinyInt, 3, 0),
        (SqlType::SmallInt, 5, 0),
        (SqlType::Int, 10, 0),
        (SqlType::BigInt, 19, 0),
        (SqlType::SmallMoney, 10, 4),
        (SqlType::Money, 19, 4),
    ] {
        let view = implicit_result_type(&TypeInfo::new(ty, false), &one_digit)
            .unwrap_or_else(|e| panic!("{ty:?} merges with numeric(1, 0): {e:?}"));
        assert_eq!(view.ty, SqlType::Numeric { precision, scale }, "{ty:?}");
    }
    // `bit` is absent on purpose, although `implicit_result_type` would answer
    // `numeric(1, 0)`: SQL Server converts it to the common type of the pair, so
    // `bind_arith` does not ask `types` for its reading.
    // `CAST(1.5 AS decimal(2,1)) * CAST(1 AS bit)` is a `decimal(5, 2)`, the precision
    // and scale of `decimal(2,1) * decimal(2,1)`, where the same product with a `tinyint`
    // is a `decimal(6, 1)`; and `CAST(1 AS bit) + CAST(0.1 AS decimal(38,38))` answers
    // 8115. A rule that reads well on paper but differs from what SQL Server does is
    // exactly what this file exists to catch.
    // Neutral on the exact numerics themselves, which is what makes the reading above
    // legitimate: an operand that already carries a precision keeps it.
    for (precision, scale) in [(2, 1), (38, 0), (38, 38), (5, 2)] {
        let kept = implicit_result_type(&numeric(precision, scale), &one_digit)
            .expect("two exact numerics always have a common type");
        assert_eq!(kept.ty, SqlType::Numeric { precision, scale });
    }
}

/// The messages `bind_expr` and `bind_condition` raise. The binder builds none of them:
/// it calls the named constructor of `vauban_errors` and adds the line.
#[test]
fn the_errors_of_the_binder_read_as_sql_server_writes_them() {
    // `SELECT c;`
    let e = SqlError::invalid_column_name("c");
    assert_eq!((e.number, e.severity), (207, 16));
    assert_eq!(e.message, "Unknown column name 'c'.");

    // `SELECT @x;`
    let e = SqlError::must_declare_scalar_variable("@x");
    assert_eq!((e.number, e.severity), (137, 15));
    assert_eq!(e.message, "The scalar variable \"@x\" is not declared.");

    // `SELECT 1 WHERE 1` — SQL Server quotes the token that follows the expression, here
    // the last one of the batch.
    let e = SqlError::non_boolean_expression("1");
    assert_eq!((e.number, e.severity), (4145, 15));
    assert_eq!(
        e.message,
        "A condition is expected near '1', but the expression is not boolean."
    );

    // `SELECT CAST(1 AS bit) * CAST(1 AS bit);`, `SELECT -CAST(1 AS bit);` and
    // `SELECT ~CAST(1.5 AS float);`: the operator is a word for the named ones and a
    // quoted symbol for the others, and the word for the unary `-` is `minus`.
    for (ty, operator, expected) in [
        (
            SqlType::Bit,
            "multiply",
            "Data type bit is not accepted by the multiply operator.",
        ),
        (
            SqlType::Bit,
            "minus",
            "Data type bit is not accepted by the minus operator.",
        ),
        (
            SqlType::Float,
            "'~'",
            "Data type float is not accepted by the '~' operator.",
        ),
    ] {
        let e = SqlError::invalid_operand_type(ty.error_name(), operator);
        assert_eq!((e.number, e.severity), (8117, 16));
        assert_eq!(e.message, expected);
    }

    // `SELECT CAST(1 AS int) COLLATE Latin1_General_CI_AS;`
    let e = SqlError::collate_on_non_string(SqlType::Int.error_name());
    assert_eq!((e.number, e.severity), (447, 16));
    assert_eq!(
        e.message,
        "COLLATE cannot apply to an expression of type int."
    );
}

/// The variants `expr.rs` produces on the predicate side of the fence, read through the
/// public [`BoundExpr::is_predicate`]: `bind_condition` accepts exactly those, and
/// `query.rs` filters a `WHERE` with the same test.
#[test]
fn the_predicates_bnd_003_produces_are_recognised_as_such() {
    let value = || {
        Box::new(BoundExpr {
            kind: BoundExprKind::Literal(Value::I32(1)),
            ty: TypeInfo::new(SqlType::Int, false),
            line: 1,
        })
    };
    let bit = TypeInfo::new(SqlType::Bit, false);

    let predicates = [
        BoundExprKind::Compare {
            op: CompareOp::Ge,
            left: value(),
            right: value(),
        },
        BoundExprKind::IsNull {
            expr: value(),
            negated: true,
        },
        BoundExprKind::In {
            expr: value(),
            list: vec![*value()],
            negated: false,
        },
        BoundExprKind::Like {
            expr: value(),
            pattern: value(),
            escape: None,
            negated: false,
        },
    ];
    for kind in predicates {
        let bound = BoundExpr {
            kind,
            ty: bit.clone(),
            line: 1,
        };
        assert!(bound.is_predicate(), "{bound:?}");
    }

    // A conversion the binder inserted around a predicate would not be one — which is why
    // it inserts none there.
    let converted = BoundExpr {
        kind: BoundExprKind::Convert {
            expr: value(),
            style: None,
            try_: false,
        },
        ty: bit,
        line: 1,
    };
    assert!(!converted.is_predicate());
}

#[test]
fn escape_conversion_has_its_own_length() {
    use vauban_binder::{BindContext, BoundStatement, LogicalPlan, SessionOptions, bind};
    use vauban_parser::{ParseOptions, parse_batch};
    vauban_sysfn::register_builtins();
    for prefix in ["", "N"] {
        for source in [
            "12",
            "CAST(1 AS bit)",
            "CAST('2020-01-01' AS date)",
            "CAST('00112233-4455-6677-8899-AABBCCDDEEFF' AS uniqueidentifier)",
            "CAST(12 AS decimal(4,1))",
            "CAST(12 AS float)",
            "CAST(NULL AS int)",
            "0x2122",
            "'12'",
        ] {
            let sql = format!("SELECT 1 WHERE {prefix}'a' LIKE {prefix}'a' ESCAPE {source};");
            let batch = parse_batch(&sql, &ParseOptions::default()).expect("parse");
            let context = BindContext::scalar(&sql, SessionOptions::default());
            // The pattern is not irrefutable: `BoundStatement` has other variants than
            // `Query`.
            let BoundStatement::Query(plan) = bind(&batch.statements[0], &context).expect("bind")
            else {
                panic!("query")
            };
            let LogicalPlan::Project { input, .. } = *plan else {
                panic!("project")
            };
            let LogicalPlan::Filter { predicate, .. } = *input else {
                panic!("filter")
            };
            let BoundExprKind::Like {
                escape: Some(escape),
                ..
            } = predicate.kind
            else {
                panic!("like")
            };
            let expected = if source == "'12'" {
                SqlType::VarChar(Len::Fixed(2))
            } else {
                let len = if source == "0x2122" {
                    Len::Max
                } else {
                    Len::Fixed(1)
                };
                if prefix.is_empty() {
                    SqlType::VarChar(len)
                } else {
                    SqlType::NVarChar(len)
                }
            };
            assert_eq!(escape.ty.ty, expected, "{sql}");
        }
    }
}
