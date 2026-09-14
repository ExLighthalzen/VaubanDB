//! `DECLARE @x <type> [= e]` and `SET @x = e`: the statements that create the variables of
//! a batch and write to them. The entry points below and the two variants they fill,
//! [`BoundStatement::Declare`] and [`BoundStatement::SetVariable`], are declared; the rules
//! — a redeclared name, 137 on an unknown one, the conversion of the value to the declared
//! type — are not bound yet.
//!
//! The assignment written as a select list item, `SELECT @x = e`, is refused by
//! `query.rs::assignment` for want of a variable scope and belongs here too.

use vauban_errors::SqlResult;
use vauban_parser::{DeclareStatement, SetStatement};

use crate::bound::BoundStatement;
use crate::context::BindContext;
use crate::query::not_implemented;

/// Binds a `DECLARE` into [`BoundStatement::Declare`].
pub(crate) fn bind_declare(
    stmt: &DeclareStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("DECLARE"))
}

/// Binds a `SET @x = e` into [`BoundStatement::SetVariable`].
pub(crate) fn bind_set_variable(
    stmt: &SetStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("SET @variable"))
}
