//! Subqueries: the decorrelated semi-join of an `IN` or an `EXISTS`, and
//! [`SubqueryEval`](PhysicalPlan::SubqueryEval) for the ones that stay correlated.
//! The two entry points below, that variant, the [`SubPlan`](crate::SubPlan) it holds and
//! the two join kinds [`Semi`](crate::PhysicalJoinKind::Semi) and
//! [`AntiSemi`](crate::PhysicalJoinKind::AntiSemi) exist; the rules are not written yet.

use vauban_binder::{BoundExpr, BoundExprKind, LogicalPlan};
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::not_implemented;

/// Plans a [`LogicalPlan::Subquery`], the derived table `FROM (SELECT …) AS d`.
pub(crate) fn plan_subquery(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let _ = (plan, ctx);
    Err(not_implemented(
        "subquery::plan_subquery",
        "a derived table",
    ))
}

/// Plans the subqueries `expr` holds, over the rows `input` produces.
///
/// `plan.rs` calls this at two sites, and at those two only: the predicate of a
/// [`Filter`](LogicalPlan::Filter) and each expression of a
/// [`Project`](LogicalPlan::Project) (`plan.rs`, `plan_node`). On the expression handed
/// here, the rule is to answer the [`SubqueryEval`](PhysicalPlan::SubqueryEval) that
/// computes its subqueries, or a semi-join substituted for the whole predicate.
///
/// Until that rule is written the answer is `input` unchanged for an expression that
/// holds no subquery, and an internal error for one that does
/// (`tests/trivial.rs`, `a_subquery_expression_is_not_implemented_yet`). The three kinds
/// that hold a plan are looked for at each depth of that expression:
/// [`Exists`](BoundExprKind::Exists),
/// [`ScalarSubquery`](BoundExprKind::ScalarSubquery) and
/// [`InSubquery`](BoundExprKind::InSubquery).
///
/// # The expression holders these two sites do not reach
///
/// `plan.rs` copies the other expressions it carries across without reading them, so a
/// subquery written in one of them reaches the `executor` inside the operator, unchecked:
/// [`BoundTop::expr`](vauban_binder::BoundTop::expr) of a
/// [`Limit`](LogicalPlan::Limit), the rows of a [`Values`](LogicalPlan::Values), the
/// condition of an [`If`](vauban_binder::BoundStatement::If) or of a
/// [`While`](vauban_binder::BoundStatement::While), and the expression of
/// [`Print`](vauban_binder::BoundStatement::Print),
/// [`SetVariable`](vauban_binder::BoundStatement::SetVariable) and
/// [`Return`](vauban_binder::BoundStatement::Return). That state is pinned by a test,
/// `tests/trivial.rs`, `subquery_holders_outside_the_two_call_sites_go_through`, which the
/// rule that plans them inverts.
pub(crate) fn plan_expr_subqueries(
    expr: &BoundExpr,
    input: PhysicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalPlan> {
    let _ = ctx;
    if holds_subquery(expr) {
        return Err(not_implemented(
            "subquery::plan_expr_subqueries",
            "an expression holding a subquery",
        ));
    }
    Ok(input)
}

/// True when `expr`, or one of its operands, is one of the three kinds that hold a
/// [`LogicalPlan`]: `Exists`, `ScalarSubquery` and `InSubquery`.
fn holds_subquery(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Exists(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::InSubquery { .. } => true,
        BoundExprKind::Literal(_)
        | BoundExprKind::ColumnRef(_)
        | BoundExprKind::Variable { .. } => false,
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => holds_subquery(inner),
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => {
            holds_subquery(left) || holds_subquery(right)
        }
        BoundExprKind::In {
            expr: tested, list, ..
        } => holds_subquery(tested) || list.iter().any(holds_subquery),
        BoundExprKind::Like {
            expr: tested,
            pattern,
            escape,
            ..
        } => {
            holds_subquery(tested)
                || holds_subquery(pattern)
                || escape.as_deref().is_some_and(holds_subquery)
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            operand.as_deref().is_some_and(holds_subquery)
                || arms
                    .iter()
                    .any(|arm| holds_subquery(&arm.when) || holds_subquery(&arm.then))
                || else_.as_deref().is_some_and(holds_subquery)
        }
        BoundExprKind::Function { args, .. } => args.iter().any(holds_subquery),
    }
}
