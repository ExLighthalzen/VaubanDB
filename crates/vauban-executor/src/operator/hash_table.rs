//! The hash table the hashing operators share: hash join, hash aggregate, `DISTINCT`,
//! `COUNT(DISTINCT …)`, `EXCEPT` and `INTERSECT`.
//!
//! A row is looked up by its **key**: the sort keys of its key columns written end to end
//! ([`vauban_types::sort_key`]), each column under the collation of its type. Two keys are
//! equal exactly when `types::compare` finds each pair of columns equal, so the table
//! groups `'x'` and `'X'` under a case-insensitive collation, `1.0` and `1.00`, `-0.0` and
//! `0.0`, `0x01` and `0x0100` (`operator::hash_table::tests`). `NULL` has a key of its own,
//! equal to the key of another `NULL`: grouping, `DISTINCT` and the set operators put the
//! `NULL`s together; a hash join, where `NULL` matches nothing, drops a row with a `NULL`
//! key before it asks for one.
//!
//! The table compares whole keys, not hashes: two keys whose hashes collide stay two
//! entries, at the cost of one comparison of byte strings.
//!
//! The table lives in memory; spilling to disk is not implemented.

use std::collections::HashMap;

use vauban_errors::SqlResult;
use vauban_types::{Collation, TypeInfo, Value, sort_key};

/// Writes the key of a row: one collation per key column, the buffer reused between rows.
pub(crate) struct KeyEncoder {
    collations: Vec<Collation>,
    buffer: Vec<u8>,
}

impl KeyEncoder {
    /// An encoder for key columns of the types `types`, in order. A type without a
    /// collation (a number, a date) keys its values under the default collation, which
    /// the key of a value that is not a string does not read.
    pub(crate) fn new<'t>(types: impl IntoIterator<Item = &'t TypeInfo>) -> Self {
        Self {
            collations: types
                .into_iter()
                .map(|ty| ty.collation.unwrap_or(Collation::DEFAULT))
                .collect(),
            buffer: Vec::new(),
        }
    }

    /// The key of `values`, the key columns of one row, in the order of the types given
    /// to [`KeyEncoder::new`]. The slice borrows the encoder until the next call.
    ///
    /// # Errors
    ///
    /// The error `types::sort_key` reports for a value that has no key (a decimal whose
    /// scale is beyond 38).
    pub(crate) fn encode<'v>(
        &mut self,
        values: impl IntoIterator<Item = &'v Value>,
    ) -> SqlResult<&[u8]> {
        self.buffer.clear();
        for (value, collation) in values.into_iter().zip(&self.collations) {
            sort_key(value, collation, &mut self.buffer)?;
        }
        Ok(&self.buffer)
    }
}

/// A map from keys to values of type `V`, which hands out the values in the order their
/// keys were first inserted (`operator::hash_table::tests::values_come_out_in_first_insertion_order`).
pub(crate) struct HashTable<V> {
    slots: HashMap<Box<[u8]>, usize>,
    values: Vec<V>,
}

impl<V> HashTable<V> {
    /// An empty table.
    pub(crate) fn new() -> Self {
        Self {
            slots: HashMap::new(),
            values: Vec::new(),
        }
    }

    /// The index of the value stored under `key`, if there is one.
    pub(crate) fn find(&self, key: &[u8]) -> Option<usize> {
        self.slots.get(key).copied()
    }

    /// The value at `index`, an index [`HashTable::find`] or [`HashTable::entry`] gave.
    pub(crate) fn get(&self, index: usize) -> Option<&V> {
        self.values.get(index)
    }

    /// The value stored under `key`, created by `make` when the key is new. The flag is
    /// `true` when the value was created by this call.
    ///
    /// # Errors
    ///
    /// What `make` reports; the table is unchanged then.
    pub(crate) fn entry(
        &mut self,
        key: &[u8],
        make: impl FnOnce() -> SqlResult<V>,
    ) -> SqlResult<(&mut V, bool)> {
        if let Some(&index) = self.slots.get(key) {
            return Ok((&mut self.values[index], false));
        }
        let value = make()?;
        let index = self.values.len();
        self.values.push(value);
        self.slots.insert(Box::from(key), index);
        Ok((&mut self.values[index], true))
    }

    /// Whether the table holds no key.
    pub(crate) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The values, in the order their keys were first inserted.
    pub(crate) fn into_values(self) -> Vec<V> {
        self.values
    }
}

/// A set of keys.
pub(crate) struct KeySet {
    table: HashTable<()>,
}

impl KeySet {
    /// An empty set.
    pub(crate) fn new() -> Self {
        Self {
            table: HashTable::new(),
        }
    }

    /// Adds `key`; `true` when it was not in the set yet.
    pub(crate) fn insert(&mut self, key: &[u8]) -> bool {
        if self.table.find(key).is_some() {
            return false;
        }
        let _ = self.table.entry(key, || Ok(()));
        true
    }

    /// Whether `key` is in the set.
    pub(crate) fn contains(&self, key: &[u8]) -> bool {
        self.table.find(key).is_some()
    }
}

#[cfg(test)]
mod tests;
