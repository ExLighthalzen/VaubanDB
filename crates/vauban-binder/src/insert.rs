//! `INSERT`: `VALUES`, `INSERT … SELECT` and `DEFAULT VALUES` (213, 544, 8101), and
//! `TRUNCATE TABLE`. The entry points below and the
//! [`InsertPlan`](crate::bound::InsertPlan) they fill are declared; the rules are not bound
//! yet.

use vauban_errors::SqlResult;
use vauban_parser::{InsertStatement, ObjectName, Span};

use crate::bound::BoundStatement;
use crate::context::BindContext;
use crate::query::not_implemented;

/// Binds an `INSERT` into [`BoundStatement::Insert`].
pub(crate) fn bind_insert(
    stmt: &InsertStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("INSERT"))
}

/// Binds a `TRUNCATE TABLE`.
///
/// `statement.rs` routes the form here rather than leaving it in its `unsupported` list:
/// the table it empties is the table an `INSERT` fills, and the two share the same
/// resolution.
pub(crate) fn bind_truncate(
    table: &ObjectName,
    span: Span,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (table, span, ctx);
    Err(not_implemented("TRUNCATE TABLE"))
}
