//! `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVE TRANSACTION`: the translation of the four
//! statements the parser reads into the [`TxnStatement`](crate::bound::TxnStatement)
//! carried by [`BoundStatement::Transaction`].
//!
//! # Translation, not semantics
//!
//! A function of this file copies what the text carries — the name written after the
//! keyword, the description of a `WITH MARK` — and reads nothing else: `@@TRANCOUNT`, the
//! list of open savepoints and the `XACT_ABORT` of the session are not consulted, and the
//! four functions take no decision that depends on them. A `COMMIT` written outside a
//! transaction therefore binds, and the 3902 a client reads comes from the execution of
//! the bound statement, not from here (`tests/bind_txn_stmt.rs`,
//! `commit_and_rollback_bind_without_state_check`).
//!
//! # The name of a transaction, of a savepoint
//!
//! Kept as the text wrote it, case included, delimiters removed by the lexer: `BEGIN TRAN
//! T1` binds the name `T1`, `BEGIN TRAN [My Tran]` the name `My Tran`
//! (`begin_tran_with_name_keeps_the_name_as_written`). Matching that name against a
//! transaction or a savepoint belongs to the executor, which is why
//! [`TxnStatement::Rollback`](crate::bound::TxnStatement::Rollback) has a single `name`
//! field: the bound statement does not record whether `ROLLBACK TRAN s1` is meant to reach
//! a transaction or a savepoint (`rollback_to_savepoint_binds`), and `SAVE TRAN s1` is the
//! form whose name the grammar requires (`save_tran_binds`).
//!
//! The description of a `WITH MARK` is copied into
//! [`TxnStatement::Begin`](crate::bound::TxnStatement::Begin) as the parser hands it over,
//! the empty string standing for a `WITH MARK` written without one (`flow.rs`, module
//! header). Whether a marked transaction reaches a log is the executor's question
//! (`begin_tran_carries_the_text_of_with_mark`).
//!
//! # Forms that reach no arm of this file
//!
//! - `BEGIN DISTRIBUTED TRANSACTION` stops at the parser with 156 (syntax error near
//!   `DISTRIBUTED`), so no internal 50000 for it is raised from here
//!   (`distributed_transaction_is_refused_by_the_parser`).
//! - `BEGIN TRAN @n` and `COMMIT TRAN @n` stop at the parser with 102 (syntax error near
//!   `@n`): [`Statement::BeginTransaction`](vauban_parser::Statement) names a transaction
//!   with an `Ident` and has nowhere to put a variable, a deliberate difference from SQL
//!   Server. This file binds no expression
//!   (`a_transaction_named_by_a_variable_is_refused_by_the_parser`).
//! - `SET TRANSACTION ISOLATION LEVEL …`, `SET LOCK_TIMEOUT n` and `SET XACT_ABORT ON`
//!   parse as `Statement::SetOption`, which `session` applies to its own state before the
//!   binder is called (`session::batch::bind_batch`) and which `statement.rs` reports as
//!   an internal 50000 on the path where it does reach `bind`
//!   (`set_options_do_not_reach_the_binder`). The bound statement has no `SetOption`
//!   variant to hold them.

use vauban_errors::SqlResult;
use vauban_parser::Ident;

use crate::bound::{BoundStatement, TxnStatement};
use crate::context::BindContext;

/// Binds a `BEGIN TRAN[SACTION] [name [WITH MARK ['text']]]`.
///
/// # Errors
///
/// The signature stays fallible because the dispatch of `statement.rs` is written on
/// `SqlResult`. On the forms the parser produces, this function answers `Ok`: the
/// statement carries an optional name and an optional mark, and neither is checked against
/// a state (`tests/bind_txn_stmt.rs`, `begin_tran_with_name_keeps_the_name_as_written` and
/// `begin_tran_carries_the_text_of_with_mark`).
pub(crate) fn bind_begin(
    name: Option<&Ident>,
    mark: Option<&str>,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    // The bound form depends on the text alone; the context carries nothing this reads.
    let _ = ctx;
    Ok(BoundStatement::Transaction(TxnStatement::Begin {
        name: name.map(written),
        mark: mark.map(str::to_owned),
    }))
}

/// Binds a `COMMIT [TRAN[SACTION] [name]]`, `COMMIT WORK` included.
///
/// # Errors
///
/// On the forms the parser produces, this function answers `Ok`: the state a `COMMIT`
/// needs is the executor's (`commit_and_rollback_bind_without_state_check`).
pub(crate) fn bind_commit(
    name: Option<&Ident>,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = ctx;
    Ok(BoundStatement::Transaction(TxnStatement::Commit {
        name: name.map(written),
    }))
}

/// Binds a `ROLLBACK [TRAN[SACTION] [name]]`, `ROLLBACK WORK` included.
///
/// # Errors
///
/// On the forms the parser produces, this function answers `Ok`: which of a transaction
/// and a savepoint the name reaches is decided at run time (`rollback_to_savepoint_binds`).
pub(crate) fn bind_rollback(
    name: Option<&Ident>,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = ctx;
    Ok(BoundStatement::Transaction(TxnStatement::Rollback {
        name: name.map(written),
    }))
}

/// Binds a `SAVE TRAN[SACTION] name`.
///
/// # Errors
///
/// On the forms the parser produces, this function answers `Ok`: the grammar already
/// required the name, and the savepoint it creates is the executor's (`save_tran_binds`).
pub(crate) fn bind_save(name: &Ident, ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let _ = ctx;
    Ok(BoundStatement::Transaction(TxnStatement::Save {
        name: written(name),
    }))
}

/// The text of an identifier as it was written, case included, delimiters already removed
/// by the lexer.
///
/// A transaction name is not folded to a case here: the comparison the executor makes is
/// what decides which case matters, and folding would lose the information
/// (`begin_tran_with_name_keeps_the_name_as_written`).
fn written(name: &Ident) -> String {
    name.value.clone()
}
