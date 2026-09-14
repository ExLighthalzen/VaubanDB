//! The rows an execution produces, and what a statement answers.

use vauban_binder::OutputSchema;
use vauban_types::Value;

/// One row: one [`Value`] per column of the [`OutputSchema`] that describes it.
///
/// An alias and not a structure: a row is passed around by slice, and
/// [`crate::eval_expr`] receives one for each column reference it evaluates.
pub type Row = Vec<Value>;

/// A materialised result set: the schema the client is sent, then the rows.
///
/// Materialised on purpose (see the crate documentation): `session` turns it into
/// `columns`/`row`/`done`. Each row has `schema.columns.len()` values, in the same order.
#[derive(Debug, Clone)]
pub struct RowSet {
    /// The columns, in the order the client receives them.
    pub schema: OutputSchema,
    /// The rows, each of the width of `schema`.
    pub rows: Vec<Vec<Value>>,
}

/// What one executed statement answers.
///
/// A `SELECT` produces `Rows`. `NoRows` is the answer of the statements without a result
/// set (`USE`, DDL); the outcomes of the control flow (`Return`, `Break`, `Continue`) are
/// not represented yet.
#[derive(Debug, Clone)]
pub enum ExecOutcome {
    /// The statement produced a result set.
    Rows(RowSet),
    /// The statement produced no result set at all.
    NoRows,
}
