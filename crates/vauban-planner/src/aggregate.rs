//! The aggregation operators: [`HashAggregate`](PhysicalPlan::HashAggregate), and
//! [`StreamAggregate`](PhysicalPlan::StreamAggregate) when the input already arrives
//! ordered on the grouping keys.
//!
//! # The rule
//!
//! The grouping expressions and the aggregate calls are carried across unchanged: what is
//! chosen here is the operator, and nothing else. A
//! [`StreamAggregate`](PhysicalPlan::StreamAggregate) reads one group at a time, so it
//! needs the rows that share a key to arrive together; that holds when the grouping
//! expressions are the leading keys of the order the input delivers (`sort.rs`,
//! `arrives_grouped_on`; `tests/aggregate_sort.rs`,
//! `group_by_over_an_index_order_is_a_stream_aggregate`). Anything else is a
//! [`HashAggregate`](PhysicalPlan::HashAggregate), which reads its input in the order it
//! comes: a key on a column the index does not lead with
//! (`tests/aggregate_sort.rs`, `a_group_by_on_a_non_prefix_column_is_a_hash_aggregate`),
//! the keys of the index taken in another order
//! (`tests/aggregate_sort.rs`, `a_group_by_in_another_order_than_the_index_is_a_hash_aggregate`),
//! or an input that delivers no order at all
//! (`tests/aggregate_sort.rs`, `group_by_over_an_unordered_input_is_a_hash_aggregate`).
//!
//! An aggregate written without `GROUP BY` is one group over the whole input. It keeps an
//! empty list of grouping expressions rather than a form of its own, and is planned as a
//! [`HashAggregate`](PhysicalPlan::HashAggregate) (`tests/aggregate_sort.rs`,
//! `an_aggregate_without_group_by_keeps_an_empty_group_list`).

use vauban_binder::LogicalPlan;
use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::plan_node;
use crate::sort;

/// Plans a [`LogicalPlan::Aggregate`] into the aggregation operator that runs it.
///
/// `plan` is the `Aggregate` node itself; its input is planned through
/// [`plan_node`](crate::plan::plan_node).
///
/// # Errors
///
/// What planning the input raises, and the internal error of a node handed to this rule
/// while being no `Aggregate`.
pub(crate) fn plan_aggregate(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let LogicalPlan::Aggregate {
        input: source,
        group_by,
        aggregates,
        schema,
    } = plan
    else {
        return Err(bug("plan_aggregate: expected an Aggregate plan"));
    };
    let input = plan_node(source, ctx)?;
    if sort::arrives_grouped_on(&input, source, group_by, ctx) {
        return Ok(PhysicalPlan::StreamAggregate {
            input: Box::new(input),
            group_by: group_by.clone(),
            aggregates: aggregates.clone(),
            schema: schema.clone(),
        });
    }
    Ok(PhysicalPlan::HashAggregate {
        input: Box::new(input),
        group_by: group_by.clone(),
        aggregates: aggregates.clone(),
        schema: schema.clone(),
    })
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
