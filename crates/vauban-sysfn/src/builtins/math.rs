//! `ABS`, `CEILING`, `FLOOR`, `ROUND`, `POWER`, `SQRT`, `SIGN`, `RAND` and `PI`: the
//! mathematical functions of the V1.
//!
//! The difficulty of these nine functions is not the computation, it is the **type of the
//! result**, because it is what a client sees: `SELECT ROUND(123.4545, 2);` prints
//! `123.4500` and not `123.45`, since the literal is a `numeric(7,4)` and `ROUND` keeps the
//! type of its argument, scale included.
//!
//! # The five overloads
//!
//! SQL Server does not have one signature per type: an argument is first converted to the
//! type of the overload that accepts it, and the result is expressed in that type. One
//! `SELECT` per line of the table below shows the overload through its result type:
//!
//! | argument | overload |
//! |---|---|
//! | `tinyint`, `smallint`, `int` | `int` — `SELECT ABS(CAST(-32768 AS smallint));` answers `32768` as an `int`, so no `smallint` overload exists |
//! | `bigint` | `bigint` |
//! | `decimal(p, s)`, `numeric(p, s)` | itself, each function stating what it does with `p` and `s` |
//! | `money`, `smallmoney` | `money` — `SELECT CEILING(CAST(1.5 AS smallmoney));` answers `2.0000` as a `money` |
//! | `bit`, `float`, `real`, the character types | `float` — `SELECT ABS(CAST(1 AS bit));` and `SELECT ABS('-5.5');` both answer a `float` |
//!
//! Everything else is refused at bind time, with the error of the implicit conversion to
//! `float` that fails (`argument_types_are_refused_as_sql_server_refuses_them`):
//!
//! - `datetime` and `smalldatetime` raise **257**, implicit conversion not allowed
//!   (`SELECT ABS(CAST('2020-01-01' AS datetime));`);
//! - `date`, `time`, `datetime2`, `datetimeoffset`, the binary types and
//!   `uniqueidentifier` raise **206**, operand type clash with `float`
//!   (`SELECT ABS(CAST('12:00' AS time));`, `SELECT ROUND(CAST(0x01 AS varbinary(4)),
//!   0);`).
//!
//! Neither is 8116: an argument these functions reject is rejected by the conversion to the
//! parameter, not by the function.
//!
//! # Exactness and rounding
//!
//! An exact operand (`decimal`, `numeric`, `money`) is never routed through an `f64`: the
//! computation happens on its **mantissa**, an `i128`, so that a digit beyond the
//! fifteenth is not lost. Rounding is *half away from zero*, the rule of SQL Server and
//! of `types::convert`, not `f64::round_ties_even`. Overflow of the result type raises
//! 8115 (`SELECT ROUND(CAST(2147483647 AS int), -1);`), the source being named
//! `expression` and not a type name (`round_negative_length`).
//!
//! Sources: Microsoft Learn, "ABS", "CEILING", "FLOOR", "ROUND", "POWER", "SQRT", "SIGN",
//! "RAND" and "PI (Transact-SQL)".

use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{Decimal, SqlType, TypeInfo, Value, convert};

use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// Scale of `money` and `smallmoney`: the amount is held in ten-thousandths.
const MONEY_SCALE: u8 = 4;

/// Largest precision a `decimal` or a `numeric` can carry.
const MAX_PRECISION: u8 = 38;

/// Beyond that many decimals, rounding a `float` cannot change it, and beyond that many
/// digits to the left of the point every `float` rounds to zero: 10^309 is already
/// infinite. `SELECT ROUND(CAST(1.5 AS float), 400), ROUND(CAST(1.5 AS float), -400);`
/// answers `1.5` and `0`, which is what the two saturations produce.
const FLOAT_DIGITS_LIMIT: i64 = 308;

/// Odd 64-bit constant of SplitMix64, the golden ratio scaled to 2^64.
const SPLITMIX_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

// ---------------------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------------------

/// The value and the declared type of argument `index`.
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

/// The declared type of argument `index` at binding time, same precondition as [`arg`].
fn arg_type(args: &[TypeInfo], index: usize) -> SqlResult<&TypeInfo> {
    args.get(index).ok_or_else(|| missing_argument(index))
}

/// The internal error a call that does not match the declared arity deserves.
fn missing_argument(index: usize) -> SqlError {
    InternalError::Bug(format!("math function: argument {index} is missing")).into()
}

/// The internal error a conversion that did not answer the requested family deserves.
fn unexpected_conversion(target: &str) -> SqlError {
    InternalError::Bug(format!(
        "math function: conversion to {target} did not answer a {target}"
    ))
    .into()
}

/// The internal error a result type outside the five overloads deserves.
fn unexpected_result(ty: &SqlType) -> SqlError {
    InternalError::Bug(format!(
        "math function: {} is not a result type of this module",
        ty.name()
    ))
    .into()
}

/// The type an argument of type `arg` is converted to before `ABS`, `CEILING`, `FLOOR`,
/// `ROUND`, `SIGN` or `POWER` applies: one of the five overloads of the module
/// documentation.
///
/// # Errors
///
/// 257 for `datetime` and `smalldatetime`, 206 for every other type that does not convert
/// implicitly to `float`. Both are the error of the conversion to the parameter, which is
/// why neither is 8116.
fn operand_type(arg: &TypeInfo) -> SqlResult<TypeInfo> {
    let ty = match arg.ty {
        SqlType::TinyInt | SqlType::SmallInt | SqlType::Int => SqlType::Int,
        SqlType::BigInt => SqlType::BigInt,
        SqlType::Decimal { .. } | SqlType::Numeric { .. } => arg.ty,
        SqlType::Money | SqlType::SmallMoney => SqlType::Money,
        SqlType::Bit
        | SqlType::Float
        | SqlType::Real
        | SqlType::Char(_)
        | SqlType::VarChar(_)
        | SqlType::NChar(_)
        | SqlType::NVarChar(_) => SqlType::Float,
        SqlType::DateTime | SqlType::SmallDateTime => {
            return Err(SqlError::implicit_conversion_not_allowed(
                arg.ty.error_name(),
                SqlType::Float.name(),
            ));
        }
        SqlType::Date
        | SqlType::Time(_)
        | SqlType::DateTime2(_)
        | SqlType::DateTimeOffset(_)
        | SqlType::Binary(_)
        | SqlType::VarBinary(_)
        | SqlType::UniqueIdentifier => {
            return Err(SqlError::operand_type_clash(
                arg.ty.error_name(),
                SqlType::Float.name(),
            ));
        }
    };
    // None of the five overloads is a character type, so no collation is carried over.
    Ok(TypeInfo::new(ty, arg.nullable))
}

// ---------------------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------------------

/// An operand already converted to its overload, in the shape that type gives it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Number {
    /// `int` or `bigint`.
    Integer(i64),
    /// `decimal(p, s)`, `numeric(p, s)` or `money`: the value is `mantissa / 10^scale`.
    Exact {
        /// Signed unscaled value.
        mantissa: i128,
        /// Number of digits to the right of the point (4 for `money`).
        scale: u8,
    },
    /// `float`.
    Float(f64),
}

/// Reads argument `index` as the number its overload takes, `None` when it is `NULL`.
fn operand(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<Number>> {
    let (value, ty) = arg(args, index)?;
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let target = operand_type(ty)?;
    number(&convert(value, ty, &target, None)?)
}

/// The [`Number`] a value converted to one of the five overloads carries.
fn number(value: &Value) -> SqlResult<Option<Number>> {
    Ok(Some(match *value {
        Value::Null => return Ok(None),
        Value::I32(n) => Number::Integer(i64::from(n)),
        Value::I64(n) => Number::Integer(n),
        Value::Decimal(d) => Number::Exact {
            mantissa: d.mantissa,
            scale: d.scale,
        },
        Value::Money(m) => Number::Exact {
            mantissa: i128::from(m),
            scale: MONEY_SCALE,
        },
        Value::F64(f) => Number::Float(f),
        _ => return Err(unexpected_conversion("number")),
    }))
}

/// Reads argument `index` as a `float`, `None` when it is `NULL`.
///
/// Used by `POWER` and `SQRT`, whose computation is approximate anyway; the argument types
/// they accept were checked by their `return_type`.
fn float_arg(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<f64>> {
    let (value, ty) = arg(args, index)?;
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    match convert(value, ty, &float_type(), None)? {
        Value::F64(f) => Ok(Some(f)),
        Value::Null => Ok(None),
        _ => Err(unexpected_conversion("float")),
    }
}

/// Reads argument `index` as a `bigint`, `None` when it is `NULL`.
///
/// The `length` and the `function` of `ROUND` are not necessarily `int`: SQL Server
/// converts them implicitly and truncates what is not integral, `SELECT ROUND(1.55,
/// CAST(1.9 AS float));` answering `1.60`. `bigint` is the widest integer target, so no
/// usable length is lost on the way.
fn integer_arg(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<i64>> {
    let (value, ty) = arg(args, index)?;
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let target = TypeInfo::new(SqlType::BigInt, true);
    match convert(value, ty, &target, None)? {
        Value::I64(n) => Ok(Some(n)),
        Value::Null => Ok(None),
        _ => Err(unexpected_conversion("bigint")),
    }
}

/// A nullable `float`, the type `SQRT`, `RAND` and `PI` answer and the one `POWER` computes
/// in.
fn float_type() -> TypeInfo {
    TypeInfo::new(SqlType::Float, true)
}

/// `10^k`, or `None` when it does not fit in an `i128` (`k > 38`).
fn pow10(k: u32) -> Option<i128> {
    10i128.checked_pow(k)
}

/// Whether a mantissa holds in `precision` digits: `decimal(p, s)` stores values strictly
/// below `10^p / 10^s`, which on the mantissa reads `|m| < 10^p`.
fn fits_precision(m: i128, precision: u8) -> bool {
    pow10(u32::from(precision)).is_some_and(|limit| m > -limit && m < limit)
}

/// The scale an exact result type carries: 0 for the integer types, 4 for `money`, the
/// declared one for `decimal` and `numeric`.
fn result_scale(ty: &SqlType) -> SqlResult<u8> {
    match *ty {
        SqlType::Int | SqlType::BigInt => Ok(0),
        SqlType::Money => Ok(MONEY_SCALE),
        SqlType::Decimal { scale, .. } | SqlType::Numeric { scale, .. } => Ok(scale),
        _ => Err(unexpected_result(ty)),
    }
}

/// Error 8115 for a result that does not fit in `result`.
///
/// The source is the word `expression`, not a type name: `SELECT ABS(CAST(-2147483648 AS
/// int));` overflows `expression` to `int`, and so do the `money` and `numeric`
/// overflows of the module (`SELECT ROUND(9.9, 0);`).
fn overflow(result: &SqlType) -> SqlError {
    SqlError::arithmetic_overflow("expression", result.error_name())
}

/// Builds the value of type `result` holding the exact number `mantissa / 10^scale`.
///
/// `scale` is never larger than the scale of `result`, so the change of scale only
/// multiplies and loses nothing; a result that does not fit raises 8115.
fn exact_value(result: &SqlType, mantissa: i128, scale: u8) -> SqlResult<Value> {
    let Some(shift) = result_scale(result)?.checked_sub(scale) else {
        return Err(unexpected_result(result));
    };
    let scaled = pow10(u32::from(shift))
        .and_then(|factor| mantissa.checked_mul(factor))
        .ok_or_else(|| overflow(result))?;
    match *result {
        SqlType::Int => i32::try_from(scaled)
            .map(Value::I32)
            .map_err(|_| overflow(result)),
        SqlType::BigInt => i64::try_from(scaled)
            .map(Value::I64)
            .map_err(|_| overflow(result)),
        SqlType::Money => i64::try_from(scaled)
            .map(Value::Money)
            .map_err(|_| overflow(result)),
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            if fits_precision(scaled, precision) {
                Ok(Value::Decimal(Decimal {
                    mantissa: scaled,
                    precision,
                    scale,
                }))
            } else {
                Err(overflow(result))
            }
        }
        _ => Err(unexpected_result(result)),
    }
}

/// Turns `-0.0` into `0.0`, as SQL Server does: `SELECT CEILING(CAST(-0.5 AS float));`
/// answers `0` and not `-0`, so the server normalises the negative zero its own rounding
/// produces.
fn normalise_zero(v: f64) -> f64 {
    if v == 0.0 { 0.0 } else { v }
}

/// `m / divisor`, rounded towards `+∞` when `up` and towards `-∞` otherwise. `divisor` is a
/// power of ten, never zero.
fn div_towards(m: i128, divisor: i128, up: bool) -> i128 {
    let quotient = m / divisor;
    let remainder = m % divisor;
    if up && remainder > 0 {
        quotient + 1
    } else if !up && remainder < 0 {
        quotient - 1
    } else {
        quotient
    }
}

/// `m / divisor`, rounded half **away from zero** (`25 / 10` is `3`, `-25 / 10` is `-3`).
///
/// The comparison is written `r >= divisor - r` rather than `2 * r >= divisor` because a
/// divisor of `10^38` would overflow the doubling.
fn div_half_away(m: i128, divisor: i128) -> i128 {
    let quotient = m / divisor;
    let remainder = (m % divisor).abs();
    if remainder >= divisor - remainder {
        if m < 0 { quotient - 1 } else { quotient + 1 }
    } else {
        quotient
    }
}

// ---------------------------------------------------------------------------------------
// ABS
// ---------------------------------------------------------------------------------------

/// Result type of `ABS`: its overload, unchanged. `ABS(decimal(5,2))` is a `decimal(5,2)`,
/// `ABS(smallint)` an `int`, `ABS('-5.5')` a `float`.
fn abs_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    operand_type(arg_type(args, 0)?)
}

/// Evaluates `ABS(x)`: the magnitude of `x`, in the type of `x`.
///
/// The only value that has no absolute value in its own type is the smallest of an integer
/// type: `SELECT ABS(CAST(-2147483648 AS int));` raises 8115, and so does the `bigint`
/// case and `SELECT ABS(CAST(-922337203685477.5808 AS money));`. Every step is `checked_`,
/// because a plain negation would panic in `debug`.
fn abs_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(number) = operand(args, 0)? else {
        return Ok(Value::Null);
    };
    let result = &args.result.ty;
    match number {
        Number::Integer(n) => {
            let magnitude = n.checked_abs().ok_or_else(|| overflow(result))?;
            exact_value(result, i128::from(magnitude), 0)
        }
        Number::Exact { mantissa, scale } => {
            let magnitude = mantissa.checked_abs().ok_or_else(|| overflow(result))?;
            exact_value(result, magnitude, scale)
        }
        Number::Float(f) => Ok(Value::F64(f.abs())),
    }
}

// ---------------------------------------------------------------------------------------
// CEILING and FLOOR
// ---------------------------------------------------------------------------------------

/// Result type of `CEILING` and `FLOOR`: their overload with the **scale brought back to
/// 0** for `decimal(p, s)` and `numeric(p, s)`; the precision does not change.
///
/// `SELECT CEILING(123.45);` prints `124` and not `124.00`, which fixes the scale. `p`
/// unchanged comes from Microsoft Learn, "CEILING (Transact-SQL)". It is wide enough:
/// `10^(p-s) - 1` needs `p - s` digits.
fn integral_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let operand = operand_type(arg_type(args, 0)?)?;
    let ty = match operand.ty {
        SqlType::Decimal { precision, .. } => SqlType::Decimal {
            precision,
            scale: 0,
        },
        SqlType::Numeric { precision, .. } => SqlType::Numeric {
            precision,
            scale: 0,
        },
        other => other,
    };
    Ok(TypeInfo::new(ty, operand.nullable))
}

/// Evaluates `CEILING(x)`: the smallest integer that is not below `x`.
fn ceiling_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    integral_eval(args, true)
}

/// Evaluates `FLOOR(x)`: the largest integer that is not above `x`.
fn floor_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    integral_eval(args, false)
}

/// `CEILING` when `up`, `FLOOR` otherwise: the two differ only by the direction.
///
/// A `money` result keeps its four decimals (`SELECT CEILING(CAST(1.5 AS money));` answers
/// `2.0000`), which is why the integral quotient is handed to [`exact_value`] at scale 0
/// and put back at the scale of the result type. The multiplication is what makes
/// `SELECT CEILING(CAST(922337203685477.5807 AS money));` raise 8115.
fn integral_eval(args: &EvalArgs<'_>, up: bool) -> SqlResult<Value> {
    let Some(number) = operand(args, 0)? else {
        return Ok(Value::Null);
    };
    let result = &args.result.ty;
    match number {
        Number::Integer(n) => exact_value(result, i128::from(n), 0),
        Number::Exact { mantissa, scale } => {
            let divisor = pow10(u32::from(scale)).ok_or_else(|| overflow(result))?;
            exact_value(result, div_towards(mantissa, divisor, up), 0)
        }
        Number::Float(f) => Ok(Value::F64(normalise_zero(if up {
            f.ceil()
        } else {
            f.floor()
        }))),
    }
}

// ---------------------------------------------------------------------------------------
// ROUND
// ---------------------------------------------------------------------------------------

/// Result type of `ROUND`: its overload, **scale included**, which is what makes
/// `SELECT ROUND(123.4545, 2);` print `123.4500`. Nullable as soon as one of the two or
/// three arguments is.
///
/// Keeping the scale also keeps the precision, so a result that no longer fits raises 8115:
/// `SELECT ROUND(9.9, 0);` overflows `expression` to `numeric` because `10.0` does not
/// fit in the `numeric(2,1)` of the literal.
fn round_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let mut result = operand_type(arg_type(args, 0)?)?;
    for extra in args.iter().skip(1) {
        result.nullable |= extra.nullable;
    }
    Ok(result)
}

/// Evaluates `ROUND(x, length [, function])`.
///
/// A positive `length` rounds to that many decimals, a negative one to the left of the
/// point (`SELECT ROUND(150, -2);` answers `200`), and a `length` larger than the number of
/// digits there are answers zero (`SELECT ROUND(748.58, -4);` answers `0.00`, at the scale
/// of the argument). A third argument other than 0 **truncates** instead of rounding
/// (`SELECT ROUND(150.75, 0, 1), ROUND(150.75, 0, 0);` answers `150.00` and `151.00`), and
/// truncation goes towards zero (`SELECT ROUND(-150.75, 0, 1);` answers `-150.00`).
fn round_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(length) = integer_arg(args, 1)? else {
        return Ok(Value::Null);
    };
    let truncate = if args.values.len() > 2 {
        match integer_arg(args, 2)? {
            Some(function) => function != 0,
            None => return Ok(Value::Null),
        }
    } else {
        false
    };
    let Some(number) = operand(args, 0)? else {
        return Ok(Value::Null);
    };
    let result = &args.result.ty;
    match number {
        Number::Integer(n) => {
            let rounded = rounded_mantissa(i128::from(n), 0, length, truncate, result)?;
            exact_value(result, rounded, 0)
        }
        Number::Exact { mantissa, scale } => {
            let rounded = rounded_mantissa(mantissa, scale, length, truncate, result)?;
            exact_value(result, rounded, scale)
        }
        Number::Float(f) => Ok(Value::F64(rounded_float(f, length, truncate))),
    }
}

/// The mantissa of `mantissa / 10^scale` rounded to `length` decimals, still expressed at
/// `scale`: the digits below `10^-length` are dropped, then put back as zeros.
///
/// Everything happens on the `i128` mantissa, never on an `f64`, so a `numeric(38, 6)`
/// keeps its 38 digits. The multiplication back is the only step that can overflow, and it
/// raises 8115 (`SELECT ROUND(CAST(2147483647 AS int), -1);`).
fn rounded_mantissa(
    mantissa: i128,
    scale: u8,
    length: i64,
    truncate: bool,
    result: &SqlType,
) -> SqlResult<i128> {
    // In `i128` because `scale - length` would overflow an `i64` for `length == i64::MIN`.
    let dropped = i128::from(scale) - i128::from(length);
    if dropped <= 0 {
        // Rounding to more decimals than there are changes nothing.
        return Ok(mantissa);
    }
    // A mantissa has at most 38 digits, so dropping 39 or more always leaves zero.
    let Some(divisor) = u32::try_from(dropped).ok().and_then(pow10) else {
        return Ok(0);
    };
    let quotient = if truncate {
        mantissa / divisor
    } else {
        div_half_away(mantissa, divisor)
    };
    quotient
        .checked_mul(divisor)
        .ok_or_else(|| overflow(result))
}

/// `ROUND` on a `float`: multiply, round half away from zero (`f64::round`, **not**
/// `round_ties_even`), divide back.
///
/// The scaling multiplies by `10^|length|` and divides by it rather than by its reciprocal,
/// which keeps the exact powers of ten exact: `SELECT ROUND(CAST(150 AS float), -2);`
/// answers `200`. Beyond [`FLOAT_DIGITS_LIMIT`] the scaling would be infinite, so the two
/// saturations are applied instead.
fn rounded_float(v: f64, length: i64, truncate: bool) -> f64 {
    if length > FLOAT_DIGITS_LIMIT {
        return v;
    }
    if length < -FLOAT_DIGITS_LIMIT {
        return 0.0;
    }
    // `length` is inside `-308..=308`, so the conversion and the power are both exact.
    let power = 10f64.powi(i32::try_from(length.abs()).unwrap_or(0));
    let scaled = if length >= 0 { v * power } else { v / power };
    let rounded = if truncate {
        scaled.trunc()
    } else {
        scaled.round()
    };
    let result = if length >= 0 {
        rounded / power
    } else {
        rounded * power
    };
    if result.is_finite() {
        normalise_zero(result)
    } else {
        v
    }
}

// ---------------------------------------------------------------------------------------
// POWER
// ---------------------------------------------------------------------------------------

/// Result type of `POWER`: the overload of the **base**, the exponent deciding nothing
/// (`SELECT POWER(CAST(2 AS int), CAST(0.5 AS float));` answers the `int` `1`).
///
/// A `decimal(p, s)` base widens to `decimal(38, s)`, which the values prove:
/// `SELECT POWER(2.0, 100);` answers `1267650600228229401496703205376.0`, 31 digits before
/// the point where the literal `2.0` is a `numeric(2,1)`. The scale is the one of the base
/// (`SELECT POWER(CAST(2.55 AS numeric(3,2)), 3);` answers `16.58`).
fn power_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let base = operand_type(arg_type(args, 0)?)?;
    let exponent = arg_type(args, 1)?;
    // The exponent is converted to the same five overloads: an argument that converts to no
    // number at all is refused here rather than at evaluation time.
    operand_type(exponent)?;
    let ty = match base.ty {
        SqlType::Decimal { scale, .. } => SqlType::Decimal {
            precision: MAX_PRECISION,
            scale,
        },
        SqlType::Numeric { scale, .. } => SqlType::Numeric {
            precision: MAX_PRECISION,
            scale,
        },
        other => other,
    };
    Ok(TypeInfo::new(ty, base.nullable || exponent.nullable))
}

/// Evaluates `POWER(x, y)`: `x` raised to `y`, computed as a `float` then brought back to
/// the type of `x`, which is what truncates `SELECT POWER(2, -1);` to the `int` `0`.
///
/// Three domains have their own answer (`power_domain_errors`):
///
/// - `0` to a negative power is a division by zero, 8134 (`SELECT POWER(CAST(0 AS int),
///   -1);`);
/// - a result that is not a number at all is 3623 (`SELECT POWER(CAST(-2 AS float),
///   0.5);`);
/// - an infinite result is 8115 on `expression` to `float` (`SELECT POWER(CAST(2 AS
///   float), 10000);`).
///
/// A finite result that does not fit the type of `x` is left to `types::convert`, which
/// raises the 232 of `SELECT POWER(CAST(2 AS int), 64);` and the 8115 of `SELECT
/// POWER(CAST(2 AS numeric(5,1)), 200);`. Their states are those of the (`float`, target)
/// pair (3 towards `int`, 6 towards `numeric`, 2 towards `money`) and they need no
/// correction here: `types::convert` reads them off the tables of `vauban-errors`, and
/// a `CAST` on the very same pair prints the very same state
/// (`SELECT CAST(CAST(1e20 AS float) AS int);` also answers 232 state 3). `SELECT
/// POWER(CAST(2 AS money), 100);` answers 232 **state 2**, which is what makes a
/// per-number rewriting of the state wrong (`power_overflow_states_follow_the_target`).
fn power_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(base), Some(exponent)) = (float_arg(args, 0)?, float_arg(args, 1)?) else {
        return Ok(Value::Null);
    };
    if base == 0.0 && exponent < 0.0 {
        return Err(SqlError::divide_by_zero());
    }
    let power = base.powf(exponent);
    if power.is_nan() {
        return Err(SqlError::invalid_floating_point_operation());
    }
    if power.is_infinite() {
        return Err(overflow(&args.result.ty));
    }
    convert(&Value::F64(power), &float_type(), args.result, None)
}

// ---------------------------------------------------------------------------------------
// SQRT
// ---------------------------------------------------------------------------------------

/// Result type of `SQRT`: always `float`, whatever the argument
/// (`SELECT SQRT(CAST(2 AS decimal(5,2)));` answers a `float`). The argument still goes
/// through [`operand_type`], which is what refuses a `time` with 206.
fn sqrt_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let arg = arg_type(args, 0)?;
    operand_type(arg)?;
    Ok(TypeInfo::new(SqlType::Float, arg.nullable))
}

/// Evaluates `SQRT(x)`: the square root of `x`, read as a `float`.
///
/// A negative argument is outside the domain and raises 3623 (`SELECT SQRT(-1);`),
/// rather than answering a `NaN`.
fn sqrt_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(v) = float_arg(args, 0)? else {
        return Ok(Value::Null);
    };
    if v < 0.0 {
        return Err(SqlError::invalid_floating_point_operation());
    }
    Ok(Value::F64(v.sqrt()))
}

// ---------------------------------------------------------------------------------------
// SIGN
// ---------------------------------------------------------------------------------------

/// Result type of `SIGN`, as `SQL_VARIANT_PROPERTY` reports it.
/// For a numeric/decimal argument, precision stays unchanged when p > s, and grows by one
/// when p = s < 38 to hold the unit digit. At (38,38), scale decreases to 37: `SELECT
/// SIGN(CAST(0.5 AS numeric(38,38)))` returns 1 followed by 37 fractional zeros. Positive,
/// negative and zero values carry the same type; NULL propagates (its variant properties
/// are NULL).
fn sign_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let operand = operand_type(arg_type(args, 0)?)?;
    let ty = match operand.ty {
        SqlType::Decimal { precision, scale } => SqlType::Decimal {
            precision: signed_unit_precision(precision, scale),
            scale: scale.min(MAX_PRECISION - 1),
        },
        SqlType::Numeric { precision, scale } => SqlType::Numeric {
            precision: signed_unit_precision(precision, scale),
            scale: scale.min(MAX_PRECISION - 1),
        },
        other => other,
    };
    Ok(TypeInfo::new(ty, operand.nullable))
}

/// The precision `SIGN` needs to hold `±1` at `scale`, starting from `precision`.
fn signed_unit_precision(precision: u8, scale: u8) -> u8 {
    precision
        .max(scale.saturating_add(1))
        .clamp(1, MAX_PRECISION)
}

/// Evaluates `SIGN(x)`: `-1`, `0` or `1`, in the type from `sign_return_type`.
///
/// `SELECT SIGN(-5.5), SIGN(CAST(-1.5 AS money));` answers `-1.0` and `-1.0000`: the sign
/// is expressed at the result scale, so the mantissa is `10^s`, not `1`.
fn sign_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(number) = operand(args, 0)? else {
        return Ok(Value::Null);
    };
    let result = &args.result.ty;
    match number {
        Number::Integer(n) => exact_value(result, i128::from(n.signum()), 0),
        Number::Exact { mantissa, .. } => exact_value(result, mantissa.signum(), 0),
        // `f64::signum` answers `1.0` for `0.0`, where SQL Server answers `0`
        // (`SELECT SIGN(CAST(0 AS float));`).
        Number::Float(f) => Ok(Value::F64(if f > 0.0 {
            1.0
        } else if f < 0.0 {
            -1.0
        } else {
            0.0
        })),
    }
}

// ---------------------------------------------------------------------------------------
// RAND and PI
// ---------------------------------------------------------------------------------------

/// Result type of `RAND`: `float`, nullable only when its optional seed is
/// (`SELECT RAND(NULL);` answers `NULL`).
fn rand_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let nullable = args.first().is_some_and(|seed| seed.nullable);
    Ok(TypeInfo::new(SqlType::Float, nullable))
}

/// Evaluates `RAND([seed])`: a pseudo-random `float` in `[0, 1)`.
///
/// **Deliberate difference from SQL Server.** With a seed, SQL Server produces a sequence
/// its documentation does not describe ("RAND (Transact-SQL)": the values depend on the
/// implementation), and VaubanDB does not reproduce it. The two properties VaubanDB does
/// reproduce are the ones a query can rely on: a draw is in `[0, 1)`, and the same seed
/// gives the same value in the same session (`SELECT CASE WHEN RAND(1) = RAND(1) THEN 1
/// ELSE 0 END;` answers `1`).
///
/// A `NULL` seed answers `NULL` without touching the generator, as SQL Server does
/// (`SELECT CASE WHEN RAND(NULL) IS NULL THEN 1 ELSE 0 END;` answers `1`).
fn rand_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    if !args.values.is_empty() {
        let (value, ty) = arg(args, 0)?;
        if matches!(value, Value::Null) {
            return Ok(Value::Null);
        }
        // Microsoft Learn: the seed is a `tinyint`, `smallint` or `int` expression.
        let target = TypeInfo::new(SqlType::Int, true);
        match convert(value, ty, &target, None)? {
            Value::I32(seed) => reseed(seed),
            Value::Null => return Ok(Value::Null),
            _ => return Err(unexpected_conversion("int")),
        }
    }
    Ok(Value::F64(next_f64()))
}

/// Result type of `PI`: a `float` that is never `NULL`.
fn pi_return_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Float, false))
}

/// Evaluates `PI()`: the constant, to the precision a `float` holds.
fn pi_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::F64(std::f64::consts::PI))
}

thread_local! {
    /// State of the per-thread generator of `RAND`, seeded once on first use.
    ///
    /// One state per thread, like the generator of `NEWID` (`system.rs`): a query runs on
    /// the blocking pool and a shared state would need a lock for nothing. A seed given by
    /// the query replaces it, which is what makes two `RAND(1)` in one session agree.
    static RANDOM_STATE: Cell<u64> = Cell::new(seed_from_system());
}

/// Draws the initial state of the thread's generator from the system.
///
/// [`RandomState`] takes its entropy from the operating system; hashing the current time
/// and the thread identifier with it gives a value that differs between runs, between
/// threads, and between two processes started in the same instant.
fn seed_from_system() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut hasher = RandomState::new().build_hasher();
    nanos.hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    hasher.finish()
}

/// Replaces the state of the thread's generator with the seed of `RAND(seed)`.
///
/// The seed is stored as it is: SplitMix64 mixes its state at every draw, so two
/// neighbouring seeds do not give two neighbouring draws.
fn reseed(seed: i32) {
    let state = u64::from(seed as u32);
    RANDOM_STATE.with(|cell| cell.set(state));
}

/// Advances the thread's generator and returns 64 pseudo-random bits.
///
/// SplitMix64, written here because no random number generator is in the dependency
/// allow-list of the root `Cargo.toml`. It is **not** cryptographic: `RAND` needs values
/// that spread over the interval, not values nobody can predict.
fn next_u64() -> u64 {
    RANDOM_STATE.with(|state| {
        let mut z = state.get().wrapping_add(SPLITMIX_GAMMA);
        state.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

/// One draw in `[0, 1)`.
///
/// The 53 high bits of a draw divided by `2^53`: both are exact in a `float`, so the
/// quotient is exact, never negative and never reaches 1.
fn next_f64() -> f64 {
    let bits = next_u64() >> 11;
    // 53 bits, the significand of an `f64`: the cast loses nothing.
    bits as f64 / (1u64 << 53) as f64
}

// ---------------------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------------------

/// `ABS`: Microsoft Learn, "ABS (Transact-SQL)".
const ABS_DEF: FunctionDef = FunctionDef {
    name: "ABS",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: abs_return_type,
    eval: abs_eval,
    aggregate: None,
};

/// `CEILING`: Microsoft Learn, "CEILING (Transact-SQL)".
const CEILING_DEF: FunctionDef = FunctionDef {
    name: "CEILING",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: integral_return_type,
    eval: ceiling_eval,
    aggregate: None,
};

/// `FLOOR`: Microsoft Learn, "FLOOR (Transact-SQL)".
const FLOOR_DEF: FunctionDef = FunctionDef {
    name: "FLOOR",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: integral_return_type,
    eval: floor_eval,
    aggregate: None,
};

/// `ROUND`: Microsoft Learn, "ROUND (Transact-SQL)". Two or three arguments, the third
/// asking for a truncation.
const ROUND_DEF: FunctionDef = FunctionDef {
    name: "ROUND",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Range(2, 3),
    return_type: round_return_type,
    eval: round_eval,
    aggregate: None,
};

/// `POWER`: Microsoft Learn, "POWER (Transact-SQL)".
const POWER_DEF: FunctionDef = FunctionDef {
    name: "POWER",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: power_return_type,
    eval: power_eval,
    aggregate: None,
};

/// `SQRT`: Microsoft Learn, "SQRT (Transact-SQL)".
const SQRT_DEF: FunctionDef = FunctionDef {
    name: "SQRT",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: sqrt_return_type,
    eval: sqrt_eval,
    aggregate: None,
};

/// `SIGN`: Microsoft Learn, "SIGN (Transact-SQL)".
const SIGN_DEF: FunctionDef = FunctionDef {
    name: "SIGN",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: sign_return_type,
    eval: sign_eval,
    aggregate: None,
};

/// `RAND`: Microsoft Learn, "RAND (Transact-SQL)". The only non-deterministic function of
/// the module.
const RAND_DEF: FunctionDef = FunctionDef {
    name: "RAND",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(0, 1),
    return_type: rand_return_type,
    eval: rand_eval,
    aggregate: None,
};

/// `PI`: Microsoft Learn, "PI (Transact-SQL)".
const PI_DEF: FunctionDef = FunctionDef {
    name: "PI",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(0),
    return_type: pi_return_type,
    eval: pi_eval,
    aggregate: None,
};

/// Registers the nine mathematical functions in the global registry.
pub(crate) fn register_all() {
    register(ABS_DEF);
    register(CEILING_DEF);
    register(FLOOR_DEF);
    register(ROUND_DEF);
    register(POWER_DEF);
    register(SQRT_DEF);
    register(SIGN_DEF);
    register(RAND_DEF);
    register(PI_DEF);
}

#[cfg(test)]
mod tests;
