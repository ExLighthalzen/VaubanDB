//! The identifiers the catalogue hands out.
//!
//! They are not the identifiers of `storage`: a table has both an [`ObjectId`] (what
//! `sys.objects.object_id` shows to a client) and a
//! [`TableId`](vauban_storage::TableId) (what `storage` was given when the table was
//! created). [`TableMeta`](crate::TableMeta) carries the two.

use std::fmt;

/// Identifier of a catalogue object — table, view, constraint, index as an object — unique
/// within its database, as published by `sys.objects.object_id`.
///
/// An `int` in SQL Server, so an `i32` here and not a
/// [`TableId`](vauban_storage::TableId): the two count different things and a table has one
/// of each. Negative values are what SQL Server uses for its own internal objects; nothing
/// in this crate assigns one yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub i32);

/// Identifier of a column within its table, as published by `sys.columns.column_id`.
///
/// An `int` in SQL Server, so an `i32`. It is not the position of the column in a
/// [`Row`](vauban_storage::Row): a dropped column leaves a hole in the identifiers, not in
/// the row, which is why [`ColumnMeta`](crate::ColumnMeta) carries `ordinal` as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnId(pub i32);

impl fmt::Display for ObjectId {
    /// Writes the bare decimal integer, without the type name (`ObjectId(7)` gives `7`),
    /// like the identifiers of `storage` (unit test `object_id_displays_as_decimal`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for ColumnId {
    /// Writes the bare decimal integer, without the type name (`ColumnId(7)` gives `7`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn object_id_displays_as_decimal() {
        assert_eq!(ObjectId(7).to_string(), "7");
        assert_eq!(ColumnId(7).to_string(), "7");
        assert_eq!(ObjectId(-1).to_string(), "-1");
        assert_eq!(ObjectId(i32::MAX).to_string(), i32::MAX.to_string());
        // `Debug` keeps the type name; `Display` is bare.
        assert_eq!(format!("{:?}", ObjectId(7)), "ObjectId(7)");
    }

    #[test]
    fn ids_are_copy_ordered_and_hashable() {
        let id = ObjectId(3);
        let copy = id;
        assert_eq!(id, copy);
        assert!(ObjectId(3) < ObjectId(4));
        assert!(ColumnId(1) < ColumnId(2));
        let set: HashSet<ObjectId> = [ObjectId(1), ObjectId(1), ObjectId(2)]
            .into_iter()
            .collect();
        assert_eq!(set.len(), 2);
    }
}
