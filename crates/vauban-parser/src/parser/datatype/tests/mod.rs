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
