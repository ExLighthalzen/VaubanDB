//! Data types and their arguments.
//!
//! A data type is a name and, sometimes, parenthesised arguments. **Nothing is
//! validated**: `varchar(999999)`, `foo(1)` and `int(3)` all parse. Deciding that a type
//! exists and that its arguments are in range is the binder's job, with the `types`
//! crate, and it is what produces the error 2715 SQL Server reports. The parser only
//! records what was written, in the shape `Display` can write back unchanged.
//!
//! The name is read from the tokens rather than through `Parser::parse_ident`, because
//! several type names of T-SQL are spelled with **reserved** words: `DOUBLE PRECISION`,
//! `NATIONAL CHARACTER VARYING`, `BINARY VARYING` (`DOUBLE`, `PRECISION`, `NATIONAL` and
//! `VARYING` are all reserved).
//!
//! The multi-word names are the ISO synonyms of the T-SQL data types.

use vauban_errors::SqlResult;

use crate::ast::expr::{DataType, TypeArg};
use crate::keyword::Keyword;
use crate::parser::Parser;
use crate::token::{Punct, TokenKind};

/// The type names T-SQL spells in more than one word, uppercase, **longest first** so
/// that the first match is the longest one (`NATIONAL CHARACTER VARYING` before
/// `NATIONAL CHARACTER`).
///
/// The ISO synonyms of the T-SQL data types.
const MULTI_WORD_NAMES: [&[&str]; 8] = [
    &["NATIONAL", "CHARACTER", "VARYING"],
    &["NATIONAL", "CHAR", "VARYING"],
    &["NATIONAL", "CHARACTER"],
    &["NATIONAL", "CHAR"],
    &["CHARACTER", "VARYING"],
    &["CHAR", "VARYING"],
    &["BINARY", "VARYING"],
    &["DOUBLE", "PRECISION"],
];

/// Reads a data type: a name, then the arguments it was written with, if any.
///
/// The name keeps the case it was written in and, for a multi-word name, gets its inner
/// whitespace normalised to a single space, which is what makes `double   precision`
/// re-serialise as `double precision`. A qualified name (`dbo.MonType`, a user-defined
/// type) is kept whole, delimiters included, so that `Display` writes it back as it was.
///
/// # Errors
///
/// The syntax error of the token that spells no type name, or of an argument that is
/// neither an integer nor `MAX`. The cursor is left where the error is.
pub(crate) fn parse_data_type(p: &mut Parser) -> SqlResult<DataType> {
    let start = p.mark();
    let name = parse_type_name(p)?;
    let args = parse_type_args(p)?;
    Ok(DataType {
        name,
        args,
        span: p.span_from(start),
    })
}

/// Reads the name of a type: a multi-word name, a qualified one, or a single word.
///
/// # Errors
///
/// The syntax error of the token that is no word.
fn parse_type_name(p: &mut Parser) -> SqlResult<String> {
    if let Some(words) = multi_word_length(p) {
        let mut name = String::new();
        for index in 0..words {
            if index > 0 {
                name.push(' ');
            }
            name.push_str(&p.advance().text);
        }
        return Ok(name);
    }
    let mut name = read_word(p)?;
    // A user-defined type is named `schema.type`, and both parts keep their source
    // spelling, brackets included.
    if p.at_punct(Punct::Dot) {
        p.advance();
        name.push('.');
        name.push_str(&read_word(p)?);
    }
    Ok(name)
}

/// How many tokens the multi-word type name the cursor sits on is made of, or `None`
/// when it sits on no such name.
fn multi_word_length(p: &Parser) -> Option<usize> {
    MULTI_WORD_NAMES
        .iter()
        .find(|words| {
            words
                .iter()
                .enumerate()
                .all(|(index, word)| word_at(p, index).is_some_and(|found| found == *word))
        })
        .map(|words| words.len())
}

/// The **uppercase** spelling of the bare word `n` tokens ahead, or `None`.
///
/// A delimited name (`[double] precision`) is not a word: the user asked for a name, and
/// a name is never half of a multi-word type.
fn word_at(p: &Parser, n: usize) -> Option<String> {
    let token = p.peek_at(n);
    match &token.kind {
        TokenKind::Ident { quoted: false, .. } | TokenKind::Keyword(_) => {
            Some(token.text.to_uppercase())
        }
        _ => None,
    }
}

/// Reads one word -- an identifier, delimited or not, or any keyword -- and returns its
/// **source text**, brackets and case included.
///
/// # Errors
///
/// The syntax error of the token that is no word.
fn read_word(p: &mut Parser) -> SqlResult<String> {
    if matches!(
        p.peek().kind,
        TokenKind::Ident { .. } | TokenKind::Keyword(_)
    ) {
        return Ok(p.advance().text);
    }
    Err(p.error_here())
}

/// Reads the parenthesised arguments of a type, or nothing when no `(` follows.
///
/// # Errors
///
/// The syntax error of an argument that is neither an integer nor `MAX`, or of a missing
/// `)`.
fn parse_type_args(p: &mut Parser) -> SqlResult<Vec<TypeArg>> {
    if !p.eat_punct(Punct::LeftParen) {
        return Ok(Vec::new());
    }
    let mut args = vec![parse_type_arg(p)?];
    while p.eat_punct(Punct::Comma) {
        args.push(parse_type_arg(p)?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(args)
}

/// Reads one argument of a type: an integer, or the word `MAX`.
///
/// [`TypeArg::Ident`] exists for the word-like arguments of types the module does not
/// read yet; no V1 rule produces it.
///
/// # Errors
///
/// The syntax error of the token that is neither, and of an integer too large for an
/// `i64` -- `decimal(99999999999999999999)` is refused here rather than truncated.
fn parse_type_arg(p: &mut Parser) -> SqlResult<TypeArg> {
    if p.at_keyword(Keyword::Max) {
        p.advance();
        return Ok(TypeArg::Max);
    }
    if matches!(p.peek().kind, TokenKind::Integer)
        && let Ok(value) = p.peek().text.parse::<i64>()
    {
        p.advance();
        return Ok(TypeArg::Number(value));
    }
    Err(p.error_here())
}

#[cfg(test)]
mod tests {
    use super::parse_data_type;
    use crate::ast::expr::{DataType, TypeArg};
    use crate::parser::{ParseOptions, Parser};
    use vauban_errors::SqlResult;

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

    /// Parses `text` as one data type and checks that the whole text was read.
    fn t(text: &str) -> DataType {
        match try_t(text) {
            Ok(ty) => ty,
            Err(error) => unreachable!("{text} should parse: {error:?}"),
        }
    }

    /// Same as [`t`], without the panic: for the tests that expect an error.
    fn try_t(text: &str) -> SqlResult<DataType> {
        let mut p = cursor(text);
        let ty = parse_data_type(&mut p)?;
        assert!(p.at_eof(), "{text} left {:?} unread", p.peek());
        Ok(ty)
    }

    /// Checks the `parse` -> `Display` -> `parse` loop on `text` and returns what
    /// `Display` wrote.
    fn roundtrip(text: &str) -> String {
        let first = t(text);
        let printed = first.to_string();
        assert_eq!(first, t(&printed), "{text} was printed as {printed}");
        printed
    }

    #[test]
    fn data_types() {
        // The case is kept as written, and nothing is normalised: `numeric` does not
        // become `decimal`, and `varchar` does not gain a `(1)`.
        assert_eq!(t("int").name, "int");
        assert!(t("int").args.is_empty());
        assert_eq!(t("INT").name, "INT");
        assert_eq!(t("uniqueidentifier").name, "uniqueidentifier");
        assert_eq!(t("varchar(10)").args, vec![TypeArg::Number(10)]);
        assert_eq!(t("varchar(max)").args, vec![TypeArg::Max]);
        assert_eq!(t("nvarchar(MAX)").args, vec![TypeArg::Max]);
        assert_eq!(
            t("decimal(18, 2)").args,
            vec![TypeArg::Number(18), TypeArg::Number(2)]
        );
        assert_eq!(t("numeric(38)").name, "numeric");
        assert_eq!(t("numeric(38)").args, vec![TypeArg::Number(38)]);
        assert_eq!(t("datetime2(7)").args, vec![TypeArg::Number(7)]);
        // A user-defined type: `schema.type`, kept whole.
        assert_eq!(t("dbo.MonType").name, "dbo.MonType");
        // Two-word names, whose words are reserved keywords.
        assert_eq!(t("double precision").name, "double precision");
        assert_eq!(t("character varying(10)").name, "character varying");
        assert_eq!(t("character varying(10)").args, vec![TypeArg::Number(10)]);
        assert_eq!(
            t("NATIONAL CHARACTER VARYING(4)").name,
            "NATIONAL CHARACTER VARYING"
        );
        assert_eq!(t("binary varying(8)").name, "binary varying");

        for text in [
            "int",
            "INT",
            "varchar(10)",
            "decimal(18, 2)",
            "numeric(38)",
            "datetime2(7)",
            "uniqueidentifier",
            "dbo.MonType",
            "double precision",
            "character varying(10)",
        ] {
            assert_eq!(roundtrip(text), text);
        }
        // The inner whitespace of a two-word name is normalised to a single space, and
        // the blanks of an argument list disappear.
        assert_eq!(roundtrip("double   precision"), "double precision");
        assert_eq!(roundtrip("varchar( 10 )"), "varchar(10)");
        // `MAX` is written uppercase by `Display`, which leaves the tree equal.
        assert_eq!(roundtrip("varchar(max)"), "varchar(MAX)");
    }

    /// A quoted word is a name, never half of a two-word type.
    #[test]
    fn quoted_name_is_not_a_two_word_type() {
        assert_eq!(t("[double]").name, "[double]");
        assert_eq!(roundtrip("[my type]"), "[my type]");
    }

    /// The parser does not know the list of types: refusing an unknown one is the
    /// binder's job, with the error 2715.
    #[test]
    fn data_type_unknown_is_accepted() {
        assert_eq!(t("foo(1)").name, "foo");
        assert_eq!(t("foo(1)").args, vec![TypeArg::Number(1)]);
        assert_eq!(t("varchar(999999)").args, vec![TypeArg::Number(999_999)]);
    }

    #[test]
    fn data_type_errors() {
        for text in [")", "varchar(", "varchar(x)", "varchar(10", "decimal(18,)"] {
            match try_t(text) {
                Ok(ty) => unreachable!("{text} should not parse, got {ty:?}"),
                // These do not fail on a reserved word: 102.
                Err(error) => assert_eq!(error.number, 102, "{text}: {error:?}"),
            }
        }
        // An integer that does not fit an `i64` is refused, never truncated.
        assert!(try_t("decimal(99999999999999999999)").is_err());
    }
}
