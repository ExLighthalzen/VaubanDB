//! Ordering: [`Sort`](PhysicalPlan::Sort), the sort an index makes unnecessary,
//! [`TopN`](PhysicalPlan::TopN) when a `TOP` sits over an `ORDER BY`, and
//! [`Distinct`](PhysicalPlan::Distinct).

use vauban_binder::{BoundTop, LogicalPlan, SortKey};
use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::plan_node;

/// Plans a [`LogicalPlan::Sort`] into the operator that orders its rows.
pub(crate) fn plan_sort(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let LogicalPlan::Sort { input, keys } = plan else {
        return Err(bug("plan_sort: expected a Sort plan"));
    };
    let input = plan_node(input, ctx)?;
    if keys.is_empty() {
        return Ok(input);
    }
    // Remove the sort when the input already delivers the order.
    let delivered = delivered_order(&input);
    if starts_with(&delivered, keys) {
        return Ok(input);
    }
    Ok(PhysicalPlan::Sort {
        input: Box::new(input),
        keys: keys.clone(),
    })
}

/// Plans a [`LogicalPlan::Distinct`] into the operator that removes its duplicate rows.
pub(crate) fn plan_distinct(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let LogicalPlan::Distinct(input) = plan else {
        return Err(bug("plan_distinct: expected a Distinct plan"));
    };
    let input = plan_node(input, ctx)?;
    Ok(PhysicalPlan::Distinct(Box::new(input)))
}

/// Tries to turn a `TOP` over an ordered input into a single [`PhysicalPlan::TopN`].
pub(crate) fn try_top_n(
    top: &BoundTop,
    input: &LogicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<Option<PhysicalPlan>> {
    let LogicalPlan::Sort {
        input: sort_input,
        keys,
    } = input
    else {
        return Ok(None);
    };
    let input = plan_node(sort_input, ctx)?;
    Ok(Some(PhysicalPlan::TopN {
        input: Box::new(input),
        keys: keys.clone(),
        top: top.clone(),
    }))
}

/// The order a physical node delivers, most significant key first.
pub(crate) fn delivered_order(plan: &PhysicalPlan) -> Vec<SortKey> {
    match plan {
        PhysicalPlan::Sort { keys, .. } | PhysicalPlan::TopN { keys, .. } => keys.clone(),
        PhysicalPlan::Filter { input, .. } | PhysicalPlan::Top { input, .. } => {
            delivered_order(input)
        }
        _ => Vec::new(),
    }
}

/// Whether `delivered` starts with the keys `required` asks for, expression and direction
/// matching per key.
fn starts_with(delivered: &[SortKey], required: &[SortKey]) -> bool {
    if required.len() > delivered.len() {
        return false;
    }
    delivered
        .iter()
        .zip(required)
        .all(|(d, r)| keys_match(d, r))
}

/// Two keys match when their expression and direction are equal.
fn keys_match(a: &SortKey, b: &SortKey) -> bool {
    a.desc == b.desc && exprs_equal(&a.expr, &b.expr)
}

/// Whether two bound expressions are structurally identical.
fn exprs_equal(a: &vauban_binder::BoundExpr, b: &vauban_binder::BoundExpr) -> bool {
    use vauban_binder::BoundExprKind;
    match (&a.kind, &b.kind) {
        (BoundExprKind::Literal(va), BoundExprKind::Literal(vb)) => va == vb,
        (BoundExprKind::ColumnRef(ca), BoundExprKind::ColumnRef(cb)) => ca.index == cb.index,
        (
            BoundExprKind::Arith {
                op: oa,
                left: la,
                right: ra,
            },
            BoundExprKind::Arith {
                op: ob,
                left: lb,
                right: rb,
            },
        ) => oa == ob && exprs_equal(la, lb) && exprs_equal(ra, rb),
        _ => false,
    }
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
