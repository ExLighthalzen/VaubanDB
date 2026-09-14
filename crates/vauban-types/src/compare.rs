//! Three-valued comparison of [`Value`]s under the default collation.
//!
//! This is the only place in the engine that compares two SQL values: `storage` uses it
//! to order index keys and detect duplicates. Conversions between type families are not
//! done here (the caller converts both sides to a common type first).

use std::cmp::Ordering;
use std::iter::repeat_n;

use vauban_errors::{InternalError, SqlResult};

use crate::{Collation, DateTime2, Decimal, Value};

/// Compares two values the way SQL Server does, with three-valued logic.
///
/// Returns:
/// - `Ok(None)` if `a` or `b` is [`Value::Null`], whatever the other operand is (`NULL`
///   compared to anything is `UNKNOWN`);
/// - `Ok(Some(ordering))` if both values belong to the same family (table below);
/// - `Err` (an [`InternalError::Bug`] converted to a `SqlError` number `50000`) if the two
///   values belong to different families. This is a broken precondition, not a SQL error:
///   the caller must have converted both sides to a common type beforehand
///   (`implicit_result_type` and `convert`). `None` is reserved to `NULL` and
///   never signals an incompatibility.
///
/// # Families
///
/// | Family | Variants | Rule |
/// |---|---|---|
/// | boolean | `Bit` | `false < true` |
/// | integers | `I8`, `I16`, `I32`, `I64` | widened to `i64`, so `I8(255) > I16(3)` |
/// | decimals | `Decimal` | numeric value, whatever the precision and scale |
/// | money | `Money` | the `i64` amount in ten-thousandths |
/// | floats | `F32`, `F64` | widened to `f64` |
/// | strings | `String` | [`Collation::compare`] |
/// | bytes | `Bytes` | byte by byte, the shorter padded with `0x00` (`0x01 = 0x0100`) |
/// | guid | `Guid` | the bytes in the order SQL Server compares them ([`GUID_ORDER`]) |
/// | date | `Date` | days |
/// | time | `Time` | ticks since midnight |
/// | datetime | `DateTime` | `(days, ticks_300th)` |
/// | datetime2 | `DateTime2` | `(date, time)` |
/// | datetimeoffset | `DateTimeOffset` | the `utc` instant only; the offset is ignored |
///
/// Every other pair (`I32` against `Decimal`, `String` against `Bytes`, `DateTime`
/// against `DateTime2`, ...) is an error. `collation` is ignored for non-string values.
///
/// # Strings
///
/// Two strings are handed to [`Collation::compare`], which holds the whole rule: trailing
/// spaces ignored, primary weights of code page 1252 first, secondary weights (the
/// accents) next, case never (`_CI`). Its documentation also states the two limits of
/// that rule: each collation is compared like the default one, and the order is the
/// `varchar` one even for `nvarchar` data.
///
/// # Guids
///
/// `uniqueidentifier` values are ordered by groups of the textual form, from the last
/// group to the first, and inside each group by the **stored** bytes read from the first
/// to the last, never by the numeric value of the group. [`GUID_ORDER`] is that reading
/// order (`guid_comparison_order` in `tests/convert_binary.rs`).
///
/// # A provisional rule
///
/// - `Bytes`: the `0x00` padding reproduces the documented behaviour of `binary`.
///
/// # Examples
///
/// ```
/// use std::cmp::Ordering;
/// use vauban_types::{compare, Collation, SqlString, Value};
///
/// let text = |t: &str| Value::String(SqlString { text: t.to_owned() });
///
/// assert_eq!(compare(&Value::Null, &Value::I32(1), &Collation::DEFAULT), Ok(None));
/// assert_eq!(
///     compare(&Value::I8(255), &Value::I64(3), &Collation::DEFAULT),
///     Ok(Some(Ordering::Greater))
/// );
/// assert_eq!(
///     compare(&text("abc  "), &text("ABC"), &Collation::DEFAULT),
///     Ok(Some(Ordering::Equal))
/// );
/// assert!(compare(&Value::I32(1), &Value::F64(1.0), &Collation::DEFAULT).is_err());
/// ```
pub fn compare(a: &Value, b: &Value, collation: &Collation) -> SqlResult<Option<Ordering>> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(None);
    }
    if let (Some(x), Some(y)) = (integer_as_i64(a), integer_as_i64(b)) {
        return Ok(Some(x.cmp(&y)));
    }
    if let (Some(x), Some(y)) = (float_as_f64(a), float_as_f64(b)) {
        return compare_floats(x, y).map(Some);
    }
    let ordering = match (a, b) {
        (Value::Bit(x), Value::Bit(y)) => x.cmp(y),
        (Value::Decimal(x), Value::Decimal(y)) => compare_decimals(x, y)?,
        (Value::Money(x), Value::Money(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => collation.compare(&x.text, &y.text),
        (Value::Bytes(x), Value::Bytes(y)) => compare_bytes(x, y),
        (Value::Guid(x), Value::Guid(y)) => compare_guids(x, y),
        (Value::Date(x), Value::Date(y)) => x.days.cmp(&y.days),
        (Value::Time(x), Value::Time(y)) => x.ticks_100ns.cmp(&y.ticks_100ns),
        (Value::DateTime(x), Value::DateTime(y)) => {
            (x.days, x.ticks_300th).cmp(&(y.days, y.ticks_300th))
        }
        (Value::DateTime2(x), Value::DateTime2(y)) => compare_datetime2(x, y),
        (Value::DateTimeOffset(x), Value::DateTimeOffset(y)) => compare_datetime2(&x.utc, &y.utc),
        _ => {
            return Err(bug(format!(
                "compare: incompatible value families {} and {}",
                variant_name(a),
                variant_name(b)
            )));
        }
    };
    Ok(Some(ordering))
}

/// Builds the internal error every precondition failure of this module reports.
fn bug(message: String) -> vauban_errors::SqlError {
    InternalError::Bug(message).into()
}

/// The integer families widened to `i64`; `None` for any other variant.
fn integer_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::I8(x) => Some(i64::from(*x)),
        Value::I16(x) => Some(i64::from(*x)),
        Value::I32(x) => Some(i64::from(*x)),
        Value::I64(x) => Some(*x),
        _ => None,
    }
}

/// The float families widened to `f64`; `None` for any other variant.
fn float_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::F32(x) => Some(f64::from(*x)),
        Value::F64(x) => Some(*x),
        _ => None,
    }
}

/// `partial_cmp` is `None` only for NaN, which SQL Server never produces: that is a bug,
/// not a `NULL`.
fn compare_floats(x: f64, y: f64) -> SqlResult<Ordering> {
    x.partial_cmp(&y)
        .ok_or_else(|| bug("compare: NaN in float comparison".to_owned()))
}

/// Compares two decimals by numeric value without overflowing `i128`.
///
/// Signs first, then integer parts, then fractional parts aligned on the larger scale:
/// a fractional part is `< 10^scale`, so once multiplied by `10^(max_scale - scale)` it
/// stays `< 10^max_scale <= 10^38`, which fits in a `u128`.
fn compare_decimals(x: &Decimal, y: &Decimal) -> SqlResult<Ordering> {
    let sign_x = x.mantissa.signum();
    let sign_y = y.mantissa.signum();
    if sign_x != sign_y {
        return Ok(sign_x.cmp(&sign_y));
    }
    if sign_x == 0 {
        return Ok(Ordering::Equal);
    }
    let magnitude = compare_decimal_magnitudes(x, y)?;
    Ok(if sign_x < 0 {
        magnitude.reverse()
    } else {
        magnitude
    })
}

/// Compares the absolute values of two decimals.
fn compare_decimal_magnitudes(x: &Decimal, y: &Decimal) -> SqlResult<Ordering> {
    let max_scale = x.scale.max(y.scale);
    let (int_x, frac_x) = split_decimal(x, max_scale)?;
    let (int_y, frac_y) = split_decimal(y, max_scale)?;
    Ok(int_x.cmp(&int_y).then(frac_x.cmp(&frac_y)))
}

/// Splits `|d|` into its integer part and its fractional part rescaled to `max_scale`.
fn split_decimal(d: &Decimal, max_scale: u8) -> SqlResult<(u128, u128)> {
    let abs = d.mantissa.unsigned_abs();
    let scale_factor = pow10(d.scale)?;
    let align_factor = pow10(max_scale - d.scale)?;
    let integer = abs / scale_factor;
    let fraction = (abs % scale_factor)
        .checked_mul(align_factor)
        .ok_or_else(|| bug(format!("compare: decimal scale {} out of range", d.scale)))?;
    Ok((integer, fraction))
}

/// `10^exp` as a `u128`; an error past 38, the largest scale a `decimal` can have.
fn pow10(exp: u8) -> SqlResult<u128> {
    10u128
        .checked_pow(u32::from(exp))
        .ok_or_else(|| bug(format!("compare: decimal scale {exp} out of range")))
}

/// The 16 stored bytes of a `uniqueidentifier` in the order SQL Server compares them: the
/// last group of the textual form first, then the fourth, the third, the second and the
/// first, each read from its first stored byte to its last.
///
/// The first three groups are stored little-endian ([`Value::Guid`]), so reading them in
/// storage order is **not** reading their numeric value:
/// `CAST('01000000-0000-0000-0000-000000000000' AS uniqueidentifier) <
/// CAST('00000100-0000-0000-0000-000000000000' AS uniqueidentifier)` being true although
/// `0x01000000` is the larger of the two first groups.
const GUID_ORDER: [usize; 16] = [10, 11, 12, 13, 14, 15, 8, 9, 6, 7, 4, 5, 0, 1, 2, 3];

/// `uniqueidentifier` comparison: the stored bytes taken in [`GUID_ORDER`].
fn compare_guids(x: &[u8; 16], y: &[u8; 16]) -> Ordering {
    for index in GUID_ORDER {
        let ordering = x[index].cmp(&y[index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// `binary` comparison: byte by byte, the shorter operand padded with `0x00`.
fn compare_bytes(a: &[u8], b: &[u8]) -> Ordering {
    let len = a.len().max(b.len());
    let padded_a = a.iter().copied().chain(repeat_n(0u8, len - a.len()));
    let padded_b = b.iter().copied().chain(repeat_n(0u8, len - b.len()));
    padded_a.cmp(padded_b)
}

/// `datetime2` as `(date.days, time.ticks_100ns)`.
fn compare_datetime2(x: &DateTime2, y: &DateTime2) -> Ordering {
    (x.date.days, x.time.ticks_100ns).cmp(&(y.date.days, y.time.ticks_100ns))
}

/// The variant name, for error messages.
fn variant_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bit(_) => "Bit",
        Value::I8(_) => "I8",
        Value::I16(_) => "I16",
        Value::I32(_) => "I32",
        Value::I64(_) => "I64",
        Value::Decimal(_) => "Decimal",
        Value::F64(_) => "F64",
        Value::F32(_) => "F32",
        Value::Money(_) => "Money",
        Value::String(_) => "String",
        Value::Bytes(_) => "Bytes",
        Value::Date(_) => "Date",
        Value::Time(_) => "Time",
        Value::DateTime(_) => "DateTime",
        Value::DateTime2(_) => "DateTime2",
        Value::DateTimeOffset(_) => "DateTimeOffset",
        Value::Guid(_) => "Guid",
    }
}
