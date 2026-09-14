//! The aggregation operators: [`HashAggregate`](PhysicalPlan::HashAggregate), and
//! [`StreamAggregate`](PhysicalPlan::StreamAggregate) when the input already arrives
//! ordered on the grouping keys. The entry point below and the two variants exist; the
//! rules are not written yet.

use vauban_binder::LogicalPlan;
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::not_implemented;

/// Plans a [`LogicalPlan::Aggregate`] into the aggregation operator that runs it.
///
/// `plan` is the `Aggregate` node itself; its input is planned through
/// [`plan_node`](crate::plan::plan_node).
pub(crate) fn plan_aggregate(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let _ = (plan, ctx);
    Err(not_implemented("aggregate::plan_aggregate", "an aggregate"))
}
