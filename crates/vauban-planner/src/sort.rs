//! Ordering: [`Sort`](PhysicalPlan::Sort), the sort an index makes unnecessary,
//! [`TopN`](PhysicalPlan::TopN) when a `TOP` sits over an `ORDER BY`, and
//! [`Distinct`](PhysicalPlan::Distinct). The three entry points below and the variants
//! they build exist; the rules are not written yet.

use vauban_binder::{BoundTop, LogicalPlan};
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::not_implemented;

/// Plans a [`LogicalPlan::Sort`] into the operator that orders its rows.
pub(crate) fn plan_sort(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let _ = (plan, ctx);
    Err(not_implemented("sort::plan_sort", "an ORDER BY"))
}

/// Plans a [`LogicalPlan::Distinct`] into the operator that removes its duplicate rows.
pub(crate) fn plan_distinct(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let _ = (plan, ctx);
    Err(not_implemented("sort::plan_distinct", "a SELECT DISTINCT"))
}

/// Tries to turn a `TOP` over an ordered input into a single [`PhysicalPlan::TopN`].
///
/// `input` is the **logical** input of the [`LogicalPlan::Limit`], not a planned one: the
/// rule reads the `Sort` under it before deciding, and plans the children itself.
/// `Ok(None)` says "no top-N here", which `plan.rs` turns into the
/// [`Top`](PhysicalPlan::Top) over the planned input it would have built anyway
/// (`tests/trivial.rs`, `values_and_limit_are_translated`). The rule is not written yet.
pub(crate) fn try_top_n(
    top: &BoundTop,
    input: &LogicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<Option<PhysicalPlan>> {
    let _ = (top, input, ctx);
    Ok(None)
}
