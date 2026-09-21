//! Unit tests for `parser::from`: the table value constructor read as a derived table.
//!
//! The joins, the hint lists and the grouped join trees of the module are covered from
//! the public entry point in `tests/select.rs`; what is here needs the shape of the
//! reference itself.

use crate::ast::expr::{Expr, Literal};
use crate::ast::query::{QueryBody, TableRef};
use crate::ast::stmt::Statement;
use crate::parser::{ParseOptions, parse_batch};
use vauban_errors::SqlError;

/// The `FROM` list of a batch holding one `SELECT`.
fn from_of(text: &str) -> Vec<TableRef> {
    let mut statements = match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => batch.statements,
        Err(error) => unreachable!("{text} should parse: {error:?}"),
    };
    let Some(Statement::Select(select)) = statements.pop() else {
        unreachable!("{text} is one SELECT");
    };
    // `QueryBody` implements `Drop`, so the specification is cloned rather than moved out.
    match &select.body {
        QueryBody::Select(spec) => spec.from.clone(),
        other => unreachable!("{text} is a plain SELECT, got {other:?}"),
    }
}

/// The error of a text that must not parse.
fn err_of(text: &str) -> SqlError {
    match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => unreachable!("{text} should not parse, got {batch:?}"),
        Err(error) => error,
    }
}

/// The re-serialisation of `text`, which must parse back to an equal batch.
fn round_trip(text: &str) -> String {
    let batch = match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => batch,
        Err(error) => unreachable!("{text} should parse: {error:?}"),
    };
    let printed = batch.to_string();
    let again = match parse_batch(&printed, &ParseOptions::default()) {
        Ok(batch) => batch,
        Err(error) => unreachable!("{printed} should parse back: {error:?}"),
    };
    assert_eq!(batch, again, "{text} was printed as {printed}");
    printed
}

/// `(VALUES …)` in the source position of a `FROM` reads as its own reference, with the
/// rows, the alias and the column list of the alias.
#[test]
fn values_reads_as_a_reference_of_its_own() {
    let from = from_of("SELECT c FROM (VALUES (1), (2)) v(c)");
    assert_eq!(from.len(), 1);
    let TableRef::Values {
        rows,
        alias,
        columns,
        ..
    } = &from[0]
    else {
        panic!("not a TableRef::Values: {:?}", from[0]);
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 1);
    assert!(matches!(rows[0][0], Expr::Literal(Literal::Integer(_), _)));
    assert_eq!(alias.as_ref().map(|a| a.value.as_str()), Some("v"));
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].value, "c");
}

/// The rows may be wider than one column and may hold expressions, not just literals.
#[test]
fn values_rows_hold_expressions() {
    let from = from_of("SELECT * FROM (VALUES (1 + 1, 'a'), (LEN('abc'), 'b')) v(n, s)");
    let TableRef::Values { rows, columns, .. } = &from[0] else {
        panic!("not a TableRef::Values: {:?}", from[0]);
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 2);
    assert_eq!(rows[1].len(), 2);
    assert_eq!(columns.len(), 2);
    assert!(matches!(rows[0][0], Expr::Binary { .. }));
    assert!(matches!(rows[1][0], Expr::Function { .. }));
}

/// Rows of differing widths parse: the count is the binder's to refuse (10709), not the
/// grammar's.
#[test]
fn values_rows_of_differing_widths_parse() {
    let from = from_of("SELECT * FROM (VALUES (1), (2, 3)) v(c)");
    let TableRef::Values { rows, .. } = &from[0] else {
        panic!("not a TableRef::Values: {:?}", from[0]);
    };
    assert_eq!(rows[0].len(), 1);
    assert_eq!(rows[1].len(), 2);
}

/// An alias without a column list parses too: 8155 is raised by the binder.
#[test]
fn values_without_a_column_list_parses() {
    let from = from_of("SELECT * FROM (VALUES (1)) v");
    let TableRef::Values { alias, columns, .. } = &from[0] else {
        panic!("not a TableRef::Values: {:?}", from[0]);
    };
    assert_eq!(alias.as_ref().map(|a| a.value.as_str()), Some("v"));
    assert!(columns.is_empty());
}

/// A constructor without an alias is refused where the alias should have been, error 102
/// on the token that follows the closing parenthesis.
#[test]
fn values_without_an_alias_is_102() {
    let error = err_of("SELECT * FROM (VALUES (1), (2));");
    assert_eq!(error.number, 102, "{}", error.message);
    assert!(error.message.contains("';'"), "{}", error.message);
}

/// `DEFAULT` is a value of an `INSERT … VALUES` and not of a constructor in a `FROM`: the
/// word is refused where it stands, error 156 because it is reserved.
#[test]
fn values_default_is_a_syntax_error() {
    let error = err_of("SELECT * FROM (VALUES (DEFAULT)) v(c)");
    assert_eq!(error.number, 156, "{}", error.message);
    assert!(error.message.contains("DEFAULT"), "{}", error.message);
    let second = err_of("SELECT * FROM (VALUES (1), (2, DEFAULT)) v(c)");
    assert_eq!(second.number, 156, "{}", second.message);
}

/// A constructor stands where a derived table stands: on either side of a join.
#[test]
fn values_stands_on_either_side_of_a_join() {
    let from = from_of("SELECT * FROM (VALUES (1)) t(c) JOIN (VALUES (1)) u(d) ON t.c = u.d");
    let TableRef::Join { left, right, .. } = &from[0] else {
        panic!("not a TableRef::Join: {:?}", from[0]);
    };
    assert!(matches!(**left, TableRef::Values { .. }));
    assert!(matches!(**right, TableRef::Values { .. }));
}

/// The `parse` -> `Display` -> `parse` loop holds on the constructor, alias and column
/// list included.
#[test]
fn values_round_trips() {
    assert_eq!(
        round_trip("SELECT c FROM (VALUES (1), (2)) v(c)"),
        "SELECT c FROM (VALUES (1), (2)) AS v (c)"
    );
    assert_eq!(
        round_trip("SELECT * FROM (VALUES (1)) AS v"),
        "SELECT * FROM (VALUES (1)) AS v"
    );
    round_trip("SELECT * FROM a JOIN (VALUES (1), (2)) v (c) ON a.k = v.c");
}
