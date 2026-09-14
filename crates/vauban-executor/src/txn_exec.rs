//! Transactions: `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVE TRANSACTION`, `@@TRANCOUNT`,
//! `XACT_ABORT`, and the frame around each statement that gives it its atomicity.
//!
//! The frame exists, the statements do not yet: [`begin_statement`] and
//! [`end_statement`] do nothing and answer `Ok(())`, and [`execute`] answers the internal
//! error 50000.

use vauban_binder::TxnStatement;
use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::context::ExecContext;
use crate::row::ExecOutcome;

/// What the executor does before a statement runs: the savepoint of its atomicity.
///
/// # Errors
///
/// The frame does nothing yet and answers `Ok(())`.
pub(crate) fn begin_statement(ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    let _ = ctx;
    Ok(())
}

/// What the executor does after a statement ran, on success as on error: releases or
/// rolls back to the savepoint of [`begin_statement`], posts `@@ERROR`.
///
/// # Errors
///
/// The frame does nothing yet and answers `Ok(())`.
pub(crate) fn end_statement(
    ctx: &mut ExecContext<'_>,
    result: &SqlResult<ExecOutcome>,
) -> SqlResult<()> {
    let _ = (ctx, result);
    Ok(())
}

/// Runs one transaction statement.
///
/// # Errors
///
/// The internal error 50000, until the statements are written.
pub(crate) fn execute(stmt: &TxnStatement, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    let _ = (stmt, ctx);
    Err(SqlError::from(InternalError::Bug(
        "execute: the transaction statements are not implemented yet".to_owned(),
    )))
}
