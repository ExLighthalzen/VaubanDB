//! `UPDATE` and `DELETE`: locating the rows, the write conflicts, the spool that protects
//! against the Halloween problem. Not written yet: the two entry points answer the
//! internal error 50000.

use vauban_errors::SqlResult;
use vauban_planner::{PhysicalDelete, PhysicalUpdate};

use crate::context::ExecContext;
use crate::dml::not_implemented;
use crate::row::ExecOutcome;

/// Runs one `UPDATE`.
///
/// # Errors
///
/// The internal error 50000, until the statement is written.
pub(crate) fn execute_update(
    stmt: &PhysicalUpdate,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let _ = (stmt, ctx);
    Err(not_implemented("UPDATE"))
}

/// Runs one `DELETE`.
///
/// # Errors
///
/// The internal error 50000, until the statement is written.
pub(crate) fn execute_delete(
    stmt: &PhysicalDelete,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let _ = (stmt, ctx);
    Err(not_implemented("DELETE"))
}
