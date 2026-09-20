use super::{ParameterDeclaration, parse_parameter_declarations};
use crate::ast::expr::TypeArg;
use crate::parser::ParseOptions;

fn parse(text: &str) -> Vec<ParameterDeclaration> {
    parse_parameter_declarations(text, &ParseOptions::default()).unwrap()
}

fn err(text: &str) -> vauban_errors::SqlError {
    match parse_parameter_declarations(text, &ParseOptions::default()) {
        Ok(params) => unreachable!("{text} should not parse, got {params:?}"),
        Err(error) => error,
    }
}

#[test]
fn empty_string_yields_empty_list() {
    assert!(parse("").is_empty(), "empty string");
    assert!(parse(" ").is_empty(), "whitespace only");
    assert!(parse("  ").is_empty(), "multiple spaces");
}

#[test]
fn single_parameter() {
    let params = parse("@a int");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name, "@a");
    assert_eq!(params[0].ty.name, "int");
    assert!(params[0].ty.args.is_empty());
    assert!(!params[0].output, "no OUTPUT");
}

#[test]
fn two_parameters_with_output() {
    let params = parse("@a int, @b nvarchar(50) OUTPUT");
    assert_eq!(params.len(), 2);

    assert_eq!(params[0].name, "@a");
    assert_eq!(params[0].ty.name, "int");
    assert!(!params[0].output);

    assert_eq!(params[1].name, "@b");
    assert_eq!(params[1].ty.name, "nvarchar");
    assert_eq!(params[1].ty.args, vec![TypeArg::Number(50)]);
    assert!(params[1].output);
}

#[test]
fn optional_as_before_type() {
    let params = parse("@a AS int");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].ty.name, "int");
}

#[test]
fn out_is_accepted() {
    let params = parse("@a int OUT");
    assert_eq!(params.len(), 1);
    assert!(params[0].output);
}

#[test]
fn many_types() {
    for text in [
        "@a nvarchar(max)",
        "@a decimal(10,2)",
        "@a varbinary(16)",
        "@a datetime2(3)",
        "@a nvarchar(50)",
        "@a uniqueidentifier",
        "@a dbo.MonType",
    ] {
        let params = parse(text);
        assert_eq!(params.len(), 1, "{text}");
        assert_eq!(params[0].name, "@a", "{text}");
    }
}

#[test]
fn comma_separated() {
    let params = parse("@a int, @b varchar(10), @c decimal(18, 2) OUTPUT");
    assert_eq!(params.len(), 3);
    assert!(params[2].output);
    assert_eq!(
        params[2].ty.args,
        vec![TypeArg::Number(18), TypeArg::Number(2)]
    );
}

#[test]
fn error_name_without_at() {
    // SQL Server: 102 near the bare name.
    let error = err("a int");
    assert_eq!(error.number, 102);
}

#[test]
fn error_missing_type() {
    // SQL Server: 102 near the end of the batch (or at EOF).
    let error = err("@a");
    assert_eq!(error.number, 102);
}

#[test]
fn error_trailing_comma() {
    // SQL Server: 102 near the end.
    let error = err("@a int,");
    assert_eq!(error.number, 102);
}

#[test]
fn error_duplicate_name() {
    // SQL Server: accepts duplicate names in the @params string;
    // the duplicate is caught later at execution.
    let params = parse("@a int, @a int");
    assert_eq!(params.len(), 2);
    // No error: SQL Server does not refuse this at parse time.
}

#[test]
fn error_default_value_accepted() {
    // SQL Server: `= value` is accepted in `sp_executesql` params
    // and becomes the default. The grammar accepts it for now.
    let params = parse("@a int = 1");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name, "@a");
}

#[test]
fn error_readonly_accepted() {
    // SQL Server: READONLY is valid syntax in sp_executesql params.
    let params = parse("@a int READONLY");
    assert_eq!(params.len(), 1);
}

#[test]
fn error_unknown_type_is_accepted_at_parse_time() {
    // The parser does not know the list of types; 2715 is the binder's.
    let params = parse("@a foo(1)");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].ty.name, "foo");
}
