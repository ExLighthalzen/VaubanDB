//! The join operators: [`NestedLoopJoin`](PhysicalPlan::NestedLoopJoin) by default, a seek
//! on the index of the inner table when an equality of the `ON` matches an indexed column
//! of that table, and [`HashJoin`](PhysicalPlan::HashJoin) on equality when no index is
//! usable on the inner side.
//!
//! # Left–right convention
//!
//! [`LogicalPlan::Join`](vauban_binder::LogicalPlan::Join) keeps the order the user wrote,
//! and so do the physical nodes. `outer` (or `probe`) is the left operand, `inner` (or
//! `build`) is the right operand. The choice of which side to build the hash table on is
//! arbitrary here, not cost-based: it follows the written order, which makes the plans
//! deterministic and the tests readable. A cost-based choice belongs to V3.
//!
//! # Seek-on-inner
//!
//! When the inner side is (or resolves to) a [`TableScan`] and an equality of the `ON`
//! matches a key column of an index of that table, the inner becomes an
//! [`IndexSeek`] whose bounds are the expressions of the outer side. Those bounds
//! may contain [`ColumnRef`](vauban_binder::BoundExprKind::ColumnRef)s that designate
//! the outer row — the executor evaluates them with the outer row in context
//! (`eval_expr(expr, row, ctx)`), not with `row: None` as a standalone seek would.
//!
//! The conjunctions consumed by the range are removed from the `on` field of the
//! [`NestedLoopJoin`]; the remaining ones stay on it.

use vauban_binder::{BoundExpr, BoundExprKind, CompareOp, LogicalOp, LogicalPlan};
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::{KeyRangeExpr, PhysicalJoinKind, PhysicalPlan};
use crate::plan::plan_node;

/// Plans a [`LogicalPlan::Join`] into the join operator that runs it.
///
/// `plan` is the `Join` node itself, so that the rule may look at both inputs before
/// choosing the operator; it plans its children through
/// [`plan_node`](crate::plan::plan_node).
pub(crate) fn plan_join(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let LogicalPlan::Join {
        left,
        right,
        kind,
        on,
        schema,
    } = plan
    else {
        unreachable!("plan_join called on a non-Join node")
    };

    let left_plan = plan_node(left, ctx)?;
    let right_plan = plan_node(right, ctx)?;

    let pk = PhysicalJoinKind::from(*kind);
    let left_width = left.schema().columns.len();

    match pk {
        PhysicalJoinKind::Cross => Ok(PhysicalPlan::NestedLoopJoin {
            outer: Box::new(left_plan),
            inner: Box::new(right_plan),
            kind: PhysicalJoinKind::Cross,
            on: None,
            schema: schema.clone(),
        }),
        PhysicalJoinKind::Right | PhysicalJoinKind::Full => Ok(PhysicalPlan::NestedLoopJoin {
            outer: Box::new(left_plan),
            inner: Box::new(right_plan),
            kind: pk,
            on: on.clone(),
            schema: schema.clone(),
        }),
        PhysicalJoinKind::Inner | PhysicalJoinKind::Left => {
            let on_pred = on.as_ref();
            if let Some(on_pred) = on_pred {
                // Try to turn the inner side into an IndexSeek.
                if let Some((inner_with_seek, remaining_on)) =
                    try_inner_seek(on_pred, &right_plan, left_width, ctx)?
                {
                    return Ok(PhysicalPlan::NestedLoopJoin {
                        outer: Box::new(left_plan),
                        inner: Box::new(inner_with_seek),
                        kind: pk,
                        on: remaining_on,
                        schema: schema.clone(),
                    });
                }
                // Try a hash join when at least one equality is present.
                if let Some(hash) = try_hash_join(on_pred, &left_plan, &right_plan, pk, schema) {
                    return Ok(hash);
                }
            }
            // Default: nested loop with the ON predicate.
            Ok(PhysicalPlan::NestedLoopJoin {
                outer: Box::new(left_plan),
                inner: Box::new(right_plan),
                kind: pk,
                on: on.clone(),
                schema: schema.clone(),
            })
        }
        PhysicalJoinKind::Semi | PhysicalJoinKind::AntiSemi => {
            unreachable!("Semi and AntiSemi are not produced by LogicalPlan::Join")
        }
    }
}

/// Tries to replace the inner (right) side of a join with an [`IndexSeek`] driven by an
/// equality of the `ON` predicate.
///
/// `inner` is the already-planned right input. When it contains a [`TableScan`], and an
/// equality of `on` matches an indexed column of that table, the scan is replaced by an
/// [`IndexSeek`] whose bound expression is the outer side of the equality. The equality is
/// removed from the returned residual predicate.
///
/// Returns `Ok(Some((new_inner, remaining_on)))` on success, `Ok(None)` when no suitable
/// equality was found.
fn try_inner_seek(
    on: &BoundExpr,
    inner: &PhysicalPlan,
    left_width: usize,
    ctx: &PlanContext<'_>,
) -> SqlResult<Option<(PhysicalPlan, Option<BoundExpr>)>> {
    let mut conjuncts = Vec::new();
    split_conjunction(on, &mut conjuncts);

    // Find the TableScan in the inner plan tree.
    let Some(scan) = find_table_scan(inner) else {
        return Ok(None);
    };
    let PhysicalPlan::TableScan {
        table,
        columns,
        schema: scan_schema,
        ..
    } = scan
    else {
        return Ok(None);
    };

    let indexes = ctx.catalog.indexes_of(*table);
    if indexes.is_empty() {
        return Ok(None);
    }

    // Find an equality that matches an index on the inner table.
    for (position, expr) in conjuncts.iter().enumerate() {
        let BoundExprKind::Compare {
            op: CompareOp::Eq,
            left,
            right,
        } = &expr.kind
        else {
            continue;
        };

        // Determine which side is the inner column reference.
        let (inner_binding, outer_expr) = match (&left.kind, &right.kind) {
            (BoundExprKind::ColumnRef(binding), _) if binding.index >= left_width => {
                (binding, right.as_ref())
            }
            (_, BoundExprKind::ColumnRef(binding)) if binding.index >= left_width => {
                (binding, left.as_ref())
            }
            _ => continue,
        };

        let inner_storage_pos = inner_binding.index - left_width;

        // Check each index for a match on this column.
        for (index_id, shape) in &indexes {
            if shape.columns.is_empty() {
                continue;
            }
            let first_key = &shape.columns[0];
            if usize::from(first_key.column) != inner_storage_pos {
                continue;
            }

            // Build the IndexSeek with the outer expression as the point bound.
            let seek = PhysicalPlan::IndexSeek {
                index: *index_id,
                range: KeyRangeExpr::Point(vec![outer_expr.clone()]),
                columns: columns.clone(),
                direction: vauban_storage::Direction::Forward,
                schema: scan_schema.clone(),
            };

            // Replace the scan with the seek in the inner plan tree.
            let inner_with_seek = replace_scan_with_seek(inner, seek);

            // Remove the consumed conjunction from on.
            let remaining_on = conjuncts
                .iter()
                .enumerate()
                .filter(|(pos, _)| *pos != position)
                .map(|(_, expr)| (*expr).clone())
                .reduce(|left, right| and(left, right, on));

            return Ok(Some((inner_with_seek, remaining_on)));
        }
    }

    Ok(None)
}

/// Tries to build a [`HashJoin`] from the equality conjunctions of `on`.
///
/// Returns `Some(HashJoin)` when `on` has at least one equality. The equality pairs become
/// `keys`, the rest becomes `residual`.
fn try_hash_join(
    on: &BoundExpr,
    left_plan: &PhysicalPlan,
    right_plan: &PhysicalPlan,
    kind: PhysicalJoinKind,
    schema: &vauban_binder::OutputSchema,
) -> Option<PhysicalPlan> {
    let mut conjuncts = Vec::new();
    split_conjunction(on, &mut conjuncts);

    let mut keys: Vec<(BoundExpr, BoundExpr)> = Vec::new();
    let mut residual_conjuncts: Vec<BoundExpr> = Vec::new();

    for expr in &conjuncts {
        let BoundExprKind::Compare {
            op: CompareOp::Eq,
            left,
            right,
        } = &expr.kind
        else {
            residual_conjuncts.push((*expr).clone());
            continue;
        };

        keys.push((*(*left).clone(), *(*right).clone()));
    }

    if keys.is_empty() {
        return None;
    }

    let residual = residual_conjuncts
        .into_iter()
        .reduce(|left, right| and(left, right, on));

    Some(PhysicalPlan::HashJoin {
        build: Box::new(right_plan.clone()),
        probe: Box::new(left_plan.clone()),
        kind,
        keys,
        residual,
        schema: schema.clone(),
    })
}

/// Finds the bottom [`TableScan`] in a plan tree, looking through [`Filter`] nodes.
fn find_table_scan(plan: &PhysicalPlan) -> Option<&PhysicalPlan> {
    match plan {
        PhysicalPlan::TableScan { .. } => Some(plan),
        PhysicalPlan::Filter { input, .. } => find_table_scan(input),
        _ => None,
    }
}

/// Replaces the bottom [`TableScan`] in `plan` with `seek`, preserving any [`Filter`]
/// nodes above it.
fn replace_scan_with_seek(plan: &PhysicalPlan, seek: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::TableScan { .. } => seek,
        PhysicalPlan::Filter { input, predicate } => {
            let new_input = replace_scan_with_seek(input, seek);
            PhysicalPlan::Filter {
                input: Box::new(new_input),
                predicate: predicate.clone(),
            }
        }
        other => other.clone(),
    }
}

/// Appends to `out` the top-level conjunctions of `expr`: `a AND (b AND c)` gives `a`,
/// `b`, `c`; anything else, an `OR` included, is one conjunction.
fn split_conjunction<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match &expr.kind {
        BoundExprKind::Logical {
            op: LogicalOp::And,
            left,
            right,
        } => {
            split_conjunction(left, out);
            split_conjunction(right, out);
        }
        _ => out.push(expr),
    }
}

/// Reassembles two conjunctions with `AND`, typed and positioned like the predicate they
/// were split from.
fn and(left: BoundExpr, right: BoundExpr, predicate: &BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Logical {
            op: LogicalOp::And,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: predicate.ty.clone(),
        line: predicate.line,
    }
}
