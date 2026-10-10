//! The version of the shape of the internal tables of `master`.
//!
//! `storage` keeps the column types of a table and not the description the catalogue reads it
//! through, so a data directory written by a binary whose internal tables had other columns
//! would open, and fail later, at the first view that reads a column it lacks. A first start
//! writes [`CATALOG_SHAPE_VERSION`] in the registry of the internal tables, under the name
//! [`SHAPE_VERSION_ENTRY`]; a later start reads it back and refuses a directory whose
//! number differs or is missing, before the server binds its port ([`check`]; test
//! `a_directory_of_another_catalogue_shape_is_refused_before_listening` of `vauban-cli`).
//! The catalogue does not migrate: such a directory is opened by the binary that wrote it.
//!
//! The entry is a row of the registry whose `table_id` column carries the number rather than
//! a [`TableId`](vauban_storage::TableId); the readers of the registry skip it by its name.

use vauban_errors::InternalError;

#[cfg(doc)]
use crate::def::InternalTableDef;

/// The shape of the internal tables this binary reads and writes.
///
/// Increase it with a change to an [`InternalTableDef`] that a data directory keeps: a
/// table added, removed or renamed, a column added, removed, renamed, retyped or reordered,
/// a clustered key changed, or a change to the registry itself. The unit test
/// `the_shape_version_follows_the_internal_table_definitions` pins this number to a
/// fingerprint of the current definitions (their names, columns and clustered keys) and fails when the definitions change
/// and the pin does not; the change then adds a line to its list with the next number.
/// The rows a description carries and the views built on it stay out of that fingerprint.
pub(crate) const CATALOG_SHAPE_VERSION: i64 = 1;

/// The name of the row of the registry that carries the shape version.
///
/// Outside the `vauban_sys_*` names the internal tables take.
pub(crate) const SHAPE_VERSION_ENTRY: &str = "vauban_catalog_shape_version";

/// Compares the shape version a data directory carries with [`CATALOG_SHAPE_VERSION`].
///
/// `found` is `None` for a directory written before the number existed.
///
/// # Errors
///
/// [`InternalError::Corruption`] naming the version found and the version expected, when
/// they differ (unit tests `a_different_version_is_refused_with_both_numbers` and
/// `a_missing_version_is_refused`).
pub(crate) fn check(found: Option<i64>) -> Result<(), InternalError> {
    if found == Some(CATALOG_SHAPE_VERSION) {
        return Ok(());
    }
    let found = found.map_or_else(|| "none".to_owned(), |version| version.to_string());
    Err(InternalError::Corruption(format!(
        "the data directory holds catalogue shape version {found}, this binary expects version \
         {CATALOG_SHAPE_VERSION}; catalogue shapes are not migrated, open it with the binary \
         that created it or start on a new data directory"
    )))
}

#[cfg(test)]
mod tests;
