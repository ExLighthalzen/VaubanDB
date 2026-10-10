//! `Spool`: reads the whole input when it opens, then hands the rows out one at a time.
//!
//! A write statement whose source reads its own target table puts its source under a
//! spool when the planner sets the `spool` flag of the statement: every row of the source
//! is then read before the first write, so a write cannot change what the source reads
//! (the Halloween problem). The input is closed as soon as it is drained, before the first
//! row is handed out (`tests/spool.rs`, `the_input_is_drained_and_closed_when_the_spool_opens`).

use std::collections::VecDeque;

use vauban_binder::OutputSchema;
use vauban_errors::SqlResult;

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::operator::Operator;
use crate::row::Row;

/// Materialises `input` when it opens.
///
/// Public so that a write statement, and a test, can put it over an operator of their own
/// (`tests/spool.rs`).
pub struct Spool<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    /// The rows read when the operator opened, not handed out yet.
    rows: VecDeque<Row>,
}

impl<'a> Spool<'a> {
    /// A spool over the rows of `input`, not opened yet.
    #[must_use]
    pub fn new(input: Box<dyn Operator<'a> + 'a>) -> Self {
        Self {
            input,
            rows: VecDeque::new(),
        }
    }

    /// `input` under a spool when `spool` is true, `input` itself otherwise: the shape a
    /// write statement takes from its `spool` flag (`tests/spool.rs`,
    /// `without_the_flag_the_input_is_handed_back_as_it_is`).
    #[must_use]
    pub fn wrap_if(spool: bool, input: Box<dyn Operator<'a> + 'a>) -> Box<dyn Operator<'a> + 'a> {
        if spool {
            Box::new(Self::new(input))
        } else {
            input
        }
    }
}

impl<'a> Operator<'a> for Spool<'a> {
    /// Opens the input, reads it to the end and closes it.
    ///
    /// The cancellation token is read every [`CANCEL_CHECK_ROWS`] rows: once it is up, the
    /// rows read so far are dropped and `next` answers `None`, which the statement reports
    /// as cancelled (`tests/spool.rs`, `a_cancelled_spool_hands_out_nothing`).
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.rows.clear();
        self.input.open(ctx)?;
        let mut rows = VecDeque::new();
        loop {
            let row = match self.input.next(ctx) {
                Ok(Some(row)) => row,
                Ok(None) => break,
                Err(err) => {
                    self.input.close();
                    return Err(err);
                }
            };
            rows.push_back(row);
            if (rows.len() as u64).is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.input.close();
                return Ok(());
            }
        }
        self.input.close();
        self.rows = rows;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        Ok(self.rows.pop_front())
    }

    fn close(&mut self) {
        self.rows.clear();
    }

    fn schema(&self) -> &OutputSchema {
        self.input.schema()
    }
}
