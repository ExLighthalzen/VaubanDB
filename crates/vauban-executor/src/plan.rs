//! Running the bound logical plan: `OneRow`, `Filter`, `Project`, `Limit`, `Scan`.
//!
//! # Materialised
//!
//! [`execute_plan`] is **materialised**: each node hands its parent a whole [`RowSet`]. A
//! `SELECT` without `FROM` yields zero or one row, so materialising costs nothing there and
//! the code reads like the algebra. An `Operator` trait (`open`/`next`/`close`) is not
//! written; when it is, the signature of [`crate::execute`] does not change, and it will
//! stream an `ExecOutcome` instead of carrying a `Vec`.
//!
//! # The shape the binder guarantees
//!
//! ```text
//! OneRow → Filter (WHERE) → Project (select list) → Limit (TOP)
//! ```
//!
//! `Filter` is **under** `Project` and `Limit` **above** it. The executor takes that order
//! as given: if it looks wrong, `query.rs` of the binder is the file to fix. The order is
//! observable and not a matter of taste, because each statement below would raise 8134
//! under the other order:
//!
//! | Query | Answer | Why it distinguishes |
//! |---|---|---|
//! | `SELECT 1 / 0 WHERE 1 = 0;` | 0 rows, **no error** | the select list is not evaluated, so `Filter` runs first |
//! | `SELECT TOP (0) 1 / 0;` | 0 rows, **no error** | the select list is not evaluated, so `Limit` runs first |
//!
//! The second one is why a `Limit` of zero rows does **not** execute its input at all (see
//! [`execute_limit`]): a volcano `Limit` that wants no row does not call `next`, and the
//! materialised form has to reproduce that or it would raise a division error SQL Server
//! does not raise (`tests/execute_select.rs`).
//!
//! # How many rows the client is told about
//!
//! `RowSet.rows.len()` is the number `session` puts in the `DONE` token and in
//! `@@ROWCOUNT`. The executor neither reads nor writes `@@ROWCOUNT`: it counts, the
//! session posts.

use vauban_binder::{BoundExpr, BoundProjection, BoundTop, LogicalPlan};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::Value;

use crate::context::ExecContext;
use crate::errors::at;
use crate::expr::{as_condition, eval_expr};
use crate::row::{Row, RowSet};
use crate::scan::execute_scan;

/// Runs one node of the bound plan and returns the rows it produces.
///
/// The result is materialised on purpose (see the module documentation). The schema of the
/// answer is `plan.schema()`, cloned, for each variant that produces rows — checked on
/// `OneRow`, `Filter`, `Project` and `Limit` by `every_row_matches_its_schema` of
/// `tests/execute_select.rs` and on `Scan` by `scan::tests::scan_empty_table_zero_rows`:
/// `Filter` and `Limit` do not change the shape of their input, `Project` and `Scan`
/// impose their own, and `OneRow` has no column.
///
/// # Errors
///
/// Whatever evaluating an expression raises, propagated unchanged: `eval_expr` has already
/// put the line of the failing sub-expression on it (`crate::errors::at`), and nothing here
/// is deep enough to know better. Plus the `TOP` errors listed on [`execute_limit`], which
/// this file does line itself, and what [`execute_scan`] raises for a `Scan`. `Values`
/// and the relational operators that are not executed yet are the internal error 50000
/// here.
pub(crate) fn execute_plan(plan: &LogicalPlan, ctx: &mut ExecContext<'_>) -> SqlResult<RowSet> {
    match plan {
        // One row of no column: the implicit source of a `SELECT` without `FROM`. Not an
        // empty result — `SELECT 1` answers a row — and not a row of one `NULL` either.
        LogicalPlan::OneRow => Ok(RowSet {
            schema: plan.schema().clone(),
            rows: vec![Row::new()],
        }),
        LogicalPlan::Filter { input, predicate } => execute_filter(input, predicate, ctx),
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let source = execute_plan(input, ctx)?;
            let mut rows = Vec::with_capacity(source.rows.len());
            for row in &source.rows {
                rows.push(project_row(exprs, row, ctx)?);
            }
            Ok(RowSet {
                schema: schema.clone(),
                rows,
            })
        }
        LogicalPlan::Limit { input, top } => execute_limit(input, top, ctx),
        LogicalPlan::Values { .. } => Err(bug(
            "execute_plan: LogicalPlan::Values is not implemented yet",
        )),
        LogicalPlan::Scan {
            table,
            columns,
            schema,
            ..
        } => execute_scan(*table, columns, schema, ctx),
        // The relational operators that are bound but not executed yet: each answers the
        // internal error 50000 until its operator is written.
        LogicalPlan::Join { .. } => Err(bug("execute_plan: Join is not implemented yet")),
        LogicalPlan::Aggregate { .. } => Err(bug("execute_plan: Aggregate is not implemented yet")),
        LogicalPlan::Sort { .. } | LogicalPlan::Distinct(_) => Err(bug(
            "execute_plan: Sort and Distinct are not implemented yet",
        )),
        LogicalPlan::Subquery { .. } => {
            Err(bug("execute_plan: a derived table is not implemented yet"))
        }
        LogicalPlan::SetOp { .. } => Err(bug("execute_plan: SetOp is not implemented yet")),
    }
}

/// Keeps the rows of `input` whose `predicate` is **true**.
///
/// Three-valued logic decides the two other outcomes the same way: `false` and *unknown*
/// both drop the row, because `WHERE` keeps what is true and the unknown is not true.
/// `SELECT 1 WHERE NULL = NULL` therefore answers a result set of one column and **no
/// row**, exactly like `SELECT 1 WHERE 1 = 0` (`tests/execute_select.rs`).
fn execute_filter(
    input: &LogicalPlan,
    predicate: &BoundExpr,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<RowSet> {
    let source = execute_plan(input, ctx)?;
    let mut rows = Vec::with_capacity(source.rows.len());
    for row in source.rows {
        // `as_condition` answers `None` for the unknown of the three-valued logic; only
        // `Some(true)` keeps the row.
        if as_condition(&eval_expr(predicate, Some(&row), ctx)?)? == Some(true) {
            rows.push(row);
        }
    }
    Ok(RowSet {
        schema: source.schema,
        rows,
    })
}

/// Evaluates the select list once for one row of the input.
///
/// One value per [`BoundProjection`], in the order they were written: the `i`-th value of
/// the answer is the `i`-th column of the schema `Project` carries.
fn project_row(
    exprs: &[BoundProjection],
    row: &Row,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Vec<Value>> {
    let mut values = Vec::with_capacity(exprs.len());
    for projection in exprs {
        values.push(eval_expr(&projection.expr, Some(row), ctx)?);
    }
    Ok(values)
}

/// Runs `TOP n [PERCENT]` over `input`.
///
/// The row count is evaluated **once, before the input is read**, and a budget of zero rows
/// short-circuits the input entirely. Both halves are observable, and each statement below
/// answers something else under the opposite rule (`tests/execute_select.rs`):
///
/// | Query | Answer | What it rules out |
/// |---|---|---|
/// | `SELECT TOP (-1) 1 / 0;` | 127 | evaluating the select list first would give 8134 |
/// | `SELECT TOP (-1) 1 WHERE 1 / 0 = 1;` | 127 | evaluating the `WHERE` first would give 8134 |
/// | `SELECT TOP (0) 1 / 0;` | 0 rows, no error | truncating an already projected row would give 8134 |
/// | `SELECT TOP (0) 1 WHERE 1 / 0 = 1;` | 0 rows, no error | same, one operator lower |
/// | `SELECT TOP (0) PERCENT 1 / 0;` | 0 rows, no error | a percentage of zero short-circuits too |
/// | `SELECT TOP (1) 1 / 0;` | 8134 | a non-zero budget does read the input |
///
/// # `PERCENT` rounds **up**
///
/// `ceil(n × p / 100)`, so a percentage above zero keeps the single row of a `SELECT`
/// without `FROM`: over one row, rounding up answers **one** row where truncating would
/// answer no row, and `SELECT TOP (50) PERCENT 1;` answers one. `SELECT TOP (0.0001)
/// PERCENT 1;` answers one row too.
///
/// # Errors
///
/// | Query | SQL Server | Here |
/// |---|---|---|
/// | `SELECT TOP (-1) 1;` | 127, severity 15, state 1 | `SqlError::top_negative` |
/// | `SELECT TOP (CAST(NULL AS int)) 1;` | 1060, severity 15, state 1 | `SqlError::top_null` |
/// | `SELECT TOP (150) PERCENT 1;`, `SELECT TOP (100.5) PERCENT 1;`, `SELECT TOP (-1) PERCENT 1;` | 1031, severity 15, state 1 | internal 50000, see below |
/// | `SELECT TOP (CAST(NULL AS float)) PERCENT 1;` | 1014, severity 15, state 1 | internal 50000, see below |
///
/// `-0.0` is **not** out of range: `SELECT TOP (-0.0) PERCENT 1;` answers zero rows and no
/// error, which the range check `(0.0..=100.0).contains(&percent)` reproduces for free,
/// because `-0.0 == 0.0` in IEEE 754.
///
/// Errors **1031** and **1014** are not raised by this crate: `vauban_errors` has the two
/// constructors, and this file still answers the internal error 50000 for both shapes,
/// which `an_out_of_range_percentage_is_still_an_internal_error` of `tests/execute_select.rs`
/// pins until the two arms are rewritten.
///
/// The bare `NULL` literal does not reach here: `bind_top` refuses `SELECT TOP (NULL) 1;`
/// with 1060 at bind time, because the node *is* the constant. A `NULL` hidden behind an
/// expression is a value, and this is where it is caught.
///
/// # Compile-time checks and the execution fallback
///
/// 127 and 1060 are **compilation** errors on SQL Server: `SELECT 1; SELECT TOP (-1) 1;`
/// answers no result set at all, where `SELECT 1; SELECT 1 / 0;` answers one and then
/// raises. [`crate::compile`] catches foldable invalid values before `session` executes
/// any statement in the batch. This check remains the fallback for values that depend on
/// execution; the error line is the same in both paths.
fn execute_limit(
    input: &LogicalPlan,
    top: &BoundTop,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<RowSet> {
    if top.with_ties {
        // `bind_top` refuses `WITH TIES` without an `ORDER BY` (error 1062) and `Sort` is
        // not executed here, so no plan reaches this arm with rows.
        return Err(bug("execute_limit: TOP … WITH TIES is not implemented yet"));
    }
    let budget = eval_budget(top, ctx)?;
    if budget.is_zero() {
        // The input is not read: see the table above, `SELECT TOP (0) 1 / 0;` raises
        // nothing. The schema is known without executing anything.
        return Ok(RowSet {
            schema: input.schema().clone(),
            rows: Vec::new(),
        });
    }
    let mut set = execute_plan(input, ctx)?;
    let kept = budget.rows_of(set.rows.len());
    set.rows.truncate(kept);
    Ok(set)
}

/// How many rows a `TOP` clause allows through.
///
/// Two variants because the two are not known at the same moment: a row count is final
/// before the input is read, a percentage needs the size of the input. The first one is
/// the one that could be pushed down to a `Scan`; `PERCENT` is a blocking operator.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RowBudget {
    /// `TOP n`: at most `n` rows, whatever the input holds.
    Rows(i64),
    /// `TOP p PERCENT`: `ceil(n × p / 100)` rows out of the `n` the input produces.
    Percent(f64),
}

impl RowBudget {
    /// Whether no row can pass, whatever the input produces.
    ///
    /// True for `TOP 0` and for `TOP 0 PERCENT`: both let the caller skip the input
    /// altogether, as SQL Server does.
    fn is_zero(self) -> bool {
        match self {
            RowBudget::Rows(n) => n == 0,
            // `p` is neither negative nor NaN here: `eval_budget` has already refused both.
            RowBudget::Percent(p) => p == 0.0,
        }
    }

    /// How many of the `n` rows the input produced are kept.
    fn rows_of(self, n: usize) -> usize {
        match self {
            RowBudget::Rows(count) => match usize::try_from(count) {
                Ok(count) => count.min(n),
                // Unreachable on a 64-bit target: `eval_budget` has already refused a
                // negative count, and every other `i64` fits a `usize`. The arm is for a
                // 32-bit one, where a count above `usize::MAX` still cannot truncate a
                // `Vec` that fits in memory, so keeping every row is the right answer.
                Err(_) => n,
            },
            // Rounds **up**, so a percentage above zero keeps the row of a `SELECT`
            // without `FROM`. `min(n)` guards the 100 % case against a rounding artefact.
            RowBudget::Percent(percent) => {
                let kept = (n as f64 * percent / 100.0).ceil();
                if kept >= n as f64 { n } else { kept as usize }
            }
        }
    }
}

/// Evaluates the row count of a `TOP` clause and checks the value SQL Server checks.
///
/// The binder has already inserted the conversion node that brings the expression to
/// `bigint`, or to `float` under `PERCENT` (`bind_top`), so the value read here is of that
/// type; anything else is a bug of the binder. See [`execute_limit`] for the statements
/// behind each error.
fn eval_budget(top: &BoundTop, ctx: &mut ExecContext<'_>) -> SqlResult<RowBudget> {
    let value = eval_expr(&top.expr, None, ctx)?;
    check_budget(top, &value)
}

/// Checks the value of a `TOP` row count, whoever computed it.
///
/// Split out of [`eval_budget`] for [`crate::compile`], which runs the very same check on
/// the value the **compiler** folded, before the plan runs and before the column metadata
/// of the statement goes out. What is valid does not depend on who asks; only the moment
/// of the answer does (`compile.rs`).
pub(crate) fn check_budget(top: &BoundTop, value: &Value) -> SqlResult<RowBudget> {
    // The line of the row count expression, put on 127 and 1060. SQL Server counts these
    // two on the **row count** and not on the statement — the exception `session` spares
    // from `at_statement`: a `SELECT` on line 2 whose `TOP (-1)` is on line 3 answers 3,
    // and the same statement with the row count cut apart (`TOP` on 3, `(` on 4, `-1` on
    // 5) answers 5, the line of the literal (`crate::errors`).
    let line = top.expr.line;
    if top.percent {
        let percent = match value {
            Value::F64(percent) => *percent,
            Value::Null => {
                return Err(bug(
                    "check_budget: error 1014 for a NULL TOP PERCENT value is not implemented \
                     yet",
                ));
            }
            other => return Err(unexpected_top(other, "float")),
        };
        // NaN fails both comparisons, so it is refused with the out-of-range values rather
        // than reaching `is_zero`. Nothing exercises that choice: `float` has no literal
        // spelling for NaN and no expression of this crate computes one.
        if !(0.0..=100.0).contains(&percent) {
            return Err(bug(
                "check_budget: error 1031 for a TOP PERCENT value outside 0 to 100 is not \
                 implemented yet",
            ));
        }
        return Ok(RowBudget::Percent(percent));
    }
    match value {
        Value::I64(count) if *count < 0 => Err(at(SqlError::top_negative(), line)),
        Value::I64(count) => Ok(RowBudget::Rows(*count)),
        Value::Null => Err(at(SqlError::top_null(), line)),
        other => Err(unexpected_top(other, "bigint")),
    }
}

/// The internal error a `TOP` value of the wrong shape raises: the binder promised a type
/// and handed another, which no client input can cause.
fn unexpected_top(value: &Value, expected: &str) -> SqlError {
    bug(&format!(
        "eval_budget: bind_top converts a TOP row count to {expected}, got {value:?}"
    ))
}

/// An engine bug, reported to the client as the generic error 50000.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::RowBudget;

    #[test]
    fn a_row_count_of_zero_skips_the_input() {
        assert!(RowBudget::Rows(0).is_zero());
        assert!(RowBudget::Percent(0.0).is_zero());
        // `SELECT TOP (-0.0) PERCENT 1;` answers zero rows and no error: the negative zero
        // is in range and stops the input like `0`.
        assert!(RowBudget::Percent(-0.0).is_zero());
        assert!(!RowBudget::Rows(1).is_zero());
        assert!(!RowBudget::Percent(0.0001).is_zero());
    }

    #[test]
    fn a_row_count_never_asks_for_more_than_there_is() {
        assert_eq!(RowBudget::Rows(5).rows_of(1), 1);
        assert_eq!(RowBudget::Rows(1).rows_of(1), 1);
        assert_eq!(RowBudget::Rows(0).rows_of(1), 0);
        assert_eq!(RowBudget::Rows(i64::MAX).rows_of(1), 1);
        assert_eq!(RowBudget::Rows(3).rows_of(10), 3);
    }

    #[test]
    fn a_percentage_rounds_up() {
        // Over one row a percentage above zero keeps it: `ceil(1 × 50 / 100) = 1`, where
        // truncating would answer 0 and SQL Server answers 1.
        assert_eq!(RowBudget::Percent(50.0).rows_of(1), 1);
        assert_eq!(RowBudget::Percent(0.0001).rows_of(1), 1);
        assert_eq!(RowBudget::Percent(100.0).rows_of(1), 1);
        assert_eq!(RowBudget::Percent(0.0).rows_of(1), 0);
        // Over several rows, listed here because the arithmetic is written now.
        assert_eq!(RowBudget::Percent(50.0).rows_of(10), 5);
        assert_eq!(RowBudget::Percent(25.0).rows_of(10), 3);
        assert_eq!(RowBudget::Percent(100.0).rows_of(10), 10);
        assert_eq!(RowBudget::Percent(0.0).rows_of(10), 0);
    }
}
