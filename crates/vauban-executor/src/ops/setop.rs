//! `Union`, `Except` and `Intersect`: the set operators. The operators are not written
//! yet; the entry point answers the internal error 50000 that names the node.

use vauban_errors::SqlResult;
use vauban_planner::PhysicalPlan;

use crate::operator::Operator;
use crate::ops::not_implemented;

/// Builds the operator of a [`PhysicalPlan::Union`], a [`PhysicalPlan::Except`] or a
/// [`PhysicalPlan::Intersect`].
///
/// # Errors
///
/// The internal error 50000, until the operators are written.
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let node = match plan {
        PhysicalPlan::Except { .. } => "Except",
        PhysicalPlan::Intersect { .. } => "Intersect",
        _ => "Union",
    };
    Err(not_implemented(node))
}
