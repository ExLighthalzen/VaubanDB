#![deny(missing_docs)]
//! Crate `vauban-errors`: the single error type sent to SQL Server clients, its
//! `Result` alias, the informational message type and the internal error type.
//!
//! Every error that reaches a client is a [`SqlError`] carrying a number, a severity and a
//! state, as SQL Server does. Errors that are not meant to
//! reach a client as-is (bugs, I/O failures) are [`InternalError`]s, converted to a generic
//! `SqlError` at the boundary. Low-severity notices (`PRINT`, textual ENVCHANGE) are
//! [`InfoMessage`]s.
//!
//! The error catalogue (number → default severity, message template) is
//! [`message_template`]. The named constructors (`SqlError::invalid_object_name("dbo.t")`,
//! …) live in `constructors`; they are the only way other crates turn a template into a
//! final message, through the crate-private substitution of `format`.

mod catalog;
mod constructors;
mod format;
mod info;
mod internal;
mod sql_error;

pub use catalog::{BatchErrorScope, ErrorDef, message_template};
pub use info::InfoMessage;
pub use internal::InternalError;
pub use sql_error::{SqlError, SqlResult};
