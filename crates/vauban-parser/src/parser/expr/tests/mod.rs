//! Unit tests for `parser::expr`: precedence climbing, literals, predicates
//! (`IS NULL`, `IN`, `LIKE`, `BETWEEN`, `EXISTS`, quantified comparisons) and
//! `CASE`.

use super::{MAX_CASE_NESTING, parse_expr, parse_expr_list, parse_value_expr, starts_a_value};
use crate::ast::expr::{Expr, InList, Literal};
use crate::parser::{ParseOptions, Parser};
use vauban_errors::{SqlError, SqlResult};

/// Builds a cursor over `text` with the default options.
fn cursor(text: &str) -> Parser<'static> {
    static OPTIONS: ParseOptions = ParseOptions {
        quoted_identifier: true,
    };
    match Parser::new(text, &OPTIONS) {
        Ok(parser) => parser,
        Err(error) => unreachable!("the test text lexes: {error:?}"),
    }
}

/// Parses `text` as one expression in predicate position and checks that the whole
/// text was read.
fn e(text: &str) -> Expr {
    match try_e(text) {
        Ok(expr) => expr,
        Err(error) => unreachable!("{text} should parse: {error:?}"),
    }
}

/// Same as [`e`], without the panic: for the tests that expect an error.
///
/// A text that parses but leaves tokens behind is an error too, and the error is the
/// one its first leftover token would give inside a statement: that is what happens
/// to `1 = 1` read in value position, where the loop stops on the `=`.
fn try_e(text: &str) -> SqlResult<Expr> {
    let mut p = cursor(text);
    let expr = parse_expr(&mut p)?;
    if p.at_eof() {
        Ok(expr)
    } else {
        Err(p.error_here())
    }
}

/// Parses `text` as one expression in **value** position, as a select item or a call
/// argument does, and checks that the whole text was read.
fn try_v(text: &str) -> SqlResult<Expr> {
    let mut p = cursor(text);
    let expr = parse_value_expr(&mut p)?;
    if p.at_eof() {
        Ok(expr)
    } else {
        Err(p.error_here())
    }
}

/// Checks the `parse` -> `Display` -> `parse` loop on `text` and returns what
/// `Display` wrote.
fn roundtrip(text: &str) -> String {
    let first = e(text);
    let printed = first.to_string();
    assert_eq!(first, e(&printed), "{text} was printed as {printed}");
    printed
}

/// The error of a text that must not parse.
fn error(text: &str) -> SqlError {
    match try_e(text) {
        Ok(expr) => unreachable!("{text} should not parse, got {expr:?}"),
        Err(error) => error,
    }
}

/// A parenthesised rendering of the tree, so that a precedence test reads like the
/// vector it checks: `1 + 2 * 3` becomes `Add(1, Mul(2, 3))`.
///
/// `Display` cannot be used for that: it adds no parenthesis, so `Add(1, Mul(2, 3))`
/// and `Mul(Add(1, 2), 3)` both print `1 + 2 * 3`.
fn shape(expr: &Expr) -> String {
    match expr {
        Expr::Literal(literal, _) => literal.to_string(),
        Expr::Column(column) => column.to_string(),
        Expr::Variable { name, .. } => name.clone(),
        Expr::Binary {
            op, left, right, ..
        } => format!("{op:?}({}, {})", shape(left), shape(right)),
        Expr::Unary { op, expr, .. } => format!("{op:?}({})", shape(expr)),
        Expr::Nested(inner, _) => format!("Nested({})", shape(inner)),
        Expr::IsNull { expr, negated, .. } => {
            let name = if *negated { "IsNotNull" } else { "IsNull" };
            format!("{name}({})", shape(expr))
        }
        Expr::In {
            expr,
            list,
            negated,
            ..
        } => {
            let name = if *negated { "NotIn" } else { "In" };
            format!("{name}({}, {list})", shape(expr))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            ..
        } => {
            let name = if *negated { "NotLike" } else { "Like" };
            format!("{name}({}, {pattern})", shape(expr))
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
            ..
        } => {
            let name = if *negated { "NotBetween" } else { "Between" };
            format!("{name}({}, {low}, {high})", shape(expr))
        }
        Expr::Collate {
            expr, collation, ..
        } => format!("Collate({}, {collation})", shape(expr)),
        other => other.to_string(),
    }
}

#[test]
fn precedence_arithmetic() {
    assert_eq!(shape(&e("1 + 2 * 3")), "Add(1, Mul(2, 3))");
    assert_eq!(shape(&e("2 * 3 + 1")), "Add(Mul(2, 3), 1)");
    // Left-associative.
    assert_eq!(shape(&e("8 / 4 / 2")), "Div(Div(8, 4), 2)");
    assert_eq!(shape(&e("10 % 3 * 2")), "Mul(Mod(10, 3), 2)");
}

/// In T-SQL, `&`, `^`, `|`, `+` and `-` are at rank 3, unlike C where the bitwise
/// operators are far looser.
#[test]
fn precedence_bitwise_equals_additive() {
    assert_eq!(shape(&e("1 | 2 + 3")), "Add(BitOr(1, 2), 3)");
    assert_eq!(shape(&e("1 & 2 * 3")), "BitAnd(1, Mul(2, 3))");
    assert_eq!(shape(&e("1 + 2 | 3")), "BitOr(Add(1, 2), 3)");
    assert_eq!(shape(&e("1 ^ 2 & 3")), "BitAnd(BitXor(1, 2), 3)");
}

/// A unary `-` binds tighter than the binary operators of ranks 2 and 3. The two
/// readings cannot be told apart on `*` (`-1 * 2` is `-2` either way), but they can on
/// a rank-3 operator: `-1 & 2` is `2` under this rule and `0` under the other, and the
/// `2` is what SQL Server returns.
#[test]
fn precedence_unary() {
    assert_eq!(shape(&e("~1 + 2")), "Add(BitNot(1), 2)");
    assert_eq!(shape(&e("-1 * 2")), "Mul(Minus(1), 2)");
    assert_eq!(shape(&e("-1 & 2")), "BitAnd(Minus(1), 2)");
    assert_eq!(
        shape(&e("NOT a = 1 AND b = 2")),
        "And(Not(Eq(a, 1)), Eq(b, 2))"
    );
    assert_eq!(
        shape(&e("NOT (a = 1 AND b = 2)")),
        "Not(Nested(And(Eq(a, 1), Eq(b, 2))))"
    );
}

/// The suffix predicates hang off rank 4, tighter than `NOT`, so `NOT a IN (…)`
/// negates the whole predicate. `a NOT IN (…)` means the same thing and keeps a
/// different tree, which is what lets `Display` write each one back as it was read.
#[test]
fn not_binds_looser_than_predicates() {
    assert_eq!(shape(&e("NOT a IN (1, 2)")), "Not(In(a, 1, 2))");
    assert_eq!(shape(&e("a NOT IN (1, 2)")), "NotIn(a, 1, 2)");
    assert_eq!(shape(&e("NOT a IS NULL")), "Not(IsNull(a))");
    assert_eq!(roundtrip("NOT a IN (1, 2)"), "NOT a IN (1, 2)");
    assert_eq!(roundtrip("a NOT IN (1, 2)"), "a NOT IN (1, 2)");
}

#[test]
fn precedence_logical() {
    assert_eq!(
        shape(&e("a = 1 OR b = 2 AND c = 3")),
        "Or(Eq(a, 1), And(Eq(b, 2), Eq(c, 3)))"
    );
    assert_eq!(
        shape(&e("a = 1 AND b = 2 OR c = 3")),
        "Or(And(Eq(a, 1), Eq(b, 2)), Eq(c, 3))"
    );
}

#[test]
fn precedence_comparison_below_arithmetic() {
    assert_eq!(shape(&e("1 + 1 = 2")), "Eq(Add(1, 1), 2)");
    assert_eq!(shape(&e("1 <> 2 * 3")), "Ne(1, Mul(2, 3))");
    // `!=` and `<>` are the same operator; `Display` writes `<>`.
    assert_eq!(shape(&e("1 != 2")), "Ne(1, 2)");
}

#[test]
fn literals() {
    let literal = |text: &str| match &e(text) {
        Expr::Literal(literal, _) => literal.clone(),
        other => unreachable!("{text} is a literal, got {other:?}"),
    };
    assert_eq!(literal("1"), Literal::Integer("1".to_owned()));
    assert_eq!(literal("1.50"), Literal::Decimal("1.50".to_owned()));
    assert_eq!(literal("1.5E-2"), Literal::Float("1.5E-2".to_owned()));
    // The `0x` and the `$` are dropped here, not by the lexer.
    assert_eq!(literal("0x00FF"), Literal::Binary("00FF".to_owned()));
    assert_eq!(literal("$1.50"), Literal::Money("1.50".to_owned()));
    assert_eq!(literal("$-1.50"), Literal::Money("-1.50".to_owned()));
    assert_eq!(
        literal("'a''b'"),
        Literal::Str {
            value: "a'b".to_owned(),
            unicode: false,
        }
    );
    assert_eq!(
        literal("N'é'"),
        Literal::Str {
            value: "é".to_owned(),
            unicode: true,
        }
    );
    assert_eq!(literal("NULL"), Literal::Null);
    assert_eq!(literal("DEFAULT"), Literal::Default);
    // The source text is kept as written: `1.50` does not become `1.5`.
    for text in [
        "1", "1.50", "1.5E-2", "0x00FF", "$1.50", "$-1.50", "'a''b'", "N'é'", "NULL",
    ] {
        assert_eq!(roundtrip(text), text);
    }
}

/// The lexer only makes a `Money` token of `$[-|+]number`, so the `-` of `-$1.50` is
/// the unary operator.
#[test]
fn unary_minus_before_money() {
    assert_eq!(shape(&e("-$1.50")), "Minus($1.50)");
    assert_eq!(roundtrip("-$1.50"), "-$1.50");
}

#[test]
fn column_refs() {
    let parts = |text: &str| match &e(text) {
        Expr::Column(column) => (
            column.qualifier.as_ref().map(|q| q.to_string()),
            column.name.value.clone(),
            column.name.quoted,
        ),
        other => unreachable!("{text} is a column, got {other:?}"),
    };
    assert_eq!(parts("c"), (None, "c".to_owned(), false));
    assert_eq!(parts("t.c"), (Some("t".to_owned()), "c".to_owned(), false));
    assert_eq!(
        parts("dbo.t.c"),
        (Some("dbo.t".to_owned()), "c".to_owned(), false)
    );
    assert_eq!(
        parts("db.dbo.t.c"),
        (Some("db.dbo.t".to_owned()), "c".to_owned(), false)
    );
    assert_eq!(
        parts("[my db].dbo.[my t].[my c]"),
        (
            Some("[my db].dbo.[my t]".to_owned()),
            "my c".to_owned(),
            true
        )
    );
    for text in [
        "c",
        "t.c",
        "dbo.t.c",
        "db.dbo.t.c",
        "[my db].dbo.[my t].[my c]",
    ] {
        assert_eq!(roundtrip(text), text);
    }
    // The part right before the column cannot be empty: `a..c` names no table.
    assert_eq!(error("a..c").number, 102);
}

#[test]
fn variables() {
    let name = |text: &str| match &e(text) {
        Expr::Variable { name, .. } => name.clone(),
        other => unreachable!("{text} is a variable, got {other:?}"),
    };
    assert_eq!(name("@x"), "@x");
    assert_eq!(name("@@ROWCOUNT"), "@@ROWCOUNT");
    assert_eq!(roundtrip("@x + 1"), "@x + 1");
}

#[test]
fn functions() {
    // The name is read part by part rather than through `ObjectName::to_string`,
    // which brackets a reserved word: writing a function name bare is the business of
    // `Expr::Function`'s own `Display`, checked by the `roundtrip` below.
    let call = |text: &str| match &e(text) {
        Expr::Function {
            name,
            args,
            star,
            distinct,
            ..
        } => {
            let mut printed = String::new();
            if let Some(schema) = &name.schema {
                printed.push_str(&schema.value);
                printed.push('.');
            }
            printed.push_str(&name.name.value);
            (printed, args.len(), *star, *distinct)
        }
        other => unreachable!("{text} is a call, got {other:?}"),
    };
    assert_eq!(call("GETDATE()"), ("GETDATE".to_owned(), 0, false, false));
    assert_eq!(call("ISNULL(a, 0)"), ("ISNULL".to_owned(), 2, false, false));
    assert_eq!(call("COUNT(*)"), ("COUNT".to_owned(), 0, true, false));
    assert_eq!(
        call("COUNT(DISTINCT c)"),
        ("COUNT".to_owned(), 1, false, true)
    );
    assert_eq!(call("dbo.f(1)"), ("dbo.f".to_owned(), 1, false, false));
    // `LEFT` is reserved and is still a function name: the name of a call does not
    // go through `parse_ident`.
    assert_eq!(call("LEFT(a, 3)"), ("LEFT".to_owned(), 2, false, false));
    assert_eq!(call("RIGHT(a, 3)"), ("RIGHT".to_owned(), 2, false, false));
    assert_eq!(
        call("COALESCE(a, b)"),
        ("COALESCE".to_owned(), 2, false, false)
    );
    assert_eq!(call("NULLIF(a, b)"), ("NULLIF".to_owned(), 2, false, false));
    for text in [
        "GETDATE()",
        "ISNULL(a, 0)",
        "COUNT(*)",
        "COUNT(DISTINCT c)",
        "dbo.f(1)",
        "LEFT(a, 3)",
        "COALESCE(a, b)",
        "NULLIF(a, b)",
    ] {
        assert_eq!(roundtrip(text), text);
    }

    // The niladic functions written without parentheses: a one-part unquoted column
    // reference, which the binder recognises as a function.
    for text in [
        "CURRENT_TIMESTAMP",
        "CURRENT_USER",
        "SESSION_USER",
        "SYSTEM_USER",
        "USER",
    ] {
        match &e(text) {
            Expr::Column(column) => {
                assert!(column.qualifier.is_none(), "{text} has no qualifier");
                assert_eq!(column.name.value, text);
                assert!(!column.name.quoted, "{text} is not quoted");
            }
            other => unreachable!("{text} is a niladic function, got {other:?}"),
        }
    }
}

/// A niladic function survives the `parse` -> `Display` -> `parse` loop: it is an
/// `Expr::Column` whose name is a reserved word, and `display::ColumnRef` writes it
/// bare instead of applying the safety rule that would bracket it (same mechanism as
/// the function-name exception).
///
/// The brackets the user wrote are a different tree and stay: `[CURRENT_TIMESTAMP]`
/// is a quoted name, which the binder reads as a column, not as a function.
#[test]
fn niladic_roundtrips() {
    for text in [
        "CURRENT_TIMESTAMP",
        "CURRENT_USER",
        "SESSION_USER",
        "SYSTEM_USER",
        "USER",
    ] {
        assert_eq!(roundtrip(text), text);
    }
    assert_eq!(roundtrip("[CURRENT_TIMESTAMP]"), "[CURRENT_TIMESTAMP]");
    assert_ne!(e("CURRENT_TIMESTAMP"), e("[CURRENT_TIMESTAMP]"));
    // The exception is spelled on the name, not on the case it was written in.
    assert_eq!(roundtrip("current_timestamp"), "current_timestamp");
}

#[test]
fn case_expressions() {
    let searched = "CASE WHEN 1 = 1 THEN 'a' WHEN 2 = 2 THEN 'b' ELSE 'c' END";
    match &e(searched) {
        Expr::Case {
            operand,
            arms,
            else_,
            ..
        } => {
            assert!(operand.is_none());
            assert_eq!(arms.len(), 2);
            assert!(else_.is_some());
        }
        other => unreachable!("{searched} is a CASE, got {other:?}"),
    }
    let simple = "CASE c WHEN 1 THEN 'a' END";
    match &e(simple) {
        Expr::Case {
            operand,
            arms,
            else_,
            ..
        } => {
            assert!(operand.is_some());
            assert_eq!(arms.len(), 1);
            assert!(else_.is_none());
        }
        other => unreachable!("{simple} is a CASE, got {other:?}"),
    }
    assert_eq!(roundtrip(searched), searched);
    assert_eq!(roundtrip(simple), simple);
    // `END` is reserved, so a 156 is reported on it.
    assert_eq!(error("CASE END").number, 156);
}

#[test]
fn cast_and_convert() {
    match &e("CAST('1' AS int)") {
        Expr::Cast { ty, try_, .. } => {
            assert_eq!(ty.name, "int");
            assert!(!try_);
        }
        other => unreachable!("a CAST, got {other:?}"),
    }
    match &e("TRY_CAST(c AS decimal(18, 2))") {
        Expr::Cast { ty, try_, .. } => {
            assert_eq!(ty.name, "decimal");
            assert_eq!(ty.args.len(), 2);
            assert!(*try_);
        }
        other => unreachable!("a TRY_CAST, got {other:?}"),
    }
    // `CONVERT` writes the type first, the other way round from `CAST`.
    match &e("CONVERT(varchar(10), GETDATE(), 120)") {
        Expr::Convert {
            ty,
            expr,
            style,
            try_,
            ..
        } => {
            assert_eq!(ty.name, "varchar");
            assert_eq!(expr.to_string(), "GETDATE()");
            assert_eq!(
                style.as_ref().map(|s| s.to_string()),
                Some("120".to_owned())
            );
            assert!(!try_);
        }
        other => unreachable!("a CONVERT, got {other:?}"),
    }
    match &e("CONVERT(int, c)") {
        Expr::Convert { style, .. } => assert!(style.is_none()),
        other => unreachable!("a CONVERT, got {other:?}"),
    }
    match &e("TRY_CONVERT(varchar(max), c)") {
        Expr::Convert { ty, try_, .. } => {
            assert_eq!(ty.args.len(), 1);
            assert!(*try_);
        }
        other => unreachable!("a TRY_CONVERT, got {other:?}"),
    }
    for text in [
        "CAST('1' AS int)",
        "TRY_CAST(c AS decimal(18, 2))",
        "CONVERT(varchar(10), GETDATE(), 120)",
        "CONVERT(int, c)",
        "TRY_CONVERT(varchar(MAX), c)",
    ] {
        assert_eq!(roundtrip(text), text);
    }
}

#[test]
fn predicates() {
    assert_eq!(shape(&e("c IS NULL")), "IsNull(c)");
    assert_eq!(shape(&e("c IS NOT NULL")), "IsNotNull(c)");
    assert_eq!(shape(&e("c IN (1, 2)")), "In(c, 1, 2)");
    assert_eq!(shape(&e("c LIKE 'a%'")), "Like(c, 'a%')");
    assert_eq!(shape(&e("c BETWEEN 1 AND 2")), "Between(c, 1, 2)");
    assert_eq!(shape(&e("c NOT BETWEEN 1 AND 2")), "NotBetween(c, 1, 2)");
    assert_eq!(
        shape(&e("c COLLATE Latin1_General_CI_AS")),
        "Collate(c, Latin1_General_CI_AS)"
    );
    match &e("c NOT LIKE 'a%' ESCAPE '!'") {
        Expr::Like {
            escape, negated, ..
        } => {
            assert!(*negated);
            assert_eq!(
                escape.as_ref().map(|x| x.to_string()),
                Some("'!'".to_owned())
            );
        }
        other => unreachable!("a LIKE, got {other:?}"),
    }
    match &e("c IN (1, 2)") {
        Expr::In { list, .. } => {
            assert!(matches!(list, InList::Exprs(items) if items.len() == 2))
        }
        other => unreachable!("an IN, got {other:?}"),
    }
    for text in [
        "c IS NULL",
        "c IS NOT NULL",
        "c IN (1, 2)",
        "c LIKE 'a%'",
        "c NOT LIKE 'a%' ESCAPE '!'",
        "c BETWEEN 1 AND 2",
        "c NOT BETWEEN 1 AND 2",
        "c COLLATE Latin1_General_CI_AS",
    ] {
        assert_eq!(roundtrip(text), text);
    }
    // `COLLATE` binds tighter than every operator.
    assert_eq!(
        shape(&e("c COLLATE Latin1_General_CI_AS = 'a'")),
        "Eq(Collate(c, Latin1_General_CI_AS), 'a')"
    );
}

/// The predicates that hold a subquery. What is checked here is the predicate; the shape
/// of the query it holds is the business of `tests/select.rs`.
#[test]
fn subquery_predicates() {
    use crate::ast::expr::{BinaryOp, Quantifier};

    match &e("c NOT IN (SELECT 1)") {
        Expr::In { list, negated, .. } => {
            assert!(*negated);
            assert!(matches!(list, InList::Subquery(_)), "a subquery IN list");
        }
        other => unreachable!("a NOT IN, got {other:?}"),
    }
    match e("EXISTS (SELECT 1)") {
        Expr::Exists(..) => {}
        other => unreachable!("an EXISTS, got {other:?}"),
    }
    match e("(SELECT 1)") {
        Expr::Subquery(..) => {}
        other => unreachable!("a scalar subquery, got {other:?}"),
    }
    let quantified = [
        ("c > ALL (SELECT 1)", BinaryOp::Gt, Quantifier::All),
        ("c = ANY (SELECT 1)", BinaryOp::Eq, Quantifier::Any),
        // `SOME` is a synonym of `ANY` that the AST keeps as written.
        ("c = SOME (SELECT 1)", BinaryOp::Eq, Quantifier::Some),
    ];
    for (text, expected_op, expected_quantifier) in quantified {
        match e(text) {
            Expr::Quantified { op, quantifier, .. } => {
                assert_eq!(op, expected_op, "{text}");
                assert_eq!(quantifier, expected_quantifier, "{text}");
            }
            other => unreachable!("{text} is a quantified comparison, got {other:?}"),
        }
    }
    for text in [
        "c NOT IN (SELECT 1)",
        "EXISTS (SELECT 1)",
        "c > ALL (SELECT 1)",
        "c = ANY (SELECT 1)",
        "c = SOME (SELECT 1)",
        "(SELECT 1)",
    ] {
        assert_eq!(roundtrip(text), text);
    }
}

#[test]
fn between_binds_tighter_than_and() {
    assert_eq!(
        shape(&e("a BETWEEN 1 AND 2 AND b = 3")),
        "And(Between(a, 1, 2), Eq(b, 3))"
    );
}

/// `(1 + 2)` is an `Expr::Nested` and `((1))` two of them, since `Display` neither
/// adds nor removes a parenthesis. `(SELECT …)` is an `Expr::Subquery`, told apart by
/// a single `peek_at(1)`; it is checked by `subquery_predicates`.
#[test]
fn subqueries() {
    assert_eq!(shape(&e("(1 + 2)")), "Nested(Add(1, 2))");
    assert_eq!(shape(&e("((1))")), "Nested(Nested(1))");
    assert_eq!(roundtrip("((1))"), "((1))");
    assert_eq!(roundtrip("(1 + 2) * 3"), "(1 + 2) * 3");
}

#[test]
fn over_clause() {
    let partitioned = "ROW_NUMBER() OVER (PARTITION BY a, b ORDER BY c DESC)";
    match &e(partitioned) {
        Expr::Function { over, .. } => match over {
            Some(over) => {
                assert_eq!(over.partition_by.len(), 2);
                assert_eq!(over.order_by.len(), 1);
                assert!(over.order_by[0].desc);
                assert!(over.frame.is_none());
            }
            None => unreachable!("{partitioned} has an OVER clause"),
        },
        other => unreachable!("{partitioned} is a call, got {other:?}"),
    }
    let framed = "SUM(x) OVER (ORDER BY d ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)";
    match &e(framed) {
        Expr::Function { over, .. } => match over {
            Some(over) => {
                assert!(over.partition_by.is_empty());
                assert_eq!(over.order_by.len(), 1);
                assert!(over.frame.is_some());
            }
            None => unreachable!("{framed} has an OVER clause"),
        },
        other => unreachable!("{framed} is a call, got {other:?}"),
    }
    assert_eq!(roundtrip(partitioned), partitioned);
    assert_eq!(roundtrip(framed), framed);
    assert_eq!(
        roundtrip("SUM(x) OVER (ORDER BY d ROWS 3 PRECEDING)"),
        "SUM(x) OVER (ORDER BY d ROWS 3 PRECEDING)"
    );
}

/// `NEXT VALUE FOR` is V3: the parser accepts it, the binder refuses it.
#[test]
fn next_value_for() {
    match &e("NEXT VALUE FOR dbo.s") {
        Expr::NextValueFor { sequence, over, .. } => {
            assert_eq!(sequence.to_string(), "dbo.s");
            assert!(over.is_none());
        }
        other => unreachable!("a NEXT VALUE FOR, got {other:?}"),
    }
    assert_eq!(roundtrip("NEXT VALUE FOR dbo.s"), "NEXT VALUE FOR dbo.s");
    // `NEXT` alone is not reserved and stays a column name.
    assert_eq!(shape(&e("next")), "next");
}

#[test]
fn expr_list_reads_values() {
    let mut p = cursor("1, 2, 3");
    let list = match parse_expr_list(&mut p) {
        Ok(list) => list,
        Err(error) => unreachable!("a list of literals: {error:?}"),
    };
    assert_eq!(list.len(), 3);
    assert!(p.at_eof());
}

#[test]
fn expr_errors() {
    // The last token is the operator `+`; which token the message names at the end
    // of a batch is `syntax_error.rs`'s business.
    let incomplete = error("1 +");
    assert_eq!(incomplete.number, 102);
    assert_eq!(incomplete.line, 1);
    // `CAST(1 AS)`: the type is missing and the `)` gets the error.
    let no_type = error("CAST(1 AS)");
    assert_eq!(no_type.number, 102);
    assert_eq!(no_type.message, "Syntax error near ')'.");
    assert_eq!(no_type.line, 1);
    // `END` and `IN` are reserved: these are 156.
    for text in ["CASE END", "IN (1, 2)"] {
        let reserved = error(text);
        assert_eq!(reserved.number, 156, "{text}: {reserved:?}");
        assert_eq!(reserved.line, 1);
    }
    // The line of the offending token, not the line of the batch.
    let second_line = error("1 +\n*");
    assert_eq!(second_line.number, 102);
    assert_eq!(second_line.line, 2);
}

/// A predicate is not a scalar value: `SELECT (1 = 1)` is a syntax error in T-SQL,
/// and so is a comparison used as an operand. The number and the token the message
/// names are the ones SQL Server answers: 102, `Syntax error near '='.`.
///
/// The refusal is the business of the **value** entry point, since the same
/// parentheses hold a predicate in `WHERE (a = 1 AND b = 2)`, which must keep
/// parsing; the vector `e("(1 = 1)")` is written here on
/// [`parse_value_expr`] for that reason, and the search-condition reading is asserted
/// right after it.
#[test]
fn predicate_is_not_a_value() {
    // The offending token is the `=` of the two first, and the reserved `NOT` of
    // the third: 102 and 156.
    for (text, number) in [("(1 = 1)", 102), ("1 + (1 = 1)", 102), ("NOT 1", 156)] {
        match try_v(text) {
            Ok(expr) => unreachable!("{text} is no value, got {expr:?}"),
            Err(error) => assert_eq!(error.number, number, "{text}: {error:?}"),
        }
    }
    let in_operand = error("1 + (1 = 1)");
    assert_eq!(in_operand.number, 102, "{in_operand:?}");
    // In predicate position the same parentheses hold a search condition, which is
    // what `WHERE (a = 1 AND b = 2)` needs.
    assert_eq!(shape(&e("(1 = 1)")), "Nested(Eq(1, 1))");
}

/// A value position also refuses the words of rank 5 to 7, and a prefix `NOT` with
/// them.
#[test]
fn value_position_refuses_logical_operators() {
    for text in ["1 = 1", "a AND b", "a OR b", "a IS NULL", "a IN (1)"] {
        assert!(try_v(text).is_err(), "{text} is no value");
    }
    // What a value position does accept.
    for text in [
        "1 + 2",
        "-1",
        "~1",
        "f(1)",
        "(1 + 2)",
        "CASE WHEN a = 1 THEN 2 END",
    ] {
        assert!(try_v(text).is_ok(), "{text} is a value");
    }
}

/// A unary operator applies to the primary that follows it, and `NOT` may be
/// repeated.
#[test]
fn prefix_operators_stack() {
    assert_eq!(shape(&e("- -1")), "Minus(Minus(1))");
    assert_eq!(shape(&e("NOT NOT a = 1")), "Not(Not(Eq(a, 1)))");
    assert_eq!(shape(&e("+1 + 2")), "Add(Plus(1), 2)");
}

/// A comparison yields a search condition, and a search condition is not a value on
/// the left of an operator any more than it is on its right: `a = b = c` is a syntax
/// error in T-SQL, not `Eq(Eq(a, b), c)` as in C. Same refusal for a chained rank-4
/// operator and for a chained suffix predicate.
///
/// A reserved word such as `IN` is a 156, hence the check on the message rather than
/// on the number alone. The two vectors marked below answer a 102 through [`try_e`], which
/// enters the grammar in **search-condition** position and stops on the leftover `=`;
/// what a client sees is the **value** position of a select item, and there
/// `parse_batch` answers the same 156 as SQL Server.
#[test]
fn chained_comparison_is_a_syntax_error() {
    let chained_eq = error("a = b = c");
    assert_eq!(chained_eq.number, 102);
    assert_eq!(chained_eq.message, "Syntax error near '='.");
    let chained_lt = error("1 < 2 < 3");
    assert_eq!(chained_lt.number, 102);
    assert_eq!(chained_lt.message, "Syntax error near '<'.");
    // The number follows the token the message names: the reserved `IN` of the
    // second predicate gives a 156, the chained `=` a 102. On SQL Server,
    // `SELECT 1 IN (1) IN (1)` answers 156 and `SELECT 1 = 1 = 1` a 102. The first
    // vector below matches that; the next two do not.
    for (text, number) in [
        ("1 IN (1) IN (1)", 156),
        // Search-condition entry point: the predicate parses and the leftover `=`
        // gives the 102. In value position `SELECT 1 IN (1) = 1` answers 156 `near
        // the keyword 'IN'`, which is what SQL Server answers too.
        ("1 IN (1) = 1", 102),
        // Same pair: `SELECT 1 IS NULL = 1` answers 156 `near the keyword 'IS'` on
        // both sides.
        ("a IS NULL = 1", 102),
    ] {
        let chained = error(text);
        assert_eq!(chained.number, number, "{text}: {chained:?}");
    }
    let chained_in = error("1 IN (1) IN (1)");
    assert!(
        chained_in.message.contains("'IN'"),
        "the second IN gets the error, got {}",
        chained_in.message
    );
    // What stays legal: the words of rank 5 to 7 do follow a search condition.
    for text in [
        "a = 1 AND b = 2",
        "a = 1 OR b = 2",
        "NOT a = 1",
        "a BETWEEN 1 AND 2 AND b = 3",
        "a IS NULL AND b IS NOT NULL",
    ] {
        assert!(try_e(text).is_ok(), "{text} is a search condition");
    }
}

/// `EXISTS (…)` is a search condition, never a value: `SELECT EXISTS (SELECT 1)` is a
/// syntax error in T-SQL while `WHERE EXISTS (SELECT 1)` is not. The error itself is
/// a 156 on the reserved `EXISTS`, as on SQL Server.
///
/// In predicate position the parse must reach the `SELECT` inside: what is asserted is
/// that the refusal did **not** happen.
#[test]
fn exists_is_not_a_value() {
    let as_value = match try_v("EXISTS (SELECT 1)") {
        Ok(expr) => unreachable!("EXISTS is no value, got {expr:?}"),
        Err(error) => error,
    };
    assert_eq!(as_value.number, 156, "{as_value:?}");
    assert!(
        as_value.message.contains("'EXISTS'"),
        "the EXISTS gets the error, got {}",
        as_value.message
    );
    // An operand is a value too, `1 + EXISTS (…)` included.
    let as_operand = error("1 + EXISTS (SELECT 1)");
    assert_eq!(as_operand.number, 156, "{as_operand:?}");
    if let Err(condition) = try_e("EXISTS (SELECT 1)") {
        assert_eq!(
            condition.number, 50000,
            "in predicate position EXISTS reaches the SELECT, got {condition:?}"
        );
    }
    if let Err(negated) = try_e("NOT EXISTS (SELECT 1)") {
        assert_eq!(negated.number, 50000, "NOT EXISTS too, got {negated:?}");
    }
}

// ---- The `CASE` limit ----
//
// The shapes below follow SQL Server; the corners stated on this parser alone are
// named where they are asserted.

/// The five positions a `CASE` can stand in inside another `CASE`, each as a format
/// with `{}` for the inner expression.
const CASE_POSITIONS: [(&str, &str, u8); 5] = [
    ("THEN", "CASE WHEN 1 = 1 THEN {} ELSE 0 END", 4),
    ("ELSE", "CASE WHEN 1 = 1 THEN 0 ELSE {} END", 4),
    ("searched WHEN", "CASE WHEN {} = 1 THEN 1 ELSE 0 END", 4),
    ("operand", "CASE {} WHEN 1 THEN 1 ELSE 0 END", 3),
    ("simple WHEN", "CASE 1 WHEN {} THEN 1 ELSE 0 END", 3),
];

/// Wraps `inner` in `levels` copies of `shape`, innermost first.
fn nest(shape: &str, levels: u32, inner: &str) -> String {
    let mut text = inner.to_owned();
    for _ in 0..levels {
        text = shape.replacen("{}", &text, 1);
    }
    text
}

/// The error of a whole batch, which is what a subquery needs.
fn batch_error(text: &str) -> SqlError {
    match crate::parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => unreachable!("{text} should not parse, got {batch:?}"),
        Err(error) => error,
    }
}

/// Checks that `error` is the 125 of the eleventh `CASE`, with `state` and `line`.
fn assert_is_125(error: &SqlError, state: u8, line: u32, what: &str) {
    assert_eq!(
        (error.number, error.severity, error.state, error.line),
        (125, 15, state, line),
        "{what}: {error:?}"
    );
    assert_eq!(
        error.message, "A CASE expression cannot be nested deeper than level 10.",
        "{what}"
    );
}

/// Ten levels pass and eleven answer 125 in the five positions, with the state of the
/// eleventh: 4 in the three positions of a searched `CASE`, 3 in the two of a simple
/// one.
#[test]
fn ten_nested_cases_pass_and_eleven_answer_125_in_the_five_positions() {
    assert_eq!(MAX_CASE_NESTING, 10);
    for (name, shape, state) in CASE_POSITIONS {
        let ten = nest(shape, 10, "1");
        assert!(try_v(&ten).is_ok(), "ten in the {name}: {:?}", try_v(&ten));
        let eleven = nest(shape, 11, "1");
        assert_is_125(&error(&eleven), state, 1, &format!("eleven in the {name}"));
        let twenty = nest(shape, 20, "1");
        assert_is_125(&error(&twenty), state, 1, &format!("twenty in the {name}"));
    }
}

/// A parenthesis, a call, a `CAST`, an operator or a scalar subquery between two
/// `CASE`s neither resets nor adds a level: ten pass, eleven answer 125.
#[test]
fn the_case_count_follows_the_syntax_through_what_stands_between_two_cases() {
    let through = [
        ("parenthesis", "CASE WHEN 1 = 1 THEN ({}) ELSE 0 END"),
        ("call", "CASE WHEN 1 = 1 THEN ABS({}) ELSE 0 END"),
        ("CAST", "CASE WHEN 1 = 1 THEN CAST({} AS int) ELSE 0 END"),
        ("operator", "CASE WHEN 1 = 1 THEN 1 + {} ELSE 0 END"),
        ("subquery", "CASE WHEN 1 = 1 THEN (SELECT {}) ELSE 0 END"),
    ];
    for (name, shape) in through {
        let ten = format!("SELECT {}", nest(shape, 10, "1"));
        let parsed = crate::parse_batch(&ten, &ParseOptions::default());
        assert!(parsed.is_ok(), "ten through a {name}: {parsed:?}");
        let eleven = format!("SELECT {}", nest(shape, 11, "1"));
        assert_is_125(
            &batch_error(&eleven),
            4,
            1,
            &format!("eleven through a {name}"),
        );
    }
    // One CASE, a subquery, then ten: the count goes on through the subquery.
    let one_over_ten = format!(
        "SELECT CASE WHEN 1 = 1 THEN (SELECT {}) ELSE 0 END",
        nest(CASE_POSITIONS[0].1, 10, "1")
    );
    assert_is_125(
        &batch_error(&one_over_ten),
        4,
        1,
        "one CASE over a subquery of ten",
    );
}

/// The state is the one of the eleventh `CASE`, counted from the outside: not the
/// outermost's, not the innermost's.
#[test]
fn the_state_of_125_names_the_eleventh_case() {
    let searched = CASE_POSITIONS[0].1;
    let simple = CASE_POSITIONS[3].1;
    // Ten searched over a simple eleventh: 3.
    let inner_simple = nest(searched, 10, &nest(simple, 1, "1"));
    assert_is_125(
        &error(&inner_simple),
        3,
        1,
        "simple eleventh under ten searched",
    );
    // A simple outermost over ten searched: the eleventh is searched, 4.
    let outer_simple = nest(simple, 1, &nest(searched, 10, "1"));
    assert_is_125(
        &error(&outer_simple),
        4,
        1,
        "simple outermost, searched eleventh",
    );
    // Twelve deep, simple eleventh, searched twelfth: 3 -- the innermost does not decide.
    let simple_eleventh = nest(searched, 10, &nest(simple, 1, &nest(searched, 1, "1")));
    assert_is_125(
        &error(&simple_eleventh),
        3,
        1,
        "simple eleventh, searched twelfth",
    );
    // Twelve deep, searched eleventh, simple twelfth: 4.
    let simple_twelfth = nest(searched, 11, &nest(simple, 1, "1"));
    assert_is_125(
        &error(&simple_twelfth),
        4,
        1,
        "searched eleventh, simple twelfth",
    );
}

/// The line of the error is the one of the eleventh `CASE` keyword, not the one of
/// its `WHEN`, of its `END` or of the statement.
#[test]
fn error_125_is_on_the_line_of_the_eleventh_case_keyword() {
    let mut lines = vec!["SELECT".to_owned()];
    lines.extend((0..12).map(|_| "CASE WHEN 1 = 1 THEN".to_owned()));
    lines.push("1".to_owned());
    lines.extend((0..12).map(|_| "ELSE 0 END".to_owned()));
    let text = lines.join("\n");
    // `SELECT` is line 1, the first `CASE` line 2, the eleventh line 12.
    assert_is_125(&batch_error(&text), 4, 12, "one CASE per line");
    // The keyword alone on its line: the `WHEN` on the next one does not move it.
    let keyword_alone = format!(
        "SELECT {}",
        nest(
            CASE_POSITIONS[0].1,
            10,
            "CASE\nWHEN 1 = 1 THEN 1\nELSE 0 END"
        )
    );
    assert_is_125(
        &batch_error(&keyword_alone),
        4,
        1,
        "keyword on line 1, WHEN on line 2",
    );
    let simple_alone = format!(
        "SELECT 1,\n{}",
        nest(CASE_POSITIONS[0].1, 10, "CASE\n1 WHEN 1 THEN 1\nELSE 0 END")
    );
    assert_is_125(
        &batch_error(&simple_alone),
        3,
        2,
        "simple keyword on line 2",
    );
}

/// The eleventh `CASE` is refused on its keyword and one token of lookahead: what is
/// inside it or after it is not read, what is before it has already been.
#[test]
fn the_eleventh_case_is_refused_on_its_keyword_and_one_token() {
    let ten = |inner: &str| nest(CASE_POSITIONS[0].1, 10, inner);
    // A syntax error inside the eleventh, in the searched and in the simple form.
    for (inner, state) in [
        ("CASE WHEN 1 + THEN 1 END", 4),
        ("CASE WHEN END", 4),
        ("CASE WHEN 1 = 1 THEN END", 4),
        ("CASE 1 + WHEN 1 THEN 1 END", 3),
        ("CASE ( END", 3),
        ("CASE 1 2 END", 3),
        ("CASE - END", 3),
        ("CASE + 1 WHEN 1 THEN 1 END", 3),
        ("CASE CASE END END", 3),
        ("CASE WHEN CASE END THEN 1 END", 4),
    ] {
        assert_is_125(&error(&ten(inner)), state, 1, inner);
    }
    // A syntax error after the eleventh: 125 first.
    let trailing = format!("SELECT {} + ;", ten("CASE WHEN 1 = 1 THEN 1 END"));
    assert_is_125(&batch_error(&trailing), 4, 1, "trailing syntax error");
    // A syntax error before it wins.
    let leading = format!("SELECT 1 +, {}", ten("CASE WHEN 1 = 1 THEN 1 END"));
    let before = batch_error(&leading);
    assert_eq!(before.number, 102, "{before:?}");
    // A token that opens neither a WHEN nor a value: the error of depth one.
    for inner in [
        "CASE END",
        "CASE THEN 1 END",
        "CASE ELSE 1 END",
        "CASE , 1 END",
        "CASE ) END",
        "CASE ; END",
        "CASE = 1 END",
        "CASE SELECT 1 END",
        "CASE FROM END",
        "CASE * END",
        "CASE NOT 1 = 1 THEN 1 END",
        "CASE EXISTS (SELECT 1) WHEN 1 THEN 1 END",
    ] {
        let shallow = error(inner);
        let deep = error(&ten(inner));
        assert_eq!(
            shallow, deep,
            "{inner}: depth eleven answers what depth one does"
        );
        assert!(matches!(shallow.number, 102 | 156), "{inner}: {shallow:?}");
    }
}

/// The count is a depth, not a total: siblings do not add up.
#[test]
fn sibling_cases_do_not_add_up() {
    let searched = CASE_POSITIONS[0].1;
    let ten = nest(searched, 10, "1");
    let siblings = format!("SELECT {ten}, {ten}");
    let parsed = crate::parse_batch(&siblings, &ParseOptions::default());
    assert!(parsed.is_ok(), "two chains of ten side by side: {parsed:?}");
    let nine = nest(searched, 9, "1");
    let both_branches = format!("CASE WHEN 1 = 1 THEN {nine} ELSE {nine} END");
    assert!(
        try_v(&both_branches).is_ok(),
        "nine in both branches: {:?}",
        try_v(&both_branches)
    );
    // Two chains of ten in the two branches of an eleventh: still a depth of eleven.
    let over_both = format!("CASE WHEN 1 = 1 THEN {ten} ELSE {ten} END");
    assert_is_125(&error(&over_both), 4, 1, "ten in both branches of one");
}

/// A `CASE` refused at the limit, or failed inside, leaks no level on the counter.
#[test]
fn a_refused_case_leaks_no_level() {
    let mut p = cursor(&nest(CASE_POSITIONS[0].1, 11, "1"));
    assert!(parse_value_expr(&mut p).is_err());
    assert_eq!(p.case_depth(), 0, "after the 125");
    let mut p = cursor("CASE WHEN 1 = 1 THEN CASE WHEN 1 + THEN 1 END END");
    assert!(parse_value_expr(&mut p).is_err());
    assert_eq!(p.case_depth(), 0, "after a syntax error inside");
}

/// `starts_a_value` says what the grammar does: it answers `true` exactly when the
/// operand parse of a simple `CASE` gets past its first token.
///
/// The grammar's verdict is read on `CASE <operand> WHEN 1 THEN 1 END` at depth one:
/// a parse, or an error that names another token than the first, means the first
/// token was accepted. The operands are chosen so that their first token is not
/// written a second time in the text.
#[test]
fn starts_a_value_agrees_with_the_grammar() {
    let operands = [
        "1",
        "1.5",
        "1e3",
        "$1",
        "0x00",
        "'a'",
        "N'a'",
        "@x",
        "@@rowcount",
        "col",
        "[col]",
        "dbo.col",
        "(1)",
        "( END",
        "1 +",
        "- 1",
        "+ 1",
        "~ 1",
        "- END",
        "NULL",
        "DEFAULT",
        "CASE 2 WHEN 2 THEN 3 END",
        "CAST(2 AS int)",
        "TRY_CAST(2 AS int)",
        "CONVERT(int, 2)",
        "TRY_CONVERT(int, 2)",
        "CURRENT_TIMESTAMP",
        "CURRENT_DATE",
        "USER",
        "NEXT VALUE FOR s",
        "LEFT('a', 2)",
        "COALESCE(2, 3)",
        "LEFT",
        "IN",
        "EXISTS (SELECT 2)",
        "NOT 2 = 2",
        "SELECT",
        "FROM",
        "END",
        "THEN",
        "ELSE",
        ",",
        ")",
        ";",
        "=",
        "*",
        "/",
        "\\",
        "{",
    ];
    let mut accepted = 0;
    for operand in operands {
        let mut p = cursor(operand);
        let first = p.peek().text.clone();
        let lookahead = starts_a_value(&mut p);
        assert_eq!(p.mark(), 0, "{operand}: the lookahead consumes nothing");
        let grammar = match try_v(&format!("CASE {operand} WHEN 1 THEN 1 END")) {
            Ok(_) => true,
            Err(error) => !error.message.contains(&format!("'{first}'")),
        };
        assert_eq!(
            lookahead, grammar,
            "{operand}: lookahead against the grammar"
        );
        accepted += usize::from(lookahead);
    }
    assert_eq!(
        accepted,
        32,
        "starters among the {} operands",
        operands.len()
    );
}

/// The single arm of the searched `CASE` that an `IIF(…)` is read as, and its `ELSE`.
fn iif_parts(text: &str) -> (String, String, String) {
    match &e(text) {
        Expr::Case {
            operand: None,
            arms,
            else_: Some(else_),
            ..
        } if arms.len() == 1 => (shape(&arms[0].when), shape(&arms[0].then), shape(else_)),
        other => unreachable!("{text} is a searched CASE with one arm, got {other:?}"),
    }
}

/// `IIF(c, a, b)` is the searched `CASE WHEN c THEN a ELSE b END`: its first argument is
/// a search condition, read with each of the predicates below.
#[test]
fn iif_is_a_searched_case() {
    let (when, then, else_) = iif_parts("IIF(@e IS NULL, N'', @e)");
    assert_eq!(when, "IsNull(@e)");
    assert_eq!(then, "N''");
    assert_eq!(else_, "@e");
    for (text, condition) in [
        ("IIF(1 = 1, 'a', 'b')", "Eq(1, 1)"),
        ("IIF(1 = 1 AND 2 > 1, 1, 0)", "And(Eq(1, 1), Gt(2, 1))"),
        (
            "IIF(1 = 0 OR NOT 1 = 0, 1, 0)",
            "Or(Eq(1, 0), Not(Eq(1, 0)))",
        ),
        ("IIF(1 IN (1, 2), 1, 0)", "In(1, 1, 2)"),
        ("IIF('ab' LIKE 'a%', 1, 0)", "Like('ab', 'a%')"),
        ("IIF(1 BETWEEN 0 AND 2, 1, 0)", "Between(1, 0, 2)"),
        ("IIF(NULL IS NOT NULL, 1, 0)", "IsNotNull(NULL)"),
        ("iif(1 = 1, 1, 0)", "Eq(1, 1)"),
        ("Iif(1 = 1, 1, 0)", "Eq(1, 1)"),
    ] {
        assert_eq!(iif_parts(text).0, condition, "{text}");
    }
    match &e("IIF(EXISTS (SELECT 1 FROM sys.objects WHERE 1 = 0), 1, 0)") {
        Expr::Case { arms, .. } => assert!(matches!(arms[0].when, Expr::Exists(..))),
        other => unreachable!("an IIF is a CASE, got {other:?}"),
    }
    // A condition that is not a predicate still parses; the binder refuses it.
    assert_eq!(iif_parts("IIF(1, 2, 3)").0, "1");
    // Nested, and used as an operand.
    let (_, then, _) = iif_parts("iif(1 = 1, IIF(2 = 2, 'a', 'b'), 'c')");
    assert_eq!(then, "CASE WHEN 2 = 2 THEN 'a' ELSE 'b' END");
    assert_eq!(
        shape(&e("IIF(1 = 1, 1, 0) = 1")),
        "Eq(CASE WHEN 1 = 1 THEN 1 ELSE 0 END, 1)"
    );
    assert!(try_v("IIF(1 = 1, 1, 0)").is_ok());
    // It prints as the CASE it is.
    assert_eq!(
        roundtrip("IIF(@e IS NULL, N'', @e)"),
        "CASE WHEN @e IS NULL THEN N'' ELSE @e END"
    );
}

/// The span of the `CASE` an `IIF` is read as covers the written `IIF(…)`, from its name
/// to its `)`.
#[test]
fn iif_span_covers_the_written_call() {
    let text = "IIF(1 = 1,\n 1,\n 'a')";
    match e(text) {
        Expr::Case { span, .. } => {
            assert_eq!(span.line, 1);
            assert_eq!(span.offset, 0);
            assert_eq!(span.len as usize, text.len());
        }
        other => unreachable!("an IIF is a CASE, got {other:?}"),
    }
}

/// A delimited or qualified `IIF` is an ordinary call, whose arguments are values.
#[test]
fn delimited_or_qualified_iif_is_a_call() {
    for text in ["[IIF](1, 2, 3)", "dbo.IIF(1, 2, 3)"] {
        assert!(matches!(e(text), Expr::Function { .. }), "{text}");
    }
    for text in ["[IIF](1 = 1, 1, 2)", "dbo.IIF(1 = 1, 1, 2)"] {
        let refused = error(text);
        assert_eq!(refused.number, 102, "{text}");
        assert_eq!(refused.message, "Syntax error near '='.", "{text}");
    }
}

/// `IIF` takes three arguments, and its results are values.
#[test]
fn iif_argument_errors() {
    for (text, token) in [
        ("IIF(1 = 1, 2)", ")"),
        ("IIF(1 = 1, 2, 3, 4)", ","),
        ("IIF(1 = 1, 1 = 1, 0)", "="),
    ] {
        let refused = error(text);
        assert_eq!(refused.number, 102, "{text}");
        assert_eq!(
            refused.message,
            format!("Syntax error near '{token}'."),
            "{text}"
        );
        assert_eq!(refused.line, 1, "{text}");
    }
}
