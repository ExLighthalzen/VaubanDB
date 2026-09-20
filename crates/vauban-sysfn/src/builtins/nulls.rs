//! `ISNULL`, `COALESCE` and `NULLIF`: the three built-ins whose whole job is to answer
//! "which of these expressions is not `NULL`?".
//!
//! They share a shape and differ on every detail that matters, all of it documented by
//! Microsoft Learn ("ISNULL (Transact-SQL)", "COALESCE (Transact-SQL)", "NULLIF
//! (Transact-SQL)"):
//!
//! | | result type | nullable | conversion of the result |
//! |---|---|---|---|
//! | `ISNULL(check, replacement)` | type of `check` | only if **both** are | replacement converted to the type of `check`, hence truncated |
//! | `COALESCE(a, b, …)` | highest precedence of all arguments | as soon as **one** is | first non-`NULL` converted to that common type |
//! | `NULLIF(a, b)` | type of `a` | always | none, `a` is returned as it stands |
//!
//! The visible consequence of the first column: `ISNULL(CAST(NULL AS varchar(3)),
//! 'abcdef')` is `'abc'` while `COALESCE(CAST(NULL AS varchar(3)), 'abcdef')` is
//! `'abcdef'` (`isnull_truncates_to_the_type_of_check`).
//!
//! No type rule is written here: the common type comes from
//! [`vauban_types::implicit_result_type`], the conversions from
//! [`vauban_types::convert`] and the comparison from [`vauban_types::compare`].

use std::cmp::Ordering;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{Collation, TypeInfo, Value, compare, convert, implicit_result_type};

use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// The type and the value of argument `index`.
///
/// `check_call` accepted the call before the `executor` evaluates it, so the argument is
/// always there; a missing one is a broken precondition of the caller, reported as an
/// internal bug rather than as a SQL error (and never as a panic, which the conventions
/// forbid on the query path).
fn arg<'a>(args: &EvalArgs<'a>, index: usize) -> SqlResult<(&'a Value, &'a TypeInfo)> {
    match (args.values.get(index), args.types.get(index)) {
        (Some(value), Some(ty)) => Ok((value, ty)),
        _ => Err(missing_argument(index)),
    }
}

/// The type of argument `index` at binding time, same precondition as [`arg`].
fn arg_type(args: &[TypeInfo], index: usize) -> SqlResult<&TypeInfo> {
    args.get(index).ok_or_else(|| missing_argument(index))
}

/// The internal error a call that does not match the declared arity deserves.
fn missing_argument(index: usize) -> SqlError {
    InternalError::Bug(format!("null function: argument {index} is missing")).into()
}

/// Result type of `ISNULL(check, replacement)`: the type of `check`, unchanged.
///
/// Length, precision and scale are those of `check` — that is what makes the result
/// truncate — and the result is nullable only when **both** arguments are (Microsoft Learn,
/// "ISNULL (Transact-SQL)", remarks comparing `ISNULL` with `COALESCE`). Whether
/// `replacement` converts to that type at all is decided by
/// [`vauban_types::convert`] at evaluation time, not here.
fn isnull_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let check = arg_type(args, 0)?;
    let replacement = arg_type(args, 1)?;
    Ok(TypeInfo {
        nullable: check.nullable && replacement.nullable,
        ..check.clone()
    })
}

/// Evaluates `ISNULL(check, replacement)`.
///
/// `check` is returned as it stands when it is not `NULL`; otherwise the replacement is
/// converted to the **declared type of `check`**, which is where the truncation happens.
/// The declared type comes from `args.types[0]`: a [`Value::String`] carries no length, so
/// nothing else could tell `varchar(3)` from `varchar(10)`.
fn isnull_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (check, check_type) = arg(args, 0)?;
    let (replacement, replacement_type) = arg(args, 1)?;
    if !matches!(check, Value::Null) {
        return Ok(check.clone());
    }
    convert(replacement, replacement_type, check_type, None)
}

/// Result type of `COALESCE(a, b, …)`: the type of highest precedence among the arguments.
///
/// [`vauban_types::implicit_result_type`] is folded from left to right, so the answer
/// is the type of the whole list and its errors (206, operand type clash) are reported
/// here. The result is nullable as soon as **one** argument is, the opposite of `ISNULL`.
///
/// # Errors
///
/// 4127 when the argument list is empty. `sysfn` never sees the text of the query: the
/// `binder` is the one that recognises `COALESCE(NULL, NULL)`, whose arguments are all the
/// untyped `NULL` constant, and asks for the type of an empty list to obtain this error.
/// A single argument is not this crate's business either: the parser rejects
/// `COALESCE(1)` with 102, which is why the arity is [`Arity::Variadic`] and no minimum
/// is checked here.
fn coalesce_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let (first, rest) = args.split_first().ok_or_else(SqlError::coalesce_all_null)?;
    let mut result = first.clone();
    for next in rest {
        result = implicit_result_type(&result, next)?;
    }
    // `implicit_result_type` already propagates nullability, but a single argument never
    // goes through it: the rule is stated here so that it holds whatever the arity.
    result.nullable = args.iter().any(|arg| arg.nullable);
    Ok(result)
}

/// Evaluates `COALESCE(a, b, …)`: the first argument that is not `NULL`, converted to the
/// result type of the call, or `NULL` when all of them are.
///
/// The target of the conversion is `args.result`, the common type of **all** the
/// arguments, not the type of the first one: this is why `COALESCE` does not truncate its
/// replacement the way `ISNULL` does.
fn coalesce_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    for index in 0..args.values.len() {
        let (value, ty) = arg(args, index)?;
        if matches!(value, Value::Null) {
            continue;
        }
        return convert(value, ty, args.result, None);
    }
    Ok(Value::Null)
}

/// Result type of `NULLIF(a, b)`: the type of `a`, always nullable.
///
/// Microsoft Learn, "NULLIF (Transact-SQL)": `NULLIF(a, b)` is `CASE WHEN a = b THEN NULL
/// ELSE a END`, so the result is `a`'s type and the `THEN NULL` branch makes it nullable
/// even when `a` is not. The second argument takes no part in it: `NULLIF(CAST(1 AS int),
/// CAST(0 AS bigint))` is an `int` and not a `bigint`, the opposite of what precedence
/// would give.
///
/// # The narrowing of an integer literal belongs to the `binder`
///
/// `SELECT NULLIF(-1, 0);` is a **`smallint`** and `SELECT NULLIF(1, 0);` a `tinyint`,
/// while `SELECT -1;`, `SELECT ISNULL(-1, 0);` and `SELECT COALESCE(-1, 0);` are `int`.
/// The narrowing applies when the first argument is written as an integer literal, and
/// not when it is a `CAST` or a sum (`NULLIF(CAST(1 AS int), 0)` and `NULLIF(1 + 1, 0)`
/// are `int`), so it reads the **shape of the argument node**, which a `return_type`
/// does not receive: `args[0]` is a [`TypeInfo`]. `vauban_binder`'s `call` applies it
/// after `check_call`; this function keeps the plain rule.
fn nullif_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let first = arg_type(args, 0)?;
    // The second argument only takes part in the comparison, never in the result type;
    // its presence is still a precondition of the call.
    arg_type(args, 1)?;
    Ok(TypeInfo {
        nullable: true,
        ..first.clone()
    })
}

/// Evaluates `NULLIF(a, b)`: `NULL` when the two are equal, `a` otherwise.
///
/// The two operands are compared the way `a = b` would be: both are converted to their
/// common type ([`vauban_types::implicit_result_type`]) and handed to
/// [`vauban_types::compare`], which applies the collation for strings — `NULLIF('abc',
/// 'ABC')` is `NULL` under the default case-insensitive collation.
///
/// A `NULL` operand needs neither conversion nor comparison: a comparison with `NULL` is
/// `UNKNOWN`, the `CASE` falls through to `ELSE a`, and `a` is returned as it stands. So
/// `NULLIF(NULL, 1)` is `NULL` and `NULLIF(1, NULL)` is `1`.
///
/// `a` is handed back **in the type of the call** and not in its own: the result type of
/// a literal first argument may be narrower than the type the literal was bound with (`NULLIF(1, 2)` is declared `tinyint` and `args.values[0]` is an `int`), and
/// a value that does not match the column its row travels in is not encodable. The
/// conversion is [`vauban_types::convert`]'s, as everywhere in this crate.
fn nullif_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (left, left_type) = arg(args, 0)?;
    let (right, right_type) = arg(args, 1)?;
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return convert(left, left_type, args.result, None);
    }
    let common = implicit_result_type(left_type, right_type)?;
    let converted_left = convert(left, left_type, &common, None)?;
    let converted_right = convert(right, right_type, &common, None)?;
    let collation = common.collation.unwrap_or(Collation::DEFAULT);
    match compare(&converted_left, &converted_right, &collation)? {
        Some(Ordering::Equal) => Ok(Value::Null),
        _ => convert(left, left_type, args.result, None),
    }
}

/// `ISNULL`: Microsoft Learn, "ISNULL (Transact-SQL)".
const ISNULL_DEF: FunctionDef = FunctionDef {
    name: "ISNULL",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: isnull_return_type,
    eval: isnull_eval,
    aggregate: None,
};

/// `COALESCE`: Microsoft Learn, "COALESCE (Transact-SQL)".
const COALESCE_DEF: FunctionDef = FunctionDef {
    name: "COALESCE",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Variadic(2),
    return_type: coalesce_return_type,
    eval: coalesce_eval,
    aggregate: None,
};

/// `NULLIF`: Microsoft Learn, "NULLIF (Transact-SQL)".
const NULLIF_DEF: FunctionDef = FunctionDef {
    name: "NULLIF",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: nullif_return_type,
    eval: nullif_eval,
    aggregate: None,
};

/// Registers `ISNULL`, `COALESCE` and `NULLIF` in the global registry.
pub(crate) fn register_all() {
    register(ISNULL_DEF);
    register(COALESCE_DEF);
    register(NULLIF_DEF);
}

#[cfg(test)]
mod tests;
