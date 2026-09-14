//! `x [NOT] LIKE p [ESCAPE e]`: the three-valued logic and the `ESCAPE` clause.
//!
//! The matching itself is **not** here. `Collation::like` is the one implementation of
//! `%`, `_`, `[abc]`, `[a-c]`, `[^abc]` and of the escape character, and it is the place a
//! wrong answer about a pattern has to be fixed. What this module owns is the branch
//! around it: which of the three operands may be `NULL`, what a `NULL` means for each of
//! them, when the `ESCAPE` clause is rejected, which collation the match runs under, and
//! how `negated` folds into the answer. The behaviour below is pinned by
//! `tests/eval_like.rs`.
//!
//! # `NULL` on the value or on the pattern is unknown
//!
//! `'abc' LIKE NULL`, `NULL LIKE 'a%'` and `NULL LIKE NULL` are *unknown*, so they answer
//! [`Value::Null`] and not a `bit`. `NOT LIKE` negates the two defined truth values and
//! leaves the third alone: `NULL NOT LIKE 'a%'` is unknown too, which is why a `WHERE` on
//! it filters the row out exactly like the positive form does.
//!
//! # `ESCAPE NULL` is **not** unknown: the escape is dropped
//!
//! `'abc' LIKE 'a%' ESCAPE NULL` answers true, which distinguishes nothing: `'abc' LIKE
//! 'a%'` is true as well. The expression that settles it is `'a!bc' LIKE 'a!%' ESCAPE
//! NULL`, where the three candidate rules disagree — unknown would answer -1 through a
//! `CASE`, "escape as usual" would answer 0 (the `!` would make the `%` literal and
//! `'a!bc'` is not `'a%'`), and "the escape is dropped" answers 1. The answer is **1**,
//! and the same predicate with `ESCAPE '!'` answers 0. A `NULL` escape character is
//! therefore dropped, and the `NULL` does *not* propagate.
//!
//! # An invalid `ESCAPE` is a run-time error, and the last check of the three
//!
//! An `ESCAPE` clause holds exactly one **UTF-16 code unit** (see below); anything else
//! raises 506 ([`escape_char`]). `ESCAPE ''` raises it too, with an empty
//! quoted text, and so does the negative form, whose message still names `LIKE`.
//!
//! Two orderings are what [`eval_like`] is shaped around:
//!
//! * a `NULL` value or a `NULL` pattern wins over a malformed `ESCAPE`.
//!   `NULL LIKE 'a' ESCAPE 'ab'` answers unknown and raises **nothing**, although the very
//!   same clause raises 506 as soon as both operands are known;
//! * the check happens while the predicate is **evaluated**, not while the batch is
//!   compiled. `SELECT CASE WHEN 1 = 0 THEN CASE WHEN 'a' LIKE 'a' ESCAPE 'ab' THEN 1
//!   ELSE 0 END ELSE 2 END;` answers 2, and a batch whose second statement carries the
//!   bad clause still returns the rows of the first one before the error. A compile-time
//!   check would have returned neither.
//!
//! The text is taken as it comes, with no trimming: `ESCAPE CAST(N'!' AS char(3))` raises
//! 506 quoting `"!  "`, the two blanks `char(3)` padded it with. The operand does not have
//! to be a character string either — the binder inserts the implicit conversion, so
//! `ESCAPE 1` is the escape character `'1'` and matches a literal `%` behind it.
//!
//! # One **UTF-16 code unit**, which is not the same as one character
//!
//! Rust counts Unicode scalars and SQL Server counts UTF-16 units, and the two disagree
//! above the BMP. The expression that separates them is a supplementary character, one
//! scalar written on two units: `'a' LIKE 'a' ESCAPE N'𝄞'` (U+1D11E) raises **506**,
//! quoting `"𝄞"`, where `'a' LIKE 'a' ESCAPE N'é'` and `'a' LIKE 'a' ESCAPE N'漢'` are
//! accepted and answer the row — so the check rejects the astral character, and does not
//! reject non-ASCII escapes in general. Counting [`char`]s would accept `N'𝄞'`.
//!
//! The unit is neither the character `LEN` counts nor a matter of collation: under
//! `Latin1_General_100_CI_AS_SC`, which knows supplementary characters, `LEN(N'𝄞')` is 1
//! (it is 2 under the default collation) and `DATALENGTH(N'𝄞')` stays 4, yet the same
//! escape written under that collation still raises 506. Hence `encode_utf16` in
//! [`escape_char`], not `chars`.
//!
//! # The state of 506 follows the bound operand types
//!
//! With `'a'`/`N'a'` on the value and pattern and `'ab'`/`N'ab'` on the escape, the
//! all-varchar form returns state 1 and the seven combinations containing nvarchar return
//! state 2 (`pattern::tests::escape_state_follows_the_three_operand_types`). The declared
//! result type counts, not the source literal: `CAST(N'!' AS char(3))` returns state 1,
//! whereas `CAST('!' AS nchar(3))` returns state 2, both quoting the padded text `"!  "`.
//! A `NULL` nvarchar value or pattern gives unknown even with `ESCAPE 'ab'`; a `NULL`
//! nvarchar escape on `'a!bc' LIKE 'a!%'` gives true
//! (`pattern::tests::unicode_null_operands_keep_their_precedence`).

use vauban_binder::BoundExpr;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{Collation, SqlType, Value};

use crate::context::ExecContext;
use crate::expr::eval_expr;
use crate::row::Row;

/// The name of the predicate in the 506 message: the one the bad `ESCAPE` was written
/// on. `LIKE` for the negative form as well as for the positive one — `SELECT 1 WHERE 'a'
/// NOT LIKE 'a' ESCAPE 'ab';` names a `LIKE` predicate, with no `NOT` anywhere
/// (`tests/eval_like.rs`).
const PREDICATE: &str = "LIKE";

/// Evaluates `expr [NOT] LIKE pattern [ESCAPE escape]`.
///
/// The three operands are evaluated left to right, without short-circuit: like `AND`,
/// `OR` and `IN`, `LIKE` evaluates its operands before it looks at them (see the
/// documentation of `expr.rs`). The answers are decided after that, in the order the module documentation
/// justifies: unknown first, then the validity of the escape character, then the match.
///
/// # Errors
///
/// The client error 506 when the `ESCAPE` clause holds anything but exactly one character,
/// and whatever the evaluation of the three operands raises, unchanged.
///
/// The internal error 50000 when an operand is neither a character string nor `NULL`:
/// `bind_like` wraps a non-character operand in a conversion towards `varchar` or
/// `nvarchar`, so a bug of the binder is what produces anything else here.
pub(crate) fn eval_like(
    expr: &BoundExpr,
    pattern: &BoundExpr,
    escape: Option<&BoundExpr>,
    negated: bool,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Value> {
    let collation = like_collation(expr, pattern);
    let value = eval_expr(expr, row, ctx)?;
    let pattern_value = eval_expr(pattern, row, ctx)?;
    let escape_value = escape
        .map(|escape| eval_expr(escape, row, ctx))
        .transpose()?;

    // Unknown wins over everything, a malformed `ESCAPE` included.
    let (Some(text), Some(pattern_text)) = (text_of(&value)?, text_of(&pattern_value)?) else {
        return Ok(Value::Null);
    };
    let unicode = [Some(expr), Some(pattern), escape]
        .into_iter()
        .flatten()
        .any(|operand| matches!(operand.ty.ty, SqlType::NChar(_) | SqlType::NVarChar(_)));
    let escape_char = escape_char(escape_value.as_ref(), unicode)?;

    // `negated` is the `NOT` of `NOT LIKE`, which the parser puts on the node rather than
    // wrapping it; on a defined truth value it is an ordinary negation, and the unknown
    // above has already returned.
    let matched = collation.like(text, pattern_text, escape_char);
    Ok(Value::Bit(matched != negated))
}

/// The collation the match runs under: that of the tested value, or that of the pattern
/// when the value carries none, or the default of the server.
///
/// Same rule, and same limit, as `comparison_collation` in `expr.rs`. SQL Server resolves
/// the two sides by **collation precedence**: an explicit `COLLATE` wins over an implicit
/// collation whichever side it sits on, and two different explicit ones raise 468.
/// Neither can be reproduced from here: `TypeInfo::collation` is a `Some(Collation)` for
/// each character expression and records nothing about *how* the collation was obtained,
/// so `'ABC' LIKE ('a%' COLLATE Latin1_General_CS_AS)` — false on SQL Server, true here —
/// is indistinguishable from `'ABC' LIKE 'a%'` at this level. Deriving the collation of a
/// `LIKE` belongs to `bind_like`; the gap is the binder's rather than papered over here.
///
/// What this rule does get right is the explicit collation on the **left**, the common
/// shape: `('ABC' COLLATE Latin1_General_CS_AS) LIKE 'a%'` is false and
/// `(N'é' COLLATE Latin1_General_CI_AI) LIKE N'e'` is true, against true and false under
/// the default `_CI_AS`.
fn like_collation(expr: &BoundExpr, pattern: &BoundExpr) -> Collation {
    expr.ty
        .collation
        .or(pattern.ty.collation)
        .unwrap_or(Collation::DEFAULT)
}

/// The escape character of the clause: `None` when there is no clause **or** when it holds
/// `NULL`, which behaves the same way (module documentation).
///
/// The length is counted in **UTF-16 code units**, the unit SQL Server counts, and not in
/// `char`s: a supplementary character such as `N'𝄞'` is one scalar but two units, and is
/// rejected (module documentation). One unit therefore means one BMP scalar, which is
/// exactly one `char`, so the `chars().next()` below yields it.
///
/// # Errors
///
/// The client error 506 when the text is not exactly one UTF-16 code unit, empty text and
/// supplementary character included. The internal error 50000 when the operand is not a
/// character string.
fn escape_char(escape: Option<&Value>, unicode: bool) -> SqlResult<Option<char>> {
    let Some(escape) = escape else {
        return Ok(None);
    };
    let Some(text) = text_of(escape)? else {
        return Ok(None);
    };
    let mut units = text.encode_utf16();
    match (units.next(), units.next()) {
        (Some(_), None) => Ok(text.chars().next()),
        // Blanks are not trimmed: `ESCAPE CAST('!' AS char(3))` raises on `"!  "`.
        _ => Err(if unicode {
            SqlError::invalid_escape_unicode(text, PREDICATE)
        } else {
            SqlError::invalid_escape(text, PREDICATE)
        }),
    }
}

/// The text of a character operand, `None` for `NULL`.
///
/// # Errors
///
/// The internal error 50000 for anything else: `bind_like` converts the non-character
/// operands, so a bug of the binder is what reaches this arm.
fn text_of(value: &Value) -> SqlResult<Option<&str>> {
    match value {
        Value::String(s) => Ok(Some(&s.text)),
        Value::Null => Ok(None),
        _ => Err(SqlError::from(InternalError::Bug(
            "eval_like: an operand of `LIKE` is neither a character string nor `NULL`".to_owned(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use vauban_binder::{BindContext, SessionOptions, bind};
    use vauban_parser::{ParseOptions, parse_batch};
    use vauban_sysfn::StaticContext;

    use super::*;

    fn run(sql: &str) -> SqlResult<crate::RowSet> {
        let batch = parse_batch(sql, &ParseOptions::default())?;
        let bound = bind(
            &batch.statements[0],
            &BindContext::scalar(sql, SessionOptions::default()),
        )?;
        let physical = vauban_planner::plan(
            bound,
            &vauban_planner::PlanContext {
                catalog: &vauban_planner::NoIndexes,
            },
        )?;
        let context = StaticContext::default();
        crate::execute_collect(
            &physical,
            &mut ExecContext::scalar(&context, SessionOptions::default()),
        )
        .map(|(_, set)| set)
    }

    #[test]
    fn escape_state_follows_the_three_operand_types() {
        for value in ["", "N"] {
            for pattern in ["", "N"] {
                for escape in ["", "N"] {
                    let sql =
                        format!("SELECT 1 WHERE {value}'a' LIKE {pattern}'a' ESCAPE {escape}'ab';");
                    let error = run(&sql).unwrap_err();
                    let state = if [value, pattern, escape].contains(&"N") {
                        2
                    } else {
                        1
                    };
                    assert_eq!(
                        (error.number, error.severity, error.state),
                        (506, 16, state),
                        "{sql}"
                    );
                }
            }
        }
        for (cast, state) in [("CAST(N'!' AS char(3))", 1), ("CAST('!' AS nchar(3))", 2)] {
            let error = run(&format!("SELECT 1 WHERE 'a' LIKE 'a' ESCAPE {cast};")).unwrap_err();
            assert_eq!(
                (error.number, error.severity, error.state),
                (506, 16, state)
            );
            assert!(error.message.contains("\"!  \""));
        }
    }

    #[test]
    fn unicode_null_operands_keep_their_precedence() {
        for (predicate, expected) in [
            ("CAST(NULL AS nvarchar(3)) LIKE 'a' ESCAPE 'ab'", -1),
            ("'a' LIKE CAST(NULL AS nvarchar(3)) ESCAPE 'ab'", -1),
            ("'a!bc' LIKE 'a!%' ESCAPE CAST(NULL AS nvarchar(3))", 1),
        ] {
            let result = run(&format!(
                "SELECT CASE WHEN {predicate} THEN 1 WHEN NOT ({predicate}) THEN 0 ELSE -1 END;"
            ))
            .unwrap();
            assert_eq!(result.rows, vec![vec![Value::I32(expected)]]);
        }
    }
}
