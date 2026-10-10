//! Subqueries: the decorrelated semi-join of an `IN` or an `EXISTS`, and
//! [`SubqueryEval`](PhysicalPlan::SubqueryEval) for the ones that stay correlated.
//!
//! # Correlation
//!
//! A subquery plan is **correlated** when an expression inside it holds a
//! [`ColumnRef`](vauban_binder::BoundExprKind::ColumnRef) to a column of a query that
//! encloses it. The binder writes on each reference the number of queries between the
//! reader and the column ([`ColumnBinding::depth`](vauban_binder::ColumnBinding::depth)):
//! the planner treats a reference whose depth reaches past the subquery as outer,
//! including one written in a
//! [`Aggregate`](vauban_binder::LogicalPlan::Aggregate) `group_by` or aggregate argument, a
//! [`Sort`](vauban_binder::LogicalPlan::Sort) key, a [`Limit`](vauban_binder::LogicalPlan::Limit)
//! `top` expression, or a [`Values`](vauban_binder::LogicalPlan::Values) row
//! (`tests/subquery.rs`, `correlated_exists_is_evaluated_per_row`,
//! `exists_grouped_on_an_outer_column_stays_correlated`,
//! `exists_max_on_an_outer_column_stays_correlated`,
//! `exists_top_on_an_outer_column_stays_correlated`,
//! `exists_ordered_on_an_outer_column_stays_correlated` and
//! `exists_values_over_an_outer_column_stays_correlated`).
//!
//! # Identity of [`SubPlan`](crate::SubPlan) entries
//!
//! Each [`SubPlan`](crate::SubPlan) carries the [`SubqueryId`](vauban_binder::SubqueryId)
//! of the node it was planned from, and the executor looks a plan up by the identity of
//! the node it evaluates. The position of an entry in the list is not read: a subquery a
//! short-circuit leaves unevaluated does not move the ones after it. The list keeps a
//! depth-first, left-to-right walk of the expression tree, one entry per identity
//! (`tests/subquery.rs`, `subplans_follow_the_expression_order`).

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundSubquery, ColumnBinding, CompareOp, LogicalOp, LogicalPlan,
    OutputColumn, OutputSchema,
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

/// Plans the subqueries of an expression evaluated outside any plan: the value of a
/// `SET`, the initial value of a `DECLARE`.
///
/// Such an expression is read over no row, so nothing is decorrelated here and no
/// [`SubqueryEval`](PhysicalPlan::SubqueryEval) is built: the statement carries the list
/// itself, in the depth-first order the module header describes for that node. An
/// expression that holds no subquery answers an empty list (`tests/subquery.rs`,
/// `a_set_without_a_subquery_carries_no_subplan`).
pub(crate) fn plan_statement_subplans(
    expr: &BoundExpr,
    ctx: &PlanContext<'_>,
) -> SqlResult<Vec<SubPlan>> {
    if !holds_subquery(expr) {
        return Ok(Vec::new());
    }
    collect_subplans(expr, ctx)
}

/// Tries to turn an uncorrelated `EXISTS`, `NOT EXISTS` or `IN (SELECT …)` predicate, or
/// one such conjunct of an `AND`, into a [`NestedLoopJoin`](PhysicalPlan::NestedLoopJoin).
///
/// When the predicate is `EXISTS (…) AND c = 1`, the semi-join replaces the `EXISTS`
/// conjunct and a [`Filter`](PhysicalPlan::Filter) keeps the residual `c = 1`
/// (`tests/subquery.rs`, `exists_and_a_residual_predicate_keeps_the_filter`). A residual
/// that holds a subquery reads it from a [`SubqueryEval`](PhysicalPlan::SubqueryEval) laid
/// over the semi-join (`tests/subquery.rs`,
/// `a_residual_not_in_is_evaluated_over_the_semi_join`).
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
    let input = plan_expr_subqueries(&predicate, join, ctx)?;
    Ok(Some(PhysicalPlan::Filter {
        input: Box::new(input),
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
            if is_correlated(&inner.plan) {
                return Ok(None);
            }
            let inner = plan_node(&inner.plan, ctx)?;
            Ok(Some(semi_join(outer, inner, PhysicalJoinKind::Semi, None)))
        }
        BoundExprKind::Not(inner) => {
            let BoundExprKind::Exists(plan) = &inner.kind else {
                return Ok(None);
            };
            if is_correlated(&plan.plan) {
                return Ok(None);
            }
            let inner = plan_node(&plan.plan, ctx)?;
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
            if is_correlated(&plan.plan) {
                return Ok(None);
            }
            let inner = plan_node(&plan.plan, ctx)?;
            let on = in_subquery_equality(expr, &plan.plan, outer)?;
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

/// The equality `on` for `e IN (SELECT x …)`: `e`, read on the outer side of the
/// concatenated row, against the single column the inner plan produces, which sits right
/// after the outer columns. The column is typed and named by the output schema of the
/// subquery, whatever its root (a projection, a `UNION`, an `INTERSECT`): `x` itself is
/// computed by the inner plan and is not read again here (`tests/subquery.rs`,
/// `in_subquery_compares_the_column_the_inner_plan_produces`,
/// `in_subquery_over_a_union_compares_its_output_column`).
fn in_subquery_equality(
    tested: &BoundExpr,
    plan: &LogicalPlan,
    outer: &PhysicalPlan,
) -> SqlResult<BoundExpr> {
    let outer_width = outer.schema().columns.len();
    let [produced] = plan.schema().columns.as_slice() else {
        return Err(crate::plan::not_implemented(
            "subquery::in_subquery_equality",
            "a subquery whose select list is not one column wide",
        ));
    };
    let inner_column = BoundExpr {
        kind: BoundExprKind::ColumnRef(ColumnBinding {
            column: ColumnId(1),
            index: outer_width,
            name: produced.name.clone(),
            ty: produced.ty.clone(),
            depth: 0,
        }),
        ty: produced.ty.clone(),
        line: tested.line,
    };
    let ty = tested.ty.clone();
    Ok(BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(tested.clone()),
            right: Box::new(inner_column),
        },
        ty,
        line: tested.line,
    })
}

/// True when `plan` reads a column of a query that encloses it: a `ColumnRef` whose
/// `depth` reaches past `plan`, the subqueries nested inside `plan` counted
/// (`tests/subquery.rs`, `exists_on_a_second_table_with_the_same_column_id_is_correlated`,
/// `a_nested_reference_to_the_enclosing_query_is_not_correlated`).
fn is_correlated(plan: &LogicalPlan) -> bool {
    plan_holds_external_column(plan, 0)
}

/// `level` is the number of subqueries descended into below the plan under test: a
/// reference of `depth` 1 met one level down names a column of that plan, not of what
/// encloses it. A derived table is not a level: it cannot name a column outside itself.
fn plan_holds_external_column(plan: &LogicalPlan, level: u8) -> bool {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            plan_holds_external_column(input, level) || expr_holds_external_column(predicate, level)
        }
        LogicalPlan::Project { input, exprs, .. } => {
            plan_holds_external_column(input, level)
                || exprs
                    .iter()
                    .any(|proj| expr_holds_external_column(&proj.expr, level))
        }
        LogicalPlan::Limit { input, top } => {
            plan_holds_external_column(input, level) || expr_holds_external_column(&top.expr, level)
        }
        LogicalPlan::Subquery { input, .. } => plan_holds_external_column(input, level),
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            ..
        } => {
            plan_holds_external_column(input, level)
                || group_by
                    .iter()
                    .any(|key| expr_holds_external_column(key, level))
                || aggregates.iter().any(|call| {
                    call.arg
                        .as_ref()
                        .is_some_and(|arg| expr_holds_external_column(arg, level))
                })
        }
        LogicalPlan::Sort { input, keys, .. } => {
            plan_holds_external_column(input, level)
                || keys
                    .iter()
                    .any(|key| expr_holds_external_column(&key.expr, level))
        }
        LogicalPlan::Values { rows, .. } => rows
            .iter()
            .flatten()
            .any(|expr| expr_holds_external_column(expr, level)),
        LogicalPlan::Distinct(input) => plan_holds_external_column(input, level),
        LogicalPlan::Join {
            left, right, on, ..
        } => {
            plan_holds_external_column(left, level)
                || plan_holds_external_column(right, level)
                || on
                    .as_ref()
                    .is_some_and(|pred| expr_holds_external_column(pred, level))
        }
        LogicalPlan::Scan { .. } | LogicalPlan::OneRow => false,
        LogicalPlan::SetOp { left, right, .. } => {
            plan_holds_external_column(left, level) || plan_holds_external_column(right, level)
        }
    }
}

fn expr_holds_external_column(expr: &BoundExpr, level: u8) -> bool {
    match &expr.kind {
        BoundExprKind::ColumnRef(binding) => binding.depth > level,
        BoundExprKind::Exists(plan) | BoundExprKind::ScalarSubquery(plan) => {
            plan_holds_external_column(&plan.plan, level.saturating_add(1))
        }
        BoundExprKind::InSubquery { expr, plan, .. } => {
            expr_holds_external_column(expr, level)
                || plan_holds_external_column(&plan.plan, level.saturating_add(1))
        }
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => expr_holds_external_column(inner, level),
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => {
            expr_holds_external_column(left, level) || expr_holds_external_column(right, level)
        }
        BoundExprKind::In {
            expr: tested, list, ..
        } => {
            expr_holds_external_column(tested, level)
                || list
                    .iter()
                    .any(|item| expr_holds_external_column(item, level))
        }
        BoundExprKind::Like {
            expr: tested,
            pattern,
            escape,
            ..
        } => {
            expr_holds_external_column(tested, level)
                || expr_holds_external_column(pattern, level)
                || escape
                    .as_deref()
                    .is_some_and(|item| expr_holds_external_column(item, level))
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            operand
                .as_deref()
                .is_some_and(|item| expr_holds_external_column(item, level))
                || arms.iter().any(|arm| {
                    expr_holds_external_column(&arm.when, level)
                        || expr_holds_external_column(&arm.then, level)
                })
                || else_
                    .as_deref()
                    .is_some_and(|item| expr_holds_external_column(item, level))
        }
        BoundExprKind::Function { args, .. } => args
            .iter()
            .any(|arg| expr_holds_external_column(arg, level)),
        BoundExprKind::Literal(_) | BoundExprKind::Variable { .. } => false,
    }
}

/// The subqueries of `expr`, one per identity, in depth-first left-to-right order
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
        BoundExprKind::Exists(subquery) | BoundExprKind::ScalarSubquery(subquery) => {
            push_subplan(subquery, ctx, out)?;
        }
        BoundExprKind::InSubquery {
            expr,
            plan: subquery,
            ..
        } => {
            collect_subplans_from_expr(expr, ctx, out)?;
            push_subplan(subquery, ctx, out)?;
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

/// Plans `subquery` into `out` under its identity, once: a node the binder copied into
/// two places of the expression carries the same identity at both, and one plan serves
/// both (`tests/subquery.rs`, `a_subquery_copied_into_two_places_is_planned_once`).
fn push_subplan(
    subquery: &BoundSubquery,
    ctx: &PlanContext<'_>,
    out: &mut Vec<SubPlan>,
) -> SqlResult<()> {
    if out.iter().any(|known| known.id == subquery.id) {
        return Ok(());
    }
    out.push(SubPlan {
        id: subquery.id,
        plan: plan_node(&subquery.plan, ctx)?,
        correlated: is_correlated(&subquery.plan),
    });
    Ok(())
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
