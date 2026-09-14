//! Expressions whose printed form is a trap: two tokens that, once glued, lex as a third
//! one: the sign operators (`+`, `-`, `~`).
//!
//! Everything goes through the public entry point, `parse_batch`, and through the loop the
//! module README makes a contract: for any accepted text, parsing what `Display` wrote
//! yields an **equal** `Batch`. `SELECT - -10` used to be printed `SELECT --10`, where the
//! two signs start a line comment: the text no longer parsed at all.
//!
//! The expected forms below are what SQL Server answers:
//!
//! ```text
//! SELECT - -10    -> 10
//! SELECT -(-10)   -> 10
//! SELECT - - -10  -> -10
//! SELECT + +1     -> 1        SELECT ++1  -> 1
//! SELECT ~ ~1     -> 1        SELECT ~~1  -> 1
//! SELECT -+1      -> -1       SELECT +-1  -> -1
//! SELECT --10     -> 102, near 'SELECT'
//! ```

use vauban_parser::{
    Batch, Expr, Literal, ParseOptions, QueryBody, SelectItem, Statement, UnaryOp, parse_batch,
};

/// Parses `text`, which must parse.
fn p(text: &str) -> Batch {
    parse_batch(text, &ParseOptions::default()).unwrap()
}

/// Checks the `parse` -> `Display` -> `parse` loop on `text`, checks that printing is a
/// fixed point (printing the reparsed batch writes the very same text), and returns what
/// `Display` wrote.
fn rt(text: &str) -> String {
    let batch = p(text);
    let printed = batch.to_string();
    let reparsed = p(&printed);
    assert_eq!(batch, reparsed, "{text} was printed as {printed}");
    assert_eq!(
        printed,
        reparsed.to_string(),
        "{text} is not a fixed point of Display"
    );
    printed
}

/// The one expression of a batch that holds one `SELECT` of one item.
fn one(text: &str) -> Expr {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    // `QueryBody` implements `Drop`, which forbids moving the boxed
    // specification out of it by pattern matching (E0509): the item list is taken out of
    // the specification instead, and yields the expression this helper yielded before.
    let items = match statements.pop() {
        Some(Statement::Select(mut select)) => match &mut select.body {
            QueryBody::Select(spec) => std::mem::take(&mut spec.items),
            other => unreachable!("{text} is one specification, got {other:?}"),
        },
        other => unreachable!("{text} is a SELECT, got {other:?}"),
    };
    match <[SelectItem; 1]>::try_from(items) {
        Ok([SelectItem::Expr { expr, .. }]) => expr,
        other => unreachable!("{text} is one expression, got {other:?}"),
    }
}

/// The operator and the operand of a unary expression.
fn unary(expr: &Expr) -> (UnaryOp, &Expr) {
    match expr {
        Expr::Unary { op, expr, .. } => (*op, expr),
        other => unreachable!("expected a unary expression, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The line comment `- -10` used to build.
// ---------------------------------------------------------------------------

#[test]
fn unary_minus_does_not_become_a_comment() {
    // The defect: `--` starts a line comment, so the printed text stopped being a query.
    let printed = rt("SELECT - -10");
    assert_eq!(printed, "SELECT - -10");
    assert!(!printed.contains("--"), "{printed} holds a line comment");

    // The tree the loop preserves: two nested negations over the source text `10`.
    let expr = one("SELECT - -10");
    let (outer, inner) = unary(&expr);
    assert_eq!(outer, UnaryOp::Minus);
    let (inner_op, operand) = unary(inner);
    assert_eq!(inner_op, UnaryOp::Minus);
    assert!(
        matches!(operand, Expr::Literal(Literal::Integer(text), _) if text == "10"),
        "expected the literal 10, got {operand:?}"
    );
}

#[test]
fn a_glued_double_minus_is_a_comment_for_us_too() {
    // What the printed text meant before the fix: `SELECT --10` is `SELECT` alone, which
    // does not parse. SQL Server answers 102 as well (near 'SELECT'; the token we name in
    // that message is a separate matter, `tests/syntax_errors.rs`).
    let error = parse_batch("SELECT --10", &ParseOptions::default()).unwrap_err();
    assert_eq!(error.number, 102, "{error}");
}

#[test]
fn nested_negations_stay_apart_at_every_depth() {
    assert_eq!(rt("SELECT - - -10"), "SELECT - - -10");
    assert_eq!(rt("SELECT - - - -10"), "SELECT - - - -10");
    // A variable operand is no different: the sign of the operator is what matters.
    assert_eq!(rt("SELECT - -@x"), "SELECT - -@x");
}

#[test]
fn a_parenthesised_operand_needs_no_space() {
    // `Expr::Nested` writes its `(` first, so nothing can glue: the user's parentheses are
    // kept as they are and no space is invented.
    assert_eq!(rt("SELECT -(-10)"), "SELECT -(-10)");
    assert_eq!(rt("SELECT -(- 10)"), "SELECT -(-10)");
}

// ---------------------------------------------------------------------------
// The other sign operators, frozen by vectors.
// ---------------------------------------------------------------------------

#[test]
fn repeated_signs_are_spaced_and_mixed_signs_are_not() {
    // Repeated: one rule, whatever the sign. `++` and `~~` are not tokens of T-SQL, but a
    // single rule beats a table of dangerous pairs, and SQL Server accepts both forms.
    assert_eq!(rt("SELECT + +1"), "SELECT + +1");
    assert_eq!(rt("SELECT ++1"), "SELECT + +1");
    assert_eq!(rt("SELECT ~ ~1"), "SELECT ~ ~1");
    assert_eq!(rt("SELECT ~~1"), "SELECT ~ ~1");

    // Mixed: the two characters cannot lex as one token, they stay glued.
    assert_eq!(rt("SELECT - +1"), "SELECT -+1");
    assert_eq!(rt("SELECT -+1"), "SELECT -+1");
    assert_eq!(rt("SELECT + -1"), "SELECT +-1");
    assert_eq!(rt("SELECT +-1"), "SELECT +-1");
    assert_eq!(rt("SELECT ~ -1"), "SELECT ~-1");
    assert_eq!(rt("SELECT - ~1"), "SELECT -~1");
}

#[test]
fn a_single_sign_is_untouched() {
    assert_eq!(rt("SELECT -10"), "SELECT -10");
    assert_eq!(rt("SELECT +10"), "SELECT +10");
    assert_eq!(rt("SELECT ~10"), "SELECT ~10");
    assert_eq!(
        rt("SELECT 1 WHERE NOT -10 = 1"),
        "SELECT 1 WHERE NOT -10 = 1"
    );
    // A money literal keeps its own sign inside the literal, after the `$`.
    assert_eq!(rt("SELECT -$1.50"), "SELECT -$1.50");
    assert_eq!(rt("SELECT $-1.50"), "SELECT $-1.50");
}

#[test]
fn a_binary_minus_before_a_negation_was_never_glued() {
    // Binary operators are written with spaces around them, so `1 - -10` never built a
    // comment. The vector guards the day someone tightens that.
    assert_eq!(rt("SELECT 1 - -10"), "SELECT 1 - -10");
    assert_eq!(rt("SELECT 1 --10"), "SELECT 1");
    assert_eq!(rt("SELECT 1 - - -10"), "SELECT 1 - - -10");
}
