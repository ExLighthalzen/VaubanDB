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
//! [`Storage::indexes`](vauban_storage::Storage::indexes): [`StorageIndexes`] is the
//! production road, [`NoIndexes`] the empty answer, and
//! [`FakeCatalog`](crate::testing::FakeCatalog) the test double.

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

/// What [`plan`](crate::plan) is given besides the statement.
pub struct PlanContext<'a> {
    /// The indexes the rules may choose from.
    pub catalog: &'a dyn PlanCatalog,
}
