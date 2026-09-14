//! Two positions of the AST: the **operator** token of a binary operation
//! (`Expr::Binary::op_span`) and the **collation name** of a `COLLATE`
//! (`Expr::Collate::collation_span`).
//!
//! Both exist for the binder, which reports errors on them: 402 and 8117 on the operator,
//! 447 and 448 on the collation name. What this file checks is that the AST puts the span
//! on the token SQL Server reports those errors on, comments included.
//!
//! Each assertion is made twice: on the **line** the span carries, and on the source text
//! the span cuts out. The second one is what the text reader got wrong — inside
//! `/* outer /* inner */ still outer */` it cut out `still`, a word in a comment.

use vauban_parser::{Expr, ParseOptions, QueryBody, SelectItem, Span, Statement, parse_batch};

/// The operator span and the whole-operation span of the one binary operation `text`
/// holds, taken from the `WHERE` when there is one and from the first select item
/// otherwise.
fn binary_spans(text: &str) -> (Span, Span) {
    let batch = parse_batch(text, &ParseOptions::default()).expect("a batch that parses");
    let Some(Statement::Select(select)) = batch.statements.first() else {
        unreachable!("{text} is one SELECT")
    };
    let QueryBody::Select(spec) = &select.body else {
        unreachable!("{text} is one query specification")
    };
    let expr = match &spec.where_ {
        Some(condition) => condition,
        None => match spec.items.first() {
            Some(SelectItem::Expr { expr, .. }) => expr,
            other => unreachable!("{text} has one expression item, got {other:?}"),
        },
    };
    let Expr::Binary { op_span, span, .. } = expr else {
        unreachable!("{text} is one binary operation, got {expr:?}")
    };
    (*op_span, *span)
}

/// The collation name span of the one `COLLATE` of the first select item of `text`.
fn collation_span(text: &str) -> Span {
    let batch = parse_batch(text, &ParseOptions::default()).expect("a batch that parses");
    let Some(Statement::Select(select)) = batch.statements.first() else {
        unreachable!("{text} is one SELECT")
    };
    let QueryBody::Select(spec) = &select.body else {
        unreachable!("{text} is one query specification")
    };
    let Some(SelectItem::Expr { expr, .. }) = spec.items.first() else {
        unreachable!("{text} has one expression item")
    };
    let Expr::Collate { collation_span, .. } = expr else {
        unreachable!("{text} is one COLLATE, got {expr:?}")
    };
    *collation_span
}

/// The source text a span cuts out.
fn cut<'a>(text: &'a str, span: &Span) -> &'a str {
    let start = span.offset as usize;
    &text[start..start + span.len as usize]
}

/// Seven shapes: the operator below both operands, on the left operand's line, on the
/// right operand's line, behind a line comment, behind a block comment, and behind a
/// nested block comment written on two lines and on one.
///
/// The batch opens with a comment line, so that the lines asserted here are the ones a
/// client that sends a leading comment sees.
#[test]
fn operator_span_is_the_token_between_the_operands() {
    let cases = [
        ("1\n+\n2", 4u32),
        ("1 +\n2", 3),
        ("1\n+ 2", 4),
        ("1\n-- c\n+\n2", 5),
        ("1 /* c\nc */ +\n2", 4),
        ("1\n/* o /* i\n*/ o */\n+\n2", 6),
        ("1\n/* o /* i */ o */\n+\n2", 5),
    ];
    for (tail, expected) in cases {
        let text = format!("-- leading comment\nSELECT\n{tail};");
        let (op, whole) = binary_spans(&text);
        assert_eq!(op.line, expected, "operator line in {text:?}");
        assert_eq!(cut(&text, &op), "+", "operator token in {text:?}");
        // The whole operation starts at the left operand, on line 3 in each shape: the
        // operator's line is a position of its own, not a copy of the node's.
        assert_eq!(whole.line, 3, "operation line in {text:?}");
    }
}

/// A nested block comment is skipped whole. Its first `*/` closes the inner comment only,
/// so the word that follows it (`still`) is inside a comment and not a token — which is
/// what a reader that stops at the first `*/` would cut out.
#[test]
fn operator_span_skips_a_nested_block_comment_whole() {
    let text = "-- leading comment\nSELECT\n1\n/* outer /* inner */ still outer */\n+\n2;";
    let (op, _) = binary_spans(text);
    assert_eq!(op.line, 5);
    assert_eq!(cut(text, &op), "+");
    assert_eq!(&text[op.offset as usize..], "+\n2;");
}

/// Operators wider than one character, and two written with a keyword: the span covers the
/// token the lexer read at each of the four widths tried here.
#[test]
fn operator_span_covers_the_whole_operator_token() {
    for (condition, token, line) in [
        ("1\n<>\n2", "<>", 4u32),
        ("1\n!=\n2", "!=", 4),
        ("1 = 1\nAND\n1 = 1", "AND", 4),
        ("1 = 1\nOR\n1 = 1", "OR", 4),
    ] {
        let text = format!("-- leading comment\nSELECT 1 WHERE\n{condition};");
        let (op, _) = binary_spans(&text);
        assert_eq!(cut(&text, &op), token, "operator token in {text:?}");
        assert_eq!(op.line, line, "operator line in {text:?}");
    }
}

/// The collation name, which 447 and 448 are reported on: neither the `COLLATE` keyword's
/// line nor the end of the expression.
#[test]
fn collation_span_is_the_name_and_not_the_keyword() {
    let text = "-- leading comment\nSELECT\n'a'\nCOLLATE\nLatin1_General_CI_AS;";
    let name = collation_span(text);
    assert_eq!(name.line, 5);
    assert_eq!(cut(text, &name), "Latin1_General_CI_AS");
}

/// A collation name written between brackets keeps them in its span: the span is the token
/// the lexer read, and its line is the one the name is written on.
#[test]
fn collation_span_of_a_quoted_name() {
    let text = "-- leading comment\nSELECT 'a'\nCOLLATE\n[Latin1_General_CI_AS];";
    let name = collation_span(text);
    assert_eq!(name.line, 4);
    assert_eq!(cut(text, &name), "[Latin1_General_CI_AS]");
}
