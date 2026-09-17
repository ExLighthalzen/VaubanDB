//! `UNION`, `EXCEPT` and `INTERSECT`.
//!
//! The bound plan pairs its operands two by two, while the physical variants hold a
//! vector, so turning a chain of `UNION` into one node is the business of this file.
//!
//! # Where the duplicates are removed
//!
//! A `UNION` written without `ALL` becomes a [`PhysicalPlan::Distinct`] over a
//! [`PhysicalPlan::Union`] that keeps the duplicates, rather than a `Union` asked to
//! remove them itself: the executor already has the operator that removes duplicates, and
//! one operator does the work of two. [`PhysicalPlan::Except`] and
//! [`PhysicalPlan::Intersect`] do not go that way: removing the duplicates is part of what
//! they compute, not a flag on top.

use vauban_binder::{LogicalPlan, SetOpKind};
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::{not_implemented, plan_node};

/// Plans a [`LogicalPlan::SetOp`] into the set operator that runs it.
///
/// The schema of the node is the one the bound plan carries, copied across: the column
/// names and the common type of each column were settled when the statement was bound,
/// and nothing here recomputes a type (`tests/setop.rs`,
/// `setop_schema_comes_from_the_first_input`).
///
/// # Errors
///
/// The internal error 50000 when `plan` is not a `SetOp`, a call `plan.rs` does not make,
/// and the errors raised while planning an operand.
pub(crate) fn plan_setop(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let LogicalPlan::SetOp {
        op,
        all,
        left,
        right,
        schema,
    } = plan
    else {
        return Err(not_implemented(
            "setop::plan_setop",
            "a node of another kind",
        ));
    };
    let schema = schema.clone();
    match op {
        SetOpKind::Union => {
            let mut inputs = Vec::new();
            collect_union_operands(left, *all, ctx, &mut inputs)?;
            collect_union_operands(right, *all, ctx, &mut inputs)?;
            let union = PhysicalPlan::Union {
                inputs,
                all: true,
                schema,
            };
            if *all {
                Ok(union)
            } else {
                Ok(PhysicalPlan::Distinct(Box::new(union)))
            }
        }
        SetOpKind::Except => Ok(PhysicalPlan::Except {
            inputs: plan_pair(left, right, ctx)?,
            all: *all,
            schema,
        }),
        SetOpKind::Intersect => Ok(PhysicalPlan::Intersect {
            inputs: plan_pair(left, right, ctx)?,
            all: *all,
            schema,
        }),
    }
}

/// Plans two operands into the vector a two-input set operator holds, left first.
///
/// `EXCEPT` and `INTERSECT` are not associative the way `UNION` is: `a EXCEPT b` and
/// `b EXCEPT a` are different results, so the operands stay in the order they were
/// written and no level is merged into another (`tests/setop.rs`,
/// `except_and_intersect_keep_two_inputs_in_order`).
fn plan_pair(
    left: &LogicalPlan,
    right: &LogicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<Vec<PhysicalPlan>> {
    Ok(vec![plan_node(left, ctx)?, plan_node(right, ctx)?])
}

/// Appends the planned operands `node` contributes to a `Union`, flattening a nested one.
///
/// `all` is the flag of the `Union` being built, not of `node`. A nested `Union` is merged
/// into it when merging leaves the rows unchanged, which happens in two cases: the nested
/// one keeps its duplicates too, or the one being built has its duplicates removed above
/// and therefore removes those of the nested one as well (`tests/setop.rs`,
/// `union_all_levels_are_flattened` and `a_union_all_over_a_union_keeps_the_inner_dedup`).
/// Any other node is planned by the usual rules and becomes one operand
/// (`each_input_gets_the_planner_rules`).
fn collect_union_operands(
    node: &LogicalPlan,
    all: bool,
    ctx: &PlanContext<'_>,
    inputs: &mut Vec<PhysicalPlan>,
) -> SqlResult<()> {
    if let LogicalPlan::SetOp {
        op: SetOpKind::Union,
        all: nested_all,
        left,
        right,
        ..
    } = node
        && (*nested_all || !all)
    {
        collect_union_operands(left, all, ctx, inputs)?;
        collect_union_operands(right, all, ctx, inputs)?;
        return Ok(());
    }
    inputs.push(plan_node(node, ctx)?);
    Ok(())
}
