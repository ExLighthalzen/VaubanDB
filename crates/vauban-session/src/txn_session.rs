//! The transaction of one session: the one a `BEGIN TRANSACTION` opened, kept open across
//! the statements of a batch and across batches, its TDS transaction descriptor, and the
//! one-statement transaction a statement outside it runs in.
//!
//! # What the wire shows
//!
//! The outermost `BEGIN` opens the transaction and announces it with an ENVCHANGE of type 8
//! carrying the descriptor; a `BEGIN` at a deeper level just counts one more. `COMMIT` at the
//! outermost level announces type 9 and closes the transaction, a nested `COMMIT` counts one
//! less. `ROLLBACK` with no name announces type 10 and closes everything; `ROLLBACK` to a
//! savepoint, and `SAVE TRANSACTION`, change nothing on the wire. A batch that ends with a
//! transaction open leaves it open and raises nothing.
//!
//! # Who owns what
//!
//! [`SessionState::txn`] holds the transaction between statements. The executor keeps its own
//! [`ExecSession`] while it runs one: [`statement_txn`] copies the session transaction into
//! it, and [`finish_statement`] reads back what the executor left, emits the ENVCHANGE and
//! updates [`SessionState`]. The copy is what lets the executor's `BEGIN`/`COMMIT`/`ROLLBACK`
//! change the session transaction without knowing the session.

use vauban_binder::TxnStatement;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, RowSink};
use vauban_planner::PhysicalStatement;
use vauban_tds::EnvChange;
use vauban_txn::TxnHandle;

use crate::Engine;
use crate::cancel::CancelHandle;
use crate::eval_context::SessionEvalContext;
use crate::set_options::{
    apply_deadlock_priority, begin_autocommit_txn, guard_snapshot_table_read, sync_exec_session,
};
use crate::sink::ResultSink;
use crate::state::SessionState;

/// The transaction a `BEGIN TRANSACTION` opened, as the session holds it between statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionTxn {
    /// The handle of the transaction manager.
    pub handle: TxnHandle,
    /// The TDS descriptor the opening ENVCHANGE announced, repeated by the closing one.
    pub descriptor: u64,
    /// How many `BEGIN TRANSACTION` are open, the value of `@@TRANCOUNT`.
    pub depth: i32,
}

/// The transaction one statement runs in, handed back to [`finish_statement`].
#[derive(Debug, Clone)]
pub(crate) enum StatementTxn {
    /// The session transaction is open: the statement ran in it and it stays open.
    Explicit,
    /// No session transaction: one was opened for this statement alone, to commit or to roll
    /// back once it returned.
    Autocommit(TxnHandle),
    /// Autocommit, but the statement failed before a handle was opened.
    Skipped,
}

/// The verb of a transaction statement, which decides between the two closing ENVCHANGE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxnKind {
    /// `BEGIN TRANSACTION`.
    Begin,
    /// `COMMIT` / `COMMIT WORK`.
    Commit,
    /// `ROLLBACK` with no name.
    Rollback,
    /// Any other statement, a `SAVE` or a `ROLLBACK` to a savepoint included.
    Other,
}

/// The verb of the transaction statement `statement` is, [`TxnKind::Other`] for the rest.
pub(crate) fn kind_of(statement: &PhysicalStatement) -> TxnKind {
    match statement {
        PhysicalStatement::Transaction(TxnStatement::Begin { .. }) => TxnKind::Begin,
        PhysicalStatement::Transaction(TxnStatement::Commit { .. }) => TxnKind::Commit,
        PhysicalStatement::Transaction(TxnStatement::Rollback { .. }) => TxnKind::Rollback,
        _ => TxnKind::Other,
    }
}

/// The transaction `statement` runs in, copied into `exec` so the executor sees it.
pub(crate) fn statement_txn(
    state: &SessionState,
    engine: &Engine,
    exec: &mut ExecSession,
) -> SqlResult<StatementTxn> {
    sync_exec_session(state, exec);
    match &state.txn {
        Some(txn) => {
            exec.txn = Some(txn.handle.clone());
            exec.trancount = txn.depth;
            apply_deadlock_priority(&engine.txn, &txn.handle, state.options.deadlock_priority);
            Ok(StatementTxn::Explicit)
        }
        None => {
            exec.txn = None;
            exec.trancount = 0;
            let handle = begin_autocommit_txn(state, &engine.storage, &engine.txn)?;
            apply_deadlock_priority(&engine.txn, &handle, state.options.deadlock_priority);
            Ok(StatementTxn::Autocommit(handle))
        }
    }
}

/// Runs one bound statement in a transaction of its own, committed on success and rolled back
/// on failure (autocommit: `@@TRANCOUNT` stays 0), or in the session transaction when one is
/// open.
pub(crate) fn execute_in_a_transaction(
    state: &SessionState,
    engine: &Engine,
    exec: &mut ExecSession,
    cancel: &CancelHandle,
    bound: &PhysicalStatement,
    sink: &mut dyn RowSink,
) -> (SqlResult<ExecOutcome>, StatementTxn) {
    if let Err(err) = guard_snapshot_table_read(state, &engine.storage, &engine.txn, bound) {
        sync_exec_session(state, exec);
        let txn = match &state.txn {
            Some(session_txn) => {
                exec.txn = Some(session_txn.handle.clone());
                exec.trancount = session_txn.depth;
                apply_deadlock_priority(
                    &engine.txn,
                    &session_txn.handle,
                    state.options.deadlock_priority,
                );
                StatementTxn::Explicit
            }
            None => StatementTxn::Skipped,
        };
        return (Err(err), txn);
    }
    let txn = match statement_txn(state, engine, exec) {
        Ok(txn) => txn,
        Err(err) => return (Err(err), StatementTxn::Skipped),
    };
    let handle = match &txn {
        StatementTxn::Explicit => state
            .txn
            .as_ref()
            .map(|session_txn| session_txn.handle.clone()),
        StatementTxn::Autocommit(handle) => Some(handle.clone()),
        StatementTxn::Skipped => None,
    };
    let Some(handle) = handle else {
        return (
            Err(SqlError::from(InternalError::Bug(
                "execute_in_a_transaction: the session transaction has no handle".to_owned(),
            ))),
            txn,
        );
    };
    let snap = engine.txn.statement_snapshot(&handle);
    let eval = SessionEvalContext::deferred(state, &engine.catalog, &handle);
    let token = cancel.token();
    let mut exec_ctx = ExecContext::scalar(&eval, state.options.to_binder())
        .with_engine(engine.storage.as_ref(), &engine.txn, &snap)
        .with_catalog(&engine.catalog)
        .with_handle(&handle)
        .with_cancel(&token)
        .with_session(exec);
    (vauban_executor::execute(bound, &mut exec_ctx, sink), txn)
}

/// Reads back what the executor left, emits the ENVCHANGE an opening or a closing statement
/// deserves, and updates [`SessionState`].
///
/// `txn` is what [`statement_txn`] handed out, `kind` the verb of the statement, and
/// `succeeded` whether the statement returned without an error: the autocommit transaction is
/// committed on success and rolled back otherwise.
pub(crate) fn finish_statement(
    state: &mut SessionState,
    engine: &Engine,
    exec: &mut ExecSession,
    txn: StatementTxn,
    kind: TxnKind,
    succeeded: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    if let StatementTxn::Autocommit(handle) = txn {
        if succeeded {
            engine.txn.commit(handle)?;
        } else {
            let _ = engine.txn.rollback(handle);
        }
    }
    match (state.txn.take(), exec.txn.clone()) {
        (None, Some(handle)) => {
            apply_deadlock_priority(&engine.txn, &handle, state.options.deadlock_priority);
            // A transaction the wire announces carries a non-zero descriptor: an exhausted
            // descriptor space is an internal error, not a silent 0 on an open transaction.
            let Some(descriptor) = state.allocate_transaction_descriptor() else {
                return Err(bug(
                    "finish_statement: the transaction descriptor space is exhausted",
                ));
            };
            state.txn = Some(SessionTxn {
                handle,
                descriptor,
                depth: exec.trancount,
            });
            state.trancount = exec.trancount;
            state.transaction_descriptor = descriptor;
            sink.env_change(&EnvChange::BeginTransaction(descriptor))?;
        }
        (Some(existing), None) => {
            let descriptor = existing.descriptor;
            state.trancount = 0;
            state.transaction_descriptor = 0;
            let change = match kind {
                TxnKind::Commit => EnvChange::CommitTransaction(descriptor),
                _ => EnvChange::RollbackTransaction(descriptor),
            };
            sink.env_change(&change)?;
        }
        (Some(existing), Some(handle)) => {
            let descriptor = existing.descriptor;
            state.txn = Some(SessionTxn {
                handle,
                descriptor,
                depth: exec.trancount,
            });
            state.trancount = exec.trancount;
        }
        (None, None) => {
            state.trancount = 0;
        }
    }
    Ok(())
}

/// What a batch does when it ends with a transaction still open: nothing.
///
/// The transaction stays open for the next batch and raises nothing: 266 belongs to an
/// `EXECUTE` whose transaction count moved, the nested-batch work.
pub(crate) fn end_of_batch(state: &SessionState, sink: &mut dyn ResultSink) -> SqlResult<()> {
    let _ = (state, sink);
    Ok(())
}

/// Rolls back the session transaction, for a connection that ends.
pub(crate) fn rollback_all(state: &mut SessionState, engine: &Engine) -> SqlResult<()> {
    if let Some(txn) = state.txn.take() {
        engine.txn.rollback(txn.handle)?;
    }
    state.trancount = 0;
    state.transaction_descriptor = 0;
    Ok(())
}

/// The internal error 50000 for a broken precondition.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
