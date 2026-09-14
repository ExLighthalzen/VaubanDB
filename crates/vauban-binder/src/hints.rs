//! The locking hints of a table reference: the words of a `WITH (…)` read into the
//! [`LockHints`] of the [`LogicalPlan::Scan`](crate::bound::LogicalPlan::Scan) they were
//! written on, 1047 and 1065 for the ones SQL Server refuses. The entry point below and
//! the `hints` field on the `Scan` are declared, so that giving the words their meaning
//! does not reopen `bound/mod.rs`.
//!
//! `query.rs` accepts those words and drops them (`TABLE_HINT_WORDS`): the `Scan` it builds
//! carries [`LockHints::default`], which is the value of a reference written without a
//! hint.

use vauban_errors::SqlResult;
use vauban_parser::TableHint;

use crate::bound::LockHints;
use crate::query::not_implemented;

/// Reads the hint list written on a table reference into the [`LockHints`] of its `Scan`.
///
/// Nothing calls it yet: the reference that carries a hint list is built by `names.rs`, and
/// those words are not read yet.
#[allow(dead_code, reason = "no caller reads the hint words yet")]
pub(crate) fn bind_hints(hints: &[TableHint]) -> SqlResult<LockHints> {
    let _ = hints;
    Err(not_implemented("a locking hint on a table reference"))
}
