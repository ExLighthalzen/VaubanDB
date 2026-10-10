//! What the planner is allowed to ask about the database: the indexes of a table.
//!
//! # Why a trait of its own
//!
//! The obvious shape would be `PlanContext<'a> { catalog: &'a Catalog, snapshot: &'a
//! CatalogSnapshot }`. Two obstacles:
//! [`Catalog::bootstrap`](vauban_catalog::Catalog::bootstrap) takes an `Arc<dyn Storage>`
//! and an `Arc<TransactionManager>`, so a planner test built on it would exercise the whole
//! chain; and `CatalogSnapshot::indexes_of` takes an
//! [`ObjectId`](vauban_catalog::ObjectId) while
//! [`LogicalPlan::Scan`](vauban_binder::LogicalPlan::Scan) carries a
//! [`TableId`](vauban_storage::TableId), with no way back from one to the other.
//!
//! [`PlanCatalog`] is therefore keyed on the [`TableId`](vauban_storage::TableId) the bound
//! `Scan` holds and has the shape of
//! [`Storage::indexes`](vauban_storage::Storage::indexes): [`SnapshotIndexes`] is the
//! production road of a statement bound against a snapshot, [`StorageIndexes`] the one of a
//! caller that holds none, [`NoIndexes`] the empty answer, and
//! [`FakeCatalog`](crate::testing::FakeCatalog) the test double.

use vauban_catalog::CatalogSnapshot;
use vauban_storage::{IndexId, IndexShape, Storage, TableId};

/// What a planning rule may read about the database.
///
/// The trait has the single question the planning rules ask: which indexes a table
/// has, and of which shape. Growing it is the business of the rule that needs more.
pub trait PlanCatalog {
    /// The indexes of `table`, each with the identifier
    /// [`Storage::seek`](vauban_storage::Storage::seek) takes and its key shape.
    ///
    /// The order of the answer is that of the implementation; a rule that prefers one
    /// index over another says so by its own comparison, not by the position in this
    /// vector.
    fn indexes_of(&self, table: TableId) -> Vec<(IndexId, IndexShape)>;
}

/// A catalogue that reports no index, which leaves a read planned as a scan.
///
/// Meant for a caller that has no catalogue yet, and the one the tests of
/// `tests/trivial.rs` build their [`PlanContext`] on.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoIndexes;

impl PlanCatalog for NoIndexes {
    /// Answers the empty vector for any table.
    fn indexes_of(&self, _table: TableId) -> Vec<(IndexId, IndexShape)> {
        Vec::new()
    }
}

/// The production [`PlanCatalog`]: the indexes the storage layer holds.
pub struct StorageIndexes<'a>(
    /// The storage the indexes are read from.
    pub &'a dyn Storage,
);

impl PlanCatalog for StorageIndexes<'_> {
    /// Delegates to [`Storage::indexes`](vauban_storage::Storage::indexes).
    ///
    /// That method answers a `SqlResult`, this one a vector: an error becomes the empty
    /// vector, which plans the read as a scan rather than failing the statement
    /// (`tests/trivial.rs`, `storage_indexes_falls_back_to_a_scan_on_error`).
    fn indexes_of(&self, table: TableId) -> Vec<(IndexId, IndexShape)> {
        self.0.indexes(table).unwrap_or_default()
    }
}

/// The [`PlanCatalog`] of a statement bound against a [`CatalogSnapshot`]: the indexes
/// `storage` holds for a table, kept to those the snapshot sees.
///
/// `storage` holds an index from its `CREATE INDEX` on, committed or not, while the snapshot
/// sees it under the rules of the transaction it was built for: another transaction's
/// `CREATE INDEX` it has not committed is left out of the plan, and so is an index the
/// transaction of the snapshot has dropped and not committed yet
/// (`crates/vauban-session/tests/ddl_visibility.rs`,
/// `an_uncommitted_create_index_is_waited_for_then_left_out`). A table the snapshot does not
/// hold keeps the answer of `storage`.
pub struct SnapshotIndexes<'a> {
    /// The storage the shapes of the indexes are read from.
    pub storage: &'a dyn Storage,
    /// The snapshot the statement was bound against.
    pub snapshot: &'a CatalogSnapshot,
}

impl PlanCatalog for SnapshotIndexes<'_> {
    /// The answer of [`StorageIndexes`], less the indexes the snapshot does not list for
    /// the table built on `table`.
    fn indexes_of(&self, table: TableId) -> Vec<(IndexId, IndexShape)> {
        let held = StorageIndexes(self.storage).indexes_of(table);
        let Some(meta) = self.snapshot.table_by_storage(table) else {
            return held;
        };
        let seen = self.snapshot.indexes_of(meta.id);
        held.into_iter()
            .filter(|(id, _)| seen.iter().any(|index| index.id == *id))
            .collect()
    }
}

/// What [`plan`](crate::plan) is given besides the statement.
pub struct PlanContext<'a> {
    /// The indexes the rules may choose from.
    pub catalog: &'a dyn PlanCatalog,
}
