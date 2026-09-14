//! `INSERT`: default values, `IDENTITY`, `@@IDENTITY` and `SCOPE_IDENTITY`, the 515 of a
//! `NULL` into a column that refuses it. Not written yet: the entry point answers the
//! internal error 50000.

use vauban_errors::SqlResult;
use vauban_planner::PhysicalInsert;

use crate::context::ExecContext;
use crate::dml::not_implemented;
use crate::row::ExecOutcome;

/// Runs one `INSERT`.
///
/// # Errors
///
/// The internal error 50000, until the statement is written.
pub(crate) fn execute(stmt: &PhysicalInsert, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    let _ = (stmt, ctx);
    Err(not_implemented("INSERT"))
}
