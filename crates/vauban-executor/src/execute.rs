//! `EXECUTE`: evaluate the target and the arguments, then hand off to the session.
//!
//! An `EXEC` runs no procedure here and writes no output variable: it evaluates what the
//! binder bound and answers [`ExecOutcome::CallProcedure`] or [`ExecOutcome::RunDynamic`].
//! The session resolves the procedure, runs the dynamic text, and writes any `OUTPUT`
//! back into the caller's variables.
//!
//! The database written left of a procedure name travels unresolved: `EXEC d.sys.sp_tables`
//! answers a `CallProcedure` whose `database` is `d`, and the session decides what that name
//! designates (`tests/execute.rs`, `exec_of_a_three_part_name_carries_the_database`). The
//! server written left of it is resolved here instead, against the name this instance
//! answers to: another one reaches no procedure and answers 7202 (`tests/execute.rs`,
//! `exec_of_a_four_part_name_on_another_server_fails`).

use vauban_binder::{BoundExecArg, BoundExecTarget, BoundExecute, BoundExpr, BoundExprKind};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{TypeInfo, Value, default_display};

use crate::context::ExecContext;
use crate::expr::eval_expr;
use crate::row::{EvaluatedExecArg, ExecOutcome};

/// Evaluates `stmt` and answers what the session runs next.
///
/// Nothing is handed to a [`RowSink`]: the caller keeps the sink unchanged.
pub(crate) fn execute(stmt: &BoundExecute, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    match &stmt.target {
        BoundExecTarget::Dynamic(expr) => {
            let text = dynamic_text(expr, ctx)?;
            Ok(ExecOutcome::RunDynamic {
                text,
                line: stmt.line,
            })
        }
        BoundExecTarget::Procedure { name, raw } => {
            if let Some(server) = &raw.server
                && !server.value.eq_ignore_ascii_case(ctx.eval.server_name())
            {
                return Err(SqlError::linked_server_not_found(&server.value).with_line(stmt.line));
            }
            Ok(ExecOutcome::CallProcedure {
                name: name.clone(),
                database: raw.database.as_ref().map(|part| part.value.clone()),
                args: eval_args(&stmt.args, stmt.line, ctx)?,
                return_into: stmt.return_into.clone(),
                line: stmt.line,
            })
        }
        BoundExecTarget::ProcedureVariable(expr) => {
            let name = procedure_name(expr, ctx)?;
            Ok(ExecOutcome::CallProcedure {
                name,
                database: None,
                args: eval_args(&stmt.args, stmt.line, ctx)?,
                return_into: stmt.return_into.clone(),
                line: stmt.line,
            })
        }
    }
}

fn eval_args(
    args: &[BoundExecArg],
    line: u32,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Vec<EvaluatedExecArg>> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        out.push(eval_arg(arg, line, ctx)?);
    }
    Ok(out)
}

/// Evaluates one argument, keeping the type the binding gave it.
///
/// A constant is evaluated in place and travels under the type its spelling was bound to:
/// `'x'` reaches the callee as `varchar` and `N'x'` as `nvarchar`
/// (`tests/execute.rs`, `a_string_constant_argument_travels_under_the_type_of_its_spelling`).
/// A variable is read from the session, under the type its `DECLARE` wrote.
fn eval_arg(
    arg: &BoundExecArg,
    line: u32,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<EvaluatedExecArg> {
    let output_variable = arg
        .output
        .then(|| match arg.value.as_ref().map(|expr| &expr.kind) {
            Some(BoundExprKind::Variable { name }) => Some(name.clone()),
            _ => None,
        })
        .flatten();
    let value = match &arg.value {
        None => None,
        Some(expr) => match &expr.kind {
            BoundExprKind::Variable { name } => Some(read_variable(name, line, ctx)?),
            _ => Some((eval_expr(expr, None, ctx)?, expr.ty.clone())),
        },
    };
    Ok(EvaluatedExecArg {
        name: arg.name.clone(),
        value,
        output: arg.output,
        output_variable,
    })
}

fn read_variable(
    name: &str,
    _line: u32,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<(Value, TypeInfo)> {
    let session = ctx.session()?;
    let ty = session
        .variable_type(name)
        .ok_or_else(|| bug(&format!("execute: variable `{name}` was not declared")))?;
    let value = session
        .variable_value(name)
        .ok_or_else(|| bug(&format!("execute: variable `{name}` was not declared")))?;
    Ok((value, ty))
}

fn dynamic_text(expr: &BoundExpr, ctx: &mut ExecContext<'_>) -> SqlResult<String> {
    let value = eval_expr(expr, None, ctx)?;
    Ok(match value {
        Value::Null => String::new(),
        value => default_display(&value, &expr.ty),
    })
}

fn procedure_name(expr: &BoundExpr, ctx: &mut ExecContext<'_>) -> SqlResult<String> {
    let value = eval_expr(expr, None, ctx)?;
    let name = match value {
        Value::Null => String::new(),
        value => default_display(&value, &expr.ty),
    };
    Ok(name.to_ascii_lowercase())
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_binder::SessionOptions;
    use vauban_sysfn::StaticContext;
    use vauban_types::{Len, SqlType};

    #[test]
    fn null_dynamic_text_is_empty() {
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        let expr = BoundExpr {
            kind: BoundExprKind::Literal(Value::Null),
            ty: TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true),
            line: 1,
        };
        let text = dynamic_text(&expr, &mut ctx).expect("NULL renders as empty text");
        assert!(text.is_empty());
    }
}
