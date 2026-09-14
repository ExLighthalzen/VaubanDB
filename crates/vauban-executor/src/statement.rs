//! The entry point of the executor: one bound statement in, one outcome out.
//!
//! One statement, not a batch. `SELECT 1; SELECT 2;` is **two** statements, so two calls of
//! [`execute`], two [`RowSet`](crate::RowSet)s and two `DONE` tokens. The batch itself is
//! `parser`'s — `parse_batch` answers a `Batch` of statements — and the **loop** over it is
//! `session`'s; it is the loop, not the notion, that this crate does not hold.

use vauban_binder::BoundStatement;
use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::compile::compile;
use crate::context::ExecContext;
use crate::ddl::execute_ddl;
use crate::plan::execute_plan;
use crate::row::ExecOutcome;

/// Runs one bound statement against `ctx` and returns what it produced.
///
/// The result set is **materialised** in the [`ExecOutcome::Rows`] it answers: there is
/// no `ResultSink` here (see the crate documentation).
///
/// # How many rows
///
/// `RowSet.rows.len()` is the number of rows the statement produced, and the number
/// `session` needs: it goes into the `DONE` token (`done(Some(n), more)`) and into
/// `@@ROWCOUNT`. The executor neither reads nor writes `@@ROWCOUNT` — it has no session
/// state to write it into — so a caller that forgets to post it leaves the variable stale.
/// A `SELECT` answers `Rows`, zero rows included: `SELECT 1 WHERE 1 = 0` sends its column
/// metadata and no row, not "no result set" (`tests/execute_select.rs`).
///
/// # Errors
///
/// The compile-time checks of [`crate::compile`] first — a client sees those **without**
/// the column metadata of the statement, which is why `session` runs them itself — then
/// the runtime errors, each a `SqlError` with the number SQL Server uses (8134 divide by
/// zero, 8115 overflow, 245 conversion…). Two numbers are not raised yet: 1031 (a `TOP`
/// percentage outside 0 to 100) and 1014 (a `NULL` `TOP` percentage) come out as the
/// internal error 50000, so on those two shapes a client tells the engines apart,
/// `SELECT TOP (150) PERCENT 1;` answering 1031 on SQL Server and 50000 here (the rustdoc
/// of `plan::execute_limit`). The statement stops at the first error and this function
/// returns it; whether the **batch** stops there too is `session`'s decision.
pub fn execute(stmt: &BoundStatement, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    // The checks SQL Server runs while it compiles the statement come first, and a caller
    // that only knows `execute` still gets them in that order. `session` calls
    // [`crate::compile`] itself, because the boundary between the two is where the column
    // metadata of the statement goes out (`compile.rs`).
    compile(stmt, ctx)?;
    // No `_ =>` arm: a variant added to `BoundStatement` must break this file rather than
    // be silently reported as something else, exactly as `binder::bind` does.
    match stmt {
        BoundStatement::Query(plan) => Ok(ExecOutcome::Rows(execute_plan(plan, ctx)?)),
        BoundStatement::Ddl(ddl) => execute_ddl(ddl, ctx),
        // `USE` changes no metadata and reads no row: the executor accepts it and answers
        // `NoRows`, and `session` reads the target out of the bound statement to switch the
        // current database and send its ENVCHANGE. Nothing is done here, so nothing has to
        // be undone when the switch fails.
        BoundStatement::Use { .. } => Ok(ExecOutcome::NoRows),
        // The statements that are bound but not executed yet: each answers the internal
        // error 50000 until its arm is written.
        BoundStatement::Insert(_) => Err(bug("execute: INSERT is not implemented yet")),
        BoundStatement::Update(_) | BoundStatement::Delete(_) => {
            Err(bug("execute: UPDATE and DELETE are not implemented yet"))
        }
        BoundStatement::SetVariable { .. }
        | BoundStatement::Declare(_)
        | BoundStatement::If { .. }
        | BoundStatement::While { .. }
        | BoundStatement::Block(_)
        | BoundStatement::Break
        | BoundStatement::Continue
        | BoundStatement::Return(_)
        | BoundStatement::Print(_) => Err(bug(
            "execute: variables and control of flow are not implemented yet",
        )),
        BoundStatement::Transaction(_) => Err(bug(
            "execute: the transaction statements are not implemented yet",
        )),
    }
}

/// An engine bug, reported to the client as the generic error 50000.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_binder::{BindContext, SessionOptions, bind};
    use vauban_parser::{ParseOptions, parse_batch};
    use vauban_sysfn::StaticContext;

    /// `USE d` answers [`ExecOutcome::NoRows`] once it has bound, and answers it in a
    /// **scalar** context: no storage, no catalogue, no transaction handle.
    ///
    /// That is the counter-proof of the arm: the DDL of the same file needs all three and
    /// answers the internal 50000 without them
    /// (`ddl::tests::a_context_without_a_handle_is_a_bug`), so the `NoRows` below is the
    /// executor doing nothing rather than the context hiding a failure. What the statement
    /// changes is `session`'s.
    ///
    /// `NoRows` carries no [`RowSet`](crate::RowSet) — it is a unit variant — so there is no
    /// schema for `session` to turn into COLMETADATA; the second assertion states it on the
    /// value rather than on the type.
    #[test]
    fn use_is_norows() {
        let text = "USE master;";
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let bind_ctx = BindContext::scalar(text, SessionOptions::default());
        let bound = bind(&batch.statements[0], &bind_ctx).expect("USE binds");
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        let outcome = execute(&bound, &mut ctx).expect("USE runs without an engine");
        assert!(matches!(outcome, ExecOutcome::NoRows));
        if let ExecOutcome::Rows(set) = outcome {
            panic!(
                "USE published a schema of {} column(s)",
                set.schema.columns.len()
            );
        }
    }
}
