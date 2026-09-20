//! `ALTER DATABASE … SET` of the two versioning options:
//! `READ_COMMITTED_SNAPSHOT` and `ALLOW_SNAPSHOT_ISOLATION`, each `ON` or `OFF`.
//!
//! [`DdlStatement::AlterDatabase`] carries the database and the options the binder
//! accepted (the four combinations of the two options, plus `WITH ROLLBACK IMMEDIATE`).
//! This file resolves the database name into a [`DbId`] and calls
//! [`Catalog::set_database_option`] for each option the statement carries.
//!
//! # What this file does not do
//!
//! The `WITH ROLLBACK IMMEDIATE` clause is accepted by the binder and silently ignored
//! here: in this version no exclusive lock is taken on the database and no other session is
//! rolled back. An acceptance criterion of this task decides whether the silence is a
//! divergence: either a named gap is opened or a later change implements the exclusive
//! lock and the rollback — nothing here decides the shape.
//!
//! The switch takes effect on the catalogue at once for the transaction that runs it, so
//! the next [`CatalogSnapshot`] the caller builds carries the new option — which is what
//! the next statement of the same session reads. The versioning itself — the snapshot or
//! the lock the statement asks for — does not live here.
//!
//! # Error separation
//!
//! The binder accepted the two options, their values and the optional `WITH` clause, so
//! the executor knows the two options: `READ_COMMITTED_SNAPSHOT` and
//! `ALLOW_SNAPSHOT_ISOLATION`. An unknown option (one the binder
//! did not name in its match) reaches the catalogue through [`DatabaseOption`] and is
//! reported as the internal 50000 with the name of the option; no client input can reach
//! that path. A database the catalogue does not hold is the internal 50000 of
//! [`Catalog::set_database_option`], which [`catalog::database::set_database_option`]
//! raises when its lookup finds nothing. The 5011 (unknown database) that a production
//! `session` raises is thinner metadata — the binder does not see the catalogue, so it
//! hands the name down without resolving it — and the `set_database_option` call is where
//! the database-not-found path has its first sight of it.

use vauban_binder::DdlStatement;
use vauban_catalog::Catalog;
use vauban_catalog::DatabaseOption;
use vauban_errors::SqlResult;
use vauban_txn::TxnHandle;

use crate::context::ExecContext;
use crate::row::ExecOutcome;

/// Runs the `ALTER DATABASE … SET` of `stmt`, one option at a time.
///
/// # Errors
///
/// What `catalog.set_database_option` raises, unchanged.
pub(crate) fn execute_set_options(
    stmt: &DdlStatement,
    ctx: &ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    match stmt {
        DdlStatement::AlterDatabase { name, options } => {
            set_options(catalog, handle, name, options)?;
        }
        DdlStatement::CreateDatabase { .. }
        | DdlStatement::DropDatabase { .. }
        | DdlStatement::CreateTable { .. }
        | DdlStatement::DropTable { .. }
        | DdlStatement::CreateIndex { .. }
        | DdlStatement::DropIndex { .. }
        | DdlStatement::TruncateTable { .. }
        | DdlStatement::AlterTable { .. } => {
            return Err(bug(
                "execute_set_options: only ALTER DATABASE … SET is run here; the rest is \
                 run by ddl.rs and ddl_index.rs",
            ));
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// Applies one or two options to the database named `name`.
fn set_options(
    catalog: &Catalog,
    handle: &TxnHandle,
    name: &str,
    options: &[(String, Option<String>)],
) -> SqlResult<()> {
    let db = catalog
        .snapshot(handle)
        .database(name)
        .map(|meta| meta.id)
        .ok_or_else(|| {
            // The binder does not resolve the database name, so this path is reached by a name
            // that does not exist. SQL Server answers 5011 severity 14 state 5 for an unknown
            // database in ALTER DATABASE; the crate does not carry that constructor yet.
            bug(&format!(
                "ALTER DATABASE: the catalogue holds no database named '{name}'"
            ))
        })?;
    for (option_name, value) in options {
        let parsed = parse_option(option_name, value.as_deref());
        let Some((opt, on)) = parsed else {
            // The `WITH` pair or any other pair the binder does not name.
            continue;
        };
        catalog.set_database_option(handle, db, opt, on)?;
    }
    Ok(())
}

/// Turns one option/value pair into a [`DatabaseOption`] and its `on` flag, or `None` for
/// a pair the executor ignores (the `WITH` clause).
fn parse_option(name: &str, value: Option<&str>) -> Option<(DatabaseOption, bool)> {
    match (name, value) {
        ("READ_COMMITTED_SNAPSHOT", Some(v)) => {
            Some((DatabaseOption::ReadCommittedSnapshot, on_off(v)?))
        }
        ("ALLOW_SNAPSHOT_ISOLATION", Some(v)) => {
            Some((DatabaseOption::AllowSnapshotIsolation, on_off(v)?))
        }
        _ => None,
    }
}

/// `true` for `ON`, `false` for `OFF`.
fn on_off(v: &str) -> Option<bool> {
    match v {
        "ON" => Some(true),
        "OFF" => Some(false),
        _ => None,
    }
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> vauban_errors::SqlError {
    vauban_errors::SqlError::from(vauban_errors::InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests;
