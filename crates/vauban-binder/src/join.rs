//! The `FROM` of more than one source: `FROM a, b`, `FROM a JOIN b ON …`, and the
//! multi-table scope the columns of such a query resolve against (209, 4104). The entry
//! point below, the [`LogicalPlan::Join`] variant and the [`Scope`](crate::expr::Scope)
//! that holds a list of sources are declared; the rules are not bound yet.

use vauban_errors::SqlResult;
use vauban_parser::TableRef;

use crate::bound::LogicalPlan;
use crate::context::BindContext;
use crate::query::not_implemented;

/// Binds a `FROM` of more than one source, or one written with a `JOIN`, into the tree of
/// [`LogicalPlan::Join`] nodes that reads it.
///
/// `statement_line` is the line the statement starts on, which is the one a 208 carries
/// (`names.rs`). The single-source `FROM` does not come here: `query.rs` hands it to
/// `names::bind_from`.
pub(crate) fn bind_from(
    from: &[TableRef],
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let _ = (from, statement_line, ctx);
    Err(not_implemented("a FROM of more than one source"))
}
