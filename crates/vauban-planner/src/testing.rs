//! The test double of [`PlanCatalog`], for the tests of the planning rules.
//!
//! A planning rule is tested by declaring the indexes it may choose from, not by building
//! a catalogue over a storage and a transaction manager. [`FakeCatalog`] is that
//! declaration; it is `pub` because the integration tests of the crate use it, and it is
//! part of no production path.

use std::collections::HashMap;

use vauban_storage::{IndexId, IndexShape, KeyColumn, TableId};

use crate::context::PlanCatalog;

/// A [`PlanCatalog`] whose indexes are the ones the test declared.
///
/// ```
/// use vauban_planner::{PlanCatalog, PlanContext, testing::FakeCatalog};
/// use vauban_storage::{KeyColumn, TableId};
///
/// let catalog = FakeCatalog::new().with_index(
///     TableId(1),
///     &[KeyColumn { column: 0, descending: false }],
///     true,
/// );
/// assert_eq!(catalog.indexes_of(TableId(1)).len(), 1);
/// assert!(catalog.indexes_of(TableId(2)).is_empty());
/// let ctx = PlanContext { catalog: &catalog };
/// let _ = ctx.catalog.indexes_of(TableId(1));
/// ```
#[derive(Debug, Clone, Default)]
pub struct FakeCatalog {
    indexes: HashMap<TableId, Vec<(IndexId, IndexShape)>>,
    next_id: u32,
}

impl FakeCatalog {
    /// A catalogue with no index, which answers like [`NoIndexes`](crate::NoIndexes).
    #[must_use]
    pub fn new() -> Self {
        Self {
            indexes: HashMap::new(),
            next_id: 1,
        }
    }

    /// Declares one index on `table`, over the given key columns.
    ///
    /// The [`IndexId`] is handed out by the double, starting at `1` and increasing in the
    /// order the indexes were declared, so a test can name the index it expects to be
    /// chosen. [`IndexShape::included`] is empty: a rule that reads it adds it here.
    #[must_use]
    pub fn with_index(mut self, table: TableId, columns: &[KeyColumn], unique: bool) -> Self {
        let id = IndexId(self.next_id);
        self.next_id += 1;
        let shape = IndexShape {
            columns: columns.to_vec(),
            unique,
            included: Vec::new(),
        };
        self.indexes.entry(table).or_default().push((id, shape));
        self
    }
}

impl PlanCatalog for FakeCatalog {
    /// The indexes declared by [`FakeCatalog::with_index`] on `table`, in declaration
    /// order; the empty vector for a table no index was declared on.
    fn indexes_of(&self, table: TableId) -> Vec<(IndexId, IndexShape)> {
        self.indexes.get(&table).cloned().unwrap_or_default()
    }
}
