use super::{at_cursor, at_cursor_expecting, offending_token};
use crate::lexer::tokenize;
use crate::parser::ParseOptions;
use crate::token::Token;

/// The tokens of `text`, which must lex.
fn tokens(text: &str) -> Vec<Token> {
    match tokenize(text, &ParseOptions::default()) {
        Ok(tokens) => tokens,
        Err(error) => unreachable!("{text} lexes: {error:?}"),
    }
}

/// The error reported on the first token of `text`.
fn first(text: &str) -> vauban_errors::SqlError {
    at_cursor(&tokens(text), 0)
}

#[test]
fn reserved_keyword_is_156() {
    let error = first("FROM t");
    assert_eq!(error.number, 156);
    assert_eq!(error.severity, 15);
    assert_eq!(error.state, 1);
    assert_eq!(error.message, "Syntax error near the keyword 'FROM'.");
}

#[test]
fn everything_else_is_102() {
    // A keyword that is not reserved, an identifier, a punctuation sign, a literal.
    for (text, printed) in [
        ("OFFSET x", "OFFSET"),
        ("SELEC x", "SELEC"),
        ("; x", ";"),
        ("'a''b' x", "a'b"),
        ("[x] y", "x"),
        ("\\ x", "\\"),
    ] {
        let error = first(text);
        assert_eq!(error.number, 102, "{text}");
        assert_eq!(error.severity, 15);
        assert_eq!(error.state, 1);
        assert_eq!(error.message, format!("Syntax error near '{printed}'."));
    }
}

/// The 102 cuts a **character string** at [`super::PRINTED_LIMIT`] characters, and
/// leaves a binary literal whole.
///
/// Counter-proof of the pair: 128 letters go through whole, 129 and 200 both come out
/// at 129, while 130 and 300 hexadecimal digits come out at 130 and 300.
#[test]
fn a_long_string_is_cut_at_129_characters() {
    for (written, printed) in [(127, 127), (128, 128), (129, 129), (200, 129)] {
        let text = format!("'{}' x", "a".repeat(written));
        assert_eq!(
            first(&text).message,
            format!("Syntax error near '{}'.", "a".repeat(printed)),
            "{written} letters"
        );
    }
    // The cut counts characters, not bytes: 200 two-byte letters print 129 of them.
    let text = format!("N'{}' x", "é".repeat(200));
    assert_eq!(
        first(&text).message,
        format!("Syntax error near '{}'.", "é".repeat(129))
    );
    // A binary literal keeps its digits, `0x` included.
    for digits in [130, 300] {
        let text = format!("0x{} x", "ab".repeat(digits / 2));
        assert_eq!(
            first(&text).message,
            format!("Syntax error near '0x{}'.", "ab".repeat(digits / 2)),
            "{digits} digits"
        );
    }
}

#[test]
fn the_source_spelling_is_kept() {
    assert_eq!(
        first("SeLeCt x").message,
        "Syntax error near the keyword 'SeLeCt'."
    );
    assert_eq!(first("SeLeC x").message, "Syntax error near 'SeLeC'.");
}

/// The delimited families print their value, the binary literal its lower case, the
/// rest its source slice. Vectors of [`super::printed_text`].
#[test]
fn delimited_tokens_print_their_value() {
    for (text, printed) in [
        ("'a''b' x", "a'b"),
        ("N'a''b' x", "a'b"),
        ("[a]]b] x", "a]b"),
        ("\"b\"\"c\" x", "b\"c"),
        ("'' x", ""),
        ("[Mixed CASE] x", "Mixed CASE"),
        ("0x1F x", "0x1f"),
        ("0X1f x", "0x1f"),
        ("@V x", "@V"),
        ("$1.50 x", "$1.50"),
        ("1E3 x", "1E3"),
    ] {
        assert_eq!(
            first(text).message,
            format!("Syntax error near '{printed}'."),
            "{text}"
        );
    }
}

#[test]
fn the_line_is_the_one_of_the_token() {
    let tokens = tokens("SELECT 1\n+\n,");
    assert_eq!(at_cursor(&tokens, 3).line, 3);
}

#[test]
fn expecting_says_nothing_more_to_the_client() {
    let tokens = tokens("; x");
    assert_eq!(
        at_cursor_expecting(&tokens, 0, "an expression"),
        at_cursor(&tokens, 0)
    );
}

/// At the end of the text the last token read is named, with a 102 even when it is
/// reserved. Vectors of [`super::at_end`].
#[test]
fn the_end_of_the_batch_names_the_last_token_read_with_a_102() {
    for (text, printed) in [
        ("SELECT", "SELECT"),
        ("SELECT 1 AS", "AS"),
        ("SELECT DISTINCT", "DISTINCT"),
        ("SELECT 1 +", "+"),
    ] {
        let tokens = tokens(text);
        let error = at_cursor(&tokens, tokens.len() - 1);
        assert_eq!(error.number, 102, "{text}");
        assert_eq!(
            error.message,
            format!("Syntax error near '{printed}'."),
            "{text}"
        );
    }
}

#[test]
fn a_token_that_is_not_the_end_is_kept() {
    let two = tokens("SELECT 1");
    assert_eq!(offending_token(&two, 1).text, "1");
    // Past the end, and on an empty batch: never a panic, never an `unwrap`.
    assert_eq!(offending_token(&two, usize::MAX).text, "1");
    let only_eof = tokens("");
    assert_eq!(offending_token(&only_eof, 0).text, "");
    assert_eq!(offending_token(&[], 0).text, "");
}
