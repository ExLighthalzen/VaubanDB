//! Control of flow: `IF … ELSE`, `WHILE`, `BEGIN … END`, `BREAK`, `CONTINUE`, `RETURN` and
//! `PRINT`. The entry points below and the seven [`BoundStatement`] variants they fill are
//! declared; which of them may nest in which, and what a `BREAK` outside a loop answers,
//! is not bound yet.

use vauban_errors::SqlResult;
use vauban_parser::{Expr, Statement};

use crate::bound::BoundStatement;
use crate::context::BindContext;
use crate::query::not_implemented;

/// Binds an `IF p s [ELSE s]` into [`BoundStatement::If`].
pub(crate) fn bind_if(
    condition: &Expr,
    then_branch: &Statement,
    else_branch: Option<&Statement>,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (condition, then_branch, else_branch, ctx);
    Err(not_implemented("IF"))
}

/// Binds a `WHILE p s` into [`BoundStatement::While`].
pub(crate) fn bind_while(
    condition: &Expr,
    body: &Statement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (condition, body, ctx);
    Err(not_implemented("WHILE"))
}

/// Binds a `BEGIN … END` into [`BoundStatement::Block`].
pub(crate) fn bind_block(
    statements: &[Statement],
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (statements, ctx);
    Err(not_implemented("BEGIN … END"))
}

/// Binds a `BREAK` into [`BoundStatement::Break`].
pub(crate) fn bind_break(ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let _ = ctx;
    Err(not_implemented("BREAK"))
}

/// Binds a `CONTINUE` into [`BoundStatement::Continue`].
pub(crate) fn bind_continue(ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let _ = ctx;
    Err(not_implemented("CONTINUE"))
}

/// Binds a `RETURN [e]` into [`BoundStatement::Return`].
pub(crate) fn bind_return(
    value: Option<&Expr>,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (value, ctx);
    Err(not_implemented("RETURN"))
}

/// Binds a `PRINT e` into [`BoundStatement::Print`].
pub(crate) fn bind_print(expr: &Expr, ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let _ = (expr, ctx);
    Err(not_implemented("PRINT"))
}
