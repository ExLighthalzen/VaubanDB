//! Conversions towards `char`, `varchar`, `nchar` and `nvarchar`.
//!
//! Two questions, in that order: how the value reads as text, then how the declared length of
//! the target treats a text that is too long. The first one is [`crate::default_display`]'s,
//! except for the `float` and `money` styles of `CONVERT`, written here; the second one has
//! three answers, silent truncation, the single character `*`, or an error, and the answer
//! depends on the **source** type.
//!
//! # Collation of the result
//!
//! The result of a conversion to a character type takes the collation of the input when the
//! input is a
//! character expression, and the default collation of the database otherwise. Since
//! [`crate::convert`] returns a [`Value`] and not a [`TypeInfo`], nothing here carries it: the
//! caller that builds the [`TypeInfo`] of the expression (the `binder`) applies the rule, with
//! `from.collation` when [`crate::SqlType::is_string`] holds for `from.ty`, and
//! [`crate::Collation::DEFAULT`] otherwise. A source whose collation this crate does not
//! support yet is not an error here: the text is copied as it is.
//!
//! # Binary sources and their three styles
//!
//! A [`Value::Bytes`] does **not** go through [`crate::default_display`], which renders `0x`
//! followed by hexadecimal: the bytes are read as characters when no style is given
//! (`SELECT CONVERT(varchar(40), 0x4E616D65);` yields `Name`) and the hexadecimal form is
//! kept for styles `1` and `2`. [`binary_as_character`] holds that rule.

use vauban_errors::{SqlError, SqlResult};

use crate::convert::binary::{BINARY_ERROR_TYPE, HEX_PREFIX};
use crate::convert::datetime;
use crate::errors;
use crate::{Len, SqlString, SqlType, TypeFamily, TypeInfo, Value, default_display};

/// Scale of a [`Value::Money`]: the amount is held in ten-thousandths.
const MONEY_SCALE: usize = 4;

/// Digits of the thousands groups of `money` style 1.
const GROUP_SIZE: usize = 3;

/// What a rendering too long for the declared length of the target becomes.
///
/// `int`, `smallint` and `tinyint` give `*` towards `char` and `varchar`, and an error
/// towards `nchar` and `nvarchar`; the other
/// numeric sources give an error towards any character type; a character source is truncated
/// without a word. A `uniqueidentifier` joins the numeric sources — its 36 characters do not
/// truncate, see [`overflow`] — and a `bit`, a binary and a date do truncate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overflow {
    /// Keep the first characters of the rendering, raise nothing.
    Truncate,
    /// Replace the whole rendering with the single character `*`, raise nothing.
    Star,
    /// Raise the error [`overflow_error`] builds for the source.
    Error,
}

/// Converts `v` to the character target `to`, applying the `CONVERT` style when given
/// (`float` styles 0/1/2/3, `money` styles 0/1/2/126, date styles).
///
/// `Value::Null` never reaches this function: [`crate::convert`] answers it first.
///
/// The declared length of `to` is applied **in characters**, never in bytes: `Len::Fixed(n)`
/// counts characters for `char` and `varchar` (one code page character each) as much as for
/// `nchar` and `nvarchar`. A `Len::Max` target never truncates. A `char(n)` or an `nchar(n)`
/// target is padded with spaces, after the truncation.
pub(crate) fn to_character(
    v: &Value,
    from: &TypeInfo,
    to: &TypeInfo,
    style: Option<i32>,
) -> SqlResult<Value> {
    // A binary source is the only one whose rendering depends on the **length** of the
    // target: styles 1 and 2 drop whole bytes rather than characters, which `fit` cannot do.
    let text = match (&from.ty, v) {
        (SqlType::Binary(_) | SqlType::VarBinary(_), Value::Bytes(bytes)) => {
            binary_as_character(bytes, &to.ty, style)?
        }
        _ => render(v, from, to, style)?,
    };
    let text = fit(text, v, from, to)?;
    Ok(Value::String(SqlString { text }))
}

/// Renders `v`, of type `from`, under the `CONVERT` style, without any length limit.
///
/// Style 0 (and no style at all) is [`crate::default_display`] for every type, which is why
/// this crate has a single rendering of a value; only the non-zero `float` and `money` styles
/// are written here, and the date styles belong to `datetime::datetime_to_character`. That
/// one alone needs `to`: its error 8114 names the target type
/// (`datetime::tests::a_style_a_type_cannot_fill_names_the_target`).
fn render(v: &Value, from: &TypeInfo, to: &TypeInfo, style: Option<i32>) -> SqlResult<String> {
    match (&from.ty, v) {
        (SqlType::Float, Value::F64(x)) => Ok(float_with_style(*x, style, v, from)),
        (SqlType::Real, Value::F32(x)) => Ok(float_with_style(f64::from(*x), style, v, from)),
        (SqlType::Money | SqlType::SmallMoney, Value::Money(amount)) => {
            Ok(money_with_style(*amount, style, v, from))
        }
        _ if from.ty.family() == TypeFamily::DateTime => {
            datetime::datetime_to_character(v, from, to, style)
        }
        _ => Ok(default_display(v, from)),
    }
}

/// Renders a `float` or a `real` under its `CONVERT` style.
///
/// Style `0` is at most 6 digits, scientific notation when appropriate (the default
/// rendering, [`crate::default_display`]), style `1` is 8 digits in scientific notation,
/// style `2` 16 digits in scientific notation, style `3` 17 digits, a lossless conversion.
/// Other values are processed as 0, with one addition: style `126` behaves like style 2.
///
/// With `DECLARE @f float = 1234567;`: `CONVERT(varchar(40), @f, 4)`, `…, 5)`, `…, 100)`
/// and `…, -1)` yield `1.23457e+006`, while `…, 126)` yields `1.234567000000000e+006`,
/// the form of style 2 (`tests/convert_to_character.rs`).
fn float_with_style(x: f64, style: Option<i32>, v: &Value, from: &TypeInfo) -> String {
    match style {
        Some(1) => scientific(x, 8),
        Some(2) | Some(126) => scientific(x, 16),
        Some(3) => scientific(x, 17),
        _ => default_display(v, from),
    }
}

/// Writes `x` in scientific notation with exactly `significant_digits` digits: one before the
/// decimal point and the rest after it.
///
/// The exponent is always signed and always padded to three digits, as SQL Server writes it:
/// `CONVERT(varchar(40), CAST(1234567 AS float), 1)` is `1.2345670e+006`. A `float` of SQL
/// Server is always finite, so the first branch is unreachable through `CONVERT`.
fn scientific(x: f64, significant_digits: usize) -> String {
    if !x.is_finite() {
        return x.to_string();
    }
    let precision = significant_digits.saturating_sub(1);
    let formatted = format!("{x:.precision$e}");
    // `{:e}` always writes a mantissa, `e`, and a parsable exponent; the fallbacks below
    // cannot be reached with a finite `f64`.
    let Some((mantissa, exponent)) = formatted.split_once('e') else {
        return formatted;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return formatted;
    };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:03}", exponent.abs())
}

/// Renders a `money` or a `smallmoney` under its `CONVERT` style.
///
/// Style `0` gives two decimals and no thousands separator (the default rendering,
/// [`crate::default_display`]), style `1` two decimals and a comma per group of three digits,
/// style `2` four decimals and no separator, style `126` is equivalent to style 2.
///
/// The other values are **not** processed as 0: with `DECLARE @m money = 4235.9819;`,
/// every style outside `{0, 2, 126}` renders like style 1 (`tests/convert_to_character.rs`):
/// `CONVERT(varchar(40), @m, 3)`, `…, 4)`, `…, 5)`, `…, 6)`, `…, 7)`, `…, 8)`, `…, 9)`,
/// `…, 10)`, `…, 20)`, `…, 21)`, `…, 99)`, `…, 100)`, `…, 127)` and `…, -1)` yield
/// `4,235.98`, while `…, 0)` yields `4235.98` and `…, 2)` and `…, 126)` yield `4235.9819`.
fn money_with_style(amount: i64, style: Option<i32>, v: &Value, from: &TypeInfo) -> String {
    match style {
        Some(2) | Some(126) => four_decimals(amount),
        None | Some(0) => default_display(v, from),
        Some(_) => group_thousands(&default_display(v, from)),
    }
}

/// Renders an amount held in ten-thousandths with its four decimals, without separator:
/// `42359819` becomes `4235.9819`. This is `money` style 2, the exact value of the amount.
fn four_decimals(amount: i64) -> String {
    let sign = if amount < 0 { "-" } else { "" };
    let digits = amount.unsigned_abs().to_string();
    // At least one digit before the point: 5 ten-thousandths is `0.0005`, not `.0005`.
    let width = MONEY_SCALE + 1;
    let padded = format!("{digits:0>width$}");
    let split = padded.len() - MONEY_SCALE;
    format!("{sign}{}.{}", &padded[..split], &padded[split..])
}

/// Inserts a comma every three digits to the left of the decimal point of `text`, which is a
/// plain decimal rendering, possibly signed: `-4235.98` becomes `-4,235.98`.
///
/// This is `money` style 1 seen as style 0 plus its separators, which keeps the rounding of
/// the amount in one place ([`crate::default_display`]).
fn group_thousands(text: &str) -> String {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (integer, fraction) = match rest.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (rest, None),
    };
    let length = integer.chars().count();
    let mut grouped = String::with_capacity(length + length / GROUP_SIZE);
    for (index, digit) in integer.chars().enumerate() {
        if index > 0 && (length - index) % GROUP_SIZE == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    match fraction {
        Some(fraction) => format!("{sign}{grouped}.{fraction}"),
        None => format!("{sign}{grouped}"),
    }
}

/// Renders a `binary` or a `varbinary` under its `CONVERT` style, already cut to the length
/// of the target when the style asks for it.
///
/// Style `0` (and no style) reads the bytes as characters, style `1` writes `0x`
/// followed by upper-case hexadecimal digits, style `2` writes the same digits without the
/// prefix. Any other style is error 9809: `SELECT CONVERT(char(8), 0x4E616D65, 3);` and
/// `…, 126);` both raise it, naming varbinary and varchar.
///
/// # Why the length is applied here
///
/// Styles `1` and `2` drop **whole bytes**, not characters: the two characters of the `0x`
/// prefix eat into the room, and an odd character left over is dropped rather than filled
/// with half a byte. `SELECT CONVERT(char(8), 0x4E616D65, 1);` = `0x4E616D` (three
/// bytes), `CONVERT(varchar(3), 0x4E616D65, 1)` = `0x`, `CONVERT(varchar(1), 0x4E616D65, 1)`
/// = the empty string, `CONVERT(varchar(3), 0x4E616D65, 2)` = `4E` and
/// `CONVERT(varchar(4), 0x4E616D65, 2)` = `4E61`. Style `0` needs nothing of the kind: one
/// character per byte, so [`fit`] truncates it correctly.
fn binary_as_character(bytes: &[u8], to: &SqlType, style: Option<i32>) -> SqlResult<String> {
    let limit = match declared_length(to) {
        Len::Max => usize::MAX,
        Len::Fixed(n) => usize::from(n),
    };
    let national = matches!(to, SqlType::NChar(_) | SqlType::NVarChar(_));
    match style {
        None | Some(0) => Ok(decode_bytes(bytes, national)),
        Some(1) if limit < HEX_PREFIX.len() => Ok(String::new()),
        Some(1) => Ok(format!(
            "{HEX_PREFIX}{}",
            hex_digits(bytes, limit - HEX_PREFIX.len())
        )),
        Some(2) => Ok(hex_digits(bytes, limit)),
        Some(other) => Err(errors::unsupported_style(
            other,
            &BINARY_ERROR_TYPE,
            &character_error_type(national),
        )),
    }
}

/// The upper-case hexadecimal digits of the first bytes of `bytes` that fit in `room`
/// characters, two per byte.
fn hex_digits(bytes: &[u8], room: usize) -> String {
    let taken = bytes.len().min(room / 2);
    let mut digits = String::with_capacity(taken * 2);
    for byte in &bytes[..taken] {
        digits.push_str(&format!("{byte:02X}"));
    }
    digits
}

/// Style `0`: the bytes read as characters — code page 1252 for a `char` or `varchar`
/// target, UTF-16LE for an `nchar` or `nvarchar` one.
///
/// `SELECT CONVERT(varchar(20), 0x4E616D65, 0);` = `Name` and
/// `SELECT CONVERT(varchar(20), 0xE9, 0), CONVERT(varchar(20), 0x80, 0);` = `é` and `€`,
/// while `SELECT CONVERT(nvarchar(20), 0x4E616D65, 0);` = `慎敭` (two UTF-16LE code units).
/// A last odd byte of a national target becomes a character of its own:
/// `SELECT CONVERT(nvarchar(20), 0x4E616D, 0);` = `慎m`, two characters.
fn decode_bytes(bytes: &[u8], national: bool) -> String {
    if !national {
        return bytes.iter().map(|b| cp1252_char(*b)).collect();
    }
    let units = bytes.chunks(2).map(|pair| match pair {
        [low, high] => u16::from(*low) | u16::from(*high) << 8,
        _ => u16::from(pair[0]),
    });
    let mut text = String::with_capacity(bytes.len() / 2);
    for decoded in char::decode_utf16(units) {
        // An unpaired surrogate is not a character; the replacement character stands in.
        text.push(match decoded {
            Ok(c) => c,
            Err(_) => char::REPLACEMENT_CHARACTER,
        });
    }
    text
}

/// The character of code page 1252 a byte spells.
///
/// The `0x80`-`0x9F` range is the only one that differs from Latin-1; the five positions the
/// code page leaves undefined (`0x81`, `0x8D`, `0x8F`, `0x90`, `0x9D`) become the C1 control
/// character of the same number, the mapping Windows itself uses. The same table lives in
/// `collation.rs` for the other direction, and
/// [`tests::cp1252_char_agrees_with_cp1252_byte`] checks the two never disagree.
fn cp1252_char(byte: u8) -> char {
    const HIGH_RANGE: [char; 32] = [
        '\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{2C6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8D}', '\u{17D}',
        '\u{8F}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}',
        '\u{2014}', '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}', '\u{17E}',
        '\u{178}',
    ];
    match byte {
        0x80..=0x9F => HIGH_RANGE[usize::from(byte) - 0x80],
        // Every other byte of code page 1252 is the code point of the same number.
        _ => char::from(byte),
    }
}

/// The type the 9809 of a binary source names as the target: the `var` form, even when the
/// target is a `char(n)` (`SELECT CONVERT(char(8), 0x4E616D65, 3);` names varbinary and
/// varchar). [`SqlType::error_name`] ignores the length, so [`Len::Max`] here carries no
/// meaning.
///
/// The 8114 of a date style names its target the same way: `nvarchar` for an `nchar(60)`
/// and an `nvarchar(max)`, `varchar` for a `char(60)` and a `varchar(max)`, on the six
/// targets and the styles 8, 24, 108 of a `date` and 1, 6, 23, 101, 112, 115 of a `time`
/// (`datetime::tests::a_style_a_type_cannot_fill_names_the_target`).
/// `datetime::render_with_style` calls it for that reason.
pub(super) fn character_error_type(national: bool) -> SqlType {
    if national {
        SqlType::NVarChar(Len::Max)
    } else {
        SqlType::VarChar(Len::Max)
    }
}

/// Applies the declared length of `to` to `text`: truncation, `*` or error, then padding.
///
/// `v` and `from` are needed to build the error, whose number and text depend on the source.
fn fit(text: String, v: &Value, from: &TypeInfo, to: &TypeInfo) -> SqlResult<String> {
    let limit = match declared_length(&to.ty) {
        // `varchar(max)` and `nvarchar(max)` hold everything: nothing to apply.
        Len::Max => return Ok(text),
        Len::Fixed(n) => usize::from(n),
    };
    let mut text = if text.chars().count() > limit {
        match overflow(&from.ty, &to.ty) {
            Overflow::Truncate => text.chars().take(limit).collect(),
            Overflow::Star => "*".to_owned(),
            Overflow::Error => return Err(overflow_error(v, from, to)),
        }
    } else {
        text
    };
    // `char(n)` and `nchar(n)` are padded to their length, after the truncation: an `int` too
    // long for a `char(2)` is `*` followed by one space (`SELECT '[' + CAST(@i AS char(2)) +
    // ']';` = `[* ]`).
    if matches!(to.ty, SqlType::Char(_) | SqlType::NChar(_)) {
        for _ in text.chars().count()..limit {
            text.push(' ');
        }
    }
    Ok(text)
}

/// The declared length of a character type; anything else is [`Len::Max`], which never
/// truncates. `to_character` is only reached with a character target, so the second arm is a
/// broken precondition of the dispatch, not a case to handle.
fn declared_length(ty: &SqlType) -> Len {
    match ty {
        SqlType::Char(len)
        | SqlType::VarChar(len)
        | SqlType::NChar(len)
        | SqlType::NVarChar(len) => *len,
        _ => Len::Max,
    }
}

/// What happens when the rendering of a `from` value is longer than the `to` target.
///
/// `DECLARE @b bigint = 123456; SELECT CAST(@b AS varchar(2));` raises 8115, and
/// `DECLARE @t tinyint = 123; SELECT CAST(@t AS varchar(2));` yields `*`.
fn overflow(from: &SqlType, to: &SqlType) -> Overflow {
    match from {
        SqlType::TinyInt | SqlType::SmallInt | SqlType::Int => match to {
            SqlType::Char(_) | SqlType::VarChar(_) => Overflow::Star,
            _ => Overflow::Error,
        },
        SqlType::BigInt
        | SqlType::Decimal { .. }
        | SqlType::Numeric { .. }
        | SqlType::Float
        | SqlType::Real
        | SqlType::Money
        | SqlType::SmallMoney
        // A GUID needs its 36 characters: a shorter target is an error, never a
        // truncation (`DECLARE @g uniqueidentifier = NEWID(); SELECT CAST(@g AS
        // varchar(35));` raises 8170, and `… AS varchar(36))` yields the GUID).
        | SqlType::UniqueIdentifier => Overflow::Error,
        // `bit`, binaries, dates and character sources: silent truncation.
        _ => Overflow::Truncate,
    }
}

/// The error a source too long for its character target raises: five numbers, chosen by
/// the source, and one wide family that flattens them all to 8115.
///
/// **The target prints its variable-length name in each of them**, `varchar` for a
/// `char(3)` as for a `varchar(3)`, like the 9809 of a binary source
/// ([`character_error_type`]): `DECLARE @f float = 1234567; SELECT CAST(@f AS char(3));`
/// raises 232 naming `varchar` (`tests/convert_to_character.rs`).
///
/// Towards `char` and `varchar`, with `DECLARE @x <type> = …; SELECT CAST(@x AS
/// varchar(3));`:
///
/// * an integer source names no type, 8115 state 2 on `expression`, even when the source
///   is a typed variable, which is why `"expression"` is passed rather than
///   `from.ty.error_name()`;
/// * a `decimal` or `numeric` source names itself, 8115 state 5;
/// * a `float` or `real` source raises **232**, which quotes the value;
/// * a `money` source raises **234** and a `smallmoney` source **292**, naming the target;
/// * a `uniqueidentifier` source raises **8170**, which names neither the target nor its
///   length.
///
/// Towards `nchar` and `nvarchar`, **only the two money numbers survive**
/// (`tests/convert_to_character.rs`): every other source raises 8115 state 2 on
/// `expression`, the `numeric`, the `float` and the GUID included.
fn overflow_error(v: &Value, from: &TypeInfo, to: &TypeInfo) -> SqlError {
    let national = matches!(to.ty, SqlType::NChar(_) | SqlType::NVarChar(_));
    let target = character_error_type(national);
    match (&from.ty, v) {
        (SqlType::Money, _) => errors::insufficient_result_space_money_to(&target),
        (SqlType::SmallMoney, _) => errors::insufficient_result_space_smallmoney_to(&target),
        _ if national => errors::arithmetic_overflow_from("expression", &target),
        (SqlType::Float, Value::F64(x)) => errors::overflow_for_type(&from.ty, &target, *x),
        (SqlType::Real, Value::F32(x)) => {
            errors::overflow_for_type(&from.ty, &target, f64::from(*x))
        }
        (SqlType::UniqueIdentifier, _) => errors::insufficient_result_space_guid(),
        _ if from.ty.family() == TypeFamily::Integer => {
            errors::arithmetic_overflow_from("expression", &target)
        }
        _ => errors::arithmetic_overflow_from(from.ty.error_name(), &target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scientific rendering at the level of the private helper: one digit before the
    /// point, the rest after, and a signed three-digit exponent.
    #[test]
    fn scientific_pads_the_exponent_to_three_digits() {
        assert_eq!(scientific(1234567.0, 8), "1.2345670e+006");
        assert_eq!(scientific(1234567.0, 16), "1.234567000000000e+006");
        assert_eq!(scientific(1234567.0, 17), "1.2345670000000000e+006");
        assert_eq!(scientific(0.1, 17), "1.0000000000000001e-001");
        assert_eq!(scientific(0.0, 8), "0.0000000e+000");
        assert_eq!(scientific(-1.5, 8), "-1.5000000e+000");
        assert_eq!(scientific(1e100, 8), "1.0000000e+100");
    }

    /// The decoding table of this file and the encoding table of `collation.rs` describe the
    /// same code page: every byte decodes to a character that encodes back to it. The five
    /// bytes the published layout leaves undefined stand for the C1 control character of the
    /// same number, which `code_page::encode` reads back.
    #[test]
    fn cp1252_char_agrees_with_cp1252_byte() {
        for byte in 0..=u8::MAX {
            let decoded = cp1252_char(byte);
            assert_eq!(
                crate::collation::cp1252_byte(decoded),
                Some(byte),
                "{byte:#04X}"
            );
        }
        assert_eq!(cp1252_char(0x4E), 'N');
        assert_eq!(cp1252_char(0xE9), 'é');
        assert_eq!(cp1252_char(0x80), '\u{20AC}');
    }

    /// Style 1 and style 2 drop whole bytes; style 1 loses two characters to its prefix.
    #[test]
    fn hex_styles_keep_whole_bytes() {
        let name = [0x4E, 0x61, 0x6D, 0x65];
        let varchar = |n: u16| SqlType::VarChar(Len::Fixed(n));
        let text = |to: SqlType, style: i32| {
            binary_as_character(&name, &to, Some(style)).expect("a supported style")
        };
        assert_eq!(text(varchar(40), 1), "0x4E616D65");
        assert_eq!(text(SqlType::Char(Len::Fixed(8)), 1), "0x4E616D");
        assert_eq!(text(varchar(3), 1), "0x");
        assert_eq!(text(varchar(1), 1), "");
        assert_eq!(text(varchar(3), 2), "4E");
        assert_eq!(text(SqlType::VarChar(Len::Max), 2), "4E616D65");
        assert_eq!(text(varchar(20), 0), "Name");
        assert_eq!(
            binary_as_character(&name, &varchar(20), Some(3))
                .expect_err("style 3 is not a binary style")
                .number,
            9809
        );
    }

    /// A national target reads the bytes as UTF-16LE, a last odd byte included.
    #[test]
    fn national_targets_read_utf16() {
        assert_eq!(decode_bytes(&[0x4E, 0x61, 0x6D, 0x65], true), "慎敭");
        assert_eq!(decode_bytes(&[0x4E, 0x61, 0x6D], true), "慎m");
        assert_eq!(decode_bytes(&[0x4E, 0x61, 0x6D, 0x65], false), "Name");
        assert_eq!(decode_bytes(&[], true), "");
    }

    #[test]
    fn four_decimals_keeps_every_ten_thousandth() {
        assert_eq!(four_decimals(42359819), "4235.9819");
        assert_eq!(four_decimals(-42359819), "-4235.9819");
        assert_eq!(four_decimals(5), "0.0005");
        assert_eq!(four_decimals(0), "0.0000");
    }

    #[test]
    fn group_thousands_starts_from_the_decimal_point() {
        assert_eq!(group_thousands("4235.98"), "4,235.98");
        assert_eq!(group_thousands("-4235.98"), "-4,235.98");
        assert_eq!(group_thousands("1234567890.12"), "1,234,567,890.12");
        assert_eq!(group_thousands("0.50"), "0.50");
        assert_eq!(group_thousands("100"), "100");
        assert_eq!(group_thousands("1000"), "1,000");
    }
}
