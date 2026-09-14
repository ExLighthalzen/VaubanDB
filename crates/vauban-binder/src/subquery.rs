//! Subqueries in the positions T-SQL allows one: `EXISTS (…)`, `IN (SELECT …)`, the scalar
//! `(SELECT …)` of a value position, and the derived table of a `FROM` (116). The entry
//! points below, the [`BoundExprKind::Exists`], `ScalarSubquery` and `InSubquery`
//! variants, the [`LogicalPlan::Subquery`] variant and the parent link of
//! [`Scope`](crate::expr::Scope) a correlated reference walks are declared; the rules are
//! not bound yet.

use vauban_errors::SqlResult;
use vauban_parser::{Expr, SelectStatement, TableRef};

use crate::bound::{BoundExpr, LogicalPlan};
use crate::context::BindContext;
use crate::expr::Scope;
use crate::query::not_implemented;

/// Binds a derived table of a `FROM`, `(SELECT …) AS d`, into a [`LogicalPlan::Subquery`].
pub(crate) fn bind_derived(
    reference: &TableRef,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let _ = (reference, statement_line, ctx);
    Err(not_implemented("a derived table in FROM"))
}

/// Binds an `EXISTS (SELECT …)` into [`BoundExprKind::Exists`].
pub(crate) fn bind_exists(
    query: &SelectStatement,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let _ = (query, ctx, scope);
    Err(not_implemented("EXISTS"))
}

/// Binds a `(SELECT …)` written where a value is expected into
/// [`BoundExprKind::ScalarSubquery`].
pub(crate) fn bind_scalar(
    query: &SelectStatement,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let _ = (query, ctx, scope);
    Err(not_implemented("a scalar subquery"))
}

/// Binds an `e [NOT] IN (SELECT …)` into [`BoundExprKind::InSubquery`].
pub(crate) fn bind_in_subquery(
    expr: &Expr,
    query: &SelectStatement,
    negated: bool,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let _ = (expr, query, negated, ctx, scope);
    Err(not_implemented("IN (SELECT …)"))
}
