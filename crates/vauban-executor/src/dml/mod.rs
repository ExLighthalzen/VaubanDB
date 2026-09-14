//! The write statements: `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE TABLE`, and what they
//! share.
//!
//! | File | Holds |
//! |---|---|
//! | `insert.rs` | `INSERT`: default values, `IDENTITY`, the identity functions |
//! | `update_delete.rs` | `UPDATE` and `DELETE`: locating the rows, the write conflicts, the spool |
//! | `assign.rs` | the assignment of a value to a column, shared by `INSERT` and `UPDATE` |
//! | `constraints.rs` | `PRIMARY KEY`, `UNIQUE` and `NOT NULL` at write time |
//! | `fk_check.rs` | `FOREIGN KEY` and `CHECK` at write time |
//! | `truncate.rs` | `TRUNCATE TABLE` |
//!
//! The statements are not written yet: the entry points of `insert.rs` and
//! `update_delete.rs` answer the internal error 50000, and the other files hold their
//! documentation alone.

use vauban_errors::{InternalError, SqlError};

pub(crate) mod assign;
pub(crate) mod constraints;
pub(crate) mod fk_check;
pub(crate) mod insert;
pub(crate) mod truncate;
pub(crate) mod update_delete;

/// The internal error 50000 a statement answers until it is written.
pub(crate) fn not_implemented(statement: &str) -> SqlError {
    SqlError::from(InternalError::Bug(format!(
        "execute: {statement} is not implemented yet"
    )))
}
