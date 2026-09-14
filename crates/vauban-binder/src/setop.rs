//! `UNION [ALL]`, `EXCEPT` and `INTERSECT`: the operands' widths (205) and the common type
//! of each column. The entry point below and the [`LogicalPlan::SetOp`] variant with its
//! [`SetOpKind`](crate::bound::SetOpKind) are declared; the rules are not bound yet.
//!
//! An `ORDER BY` written after a set operation orders the whole of it and belongs to
//! `sort.rs`.

use vauban_errors::SqlResult;
use vauban_parser::{QueryBody, SelectStatement};

use crate::bound::LogicalPlan;
use crate::context::BindContext;
use crate::query::not_implemented;

/// Binds a `QueryBody::SetOp` into a [`LogicalPlan::SetOp`].
///
/// `stmt` is the statement the body belongs to: the clauses written after the operation —
/// an `ORDER BY`, a `TOP` — hang on the statement, not on the body.
pub(crate) fn bind_set_op(
    body: &QueryBody,
    stmt: &SelectStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let _ = (body, stmt, ctx);
    Err(not_implemented("UNION, EXCEPT and INTERSECT"))
}
