//! The rows an execution produces, and what a statement answers.

use vauban_binder::OutputSchema;
use vauban_errors::SqlError;
use vauban_types::Value;

/// One row: one [`Value`] per column of the [`OutputSchema`] that describes it.
///
/// An alias and not a structure: a row is passed around by slice, and
/// [`crate::eval_expr`] receives one for each column reference it evaluates.
pub type Row = Vec<Value>;

/// A materialised result set: the schema the client is sent, then the rows.
///
/// What [`execute_collect`](crate::execute_collect) answers: the rows a statement handed
/// to its sink, kept in a `Vec` for the callers that read them after the fact, `session`
/// and the tests. Each row has `schema.columns.len()` values, in the same order.
#[derive(Debug, Clone)]
pub struct RowSet {
    /// The columns, in the order the client receives them.
    pub schema: OutputSchema,
    /// The rows, each of the width of `schema`.
    pub rows: Vec<Vec<Value>>,
}

/// What one executed statement answers.
///
/// A `SELECT` produces `Rows`, with the number of rows the sink received. `NoRows` is
/// the answer of the statements without a result set (`USE`, DDL). The outcomes of the
/// control flow (`Return`, `Break`, `Continue`) are declared for the statements that
/// produce them; `Cancelled` is what a statement answers once the caller raised its
/// token, and it is not an error: no number is attached to it. `BatchAbort` carries an
/// error that ends the batch, not the statement alone.
#[derive(Debug, Clone)]
pub enum ExecOutcome {
    /// The statement produced a result set of this many rows.
    Rows(u64),
    /// The statement produced no result set at all.
    NoRows,
    /// `RETURN`, with its value.
    Return(i32),
    /// `BREAK`, out of the enclosing `WHILE`.
    Break,
    /// `CONTINUE`, to the next iteration of the enclosing `WHILE`.
    Continue,
    /// The caller raised the cancellation token and the statement stopped.
    Cancelled,
    /// An error whose scope is the whole batch.
    BatchAbort(SqlError),
}
