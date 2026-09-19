//! `EXECUTE`: evaluate the target and the arguments, then hand off to the session.
//!
//! An `EXEC` runs no procedure here and writes no output variable: it evaluates what the
//! binder bound and answers [`ExecOutcome::CallProcedure`] or [`ExecOutcome::RunDynamic`].
//! The session resolves the procedure, runs the dynamic text, and writes any `OUTPUT`
//! back into the caller's variables.

use vauban_binder::{BoundExecArg, BoundExecTarget, BoundExecute, BoundExpr, BoundExprKind};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{Len, SqlType, TypeInfo, Value, default_display};

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
        BoundExecTarget::Procedure { name, .. } => Ok(ExecOutcome::CallProcedure {
            name: name.clone(),
            args: eval_args(&stmt.args, stmt.line, ctx)?,
            return_into: stmt.return_into.clone(),
            line: stmt.line,
        }),
        BoundExecTarget::ProcedureVariable(expr) => {
            let name = procedure_name(expr, ctx)?;
            Ok(ExecOutcome::CallProcedure {
                name,
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

fn eval_arg(
    arg: &BoundExecArg,
    line: u32,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<EvaluatedExecArg> {
    let output_variable = arg
        .output
        .then(|| match &arg.value {
            Some(BoundExprKind::Variable { name }) => Some(name.clone()),
            _ => None,
        })
        .flatten();
    let value = match &arg.value {
        None => None,
        Some(BoundExprKind::Variable { name }) => Some(read_variable(name, line, ctx)?),
        Some(kind) => {
            let expr = constant_expr(kind, line);
            let evaluated = eval_expr(&expr, None, ctx)?;
            Some((evaluated, expr.ty))
        }
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

fn constant_expr(kind: &BoundExprKind, line: u32) -> BoundExpr {
    BoundExpr {
        kind: kind.clone(),
        ty: constant_type(kind),
        line,
    }
}

fn constant_type(kind: &BoundExprKind) -> TypeInfo {
    match kind {
        BoundExprKind::Literal(value) => type_of_literal(value),
        BoundExprKind::Negate(inner) => constant_type(&inner.kind),
        _ => TypeInfo::new(SqlType::Int, true),
    }
}

fn type_of_literal(value: &Value) -> TypeInfo {
    match value {
        Value::Null => TypeInfo::new(SqlType::Int, true),
        Value::Bit(_) => TypeInfo::new(SqlType::Bit, true),
        Value::I8(_) => TypeInfo::new(SqlType::TinyInt, false),
        Value::I16(_) => TypeInfo::new(SqlType::SmallInt, false),
        Value::I32(_) => TypeInfo::new(SqlType::Int, false),
        Value::I64(_) => TypeInfo::new(SqlType::BigInt, false),
        Value::F32(_) => TypeInfo::new(SqlType::Real, false),
        Value::F64(_) => TypeInfo::new(SqlType::Float, false),
        Value::Decimal(_) => TypeInfo::new(
            SqlType::Decimal {
                precision: 18,
                scale: 0,
            },
            false,
        ),
        Value::Money(_) => TypeInfo::new(SqlType::Money, false),
        Value::String(s) => {
            let len = u16::try_from(s.text.len()).unwrap_or(u16::MAX);
            TypeInfo::new(SqlType::VarChar(Len::Fixed(len.max(1))), true)
        }
        Value::Bytes(b) => {
            let len = u16::try_from(b.len()).unwrap_or(u16::MAX);
            TypeInfo::new(SqlType::VarBinary(Len::Fixed(len.max(1))), true)
        }
        Value::Guid(_) => TypeInfo::new(SqlType::UniqueIdentifier, false),
        Value::DateTime(_) => TypeInfo::new(SqlType::DateTime, false),
        Value::DateTime2(_) => TypeInfo::new(SqlType::DateTime2(7), false),
        Value::DateTimeOffset(_) => TypeInfo::new(SqlType::DateTimeOffset(7), false),
        Value::Date(_) => TypeInfo::new(SqlType::Date, false),
        Value::Time(_) => TypeInfo::new(SqlType::Time(7), false),
    }
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
