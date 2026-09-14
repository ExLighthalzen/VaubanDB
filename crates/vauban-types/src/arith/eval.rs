//! [`eval_binary`]: the value of a binary operation, once its type is known.
//!
//! `op_type` answers "what is the type of `1.5 * 2.25`?"; this module answers "what is
//! its value?". The two are read together: the scale of an exact result is the one
//! `binary_op_type` computed, never one this module recomputes, because two
//! implementations of the precision-and-scale table would end up disagreeing.
//!
//! **Precondition.** `a` and `b` have already been converted to their common type — the
//! caller does `implicit_result_type` then `convert` — and `out` is what `binary_op_type`
//! returned for that operator. A pair of operands from two different families is therefore
//! a broken precondition: it raises an internal error (50000), never a SQL error the user
//! could have caused.
//!
//! **Everything is exact.** No `f64` ever appears between two exact operands: `0.1` is not
//! representable there and the result would drift away from SQL Server. Integers compute
//! in `i64`, `money` in `i128` (its scale is 4), and `decimal` in the arbitrary-precision
//! integers of this module, because the exact product of two 38-digit mantissas has 76
//! digits and an `i128` stops at 38. See [`BigUint::times`] for the long multiplication
//! and [`BigUint::divided_by`] for the long division.
//!
//! **Rounding** is *half away from zero* everywhere it happens, never banker's rounding
//! and never `f64::round`: one rule, in [`BigInt::divided_rounded`], which
//! [`divide_rounded`] and [`rescaled`] both go through. The one operation that does *not*
//! round is the quotient of `/`, which is truncated towards zero: `SELECT 2.0 / 3.0;` is
//! `0.666666` and `SELECT CONVERT(varchar(40), CAST(2 AS money) / CAST(3 AS money), 2);`
//! is `0.6666` (`tests/eval_binary.rs`).

use std::cmp::Ordering;

use vauban_errors::{SqlError, SqlResult};

use crate::arith::BinaryOp;
use crate::errors;
use crate::sql_type::{Len, SqlType, TypeFamily, TypeInfo};
use crate::value::{DateTime, Decimal, SqlString, Value};

/// The scale of `money` and `smallmoney`: the amount is held in ten-thousandths.
const MONEY_SCALE: u32 = 4;

/// Ticks of 1/300 s in a day, the unit of [`DateTime::ticks_300th`].
const TICKS_PER_DAY: i64 = 25_920_000;

/// First day of `datetime`, 1753-01-01, counted from 1900-01-01 (see the unit test
/// `datetime_bounds_are_the_calendar_ones`, which recomputes it from `calendar`).
const DATETIME_MIN_DAYS: i32 = -53_690;

/// Last day of `datetime`, 9999-12-31, counted from 1900-01-01.
const DATETIME_MAX_DAYS: i32 = 2_958_463;

/// Last day of `smalldatetime`, 2079-06-06: the type stores its day count on two unsigned
/// bytes, so it starts at its own epoch, 1900-01-01.
const SMALLDATETIME_MAX_DAYS: i32 = 65_535;

/// The value of `a op b`, whose type is `out`.
///
/// # Preconditions
///
/// `a` and `b` are already of the common type of the operands (the caller runs
/// [`implicit_result_type`](crate::implicit_result_type) then
/// [`convert`](crate::convert)), and `out` is the [`TypeInfo`] that
/// [`binary_op_type`](crate::binary_op_type) returned for `op` on those two types. The
/// scale and the precision of an exact result, and the declared length of a
/// concatenation, are read off `out`; they are not recomputed here.
///
/// `NULL` wins over everything: if either operand is [`Value::Null`] the result is
/// [`Value::Null`], for the nine operators, [`BinaryOp::Concat`] included — `'a' + NULL`
/// is `NULL`, which is `CONCAT_NULL_YIELDS_NULL ON`, the only setting this engine
/// supports.
///
/// # Errors
///
/// - **8134**, `Division by zero.`, when the divisor of `/` or `%` is
///   zero, `float` included: SQL Server raises the error instead of producing an IEEE
///   infinity;
/// - **8115**, `Converting expression to data type int overflowed.`, when
///   the result does not fit in `out` — the word is `expression`, not a type name,
///   because what overflows is the result of a computation, and the number is 8115 for
///   every type, `tinyint` included, where a `CAST` would raise 220 or 232;
/// - **50000**, an internal error, when the two operands are not of the same family or
///   when `out` does not match them: the caller broke the precondition above.
///
/// # Examples
///
/// ```
/// use vauban_types::{BinaryOp, SqlType, TypeInfo, Value, eval_binary};
///
/// let int = TypeInfo::new(SqlType::Int, false);
/// let quotient = eval_binary(BinaryOp::Div, &Value::I32(-7), &Value::I32(2), &int)?;
/// assert_eq!(quotient, Value::I32(-3)); // integral division, truncated towards zero
/// # Ok::<(), vauban_errors::SqlError>(())
/// ```
pub fn eval_binary(op: BinaryOp, a: &Value, b: &Value, out: &TypeInfo) -> SqlResult<Value> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    match (op, a, b) {
        // `+` between two character or two binary operands concatenates, whichever of the
        // two spellings the binder chose (`op_type` accepts both as well).
        (BinaryOp::Concat | BinaryOp::Add, Value::String(x), Value::String(y)) => {
            concat_strings(x, y, out)
        }
        (BinaryOp::Concat | BinaryOp::Add, Value::Bytes(x), Value::Bytes(y)) => {
            concat_bytes(x, y, out)
        }
        (BinaryOp::Concat, _, _) => Err(incompatible(op, a, b)),
        (BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor, _, _) => {
            match (integer(a), integer(b)) {
                (Some(x), Some(y)) => eval_bitwise(op, x, y, out),
                _ => Err(incompatible(op, a, b)),
            }
        }
        (_, Value::Decimal(x), Value::Decimal(y)) => eval_decimal(op, x, y, out),
        (_, Value::Money(x), Value::Money(y)) => eval_money(op, *x, *y, out),
        (_, Value::DateTime(x), Value::DateTime(y)) => eval_datetime(op, *x, *y, out),
        _ => match (approximate(a), approximate(b)) {
            (Some(x), Some(y)) => eval_float(op, x, y, out),
            _ => match (integer(a), integer(b)) {
                (Some(x), Some(y)) => eval_integer(op, x, y, out),
                _ => Err(incompatible(op, a, b)),
            },
        },
    }
}

// ---------------------------------------------------------------------------------------
// Operands and results
// ---------------------------------------------------------------------------------------

/// The `i64` an integral operand carries, `bit` promoted to 0 or 1.
fn integer(v: &Value) -> Option<i64> {
    match v {
        Value::Bit(b) => Some(i64::from(*b)),
        Value::I8(n) => Some(i64::from(*n)),
        Value::I16(n) => Some(i64::from(*n)),
        Value::I32(n) => Some(i64::from(*n)),
        Value::I64(n) => Some(*n),
        _ => None,
    }
}

/// The `f64` an approximate operand carries; a `real` widens losslessly.
fn approximate(v: &Value) -> Option<f64> {
    match v {
        Value::F64(f) => Some(*f),
        Value::F32(f) => Some(f64::from(*f)),
        _ => None,
    }
}

/// The name of the SQL type `v` is a value of, for the internal error messages.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Bit(_) => "bit",
        Value::I8(_) => "tinyint",
        Value::I16(_) => "smallint",
        Value::I32(_) => "int",
        Value::I64(_) => "bigint",
        Value::Decimal(_) => "numeric",
        Value::F64(_) => "float",
        Value::F32(_) => "real",
        Value::Money(_) => "money",
        Value::String(_) => "varchar",
        Value::Bytes(_) => "varbinary",
        Value::Date(_) => "date",
        Value::Time(_) => "time",
        Value::DateTime(_) => "datetime",
        Value::DateTime2(_) => "datetime2",
        Value::DateTimeOffset(_) => "datetimeoffset",
        Value::Guid(_) => "uniqueidentifier",
    }
}

/// The broken precondition: two operands this module cannot combine.
///
/// Never a SQL error: the pairs a query can really write are refused earlier, by
/// `binary_op_type` (206, 257, 8117), and the caller converts both operands before
/// calling in.
fn incompatible(op: BinaryOp, a: &Value, b: &Value) -> SqlError {
    errors::bug(format!(
        "eval_binary: {op:?} between {} and {}: the caller converts both operands to \
         their common type first",
        kind(a),
        kind(b)
    ))
}

/// `out` is not a type this arithmetic can produce: another broken precondition.
fn wrong_result_type(op: BinaryOp, out: &TypeInfo) -> SqlError {
    errors::bug(format!(
        "eval_binary: {op:?} cannot produce a {}",
        out.ty.name()
    ))
}

/// Error 8115 on the result of a computation: the source is the word `expression`, not a
/// type name (`Converting expression to data type int.`) overflowed.
fn overflow(out: &TypeInfo) -> SqlError {
    errors::arithmetic_overflow("expression", &out.ty)
}

// ---------------------------------------------------------------------------------------
// Integers and bitwise operators
// ---------------------------------------------------------------------------------------

/// `a op b` on two integral operands, in `i64`.
///
/// Between two integers the quotient is integral and truncated towards zero, and the
/// remainder takes the sign of the dividend. Rust's `/` and `%` on signed integers do
/// exactly that, so `-7 / 2` is `-3` and `-7 % 2` is `-1` without any correction.
fn eval_integer(op: BinaryOp, x: i64, y: i64, out: &TypeInfo) -> SqlResult<Value> {
    let n = match op {
        BinaryOp::Add => x.checked_add(y),
        BinaryOp::Sub => x.checked_sub(y),
        BinaryOp::Mul => x.checked_mul(y),
        // `checked_div` also covers `i64::MIN / -1`, whose quotient is not an `i64`.
        BinaryOp::Div | BinaryOp::Mod => {
            if y == 0 {
                return Err(errors::divide_by_zero());
            }
            if matches!(op, BinaryOp::Div) {
                x.checked_div(y)
            } else {
                x.checked_rem(y)
            }
        }
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Concat => {
            return Err(wrong_result_type(op, out));
        }
    };
    let n = n.ok_or_else(|| overflow(out))?;
    integer_result(out, i128::from(n))
}

/// `a & b`, `a | b` or `a ^ b` on two integral operands, in `i64`.
///
/// The operands are integers or `bit`, and `bit & bit` stays a `bit`, which
/// [`integer_result`] rebuilds.
fn eval_bitwise(op: BinaryOp, x: i64, y: i64, out: &TypeInfo) -> SqlResult<Value> {
    let n = match op {
        BinaryOp::BitAnd => x & y,
        BinaryOp::BitOr => x | y,
        BinaryOp::BitXor => x ^ y,
        _ => return Err(wrong_result_type(op, out)),
    };
    integer_result(out, i128::from(n))
}

/// Rebuilds an integral result in the variant `out` calls for, checking its range.
fn integer_result(out: &TypeInfo, n: i128) -> SqlResult<Value> {
    let fitted = match out.ty {
        // Only the bitwise operators produce a `bit`, and their operands are 0 or 1.
        SqlType::Bit => return Ok(Value::Bit(n != 0)),
        SqlType::TinyInt => u8::try_from(n).ok().map(Value::I8),
        SqlType::SmallInt => i16::try_from(n).ok().map(Value::I16),
        SqlType::Int => i32::try_from(n).ok().map(Value::I32),
        SqlType::BigInt => i64::try_from(n).ok().map(Value::I64),
        ref other => {
            return Err(errors::bug(format!(
                "eval_binary: an integral result cannot be a {}",
                other.name()
            )));
        }
    };
    // Every integral overflow of an *operator* is 8115, the four types alike
    // (`integer_overflow_is_always_8115` in `tests/eval_binary.rs`): `SELECT CAST(255 AS
    // tinyint) + CAST(1 AS tinyint);` raises 8115 on `expression` and `tinyint`, not the
    // 220 a `CAST` would raise.
    fitted.ok_or_else(|| overflow(out))
}

// ---------------------------------------------------------------------------------------
// decimal and numeric
// ---------------------------------------------------------------------------------------

/// `a op b` on two `decimal` or `numeric` operands, exactly, at the scale of `out`.
///
/// The mantissas are exact integers, so the whole computation is: bring the operands to a
/// common scale (or, for a product, keep the natural scale `s1 + s2`), compute, then
/// [`rescaled`] to the scale of `out`. The precision of `out` is checked **last**, on the
/// final value: an intermediate wider than 38 digits is normal — `decimal(30, 20) *
/// decimal(30, 20)` has a 60-digit exact product and a `decimal(38, 17)` result
/// (`3.37500000000000000`), and the final value alone decides the overflow.
///
/// **A quotient truncates, every other reduction rounds** (`decimal_arithmetic` in
/// `tests/eval_binary.rs`). `SELECT 2.0 / 3.0;` is `0.666666` and `SELECT 5.0 / 7.0;`
/// `0.714285`, where a rounding would end in 7 and 6; a product or a sum that has to give
/// up digits rounds half away from zero, `SELECT CAST(1.00000000000000000600 AS
/// decimal(30,20)) * CAST(1 AS decimal(30,20));` being `1.00000000000000001`.
///
/// # Errors
///
/// 8134 when the divisor is zero, 8115 when the result needs more digits than the
/// precision of `out`.
fn eval_decimal(op: BinaryOp, a: &Decimal, b: &Decimal, out: &TypeInfo) -> SqlResult<Value> {
    let (precision, scale) = match out.ty {
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            (precision, scale)
        }
        _ => return Err(wrong_result_type(op, out)),
    };
    let x = BigInt::from_i128(a.mantissa);
    let y = BigInt::from_i128(b.mantissa);
    let common = a.scale.max(b.scale);
    let value = match op {
        BinaryOp::Add | BinaryOp::Sub => {
            let x = rescaled(&x, a.scale, common);
            let y = rescaled(&y, b.scale, common);
            let sum = if matches!(op, BinaryOp::Add) {
                x.plus(&y)
            } else {
                x.minus(&y)
            };
            rescaled(&sum, common, scale)
        }
        // The exact product is at scale `s1 + s2`, up to 76 digits: long multiplication,
        // then one rounding down to the scale of `out`.
        BinaryOp::Mul => rescaled(&x.times(&y), a.scale + b.scale, scale),
        BinaryOp::Div => {
            if b.mantissa == 0 {
                return Err(errors::divide_by_zero());
            }
            // `(m1 / 10^s1) / (m2 / 10^s2)` at the scale of `out` is
            // `m1 * 10^(scale + s2 - s1) / m2`: the dividend is scaled up *before* the
            // division, so that the quotient carries every digit `out` asks for. A
            // negative exponent scales the divisor instead, which is the same quotient.
            let e = i32::from(scale) + i32::from(b.scale) - i32::from(a.scale);
            let (dividend, divisor) = if e >= 0 {
                (x.times_pow10(e.unsigned_abs()), y)
            } else {
                (x, y.times_pow10(e.unsigned_abs()))
            };
            // The quotient is truncated towards zero, not rounded: see the doc comment.
            dividend.divided_by(&divisor).0
        }
        BinaryOp::Mod => {
            if b.mantissa == 0 {
                return Err(errors::divide_by_zero());
            }
            let x = rescaled(&x, a.scale, common);
            let y = rescaled(&y, b.scale, common);
            let (_, remainder) = x.divided_by(&y);
            rescaled(&remainder, common, scale)
        }
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Concat => {
            return Err(wrong_result_type(op, out));
        }
    };
    let mantissa = value.mantissa(precision).ok_or_else(|| overflow(out))?;
    Ok(Value::Decimal(Decimal {
        mantissa,
        precision,
        scale,
    }))
}

// ---------------------------------------------------------------------------------------
// money and smallmoney
// ---------------------------------------------------------------------------------------

/// `a op b` on two `money` or `smallmoney` operands.
///
/// `money` is an exact integer of ten-thousandths, so the four operations are the decimal
/// ones at scale 4: an addition is exact, a product divides by `10^4` **rounding** half
/// away from zero (`SELECT CONVERT(varchar(40), CAST(1.5 AS money) * CAST(0.0001 AS
/// money), 2);` is `0.0002`) and a quotient multiplies the dividend by `10^4` and
/// **truncates** (`CAST(2 AS money) / CAST(3 AS money)` is `0.6666`), the same split as
/// `decimal`. An `i128` is wide
/// enough for all of them (`i64 * i64` and `i64 * 10^4` both fit), so no long arithmetic
/// is needed here.
///
/// # Errors
///
/// 8134 when the divisor is zero, 8115 when the result leaves the range of `out` —
/// `money` spans the whole `i64` in ten-thousandths, `smallmoney` the whole `i32`.
fn eval_money(op: BinaryOp, x: i64, y: i64, out: &TypeInfo) -> SqlResult<Value> {
    let (x, y) = (i128::from(x), i128::from(y));
    let m = match op {
        // Two `i64` widened to `i128`: the sum and the difference cannot overflow.
        BinaryOp::Add => x + y,
        BinaryOp::Sub => x - y,
        BinaryOp::Mul => {
            divide_rounded(x * y, pow10_i128(MONEY_SCALE)).ok_or_else(|| overflow(out))?
        }
        BinaryOp::Div => {
            if y == 0 {
                return Err(errors::divide_by_zero());
            }
            // Truncated towards zero, like every T-SQL quotient; `i128` division already
            // truncates that way, and `x * 10^4` cannot overflow it.
            x * pow10_i128(MONEY_SCALE) / y
        }
        BinaryOp::Mod => {
            if y == 0 {
                return Err(errors::divide_by_zero());
            }
            x % y
        }
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Concat => {
            return Err(wrong_result_type(op, out));
        }
    };
    let fitted = match out.ty {
        SqlType::Money => i64::try_from(m).ok().map(Value::Money),
        SqlType::SmallMoney => i32::try_from(m).ok().map(|m| Value::Money(i64::from(m))),
        _ => return Err(wrong_result_type(op, out)),
    };
    fitted.ok_or_else(|| overflow(out))
}

// ---------------------------------------------------------------------------------------
// float and real
// ---------------------------------------------------------------------------------------

/// `a op b` on two approximate operands, in `f64`, narrowed to `f32` when `out` is `real`.
///
/// # Errors
///
/// 8134 when the divisor is zero — SQL Server raises the error where IEEE would return an
/// infinity — and 8115 when the result is infinite or NaN, or when it leaves the range of
/// `real`.
fn eval_float(op: BinaryOp, x: f64, y: f64, out: &TypeInfo) -> SqlResult<Value> {
    let v = match op {
        BinaryOp::Add => x + y,
        BinaryOp::Sub => x - y,
        BinaryOp::Mul => x * y,
        BinaryOp::Div | BinaryOp::Mod => {
            if y == 0.0 {
                return Err(errors::divide_by_zero());
            }
            if matches!(op, BinaryOp::Div) {
                x / y
            } else {
                x % y
            }
        }
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Concat => {
            return Err(wrong_result_type(op, out));
        }
    };
    if !v.is_finite() {
        return Err(overflow(out));
    }
    match out.ty {
        SqlType::Float => Ok(Value::F64(v)),
        SqlType::Real => {
            let narrowed = v as f32;
            if narrowed.is_finite() {
                Ok(Value::F32(narrowed))
            } else {
                Err(overflow(out))
            }
        }
        _ => Err(wrong_result_type(op, out)),
    }
}

// ---------------------------------------------------------------------------------------
// datetime and smalldatetime
// ---------------------------------------------------------------------------------------

/// `a + b` and `a - b` on two `datetime` or `smalldatetime` operands.
///
/// The two are added as numbers of days since their epoch: `CAST('2000-01-01' AS
/// datetime) + CAST('2000-01-01' AS datetime)` is day 73 048, a `datetime` of the year
/// 2100, while the same sum on two `smalldatetime` operands leaves the 65 535 days that
/// type can hold and raises **8115** on `expression` and `smalldatetime`.
///
/// Only `+` and `-` reach here: `binary_op_type` refuses `*`, `/` and `%` on a date with
/// error 257, and refuses `date`, `time`, `datetime2` and `datetimeoffset` outright. The
/// wider arithmetic on dates (`DATEADD`-like offsets by a number) is `sysfn`'s.
fn eval_datetime(op: BinaryOp, a: DateTime, b: DateTime, out: &TypeInfo) -> SqlResult<Value> {
    let ticks = |d: DateTime| i64::from(d.days) * TICKS_PER_DAY + i64::from(d.ticks_300th);
    let total = match op {
        BinaryOp::Add => ticks(a).checked_add(ticks(b)),
        BinaryOp::Sub => ticks(a).checked_sub(ticks(b)),
        _ => return Err(wrong_result_type(op, out)),
    }
    .ok_or_else(|| overflow(out))?;

    let (min, max) = match out.ty {
        SqlType::DateTime => (DATETIME_MIN_DAYS, DATETIME_MAX_DAYS),
        SqlType::SmallDateTime => (0, SMALLDATETIME_MAX_DAYS),
        _ => return Err(wrong_result_type(op, out)),
    };
    let days = i32::try_from(total.div_euclid(TICKS_PER_DAY))
        .ok()
        .filter(|days| (min..=max).contains(days))
        .ok_or_else(|| overflow(out))?;
    // `rem_euclid` is in `0..TICKS_PER_DAY`, which is far below `u32::MAX`.
    let ticks_300th = u32::try_from(total.rem_euclid(TICKS_PER_DAY))
        .map_err(|_| errors::bug("eval_binary: a time of day outside its own day"))?;
    Ok(Value::DateTime(DateTime { days, ticks_300th }))
}

// ---------------------------------------------------------------------------------------
// Concatenation
// ---------------------------------------------------------------------------------------

/// The declared length of a character or binary type, or `None` for `max`.
fn fixed_length(ty: &SqlType) -> Option<u16> {
    match ty {
        SqlType::Char(len)
        | SqlType::VarChar(len)
        | SqlType::NChar(len)
        | SqlType::NVarChar(len)
        | SqlType::Binary(len)
        | SqlType::VarBinary(len) => match len {
            Len::Fixed(n) => Some(*n),
            Len::Max => None,
        },
        _ => None,
    }
}

/// `a + b` on two character operands.
///
/// The result is truncated, silently, to the declared length `binary_op_type` computed;
/// that length is the sum of
/// the two operand lengths, capped at 8000 characters (4000 for the Unicode types), so a
/// truncation only happens when a shorter length was declared upstream.
fn concat_strings(a: &SqlString, b: &SqlString, out: &TypeInfo) -> SqlResult<Value> {
    if out.ty.family() != TypeFamily::Character {
        return Err(wrong_result_type(BinaryOp::Concat, out));
    }
    let mut text = String::with_capacity(a.text.len() + b.text.len());
    text.push_str(&a.text);
    text.push_str(&b.text);
    if let Some(limit) = fixed_length(&out.ty)
        && let Some((end, _)) = text.char_indices().nth(usize::from(limit))
    {
        text.truncate(end);
    }
    Ok(Value::String(SqlString { text }))
}

/// `a + b` on two binary operands: the bytes of one after the bytes of the other, cut at
/// the declared length of `out`.
fn concat_bytes(a: &[u8], b: &[u8], out: &TypeInfo) -> SqlResult<Value> {
    if out.ty.family() != TypeFamily::Binary {
        return Err(wrong_result_type(BinaryOp::Concat, out));
    }
    let mut bytes = Vec::with_capacity(a.len() + b.len());
    bytes.extend_from_slice(a);
    bytes.extend_from_slice(b);
    if let Some(limit) = fixed_length(&out.ty) {
        bytes.truncate(usize::from(limit));
    }
    Ok(Value::Bytes(bytes))
}

// ---------------------------------------------------------------------------------------
// Exact arithmetic beyond i128
// ---------------------------------------------------------------------------------------

/// Number of decimal digits one limb holds.
const LIMB_DIGITS: u32 = 9;

/// Base of the limbs, `10^9`: a product of two limbs plus a carry stays below `10^18` and
/// therefore inside a `u64`.
const LIMB_BASE: u64 = 1_000_000_000;

/// A non-negative integer of arbitrary size, little-endian limbs in base `10^9`.
///
/// A `decimal(38, s)` mantissa reaches `10^38 - 1`, an `i128` stops at about
/// `1.7 · 10^38`: the product of two mantissas therefore overflows an `i128` every time,
/// and so does a dividend scaled up before a division. This type carries those
/// intermediates exactly. Base `10^9` rather than a power of two because every scaling
/// this module needs is a power of ten, which is then a limb shift plus one small
/// multiplication.
///
/// The representation is normalised: no zero limb on top, so zero is the empty vector and
/// `PartialEq` is value equality.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BigUint {
    /// Limbs, least significant first, each below [`LIMB_BASE`].
    limbs: Vec<u32>,
}

impl Ord for BigUint {
    fn cmp(&self, other: &Self) -> Ordering {
        // Normalised, so the longer number is the larger one.
        self.limbs
            .len()
            .cmp(&other.limbs.len())
            .then_with(|| self.limbs.iter().rev().cmp(other.limbs.iter().rev()))
    }
}

impl PartialOrd for BigUint {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl BigUint {
    /// The number zero.
    fn zero() -> Self {
        Self { limbs: Vec::new() }
    }

    /// Whether the number is zero.
    fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// `v`, split into limbs.
    fn from_u128(mut v: u128) -> Self {
        let base = u128::from(LIMB_BASE);
        let mut limbs = Vec::new();
        while v > 0 {
            // The remainder of a division by `10^9` fits in a `u32`.
            limbs.push((v % base) as u32);
            v /= base;
        }
        Self { limbs }
    }

    /// The number as a `u128`, or `None` when it needs more than 128 bits.
    fn to_u128(&self) -> Option<u128> {
        let mut acc: u128 = 0;
        for limb in self.limbs.iter().rev() {
            acc = acc
                .checked_mul(u128::from(LIMB_BASE))?
                .checked_add(u128::from(*limb))?;
        }
        Some(acc)
    }

    /// Limb `i`, or zero beyond the end.
    fn limb(&self, i: usize) -> u64 {
        self.limbs.get(i).copied().map_or(0, u64::from)
    }

    /// Drops the zero limbs on top, so that the representation stays normalised.
    fn trimmed(mut self) -> Self {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
        self
    }

    /// `self + other`.
    fn plus(&self, other: &Self) -> Self {
        let width = self.limbs.len().max(other.limbs.len());
        let mut limbs = Vec::with_capacity(width + 1);
        let mut carry = 0u64;
        for i in 0..width {
            let sum = carry + self.limb(i) + other.limb(i);
            limbs.push((sum % LIMB_BASE) as u32);
            carry = sum / LIMB_BASE;
        }
        if carry > 0 {
            limbs.push(carry as u32);
        }
        Self { limbs }.trimmed()
    }

    /// `self - other`, which callers only ask for when `self >= other`; an underflow
    /// gives zero rather than a panic.
    fn minus(&self, other: &Self) -> Self {
        let mut limbs = Vec::with_capacity(self.limbs.len());
        let mut borrow = 0i64;
        for i in 0..self.limbs.len() {
            let mut digit = self.limb(i) as i64 - other.limb(i) as i64 - borrow;
            borrow = i64::from(digit < 0);
            if digit < 0 {
                digit += LIMB_BASE as i64;
            }
            limbs.push(digit as u32);
        }
        if borrow != 0 {
            return Self::zero();
        }
        Self { limbs }.trimmed()
    }

    /// `self * other`, by **long multiplication**: the schoolbook algorithm, limb by limb,
    /// with a `u64` accumulator that a product of two limbs plus a carry never overflows.
    ///
    /// Exact whatever the size of the operands, which is the point: the product of two
    /// 38-digit mantissas has 76 digits, and reducing it to the scale of the result comes
    /// **after** the exact product, never before.
    fn times(&self, other: &Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        let mut limbs = vec![0u32; self.limbs.len() + other.limbs.len()];
        for (i, x) in self.limbs.iter().enumerate() {
            let mut carry = 0u64;
            for (j, y) in other.limbs.iter().enumerate() {
                let current = u64::from(limbs[i + j]) + u64::from(*x) * u64::from(*y) + carry;
                limbs[i + j] = (current % LIMB_BASE) as u32;
                carry = current / LIMB_BASE;
            }
            let mut k = i + other.limbs.len();
            while carry > 0 {
                let current = u64::from(limbs[k]) + carry;
                limbs[k] = (current % LIMB_BASE) as u32;
                carry = current / LIMB_BASE;
                k += 1;
            }
        }
        Self { limbs }.trimmed()
    }

    /// `self * m`, where `m` is below [`LIMB_BASE`].
    fn times_small(&self, m: u32) -> Self {
        self.times(&Self::from_u128(u128::from(m)))
    }

    /// `self * 10^k`: `k / 9` limbs of shift, then one small multiplication.
    fn times_pow10(&self, k: u32) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        let mut limbs = vec![0u32; (k / LIMB_DIGITS) as usize];
        limbs.extend_from_slice(&self.limbs);
        let shifted = Self { limbs };
        match k % LIMB_DIGITS {
            0 => shifted,
            rest => shifted.times_small(10u32.pow(rest)),
        }
    }

    /// The quotient and the remainder of `self / divisor`, by **long division** on
    /// nine-digit limbs.
    ///
    /// Each quotient limb is found by binary search over `0..10^9` — thirty comparisons
    /// against `divisor * digit` — which needs neither digit estimation nor
    /// normalisation, and is exact by construction. The numbers here are a handful of
    /// limbs, so the cost does not matter.
    ///
    /// A zero divisor gives `(0, 0)`; callers raise 8134 before they get here.
    fn divided_by(&self, divisor: &Self) -> (Self, Self) {
        if divisor.is_zero() {
            return (Self::zero(), Self::zero());
        }
        let mut quotient = vec![0u32; self.limbs.len()];
        let mut remainder = Self::zero();
        for i in (0..self.limbs.len()).rev() {
            // remainder = remainder * 10^9 + limb i, which is a limb shift by one.
            let mut limbs = Vec::with_capacity(remainder.limbs.len() + 1);
            limbs.push(self.limbs[i]);
            limbs.extend_from_slice(&remainder.limbs);
            remainder = Self { limbs }.trimmed();

            let (mut low, mut high) = (0u32, (LIMB_BASE - 1) as u32);
            while low < high {
                let middle = low + (high - low).div_ceil(2);
                if divisor.times_small(middle) <= remainder {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            quotient[i] = low;
            remainder = remainder.minus(&divisor.times_small(low));
        }
        (Self { limbs: quotient }.trimmed(), remainder)
    }
}

/// A signed exact integer: a sign and a magnitude.
///
/// Sign and magnitude rather than two's complement because every rule of T-SQL
/// arithmetic is written on the magnitude — rounding is *away from zero*, a remainder
/// takes the sign of the dividend, a quotient truncates *towards zero*.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BigInt {
    /// Whether the number is strictly negative; zero is never negative.
    negative: bool,
    /// The absolute value.
    magnitude: BigUint,
}

impl BigInt {
    /// `magnitude`, negated when `negative`; zero is normalised to a positive zero.
    fn signed(negative: bool, magnitude: BigUint) -> Self {
        Self {
            negative: negative && !magnitude.is_zero(),
            magnitude,
        }
    }

    /// `v` as a signed big integer.
    fn from_i128(v: i128) -> Self {
        Self::signed(v < 0, BigUint::from_u128(v.unsigned_abs()))
    }

    /// The number as an `i128`, or `None` when it does not fit.
    fn to_i128(&self) -> Option<i128> {
        let magnitude = i128::try_from(self.to_u128()?).ok()?;
        Some(if self.negative { -magnitude } else { magnitude })
    }

    /// The magnitude as a `u128`, or `None` when it does not fit.
    fn to_u128(&self) -> Option<u128> {
        self.magnitude.to_u128()
    }

    /// `-self`.
    fn negated(&self) -> Self {
        Self::signed(!self.negative, self.magnitude.clone())
    }

    /// `self + other`.
    fn plus(&self, other: &Self) -> Self {
        if self.negative == other.negative {
            Self::signed(self.negative, self.magnitude.plus(&other.magnitude))
        } else if self.magnitude >= other.magnitude {
            Self::signed(self.negative, self.magnitude.minus(&other.magnitude))
        } else {
            Self::signed(other.negative, other.magnitude.minus(&self.magnitude))
        }
    }

    /// `self - other`.
    fn minus(&self, other: &Self) -> Self {
        self.plus(&other.negated())
    }

    /// `self * other`, by long multiplication ([`BigUint::times`]).
    fn times(&self, other: &Self) -> Self {
        Self::signed(
            self.negative != other.negative,
            self.magnitude.times(&other.magnitude),
        )
    }

    /// `self * 10^k`.
    fn times_pow10(&self, k: u32) -> Self {
        Self::signed(self.negative, self.magnitude.times_pow10(k))
    }

    /// The quotient **truncated towards zero** and the remainder, whose sign is the sign
    /// of the dividend: the rule of T-SQL's `/` and `%` (`-7 / 2 = -3`, `-7 % 2 = -1`).
    fn divided_by(&self, other: &Self) -> (Self, Self) {
        let (quotient, remainder) = self.magnitude.divided_by(&other.magnitude);
        (
            Self::signed(self.negative != other.negative, quotient),
            Self::signed(self.negative, remainder),
        )
    }

    /// The quotient rounded **half away from zero**, the only rounding rule of this
    /// module: `2.5` rounds to `3` and `-2.5` to `-3`, never to the even neighbour.
    fn divided_rounded(&self, other: &Self) -> Self {
        let (quotient, remainder) = self.magnitude.divided_by(&other.magnitude);
        let magnitude = if remainder.times_small(2) >= other.magnitude {
            quotient.plus(&BigUint::from_u128(1))
        } else {
            quotient
        };
        Self::signed(self.negative != other.negative, magnitude)
    }

    /// The value as a mantissa of `precision` digits, or `None` when it needs more:
    /// `decimal(p, s)` holds a mantissa strictly below `10^p`.
    fn mantissa(&self, precision: u8) -> Option<i128> {
        let limit = BigUint::from_u128(1).times_pow10(u32::from(precision));
        if self.magnitude >= limit {
            return None;
        }
        self.to_i128()
    }
}

/// `10^k` as an `i128`, for the scales this module handles (`k <= 4`).
fn pow10_i128(k: u32) -> i128 {
    10i128.pow(k)
}

/// `n / d`, rounded **half away from zero**, or `None` when the divisor is zero or the
/// quotient does not fit in an `i128`.
///
/// The single entry point of the rounding rule for the values that fit in an `i128`
/// (`money`): it delegates to [`BigInt::divided_rounded`], so there is one implementation
/// of the rule and not two. A T-SQL quotient does not go through it — `/` truncates.
fn divide_rounded(n: i128, d: i128) -> Option<i128> {
    if d == 0 {
        return None;
    }
    BigInt::from_i128(n)
        .divided_rounded(&BigInt::from_i128(d))
        .to_i128()
}

/// Moves the exact value `value / 10^from_scale` to the scale `to_scale`, rounding half
/// away from zero when the scale shrinks and multiplying exactly when it grows.
fn rescaled(value: &BigInt, from_scale: u8, to_scale: u8) -> BigInt {
    if to_scale >= from_scale {
        value.times_pow10(u32::from(to_scale - from_scale))
    } else {
        let divisor = BigInt::from_i128(1).times_pow10(u32::from(from_scale - to_scale));
        value.divided_rounded(&divisor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{DAYS_1900, days_from_civil};

    /// The three day counts the `datetime` range check uses, recomputed from the
    /// calendar: 1753-01-01 and 9999-12-31 for `datetime`, 2079-06-06 for
    /// `smalldatetime`, whose two unsigned bytes of days start at 1900-01-01.
    #[test]
    fn datetime_bounds_are_the_calendar_ones() {
        assert_eq!(days_from_civil(1753, 1, 1) - DAYS_1900, DATETIME_MIN_DAYS);
        assert_eq!(days_from_civil(9999, 12, 31) - DAYS_1900, DATETIME_MAX_DAYS);
        assert_eq!(
            days_from_civil(2079, 6, 6) - DAYS_1900,
            SMALLDATETIME_MAX_DAYS
        );
    }

    /// The long multiplication against the `i128` one, on values small enough for both.
    #[test]
    fn long_multiplication_agrees_with_i128() {
        let vectors = [
            (0i128, 0i128),
            (1, 1),
            (999_999_999, 999_999_999),
            (1_000_000_000, 1_000_000_000),
            (-123_456_789_012_345, 987_654_321),
            (170_141_183_460_469_231, -1),
            (12_345_678_901_234_567_890, 1_000_000_007),
        ];
        for (x, y) in vectors {
            let product = BigInt::from_i128(x).times(&BigInt::from_i128(y));
            assert_eq!(product.to_i128(), Some(x * y), "{x} * {y}");
        }
    }

    /// A product wider than an `i128`, checked digit by digit: `(10^38 - 1)^2` is
    /// `10^76 - 2 · 10^38 + 1`, whose decimal form is 37 nines, then an 8, then 37 zeros,
    /// then a 1.
    #[test]
    fn long_multiplication_goes_beyond_i128() {
        let max = BigUint::from_u128(1)
            .times_pow10(38)
            .minus(&BigUint::from_u128(1));
        let square = max.times(&max);
        assert_eq!(
            square.to_u128(),
            None,
            "the square needs more than 128 bits"
        );

        let expected = BigUint::from_u128(1)
            .times_pow10(76)
            .minus(&BigUint::from_u128(2).times_pow10(38))
            .plus(&BigUint::from_u128(1));
        assert_eq!(square, expected);
    }

    /// The long division against the `i128` one, quotient and remainder, signs included.
    #[test]
    fn long_division_agrees_with_i128() {
        let vectors = [
            (7i128, 2i128),
            (-7, 2),
            (7, -2),
            (-7, -2),
            (0, 5),
            (1_000_000_000_000_000_000, 999_999_999),
            (i128::MAX, 3),
            (-123_456_789_012_345_678, 1_000_003),
        ];
        for (n, d) in vectors {
            let (quotient, remainder) = BigInt::from_i128(n).divided_by(&BigInt::from_i128(d));
            assert_eq!(quotient.to_i128(), Some(n / d), "{n} / {d}");
            assert_eq!(remainder.to_i128(), Some(n % d), "{n} % {d}");
        }
    }

    /// The rounding is half away from zero, on both signs, and never banker's rounding.
    #[test]
    fn rounding_is_half_away_from_zero() {
        assert_eq!(divide_rounded(25, 10), Some(3));
        assert_eq!(divide_rounded(-25, 10), Some(-3));
        assert_eq!(divide_rounded(35, 10), Some(4));
        assert_eq!(divide_rounded(-35, 10), Some(-4));
        assert_eq!(divide_rounded(24, 10), Some(2));
        assert_eq!(divide_rounded(-24, 10), Some(-2));
        assert_eq!(divide_rounded(1, 0), None);
    }

    /// A quotient that needs more than 128 bits is refused rather than wrapped.
    #[test]
    fn a_quotient_beyond_i128_does_not_fit() {
        let huge = BigInt::from_i128(1).times_pow10(60);
        assert_eq!(huge.to_i128(), None);
        assert_eq!(huge.mantissa(38), None);
        assert_eq!(BigInt::from_i128(-50).mantissa(1), None);
        assert_eq!(BigInt::from_i128(-50).mantissa(2), Some(-50));
    }

    /// `rescaled` grows exactly and shrinks with the rounding rule.
    #[test]
    fn rescaling_rounds_only_when_it_shrinks() {
        let value = BigInt::from_i128(12_345);
        assert_eq!(rescaled(&value, 2, 4).to_i128(), Some(1_234_500));
        assert_eq!(rescaled(&value, 2, 2).to_i128(), Some(12_345));
        assert_eq!(rescaled(&value, 2, 1).to_i128(), Some(1_235));
        assert_eq!(rescaled(&value.negated(), 2, 1).to_i128(), Some(-1_235));
    }
}
