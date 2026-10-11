//! Unit tests for `binder::expr`: literal and operator typing, implicit
//! conversions, `CASE`, `LIKE`, `COLLATE`, nullability, and the diagnostics
//! `bind_expr` and `bind_condition` raise.

use super::{
    Scope, bind_condition, bind_expr, exact_numeric_view, implicit_conversion_may_be_null,
    integer_digits, near_token_at,
};
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    BinaryOp as AstBinaryOp, CaseArm, ColumnRef, Expr, Ident, InList, Literal, ObjectName, Span,
    UnaryOp,
};
use vauban_types::{Collation, Len, SqlType, TypeInfo, Value};

use crate::bound::{BoundExpr, BoundExprKind, CompareOp, LogicalOp};
use crate::call::tests::registry;
use crate::context::{BindContext, SessionOptions, VariableScope};

mod collation;

#[test]
fn invalid_niladic_binding_preserves_variable_order() {
    registry();
    for name in [
        "CURRENT_TIMESTAMP()",
        "CURRENT_USER()",
        "SESSION_USER()",
        "SYSTEM_USER()",
        "USER()",
        "CURRENT_DATE",
        "CURRENT_TIME",
        "[CURRENT_TIMESTAMP]()",
        "[CURRENT_DATE](1)",
    ] {
        let number = if matches!(name, "CURRENT_DATE" | "CURRENT_TIME") {
            156
        } else {
            102
        };
        for (variable, before, expected) in [
            ("@missing", true, 137),
            ("@missing", false, number),
            ("@int", true, number),
            ("@int", false, number),
        ] {
            let sql = if before {
                format!("SELECT {variable}, {name};")
            } else {
                format!("SELECT {name}, {variable};")
            };
            let batch =
                vauban_parser::parse_batch(&sql, &vauban_parser::ParseOptions::default()).unwrap();
            let mut ctx = BindContext::scalar(&sql, SessionOptions::default());
            ctx.variables = &Vars;
            let error = crate::bind(&batch.statements[0], &ctx).unwrap_err();
            assert_eq!(error.number, expected, "{sql}");
            assert_eq!(
                (error.severity, error.state),
                (15, if expected == 137 { 2 } else { 1 }),
                "{sql}"
            );
        }
    }
}

/// The error `sql` answers with, bound through the whole statement so that the order
/// the binder walks the tree in is the one under test.
fn statement_error(sql: &str) -> SqlError {
    registry();
    let batch = vauban_parser::parse_batch(sql, &vauban_parser::ParseOptions::default()).unwrap();
    let mut ctx = BindContext::scalar(sql, SessionOptions::default());
    ctx.variables = &Vars;
    crate::bind(&batch.statements[0], &ctx).unwrap_err()
}

/// A condition that opens on a parenthesised niladic spelling answers 4145 quoting the
/// call parenthesis, and the shapes that stop that walk keep their own number. Each of
/// the nine batches below compares with `=`.
#[test]
fn a_niladic_call_that_opens_a_condition_is_4145() {
    let quoting_the_parenthesis = SqlError::non_boolean_expression("(").message;
    for sql in [
        "SELECT 1 WHERE CURRENT_TIMESTAMP()=1;",
        "SELECT 1 WHERE CURRENT_TIMESTAMP(1)=1;",
        "SELECT 1 WHERE [CURRENT_TIMESTAMP]()=1;",
        "SELECT 1 WHERE [CURRENT_DATE]()=1;",
        "SELECT CASE WHEN CURRENT_TIMESTAMP()=1 THEN 1 ELSE 0 END;",
    ] {
        let error = statement_error(sql);
        assert_eq!(
            (error.number, error.severity, error.state, error.line),
            (4145, 15, 1, 1),
            "{sql}"
        );
        assert_eq!(error.message, quoting_the_parenthesis, "{sql}");
    }

    // The discriminating side: the spelling on the right of the comparison, the same
    // spelling inside parentheses, and the two names written without a call, which
    // SQL Server answers differently from 4145.
    for (sql, number, near) in [
        ("SELECT 1 WHERE 1=CURRENT_TIMESTAMP();", 102, "')'"),
        ("SELECT 1 WHERE 1=CURRENT_TIMESTAMP(1);", 102, "'1'"),
        ("SELECT 1 WHERE (CURRENT_TIMESTAMP())=1;", 102, "'('"),
        ("SELECT 1 WHERE CURRENT_DATE=1;", 156, "'CURRENT_DATE'"),
    ] {
        let error = statement_error(sql);
        assert_eq!((error.number, error.line), (number, 1), "{sql}");
        assert!(error.message.contains(near), "{sql}: {}", error.message);
    }
}

/// An unresolved call around an invalid niladic spelling: the answer is about the
/// argument, and about an undeclared variable written before it, not about the name of
/// the enclosing function, which does not exist either.
#[test]
fn an_argument_is_diagnosed_before_the_enclosing_call() {
    for (sql, number, named) in [
        ("SELECT unknown(CURRENT_TIMESTAMP());", 102, "'('"),
        ("SELECT unknown(CURRENT_DATE);", 156, "'CURRENT_DATE'"),
        (
            "SELECT unknown(@missing,CURRENT_TIMESTAMP());",
            137,
            "\"@missing\"",
        ),
        (
            "SELECT ISNULL(@missing,CURRENT_TIMESTAMP());",
            137,
            "\"@missing\"",
        ),
    ] {
        let error = statement_error(sql);
        assert_eq!((error.number, error.line), (number, 1), "{sql}");
        assert!(error.message.contains(named), "{sql}: {}", error.message);
    }

    // The preflight leaves the ordinary path alone: without such a spelling among its
    // arguments, an unresolved call still raises the 195 of the registry. A control of
    // the four batches above.
    let error = statement_error("SELECT unknown(1);");
    assert_eq!(error.number, 195, "{}", error.message);
}

/// The tests build the AST node by hand: what is under test is the binding of a
/// shape, and the shape is the parser's contract, written down in `parser::ast::expr`.
///
/// A span on line 1, offset 0: the line alone survives into a `BoundExpr`.
fn any_span() -> Span {
    Span {
        line: 1,
        column: 1,
        offset: 0,
        len: 0,
    }
}

/// An integer literal, as the lexer would classify it.
fn int(text: &str) -> Expr {
    Expr::Literal(Literal::Integer(text.to_owned()), any_span())
}

/// A fixed-point literal (`1.5` is a `numeric(2, 1)`).
fn dec(text: &str) -> Expr {
    Expr::Literal(Literal::Decimal(text.to_owned()), any_span())
}

/// A non-Unicode string literal (`'a'` is a `varchar(1)`).
fn text(value: &str) -> Expr {
    Expr::Literal(
        Literal::Str {
            value: value.to_owned(),
            unicode: false,
        },
        any_span(),
    )
}

/// A binary literal (`0x00` is a non-nullable `varbinary(1)`).
fn hex(text: &str) -> Expr {
    Expr::Literal(Literal::Binary(text.to_owned()), any_span())
}

/// The untyped `NULL`, which `literal.rs` types `int`, nullable.
fn null() -> Expr {
    Expr::Literal(Literal::Null, any_span())
}

/// A variable reference, resolved by [`Vars`].
fn var(name: &str) -> Expr {
    Expr::Variable {
        name: name.to_owned(),
        span: any_span(),
    }
}

fn binary(op: AstBinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        op_span: any_span(),
        left: Box::new(left),
        right: Box::new(right),
        span: any_span(),
    }
}

fn unary(op: UnaryOp, expr: Expr) -> Expr {
    Expr::Unary {
        op,
        expr: Box::new(expr),
        span: any_span(),
    }
}

/// A one-part column reference on `line`.
fn column(name: &str, line: u32) -> Expr {
    Expr::Column(ColumnRef {
        qualifier: None,
        name: Ident {
            value: name.to_owned(),
            quoted: false,
        },
        span: Span {
            line,
            column: 8,
            offset: 7,
            len: name.len() as u32,
        },
    })
}

/// The variables the tests declare, so that an operand can have a type no literal has
/// (`bit`, `uniqueidentifier`, `datetime`, `tinyint`).
struct Vars;

impl VariableScope for Vars {
    fn type_of(&self, name: &str) -> Option<TypeInfo> {
        let ty = match name {
            "@bit" => SqlType::Bit,
            "@guid" => SqlType::UniqueIdentifier,
            "@datetime" => SqlType::DateTime,
            "@tiny" => SqlType::TinyInt,
            "@int" => SqlType::Int,
            "@float" => SqlType::Float,
            "@varchar" => SqlType::VarChar(Len::Fixed(10)),
            "@big" => SqlType::BigInt,
            "@small" => SqlType::SmallInt,
            "@real" => SqlType::Real,
            "@money" => SqlType::Money,
            "@smallmoney" => SqlType::SmallMoney,
            "@dec" => SqlType::Decimal {
                precision: 10,
                scale: 2,
            },
            // The type whose common type with an `int` holds no `int` at all: it is
            // what `arith_target` is stated on.
            "@tiny_scale" => SqlType::Decimal {
                precision: 38,
                scale: 38,
            },
            // The nullable ones, as a `DECLARE`d variable is on the server: the
            // nullability tests are written over them.
            "@nint" => return Some(TypeInfo::new(SqlType::Int, true)),
            "@ndec" => {
                return Some(TypeInfo::new(
                    SqlType::Decimal {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ));
            }
            "@nvarchar" => {
                return Some(TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true));
            }
            "@nbin" => {
                return Some(TypeInfo::new(SqlType::VarBinary(Len::Fixed(10)), true));
            }
            _ => return None,
        };
        Some(TypeInfo::new(ty, false))
    }
}

static VARS: Vars = Vars;

/// A binding context over `text`, with the variables above in scope.
fn ctx(text: &str) -> BindContext<'_> {
    BindContext {
        text,
        catalog: None,
        database: "master",
        default_schema: "dbo",
        variables: &VARS,
        options: SessionOptions::default(),
    }
}

/// The bound form of an expression, or the error it raised.
///
/// The registry is filled first: a function call, a `@@x` variable and a bare niladic
/// name go through `sysfn::lookup`, and a test must not depend on whether another test
/// filled it.
fn bind(e: &Expr) -> SqlResult<BoundExpr> {
    registry();
    bind_expr(e, &ctx("SELECT 1"), &Scope::empty())
}

/// The bound form of an expression the test expects to bind.
fn b(e: &Expr) -> BoundExpr {
    bind(e).expect("binds")
}

/// The error an expression the test expects to fail raised.
fn err(e: &Expr) -> SqlError {
    bind(e).expect_err("fails to bind")
}

/// The `ty` of the left and right operands of a binary bound node.
fn operands(e: &BoundExpr) -> (&BoundExpr, &BoundExpr) {
    match &e.kind {
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => (left, right),
        other => panic!("expected a binary node, got {other:?}"),
    }
}

/// Whether a node is a binder-inserted conversion towards `ty`.
fn is_convert_to(e: &BoundExpr, ty: SqlType) -> bool {
    matches!(
        &e.kind,
        BoundExprKind::Convert {
            style: None,
            try_: false,
            ..
        }
    ) && e.ty.ty == ty
}

#[test]
fn arithmetic_types() {
    assert_eq!(
        b(&binary(AstBinaryOp::Add, int("1"), int("1"))).ty.ty,
        SqlType::Int
    );
    // `1 + 1.5` is a numeric(3, 1) and **not** a numeric(12, 1): the integer literal
    // enters the precision table with its own digit count (precision 3, scale 1,
    // against the 12 of `CAST(1 AS int) + CAST(1.5 AS numeric(2,1))`).
    assert_eq!(
        b(&binary(AstBinaryOp::Add, int("1"), dec("1.5"))).ty.ty,
        SqlType::Numeric {
            precision: 3,
            scale: 1
        }
    );
    // Two integers divide as integers.
    assert_eq!(
        b(&binary(AstBinaryOp::Div, int("1"), int("2"))).ty.ty,
        SqlType::Int
    );
    assert!(
        b(&binary(AstBinaryOp::Div, dec("1.0"), int("2")))
            .ty
            .ty
            .is_exact_numeric()
    );
    // An `int` typed by a variable keeps the precision of its type: numeric(12, 1).
    assert_eq!(
        b(&binary(AstBinaryOp::Add, var("@int"), dec("1.5"))).ty.ty,
        SqlType::Numeric {
            precision: 12,
            scale: 1
        }
    );
}

/// The `Convert` nodes `bind_arith` inserts on its operands, and the ones it does not,
/// each fixed by what SQL Server answers to the query named next to it.
#[test]
fn arith_inserts_the_conversion() {
    // `SELECT 1.5 * 2;` answers `3.0`: the
    // integer is converted, the `numeric(2, 1)` is not, and the product is a
    // `numeric(4, 1)` — a type neither operand has.
    let mul = b(&binary(AstBinaryOp::Mul, dec("1.5"), int("2")));
    let (left, right) = operands(&mul);
    assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
    assert!(
        is_convert_to(
            right,
            SqlType::Numeric {
                precision: 1,
                scale: 0
            }
        ),
        "{right:?}"
    );
    assert_eq!(
        mul.ty.ty,
        SqlType::Numeric {
            precision: 4,
            scale: 1
        }
    );

    // `SELECT 1.5 * 2.5;`: two exact numerics convert nothing at all, whatever their
    // precision and scale. `SELECT CAST(1.5 AS decimal(5,2)) * CAST(1.5 AS
    // decimal(5,2));` is a `decimal(11,4)` without
    // either operand moving.
    let mul = b(&binary(AstBinaryOp::Mul, dec("1.5"), dec("2.5")));
    let (left, right) = operands(&mul);
    assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
    assert!(matches!(right.kind, BoundExprKind::Literal(_)), "{right:?}");

    // An `int` that is not a literal enters with the precision of its own type, and
    // that is the type it is converted to: `numeric(10, 0)`, neither the common type
    // of the pair (`numeric(11, 1)`) nor the type of the result (`numeric(12, 1)`).
    let add = b(&binary(AstBinaryOp::Add, var("@int"), dec("1.5")));
    let (left, right) = operands(&add);
    assert!(
        is_convert_to(
            left,
            SqlType::Numeric {
                precision: 10,
                scale: 0
            }
        ),
        "{left:?}"
    );
    assert!(matches!(right.kind, BoundExprKind::Literal(_)), "{right:?}");

    // What rules out the common type: `SELECT CAST(12345 AS int) + CAST(0.1
    // AS decimal(38,38));` answers `12345.1000000000000000000000000000`, so the `int`
    // is not converted to the
    // `decimal(38, 38)` the pair has in common — which holds no `12345`.
    let add = b(&binary(AstBinaryOp::Add, var("@int"), var("@tiny_scale")));
    let (left, _) = operands(&add);
    assert!(
        is_convert_to(
            left,
            SqlType::Numeric {
                precision: 10,
                scale: 0
            }
        ),
        "{left:?}"
    );

    // Two integers convert nothing: `eval_binary` widens both to `i64` on its own, and
    // `SELECT CAST(200 AS tinyint) * CAST(200 AS tinyint);` overflows a `tinyint`
    // while `CAST(200 AS tinyint) * CAST(200 AS int)` answers `40000` — the result
    // type alone makes the difference.
    for pair in [("@tiny", "@tiny"), ("@tiny", "@int"), ("@int", "@big")] {
        let mul = b(&binary(AstBinaryOp::Mul, var(pair.0), var(pair.1)));
        let (left, right) = operands(&mul);
        assert!(
            matches!(left.kind, BoundExprKind::Variable { .. }),
            "{left:?}"
        );
        assert!(
            matches!(right.kind, BoundExprKind::Variable { .. }),
            "{right:?}"
        );
    }

    // `money` against an `int` stays in the money family and the `int` joins it. What
    // proves the *operand* is what moves, and towards the narrow type:
    // `SELECT CAST(0 AS smallmoney) * CAST(3000000 AS int);` answers an arithmetic
    // overflow for `smallmoney` on the value 3000000 although the product is 0 and
    // although a `money` holds 3000000 — so neither the result nor a promotion of the
    // pair explains the failure, the conversion of the operand alone does.
    let add = b(&binary(AstBinaryOp::Add, var("@smallmoney"), var("@int")));
    let (left, right) = operands(&add);
    assert!(
        matches!(left.kind, BoundExprKind::Variable { .. }),
        "{left:?}"
    );
    assert!(is_convert_to(right, SqlType::SmallMoney), "{right:?}");

    // A `money` against a `decimal` leaves the money family instead, as
    // `numeric(19, 4)`: `SELECT CAST(1.5 AS money) + CAST(2.25 AS decimal(5,2));` is a
    // `decimal(20,4)`, which is that reading of `money`
    // put through the precision table.
    let add = b(&binary(AstBinaryOp::Add, var("@money"), dec("2.25")));
    let (left, right) = operands(&add);
    assert!(
        is_convert_to(
            left,
            SqlType::Numeric {
                precision: 19,
                scale: 4
            }
        ),
        "{left:?}"
    );
    assert!(matches!(right.kind, BoundExprKind::Literal(_)), "{right:?}");

    // A `bit` carries no precision of its own either, and takes the common type like
    // a character operand. `SELECT CAST(1.5 AS decimal(2,1)) * CAST(1 AS bit);` is a
    // `decimal(5, 2)`, which is `decimal(2,1) * decimal(2,1)`; a `numeric(1, 0)`
    // reading would have given a `decimal(4, 1)`.
    let mul = b(&binary(AstBinaryOp::Mul, dec("1.5"), var("@bit")));
    let (left, right) = operands(&mul);
    assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
    assert!(
        is_convert_to(
            right,
            SqlType::Numeric {
                precision: 2,
                scale: 1
            }
        ),
        "{right:?}"
    );

    // The witness that makes that answer readable: the same product with a `tinyint`,
    // which does carry a precision, is a `decimal(6, 1)`. Two queries of the same
    // shape, two different answers.
    let mul = b(&binary(AstBinaryOp::Mul, dec("1.5"), var("@tiny")));
    let (_, right) = operands(&mul);
    assert!(
        is_convert_to(
            right,
            SqlType::Numeric {
                precision: 3,
                scale: 0
            }
        ),
        "{right:?}"
    );

    // And what settles it: the common type of a `bit` and a `decimal(38, 38)` holds
    // no 1, so converting the operand to it overflows — which is what SQL Server
    // answers, an arithmetic overflow converting `tinyint` to `numeric`. Under the
    // `numeric(1, 0)` reading the sum would have been a value.
    let add = b(&binary(AstBinaryOp::Add, var("@bit"), var("@tiny_scale")));
    let (left, _) = operands(&add);
    assert!(
        is_convert_to(
            left,
            SqlType::Decimal {
                precision: 38,
                scale: 38
            }
        ),
        "{left:?}"
    );

    // A character operand has no precision of its own and takes the common type:
    // `SELECT '1' + 2;` answers the `int` 3.
    let add = b(&binary(AstBinaryOp::Add, text("1"), var("@int")));
    let (left, right) = operands(&add);
    assert!(is_convert_to(left, SqlType::Int), "{left:?}");
    assert!(
        matches!(right.kind, BoundExprKind::Variable { .. }),
        "{right:?}"
    );

    // Two approximate operands convert nothing (`real * float` is a `float`), and an
    // exact operand facing one becomes a `float`.
    let mul = b(&binary(AstBinaryOp::Mul, var("@real"), var("@float")));
    let (left, right) = operands(&mul);
    assert!(
        matches!(left.kind, BoundExprKind::Variable { .. }),
        "{left:?}"
    );
    assert!(
        matches!(right.kind, BoundExprKind::Variable { .. }),
        "{right:?}"
    );
    let mul = b(&binary(AstBinaryOp::Mul, var("@float"), int("2")));
    let (_, right) = operands(&mul);
    assert!(is_convert_to(right, SqlType::Float), "{right:?}");

    // `datetime + 1` is a `datetime` and the number joins it (`Jan  2 2000 12:00AM`).
    // Same proof as for the money family that the operand is converted:
    // `SELECT CAST('9999-12-31' AS datetime) + CAST(-100000 AS int);` answers an
    // arithmetic overflow converting the expression to `datetime` although the sum
    // falls in 9726, a date `datetime` holds — it is `-100000`, read as a date of
    // 1626, that leaves the type.
    let add = b(&binary(AstBinaryOp::Add, var("@datetime"), int("1")));
    let (left, right) = operands(&add);
    assert!(
        matches!(left.kind, BoundExprKind::Variable { .. }),
        "{left:?}"
    );
    assert!(is_convert_to(right, SqlType::DateTime), "{right:?}");

    // The bitwise operators follow the same rule: `CAST(1 AS bit) & CAST(200 AS int)`
    // is an `int`, so the `bit` is the operand that moves.
    let and = b(&binary(AstBinaryOp::BitAnd, var("@bit"), var("@int")));
    let (left, right) = operands(&and);
    assert!(is_convert_to(left, SqlType::Int), "{left:?}");
    assert!(
        matches!(right.kind, BoundExprKind::Variable { .. }),
        "{right:?}"
    );

    // A concatenation converts nothing: the two strings reach `eval_binary` as they
    // are (`SELECT 'a' + 'b';`).
    let concat = b(&binary(AstBinaryOp::Add, text("a"), text("b")));
    let (left, right) = operands(&concat);
    assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
    assert!(matches!(right.kind, BoundExprKind::Literal(_)), "{right:?}");
}

/// The precondition of [`vauban_types::eval_binary`], checked over the shapes above:
/// "a pair of operands from two different families is a broken precondition".
///
/// The check is on the **family** and not on the type, because that is what
/// `eval_binary` dispatches on and because SQL Server does not equalise the types: the
/// operands of `CAST(12345 AS int) + CAST(0.1 AS decimal(38,38))` end up
/// `numeric(10, 0)` and `decimal(38, 38)`, and equalising them would answer 8115 where
/// SQL Server answers a value (see [`arith_target`]).
#[test]
fn every_arithmetic_pair_reaches_eval_binary_in_one_family() {
    let numbers = [
        "@bit",
        "@tiny",
        "@small",
        "@int",
        "@big",
        "@smallmoney",
        "@money",
        "@real",
        "@float",
        "@tiny_scale",
        "@varchar",
    ];
    let operators = [
        AstBinaryOp::Add,
        AstBinaryOp::Sub,
        AstBinaryOp::Mul,
        AstBinaryOp::Div,
        AstBinaryOp::Mod,
        AstBinaryOp::BitAnd,
        AstBinaryOp::BitOr,
        AstBinaryOp::BitXor,
    ];
    let mut checked = 0_u32;
    for op in operators {
        for left in numbers {
            for right in numbers {
                // A refused pair (`bit * bit`, `float % float`, a bitwise operator on a
                // `decimal`…) does not reach the executor.
                let Ok(bound) = bind(&binary(op, var(left), var(right))) else {
                    continue;
                };
                let (a, b) = operands(&bound);
                assert_eq!(
                    a.ty.ty.family(),
                    b.ty.ty.family(),
                    "{left} {op:?} {right}: {a:?} against {b:?}"
                );
                checked += 1;
            }
        }
    }
    // The literals and the temporal operands, which no variable of `Vars` spells.
    for (op, left, right) in [
        (AstBinaryOp::Mul, dec("1.5"), int("2")),
        (AstBinaryOp::Mul, int("2"), dec("1.5")),
        (AstBinaryOp::Add, int("1"), int("1")),
        (AstBinaryOp::Add, dec("1.5"), dec("2.5")),
        (AstBinaryOp::Add, text("a"), text("b")),
        (AstBinaryOp::Add, var("@datetime"), int("1")),
        (AstBinaryOp::Sub, var("@datetime"), var("@datetime")),
        (AstBinaryOp::Add, text("1"), int("2")),
    ] {
        let bound = b(&binary(op, left, right));
        let (a, b) = operands(&bound);
        assert_eq!(a.ty.ty.family(), b.ty.ty.family(), "{a:?} against {b:?}");
        checked += 1;
    }
    // A guard against a loop that silently binds nothing.
    assert!(checked > 100, "only {checked} pairs bound");
}

/// [`exact_numeric_view`] answers the precision and scale defined for the data type of
/// the expression — **minus `bit`**, which a `numeric(1, 0)` reading would include and
/// which SQL Server types otherwise.
///
/// Each of the six is fixed by a query: `CAST(1.5 AS decimal(2,1)) + CAST(1 AS t)`
/// reports the precision the reading of `t` predicts — 5 for a `tinyint`, 7 for a
/// `smallint`, 12 for an `int`, 21 for a `bigint`, 11 and scale 4 for a `smallmoney`,
/// 20 and scale 4 for a `money`. The `bit`, whose 3 and 1 agree with both readings,
/// settles nothing.
#[test]
fn an_operand_enters_with_the_precision_of_its_own_type() {
    for (ty, precision, scale) in [
        (SqlType::TinyInt, 3, 0),
        (SqlType::SmallInt, 5, 0),
        (SqlType::Int, 10, 0),
        (SqlType::BigInt, 19, 0),
        (SqlType::SmallMoney, 10, 4),
        (SqlType::Money, 19, 4),
    ] {
        let view = exact_numeric_view(&TypeInfo::new(ty, false))
            .unwrap_or_else(|| panic!("{ty:?} has an exact-numeric reading"));
        assert_eq!(view.ty, SqlType::Numeric { precision, scale }, "{ty:?}");
    }
    // The types that have none: they are converted to the common type instead. `bit`
    // leads the list: `CAST(1.5 AS decimal(2,1)) * CAST(1 AS bit)` is a
    // `decimal(5, 2)`, which is `decimal(2,1) * decimal(2,1)`, and
    // `CAST(1 AS bit) + CAST(0.1 AS decimal(38,38))` overflows.
    for ty in [
        SqlType::Bit,
        SqlType::Float,
        SqlType::Real,
        SqlType::VarChar(Len::Fixed(10)),
        SqlType::DateTime,
        SqlType::UniqueIdentifier,
    ] {
        assert!(
            exact_numeric_view(&TypeInfo::new(ty, false)).is_none(),
            "{ty:?}"
        );
    }
}

#[test]
fn concat_is_not_add() {
    let concat = b(&binary(AstBinaryOp::Add, text("a"), text("b")));
    assert!(matches!(
        concat.kind,
        BoundExprKind::Arith {
            op: vauban_types::BinaryOp::Concat,
            ..
        }
    ));
    assert_eq!(concat.ty.ty, SqlType::VarChar(Len::Fixed(2)));

    // One character operand is not enough: `types` converts the string to the number.
    let add = b(&binary(AstBinaryOp::Add, int("1"), text("a")));
    assert!(matches!(
        add.kind,
        BoundExprKind::Arith {
            op: vauban_types::BinaryOp::Add,
            ..
        }
    ));
    assert_eq!(add.ty.ty, SqlType::Int);
}

#[test]
fn invalid_operand_is_8117() {
    // `SELECT CAST(1 AS bit) * CAST(1 AS bit);` answers 8117 naming `bit` and the
    // multiply operator. The binder relays the error of `types` and adds the line.
    let e = err(&binary(AstBinaryOp::Mul, var("@bit"), var("@bit")));
    assert_eq!(e.number, 8117);
    assert_eq!(e.severity, 16);
    assert_eq!(
        e.message,
        SqlError::invalid_operand_type("bit", "multiply").message
    );
}

#[test]
fn error_numbers_are_not_retranslated() {
    // 206: `SELECT CAST('0E98…' AS uniqueidentifier) + 1;` (severity 16, state 2).
    // Not 8117.
    let clash = err(&binary(AstBinaryOp::Add, var("@guid"), int("1")));
    assert_eq!(clash.number, 206);
    assert_eq!(
        clash.message,
        SqlError::operand_type_clash("uniqueidentifier", "int").message
    );

    // 257: a date with `*`, `/` or `%`.
    let implicit = err(&binary(AstBinaryOp::Mul, var("@datetime"), int("1")));
    assert_eq!(implicit.number, 257);
    assert_eq!(
        implicit.message,
        SqlError::implicit_conversion_not_allowed("datetime", "int").message
    );

    // 402 on SQL Server (`bit` and `bit` in the add operator), which the `errors`
    // crate cannot spell yet, so `types` raises 8117 there. Either way the binder
    // passes the number on untouched.
    let pair = err(&binary(AstBinaryOp::Add, var("@bit"), var("@bit")));
    assert!(matches!(pair.number, 402 | 8117), "{pair:?}");
}

#[test]
fn comparison_is_a_predicate() {
    let eq = b(&binary(AstBinaryOp::Eq, int("1"), int("1")));
    assert!(eq.is_predicate());
    assert!(matches!(
        eq.kind,
        BoundExprKind::Compare {
            op: CompareOp::Eq,
            ..
        }
    ));
    assert_eq!(eq.ty.ty, SqlType::Bit);

    // `!<` and `!>` are normalised into their meaning.
    assert!(matches!(
        b(&binary(AstBinaryOp::NotLt, int("1"), int("2"))).kind,
        BoundExprKind::Compare {
            op: CompareOp::Ge,
            ..
        }
    ));
    assert!(matches!(
        b(&binary(AstBinaryOp::NotGt, int("1"), int("2"))).kind,
        BoundExprKind::Compare {
            op: CompareOp::Le,
            ..
        }
    ));
}

#[test]
fn comparison_inserts_the_conversion() {
    let cmp = b(&binary(AstBinaryOp::Eq, int("1"), text("1")));
    let (left, right) = operands(&cmp);
    // `int` outranks `varchar`, so the string is converted and not the number.
    assert!(matches!(left.kind, BoundExprKind::Literal(Value::I32(1))));
    assert!(is_convert_to(right, SqlType::Int), "{right:?}");
}

#[test]
fn logical_operators() {
    let and = b(&binary(
        AstBinaryOp::And,
        binary(AstBinaryOp::Eq, int("1"), int("1")),
        binary(AstBinaryOp::Eq, int("2"), int("2")),
    ));
    assert!(matches!(
        and.kind,
        BoundExprKind::Logical {
            op: LogicalOp::And,
            ..
        }
    ));
    assert_eq!(and.ty.ty, SqlType::Bit);

    let not = b(&unary(
        UnaryOp::Not,
        binary(AstBinaryOp::Eq, int("1"), int("1")),
    ));
    assert!(matches!(not.kind, BoundExprKind::Not(_)));

    // An operand of `AND` that is not a condition is 4145, like a `WHERE`.
    assert_eq!(
        err(&binary(
            AstBinaryOp::And,
            int("1"),
            binary(AstBinaryOp::Eq, int("1"), int("1")),
        ))
        .number,
        4145
    );
}

#[test]
fn non_boolean_condition_is_4145() {
    // `SELECT 1 WHERE 1` (no semicolon) answers `near '1'`, and the same batch with a
    // semicolon answers `near ';'`: SQL Server echoes the token that **follows** the
    // expression, and falls back on its last token at the end of the batch.
    let source = "SELECT 1 WHERE 1";
    let one = Expr::Literal(
        Literal::Integer("1".to_owned()),
        Span {
            line: 1,
            column: 16,
            offset: 15,
            len: 1,
        },
    );
    let e = bind_condition(&one, &ctx(source), &Scope::empty()).expect_err("1 is not a condition");
    assert_eq!(e.number, 4145);
    assert_eq!(e.severity, 15);
    assert_eq!(e.message, SqlError::non_boolean_expression("1").message);
    assert_eq!(e.line, 1);

    let e = bind_condition(&one, &ctx("SELECT 1 WHERE 1;"), &Scope::empty())
        .expect_err("1 is not a condition");
    assert!(e.message.contains("near ';'"), "{}", e.message);
}

#[test]
fn between_is_desugared() {
    let between = |negated| Expr::Between {
        expr: Box::new(int("1")),
        low: Box::new(int("0")),
        high: Box::new(int("2")),
        negated,
        span: any_span(),
    };

    let plain = b(&between(false));
    let (left, right) = operands(&plain);
    assert!(matches!(
        plain.kind,
        BoundExprKind::Logical {
            op: LogicalOp::And,
            ..
        }
    ));
    assert!(matches!(
        left.kind,
        BoundExprKind::Compare {
            op: CompareOp::Ge,
            ..
        }
    ));
    assert!(matches!(
        right.kind,
        BoundExprKind::Compare {
            op: CompareOp::Le,
            ..
        }
    ));

    let negated = b(&between(true));
    let (left, right) = operands(&negated);
    assert!(matches!(
        negated.kind,
        BoundExprKind::Logical {
            op: LogicalOp::Or,
            ..
        }
    ));
    assert!(matches!(
        left.kind,
        BoundExprKind::Compare {
            op: CompareOp::Lt,
            ..
        }
    ));
    assert!(matches!(
        right.kind,
        BoundExprKind::Compare {
            op: CompareOp::Gt,
            ..
        }
    ));
}

#[test]
fn case_result_type() {
    let searched = |else_: Option<Expr>| Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: binary(AstBinaryOp::Eq, int("1"), int("1")),
            then: int("1"),
        }],
        else_: else_.map(Box::new),
        span: any_span(),
    };

    // `SELECT SQL_VARIANT_PROPERTY(CASE WHEN 1 = 1 THEN 1 ELSE 2.5 END, 'Precision');`
    // answers 2, and 'Scale' answers 1: the common type is numeric(2, 1), the type of
    // the `ELSE` branch, because the integer literal enters with one digit. The `THEN`
    // branch alone therefore carries a conversion — `CAST(1 AS int)` would put one on
    // each (numeric(11, 1)).
    let case = b(&searched(Some(dec("2.5"))));
    assert_eq!(
        case.ty.ty,
        SqlType::Numeric {
            precision: 2,
            scale: 1
        }
    );
    let BoundExprKind::Case {
        operand,
        arms,
        else_,
    } = &case.kind
    else {
        panic!("expected a CASE, got {:?}", case.kind);
    };
    assert!(operand.is_none(), "the simple CASE is desugared");
    assert!(
        is_convert_to(
            &arms[0].then,
            SqlType::Numeric {
                precision: 2,
                scale: 1
            }
        ),
        "{:?}",
        arms[0].then
    );
    let else_ = else_.as_ref().expect("ELSE 2.5");
    assert!(matches!(
        else_.kind,
        BoundExprKind::Literal(Value::Decimal(_))
    ));

    // No `ELSE`: the `CASE` answers NULL when no arm matches.
    let without_else = b(&searched(None));
    assert_eq!(without_else.ty.ty, SqlType::Int);
    assert!(without_else.ty.nullable);
    assert!(!b(&searched(Some(int("2")))).ty.nullable);

    // A bare `NULL` branch takes the common type of the typed branches.
    let null_else_string = b(&Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: binary(AstBinaryOp::Eq, int("1"), int("1")),
            then: null(),
        }],
        else_: Some(Box::new(text("a"))),
        span: any_span(),
    });
    assert_eq!(null_else_string.ty.ty, SqlType::VarChar(Len::Fixed(1)));
    let null_else_decimal = b(&Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: binary(AstBinaryOp::Eq, int("1"), int("1")),
            then: null(),
        }],
        else_: Some(Box::new(var("@dec"))),
        span: any_span(),
    });
    assert_eq!(
        null_else_decimal.ty.ty,
        SqlType::Decimal {
            precision: 10,
            scale: 2
        }
    );
    let two_nulls_one_string = b(&Expr::Case {
        operand: None,
        arms: vec![
            CaseArm {
                when: binary(AstBinaryOp::Eq, int("1"), int("1")),
                then: null(),
            },
            CaseArm {
                when: binary(AstBinaryOp::Eq, int("2"), int("2")),
                then: null(),
            },
        ],
        else_: Some(Box::new(text("a"))),
        span: any_span(),
    });
    assert_eq!(two_nulls_one_string.ty.ty, SqlType::VarChar(Len::Fixed(1)));
    let all_null = b(&Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: binary(AstBinaryOp::Eq, int("1"), int("1")),
            then: null(),
        }],
        else_: Some(Box::new(null())),
        span: any_span(),
    });
    assert_eq!(all_null.ty.ty, SqlType::Int);
    assert!(all_null.ty.nullable);
}

#[test]
fn simple_case_is_desugared_into_a_searched_one() {
    let simple = Expr::Case {
        operand: Some(Box::new(int("1"))),
        arms: vec![CaseArm {
            when: int("1"),
            then: text("a"),
        }],
        else_: None,
        span: any_span(),
    };
    let case = b(&simple);
    assert_eq!(case.ty.ty, SqlType::VarChar(Len::Fixed(1)));
    let BoundExprKind::Case { operand, arms, .. } = &case.kind else {
        panic!("expected a CASE, got {:?}", case.kind);
    };
    assert!(operand.is_none(), "the operand moved into each WHEN");
    assert!(arms[0].when.is_predicate());
    assert!(matches!(
        arms[0].when.kind,
        BoundExprKind::Compare {
            op: CompareOp::Eq,
            ..
        }
    ));
}

#[test]
fn is_null_type() {
    let is_null = b(&Expr::IsNull {
        expr: Box::new(null()),
        negated: false,
        span: any_span(),
    });
    assert_eq!(is_null.ty.ty, SqlType::Bit);
    assert!(!is_null.ty.nullable);
    assert!(is_null.is_predicate());

    let is_not_null = b(&Expr::IsNull {
        expr: Box::new(null()),
        negated: true,
        span: any_span(),
    });
    assert!(matches!(
        is_not_null.kind,
        BoundExprKind::IsNull { negated: true, .. }
    ));
}

#[test]
fn in_list_types() {
    let in_list = |items: Vec<Expr>| Expr::In {
        expr: Box::new(int("1")),
        list: InList::Exprs(items),
        negated: false,
        span: any_span(),
    };

    let plain = b(&in_list(vec![int("1"), int("2"), int("3")]));
    let BoundExprKind::In { list, .. } = &plain.kind else {
        panic!("expected an IN, got {:?}", plain.kind);
    };
    assert_eq!(list.len(), 3);
    assert!(plain.is_predicate());

    // `SELECT CASE WHEN 1 IN (1, '2') THEN 1 ELSE 0 END;` answers 1: the string is
    // converted to the common type, `int`.
    let mixed = b(&in_list(vec![int("1"), text("2")]));
    let BoundExprKind::In { list, .. } = &mixed.kind else {
        panic!("expected an IN, got {:?}", mixed.kind);
    };
    assert!(is_convert_to(&list[1], SqlType::Int), "{:?}", list[1]);

    // The tested value is converted too when the list decides the common type, and it
    // enters the precision table with its digit count like anywhere else.
    let decimals = b(&in_list(vec![dec("1.5")]));
    let BoundExprKind::In { expr, .. } = &decimals.kind else {
        panic!("expected an IN, got {:?}", decimals.kind);
    };
    assert!(
        is_convert_to(
            expr,
            SqlType::Numeric {
                precision: 2,
                scale: 1
            }
        ),
        "{expr:?}"
    );

    // `SELECT CASE WHEN 1 IN (1, CAST('0E98…' AS uniqueidentifier)) …` answers 206
    // naming `uniqueidentifier` then `tinyint`: the element is named first, which is
    // the order the fold passes the operands in.
    let clash = err(&in_list(vec![int("1"), var("@guid")]));
    assert_eq!(clash.number, 206);
    assert_eq!(
        clash.message,
        SqlError::operand_type_clash("uniqueidentifier", "int").message
    );
}

#[test]
fn like_converts_instead_of_refusing() {
    let like = |expr: Expr, pattern: Expr| Expr::Like {
        expr: Box::new(expr),
        pattern: Box::new(pattern),
        escape: None,
        negated: false,
        span: any_span(),
    };

    let strings = b(&like(text("abc"), text("a%")));
    assert!(strings.is_predicate());
    let BoundExprKind::Like { expr, pattern, .. } = &strings.kind else {
        panic!("expected a LIKE, got {:?}", strings.kind);
    };
    // Two character operands: nothing to convert.
    assert!(matches!(expr.kind, BoundExprKind::Literal(_)));
    assert!(matches!(pattern.kind, BoundExprKind::Literal(_)));

    // `SELECT CASE WHEN 1 LIKE 2 THEN 1 ELSE 0 END;` answers 0 — no 8117: SQL Server
    // converts both operands to a string.
    let numbers = b(&like(int("1"), int("2")));
    let BoundExprKind::Like { expr, pattern, .. } = &numbers.kind else {
        panic!("expected a LIKE, got {:?}", numbers.kind);
    };
    assert!(is_convert_to(expr, SqlType::VarChar(Len::Max)), "{expr:?}");
    assert!(
        is_convert_to(pattern, SqlType::VarChar(Len::Max)),
        "{pattern:?}"
    );
}

#[test]
fn collate_changes_the_collation() {
    let collate = |expr: Expr, name: &str| Expr::Collate {
        expr: Box::new(expr),
        collation: name.to_owned(),
        collation_span: any_span(),
        span: any_span(),
    };

    let bound = b(&collate(text("a"), "Latin1_General_CS_AS"));
    assert_eq!(bound.ty.ty, SqlType::VarChar(Len::Fixed(1)));
    assert_eq!(
        bound.ty.collation,
        Some(Collation::parse("Latin1_General_CS_AS").expect("a known collation"))
    );
    assert!(matches!(bound.kind, BoundExprKind::Collate { .. }));

    assert_eq!(err(&collate(text("a"), "Klingon_CI_AS")).number, 448);

    // `SELECT CAST(1 AS int) COLLATE Latin1_General_CI_AS;` answers error 447 naming
    // `int`.
    let on_int = err(&collate(var("@int"), "Latin1_General_CI_AS"));
    assert_eq!(on_int.number, 447);
    assert_eq!(
        on_int.message,
        SqlError::collate_on_non_string("int").message
    );
}

#[test]
fn unary_operators() {
    // `+` is transparent, on any type: `SELECT +CAST('abc' AS varchar(10));` answers
    // `abc`.
    let plus = b(&unary(UnaryOp::Plus, var("@varchar")));
    assert_eq!(plus.ty.ty, SqlType::VarChar(Len::Fixed(10)));
    assert!(matches!(plus.kind, BoundExprKind::Variable { .. }));

    // `-tinyint` is a `smallint`, `-int` an `int`.
    let negate = b(&unary(UnaryOp::Minus, var("@tiny")));
    assert_eq!(negate.ty.ty, SqlType::SmallInt);
    assert!(matches!(negate.kind, BoundExprKind::Negate(_)));
    assert_eq!(b(&unary(UnaryOp::Minus, int("1"))).ty.ty, SqlType::Int);

    // `SELECT -CAST(1 AS bit);` answers 8117 naming `bit` and the `minus` operator —
    // the word is `minus`, not `unary minus`.
    let bad = err(&unary(UnaryOp::Minus, var("@bit")));
    assert_eq!(bad.number, 8117);
    assert_eq!(
        bad.message,
        SqlError::invalid_operand_type("bit", "minus").message
    );

    // `~` keeps the type of an integer or of a `bit` (`~tinyint` is a `tinyint`,
    // `~bit` is a `bit`).
    assert_eq!(
        b(&unary(UnaryOp::BitNot, var("@tiny"))).ty.ty,
        SqlType::TinyInt
    );
    assert_eq!(b(&unary(UnaryOp::BitNot, var("@bit"))).ty.ty, SqlType::Bit);
    let bad = err(&unary(UnaryOp::BitNot, var("@float")));
    assert_eq!(bad.number, 8117);
    assert_eq!(
        bad.message,
        SqlError::invalid_operand_type("float", "'~'").message
    );
}

#[test]
fn column_without_from_is_207() {
    let e = err(&column("c", 1));
    assert_eq!(e.number, 207);
    assert_eq!(e.severity, 16);
    assert_eq!(e.message, SqlError::invalid_column_name("c").message);

    // `SELECT t.c;` answers 4104 naming `"t.c"`, severity 16 state 1.
    let qualified = Expr::Column(ColumnRef {
        qualifier: Some(ObjectName {
            server: None,
            database: None,
            schema: None,
            name: Ident {
                value: "t".to_owned(),
                quoted: false,
            },
            span: any_span(),
        }),
        name: Ident {
            value: "c".to_owned(),
            quoted: false,
        },
        span: any_span(),
    });
    let error = err(&qualified);
    assert_eq!((error.number, error.severity, error.state), (4104, 16, 1));
    assert!(error.message.contains("\"t.c\""), "{}", error.message);
}

#[test]
fn niladic_functions_are_not_columns() {
    // `CURRENT_TIMESTAMP` reaches the binder as a column reference and must not become
    // error 207: `call::bind_niladic` binds it as a call.
    assert!(matches!(
        b(&column("CURRENT_TIMESTAMP", 1)).kind,
        BoundExprKind::Function { .. }
    ));
    // A name that *looks* niladic and is not stays a column, so that the user reads
    // 207 and not a message about a function (`call::bind_niladic`).
    // `CURRENT_CATALOG` is the SQL:2003 niladic function SQL Server does not have:
    // `SELECT CURRENT_CATALOG;` answers 207. Two spellings, for the case-insensitivity
    // of the lookup.
    for name in ["current_catalog", "CURRENT_CATALOG"] {
        assert_eq!(err(&column(name, 1)).number, 207, "{name}");
    }
    // A delimited name is a column, whatever it spells.
    let quoted = Expr::Column(ColumnRef {
        qualifier: None,
        name: Ident {
            value: "USER".to_owned(),
            quoted: true,
        },
        span: any_span(),
    });
    assert_eq!(err(&quoted).number, 207);
}

#[test]
fn errors_carry_the_line() {
    // "SELECT 1;\n\nSELECT c": the column is on line 3.
    let e = err(&column("c", 3));
    assert_eq!(e.number, 207);
    assert_eq!(e.line, 3);
    // An error raised by `types` gets the line of the operator node.
    let clash = binary(AstBinaryOp::Add, var("@guid"), int("1"));
    let Expr::Binary { span, .. } = &clash else {
        panic!("built as a binary node");
    };
    assert_eq!(span.line, 1);
    assert_eq!(err(&clash).line, 1);
}

#[test]
fn unknown_variable_is_137() {
    let e = err(&var("@x"));
    assert_eq!(e.number, 137);
    assert_eq!(e.severity, 15);
    assert_eq!(
        e.message,
        SqlError::must_declare_scalar_variable("@x").message
    );
    // `@@x` is a function of the registry, not a variable: `call::bind_variable_function`
    // binds it, case-insensitively.
    assert!(matches!(
        b(&var("@@rowcount")).kind,
        BoundExprKind::Function { .. }
    ));
    // A `@@x` the registry does not know is 137 too, state 2, with the name as written.
    let global = err(&var("@@NO_SUCH"));
    assert_eq!((global.number, global.severity, global.state), (137, 15, 2));
    assert!(
        global.message.contains("\"@@NO_SUCH\""),
        "{}",
        global.message
    );
}

#[test]
fn parentheses_are_dropped() {
    let nested = Expr::Nested(
        Box::new(binary(AstBinaryOp::Add, int("1"), int("1"))),
        any_span(),
    );
    assert!(matches!(b(&nested).kind, BoundExprKind::Arith { .. }));
}

#[test]
fn near_token_reads_what_follows_the_expression() {
    let span = |offset: u32, len: u32| Span {
        line: 1,
        column: offset + 1,
        offset,
        len,
    };
    assert_eq!(near_token_at("SELECT 1 WHERE 1;", &span(15, 1)).0, ";");
    assert_eq!(
        near_token_at("SELECT 1 WHERE 1 AND 1 = 1", &span(15, 1)).0,
        "AND"
    );
    assert_eq!(near_token_at("IF 1 SELECT 1;", &span(3, 1)).0, "SELECT");
    assert_eq!(near_token_at("SELECT 1 WHERE 1", &span(15, 1)).0, "1");
    assert_eq!(near_token_at("SELECT 1 WHERE 1 + 1", &span(15, 5)).0, "1");
    // Comments between the expression and the next token are skipped.
    assert_eq!(near_token_at("... 1 /* c */ -- x\n;", &span(4, 1)).0, ";");
    // And a **nested** block comment is skipped whole: `still` is a word inside a
    // comment, not the token 4145 quotes. The batch
    // `SELECT` / `1` / `WHERE` / `1` / `/* outer /* inner` / `*/ still outer */` / `;`
    // answers `near ';'` on line 8, where the first `*/` would have given
    // `near 'still'` on line 7.
    assert_eq!(
        near_token_at("... 1 /* outer /* inner */ still outer */ ;", &span(4, 1)).0,
        ";"
    );
    // A span outside the text is the empty slice, never a panic.
    assert_eq!(near_token_at("SELECT 1", &span(99, 3)).0, "");
}

/// The line 4145 carries is the quoted token's; each batch here puts that token on a
/// line the expression does not start on.
#[test]
fn near_token_carries_the_line_of_the_token_it_quotes() {
    // `SELECT\n1\nWHERE\n1\n+\n1;`: the expression `1\n+\n1` starts on line 4, the `;`
    // that follows it is on line 6.
    let text = "SELECT\n1\nWHERE\n1\n+\n1;";
    let at = text.rfind("1\n+").expect("the condition");
    let condition = Span {
        line: 4,
        column: 1,
        offset: at as u32,
        len: (text.len() - at - 1) as u32,
    };
    assert_eq!(near_token_at(text, &condition), (";", 6));

    // The `AND` on its own line, three lines below the expression it follows.
    let text = "SELECT 1 WHERE\n1\n\n\nAND 1 = 1;";
    let operand = Span {
        line: 2,
        column: 1,
        offset: 15,
        len: 1,
    };
    assert_eq!(near_token_at(text, &operand), ("AND", 5));

    // End of batch: the fall-back is the last token of the expression itself, and the
    // line is that token's, not the expression's.
    let text = "SELECT 1 WHERE\n1\n+\n2";
    let condition = Span {
        line: 2,
        column: 1,
        offset: 15,
        len: (text.len() - 15) as u32,
    };
    assert_eq!(near_token_at(text, &condition), ("2", 4));
}

#[test]
fn an_integer_literal_counts_its_digits() {
    assert_eq!(integer_digits(&Value::I32(1)), Some(1));
    assert_eq!(integer_digits(&Value::I32(-999)), Some(3));
    assert_eq!(integer_digits(&Value::I32(0)), Some(1));
    assert_eq!(integer_digits(&Value::I64(i64::MIN)), Some(19));
    assert_eq!(integer_digits(&Value::I8(255)), Some(3));
    assert_eq!(integer_digits(&Value::Null), None);
}

/// `CAST(<expr> AS <name>)`, without type arguments.
fn cast(expr: Expr, name: &str) -> Expr {
    Expr::Cast {
        expr: Box::new(expr),
        ty: vauban_parser::DataType {
            name: name.to_owned(),
            args: Vec::new(),
            span: any_span(),
        },
        try_: false,
        span: any_span(),
    }
}

/// `CASE WHEN @@SPID = 0 THEN <then> [ELSE <else_>] END`: a condition the server
/// cannot fold, so that the `CASE` is typed and not simplified.
fn case(then: Expr, else_: Option<Expr>) -> Expr {
    Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: binary(AstBinaryOp::Eq, var("@@SPID"), int("0")),
            then,
        }],
        else_: else_.map(Box::new),
        span: any_span(),
    }
}

/// The shapes of `arith_is_nullable` (`fNullable` of COLMETADATA): `@int` and
/// `@varchar` are non-nullable here, which is what `ISNULL(@i, 1)` is there.
#[test]
fn arithmetic_is_nullable_and_the_bitwise_and_concatenation_are_not() {
    for op in [
        AstBinaryOp::Add,
        AstBinaryOp::Sub,
        AstBinaryOp::Mul,
        AstBinaryOp::Div,
        AstBinaryOp::Mod,
    ] {
        assert!(b(&binary(op, var("@int"), int("1"))).ty.nullable, "{op:?}");
        assert!(b(&binary(op, int("1"), int("1"))).ty.nullable, "{op:?}");
    }
    for op in [AstBinaryOp::BitAnd, AstBinaryOp::BitOr, AstBinaryOp::BitXor] {
        assert!(!b(&binary(op, var("@int"), int("2"))).ty.nullable, "{op:?}");
        assert!(b(&binary(op, var("@nint"), int("2"))).ty.nullable, "{op:?}");
        // `~CAST(1 AS int) & 1`-like: an explicit conversion carries its nullability.
        assert!(
            b(&binary(op, cast(int("1"), "int"), int("2"))).ty.nullable,
            "{op:?}"
        );
    }
    // `@varchar + 'x'` → 0, `@varchar + CAST('x' AS varchar(1))` → 1, `'a' + NULL` → 1.
    assert!(
        !b(&binary(AstBinaryOp::Add, var("@varchar"), text("x")))
            .ty
            .nullable
    );
    assert!(
        b(&binary(
            AstBinaryOp::Add,
            var("@varchar"),
            cast(text("x"), "varchar")
        ))
        .ty
        .nullable
    );
    assert!(b(&binary(AstBinaryOp::Add, text("a"), null())).ty.nullable);
    // The binary concatenation stays an `Add` and follows its operands like the
    // character one: `0x00 + 0x01` → 0, `@nbin + 0x01` → 1,
    // `CAST(NULL AS varbinary(10)) + 0x01` → 1.
    let bin = b(&binary(AstBinaryOp::Add, hex("00"), hex("01")));
    assert_eq!(bin.ty.ty, SqlType::VarBinary(Len::Fixed(2)));
    assert!(!bin.ty.nullable);
    assert!(
        b(&binary(AstBinaryOp::Add, var("@nbin"), hex("01")))
            .ty
            .nullable
    );
    assert!(
        b(&binary(
            AstBinaryOp::Add,
            cast(null(), "varbinary"),
            hex("01")
        ))
        .ty
        .nullable
    );
    // A bitwise operator reads its operands once converted: `@tiny & @int` widens
    // the `tinyint` without loss, `@big & @int` narrows nothing either (both to
    // `bigint`), and the result follows the operands.
    assert!(
        !b(&binary(AstBinaryOp::BitAnd, var("@tiny"), var("@int")))
            .ty
            .nullable
    );
    assert!(
        !b(&binary(AstBinaryOp::BitAnd, var("@big"), var("@int")))
            .ty
            .nullable
    );
    // `@datetime + 1` → 1: arithmetic on a date is arithmetic.
    assert!(
        b(&binary(AstBinaryOp::Add, var("@datetime"), int("1")))
            .ty
            .nullable
    );
}

/// The shapes of `bind_unary` on nullability.
#[test]
fn negation_is_nullable_but_for_a_signed_literal() {
    let neg = |e: Expr| unary(UnaryOp::Minus, e);
    let not = |e: Expr| unary(UnaryOp::BitNot, e);
    assert!(!b(&neg(int("1"))).ty.nullable);
    assert!(!b(&neg(neg(int("1")))).ty.nullable);
    assert!(!b(&neg(dec("1.5"))).ty.nullable);
    assert!(!b(&neg(unary(UnaryOp::Plus, int("1")))).ty.nullable);
    assert!(b(&neg(var("@int"))).ty.nullable);
    assert!(b(&neg(var("@@SPID"))).ty.nullable);
    assert!(b(&neg(cast(int("1"), "int"))).ty.nullable);
    assert!(
        b(&neg(binary(AstBinaryOp::BitAnd, int("1"), int("1"))))
            .ty
            .nullable
    );
    assert!(!b(&not(int("1"))).ty.nullable);
    assert!(!b(&not(var("@int"))).ty.nullable);
    assert!(!b(&not(var("@@SPID"))).ty.nullable);
    assert!(b(&not(var("@nint"))).ty.nullable);
    assert!(b(&not(cast(int("1"), "int"))).ty.nullable);
}

/// The shapes of `bind_case` on nullability.
#[test]
fn case_is_nullable_by_its_branches_once_converted() {
    assert!(!b(&case(int("1"), Some(int("2")))).ty.nullable);
    assert!(b(&case(int("1"), None)).ty.nullable);
    assert!(b(&case(cast(int("1"), "int"), Some(int("2")))).ty.nullable);
    assert!(b(&case(int("1"), Some(cast(int("2"), "int")))).ty.nullable);
    assert!(b(&case(int("1"), Some(null()))).ty.nullable);
    assert!(b(&case(var("@nint"), Some(int("2")))).ty.nullable);
    // The condition does not count: `CASE WHEN @nint = 0 THEN 1 ELSE 2 END` → 0.
    let nullable_condition = Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: binary(AstBinaryOp::Eq, var("@nint"), int("0")),
            then: int("1"),
        }],
        else_: Some(Box::new(int("2"))),
        span: any_span(),
    };
    assert!(!b(&nullable_condition).ty.nullable);
    // The conversion of a branch counts, read from the type the branch exposes:
    // `THEN @int ELSE @dec` fits (`int` → `decimal(12,2)`), `THEN @tiny ELSE @int`
    // too, the literal `1` facing `1.5` does not (`int` → `numeric(2,1)`) and `@int`
    // facing `1.5` does (`numeric(11,1)`).
    let case_int_dec = b(&case(var("@int"), Some(var("@dec"))));
    assert_eq!(
        case_int_dec.ty.ty,
        SqlType::Decimal {
            precision: 12,
            scale: 2
        }
    );
    assert!(!case_int_dec.ty.nullable);
    assert!(!b(&case(var("@tiny"), Some(var("@int")))).ty.nullable);
    assert!(b(&case(int("1"), Some(dec("1.5")))).ty.nullable);
    assert!(!b(&case(var("@int"), Some(dec("1.5")))).ty.nullable);
}

/// The table of `implicit_conversion_may_be_null`, one row per rule and its
/// counter-example.
#[test]
fn implicit_conversions_that_may_lose_a_value_are_nullable() {
    use SqlType::{
        BigInt, Binary, Bit, Char, Date, DateTime, DateTime2, DateTimeOffset, Decimal, Float, Int,
        Money, NChar, NVarChar, Numeric, Real, SmallDateTime, SmallInt, SmallMoney, Time, TinyInt,
        VarBinary, VarChar,
    };
    let dec = |precision: u8, scale: u8| Decimal { precision, scale };
    let num = |precision: u8, scale: u8| Numeric { precision, scale };
    let lossless: &[(SqlType, SqlType)] = &[
        (Int, Int),
        (TinyInt, SmallInt),
        (TinyInt, Int),
        (TinyInt, BigInt),
        (SmallInt, Int),
        (Int, BigInt),
        (Bit, TinyInt),
        (Bit, Int),
        (Int, dec(10, 0)),
        (Int, dec(12, 2)),
        (BigInt, dec(19, 0)),
        (SmallInt, dec(5, 0)),
        (TinyInt, dec(3, 0)),
        (Bit, dec(1, 0)),
        (Int, Float),
        (SmallInt, Real),
        (Bit, Real),
        (Int, Money),
        (SmallInt, SmallMoney),
        (Bit, Money),
        (SmallMoney, Money),
        (Money, dec(19, 4)),
        (SmallMoney, dec(10, 4)),
        (dec(5, 2), Money),
        (dec(5, 2), dec(6, 3)),
        (dec(5, 2), dec(6, 2)),
        (dec(5, 2), num(6, 2)),
        (num(5, 2), dec(6, 2)),
        (Real, Float),
        (Char(Len::Fixed(5)), Char(Len::Fixed(10))),
        (Char(Len::Fixed(10)), VarChar(Len::Fixed(10))),
        (VarChar(Len::Fixed(5)), Char(Len::Fixed(10))),
        (VarChar(Len::Fixed(10)), NChar(Len::Fixed(10))),
        (NChar(Len::Fixed(10)), NVarChar(Len::Fixed(10))),
        (NVarChar(Len::Fixed(10)), VarChar(Len::Fixed(20))),
        (NVarChar(Len::Fixed(10)), NVarChar(Len::Max)),
        (VarChar(Len::Max), NVarChar(Len::Max)),
        (NVarChar(Len::Max), VarChar(Len::Max)),
        (VarBinary(Len::Fixed(10)), VarBinary(Len::Fixed(20))),
        (VarBinary(Len::Fixed(5)), Binary(Len::Fixed(10))),
        (VarBinary(Len::Fixed(10)), VarBinary(Len::Max)),
        (Date, DateTime2(7)),
        (Date, DateTimeOffset(7)),
        (Time(7), DateTime2(7)),
        (SmallDateTime, DateTime),
        (SmallDateTime, DateTime2(7)),
        (DateTime2(3), DateTime2(7)),
        (DateTime2(7), DateTimeOffset(7)),
        (Time(3), Time(7)),
        (DateTimeOffset(3), DateTimeOffset(7)),
    ];
    for (from, to) in lossless {
        assert!(
            !implicit_conversion_may_be_null(from, to),
            "{from:?} → {to:?} is lossless"
        );
    }
    let nullable: &[(SqlType, SqlType)] = &[
        (BigInt, Int),
        (Int, TinyInt),
        (TinyInt, Bit),
        (Int, dec(9, 0)),
        (Int, dec(10, 2)),
        (BigInt, dec(18, 0)),
        (SmallInt, dec(4, 0)),
        (TinyInt, dec(2, 0)),
        (BigInt, Float),
        (Int, Real),
        (BigInt, Money),
        (Int, SmallMoney),
        (Money, dec(18, 4)),
        (Money, BigInt),
        (SmallMoney, Int),
        (Money, Float),
        (dec(5, 2), dec(5, 3)),
        (dec(5, 2), dec(5, 1)),
        (dec(12, 2), dec(10, 2)),
        (Float, Real),
        (Float, dec(10, 2)),
        (dec(10, 2), Float),
        (Char(Len::Fixed(10)), Char(Len::Fixed(5))),
        (NVarChar(Len::Fixed(10)), VarChar(Len::Fixed(10))),
        (VarChar(Len::Fixed(10)), NVarChar(Len::Fixed(5))),
        (VarChar(Len::Max), VarChar(Len::Fixed(10))),
        (Binary(Len::Fixed(10)), Binary(Len::Fixed(5))),
        (Date, DateTime),
        (DateTime, SmallDateTime),
        (DateTime, DateTime2(7)),
        (DateTime, DateTimeOffset(7)),
        (DateTime2(7), DateTime2(3)),
        (Time(7), Time(3)),
        (DateTime2(7), DateTime),
        (Int, VarChar(Len::Fixed(30))),
        (Bit, VarChar(Len::Fixed(10))),
        (DateTime, VarChar(Len::Fixed(30))),
        (VarChar(Len::Fixed(10)), Int),
    ];
    for (from, to) in nullable {
        assert!(
            implicit_conversion_may_be_null(from, to),
            "{from:?} → {to:?} may lose a value"
        );
    }
}

/// `CAST(<expr> AS <name>(<args>))`, whose arguments are numbers.
fn cast_args(expr: Expr, name: &str, args: &[i64]) -> Expr {
    Expr::Cast {
        expr: Box::new(expr),
        ty: vauban_parser::DataType {
            name: name.to_owned(),
            args: args
                .iter()
                .map(|a| vauban_parser::TypeArg::Number(*a))
                .collect(),
            span: any_span(),
        },
        try_: false,
        span: any_span(),
    }
}

/// Beside an operand, the bare `NULL` takes that operand's type instead of the `int`
/// `bind_literal` gives it alone. Each line is a row of the table of
/// [`super::untyped_null_beside`].
#[test]
fn the_bare_null_takes_the_type_of_its_sibling() {
    let vectors: Vec<(Expr, SqlType)> = vec![
        // `SELECT 'a' + NULL;` is a concatenation, not an integer addition that would
        // answer 245.
        (text("a"), SqlType::VarChar(Len::Fixed(2))),
        (int("1"), SqlType::Int),
        (
            dec("1.5"),
            SqlType::Numeric {
                precision: 3,
                scale: 1,
            },
        ),
        (hex("01"), SqlType::VarBinary(Len::Fixed(2))),
        (cast(int("1"), "tinyint"), SqlType::TinyInt),
        (cast(int("1"), "bigint"), SqlType::BigInt),
        (cast(int("1"), "float"), SqlType::Float),
        (cast(int("1"), "money"), SqlType::Money),
        (
            cast_args(text("abc"), "varchar", &[3]),
            SqlType::VarChar(Len::Fixed(4)),
        ),
        (
            cast_args(text("abc"), "char", &[3]),
            SqlType::VarChar(Len::Fixed(4)),
        ),
        (
            cast_args(text("abc"), "nchar", &[3]),
            SqlType::NVarChar(Len::Fixed(4)),
        ),
        (
            cast_args(hex("01"), "binary", &[8]),
            SqlType::VarBinary(Len::Fixed(9)),
        ),
        (
            cast_args(dec("0.5"), "decimal", &[2, 2]),
            SqlType::Decimal {
                precision: 3,
                scale: 2,
            },
        ),
        (
            cast_args(dec("1.5"), "decimal", &[5, 2]),
            SqlType::Decimal {
                precision: 6,
                scale: 2,
            },
        ),
    ];
    for (sibling, expected) in vectors {
        let left = b(&binary(AstBinaryOp::Add, sibling.clone(), null()));
        assert_eq!(left.ty.ty, expected, "{sibling:?} + NULL");
        let right = b(&binary(AstBinaryOp::Add, null(), sibling.clone()));
        assert_eq!(right.ty.ty, expected, "NULL + {sibling:?}");
        assert!(left.ty.nullable, "{sibling:?} + NULL is nullable");
    }
}

/// The counter-proof of the rule above: a bare `NULL` on **both** sides keeps the `int`
/// of `SELECT NULL;`, and so do the operators that have no sibling to read.
#[test]
fn two_bare_nulls_stay_an_int() {
    for op in [
        AstBinaryOp::Add,
        AstBinaryOp::Sub,
        AstBinaryOp::Mul,
        AstBinaryOp::Div,
        AstBinaryOp::Mod,
        AstBinaryOp::BitAnd,
        AstBinaryOp::BitOr,
        AstBinaryOp::BitXor,
    ] {
        assert_eq!(b(&binary(op, null(), null())).ty.ty, SqlType::Int, "{op:?}");
    }
    assert_eq!(b(&unary(UnaryOp::Minus, null())).ty.ty, SqlType::Int);
    assert_eq!(b(&unary(UnaryOp::BitNot, null())).ty.ty, SqlType::Int);
}

/// `CAST(1 AS tinyint) + NULL` is a `tinyint` and `CAST(1.5 AS decimal(2,1)) * NULL` a
/// `decimal(5,2)`: neither is what typing the bare `NULL` `int` would give, an `int`
/// and a `decimal(13,1)`.
#[test]
fn the_bare_null_is_not_an_int_beside_a_narrower_sibling() {
    let tiny = b(&binary(AstBinaryOp::Add, cast(int("1"), "tinyint"), null()));
    assert_eq!(tiny.ty.ty, SqlType::TinyInt);
    let product = b(&binary(
        AstBinaryOp::Mul,
        cast_args(dec("1.5"), "decimal", &[2, 1]),
        null(),
    ));
    assert_eq!(
        product.ty.ty,
        SqlType::Decimal {
            precision: 5,
            scale: 2
        }
    );
}

/// A pair the operator refuses names the written operand `NULL`, and answers 402 where
/// the same pair of typed operands answers 8117
/// ([`super::refusal_names_the_bare_null`]).
#[test]
fn a_refused_bare_null_is_named_null_in_402() {
    let date = || cast(text("2020-01-01"), "date");
    let typed = err(&binary(AstBinaryOp::Add, date(), date()));
    assert_eq!(typed.number, 8117);
    assert_eq!(
        typed.message,
        SqlError::invalid_operand_type("date", "add").message
    );
    let right_null = err(&binary(AstBinaryOp::Add, date(), null()));
    assert_eq!(right_null.number, 402);
    assert_eq!(
        right_null.message,
        SqlError::incompatible_types_for_operator("date", "NULL", "add").message
    );
    let left_null = err(&binary(AstBinaryOp::Add, null(), date()));
    assert_eq!(left_null.number, 402);
    assert_eq!(
        left_null.message,
        SqlError::incompatible_types_for_operator("NULL", "date", "add").message
    );
    let bit = || cast(int("1"), "bit");
    assert_eq!(
        err(&binary(AstBinaryOp::Add, bit(), null())).message,
        SqlError::incompatible_types_for_operator("bit", "NULL", "add").message
    );
    assert_eq!(
        err(&binary(AstBinaryOp::Mul, text("a"), null())).message,
        SqlError::incompatible_types_for_operator("varchar", "NULL", "multiply").message
    );
}

/// The eight words of the table of [`super::operator_in_402`], each asserted on the
/// message: `SELECT CAST('2020-01-01' AS date) <op> NULL;` answers 402 on SQL Server
/// for the eight operators, and names the operand `NULL`.
///
/// Counter-proof of the same table: typing the bare `NULL` `int` would answer 206
/// naming `date` and `int`, which no assertion below accepts.
#[test]
fn every_operator_word_of_402_is_the_one_sql_server_prints() {
    let date = || cast(text("2020-01-01"), "date");
    let words = [
        (AstBinaryOp::Add, "add"),
        (AstBinaryOp::Sub, "subtract"),
        (AstBinaryOp::Mul, "multiply"),
        (AstBinaryOp::Div, "divide"),
        (AstBinaryOp::Mod, "modulo"),
        (AstBinaryOp::BitAnd, "'&'"),
        (AstBinaryOp::BitOr, "'|'"),
        (AstBinaryOp::BitXor, "'^'"),
    ];
    for (op, word) in words {
        let right = err(&binary(op, date(), null()));
        assert_eq!(right.number, 402, "{op:?}");
        assert_eq!(
            right.message,
            SqlError::incompatible_types_for_operator("date", "NULL", word).message
        );
        let left = err(&binary(op, null(), date()));
        assert_eq!(
            left.message,
            SqlError::incompatible_types_for_operator("NULL", "date", word).message
        );
    }
}
