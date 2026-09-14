//! The row budget of a `TOP`: how many rows a `TOP n [PERCENT]` lets through, and the
//! checks SQL Server runs on that number.
//!
//! The budget is read at two moments by two callers, and this file holds the one rule
//! they share: [`crate::compile`] checks the value the compiler folds, before any row is
//! produced; the `Top` operator (`ops/limit.rs`) checks the value it evaluates when it
//! opens. What is valid does not depend on who asks; only the moment of the answer does.
//!
//! # Errors
//!
//! | Query | SQL Server | Here |
//! |---|---|---|
//! | `SELECT TOP (-1) 1;` | 127, severity 15, state 1 | `SqlError::top_negative` |
//! | `SELECT TOP (CAST(NULL AS int)) 1;` | 1060, severity 15, state 1 | `SqlError::top_null` |
//! | `SELECT TOP (150) PERCENT 1;`, `SELECT TOP (100.5) PERCENT 1;`, `SELECT TOP (-1) PERCENT 1;` | 1031, severity 15, state 1 | internal 50000, see below |
//! | `SELECT TOP (CAST(NULL AS float)) PERCENT 1;` | 1014, severity 15, state 1 | internal 50000, see below |
//!
//! `-0.0` is **not** out of range: `SELECT TOP (-0.0) PERCENT 1;` answers zero rows and no
//! error, which the range check `(0.0..=100.0).contains(&percent)` reproduces for free,
//! because `-0.0 == 0.0` in IEEE 754.
//!
//! Errors **1031** and **1014** are not raised by this crate: `vauban_errors` has the two
//! constructors, and this file still answers the internal error 50000 for both shapes,
//! which `an_out_of_range_percentage_is_still_an_internal_error` of `tests/execute_select.rs`
//! pins until the two arms are rewritten.
//!
//! The bare `NULL` literal does not reach here: `bind_top` refuses `SELECT TOP (NULL) 1;`
//! with 1060 at bind time, because the node *is* the constant. A `NULL` hidden behind an
//! expression is a value, and this is where it is caught.
//!
//! # Compile-time checks and the execution fallback
//!
//! 127 and 1060 are **compilation** errors on SQL Server: `SELECT 1; SELECT TOP (-1) 1;`
//! answers no result set, where `SELECT 1; SELECT 1 / 0;` answers one and then
//! raises. [`crate::compile`] catches foldable invalid values before `session` executes
//! any statement in the batch. [`eval_budget`] remains the fallback for values that depend
//! on execution; the error line is the same in both paths.

use vauban_binder::BoundTop;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::Value;

use crate::context::ExecContext;
use crate::errors::at;
use crate::expr::eval_expr;

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
    pub(crate) fn is_zero(self) -> bool {
        match self {
            RowBudget::Rows(n) => n == 0,
            // `p` is neither negative nor NaN here: `eval_budget` has already refused both.
            RowBudget::Percent(p) => p == 0.0,
        }
    }

    /// How many of the `n` rows the input produced are kept.
    pub(crate) fn rows_of(self, n: usize) -> usize {
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
/// type; anything else is a bug of the binder. The module documentation lists the
/// statements behind each error.
pub(crate) fn eval_budget(top: &BoundTop, ctx: &mut ExecContext<'_>) -> SqlResult<RowBudget> {
    let value = eval_expr(&top.expr, None, ctx)?;
    check_budget(top, &value)
}

/// Checks the value of a `TOP` row count, whoever computed it.
///
/// Split out of [`eval_budget`] for [`crate::compile`], which runs the very same check on
/// the value the **compiler** folded, before the plan runs and before the column metadata
/// of the statement goes out (`compile.rs`).
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
