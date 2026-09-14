//! The join operators: [`NestedLoopJoin`](PhysicalPlan::NestedLoopJoin), a seek on the
//! index of the inner table when the condition allows it, and
//! [`HashJoin`](PhysicalPlan::HashJoin) on equality. The entry point below, the two
//! variants and [`PhysicalJoinKind`](crate::PhysicalJoinKind) exist; the rules are not
//! written yet.

use vauban_binder::LogicalPlan;
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::not_implemented;

/// Plans a [`LogicalPlan::Join`] into the join operator that runs it.
///
/// `plan` is the `Join` node itself, so that the rule may look at both inputs before
/// choosing the operator; it plans its children through
/// [`plan_node`](crate::plan::plan_node).
pub(crate) fn plan_join(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let _ = (plan, ctx);
    Err(not_implemented("join::plan_join", "a join"))
}
