//! Variables and the control of flow: `DECLARE`, `SET @x = e`, `IF`, `WHILE`, `BEGIN ...
//! END`, `BREAK`, `CONTINUE`, `RETURN`, `PRINT`. Not written yet: the entry point answers
//! the internal error 50000.

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::PhysicalStatement;

use crate::context::{ExecContext, RowSink};
use crate::row::ExecOutcome;

/// Runs one statement of the control of flow, the nested statements included.
///
/// # Errors
///
/// The internal error 50000, until the statements are written.
pub(crate) fn execute(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'_>,
    sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    let _ = (stmt, ctx, sink);
    Err(SqlError::from(InternalError::Bug(
        "execute: variables and control of flow are not implemented yet".to_owned(),
    )))
}
