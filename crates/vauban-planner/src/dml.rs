//! The write statements: [`PhysicalInsert`](crate::PhysicalInsert),
//! [`PhysicalUpdate`](crate::PhysicalUpdate) and [`PhysicalDelete`](crate::PhysicalDelete),
//! the seek that reaches the target rows, and the `spool` flag that protects against the
//! Halloween problem. The three entry points below and the three structs with their
//! `spool` field exist; the rules are not written yet.

use vauban_binder::{DeletePlan, InsertPlan, UpdatePlan};
use vauban_errors::SqlResult;

use crate::context::PlanContext;
use crate::physical::PhysicalStatement;
use crate::plan::not_implemented;

/// Plans an `INSERT` into [`PhysicalStatement::Insert`].
pub(crate) fn plan_insert(
    stmt: &InsertPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("dml::plan_insert", "an INSERT"))
}

/// Plans an `UPDATE` into [`PhysicalStatement::Update`].
pub(crate) fn plan_update(
    stmt: &UpdatePlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("dml::plan_update", "an UPDATE"))
}

/// Plans a `DELETE` into [`PhysicalStatement::Delete`].
pub(crate) fn plan_delete(
    stmt: &DeletePlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<PhysicalStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("dml::plan_delete", "a DELETE"))
}
