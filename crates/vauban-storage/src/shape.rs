//! Shape metadata of tables and indexes: column types, keys, uniqueness.
//!
//! The storage layer knows the *shape* of a table (number and types of columns, clustered
//! key) and of an index (key columns, uniqueness, included columns), nothing more: no
//! names, no constraints other than uniqueness, no defaults. Those belong to the catalogue.

use vauban_types::TypeInfo;

/// One column of a key (clustered key or index key) and its sort direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyColumn {
    /// Position of the column in [`TableShape::columns`], zero-based.
    pub column: u16,
    /// `true` reverses the order of this column, `NULL` included: `NULL` sorts first on an
    /// ascending column and last on a descending one.
    pub descending: bool,
}

/// Shape of a table: the type of each column and the optional clustered key.
///
/// # Key order
///
/// Wherever this crate orders keys (clustered key, index key, [`crate::KeyRange`] bounds),
/// the order of one column is: `NULL` first (handled by `storage` itself, `compare` returning
/// `Ok(None)`), then `vauban_types::compare(a, b, &collation)` (signature
/// `SqlResult<Option<Ordering>>`) with the collation of the column's
/// [`TypeInfo`] ([`vauban_types::Collation::DEFAULT`] when it is `None`).
/// [`KeyColumn::descending`] reverses the whole thing, `NULL` included. Composite keys are
/// compared column by column. Equal keys are ordered by increasing [`crate::RowId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableShape {
    /// Type of each column, in position order. Never empty. A [`crate::Row`] of this table
    /// has exactly `columns.len()` values.
    pub columns: Vec<TypeInfo>,
    /// Clustered key, if any. Defines the order of [`crate::Storage::scan`] and the physical
    /// placement on disk. It does **not** imply uniqueness (SQL Server adds an uniquifier);
    /// uniqueness is declared with a `unique` [`IndexShape`]. A clustered key is not an
    /// index: to search it with [`crate::Storage::seek`], the caller creates an `IndexShape`
    /// whose `columns` equal `clustered_key` (with `unique: true` for a primary key). An
    /// implementation is free to serve such an index from the clustered tree without any
    /// additional structure. Every `column` is `< columns.len()`.
    pub clustered_key: Option<Vec<KeyColumn>>,
}

/// Shape of an index on one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexShape {
    /// Key columns, in key order. Never empty; every `column` is `< columns.len()` of the
    /// table. Ordered as described in [`TableShape`] ("Key order").
    pub columns: Vec<KeyColumn>,
    /// `true` forbids two *live* rows with the same key. For this test, `NULL` equals `NULL`
    /// in every column, so `(1, NULL)` can exist only once, like in a SQL Server unique index
    /// (unlike the `=` predicate). See [`crate::Storage`] ("Duplicate keys") for the
    /// definition of "live" and the error returned.
    pub unique: bool,
    /// Positions of the non-key columns stored in the index leaves. Stored as given, with no
    /// effect on semantics (an on-disk optimisation).
    pub included: Vec<u16>,
}
