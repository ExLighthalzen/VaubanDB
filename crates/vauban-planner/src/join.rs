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
//! # Comma join and the `WHERE`
//!
//! The binder keeps `FROM a, b WHERE a.k = b.k` as a cross join with a [`Filter`] above
//! it. The physical shape is a [`NestedLoopJoin`] of kind [`PhysicalJoinKind::Cross`]
//! under that filter, not a [`HashJoin`] built from the `WHERE` equalities.
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
//!
//! # Subqueries in the `ON`
//!
//! A conjunction that holds a subquery is neither a seek bound nor a hash key: it stays in
//! the `on` of the loop or the `residual` of the hash join, and the join is laid under a
//! [`SubqueryEval`](PhysicalPlan::SubqueryEval) that carries the plans of those subqueries
//! ([`plan_on_subqueries`](crate::subquery::plan_on_subqueries); `tests/join.rs`,
//! `a_subquery_in_the_on_puts_the_loop_under_a_subquery_eval`,
//! `a_subquery_in_the_on_stays_in_the_residual_of_the_hash_join`).

use vauban_binder::{BoundExpr, BoundExprKind, CompareOp, LogicalOp, LogicalPlan};
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::{KeyRangeExpr, PhysicalJoinKind, PhysicalPlan};
use crate::plan::plan_node;
use crate::subquery::plan_on_subqueries;

/// Plans a [`LogicalPlan::Join`] into the join operator that runs it.
///
/// `plan` is the `Join` node itself, so that the rule may look at both inputs before
/// choosing the operator; it plans its children through
/// [`plan_node`](crate::plan::plan_node).
pub(crate) fn plan_join(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let joined = plan_join_operator(plan, ctx)?;
    match plan {
        LogicalPlan::Join { on: Some(on), .. } => plan_on_subqueries(on, joined, ctx),
        _ => Ok(joined),
    }
}

/// Chooses the operator of a [`LogicalPlan::Join`], the subqueries of its `ON` left to
/// [`plan_join`].
fn plan_join_operator(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
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
                if let Some(hash) =
                    try_hash_join(on_pred, &left_plan, &right_plan, pk, schema, left_width)
                {
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
        hints,
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

        // One member is a column of the inner row, the other reads no inner column: the
        // bound is evaluated before the inner row it looks for exists.
        let (inner_binding, outer_expr) = match (&left.kind, &right.kind) {
            (BoundExprKind::ColumnRef(binding), _)
                if is_inner_column(binding, left_width)
                    && sides(right, left_width).outer_or_constant() =>
            {
                (binding, right.as_ref())
            }
            (_, BoundExprKind::ColumnRef(binding))
                if is_inner_column(binding, left_width)
                    && sides(left, left_width).outer_or_constant() =>
            {
                (binding, left.as_ref())
            }
            _ => continue,
        };

        // The position in the inner row is a position in the output of the scan; the
        // index counts positions in the row `storage` produces, which the scan's column
        // binding carries.
        let Some(inner_storage_pos) = columns
            .get(inner_binding.index - left_width)
            .map(|column| column.index)
        else {
            continue;
        };

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
                hints: *hints,
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
/// Returns `Some(HashJoin)` when `on` has at least one equality. An equality with one
/// member reading the left row alone and the other the right row alone becomes a key,
/// right member first (the `build` side); the other conjunctions, equalities included,
/// become `residual`.
fn try_hash_join(
    on: &BoundExpr,
    left_plan: &PhysicalPlan,
    right_plan: &PhysicalPlan,
    kind: PhysicalJoinKind,
    schema: &vauban_binder::OutputSchema,
    left_width: usize,
) -> Option<PhysicalPlan> {
    let mut has_equality = false;
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
        has_equality = true;

        let left_sides = sides(left, left_width);
        let right_sides = sides(right, left_width);
        if left_sides.outer_only() && right_sides.inner_only() {
            keys.push((*(*right).clone(), *(*left).clone()));
        } else if left_sides.inner_only() && right_sides.outer_only() {
            keys.push((*(*left).clone(), *(*right).clone()));
        } else {
            // A member that reads both rows, two members that read the same row, or a
            // constant member: the equality cannot pair the two sides through the hash
            // table, so it is checked on each paired row.
            residual_conjuncts.push((*expr).clone());
        }
    }

    if !has_equality {
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

/// Which rows of a join an expression reads: the left (`outer`) one, the right (`inner`)
/// one, and whether it holds a subquery, whose references are not walked here.
#[derive(Clone, Copy, Default)]
struct Sides {
    outer: bool,
    inner: bool,
    subquery: bool,
}

impl Sides {
    fn union(self, other: Self) -> Self {
        Self {
            outer: self.outer || other.outer,
            inner: self.inner || other.inner,
            subquery: self.subquery || other.subquery,
        }
    }

    /// Reads the left row and nothing of the right one.
    fn outer_only(self) -> bool {
        self.outer && !self.inner && !self.subquery
    }

    /// Reads the right row and nothing of the left one.
    fn inner_only(self) -> bool {
        self.inner && !self.outer && !self.subquery
    }

    /// Reads nothing of the right row: an expression of the left row, or a constant.
    fn outer_or_constant(self) -> bool {
        !self.inner && !self.subquery
    }
}

/// True when `binding` is a column of the right operand of a join whose left operand
/// has `left_width` columns.
fn is_inner_column(binding: &vauban_binder::ColumnBinding, left_width: usize) -> bool {
    binding.depth == 0 && binding.index >= left_width
}

/// The rows of the join `expr` reads. A column of an enclosing query (`depth > 0`) is a
/// constant for the join and reads neither row.
fn sides(expr: &BoundExpr, left_width: usize) -> Sides {
    let all = |exprs: &mut dyn Iterator<Item = &BoundExpr>| {
        exprs.fold(Sides::default(), |acc, item| {
            acc.union(sides(item, left_width))
        })
    };
    match &expr.kind {
        BoundExprKind::ColumnRef(binding) if binding.depth > 0 => Sides::default(),
        BoundExprKind::ColumnRef(binding) => Sides {
            outer: binding.index < left_width,
            inner: binding.index >= left_width,
            subquery: false,
        },
        BoundExprKind::Literal(_) | BoundExprKind::Variable { .. } => Sides::default(),
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => sides(inner, left_width),
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => {
            sides(left, left_width).union(sides(right, left_width))
        }
        BoundExprKind::In {
            expr: tested, list, ..
        } => all(&mut std::iter::once(tested.as_ref()).chain(list.iter())),
        BoundExprKind::Like {
            expr: tested,
            pattern,
            escape,
            ..
        } => all(&mut [tested.as_ref(), pattern.as_ref()]
            .into_iter()
            .chain(escape.as_deref())),
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => all(&mut operand
            .as_deref()
            .into_iter()
            .chain(arms.iter().flat_map(|arm| [&arm.when, &arm.then]))
            .chain(else_.as_deref())),
        BoundExprKind::Function { args, .. } => all(&mut args.iter()),
        BoundExprKind::Exists(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::InSubquery { .. } => Sides {
            subquery: true,
            ..Sides::default()
        },
    }
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
