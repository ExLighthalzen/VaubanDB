use vauban_errors::SqlResult;

use super::{MAX_NESTING_DEPTH, ParseOptions, Parser};
use crate::keyword::Keyword;
use crate::token::{Punct, TokenKind};

/// Builds a cursor over `text` with the default options.
fn cursor(text: &str) -> Parser<'static> {
    // `OPTIONS` is a `static`, so the borrow lives as long as the test needs it.
    static OPTIONS: ParseOptions = ParseOptions {
        quoted_identifier: true,
    };
    match Parser::new(text, &OPTIONS) {
        Ok(parser) => parser,
        Err(error) => unreachable!("the test text lexes: {error:?}"),
    }
}

#[test]
fn parse_options_default() {
    assert!(ParseOptions::default().quoted_identifier);
}

#[test]
fn cursor_walks_tokens() {
    let mut p = cursor("SELECT 1");
    assert_eq!(p.peek().kind, TokenKind::Keyword(Keyword::Select));
    assert_eq!(p.advance().kind, TokenKind::Keyword(Keyword::Select));
    assert_eq!(p.peek().kind, TokenKind::Integer);
    assert_eq!(p.advance().kind, TokenKind::Integer);
    assert_eq!(p.peek().kind, TokenKind::Eof);
    assert!(p.at_eof());
    // `Eof` is handed out again and again, and the index stays in bounds.
    let at_end = p.mark();
    assert_eq!(p.advance().kind, TokenKind::Eof);
    assert_eq!(p.advance().kind, TokenKind::Eof);
    assert_eq!(p.mark(), at_end);
    assert_eq!(p.peek().kind, TokenKind::Eof);
}

#[test]
fn cursor_peek_at_reads_ahead() {
    let p = cursor("SELECT 1");
    assert_eq!(p.peek_at(0).kind, TokenKind::Keyword(Keyword::Select));
    assert_eq!(p.peek_at(1).kind, TokenKind::Integer);
    assert_eq!(p.peek_at(2).kind, TokenKind::Eof);
    // Far past the end, and past `usize::MAX` too.
    assert_eq!(p.peek_at(99).kind, TokenKind::Eof);
    assert_eq!(p.peek_at(usize::MAX).kind, TokenKind::Eof);
    assert_eq!(p.mark(), 0);
}

#[test]
fn cursor_eat_keyword() {
    let mut p = cursor("SELECT 1");
    assert!(p.eat_keyword(Keyword::Select));
    assert_eq!(p.mark(), 1);
    assert!(!p.eat_keyword(Keyword::From));
    assert_eq!(p.mark(), 1);
}

#[test]
fn cursor_eat_punct() {
    let mut p = cursor("(1)");
    assert!(!p.eat_punct(Punct::RightParen));
    assert!(p.eat_punct(Punct::LeftParen));
    assert_eq!(p.mark(), 1);
}

#[test]
fn cursor_expect_keyword_errors() {
    let mut p = cursor("SELECT 1");
    p.advance();
    // The cursor sits on `1`, which is not a reserved word: this stays a 102, where
    // the same test on `SELECT` gives a 156.
    let error = match p.expect_keyword(Keyword::From) {
        Ok(()) => unreachable!("there is no FROM here"),
        Err(error) => error,
    };
    assert_eq!(error.number, 102);
    assert_eq!(error.line, 1);
    assert_eq!(p.mark(), 1);
}

#[test]
fn cursor_expect_punct_errors() {
    let mut p = cursor("1");
    let error = match p.expect_punct(Punct::Comma) {
        Ok(()) => unreachable!("there is no comma here"),
        Err(error) => error,
    };
    assert_eq!(error.number, 102);
    assert_eq!(p.mark(), 0);
}

#[test]
fn cursor_eat_seq() {
    let mut p = cursor("GROUP BY a");
    assert!(p.eat_keyword_seq(&[Keyword::Group, Keyword::By]));
    assert_eq!(p.mark(), 2);

    let mut p = cursor("GROUP a");
    assert!(!p.eat_keyword_seq(&[Keyword::Group, Keyword::By]));
    // The `GROUP` already eaten is given back.
    assert_eq!(p.mark(), 0);
    assert!(p.at_keyword(Keyword::Group));
}

#[test]
fn cursor_parse_ident_accepts_non_reserved_keyword() {
    let ident = match cursor("sum").parse_ident() {
        Ok(ident) => ident,
        Err(error) => unreachable!("SUM is not reserved: {error:?}"),
    };
    assert_eq!(ident.value, "sum");
    assert!(!ident.quoted);

    let mut p = cursor("select");
    let error = match p.parse_ident() {
        Ok(ident) => unreachable!("SELECT is reserved, got {ident:?}"),
        Err(error) => error,
    };
    // SQL Server: `USE select` => 156, exercising a reserved word as an identifier.
    assert_eq!(error.number, 156);
    assert_eq!(p.mark(), 0);

    let ident = match cursor("[select]").parse_ident() {
        Ok(ident) => ident,
        Err(error) => unreachable!("a delimited name may be any word: {error:?}"),
    };
    assert_eq!(ident.value, "select");
    assert!(ident.quoted);
}

#[test]
fn cursor_parse_object_name() {
    let parts = |text: &str| {
        let name = match cursor(text).parse_object_name() {
            Ok(name) => name,
            Err(error) => unreachable!("{text} is a valid name: {error:?}"),
        };
        let part = |ident: Option<crate::ast::expr::Ident>| ident.map(|i| i.value);
        (
            part(name.server),
            part(name.database),
            part(name.schema),
            name.name.value,
        )
    };
    assert_eq!(parts("a"), (None, None, None, "a".to_owned()));
    assert_eq!(
        parts("a.b"),
        (None, None, Some("a".to_owned()), "b".to_owned())
    );
    assert_eq!(
        parts("a.b.c"),
        (
            None,
            Some("a".to_owned()),
            Some("b".to_owned()),
            "c".to_owned()
        )
    );
    assert_eq!(
        parts("a.b.c.d"),
        (
            Some("a".to_owned()),
            Some("b".to_owned()),
            Some("c".to_owned()),
            "d".to_owned()
        )
    );
    // An empty part is legal T-SQL: `a..c` is a database and an object, no schema.
    assert_eq!(
        parts("a..c"),
        (None, Some("a".to_owned()), None, "c".to_owned())
    );

    let mut p = cursor("a.b.c.d.e");
    let error = match p.parse_object_name() {
        Ok(name) => unreachable!("five parts is too many, got {name:?}"),
        Err(error) => error,
    };
    assert_eq!(error.number, 102);
    assert_eq!(p.mark(), 0);
}

#[test]
fn cursor_span_from() {
    let mut p = cursor("SELECT 1");
    let start = p.mark();
    // Nothing consumed yet: an empty span on the first token.
    assert_eq!(p.span_from(start).len, 0);
    p.advance();
    p.advance();
    let span = p.span_from(start);
    assert_eq!(span.line, 1);
    assert_eq!(span.column, 1);
    assert_eq!(span.offset, 0);
    assert_eq!(span.len, 8);

    // From the middle: the position is the one of the first token consumed.
    let mut p = cursor("SELECT\n  a + 1");
    p.advance();
    let start = p.mark();
    p.advance();
    p.advance();
    p.advance();
    let span = p.span_from(start);
    assert_eq!(span.line, 2);
    assert_eq!(span.column, 3);
    assert_eq!(span.offset, 9);
    assert_eq!(span.len, 5);
}

#[test]
fn cursor_reset_undoes_everything() {
    let mut p = cursor("SELECT 1");
    let start = p.mark();
    p.advance();
    p.advance();
    p.reset(start);
    assert_eq!(p.peek().kind, TokenKind::Keyword(Keyword::Select));
}

#[test]
fn cursor_opts_are_the_ones_given() {
    assert!(cursor("SELECT 1").opts().quoted_identifier);
}

#[test]
fn cursor_reports_an_unknown_character() {
    // The lexer hands `\` over as `Unknown`; the cursor turns it into a 102.
    let p = cursor("\\");
    assert_eq!(p.peek().kind, TokenKind::Unknown);
    let error = p.error_here();
    assert_eq!(error.number, 102);
    assert_eq!(error.message, "Syntax error near '\\'.");
}

/// Recurses through [`Parser::nested`] `levels` times, counting how far it got.
///
/// The closure is the shape every guarded rule has, so what it measures is what the
/// grammar gets. The batch is irrelevant: only the counter is under test here.
fn descend(p: &mut Parser, levels: u32, reached: &mut u32) -> SqlResult<()> {
    p.nested(|p| {
        *reached += 1;
        if levels == 0 {
            Ok(())
        } else {
            descend(p, levels - 1, reached)
        }
    })
}

#[test]
fn nesting_is_allowed_up_to_the_limit() {
    let mut p = cursor("SELECT 1");
    let mut reached = 0;
    let outcome = descend(&mut p, MAX_NESTING_DEPTH - 1, &mut reached);
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(reached, MAX_NESTING_DEPTH);
}

#[test]
fn nesting_one_level_past_the_limit_is_error_191() {
    let mut p = cursor("SELECT 1");
    let mut reached = 0;
    let Err(error) = descend(&mut p, MAX_NESTING_DEPTH, &mut reached) else {
        unreachable!("one level past the limit must be refused");
    };
    // The rule at the limit ran; the one below it never started.
    assert_eq!(reached, MAX_NESTING_DEPTH);
    assert_eq!(error.number, 191);
    assert_eq!(error.severity, 15);
    assert_eq!(error.state, 1);
    assert_eq!(
        error.message,
        "The statement is nested too deeply; split it into smaller queries."
    );
}

#[test]
fn the_counter_comes_back_down_after_a_failure() {
    let mut p = cursor("SELECT 1");
    let mut wasted = 0;
    assert!(descend(&mut p, MAX_NESTING_DEPTH, &mut wasted).is_err());
    assert_eq!(p.depth, 0, "a refused descent left levels behind");
    // And the cursor is good for another full descent, which a leak would refuse.
    let mut reached = 0;
    assert!(descend(&mut p, MAX_NESTING_DEPTH - 1, &mut reached).is_ok());
}

#[test]
fn the_191_carries_the_line_the_cursor_is_on() {
    let mut p = cursor("SELECT\n1");
    p.advance();
    assert_eq!(p.nested_too_deeply().line, 2);
}
