#![deny(missing_docs)]
//! Crate `vauban-executor`: runs a physical statement and hands its rows to a sink.
//!
//! # Shape of the executor
//!
//! [`execute`] takes the [`PhysicalStatement`](vauban_planner::PhysicalStatement) the
//! planner built, runs it against an [`ExecContext`], and hands each row of a query to a
//! [`RowSink`] as the operator tree produces it: `columns` once, `row` per row, `info`
//! for the messages the statement queued. The tree is made of [`Operator`]s, one per node
//! of the physical plan, opened once, pulled row by row and closed once
//! ([`build_operator`], `operator.rs`).
//!
//! [`RowSink`] is defined here and implemented by `session`, because `session` depends on
//! this crate and its own sink speaks of `tds::ColumnMeta`, which the synchronous engine
//! must not depend on. For the same reason the state of the session the executor writes,
//! variables, `@@ROWCOUNT`, `@@TRANCOUNT`, the open transaction, is an [`ExecSession`]
//! defined here and lent through the context, and [`ExecContext`] holds neither an
//! `Engine` nor a `SessionState`: it carries the engine parts side by side, storage,
//! transaction manager, catalogue and statement snapshot.
//!
//! [`execute_collect`] runs a statement with a [`CollectSink`] and answers the rows as a
//! materialised [`RowSet`]: what the tests read, and what `session` streams from until
//! its own sink implements [`RowSink`].
//!
//! # Cancellation
//!
//! Cooperative: the caller raises a [`CancelToken`] the context carries, and the driving
//! loop reads it before the tree is opened, every 1024 rows and once the tree is
//! exhausted (`statement.rs`). A cancelled statement answers
//! [`ExecOutcome::Cancelled`]; no error number is attached to it.
//!
//! # Reading a table and running DDL
//!
//! [`TableScan`](vauban_planner::PhysicalPlan::TableScan) reads one table over
//! `storage.scan`, one row at a time (`ops/scan.rs`), and a
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
//! | `context.rs`, `row.rs`, this file | the context, the token, the session state, the sinks, the result types |
//! | `operator.rs` | the `Operator` trait, `build_operator`, the hashing helpers |
//! | `ops/` | one file per operator (`ops/mod.rs`) |
//! | `expr.rs` | the evaluation of a bound expression |
//! | `statement.rs` | the dispatch on a statement and the driving loop of a query |
//! | `plan.rs` | the row budget of a `TOP` |
//! | `convert.rs` | `CAST`, `CONVERT` and their `TRY_` twins |
//! | `pattern.rs` | `LIKE` |
//! | `errors.rs` | the line a runtime error carries |
//! | `compile.rs` | the compile-time checks |
//! | `ddl.rs` | the DDL of databases and tables |
//! | `ddl_index.rs` | `CREATE INDEX` and `DROP INDEX` |
//! | `ddl_options.rs`, `ddl_alter.rs` | `ALTER DATABASE ... SET`, `ALTER TABLE` |
//! | `dml/` | `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE TABLE` (`dml/mod.rs`) |
//! | `control.rs` | variables and the control of flow |
//! | `execute.rs` | `EXECUTE`: arguments evaluated, hand-off to the session |
//! | `txn_exec.rs` | the transaction statements and the frame around each statement |
//! | `locking.rs` | reading and writing under locks |
//! | `tests/exec_shape.rs` | the names `session` compiles against |

mod compile;
mod context;
mod control;
mod convert;
mod ddl;
mod ddl_alter;
mod ddl_index;
mod ddl_options;
mod dml;
mod errors;
mod execute;
mod expr;
mod locking;
mod operator;
pub mod ops;
mod pattern;
mod plan;
mod row;
mod statement;
pub mod txn_exec;

pub use compile::compile;
pub use context::{CancelToken, CollectSink, ExecContext, ExecSession, RowSink};
pub use expr::eval_expr;
pub use operator::{Operator, bucket_hash, build_operator, keys_equal};
pub use row::{EvaluatedExecArg, ExecOutcome, Row, RowSet};
pub use statement::{execute, execute_collect};

use vauban_binder::BoundExecute;
use vauban_errors::SqlResult;

/// Evaluates one bound `EXECUTE` and answers what the session runs next.
///
/// The entry point until the planner carries `Execute` on [`PhysicalStatement`]: nothing
/// is handed to `sink`.
///
/// # Errors
///
/// As [`execute`].
pub fn execute_bound(
    stmt: &BoundExecute,
    ctx: &mut ExecContext<'_>,
    _sink: &mut dyn RowSink,
) -> SqlResult<ExecOutcome> {
    txn_exec::begin_statement(ctx)?;
    let result = execute::execute(stmt, ctx);
    txn_exec::end_statement(ctx, &result)?;
    result
}
