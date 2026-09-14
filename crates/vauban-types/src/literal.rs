//! `LiteralKind` and `parse_literal`: the value and the type of a T-SQL literal.
//!
//! Typing a literal is not the parser's business: `1.50` is a `numeric(3,2)`, `1e3` a
//! `float`, `$1.5` a `money`, `0x1F` a `varbinary(1)`, `'x'` a `varchar(1)`. The parser
//! splits the source text and says which **kind** it read; this module turns that payload
//! into a [`Value`] and the [`TypeInfo`] SQL Server gives it. The enumeration lives here
//! and not in `parser` because `types` does not depend on `parser`: the binder maps
//! `parser::Literal` onto [`LiteralKind`], one variant per variant.
//!
//! The rules below are exercised by `tests/literal.rs`.

use vauban_errors::SqlResult;

use crate::errors;
use crate::{Len, SqlString, SqlType, TypeInfo, Value};

/// Largest precision a `decimal`/`numeric` can declare. A literal that needs more digits
/// is refused, see [`parse_literal`].
const MAX_PRECISION: usize = 38;

/// Largest declared length of a non-`max` `varchar`, in characters.
const MAX_VARCHAR: usize = 8_000;

/// Largest declared length of a non-`max` `nvarchar`, in characters.
const MAX_NVARCHAR: usize = 4_000;

/// Largest declared length of a non-`max` `varbinary`, in bytes.
const MAX_VARBINARY: usize = 8_000;

/// Number of decimals `money` and `smallmoney` hold.
const MONEY_SCALE: usize = 4;

/// Beyond this many integer digits a `money` literal is certainly out of range: `money`
/// stops at 922 337 203 685 477.5807, fifteen digits before the point.
const MAX_MONEY_DIGITS: usize = 20;

/// The kind of literal the lexer read, as the binder hands it over.
///
/// The kind is **given**, never guessed from the text: the lexer already decided it from
/// the delimiters (`N'…'`, `0x…`, `$…`) and from the shape of the number, and the same
/// characters can belong to two kinds (`15` is an `Integer`, `1.5` a `Decimal`, `$1.5` a
/// `Money`). See [`parse_literal`] for the exact payload each kind expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiteralKind {
    /// A whole number written without an exponent: `parser::Literal::Integer`.
    Integer,
    /// A fixed-point number: `parser::Literal::Decimal`.
    Decimal,
    /// A number written with an exponent: `parser::Literal::Float`.
    Float,
    /// A `$`-prefixed amount: `parser::Literal::Money`.
    Money,
    /// A `0x`-prefixed binary string: `parser::Literal::Binary`.
    Hex,
    /// A character string written `'…'`: `parser::Literal::Str { unicode: false }`.
    Str,
    /// A character string written `N'…'`: `parser::Literal::Str { unicode: true }`.
    NStr,
}

/// Gives a T-SQL literal its value and its type.
///
/// `text` is the **payload the parser stored in its AST**, never the source text between
/// delimiters. The parser produces exactly these forms (this module is the reference in
/// case of disagreement):
///
/// | [`LiteralKind`] | content of `text` | examples |
/// |---|---|---|
/// | [`Integer`](LiteralKind::Integer) | decimal digits, no sign | `"0"`, `"2147483648"` |
/// | [`Decimal`](LiteralKind::Decimal) | digits and one point, no sign | `"1.50"`, `".5"`, `"1."` |
/// | [`Float`](LiteralKind::Float) | mantissa, `e`/`E`, signed exponent | `"1e3"`, `"1.5E-2"` |
/// | [`Money`](LiteralKind::Money) | the amount **without** the `$`, sign kept | `"1.5"`, `"-1.50"` |
/// | [`Hex`](LiteralKind::Hex) | hexadecimal digits **without** the `0x` | `"1F"`, `""` |
/// | [`Str`](LiteralKind::Str) | the text, already unescaped, no quotes | `"a'b"`, `""` |
/// | [`NStr`](LiteralKind::NStr) | idem, for `N'…'` | `"x"` |
///
/// The typing rules:
///
/// - `Integer` is `int` while the value fits, then `numeric(p, 0)` where `p` counts the
///   digits **after leading zeros are dropped**: `2147483648` is `numeric(10,0)` and
///   `00000000002147483648` is `numeric(10,0)` too. No literal is a `bigint`;
/// - `Decimal` is `numeric(p, s)` where `s` counts the digits written after the point and
///   `p` is `s` plus the digits of the integer part **after leading zeros are dropped**,
///   at least 1: `1.50` and `01.50` are `numeric(3,2)`, `.5` is `numeric(1,1)`, `1.` is
///   `numeric(1,0)` and `0.000` is `numeric(3,3)` — **not** `numeric(4,3)`, the leading
///   zero of the integer part is not counted;
/// - `Float` is `float`, read by [`str::parse::<f64>`](str::parse);
/// - `Money` is `money`, rounded half away from zero to four decimals;
/// - `Hex` is `varbinary(n)` with `n` the number of bytes; `0x` alone is an empty
///   `varbinary(1)`;
/// - `Str` is `varchar(n)` and `NStr` is `nvarchar(n)`, `n` counting **characters**, at
///   least 1: `''` is `varchar(1)`. Past 8 000 characters (4 000 for `nvarchar`) the type
///   is `varchar(max)` / `nvarchar(max)`.
///
/// The returned [`TypeInfo`] is never nullable — a literal is never `NULL`, and `NULL` and
/// `DEFAULT` are not kinds of this function, the binder types them — and carries
/// [`Collation::DEFAULT`](crate::Collation::DEFAULT) for `Str` and `NStr`.
///
/// # Errors
///
/// A `text` that does not match its `kind` is an internal error: the lexer cannot produce
/// it, so it means the caller built the payload by hand.
///
/// Three user errors, one per shape of number:
///
/// - more than 38 digits, integer or fixed-point: error 1007, severity 15, state 1, which
///   quotes the number; there is **no** fallback on `float`;
/// - a `float` literal out of the range of a double: error 168, severity 15, state 1,
///   which quotes the literal;
/// - a `money` literal out of the range of `money`: error 151, severity 15, state 1, where
///   the literal is quoted **with** its `$`.
///
/// # Examples
///
/// ```
/// use vauban_types::{LiteralKind, SqlType, Value, parse_literal};
///
/// let (value, ty) = parse_literal(LiteralKind::Integer, "1")?;
/// assert_eq!(value, Value::I32(1));
/// assert_eq!(ty.ty, SqlType::Int);
/// assert!(!ty.nullable);
/// # Ok::<(), vauban_errors::SqlError>(())
/// ```
pub fn parse_literal(kind: LiteralKind, text: &str) -> SqlResult<(Value, TypeInfo)> {
    let (value, ty) = match kind {
        LiteralKind::Integer => integer_literal(text)?,
        LiteralKind::Decimal => decimal_literal(text)?,
        LiteralKind::Float => float_literal(text)?,
        LiteralKind::Money => money_literal(text)?,
        LiteralKind::Hex => hex_literal(text)?,
        LiteralKind::Str => string_literal(text, false),
        LiteralKind::NStr => string_literal(text, true),
    };
    Ok((value, TypeInfo::new(ty, false)))
}

/// `int` while the value fits, `numeric(p, 0)` beyond.
fn integer_literal(text: &str) -> SqlResult<(Value, SqlType)> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(errors::bug(format!(
            "parse_literal: `{text}` is not an Integer literal payload"
        )));
    }
    let digits = significant(text);
    let precision = digits.len();
    if precision > MAX_PRECISION {
        return Err(errors::number_out_of_numeric_range(text));
    }
    let mantissa = mantissa_of(digits);
    match i32::try_from(mantissa) {
        Ok(small) => Ok((Value::I32(small), SqlType::Int)),
        // Not `bigint`: SQL Server goes straight from `int` to `numeric` for a literal.
        Err(_) => Ok(decimal_value(mantissa, precision, 0)),
    }
}

/// `numeric(p, s)`, counting the digits as written minus the leading zeros.
fn decimal_literal(text: &str) -> SqlResult<(Value, SqlType)> {
    let malformed = || {
        errors::bug(format!(
            "parse_literal: `{text}` is not a Decimal literal payload"
        ))
    };
    let (int_part, frac_part) = text.split_once('.').ok_or_else(malformed)?;
    let digits_only = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(int_part) || !digits_only(frac_part) || int_part.len() + frac_part.len() == 0 {
        return Err(malformed());
    }

    let int_digits = significant_or_empty(int_part);
    let scale = frac_part.len();
    let precision = (int_digits.len() + scale).max(1);
    if precision > MAX_PRECISION {
        return Err(errors::number_out_of_numeric_range(text));
    }
    let mantissa = mantissa_of(int_digits) * pow10(scale) + mantissa_of(frac_part);
    Ok(decimal_value(mantissa, precision, scale))
}

/// `float`, read by the standard library from the exponential form the lexer isolated.
fn float_literal(text: &str) -> SqlResult<(Value, SqlType)> {
    if !is_exponential_form(text) {
        return Err(errors::bug(format!(
            "parse_literal: `{text}` is not a Float literal payload"
        )));
    }
    let Ok(value) = text.parse::<f64>() else {
        return Err(errors::bug(format!(
            "parse_literal: `{text}` is not a Float literal payload"
        )));
    };
    if !value.is_finite() {
        return Err(errors::float_out_of_range(text));
    }
    Ok((Value::F64(value), SqlType::Float))
}

/// `money`: the amount without its `$`, rounded half away from zero to four decimals.
fn money_literal(text: &str) -> SqlResult<(Value, SqlType)> {
    let malformed = || {
        errors::bug(format!(
            "parse_literal: `{text}` is not a Money literal payload"
        ))
    };
    let (negative, rest) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (int_part, frac_part) = rest.split_once('.').unwrap_or((rest, ""));
    let digits_only = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(int_part) || !digits_only(frac_part) || int_part.len() + frac_part.len() == 0 {
        return Err(malformed());
    }
    let int_digits = significant_or_empty(int_part);
    // `money` tops out below 10^15 units, so 20 integer digits are already out of range;
    // the check also keeps `scale_to` inside an `i128`.
    if int_digits.len() > MAX_MONEY_DIGITS {
        return Err(errors::invalid_money_value(text));
    }
    let scaled = scale_to(int_digits, frac_part, MONEY_SCALE, negative);
    let Ok(units) = i64::try_from(scaled) else {
        return Err(errors::invalid_money_value(text));
    };
    Ok((Value::Money(units), SqlType::Money))
}

/// `varbinary(n)`, `n` counting bytes and never zero.
fn hex_literal(text: &str) -> SqlResult<(Value, SqlType)> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(errors::bug(format!(
            "parse_literal: `{text}` is not a Hex literal payload"
        )));
    }
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    let mut bytes = Vec::with_capacity(pairs.len());
    for [hi, lo] in pairs {
        bytes.push(hex_digit(*hi) * 16 + hex_digit(*lo));
    }
    let len = declared_len(bytes.len(), MAX_VARBINARY);
    Ok((Value::Bytes(bytes), SqlType::VarBinary(len)))
}

/// `varchar(n)` or `nvarchar(n)`, `n` counting characters and never zero.
fn string_literal(text: &str, unicode: bool) -> (Value, SqlType) {
    let chars = text.chars().count();
    let value = Value::String(SqlString {
        text: text.to_owned(),
    });
    let ty = if unicode {
        SqlType::NVarChar(declared_len(chars, MAX_NVARCHAR))
    } else {
        SqlType::VarChar(declared_len(chars, MAX_VARCHAR))
    };
    (value, ty)
}

/// The declared length of a literal of `used` characters or bytes: at least 1, `(max)`
/// past `limit`, which is the largest length the type can declare.
fn declared_len(used: usize, limit: usize) -> Len {
    if used > limit {
        Len::Max
    } else {
        // `used <= limit <= 8000` fits in a `u16`, and 0 becomes 1.
        Len::Fixed(used.max(1) as u16)
    }
}

/// `text` without its leading zeros, `"0"` when every digit is a zero.
fn significant(text: &str) -> &str {
    let trimmed = text.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
}

/// `text` without its leading zeros, **empty** when every digit is a zero: what the
/// precision of a fixed-point literal counts (`0.000` is `numeric(3,3)`).
fn significant_or_empty(text: &str) -> &str {
    text.trim_start_matches('0')
}

/// The unsigned value of a run of decimal digits, which the caller has bounded to 38.
fn mantissa_of(digits: &str) -> i128 {
    digits
        .bytes()
        .fold(0i128, |acc, b| acc * 10 + i128::from(b - b'0'))
}

/// `10^n` for `n` up to 38, the largest scale a `numeric` can declare.
fn pow10(n: usize) -> i128 {
    let mut value = 1i128;
    for _ in 0..n {
        value *= 10;
    }
    value
}

/// The value of `int_digits.frac_digits` at scale `scale`, rounded half away from zero,
/// negated when `negative`.
fn scale_to(int_digits: &str, frac_digits: &str, scale: usize, negative: bool) -> i128 {
    let kept = frac_digits.len().min(scale);
    let mut value = mantissa_of(int_digits) * pow10(scale);
    value += mantissa_of(&frac_digits[..kept]) * pow10(scale - kept);
    // Half away from zero only looks at the first dropped digit: anything past it is
    // already above the half and rounds the same way.
    if let Some(next) = frac_digits.as_bytes().get(scale)
        && *next >= b'5'
    {
        value += 1;
    }
    if negative { -value } else { value }
}

/// The value of one hexadecimal digit, which the caller has checked.
fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        // `is_ascii_hexdigit` leaves no other case.
        _ => b - b'A' + 10,
    }
}

/// A `numeric(precision, scale)` value; `precision` is bounded to 38 by the caller.
fn decimal_value(mantissa: i128, precision: usize, scale: usize) -> (Value, SqlType) {
    let precision = precision as u8;
    let scale = scale as u8;
    (
        Value::Decimal(crate::Decimal {
            mantissa,
            precision,
            scale,
        }),
        SqlType::Numeric { precision, scale },
    )
}

/// True for `digits[.digits] e|E [sign] digits`, the shape the lexer gives a `Float`.
fn is_exponential_form(text: &str) -> bool {
    let Some(exponent) = text.find(['e', 'E']) else {
        return false;
    };
    let (mantissa, rest) = text.split_at(exponent);
    let exponent = &rest[1..];
    let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
    if exponent.is_empty() || !exponent.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits_only = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    digits_only(int_part) && digits_only(frac_part) && int_part.len() + frac_part.len() > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Collation;

    fn parse(kind: LiteralKind, text: &str) -> (Value, TypeInfo) {
        parse_literal(kind, text).expect("literal")
    }

    #[test]
    fn integer_leading_zeros_do_not_count() {
        // `007` is an int, `00000000002147483648` is
        // numeric(10,0) and not numeric(20,0).
        let (v, t) = parse(LiteralKind::Integer, "007");
        assert_eq!(v, Value::I32(7));
        assert_eq!(t.ty, SqlType::Int);

        let (v, t) = parse(LiteralKind::Integer, "00000000002147483648");
        assert_eq!(
            v,
            Value::Decimal(crate::Decimal {
                mantissa: 2_147_483_648,
                precision: 10,
                scale: 0
            })
        );
        assert_eq!(
            t.ty,
            SqlType::Numeric {
                precision: 10,
                scale: 0
            }
        );
    }

    #[test]
    fn thirty_eight_digits_is_the_limit() {
        // numeric(38,0) and numeric(38,2) exist...
        let (_, t) = parse(LiteralKind::Integer, &"1".repeat(38));
        assert_eq!(
            t.ty,
            SqlType::Numeric {
                precision: 38,
                scale: 0
            }
        );
        let (_, t) = parse(
            LiteralKind::Decimal,
            "123456789012345678901234567890123456.78",
        );
        assert_eq!(
            t.ty,
            SqlType::Numeric {
                precision: 38,
                scale: 2
            }
        );

        // ... and one digit more
        // is error 1007, severity 15, state 1 — not a `float`.
        let digits = "123456789012345678901234567890123456789";
        let e = parse_literal(LiteralKind::Integer, digits).expect_err("1007");
        assert_eq!(e.number, 1007);
        assert_eq!(e.severity, 15);
        assert_eq!(e.state, 1);
        assert_eq!(
            e.message,
            format!(
                "The number '{digits}' exceeds the numeric range (precision is limited to 38)."
            )
        );
        let e = parse_literal(
            LiteralKind::Decimal,
            "1234567890123456789012345678901234567.89",
        )
        .expect_err("1007");
        assert_eq!(e.number, 1007);
        assert_eq!(
            e.message,
            "The number '1234567890123456789012345678901234567.89' exceeds the numeric \
             range (precision is limited to 38)."
        );
    }

    #[test]
    fn malformed_payloads_are_internal_errors() {
        for (kind, text) in [
            (LiteralKind::Integer, ""),
            (LiteralKind::Integer, "-1"),
            (LiteralKind::Integer, "1.5"),
            (LiteralKind::Decimal, "15"),
            (LiteralKind::Decimal, "1.2.3"),
            (LiteralKind::Decimal, "-1.5"),
            (LiteralKind::Decimal, "."),
            (LiteralKind::Float, "15"),
            (LiteralKind::Float, "1e"),
            (LiteralKind::Float, "e3"),
            (LiteralKind::Float, "1e2e3"),
            (LiteralKind::Money, "$1.5"),
            (LiteralKind::Money, ""),
            (LiteralKind::Money, "1,234.5"),
            (LiteralKind::Hex, "1"),
            (LiteralKind::Hex, "0x1F"),
            (LiteralKind::Hex, "zz"),
        ] {
            let e = parse_literal(kind, text).expect_err("internal error");
            assert_eq!(e.number, 50_000, "{kind:?} {text}");
        }
    }

    #[test]
    fn float_out_of_range_is_reported() {
        // 168, severity 15, state 1, and the width of a
        // `float` in the message.
        let e = parse_literal(LiteralKind::Float, "1e400").expect_err("168");
        assert_eq!(e.number, 168);
        assert_eq!(e.severity, 15);
        assert_eq!(e.state, 1);
        assert_eq!(
            e.message,
            "The floating point literal '1e400' cannot be represented in 8 bytes."
        );
    }

    #[test]
    fn money_out_of_range_is_reported() {
        // 151, severity 15, state 1, and the literal quoted
        // with the `$` its payload does not carry.
        let e = parse_literal(LiteralKind::Money, "99999999999999999999").expect_err("151");
        assert_eq!(e.number, 151);
        assert_eq!(e.severity, 15);
        assert_eq!(e.state, 1);
        assert_eq!(
            e.message,
            "'$99999999999999999999' cannot be read as a money value."
        );
    }

    #[test]
    fn money_keeps_four_decimals() {
        // $1.23456 prints 1.2346 under style 2.
        assert_eq!(parse(LiteralKind::Money, "1.23456").0, Value::Money(12_346));
        assert_eq!(parse(LiteralKind::Money, "1.5").0, Value::Money(15_000));
        assert_eq!(parse(LiteralKind::Money, "1").0, Value::Money(10_000));
    }

    #[test]
    fn long_strings_become_max() {
        let (_, t) = parse(LiteralKind::Str, &"x".repeat(8_000));
        assert_eq!(t.ty, SqlType::VarChar(Len::Fixed(8_000)));
        let (_, t) = parse(LiteralKind::Str, &"x".repeat(8_001));
        assert_eq!(t.ty, SqlType::VarChar(Len::Max));
        let (_, t) = parse(LiteralKind::NStr, &"y".repeat(4_000));
        assert_eq!(t.ty, SqlType::NVarChar(Len::Fixed(4_000)));
        let (_, t) = parse(LiteralKind::NStr, &"y".repeat(4_001));
        assert_eq!(t.ty, SqlType::NVarChar(Len::Max));
    }

    /// A literal is never `NULL`, and only a character literal carries a collation.
    #[test]
    fn literals_are_never_nullable() {
        for (kind, text) in [
            (LiteralKind::Integer, "1"),
            (LiteralKind::Decimal, "1.5"),
            (LiteralKind::Float, "1e3"),
            (LiteralKind::Money, "1.5"),
            (LiteralKind::Hex, "1F"),
            (LiteralKind::Str, "x"),
            (LiteralKind::NStr, "x"),
        ] {
            let (_, t) = parse(kind, text);
            assert!(!t.nullable, "{kind:?}");
            let expected = if t.ty.is_string() {
                Some(Collation::DEFAULT)
            } else {
                None
            };
            assert_eq!(t.collation, expected, "{kind:?}");
        }
    }

    /// Multi-byte characters count as one character, not as their UTF-8 length.
    #[test]
    fn string_length_counts_characters() {
        let (v, t) = parse(LiteralKind::NStr, "é");
        assert_eq!(
            v,
            Value::String(SqlString {
                text: "é".to_owned()
            })
        );
        assert_eq!(t.ty, SqlType::NVarChar(Len::Fixed(1)));
    }
}
