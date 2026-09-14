//! The four attributes of a runtime error, and the line above all.
//!
//! `types` and `sysfn` raise the number, the severity, the state and the message; the
//! **line** is the executor's, because it is the first layer to hold a `BoundExpr` and a
//! `BoundExpr` carries the line of the AST node it came from. Each vector below therefore
//! starts from SQL text and goes through the whole chain — parse, bind, execute — since a
//! hand-built `BoundExpr` would prove nothing about the line.
//!
//! The line is counted on the **text of the batch**, 1-based, exactly as the server counts
//! it: `"SELECT 1;\nSELECT 1 / 0"` fails on line 2.
//!
//! **Which line, when a statement spans several lines**: SQL Server answers the line the
//! *statement* starts on, not the line of the sub-expression that raised
//! (`crates/vauban-executor/src/errors.rs`). This crate answers the second, which is the
//! same number for a statement written on one line — each vector below but the two that
//! say otherwise in their comment.

use vauban_binder::{BindContext, SessionOptions, bind};
use vauban_errors::SqlError;
use vauban_executor::{ExecContext, ExecOutcome, RowSet, execute};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_sysfn::{StaticContext, register_builtins};

// ---------------------------------------------------------------------------------------
// The chain: parse, bind, execute
// ---------------------------------------------------------------------------------------

/// The error the first failing statement of `text` raises.
fn err(text: &str) -> SqlError {
    outcomes(text).expect_err("the batch does not execute")
}

/// Parses, binds and executes each statement of `text`, stopping at the first error.
///
/// Stopping is the point of the `?`: the executor runs one statement, and a statement that
/// raised produces nothing more. What the *batch* does next belongs to `session`.
fn outcomes(text: &str) -> Result<Vec<RowSet>, SqlError> {
    // The registry is global and idempotent, like in the other test files of the crate.
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())?;
    let bind_ctx = BindContext::scalar(text, SessionOptions::default());
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let mut sets = Vec::with_capacity(batch.statements.len());
    for statement in &batch.statements {
        let bound = bind(statement, &bind_ctx)?;
        match execute(&bound, &mut ctx)? {
            ExecOutcome::Rows(set) => sets.push(set),
            ExecOutcome::NoRows => panic!("a `SELECT` produces a result set"),
        }
    }
    Ok(sets)
}

// ---------------------------------------------------------------------------------------
// The line
// ---------------------------------------------------------------------------------------

#[test]
fn divide_by_zero_carries_its_line() {
    // Three blank lines between the two statements: nothing but the line count changes,
    // which is exactly what the assertion is about.
    let error = err("SELECT 1;\n\n\nSELECT 1 / 0");
    assert_eq!(error.number, 8134);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert_eq!(error.line, 4);
}

#[test]
fn a_modulo_by_zero_is_the_same_error() {
    // The remainder shares the number, the severity and the state of the quotient, and is
    // lined the same way.
    let error = err("SELECT 1;\nSELECT 1 % 0");
    assert_eq!(error.number, 8134);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert_eq!(error.line, 2);
}

#[test]
fn conversion_error_carries_its_line() {
    // 245 names the source type as the plan knows it, `varchar` here.
    let error = err("SELECT 1;\nSELECT CAST('abc' AS int)");
    assert_eq!(error.number, 245);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert!(error.message.contains("varchar"), "{}", error.message);
    assert_eq!(error.line, 2);
}

#[test]
fn a_statement_on_several_lines_does_not_carry_its_own_line_yet() {
    // **This test pins a known gap, not a rule.** SQL Server answers the line the failing
    // *statement* starts on: for the same batch, the line of the `SELECT`, one above the
    // division. Here the second statement starts on line 2 and the division sits on
    // line 3, and 3 is what comes out.
    //
    // The line the server answers is out of reach of this crate: a line lives on
    // `BoundExpr` and nowhere else, and `BoundStatement::Query` holds a bare
    // `LogicalPlan`, whose nodes carry none. `session` writes the statement's line over
    // this one. When a statement carries its line into the executor, this assertion
    // becomes 2.
    let error = err("SELECT 1;\nSELECT CAST(\n    1 / 0\n    AS int)");
    assert_eq!(error.number, 8134);
    assert_eq!(error.line, 3);
}

#[test]
fn a_statement_keyword_alone_on_its_line_shows_the_same_gap() {
    // The vector that proves the gap cannot be closed inside this crate: `SELECT` is alone
    // on line 2 and SQL Server answers 2, while **no bound expression sits on line 2** —
    // the whole select list starts on line 3. Neither
    // the deepest node, nor the outermost one, nor the smallest line of the plan can
    // answer 2 here. Only the statement itself knows.
    let error = err("SELECT 1;\nSELECT\n1 + (1 / 0)");
    assert_eq!(error.number, 8134);
    assert_eq!(error.line, 3);
}

#[test]
fn a_function_error_carries_the_line_of_its_call() {
    // `sysfn` raises 536 with no line of its own; the `Function` node has one.
    let error = err("SELECT 1;\nSELECT 2;\nSELECT LEFT('abc', -1)");
    assert_eq!(error.number, 536);
    assert_eq!(error.line, 3);
}

#[test]
fn a_like_escape_error_carries_the_line_of_its_predicate() {
    // 506 is raised inside `pattern.rs`, which has no `BoundExpr` of its own: the `Like`
    // node of `expr.rs` is what lines it.
    let error = err("SELECT 1;\nSELECT 1 WHERE 'abc' LIKE 'a' ESCAPE 'ab'");
    assert_eq!(error.number, 506);
    assert_eq!(error.line, 2);
}

#[test]
fn an_error_in_a_where_clause_carries_its_own_line() {
    // The predicate is evaluated by `execute_filter`, one operator below the select list,
    // and the line travels the same way out.
    let error = err("SELECT 1;\nSELECT 1\nWHERE 1 / 0 = 1");
    assert_eq!(error.number, 8134);
    assert_eq!(error.line, 3);
}

// ---------------------------------------------------------------------------------------
// What keeps no line
// ---------------------------------------------------------------------------------------

#[test]
fn internal_errors_have_no_line() {
    // `SELECT TOP (150) PERCENT 1;` is 1031 on SQL Server; `execute_limit` does not raise
    // it and reports the internal error 50000 instead. An internal error describes a bug
    // of the engine, not a place in the query, so `errors::at` leaves its line at 0 even
    // though the node it came from has one.
    let error = err("SELECT 1;\nSELECT TOP (150) PERCENT 1");
    assert_eq!(error.number, 50000);
    assert_eq!(error.line, 0);
    assert!(
        error.message.starts_with("Internal error: "),
        "not an internal error: {error}"
    );
}

#[test]
fn a_top_error_of_the_client_does_carry_a_line() {
    // The other half of the rule: 127 and 1060 are real SQL Server errors, so they are
    // lined like the rest. This executor-level test goes through `execute`, whose first
    // step is the compile-time check; `session` compiles foldable values before any
    // statement of the batch runs. Both paths preserve the same source line.
    let negative = err("SELECT 1;\nSELECT TOP (-1) 1");
    assert_eq!(negative.number, 127);
    assert_eq!(negative.line, 2);

    let null = err("SELECT 1;\nSELECT 2;\nSELECT TOP (CAST(NULL AS int)) 1");
    assert_eq!(null.number, 1060);
    assert_eq!(null.line, 3);
}

// ---------------------------------------------------------------------------------------
// The numbers, which follow the target type
// ---------------------------------------------------------------------------------------

#[test]
fn overflow_numbers_follow_the_target_type() {
    // An overflow towards `tinyint` is **220** and names the type and the value; one
    // towards `int` is **8115** and names the target only. Both carry state 2, and the
    // number does not simply follow the width of the target.
    let tinyint = err("SELECT 1;\nSELECT CAST(300 AS tinyint)");
    assert_eq!(tinyint.number, 220);
    assert_eq!(tinyint.severity, 16);
    assert_eq!(tinyint.state, 2);
    assert!(tinyint.message.contains("tinyint"), "{}", tinyint.message);
    assert!(tinyint.message.contains("300"), "{}", tinyint.message);
    assert_eq!(tinyint.line, 2);

    let int = err("SELECT 1;\nSELECT CAST(3000000000 AS int)");
    assert_eq!(int.number, 8115);
    assert_eq!(int.severity, 16);
    assert_eq!(int.state, 2);
    assert!(int.message.contains("int"), "{}", int.message);
    assert_eq!(int.line, 2);
}

// ---------------------------------------------------------------------------------------
// What the error stops
// ---------------------------------------------------------------------------------------

#[test]
fn the_statement_that_raised_produces_nothing() {
    // The executor's half: the first statement answers, the second raises and `execute`
    // returns the error instead of a `RowSet`. SQL Server goes on to the third statement
    // of the batch — three result sets — which is `session`'s to reproduce, not this
    // crate's.
    let sets = outcomes("SELECT 1;\nSELECT 1 / 0;\nSELECT 2;");
    let error = sets.expect_err("the second statement raises");
    assert_eq!(error.number, 8134);
    assert_eq!(error.line, 2);
}
