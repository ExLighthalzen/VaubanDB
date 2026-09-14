//! Syntax errors 102, 105 and 156, built from the offending token.
//!
//! `Parser::error_here` calls [`at_token`], so that every grammar rule builds its syntax
//! errors through a single function (`tests/syntax_errors.rs`). This file chooses between
//! 102 and 156, frames the printed text, and handles the end of the batch.
//!
//! # The four rules
//!
//! Each rule below follows what SQL Server answers, with the text that shows it.
//!
//! 1. **102 or 156**: 156 when the unexpected token is a **reserved** keyword
//!    (`SELECT FROM t` -> `Syntax error near the keyword 'FROM'.`), 102 for anything
//!    else. [`crate::keyword::Keyword::is_reserved`] is the only criterion: a keyword that
//!    is not reserved is an ordinary word, and a misplaced `OFFSET` gives a 102 that
//!    prints it like any other, because `OFFSET` is not in the reserved table
//!    (`OFFSETS` is).
//! 2. **What is printed**: [`Token::text`], the exact slice of batch text, case included
//!    (`SELECT 1 x @V` -> `'@V'`, `SELECT 1 x $1.50` -> `'$1.50'`, `SELECT 1 x 1E3` ->
//!    `'1E3'`) -- except for the two families [`printed_text`] documents.
//! 3. **The line**: the line of the offending **token**, not the line the statement starts
//!    at: `tests/syntax_errors.rs` `error_line_is_the_token_line` puts `SELEC` on the
//!    fourth line of the batch, comment lines included, and that is the line reported.
//! 4. **The end of the text** is a case of its own, and not a case of rule 1: see
//!    [`at_end`].
//!
//! Error 105 does not come from here: the lexer raises it before a token exists
//! (`SqlError::unclosed_quotation_mark`), and `parse_batch` only lets it through.
//!
//! # Never a `format!`
//!
//! No message is built here. The three constructors of `vauban-errors`
//! read the catalogue, which is what makes number, severity and state (15 and 1 for all
//! three) come from one place. `grep "Syntax error" crates/vauban-parser/src/`
//! must stay empty.

use vauban_errors::SqlError;

use crate::token::{Token, TokenKind};

/// The syntax error to report at `index` in `tokens`.
///
/// The single entry point of the module, and what `Parser::error_here` calls: it is the
/// cursor position, not the token alone, that says whether the batch simply stopped
/// ([`at_end`], always a 102) or whether a token was refused where it stands
/// ([`at_token`], a 156 when it is reserved).
pub(crate) fn at_cursor(tokens: &[Token], index: usize) -> SqlError {
    let token = offending_token(tokens, index);
    if text_ended_at(tokens, index) {
        at_end(token)
    } else {
        at_token(token)
    }
}

/// The syntax error SQL Server reports on a token it refuses **where it stands**.
///
/// Error 156 when `token` is a reserved keyword, error 102 otherwise; both name what
/// [`printed_text`] prints and carry the line the token starts at.
///
/// The end of the batch never comes here, because a single token cannot tell what came
/// before it: [`at_cursor`] sorts the two paths out.
fn at_token(token: &Token) -> SqlError {
    let printed = printed_text(token);
    let line = token.span.line;
    match token.kind {
        TokenKind::Keyword(keyword) if keyword.is_reserved() => {
            SqlError::incorrect_syntax_near_keyword(&printed, line)
        }
        _ => SqlError::incorrect_syntax_near(&printed, line),
    }
}

/// The syntax error SQL Server reports when the batch text simply **stops**.
///
/// Always a **102**, never a 156, even when the last token read is a reserved word, and it
/// names that last token, on **its** line (`tests/syntax_errors.rs`
/// `error_at_end_of_input`), each of the texts below a 102:
///
/// | text | message |
/// |---|---|
/// | `SELECT` | `near 'SELECT'.` |
/// | `SELECT 1 AS` | `near 'AS'.` |
/// | `SELECT DISTINCT` | `near 'DISTINCT'.` |
/// | `SELECT * FROM` | `near 'FROM'.` |
/// | `SELECT 1 WHERE` | `near 'WHERE'.` |
/// | `SELECT 1 UNION` | `near 'UNION'.` |
/// | `SELECT 1 GROUP` | `near 'GROUP'.` |
/// | `SELECT 1 ORDER` | `near 'ORDER'.` |
/// | `SELECT 1 INTO` | `near 'INTO'.` |
/// | `SELECT TOP` | `near 'TOP'.` |
/// | `SELECT 1 +` | `near '+'.` |
/// | `SELECT 1\n\nFROM` | `near 'FROM'.`, **line 3** |
///
/// The rule the two columns give: 156 is for a reserved word that is **present and
/// unexpected**, 102 for a text that ran out. `SELECT 1 GROUP BY 2 GROUP` shows the
/// difference is real and not merely "the last token": the trailing `GROUP` is refused on
/// sight, after a complete clause, and gives a **156** -- while the `GROUP` of
/// `SELECT 1 GROUP` was accepted as the head of a clause whose `BY` never came, and gives
/// a 102.
fn at_end(token: &Token) -> SqlError {
    SqlError::incorrect_syntax_near(&printed_text(token), token.span.line)
}

/// The same error as [`at_cursor`], for a rule that knows what it was expecting.
///
/// `expected` never reaches the client: SQL Server does not say what it expected, it only
/// names the token it choked on. It is there for a rule that wants to state its intent at
/// the call site, and for a trace that is not written yet, because `vauban-parser` has no
/// `tracing` dependency.
#[allow(dead_code)] // kept for the grammar rules that want to state their intent
pub(crate) fn at_cursor_expecting(
    tokens: &[Token],
    index: usize,
    expected: &'static str,
) -> SqlError {
    let _ = expected;
    at_cursor(tokens, index)
}

/// What the message prints for `token`.
///
/// Usually [`Token::text`], the source slice, case included. Two families depart from it:
///
/// - a **character string** (`'…'`), a **national string** (`N'…'`) or a **delimited
///   identifier** (`[…]`, `"…"`) prints its **value**: delimiters gone, doubled
///   delimiters reduced, `N` prefix dropped -- and the inner quote is **not** escaped
///   again. `SELECT 1 'a' 'b''c'` -> `near 'b'c'.`, `SELECT 1 x N'a''b'` -> `near 'a'b'.`,
///   `SELECT 1 [x] [a]]b]` -> `near 'a]b'.`, `SELECT 1 'a' "b""c"` -> `near 'b"c'.`,
///   `SELECT 1 'a' ''` -> `near ''.`. The case of the value is kept
///   (`SELECT 1 x [Mixed CASE]` -> `near 'Mixed CASE'.`).
///   In `SELECT 1 'a''b' 'c'` the offending token is the **second** literal, not the
///   first: a character string is a legal alias in T-SQL, so the parse gets past it.
/// - a **binary literal** is lower-cased whole, prefix included: `SELECT 1 x 0x1F` and
///   `SELECT 1 x 0X1f` both give `near '0x1f'.`.
///
/// A **character string** is then cut to [`PRINTED_LIMIT`] characters; the other kinds
/// are printed whole.
fn printed_text(token: &Token) -> String {
    match &token.kind {
        // `value` is what the lexer already unescaped; a regular identifier has no
        // delimiter to drop, so its source slice is its value and stays untouched.
        TokenKind::Ident {
            value,
            quoted: true,
        } => value.clone(),
        TokenKind::Str { value, .. } => cut(value),
        TokenKind::Binary(_) => token.text.to_lowercase(),
        _ => token.text.clone(),
    }
}

/// The first [`PRINTED_LIMIT`] characters of `text`, or `text` when it is shorter.
fn cut(text: &str) -> String {
    match text.char_indices().nth(PRINTED_LIMIT) {
        Some((end, _)) => text.get(..end).unwrap_or(text).to_owned(),
        None => text.to_owned(),
    }
}

/// How many **characters** of a character string a 102 prints before cutting it.
///
/// On `SELECT 1 'x' '<n times a>'`, the message carries 127 letters for n = 127, 128 for
/// n = 128, and 129 for n = 129, 130, 131, 140, 200 and 300. The cut counts characters,
/// not bytes: the same query with 200 times `é` -- two bytes each, `N'…'` and `'…'`
/// alike -- prints 129 of them (`a_long_string_is_cut_at_129_characters` below).
///
/// **The cut stops there.** A *binary literal* is printed whole:
/// `SELECT 1 x 0x<130 hexadecimal digits>` answers a 102 of 132 characters and
/// `0x<300 digits>` one of 302. A *delimited identifier*, a *variable* and a *number*
/// take another road on SQL Server, error 103 (an identifier longer than 128 characters):
/// on `SELECT 1 'x' [<n times b>]`, 102 at n = 128 and 103 at n = 129. VaubanDB answers
/// its ordinary 102 there instead, a deliberate difference.
const PRINTED_LIMIT: usize = 129;

/// Whether the cursor at `index` sits past the last real token of `tokens`.
///
/// True on the [`TokenKind::Eof`] the lexer always appends, and true past the end of the
/// slice, which a rule that walked too far could reach.
fn text_ended_at(tokens: &[Token], index: usize) -> bool {
    tokens
        .get(index)
        .is_none_or(|token| matches!(token.kind, TokenKind::Eof))
}

/// The token an error reported at `index` must actually name.
///
/// Normally `tokens[index]`. At the end of the batch there is nothing left to name --
/// [`TokenKind::Eof`] has an empty [`Token::text`] and SQL Server never prints
/// `Syntax error near ''.` for a truncated batch -- so the rule is to walk back to the
/// last token that was read and to report on it, with **its** line: `SELECT` alone gives
/// `Syntax error near 'SELECT'.`, `SELECT 1 AS` names the `AS`, `SELECT 1 +` the `+`.
///
/// A batch with no token at all keeps the `Eof`, and so prints nothing -- `parse_batch`
/// never fails on an empty batch anyway.
fn offending_token(tokens: &[Token], index: usize) -> &Token {
    let last = tokens.len().saturating_sub(1);
    let from = index.min(last);
    tokens
        .get(..=from)
        .unwrap_or_default()
        .iter()
        .rev()
        .find(|token| !matches!(token.kind, TokenKind::Eof))
        .or_else(|| tokens.get(from))
        .unwrap_or(&EMPTY_EOF)
}

/// What [`offending_token`] answers for an empty token slice, so that it never needs an
/// `unwrap`. `tokenize` always yields at least an `Eof`, so this is unreachable in
/// practice.
static EMPTY_EOF: Token = Token {
    kind: TokenKind::Eof,
    text: String::new(),
    span: crate::span::Span::EMPTY,
};

#[cfg(test)]
mod tests {
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
}
