//! The one piece of position information a binding error needs: the line it is reported
//! on — and, because there is no single rule, **which node** each number takes it from.
//!
//! No message text is written here: the errors the binder raises go through the named
//! constructors of `vauban_errors`, which own the catalogue templates.
//!
//! There is no `near` helper slicing the source text a node covers: message 4145, the one
//! binder message with that placeholder, quotes the token that **follows** the expression
//! and not the expression itself (`SELECT 1 WHERE 1;` answers `near ';'`,
//! `SELECT 1 WHERE 1 AND 1 = 1;` answers `near 'AND'`), which is `expr::near_token_at`'s
//! job.
//!
//! # There is no rule: each number names its own node
//!
//! An error of **execution** carries the line its **statement** starts on, not that of the
//! sub-expression that raised it. An error of **compilation** does not, and it does not follow a
//! single rule either. The table below is stated for batches that put the node and the
//! statement on different lines; a batch of one line, or one whose node sits on the first
//! line of its statement, answers the same under both hypotheses and separates nothing:
//!
//! | error | node whose line it carries |
//! |---|---|
//! | 107, 117 | the qualified wildcard (`t.*`) |
//! | 127, 1060 | the `TOP` row count — its **literal**, not its parenthesis |
//! | 137 | the variable |
//! | 155 | the function **call**, not the option it refused |
//! | 174, 189, 195 | the function |
//! | 206 | **the statement** |
//! | 207 | the column |
//! | 208 | **the statement** |
//! | 209 | the column |
//! | 243, 529 | the `CAST` / `CONVERT` node, neither its operand nor its type name |
//! | 257 | **the statement** — though it is raised by the same call as 402 and 8117 |
//! | 263 | **the statement** |
//! | 402, 8117 | the operator |
//! | 447, 448 | the collation name |
//! | 1031 | the first line of the select list |
//! | 1062 | the **last** token of the statement, its `;` included |
//! | 4104 | the identifier |
//! | 4121, 4127, 4151 | **the statement** |
//! | 4145 | the token the message quotes in `near '…'` |
//! | 8116 | **the statement** |
//!
//! Nothing observable separates the two families; each hypothesis that would turn the table
//! into a formula has a counter-example:
//!
//! - it is not the construct. On one `a + b` (`SELECT` on line 2, left operand on 3,
//!   operator on 4), `CAST(1 AS bit) + CAST(1 AS bit)` answers **4** (402, the operator)
//!   and `NEWID() + 1` answers **2** (206, the statement). Same node, same operator, two
//!   different lines.
//! - nor is it the site that raises it. `binary_op_type` is **one call**, and its four
//!   numbers do not agree: on `SELECT` (2) / operand (3) / operator (4) / operand (5),
//!   `CAST(1 AS bit) + CAST(1 AS bit)` answers **4** (402) and
//!   `CAST('2020-01-01' AS datetime) * 2` answers **2** (257). One `map_err`, two lines.
//! - nor is it the clause or the check's family. On one call
//!   (`SELECT` on 2, the call on 5), `LEN()` answers **5** (174, the function) and
//!   `SUBSTRING(CAST('12:00' AS time), 1, 2)` answers **2** (8116, the statement): the
//!   number of arguments names the function, the type of an argument names the statement.
//! - nor the severity: the statement family is severity 16, but so are 207, 243, 402, 447,
//!   448, 529, 4104 and 8117, which name a node; and 127, 137, 174, 189, 195, 1031, 1062 and
//!   4145 name a node at severity 15.
//! - nor "the batch sends no result set": the batches of the table above send zero, on both
//!   sides of the split.
//!
//! So this module carries a **table**, [`NAMES_THE_STATEMENT`], and not a formula.

use vauban_errors::SqlError;
use vauban_parser::Span;

/// The numbers whose line is the one their **statement** starts on, and not their node's.
///
/// Each batch below puts the failing node several lines below its `SELECT`; with a
/// `SELECT 1;` on line 2 and the failing `SELECT` on line 3, the same numbers answer **3**,
/// which tells "the statement" apart from "the batch".
///
/// | number | batch (`SELECT` on line 2) | answer |
/// |---|---|:-:|
/// | 206 | `SELECT` / `NEWID()` / `+` / `1;` | **2** |
/// | 208 | `SELECT` / `1` / `FROM` / `nosuch;` | **2** |
/// | 257 | `SELECT` / `CAST('2020-01-01' AS datetime)` / `*` / `2;` | **2** |
/// | 263 | `SELECT` / `1` / `,` / `*;` | **2** |
/// | 4121 | `SELECT` / `1` / `,` / `dbo.no_such_fn(1);` | **2** |
/// | 4127 | `SELECT` / `1` / `,` / `COALESCE(NULL,` / `NULL);` | **2** |
/// | 4151 | `SELECT` / `1` / `,` / `NULLIF(NULL,` / `1);` | **2** |
/// | 8116 | `SELECT` / `1` / `,` / `SUBSTRING(CAST('12:00' AS time),` / `1,` / `2);` | **2** |
///
/// 4127 is listed but not raised yet: `COALESCE(NULL, NULL)` is not diagnosed.
///
/// **257 is the one that is easy to miss.** It comes out of the very `binary_op_type` call
/// that raises 402 and 8117, which name the operator, so the family of a number cannot be
/// read off the code that raises it: the same `map_err` feeds both sides of this list.
/// `smalldatetime` answers the same as `datetime`, `/` the same as `*`, and the operand
/// order does not matter — `2 * CAST('2020-01-01' AS datetime)` also answers 2. Its state is
/// 3, where 206's is 2, which is one more thing that does not separate the two lists.
///
/// **208 is here and its neighbour 209 is not.** With `SELECT` on line 4, `1` on 5, `FROM`
/// on 6 and `nosuch` on 7, the 208 carries line **4**; with `SELECT` on 4 and an ambiguous
/// `a` on 5, the 209 carries line **5**, and it follows the ambiguous name into a `WHERE`
/// five lines further down (**8**). Same clause, same batch shape, two different lines
/// (`a_208_takes_the_line_of_its_statement_and_a_209_that_of_its_column`).
///
/// Those two numbers also split the batch in two, which is a second thing this list does not
/// say and `session` does: with `SELECT 1;` and `SELECT 2;` before the failing statement, a
/// 208 lets both rows through where a 207, a 209 or a 4104 stops them.
///
/// A number joins this array with a batch whose node and statement are on different lines.
/// The mirror of this list in `session` (`batch.rs`, `COMPILED_WITH_THE_STATEMENT`) carries
/// the same warning for the opposite reason.
pub(crate) const NAMES_THE_STATEMENT: [u32; 8] = [206, 208, 257, 263, 4121, 4127, 4151, 8116];

/// The 1-based line a node starts on, as `SqlError::line` and `BoundExpr::line` count them.
///
/// `parser::Span` already counts lines from 1; this function exists so that the convention
/// is stated in one place rather than assumed at every call site.
pub(crate) fn line_of(span: &Span) -> u32 {
    span.line
}

/// Puts the line of the statement on the errors of [`NAMES_THE_STATEMENT`], over the line
/// of the node the raising site knew about.
///
/// Overriding rather than filling is the point: each raising site of the binder answers a
/// line, and for the numbers of that list that line is the right one when the statement
/// fits on one line and wrong otherwise. `line` is 0 when the caller has no line to offer, and the error is then left
/// alone.
pub(crate) fn on_the_statement(err: SqlError, line: u32) -> SqlError {
    if line == 0 || !NAMES_THE_STATEMENT.contains(&err.number) {
        return err;
    }
    err.with_line(line)
}

/// The 1-based line of the byte at `offset`, counted forward from the start of `from`.
///
/// `from.line` is the line `from` starts on, so the answer is that line plus the newlines
/// between the two positions. An `offset` that does not land on a character boundary of
/// `text`, or that falls before `from`, yields `from.line`: a wrong line is bad, a panic on
/// the query path is worse.
pub(crate) fn line_at(text: &str, from: &Span, offset: usize) -> u32 {
    let start = from.offset as usize;
    match text.get(start..offset) {
        Some(between) => {
            let newlines = between.bytes().filter(|byte| *byte == b'\n').count();
            from.line
                .saturating_add(u32::try_from(newlines).unwrap_or(u32::MAX))
        }
        None => from.line,
    }
}

/// `text` without its leading whitespace, `--` line comments and `/* */` block comments.
///
/// This is meant to be the very trivia the lexer drops, so that the positions computed on
/// top of it land on the tokens the parser saw: `SELECT` / `1` / `WHERE` / `1` /
/// `-- a comment` / `;` answers 4145 near `;` on the line of the `;` (7), not on the
/// comment's.
///
/// **This is a second reader of the batch text, and what keeps it honest is that it copies
/// [`vauban_parser`]'s rules by hand.** T-SQL block comments **nest** (`lexer.rs`: the
/// scanner counts levels), so closing a comment at the first `*/` would leave
/// `/* outer /* inner` then `*/ still outer */` between two operands pointing at `still`, a
/// word inside a comment, one line short. With the `SELECT` on line 2, the left operand on
/// line 3 and the comment filling the lines between it and the `+`:
///
/// | comment between the operands | first `*/` | closing `*/` | line of the 402 |
/// |---|:-:|:-:|:-:|
/// | `/* outer /* inner` / `*/ still outer */` on 4-5 | 5 | 5 | **6** |
/// | `/* outer /* inner */ still outer */` on 4 | 4 | 4 | **5** |
/// | two such comments on 4-5 and 6-7 | 5 | 7 | **8** |
///
/// The same three shapes move 8117, 4145 — whose `near '…'` would otherwise quote `still`
/// instead of `;` — and 1062, through [`trivia_len`]
/// (`trivia_counts_the_levels_of_a_nested_block_comment`).
///
/// 402 and 8117 read `Expr::Binary::op_span`, 447 and 448 read
/// `Expr::Collate::collation_span`; two callers of this reader are left, and what each of
/// them points at is not a node of the AST:
///
/// - 4145 quotes the token that **follows** a condition (`expr::near_token_at`), a token of
///   the clause or of the batch and not of the expression the AST holds;
/// - 1062 counts the `;` that `parse_batch` eats after the statement
///   ([`statement_last_line`]), which is therefore outside the statement's span.
///
/// Both would need a position the parser does not keep yet — a change to `parse_batch` and
/// to the statement nodes.
pub(crate) fn skip_trivia(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after
                .find('\n')
                .map_or("", |i| &after[i + 1..])
                .trim_start();
        } else if rest.starts_with("/*") {
            rest = skip_block_comment(rest).trim_start();
        } else {
            return rest;
        }
    }
}

/// `text`, which starts with `/*`, from just past the `*/` that closes that comment.
///
/// Counts levels, because T-SQL block comments nest — the same loop as
/// `vauban_parser`'s `lexer::skip_block_comment`, byte for byte in its decisions. A
/// comment left unclosed at the end of the text eats the rest of it, as the lexer does
/// (the lexer raises no error there either).
fn skip_block_comment(text: &str) -> &str {
    let mut rest = &text[2..];
    let mut depth = 1usize;
    while depth > 0 {
        if let Some(after) = rest.strip_prefix("/*") {
            depth += 1;
            rest = after;
        } else if let Some(after) = rest.strip_prefix("*/") {
            depth -= 1;
            rest = after;
        } else {
            match rest.chars().next() {
                Some(character) => rest = &rest[character.len_utf8()..],
                None => return rest,
            }
        }
    }
    rest
}

/// The number of leading bytes of `text` that are trivia (see [`skip_trivia`]).
pub(crate) fn trivia_len(text: &str) -> usize {
    text.len() - skip_trivia(text).len()
}

/// The last line of a statement, its terminating `;` **included**: error 1062.
///
/// A `;` is a separator the batch loop eats after the statement (`parser`, `parse_batch`),
/// so it is not in the statement's span; SQL Server counts it nonetheless. With the
/// `SELECT` on line 2 and `TOP (1) WITH TIES` on line 3
/// (`statement_last_line_counts_the_terminator_and_nothing_after_it`):
///
/// | batch | last token of the span | `;` | line of the 1062 |
/// |---|:-:|:-:|:-:|
/// | `1` / `,` / `2;` | 6 | 6 | **6** |
/// | `1` / `,` / `2` / `,` / `3;` | 8 | 8 | **8** |
/// | `1` / `WHERE` / `1 = 1;` | 6 | 6 | **6** — the statement's end, not the select list's (4) |
/// | `1` / `;` | 4 | 5 | **5** — the `;` counts |
/// | `1;` / `-- a trailing comment` | 4 | 4 | **4** — a comment after it does not |
/// | `1;` / `SELECT` / `2;` | 4 | 4 | **4** — nor does the next statement |
/// | `1` (no `;` at all) | 4 | — | **4** |
/// | `1` / `;` / `;` | 4 | 5, 6 | **5** — one terminator, not two |
pub(crate) fn statement_last_line(text: &str, span: &Span) -> u32 {
    let end = (span.offset as usize).saturating_add(span.len as usize);
    let terminated = match text.get(end..) {
        Some(rest) => {
            let skipped = trivia_len(rest);
            if rest[skipped..].starts_with(';') {
                end.saturating_add(skipped).saturating_add(1)
            } else {
                end
            }
        }
        None => end,
    };
    line_at(text, span, terminated)
}

#[cfg(test)]
mod tests {
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
}
