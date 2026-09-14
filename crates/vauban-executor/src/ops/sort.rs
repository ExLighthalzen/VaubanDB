//! `Sort`, `TopN` and `Distinct`: ordering, the ordered top and the removal of
//! duplicates. The operators are not written yet; the entry point answers the internal
//! error 50000 that names the node.

use vauban_errors::SqlResult;
use vauban_planner::PhysicalPlan;

use crate::operator::Operator;
use crate::ops::not_implemented;

/// Builds the operator of a [`PhysicalPlan::Sort`], a [`PhysicalPlan::TopN`] or a
/// [`PhysicalPlan::Distinct`].
///
/// # Errors
///
/// The internal error 50000, until the operators are written
/// (`tests/operator_basics.rs`, `unserved_variant_is_a_bug`).
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let node = match plan {
        PhysicalPlan::TopN { .. } => "TopN",
        PhysicalPlan::Distinct(_) => "Distinct",
        _ => "Sort",
    };
    Err(not_implemented(node))
}
