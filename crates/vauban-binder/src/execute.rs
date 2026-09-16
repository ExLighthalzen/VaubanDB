//! `EXECUTE`: the target and the arguments of a call, bound.
//!
//! # What is bound here, and where the rest happens
//!
//! The binder binds the **call**: the procedure name (normalised, not resolved) or the
//! T-SQL text a parenthesised form builds, the arguments (each one a constant, a variable
//! or `DEFAULT`, with its `OUTPUT` flag) and the `@r =` variable. Resolving the name
//! against the catalogue (2812) and running the call belong to the session; the binder
//! consults no catalogue for a procedure.
//!
//! # The procedure name is not resolved here
//!
//! `EXEC sp_nosuch` in a batch whose first statement is a `SELECT` sends the `SELECT`'s
//! result set first and 2812 after: SQL Server resolves the procedure late. The binder
//! therefore normalises the name — the object part, lower-cased, the schema and the
//! database dropped — and leaves the resolution to the session.
//!
//! # The errors binding raises, in SQL Server's order
//!
//! The `@r =` variable is resolved first, then the target, then the arguments one
//! by one. Within one argument, a value that is neither a constant, a variable nor `DEFAULT`
//! is 102; a variable no `DECLARE` entered is 137; a positional argument that follows a
//! named one is 119; and `OUTPUT` on anything but a variable is 179.

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{ExecuteArg, ExecuteStatement, ExecuteTarget, Expr, Literal, ObjectName};

use crate::bound::{BoundExpr, BoundExprKind, BoundStatement};
use crate::context::BindContext;
use crate::expr::{Scope, bind_expr, syntax_span, token_at};

/// An `EXECUTE`, bound.
#[derive(Debug, Clone)]
pub struct BoundExecute {
    /// What is run.
    pub target: BoundExecTarget,
    /// The arguments, in the order they were written.
    pub args: Vec<BoundExecArg>,
    /// The `@rc =` variable receiving the return status, `None` when the form was not
    /// written.
    pub return_into: Option<String>,
    /// Line of the statement.
    pub line: u32,
}

/// What an `EXECUTE` runs.
#[derive(Debug, Clone)]
pub enum BoundExecTarget {
    /// A stored procedure, by name. `name` is normalised and **not** resolved: the session
    /// answers 2812 for a name the catalogue does not hold. `raw` is the name as written,
    /// for the caller that resolves it.
    Procedure {
        /// Normalised name: the object part, lower-cased, schema and database dropped.
        name: String,
        /// The name as the statement wrote it.
        raw: ObjectName,
    },
    /// A stored procedure whose name is read from a variable, `EXEC @t`: the session
    /// resolves the procedure the variable names when it runs. Distinct from
    /// [`BoundExecTarget::Dynamic`], which runs text: `EXEC(@t)` executes the variable's
    /// value as T-SQL, where `EXEC @t` looks a procedure up by it.
    ProcedureVariable(BoundExpr),
    /// T-SQL text built by the statement: `EXEC('…')`, `EXEC(@t)` and `EXEC('a' + @b')`.
    /// The expression is bound and of a character type; its value is the text to run.
    Dynamic(BoundExpr),
}

/// One argument of an `EXECUTE`.
#[derive(Debug, Clone)]
pub struct BoundExecArg {
    /// The parameter name, `@` included, for a named argument; `None` for a positional one.
    pub name: Option<String>,
    /// The value: a bound constant or variable, or `None` for `DEFAULT`, which keeps no
    /// value.
    pub value: Option<BoundExprKind>,
    /// `OUTPUT` was written.
    pub output: bool,
}

/// Binds one `EXECUTE`. See the module documentation for the order of the checks.
pub(crate) fn bind_execute(
    stmt: &ExecuteStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let line = stmt.span.line;
    // The `@r =` variable is resolved before the target and the arguments: an undeclared
    // `@r` is 137 on its own.
    let return_into = match &stmt.return_into {
        Some(name) => {
            if ctx.variables.type_of(name).is_none() {
                return Err(SqlError::must_declare_scalar_variable(name).with_line(line));
            }
            Some(name.clone())
        }
        None => None,
    };
    let target = bind_target(&stmt.target, ctx, line)?;
    let args = bind_args(&stmt.args, ctx, line)?;
    Ok(BoundStatement::Execute(BoundExecute {
        target,
        args,
        return_into,
        line,
    }))
}

/// Binds the statement's target.
///
/// The two variable forms are told apart: `EXEC @t` names a procedure by the variable's
/// value ([`BoundExecTarget::ProcedureVariable`]), where `EXEC(@t)` runs that value as T-SQL
/// ([`BoundExecTarget::Dynamic`]). The type of the variable and the 8199 SQL Server raises
/// for a non-character one belong to the work that brings 8199 to the catalogue.
fn bind_target(
    target: &ExecuteTarget,
    ctx: &BindContext<'_>,
    line: u32,
) -> SqlResult<BoundExecTarget> {
    match target {
        ExecuteTarget::Procedure(raw) => Ok(BoundExecTarget::Procedure {
            name: normalise(&raw.name.value),
            raw: raw.clone(),
        }),
        ExecuteTarget::Variable(name) => {
            let variable = bind_variable(name, ctx, line)?;
            Ok(BoundExecTarget::ProcedureVariable(variable))
        }
        ExecuteTarget::Literal(expr) => {
            let bound = bind_expr(expr, ctx, &Scope::empty())?;
            if !bound.ty.ty.is_string() {
                return Err(syntax_near_target(expr, ctx));
            }
            Ok(BoundExecTarget::Dynamic(bound))
        }
    }
}

/// Binds the arguments, in written order, with SQL Server's precedence.
///
/// An argument may not follow a named one once it is positional: that is 119, and it comes
/// after the value has been read (102, 137) and before `OUTPUT` on a constant (179).
fn bind_args(
    args: &[ExecuteArg],
    ctx: &BindContext<'_>,
    line: u32,
) -> SqlResult<Vec<BoundExecArg>> {
    let mut out = Vec::with_capacity(args.len());
    let mut named_seen = false;
    for (position, arg) in args.iter().enumerate() {
        let parameter = i64::try_from(position).unwrap_or(i64::MAX) + 1;
        let (value, is_variable) = match &arg.value {
            Expr::Literal(Literal::Default, _) => (None, false),
            Expr::Variable { name, .. } => {
                bind_variable(name, ctx, line)?;
                (Some(BoundExprKind::Variable { name: name.clone() }), true)
            }
            other => {
                let bound = bind_expr(other, ctx, &Scope::empty())?;
                if !is_constant(&bound.kind) {
                    return Err(syntax_near_argument(other, ctx));
                }
                (Some(bound.kind), false)
            }
        };
        if named_seen && arg.name.is_none() {
            return Err(SqlError::positional_after_named(parameter).with_line(line));
        }
        if arg.output && !is_variable {
            return Err(SqlError::output_on_a_constant().with_line(line));
        }
        if arg.name.is_some() {
            named_seen = true;
        }
        out.push(BoundExecArg {
            name: arg.name.clone(),
            value,
            output: arg.output,
        });
    }
    Ok(out)
}

/// The bound variable `@name`, or 137 when no `DECLARE` entered it.
fn bind_variable(name: &str, ctx: &BindContext<'_>, line: u32) -> SqlResult<BoundExpr> {
    let ty = ctx
        .variables
        .type_of(name)
        .ok_or_else(|| SqlError::must_declare_scalar_variable(name).with_line(line))?;
    Ok(BoundExpr {
        kind: BoundExprKind::Variable {
            name: name.to_owned(),
        },
        ty,
        line,
    })
}

/// Whether `kind` is a constant: a literal, or a literal under a sign (`-1`).
fn is_constant(kind: &BoundExprKind) -> bool {
    match kind {
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Negate(inner) => matches!(inner.kind, BoundExprKind::Literal(_)),
        _ => false,
    }
}

/// The normalised name of a procedure: the object part, lower-cased.
///
/// The parser has taken the brackets off and `ObjectName` split the three parts; the schema
/// and the database are dropped, so `[sys].[sp_x]` and `master..sp_x` both answer `sp_x`.
fn normalise(name: &str) -> String {
    name.to_lowercase()
}

/// 102 near the first token of a target whose text is not a character expression.
fn syntax_near_target(expr: &Expr, ctx: &BindContext<'_>) -> SqlError {
    let (token, line) = token_at(ctx.text, &syntax_span(expr));
    SqlError::incorrect_syntax_near(token, line)
}

/// 102 near the operator of an argument that is neither a constant, a variable nor
/// `DEFAULT`: SQL Server quotes `'+'` for `1 + 1`, not the whole expression.
fn syntax_near_argument(expr: &Expr, ctx: &BindContext<'_>) -> SqlError {
    let span = match expr {
        Expr::Binary { op_span, .. } => *op_span,
        _ => syntax_span(expr),
    };
    let (token, line) = token_at(ctx.text, &span);
    SqlError::incorrect_syntax_near(token, line)
}
