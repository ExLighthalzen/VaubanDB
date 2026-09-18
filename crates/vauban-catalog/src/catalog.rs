//! The [`Catalog`] itself: the type the engine holds, and the entry point of each mutation.
//!
//! The methods here are **dispatch**: each one checks nothing and computes nothing, it calls
//! the function of the file that owns the operation (`database.rs`, `table.rs`, `index.rs`,
//! `snapshot.rs`).

use std::sync::Arc;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{DbId, IndexId, Storage};
use vauban_txn::{TransactionManager, TxnHandle};
use vauban_types::{Collation, Decimal};

use crate::def::{IndexDef, TableDef};
use crate::ids::ObjectId;
use crate::meta::{AlterTable, IndexMeta, TableMeta};
use crate::snapshot::CatalogSnapshot;
use crate::{bootstrap, database, identity, index, snapshot, table};

/// The error a stub returns: an internal bug naming the method.
///
/// Written as `Catalog::<method> not implemented`, so that `grep 'not implemented'` lists
/// what is left to write (unit test `stub_message_names_its_method`).
pub(crate) fn not_implemented(method: &str) -> SqlError {
    InternalError::Bug(format!("Catalog::{method} not implemented")).into()
}

/// The metadata of the instance: databases, schemas, tables, columns, indexes, constraints,
/// and the `sys.*` / `INFORMATION_SCHEMA` views built on them.
///
/// Built by [`Catalog::bootstrap`], shared behind an `Arc` by the sessions. Its state lives
/// in ordinary `storage` tables, so a mutation takes the [`TxnHandle`] it must be part of.
pub struct Catalog {
    /// Where the internal tables live.
    pub(crate) storage: Arc<dyn Storage>,
    /// Manager the catalogue opens its own transactions with — the autonomous transaction of
    /// `next_identity`, and the one of the bootstrap.
    pub(crate) txn: Arc<TransactionManager>,
    /// The tables created through this catalogue: the map `ObjectId` → `TableMeta`, and the
    /// counter that hands those identifiers out. `table.rs` owns the type and the accesses
    /// to it, and says there why the metadata sits in memory. Behind a `Mutex` because a
    /// `Catalog` is shared by the sessions behind an `Arc` while `create_table` and
    /// `drop_table` write to it.
    pub(crate) tables: std::sync::Mutex<crate::table::TableStore>,
}

impl Catalog {
    /// Opens the catalogue of `storage`, creating the system databases, their schemas and
    /// the internal tables when they are not there yet.
    ///
    /// Idempotent: a second call on the same storage finds what the first one created and
    /// creates nothing (integration test `bootstrap_is_idempotent`, `tests/bootstrap.rs`).
    /// What a first start writes is laid out in `bootstrap.rs`.
    ///
    /// # Errors
    ///
    /// [`InternalError`] when a call to `storage` or to the transaction manager fails; the
    /// message starts with `bootstrap:` and keeps the text of the underlying error.
    pub fn bootstrap(
        storage: Arc<dyn Storage>,
        txn: Arc<TransactionManager>,
    ) -> Result<Self, InternalError> {
        bootstrap::bootstrap(storage, txn)
    }

    /// The catalogue as `txn` sees it: the rows its snapshot makes visible.
    pub fn snapshot(&self, txn: &TxnHandle) -> CatalogSnapshot {
        snapshot::build(self, txn)
    }

    /// Creates a database named `name`, with `collation` or the collation of the instance.
    ///
    /// # Errors
    ///
    /// The errors of `database.rs`: 1801 for a name already taken, among others.
    pub fn create_database(
        &self,
        txn: &TxnHandle,
        name: &str,
        collation: Option<Collation>,
    ) -> SqlResult<DbId> {
        database::create_database(self, txn, name, collation)
    }

    /// Drops the database named `name`.
    ///
    /// # Errors
    ///
    /// The errors of `database.rs`: 3701 for an unknown name, 3708 for a system database.
    pub fn drop_database(&self, txn: &TxnHandle, name: &str) -> SqlResult<()> {
        database::drop_database(self, txn, name)
    }

    /// Creates the table described by `def` and returns what was stored.
    ///
    /// # Errors
    ///
    /// The errors of `table.rs`, `index.rs` and `constraints.rs`: 2714 for a name already
    /// taken, among others.
    pub fn create_table(&self, txn: &TxnHandle, def: &TableDef) -> SqlResult<TableMeta> {
        table::create_table(self, txn, def)
    }

    /// Applies `change` to `table` and returns the table as it now stands.
    ///
    /// # Errors
    ///
    /// For now, on each call: the stub `not_implemented` of `table.rs`; `ALTER TABLE` is
    /// not implemented yet.
    pub fn alter_table(
        &self,
        txn: &TxnHandle,
        table_id: ObjectId,
        change: &AlterTable,
    ) -> SqlResult<TableMeta> {
        crate::alter::alter_table(self, txn, table_id, change)
    }

    /// Drops the table of identifier `table`.
    ///
    /// # Errors
    ///
    /// The errors of `table.rs`: 3701 for an unknown table, 3726 for a referenced one.
    pub fn drop_table(&self, txn: &TxnHandle, table_id: ObjectId) -> SqlResult<()> {
        table::drop_table(self, txn, table_id)
    }

    /// Creates the index described by `def` and returns what was stored.
    ///
    /// # Errors
    ///
    /// The errors of `index.rs`: 1913 for a name already taken on the table, among others;
    /// the stub `not_implemented` for a `CLUSTERED` index.
    pub fn create_index(&self, txn: &TxnHandle, def: &IndexDef) -> SqlResult<IndexMeta> {
        index::create_index(self, txn, def)
    }

    /// Drops the index of identifier `index`.
    ///
    /// # Errors
    ///
    /// The errors of `index.rs`: 3701 for an unknown index, 3723 for the index of a
    /// constraint.
    pub fn drop_index(&self, txn: &TxnHandle, index_id: IndexId) -> SqlResult<()> {
        index::drop_index(self, txn, index_id)
    }

    /// Hands out the next `IDENTITY` value of `table`.
    ///
    /// Outside the transaction, as in SQL Server: a rolled-back `INSERT` leaves its value
    /// consumed. How `identity.rs` does it, and what it does not cover: the module
    /// documentation of that file.
    ///
    /// # Errors
    ///
    /// The errors of `identity.rs`: an internal bug for a table this catalogue does not
    /// hold, or holds without an `IDENTITY` column; the error of a `storage` or `txn` call
    /// otherwise.
    pub fn next_identity(&self, txn: &TxnHandle, table_id: ObjectId) -> SqlResult<Decimal> {
        identity::next_identity(self, txn, table_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_message_names_its_method() {
        let err = not_implemented("create_table");
        assert_eq!(
            err.message,
            "Internal error: internal bug: Catalog::create_table not implemented"
        );
    }
}
