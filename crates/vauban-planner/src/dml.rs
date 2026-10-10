//! The write statements: [`PhysicalInsert`](crate::PhysicalInsert),
//! [`PhysicalUpdate`](crate::PhysicalUpdate) and [`PhysicalDelete`](crate::PhysicalDelete),
//! the seek that reaches the target rows, and the `spool` flag that protects against the
//! Halloween problem.

use vauban_binder::{
    BoundExpr, ColumnBinding, DeletePlan, InsertPlan, OutputColumn, OutputSchema, SelectIntoPlan,
    UpdatePlan,
};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::TableId;
use vauban_types::{SqlType, TypeInfo};

use crate::context::PlanContext;
use crate::physical::{
    PhysicalDelete, PhysicalInsert, PhysicalJoinKind, PhysicalPlan, PhysicalSelectInto,
    PhysicalStatement, PhysicalUpdate, row_id_binding, row_id_table,
};
use crate::plan::plan_node;
use crate::subquery::plan_statement_subplans;

/// Plans a `SELECT … INTO` into [`PhysicalStatement::SelectInto`].
pub(crate) fn plan_select_into(
    stmt: &SelectIntoPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let source = plan_node(&stmt.source, ctx)?;
    Ok(PhysicalStatement::SelectInto(PhysicalSelectInto {
        def: stmt.def.clone(),
        source,
        spool: false,
    }))
}

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
        target: stmt.target.clone(),
    }))
}

/// Plans an `UPDATE` into [`PhysicalStatement::Update`].
pub(crate) fn plan_update(
    stmt: &UpdatePlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let mut input = plan_node(&stmt.input, ctx)?;
    if !input.row_id_tables().contains(&stmt.table) {
        request_row_ids(&mut input, stmt.table, ctx)?;
    }
    let input = with_assignment_subqueries(input, &stmt.assignments, ctx)?;
    let spool = spool_for_update(&input, &stmt.assignments, stmt.table, ctx);
    Ok(PhysicalStatement::Update(PhysicalUpdate {
        table: stmt.table,
        input,
        assignments: stmt.assignments.clone(),
        spool,
        target: stmt.target.clone(),
    }))
}

/// Plans a `DELETE` into [`PhysicalStatement::Delete`].
pub(crate) fn plan_delete(
    stmt: &DeletePlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let mut input = plan_node(&stmt.input, ctx)?;
    if !input.row_id_tables().contains(&stmt.table) {
        request_row_ids(&mut input, stmt.table, ctx)?;
    }
    let spool = count_table_references(&input, stmt.table, ctx) > 1;
    Ok(PhysicalStatement::Delete(PhysicalDelete {
        table: stmt.table,
        input,
        spool,
    }))
}

/// `input` under a [`SubqueryEval`](PhysicalPlan::SubqueryEval) that holds the plans of the
/// subqueries the assigned values of an `UPDATE` hold, in the order of the assignments;
/// `input` unchanged when none holds one (`tests/halloween.rs`,
/// `the_subqueries_of_a_set_are_planned_over_the_input`).
///
/// The node is the root of the input of the statement, where a `WHERE` that holds a subquery
/// is planned as a `Filter` or a join above its own `SubqueryEval`. The executor evaluates each assigned value over a row of `input` with these plans installed,
/// a correlated subquery reading that row as the row of the query that encloses it.
///
/// # Errors
///
/// What planning a subquery raises.
fn with_assignment_subqueries(
    input: PhysicalPlan,
    assignments: &[(ColumnBinding, BoundExpr)],
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalPlan> {
    let mut subplans = Vec::new();
    for (_, value) in assignments {
        subplans.extend(plan_statement_subplans(value, ctx)?);
    }
    if subplans.is_empty() {
        return Ok(input);
    }
    let mut columns = input.schema().columns.clone();
    for subplan in &subplans {
        let ty = subplan.plan.schema().columns.first().map_or_else(
            || TypeInfo::new(SqlType::Bit, false),
            |column| column.ty.clone(),
        );
        columns.push(OutputColumn {
            name: String::new(),
            ty,
        });
    }
    Ok(PhysicalPlan::SubqueryEval {
        input: Box::new(input),
        subplans,
        schema: OutputSchema { columns },
    })
}

/// Asks each scan and each seek of `table` in `plan` for the row identifier of the rows it
/// reads ([`row_id_binding`]), so that the rows of the input of a write statement carry the
/// identifier of the target row they come from (`tests/row_id.rs`,
/// `an_update_over_a_join_asks_the_target_side_alone`).
///
/// The walk goes down the nodes that keep the tail of their input
/// ([`PhysicalPlan::row_id_tables`]) and stops at the others, and at the inner side of a
/// semi-join or an anti-semi-join, whose rows are not handed over. A node that already
/// carries the binding is left as it is.
///
/// Not called when the bound input already carries the binding for `table`: the binder puts
/// it on the source the target of a joined statement names, and the other reads of the
/// table, a self-join among them, stay unmarked (`tests/row_id.rs`,
/// `a_self_join_marks_the_source_the_target_names`).
///
/// # Errors
///
/// The internal error 50000 for a table whose identifier does not fit in a column
/// identifier.
fn request_row_ids(
    plan: &mut PhysicalPlan,
    table: TableId,
    ctx: &PlanContext<'_>,
) -> SqlResult<()> {
    match plan {
        PhysicalPlan::TableScan {
            table: read,
            columns,
            ..
        } => {
            if *read == table {
                push_row_id(columns, table)?;
            }
        }
        PhysicalPlan::IndexSeek { index, columns, .. } => {
            let index = *index;
            if ctx
                .catalog
                .indexes_of(table)
                .iter()
                .any(|(id, _)| *id == index)
            {
                push_row_id(columns, table)?;
            }
        }
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. } => request_row_ids(input, table, ctx)?,
        PhysicalPlan::NestedLoopJoin {
            outer, inner, kind, ..
        } => {
            request_row_ids(outer, table, ctx)?;
            if !matches!(kind, PhysicalJoinKind::Semi | PhysicalJoinKind::AntiSemi) {
                request_row_ids(inner, table, ctx)?;
            }
        }
        PhysicalPlan::HashJoin { build, probe, .. } => {
            request_row_ids(probe, table, ctx)?;
            request_row_ids(build, table, ctx)?;
        }
        PhysicalPlan::OneRow
        | PhysicalPlan::Values { .. }
        | PhysicalPlan::HashAggregate { .. }
        | PhysicalPlan::StreamAggregate { .. }
        | PhysicalPlan::Distinct(_)
        | PhysicalPlan::Union { .. }
        | PhysicalPlan::Except { .. }
        | PhysicalPlan::Intersect { .. } => {}
    }
    Ok(())
}

/// Appends the row-identifier binding of `table` to the `columns` of a scan or a seek,
/// unless they already carry it.
fn push_row_id(columns: &mut Vec<ColumnBinding>, table: TableId) -> SqlResult<()> {
    if columns
        .iter()
        .any(|binding| row_id_table(binding) == Some(table))
    {
        return Ok(());
    }
    let binding = row_id_binding(table).ok_or_else(|| {
        SqlError::from(InternalError::Bug(format!(
            "row identifier: table {table} does not fit in a column identifier"
        )))
    })?;
    columns.push(binding);
    Ok(())
}

/// Halloween protection for an `UPDATE`:
///
/// `spool: true` when the location plan references the target table more than once
/// (`UPDATE … FROM`, or a subquery of the `WHERE` that reads the target), when a subquery
/// of an assigned value reads the target table, or when an assigned column is a key column
/// of an index the plan uses and the plan reads the target table.
fn spool_for_update(
    input: &PhysicalPlan,
    assignments: &[(ColumnBinding, BoundExpr)],
    table: TableId,
    ctx: &PlanContext<'_>,
) -> bool {
    if count_table_references(input, table, ctx) > 1 {
        return true;
    }
    if assignments_read_table(assignments, table, ctx) {
        return true;
    }
    if assigned_column_is_index_key(assignments, table, ctx) && reads_table(input, table, ctx) {
        return true;
    }
    false
}

/// True when an assigned value holds a subquery that reads `table`, at any depth of its
/// plan (`tests/halloween.rs`, `a_set_value_reading_the_target_requires_a_spool`).
///
/// The values of an `UPDATE` are not planned with the statement, so their subqueries are
/// planned here for the question alone. A value whose subqueries cannot be planned counts
/// as reading the table: the spool costs memory, its absence a wrong result.
fn assignments_read_table(
    assignments: &[(ColumnBinding, BoundExpr)],
    table: TableId,
    ctx: &PlanContext<'_>,
) -> bool {
    assignments
        .iter()
        .any(|(_, value)| match plan_statement_subplans(value, ctx) {
            Ok(subplans) => subplans
                .iter()
                .any(|subplan| reads_table(&subplan.plan, table, ctx)),
            Err(_) => true,
        })
}

/// True when `plan` reads `table` somewhere in its tree: a [`TableScan`] whose table
/// matches, or an [`IndexSeek`] whose index belongs to `table`, including inside the plan
/// of a subquery a [`SubqueryEval`](PhysicalPlan::SubqueryEval) evaluates, at any depth
/// (`tests/halloween.rs`, `an_insert_whose_not_exists_reads_the_target_requires_a_spool`,
/// `a_target_read_two_subqueries_deep_requires_a_spool`).
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
        | PhysicalPlan::TopN { input, .. } => reads_table(input, table, ctx),
        PhysicalPlan::SubqueryEval {
            input, subplans, ..
        } => {
            reads_table(input, table, ctx)
                || subplans
                    .iter()
                    .any(|subplan| reads_table(&subplan.plan, table, ctx))
        }
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

/// Counts the times `plan` references `table`, directly or through an index, the plans of
/// the subqueries of a [`SubqueryEval`](PhysicalPlan::SubqueryEval) included.
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
        | PhysicalPlan::TopN { input, .. } => count_table_references(input, table, ctx),
        PhysicalPlan::SubqueryEval {
            input, subplans, ..
        } => {
            count_table_references(input, table, ctx)
                + subplans
                    .iter()
                    .map(|subplan| count_table_references(&subplan.plan, table, ctx))
                    .sum::<usize>()
        }
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
