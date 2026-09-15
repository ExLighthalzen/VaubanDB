//! Control of flow: `IF … ELSE`, `WHILE`, `BEGIN … END`, `BREAK`, `CONTINUE`, `RETURN` and
//! `PRINT`.

use std::cell::Cell;

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Expr, Statement};
use vauban_types::{Len, SqlType, TypeInfo};

use crate::bound::{BoundExpr, BoundExprKind, BoundStatement};
use crate::context::BindContext;
use crate::expr::{Scope, bind_condition, bind_expr};
use crate::statement;

thread_local! {
    /// Depth of `WHILE` nesting. Incremented when entering `bind_while`, decremented on
    /// exit. `bind_break` and `bind_continue` check this to raise 135 / 136.
    static WHILE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Binds an `IF p s [ELSE s]` into [`BoundStatement::If`].
pub(crate) fn bind_if(
    condition: &Expr,
    then_branch: &Statement,
    else_branch: Option<&Statement>,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let condition = bind_condition(condition, ctx, &Scope::empty())?;
    let then_ = statement::bind(then_branch, ctx)?;
    let else_ = else_branch.map(|s| statement::bind(s, ctx)).transpose()?;
    Ok(BoundStatement::If {
        condition,
        then_: Box::new(then_),
        else_: else_.map(Box::new),
    })
}

/// Binds a `WHILE p s` into [`BoundStatement::While`].
///
/// Increments [`WHILE_DEPTH`] before binding the body and decrements it on exit, so that
/// `bind_break` and `bind_continue` can check that they are inside a WHILE.
pub(crate) fn bind_while(
    condition: &Expr,
    body: &Statement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let condition = bind_condition(condition, ctx, &Scope::empty())?;
    WHILE_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let body = statement::bind(body, ctx);
    WHILE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    let body = body?;
    Ok(BoundStatement::While {
        condition,
        body: Box::new(body),
    })
}

/// Binds a `BEGIN … END` into [`BoundStatement::Block`].
pub(crate) fn bind_block(
    statements: &[Statement],
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let bound: SqlResult<Vec<BoundStatement>> =
        statements.iter().map(|s| statement::bind(s, ctx)).collect();
    Ok(BoundStatement::Block(bound?))
}

/// Binds a `BREAK` into [`BoundStatement::Break`].
///
/// Error 135 when outside a WHILE.
pub(crate) fn bind_break(_ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let inside_while = WHILE_DEPTH.with(|depth| depth.get() > 0);
    if inside_while {
        Ok(BoundStatement::Break)
    } else {
        Err(SqlError::break_without_while())
    }
}

/// Binds a `CONTINUE` into [`BoundStatement::Continue`].
///
/// Error 136 when outside a WHILE.
pub(crate) fn bind_continue(_ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let inside_while = WHILE_DEPTH.with(|depth| depth.get() > 0);
    if inside_while {
        Ok(BoundStatement::Continue)
    } else {
        Err(SqlError::continue_without_while())
    }
}

/// Binds a `RETURN [e]` into [`BoundStatement::Return`].
///
/// A bare `RETURN` (without a value) is accepted (`RETURN;`). A `RETURN` with a value
/// outside a procedure is error 178, carrying the line of the RETURN statement
/// (`RETURN 1;`).
pub(crate) fn bind_return(
    value: Option<&Expr>,
    line: u32,
    _ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    match value {
        Some(_) => Err(SqlError::return_with_value_outside_procedure().with_line(line)),
        None => Ok(BoundStatement::Return(None)),
    }
}

/// Binds a `PRINT e` into [`BoundStatement::Print`].
///
/// The expression is bound then wrapped in a [`BoundExprKind::Convert`] towards
/// `nvarchar(max)`, the type PRINT's argument is converted to. The conversion covers
/// `PRINT 1`, `PRINT NULL`, `PRINT GETDATE()`, `PRINT @x` of an `int`.
pub(crate) fn bind_print(expr: &Expr, ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let bound = bind_expr(expr, ctx, &Scope::empty())?;
    let nvarchar_max = TypeInfo::new(SqlType::NVarChar(Len::Max), true);
    let converted = if bound.ty == nvarchar_max {
        bound
    } else {
        let line = bound.line;
        BoundExpr {
            kind: BoundExprKind::Convert {
                expr: Box::new(bound),
                style: None,
                try_: false,
            },
            ty: nvarchar_max,
            line,
        }
    };
    Ok(BoundStatement::Print(converted))
}
