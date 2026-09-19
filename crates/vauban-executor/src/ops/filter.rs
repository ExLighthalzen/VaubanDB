//! `Filter`: keeps the rows of its input whose predicate is **true**.
//!
//! Three-valued logic decides the two other outcomes the same way: `false` and *unknown*
//! both drop the row, because `WHERE` keeps what is true and the unknown is not true.
//! `SELECT 1 WHERE NULL = NULL` therefore answers a result set of one column and **no
//! row**, exactly like `SELECT 1 WHERE 1 = 0` (`tests/execute_select.rs`).

use vauban_binder::{BoundExpr, BoundExprKind, OutputSchema};
use vauban_errors::SqlResult;
use vauban_planner::PhysicalPlan;

use crate::context::{CANCEL_CHECK_ROWS, ExecContext, SubqueryLocalsSource};
use crate::expr::{as_condition, eval_expr};
use crate::operator::{Operator, build_operator};
use crate::row::Row;

fn predicate_holds_subquery(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Exists(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::InSubquery { .. } => true,
        BoundExprKind::Not(inner)
        | BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => predicate_holds_subquery(inner),
        BoundExprKind::Logical { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Arith { left, right, .. } => {
            predicate_holds_subquery(left) || predicate_holds_subquery(right)
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            operand
                .as_ref()
                .is_some_and(|item| predicate_holds_subquery(item))
                || arms.iter().any(|arm| {
                    predicate_holds_subquery(&arm.when) || predicate_holds_subquery(&arm.then)
                })
                || else_
                    .as_ref()
                    .is_some_and(|item| predicate_holds_subquery(item))
        }
        BoundExprKind::In {
            expr: tested, list, ..
        } => predicate_holds_subquery(tested) || list.iter().any(predicate_holds_subquery),
        BoundExprKind::Like {
            expr: tested,
            pattern,
            escape,
            ..
        } => {
            predicate_holds_subquery(tested)
                || predicate_holds_subquery(pattern)
                || escape
                    .as_ref()
                    .is_some_and(|item| predicate_holds_subquery(item))
        }
        BoundExprKind::Function { args, .. } => args.iter().any(predicate_holds_subquery),
        BoundExprKind::Literal(_)
        | BoundExprKind::ColumnRef(_)
        | BoundExprKind::Variable { .. } => false,
    }
}

/// The rows of `input` for which `predicate` is true.
struct Filter<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    predicate: BoundExpr,
}

/// Builds the operator of a [`PhysicalPlan::Filter`].
///
/// # Errors
///
/// What building the input raises.
pub(crate) fn build<'a>(
    input: &PhysicalPlan,
    predicate: &BoundExpr,
) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    Ok(Box::new(Filter {
        input: build_operator(input)?,
        predicate: predicate.clone(),
    }))
}

impl<'a> Operator<'a> for Filter<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)
    }

    /// Pulls the input until a row passes. A run of rejected rows reads the cancellation
    /// token once per [`CANCEL_CHECK_ROWS`] rejected rows, since the driving loop sees
    /// no rejected row, and answers `None` once the token is up.
    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        let mut rejected = 0u64;
        while let Some(row) = self.input.next(ctx)? {
            // `as_condition` answers `None` for the unknown of the three-valued logic;
            // `Some(true)` alone keeps the row (`tests/execute_select.rs`).
            let inner_width = ctx.subquery_row_binding_count();
            let passes = if ctx.subquery_locals_source()
                == Some(SubqueryLocalsSource::JoinInnerScan)
                && row.len() > inner_width
                && !predicate_holds_subquery(&self.predicate)
            {
                ctx.eval_over_concatenated_row(|ctx| {
                    as_condition(&eval_expr(&self.predicate, Some(&row), ctx)?)
                })?
            } else {
                as_condition(&eval_expr(&self.predicate, Some(&row), ctx)?)?
            };
            if passes == Some(true) {
                return Ok(Some(row));
            }
            rejected += 1;
            if rejected.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                return Ok(None);
            }
        }
        Ok(None)
    }

    fn close(&mut self) {
        self.input.close();
    }

    fn schema(&self) -> &OutputSchema {
        self.input.schema()
    }
}
