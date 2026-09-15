//! Integration tests for `parse_parameter_declarations`.
//!
//! Each form of the grammar is tested: the empty list, one parameter, several
//! parameters, `OUTPUT`/`OUT`, `AS` before the type, default values and `READONLY`.
//! The error cases are tested with the expected number from SQL Server 2022, and the
//! accepted cases show that the parser accepts the same strings SQL Server accepts.

use vauban_parser::{ParameterDeclaration, ParseOptions, parse_parameter_declarations};

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
fn empty_string_yields_no_parameters() {
    assert!(parse("").is_empty());
    assert!(parse(" ").is_empty());
}

#[test]
fn one_integer_parameter() {
    let params = parse("@a int");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name, "@a");
    assert_eq!(params[0].ty.name, "int");
    assert!(params[0].ty.args.is_empty());
    assert!(!params[0].output);
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
    assert!(!params[1].ty.args.is_empty());
    assert!(params[1].output);
}

#[test]
fn optional_as_keyword() {
    assert_eq!(parse("@a AS int").len(), 1);
}

#[test]
fn out_is_a_synonym_of_output() {
    let params = parse("@a int OUT");
    assert_eq!(params.len(), 1);
    assert!(params[0].output);
}

#[test]
fn decimal_with_precision_and_scale() {
    let params = parse("@a decimal(10, 2)");
    assert_eq!(params[0].ty.name, "decimal");
    assert_eq!(params[0].ty.args.len(), 2);
}

#[test]
fn varchar_max() {
    let params = parse("@a nvarchar(max)");
    assert_eq!(params[0].ty.name, "nvarchar");
    assert_eq!(params[0].ty.args.len(), 1);
}

#[test]
fn user_defined_type() {
    let params = parse("@a dbo.MonType");
    assert_eq!(params[0].ty.name, "dbo.MonType");
}

#[test]
fn duplicate_name_is_accepted_at_parse_time() {
    // SQL Server: 134 at execution, not a parse error.
    let params = parse("@a int, @a int");
    assert_eq!(params.len(), 2);
}

#[test]
fn default_value_is_accepted() {
    // SQL Server accepts `= 1` in the parameter string.
    let params = parse("@a int = 1");
    assert_eq!(params.len(), 1);
}

#[test]
fn default_value_with_string() {
    let params = parse("@a nvarchar(50) = N'hello'");
    assert_eq!(params.len(), 1);
}

#[test]
fn readonly_is_accepted() {
    // SQL Server: 346 at semantic analysis, not a syntax error.
    let params = parse("@a int READONLY");
    assert_eq!(params.len(), 1);
}

#[test]
fn unknown_type_is_accepted_at_parse_time() {
    // The parser does not resolve types; 2715 is the binder's.
    let params = parse("@a foo");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].ty.name, "foo");
}

#[test]
fn name_without_at_is_error_102() {
    let error = err("a int");
    assert_eq!(error.number, 102);
}

#[test]
fn missing_type_is_error_102() {
    let error = err("@a");
    assert_eq!(error.number, 102);
}

#[test]
fn trailing_comma_is_error_102() {
    let error = err("@a int,");
    assert_eq!(error.number, 102);
}

#[test]
fn span_of_first_parameter() {
    let params = parse("@a int");
    assert_eq!(params[0].span.line, 1);
    assert_eq!(params[0].span.column, 1);
    assert!(params[0].span.len > 0);
}

#[test]
fn span_of_second_parameter() {
    let params = parse("@a int, @b varchar(10)");
    assert_eq!(params[1].span.line, 1);
    assert!(params[1].span.len > 0);
}
