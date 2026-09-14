//! Block comments `/* … */`, as T-SQL reads them.
//!
//! What this file adds to `lex_comments` are two vectors: an opener that sits inside
//! *quoted text* within a comment, and an opener that sits inside a *string literal*
//! outside any comment.

use vauban_parser::{Expr, Literal, ParseOptions, QueryBody, SelectItem, Statement, parse_batch};

/// The classic nesting example, without `GO` separators: one batch, one extra `*/`.
///
/// `GO` is a client-side word, so the two scripts are two texts of one batch each.
const WITH_WORKAROUND: &str = "/*\nSELECT @comment = '/*';\n*/\n*/\nSELECT 1";

/// The same text without the extra `*/`: the `'/*'` opened a level nothing closes.
const WITHOUT_WORKAROUND: &str = "/*\nSELECT @comment = '/*';\n*/\nSELECT 1";

/// The `/*` written inside quoted text opens a comment level, so one `*/` is not enough.
///
/// This pair is what separates "count the levels" from "close at the first `*/`". Under the
/// second rule the two texts would swap roles: [`WITHOUT_WORKAROUND`] would end its comment
/// at the first `*/` and yield the `SELECT 1`, while the trailing `*/` of
/// [`WITH_WORKAROUND`] would be stray text. On this shape the quotes carry no weight, which
/// extends what `lex_comments` already shows on `SELECT /* it's fine */ 1`: there a
/// lone `'` inside a comment opens no string, here a quoted `/*` inside a comment still opens
/// a level.
///
/// On SQL Server, [`WITH_WORKAROUND`] returns a single row `1`, and [`WITHOUT_WORKAROUND`]
/// returns no result set and error 113, severity 15, state 1. The assertion below stays
/// on what both answers share -- no statement comes out -- and accepts an error of any
/// number.
#[test]
fn learn_nested_opener_inside_quoted_text() {
    let batch = parse_batch(WITH_WORKAROUND, &ParseOptions::default())
        .expect("the extra `*/` closes the comment and leaves a statement");
    assert_eq!(batch.statements.len(), 1, "{batch}");
    assert_eq!(batch.to_string(), "SELECT 1");

    // Today the unterminated comment swallows the `SELECT 1` and the batch comes back
    // empty. An `Err` is accepted on purpose, whatever its number: this assertion must
    // not pull the lexer back to silence on an unterminated comment.
    if let Ok(batch) = parse_batch(WITHOUT_WORKAROUND, &ParseOptions::default()) {
        assert!(
            batch.statements.is_empty(),
            "the missing `*/` must not leave a statement, got `{batch}`"
        );
    }
}

/// Outside a comment, `/*` inside a string literal is text, not an opener.
///
/// The mirror of the test above: `lex_strings` (`lexer.rs`) already shows that `--`
/// inside a string opens nothing, and this is the `/*` half of that statement. SQL Server
/// answers one `varchar` column whose only row is `/* x */`.
#[test]
fn slash_star_inside_a_string_is_not_a_comment() {
    let batch = parse_batch("SELECT '/* x */'", &ParseOptions::default())
        .expect("`/*` inside a string literal is not a comment");
    assert_eq!(batch.statements.len(), 1, "{batch}");
    let Statement::Select(select) = &batch.statements[0] else {
        panic!("expected a SELECT, got `{batch}`");
    };
    let QueryBody::Select(spec) = &select.body else {
        panic!("expected a query specification, got `{batch}`");
    };
    let [SelectItem::Expr { expr, .. }] = spec.items.as_slice() else {
        panic!("expected one select item, got `{batch}`");
    };
    let Expr::Literal(Literal::Str { value, unicode }, _) = expr else {
        panic!("expected a string literal, got `{expr}`");
    };
    assert_eq!(value, "/* x */");
    assert!(!unicode);
}
