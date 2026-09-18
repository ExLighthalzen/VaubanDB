//! `SubqueryEval`: outer-row context and uncorrelated subquery caching for expressions
//! that hold subqueries.

use std::collections::HashSet;

use vauban_binder::{AggregateCall, BoundExpr, BoundExprKind, ColumnBinding, OutputSchema};
use vauban_catalog::ColumnId;

use crate::context::SLOT_COLUMN_ID;
use vauban_errors::{SqlError, SqlResult};
use vauban_planner::{PhysicalPlan, SubPlan};
use vauban_types::{Value, compare};

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::errors::at;
use crate::expr::{as_condition, eval_expr, from_condition};
use crate::operator::{Operator, build_operator};
use crate::row::Row;

/// Pushes the outer row and subquery state for each row of `input`.
pub struct SubqueryEval<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    subplans: Vec<SubPlan>,
    schema: OutputSchema,
    input_bindings: Vec<ColumnBinding>,
    rows_since_cancel: u64,
    outer_pushed: bool,
}

/// Builds the operator of a [`PhysicalPlan::SubqueryEval`].
///
/// # Errors
///
/// What building the input raises.
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let PhysicalPlan::SubqueryEval {
        input,
        subplans,
        schema,
    } = plan
    else {
        return Err(crate::ops::not_implemented("SubqueryEval"));
    };
    Ok(Box::new(SubqueryEval {
        input: build_operator(input)?,
        subplans: subplans.clone(),
        schema: schema.clone(),
        input_bindings: scan_column_bindings(input),
        rows_since_cancel: 0,
        outer_pushed: false,
    }))
}

impl<'a> Operator<'a> for SubqueryEval<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)?;
        self.rows_since_cancel = 0;
        self.outer_pushed = false;
        ctx.reset_subquery_cache(self.subplans.len());
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        if self.outer_pushed {
            let _ = ctx.pop_outer();
            self.outer_pushed = false;
        }
        if ctx.cancelled() {
            return Ok(None);
        }
        let Some(row) = self.input.next(ctx)? else {
            ctx.clear_subquery_state();
            return Ok(None);
        };
        self.rows_since_cancel += 1;
        if self.rows_since_cancel.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
            ctx.clear_subquery_state();
            return Ok(None);
        }
        ctx.begin_subquery_row(&self.subplans);
        ctx.push_outer(row.clone(), self.input_bindings.clone());
        self.outer_pushed = true;
        Ok(Some(row))
    }

    fn close(&mut self) {
        self.input.close();
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

/// Evaluates one [`SubPlan`] slot, reusing the cache for an uncorrelated plan.
pub(crate) fn eval_subplan_slot(
    subplan: &SubPlan,
    slot: usize,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
    line: u32,
    exists: bool,
) -> SqlResult<Value> {
    if !subplan.correlated {
        if let Some(cached) = ctx.subquery_cached(slot) {
            return Ok(cached);
        }
        let value = if exists {
            eval_exists_plan(&subplan.plan, ctx, row, None)?
        } else {
            eval_scalar_plan(&subplan.plan, row, ctx, Some(line), None)?
        };
        ctx.store_subquery_cache(slot, value.clone());
        return Ok(value);
    }
    if exists {
        eval_exists_plan(&subplan.plan, ctx, row, None)
    } else {
        eval_scalar_plan(&subplan.plan, row, ctx, Some(line), None)
    }
}

/// A scalar subquery: `NULL` on zero rows, the single column on one row, 512 beyond.
pub(crate) fn eval_scalar_plan(
    plan: &PhysicalPlan,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
    line: Option<u32>,
    source_plan: Option<&PhysicalPlan>,
) -> SqlResult<Value> {
    let locals = local_column_ids(plan);
    let bindings = scan_column_bindings(plan);
    ctx.push_subquery_locals(locals, bindings);
    let pushed = push_enclosing_outer(ctx, row, source_plan.or(Some(plan)));
    let result = eval_scalar_plan_inner(plan, ctx, line);
    if pushed {
        let _ = ctx.pop_outer();
    }
    ctx.pop_subquery_locals();
    result
}

fn eval_scalar_plan_inner(
    plan: &PhysicalPlan,
    ctx: &mut ExecContext<'_>,
    line: Option<u32>,
) -> SqlResult<Value> {
    let mut op = build_operator(plan)?;
    op.open(ctx)?;
    ctx.note_subquery_open();
    let first = op.next(ctx)?;
    ctx.note_subquery_next();
    if first.is_none() {
        op.close();
        return Ok(Value::Null);
    }
    if op.next(ctx)?.is_some() {
        ctx.note_subquery_next();
        op.close();
        return Err(at(
            SqlError::subquery_returned_more_than_one_value(),
            line.unwrap_or(0),
        ));
    }
    op.close();
    first
        .and_then(|r| r.into_iter().next())
        .ok_or_else(|| bug("subquery plan produced a row with no column"))
}

/// `EXISTS`: true on the first row, false on an empty input.
pub(crate) fn eval_exists_plan(
    plan: &PhysicalPlan,
    ctx: &mut ExecContext<'_>,
    enclosing: Option<&Row>,
    source_plan: Option<&PhysicalPlan>,
) -> SqlResult<Value> {
    let locals = local_column_ids(plan);
    let bindings = scan_column_bindings(plan);
    ctx.push_subquery_locals(locals, bindings);
    let pushed = push_enclosing_outer(ctx, enclosing, source_plan.or(Some(plan)));
    let result = eval_exists_plan_inner(plan, ctx);
    if pushed {
        let _ = ctx.pop_outer();
    }
    ctx.pop_subquery_locals();
    result
}

fn eval_exists_plan_inner(plan: &PhysicalPlan, ctx: &mut ExecContext<'_>) -> SqlResult<Value> {
    let mut op = build_operator(plan)?;
    op.open(ctx)?;
    ctx.note_subquery_open();
    let found = op.next(ctx)?.is_some();
    ctx.note_subquery_next();
    op.close();
    Ok(Value::Bit(found))
}

/// `IN (SELECT …)` with three-valued logic.
pub(crate) fn eval_in_subquery_plan(
    plan: &PhysicalPlan,
    tested: &BoundExpr,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
    line: u32,
) -> SqlResult<Value> {
    let locals = local_column_ids(plan);
    let bindings = scan_column_bindings(plan);
    ctx.push_subquery_locals(locals, bindings);
    let pushed = push_enclosing_outer(ctx, row, None);
    let result = eval_in_subquery_plan_inner(plan, tested, row, ctx, line);
    if pushed {
        let _ = ctx.pop_outer();
    }
    ctx.pop_subquery_locals();
    result
}

fn eval_in_subquery_plan_inner(
    plan: &PhysicalPlan,
    tested: &BoundExpr,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
    line: u32,
) -> SqlResult<Value> {
    let collation = tested
        .ty
        .collation
        .unwrap_or(vauban_types::Collation::DEFAULT);
    let tested = eval_expr(tested, row, ctx)?;
    let mut op = build_operator(plan)?;
    op.open(ctx)?;
    ctx.note_subquery_open();
    let mut found = false;
    let mut unknown = false;
    while let Some(inner) = op.next(ctx)? {
        ctx.note_subquery_next();
        let candidate = inner
            .first()
            .cloned()
            .ok_or_else(|| bug("subquery plan produced a row with no column"))?;
        match compare(&tested, &candidate, &collation).map_err(|e| at(e, line))? {
            Some(std::cmp::Ordering::Equal) => {
                found = true;
                break;
            }
            Some(_) => {}
            None => unknown = true,
        }
    }
    op.close();
    let held = if found {
        Some(true)
    } else if unknown {
        None
    } else {
        Some(false)
    };
    Ok(from_condition(held))
}

/// Evaluates a bound subquery node while a [`SubqueryEval`] row is active.
pub(crate) fn eval_subquery_expr(
    expr: &BoundExpr,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Value> {
    match &expr.kind {
        BoundExprKind::Exists(_) => {
            let (subplan, slot) = ctx.take_subplan()?;
            eval_subplan_slot(&subplan, slot, row, ctx, expr.line, true)
                .map_err(|e| at(e, expr.line))
        }
        BoundExprKind::ScalarSubquery(_) => {
            let (subplan, slot) = ctx.take_subplan()?;
            eval_subplan_slot(&subplan, slot, row, ctx, expr.line, false)
                .map_err(|e| at(e, expr.line))
        }
        BoundExprKind::InSubquery {
            expr: tested,
            plan: _,
            negated,
        } => {
            let (subplan, _slot) = ctx.take_subplan()?;
            let value = eval_in_subquery_plan(&subplan.plan, tested, row, ctx, expr.line)?;
            if *negated {
                Ok(from_condition(as_condition(&value)?.map(|b| !b)))
            } else {
                Ok(value)
            }
        }
        _ => Err(bug("eval_subquery_expr: not a subquery node")),
    }
}

/// Pushes `row` when it carries columns an nested correlated plan must read.
fn push_enclosing_outer(
    ctx: &mut ExecContext<'_>,
    row: Option<&Row>,
    source_plan: Option<&PhysicalPlan>,
) -> bool {
    let Some(row) = row else {
        return false;
    };
    if ctx
        .outer_rows()
        .last()
        .is_some_and(|top| top.as_slice() == row)
    {
        return false;
    }
    let bindings = source_plan.map_or_else(Vec::new, scan_column_bindings);
    if bindings.is_empty() {
        return false;
    }
    ctx.push_outer(row.clone(), bindings);
    true
}

/// Scan bindings describing how each position of a plan's row is laid out.
pub(crate) fn scan_column_bindings(plan: &PhysicalPlan) -> Vec<ColumnBinding> {
    match plan {
        PhysicalPlan::TableScan { columns, .. } | PhysicalPlan::IndexSeek { columns, .. } => {
            columns.clone()
        }
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. } => scan_column_bindings(input),
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
            schema,
            ..
        }
        | PhysicalPlan::StreamAggregate {
            input,
            group_by,
            aggregates,
            schema,
            ..
        } => aggregate_row_bindings(input, group_by, aggregates, schema),
        PhysicalPlan::Distinct(input) => scan_column_bindings(input),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => join_row_bindings(outer, inner),
        PhysicalPlan::HashJoin { build, probe, .. } => join_row_bindings(build, probe),
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => {
            inputs.first().map_or_else(Vec::new, scan_column_bindings)
        }
        PhysicalPlan::OneRow | PhysicalPlan::Values { .. } => Vec::new(),
    }
}

fn local_column_ids(plan: &PhysicalPlan) -> HashSet<ColumnId> {
    let mut ids = HashSet::new();
    collect_local_column_ids(plan, &mut ids);
    ids
}

fn collect_local_column_ids(plan: &PhysicalPlan, ids: &mut HashSet<ColumnId>) {
    match plan {
        PhysicalPlan::TableScan { columns, .. } => {
            for binding in columns {
                ids.insert(binding.column);
            }
        }
        PhysicalPlan::IndexSeek { columns, .. } => {
            for binding in columns {
                ids.insert(binding.column);
            }
        }
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            collect_local_column_ids(outer, ids);
            collect_local_column_ids(inner, ids);
        }
        PhysicalPlan::HashJoin { build, probe, .. } => {
            collect_local_column_ids(build, ids);
            collect_local_column_ids(probe, ids);
        }
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. }
        | PhysicalPlan::HashAggregate { input, .. }
        | PhysicalPlan::StreamAggregate { input, .. } => {
            collect_local_column_ids(input, ids);
            ids.insert(SLOT_COLUMN_ID);
        }
        PhysicalPlan::Distinct(input) => collect_local_column_ids(input, ids),
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => {
            for input in inputs {
                collect_local_column_ids(input, ids);
            }
        }
        PhysicalPlan::OneRow | PhysicalPlan::Values { .. } => {}
    }
}

fn join_row_bindings(left: &PhysicalPlan, right: &PhysicalPlan) -> Vec<ColumnBinding> {
    let mut bindings = scan_column_bindings(left);
    let left_width = bindings.len();
    for mut binding in scan_column_bindings(right) {
        binding.index += left_width;
        bindings.push(binding);
    }
    bindings
}

fn aggregate_row_bindings(
    input: &PhysicalPlan,
    group_by: &[BoundExpr],
    aggregates: &[AggregateCall],
    schema: &OutputSchema,
) -> Vec<ColumnBinding> {
    let _ = (group_by, aggregates);
    let mut bindings = scan_column_bindings(input);
    let group_len = schema.columns.len().saturating_sub(aggregates.len());
    for index in group_len..schema.columns.len() {
        bindings.push(ColumnBinding {
            column: SLOT_COLUMN_ID,
            index,
            name: schema.columns[index].name.clone(),
            ty: schema.columns[index].ty.clone(),
        });
    }
    bindings
}

fn bug(what: &str) -> SqlError {
    SqlError::from(vauban_errors::InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vauban_binder::{BoundExpr, BoundExprKind, OutputColumn, OutputSchema, SessionOptions};
    use vauban_planner::{PhysicalPlan, SubPlan};
    use vauban_storage::{MemoryStorage, Storage, TableShape};
    use vauban_sysfn::StaticContext;
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::{SqlType, TypeInfo, Value};

    use super::*;

    fn int_ty() -> TypeInfo {
        TypeInfo::new(SqlType::Int, true)
    }

    fn values_plan(values: &[i32]) -> PhysicalPlan {
        PhysicalPlan::Values {
            rows: values
                .iter()
                .map(|v| {
                    vec![BoundExpr {
                        kind: BoundExprKind::Literal(Value::I32(*v)),
                        ty: int_ty(),
                        line: 1,
                    }]
                })
                .collect(),
            schema: OutputSchema {
                columns: vec![OutputColumn {
                    name: "v".to_owned(),
                    ty: int_ty(),
                }],
            },
        }
    }

    #[test]
    fn uncorrelated_subquery_is_evaluated_once() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage.create_database("mydb").expect("db");
        let shape = TableShape {
            columns: vec![int_ty()],
            clustered_key: None,
        };
        let _table = storage.create_table(db, &shape).expect("table");
        let txn = TransactionManager::new(Arc::clone(&storage));
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let snap = txn.statement_snapshot(&handle);
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            storage.as_ref(),
            &txn,
            &snap,
        );

        let plan = values_plan(&[99]);
        let subplan = SubPlan {
            plan,
            correlated: false,
        };
        ctx.reset_subquery_cache(1);
        ctx.begin_subquery_row(std::slice::from_ref(&subplan));
        let first = eval_subplan_slot(&subplan, 0, None, &mut ctx, 1, false).expect("first");
        assert_eq!(ctx.subquery_cached(0), Some(Value::I32(99)));
        ctx.begin_subquery_row(std::slice::from_ref(&subplan));
        let second = eval_subplan_slot(&subplan, 0, None, &mut ctx, 1, false).expect("second");
        assert_eq!(first, Value::I32(99));
        assert_eq!(second, Value::I32(99));
    }
}
