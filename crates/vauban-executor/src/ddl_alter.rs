//! Running `ALTER TABLE ADD` / `DROP COLUMN` and `ADD` / `DROP CONSTRAINT`.
//!
//! [`DdlStatement::AlterTable`] carries the three-part table name the binder resolved and
//! the [`AlterTable`](vauban_catalog::AlterTable) action the catalogue takes. This file
//! turns the name into an [`ObjectId`](vauban_catalog::ObjectId) and calls
//! [`Catalog::alter_table`](vauban_catalog::Catalog::alter_table) inside the transaction the
//! caller opened ([`ExecContext::handle`](crate::ExecContext)), then answers
//! [`ExecOutcome::NoRows`]: no COLMETADATA and no row, like the database and table DDL of
//! `ddl.rs`.
//!
//! # What this file checks and what it propagates
//!
//! Nothing is re-checked here. 4901 when a `NOT NULL` column without a default is added to a
//! table that already holds rows, 5074 when a column an index or constraint still references,
//! 3728 when a constraint name is unknown on `DROP CONSTRAINT`, and what
//! [`Catalog::alter_table`] raises for `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints come
//! from `catalog`, already built with their number, severity and state. The binder refused
//! `ALTER COLUMN` and `WITH NOCHECK` before the statement reaches this file.
//!
//! # How a table is found
//!
//! [`CatalogSnapshot::resolve_object`](vauban_catalog::CatalogSnapshot::resolve_object) is the
//! way from the three-part [`QualifiedName`] the binder stored to the [`ObjectId`] that
//! [`Catalog::alter_table`] takes — the binder already proved the table exists when it
//! bound the statement, so a name that resolves to nothing here is the internal 50000 of
//! [`locate`], not a client error number.

use vauban_binder::DdlStatement;
use vauban_catalog::{AlterTable, Catalog, ConstraintDef, ObjectId, QualifiedName};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_txn::TxnHandle;

use crate::context::ExecContext;
use crate::row::ExecOutcome;

/// Runs the `ALTER TABLE` of `stmt` and answers [`ExecOutcome::NoRows`].
///
/// The single entry point `ddl.rs` calls from its `AlterTable` arm; the other variants of
/// [`DdlStatement`] belong to `ddl.rs`, `ddl_index.rs` and `ddl_options.rs` and are the
/// internal 50000 here (`a_statement_that_is_not_an_alter_table_one_is_a_bug`), so a variant
/// routed to the wrong file is reported rather than run as something else.
///
/// # Errors
///
/// What `catalog.alter_table` raises, unchanged — 4901, 5074, 3728 and what the constraint
/// helpers answer — plus the internal 50000 this file raises when the table name no longer
/// resolves, and the internal 50000 for a statement that is not an `ALTER TABLE` one.
pub(crate) fn execute_alter_table(
    stmt: &DdlStatement,
    ctx: &ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    match stmt {
        DdlStatement::AlterTable { table, action } => {
            let id = locate(catalog, handle, table)?;
            if let Err(err) = catalog.alter_table(handle, id, action) {
                if err.number == 2601 && key_constraint_add(action) {
                    return Err(SqlError::could_not_create_constraint_or_index());
                }
                return Err(err);
            }
        }
        DdlStatement::CreateDatabase { .. }
        | DdlStatement::DropDatabase { .. }
        | DdlStatement::CreateTable { .. }
        | DdlStatement::DropTable { .. }
        | DdlStatement::AlterDatabase { .. }
        | DdlStatement::CreateIndex { .. }
        | DdlStatement::DropIndex { .. }
        | DdlStatement::TruncateTable { .. } => {
            return Err(bug(
                "execute_alter_table: only ALTER TABLE is run here; the rest is run by \
                 ddl.rs, ddl_index.rs and ddl_options.rs",
            ));
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// The [`ObjectId`] of the table named `table`.
///
/// # Errors
///
/// The internal 50000 when `table` resolves to nothing (module documentation).
fn locate(catalog: &Catalog, handle: &TxnHandle, table: &QualifiedName) -> SqlResult<ObjectId> {
    catalog
        .snapshot(handle)
        .resolve_object(
            &table.database,
            Some(&table.schema),
            &table.name,
            &table.schema,
        )
        .map(|object| object.id)
        .ok_or_else(|| {
            bug(&format!(
                "ALTER TABLE: the catalogue holds no table named '{}.{}'",
                table.schema, table.name
            ))
        })
}

/// Whether `action` adds a `PRIMARY KEY` or `UNIQUE` constraint through a table rebuild.
fn key_constraint_add(action: &AlterTable) -> bool {
    matches!(
        action,
        AlterTable::AddConstraint { constraint } if matches!(
            constraint.as_ref(),
            ConstraintDef::PrimaryKey { .. } | ConstraintDef::Unique { .. }
        )
    )
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests;
