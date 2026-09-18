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

use vauban_binder::BoundExpr;
use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult};
use vauban_planner::PhysicalStatement;
use vauban_types::{SqlType, TypeInfo, Value, convert, default_display};

use crate::context::{ExecContext, RowSink};
use crate::errors::at;
use crate::expr::eval_expr;
use crate::row::ExecOutcome;

/// Runs one statement of the control of flow, the nested statements included.
///
/// # Errors
///
/// What evaluating a condition or a value raises, and the conversion error of a `SET`,
/// which keeps the number `types::convert` gave it. A statement that is not one of the
/// control-of-flow variants is a bug of `statement.rs`, not something a client reaches.
pub(crate) fn execute(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'_>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    match stmt {
        PhysicalStatement::Declare(declarations) => {
            for declaration in declarations {
                let value = match &declaration.value {
                    Some(expr) => eval_expr(expr, None, ctx)?,
                    None => Value::Null,
                };
                let session = ctx.session()?;
                session.variables.insert(declaration.name.clone(), value);
                session
                    .variable_types
                    .insert(declaration.name.clone(), declaration.ty.clone());
            }
            Ok(ExecOutcome::NoRows)
        }
        PhysicalStatement::SetVariable { name, value } => {
            set_variable(name, value, ctx)?;
            ctx.session()?.rowcount = 1;
            Ok(ExecOutcome::NoRows)
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
fn run_nested(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'_>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    match stmt {
        PhysicalStatement::Declare(_)
        | PhysicalStatement::SetVariable { .. }
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
fn set_variable(name: &str, value: &BoundExpr, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    let evaluated = eval_expr(value, None, ctx)?;
    let declared = ctx.session()?.variable_types.get(name).cloned();
    let stored = match declared {
        Some(declared) => {
            convert(&evaluated, &value.ty, &declared, None).map_err(|err| at(err, value.line))?
        }
        None => evaluated,
    };
    ctx.session()?.variables.insert(name.to_owned(), stored);
    Ok(())
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
