//! The value of an operator, of a comparison and of the three-valued logic.
//!
//! # Why the trees below are built by hand
//!
//! The bound trees are built here the way `tests/exec_shape.rs` builds its literal, but
//! with the **real rules of `types`** for the pieces the binder would have used:
//! [`parse_literal`] gives a literal its value and its type, [`binary_op_type`] gives an
//! operator node its type. Each test states the SQL it stands for. Binding the text
//! instead would leave each assertion below unchanged.

use vauban_binder::{BoundExpr, BoundExprKind, CompareOp, LogicalOp, SessionOptions};
use vauban_errors::{SqlError, SqlResult};
use vauban_executor::{ExecContext, eval_expr};
use vauban_sysfn::StaticContext;
use vauban_types::{
    BinaryOp, Decimal, LiteralKind, SqlString, SqlType, TypeInfo, Value, binary_op_type,
    parse_literal,
};

// ---------------------------------------------------------------------------------------
// Evaluating
// ---------------------------------------------------------------------------------------

/// Evaluates `expr` under `options`, with the session context of a test.
fn run(expr: &BoundExpr, options: SessionOptions) -> SqlResult<Value> {
    let context = StaticContext::default();
    let mut ctx = ExecContext::scalar(&context, options);
    eval_expr(expr, None, &mut ctx)
}

/// The value of `expr` under the default options.
fn v(expr: &BoundExpr) -> Value {
    run(expr, SessionOptions::default()).expect("the expression evaluates")
}

/// The error `expr` raises under the default options.
fn err(expr: &BoundExpr) -> SqlError {
    run(expr, SessionOptions::default()).expect_err("the expression raises")
}

// ---------------------------------------------------------------------------------------
// Building the bound tree a statement would have produced
// ---------------------------------------------------------------------------------------

/// A literal, with the value and the type `types` gives it (`parse_literal`).
fn lit(kind: LiteralKind, text: &str) -> BoundExpr {
    let (value, ty) = parse_literal(kind, text).expect("the literal is well formed");
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 1,
    }
}

/// An integer literal: `1`, `0`, `2147483647`.
fn int(text: &str) -> BoundExpr {
    lit(LiteralKind::Integer, text)
}

/// A fixed-point literal: `1.5`, `2.0`.
fn dec(text: &str) -> BoundExpr {
    lit(LiteralKind::Decimal, text)
}

/// A character literal: `'a'`.
fn str_lit(text: &str) -> BoundExpr {
    lit(LiteralKind::Str, text)
}

/// The untyped `NULL`, which the binder binds to `Value::Null` typed `int`, nullable.
fn null() -> BoundExpr {
    typed_null(TypeInfo::new(SqlType::Int, true))
}

/// A `NULL` of a given type, what `CAST(NULL AS …)` binds to.
fn typed_null(ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::Null),
        ty,
        line: 1,
    }
}

/// A literal of an arbitrary value and type, for the operands a literal cannot spell
/// (`CAST(1 AS tinyint)`).
fn value_of(value: Value, ty: SqlType) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty: TypeInfo::new(ty, false),
        line: 1,
    }
}

/// `expr` wrapped in the implicit conversion `bind_arith` inserts on it: a
/// `Convert { style: None, try_: false }` whose `ty` is the target.
fn convert(expr: BoundExpr, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(expr),
            style: None,
            try_: false,
        },
        ty,
        line: 1,
    }
}

/// A `numeric(p, s)`, not nullable.
fn numeric(precision: u8, scale: u8) -> TypeInfo {
    TypeInfo::new(SqlType::Numeric { precision, scale }, false)
}

/// `left op right`, typed by `binary_op_type` the way `bind_arith` types it.
fn arith(op: BinaryOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    let ty = binary_op_type(op, &left.ty, &right.ty).expect("the operands accept the operator");
    BoundExpr {
        kind: BoundExprKind::Arith {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty,
        line: 1,
    }
}

/// `- expr`, typed as `bind_unary` types it: `tinyint` widens to `smallint`, since
/// `tinyint` is unsigned.
fn negate(expr: BoundExpr) -> BoundExpr {
    let widened = if expr.ty.ty == SqlType::TinyInt {
        SqlType::SmallInt
    } else {
        expr.ty.ty
    };
    let ty = TypeInfo::new(widened, expr.ty.nullable);
    BoundExpr {
        kind: BoundExprKind::Negate(Box::new(expr)),
        ty,
        line: 1,
    }
}

/// `~ expr`, which keeps the type of its operand.
fn bit_not(expr: BoundExpr) -> BoundExpr {
    let ty = expr.ty.clone();
    BoundExpr {
        kind: BoundExprKind::BitNot(Box::new(expr)),
        ty,
        line: 1,
    }
}

/// `left op right`, a comparison: a `bit`, nullable as soon as an operand is.
fn cmp(op: CompareOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    let nullable = left.ty.nullable || right.ty.nullable;
    BoundExpr {
        kind: BoundExprKind::Compare {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line: 1,
    }
}

/// `left AND right` or `left OR right`.
fn logical(op: LogicalOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Logical {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    }
}

/// `NOT expr`.
fn not(expr: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Not(Box::new(expr)),
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    }
}

/// `expr IS NULL`, or `expr IS NOT NULL` when `negated`: a `bit` that is never `NULL`.
fn is_null(expr: BoundExpr, negated: bool) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::IsNull {
            expr: Box::new(expr),
            negated,
        },
        ty: TypeInfo::new(SqlType::Bit, false),
        line: 1,
    }
}

/// `1 = 1`: the true of the truth tables.
fn yes() -> BoundExpr {
    cmp(CompareOp::Eq, int("1"), int("1"))
}

/// `1 = 0`: the false of the truth tables.
fn no() -> BoundExpr {
    cmp(CompareOp::Eq, int("1"), int("0"))
}

/// `NULL = NULL`: the unknown of the truth tables.
fn unknown() -> BoundExpr {
    cmp(CompareOp::Eq, null(), null())
}

// ---------------------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------------------

#[test]
fn arithmetic() {
    // `SELECT 1 + 1;`, `2 * 3;`, `7 / 2;`, `7 % 2;`
    assert_eq!(v(&arith(BinaryOp::Add, int("1"), int("1"))), Value::I32(2));
    assert_eq!(v(&arith(BinaryOp::Mul, int("2"), int("3"))), Value::I32(6));
    // Between two integers the quotient is integral, truncated towards zero.
    assert_eq!(v(&arith(BinaryOp::Div, int("7"), int("2"))), Value::I32(3));
    assert_eq!(v(&arith(BinaryOp::Mod, int("7"), int("2"))), Value::I32(1));

    // `SELECT 1.5 * 2.0;`: two exact numerics, which SQL Server does not convert at all.
    // The value carries the scale `binary_op_type` computed.
    let product = v(&arith(BinaryOp::Mul, dec("1.5"), dec("2.0")));
    assert_eq!(
        product,
        Value::Decimal(Decimal {
            mantissa: 300,
            precision: 5,
            scale: 2,
        })
    );

    // `SELECT 1.5 * 2;`: `eval_binary` needs both operands in one value family, and
    // `bind_arith` inserts the `Convert` of the `int` operand towards `numeric(1, 0)`: an
    // integer *literal* enters the precision table with its own number of digits, so the
    // product is a `numeric(4, 1)` and SQL Server answers `3.0`.
    //
    // The pair is written here as the two values that reach `eval_binary` once the
    // conversion has run, and `decimal_times_int_evaluates_the_whole_bound_tree` below
    // evaluates the real tree.
    let two = value_of(
        Value::Decimal(Decimal {
            mantissa: 2,
            precision: 1,
            scale: 0,
        }),
        SqlType::Numeric {
            precision: 1,
            scale: 0,
        },
    );
    let product = arith(BinaryOp::Mul, dec("1.5"), two);
    assert_eq!(product.ty, numeric(4, 1));
    assert_eq!(
        v(&product),
        Value::Decimal(Decimal {
            mantissa: 30,
            precision: 4,
            scale: 1,
        })
    );
}

/// `SELECT 1.5 * 2;` over the tree `bind_arith` really builds, `Convert` node included.
///
/// The same vector as in `arithmetic`, but evaluated from the top instead of from the two
/// values the conversion yields.
#[test]
fn decimal_times_int_evaluates_the_whole_bound_tree() {
    let product = arith(BinaryOp::Mul, dec("1.5"), convert(int("2"), numeric(1, 0)));
    assert_eq!(product.ty, numeric(4, 1));
    assert_eq!(
        v(&product),
        Value::Decimal(Decimal {
            mantissa: 30,
            precision: 4,
            scale: 1,
        })
    );
}

#[test]
fn null_propagates_through_arithmetic() {
    // `SELECT NULL + 1;`, `1 + NULL;`, `NULL * NULL;`
    assert_eq!(v(&arith(BinaryOp::Add, null(), int("1"))), Value::Null);
    assert_eq!(v(&arith(BinaryOp::Add, int("1"), null())), Value::Null);
    assert_eq!(v(&arith(BinaryOp::Mul, null(), null())), Value::Null);
}

#[test]
fn unary_minus_goes_through_eval_binary() {
    // `SELECT -1;` — the parser writes a unary minus, not a negative literal.
    assert_eq!(v(&negate(int("1"))), Value::I32(-1));
    // `SELECT -NULL;`
    assert_eq!(v(&negate(null())), Value::Null);
    // `SELECT -CAST(1 AS tinyint);`: `tinyint` is unsigned, so the result is a `smallint`.
    assert_eq!(
        v(&negate(value_of(Value::I8(1), SqlType::TinyInt))),
        Value::I16(-1)
    );
}

#[test]
fn division_by_zero_is_8134() {
    // `SELECT 1 / 0;` and `SELECT 1 % 0;`
    for op in [BinaryOp::Div, BinaryOp::Mod] {
        let error = err(&arith(op, int("1"), int("0")));
        assert_eq!(error.number, 8134);
        assert_eq!(error.severity, 16);
        assert_eq!(error.state, 1);
    }
}

// ---------------------------------------------------------------------------------------
// Bitwise operators
// ---------------------------------------------------------------------------------------

#[test]
fn bitwise() {
    // `SELECT 1 | 2;`, `6 & 3;`, `6 ^ 3;`, `~1;`
    assert_eq!(
        v(&arith(BinaryOp::BitOr, int("1"), int("2"))),
        Value::I32(3)
    );
    assert_eq!(
        v(&arith(BinaryOp::BitAnd, int("6"), int("3"))),
        Value::I32(2)
    );
    assert_eq!(
        v(&arith(BinaryOp::BitXor, int("6"), int("3"))),
        Value::I32(5)
    );
    assert_eq!(v(&bit_not(int("1"))), Value::I32(-2));
    // `SELECT ~NULL;`
    assert_eq!(v(&bit_not(null())), Value::Null);
    // `SELECT ~CAST(1 AS tinyint);`: the mask has as many bits as the type, and `tinyint`
    // is unsigned, so the answer is 254 and not -2.
    assert_eq!(
        v(&bit_not(value_of(Value::I8(1), SqlType::TinyInt))),
        Value::I8(254)
    );
}

// ---------------------------------------------------------------------------------------
// Comparisons
// ---------------------------------------------------------------------------------------

#[test]
fn comparisons() {
    // `SELECT CASE WHEN 1 = 1 THEN … END;` and friends: a predicate cannot be projected.
    assert_eq!(v(&yes()), Value::Bit(true));
    assert_eq!(v(&no()), Value::Bit(false));
    assert_eq!(v(&cmp(CompareOp::Gt, int("2"), int("1"))), Value::Bit(true));
    // `'a' = 'A'`: the default collation is case-insensitive (`CI_AS`), and the executor
    // reads it off the operands, the comparison itself being a `bit`.
    assert_eq!(
        v(&cmp(CompareOp::Eq, str_lit("a"), str_lit("A"))),
        Value::Bit(true)
    );
    // The five other operators, so that no arm of the mapping is left unchecked.
    assert_eq!(v(&cmp(CompareOp::Ne, int("1"), int("2"))), Value::Bit(true));
    assert_eq!(v(&cmp(CompareOp::Lt, int("1"), int("2"))), Value::Bit(true));
    assert_eq!(v(&cmp(CompareOp::Le, int("2"), int("2"))), Value::Bit(true));
    assert_eq!(
        v(&cmp(CompareOp::Ge, int("1"), int("2"))),
        Value::Bit(false)
    );
}

#[test]
fn comparison_with_null_is_unknown() {
    // `NULL = NULL`, `1 = NULL`, `NULL <> 1`: unknown, which is `Value::Null` and not
    // `Value::Bit(false)` — this is `ANSI_NULLS ON`, the only behaviour the executor has.
    assert_eq!(v(&unknown()), Value::Null);
    assert_eq!(v(&cmp(CompareOp::Eq, int("1"), null())), Value::Null);
    assert_eq!(v(&cmp(CompareOp::Ne, null(), int("1"))), Value::Null);
}

// ---------------------------------------------------------------------------------------
// Three-valued logic
// ---------------------------------------------------------------------------------------

#[test]
fn and_truth_table() {
    let table = [
        (yes(), yes(), Value::Bit(true)),
        (yes(), no(), Value::Bit(false)),
        (yes(), unknown(), Value::Null),
        (no(), yes(), Value::Bit(false)),
        (no(), no(), Value::Bit(false)),
        // False wins over unknown, in both directions.
        (no(), unknown(), Value::Bit(false)),
        (unknown(), yes(), Value::Null),
        (unknown(), no(), Value::Bit(false)),
        (unknown(), unknown(), Value::Null),
    ];
    for (index, (left, right, expected)) in table.into_iter().enumerate() {
        assert_eq!(
            v(&logical(LogicalOp::And, left, right)),
            expected,
            "row {index}"
        );
    }
}

#[test]
fn or_truth_table() {
    let table = [
        (yes(), yes(), Value::Bit(true)),
        (yes(), no(), Value::Bit(true)),
        // True wins over unknown, in both directions.
        (yes(), unknown(), Value::Bit(true)),
        (no(), yes(), Value::Bit(true)),
        (no(), no(), Value::Bit(false)),
        (no(), unknown(), Value::Null),
        (unknown(), yes(), Value::Bit(true)),
        (unknown(), no(), Value::Null),
        (unknown(), unknown(), Value::Null),
    ];
    for (index, (left, right, expected)) in table.into_iter().enumerate() {
        assert_eq!(
            v(&logical(LogicalOp::Or, left, right)),
            expected,
            "row {index}"
        );
    }
}

#[test]
fn not_truth_table() {
    // `NOT (1 = 1)`, `NOT (1 = 0)`, `NOT (NULL = NULL)`.
    assert_eq!(v(&not(yes())), Value::Bit(false));
    assert_eq!(v(&not(no())), Value::Bit(true));
    assert_eq!(v(&not(unknown())), Value::Null);
}

#[test]
fn is_null_is_never_null() {
    // `NULL IS NULL`, `1 IS NULL`, `NULL IS NOT NULL`: a `bit`, always.
    assert_eq!(v(&is_null(null(), false)), Value::Bit(true));
    assert_eq!(v(&is_null(int("1"), false)), Value::Bit(false));
    assert_eq!(v(&is_null(null(), true)), Value::Bit(false));
    assert_eq!(v(&is_null(int("1"), true)), Value::Bit(true));
}

#[test]
fn and_does_not_short_circuit() {
    // `SELECT … WHERE 1 = 0 AND 1 / 0 = 1;`
    //
    // SQL Server guarantees **no** evaluation order for `AND` and `OR`: the optimiser may
    // evaluate either operand, or both, in any order, and the only short-circuit it
    // promises is that of `CASE`. This test freezes the choice of VaubanDB — evaluate
    // both operands, left to right — so that a divide by zero on the right of a false
    // `AND` raises 8134 instead of being silently skipped. SQL Server does short-circuit
    // in practice (module documentation of `expr.rs`): that is a known gap, not a rule to
    // imitate.
    let divide = cmp(
        CompareOp::Eq,
        arith(BinaryOp::Div, int("1"), int("0")),
        int("1"),
    );
    let error = err(&logical(LogicalOp::And, no(), divide));
    assert_eq!(error.number, 8134);

    // The same on the right of a true `OR`, whose left operand already settles the answer.
    let divide = cmp(
        CompareOp::Eq,
        arith(BinaryOp::Div, int("1"), int("0")),
        int("1"),
    );
    let error = err(&logical(LogicalOp::Or, yes(), divide));
    assert_eq!(error.number, 8134);
}

// ---------------------------------------------------------------------------------------
// Options and transparent nodes
// ---------------------------------------------------------------------------------------

/// `'a' + NULL` and `NULL + NULL` under both settings of `CONCAT_NULL_YIELDS_NULL`.
///
/// The trees are the ones `'a' + CAST(NULL AS varchar(1))` binds to: the option only
/// concerns the concatenation of strings, so both operands are character.
#[test]
fn concat_null_yields_null_switches_the_answer() {
    let text = || TypeInfo::new(SqlType::VarChar(vauban_types::Len::Fixed(1)), true);
    let a_plus_null = || arith(BinaryOp::Concat, str_lit("a"), typed_null(text()));
    let null_plus_null = || arith(BinaryOp::Concat, typed_null(text()), typed_null(text()));

    // `ON`, the setting of every modern driver and the default of `SessionOptions`.
    let on = SessionOptions::default();
    assert!(on.concat_null_yields_null);
    assert_eq!(run(&a_plus_null(), on), Ok(Value::Null));
    assert_eq!(run(&null_plus_null(), on), Ok(Value::Null));

    // `OFF`, the historical setting: a `NULL` operand stands for an empty string.
    let off = SessionOptions {
        concat_null_yields_null: false,
        ..SessionOptions::default()
    };
    assert_eq!(
        run(&a_plus_null(), off),
        Ok(Value::String(SqlString {
            text: "a".to_owned()
        }))
    );
    assert_eq!(
        run(&null_plus_null(), off),
        Ok(Value::String(SqlString {
            text: String::new()
        }))
    );

    // The option never touches numeric arithmetic: `1 + NULL` is `NULL` either way.
    assert_eq!(
        run(&arith(BinaryOp::Add, int("1"), null()), off),
        Ok(Value::Null)
    );
}

#[test]
fn collate_is_transparent_to_evaluation() {
    // `SELECT 'a' COLLATE Latin1_General_CI_AS;`: the collation lives in the type, where
    // `compare` and `LIKE` read it; the value is the one of the operand.
    let inner = str_lit("a");
    let ty = inner.ty.clone();
    let collate = BoundExpr {
        kind: BoundExprKind::Collate {
            expr: Box::new(inner),
        },
        ty,
        line: 1,
    };
    assert_eq!(
        v(&collate),
        Value::String(SqlString {
            text: "a".to_owned()
        })
    );
}

#[test]
fn a_variable_is_an_internal_error() {
    // A local variable is not evaluated yet. It is not a client error: it is a bug if it
    // is ever reached before the variables land.
    let variable = BoundExpr {
        kind: BoundExprKind::Variable {
            name: "@x".to_owned(),
        },
        ty: TypeInfo::new(SqlType::Int, true),
        line: 1,
    };
    assert_eq!(err(&variable).number, 50000);
}
