//! The rows an execution produces, and what a statement answers.

use vauban_binder::OutputSchema;
use vauban_errors::SqlError;
use vauban_types::{TypeInfo, Value};

/// One argument of an `EXECUTE`, evaluated and ready for the session.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluatedExecArg {
    /// The parameter name for a named argument; `None` for a positional one.
    pub name: Option<String>,
    /// The value and its type, or `None` for `DEFAULT`.
    pub value: Option<(Value, TypeInfo)>,
    /// `OUTPUT` was written on this argument.
    pub output: bool,
    /// For an `OUTPUT` argument, the caller's variable (`@o` in `@o OUTPUT`).
    pub output_variable: Option<String>,
}

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
    /// A stored procedure call, with its arguments evaluated. The session resolves the
    /// name and runs the call.
    CallProcedure {
        /// Normalised procedure name, or the value of a procedure variable, lower-cased.
        name: String,
        /// Arguments in written order.
        args: Vec<EvaluatedExecArg>,
        /// The `@rc =` variable receiving the return status, when the form was written.
        return_into: Option<String>,
        /// Line of the statement.
        line: u32,
    },
    /// T-SQL text to run dynamically. The session parses and executes it.
    RunDynamic {
        /// The text to run.
        text: String,
        /// Line of the statement.
        line: u32,
    },
}
