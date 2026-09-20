use super::{
    NAMES_THE_STATEMENT, line_at, line_of, on_the_statement, skip_trivia, statement_last_line,
    trivia_len,
};
use vauban_errors::SqlError;
use vauban_parser::Span;

fn span(offset: u32, len: u32) -> Span {
    Span {
        line: 1,
        column: offset.saturating_add(1),
        offset,
        len,
    }
}

/// A span over `text` starting at `at`, running `len` bytes, on the line `at` falls on.
fn span_in(text: &str, at: usize, len: usize) -> Span {
    let line = 1 + text[..at].bytes().filter(|b| *b == b'\n').count() as u32;
    Span {
        line,
        column: 1,
        offset: at as u32,
        len: len as u32,
    }
}

#[test]
fn line_of_is_the_span_line() {
    assert_eq!(line_of(&span(0, 1)), 1);
    assert_eq!(
        line_of(&Span {
            line: 4,
            column: 2,
            offset: 30,
            len: 1,
        }),
        4
    );
}

#[test]
fn line_at_counts_the_newlines_from_the_span() {
    let text = "SELECT\n1\n+\n2";
    let from = span_in(text, 7, 1); // the `1`, line 2
    assert_eq!(from.line, 2);
    assert_eq!(line_at(text, &from, 7), 2);
    assert_eq!(line_at(text, &from, 9), 3); // the `+`
    assert_eq!(line_at(text, &from, 11), 4); // the `2`
    // An offset outside the text, or before the span, keeps the span's line.
    assert_eq!(line_at(text, &from, 999), 2);
    assert_eq!(line_at(text, &from, 0), 2);
}

#[test]
fn trivia_is_whitespace_and_both_kinds_of_comment() {
    assert_eq!(skip_trivia("  \n+ 1"), "+ 1");
    assert_eq!(skip_trivia("\n-- a comment\n+ 1"), "+ 1");
    assert_eq!(skip_trivia(" /* a\nb */ + 1"), "+ 1");
    assert_eq!(skip_trivia("+ 1"), "+ 1");
    assert_eq!(trivia_len("\n-- c\n+"), 6);
    assert_eq!(trivia_len("+"), 0);
}

/// Block comments nest, as `vauban_parser`'s lexer counts them: the first `*/` closes
/// the inner comment only, and `still` is a word inside a comment, not a token.
#[test]
fn trivia_counts_the_levels_of_a_nested_block_comment() {
    assert_eq!(
        skip_trivia("/* outer /* inner\n*/ still outer */\n+ 1"),
        "+ 1"
    );
    assert_eq!(
        skip_trivia("/* outer /* inner */ still outer */ + 1"),
        "+ 1"
    );
    assert_eq!(
        skip_trivia("/* a /* b\n*/ c */\n/* d /* e\n*/ f */\n+ 1"),
        "+ 1"
    );
    // Three levels, and a `/*` that opens no comment because it is the tail of a `*/`.
    assert_eq!(skip_trivia("/* a /* b /* c */ d */ e */+"), "+");
    // A line comment inside a block comment closes nothing.
    assert_eq!(skip_trivia("/* a -- b */\n+"), "+");
    // An unclosed comment eats the rest of the text, as the lexer does.
    assert_eq!(skip_trivia("/* a /* b */"), "");
    assert_eq!(skip_trivia("/* a"), "");
    // The bytes counted are the whole comment, not up to its first `*/`.
    assert_eq!(trivia_len("/* a /* b */ c */+"), 17);
}

/// The eight rows of `statement_last_line`'s table.
///
/// The batch opens with a comment line, so that the statement starts on line 2.
#[test]
fn statement_last_line_counts_the_terminator_and_nothing_after_it() {
    let cases = [
        ("SELECT\nTOP (1) WITH TIES\n1\n,\n2", ";", "", 6u32),
        ("SELECT\nTOP (1) WITH TIES\n1\n,\n2\n,\n3", ";", "", 8),
        ("SELECT\nTOP (1) WITH TIES\n1\nWHERE\n1 = 1", ";", "", 6),
        ("SELECT\nTOP (1) WITH TIES\n1", "\n;", "", 5),
        ("SELECT\nTOP (1) WITH TIES\n1", ";", "\n-- trailing", 4),
        ("SELECT\nTOP (1) WITH TIES\n1", ";", "\nSELECT\n2;", 4),
        ("SELECT\nTOP (1) WITH TIES\n1", "", "\n", 4),
        ("SELECT\nTOP (1) WITH TIES\n1", "\n;", "\n;", 5),
    ];
    for (statement, terminator, after, expected) in cases {
        let head = "-- head\n";
        let text = format!("{head}{statement}{terminator}{after}");
        let stmt = span_in(&text, head.len(), statement.len());
        assert_eq!(statement_last_line(&text, &stmt), expected, "in {text:?}");
    }
}

#[test]
fn on_the_statement_moves_the_listed_numbers_and_leaves_the_others() {
    for number in NAMES_THE_STATEMENT {
        let moved = on_the_statement(SqlError::new(number, 16, 1, "x").with_line(7), 2);
        assert_eq!(moved.line, 2, "error {number} names its statement");
    }
    // A number that names a node keeps the line its site gave it.
    for number in [127u32, 137, 195, 207, 402, 447, 1062, 4145, 8117] {
        let kept = on_the_statement(SqlError::new(number, 15, 1, "x").with_line(7), 2);
        assert_eq!(kept.line, 7, "error {number} names its own node");
    }
    // No line to offer: the error is left alone.
    let kept = on_the_statement(SqlError::new(206, 16, 2, "x").with_line(7), 0);
    assert_eq!(kept.line, 7);
}

/// 208 and 209 come out of the same clause and do not carry the same line: the 208
/// carries the `SELECT`'s line (4), the 209 the column's (5).
#[test]
fn a_208_takes_the_line_of_its_statement_and_a_209_that_of_its_column() {
    let statement_line = 4;
    let node_line = 5;
    let moved = on_the_statement(
        SqlError::new(208, 16, 1, "Unknown object name 'nosuch'.").with_line(node_line),
        statement_line,
    );
    assert_eq!(moved.line, statement_line);
    let kept = on_the_statement(
        SqlError::new(209, 16, 1, "Column name 'a' is ambiguous.").with_line(node_line),
        statement_line,
    );
    assert_eq!(kept.line, node_line);
}
