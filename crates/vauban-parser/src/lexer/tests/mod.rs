use super::tokenize;
use crate::keyword::Keyword;
use crate::parser::ParseOptions;
use crate::token::{Op, Punct, Token, TokenKind};
use vauban_errors::SqlError;

/// The tokens of `source`, without the closing `Eof`.
fn tok(source: &str) -> Vec<Token> {
    tok_with(source, &ParseOptions::default())
}

/// The same, under explicit options.
fn tok_with(source: &str, options: &ParseOptions) -> Vec<Token> {
    let mut tokens = tokenize(source, options).expect("the source lexes");
    let last = tokens.pop().expect("there is always an Eof");
    assert_eq!(last.kind, TokenKind::Eof, "the flow must end with Eof");
    assert_eq!(last.text, "");
    tokens
}

fn kinds(source: &str) -> Vec<TokenKind> {
    tok(source).into_iter().map(|token| token.kind).collect()
}

fn texts(source: &str) -> Vec<String> {
    tok(source).into_iter().map(|token| token.text).collect()
}

fn name(value: &str) -> TokenKind {
    TokenKind::Ident {
        value: value.to_owned(),
        quoted: false,
    }
}

fn quoted_name(value: &str) -> TokenKind {
    TokenKind::Ident {
        value: value.to_owned(),
        quoted: true,
    }
}

fn text(value: &str) -> TokenKind {
    TokenKind::Str {
        value: value.to_owned(),
        unicode: false,
    }
}

/// The error `tokenize` stops on. Panics when it succeeds, which is a test failure.
fn lex_error(source: &str) -> SqlError {
    match tokenize(source, &ParseOptions::default()) {
        Ok(_) => panic!("{source} must not lex"),
        Err(error) => error,
    }
}

#[test]
fn lex_simple_select() {
    let tokens = tok("SELECT 1");
    assert_eq!(tokens.len(), 2);
    assert_eq!(tokens[0].kind, TokenKind::Keyword(Keyword::Select));
    assert_eq!(tokens[0].text, "SELECT");
    assert_eq!((tokens[0].span.line, tokens[0].span.column), (1, 1));
    assert_eq!(tokens[1].kind, TokenKind::Integer);
    assert_eq!(tokens[1].text, "1");
    assert_eq!((tokens[1].span.line, tokens[1].span.column), (1, 8));
}

#[test]
fn lex_keywords_are_case_insensitive() {
    for source in ["select", "SeLeCt", "SELECT"] {
        let tokens = tok(source);
        assert_eq!(tokens[0].kind, TokenKind::Keyword(Keyword::Select));
        // The text is the source slice, never normalised.
        assert_eq!(tokens[0].text, source);
    }
}

#[test]
fn lex_identifiers() {
    assert_eq!(kinds("[my table]"), vec![quoted_name("my table")]);
    assert_eq!(texts("[my table]"), vec!["[my table]"]);
    assert_eq!(kinds("[a]]b]"), vec![quoted_name("a]b")]);
    assert_eq!(kinds("\"a\"\"b\""), vec![quoted_name("a\"b")]);
    // A word that is no keyword, and one that holds `$`, `@` and `#`.
    assert_eq!(kinds("customers"), vec![name("customers")]);
    assert_eq!(kinds("a$b@c#d"), vec![name("a$b@c#d")]);
    // `GO` is a client separator, not a keyword: the lexer sees a name.
    assert_eq!(kinds("GO"), vec![name("GO")]);
    // A lone `N` is a name; only `N'` opens a Unicode string.
    assert_eq!(kinds("N"), vec![name("N")]);
}

#[test]
fn lex_double_quotes_off_are_strings() {
    // Under QUOTED_IDENTIFIER OFF, `""` is the escape of `"`, so `"a""b"` is **one**
    // string `a"b`: SQL Server answers one unnamed `varchar` column holding `a"b`;
    // two strings would have given the column the alias `b` and the value `a`.
    let options = ParseOptions {
        quoted_identifier: false,
    };
    let kinds: Vec<TokenKind> = tok_with("\"a\"\"b\"", &options)
        .into_iter()
        .map(|token| token.kind)
        .collect();
    assert_eq!(kinds, vec![text("a\"b")]);
    // The counter-proof of the pair: two separate strings stay two tokens.
    let kinds: Vec<TokenKind> = tok_with("\"a\" \"b\"", &options)
        .into_iter()
        .map(|token| token.kind)
        .collect();
    assert_eq!(kinds, vec![text("a"), text("b")]);
    // `''` is still the escape of `'` in that mode.
    let kinds: Vec<TokenKind> = tok_with("'a''b'", &options)
        .into_iter()
        .map(|token| token.kind)
        .collect();
    assert_eq!(kinds, vec![text("a'b")]);
}

#[test]
fn lex_variables_and_temp_names() {
    let tokens = tok("@x @@ROWCOUNT #t ##g");
    let kinds: Vec<TokenKind> = tokens.iter().map(|token| token.kind.clone()).collect();
    assert_eq!(
        kinds,
        vec![
            TokenKind::Variable,
            TokenKind::Variable,
            name("#t"),
            name("##g"),
        ]
    );
    let texts: Vec<&str> = tokens.iter().map(|token| token.text.as_str()).collect();
    assert_eq!(texts, vec!["@x", "@@ROWCOUNT", "#t", "##g"]);
}

#[test]
fn lex_strings() {
    assert_eq!(kinds("'a''b'"), vec![text("a'b")]);
    assert_eq!(texts("'a''b'"), vec!["'a''b'"]);
    assert_eq!(
        kinds("N'é'"),
        vec![TokenKind::Str {
            value: "é".to_owned(),
            unicode: true,
        }]
    );
    assert_eq!(texts("N'é'"), vec!["N'é'"]);
    assert_eq!(kinds("''"), vec![text("")]);
    // A `--` inside a string opens no comment.
    assert_eq!(kinds("'-- x'"), vec![text("-- x")]);
}

#[test]
fn odd_hex_payload_is_padded_without_changing_source_positions() {
    assert_eq!(
        kinds("0x0 0xABC 0xabc 0Xf 0x"),
        vec![
            TokenKind::Binary("00".to_owned()),
            TokenKind::Binary("0ABC".to_owned()),
            TokenKind::Binary("0abc".to_owned()),
            TokenKind::Binary("0f".to_owned()),
            TokenKind::Binary(String::new()),
        ]
    );
    assert_eq!(texts("0xABC + 1"), vec!["0xABC", "+", "1"]);
    let tokens = tok("0xABC + 1");
    assert_eq!((tokens[0].span.offset, tokens[0].span.len), (0, 5));
    assert_eq!((tokens[1].span.offset, tokens[1].span.column), (6, 7));
}

#[test]
fn lex_numbers() {
    let source = "1 1.5 .5 1. 1e3 1.5E-2 0x00FF $1.50 $-1.50";
    assert_eq!(
        kinds(source),
        vec![
            TokenKind::Integer,
            TokenKind::Decimal,
            TokenKind::Decimal,
            TokenKind::Decimal,
            TokenKind::Float,
            TokenKind::Float,
            TokenKind::Binary("00FF".to_owned()),
            TokenKind::Money,
            TokenKind::Money,
        ]
    );
    assert_eq!(
        texts(source),
        vec![
            "1", "1.5", ".5", "1.", "1e3", "1.5E-2", "0x00FF", "$1.50", "$-1.50"
        ]
    );
    // Only the `$…` shape is money: `-$1.50` is a sign and a money literal, and
    // the parser is the one that drops the `$` when it builds the literal.
    assert_eq!(
        kinds("-$1.50"),
        vec![TokenKind::Op(Op::Minus), TokenKind::Money]
    );
    // `a$b` is a name, not a name and a money literal.
    assert_eq!(kinds("a$b"), vec![name("a$b")]);
}

#[test]
fn lex_operators() {
    let source = "+= <> != !< !> <= >= :: ~ ^ &";
    assert_eq!(
        kinds(source),
        vec![
            TokenKind::Op(Op::PlusEq),
            TokenKind::Op(Op::Ne),
            TokenKind::Op(Op::BangEq),
            TokenKind::Op(Op::NotLt),
            TokenKind::Op(Op::NotGt),
            TokenKind::Op(Op::Le),
            TokenKind::Op(Op::Ge),
            TokenKind::Punct(Punct::DoubleColon),
            TokenKind::Op(Op::Tilde),
            TokenKind::Op(Op::Caret),
            TokenKind::Op(Op::Ampersand),
        ]
    );
    // `!=` and `<>` keep the text the user wrote, for errors 102 and 156.
    assert_eq!(texts("<> !="), vec!["<>", "!="]);
    // The `.` of a qualified name is never absorbed by a number.
    assert_eq!(
        kinds("a.b"),
        vec![name("a"), TokenKind::Punct(Punct::Dot), name("b")]
    );
    assert_eq!(
        kinds("t.5"),
        vec![name("t"), TokenKind::Punct(Punct::Dot), TokenKind::Integer]
    );
}

#[test]
fn lex_comments() {
    let expected = vec![TokenKind::Keyword(Keyword::Select), TokenKind::Integer];
    assert_eq!(kinds("SELECT -- x\n1"), expected);
    // Block comments nest, so the first `*/` closes the inner one only.
    assert_eq!(kinds("SELECT /* a /* b */ c */ 1"), expected);
    // A `'` inside a comment opens no string.
    assert_eq!(kinds("SELECT /* it's fine */ 1"), expected);
    // An unclosed `/*` is error 113, not a silent drop.
    assert_eq!(
        lex_error("SELECT 1 /* oops"),
        SqlError::missing_end_comment_mark(1)
    );
    // The line is the **last** line of the batch, not the one the `/*` sits on --
    // the pair below is what tells the two apart, since the `/*` is on line 3 in
    // both (the shapes quoted on `skip_block_comment`).
    assert_eq!(
        lex_error("SELECT 1\n-- c\n/* oops"),
        SqlError::missing_end_comment_mark(3)
    );
    assert_eq!(
        lex_error("SELECT 1\n-- c\n/* oops\n\n"),
        SqlError::missing_end_comment_mark(5)
    );
    // A batch that ends on a line ending reports the empty line it opens.
    assert_eq!(
        lex_error("SELECT 1 /* oops\n"),
        SqlError::missing_end_comment_mark(2)
    );
    // A nested `/*` left open closes nothing: the outer comment is still open.
    assert_eq!(
        lex_error("SELECT 1 /* a /* b */"),
        SqlError::missing_end_comment_mark(1)
    );
    // A `--` on the last line needs no newline to end.
    assert_eq!(kinds("SELECT 1 -- done"), expected);
}

#[test]
fn lex_positions() {
    let tokens = tok("SELECT 1\nFROM t");
    let from = &tokens[2];
    assert_eq!(from.kind, TokenKind::Keyword(Keyword::From));
    assert_eq!((from.span.line, from.span.column), (2, 1));
    assert_eq!(from.span.offset, 9);
    assert_eq!(from.span.len, 4);
    // A `\r\n` is a single line ending.
    let tokens = tok("SELECT\r\n  1");
    let one = &tokens[1];
    assert_eq!(one.kind, TokenKind::Integer);
    assert_eq!((one.span.line, one.span.column), (2, 3));
}

#[test]
fn lex_unicode_column_is_chars() {
    // `S`=1 … `T`=6, space=7, `N`=8, `'`=9, `é`=10, `é`=11, `'`=12, `,`=13,
    // space=14, `x`=15, while the two `é` weigh two bytes each.
    let tokens = tok("SELECT N'éé', x");
    let last = &tokens[3];
    assert_eq!(last.kind, name("x"));
    assert_eq!(last.span.column, 15);
    assert_eq!(last.span.offset, 16);
}

#[test]
fn lex_unclosed_string_is_105() {
    let error = lex_error("SELECT 'abc;");
    assert_eq!(error.number, 105);
    assert_eq!(error.severity, 15);
    assert_eq!(error.state, 1);
    assert_eq!(error.line, 1);
    // The exact wording is pinned by the catalogue of `vauban-errors`, whose own
    // test `unclosed_quotation_mark_is_105` compares it letter by letter.
    // It is deliberately not spelled out anywhere in this crate, which writes no
    // error message of its own: only the argument, `abc;`, comes from the lexer.
    assert_eq!(
        error.message,
        SqlError::unclosed_quotation_mark("abc;", 1).message
    );
}

#[test]
fn lex_unclosed_string_reports_its_own_line() {
    let error = lex_error("SELECT 1\n-- c\nSELECT 'x");
    assert_eq!(error.number, 105);
    assert_eq!(error.line, 3);
    assert_eq!(error, SqlError::unclosed_quotation_mark("x", 3));
    // Blanks are part of the reported text, and a doubled quote is reduced to one:
    // `SELECT 1 x 'a  ` answers `'a  '`, `SELECT 1 x 'a` followed by a line ending
    // answers `'a\n'`, `SELECT 'a''` answers `'a''` (argument `a'`) and
    // `SELECT 1 x 'a''b` answers `'a'b'`. A trimmed end or the raw slice would not
    // fit those shapes.
    assert_eq!(
        lex_error("SELECT 'x  "),
        SqlError::unclosed_quotation_mark("x  ", 1)
    );
    assert_eq!(
        lex_error("SELECT 'x\n"),
        SqlError::unclosed_quotation_mark("x\n", 1)
    );
    assert_eq!(
        lex_error("SELECT 'a''"),
        SqlError::unclosed_quotation_mark("a'", 1)
    );
}

#[test]
fn lex_unclosed_bracket_is_105() {
    // A delimited name reports its **value** -- opening delimiter gone, doubled
    // delimiter reduced -- not the raw slice with its `[`.
    assert_eq!(
        lex_error("SELECT [a"),
        SqlError::unclosed_quotation_mark("a", 1)
    );
    assert_eq!(
        lex_error("SELECT [a]]b"),
        SqlError::unclosed_quotation_mark("a]b", 1)
    );
    assert_eq!(
        lex_error("SELECT 1 x [a]]b]]c"),
        SqlError::unclosed_quotation_mark("a]b]c", 1)
    );
    // A `"` name under QUOTED_IDENTIFIER ON reports its value the same way, doubled
    // `""` reduced to one; the character string above is the family that keeps its
    // slice.
    assert_eq!(
        lex_error("SELECT \"a"),
        SqlError::unclosed_quotation_mark("a", 1)
    );
    assert_eq!(
        lex_error("SELECT \"a\"\"b"),
        SqlError::unclosed_quotation_mark("a\"b", 1)
    );
    // Blanks are kept here too: `SELECT 1 x [a  ` answers `'a  '`.
    assert_eq!(
        lex_error("SELECT [a  "),
        SqlError::unclosed_quotation_mark("a  ", 1)
    );
}

#[test]
fn unclosed_quotation_mark_comes_from_the_catalog() {
    assert_eq!(
        lex_error("SELECT 'abc;"),
        SqlError::unclosed_quotation_mark("abc;", 1)
    );
}

#[test]
fn lex_unknown_character_is_a_token() {
    // The lexer raises no syntax error: it hands the character to the parser, which
    // reports the 102.
    assert_eq!(kinds("\\"), vec![TokenKind::Unknown]);
    assert_eq!(texts("\\"), vec!["\\"]);
    // A lone `!` is not an operator, and a no-break space is not whitespace.
    assert_eq!(kinds("!"), vec![TokenKind::Unknown]);
    assert_eq!(kinds("\u{a0}"), vec![TokenKind::Unknown]);
}

#[test]
fn lex_empty_and_blank_texts() {
    assert_eq!(kinds(""), vec![]);
    assert_eq!(kinds("  \r\n\t"), vec![]);
    assert_eq!(kinds("-- only a comment"), vec![]);
    let tokens = tokenize("", &ParseOptions::default()).expect("the empty batch lexes");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].kind, TokenKind::Eof);
    assert_eq!(tokens[0].span.len, 0);
}
