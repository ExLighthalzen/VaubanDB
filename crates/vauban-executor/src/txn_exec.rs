//! Transactions: `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVE TRANSACTION`, `@@TRANCOUNT`,
//! `XACT_ABORT`, and the frame around each statement that gives it its atomicity.

use vauban_binder::TxnStatement;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_txn::TransactionManager;

use crate::context::ExecContext;
use crate::row::ExecOutcome;

/// What the executor does before a statement runs: take a savepoint for the atomicity of
/// the statement when a transaction is open.
///
/// # Errors
///
/// `InternalError::Bug` when the TransactionManager is missing from the context while a
/// transaction is open.
pub fn begin_statement(ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    let Some(session) = ctx.session.as_deref_mut() else {
        return Ok(());
    };
    let Some(ref handle) = session.txn else {
        return Ok(());
    };
    let Some(txn_mgr) = ctx.txn else {
        return Err(bug("begin_statement: no TransactionManager in the context"));
    };
    let sp = txn_mgr.savepoint(handle)?;
    session.stmt_savepoint = Some(sp);
    Ok(())
}

/// What the executor does after a statement ran, on success as on error: rolls the
/// statement back to its savepoint on error (unless `XACT_ABORT ON`), or discards the
/// savepoint on success.
///
/// On `XACT_ABORT ON` the whole transaction is rolled back.
///
/// # Errors
///
/// `InternalError::Bug` when the TransactionManager is missing from the context while a
/// transaction is open, or when a rollback to the statement savepoint fails.
pub fn end_statement(ctx: &mut ExecContext<'_>, result: &SqlResult<ExecOutcome>) -> SqlResult<()> {
    let Some(session) = ctx.session.as_deref_mut() else {
        return Ok(());
    };
    if result.is_ok() {
        session.stmt_savepoint = None;
        return Ok(());
    }
    if session.xact_abort {
        session.stmt_savepoint = None;
        if let Some(handle) = session.txn.take() {
            let Some(txn_mgr) = ctx.txn else {
                return Err(bug("end_statement: no TransactionManager in the context"));
            };
            txn_mgr.rollback(handle)?;
            session.trancount = 0;
            session.savepoints.clear();
        }
    } else if let Some(sp) = session.stmt_savepoint.take()
        && let Some(ref handle) = session.txn
    {
        let Some(txn_mgr) = ctx.txn else {
            return Err(bug("end_statement: no TransactionManager in the context"));
        };
        if txn_mgr.rollback_to(handle, sp).is_err() {
            return Err(bug(
                "end_statement: rollback_to failed on the statement savepoint",
            ));
        }
    }
    Ok(())
}

/// Runs one transaction statement.
///
/// # Errors
///
/// The errors the transaction manager returns, or the transaction errors 3902 (`COMMIT`
/// without a matching `BEGIN`) and 3903 (`ROLLBACK` without a matching `BEGIN`).
pub(crate) fn execute(stmt: &TxnStatement, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    let txn_mgr = ctx
        .txn
        .ok_or_else(|| bug("a transaction statement needs a TransactionManager"))?;
    let session = ctx.session()?;
    match stmt {
        TxnStatement::Begin { name: _, mark: _ } => begin(session, txn_mgr),
        TxnStatement::Commit { name: _ } => commit(session, txn_mgr),
        TxnStatement::Rollback { name } => rollback(session, txn_mgr, name.as_deref()),
        TxnStatement::Save { name } => save(session, txn_mgr, name),
    }
}

/// `BEGIN TRAN[SACTION] [name [WITH MARK ['text']]]`.
fn begin(session: &mut crate::ExecSession, txn_mgr: &TransactionManager) -> SqlResult<ExecOutcome> {
    if session.trancount == 0 {
        let handle = txn_mgr.begin(session.isolation);
        session.txn = Some(handle);
        session.trancount = 1;
    } else {
        session.trancount += 1;
    }
    Ok(ExecOutcome::NoRows)
}

/// `COMMIT [TRAN[SACTION] [name]]`.
fn commit(
    session: &mut crate::ExecSession,
    txn_mgr: &TransactionManager,
) -> SqlResult<ExecOutcome> {
    if session.trancount == 0 {
        return Err(SqlError::commit_without_begin());
    }
    if session.trancount == 1 {
        let handle = session
            .txn
            .take()
            .ok_or_else(|| bug("COMMIT: session.txn is None while trancount > 0"))?;
        txn_mgr.commit(handle)?;
        session.trancount = 0;
        session.savepoints.clear();
    } else {
        session.trancount -= 1;
    }
    Ok(ExecOutcome::NoRows)
}

/// `ROLLBACK [TRAN[SACTION]]` without a name, and `ROLLBACK [TRAN[SACTION] name]`.
fn rollback(
    session: &mut crate::ExecSession,
    txn_mgr: &TransactionManager,
    name: Option<&str>,
) -> SqlResult<ExecOutcome> {
    match name {
        None => {
            if session.trancount == 0 {
                return Err(SqlError::rollback_without_begin());
            }
            let handle = session
                .txn
                .take()
                .ok_or_else(|| bug("ROLLBACK: session.txn is None while trancount > 0"))?;
            txn_mgr.rollback(handle)?;
            session.trancount = 0;
            session.savepoints.clear();
        }
        Some(n) => {
            let Some(ref handle) = session.txn else {
                return Err(SqlError::rollback_without_begin());
            };
            let sp = session.savepoints.get(n).copied().ok_or_else(|| {
                SqlError::new(
                    6401,
                    16,
                    1,
                    format!(
                        "Cannot roll back {n}. No transaction or savepoint of that name was found."
                    ),
                )
            })?;
            txn_mgr.rollback_to(handle, sp)?;
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// `SAVE TRAN[SACTION] name`.
fn save(
    session: &mut crate::ExecSession,
    txn_mgr: &TransactionManager,
    name: &str,
) -> SqlResult<ExecOutcome> {
    let Some(ref handle) = session.txn else {
        return Err(SqlError::new(
            628,
            0,
            1,
            "Cannot use SAVE TRANSACTION within a distributed transaction.",
        ));
    };
    let sp = txn_mgr.savepoint(handle)?;
    session.savepoints.insert(name.to_owned(), sp);
    Ok(ExecOutcome::NoRows)
}

/// The internal error 50000 for a broken precondition.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
