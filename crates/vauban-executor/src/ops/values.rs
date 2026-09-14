//! `Values` and `OneRow`: constant rows, evaluated one row at a time.
//!
//! `OneRow` is a `Values` of one row and no column: the implicit source of a `SELECT`
//! without `FROM`. Not an empty result, `SELECT 1` answers a row, and not a row of one
//! `NULL` either (`tests/operator_basics.rs`, `values_operator`).

use vauban_binder::{BoundExpr, OutputSchema};
use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::context::ExecContext;
use crate::expr::eval_expr;
use crate::operator::Operator;
use crate::row::Row;

/// Constant rows: each expression is evaluated when its row is asked for, with no row
/// below it.
pub(crate) struct Values {
    /// The rows, each of the width of `schema`.
    rows: Vec<Vec<BoundExpr>>,
    /// The columns the rows are described by.
    schema: OutputSchema,
    /// The index of the next row to produce.
    next: usize,
}

/// Builds the operator of a [`PhysicalPlan::Values`](vauban_planner::PhysicalPlan::Values).
///
/// # Errors
///
/// The internal error 50000 for a row whose width is not the schema's, which no client
/// input can cause: the binder builds both.
pub(crate) fn build<'a>(
    rows: &[Vec<BoundExpr>],
    schema: &OutputSchema,
) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let width = schema.columns.len();
    if let Some(row) = rows.iter().find(|row| row.len() != width) {
        return Err(SqlError::from(InternalError::Bug(format!(
            "Values: a row of {} value(s) under a schema of {width} column(s)",
            row.len()
        ))));
    }
    Ok(Box::new(Values {
        rows: rows.to_vec(),
        schema: schema.clone(),
        next: 0,
    }))
}

/// Builds the operator of a [`PhysicalPlan::OneRow`](vauban_planner::PhysicalPlan::OneRow):
/// one row of no column.
///
/// # Errors
///
/// The single empty row matches the empty schema, so [`build`] raises nothing here.
pub(crate) fn build_one_row<'a>() -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    build(
        &[Vec::new()],
        &OutputSchema {
            columns: Vec::new(),
        },
    )
}

impl<'a> Operator<'a> for Values {
    fn open(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.next = 0;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        let Some(exprs) = self.rows.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        let mut row = Vec::with_capacity(exprs.len());
        for expr in exprs {
            row.push(eval_expr(expr, None, ctx)?);
        }
        Ok(Some(row))
    }

    fn close(&mut self) {
        self.next = self.rows.len();
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}
