//! `UPDATE` and `DELETE` (8102, 271). The two entry points below and the
//! [`UpdatePlan`](crate::bound::UpdatePlan) and [`DeletePlan`](crate::bound::DeletePlan)
//! they fill are declared; the rules are not bound yet.

use vauban_errors::SqlResult;
use vauban_parser::{DeleteStatement, UpdateStatement};

use crate::bound::BoundStatement;
use crate::context::BindContext;
use crate::query::not_implemented;

/// Binds an `UPDATE` into [`BoundStatement::Update`].
pub(crate) fn bind_update(
    stmt: &UpdateStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("UPDATE"))
}

/// Binds a `DELETE` into [`BoundStatement::Delete`].
pub(crate) fn bind_delete(
    stmt: &DeleteStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("DELETE"))
}
