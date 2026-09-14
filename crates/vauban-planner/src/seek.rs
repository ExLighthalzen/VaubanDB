//! Turning a `Filter` over a read into a [`PhysicalPlan::IndexSeek`]: equality on a primary
//! key or on a unique index, range on a key prefix. The entry point below, the
//! [`IndexSeek`](PhysicalPlan::IndexSeek) variant, the
//! [`KeyRangeExpr`](crate::KeyRangeExpr) it carries and the
//! [`PlanCatalog`](crate::PlanCatalog) it reads the indexes from exist; the rules are not
//! written yet.

use vauban_binder::BoundExpr;
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;

/// Tries to serve `predicate` over `input` with an index instead of a scan.
///
/// `Ok(None)` says "no seek for this predicate", which is what `plan.rs` turns into the
/// `Filter` over `input` it would have built anyway. That is the answer for now: no rule
/// exists yet, so no index is chosen, and a `Filter` over a `TableScan` keeps its shape
/// even when the catalogue declares a unique index on the filtered column
/// (`tests/trivial.rs`, `filter_project_over_scan_keeps_its_shape`).
pub(crate) fn try_index_seek(
    predicate: &BoundExpr,
    input: &PhysicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<Option<PhysicalPlan>> {
    let _ = (predicate, input, ctx);
    Ok(None)
}
