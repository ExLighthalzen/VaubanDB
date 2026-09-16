//! Turning a `Filter` over a table scan into a [`PhysicalPlan::IndexSeek`]: equality on a
//! primary key or on a unique index, range on a key prefix.
//!
//! # The rule
//!
//! The predicate is split into its top-level conjunctions (`AND` unfolded; an `OR` at the
//! top is one conjunction and serves no seek, `tests/seek.rs`,
//! `an_or_at_the_top_is_not_split`). A conjunction is usable when it compares a column of
//! the scanned table with a *bound*, in either order of writing (`col = 3` and `3 = col`),
//! with one of `=`, `<`, `<=`, `>`, `>=`.
//!
//! A bound is an expression that reads no column: a literal, a variable, a negated bound,
//! a `CONVERT` of a bound, or a function call whose arguments are bounds. `WHERE a = b`
//! reads a column on both sides and serves no seek (`tests/seek.rs`,
//! `a_bound_that_reads_a_column_is_not_a_key`). A bound is kept as an expression: the
//! planner computes no value, the executor evaluates the bounds when it opens the operator
//! (`tests/seek.rs`, `a_variable_bound_is_kept_as_an_expression`). `NULL` is a bound like
//! another literal, with no special case here.
//!
//! An index applies when an equality is found on its first key column. The equalities
//! consumed on the first `k` key columns form the prefix; at most one range column
//! follows, column `k + 1`, with one or two bounds. The result is
//! [`KeyRangeExpr::Point`] when the equalities cover the whole key and
//! [`KeyRangeExpr::Between`] otherwise; [`KeyRangeExpr::Full`] is not produced by this
//! file. A range bound on a descending key column is written on the other side of the
//! `Between`, since the bounds are expressed in index order
//! (`tests/seek.rs`, `a_descending_range_column_flips_the_bounds`).
//!
//! The conjunctions not consumed by the range are reassembled with `AND` into a `Filter`
//! above the seek; the consumed ones are not kept there (`tests/seek.rs`,
//! `residual_predicate_stays_in_a_filter`). With no conjunction left, the seek stands
//! alone.
//!
//! # Which index
//!
//! The indexes are read from [`PlanCatalog::indexes_of`](crate::PlanCatalog::indexes_of)
//! and the **first applicable one in that order** is chosen (`tests/seek.rs`,
//! `the_first_applicable_index_wins`). There is no comparison between two applicable
//! indexes, no cost, and no preference for a unique index over a non-unique one; a
//! cost-based choice belongs to a later version, where statistics exist. The seek is
//! [`Direction::Forward`]; a rule that avoids a descending sort may flip it.
//!
//! # What is left out
//!
//! An `OR` rewritten into a union of ranges, a range on an expression of a column, and a
//! bound that reads the outer row of a nested loop are not handled here: a bound that
//! reads a column, from whichever table, disqualifies its conjunction.

use std::ops::Bound;

use vauban_binder::{BoundExpr, BoundExprKind, CompareOp, LogicalOp};
use vauban_errors::SqlResult;
use vauban_storage::{Direction, IndexShape};

use crate::context::PlanContext;
use crate::physical::{KeyRangeExpr, PhysicalPlan};
use crate::subquery;

/// Tries to serve `predicate` over `input` with an index instead of a scan.
///
/// `Ok(None)` says "no seek for this predicate", which is what `plan.rs` turns into the
/// `Filter` over `input` it would have built anyway: `input` is not a
/// [`TableScan`](PhysicalPlan::TableScan), the table has no index, or no index has an
/// equality on its first key column (`tests/seek.rs`,
/// `a_non_indexed_column_keeps_the_table_scan`).
///
/// `Ok(Some(_))` is the [`IndexSeek`](PhysicalPlan::IndexSeek), under a `Filter` holding
/// the residual predicate when one remains. The seek carries the `columns` and the
/// `schema` of the scan it replaces, in the same order (`tests/seek.rs`,
/// `seek_keeps_the_output_schema`).
///
/// # Errors
///
/// What planning the subqueries of the residual predicate raises, as for the predicate of
/// a `Filter` built by `plan.rs`.
pub(crate) fn try_index_seek(
    predicate: &BoundExpr,
    input: &PhysicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<Option<PhysicalPlan>> {
    let PhysicalPlan::TableScan {
        table,
        columns,
        alias: _,
        schema,
        hints,
    } = input
    else {
        return Ok(None);
    };

    let mut conjuncts = Vec::new();
    split_conjunction(predicate, &mut conjuncts);
    let conditions: Vec<Option<KeyCondition<'_>>> =
        conjuncts.iter().map(|expr| key_condition(expr)).collect();
    if conditions.iter().all(Option::is_none) {
        return Ok(None);
    }

    for (index, shape) in ctx.catalog.indexes_of(*table) {
        let Some(matched) = match_index(&shape, &conditions) else {
            continue;
        };
        let seek = PhysicalPlan::IndexSeek {
            index,
            range: matched.range,
            columns: columns.clone(),
            direction: Direction::Forward,
            schema: schema.clone(),
            hints: *hints,
        };
        let residual = conjuncts
            .iter()
            .enumerate()
            .filter(|(position, _)| !matched.consumed.contains(position))
            .map(|(_, expr)| (*expr).clone())
            .reduce(|left, right| and(left, right, predicate));
        return Ok(Some(match residual {
            Some(residual) => {
                let input = subquery::plan_expr_subqueries(&residual, seek, ctx)?;
                PhysicalPlan::Filter {
                    input: Box::new(input),
                    predicate: residual,
                }
            }
            None => seek,
        }));
    }
    Ok(None)
}

/// One usable conjunction: a column of the scanned table, compared with a bound.
///
/// `op` is the operator with the column on its left, whichever side it was written on:
/// `3 < col` is read as `col > 3`.
struct KeyCondition<'a> {
    /// The 0-based position of the column in the row `storage` produces, which is what
    /// [`KeyColumn::column`](vauban_storage::KeyColumn::column) counts.
    column: usize,
    /// The comparison, column on the left.
    op: CompareOp,
    /// The bound, an expression that reads no column.
    bound: &'a BoundExpr,
}

/// The outcome of matching the conditions against one index.
struct Matched {
    /// The keys served.
    range: KeyRangeExpr,
    /// The positions, in the conjunction list, of the conditions the range consumed.
    consumed: Vec<usize>,
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

/// Reads `expr` as a comparison between a column and a bound, in either order of writing.
///
/// `None` for anything else: another operator (`<>` is not a range), two columns, two
/// bounds, or a bound that reads a column.
fn key_condition(expr: &BoundExpr) -> Option<KeyCondition<'_>> {
    let BoundExprKind::Compare { op, left, right } = &expr.kind else {
        return None;
    };
    let op = *op;
    match (&left.kind, &right.kind) {
        (BoundExprKind::ColumnRef(binding), _) if is_bound(right) => Some(KeyCondition {
            column: binding.index,
            op: range_op(op)?,
            bound: right,
        }),
        (_, BoundExprKind::ColumnRef(binding)) if is_bound(left) => Some(KeyCondition {
            column: binding.index,
            op: range_op(mirrored(op))?,
            bound: left,
        }),
        _ => None,
    }
}

/// The operators that bound a range; `<>` is not one of them.
fn range_op(op: CompareOp) -> Option<CompareOp> {
    match op {
        CompareOp::Eq | CompareOp::Lt | CompareOp::Le | CompareOp::Gt | CompareOp::Ge => Some(op),
        CompareOp::Ne => None,
    }
}

/// The operator read from the other side: `3 < col` is `col > 3`.
fn mirrored(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::Le => CompareOp::Ge,
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::Ge => CompareOp::Le,
        CompareOp::Eq | CompareOp::Ne => op,
    }
}

/// True when `expr` is a bound: a literal, a variable, a negated bound, a `CONVERT` of a
/// bound, or a function call whose arguments are bounds. A column reference, a
/// subquery, or a shape outside that list makes the expression no bound.
fn is_bound(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Literal(_) | BoundExprKind::Variable { .. } => true,
        BoundExprKind::Negate(inner) | BoundExprKind::Convert { expr: inner, .. } => {
            is_bound(inner)
        }
        BoundExprKind::Function { args, .. } => args.iter().all(is_bound),
        _ => false,
    }
}

/// Matches the conditions against the key columns of `shape`, in key order.
///
/// Each key column takes the first unconsumed equality found on it; the first column
/// without one takes the first lower bound and the first upper bound found on it, if any,
/// and closes the range. `None` when the first key column has no condition.
fn match_index(shape: &IndexShape, conditions: &[Option<KeyCondition<'_>>]) -> Option<Matched> {
    let mut consumed: Vec<usize> = Vec::new();
    let mut prefix: Vec<BoundExpr> = Vec::new();
    let mut lower: Option<(bool, &BoundExpr)> = None;
    let mut upper: Option<(bool, &BoundExpr)> = None;

    for key in &shape.columns {
        let on_column = conditions
            .iter()
            .enumerate()
            .filter_map(|(position, condition)| condition.as_ref().map(|c| (position, c)))
            .filter(|(position, condition)| {
                condition.column == usize::from(key.column) && !consumed.contains(position)
            });
        let mut equality = None;
        let mut low = None;
        let mut high = None;
        for (position, condition) in on_column {
            match condition.op {
                CompareOp::Eq => {
                    if equality.is_none() {
                        equality = Some((position, condition));
                    }
                }
                CompareOp::Gt | CompareOp::Ge => {
                    if low.is_none() {
                        low = Some((position, condition));
                    }
                }
                CompareOp::Lt | CompareOp::Le => {
                    if high.is_none() {
                        high = Some((position, condition));
                    }
                }
                CompareOp::Ne => {}
            }
        }
        if let Some((position, condition)) = equality {
            consumed.push(position);
            prefix.push(condition.bound.clone());
            continue;
        }
        // A descending key column stores its values in reverse: a lower bound on the
        // value is an upper bound in index order, and the other way round.
        let (low, high) = if key.descending {
            (high, low)
        } else {
            (low, high)
        };
        if let Some((position, condition)) = low {
            consumed.push(position);
            lower = Some((inclusive(condition.op), condition.bound));
        }
        if let Some((position, condition)) = high {
            consumed.push(position);
            upper = Some((inclusive(condition.op), condition.bound));
        }
        break;
    }

    if consumed.is_empty() {
        return None;
    }
    let range = if prefix.len() == shape.columns.len() {
        KeyRangeExpr::Point(prefix)
    } else {
        KeyRangeExpr::Between(side(&prefix, lower), side(&prefix, upper))
    };
    Some(Matched { range, consumed })
}

/// Whether a bound written with `op` includes its own value: `<=` and `>=` do, `<` and
/// `>` do not.
fn inclusive(op: CompareOp) -> bool {
    matches!(op, CompareOp::Le | CompareOp::Ge)
}

/// One side of a [`KeyRangeExpr::Between`]: the prefix extended with the bound when there
/// is one, otherwise the whole set of keys starting with the prefix, which is
/// [`Bound::Unbounded`] when the prefix is empty.
fn side(prefix: &[BoundExpr], bound: Option<(bool, &BoundExpr)>) -> Bound<Vec<BoundExpr>> {
    match bound {
        Some((inclusive, expr)) => {
            let mut key = prefix.to_vec();
            key.push(expr.clone());
            if inclusive {
                Bound::Included(key)
            } else {
                Bound::Excluded(key)
            }
        }
        None if prefix.is_empty() => Bound::Unbounded,
        None => Bound::Included(prefix.to_vec()),
    }
}
