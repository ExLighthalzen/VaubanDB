use super::{fingerprint, is_constant, order_by_invalid_in_a_subquery};
use crate::bound::{BoundExpr, BoundExprKind};
use vauban_parser::{Expr, ParseOptions, Statement, parse_batch};
use vauban_types::{SqlType, TypeInfo, Value};

/// The first `ORDER BY` key of a statement, as the parser produces it.
fn key(text: &str) -> Expr {
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let Statement::Select(select) = &batch.statements[0] else {
        unreachable!("{text} is a SELECT")
    };
    select.order_by[0].expr.clone()
}

/// The constant walk answers on the seven forms SQL Server refuses with 408 and on the
/// eight it accepts (module documentation).
#[test]
fn a_constant_key_is_the_one_408_names() {
    for text in [
        "SELECT a FROM dbo.t ORDER BY 'abc'",
        "SELECT a FROM dbo.t ORDER BY NULL",
        "SELECT a FROM dbo.t ORDER BY 2.0",
        "SELECT a FROM dbo.t ORDER BY CAST(1 AS int)",
        "SELECT a FROM dbo.t ORDER BY LEN('abc')",
        "SELECT a FROM dbo.t ORDER BY CASE WHEN 1 = 1 THEN 1 ELSE 2 END",
        "SELECT a FROM dbo.t ORDER BY (1 + 1)",
    ] {
        assert!(is_constant(&key(text)), "{text}");
    }
    for text in [
        "SELECT a FROM dbo.t ORDER BY @@SPID",
        "SELECT a FROM dbo.t ORDER BY GETDATE()",
        "SELECT a FROM dbo.t ORDER BY LEN(@@VERSION)",
        "SELECT a FROM dbo.t ORDER BY ABS(@v)",
        "SELECT a FROM dbo.t ORDER BY @v + 0",
        "SELECT a FROM dbo.t ORDER BY 1 + a",
        "SELECT a FROM dbo.t ORDER BY a",
        "SELECT a FROM dbo.t ORDER BY CASE WHEN a = 1 THEN 1 ELSE 2 END",
    ] {
        assert!(!is_constant(&key(text)), "{text}");
    }
}

/// Two expressions of the same shape written on two lines share their fingerprint, and
/// two of different shapes do not.
#[test]
fn a_fingerprint_ignores_the_line_a_node_was_written_on() {
    let literal = |line: u32| BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(1)),
        ty: TypeInfo::new(SqlType::Int, false),
        line,
    };
    let negated = |line: u32| BoundExpr {
        kind: BoundExprKind::Negate(Box::new(literal(line))),
        ty: TypeInfo::new(SqlType::Int, false),
        line,
    };
    assert_eq!(fingerprint(&negated(1)), fingerprint(&negated(9)));
    assert_ne!(fingerprint(&negated(1)), fingerprint(&literal(1)));
    assert!(
        !fingerprint(&negated(1)).contains("line: 1"),
        "the line is blanked"
    );
}

/// The 1033 a nested query raises carries the number, the severity and the state SQL
/// Server gives it.
#[test]
fn an_order_by_in_a_subquery_is_1033() {
    let error = order_by_invalid_in_a_subquery();
    assert_eq!((error.number, error.severity, error.state), (1033, 15, 1));
    assert_eq!(
        error.message,
        "ORDER BY is not allowed in a view, a derived table or a subquery without TOP, \
         OFFSET or FOR XML."
    );
}
