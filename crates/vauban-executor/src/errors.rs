//! The line a runtime error carries, and the one rule about where it comes from.
//!
//! `types` and `sysfn` raise the right number, the right severity and the right message,
//! but they know nothing of the batch the query came from: the errors they build carry
//! `line == 0`. The line is the executor's to add, because the executor is the first layer
//! that holds a [`vauban_binder::BoundExpr`], and a `BoundExpr` carries the line of the AST
//! node it was bound from.
//!
//! # The line a client sees is the **statement**'s, and `session` puts it on
//!
//! SQL Server reports the line the failing **statement** starts on, not the line of the
//! sub-expression that raised. The executor cannot know that line: a line lives on
//! [`vauban_binder::BoundExpr`] and nowhere else, `BoundStatement::Query` holding a bare
//! `LogicalPlan` whose nodes carry none. The line answered here is therefore the failing
//! sub-expression's, which is the statement's own line whenever the statement fits on one
//! line, and one line too far down otherwise.
//!
//! What closes the gap is one layer up: `vauban_session::batch` holds the `&Statement` and
//! its `Span`, and puts the statement's line over the one this module produced, for the
//! errors `execute` returns. **Three** cases escape that override and keep the line given
//! here — the three that `vauban_session::batch::at_statement` lets through:
//!
//! - **127**, a `TOP` row count below zero, and **1060**, a `TOP` row count that is `NULL`.
//!   SQL Server counts these two on the `TOP`'s **row-count expression** and not on the
//!   statement: with the statement starting on line 3, `SELECT` / `TOP (-1)` / `1` / `,`
//!   / `2;` answers **4**, `SELECT` / `TOP (NULL)` / … the same way, and `SELECT` / `TOP`
//!   / `(` / `-1` / `)` / `1;` answers **6** — the line of the literal, not of the `TOP`
//!   keyword (4) nor of the parenthesis (5). That is exactly what [`crate::plan`] gives
//!   them (`eval_budget`, `top.expr.line`).
//!
//!   These are **two numbers, not a class of errors**: 1062 and 1031, raised while the
//!   same `TOP` clause is compiled, answer respectively the **last** line of the statement
//!   (**7** on the five-line shape above) and the line the **select list** starts on
//!   (**5**), neither being the row count. And nothing about how an error is raised
//!   predicts the side it falls on: 207 is severity **16** and takes its node's line, while
//!   536 sends no result set and takes its statement's line. Whoever adds 1031 or
//!   1062 has to establish the node of that number rather than follow this bullet.
//!
//! - **50000**, the internal error, which carries no line at all (next section): [`at`]
//!   refuses to place it and `session` has nothing to write over.
//!
//! Everything else that comes out of `execute` is a run-time error and gets the statement's
//! line; the shapes behind that rule are tabled in the module header of
//! `vauban_session::batch`, next to the code that applies it.
//!
//! The line this module produces is still the executor's best answer on its own boundary,
//! and `tests/runtime_errors.rs` pins it: a caller of `execute` that offers no statement
//! line — the tests of this crate — keeps getting the sub-expression's.
//!
//! # Whether the following statement runs is not an executor decision
//!
//! The executor stops at each run-time error and returns it. SQL Server stops the
//! surrounding batch for some numbers and not for others: 8134 (division by zero), 220
//! (tinyint overflow), 506 (invalid `LIKE` escape), 3623 (invalid floating-point
//! operation) and 9810 (datepart unsupported by a type) let the next statement run,
//! whereas 245 (failed integer
//! conversion), 241 (failed date conversion), 127 (negative dynamic `TOP`), 292 (smallmoney
//! output too narrow) and 8169 (invalid uniqueidentifier conversion) stop it. Severity and
//! state do not separate the families: 8134 and 245 are both severity 16, state 1.
//! [`vauban_errors::ErrorDef::batch_scope`] therefore carries the property per number, and
//! `vauban_session::batch` reads it after this module returns the error. `XACT_ABORT ON`
//! is also applied there and stops the batch for either scope.
//!
//! # Internal errors keep no line
//!
//! The generic error 50000 is not a SQL Server error: it is the project's way of reporting
//! a broken precondition (`vauban_errors::InternalError`). An ordinary client **does** meet
//! it — `SELECT TOP (150) PERCENT 1;` answers 1031 on SQL Server and 50000 here — but what
//! it names is a hole in this engine, not a place in the query the client wrote. Giving it
//! a line would suggest otherwise. [`at`] leaves it alone.
//!
//! # What the line is counted on
//!
//! The **text of the batch the server received**, 1-based: `session` hands the parser the
//! whole batch and the parser puts the line of each token on its span.

use vauban_errors::SqlError;

/// The number `vauban_errors::InternalError` converts to, and the only number [`at`]
/// refuses to place in the batch.
const INTERNAL_ERROR: u32 = 50000;

/// Puts `line` on `err`, unless `err` already knows better.
///
/// Two errors keep the line they came with:
///
/// - one that already carries a non-zero line: a layer that has already lined an error
///   knew the batch better than its caller does (the binder puts the line on 1001 and 1002
///   itself, and the message repeats it), and the parents it passes through on the way out
///   have nothing better to offer — the line of the statement, which is what SQL Server
///   answers for a run-time error, is not among the things they hold, and `session` is
///   what writes it over this one (see the module header);
/// - the internal error 50000, which describes a bug of the engine and not a place in the
///   query.
///
/// Callers wrap the **fallible call**, not the recursive evaluation of a sub-expression:
/// `types::eval_binary`, `types::convert`, `FunctionDef::eval`, `types::compare` and
/// `Collation::like` all answer errors with no line, and the `BoundExprKind` being
/// evaluated is the one whose `line` is used. Several kinds share a call — `eval_binary`
/// is reached from `Arith` (through `expr::eval_arith`), `Negate` and `BitNot`, and
/// `compare` from `Compare` and `In` — so the wrapping is written at each of those call
/// sites, not once per callee.
pub(crate) fn at(err: SqlError, line: u32) -> SqlError {
    if err.line != 0 || err.number == INTERNAL_ERROR {
        return err;
    }
    err.with_line(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_errors::InternalError;

    #[test]
    fn a_line_is_put_on_an_error_that_has_none() {
        let err = at(SqlError::divide_by_zero(), 4);
        assert_eq!(err.number, 8134);
        assert_eq!(err.line, 4);
    }

    #[test]
    fn an_error_that_already_has_a_line_keeps_it() {
        let already_lined = SqlError::divide_by_zero().with_line(3);
        assert_eq!(at(already_lined, 2).line, 3);
    }

    #[test]
    fn line_zero_is_not_a_line() {
        // Wrapping with `0` is the identity: a caller that has no line to offer — the
        // binder never saw the node — leaves the error exactly as it found it.
        assert_eq!(at(SqlError::divide_by_zero(), 0).line, 0);
    }

    #[test]
    fn an_internal_error_gets_no_line() {
        let bug = SqlError::from(InternalError::Bug("x".to_owned()));
        let wrapped = at(bug, 7);
        assert_eq!(wrapped.number, 50000);
        assert_eq!(wrapped.line, 0);
    }
}
