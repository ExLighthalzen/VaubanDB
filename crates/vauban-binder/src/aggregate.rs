//! `GROUP BY`, `HAVING` and the aggregate calls of a select list (8120, 8121). The entry
//! point below, the [`LogicalPlan::Aggregate`] variant and the
//! [`AggregateCall`](crate::bound::AggregateCall) it holds are declared; the rules are not
//! bound yet.
//!
//! The order of the columns an `Aggregate` publishes is written on the variant and is a
//! contract, not a choice left open here: the `GROUP BY` keys first, then the aggregates,
//! and the projection above names them by index.

use vauban_errors::SqlResult;
use vauban_parser::QuerySpec;

use crate::bound::LogicalPlan;
use crate::context::BindContext;
use crate::query::not_implemented;

/// Binds the grouping of a `SELECT` into a [`LogicalPlan::Aggregate`] over `input`.
pub(crate) fn bind_aggregate(
    spec: &QuerySpec,
    input: LogicalPlan,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let _ = (spec, input, ctx);
    Err(not_implemented("GROUP BY and HAVING"))
}
