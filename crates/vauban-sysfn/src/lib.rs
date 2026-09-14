//! Crate `vauban-sysfn`: registry of the T-SQL built-in functions.
//!
//! The `binder` consults the registry to type a function call, the `executor` to evaluate
//! it. The foundation is the definition of a function ([`FunctionDef`]), the
//! case-insensitive global registry ([`register`], [`lookup`], [`all`]), and the
//! [`EvalContext`] contract through which a function reaches the session (clock,
//! `@@ROWCOUNT`, SPID, current database, catalogue lookups) without this crate depending
//! on it. Evaluation receives an [`EvalArgs`], which carries the argument values together
//! with their declared types and the result type of the call, because a `Value` alone says
//! nothing of its length, family or collation. The registry starts empty and is filled by
//! [`register_builtins`] and by the `compat` crate (`@@VERSION`, `SERVERPROPERTY`).
//!
//! The functions of this crate live in `builtins`, one submodule per family, and are all
//! registered by [`register_builtins`], which a server calls once at start-up.
//! [`check_call`] is what the `binder` calls to type one call: it checks the number of
//! arguments, then asks the function for its result type. An unknown *name* is not this
//! crate's business: [`lookup`] answers `None` and the `binder` raises error 195.
//!
//! Terminology (scalar vs aggregate, deterministic) follows Microsoft Learn,
//! "Built-in functions (Transact-SQL)".

mod builtins;
mod context;
mod registry;

pub use builtins::{DatePart, check_call, parse_datepart, register_builtins};
pub use context::{EvalContext, StaticContext};
pub use registry::{
    AggregateFactory, AggregateState, Arity, EvalArgs, FunctionDef, FunctionKind, all, lookup,
    register,
};
