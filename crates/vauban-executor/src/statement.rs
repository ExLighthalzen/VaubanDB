//! The entry point of the executor: one physical statement in, one outcome out, the rows
//! handed to a [`RowSink`] as they are produced.
//!
//! One statement, not a batch. `SELECT 1; SELECT 2;` is **two** statements, so two calls of
//! [`execute`], two result sets and two `DONE` tokens. The batch itself is `parser`'s —
//! `parse_batch` answers a `Batch` of statements — and the **loop** over it is `session`'s;
//! it is the loop, not the notion, that this crate does not hold.
//!
//! # The frame around a statement
//!
//! [`execute`] runs the compile-time checks first, then `txn_exec::begin_statement`, then
//! the statement itself, then `txn_exec::end_statement` with what the statement answered,
//! whether it succeeded or not: the two hooks are where the atomicity of a statement and
//! `@@ERROR` will be posted.
//!
//! # The driving loop of a query
//!
//! [`run_query`] is the one caller that opens the root of an operator tree. It reads the
//! cancellation token before opening the tree, every [`CANCEL_CHECK_ROWS`] rows, and once
//! the tree is exhausted, so that an operator that materialises its input and stops
//! early on the token is reported as cancelled and not as a short result set. The column
//! metadata goes to the sink after the tree is built and **before** it is opened: an
//! error raised at `open`, the 127 of `SELECT TOP (@@SPID * 0 - 1) 1;`, reaches the
//! client after an empty result set, where the same 127 folded at compile time reaches
//! it before any metadata (`compile.rs`).

use vauban_binder::DdlStatement;
use vauban_errors::SqlResult;
use vauban_planner::{PhysicalPlan, PhysicalStatement};

use crate::compile::compile;
use crate::context::{CANCEL_CHECK_ROWS, CollectSink, ExecContext, RowSink};
use crate::ddl::execute_ddl;
use crate::operator::build_operator;
use crate::row::{ExecOutcome, RowSet};
use crate::{control, dml, txn_exec};

/// Runs one physical statement against `ctx`, handing its rows to `sink`, and returns
/// what it produced.
///
/// # How many rows
///
/// [`ExecOutcome::Rows`] carries the number of rows the sink received, which is the
/// number `session` needs: it goes into the `DONE` token (`done(Some(n), more)`) and into
/// `@@ROWCOUNT` (`tests/operator_basics.rs`, `row_count_is_what_the_sink_saw`). A
/// `SELECT` answers `Rows`, zero rows included: `SELECT 1 WHERE 1 = 0` sends its column
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
/// `SELECT TOP (150) PERCENT 1;` answering 1031 on SQL Server and 50000 here (`plan.rs`).
/// The statement stops at the first error and this function returns it; whether the
/// **batch** stops there too is `session`'s decision. A statement whose execution is not
/// written yet answers the internal error 50000 with a message that names it.
pub fn execute<'a>(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    // The checks SQL Server runs while it compiles the statement come first, and a caller
    // that only knows `execute` still gets them in that order. `session` calls
    // [`crate::compile`] itself, because the boundary between the two is where the column
    // metadata of the statement goes out (`compile.rs`).
    compile(stmt, ctx)?;
    txn_exec::begin_statement(ctx)?;
    let result = dispatch(stmt, ctx, sink);
    txn_exec::end_statement(ctx, &result)?;
    result
}

/// Runs `stmt` with a [`CollectSink`] and answers the outcome with what the sink saw.
///
/// The `RowSet` carries the schema the sink was announced, or an empty one for a
/// statement that announced no columns; its rows are the rows the sink received, in
/// order.
///
/// # Errors
///
/// As [`execute`].
pub fn execute_collect(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<(ExecOutcome, RowSet)> {
    let mut sink = CollectSink::new();
    let outcome = execute(stmt, ctx, &mut sink)?;
    let schema = sink.schema.unwrap_or(vauban_binder::OutputSchema {
        columns: Vec::new(),
    });
    Ok((
        outcome,
        RowSet {
            schema,
            rows: sink.rows,
        },
    ))
}

/// Hands the statement to the file that runs it.
fn dispatch<'a>(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    // No `_ =>` arm: a variant added to `PhysicalStatement` must break this file rather
    // than be silently reported as something else, exactly as `planner::plan` does.
    match stmt {
        PhysicalStatement::Query(plan) => run_query(plan, ctx, sink),
        PhysicalStatement::Ddl(ddl) => match ddl {
            DdlStatement::TruncateTable { name } => crate::dml::truncate::execute(name, ctx),
            _ => execute_ddl(ddl, ctx),
        },
        // `USE` changes no metadata and reads no row: the executor accepts it and answers
        // `NoRows`, and `session` reads the target out of the statement to switch the
        // current database and send its ENVCHANGE. Nothing is done here, so nothing has to
        // be undone when the switch fails.
        PhysicalStatement::Use { .. } => Ok(ExecOutcome::NoRows),
        PhysicalStatement::Insert(insert) => dml::insert::execute(insert, ctx),
        PhysicalStatement::SelectInto(select_into) => {
            dml::insert::execute_select_into(select_into, ctx)
        }
        PhysicalStatement::Update(update) => dml::update_delete::execute_update(update, ctx),
        PhysicalStatement::Delete(delete) => dml::update_delete::execute_delete(delete, ctx),
        PhysicalStatement::SetVariable { .. }
        | PhysicalStatement::Declare(_)
        | PhysicalStatement::If { .. }
        | PhysicalStatement::While { .. }
        | PhysicalStatement::Block(_)
        | PhysicalStatement::Break
        | PhysicalStatement::Continue
        | PhysicalStatement::Return(_)
        | PhysicalStatement::Print(_) => control::execute(stmt, ctx, sink),
        PhysicalStatement::Transaction(txn) => txn_exec::execute(txn, ctx),
        PhysicalStatement::Execute(stmt) => crate::execute::execute(stmt, ctx),
    }
}

/// Builds, opens, drives and closes the operator tree of `plan` (module documentation).
fn run_query<'a>(
    plan: &PhysicalPlan,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    if ctx.cancelled() {
        return Ok(ExecOutcome::Cancelled);
    }
    let mut root = build_operator(plan)?;
    sink.columns(root.schema())?;
    let driven = drive(root.as_mut(), ctx, sink);
    root.close();
    let count = driven?;
    if ctx.cancelled() {
        return Ok(ExecOutcome::Cancelled);
    }
    for info in ctx.take_infos() {
        sink.info(&info)?;
    }
    Ok(ExecOutcome::Rows(count))
}

/// Opens `root` and pulls its rows into `sink` until it is exhausted or the token is up;
/// answers how many rows the sink received. The caller closes the root, on success as on
/// error.
fn drive<'a>(
    root: &mut (dyn crate::Operator<'a> + 'a),
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn RowSink,
) -> SqlResult<u64> {
    root.open(ctx)?;
    let mut count = 0u64;
    while let Some(row) = root.next(ctx)? {
        sink.row(&row)?;
        count += 1;
        if count.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
            break;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_binder::{BindContext, SessionOptions, bind};
    use vauban_parser::{ParseOptions, parse_batch};
    use vauban_planner::{NoIndexes, PlanContext, plan};
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
    /// `NoRows` carries no [`RowSet`] — it is a unit variant — so there is no schema for
    /// `session` to turn into COLMETADATA; the second assertion states it on the sink,
    /// which was announced no columns.
    #[test]
    fn use_is_norows() {
        let text = "USE master;";
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let bind_ctx = BindContext::scalar(text, SessionOptions::default());
        let bound = bind(&batch.statements[0], &bind_ctx).expect("USE binds");
        let physical = plan(
            bound,
            &PlanContext {
                catalog: &NoIndexes,
            },
        )
        .expect("USE plans");
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        let mut sink = CollectSink::new();
        let outcome = execute(&physical, &mut ctx, &mut sink).expect("USE runs without an engine");
        assert!(matches!(outcome, ExecOutcome::NoRows));
        assert!(sink.schema.is_none(), "USE published no schema");
        assert!(sink.rows.is_empty());
    }
}
