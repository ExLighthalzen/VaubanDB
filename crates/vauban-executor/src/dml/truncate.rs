//! `TRUNCATE TABLE`: removes the rows visible to the statement in the current transaction,
//! and resets the `IDENTITY` counter when the table carries one.

use vauban_catalog::{Catalog, QualifiedName};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::Storage;
use vauban_txn::TxnHandle;

use crate::context::ExecContext;
use crate::row::ExecOutcome;

/// Empties the table `name` names and returns [`ExecOutcome::NoRows`].
///
/// The binder refuses a table referenced by a foreign key (4712); this function assumes that
/// check already ran.
pub(crate) fn execute(name: &QualifiedName, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    let catalog: &Catalog = ctx.catalog()?;
    let handle: &TxnHandle = ctx.handle()?;
    let storage: &dyn Storage = ctx.storage()?;
    let snap = catalog.snapshot(handle);
    let object = snap
        .resolve_object(
            &name.database,
            Some(name.schema.as_str()),
            &name.name,
            "dbo",
        )
        .ok_or_else(|| SqlError::cannot_find_object_to_truncate(&name.name).with_line(0))?;
    let meta = snap
        .table(object.id)
        .ok_or_else(|| bug("TRUNCATE: resolved object is not a table"))?;
    let table_id = meta.storage_id;

    let mut row_ids = Vec::new();
    for row in storage.scan(ctx.snapshot()?, table_id)? {
        row_ids.push(row?.0);
    }
    for id in row_ids {
        storage.delete(handle.id, table_id, id)?;
    }

    catalog.reset_identity_for_truncate(handle, object.id)?;

    if let Some(session) = ctx.session.as_deref_mut() {
        session.rowcount = 0;
    }
    Ok(ExecOutcome::NoRows)
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
