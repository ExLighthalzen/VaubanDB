//! Running `CREATE INDEX` and `DROP INDEX`.
//!
//! The two variants of [`DdlStatement`] this file serves call one method of
//! [`Catalog`] inside the transaction the caller opened
//! ([`ExecContext::handle`](crate::ExecContext)) and answer
//! [`ExecOutcome::NoRows`], like the database and table DDL of `ddl.rs`: no COLMETADATA
//! and no row (`create_index_is_visible_on_snapshot`,
//! `drop_index_unknown_is_3701_in_two_states`). A standalone index only; the index of a
//! `PRIMARY KEY` or of a `UNIQUE` written inside a `CREATE TABLE` is created by
//! `catalog.create_table`, which `ddl.rs` already calls.
//!
//! # `CREATE INDEX`: where each answer comes from
//!
//! `CREATE INDEX` re-checks nothing here: the duplicate key column (1909) is refused at
//! binding time, and the unknown column (1911), the taken name (1913), the second
//! clustered index (1902) and the unknown table come from `catalog.create_index`. The
//! catalogue raises the internal 50000 naming 1911 and 1913 rather than the two SQL Server
//! errors, which is what a client sees
//! (`create_index_twice_names_1913`, `create_index_on_an_unknown_column_names_1911`); the
//! divergence is the catalogue's, not this file's. A second divergence is on `drop_index`:
//! SQL Server frees the name at once, the deferred `DROP` of the catalogue keeps it taken
//! until the `COMMIT`.
//!
//! This file adds one thing the catalogue cannot do: turning the name and table a
//! `DROP INDEX` was written with into the [`IndexId`](vauban_storage::IndexId) that
//! [`Catalog::drop_index`] takes.
//!
//! # `DROP INDEX`: two states of 3701, and `IF EXISTS`
//!
//! The two states are what this file exists for: the state follows whether the table was
//! found, **7** when it was and the index was not, **6** when it was not. The number of
//! parts written is **not** what the state follows: a three-part name whose table is there
//! answers 7 next to a two-part 7, and a three-part name whose database is not there
//! answers 6 next to a two-part 6, which is the pair that separates "the state follows the
//! table" from "the state follows the shape of the name"
//! (`a_three_part_name_answers_the_state_of_its_table`,
//! `drop_index_unknown_is_3701_in_two_states`). The two `IF EXISTS` forms answer
//! [`ExecOutcome::NoRows`] where the two bare ones answer an error
//! (`locate_reports_the_table_before_the_index`).
//!
//! [`SqlError::cannot_drop`] keys its state on the kind and gives 7 for `index`, so state 6
//! is written here, as `catalog/index.rs` writes it for the neighbouring case of a table
//! carrying a deferred `DROP`. The comparison of the index name written in [`locate`]
//! ignores ASCII case, as `catalog/index.rs` does when it refuses a duplicate index name:
//! `drop_index_of_a_created_index_reaches_the_catalogue` drops `ix_a` written `IX_A`,
//! which a case-sensitive comparison would miss.
//!
//! # Deliberate difference: the name printed in the 3701
//!
//! The `%.*ls` of 3701 carries the table and the index joined by a dot:
//! `'dbo.t.ix_nope'` and `'dbo.nosuch.ix_nope'`. SQL Server prints the table **as
//! written** (`'master.dbo.t.ix_nope'` for a three-part name); the binder hands over a
//! resolved three-part [`QualifiedName`] and drops what the client wrote, so this file
//! prints `<schema>.<table>.<index>` for the two-part and the three-part forms alike. That
//! is the difference `ddl.rs` already carries for the 3701 of a `DROP TABLE`, same cause.
//!
//! # How a `DROP INDEX` reaches an index by its name
//!
//! [`CatalogSnapshot::indexes_of`](vauban_catalog::CatalogSnapshot::indexes_of) is the way
//! from a name to an [`IndexId`](vauban_storage::IndexId): the `pub` items of
//! `vauban-catalog` offer no second one —
//! [`TableMeta`](vauban_catalog::TableMeta) lists the clustered index and the constraints
//! but not the indexes, and the `IndexShape` of `storage` carries no name.
//! `create_index_is_visible_on_snapshot` reads the `IndexMeta` of the index it created out
//! of the snapshot, and `drop_index_of_a_created_index_reaches_the_catalogue` drops that
//! index by its name. What a name that reaches no index answers is the 3701 of the section
//! above.

use vauban_binder::DdlStatement;
use vauban_catalog::{Catalog, IndexDef, QualifiedName};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::IndexId;
use vauban_txn::TxnHandle;

use crate::row::ExecOutcome;

/// Error 3701, the number a `DROP` of something that is not there answers.
///
/// The `IF EXISTS` of a `DROP INDEX` swallows this one, in both of its states (module
/// documentation).
const CANNOT_DROP: u32 = 3701;

/// The state of the 3701 a `DROP INDEX` answers when its **table** is not there.
///
/// [`SqlError::cannot_drop`] keys the state on the kind and gives 7 for `index`, the state
/// of a table that is there without the index; 6 is the state of the missing table
/// (module documentation).
const INDEX_OF_A_MISSING_TABLE_3701_STATE: u8 = 6;

/// Runs the `CREATE INDEX` or the `DROP INDEX` of `stmt` and answers
/// [`ExecOutcome::NoRows`].
///
/// The single entry point `ddl.rs` calls from the two index arms of its `Ddl` match; the
/// other four variants of [`DdlStatement`] belong to `ddl.rs` and are the internal 50000
/// here (`a_statement_that_is_not_an_index_one_is_a_bug`), so a variant routed to the wrong
/// file is reported rather than run as something else.
///
/// # Errors
///
/// What `catalog.create_index` and `catalog.drop_index` raise, unchanged — including the
/// internal 50000 the catalogue raises in place of the SQL Server numbers it does not
/// build (1909, 1911, 1913, 3723) — plus the 3701 this file raises for a `DROP INDEX` whose
/// table or index resolves to nothing, and the internal 50000 for a statement that is not
/// one of the two index ones.
pub(crate) fn execute_index_ddl(
    stmt: &DdlStatement,
    catalog: &Catalog,
    handle: &TxnHandle,
) -> SqlResult<ExecOutcome> {
    match stmt {
        DdlStatement::CreateIndex { def } => create_index(catalog, handle, def)?,
        DdlStatement::DropIndex {
            name,
            table,
            if_exists,
        } => drop_index(catalog, handle, name, table, *if_exists)?,
        DdlStatement::CreateDatabase { .. }
        | DdlStatement::DropDatabase { .. }
        | DdlStatement::CreateTable { .. }
        | DdlStatement::DropTable { .. }
        // The two `ALTER` statements reach `ddl.rs`, which answers for them; they are
        // listed here for the same reason as the four above.
        | DdlStatement::AlterDatabase { .. }
        | DdlStatement::TruncateTable { .. }
        | DdlStatement::AlterTable { .. } => {
            return Err(bug(
                "execute_index_ddl: only CREATE INDEX and DROP INDEX are run here; the \
                 database and table DDL is run by ddl.rs",
            ));
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// `CREATE [UNIQUE] [CLUSTERED] INDEX ix ON t (c1, c2 DESC)`: one call, nothing re-checked.
///
/// [`IndexDef::table`] is the [`ObjectId`](vauban_catalog::ObjectId) the binder resolved,
/// so no name is turned into an identifier here.
fn create_index(catalog: &Catalog, handle: &TxnHandle, def: &IndexDef) -> SqlResult<()> {
    catalog.create_index(handle, def)?;
    Ok(())
}

/// `DROP INDEX [IF EXISTS] ix ON t`: the name resolved into an identifier, then one call.
fn drop_index(
    catalog: &Catalog,
    handle: &TxnHandle,
    name: &str,
    table: &QualifiedName,
    if_exists: bool,
) -> SqlResult<()> {
    let outcome =
        locate(catalog, handle, name, table).and_then(|index| catalog.drop_index(handle, index));
    match outcome {
        Ok(()) => Ok(()),
        Err(err) if if_exists && err.number == CANNOT_DROP => Ok(()),
        Err(err) => Err(err),
    }
}

/// The identifier of the index called `name` on `table`.
///
/// # Errors
///
/// 3701 state 6 when `table` resolves to nothing, 3701 state 7 when it resolves and carries
/// no index of that name (module documentation). The comparison of the index name ignores
/// ASCII case, as `catalog/index.rs` does when it refuses a duplicate name.
fn locate(
    catalog: &Catalog,
    handle: &TxnHandle,
    name: &str,
    table: &QualifiedName,
) -> SqlResult<IndexId> {
    let printed = printed(table, name);
    let snapshot = catalog.snapshot(handle);
    let Some(object) = snapshot.resolve_object(
        &table.database,
        Some(&table.schema),
        &table.name,
        &table.schema,
    ) else {
        let mut err = SqlError::cannot_drop("drop", "index", &printed);
        err.state = INDEX_OF_A_MISSING_TABLE_3701_STATE;
        return Err(err);
    };
    snapshot
        .indexes_of(object.id)
        .iter()
        .find(|index| index.name.eq_ignore_ascii_case(name))
        .map(|index| index.id)
        .ok_or_else(|| SqlError::cannot_drop("drop", "index", &printed))
}

/// The name a 3701 of `DROP INDEX` prints: `<schema>.<table>.<index>`, the deliberate
/// difference of the module documentation.
fn printed(table: &QualifiedName, name: &str) -> String {
    format!("{}.{}.{}", table.schema, table.name, name)
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests;
