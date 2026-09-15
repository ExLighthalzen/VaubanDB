//! The write statements: [`PhysicalInsert`](crate::PhysicalInsert),
//! [`PhysicalUpdate`](crate::PhysicalUpdate) and [`PhysicalDelete`](crate::PhysicalDelete),
//! the seek that reaches the target rows, and the `spool` flag that protects against the
//! Halloween problem.

use vauban_binder::{BoundExpr, ColumnBinding, DeletePlan, InsertPlan, UpdatePlan};
use vauban_errors::SqlResult;
use vauban_storage::TableId;

use crate::context::PlanContext;
use crate::physical::{
    PhysicalDelete, PhysicalInsert, PhysicalPlan, PhysicalStatement, PhysicalUpdate,
};
use crate::plan::plan_node;

/// Plans an `INSERT` into [`PhysicalStatement::Insert`].
pub(crate) fn plan_insert(
    stmt: &InsertPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let source = plan_node(&stmt.source, ctx)?;
    let spool = reads_table(&source, stmt.table, ctx);
    Ok(PhysicalStatement::Insert(PhysicalInsert {
        table: stmt.table,
        columns: stmt.columns.clone(),
        source,
        spool,
    }))
}

/// Plans an `UPDATE` into [`PhysicalStatement::Update`].
pub(crate) fn plan_update(
    stmt: &UpdatePlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let input = plan_node(&stmt.input, ctx)?;
    let spool = spool_for_update(&input, &stmt.assignments, stmt.table, ctx);
    Ok(PhysicalStatement::Update(PhysicalUpdate {
        table: stmt.table,
        input,
        assignments: stmt.assignments.clone(),
        spool,
    }))
}

/// Plans a `DELETE` into [`PhysicalStatement::Delete`].
pub(crate) fn plan_delete(
    stmt: &DeletePlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let input = plan_node(&stmt.input, ctx)?;
    let spool = count_table_references(&input, stmt.table, ctx) > 1;
    Ok(PhysicalStatement::Delete(PhysicalDelete {
        table: stmt.table,
        input,
        spool,
    }))
}

/// Halloween protection for an `UPDATE`:
///
/// `spool: true` when the location plan references the target table more than once
/// (`UPDATE … FROM`), or when an assigned column is a key column of an index the plan uses
/// and the plan reads the target table.
fn spool_for_update(
    input: &PhysicalPlan,
    assignments: &[(ColumnBinding, BoundExpr)],
    table: TableId,
    ctx: &PlanContext<'_>,
) -> bool {
    if count_table_references(input, table, ctx) > 1 {
        return true;
    }
    if assigned_column_is_index_key(assignments, table, ctx) && reads_table(input, table, ctx) {
        return true;
    }
    false
}

/// True when `plan` reads `table` somewhere in its tree: a [`TableScan`] whose table
/// matches, or an [`IndexSeek`] whose index belongs to `table`.
fn reads_table(plan: &PhysicalPlan, table: TableId, ctx: &PlanContext<'_>) -> bool {
    match plan {
        PhysicalPlan::TableScan { table: t, .. } => *t == table,
        PhysicalPlan::IndexSeek { index, .. } => ctx
            .catalog
            .indexes_of(table)
            .iter()
            .any(|(id, _)| *id == *index),
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. } => reads_table(input, table, ctx),
        PhysicalPlan::Distinct(input)
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. } => reads_table(input, table, ctx),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            reads_table(outer, table, ctx) || reads_table(inner, table, ctx)
        }
        PhysicalPlan::HashJoin { build, probe, .. } => {
            reads_table(build, table, ctx) || reads_table(probe, table, ctx)
        }
        PhysicalPlan::HashAggregate { input, .. } | PhysicalPlan::StreamAggregate { input, .. } => {
            reads_table(input, table, ctx)
        }
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => {
            inputs.iter().any(|input| reads_table(input, table, ctx))
        }
        PhysicalPlan::OneRow | PhysicalPlan::Values { .. } => false,
    }
}

/// Counts the times `plan` references `table`, directly or through an index.
fn count_table_references(plan: &PhysicalPlan, table: TableId, ctx: &PlanContext<'_>) -> usize {
    match plan {
        PhysicalPlan::TableScan { table: t, .. } => usize::from(*t == table),
        PhysicalPlan::IndexSeek { index, .. } => usize::from(
            ctx.catalog
                .indexes_of(table)
                .iter()
                .any(|(id, _)| *id == *index),
        ),
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. } => count_table_references(input, table, ctx),
        PhysicalPlan::Distinct(input)
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. } => count_table_references(input, table, ctx),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            count_table_references(outer, table, ctx) + count_table_references(inner, table, ctx)
        }
        PhysicalPlan::HashJoin { build, probe, .. } => {
            count_table_references(build, table, ctx) + count_table_references(probe, table, ctx)
        }
        PhysicalPlan::HashAggregate { input, .. } | PhysicalPlan::StreamAggregate { input, .. } => {
            count_table_references(input, table, ctx)
        }
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => inputs
            .iter()
            .map(|input| count_table_references(input, table, ctx))
            .sum(),
        PhysicalPlan::OneRow | PhysicalPlan::Values { .. } => 0,
    }
}

/// True when any of the assigned columns is a key column of an index on `table`.
fn assigned_column_is_index_key(
    assignments: &[(ColumnBinding, BoundExpr)],
    table: TableId,
    ctx: &PlanContext<'_>,
) -> bool {
    let key_indices: Vec<usize> = ctx
        .catalog
        .indexes_of(table)
        .iter()
        .flat_map(|(_, shape)| shape.columns.iter().map(|kc| usize::from(kc.column)))
        .collect();
    if key_indices.is_empty() {
        return false;
    }
    assignments
        .iter()
        .any(|(cb, _)| key_indices.contains(&cb.index))
}
