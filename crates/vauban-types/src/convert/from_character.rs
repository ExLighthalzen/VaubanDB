//! Reading a number out of a character string, shared by the conversion of a string to a
//! numeric type and by the typing of a literal.
//!
//! The rules, error numbers and states below are exercised by
//! `tests/convert_from_character.rs` and `tests/convert_numeric.rs`.

use vauban_errors::{SqlError, SqlResult};

use crate::errors;
use crate::{Len, SqlType, TypeFamily, TypeInfo, Value};

/// Number of decimals `money` and `smallmoney` hold.
const MONEY_SCALE: usize = 4;

/// `smallmoney` stops at ±214 748.3647, i.e. the range of an `i32` in money units.
const SMALLMONEY_MIN: i128 = i32::MIN as i128;

/// See [`SMALLMONEY_MIN`].
const SMALLMONEY_MAX: i128 = i32::MAX as i128;

/// Largest precision a `decimal`/`numeric` can declare.
const MAX_PRECISION: usize = 38;

/// Parses `s`, read as type `from`, into a value of the numeric type `to`.
///
/// `from` is a character type and `to` belongs to one of the five numeric families
/// (`Bit`, `Integer`, `ExactNumeric`, `ApproxNumeric`, `Money`); anything else is a broken
/// precondition, because `convert` never routes a string to a date, a GUID or a binary
/// through this function ([`super::datetime`] and [`super::binary`] own those).
///
/// Leading and trailing ASCII whitespace is ignored, and an empty (or blank) string is
/// `0` for every target **except** `decimal`/`numeric`, which refuses it
/// (`tests::string_to_decimal_and_money`). The accepted shape depends on the family:
///
/// - `bit` and the integer types accept a sign, spaces, then digits, nothing else:
///   `'1.9'`, `'1e3'` and `'1,234'` fail with 245. `'0'` is false and any other number is
///   true. `'- 1'` is `-1` and a sign with no digit behind it is `0`, see
///   [`scan_integer`];
/// - `decimal`/`numeric` accepts a sign, spaces, digits, a point and digits, **without** an
///   exponent (`'1e3'` is error 8114, not 1000) and rounds half away from zero to the
///   target scale: `'1.567'` is `1.57` and `'-1.565'` is `-1.57`. A sign with no digit
///   behind it is refused here, with a number this crate does not yet align, see
///   [`to_exact_numeric`];
/// - `money` and `smallmoney` accept in addition a `$` before or after the sign, spaces
///   after it and commas among the digits, and round half away from zero to four
///   decimals. A string that spells the grammar without a digit is `0.0000`, see
///   [`to_money`];
/// - `float` and `real` accept the exponential form, spelled with `e` or with `d`
///   ([`to_approx_numeric`]), but not the `inf`/`NaN` spellings the Rust parser knows.
///
/// # Errors
///
/// - 245 for a malformed string towards `bit` or an integer type. The value is echoed
///   **as written**, spaces included, and the source is named by
///   [`SqlType::error_name`](crate::SqlType::error_name), so `nvarchar` reads `nvarchar`;
/// - 8114 for a malformed or empty string towards `decimal`/`numeric`, and towards
///   `float`/`real`;
/// - 8115 naming the source when the integer part does not fit the target precision, and
///   naming `expression` when the value leaves the range of `bigint`, `float`, `real`,
///   `money` or `smallmoney`.
///
/// Two more situations get a number of their own rather than 8115:
///
/// - a value out of the range of `int`, `smallint` or `tinyint`: error 248 state 1 for
///   `int`, and error 244 state 2/1 for the two others, which the message names `INT2`
///   and `INT1`;
/// - a malformed string towards `money`: error 235 state 0.
// Reachable from the crate through `numeric::to_numeric`.
#[allow(dead_code)] // called by convert::numeric
pub(crate) fn string_to_numeric(s: &str, from: &TypeInfo, to: &SqlType) -> SqlResult<Value> {
    if !from.ty.is_string() {
        return Err(errors::bug(format!(
            "string_to_numeric: source {} is not a character type",
            from.ty.declaration()
        )));
    }
    let trimmed = s.trim_matches(|c: char| c.is_ascii_whitespace());
    match to.family() {
        TypeFamily::Bit => to_bit(trimmed, s, from, to),
        TypeFamily::Integer => to_integer(trimmed, s, from, to),
        TypeFamily::ExactNumeric => to_exact_numeric(trimmed, from, to),
        TypeFamily::ApproxNumeric => to_approx_numeric(trimmed, from, to),
        TypeFamily::Money => to_money(trimmed, to),
        TypeFamily::Character | TypeFamily::Binary | TypeFamily::DateTime | TypeFamily::Guid => {
            Err(errors::bug(format!(
                "string_to_numeric: target {} is not a numeric type",
                to.declaration()
            )))
        }
    }
}

/// A signed run of decimal digits, split into its sign and its significant digits.
struct Digits<'a> {
    /// True when the string opened with `-`.
    negative: bool,
    /// The digits without their leading zeros; empty when the value is zero.
    digits: &'a str,
}

/// Splits `[+|-] spaces* digits*` and drops the leading zeros; `None` on anything else.
///
/// **The grammar is `[+|-] spaces* digits*`** (`tests::spaces_fall_between_the_sign_and_the_digits`
/// and `tests::a_lone_sign_is_zero_for_the_integers_and_for_money`):
///
/// - spaces (0x20) fall between the sign and the digits: `CAST('- 1' AS int)` is `-1`,
///   `'+ 1'` is `1`, `'-  12'` is `-12`, `'- 0'` and `'- 00'` are `0`, towards `bit`,
///   `tinyint`, `smallint`, `int` and `bigint` alike (`'- 1'` towards `tinyint` is the
///   244 of `-1`, not 245);
/// - a sign with no digit behind it is the degenerate case of the same rule, and reads
///   as zero: `CAST('-' AS int)`, `CAST('+' AS int)` and `CAST('- ' AS int)` are `0`;
/// - the space is the one blank the grammar admits, and the sign comes once and first:
///   `'-\t1'`, `'- -1'`, `'- +1'`, `'1 -'`, `'--'`, `'-.'` and `'-,'` are 245.
///
/// `float` and `real` do not share the rule — `CAST('-' AS float)` and `CAST('- 1' AS
/// float)` are 8114 — and `decimal`/`numeric` share the spaces but not the lone sign, see
/// [`to_exact_numeric`]. The empty string does not reach this function: the callers
/// answer it before.
fn scan_integer(t: &str) -> Option<Digits<'_>> {
    let (negative, rest) = split_sign(t);
    let rest = skip_spaces(rest);
    if !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(Digits {
        negative,
        digits: rest.trim_start_matches('0'),
    })
}

/// Splits an optional leading `+` or `-` off `t`.
fn split_sign(t: &str) -> (bool, &str) {
    match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    }
}

/// Drops the ASCII spaces (0x20) that open `t`: the one blank admitted between a sign
/// and the digits of an integer or a `numeric` (`'- 1'` is read, `'-\t1'` is not).
fn skip_spaces(t: &str) -> &str {
    t.trim_start_matches(' ')
}

/// `bit`: `'0'` is false, any other number is true, non-numeric text is error 245.
fn to_bit(t: &str, raw: &str, from: &TypeInfo, to: &SqlType) -> SqlResult<Value> {
    if t.is_empty() {
        return Ok(Value::Bit(false));
    }
    let scanned = scan_integer(t).ok_or_else(|| errors::conversion_failed(&from.ty, raw, to))?;
    // No arithmetic: a single non-zero digit is enough, whatever the magnitude.
    Ok(Value::Bit(!scanned.digits.is_empty()))
}

/// `tinyint`, `smallint`, `int` and `bigint`: a sign and digits, nothing else.
fn to_integer(t: &str, raw: &str, from: &TypeInfo, to: &SqlType) -> SqlResult<Value> {
    if t.is_empty() {
        return integer_value(0, raw, from, to);
    }
    let scanned = scan_integer(t).ok_or_else(|| errors::conversion_failed(&from.ty, raw, to))?;
    // 20 digits already exceed `bigint`; the bound also keeps the fold inside an `i128`.
    if scanned.digits.len() > 20 {
        return Err(integer_overflow(raw, from, to));
    }
    let magnitude = fold_digits(scanned.digits);
    let value = if scanned.negative {
        -magnitude
    } else {
        magnitude
    };
    integer_value(value, raw, from, to)
}

/// Fits `value` in the integer target `to`, or raises the overflow SQL Server raises.
fn integer_value(value: i128, raw: &str, from: &TypeInfo, to: &SqlType) -> SqlResult<Value> {
    let fitted = match to {
        SqlType::TinyInt => u8::try_from(value).ok().map(Value::I8),
        SqlType::SmallInt => i16::try_from(value).ok().map(Value::I16),
        SqlType::Int => i32::try_from(value).ok().map(Value::I32),
        SqlType::BigInt => i64::try_from(value).ok().map(Value::I64),
        other => {
            return Err(errors::bug(format!(
                "string_to_numeric: {} is not an integer type",
                other.declaration()
            )));
        }
    };
    fitted.ok_or_else(|| integer_overflow(raw, from, to))
}

/// What message 8115 calls a character source: its **variable-length** spelling.
///
/// `SELECT CAST(1.5 AS decimal(2,1)) + CAST('12' AS char(2));` raises 8115 naming
/// `varchar` for a `char(2)` source, the way the message already names a character target
/// (`crate::errors`). A `varchar` source keeps its own name, which is the counter-vector;
/// nothing is claimed for `nchar`.
fn overflow_source_name(from: &TypeInfo) -> &'static str {
    match from.ty {
        SqlType::Char(_) => SqlType::VarChar(Len::Max).error_name(),
        _ => from.ty.error_name(),
    }
}

/// `decimal(p, s)` / `numeric(p, s)`: no exponent, half away from zero, 8114 and 8115.
///
/// The spaces between the sign and the digits are read here as [`scan_integer`] reads
/// them: `CAST('- 1.5' AS numeric(10,2))` is `-1.50`. A lone sign is where this target
/// parts from the integer ones, and it is a **deliberate difference from SQL Server**:
/// `CAST('-' AS numeric(10,2))` and `CAST('+' AS decimal(18,4))` raise 8115 state 6
/// there, where a precision overflow of the same pair sends state 8; the state comes from
/// a table of the `errors` crate keyed on the pair alone, so this function answers 8114
/// instead. The same string is `0` towards `int` ([`scan_integer`]) and 8114 towards
/// `float` and `real`, which is the frontier of the zero rule.
fn to_exact_numeric(t: &str, from: &TypeInfo, to: &SqlType) -> SqlResult<Value> {
    let (SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale }) = *to
    else {
        return Err(errors::bug(format!(
            "string_to_numeric: {} is not an exact numeric type",
            to.declaration()
        )));
    };
    // The empty string is an error here and only here (`tests::string_to_decimal_and_money`).
    let failed = || errors::error_converting(&from.ty, to);
    if t.is_empty() {
        return Err(failed());
    }
    let (negative, int_digits, frac_digits) = scan_fixed_point(t).ok_or_else(failed)?;
    let scale = usize::from(scale);
    let precision = usize::from(precision);
    if int_digits.len() + scale > MAX_PRECISION {
        return Err(errors::arithmetic_overflow_from(
            overflow_source_name(from),
            to,
        ));
    }
    let mantissa = rescale(int_digits, frac_digits, scale, negative);
    if digit_count(mantissa) > precision {
        return Err(errors::arithmetic_overflow_from(
            overflow_source_name(from),
            to,
        ));
    }
    Ok(Value::Decimal(crate::Decimal {
        mantissa,
        precision: precision as u8,
        scale: scale as u8,
    }))
}

/// `float` and `real`: the exponential form is welcome, `inf` and `NaN` are not.
///
/// **`d` and `D` spell the exponent as `e` and `E` do**: `CAST('1d5' AS float)` is
/// `100000`, the answer of `CAST('1e5' AS float)`, and so are `'1D5'`, `'1.d5'`, `'1d+5'`,
/// `'1d05'` and `' 1d5 '`; `'1d-5'` is `1e-5`. The two letters are interchangeable and not
/// cumulative: `'1d'`, `'1d.5'`, `'1d+'`, `'1de5'`, `'1ed5'`, `'1d5e2'`, `'1d5.2'` and
/// `'1 d5'` are 8114, exactly like the same shapes written with `e`, and `'1f5'` and
/// `'1g5'` are 8114 too, so the letter added is `d`. The rule stops at `float` and `real`:
/// `CAST('1d5' AS int)` is 245, `AS numeric(10,2)` is 8114 and `AS money` is 235, because
/// those three grammars do not read an exponent.
fn to_approx_numeric(t: &str, from: &TypeInfo, to: &SqlType) -> SqlResult<Value> {
    if t.is_empty() {
        return approx_value(0.0, to);
    }
    if !is_float_form(t) {
        return Err(errors::error_converting(&from.ty, to));
    }
    // `str::parse` knows `e` alone, so the `d` spelling is rewritten before it is read;
    // `is_float_form` has already checked the shape, marker included.
    let rewritten;
    let readable = if t.contains(['d', 'D']) {
        rewritten = t.replace(['d', 'D'], "e");
        rewritten.as_str()
    } else {
        t
    };
    let Ok(value) = readable.parse::<f64>() else {
        return Err(errors::error_converting(&from.ty, to));
    };
    approx_value(value, to)
}

/// Fits `value` in `float` or `real`; a value that runs off to infinity is error 8115,
/// whose source the message writes as `expression`.
fn approx_value(value: f64, to: &SqlType) -> SqlResult<Value> {
    match to {
        SqlType::Float if value.is_finite() => Ok(Value::F64(value)),
        SqlType::Real if (value as f32).is_finite() => Ok(Value::F32(value as f32)),
        SqlType::Float | SqlType::Real => Err(errors::arithmetic_overflow_from("expression", to)),
        other => Err(errors::bug(format!(
            "string_to_numeric: {} is not an approximate numeric type",
            other.declaration()
        ))),
    }
}

/// `money` and `smallmoney`: a `$`, commas among the digits, four decimals.
///
/// The source type is not a parameter: error 235 names neither the value nor the source
/// (the same text for an `nvarchar` source).
///
/// **A string that spells the grammar without a single digit is `0.0000`**, not 235:
/// `CAST('$' AS money)`, `'+'`, `'-'`, `'.'`, `','`, `'$-.'`, `'-$.'`, `',,'` and `'$ - .'`
/// are `0.0000`. The frontier is the grammar itself and nothing looser: what comes in the
/// wrong order or twice is still 235, `'$$'`, `'++'`, `'--'`, `'..'`, `'$.-'`, `'.-$'`
/// and `'+-.,$'` among them, and so is a symbol glued to a digit outside the grammar,
/// `'1$'`, `'1-'` and `'$1$'` (`tests::money_reads_a_grammar_without_digits_as_zero`).
fn to_money(t: &str, to: &SqlType) -> SqlResult<Value> {
    if t.is_empty() {
        return Ok(Value::Money(0));
    }
    let Some((negative, int_digits, frac_digits)) = scan_money(t) else {
        return Err(errors::char_to_money_syntax());
    };
    // 20 integer digits are already past `money`, and the bound keeps `rescale` inside
    // an `i128`.
    if int_digits.len() > 20 {
        return Err(errors::arithmetic_overflow_from("expression", to));
    }
    let units = rescale(&int_digits, &frac_digits, MONEY_SCALE, negative);
    let (min, max) = match to {
        SqlType::Money => (i128::from(i64::MIN), i128::from(i64::MAX)),
        SqlType::SmallMoney => (SMALLMONEY_MIN, SMALLMONEY_MAX),
        other => {
            return Err(errors::bug(format!(
                "string_to_numeric: {} is not a money type",
                other.declaration()
            )));
        }
    };
    if units < min || units > max {
        return Err(errors::arithmetic_overflow_from("expression", to));
    }
    // The range check above is what makes this conversion infallible.
    let units = i64::try_from(units).unwrap_or_default();
    Ok(Value::Money(units))
}

/// Splits `[+|-] spaces* digits [. digits]` into its sign, its significant integer digits
/// and its fractional digits; `None` on any other shape, exponent included.
///
/// The spaces after the sign are the ones [`scan_integer`] admits: `CAST('- 1.5' AS
/// numeric(10,2))` is `-1.50` and `'+ 1'` is `1.00`, where `'-\t1'`, `'- -1'` and
/// `'+ 1e2'` are 8114.
fn scan_fixed_point(t: &str) -> Option<(bool, &str, &str)> {
    let (negative, rest) = split_sign(t);
    let rest = skip_spaces(rest);
    let (int_part, frac_part) = rest.split_once('.').unwrap_or((rest, ""));
    let digits_only = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(int_part) || !digits_only(frac_part) || int_part.len() + frac_part.len() == 0 {
        return None;
    }
    Some((negative, int_part.trim_start_matches('0'), frac_part))
}

/// The money grammar: a `$` before or after the sign, spaces around it, and commas
/// anywhere among the digits. Returns the sign, the significant integer digits and the
/// fractional digits.
///
/// **The grammar is allowed to end without a digit**, and [`to_money`] reads what it
/// returns then as zero. Commas fall on both sides of the point: `CAST('1.2,3' AS money)`
/// is `1.2300` and `CAST('1,.2' AS money)` is `1.2000`, which is why the filter below runs
/// on the whole rest and not on the integer part alone.
fn scan_money(t: &str) -> Option<(bool, String, String)> {
    let mut rest = t;
    let mut negative = false;
    let mut sign_seen = false;
    let mut dollar_seen = false;
    loop {
        match rest.as_bytes().first() {
            Some(b'-') | Some(b'+') if !sign_seen => {
                sign_seen = true;
                negative = rest.as_bytes().first() == Some(&b'-');
                rest = &rest[1..];
            }
            Some(b'$') if !dollar_seen => {
                dollar_seen = true;
                rest = &rest[1..];
            }
            Some(b) if b.is_ascii_whitespace() => rest = &rest[1..],
            _ => break,
        }
    }
    let (int_part, frac_part) = rest.split_once('.').unwrap_or((rest, ""));
    // Commas group the thousands; SQL Server does not check where they fall, on either
    // side of the point.
    let drop_commas = |s: &str| -> String { s.chars().filter(|c| *c != ',').collect() };
    let int_digits = drop_commas(int_part);
    let frac_digits = drop_commas(frac_part);
    let digits_only = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(&int_digits) || !digits_only(&frac_digits) {
        return None;
    }
    Some((
        negative,
        int_digits.trim_start_matches('0').to_owned(),
        frac_digits,
    ))
}

/// True for `[+|-] digits [. digits] [e|E|d|D [+|-] digits]` with at least one mantissa
/// digit: the shapes SQL Server reads as a `float`, which exclude `inf` and `NaN`.
///
/// The marker is the **first** of the four letters, so a string that carries two of them
/// leaves a letter inside the exponent and is refused: `'1d5e2'`, `'1de5'` and `'1ed5'` are
/// 8114, like `'1e5e2'`.
fn is_float_form(t: &str) -> bool {
    let (_, rest) = split_sign(t);
    let (mantissa, exponent) = match rest.find(['e', 'E', 'd', 'D']) {
        Some(at) => (&rest[..at], Some(&rest[at + 1..])),
        None => (rest, None),
    };
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if exponent.is_empty() || !exponent.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits_only = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    digits_only(int_part) && digits_only(frac_part) && int_part.len() + frac_part.len() > 0
}

/// The unsigned value of a run of decimal digits, which the caller has bounded.
fn fold_digits(digits: &str) -> i128 {
    digits
        .bytes()
        .fold(0i128, |acc, b| acc * 10 + i128::from(b - b'0'))
}

/// `10^n`, for the `n` a `numeric` scale or a money scale allows.
fn pow10(n: usize) -> i128 {
    let mut value = 1i128;
    for _ in 0..n {
        value *= 10;
    }
    value
}

/// The value of `int_digits.frac_digits` at `scale`, rounded half away from zero and
/// negated when `negative`.
fn rescale(int_digits: &str, frac_digits: &str, scale: usize, negative: bool) -> i128 {
    let kept = frac_digits.len().min(scale);
    let mut value = fold_digits(int_digits) * pow10(scale);
    value += fold_digits(&frac_digits[..kept]) * pow10(scale - kept);
    // Half away from zero only looks at the first dropped digit: anything past it is
    // already above the half and rounds the same way.
    if let Some(next) = frac_digits.as_bytes().get(scale)
        && *next >= b'5'
    {
        value += 1;
    }
    if negative { -value } else { value }
}

/// Number of decimal digits of `mantissa`, `0` counting as one digit.
fn digit_count(mantissa: i128) -> usize {
    let mut rest = mantissa.unsigned_abs();
    let mut digits = 1;
    while rest >= 10 {
        rest /= 10;
        digits += 1;
    }
    digits
}

/// Errors 248 and 244, severity 16: a value out of the range of `int`, `smallint` or
/// `tinyint`. `bigint` is the one integer target reported with 8115, and the word the
/// message writes there is `expression`.
fn integer_overflow(raw: &str, from: &TypeInfo, to: &SqlType) -> SqlError {
    match to {
        SqlType::BigInt => errors::arithmetic_overflow_from("expression", to),
        SqlType::Int => errors::conversion_overflowed_int(&from.ty, raw),
        _ => errors::conversion_overflowed_small_int(&from.ty, raw, to),
    }
}

#[cfg(test)]
mod tests;
