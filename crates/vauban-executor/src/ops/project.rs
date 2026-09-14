//! `Project`: computes the select list once for each row of its input.
//!
//! One value per [`BoundProjection`], in the order they were written: the `i`-th value of
//! the answer is the `i`-th column of the schema the node carries
//! (`tests/operator_basics.rs`, `filter_project_limit_pipeline`).

use vauban_binder::{BoundProjection, OutputSchema};
use vauban_errors::SqlResult;
use vauban_planner::PhysicalPlan;

use crate::context::ExecContext;
use crate::expr::eval_expr;
use crate::operator::{Operator, build_operator};
use crate::row::Row;

/// The select list over the rows of `input`.
struct Project<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    exprs: Vec<BoundProjection>,
    schema: OutputSchema,
}

/// Builds the operator of a [`PhysicalPlan::Project`].
///
/// # Errors
///
/// What building the input raises.
pub(crate) fn build<'a>(
    input: &PhysicalPlan,
    exprs: &[BoundProjection],
    schema: &OutputSchema,
) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    Ok(Box::new(Project {
        input: build_operator(input)?,
        exprs: exprs.to_vec(),
        schema: schema.clone(),
    }))
}

impl<'a> Operator<'a> for Project<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        let Some(row) = self.input.next(ctx)? else {
            return Ok(None);
        };
        let mut values = Vec::with_capacity(self.exprs.len());
        for projection in &self.exprs {
            values.push(eval_expr(&projection.expr, Some(&row), ctx)?);
        }
        Ok(Some(values))
    }

    fn close(&mut self) {
        self.input.close();
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}
