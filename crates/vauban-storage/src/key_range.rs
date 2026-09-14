//! Key ranges and scan directions for [`crate::Storage::seek`].

use std::ops::Bound;

use vauban_types::Value;

/// The set of index keys served by one [`crate::Storage::seek`].
///
/// Values are compared column by column with the same comparator as the index
/// ([`crate::TableShape`], "Key order"). Bounds are expressed **in index order**
/// ([`crate::KeyColumn::descending`] already applied): `lo` is the first key served in
/// [`Direction::Forward`], `hi` the last.
///
/// A key of length `p` shorter than the index key is a *prefix* and designates the set of
/// all keys that start with it, `NULL` equal to `NULL` in every column. A prefix longer than
/// the index key, or a [`Value`] whose variant does not match the [`vauban_types::TypeInfo`]
/// of its column, is a caller bug (`InternalError::Bug`). Not `Eq`: [`Value`] is not.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyRange {
    /// The keys whose first `k.len()` columns equal `k` (`k.len()` may be shorter than the
    /// key: prefix match, `NULL` equal to `NULL`). `Point(vec![])` is equivalent to
    /// [`KeyRange::Full`].
    Point(Vec<Value>),
    /// The keys between two bounds, in index order. `Bound::Included(p)` includes the whole
    /// set of keys starting with the prefix `p`, `Bound::Excluded(p)` excludes that whole
    /// set, `Bound::Unbounded` is accepted on either side.
    Between(Bound<Vec<Value>>, Bound<Vec<Value>>),
    /// The whole index.
    Full,
}

/// Direction of a [`crate::Storage::seek`] relative to the index order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// From the lowest key to the highest, in index order.
    Forward,
    /// From the highest key to the lowest, in index order.
    Backward,
}
