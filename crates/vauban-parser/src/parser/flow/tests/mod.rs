use crate::{Batch, Expr, Literal, ParseOptions, Statement, parse_batch};
use vauban_errors::SqlError;

/// Parses `text`, which must parse.
fn p(text: &str) -> Batch {
    match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => batch,
        Err(error) => unreachable!("{text} should parse: {} {}", error.number, error.message),
    }
}

/// The one statement of a batch that holds exactly one.
fn one(text: &str) -> Statement {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    match statements.pop() {
        Some(statement) => statement,
        None => unreachable!("{text} has one statement"),
    }
}

/// The error of a text that must **not** parse.
fn p_err(text: &str) -> SqlError {
    match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => unreachable!("{text} should not parse, got {batch:?}"),
        Err(error) => error,
    }
}

/// The string a `PRINT` statement carries, so a test can tell branches apart by their
/// value rather than by their shape alone.
fn printed_value(statement: &Statement) -> &str {
    match statement {
        Statement::Print {
            expr: Expr::Literal(Literal::Str { value, .. }, _),
            ..
        } => value,
        other => unreachable!("expected a PRINT of a string, got {other:?}"),
    }
}

#[test]
fn semicolon_before_else_after_a_plain_statement() {
    match one("IF 1 = 1 PRINT 'then'; ELSE PRINT 'else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert_eq!(printed_value(&then_branch), "then");
            assert_eq!(
                printed_value(&else_branch.expect("the ELSE was read past the ';'")),
                "else"
            );
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

#[test]
fn semicolon_before_else_after_a_block() {
    match one("IF 1 = 1 BEGIN PRINT 'then' END; ELSE PRINT 'else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            match *then_branch {
                Statement::Block { statements, .. } => {
                    assert_eq!(statements.len(), 1, "the THEN block holds one statement");
                    assert_eq!(printed_value(&statements[0]), "then");
                }
                other => unreachable!("the THEN is a block, got {other:?}"),
            }
            assert_eq!(
                printed_value(&else_branch.expect("the ELSE was read past the ';'")),
                "else"
            );
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

/// Two `;` in a row before `ELSE` are both consumed, not just the first: a script that
/// doubles its separators by mistake still reaches the `ELSE`.
#[test]
fn a_run_of_semicolons_before_else_is_consumed_as_one() {
    match one("IF 1 = 1 PRINT 'then';; ELSE PRINT 'else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert_eq!(printed_value(&then_branch), "then");
            assert_eq!(
                printed_value(&else_branch.expect("the ELSE was read past both ';'")),
                "else"
            );
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

/// A comment between two of the `;` changes nothing: it produces no token, so the run
/// looks the same to the parser as `;;`.
#[test]
fn a_comment_between_two_semicolons_before_else_changes_nothing() {
    match one("IF 1 = 1 PRINT 'then'; /* c */ ; ELSE PRINT 'else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert_eq!(printed_value(&then_branch), "then");
            assert_eq!(
                printed_value(&else_branch.expect("the ELSE was read past both ';'")),
                "else"
            );
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

/// The `ELSE` still binds to the nearest `IF` once a `;` can come before it: the inner
/// `IF` reads its own `;` and `ELSE` before the outer one ever looks for either.
#[test]
fn semicolon_before_else_binds_to_the_nearest_if() {
    match one("IF 1 = 1 IF 2 = 3 PRINT 'inner-then'; ELSE PRINT 'inner-else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert!(else_branch.is_none(), "the outer IF has no ELSE");
            match *then_branch {
                Statement::If {
                    then_branch: inner_then,
                    else_branch: inner_else,
                    ..
                } => {
                    assert_eq!(printed_value(&inner_then), "inner-then");
                    assert_eq!(
                        printed_value(&inner_else.expect("the inner IF has the ELSE")),
                        "inner-else"
                    );
                }
                other => unreachable!("the THEN is the inner IF, got {other:?}"),
            }
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

/// Without an `ELSE` in sight, the `;` after the branch is left for the caller: it does
/// not become part of the `IF`, and a second statement still follows in the same batch.
#[test]
fn semicolon_without_else_is_left_to_the_caller() {
    let batch = p("IF 1 = 0 PRINT 'taken'; PRINT 'after';");
    assert_eq!(
        batch.statements.len(),
        2,
        "the ';' still separates two statements"
    );
    match &batch.statements[0] {
        Statement::If { else_branch, .. } => {
            assert!(else_branch.is_none(), "no ELSE was written")
        }
        other => unreachable!("the first statement is an IF, got {other:?}"),
    }
    assert_eq!(printed_value(&batch.statements[1]), "after");
}

#[test]
fn semicolon_right_after_the_condition_is_refused() {
    assert_eq!(p_err("IF 1 = 1; SELECT 1").number, 102);
}

#[test]
fn semicolon_alone_after_else_is_refused() {
    assert_eq!(p_err("IF 1 = 1 SELECT 1; ELSE;").number, 102);
}

#[test]
fn condition_with_nothing_after_it_is_refused() {
    assert_eq!(p_err("IF 1 = 1;").number, 102);
}
