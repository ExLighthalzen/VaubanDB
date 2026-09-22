//! Control of flow: `IF … ELSE`, `WHILE`, `BEGIN … END`, `BREAK`, `CONTINUE`, `RETURN` and
//! `PRINT`.
//!
//! # A branch that names a table the catalogue does not hold
//!
//! A statement written inside `IF`, `WHILE` or `BEGIN … END` is not bound here when its
//! binding answers one of [`BOUND_WHEN_REACHED`]: it becomes a
//! [`BoundStatement::Deferred`], and whoever runs it binds it against the catalogue of the
//! moment the control of flow reaches it. Two things follow, and each has its test in
//! `tests/bind_control_flow.rs`:
//!
//! * a branch that is not taken stays unbound, so `IF 1 = 0 SELECT * FROM dbo.absent;`
//!   binds (`a_branch_over_a_missing_table_binds_deferred`);
//! * a block that creates a table and then writes to it binds, since the write waits for
//!   the moment the `CREATE TABLE` before it has run
//!   (`a_block_that_creates_then_inserts_defers_the_insert`).
//!
//! The rest of the binding is unchanged: another number is still a binding error of the
//! whole statement (`a_branch_that_fails_on_another_number_is_still_a_binding_error`), and
//! so is a name written in the **condition**, which is read before the branch is chosen:
//! the statement the caller handed over answers 208 rather than binding
//! (`a_condition_over_a_missing_table_is_still_208`). Written one level down, that same
//! condition travels with its own `IF`, which this file defers as one statement
//! (`a_condition_of_a_nested_if_is_deferred_with_it`).

use std::cell::Cell;
use std::sync::Arc;

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Expr, Statement};
use vauban_types::{Len, SqlType, TypeInfo};

use crate::bound::{BoundExpr, BoundExprKind, BoundStatement, DeferredStatement};
use crate::context::BindContext;
use crate::expr::{Scope, bind_condition, bind_expr};
use crate::statement;

/// Numbers that make a statement of a branch or of a loop body wait for its execution
/// instead of failing the binding: each names a table that is not in the catalogue yet and
/// may be created before the control of flow gets there.
///
/// 208 is `INSERT`, `UPDATE`, `DELETE` and `SELECT` over a missing table, 1088 is
/// `CREATE INDEX` on one, 4902 is `ALTER TABLE` on one.
const BOUND_WHEN_REACHED: [u32; 3] = [208, 1088, 4902];

thread_local! {
    /// Depth of `WHILE` nesting. Incremented when entering `bind_while`, decremented on
    /// exit. `bind_break` and `bind_continue` check this to raise 135 / 136.
    static WHILE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Binds one statement written inside a control-of-flow statement, keeping it unbound when
/// its binding answers one of [`BOUND_WHEN_REACHED`] (module header).
fn bind_nested(stmt: &Statement, ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    match statement::bind(stmt, ctx) {
        Err(error) if BOUND_WHEN_REACHED.contains(&error.number) => {
            Ok(BoundStatement::Deferred(Box::new(DeferredStatement {
                text: Arc::from(ctx.text),
                statement: Box::new(stmt.clone()),
                database: ctx.database.to_owned(),
                default_schema: ctx.default_schema.to_owned(),
                line: error.line,
            })))
        }
        other => other,
    }
}

/// Binds an `IF p s [ELSE s]` into [`BoundStatement::If`].
pub(crate) fn bind_if(
    condition: &Expr,
    then_branch: &Statement,
    else_branch: Option<&Statement>,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let condition = bind_condition(condition, ctx, &Scope::empty())?;
    let then_ = bind_nested(then_branch, ctx)?;
    let else_ = else_branch.map(|s| bind_nested(s, ctx)).transpose()?;
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
    let body = bind_nested(body, ctx);
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
        statements.iter().map(|s| bind_nested(s, ctx)).collect();
    Ok(BoundStatement::Block(bound?))
}

/// Binds a `BREAK` into [`BoundStatement::Break`].
///
/// Error 135 when outside a WHILE, carrying the line of the BREAK statement.
pub(crate) fn bind_break(line: u32, _ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let inside_while = WHILE_DEPTH.with(|depth| depth.get() > 0);
    if inside_while {
        Ok(BoundStatement::Break)
    } else {
        Err(SqlError::break_without_while().with_line(line))
    }
}

/// Binds a `CONTINUE` into [`BoundStatement::Continue`].
///
/// Error 136 when outside a WHILE, carrying the line of the CONTINUE statement.
pub(crate) fn bind_continue(line: u32, _ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    let inside_while = WHILE_DEPTH.with(|depth| depth.get() > 0);
    if inside_while {
        Ok(BoundStatement::Continue)
    } else {
        Err(SqlError::continue_without_while().with_line(line))
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
