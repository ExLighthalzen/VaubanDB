//! `UNION`, `EXCEPT` and `INTERSECT`. The entry point below and the three variants
//! ([`Union`](PhysicalPlan::Union), [`Except`](PhysicalPlan::Except),
//! [`Intersect`](PhysicalPlan::Intersect)) exist; the rules are not written yet.

use vauban_binder::LogicalPlan;
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::not_implemented;

/// Plans a [`LogicalPlan::SetOp`] into the set operator that runs it.
///
/// `plan` is the `SetOp` node itself: the bound plan pairs its operands two by two, while
/// the physical variants hold a vector, so flattening a chain of `UNION` into one node is
/// the business of this file.
pub(crate) fn plan_setop(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let _ = (plan, ctx);
    Err(not_implemented("setop::plan_setop", "a set operator"))
}
