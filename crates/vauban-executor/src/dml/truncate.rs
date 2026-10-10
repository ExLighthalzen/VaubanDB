//! `TRUNCATE TABLE`: removes the rows visible to the statement in the current transaction,
//! and resets the `IDENTITY` counter when the table carries one.
//!
//! A table a `FOREIGN KEY` of another table references is refused with 4712 here, while the
//! statement runs, against the catalogue of its transaction: a `TRUNCATE` that the control
//! of flow does not reach raises nothing, and one that follows the `DROP TABLE` of the
//! referencing table in the same batch empties the table (`tests/truncate_into.rs`,
//! `truncate_after_the_referencing_table_is_dropped_empties_the_table`).

use vauban_catalog::{Catalog, CatalogSnapshot, ConstraintMeta, ObjectId, QualifiedName};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::Storage;
use vauban_txn::TxnHandle;

use crate::context::ExecContext;
use crate::row::ExecOutcome;

/// Empties the table `name` names and returns [`ExecOutcome::NoRows`].
///
/// # Errors
///
/// 4712 when a `FOREIGN KEY` of another table references the table
/// ([`referenced_by_another_table`]); a `FOREIGN KEY` of the table on itself does not count
/// (`tests/truncate_into.rs`, `truncate_of_a_referenced_table_is_4712`,
/// `truncate_of_a_self_referencing_table_empties_it`).
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
    if referenced_by_another_table(&snap, object.id) {
        let printed = format!("{}.{}", name.schema, name.name);
        return Err(SqlError::cannot_truncate_referenced_table(&printed));
    }
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

/// Whether a table of `snap` other than `object` declares a `FOREIGN KEY` that points at
/// `object`.
fn referenced_by_another_table(snap: &CatalogSnapshot, object: ObjectId) -> bool {
    snap.tables()
        .filter(|table| table.id != object)
        .flat_map(|table| table.constraints.iter())
        .any(|constraint| {
            matches!(
                constraint,
                ConstraintMeta::ForeignKey { referenced_table, .. } if *referenced_table == object
            )
        })
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
