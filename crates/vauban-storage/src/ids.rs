//! Identifier newtypes handed out by the storage layer and by the transaction manager.
//!
//! All of them are plain integers with a `Display` that writes the **bare decimal integer**
//! (`TableId(7)` displays as `7`). The duplicate-key error emitted by an implementation
//! uses this `Display` in place of the object names, which lets the caller identify the
//! violated index and rephrase the error with the catalogue names.

use std::fmt;

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident($inner:ty)) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub $inner);

        impl fmt::Display for $name {
            /// Writes the bare decimal integer, without the type name.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

id_newtype! {
    /// Identifier of a database, unique within the instance, assigned by
    /// [`Storage::create_database`](crate::Storage::create_database).
    DbId(u32)
}

id_newtype! {
    /// Identifier of a table, unique within the **whole instance** (not per database),
    /// assigned by [`Storage::create_table`](crate::Storage::create_table). This is why
    /// [`Storage::drop_table`](crate::Storage::drop_table) takes no database parameter.
    TableId(u32)
}

id_newtype! {
    /// Identifier of an index, unique within the **whole instance** (not per table),
    /// assigned by [`Storage::create_index`](crate::Storage::create_index).
    IndexId(u32)
}

id_newtype! {
    /// Identifier of a logical row, unique within its table, never reused after a delete or
    /// a vacuum, and stable for the whole life of the row in every implementation.
    RowId(u64)
}

id_newtype! {
    /// Identifier of a transaction, assigned by the `txn` module, strictly increasing over
    /// time. The storage layer never creates one.
    TxnId(u64)
}

id_newtype! {
    /// Identifier of a savepoint, assigned by
    /// [`Storage::savepoint`](crate::Storage::savepoint), increasing within its transaction
    /// and meaningful only for that transaction.
    SavepointId(u64)
}

#[cfg(test)]
mod tests;
