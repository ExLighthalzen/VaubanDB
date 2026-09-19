//! Evaluation of a bound scalar expression: operators, comparisons, three-valued logic.
//!
//! The executor computes no arithmetic and no ordering of its own: `types::eval_binary`
//! holds the rules about scale, rounding and overflow, and `types::compare` the rules
//! about ordering and collation. What lives here, and nowhere else, is the **three-valued
//! logic**: `NULL` is neither true nor false, so a comparison answers `Value::Null` for
//! *unknown* and `AND`, `OR` and `NOT` propagate that third value.
//!
//! # No short-circuit outside `CASE`, and it is a deliberate difference from SQL Server
//!
//! `AND` and `OR` evaluate **both** operands, left to right, and `IN` — which is
//! `x = a OR x = b` — evaluates each element of its list. That is the simplest and the
//! most predictable rule, and it is what this file does (`tests/eval_expr.rs`,
//! `and_does_not_short_circuit`).
//!
//! Two mechanisms of SQL Server spare an operand VaubanDB evaluates:
//!
//! - constant folding — a comparison between two **literals** is simplified before the
//!   query runs, and the constant it yields kills the rest of the expression **wherever it
//!   sits**: `1 / 0 = 1 OR 7 = 7` answers 1 and `1 / 0 = 1 AND 7 = 8` answers 0, while
//!   `1 / 0 = 1 OR 7 = 8` raises 8134. The failing operand is on the *left* in the three,
//!   so no left-to-right rule explains them.
//! - run-time short-circuit — SQL Server stops as soon as the answer is settled. With `@a`
//!   worth 3, `@a = 3 OR @a = 1 / 0` answers 1 and `@a = 1 / 0 OR @a = 3` raises: the
//!   logical operators read **left to right**. The written value list of an `IN` reads
//!   **right to left** and stops at the first true equality — `@a IN (1 / 0, 3, 2)`
//!   answers a value, `@a IN (3, 1 / 0)` raises — which is what tells `@a IN (3, 1 / 0)`
//!   and `@a IN (1 / 0, 3)` apart. The inverted direction belongs to that construct alone:
//!   a simple `CASE` and `COALESCE` both read left to right.
//!
//! The two are separate: folding happens at compile time on literals, short-circuiting at
//! run time on values, and closing one would not close the other. Neither is implemented
//! here.
//!
//! Only `CASE` stops early here, and it stops at the first `WHEN` that is *true* — an
//! unknown `WHEN` is not true and does not fire (`tests/eval_calls.rs`,
//! `case_short_circuits`). The two lazy built-ins, `ISNULL` and `COALESCE`, are the
//! documented exception and are handled in [`eval_call`], not by generalising the rule.
//!
//! # The simple `CASE` and its operand
//!
//! `bind_case` desugars `CASE a WHEN b THEN …` into `CASE WHEN a = b THEN …` and leaves
//! `operand` at `None`: the executor has one shape to evaluate, and
//! [`BoundExprKind::Case`] with an `operand` is a bug of the binder, reported as such.
//! The strategy for the operand therefore follows from the desugaring, and not from a
//! choice made here: `a` is cloned into each arm, so it is evaluated once per **tested**
//! arm — once when the first arm matches, `n` times when the `n`-th does
//! (`tests/eval_calls.rs`, `case_simple_reads_its_operand_once_per_tested_arm`). Hoisting
//! it back out is not something this crate could do anyway: recognising that the `n`
//! copies are the same expression would need a structural equality on `BoundExpr`, which
//! `binder::bound` deliberately does not provide.
//!
//! SQL Server reads a non-deterministic operand once per tested arm as well: `CASE
//! ABS(CHECKSUM(NEWID())) % 2 WHEN 0 THEN 'zero' WHEN 1 THEN 'one' ELSE 'neither' END`
//! answers `'neither'` for about a quarter of the rows, which requires the operand to be
//! read a second time for the second arm (a quarter being `P(≠ 0) × P(≠ 1)`). The
//! desugaring is therefore faithful.
//!
//! # `ANSI_NULLS`
//!
//! `SET ANSI_NULLS OFF` would turn `= NULL` into `IS NULL`. It is `ON` for the modern
//! drivers and its removal from SQL Server is announced, so this evaluator behaves as
//! `ANSI_NULLS ON` and reads no `ctx.options.ansi_nulls`: there is deliberately no dead
//! branch below.

use std::cmp::Ordering;

use vauban_binder::{BoundExpr, BoundExprKind, CompareOp, LogicalOp};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_sysfn::{EvalArgs, FunctionDef};
use vauban_types::{
    BinaryOp, Collation, Decimal, SqlString, SqlType, TypeFamily, TypeInfo, Value, compare,
    eval_binary,
};

use crate::context::{ExecContext, SLOT_COLUMN_ID};
use crate::errors::at;
use crate::row::Row;

/// Evaluates one bound expression over `row` and returns its value.
///
/// `row` is the row the column references of `expr` are read from. It is `None` for a
/// statement with no source, an expression of a `SELECT` without `FROM`, and `Some`
/// under a [`Scan`](vauban_binder::LogicalPlan::Scan), where a
/// [`ColumnRef`](vauban_binder::BoundExprKind::ColumnRef) takes the value at its `index`
/// (unit tests `column_ref_reads_the_index` and `column_ref_without_row_is_a_bug`).
///
/// A predicate (`=`, `AND`, `NOT`, `IS NULL`…) has no boolean type in T-SQL: it answers a
/// [`Value::Bit`] for true and false, and [`Value::Null`] for the *unknown* of the
/// three-valued logic. [`IsNull`](BoundExprKind::IsNull) is the one predicate that never
/// answers unknown.
///
/// # Errors
///
/// A runtime error is a `SqlError` with the number SQL Server uses (8134 divide by zero,
/// 8115 overflow, 245 conversion…), raised by `types` or by `sysfn` with no line, and
/// given one here: each fallible call is wrapped in [`crate::errors::at`] with the `line`
/// of the node that made it. Nesting takes care of itself, `at` not overwriting a line
/// that is already set, so the node that failed is the one the client is told about —
/// where SQL Server names the line of the whole **statement**, a line this crate does not
/// hold. The gap is described in [`crate::errors`]; it is invisible on a statement written
/// on one line.
///
/// A broken precondition — two operands `types` cannot compare, a variant that is not
/// evaluated yet — is the internal error 50000 instead, because a bug of the binder is
/// what produces it.
pub fn eval_expr(
    expr: &BoundExpr,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Value> {
    match &expr.kind {
        BoundExprKind::Literal(value) => Ok(value.clone()),
        BoundExprKind::Exists(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::InSubquery { .. } => {
            if ctx.has_active_subplans() {
                crate::ops::subquery::eval_subquery_expr(expr, row, ctx)
            } else {
                Err(bug(
                    "eval_expr: a subquery is evaluated without SubqueryEval",
                ))
            }
        }
        BoundExprKind::Arith { op, left, right } => {
            let left = eval_expr(left, row, ctx)?;
            let right = eval_expr(right, row, ctx)?;
            eval_arith(*op, &left, &right, &expr.ty, ctx).map_err(|e| at(e, expr.line))
        }
        BoundExprKind::Negate(inner) => {
            let value = eval_expr(inner, row, ctx)?;
            if matches!(value, Value::Null) {
                return Ok(Value::Null);
            }
            // `- x` is `0 - x`: the range check and the 8115 it raises stay in `types`.
            eval_binary(BinaryOp::Sub, &zero_like(&value)?, &value, &expr.ty)
                .map_err(|e| at(e, expr.line))
        }
        BoundExprKind::BitNot(inner) => {
            let value = eval_expr(inner, row, ctx)?;
            if matches!(value, Value::Null) {
                return Ok(Value::Null);
            }
            // `~ x` is `x ^ 111…1` on as many bits as the result type has.
            eval_binary(BinaryOp::BitXor, &value, &all_ones(&expr.ty)?, &expr.ty)
                .map_err(|e| at(e, expr.line))
        }
        BoundExprKind::Compare { op, left, right } => {
            let collation = comparison_collation(left, right);
            let line = expr.line;
            let left =
                eval_expr(left, row, ctx).map_err(|e| compare_operand_error(e, left.line))?;
            let right =
                eval_expr(right, row, ctx).map_err(|e| compare_operand_error(e, right.line))?;
            match compare(&left, &right, &collation).map_err(|e| at(e, line))? {
                Some(ordering) => Ok(Value::Bit(holds(*op, ordering))),
                None => Ok(Value::Null),
            }
        }
        BoundExprKind::Logical { op, left, right } => {
            // Both operands, always, left to right: see the module documentation.
            let left = eval_expr(left, row, ctx)?;
            let right = eval_expr(right, row, ctx)?;
            let (left, right) = (as_condition(&left)?, as_condition(&right)?);
            Ok(from_condition(connective(*op, left, right)))
        }
        BoundExprKind::Not(inner) => {
            let value = eval_expr(inner, row, ctx)?;
            // `NOT` of unknown is unknown, which `Option::map` gives for free.
            Ok(from_condition(as_condition(&value)?.map(|b| !b)))
        }
        BoundExprKind::IsNull {
            expr: inner,
            negated,
        } => {
            let value = eval_expr(inner, row, ctx)?;
            // The one predicate that is not unknown for a `NULL`: `NULL IS NULL` is true.
            Ok(Value::Bit(matches!(value, Value::Null) != *negated))
        }
        BoundExprKind::Collate { expr: inner } => {
            // Transparent: the collation the user asked for is already in `expr.ty`, where
            // `compare` and `LIKE` read it.
            eval_expr(inner, row, ctx)
        }
        // A local variable is read from the session state `DECLARE` entered it in. A name
        // the binder did not declare is a bug of the binder, reported as such.
        BoundExprKind::Variable { name } => ctx.session()?.variable_value(name).ok_or_else(|| {
            bug(&format!(
                "eval_expr: the variable `{name}` was not declared"
            ))
        }),
        // A column is read by position and consults nothing: `index` is where the value
        // sits in the row the node below produced, and the binder put the name and the
        // type in the binding while it resolved the column. A `row` of `None` means the
        // binder let a column reference through where there is no source to read — 50000
        // (`column_ref_without_row_is_a_bug`), no client input causing it — unless the
        // reference is to an outer row pushed by a correlated join, in which case the
        // outer rows of the context are consulted instead.
        BoundExprKind::ColumnRef(binding) => {
            if binding.column == SLOT_COLUMN_ID {
                let row = if let Some(row) = row {
                    row
                } else {
                    ctx.outer_rows().last().ok_or_else(|| {
                        bug(&format!(
                            "eval_expr: column `{}` is evaluated without a row",
                            binding.name
                        ))
                    })?
                };
                return ctx.read_local_column(row, binding);
            }
            let row = if let Some(row) = row {
                row
            } else {
                ctx.outer_rows().last().ok_or_else(|| {
                    bug(&format!(
                        "eval_expr: column `{}` is evaluated without a row",
                        binding.name
                    ))
                })?
            };
            if ctx.subquery_locals().is_some() {
                if ctx.column_is_outer(binding.column, &binding.name) {
                    return ctx.read_outer_column(binding.column, &binding.name);
                }
                return ctx.read_local_column(row, binding);
            }
            if let Some(value) = row.get(binding.index) {
                return Ok(value.clone());
            }
            Err(bug(&format!(
                "eval_expr: column `{}` is at index {} of a row of {} value(s)",
                binding.name,
                binding.index,
                row.len()
            )))
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            if operand.is_some() {
                // `bind_case` desugars the simple form into the searched one, so this
                // shape never reaches the executor: see the module documentation.
                return Err(bug(
                    "eval_expr: a simple `CASE` is desugared by the binder, not evaluated here",
                ));
            }
            // The one short-circuit T-SQL guarantees: stop at the first `WHEN` that is
            // **true**, and evaluate nothing else — neither the later `WHEN`, nor the
            // `THEN` of the arms that did not fire, nor the `ELSE`.
            for arm in arms {
                let when = eval_expr(&arm.when, row, ctx)?;
                if as_condition(&when)? == Some(true) {
                    return eval_expr(&arm.then, row, ctx);
                }
            }
            match else_ {
                Some(else_) => eval_expr(else_, row, ctx),
                None => Ok(Value::Null),
            }
        }
        BoundExprKind::Convert {
            expr: inner,
            style,
            try_,
        } => {
            let value = eval_expr(inner, row, ctx)?;
            // The source type is the one the binder gave the operand, never a guess made
            // from the value: error 245 names `varchar` or `nvarchar`, and only the plan
            // knows which.
            crate::convert::eval_convert(&value, &inner.ty, &expr.ty, *style, *try_, expr.line)
        }
        BoundExprKind::In {
            expr: inner,
            list,
            negated,
        } => {
            let collation = in_collation(inner, list);
            let value = eval_expr(inner, row, ctx)?;
            let mut found = false;
            let mut unknown = false;
            for item in list {
                let item = eval_expr(item, row, ctx)?;
                match compare(&value, &item, &collation).map_err(|e| at(e, expr.line))? {
                    Some(Ordering::Equal) => found = true,
                    // Not equal: this element says nothing, the others may still.
                    Some(_) => {}
                    // One side is `NULL`: this element is unknown, not false.
                    None => unknown = true,
                }
            }
            // `x IN (a, b)` is `x = a OR x = b`: true wins over unknown, unknown over
            // false. `NOT IN` negates that, and `NOT unknown` is still unknown — which is
            // why `3 NOT IN (1, NULL)` is `NULL` and filters the row out of a `WHERE`.
            let held = if found {
                Some(true)
            } else if unknown {
                None
            } else {
                Some(false)
            };
            Ok(from_condition(if *negated {
                held.map(|b| !b)
            } else {
                held
            }))
        }
        BoundExprKind::Function { def, args } => {
            eval_call(def, args, &expr.ty, row, ctx, expr.line)
        }
        BoundExprKind::Like {
            expr: value,
            pattern,
            escape,
            negated,
        } => crate::pattern::eval_like(value, pattern, escape.as_deref(), *negated, row, ctx)
            .map_err(|e| at(e, expr.line)),
    }
}

/// The built-in functions whose arguments are **not** all evaluated
/// (`tests/eval_calls.rs`, `isnull_does_not_evaluate_the_replacement`).
///
/// SQL Server rewrites `COALESCE` as a `CASE`, which makes it short-circuit; `ISNULL`
/// behaves the same way. The `sysfn` registry receives *values*, so the laziness cannot
/// live there: it is written here, on a closed list of two names compared
/// case-insensitively against the canonical `def.name`.
///
/// Desugaring both calls into a `CASE` in the binder, as SQL Server does, would remove
/// this list; [`eval_call`] would then evaluate the arguments of each function.
const LAZY_FUNCTIONS: [&str; 2] = ["ISNULL", "COALESCE"];

/// Evaluates one call of a built-in: the single place where `executor` and `sysfn` meet.
///
/// The three fields of [`EvalArgs`] all come from the **bound plan**, never from a value:
/// `types[i]` is the `ty` of the `BoundExpr` of argument `i`, `result` is the `ty` of the
/// `Function` node itself, which is exactly what `sysfn::check_call` computed at bind time.
/// That is the whole point of the structure: `DATALENGTH('abc')` and `DATALENGTH(N'abc')`
/// produce the same [`Value::String`] and must not answer the same number.
///
/// Arguments are evaluated **left to right**. For the two names of [`LAZY_FUNCTIONS`] the
/// evaluation stops at the first argument that is not `NULL`; the arguments that follow
/// are not evaluated and their slot carries [`Value::Null`], so that `values` and `types`
/// keep the same length, which [`EvalArgs`] states as a precondition of the caller. No
/// function can observe the difference: both `ISNULL` and `COALESCE` answer from the first
/// non-`NULL` argument and never look further.
///
/// # Errors
///
/// Whatever the function raises (536 for an out-of-range length, 8115, 8116, 9810…) and
/// whatever the evaluation of an argument raises, unchanged.
fn eval_call(
    def: &'static FunctionDef,
    args: &[BoundExpr],
    result: &TypeInfo,
    row: Option<&Row>,
    ctx: &mut ExecContext<'_>,
    line: u32,
) -> SqlResult<Value> {
    let types: Vec<TypeInfo> = args.iter().map(|arg| arg.ty.clone()).collect();
    let lazy = LAZY_FUNCTIONS
        .iter()
        .any(|name| name.eq_ignore_ascii_case(def.name));

    let mut values: Vec<Value> = Vec::with_capacity(args.len());
    let mut satisfied = false;
    for arg in args {
        if lazy && satisfied {
            values.push(Value::Null);
            continue;
        }
        let value = eval_expr(arg, row, ctx)?;
        satisfied = !matches!(value, Value::Null);
        values.push(value);
    }

    let args = EvalArgs {
        values: &values,
        types: &types,
        result,
    };
    (def.eval)(&args, ctx.eval).map_err(|e| at(e, line))
}

/// The collation `types::compare` compares the operand of an `IN` with each element under.
///
/// `bind_in` has already brought the tested value and every element to one common type, so
/// they all agree; the first collation found is that common one. `compare` ignores it for
/// everything but strings, hence the default when none of them is a character type.
fn in_collation(expr: &BoundExpr, list: &[BoundExpr]) -> Collation {
    expr.ty
        .collation
        .or_else(|| list.iter().find_map(|item| item.ty.collation))
        .unwrap_or(Collation::DEFAULT)
}

/// The truth value a predicate evaluated to: `Some(b)` for true or false, `None` for the
/// *unknown* of the three-valued logic.
///
/// A bare `Option<bool>` could not also answer the internal error for a value that is
/// neither a `bit` nor `NULL`, so the result is wrapped: the `Option` still carries the
/// third truth value, and `Err` carries the bug.
///
/// # Errors
///
/// The internal error 50000 when `value` is neither [`Value::Bit`] nor [`Value::Null`]:
/// the binder types a predicate `bit` (`BoundExpr::is_predicate`), so anything else is a
/// bug of the binder, not something a client can write.
pub(crate) fn as_condition(value: &Value) -> SqlResult<Option<bool>> {
    match value {
        Value::Bit(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        _ => Err(bug("as_condition: a predicate must evaluate to a bit")),
    }
}

/// The [`Value`] a truth value is returned as: `bit` for true and false, `NULL` for
/// unknown.
pub(crate) fn from_condition(condition: Option<bool>) -> Value {
    match condition {
        Some(b) => Value::Bit(b),
        None => Value::Null,
    }
}

/// The truth table of `AND` and `OR`, unknown included.
///
/// `AND` is false as soon as one side is false, even when the other is unknown
/// (`NULL AND 1 = 0` is false); `OR` is true as soon as one side is true
/// (`NULL OR 1 = 1` is true). Everything else that involves an unknown is unknown.
fn connective(op: LogicalOp, left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match op {
        LogicalOp::And => match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        LogicalOp::Or => match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
    }
}

/// Whether `ordering`, the answer of `types::compare`, satisfies `op`.
fn holds(op: CompareOp, ordering: Ordering) -> bool {
    match op {
        CompareOp::Eq => ordering == Ordering::Equal,
        CompareOp::Ne => ordering != Ordering::Equal,
        CompareOp::Lt => ordering == Ordering::Less,
        CompareOp::Le => ordering != Ordering::Greater,
        CompareOp::Gt => ordering == Ordering::Greater,
        CompareOp::Ge => ordering != Ordering::Less,
    }
}

/// The collation `types::compare` compares the two operands under.
///
/// It is **not** read off the type of the comparison itself, which is the `bit` the binder
/// put there and which carries no collation: it is the collation of the operands, which
/// the binder has already brought to a common type (`bind_comparison`), so both sides
/// agree. `compare` ignores it for everything but strings, hence the default for the
/// non-character operands.
fn comparison_collation(left: &BoundExpr, right: &BoundExpr) -> Collation {
    left.ty
        .collation
        .or(right.ty.collation)
        .unwrap_or(Collation::DEFAULT)
}

fn compare_operand_error(err: SqlError, line: u32) -> SqlError {
    let err = at(err, line);
    if err.number == 512 && err.line == line && line != 0 {
        return err.with_line(line + 1);
    }
    err
}

/// `left op right`, with the `NULL` of the nine arithmetic operators.
///
/// `NULL` wins over everything and `eval_binary` is not even called — except for a
/// concatenation under `SET CONCAT_NULL_YIELDS_NULL OFF`, where a `NULL` operand behaves
/// as an empty string: `'a' + NULL` is then `'a'` and `NULL + NULL` is the empty string.
/// The option touches concatenation and nothing else: `1 + NULL` is `NULL` under both
/// settings (`tests/eval_expr.rs`, `concat_null_yields_null_switches_the_answer`).
fn eval_arith(
    op: BinaryOp,
    left: &Value,
    right: &Value,
    out: &TypeInfo,
    ctx: &ExecContext<'_>,
) -> SqlResult<Value> {
    if !matches!(left, Value::Null) && !matches!(right, Value::Null) {
        return eval_binary(op, left, right, out);
    }
    if op != BinaryOp::Concat || ctx.options.concat_null_yields_null {
        return Ok(Value::Null);
    }
    let empty = empty_operand(out)?;
    let left = if matches!(left, Value::Null) {
        &empty
    } else {
        left
    };
    let right = if matches!(right, Value::Null) {
        &empty
    } else {
        right
    };
    eval_binary(op, left, right, out)
}

/// The empty value a `NULL` operand of a concatenation stands for under
/// `CONCAT_NULL_YIELDS_NULL OFF`: the empty string, or the empty byte string for the
/// `binary` family, which `+` concatenates as well.
///
/// # Errors
///
/// The internal error 50000 when the result of the concatenation is neither character nor
/// binary: the binder produces [`BinaryOp::Concat`] for those two families and no other.
fn empty_operand(out: &TypeInfo) -> SqlResult<Value> {
    match out.ty.family() {
        TypeFamily::Character => Ok(Value::String(SqlString {
            text: String::new(),
        })),
        TypeFamily::Binary => Ok(Value::Bytes(Vec::new())),
        _ => Err(bug(
            "eval_expr: a concatenation whose result is neither character nor binary",
        )),
    }
}

/// The zero `- x` subtracts `x` from, in the family of `x`.
///
/// The integral variants share one zero: `eval_binary` widens every integer to `i64`
/// before it computes, and rebuilds the result in the type the binder asked for — which is
/// `smallint` for `- tinyint`, `tinyint` being unsigned.
///
/// # Errors
///
/// The internal error 50000 for a value unary minus does not apply to; the binder raises
/// the client error 8117 for those (`bind_unary`).
fn zero_like(value: &Value) -> SqlResult<Value> {
    match value {
        Value::I8(_) | Value::I16(_) | Value::I32(_) | Value::I64(_) => Ok(Value::I64(0)),
        Value::Decimal(d) => Ok(Value::Decimal(Decimal {
            mantissa: 0,
            precision: d.precision,
            scale: d.scale,
        })),
        Value::Money(_) => Ok(Value::Money(0)),
        Value::F32(_) => Ok(Value::F32(0.0)),
        Value::F64(_) => Ok(Value::F64(0.0)),
        _ => Err(bug("eval_expr: unary minus on a non-numeric value")),
    }
}

/// The mask `~ x` exclusive-ors `x` with: as many one bits as the result type holds.
///
/// `tinyint` is unsigned, so its mask is `255` and not `-1`: `~ CAST(1 AS tinyint)` is
/// `254`. A `bit` has a single bit, so its mask is `1`. `eval_binary` widens the three
/// families to `i64` and rebuilds the result in `out`.
///
/// # Errors
///
/// The internal error 50000 for a result type `~` does not apply to; the binder raises the
/// client error 8117 for those (`bind_unary`).
fn all_ones(out: &TypeInfo) -> SqlResult<Value> {
    match &out.ty {
        SqlType::Bit => Ok(Value::I64(1)),
        SqlType::TinyInt => Ok(Value::I64(0xFF)),
        SqlType::SmallInt | SqlType::Int | SqlType::BigInt => Ok(Value::I64(-1)),
        _ => Err(bug("eval_expr: bitwise NOT on a non-integral type")),
    }
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_binder::{ColumnBinding, SessionOptions};
    use vauban_catalog::ColumnId;
    use vauban_sysfn::StaticContext;

    /// A `ColumnRef` on the column at position `index`, of type `int`.
    fn column_ref(index: usize) -> BoundExpr {
        BoundExpr {
            kind: BoundExprKind::ColumnRef(ColumnBinding {
                column: ColumnId(1),
                index,
                name: format!("c{index}"),
                ty: TypeInfo::new(SqlType::Int, true),
            }),
            ty: TypeInfo::new(SqlType::Int, true),
            line: 1,
        }
    }

    /// Evaluates `expr` over `row` with a scalar context: a column reference consults no
    /// storage, so the engine fields stay `None`.
    fn eval(expr: &BoundExpr, row: Option<&Row>) -> SqlResult<Value> {
        let context = StaticContext::default();
        let mut ctx = ExecContext::scalar(&context, SessionOptions::default());
        eval_expr(expr, row, &mut ctx)
    }

    /// The value read is the one at `index`, and not the first of the row: the row below
    /// holds three distinct values and each index answers its own.
    #[test]
    fn column_ref_reads_the_index() {
        let row: Row = vec![Value::I64(10), Value::Null, Value::I64(30)];
        assert_eq!(
            eval(&column_ref(0), Some(&row)).expect("index 0"),
            Value::I64(10)
        );
        assert_eq!(
            eval(&column_ref(1), Some(&row)).expect("index 1"),
            Value::Null
        );
        assert_eq!(
            eval(&column_ref(2), Some(&row)).expect("index 2"),
            Value::I64(30)
        );
    }

    /// No row to read from is the internal error 50000, and the message names the column.
    #[test]
    fn column_ref_without_row_is_a_bug() {
        let error = eval(&column_ref(0), None).expect_err("there is no row");
        assert_eq!(error.number, 50000);
        assert!(error.message.contains("c0"), "message: {}", error.message);
    }

    /// An `index` past the end of the row is 50000 too, which is the counter-proof of
    /// `column_ref_reads_the_index`: the value is taken by position, not by name.
    #[test]
    fn column_ref_past_the_row_is_a_bug() {
        let row: Row = vec![Value::I64(10)];
        let error = eval(&column_ref(4), Some(&row)).expect_err("index 4 of a row of 1");
        assert_eq!(error.number, 50000);
    }
}
