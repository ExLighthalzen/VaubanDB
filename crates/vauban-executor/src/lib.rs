#![deny(missing_docs)]
//! Crate `vauban-executor`: runs a bound statement and produces its rows.
//!
//! # Shape of the executor
//!
//! The executor evaluates scalar expressions and runs a `SELECT` without `FROM` or over
//! one table, plus the DDL of databases, tables and indexes. There is no volcano operator:
//! [`execute`] returns a **materialised** [`RowSet`] instead of writing to a
//! `session::ResultSink`, because `session` depends on `executor`, so `executor` cannot
//! name its types, and `ResultSink` speaks of `tds::ColumnMeta`, which the synchronous
//! engine must not depend on. A `SELECT` without `FROM` yields zero or one row, so
//! materialising costs nothing there. Where `ResultSink` and an `Operator` trait live once
//! millions of rows have to be streamed is an open question.
//!
//! For the same reason [`ExecContext`] holds neither an `Engine` nor a `SessionState`:
//! the two things a scalar evaluation needs from the session already exist without
//! creating a cycle, `sysfn::EvalContext` and `binder::SessionOptions`. The structure
//! carries the engine parts side by side — storage, transaction manager, catalogue and
//! statement snapshot — because `Engine` is a type of `session`. The cancellation token
//! and the batch variables are not carried yet.
//!
//! # Reading a table and running DDL
//!
//! [`LogicalPlan::Scan`](vauban_binder::LogicalPlan::Scan) reads one table over
//! `storage.scan`, still materialised, and a
//! [`ColumnRef`](vauban_binder::BoundExprKind::ColumnRef) is evaluated by position in the
//! row of the node below. A statement that touches no table keeps the scalar shape through
//! [`ExecContext::scalar`].
//!
//! The DDL of databases and tables runs through `catalog`, in the transaction the caller
//! opened ([`ExecContext::with_handle`]), and answers [`ExecOutcome::NoRows`] for it and
//! for a `USE`: neither sends column metadata, and what a `USE` changes belongs to
//! `session`.
//!
//! The two index statements are routed from the same `Ddl` match to `ddl_index.rs`:
//! `CREATE INDEX` hands `catalog.create_index` the `IndexDef` the binder resolved, and
//! `DROP INDEX` turns the name and table it was written with into the identifier
//! `catalog.drop_index` takes. The index of a `PRIMARY KEY` or `UNIQUE` written inside a
//! `CREATE TABLE` is not replayed there: `catalog.create_table` already created it.
//!
//! # File plan
//!
//! | File(s) | Holds |
//! |---|---|
//! | `context.rs`, `row.rs`, this file | the context and the result types |
//! | `expr.rs` | the evaluation of a bound expression |
//! | `statement.rs`, `plan.rs` | the dispatch on a statement and on a plan node |
//! | `convert.rs` | `CAST`, `CONVERT` and their `TRY_` twins |
//! | `pattern.rs` | `LIKE` |
//! | `errors.rs` | the line a runtime error carries |
//! | `compile.rs` | the compile-time checks |
//! | `scan.rs` | reading one table |
//! | `ddl.rs` | the DDL of databases and tables |
//! | `ddl_index.rs` | `CREATE INDEX` and `DROP INDEX` |
//! | `tests/exec_shape.rs` | the names `session` compiles against |

mod compile;
mod context;
mod convert;
mod ddl;
mod ddl_index;
mod errors;
mod expr;
mod pattern;
mod plan;
mod row;
mod scan;
mod statement;

pub use compile::compile;
pub use context::ExecContext;
pub use expr::eval_expr;
pub use row::{ExecOutcome, Row, RowSet};
pub use statement::execute;
