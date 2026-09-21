//! Variables and the control of flow: `DECLARE`, `SET @x = e`, `SELECT @x = e`, `IF`,
//! `WHILE`, `BEGIN ... END`, `BREAK`, `CONTINUE`, `RETURN`, `PRINT`.
//!
//! # What one statement answers
//!
//! `DECLARE`, `SET`, `WHILE` and `PRINT` answer [`ExecOutcome::NoRows`]; `IF` and `Block`
//! answer what the statement they ran answered, and so pass a `RETURN` or a `BREAK` up the
//! tree. `BREAK` and `CONTINUE` answer themselves, and their enclosing `WHILE` reads them.
//!
//! A `WHILE` reads the cancellation token once per iteration, so an infinite loop ends by
//! [`ExecOutcome::Cancelled`] and not by waiting.
//!
//! # Variables
//!
//! `DECLARE` enters the name with `NULL` and its declared type. `SET` evaluates its value
//! and converts it to that type, unless the binder already wrapped it in the conversion
//! (`BoundStatement::SetVariable`); the conversion error, 245 or another, is the one
//! `types::convert` returns, unchanged. `SET` puts `@@ROWCOUNT` at 1, as SQL Server does.
//! Reading a name no `DECLARE` entered is a bug of the binder and answers the internal
//! error.
//!
//! A value written as a subquery — `SET @x = (SELECT MAX(col) FROM t)`, and the same in
//! the initializer of a `DECLARE` — is read outside a plan, so the plan of that subquery
//! travels on the statement rather than under an operator; [`eval_statement_expr`]
//! installs it for the time of the evaluation.
//!
//! # Assigning from rows
//!
//! `SELECT @x = col FROM t` sends no result set: it walks the rows of its plan and
//! re-runs the assignment on each one, so the row walked last leaves its value behind
//! and an `ORDER BY` decides which row that is (`tests/control.rs`,
//! `select_assign_keeps_the_last_row` and `select_assign_follows_the_order_by`). The
//! targets of one row are stored left to right, so a value that reads a target assigned
//! before it on the same list reads the current row (`select_assign_reads_a_target_assigned_before_it`).
//! Over a plan that produces nothing the variables are left as they were
//! (`select_assign_over_no_row_keeps_the_value`), where `SET @x = (SELECT …)` over the
//! same rows stores `NULL`. `@@ROWCOUNT` afterwards is the number of rows walked, `0`
//! included (`select_assign_sets_rowcount_to_the_rows_walked`,
//! `select_assign_over_no_row_sets_rowcount_to_zero`).

use vauban_binder::BoundExpr;
use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult};
use vauban_planner::{PhysicalPlan, PhysicalStatement, SubPlan};
use vauban_types::{SqlType, TypeInfo, Value, convert, default_display};

use crate::context::{CANCEL_CHECK_ROWS, ExecContext, RowSink};
use crate::errors::at;
use crate::expr::eval_expr;
use crate::operator::{Operator, build_operator};
use crate::row::ExecOutcome;

/// Runs one statement of the control of flow, the nested statements included.
///
/// # Errors
///
/// What evaluating a condition or a value raises, and the conversion error of a `SET`,
/// which keeps the number `types::convert` gave it. A statement that is not one of the
/// control-of-flow variants is a bug of `statement.rs`, not something a client reaches.
pub(crate) fn execute<'a>(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    match stmt {
        PhysicalStatement::Declare(declarations) => {
            for declaration in declarations {
                // The name is entered before its initial value is read, so a statement
                // that runs after a failed initializer reads the variable at `NULL`
                // rather than a name nothing declared (`tests/subquery.rs`,
                // `a_declare_whose_subquery_returns_two_rows_answers_512`).
                let session = ctx.session()?;
                session
                    .variables
                    .insert(declaration.name.clone(), Value::Null);
                session
                    .variable_types
                    .insert(declaration.name.clone(), declaration.ty.clone());
                let Some(expr) = &declaration.value else {
                    continue;
                };
                let value = eval_statement_expr(expr, &declaration.subplans, ctx)?;
                ctx.session()?
                    .variables
                    .insert(declaration.name.clone(), value);
            }
            Ok(ExecOutcome::NoRows)
        }
        PhysicalStatement::SetVariable {
            name,
            value,
            subplans,
        } => {
            set_variable(name, value, subplans, ctx)?;
            ctx.session()?.rowcount = 1;
            Ok(ExecOutcome::NoRows)
        }
        PhysicalStatement::SelectAssign { input, assignments } => {
            select_assign(input, assignments, ctx, sink)
        }
        // An unknown condition takes the `ELSE` branch, as it does on SQL Server: a true
        // predicate runs the `THEN`, an unknown one does not.
        PhysicalStatement::If {
            condition,
            then_,
            else_,
        } => {
            let value = eval_expr(condition, None, ctx)?;
            if matches!(value, Value::Bit(true)) {
                run_nested(then_, ctx, sink)
            } else if let Some(else_) = else_ {
                run_nested(else_, ctx, sink)
            } else {
                Ok(ExecOutcome::NoRows)
            }
        }
        PhysicalStatement::While { condition, body } => {
            loop {
                if ctx.cancelled() {
                    return Ok(ExecOutcome::Cancelled);
                }
                let value = eval_expr(condition, None, ctx)?;
                if !matches!(value, Value::Bit(true)) {
                    break;
                }
                match run_nested(body, ctx, sink)? {
                    ExecOutcome::Break => break,
                    ExecOutcome::Return(code) => return Ok(ExecOutcome::Return(code)),
                    flow @ (ExecOutcome::CallProcedure { .. } | ExecOutcome::RunDynamic { .. }) => {
                        return Ok(flow);
                    }
                    ExecOutcome::Cancelled => return Ok(ExecOutcome::Cancelled),
                    ExecOutcome::BatchAbort(err) => return Ok(ExecOutcome::BatchAbort(err)),
                    ExecOutcome::NoRows | ExecOutcome::Rows(_) | ExecOutcome::Continue => {}
                }
            }
            Ok(ExecOutcome::NoRows)
        }
        PhysicalStatement::Block(statements) => {
            for statement in statements {
                match run_nested(statement, ctx, sink)? {
                    ExecOutcome::NoRows | ExecOutcome::Rows(_) => {}
                    flow @ (ExecOutcome::Continue
                    | ExecOutcome::Break
                    | ExecOutcome::Return(_)
                    | ExecOutcome::CallProcedure { .. }
                    | ExecOutcome::RunDynamic { .. }
                    | ExecOutcome::Cancelled
                    | ExecOutcome::BatchAbort(_)) => return Ok(flow),
                }
            }
            Ok(ExecOutcome::NoRows)
        }
        PhysicalStatement::Break => Ok(ExecOutcome::Break),
        PhysicalStatement::Continue => Ok(ExecOutcome::Continue),
        PhysicalStatement::Return(expr) => {
            let code = match expr {
                None => 0,
                Some(expr) => {
                    let value = eval_expr(expr, None, ctx)?;
                    return_code(&value, &expr.ty, expr.line)?
                }
            };
            Ok(ExecOutcome::Return(code))
        }
        PhysicalStatement::Print(expr) => {
            let value = eval_expr(expr, None, ctx)?;
            let message = match value {
                Value::Null => String::new(),
                value => default_display(&value, &expr.ty),
            };
            sink.info(&InfoMessage {
                number: 0,
                severity: 0,
                state: 1,
                message,
                line: expr.line,
            })?;
            Ok(ExecOutcome::NoRows)
        }
        _ => Err(bug("control::execute: not a control-of-flow statement")),
    }
}

/// Runs one statement nested in an `IF`, a `WHILE` or a `BLOCK`.
///
/// A control-of-flow statement is run here without another frame, so the statement
/// savepoint of `txn_exec` stays the one of the outermost statement; anything else — a
/// query, a DML or DDL statement written in a branch — is a statement of its own and goes
/// through [`crate::execute`].
fn run_nested<'a>(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    match stmt {
        PhysicalStatement::Declare(_)
        | PhysicalStatement::SetVariable { .. }
        | PhysicalStatement::SelectAssign { .. }
        | PhysicalStatement::If { .. }
        | PhysicalStatement::While { .. }
        | PhysicalStatement::Block(_)
        | PhysicalStatement::Break
        | PhysicalStatement::Continue
        | PhysicalStatement::Return(_)
        | PhysicalStatement::Print(_) => execute(stmt, ctx, sink),
        _ => crate::execute(stmt, ctx, sink),
    }
}

/// `SET @x = e`: evaluates `e` and stores it under `name`, converted to the declared type
/// when the binder did not already convert it.
///
/// `subplans` are the plans of the subqueries `e` holds, which the planner attached to the
/// statement ([`PhysicalStatement::SetVariable`]). A value the evaluation refuses — 512
/// for a subquery of two rows — stores nothing: the variable keeps what it held, `NULL` in
/// `tests/subquery.rs`, `a_set_whose_subquery_returns_two_rows_answers_512`, which starts
/// from an unset variable.
fn set_variable(
    name: &str,
    value: &BoundExpr,
    subplans: &[SubPlan],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<()> {
    let evaluated = eval_statement_expr(value, subplans, ctx)?;
    store_assignment(name, value, evaluated, ctx)
}

/// Stores `evaluated` under `name`, converted to the declared type of the variable when
/// the session holds one for it.
///
/// The one place a variable is written from an assignment, `SET @x = e` and the list of
/// `SELECT @x = e FROM t` alike: the binder already wraps the value in a conversion
/// towards the declared type, and this second conversion is the one that applies the
/// length and the collation the declaration carries. A name the session does not hold is
/// stored as it was evaluated.
fn store_assignment(
    name: &str,
    value: &BoundExpr,
    evaluated: Value,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<()> {
    let declared = ctx.session()?.variable_type(name);
    let stored = match declared {
        Some(declared) => {
            convert(&evaluated, &value.ty, &declared, None).map_err(|err| at(err, value.line))?
        }
        None => evaluated,
    };
    ctx.session()?.store_variable_value(name, stored);
    Ok(())
}

/// `SELECT @x = e, @y = f FROM t …`: walks the rows of `input` and re-runs the whole list
/// of assignments on each one (module header, "Assigning from rows").
///
/// The statement answers [`ExecOutcome::NoRows`]: no column metadata goes to `sink`, and
/// the rows the plan produced are counted rather than sent. `@@ROWCOUNT` is posted to
/// that count before the statement ends, which is what a following statement reads. The
/// informational messages the plan raised leave through `sink`, as they do for a query
/// (`statement.rs`, `run_query`).
fn select_assign<'a>(
    input: &PhysicalPlan,
    assignments: &[(String, BoundExpr)],
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    if ctx.cancelled() {
        return Ok(ExecOutcome::Cancelled);
    }
    let mut root = build_operator(input)?;
    let walked = walk_assignments(root.as_mut(), assignments, ctx);
    root.close();
    let count = walked?;
    if ctx.cancelled() {
        return Ok(ExecOutcome::Cancelled);
    }
    ctx.session()?.rowcount = count;
    for info in ctx.take_infos() {
        sink.info(&info)?;
    }
    Ok(ExecOutcome::NoRows)
}

/// Opens `root`, assigns from each of its rows and answers how many rows were walked.
///
/// Within one row the targets are assigned in the order they were written, each stored
/// before the next value is evaluated. The caller closes the root, on success as on
/// error. The cancellation token is read once per [`CANCEL_CHECK_ROWS`] rows walked, as
/// the driving loop of a query reads it.
fn walk_assignments<'a>(
    root: &mut (dyn Operator<'a> + 'a),
    assignments: &[(String, BoundExpr)],
    ctx: &mut ExecContext<'a>,
) -> SqlResult<i64> {
    root.open(ctx)?;
    let mut count = 0i64;
    while let Some(row) = root.next(ctx)? {
        for (name, value) in assignments {
            let evaluated = eval_expr(value, Some(&row), ctx)?;
            store_assignment(name, value, evaluated, ctx)?;
        }
        count += 1;
        if count.unsigned_abs().is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
            break;
        }
    }
    Ok(count)
}

/// Evaluates an expression a statement holds outside any plan — the value of a `SET`, the
/// initial value of a `DECLARE` — with the plans of its subqueries installed while it is
/// read, and taken back after, the error path included.
///
/// A subquery answers `NULL` over an empty source, its single column over one row, and
/// 512 beyond, as it does in a select list (`tests/subquery.rs`,
/// `a_set_from_an_empty_subquery_is_null`,
/// `a_set_from_a_subquery_of_one_row_takes_its_value`,
/// `a_set_whose_subquery_returns_two_rows_answers_512`). An expression with no subquery
/// carries no plan and is read as it was before.
fn eval_statement_expr(
    expr: &BoundExpr,
    subplans: &[SubPlan],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Value> {
    if subplans.is_empty() {
        return eval_expr(expr, None, ctx);
    }
    let mark = ctx.open_statement_subplans(subplans);
    let value = eval_expr(expr, None, ctx);
    ctx.unwind_to(mark);
    value
}

/// The integer a `RETURN e` returns: `e` converted to `int`, `NULL` counting as 0.
fn return_code(value: &Value, ty: &TypeInfo, line: u32) -> SqlResult<i32> {
    let int = TypeInfo::new(SqlType::Int, false);
    let converted = convert(value, ty, &int, None).map_err(|err| at(err, line))?;
    Ok(match converted {
        Value::I32(n) => n,
        _ => 0,
    })
}

/// The internal error 50000 for a broken precondition.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
