//! The string functions that **build** a string or read a character code, rather than
//! search inside one: `CONCAT`, `CHAR`, `ASCII`, `NCHAR`, `UNICODE` and `QUOTENAME`.
//!
//! They share nothing but that shape, and each has one trap of its own:
//!
//! | | trap |
//! |---|---|
//! | `CONCAT` | the one V1 function that reads `NULL` as the empty string, and does not answer `NULL` |
//! | `CHAR` | the argument names a **code page byte**, not a Unicode code point, and each of the 256 has a character |
//! | `ASCII` | symmetrically, the answer is a code page byte, an `nvarchar` argument included |
//! | `NCHAR` | a code outside `0..=65535` answers `NULL`, it is not an error |
//! | `UNICODE` | the answer is a UTF-16 **code unit**, and the empty string answers `NULL` |
//! | `QUOTENAME` | the closing delimiter found inside the string is doubled; an input over 128 characters or an unknown delimiter answers `NULL` |
//!
//! Microsoft Learn ("CONCAT", "CHAR", "ASCII", "NCHAR", "UNICODE", "QUOTENAME") supplies
//! the rest.
//!
//! # The code page belongs to `types`
//!
//! `CHAR` and `ASCII` read and write the code page of a collation, and this crate carries
//! no table of its own: both go through [`vauban_types::code_page`], the same table the
//! sort weights of a collation are indexed by.
//!
//! The code page is really needed rather than a Unicode code point: `SELECT CHAR(128),
//! CHAR(0x9C), ASCII('€'), ASCII(N'€');` answers `€`, `œ`, `128` and `128`
//! (`char_ascii_use_the_code_page_of_the_collation`). Code page 1252 and Latin-1 agree
//! everywhere but on `0x80..=0x9F`, and those are exactly the vectors above, so
//! `char::from_u32` would be wrong for 27 of the 256 codes.
//!
//! The 27 characters are the whole of what the page adds: the five bytes its published
//! layout leaves unassigned (`0x81`, `0x8D`, `0x8F`, `0x90`, `0x9D`) are **not** holes for
//! SQL Server, which reads them as the C1 control character of the same value: `CHAR(129)`
//! is a one-byte non-`NULL` string, `ASCII(CHAR(129))` and `UNICODE(CAST(CHAR(129) AS
//! nvarchar(2)))` are both `129`, `ASCII(NCHAR(0x8D))` is `141`, and `CAST(CHAR(0x9D) AS
//! nvarchar(2))` holds the UTF-16 bytes `0x9D00`. So `CHAR` answers `NULL` on an
//! out-of-range code and on nothing else, and `ASCII` answers `63` on a character the page
//! really has no byte for, `N'日'` and not `NCHAR(129)`.
//!
//! # Declared lengths
//!
//! `sys.dm_exec_describe_first_result_set` describes `CHAR(65)`, `ASCII('A')`,
//! `NCHAR(233)`, `UNICODE(N'é')`, `QUOTENAME('abc')` and `QUOTENAME('abc', '"')` as
//! `char(1)`, `int`, `nchar(1)`, `int`, `nvarchar(258)`, `nvarchar(258)`: the declared
//! length of `QUOTENAME` does not depend on its delimiter (`return_types`). The same view
//! types [`concat_return_type`].

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value, code_page, convert};

use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// Largest declared length of a `varchar`, in characters; beyond it comes `varchar(max)`.
const VARCHAR_LIMIT: u32 = 8000;

/// Largest declared length of an `nvarchar`, in characters.
const NVARCHAR_LIMIT: u32 = 4000;

/// Largest code `CHAR` accepts: the argument names a byte of the code page, and a code page
/// is single-byte. Outside `0..=255` the answer is `NULL` (`char_ascii_roundtrip`).
const MAX_CODE_PAGE_BYTE: i32 = 255;

/// The byte `ASCII` answers for a character the code page has no byte for **at all**.
///
/// `ASCII` takes a `varchar`, so an `nvarchar` argument is converted first, and that
/// conversion replaces an unmappable character by a question mark: `ASCII(N'日')` is
/// `63`, which is `?`.
///
/// A C1 control character is **not** unmappable: code page 1252 spends 27 of the bytes
/// `0x80..=0x9F` on characters of its own but keeps the five others for the control
/// character of the same value, so `ASCII(NCHAR(129))` is `129` and not `63`
/// (`char_ascii_use_the_code_page_of_the_collation`).
const UNMAPPABLE_BYTE: u8 = b'?';

/// Largest code `NCHAR` accepts under a collation without supplementary characters: the
/// argument names a UTF-16 code unit, which is 16 bits wide.
const MAX_CODE_UNIT: u32 = 65_535;

/// Longest string `QUOTENAME` accepts; a longer one answers `NULL` (Microsoft Learn,
/// "QUOTENAME (Transact-SQL)"; `quotename_doubles_the_closing_bracket`).
const QUOTENAME_LIMIT: usize = 128;

/// Declared length of the result of `QUOTENAME`: 128 characters that may all be doubled,
/// plus the two delimiters, as [`quotename_return_type`] documents.
const QUOTENAME_RESULT_LENGTH: u16 = 258;

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
    InternalError::Bug(format!("string function: argument {index} is missing")).into()
}

/// The text of a value [`vauban_types::convert`] has just moved to a character type.
///
/// Anything else is a bug of `types`, not a SQL error: the target was a character type and
/// the source was not `NULL`, so the answer can only be a [`Value::String`].
fn text_of(value: &Value) -> SqlResult<&str> {
    match value {
        Value::String(s) => Ok(&s.text),
        other => Err(InternalError::Bug(format!(
            "string function: conversion to a character type produced {other:?}"
        ))
        .into()),
    }
}

/// `nvarchar(max)`, the target every function of this module converts a character argument
/// to when it only needs its text: it truncates nothing and holds both families.
fn nvarchar_max() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Max), true)
}

/// Result type of `CONCAT(a, b, …)`: `nvarchar` as soon as one argument is `nchar` or
/// `nvarchar`, `varchar` otherwise, and **not** nullable.
///
/// The family is Microsoft Learn's table ("CONCAT (Transact-SQL)", *Return types*):
/// `CONCAT('a', N'b')` is an `nvarchar` (`concat_return_type_family`). It is a rule of
/// `CONCAT` and not the general precedence rule, so
/// [`vauban_types::implicit_result_type`] is not what answers it: that function would
/// make `CONCAT(1, 2)` an `int`, while SQL Server makes it a `varchar`.
///
/// The declared length is the sum of the lengths every argument contributes, capped at
/// `varchar(8000)` / `nvarchar(4000)`, and `(max)` as soon as one argument is `(max)`.
/// `sys.dm_exec_describe_first_result_set` describes the following calls:
///
/// ```text
/// SELECT column_ordinal, system_type_name FROM sys.dm_exec_describe_first_result_set(
///     N'SELECT CONCAT(CAST(''a'' AS varchar(3)), CAST(''b'' AS varchar(5))),
///              CONCAT(CAST(''a'' AS varchar(4000)), CAST(''b'' AS varchar(5000))),
///              CONCAT(CAST(''a'' AS nvarchar(3000)), CAST(''b'' AS nvarchar(3000))),
///              CONCAT(CAST(''a'' AS varchar(max)), ''b''),
///              CONCAT(CAST(''a'' AS varchar(3)), N''b'')', NULL, 0);
/// ```
///
/// answers `varchar(8)`, `varchar(8000)`, `nvarchar(4000)`, `varchar(max)`,
/// `nvarchar(4)`: the sum is capped, not promoted to `(max)`, and the length is counted in
/// characters across both families. The contribution of each type is
/// [`concat_argument_length`].
///
/// The result is not nullable because `CONCAT` does not answer `NULL` ([`concat_eval`]).
fn concat_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    // Two arguments at least: `check_call` guarantees it, the reads make it explicit.
    arg_type(args, 0)?;
    arg_type(args, 1)?;
    let wide = args
        .iter()
        .any(|arg| matches!(arg.ty, SqlType::NChar(_) | SqlType::NVarChar(_)));
    let limit = if wide { NVARCHAR_LIMIT } else { VARCHAR_LIMIT };
    let mut total: u32 = 0;
    let mut unbounded = false;
    for arg in args {
        match concat_argument_length(&arg.ty) {
            Some(length) => total = total.saturating_add(length),
            None => unbounded = true,
        }
    }
    let len = if unbounded {
        Len::Max
    } else {
        // `total.min(limit)` is at most 8000, so the conversion keeps its value.
        Len::Fixed(u16::try_from(total.min(limit)).unwrap_or(u16::MAX))
    };
    let ty = if wide {
        SqlType::NVarChar(len)
    } else {
        SqlType::VarChar(len)
    };
    Ok(TypeInfo::new(ty, false))
}

/// How many characters a `CONCAT` argument of type `ty` adds to the declared length of the
/// result, `None` for a `(max)` argument, which makes the whole result `(max)`.
///
/// A character or binary type contributes its own declared length; the other types
/// contribute a fixed width that has nothing to do with the shortest rendering of their
/// values (an `int` contributes 12 while `-2147483648` is 11 characters long, a `date`
/// contributes 40 while `2026-09-09` is 10). The widths are those
/// `sys.dm_exec_describe_first_result_set` describes for `CONCAT(CAST(NULL AS <type>),
/// CAST('z' AS varchar(1)))`, `varchar(n + 1)` with
///
/// | type | contribution |
/// |---|---|
/// | `bit` | 1 |
/// | `tinyint` | 4 |
/// | `smallint` | 6 |
/// | `int` | 12 |
/// | `bigint` | 24 |
/// | `float`, `real` | 23 |
/// | `money`, `smallmoney` | 40 |
/// | `decimal(p, s)`, `numeric(p, s)` | 41, whatever `p` and `s` |
/// | every date or time type, `uniqueidentifier` | 40 |
/// | `char(n)`, `varchar(n)`, `nchar(n)`, `nvarchar(n)`, `binary(n)`, `varbinary(n)` | `n` |
fn concat_argument_length(ty: &SqlType) -> Option<u32> {
    /// Width of the types that carry neither a declared length nor a shorter width.
    const WIDE: u32 = 40;
    match ty {
        SqlType::Bit => Some(1),
        SqlType::TinyInt => Some(4),
        SqlType::SmallInt => Some(6),
        SqlType::Int => Some(12),
        SqlType::BigInt => Some(24),
        SqlType::Float | SqlType::Real => Some(23),
        SqlType::Money | SqlType::SmallMoney => Some(WIDE),
        SqlType::Decimal { .. } | SqlType::Numeric { .. } => Some(41),
        SqlType::Char(len)
        | SqlType::VarChar(len)
        | SqlType::NChar(len)
        | SqlType::NVarChar(len)
        | SqlType::Binary(len)
        | SqlType::VarBinary(len) => match len {
            Len::Fixed(n) => Some(u32::from(*n)),
            Len::Max => None,
        },
        SqlType::Date
        | SqlType::Time(_)
        | SqlType::DateTime
        | SqlType::SmallDateTime
        | SqlType::DateTime2(_)
        | SqlType::DateTimeOffset(_)
        | SqlType::UniqueIdentifier => Some(WIDE),
    }
}

/// Evaluates `CONCAT(a, b, …)`: every argument as text, end to end, `NULL` reading as the
/// empty string.
///
/// The one function of the V1 that is not `NULL`-propagating (Microsoft Learn, "CONCAT
/// (Transact-SQL)": null values are implicitly converted to an empty string), so it does
/// not answer `NULL`: `CONCAT('a', NULL, 1)` is `a1` and `'[' + CONCAT(NULL, NULL) + ']'`
/// is `[]` (`concat_treats_null_as_empty`).
///
/// Each argument is converted to the result type of the call, whose declared length is the
/// sum of the arguments' ([`concat_return_type`]) and therefore does not truncate one of
/// them. A `binary` argument renders as [`vauban_types::convert`] renders it.
fn concat_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let mut text = String::new();
    for index in 0..args.values.len() {
        let (value, ty) = arg(args, index)?;
        if matches!(value, Value::Null) {
            continue;
        }
        let converted = convert(value, ty, args.result, None)?;
        text.push_str(text_of(&converted)?);
    }
    Ok(Value::String(SqlString { text }))
}

/// The value of an integer argument, moved to `int` by [`vauban_types::convert`].
///
/// `CHAR` and `NCHAR` both take "an integer expression": what a non-integer argument does
/// is the conversion's business, not theirs, so neither reads the value directly.
fn integer_argument(value: &Value, ty: &TypeInfo) -> SqlResult<i32> {
    let converted = convert(value, ty, &TypeInfo::new(SqlType::Int, true), None)?;
    match converted {
        Value::I32(code) => Ok(code),
        other => Err(InternalError::Bug(format!(
            "string function: conversion to int produced {other:?}"
        ))
        .into()),
    }
}

/// The code page a character argument, or a character result, is read through: the
/// collation of the type when it has one, the default collation otherwise.
fn collation_of(ty: &TypeInfo) -> Collation {
    ty.collation.unwrap_or(Collation::DEFAULT)
}

/// Result type of `CHAR(n)`: `char(1)`, nullable because a code outside `0..=255` answers
/// `NULL`.
///
/// `char` and not `varchar`: the column type of `CHAR(65)` is `char`, and `CHAR(256)` is
/// `NULL`, which is what makes the type nullable.
fn char_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    arg_type(args, 0)?;
    Ok(TypeInfo::new(SqlType::Char(Len::Fixed(1)), true))
}

/// Evaluates `CHAR(n)`: the character byte `n` stands for **in the code page of the
/// collation**.
///
/// Microsoft Learn, "CHAR (Transact-SQL)": the argument is an integer between 0 and 255,
/// and a value outside that range answers `NULL` rather than an error (`CHAR(256)` is
/// `NULL`). The byte is read through [`vauban_types::code_page::decode`], so `CHAR(128)`
/// is `€` and `CHAR(0x9C)` is `œ` where a Unicode reading would give C1 control
/// characters.
///
/// **Each byte inside the range has a character**, the five code page 1252 leaves
/// unassigned included: `CHAR(129)`, `CHAR(0x8D)`, `CHAR(0x8F)`, `CHAR(0x90)` and
/// `CHAR(0x9D)` answer the C1 control character of the same value, not `NULL`
/// (`char_ascii_use_the_code_page_of_the_collation`). `NULL` is therefore the answer of
/// an out-of-range code and of nothing else.
///
/// The collation is the one of the result type: the argument is an integer and carries
/// none, so the code page is the database's, which is what `check_call` puts in the result.
fn char_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (value, ty) = arg(args, 0)?;
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let code = integer_argument(value, ty)?;
    if !(0..=MAX_CODE_PAGE_BYTE).contains(&code) {
        return Ok(Value::Null);
    }
    let Ok(byte) = u8::try_from(code) else {
        // Unreachable: the range test above is exactly the domain of `u8`.
        return Ok(Value::Null);
    };
    let Some(character) = code_page::decode(byte, &collation_of(args.result)) else {
        // Unreachable too: `decode` answers `Some` for all 256 bytes. A `None` would be a
        // bug of `types`, not a `NULL` of `CHAR`.
        return Err(InternalError::Bug(format!(
            "CHAR: code page byte {byte} decodes to no character"
        ))
        .into());
    };
    Ok(Value::String(SqlString {
        text: character.to_string(),
    }))
}

/// Result type of `ASCII(s)`: `int`, nullable (`ASCII('')` is `NULL`).
fn ascii_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    arg_type(args, 0)?;
    Ok(TypeInfo::new(SqlType::Int, true))
}

/// Evaluates `ASCII(s)`: the code page byte of the **first character** of `s`.
///
/// Microsoft Learn, "ASCII (Transact-SQL)": the answer is the ASCII code value of the
/// leftmost character. "ASCII" names the function, not its domain: the byte comes from
/// the code page of the collation of the argument, so `ASCII('€')` is `128` and
/// `ASCII('é')` is `233`, through [`vauban_types::code_page::encode`].
///
/// An `nvarchar` argument goes through the same page, because SQL Server converts it to
/// `varchar` first: `ASCII(N'€')` is `128` too, and a character the page has no byte for
/// becomes the question mark that conversion substitutes, `ASCII(N'日')` being `63`
/// ([`UNMAPPABLE_BYTE`]). The five C1 control characters the page keeps a byte for are not
/// in that situation: `ASCII(NCHAR(0x8D))` is `141`.
///
/// The empty string answers `NULL` (`char_ascii_roundtrip`).
fn ascii_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (value, ty) = arg(args, 0)?;
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let collation = collation_of(ty);
    let converted = convert(value, ty, &nvarchar_max(), None)?;
    let Some(first) = text_of(&converted)?.chars().next() else {
        return Ok(Value::Null);
    };
    let byte = code_page::encode(first, &collation).unwrap_or(UNMAPPABLE_BYTE);
    Ok(Value::I32(i32::from(byte)))
}

/// Result type of `NCHAR(n)`: `nchar(1)`, nullable because a code outside the domain
/// answers `NULL`.
///
/// The column type of `NCHAR(233)` is `nchar(1)` and `NCHAR(65536)` is `NULL`
/// (`nchar_unicode_roundtrip`, `return_types`).
fn nchar_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    arg_type(args, 0)?;
    Ok(TypeInfo::new(SqlType::NChar(Len::Fixed(1)), true))
}

/// Evaluates `NCHAR(n)`: the character whose UTF-16 code unit is `n`.
///
/// Microsoft Learn, "NCHAR (Transact-SQL)": the argument is an integer expression, and
/// the accepted range is `0..=65535` when the collation of the database does not support
/// supplementary characters, the case of `SQL_Latin1_General_CP1_CI_AS`. Outside it the
/// answer is `NULL` and not an error: `NCHAR(65536)` is `NULL` and `NCHAR(0)` is the
/// character `U+0000`. The argument is moved to `int` by [`vauban_types::convert`], which
/// is what decides what a non-integer argument does.
///
/// **A deliberate difference from SQL Server.** SQL Server accepts a lone surrogate:
/// `UNICODE(NCHAR(0xD83D))` answers `55357`. A [`Value::String`] is Rust text, which cannot
/// hold an unpaired surrogate, so `NCHAR(0xD800..=0xDFFF)` answers `NULL` here. Lifting
/// the limit means changing how the whole engine represents a string, not this function.
fn nchar_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (value, ty) = arg(args, 0)?;
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let Ok(code) = u32::try_from(integer_argument(value, ty)?) else {
        return Ok(Value::Null);
    };
    if code > MAX_CODE_UNIT {
        return Ok(Value::Null);
    }
    // `None` for a lone surrogate, the known limit documented above.
    let Some(character) = char::from_u32(code) else {
        return Ok(Value::Null);
    };
    Ok(Value::String(SqlString {
        text: character.to_string(),
    }))
}

/// Result type of `UNICODE(s)`: `int`, nullable (`UNICODE(N'')` is `NULL`).
fn unicode_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    arg_type(args, 0)?;
    Ok(TypeInfo::new(SqlType::Int, true))
}

/// Evaluates `UNICODE(s)`: the UTF-16 code unit of the **first character** of `s`.
///
/// Microsoft Learn, "UNICODE (Transact-SQL)": the answer is the Unicode value of the
/// first character, and for a character outside the basic multilingual plane it is the
/// first code unit of its surrogate pair, which is why the answer is a code unit and not a
/// code point. An empty string answers `NULL`, and the first character alone is read:
/// `UNICODE(N'')` is `NULL` and `UNICODE(N'ab')` is `97` (`nchar_unicode_roundtrip`).
///
/// A non-character argument is moved to `nvarchar` first, as SQL Server's implicit
/// conversion does.
fn unicode_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (value, ty) = arg(args, 0)?;
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let converted = convert(value, ty, &nvarchar_max(), None)?;
    let Some(first) = text_of(&converted)?.chars().next() else {
        return Ok(Value::Null);
    };
    let mut units = [0u16; 2];
    let encoded = first.encode_utf16(&mut units);
    // `encode_utf16` writes one or two units and never zero, so the fallback is dead code
    // kept only to avoid an index that could panic on the query path.
    let Some(unit) = encoded.first() else {
        return Ok(Value::Null);
    };
    Ok(Value::I32(i32::from(*unit)))
}

/// Result type of `QUOTENAME(s [, quote_char])`: `nvarchar(258)`, nullable.
///
/// 258 is 128 characters that may each need doubling plus the two delimiters, and it is
/// the declared length of the module documentation; it is reached: `QUOTENAME` of 128
/// closing brackets is 258 characters long.
fn quotename_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    arg_type(args, 0)?;
    Ok(TypeInfo::new(
        SqlType::NVarChar(Len::Fixed(QUOTENAME_RESULT_LENGTH)),
        true,
    ))
}

/// The opening and closing delimiters `quote` designates, `None` when it is not one.
///
/// Microsoft Learn, "QUOTENAME (Transact-SQL)": the delimiter may be a single quotation
/// mark, a left or right bracket, a double quotation mark, a left or right parenthesis, a
/// greater-than or less-than sign, a left or right brace, or a backtick; anything else
/// answers `NULL`. Either character of a pair designates the pair, and the closing one is
/// the one that gets doubled inside the string: `QUOTENAME('a>b', '<')` is `<a>>b>` and
/// `QUOTENAME('a]b', ']')` is `[a]]b]` (`quotename_accepts_every_documented_delimiter`).
fn quote_pair(quote: char) -> Option<(char, char)> {
    match quote {
        '\'' => Some(('\'', '\'')),
        '"' => Some(('"', '"')),
        '`' => Some(('`', '`')),
        '[' | ']' => Some(('[', ']')),
        '(' | ')' => Some(('(', ')')),
        '{' | '}' => Some(('{', '}')),
        '<' | '>' => Some(('<', '>')),
        _ => None,
    }
}

/// The delimiter of a `QUOTENAME` call: the second argument when there is one, `[`
/// otherwise. `Ok(None)` means the call answers `NULL` without being an error.
fn quotename_delimiter(args: &EvalArgs<'_>) -> SqlResult<Option<(char, char)>> {
    let (Some(value), Some(ty)) = (args.values.get(1), args.types.get(1)) else {
        return Ok(Some(('[', ']')));
    };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let converted = convert(value, ty, &nvarchar_max(), None)?;
    let mut characters = text_of(&converted)?.chars();
    // A delimiter is "a one-character string": anything longer, the empty string
    // included, is not one and answers `NULL` like an unknown character would.
    match (characters.next(), characters.next()) {
        (Some(quote), None) => Ok(quote_pair(quote)),
        _ => Ok(None),
    }
}

/// Evaluates `QUOTENAME(s [, quote_char])`: `s` between its delimiters, with each
/// occurrence of the closing delimiter doubled.
///
/// `NULL` in three situations, not one of them an error (Microsoft Learn, "QUOTENAME
/// (Transact-SQL)"; `quotename_doubles_the_closing_bracket`): a `NULL` argument, a string
/// longer than 128 characters, and a delimiter outside the accepted list.
fn quotename_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (value, ty) = arg(args, 0)?;
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let Some((open, close)) = quotename_delimiter(args)? else {
        return Ok(Value::Null);
    };
    let converted = convert(value, ty, &nvarchar_max(), None)?;
    let text = text_of(&converted)?;
    if text.chars().count() > QUOTENAME_LIMIT {
        return Ok(Value::Null);
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push(open);
    for character in text.chars() {
        if character == close {
            quoted.push(close);
        }
        quoted.push(character);
    }
    quoted.push(close);
    Ok(Value::String(SqlString { text: quoted }))
}

/// `CONCAT`: Microsoft Learn, "CONCAT (Transact-SQL)".
///
/// Bounded, not variadic: a call with 255 arguments is 189, 2 to 254 arguments
/// (`concat_argument_limit`), which [`crate::check_call`] produces from an
/// [`Arity::Range`].
const CONCAT_DEF: FunctionDef = FunctionDef {
    name: "CONCAT",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Range(2, 254),
    return_type: concat_return_type,
    eval: concat_eval,
    aggregate: None,
};

/// `CHAR`: Microsoft Learn, "CHAR (Transact-SQL)".
const CHAR_DEF: FunctionDef = FunctionDef {
    name: "CHAR",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: char_return_type,
    eval: char_eval,
    aggregate: None,
};

/// `ASCII`: Microsoft Learn, "ASCII (Transact-SQL)".
const ASCII_DEF: FunctionDef = FunctionDef {
    name: "ASCII",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: ascii_return_type,
    eval: ascii_eval,
    aggregate: None,
};

/// `NCHAR`: Microsoft Learn, "NCHAR (Transact-SQL)".
const NCHAR_DEF: FunctionDef = FunctionDef {
    name: "NCHAR",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: nchar_return_type,
    eval: nchar_eval,
    aggregate: None,
};

/// `UNICODE`: Microsoft Learn, "UNICODE (Transact-SQL)".
const UNICODE_DEF: FunctionDef = FunctionDef {
    name: "UNICODE",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: unicode_return_type,
    eval: unicode_eval,
    aggregate: None,
};

/// `QUOTENAME`: Microsoft Learn, "QUOTENAME (Transact-SQL)".
const QUOTENAME_DEF: FunctionDef = FunctionDef {
    name: "QUOTENAME",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Range(1, 2),
    return_type: quotename_return_type,
    eval: quotename_eval,
    aggregate: None,
};

/// Registers `CONCAT`, `CHAR`, `ASCII`, `NCHAR`, `UNICODE` and `QUOTENAME` in the global
/// registry.
pub(crate) fn register_all() {
    register(CONCAT_DEF);
    register(CHAR_DEF);
    register(ASCII_DEF);
    register(NCHAR_DEF);
    register(UNICODE_DEF);
    register(QUOTENAME_DEF);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::check_call;
    use crate::context::StaticContext;

    fn int(nullable: bool) -> TypeInfo {
        TypeInfo::new(SqlType::Int, nullable)
    }

    fn varchar(length: u16, nullable: bool) -> TypeInfo {
        TypeInfo::new(SqlType::VarChar(Len::Fixed(length)), nullable)
    }

    fn nvarchar(length: u16, nullable: bool) -> TypeInfo {
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(length)), nullable)
    }

    fn text(value: &str) -> Value {
        Value::String(SqlString {
            text: value.to_owned(),
        })
    }

    /// Evaluates `def` on the given values and types, with the result type `check_call`
    /// would have computed.
    fn eval(def: &FunctionDef, values: &[Value], types: &[TypeInfo]) -> SqlResult<Value> {
        let result = (def.return_type)(types)?;
        let args = EvalArgs {
            values,
            types,
            result: &result,
        };
        (def.eval)(&args, &StaticContext::default())
    }

    #[test]
    fn concat_treats_null_as_empty() {
        // CONCAT('a', NULL, 1) is 'a1'.
        assert_eq!(
            eval(
                &CONCAT_DEF,
                &[text("a"), Value::Null, Value::I32(1)],
                &[varchar(1, false), int(true), int(false)],
            ),
            Ok(text("a1"))
        );
        // The same case: CONCAT(NULL, NULL) is the empty string, never NULL.
        assert_eq!(
            eval(
                &CONCAT_DEF,
                &[Value::Null, Value::Null],
                &[int(true), int(true)],
            ),
            Ok(text(""))
        );
        let result = concat_return_type(&[int(true), int(true)]).expect("two arguments are valid");
        assert!(!result.nullable, "CONCAT never answers NULL");
    }

    #[test]
    fn concat_return_type_family() {
        let same =
            concat_return_type(&[varchar(3, true), varchar(3, true)]).expect("valid arguments");
        assert_eq!(same.ty, SqlType::VarChar(Len::Fixed(6)));

        let wide =
            concat_return_type(&[varchar(3, true), nvarchar(3, true)]).expect("valid arguments");
        assert_eq!(wide.ty, SqlType::NVarChar(Len::Fixed(6)));

        // Not the precedence rule: `implicit_result_type(int, int)` is `int`, CONCAT is
        // still a `varchar`.
        let numbers = concat_return_type(&[int(true), int(true)]).expect("valid arguments");
        assert_eq!(numbers.ty, SqlType::VarChar(Len::Fixed(24)));
    }

    /// The declared length rule, with the vectors of the documentation of
    /// [`concat_return_type`].
    #[test]
    fn concat_declared_length_is_the_capped_sum() {
        let sum =
            concat_return_type(&[varchar(3, true), varchar(5, true)]).expect("valid arguments");
        assert_eq!(sum.ty, SqlType::VarChar(Len::Fixed(8)));

        let capped = concat_return_type(&[varchar(4000, true), varchar(5000, true)])
            .expect("valid arguments");
        assert_eq!(capped.ty, SqlType::VarChar(Len::Fixed(8000)));

        let wide_capped = concat_return_type(&[nvarchar(3000, true), nvarchar(3000, true)])
            .expect("valid arguments");
        assert_eq!(wide_capped.ty, SqlType::NVarChar(Len::Fixed(4000)));

        let unbounded = concat_return_type(&[
            TypeInfo::new(SqlType::VarChar(Len::Max), true),
            varchar(1, true),
        ])
        .expect("valid arguments");
        assert_eq!(unbounded.ty, SqlType::VarChar(Len::Max));

        let mixed =
            concat_return_type(&[varchar(3, true), nvarchar(1, true)]).expect("valid arguments");
        assert_eq!(mixed.ty, SqlType::NVarChar(Len::Fixed(4)));

        let integer = concat_return_type(&[int(true), varchar(5, true)]).expect("valid arguments");
        assert_eq!(integer.ty, SqlType::VarChar(Len::Fixed(17)));
    }

    #[test]
    fn concat_argument_limit() {
        let many = vec![varchar(1, false); 254];
        let result = check_call(&CONCAT_DEF, &many).expect("254 arguments are accepted");
        assert_eq!(result.ty, SqlType::VarChar(Len::Fixed(254)));

        let too_many = vec![varchar(1, false); 255];
        let err = check_call(&CONCAT_DEF, &too_many).expect_err("255 arguments are rejected");
        assert_eq!(err.number, 189);
        assert_eq!(err.severity, 15);
        assert_eq!(
            err.message,
            "The function concat takes between 2 and 254 arguments."
        );
    }

    #[test]
    fn char_ascii_roundtrip() {
        assert_eq!(
            eval(&CHAR_DEF, &[Value::I32(65)], &[int(false)]),
            Ok(text("A"))
        );
        assert_eq!(
            eval(&ASCII_DEF, &[text("A")], &[varchar(1, false)]),
            Ok(Value::I32(65))
        );
        // Outside 0..=255 the answer is NULL, not an error.
        assert_eq!(
            eval(&CHAR_DEF, &[Value::I32(-1)], &[int(false)]),
            Ok(Value::Null)
        );
        assert_eq!(
            eval(&CHAR_DEF, &[Value::I32(256)], &[int(false)]),
            Ok(Value::Null)
        );
        // The empty string has no first character.
        assert_eq!(
            eval(&ASCII_DEF, &[text("")], &[varchar(1, false)]),
            Ok(Value::Null)
        );
        for def in [&CHAR_DEF, &ASCII_DEF] {
            assert_eq!(eval(def, &[Value::Null], &[int(true)]), Ok(Value::Null));
        }
    }

    /// The vectors where code page 1252 and Latin-1 disagree, which is what proves the
    /// round trip goes through the code page and not through Unicode.
    #[test]
    fn char_ascii_use_the_code_page_of_the_collation() {
        for (code, expected) in [(128, "€"), (0x9C, "œ"), (233, "é")] {
            assert_eq!(
                eval(&CHAR_DEF, &[Value::I32(code)], &[int(false)]),
                Ok(text(expected)),
                "CHAR({code})"
            );
        }
        for (input, ty, expected) in [
            ("€", varchar(1, false), 128),
            ("€", nvarchar(1, false), 128),
            ("é", varchar(1, false), 233),
            // A character the page has no byte for: the question mark the conversion to
            // varchar substitutes.
            ("日", nvarchar(1, false), 63),
        ] {
            assert_eq!(
                eval(&ASCII_DEF, &[text(input)], &[ty]),
                Ok(Value::I32(expected)),
                "ASCII({input:?})"
            );
        }
        // The five bytes code page 1252 leaves unassigned in its published layout stand
        // for the C1 control character of the same value, and the round trip holds on
        // them like on the other bytes.
        for code in [0x81_i32, 0x8D, 0x8F, 0x90, 0x9D] {
            let scalar = u32::try_from(code).expect("a positive code");
            let control = char::from_u32(scalar).expect("a C1 control character");
            let as_text = text(&control.to_string());
            assert_eq!(
                eval(&CHAR_DEF, &[Value::I32(code)], &[int(false)]),
                Ok(as_text.clone()),
                "CHAR({code})"
            );
            assert_eq!(
                eval(&ASCII_DEF, &[as_text], &[nvarchar(1, false)]),
                Ok(Value::I32(code)),
                "ASCII(NCHAR({code}))"
            );
        }
        // Only the first character is read.
        assert_eq!(
            eval(&ASCII_DEF, &[text("Ab")], &[varchar(2, false)]),
            Ok(Value::I32(65))
        );
    }

    #[test]
    fn nchar_unicode_roundtrip() {
        assert_eq!(
            eval(&NCHAR_DEF, &[Value::I32(233)], &[int(false)]),
            Ok(text("é"))
        );
        assert_eq!(
            eval(&UNICODE_DEF, &[text("é")], &[nvarchar(1, false)]),
            Ok(Value::I32(233))
        );
        // Out of the domain of a non-SC collation.
        assert_eq!(
            eval(&NCHAR_DEF, &[Value::I32(65536)], &[int(false)]),
            Ok(Value::Null)
        );
        assert_eq!(
            eval(&NCHAR_DEF, &[Value::I32(-1)], &[int(false)]),
            Ok(Value::Null)
        );
        assert_eq!(
            eval(&NCHAR_DEF, &[Value::I32(0)], &[int(false)]),
            Ok(text("\u{0}"))
        );
        // The same case: the empty string has no first character, several characters read
        // only the first one.
        assert_eq!(
            eval(&UNICODE_DEF, &[text("")], &[nvarchar(1, false)]),
            Ok(Value::Null)
        );
        assert_eq!(
            eval(&UNICODE_DEF, &[text("ab")], &[nvarchar(2, false)]),
            Ok(Value::I32(97))
        );
        // A lone surrogate cannot live in a Rust string: the documented limit.
        assert_eq!(
            eval(&NCHAR_DEF, &[Value::I32(0xD83D)], &[int(false)]),
            Ok(Value::Null)
        );
        // Outside the basic multilingual plane, UNICODE answers the high surrogate.
        assert_eq!(
            eval(&UNICODE_DEF, &[text("😀")], &[nvarchar(2, false)]),
            Ok(Value::I32(0xD83D))
        );
        for def in [&NCHAR_DEF, &UNICODE_DEF] {
            assert_eq!(eval(def, &[Value::Null], &[int(true)]), Ok(Value::Null));
        }
    }

    #[test]
    fn quotename_doubles_the_closing_bracket() {
        let one = [varchar(130, true)];
        assert_eq!(
            eval(&QUOTENAME_DEF, &[text("abc")], &one),
            Ok(text("[abc]"))
        );
        assert_eq!(
            eval(&QUOTENAME_DEF, &[text("a]b")], &one),
            Ok(text("[a]]b]"))
        );
        let two = [varchar(130, true), varchar(1, true)];
        assert_eq!(
            eval(&QUOTENAME_DEF, &[text("abc"), text("\"")], &two),
            Ok(text("\"abc\""))
        );
        // 128 characters pass, 129 answer NULL.
        let long = "a".repeat(128);
        assert_eq!(
            eval(&QUOTENAME_DEF, &[text(&long)], &one),
            Ok(text(&format!("[{long}]")))
        );
        assert_eq!(
            eval(&QUOTENAME_DEF, &[text(&"a".repeat(129))], &one),
            Ok(Value::Null)
        );
        // An unknown delimiter, a NULL delimiter and a NULL string all answer NULL.
        assert_eq!(
            eval(&QUOTENAME_DEF, &[text("abc"), text("#")], &two),
            Ok(Value::Null)
        );
        assert_eq!(
            eval(&QUOTENAME_DEF, &[text("abc"), Value::Null], &two),
            Ok(Value::Null)
        );
        assert_eq!(eval(&QUOTENAME_DEF, &[Value::Null], &one), Ok(Value::Null));
    }

    /// Each documented delimiter, with its answer.
    #[test]
    fn quotename_accepts_every_documented_delimiter() {
        let two = [varchar(130, true), varchar(1, true)];
        for (input, quote, expected) in [
            ("a>b", "<", "<a>>b>"),
            ("a]b", "]", "[a]]b]"),
            ("a'b", "'", "'a''b'"),
            ("a`b", "`", "`a``b`"),
            ("a}b", "{", "{a}}b}"),
            ("a)b", "(", "(a))b)"),
        ] {
            assert_eq!(
                eval(&QUOTENAME_DEF, &[text(input), text(quote)], &two),
                Ok(text(expected)),
                "QUOTENAME({input:?}, {quote:?})"
            );
        }
        // The result fills its declared length: 128 closing brackets give 258 characters.
        let brackets = "]".repeat(128);
        let quoted = eval(&QUOTENAME_DEF, &[text(&brackets)], &[varchar(130, true)])
            .expect("128 characters are accepted");
        assert_eq!(text_of(&quoted).expect("a string").chars().count(), 258);
    }

    #[test]
    fn return_types() {
        assert_eq!(
            char_return_type(&[int(false)]).expect("one argument").ty,
            SqlType::Char(Len::Fixed(1))
        );
        assert_eq!(
            ascii_return_type(&[varchar(1, false)])
                .expect("one argument")
                .ty,
            SqlType::Int
        );
        assert_eq!(
            nchar_return_type(&[int(false)]).expect("one argument").ty,
            SqlType::NChar(Len::Fixed(1))
        );
        assert_eq!(
            unicode_return_type(&[nvarchar(1, false)])
                .expect("one argument")
                .ty,
            SqlType::Int
        );
        assert_eq!(
            quotename_return_type(&[varchar(10, true)])
                .expect("one argument")
                .ty,
            SqlType::NVarChar(Len::Fixed(QUOTENAME_RESULT_LENGTH))
        );
        // Every one of them may answer NULL, out of an out-of-range code, an empty string
        // or an unknown delimiter.
        for result in [
            char_return_type(&[int(false)]),
            ascii_return_type(&[varchar(1, false)]),
            nchar_return_type(&[int(false)]),
            unicode_return_type(&[nvarchar(1, false)]),
            quotename_return_type(&[varchar(10, false)]),
        ] {
            assert!(result.expect("one argument").nullable);
        }
    }

    #[test]
    fn the_functions_are_registered_as_scalars() {
        crate::builtins::register_builtins();
        for name in ["concat", "CHAR", "Ascii", "NCHAR", "Unicode", "QuoteName"] {
            let def = crate::lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
            assert_eq!(def.kind, FunctionKind::Scalar);
            assert!(def.aggregate.is_none());
            assert!(def.deterministic);
        }
    }
}
