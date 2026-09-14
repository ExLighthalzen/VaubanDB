//! `HashAggregate` and `StreamAggregate`: grouping and the aggregate functions. The
//! operators are not written yet; the entry point answers the internal error 50000 that
//! names the node.

use vauban_errors::SqlResult;
use vauban_planner::PhysicalPlan;

use crate::operator::Operator;
use crate::ops::not_implemented;

/// Builds the operator of a [`PhysicalPlan::HashAggregate`] or a
/// [`PhysicalPlan::StreamAggregate`].
///
/// # Errors
///
/// The internal error 50000, until the operators are written
/// (`tests/operator_basics.rs`, `unserved_variant_is_a_bug`).
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let node = match plan {
        PhysicalPlan::StreamAggregate { .. } => "StreamAggregate",
        _ => "HashAggregate",
    };
    Err(not_implemented(node))
}
