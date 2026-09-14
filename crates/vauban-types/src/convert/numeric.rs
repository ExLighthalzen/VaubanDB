//! Conversions towards `bit`, the integer types, `decimal`/`numeric`, `money` and `float`.
//!
//! **One computation path.** Every exact source — `bit`, the integer types, `decimal`,
//! `numeric`, `money` — is read as a pair (128-bit mantissa, scale), rescaled once, then
//! checked against the range of the target. `money` is exactly a scale of 4. An exact
//! source never goes through `f64`: `0.1` is not representable there and the result would
//! drift away from SQL Server. Only `float`, `real` and `datetime` take the approximate
//! path.
//!
//! **Rounding and truncation**: `numeric` → `numeric` rounds,
//! `numeric` → integer **truncates**, `numeric` → `money` rounds, `money` → integer
//! rounds, `money` → `numeric` rounds, `float` → integer truncates, `float` → `numeric`
//! rounds, `datetime` → integer rounds. Every rounding to an exact type is *half away from
//! zero*, never banker's rounding: `CAST(2.5 AS numeric(2,0))` is 3.
//!
//! **Between exact and approximate, the rounding is correct to the last bit**, in both
//! directions, on the **exact** value and in one step:
//!
//! * an exact source becomes the `float` nearest to `m / 10^s`, ties to the even
//!   significand — IEEE, not half away from zero: `9007199254740993` (2^53 + 1) is
//!   `9007199254740992`; and it becomes the `real` nearest to the same exact value, never
//!   the `real` nearest to the `float` — `8388608.5000000001` is `8388609`, where a second
//!   rounding from the double `8388608.5` would give `8388608`;
//! * a `float` or `real` source is expanded in decimal **exactly** and rounded half away
//!   from zero at the scale of the target, `numeric(p,s)` or the four decimals of `money`:
//!   `CAST(CAST(0.1 AS float) AS numeric(38,37))` is `0.1000000000000000055511151231257827021`,
//!   `CAST(1e23 AS numeric(38,0))` is `99999999999999991611392`, `0.125` in `numeric(3,2)`
//!   is `0.13` and `0.03125` in `money` is `0.0313`.
//!
//! Both rules hold across the number of digits, the scale, the sign, the declared
//! precision and the distance to the nearest rounding boundary
//! (`tests/convert_numeric.rs`). The naive arithmetic, `m as f64 / 10f64.powi(s)` and
//! `(v * 10f64.powi(s)).round()`, diverges on those axes, and one of its divergences
//! moves a `datetime` by three milliseconds three conversions later.
//!
//! **Which overflow error** depends on the pair of types:
//!
//! | target | source | error |
//! |---|---|---|
//! | integer | `bit`, `tinyint`, `smallint`, `int` | 220, which quotes the value |
//! | integer | `bigint` | 8115, see [`integer_overflow`] |
//! | integer | `decimal`, `numeric`, `datetime` | 8115 |
//! | `tinyint`, `smallint`, `int` | `float`, `real` | 232, which quotes the value |
//! | `bigint` | `float`, `real` | 8115, see [`approx_overflow`] |
//! | integer | `money`, `smallmoney` | 237, 232, 220 or 8115, see [`money_overflow`] |
//! | `decimal`, `numeric` | any | 8115 |
//! | `money` | `decimal`, `numeric`, integer, `datetime` | 8115 |
//! | `money` | `money` | 237 |
//! | `money` | `float`, `real` | 232 |
//! | `real` | `float` | 232 |
//!
//! Message 8115 names either the source type or the word `expression`, see
//! [`overflow_source_name`]. Its state, and the states of 220 and 232, follow the pair of
//! types; the tables live in the `errors` crate, and this module passes the source type
//! along.

use vauban_errors::{SqlError, SqlResult};

use crate::convert::from_character;
use crate::errors;
use crate::{Decimal, Len, SqlType, TypeFamily, TypeInfo, Value};

/// The scale of `money` and `smallmoney`: the amount is stored in ten-thousandths.
const MONEY_SCALE: u8 = 4;

/// Ticks of 1/300 s in a day, the unit of [`crate::DateTime::ticks_300th`].
const TICKS_PER_DAY: f64 = 25_920_000.0;

/// `2^127` as an `f64`, the exclusive bound of an `i128`.
///
/// `i128::MAX as f64` rounds up to this very value, so a comparison against it is the only
/// safe way to reject an overflow before a cast: a `as` that overflows saturates silently
/// in Rust and would hide the error.
const TWO_POW_127: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;

/// The unit bit of a double's significand is `2^(biased exponent - 1075)`: below this
/// exponent the value has a fractional part, and `1075 - exponent` bits of it.
const F64_UNIT_BIAS: u32 = 1075;

/// The bits of the biased exponent of a double, above the 52 bits of its significand.
const F64_EXPONENT_MASK: u64 = 0x7FF;

/// The 52 explicit bits of the significand of a double.
const F64_SIGNIFICAND_MASK: u64 = (1 << 52) - 1;

/// The family of an exact source, which decides how an integer target rounds and which
/// error a target overflow raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExactSource {
    /// `bit` and the integer types: scale 0, an integer overflow is error 220 — 8115 when
    /// the source is a `bigint`, see [`integer_overflow`].
    Integral,
    /// `decimal(p, s)` and `numeric(p, s)`: an integer target truncates, overflow is 8115.
    Numeric,
    /// `money` and `smallmoney`: an integer target rounds, overflow is 8115.
    Money,
}

/// The family of an approximate source, which decides how an integer target rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApproxSource {
    /// `float` and `real`: an integer target truncates, overflow is error 232 — except
    /// towards `bigint`, which answers 8115, see [`approx_overflow`].
    Float,
    /// `datetime` and `smalldatetime`, as days since 1900-01-01: an integer target rounds.
    DateTime,
}

/// Converts `v` to the numeric target `to` (families `Bit`, `Integer`, `ExactNumeric`,
/// `ApproxNumeric` and `Money`).
///
/// `from` names the source type in the error messages and nothing else: the rounding rule
/// is read off the variant of `v`, which is the value's own truth.
pub(crate) fn to_numeric(
    v: &Value,
    from: &TypeInfo,
    to: &TypeInfo,
    style: Option<i32>,
) -> SqlResult<Value> {
    // A style means nothing for a numeric target: the argument is accepted and ignored.
    // The `float` and `money` styles apply to the character *output*.
    let _ = style;
    match v {
        Value::Null => Ok(Value::Null),
        Value::Bit(b) => from_exact(i128::from(*b), 0, ExactSource::Integral, from, to),
        Value::I8(n) => from_exact(i128::from(*n), 0, ExactSource::Integral, from, to),
        Value::I16(n) => from_exact(i128::from(*n), 0, ExactSource::Integral, from, to),
        Value::I32(n) => from_exact(i128::from(*n), 0, ExactSource::Integral, from, to),
        Value::I64(n) => from_exact(i128::from(*n), 0, ExactSource::Integral, from, to),
        Value::Decimal(d) => from_exact(d.mantissa, d.scale, ExactSource::Numeric, from, to),
        Value::Money(m) => from_exact(i128::from(*m), MONEY_SCALE, ExactSource::Money, from, to),
        Value::F64(f) => from_approx(*f, ApproxSource::Float, from, to),
        Value::F32(f) => from_approx(f64::from(*f), ApproxSource::Float, from, to),
        // `datetime` and `smalldatetime` convert to a number as days plus a fraction of a
        // day since 1900-01-01.
        Value::DateTime(dt) => {
            let days = f64::from(dt.days) + f64::from(dt.ticks_300th) / TICKS_PER_DAY;
            from_approx(days, ApproxSource::DateTime, from, to)
        }
        Value::String(s) => from_character::string_to_numeric(&s.text, from, &to.ty),
        // `date`, `time`, `datetime2`, `datetimeoffset` and `uniqueidentifier` have no
        // conversion to a number: 529.
        Value::Date(_)
        | Value::Time(_)
        | Value::DateTime2(_)
        | Value::DateTimeOffset(_)
        | Value::Guid(_) => Err(errors::explicit_conversion_not_allowed(&from.ty, &to.ty)),
        Value::Bytes(bytes) => from_bytes(bytes, from, to),
    }
}

/// Converts a `binary` or `varbinary` source to a numeric target.
///
/// The bytes are read as the **storage** of the target, keeping its low-order end, which
/// is the tail of the byte string: the value is the last `n` bytes read big-endian, where
/// `n` is the width of the target, with no error when the source is longer. With
/// `SELECT CAST(CAST(<hex> AS binary(k)) AS <target>);`
/// (`binary_source_keeps_the_low_bytes` in `tests/convert_numeric.rs`):
///
/// | vector | answer |
/// |---|---|
/// | `0x0F` → `int` | 15 |
/// | `0xFF` → `tinyint` | 255 — `tinyint` is unsigned |
/// | `0xFFFF` → `smallint`, `0xFFFFFFFF` → `int` | −1 — the others are two's complement |
/// | `0x0102030405060708` → `int` | 84281096, the low four bytes `0x05060708` |
/// | `0x0102030405060708` → `bigint` | 72623859790382856 |
/// | `0x0102030405060708090A0B0C` → `bigint` | 361984551142689548, the low eight |
/// | `0x` → `int` | 0 |
/// | `0x0F` → `money`, `smallmoney` | 0.0015 — the amount in ten-thousandths |
/// | `0x0102030405060708090A0B0C0D0E0F10` → `money` | 65134524249499.6250 |
/// | `0x0100` → `bit` | 0 — the low byte, not "some byte is non-zero" |
/// | `0x0001` → `bit` | 1 |
///
/// The `decimal`, `numeric`, `float` and `real` targets stay unconverted: 8114 on the
/// first pair, and 8114 on the second too where SQL Server raises 529, a deliberate
/// difference.
fn from_bytes(bytes: &[u8], from: &TypeInfo, to: &TypeInfo) -> SqlResult<Value> {
    let width = match to.ty {
        SqlType::Bit | SqlType::TinyInt => 1,
        SqlType::SmallInt => 2,
        SqlType::Int | SqlType::SmallMoney => 4,
        SqlType::BigInt | SqlType::Money => 8,
        _ => return Err(errors::error_converting(&from.ty, &to.ty)),
    };
    let start = bytes.len().saturating_sub(width);
    let low = bytes[start..]
        .iter()
        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
    // `low` holds `width` bytes at most, so each narrowing below keeps its value; the
    // sign comes from the target, which is why the cast goes through the unsigned type of
    // the same width first.
    Ok(match to.ty {
        SqlType::Bit => Value::Bit(low != 0),
        SqlType::TinyInt => Value::I8(low as u8),
        SqlType::SmallInt => Value::I16(low as u16 as i16),
        SqlType::Int => Value::I32(low as u32 as i32),
        SqlType::BigInt => Value::I64(low as i64),
        SqlType::SmallMoney => Value::Money(i64::from(low as u32 as i32)),
        SqlType::Money => Value::Money(low as i64),
        // The `match` above sent the other targets to the 8114 of an unconverted pair.
        ref other => Err(errors::bug(format!(
            "convert: {} has no binary width",
            other.name()
        )))?,
    })
}

/// Converts the exact value `m / 10^scale` to the numeric target `to`.
fn from_exact(
    m: i128,
    scale: u8,
    source: ExactSource,
    from: &TypeInfo,
    to: &TypeInfo,
) -> SqlResult<Value> {
    match to.ty {
        // Any non-zero value is 1, negative ones included; only an exact zero is 0.
        SqlType::Bit => Ok(Value::Bit(m != 0)),
        SqlType::TinyInt | SqlType::SmallInt | SqlType::Int | SqlType::BigInt => {
            let n = match source {
                ExactSource::Integral | ExactSource::Numeric => div_trunc(m, scale),
                ExactSource::Money => div_round(m, scale),
            };
            let overflow = || integer_overflow(source, m, n, from, to);
            n.and_then(|n| integer_value(&to.ty, n))
                .ok_or_else(overflow)
        }
        SqlType::Decimal {
            precision,
            scale: s,
        }
        | SqlType::Numeric {
            precision,
            scale: s,
        } => {
            let mantissa = rescale(m, scale, s)
                .filter(|m| fits_precision(*m, precision))
                .ok_or_else(|| {
                    errors::arithmetic_overflow_from(overflow_source_name(from, to), &to.ty)
                })?;
            Ok(Value::Decimal(Decimal {
                mantissa,
                precision,
                scale: s,
            }))
        }
        // `money` to `smallmoney` is the pair SQL Server answers with 237, not 8115.
        SqlType::Money | SqlType::SmallMoney => rescale(m, scale, MONEY_SCALE)
            .and_then(|m| money_value(&to.ty, m))
            .ok_or_else(|| match source {
                ExactSource::Money => errors::insufficient_result_space_money(&to.ty),
                ExactSource::Integral | ExactSource::Numeric => {
                    errors::arithmetic_overflow_from(overflow_source_name(from, to), &to.ty)
                }
            }),
        SqlType::Float => exact_to_f64(m, scale).map(Value::F64),
        // `numeric(38,0)` tops at 10^38 - 1, inside `real`: there is no overflow to report,
        // and the rounding happens once, on the exact value, never through `float`.
        SqlType::Real => exact_to_f32(m, scale).map(Value::F32),
        ref other => Err(errors::bug(format!(
            "convert: {} is not a numeric target",
            other.name()
        ))),
    }
}

/// Converts the approximate value `v` to the numeric target `to`.
fn from_approx(v: f64, source: ApproxSource, from: &TypeInfo, to: &TypeInfo) -> SqlResult<Value> {
    match to.ty {
        SqlType::Bit => Ok(Value::Bit(v != 0.0)),
        SqlType::TinyInt | SqlType::SmallInt | SqlType::Int | SqlType::BigInt => {
            // `float` truncates towards zero, `datetime` rounds; the range is checked
            // before the cast, because a `as` that overflows saturates silently in Rust
            // and would hide the error.
            let n = match source {
                ApproxSource::Float => v.trunc(),
                ApproxSource::DateTime => v.round(),
            };
            f64_to_i128(n)
                .and_then(|n| integer_value(&to.ty, n))
                .ok_or_else(|| approx_overflow(source, v, from, to))
        }
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            // An exact target answers 8115 even to a `float` source, where an integer
            // target answers 232.
            let mantissa = round_scaled(v, scale)
                .filter(|m| fits_precision(*m, precision))
                .ok_or_else(|| {
                    errors::arithmetic_overflow_from(overflow_source_name(from, to), &to.ty)
                })?;
            Ok(Value::Decimal(Decimal {
                mantissa,
                precision,
                scale,
            }))
        }
        SqlType::Money | SqlType::SmallMoney => round_scaled(v, MONEY_SCALE)
            .and_then(|m| money_value(&to.ty, m))
            .ok_or_else(|| approx_overflow(source, v, from, to)),
        SqlType::Float => Ok(Value::F64(v)),
        SqlType::Real => real_value(from, v, to),
        ref other => Err(errors::bug(format!(
            "convert: {} is not a numeric target",
            other.name()
        ))),
    }
}

/// The error an integer target raises when an exact value does not fit: 220 for a `bit`,
/// `tinyint`, `smallint` or `int` source, which quotes the value, **8115 for a `bigint`
/// source**, 8115 for `decimal` and `numeric`, and four different numbers for a money
/// source, see [`money_overflow`].
///
/// The `bigint` line is decided by the source type alone, on the six couples of integers
/// that can overflow: `DECLARE @n bigint = 99999999999;` cast to `int`, `smallint` and
/// `tinyint` raises 8115 state 2 on `expression`, where the same amount held in an `int`
/// or a `smallint` raises 220 quoting the value (`DECLARE @n int = 99999;` towards
/// `smallint` and towards `tinyint`, `DECLARE @n smallint = 300;` towards `tinyint`). The
/// bounds of each target are the counter-vectors: a `bigint` of 2147483647 reaches `int`,
/// 2147483648 does not
/// (`tests::a_bigint_source_overflows_with_8115_where_an_int_source_overflows_with_220`).
///
/// `units` is the mantissa as the source holds it, the amount in ten-thousandths for a
/// money source, and `value` the same number brought to scale 0, which is what the
/// message prints for an integral source.
fn integer_overflow(
    source: ExactSource,
    units: i128,
    value: Option<i128>,
    from: &TypeInfo,
    to: &TypeInfo,
) -> SqlError {
    match source {
        ExactSource::Integral if from.ty == SqlType::BigInt => {
            errors::arithmetic_overflow_from(overflow_source_name(from, to), &to.ty)
        }
        // The other integral sources fit in an `i64`, which is what 220 prints.
        ExactSource::Integral => {
            let value = value
                .and_then(|n| i64::try_from(n).ok())
                .unwrap_or(i64::MAX);
            errors::overflow_for_data_type(&from.ty, &to.ty, value)
        }
        ExactSource::Numeric => {
            errors::arithmetic_overflow_from(overflow_source_name(from, to), &to.ty)
        }
        ExactSource::Money => money_overflow(units, value, from, to),
    }
}

/// The error a `money` or a `smallmoney` too large for an integer target raises: four
/// numbers, with `DECLARE @m money = <v>; SELECT CAST(@m AS <target>);` and its
/// `smallmoney` twin (`money_conversions` in `tests/convert_numeric.rs`).
///
/// A `money` first passes through the four-byte money representation, which holds the
/// amount in ten-thousandths in an `i32`; the conversion stops there, with **237**, as soon
/// as the amount leaves ±214 748.3647 (`@m = 214749` towards `smallint` raises 237
/// naming `smallint`, and `@m = 214748` raises 220 instead). Below that bound the target
/// decides:
///
/// * `smallint`: **220** state 7, quoting the amount **in ten-thousandths** (`@m = 40000`
///   prints `value = 400000000`);
/// * `tinyint`: **232** state 11, quoting the amount itself (`@m = 5000` prints
///   `value = 5000.000000`);
/// * `int` and `bigint`: unreachable below the bound, an amount that fits four-byte money
///   fitting an `int`, and **237** above it.
///
/// A `smallmoney` source is already inside the bound and answers on the target alone:
/// **220** state 5 quoting the amount, not its ten-thousandths (`DECLARE @s smallmoney =
/// 70000; SELECT CAST(@s AS smallint);` prints `value = 70000`), and **8115** state 2 on
/// `expression` towards `tinyint`.
///
/// The state of 237 follows the target, and the target alone: 1 for `int`, 2 for
/// `smallint`, 3 for `tinyint` and for `smallmoney`.
/// `SqlError::insufficient_result_space_money` carries the four, so this module has
/// nothing to add: `DECLARE @m money = 214749; SELECT CAST(@m AS smallint);` (state 2),
/// the same towards `tinyint` (state 3), `DECLARE @m money = 3000000000; SELECT CAST(@m
/// AS int);` (state 1) and `DECLARE @m money = 300000; SELECT CAST(@m AS smallmoney);`
/// (state 3).
fn money_overflow(units: i128, value: Option<i128>, from: &TypeInfo, to: &TypeInfo) -> SqlError {
    // The amount a four-byte money holds, in ten-thousandths: the whole `i32`.
    let fits_small_money = i32::try_from(units).is_ok();

    if from.ty == SqlType::SmallMoney {
        return match to.ty {
            SqlType::SmallInt => {
                errors::overflow_for_data_type(&from.ty, &to.ty, saturating_i64(value))
            }
            _ => errors::arithmetic_overflow_from("expression", &to.ty),
        };
    }
    if !fits_small_money {
        return errors::insufficient_result_space_money(&to.ty);
    }
    match to.ty {
        SqlType::SmallInt => {
            errors::overflow_for_data_type(&from.ty, &to.ty, saturating_i64(Some(units)))
        }
        SqlType::TinyInt => {
            // The amount fits four-byte money, hence a `f64` exactly, and dividing by
            // 10^4 rounds once: the six decimals of message 232 are those of the amount.
            let amount = units as f64 / 10_000.0;
            errors::overflow_for_type(&from.ty, &to.ty, amount)
        }
        _ => errors::insufficient_result_space_money(&to.ty),
    }
}

/// The value messages 220 and 232 print, which is inside an `i64` when it is reached: an
/// amount too wide for one is not a shape the callers can produce.
fn saturating_i64(value: Option<i128>) -> i64 {
    value
        .and_then(|n| i64::try_from(n).ok())
        .unwrap_or(i64::MAX)
}

/// The error an integer or `money` target raises when an approximate value does not fit:
/// a `float` or `real` source raises 232, which quotes the value, a `datetime` source
/// raises 8115 (`SELECT CAST(CAST('2000-01-01' AS datetime) AS tinyint);`).
///
/// `bigint` is the one exception: a `float` or `real` too large for it raises **8115** on
/// `expression`, not 232 (`tests::float_overflow_is_232_except_towards_bigint`).
/// `SELECT CAST(CAST(1e20 AS float) AS bigint);` raises 8115 state 2, and so do the `real`
/// source, a negative value, `1e300` and `POWER(CAST(2 AS bigint), 100)`, while the same
/// `1e20` raises 232 towards `tinyint` (state 1), `smallint` (state 2), `int` (state 3)
/// and `money` (state 2).
fn approx_overflow(source: ApproxSource, v: f64, from: &TypeInfo, to: &TypeInfo) -> SqlError {
    match source {
        ApproxSource::Float if to.ty != SqlType::BigInt => {
            errors::overflow_for_type(&from.ty, &to.ty, v)
        }
        ApproxSource::Float | ApproxSource::DateTime => {
            errors::arithmetic_overflow_from(overflow_source_name(from, to), &to.ty)
        }
    }
}

/// What message 8115 calls the source: the type it was read as, or the word `expression`.
///
/// The message names the source type when the target is `decimal` or `numeric`
/// (`int to data type numeric`, `datetime to data type numeric`) and when an exact
/// numeric goes to `money` (`numeric to data type money`). Everywhere else the message
/// says `expression`, even for a plain variable: `DECLARE @n numeric(10,0) = 2147483648;
/// SELECT CAST(@n AS int);` raises 8115 on `expression`, and so does `DECLARE @b bigint
/// = 9223372036854775807; SELECT CAST(@b AS money);`.
///
/// Two source types are named after another:
///
/// * a `bit` is named **`tinyint`**: `SELECT COALESCE(CAST(1 AS bit), CAST(0.1 AS
///   decimal(1,1)));` raises 8115 naming `tinyint` and `numeric`
///   (`bit_overflow_names_tinyint` in `tests/convert_numeric.rs`);
/// * a `char(n)` is named **`varchar`**, its variable-length spelling, the way message
///   8115 already names a character *target*: `SELECT CAST(1.5 AS decimal(2,1)) +
///   CAST('12' AS char(2));` raises 8115 naming `varchar` and `numeric`.
///
/// Nothing is claimed for `nchar`.
fn overflow_source_name<'a>(from: &'a TypeInfo, to: &TypeInfo) -> &'a str {
    let names_the_type = to.ty.is_exact_numeric()
        || (to.ty.family() == TypeFamily::Money && from.ty.is_exact_numeric());
    match (names_the_type, from.ty) {
        (true, SqlType::Bit) => SqlType::TinyInt.error_name(),
        (true, SqlType::Char(_)) => SqlType::VarChar(Len::Max).error_name(),
        (true, _) => from.ty.error_name(),
        (false, _) => "expression",
    }
}

/// `10^k`, or `None` when it does not fit in an `i128` (`k > 38`).
fn pow10(k: u32) -> Option<i128> {
    10i128.checked_pow(k)
}

/// Moves `m` from `from_scale` to `to_scale`, rounding half away from zero when the scale
/// shrinks. `None` when the result does not fit in an `i128`.
fn rescale(m: i128, from_scale: u8, to_scale: u8) -> Option<i128> {
    if to_scale >= from_scale {
        let k = u32::from(to_scale - from_scale);
        pow10(k).and_then(|p| m.checked_mul(p))
    } else {
        div_round(m, from_scale - to_scale)
    }
}

/// Divides `m` by `10^k` truncating towards zero, the rule of `numeric` → integer.
fn div_trunc(m: i128, k: u8) -> Option<i128> {
    match pow10(u32::from(k)) {
        Some(p) => Some(m / p),
        // `|m| < 10^38 <= 10^k`, so the quotient is zero.
        None => Some(0),
    }
}

/// Divides `m` by `10^k` rounding half away from zero.
///
/// The sum is computed on a `u128` so that a mantissa close to `i128::MAX` does not
/// overflow before the division; only the result is checked against `i128`.
fn div_round(m: i128, k: u8) -> Option<i128> {
    let Some(p) = pow10(u32::from(k)) else {
        // `|m| < 10^38 <= 10^k / 10`, so the quotient rounds to zero.
        return Some(0);
    };
    let p = p.unsigned_abs();
    let half = p / 2;
    let quotient = (m.unsigned_abs() + half) / p;
    let quotient = i128::try_from(quotient).ok()?;
    Some(if m < 0 { -quotient } else { quotient })
}

/// Rounds the **exact** value of `v` half away from zero at `scale` decimals, into a
/// mantissa. `None` when `v` is not finite or the mantissa does not fit in an `i128`.
///
/// A binary fraction of `k` bits has exactly `k` decimal digits, and `format!("{:.k$}")`
/// writes them all without rounding — the standard library's `flt2dec` is exact. The digit
/// after the last kept one then decides the rounding, and nothing else has to: half away
/// from zero reads "round up when the remainder is at least one half", which is that
/// digit being 5 or more. Multiplying by `10^scale` in floating point instead rounds twice
/// and, at scale 37, throws away the digits past the twentieth, which are kept here:
/// `CAST(CAST(0.1 AS float) AS numeric(38,37))` is
/// `0.1000000000000000055511151231257827021`, and `1.005` in `numeric(5,2)` is `1.00`
/// (the double is 1.00499999999999989…) where `(v * 100.0).round()` says `1.01`
/// (`float_to_numeric_uses_the_exact_expansion` in `tests/convert_numeric.rs`).
fn round_scaled(v: f64, scale: u8) -> Option<i128> {
    if !v.is_finite() {
        return None;
    }
    if v == 0.0 {
        return Some(0);
    }
    let scale = usize::from(scale);
    let text = format!("{:.*}", fraction_bits(v), v.abs());
    let (integer, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let kept = fraction.bytes().take(scale);
    let padding = std::iter::repeat_n(b'0', scale.saturating_sub(fraction.len()));
    let mut mantissa: i128 = 0;
    for digit in integer.bytes().chain(kept).chain(padding) {
        mantissa = mantissa
            .checked_mul(10)?
            .checked_add(i128::from(digit - b'0'))?;
    }
    if fraction.as_bytes().get(scale).is_some_and(|d| *d >= b'5') {
        mantissa = mantissa.checked_add(1)?;
    }
    Some(if v < 0.0 { -mantissa } else { mantissa })
}

/// The number of significant bits of `v` below the binary point, which is also the number
/// of decimal digits of its exact expansion: `2^-k` has `k` of them, and no fewer. Zero
/// for an integer, 55 for `0.1`, 1074 for the smallest subnormal.
fn fraction_bits(v: f64) -> usize {
    let bits = v.to_bits();
    let exponent = ((bits >> 52) & F64_EXPONENT_MASK) as u32;
    // A subnormal has no implicit bit, and the unit of the smallest normal, 2^-1074.
    let implicit = if exponent == 0 { 0 } else { 1 << 52 };
    let significand = bits & F64_SIGNIFICAND_MASK | implicit;
    if significand == 0 {
        return 0;
    }
    // Above the bias the unit bit is an integer, and so is every bit of the value.
    let unit = F64_UNIT_BIAS.saturating_sub(exponent.max(1));
    unit.saturating_sub(significand.trailing_zeros()) as usize
}

/// The `float` nearest to the exact value `m / 10^scale`, ties to even.
///
/// `str::parse::<f64>` is the correctly rounded primitive of the standard library
/// (`core::num::dec2flt` rounds to 0.5 units in the last place, ties to even), so the
/// value goes through its decimal text. `m as f64 / 10f64.powi(scale)` rounds up to
/// three times, the mantissa above 2^53, the power of ten above 10^22, the quotient, and
/// lands one ULP off on `1999114.5855581982` as a `numeric(38,10)`: `…60` for the
/// correct `413E810A95E7245F`
/// (`numeric_to_float_rounds_correctly_at_the_last_bit` in `tests/convert_numeric.rs`).
fn exact_to_f64(m: i128, scale: u8) -> SqlResult<f64> {
    format!("{m}e-{scale}")
        .parse()
        .map_err(|_| errors::bug(format!("exact_to_f64: {m}e-{scale} did not parse")))
}

/// The `real` nearest to the exact value `m / 10^scale`, ties to even, rounded **once**:
/// `str::parse::<f32>` rounds from the decimal text straight to `f32`. Going through
/// `exact_to_f64` and `as f32` rounds twice and lands on the wrong side of a `real`
/// midpoint whenever the double is that midpoint, `8388608.5000000001` as a
/// `numeric(17,10)` for one (`numeric_to_real_rounds_once` in
/// `tests/convert_numeric.rs`).
fn exact_to_f32(m: i128, scale: u8) -> SqlResult<f32> {
    format!("{m}e-{scale}")
        .parse()
        .map_err(|_| errors::bug(format!("exact_to_f32: {m}e-{scale} did not parse")))
}

/// Converts an already truncated or rounded `f64` to an `i128`, rejecting what does not
/// fit. The range of the target itself is checked afterwards, by [`integer_value`].
fn f64_to_i128(v: f64) -> Option<i128> {
    if v.is_finite() && (-TWO_POW_127..TWO_POW_127).contains(&v) {
        Some(v as i128)
    } else {
        None
    }
}

/// Builds the value of an integer target from `n`, or `None` when `n` is out of range.
fn integer_value(ty: &SqlType, n: i128) -> Option<Value> {
    match ty {
        SqlType::TinyInt => u8::try_from(n).ok().map(Value::I8),
        SqlType::SmallInt => i16::try_from(n).ok().map(Value::I16),
        SqlType::Int => i32::try_from(n).ok().map(Value::I32),
        SqlType::BigInt => i64::try_from(n).ok().map(Value::I64),
        _ => None,
    }
}

/// Builds a `money` or `smallmoney` value from a mantissa of ten-thousandths, or `None`
/// when it is out of range: `money` spans the whole `i64`
/// (±922 337 203 685 477.580 8), `smallmoney` the whole `i32` (±214 748.364 8).
fn money_value(ty: &SqlType, m: i128) -> Option<Value> {
    match ty {
        SqlType::Money => i64::try_from(m).ok().map(Value::Money),
        SqlType::SmallMoney => i32::try_from(m).ok().map(|m| Value::Money(i64::from(m))),
        _ => None,
    }
}

/// Builds a `real` value, raising 232 when a finite `float` falls outside `real`.
fn real_value(from: &TypeInfo, v: f64, to: &TypeInfo) -> SqlResult<Value> {
    let narrowed = v as f32;
    if v.is_finite() && !narrowed.is_finite() {
        return Err(errors::overflow_for_type(&from.ty, &to.ty, v));
    }
    Ok(Value::F32(narrowed))
}

/// Whether a mantissa holds in `precision` digits: `decimal(p, s)` stores values strictly
/// below `10^p / 10^s`, which on the mantissa reads `|m| < 10^p`.
fn fits_precision(m: i128, precision: u8) -> bool {
    match pow10(u32::from(precision)) {
        Some(limit) => m.unsigned_abs() < limit.unsigned_abs(),
        // A precision above 38 is not a type SQL Server accepts; refuse rather than pass.
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert;

    fn float(v: f64) -> (Value, TypeInfo) {
        (Value::F64(v), TypeInfo::new(SqlType::Float, false))
    }

    fn real(v: f32) -> (Value, TypeInfo) {
        (Value::F32(v), TypeInfo::new(SqlType::Real, false))
    }

    /// An approximate source too large for an integer target raises 232 with the value,
    /// except towards `bigint`, which raises 8115 on `expression`.
    ///
    /// `SELECT CAST(CAST(1e20 AS float) AS tinyint);` raises 232 state 1,
    /// `… AS smallint);` state 2, `… AS int);` state 3, `… AS money);` state 2, while
    /// `… AS bigint);` raises 8115, severity 16, state 2, on `expression`. The `real`
    /// source, `-1e20`, `1e300` and `POWER(CAST(2 AS bigint), 100)` answer the same.
    #[test]
    fn float_overflow_is_232_except_towards_bigint() {
        for (to, state) in [
            (SqlType::TinyInt, 1),
            (SqlType::SmallInt, 2),
            (SqlType::Int, 3),
            (SqlType::Money, 2),
        ] {
            let (v, from) = float(1e20);
            let e = convert(&v, &from, &TypeInfo::new(to, true), None).unwrap_err();
            assert_eq!(e.number, 232, "{to:?}");
            assert_eq!(e.state, state, "{to:?}");
        }

        let bigint = TypeInfo::new(SqlType::BigInt, true);
        for (v, from) in [float(1e20), float(-1e20), float(1e300), real(1e20)] {
            let e = convert(&v, &from, &bigint, None).unwrap_err();
            assert_eq!(e.number, 8115);
            assert_eq!(e.severity, 16);
            assert_eq!(e.state, 2);
            assert_eq!(
                e.message,
                "Converting expression to data type bigint overflowed."
            );
        }
    }

    /// The exact expansion of a double has as many decimal digits as it has significant
    /// bits below the binary point: none for an integer, 1074 for the smallest subnormal.
    #[test]
    fn fraction_bits_counts_the_digits_of_the_exact_expansion() {
        assert_eq!(fraction_bits(1.0), 0);
        assert_eq!(fraction_bits(3.0e15), 0);
        assert_eq!(fraction_bits(1.0e300), 0);
        assert_eq!(fraction_bits(0.5), 1);
        assert_eq!(fraction_bits(0.75), 2);
        assert_eq!(fraction_bits(1.5), 1);
        // 0.1 = 3602879701896397 × 2^-55.
        assert_eq!(fraction_bits(0.1), 55);
        assert_eq!(fraction_bits(f64::MIN_POSITIVE), 1022);
        assert_eq!(fraction_bits(5e-324), 1074);
        assert_eq!(fraction_bits(-0.1), 55);
        assert_eq!(fraction_bits(0.0), 0);
    }

    /// `round_scaled` reads the exact value: a subnormal is a long string of zeros at any
    /// scale of `numeric`, a value that is not finite or too wide for an `i128` is `None`,
    /// and both zeros are zero.
    #[test]
    fn round_scaled_reads_the_exact_value() {
        assert_eq!(round_scaled(0.0, 38), Some(0));
        assert_eq!(round_scaled(-0.0, 4), Some(0));
        assert_eq!(round_scaled(5e-324, 38), Some(0));
        assert_eq!(round_scaled(f64::MIN_POSITIVE, 38), Some(0));
        // 2^-128 = 2.938…e-39 is below half a unit of the 38th decimal, so zero; 2^-127 =
        // 5.877…e-39 is above it, so one unit.
        assert_eq!(round_scaled(2f64.powi(-128), 38), Some(0));
        assert_eq!(round_scaled(2f64.powi(-127), 38), Some(1));
        assert_eq!(
            round_scaled(0.1, 37),
            Some(1_000_000_000_000_000_055_511_151_231_257_827_021)
        );
        assert_eq!(round_scaled(-0.125, 2), Some(-13));
        // The double nearest 1.005 is 1.00499999999999989…: below the half.
        assert_eq!(round_scaled(1.005, 2), Some(100));
        assert_eq!(round_scaled(1e23, 0), Some(99_999_999_999_999_991_611_392));
        assert_eq!(
            round_scaled(1e38, 0),
            Some(99_999_999_999_999_997_748_809_823_456_034_029_568)
        );
        assert_eq!(
            round_scaled(1.7e38, 0),
            Some(169_999_999_999_999_998_061_923_293_023_115_935_744)
        );
        // 2^127 does not fit an `i128`, nor does anything wider.
        assert_eq!(round_scaled(2f64.powi(127), 0), None);
        assert_eq!(round_scaled(1e39, 0), None);
        assert_eq!(round_scaled(1e308, 0), None);
        assert_eq!(round_scaled(f64::MAX, 38), None);
        assert_eq!(round_scaled(f64::INFINITY, 0), None);
        assert_eq!(round_scaled(f64::NAN, 0), None);
    }

    /// The decimal text of `m / 10^s` is what the standard library rounds, once, to the
    /// target precision: the same exact value gives a different last bit in `float` and in
    /// `real` when it sits on a `real` midpoint.
    #[test]
    fn exact_to_float_and_real_round_once_each() {
        assert_eq!(
            exact_to_f64(19_991_145_855_581_982, 10).map(f64::to_bits),
            Ok(0x413E_810A_95E7_245F)
        );
        assert_eq!(exact_to_f64(1, 0), Ok(1.0));
        assert_eq!(exact_to_f64(-15, 1), Ok(-1.5));
        assert_eq!(exact_to_f64(0, 38), Ok(0.0));
        assert_eq!(exact_to_f64(10i128.pow(38) - 1, 38), Ok(1.0));
        assert_eq!(
            exact_to_f32(83_886_085_000_000_001, 10).map(f32::to_bits),
            Ok(0x4B00_0001)
        );
        assert_eq!(exact_to_f32(16_777_217, 0), Ok(16_777_216.0));
        assert_eq!(exact_to_f32(-(10i128.pow(38) - 1), 0), Ok(-1e38));
    }

    /// A `datetime` source keeps its own 8115 towards `bigint` as towards any integer:
    /// `SELECT CAST(CAST('2000-01-01' AS datetime) AS tinyint);`.
    #[test]
    fn datetime_overflow_stays_8115() {
        let from = TypeInfo::new(SqlType::DateTime, false);
        let v = Value::DateTime(crate::DateTime {
            days: 36_524,
            ticks_300th: 0,
        });
        let e = convert(&v, &from, &TypeInfo::new(SqlType::TinyInt, true), None).unwrap_err();
        assert_eq!(e.number, 8115);
    }

    /// Which integral source raises 8115 and which raises 220.
    ///
    /// With `DECLARE @n <source> = <value>; SELECT CAST(@n AS <target>);` on the six
    /// couples of integers that can overflow: a `bigint` source raises 8115 state 2 on
    /// `int`, `smallint` and `tinyint`; an `int` source raises 220 on `smallint` (state 1)
    /// and `tinyint` (state 2), and a `smallint` source raises 220 on `tinyint`. The
    /// bounds are the counter-vectors: 2147483647, 32767 and 255 go through, one more
    /// does not.
    #[test]
    fn a_bigint_source_overflows_with_8115_where_an_int_source_overflows_with_220() {
        let bigint = TypeInfo::new(SqlType::BigInt, true);
        for to in [SqlType::Int, SqlType::SmallInt, SqlType::TinyInt] {
            let e = convert(
                &Value::I64(99_999_999_999),
                &bigint,
                &TypeInfo::new(to, true),
                None,
            )
            .unwrap_err();
            assert_eq!((e.number, e.state), (8115, 2), "{to:?}");
            assert_eq!(
                e.message,
                format!(
                    "Converting expression to data type {} overflowed.",
                    to.error_name()
                )
            );
            // The same amount, held in the same `i128`, but declared `int`: 220 towards
            // `smallint` (state 1) and `tinyint` (state 2), and `int` -> `int` goes through.
            let int = TypeInfo::new(SqlType::Int, true);
            let got = convert(&Value::I32(99_999), &int, &TypeInfo::new(to, true), None);
            match to {
                SqlType::Int => assert_eq!(got, Ok(Value::I32(99_999))),
                SqlType::SmallInt => {
                    let e = got.expect_err("220");
                    assert_eq!((e.number, e.state), (220, 1));
                }
                _ => {
                    let e = got.expect_err("220");
                    assert_eq!((e.number, e.state), (220, 2));
                }
            }
        }
        let smallint = TypeInfo::new(SqlType::SmallInt, true);
        let e = convert(
            &Value::I16(300),
            &smallint,
            &TypeInfo::new(SqlType::TinyInt, true),
            None,
        )
        .unwrap_err();
        assert_eq!((e.number, e.state), (220, 2));
        assert_eq!(e.message, "Value out of range for data type tinyint: 300.");
        // The bounds of each target are reached, not refused.
        for (value, to, expected) in [
            (2_147_483_647i64, SqlType::Int, Value::I32(2_147_483_647)),
            (-2_147_483_648, SqlType::Int, Value::I32(-2_147_483_648)),
            (32_767, SqlType::SmallInt, Value::I16(32_767)),
            (255, SqlType::TinyInt, Value::I8(255)),
        ] {
            assert_eq!(
                convert(&Value::I64(value), &bigint, &TypeInfo::new(to, true), None),
                Ok(expected),
                "{value} {to:?}"
            );
        }
        for (value, to) in [
            (2_147_483_648i64, SqlType::Int),
            (32_768, SqlType::SmallInt),
            (256, SqlType::TinyInt),
            (-1, SqlType::TinyInt),
        ] {
            let e =
                convert(&Value::I64(value), &bigint, &TypeInfo::new(to, true), None).unwrap_err();
            assert_eq!(e.number, 8115, "{value} {to:?}");
        }
    }
}
