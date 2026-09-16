//! The translation itself: one physical statement per bound statement, one operator per
//! logical node, and the call to the rule that owns each of the others.
//!
//! The rules live in their own files; this one holds the dispatch.
//!
//! # What this file decides, and what it does not
//!
//! It decides nothing. A node whose translation is one-to-one — `OneRow`, `Values`, `Scan`,
//! `Filter`, `Project`, `Limit` — becomes its operator; the six remaining nodes of
//! [`LogicalPlan`] are handed to the file that owns each of them (`tests/trivial.rs`,
//! `join_is_not_implemented_yet`). The two rewriting hooks, [`seek::try_index_seek`] and
//! [`sort::try_top_n`], answer `Ok(None)` when their rule does not apply, and this file
//! then builds the operator it would have built anyway.

use vauban_binder::{BoundStatement, LogicalPlan};
use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::context::PlanContext;
use crate::physical::{PhysicalPlan, PhysicalStatement};
use crate::{aggregate, dml, seek, setop, sort, subquery};

/// Turns a bound statement into the physical statement the `executor` runs.
///
/// # Errors
///
/// The internal error 50000 of a form not implemented yet (`tests/trivial.rs`,
/// `join_is_not_implemented_yet`), and what a planning rule raises once it is written.
pub fn plan(stmt: BoundStatement, ctx: &PlanContext<'_>) -> SqlResult<PhysicalStatement> {
    match stmt {
        BoundStatement::Query(plan) => Ok(PhysicalStatement::Query(plan_node(&plan, ctx)?)),
        BoundStatement::Ddl(ddl) => Ok(PhysicalStatement::Ddl(ddl)),
        BoundStatement::Use { database } => Ok(PhysicalStatement::Use { database }),
        BoundStatement::Insert(insert) => dml::plan_insert(&insert, ctx),
        BoundStatement::Update(update) => dml::plan_update(&update, ctx),
        BoundStatement::Delete(delete) => dml::plan_delete(&delete, ctx),
        BoundStatement::SetVariable { name, value } => {
            Ok(PhysicalStatement::SetVariable { name, value })
        }
        BoundStatement::SelectAssign { .. } => Err(not_implemented(
            "plan",
            "a SELECT that assigns variables from a FROM",
        )),
        BoundStatement::Declare(declarations) => Ok(PhysicalStatement::Declare(declarations)),
        BoundStatement::If {
            condition,
            then_,
            else_,
        } => {
            let then_ = Box::new(plan(*then_, ctx)?);
            let else_ = match else_ {
                Some(stmt) => Some(Box::new(plan(*stmt, ctx)?)),
                None => None,
            };
            Ok(PhysicalStatement::If {
                condition,
                then_,
                else_,
            })
        }
        BoundStatement::While { condition, body } => Ok(PhysicalStatement::While {
            condition,
            body: Box::new(plan(*body, ctx)?),
        }),
        BoundStatement::Block(statements) => {
            let mut planned = Vec::with_capacity(statements.len());
            for stmt in statements {
                planned.push(plan(stmt, ctx)?);
            }
            Ok(PhysicalStatement::Block(planned))
        }
        BoundStatement::Break => Ok(PhysicalStatement::Break),
        BoundStatement::Continue => Ok(PhysicalStatement::Continue),
        BoundStatement::Return(expr) => Ok(PhysicalStatement::Return(expr)),
        BoundStatement::Print(expr) => Ok(PhysicalStatement::Print(expr)),
        BoundStatement::Transaction(txn) => Ok(PhysicalStatement::Transaction(txn)),
    }
}

/// Plans one node of the logical plan and its children.
///
/// The recursion the rule files call to plan their own inputs: it takes the node by
/// reference and clones the payloads it carries across, so that a rule may look at a
/// subtree before deciding what to do with it.
///
/// # Errors
///
/// As [`plan`].
pub(crate) fn plan_node(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    match plan {
        LogicalPlan::OneRow => Ok(PhysicalPlan::OneRow),
        LogicalPlan::Values { rows, schema } => Ok(PhysicalPlan::Values {
            rows: rows.clone(),
            schema: schema.clone(),
        }),
        LogicalPlan::Scan {
            table,
            columns,
            alias,
            schema,
            hints,
        } => Ok(PhysicalPlan::TableScan {
            table: *table,
            columns: columns.clone(),
            alias: alias.clone(),
            schema: schema.clone(),
            hints: *hints,
        }),
        LogicalPlan::Filter { input, predicate } => {
            let input = plan_node(input, ctx)?;
            if let Some(seek) = seek::try_index_seek(predicate, &input, ctx)? {
                return Ok(seek);
            }
            let input = subquery::plan_expr_subqueries(predicate, input, ctx)?;
            Ok(PhysicalPlan::Filter {
                input: Box::new(input),
                predicate: predicate.clone(),
            })
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let mut planned = plan_node(input, ctx)?;
            for projection in exprs {
                planned = subquery::plan_expr_subqueries(&projection.expr, planned, ctx)?;
            }
            Ok(PhysicalPlan::Project {
                input: Box::new(planned),
                exprs: exprs.clone(),
                schema: schema.clone(),
            })
        }
        LogicalPlan::Limit { input, top } => {
            if let Some(top_n) = sort::try_top_n(top, input, ctx)? {
                return Ok(top_n);
            }
            Ok(PhysicalPlan::Top {
                input: Box::new(plan_node(input, ctx)?),
                top: top.clone(),
            })
        }
        LogicalPlan::Join { .. } => crate::join::plan_join(plan, ctx),
        LogicalPlan::Aggregate { .. } => aggregate::plan_aggregate(plan, ctx),
        LogicalPlan::Sort { .. } => sort::plan_sort(plan, ctx),
        LogicalPlan::Distinct(_) => sort::plan_distinct(plan, ctx),
        LogicalPlan::SetOp { .. } => setop::plan_setop(plan, ctx),
        LogicalPlan::Subquery { .. } => subquery::plan_subquery(plan, ctx),
    }
}

/// The internal error a form answers until its rule is written: where it was refused and
/// what was written.
///
/// The same shape as the binder's, so that whoever reads a log gets the site to look up.
pub(crate) fn not_implemented(site: &str, form: &str) -> SqlError {
    SqlError::from(InternalError::Bug(format!(
        "{site}: {form} is not implemented yet"
    )))
}
