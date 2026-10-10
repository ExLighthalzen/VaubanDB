//! The rows an execution produces, and what a statement answers.

use vauban_binder::OutputSchema;
use vauban_errors::SqlError;
use vauban_storage::RowId;
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
///
/// A row of a plan that asked for row identifiers carries them after those values, as a
/// tail no expression reads ([`vauban_planner::row_id_binding`]): the tail of a node is
/// `PhysicalPlan::row_id_count` values long, and empty for a plan that did not ask for it
/// (`tests/row_id.rs`, `a_scan_asked_nothing_keeps_its_width`).
pub type Row = Vec<Value>;

/// The value a [`RowId`] takes in the tail of a row: a `bigint` holding the same 64 bits,
/// so that reading them back gives the identifier unchanged (`row/tests/mod.rs`,
/// `a_row_id_round_trips_through_its_value`). The `NULL` of a side a join padded names no
/// row.
pub(crate) fn row_id_value(id: RowId) -> Value {
    Value::I64(i64::from_ne_bytes(id.0.to_ne_bytes()))
}

/// Splits `row` into its visible values and its tail of `tail` row identifiers. A row
/// shorter than its tail is taken as a tail and nothing else, which takes a bug of the node
/// that built it.
fn split_tail(row: &[Value], tail: usize) -> (&[Value], &[Value]) {
    row.split_at(row.len().saturating_sub(tail))
}

/// One side of a join, as [`join_rows`] takes it: the row, or `None` for a side padded
/// with `NULL`, then the visible width and the tail length of that side.
pub(crate) struct JoinSide<'r> {
    /// The row of the side, `None` when the join pads it.
    pub(crate) row: Option<&'r [Value]>,
    /// How many visible values a padded row of the side has.
    pub(crate) width: usize,
    /// How many row identifiers the rows of the side carry.
    pub(crate) tail: usize,
}

/// The row a join hands over for a pair of sides: the visible values of the left side,
/// those of the right, then the tail of the left and the tail of the right. A padded side
/// gives `NULL` for each of its visible values and for each of its row identifiers
/// (`tests/row_id.rs`, `a_left_join_pads_the_row_id_of_the_missing_side`).
pub(crate) fn join_rows(left: &JoinSide<'_>, right: &JoinSide<'_>) -> Row {
    let (left_values, left_tail) = left.row.map_or((None, None), |row| {
        let (values, tail) = split_tail(row, left.tail);
        (Some(values), Some(tail))
    });
    let (right_values, right_tail) = right.row.map_or((None, None), |row| {
        let (values, tail) = split_tail(row, right.tail);
        (Some(values), Some(tail))
    });
    let mut out = Row::with_capacity(left.width + right.width + left.tail + right.tail);
    extend_or_null(&mut out, left_values, left.width);
    extend_or_null(&mut out, right_values, right.width);
    extend_or_null(&mut out, left_tail, left.tail);
    extend_or_null(&mut out, right_tail, right.tail);
    out
}

/// Appends `values`, or `count` `NULL`s for a side without a row.
fn extend_or_null(out: &mut Row, values: Option<&[Value]>, count: usize) {
    match values {
        Some(values) => out.extend_from_slice(values),
        None => out.extend(std::iter::repeat_n(Value::Null, count)),
    }
}

/// The tail of `row`: its last `tail` values.
pub(crate) fn tail_of(row: &[Value], tail: usize) -> &[Value] {
    split_tail(row, tail).1
}

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
/// control flow (`Return`, `Break`, `Continue`, `Goto`) are declared for the statements that
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
    /// `GOTO`, with the label it named: the statement stops, and so does each statement
    /// that holds it, up to the caller that owns the list of statements and moves to the
    /// label (`tests/control.rs`, `a_goto_in_a_block_stops_the_block`).
    Goto(String),
    /// The caller raised the cancellation token and the statement stopped.
    Cancelled,
    /// An error whose scope is the whole batch.
    BatchAbort(SqlError),
    /// A stored procedure call, with its arguments evaluated. The session resolves the
    /// name and runs the call.
    CallProcedure {
        /// Normalised procedure name, or the value of a procedure variable, lower-cased.
        name: String,
        /// Database part written left of the procedure name, absent when the statement did
        /// not write one. The name keeps the spelling of the statement; the caller resolves
        /// it (`tests/execute.rs`, `exec_of_a_three_part_name_carries_the_database`).
        database: Option<String>,
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

#[cfg(test)]
mod tests;
