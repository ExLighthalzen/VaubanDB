//! `SubqueryEval`: outer-row context and uncorrelated subquery caching for expressions
//! that hold subqueries.
//!
//! # The row a correlated subquery reads
//!
//! Each evaluation of a subquery plan ([`eval_scalar_plan`], [`eval_exists_plan`],
//! [`eval_in_subquery_plan`]) pushes the row it is evaluated over as the row of the
//! enclosing query ([`ExecContext::push_query_row`]) and pops it after. Inside the plan,
//! a reference with [`ColumnBinding::depth`] 1 reads that row, a reference with depth 2
//! the row pushed by the evaluation that encloses this one, even when the inner plan
//! scans the table of the enclosing query under another alias (`tests/subquery.rs`,
//! `correlated_exists_two_levels_deep_over_the_same_table`). Either reads the value at
//! the index the reference carries.

use std::rc::Rc;

use vauban_binder::{BoundExpr, BoundExprKind, OutputSchema};
use vauban_errors::{SqlError, SqlResult};
use vauban_planner::{PhysicalPlan, SubPlan};
use vauban_types::{Value, compare};

use crate::context::{CANCEL_CHECK_ROWS, ExecContext, FoundSubplan};
use crate::errors::at;
use crate::expr::{as_condition, eval_expr, from_condition};
use crate::operator::{Operator, build_operator};
use crate::row::Row;

/// Opens the plans of the subqueries its expressions hold for the rows of `input`.
pub struct SubqueryEval<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    /// Copied out of the plan once, when the operator is built, and shared with the frame
    /// each `open` pushes.
    subplans: Rc<[SubPlan]>,
    schema: OutputSchema,
    rows_since_cancel: u64,
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
        subplans: Rc::from(subplans.as_slice()),
        schema: schema.clone(),
        rows_since_cancel: 0,
    }))
}

impl<'a> Operator<'a> for SubqueryEval<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)?;
        self.rows_since_cancel = 0;
        ctx.open_subquery_eval(Rc::clone(&self.subplans));
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
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
        Ok(Some(row))
    }

    fn close(&mut self) {
        self.input.close();
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

/// Evaluates the plan `found` designates, reusing the cache for an uncorrelated plan.
pub(crate) fn eval_found_subplan(
    found: &FoundSubplan,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
    line: u32,
    exists: bool,
) -> SqlResult<Value> {
    let subplan = found.subplan();
    if !subplan.correlated {
        if let Some(cached) = ctx.subquery_cached(found) {
            return Ok(cached);
        }
        let value = if exists {
            eval_exists_plan(&subplan.plan, ctx, row)?
        } else {
            eval_scalar_plan(&subplan.plan, row, ctx, Some(line))?
        };
        ctx.store_subquery_cache(found, value.clone());
        return Ok(value);
    }
    if exists {
        eval_exists_plan(&subplan.plan, ctx, row)
    } else {
        eval_scalar_plan(&subplan.plan, row, ctx, Some(line))
    }
}

/// A scalar subquery: `NULL` on zero rows, the single column on one row, 512 beyond.
/// `row` is the row of the enclosing query, read by the references at depth 1.
pub(crate) fn eval_scalar_plan(
    plan: &PhysicalPlan,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
    line: Option<u32>,
) -> SqlResult<Value> {
    with_enclosing_row(row, ctx, |ctx| eval_scalar_plan_inner(plan, ctx, line))
}

/// Runs `f` with `row` pushed as the row of the query that encloses the subquery. The
/// correlation stacks are brought back to where they stood before, which also drops what
/// a `SubqueryEval` or a join of the subquery left when `f` closed it before its input
/// was exhausted ([`ExecContext::unwind_to`]).
fn with_enclosing_row(
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
    f: impl FnOnce(&mut ExecContext<'_>) -> SqlResult<Value>,
) -> SqlResult<Value> {
    let mark = ctx.correlation_mark();
    ctx.push_query_row(row);
    let result = f(ctx);
    ctx.unwind_to(mark);
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

/// `EXISTS`: true on the first row, false on an empty input. `enclosing` is the row of
/// the enclosing query, read by the references at depth 1.
pub(crate) fn eval_exists_plan(
    plan: &PhysicalPlan,
    ctx: &mut ExecContext<'_>,
    enclosing: Option<&Row>,
) -> SqlResult<Value> {
    with_enclosing_row(enclosing, ctx, |ctx| eval_exists_plan_inner(plan, ctx))
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
    let collation = tested
        .ty
        .collation
        .unwrap_or(vauban_types::Collation::DEFAULT);
    let tested = eval_expr(tested, row, ctx)?;
    with_enclosing_row(row, ctx, |ctx| {
        eval_in_subquery_plan_inner(plan, tested, ctx, line, collation)
    })
}

fn eval_in_subquery_plan_inner(
    plan: &PhysicalPlan,
    tested: Value,
    ctx: &mut ExecContext<'_>,
    line: u32,
    collation: vauban_types::Collation,
) -> SqlResult<Value> {
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

/// Evaluates a bound subquery node while a [`SubqueryEval`] row is active, with the plan
/// the identity of the node designates.
pub(crate) fn eval_subquery_expr(
    expr: &BoundExpr,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Value> {
    match &expr.kind {
        BoundExprKind::Exists(subquery) => {
            let found = ctx.find_subplan(subquery.id)?;
            eval_found_subplan(&found, row, ctx, expr.line, true).map_err(|e| at(e, expr.line))
        }
        BoundExprKind::ScalarSubquery(subquery) => {
            let found = ctx.find_subplan(subquery.id)?;
            eval_found_subplan(&found, row, ctx, expr.line, false).map_err(|e| at(e, expr.line))
        }
        BoundExprKind::InSubquery {
            expr: tested,
            plan: subquery,
            negated,
        } => {
            let found = ctx.find_subplan(subquery.id)?;
            let value = eval_in_subquery_plan(&found.subplan().plan, tested, row, ctx, expr.line)?;
            if *negated {
                Ok(from_condition(as_condition(&value)?.map(|b| !b)))
            } else {
                Ok(value)
            }
        }
        _ => Err(bug("eval_subquery_expr: not a subquery node")),
    }
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
        let id = vauban_binder::SubqueryId::fresh();
        let subplan = SubPlan {
            id,
            plan,
            correlated: false,
        };
        ctx.open_subquery_eval(Rc::from(vec![subplan]));
        let found = ctx.find_subplan(id).expect("found");
        let first = eval_found_subplan(&found, None, &mut ctx, 1, false).expect("first");
        assert_eq!(ctx.subquery_cached(&found), Some(Value::I32(99)));
        let found = ctx.find_subplan(id).expect("found again");
        let second = eval_found_subplan(&found, None, &mut ctx, 1, false).expect("second");
        assert_eq!(first, Value::I32(99));
        assert_eq!(second, Value::I32(99));
    }
}
