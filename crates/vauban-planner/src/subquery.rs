//! Subqueries: the decorrelated semi-join of an `IN` or an `EXISTS`, and
//! [`SubqueryEval`](PhysicalPlan::SubqueryEval) for the ones that stay correlated.
//!
//! # Correlation
//!
//! A subquery plan is **correlated** when an expression inside it holds a
//! [`ColumnRef`](vauban_binder::BoundExprKind::ColumnRef) to a column of a table the
//! subquery does not read. The binder keeps the [`ColumnId`](vauban_catalog::ColumnId)
//! of the column referenced; the planner collects the ids of each column the inner
//! [`Scan`](vauban_binder::LogicalPlan::Scan) and [`Join`](vauban_binder::LogicalPlan::Join)
//! exposes and treats a reference outside that set as outer, including one written in a
//! [`Aggregate`](vauban_binder::LogicalPlan::Aggregate) `group_by`, a [`Sort`](vauban_binder::LogicalPlan::Sort) key or a
//! [`Values`](vauban_binder::LogicalPlan::Values) row
//! (`tests/subquery.rs`, `correlated_exists_is_evaluated_per_row`,
//! `exists_grouped_on_an_outer_column_stays_correlated`,
//! `exists_ordered_on_an_outer_column_stays_correlated` and
//! `exists_values_over_an_outer_column_stays_correlated`).
//!
//! # Index of [`SubPlan`](crate::SubPlan) entries
//!
//! [`SubqueryEval`](PhysicalPlan::SubqueryEval) lists its [`SubPlan`](crate::SubPlan)
//! entries in the order of a depth-first, left-to-right walk of the expression tree.
//! The executor counts subqueries in that same order; changing one side without the
//! other silently pairs the wrong plan to the wrong node
//! (`tests/subquery.rs`, `subplans_follow_the_expression_order`).

use std::collections::HashSet;

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, CompareOp, LogicalOp, LogicalPlan, OutputColumn,
    OutputSchema,
};
use vauban_catalog::ColumnId;
use vauban_errors::SqlResult;
use vauban_types::{SqlType, TypeInfo};

use crate::context::PlanContext;
use crate::physical::{PhysicalJoinKind, PhysicalPlan, SubPlan};
use crate::plan::plan_node;
use crate::seek;

/// Plans a [`LogicalPlan::Subquery`], the derived table `FROM (SELECT …) AS d`.
pub(crate) fn plan_subquery(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let LogicalPlan::Subquery { input, .. } = plan else {
        unreachable!("plan_subquery called on a non-Subquery node")
    };
    plan_node(input, ctx)
}

/// Plans a [`LogicalPlan::Filter`]: index seek, decorrelated semi-join, residual filter,
/// or [`SubqueryEval`](PhysicalPlan::SubqueryEval) when a predicate holds a subquery.
pub(crate) fn plan_filter(
    input: &LogicalPlan,
    predicate: &BoundExpr,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalPlan> {
    let input = plan_node(input, ctx)?;
    if let Some(seek) = seek::try_index_seek(predicate, &input, ctx)? {
        return Ok(seek);
    }
    if let Some(plan) = try_decorrelate_predicate(predicate, &input, ctx)? {
        return Ok(plan);
    }
    let planned = plan_expr_subqueries(predicate, input, ctx)?;
    if matches!(planned, PhysicalPlan::SubqueryEval { .. }) {
        return Ok(planned);
    }
    Ok(PhysicalPlan::Filter {
        input: Box::new(planned),
        predicate: predicate.clone(),
    })
}

/// Plans the subqueries `expr` holds, over the rows `input` produces.
///
/// `plan.rs` calls this at two sites, and at those two only: the predicate of a
/// [`Filter`](LogicalPlan::Filter) goes through [`plan_filter`] first; each expression of a
/// [`Project`](LogicalPlan::Project) calls this entry point directly. On the expression
/// handed here, the rule is to answer the [`SubqueryEval`](PhysicalPlan::SubqueryEval) that
/// computes its subqueries, or — for a [`Filter`](LogicalPlan::Filter) predicate handled by
/// [`plan_filter`] — a semi-join substituted for the whole predicate.
///
/// An expression that holds no subquery is answered by `input` unchanged. The three kinds
/// that hold a plan are looked for at each depth of that expression:
/// [`Exists`](BoundExprKind::Exists),
/// [`ScalarSubquery`](BoundExprKind::ScalarSubquery) and
/// [`InSubquery`](BoundExprKind::InSubquery).
pub(crate) fn plan_expr_subqueries(
    expr: &BoundExpr,
    input: PhysicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalPlan> {
    if !holds_subquery(expr) {
        return Ok(input);
    }
    let subplans = collect_subplans(expr, ctx)?;
    let schema = subquery_eval_schema(&input, &subplans);
    Ok(PhysicalPlan::SubqueryEval {
        input: Box::new(input),
        subplans,
        schema,
    })
}

/// Tries to turn an uncorrelated `EXISTS`, `NOT EXISTS` or `IN (SELECT …)` predicate, or
/// one such conjunct of an `AND`, into a [`NestedLoopJoin`](PhysicalPlan::NestedLoopJoin).
///
/// When the predicate is `EXISTS (…) AND c = 1`, the semi-join replaces the `EXISTS`
/// conjunct and a [`Filter`](PhysicalPlan::Filter) keeps the residual `c = 1`
/// (`tests/subquery.rs`, `exists_and_a_residual_predicate_keeps_the_filter`).
fn try_decorrelate_predicate(
    predicate: &BoundExpr,
    outer: &PhysicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<Option<PhysicalPlan>> {
    let mut conjuncts = Vec::new();
    split_conjunction(predicate, &mut conjuncts);
    if conjuncts.len() == 1 {
        return try_decorrelate_conjunct(conjuncts[0], outer, ctx);
    }
    let mut decorrelated = None;
    for (index, conjunct) in conjuncts.iter().enumerate() {
        if is_decorrelatable_conjunct(conjunct) {
            if decorrelated.is_some() {
                return Ok(None);
            }
            decorrelated = Some(index);
        }
    }
    let Some(index) = decorrelated else {
        return Ok(None);
    };
    let Some(join) = try_decorrelate_conjunct(conjuncts[index], outer, ctx)? else {
        return Ok(None);
    };
    let residual: Vec<BoundExpr> = conjuncts
        .iter()
        .enumerate()
        .filter(|(position, _)| *position != index)
        .map(|(_, conjunct)| (*conjunct).clone())
        .collect();
    let Some(predicate) = residual
        .into_iter()
        .reduce(|left, right| and(left, right, predicate))
    else {
        return Ok(Some(join));
    };
    Ok(Some(PhysicalPlan::Filter {
        input: Box::new(join),
        predicate,
    }))
}

/// True when `conjunct` is a whole `EXISTS`, `NOT EXISTS` or non-negated `IN (SELECT …)`.
fn is_decorrelatable_conjunct(conjunct: &BoundExpr) -> bool {
    matches!(
        conjunct.kind,
        BoundExprKind::Exists(_) | BoundExprKind::InSubquery { negated: false, .. }
    ) || matches!(
        &conjunct.kind,
        BoundExprKind::Not(inner) if matches!(inner.kind, BoundExprKind::Exists(_))
    )
}

/// Tries to turn one decorrelatable conjunct into a semi- or anti-semi-join.
fn try_decorrelate_conjunct(
    conjunct: &BoundExpr,
    outer: &PhysicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<Option<PhysicalPlan>> {
    match &conjunct.kind {
        BoundExprKind::Exists(inner) => {
            if is_correlated(inner) {
                return Ok(None);
            }
            let inner = plan_node(inner, ctx)?;
            Ok(Some(semi_join(outer, inner, PhysicalJoinKind::Semi, None)))
        }
        BoundExprKind::Not(inner) => {
            let BoundExprKind::Exists(plan) = &inner.kind else {
                return Ok(None);
            };
            if is_correlated(plan) {
                return Ok(None);
            }
            let inner = plan_node(plan, ctx)?;
            Ok(Some(semi_join(
                outer,
                inner,
                PhysicalJoinKind::AntiSemi,
                None,
            )))
        }
        BoundExprKind::InSubquery {
            expr,
            plan,
            negated: false,
        } => {
            if is_correlated(plan) {
                return Ok(None);
            }
            let inner = plan_node(plan, ctx)?;
            let on = in_subquery_equality(expr, plan)?;
            Ok(Some(semi_join(
                outer,
                inner,
                PhysicalJoinKind::Semi,
                Some(on),
            )))
        }
        BoundExprKind::InSubquery { negated: true, .. } => Ok(None),
        _ => Ok(None),
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

/// Builds a decorrelated semi- or anti-semi-join over `outer`.
fn semi_join(
    outer: &PhysicalPlan,
    inner: PhysicalPlan,
    kind: PhysicalJoinKind,
    on: Option<BoundExpr>,
) -> PhysicalPlan {
    PhysicalPlan::NestedLoopJoin {
        schema: outer.schema().clone(),
        outer: Box::new(outer.clone()),
        inner: Box::new(inner),
        kind,
        on,
    }
}

/// The equality `on` for `e IN (SELECT x …)` when the subquery select list is a bare
/// column reference.
fn in_subquery_equality(tested: &BoundExpr, plan: &LogicalPlan) -> SqlResult<BoundExpr> {
    let inner_expr = subquery_select_expr(plan)?;
    let ty = tested.ty.clone();
    Ok(BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(tested.clone()),
            right: Box::new(inner_expr),
        },
        ty,
        line: tested.line,
    })
}

/// The single expression of a one-column subquery select list.
fn subquery_select_expr(plan: &LogicalPlan) -> SqlResult<BoundExpr> {
    let Some(LogicalPlan::Project { exprs, .. }) = project_node(plan) else {
        return Err(crate::plan::not_implemented(
            "subquery::subquery_select_expr",
            "a subquery whose root is not a Project",
        ));
    };
    let [BoundProjection { expr, .. }] = exprs.as_slice() else {
        return Err(crate::plan::not_implemented(
            "subquery::subquery_select_expr",
            "a subquery whose select list is not one column wide",
        ));
    };
    Ok(expr.clone())
}

fn project_node(plan: &LogicalPlan) -> Option<&LogicalPlan> {
    match plan {
        LogicalPlan::Project { .. } => Some(plan),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Subquery { input, .. } => project_node(input),
        LogicalPlan::Distinct(input) => project_node(input),
        LogicalPlan::OneRow
        | LogicalPlan::Scan { .. }
        | LogicalPlan::Values { .. }
        | LogicalPlan::Join { .. }
        | LogicalPlan::SetOp { .. } => None,
    }
}

/// True when `plan` reads a column its own scans do not expose.
fn is_correlated(plan: &LogicalPlan) -> bool {
    let local = local_column_ids(plan);
    plan_holds_external_column(plan, &local)
}

/// Each [`ColumnId`](vauban_catalog::ColumnId) a scan or join inside `plan` exposes.
fn local_column_ids(plan: &LogicalPlan) -> HashSet<ColumnId> {
    let mut ids = HashSet::new();
    collect_local_column_ids(plan, &mut ids);
    ids
}

fn collect_local_column_ids(plan: &LogicalPlan, ids: &mut HashSet<ColumnId>) {
    match plan {
        LogicalPlan::Scan { columns, .. } => {
            for binding in columns {
                ids.insert(binding.column);
            }
        }
        LogicalPlan::Join { left, right, .. } => {
            collect_local_column_ids(left, ids);
            collect_local_column_ids(right, ids);
        }
        LogicalPlan::Filter { input, predicate } => {
            collect_local_column_ids(input, ids);
            let _ = predicate;
        }
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Subquery { input, .. } => collect_local_column_ids(input, ids),
        LogicalPlan::Distinct(input) => collect_local_column_ids(input, ids),
        LogicalPlan::OneRow | LogicalPlan::Values { .. } | LogicalPlan::SetOp { .. } => {}
    }
}

fn plan_holds_external_column(plan: &LogicalPlan, local: &HashSet<ColumnId>) -> bool {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            plan_holds_external_column(input, local) || expr_holds_external_column(predicate, local)
        }
        LogicalPlan::Project { input, exprs, .. } => {
            plan_holds_external_column(input, local)
                || exprs
                    .iter()
                    .any(|proj| expr_holds_external_column(&proj.expr, local))
        }
        LogicalPlan::Limit { input, .. } | LogicalPlan::Subquery { input, .. } => {
            plan_holds_external_column(input, local)
        }
        LogicalPlan::Aggregate {
            input, group_by, ..
        } => {
            plan_holds_external_column(input, local)
                || group_by
                    .iter()
                    .any(|key| expr_holds_external_column(key, local))
        }
        LogicalPlan::Sort { input, keys, .. } => {
            plan_holds_external_column(input, local)
                || keys
                    .iter()
                    .any(|key| expr_holds_external_column(&key.expr, local))
        }
        LogicalPlan::Values { rows, .. } => rows
            .iter()
            .flatten()
            .any(|expr| expr_holds_external_column(expr, local)),
        LogicalPlan::Distinct(input) => plan_holds_external_column(input, local),
        LogicalPlan::Join {
            left, right, on, ..
        } => {
            plan_holds_external_column(left, local)
                || plan_holds_external_column(right, local)
                || on
                    .as_ref()
                    .is_some_and(|pred| expr_holds_external_column(pred, local))
        }
        LogicalPlan::Scan { .. } | LogicalPlan::OneRow => false,
        LogicalPlan::SetOp { .. } => false,
    }
}

fn expr_holds_external_column(expr: &BoundExpr, local: &HashSet<ColumnId>) -> bool {
    match &expr.kind {
        BoundExprKind::ColumnRef(binding) => !local.contains(&binding.column),
        BoundExprKind::Exists(plan) | BoundExprKind::ScalarSubquery(plan) => {
            plan_holds_external_column(plan, local)
        }
        BoundExprKind::InSubquery { expr, plan, .. } => {
            expr_holds_external_column(expr, local) || plan_holds_external_column(plan, local)
        }
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => expr_holds_external_column(inner, local),
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => {
            expr_holds_external_column(left, local) || expr_holds_external_column(right, local)
        }
        BoundExprKind::In {
            expr: tested, list, ..
        } => {
            expr_holds_external_column(tested, local)
                || list
                    .iter()
                    .any(|item| expr_holds_external_column(item, local))
        }
        BoundExprKind::Like {
            expr: tested,
            pattern,
            escape,
            ..
        } => {
            expr_holds_external_column(tested, local)
                || expr_holds_external_column(pattern, local)
                || escape
                    .as_deref()
                    .is_some_and(|item| expr_holds_external_column(item, local))
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            operand
                .as_deref()
                .is_some_and(|item| expr_holds_external_column(item, local))
                || arms.iter().any(|arm| {
                    expr_holds_external_column(&arm.when, local)
                        || expr_holds_external_column(&arm.then, local)
                })
                || else_
                    .as_deref()
                    .is_some_and(|item| expr_holds_external_column(item, local))
        }
        BoundExprKind::Function { args, .. } => args
            .iter()
            .any(|arg| expr_holds_external_column(arg, local)),
        BoundExprKind::Literal(_) | BoundExprKind::Variable { .. } => false,
    }
}

/// The subqueries of `expr`, in depth-first left-to-right order
/// (`tests/subquery.rs`, `subplans_follow_the_expression_order`).
fn collect_subplans(expr: &BoundExpr, ctx: &PlanContext<'_>) -> SqlResult<Vec<SubPlan>> {
    let mut subplans = Vec::new();
    collect_subplans_from_expr(expr, ctx, &mut subplans)?;
    Ok(subplans)
}

fn collect_subplans_from_expr(
    expr: &BoundExpr,
    ctx: &PlanContext<'_>,
    out: &mut Vec<SubPlan>,
) -> SqlResult<()> {
    match &expr.kind {
        BoundExprKind::Exists(plan) => {
            out.push(subplan(plan, ctx)?);
        }
        BoundExprKind::ScalarSubquery(plan) => {
            out.push(subplan(plan, ctx)?);
        }
        BoundExprKind::InSubquery { expr, plan, .. } => {
            collect_subplans_from_expr(expr, ctx, out)?;
            out.push(subplan(plan, ctx)?);
        }
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => collect_subplans_from_expr(inner, ctx, out)?,
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => {
            collect_subplans_from_expr(left, ctx, out)?;
            collect_subplans_from_expr(right, ctx, out)?;
        }
        BoundExprKind::In {
            expr: tested, list, ..
        } => {
            collect_subplans_from_expr(tested, ctx, out)?;
            for item in list {
                collect_subplans_from_expr(item, ctx, out)?;
            }
        }
        BoundExprKind::Like {
            expr: tested,
            pattern,
            escape,
            ..
        } => {
            collect_subplans_from_expr(tested, ctx, out)?;
            collect_subplans_from_expr(pattern, ctx, out)?;
            if let Some(item) = escape {
                collect_subplans_from_expr(item, ctx, out)?;
            }
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            if let Some(item) = operand {
                collect_subplans_from_expr(item, ctx, out)?;
            }
            for arm in arms {
                collect_subplans_from_expr(&arm.when, ctx, out)?;
                collect_subplans_from_expr(&arm.then, ctx, out)?;
            }
            if let Some(item) = else_ {
                collect_subplans_from_expr(item, ctx, out)?;
            }
        }
        BoundExprKind::Function { args, .. } => {
            for arg in args {
                collect_subplans_from_expr(arg, ctx, out)?;
            }
        }
        BoundExprKind::Literal(_)
        | BoundExprKind::ColumnRef(_)
        | BoundExprKind::Variable { .. } => {}
    }
    Ok(())
}

fn subplan(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<SubPlan> {
    Ok(SubPlan {
        plan: plan_node(plan, ctx)?,
        correlated: is_correlated(plan),
    })
}

fn subquery_eval_schema(input: &PhysicalPlan, subplans: &[SubPlan]) -> OutputSchema {
    let mut columns = input.schema().columns.clone();
    for subplan in subplans {
        let ty = subplan
            .plan
            .schema()
            .columns
            .first()
            .map(|column| column.ty.clone())
            .unwrap_or_else(|| TypeInfo::new(SqlType::Bit, false));
        columns.push(OutputColumn {
            name: String::new(),
            ty,
        });
    }
    OutputSchema { columns }
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
