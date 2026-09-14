//! `Top`: `TOP n [PERCENT]` over its input.
//!
//! The row count is evaluated **once, when the operator opens**, and a budget of zero
//! rows leaves the input unopened. Both halves are observable, and each statement below
//! answers something else under the opposite rule (`tests/execute_select.rs`):
//!
//! | Query | Answer | What it rules out |
//! |---|---|---|
//! | `SELECT TOP (-1) 1 / 0;` | 127 | evaluating the select list first would give 8134 |
//! | `SELECT TOP (-1) 1 WHERE 1 / 0 = 1;` | 127 | evaluating the `WHERE` first would give 8134 |
//! | `SELECT TOP (0) 1 / 0;` | 0 rows, no error | pulling a row to drop it would give 8134 |
//! | `SELECT TOP (0) 1 WHERE 1 / 0 = 1;` | 0 rows, no error | same, one operator lower |
//! | `SELECT TOP (0) PERCENT 1 / 0;` | 0 rows, no error | a percentage of zero short-circuits too |
//! | `SELECT TOP (1) 1 / 0;` | 8134 | a non-zero budget does read the input |
//!
//! A row count that is not zero pulls exactly the rows it keeps: `TOP 1` over three rows
//! asks its input for one row (`tests/operator_basics.rs`, `limit_stops_early`).
//!
//! # `PERCENT` rounds **up**
//!
//! `ceil(n × p / 100)`, so a percentage above zero keeps the single row of a `SELECT`
//! without `FROM`: over one row, rounding up answers **one** row where truncating would
//! answer no row, and `SELECT TOP (50) PERCENT 1;` answers one. `SELECT TOP (0.0001)
//! PERCENT 1;` answers one row too. A percentage needs the size of the input, so the
//! operator reads the whole input when it opens, reading the cancellation token as it
//! goes, and hands the kept rows out one at a time.
//!
//! The errors of the row count, 127, 1060 and the two shapes still answered as the
//! internal error 50000, are listed in `plan.rs`.

use std::collections::VecDeque;

use vauban_binder::{BoundTop, OutputSchema};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::PhysicalPlan;

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::operator::{Operator, build_operator};
use crate::plan::{RowBudget, eval_budget};
use crate::row::Row;

/// `TOP n [PERCENT]` over `input`.
///
/// Public so that a test can put an operator of its own under it
/// (`tests/operator_basics.rs`, `limit_stops_early`).
pub struct Limit<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    top: BoundTop,
    state: State,
    /// Whether `open` opened the input, which decides whether `close` closes it.
    input_open: bool,
}

/// Where the operator stands between `open` and `close`.
enum State {
    /// Not opened, or exhausted: `next` answers `None`.
    Exhausted,
    /// `TOP n`: this many rows may still be pulled from the input.
    Counting { remaining: usize },
    /// `TOP p PERCENT`: the kept rows, read when the operator opened.
    Buffered { rows: VecDeque<Row> },
}

/// Builds the operator of a [`PhysicalPlan::Top`].
///
/// # Errors
///
/// What building the input raises.
pub(crate) fn build<'a>(
    input: &PhysicalPlan,
    top: &BoundTop,
) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    Ok(Box::new(Limit::new(build_operator(input)?, top.clone())))
}

impl<'a> Limit<'a> {
    /// `top` over the rows of `input`, not opened yet.
    #[must_use]
    pub fn new(input: Box<dyn Operator<'a> + 'a>, top: BoundTop) -> Self {
        Self {
            input,
            top,
            state: State::Exhausted,
            input_open: false,
        }
    }

    /// Reads the whole input for a percentage, stopping early once the token is up: the
    /// rows read so far are then dropped, and `next` answers `None`.
    fn materialise(&mut self, percent: RowBudget, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        let mut rows = Vec::new();
        while let Some(row) = self.input.next(ctx)? {
            rows.push(row);
            if (rows.len() as u64).is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.state = State::Exhausted;
                return Ok(());
            }
        }
        let kept = percent.rows_of(rows.len());
        rows.truncate(kept);
        self.state = State::Buffered {
            rows: rows.into_iter().collect(),
        };
        Ok(())
    }
}

impl<'a> Operator<'a> for Limit<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        if self.top.with_ties {
            // `bind_top` refuses `WITH TIES` without an `ORDER BY` (error 1062), and the
            // ordered top is not written, so no plan reaches this arm with rows.
            return Err(SqlError::from(InternalError::Bug(
                "Top: TOP ... WITH TIES is not implemented yet".to_owned(),
            )));
        }
        let budget = eval_budget(&self.top, ctx)?;
        if budget.is_zero() {
            // The input is not opened: `SELECT TOP (0) 1 / 0;` raises nothing.
            self.state = State::Exhausted;
            return Ok(());
        }
        self.input.open(ctx)?;
        self.input_open = true;
        match budget {
            RowBudget::Rows(count) => {
                self.state = State::Counting {
                    remaining: usize::try_from(count).unwrap_or(usize::MAX),
                };
                Ok(())
            }
            RowBudget::Percent(_) => self.materialise(budget, ctx),
        }
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match &mut self.state {
            State::Exhausted => Ok(None),
            State::Counting { remaining } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                match self.input.next(ctx)? {
                    Some(row) => {
                        *remaining -= 1;
                        Ok(Some(row))
                    }
                    None => {
                        self.state = State::Exhausted;
                        Ok(None)
                    }
                }
            }
            State::Buffered { rows } => Ok(rows.pop_front()),
        }
    }

    fn close(&mut self) {
        if self.input_open {
            self.input.close();
            self.input_open = false;
        }
        self.state = State::Exhausted;
    }

    fn schema(&self) -> &OutputSchema {
        self.input.schema()
    }
}
