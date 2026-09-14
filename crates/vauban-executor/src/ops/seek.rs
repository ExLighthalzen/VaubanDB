//! `IndexSeek`: the index seek. The operator is not written yet; its entry point answers the
//! internal error 50000 that names the node.

use vauban_errors::SqlResult;
use vauban_planner::PhysicalPlan;

use crate::operator::Operator;
use crate::ops::not_implemented;

/// Builds the operator of a [`PhysicalPlan::IndexSeek`].
///
/// # Errors
///
/// The internal error 50000, until the operator is written
/// (`tests/operator_basics.rs`, `unserved_variant_is_a_bug`).
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let _ = plan;
    Err(not_implemented("IndexSeek"))
}
