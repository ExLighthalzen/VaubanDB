//! `Spool`: reads the whole input when it opens, then hands the rows out one at a time.
//!
//! An `UPDATE` or a `DELETE` whose input reads its own target table puts that input under a
//! spool when the planner sets the `spool` flag of the statement (`dml/update_delete.rs`):
//! every row of the input is then read before the first write, so a write cannot change
//! what the input reads (the Halloween problem; `tests/dml_input.rs`,
//! `an_in_over_the_target_writes_each_row_once`). An `INSERT` with the flag reads its source
//! to the end itself, before its first write (`dml/insert.rs`). The input is closed as soon
//! as it is drained, before the first row is handed out (`tests/spool.rs`,
//! `the_input_is_drained_and_closed_when_the_spool_opens`), and each row is handed out
//! whole, the row identifiers of its tail included (`tests/dml_input.rs`,
//! `the_row_identifier_crosses_the_spool`).

use std::collections::VecDeque;

use vauban_binder::OutputSchema;
use vauban_errors::SqlResult;

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::operator::Operator;
use crate::row::Row;

/// Materialises `input` when it opens.
///
/// Public so that a test can put it over an operator of its own (`tests/spool.rs`).
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

    /// `input` under a spool when `spool` is true, `input` itself otherwise: the shape the
    /// input of an `UPDATE` or a `DELETE` takes from its `spool` flag (`tests/spool.rs`,
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
    /// rows read so far are dropped and `next` answers `None` (`tests/spool.rs`,
    /// `a_cancelled_spool_hands_out_nothing`). The statement reads the token once the spool
    /// is open and reports itself cancelled, not as a write of zero rows
    /// (`dml/update_delete/tests/mod.rs`, `a_cancelled_spool_cancels_the_statement`).
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
