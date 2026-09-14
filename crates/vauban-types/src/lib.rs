//! Crate `vauban-types`: SQL Server data types, in-memory values, `NULL` and collations.
//!
//! The crate holds the data structures — [`SqlType`], [`Len`], [`TypeFamily`], [`TypeInfo`],
//! [`Value`] with its payload types, [`Collation`] — and the rules of the type system:
//! three-valued comparison, conversions ([`convert`]), arithmetic, literals, precedence and
//! display.
//!
//! The modules are declared here so that each rule lives in its own file and this one
//! stays a table of contents.

mod arith;
pub mod calendar;
pub mod code_page;
mod collation;
mod compare;
mod convert;
mod display;
mod errors;
mod like;
mod literal;
mod precedence;
mod sql_type;
mod value;

pub use arith::*;
pub use collation::Collation;
pub use compare::*;
pub use convert::convert;
pub use display::*;
pub use literal::*;
pub use precedence::*;
pub use sql_type::{Len, SqlType, TypeFamily, TypeInfo};
pub use value::{Date, DateTime, DateTime2, DateTimeOffset, Decimal, SqlString, Time, Value};
