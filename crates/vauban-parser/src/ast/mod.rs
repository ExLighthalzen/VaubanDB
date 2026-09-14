//! The abstract syntax tree, split by family.
//!
//! Every node derives `Debug, Clone, PartialEq, Eq` and **nothing else**: no `Hash`,
//! because [`Span`](crate::Span) compares equal to any other span, and no `Copy` outside
//! the payload-free enumerations. The tree holds almost no logic at all; its behaviour is
//! the `Display` implementation, which lives in `display/`, and the hand-written `Drop` of
//! the three families a batch can nest without bound, which lives in `drop.rs`.
//!
//! The whole tree is re-exported at the crate root, which is the only path other crates
//! use.

pub(crate) mod ddl;
pub(crate) mod drop;
pub(crate) mod expr;
pub(crate) mod proc;
pub(crate) mod query;
pub(crate) mod stmt;
