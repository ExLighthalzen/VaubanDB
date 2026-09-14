//! `LEN`, `DATALENGTH`, `LEFT`, `RIGHT`, `SUBSTRING`, `UPPER`, `LOWER`, `LTRIM` and `RTRIM`:
//! the string functions that need a length and a case and nothing else.
//!
//! Searching (`CHARINDEX`, `REPLACE`…) is `strings_search`, character codes (`CONCAT`,
//! `CHAR`…) `strings_codes`. What the nine functions here share is the measure of a string,
//! and the three ways SQL Server measures one (Microsoft Learn, "LEN (Transact-SQL)",
//! "DATALENGTH (Transact-SQL)", "SUBSTRING (Transact-SQL)"):
//!
//! | | counts | trailing spaces | `(max)` argument |
//! |---|---|---|---|
//! | `LEN` | characters | ignored | result is `bigint` |
//! | `DATALENGTH` | bytes the value occupies in its **declared** type | counted | result is `bigint` |
//! | `LEFT`, `RIGHT`, `SUBSTRING` | characters | kept | no effect |
//!
//! No count here is in UTF-8 bytes. `DATALENGTH` bills a character string in **UTF-16 code
//! units**: one byte per unit in a `varchar`, two in an `nvarchar`, code page 1252 spending
//! its byte on a `?` for a unit it cannot represent (`datalength_counts_utf16_code_units`):
//! `DATALENGTH(CAST(N'é' AS nvarchar(10)))` is 2 against 1 for the same character in a
//! `varchar`, `N'日本'` is 4 against 2, and U+1D11E (one character, two code units) is 4
//! against 2. `args.types[0]`, the declared type, is what tells the two families apart (a
//! [`Value::String`] carries neither family nor length), which is why `eval` receives an
//! [`EvalArgs`].
//!
//! No conversion and no type name is written here: a non-character argument goes through
//! [`vauban_types::convert`], which is what makes `LEN(1.50)` equal to 4 (the length of
//! `'1.50'`) and `LEN(NEWID())` equal to 36.

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{Collation, Len, SqlString, SqlType, TypeFamily, TypeInfo, Value, convert};

use crate::builtins::args::invalid_argument_type;
use crate::builtins::system::mantissa_bytes;
use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// Bytes one UTF-16 code unit of a `char` or `varchar` occupies: code page 1252, one byte
/// each, including the units it cannot represent (SQL Server stores them as `?`).
const BYTES_PER_NARROW_UNIT: i64 = 1;

/// Bytes one UTF-16 code unit of an `nchar` or `nvarchar` occupies (UCS-2).
const BYTES_PER_WIDE_UNIT: i64 = 2;

/// Widest declared length of a non-`(max)` `varchar` (Microsoft Learn, "char and varchar
/// (Transact-SQL)": `n` is 1 to 8000). Used as the declared length of the result when the
/// argument is not a character expression, see [`text_result_type`].
const WIDEST_VARCHAR: u16 = 8000;

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
    InternalError::Bug(format!("string function: argument {index} is missing")).into()
}

/// The internal error a conversion that did not answer the requested family deserves.
fn unexpected_conversion(target: &str) -> SqlError {
    InternalError::Bug(format!(
        "string function: conversion to {target} did not answer a {target}"
    ))
    .into()
}

/// `true` for the `(max)` character and binary types, whose length functions answer a
/// `bigint` (Microsoft Learn, "LEN" and "DATALENGTH": *"bigint if expression is of the
/// varchar(max), nvarchar(max) or varbinary(max) data types"*).
fn is_max(ty: &SqlType) -> bool {
    matches!(
        ty,
        SqlType::Char(Len::Max)
            | SqlType::VarChar(Len::Max)
            | SqlType::NChar(Len::Max)
            | SqlType::NVarChar(Len::Max)
            | SqlType::Binary(Len::Max)
            | SqlType::VarBinary(Len::Max)
    )
}

/// An unlimited `nvarchar`, the target every non-character argument is converted to.
///
/// `(max)` on purpose: the conversion must render the value, not truncate it, so that
/// `LEN` and `SUBSTRING` see the whole rendering.
fn unlimited_text() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Max), true)
}

/// The text of argument `index`, or `None` when it is `NULL`.
///
/// A character argument is read as it stands; anything else is converted by
/// [`vauban_types::convert`], which is the implicit conversion SQL Server applies
/// (`LEN(12345)` is 5, `LEFT(12345, 2)` is `'12'`). A `char(n)` argument is padded to its
/// declared length by the conversion, which is what makes `LEN(CAST('abc' AS char(5)))`
/// count 3 characters and not 5: the padding is removed again by the trailing-space rule.
fn text_arg(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<String>> {
    let (value, ty) = arg(args, index)?;
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    match convert(value, ty, &unlimited_text(), None)? {
        Value::String(s) => Ok(Some(s.text)),
        Value::Null => Ok(None),
        _ => Err(unexpected_conversion("string")),
    }
}

/// The integer value of argument `index`, or `None` when it is `NULL`.
///
/// The position and the length of `LEFT`, `RIGHT` and `SUBSTRING` are not necessarily
/// `int`: SQL Server converts implicitly, so `LEFT('abcdef', '3')` works. `bigint` is the
/// widest integer target, so no valid position is lost on the way.
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

/// Builds a string result.
fn text_value(text: String) -> Value {
    Value::String(SqlString { text })
}

/// Builds a length result in the integer type of the call: `bigint` for a `(max)`
/// argument, `int` otherwise ([`length_return_type`]).
fn length_value(result: &TypeInfo, length: i64) -> Value {
    match result.ty {
        SqlType::BigInt => Value::I64(length),
        _ => Value::I32(i32::try_from(length).unwrap_or(i32::MAX)),
    }
}

/// Result type of `LEN` and `DATALENGTH`: `int`, or `bigint` for a `(max)` argument.
///
/// No argument type is rejected here. `LEN` accepts the types this crate represents,
/// including the date and time types and `uniqueidentifier`, because they convert
/// implicitly to a string: `SELECT LEN(CAST('12:00' AS time));` is 16 and `SELECT
/// LEN(NEWID());` is 36 (`len_accepts_time_and_the_other_date_types`,
/// `len_accepts_uniqueidentifier`). The one 8116 SQL Server raises on `LEN` names `xml`, a
/// type VaubanDB does not represent.
fn length_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let arg = arg_type(args, 0)?;
    let ty = if is_max(&arg.ty) {
        SqlType::BigInt
    } else {
        SqlType::Int
    };
    Ok(TypeInfo::new(ty, arg.nullable))
}

/// Evaluates `LEN(s)`: the number of characters of `s`, trailing spaces excluded.
///
/// "Trailing spaces" means U+0020 and nothing else: a trailing tabulation counts. Leading
/// spaces count too, `LEN('  abc')` is 5.
///
/// Binary data is counted in bytes, without conversion: `SELECT LEN(CAST(0x01 AS
/// varbinary(max)));` is 1 (`len_of_varbinary_max_is_bigint`), which the rendering of a
/// binary value as `0x01` would not give.
fn len_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (value, ty) = arg(args, 0)?;
    if let (Value::Bytes(bytes), TypeFamily::Binary) = (value, ty.ty.family()) {
        let length = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        return Ok(length_value(args.result, length));
    }
    let Some(text) = text_arg(args, 0)? else {
        return Ok(Value::Null);
    };
    let length = i64::try_from(text.trim_end_matches(' ').chars().count()).unwrap_or(i64::MAX);
    Ok(length_value(args.result, length))
}

/// Evaluates `DATALENGTH(x)`: the number of bytes the value occupies **in its declared
/// type**, trailing spaces included.
///
/// The declared type is `args.types[0]` and nothing else: the same [`Value::String`] gives
/// 1 as a `varchar` and 2 as an `nvarchar`, and 5 as a `char(5)` holding `'abc'`
/// (`datalength_counts_bytes`). A [`Value::Null`] gives `NULL` and the declared type does
/// not save it, whether the `NULL` is bare or typed `int`, `decimal(38,0)`, `char(5)` or
/// `nvarchar(10)`. The check comes first because the `decimal` branch of [`byte_length`]
/// reads the value, where a `NULL` has no mantissa to read.
fn datalength_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (value, ty) = arg(args, 0)?;
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    Ok(length_value(args.result, byte_length(&ty.ty, value)))
}

/// The number of bytes a value of declared type `ty` occupies.
///
/// Four families of answers, the first three from Microsoft Learn, "Data types
/// (Transact-SQL)" and its per-type pages: a fixed size for the integer, approximate,
/// money, date and `uniqueidentifier` types; the **declared** length for `char(n)`,
/// `nchar(n)` and `binary(n)`, padding included; the length of the value itself for the
/// variable-length types, in UTF-16 code units for a character one ([`code_units`]).
///
/// The fourth family is `decimal(p, s)` and `numeric(p, s)`, the reason this function reads
/// the value at all: SQL Server bills the 32-bit words of their **scaled mantissa**, not the
/// bytes their precision reserves. `CAST(1 AS decimal(38,0))` is 5, `CAST(4294967295 AS
/// decimal(38,0))` is 5 and `CAST(4294967296 AS decimal(38,0))` is 9, where a rule on the
/// precision answers 17 for the three; at the constant value `1` the answer follows the
/// scale alone — 5 at scale 0, 9 at 10, 13 at 20, 17 at 30 — and at a value below 1 it still
/// follows the mantissa, `CAST(0.4294967295 AS decimal(38,10))` giving 5 against 9 for
/// `CAST(0.4294967296 AS decimal(38,10))` (`datalength_of_a_decimal_follows_the_mantissa`
/// and the tests next to it). The rule itself is [`mantissa_bytes`], shared with
/// `SQL_VARIANT_PROPERTY(..., 'MaxLength')` rather than written twice.
fn byte_length(ty: &SqlType, value: &Value) -> i64 {
    let units = code_units(value);
    let bytes = match value {
        Value::Bytes(b) => i64::try_from(b.len()).unwrap_or(i64::MAX),
        _ => 0,
    };
    match ty {
        SqlType::Bit | SqlType::TinyInt => 1,
        SqlType::SmallInt => 2,
        SqlType::Date => 3,
        SqlType::Int | SqlType::Real | SqlType::SmallDateTime | SqlType::SmallMoney => 4,
        SqlType::BigInt | SqlType::Float | SqlType::Money | SqlType::DateTime => 8,
        SqlType::UniqueIdentifier => 16,
        SqlType::Decimal { .. } | SqlType::Numeric { .. } => mantissa_bytes(value),
        // `date` (3) or nothing, plus the time itself, plus the offset (2) — Learn, "time",
        // "datetime2" and "datetimeoffset (Transact-SQL)", section *Storage size*.
        SqlType::Time(scale) => 3 + fraction_bytes(*scale),
        SqlType::DateTime2(scale) => 6 + fraction_bytes(*scale),
        SqlType::DateTimeOffset(scale) => 8 + fraction_bytes(*scale),
        SqlType::Char(len) => declared_or(len, units) * BYTES_PER_NARROW_UNIT,
        SqlType::NChar(len) => declared_or(len, units) * BYTES_PER_WIDE_UNIT,
        SqlType::VarChar(_) => units * BYTES_PER_NARROW_UNIT,
        SqlType::NVarChar(_) => units * BYTES_PER_WIDE_UNIT,
        SqlType::Binary(len) => declared_or(len, bytes),
        SqlType::VarBinary(_) => bytes,
    }
}

/// The number of UTF-16 code units a character value holds, `0` for anything else.
///
/// Not its number of characters: a character outside the BMP is a surrogate pair, and
/// `DATALENGTH` charges both halves. `CAST(N'𝄞' AS nvarchar(10))` (U+1D11E) is 4 bytes and
/// `CAST('𝄞' AS varchar(10))` is 2, where a count of characters would answer 2 and 1
/// (`datalength_counts_utf16_code_units`); the `char(n)` and `nchar(n)` forms report their
/// declared length instead.
fn code_units(value: &Value) -> i64 {
    match value {
        Value::String(s) => i64::try_from(s.text.encode_utf16().count()).unwrap_or(i64::MAX),
        _ => 0,
    }
}

/// The declared length of a fixed-length type, or `actual` when the type is `(max)` (a
/// form `char(max)` does not have in T-SQL, and which a synthetic type alone could take).
fn declared_or(len: &Len, actual: i64) -> i64 {
    match len {
        Len::Fixed(n) => i64::from(*n),
        Len::Max => actual,
    }
}

/// Extra bytes the fractional seconds of a `time`, `datetime2` or `datetimeoffset` cost:
/// none up to a scale of 2, one up to 4, two up to 7.
fn fraction_bytes(scale: u8) -> i64 {
    match scale {
        0..=2 => 0,
        3..=4 => 1,
        _ => 2,
    }
}

/// The character type `LEFT`, `RIGHT`, `SUBSTRING`, `UPPER`, `LOWER`, `LTRIM` and `RTRIM`
/// give back for an argument of type `arg`.
///
/// The family of the argument is kept (a `varchar` stays a `varchar`, an `nvarchar` stays
/// an `nvarchar`; `return_type_keeps_the_string_family`) with its collation; a `char(n)`
/// becomes a `varchar(n)` and an `nchar(n)` an `nvarchar(n)`: the return type of the
/// seven is `varchar` or `nvarchar`. A non-character argument is converted
/// implicitly and gives a `varchar` (`SELECT LEFT(12345, 2);` is a `varchar` column).
///
/// **Declared length, approximate rule.** SQL Server declares `LEFT(s, n)` as `varchar(n)`
/// when `n` is a constant. The `binder` does not propagate constants, so the length kept
/// here is the declared length of the argument, an upper bound and not a truncation, and
/// [`WIDEST_VARCHAR`] for a non-character argument.
fn text_result_type(arg: &TypeInfo) -> TypeInfo {
    let ty = match arg.ty {
        SqlType::Char(len) | SqlType::VarChar(len) => SqlType::VarChar(len),
        SqlType::NChar(len) | SqlType::NVarChar(len) => SqlType::NVarChar(len),
        _ => SqlType::VarChar(Len::Fixed(WIDEST_VARCHAR)),
    };
    TypeInfo {
        ty,
        nullable: arg.nullable,
        // Always a character type, so always a collation: the one of the argument when it
        // had one (Microsoft Learn, "Collation precedence", an argument's collation wins),
        // the default one for a converted argument.
        collation: Some(arg.collation.unwrap_or(Collation::DEFAULT)),
    }
}

/// Result type of `UPPER`, `LOWER`, `LTRIM` and `RTRIM`: [`text_result_type`] of their only
/// argument. Every type is accepted, as `LEN` accepts every type.
fn one_string_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(text_result_type(arg_type(args, 0)?))
}

/// Result type of `LEFT(s, n)` and `RIGHT(s, n)`: [`text_result_type`] of `s`, nullable as
/// soon as one of the two arguments is (`LEFT('abc', NULL)` is `NULL`).
///
/// No argument type is rejected: the argument is a type that converts implicitly to
/// `varchar` or `nvarchar`, and `SELECT LEFT(12345, 2);` answers `'12'`
/// (`left_and_right_basics`).
fn left_right_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let text = arg_type(args, 0)?;
    let count = arg_type(args, 1)?;
    Ok(TypeInfo {
        nullable: text.nullable || count.nullable,
        ..text_result_type(text)
    })
}

/// Result type of `SUBSTRING(s, start, length)`.
///
/// Unlike `LEFT`, `SUBSTRING` rejects what is not character or binary data instead of
/// converting it: `SELECT SUBSTRING(12345, 1, 2);` and `SELECT SUBSTRING(CAST('12:00' AS
/// time), 1, 2);` both raise 8116 on argument 1 (`substring_rejects_a_non_string_argument`).
/// The argument is a character or binary expression, with no implicit conversion.
///
/// A binary argument keeps its own type, since the substring of a `varbinary` is a
/// `varbinary` and not its hexadecimal rendering.
///
/// # The start and the length are checked here too
///
/// `SUBSTRING` does not convert its two numeric arguments either: it refuses at binding
/// time the types outside [`is_substring_offset`], with the same 8116 and the position of
/// the offending argument. `SELECT SUBSTRING('abc', 1, -1e0);` is 8116 on argument 3 and
/// opens no result set (`substring_offset_types_follow_the_engine`). The positions are
/// read left to right, argument 1 first: `SELECT SUBSTRING(12345, 1, 1e0);` names argument
/// **1** although argument 3 is invalid too.
///
/// The sign of the value plays no part at binding time: `SELECT SUBSTRING('abc', 1, 2e0);`
/// is 8116 as much as `-1e0`, which tells this check from the run-time 536 on a negative
/// length.
fn substring_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let text = arg_type(args, 0)?;
    let start = arg_type(args, 1)?;
    let length = arg_type(args, 2)?;
    let nullable = text.nullable || start.nullable || length.nullable;
    let result = match text.ty.family() {
        TypeFamily::Character => text_result_type(text),
        TypeFamily::Binary => text.clone(),
        _ => return Err(invalid_argument_type(&text.ty, 1, "substring")),
    };
    for (position, offset) in [(2, start), (3, length)] {
        if !is_substring_offset(&offset.ty) {
            return Err(invalid_argument_type(&offset.ty, position, "substring"));
        }
    }
    Ok(TypeInfo { nullable, ..result })
}

/// Whether `ty` is a type `SUBSTRING` accepts as a start or as a length.
///
/// Two families pass (`substring_rejects_a_start_or_a_length_of_the_wrong_type`):
/// `tinyint`, `smallint`, `int`, `bigint`, `decimal(p, s)` and `numeric(p, s)` give a
/// row, while `bit`, `float`, `real`, `money`, `smallmoney`, `char`, `varchar`, `nchar`,
/// `nvarchar`, `date`, `datetime`, `time`, `uniqueidentifier` and `binary` give 8116;
/// `varbinary` follows `binary` by family. The rule is therefore not "a number": `bit`,
/// `money` and `float` are numbers and are refused. A `decimal` offset drops its fraction
/// towards zero instead of rounding (`SUBSTRING('abcdef', 1, CAST(2.7 AS decimal(5,1)))`
/// is `'ab'` and not `'abc'`, and `SUBSTRING('abcdef', CAST(2.7 AS decimal(5,1)), 3)` is
/// `'bcd'` and not `'cde'`), which is [`vauban_types::convert`]'s business at evaluation
/// time.
///
/// `LEFT` and `RIGHT` do **not** share this check: `SELECT LEFT('abc', -1e0);` and `SELECT
/// RIGHT('abc', '-1');` open a result set and fail at run time, which is why
/// [`left_right_return_type`] rejects no type.
fn is_substring_offset(ty: &SqlType) -> bool {
    matches!(ty.family(), TypeFamily::Integer | TypeFamily::ExactNumeric)
}

/// Evaluates `LEFT(s, n)`: the first `n` characters of `s`.
///
/// `n` greater than the length of `s` gives the whole string, `n` equal to 0 the empty
/// string. A negative length the compiler does not fold, such as `LEFT('abc', CAST(-1 AS
/// bigint))` or `LEFT('abc', CAST(@@SPID * 0 - 1 AS int))`, is 537/16/2 with a result set
/// before the error (`runtime_negative_lengths_have_their_errors`). Swapping `'abc'` for
/// `CAST(NULL AS varchar(3))` under such a length answers NULL and no error
/// (`runtime_null_string_masks_negative_length`). A folded `int` -1 stops earlier, with
/// 536/16/6 while compiling and no result set; that check belongs to the `executor`.
fn left_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    if matches!(arg(args, 0)?.0, Value::Null) {
        return Ok(Value::Null);
    }
    let count = checked_length(args, 1, "left")?;
    let (Some(text), Some(count)) = (text_arg(args, 0)?, count) else {
        return Ok(Value::Null);
    };
    Ok(text_value(text.chars().take(count).collect()))
}

/// Evaluates `RIGHT(s, n)`: the last `n` characters of `s`, same bounds as `LEFT`. The
/// runtime error on a negative length the compiler does not fold is 536/16/2 here, and
/// not 537 as for `LEFT` (`runtime_negative_lengths_have_their_errors`). A folded `int`
/// -1 stops earlier, with 536/16/6 while compiling and no result set. A `NULL` string
/// answers NULL before the length is read.
fn right_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    if matches!(arg(args, 0)?.0, Value::Null) {
        return Ok(Value::Null);
    }
    let count = checked_length(args, 1, "right")?;
    let (Some(text), Some(count)) = (text_arg(args, 0)?, count) else {
        return Ok(Value::Null);
    };
    let skipped = text.chars().count().saturating_sub(count);
    Ok(text_value(text.chars().skip(skipped).collect()))
}

/// The length argument at `index`, as a number of characters, or `None` when it is `NULL`.
///
/// # Errors
///
/// A negative length is state 2 for char/varchar, state 3 for Unicode LEFT, and state 4
/// for Unicode RIGHT (`runtime_unicode_length_states_follow_the_result_family`).
/// Compilation checks of folded int lengths are performed separately by the executor.
fn checked_length(args: &EvalArgs<'_>, index: usize, function: &str) -> SqlResult<Option<usize>> {
    let Some(length) = integer_arg(args, index)? else {
        return Ok(None);
    };
    if length < 0 {
        return Err(runtime_length_error(args, function));
    }
    // A length wider than `usize` is a length longer than any string this process holds.
    Ok(Some(usize::try_from(length).unwrap_or(usize::MAX)))
}

/// Runtime error states for the character family selected by the function's type rule.
fn runtime_length_error(args: &EvalArgs<'_>, function: &str) -> SqlError {
    let unicode = matches!(args.result.ty, SqlType::NChar(_) | SqlType::NVarChar(_));
    SqlError::runtime_length_parameter(function, unicode)
}

/// Evaluates `SUBSTRING(s, start, length)`.
///
/// The window is the 1-based half-open interval `[start, start + length)` intersected with
/// the string, which is exactly the rule for a `start` below 1: the substring
/// starts at the first character and `length` is reduced by as much, so
/// `SUBSTRING('abcdef', 0, 3)` is `'ab'` and `SUBSTRING('abcdef', -5, 3)` is `''`
/// (`substring_clamps_start`). A `start` past the end gives the empty string, not an
/// error; a runtime length -1 gives 537/16/2 with `s = 'abc'` and start 1. A `NULL`
/// string or a `NULL` start answers NULL (`runtime_null_start_masks_negative_length`).
///
/// A binary argument is cut in bytes rather than in characters: its type says so, and its
/// result type is a binary type too.
fn substring_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    if matches!(arg(args, 0)?.0, Value::Null) {
        return Ok(Value::Null);
    }
    let start = integer_arg(args, 1)?;
    let length = integer_arg(args, 2)?;
    let (Some(start), Some(length)) = (start, length) else {
        return Ok(Value::Null);
    };
    if length < 0 {
        return Err(runtime_length_error(args, "substring"));
    }
    let (skipped, taken) = window(start, length);
    let (value, ty) = arg(args, 0)?;
    if let (Value::Bytes(bytes), TypeFamily::Binary) = (value, ty.ty.family()) {
        return Ok(Value::Bytes(
            bytes.iter().skip(skipped).take(taken).copied().collect(),
        ));
    }
    let Some(text) = text_arg(args, 0)? else {
        return Ok(Value::Null);
    };
    Ok(text_value(text.chars().skip(skipped).take(taken).collect()))
}

/// The window of `SUBSTRING` as a number of characters to skip and a number to take.
///
/// Computed in `i128` so that `start + length` cannot overflow whatever the two `bigint`
/// arguments hold; the result is clamped to the sizes a string can have.
fn window(start: i64, length: i64) -> (usize, usize) {
    let end = i128::from(start) + i128::from(length);
    let first = i128::from(start).max(1);
    let taken = (end - first).max(0);
    let skipped = first - 1;
    (
        usize::try_from(skipped).unwrap_or(usize::MAX),
        usize::try_from(taken).unwrap_or(usize::MAX),
    )
}

/// The single character `folded` produces, or `original` when it produces anything else.
///
/// Rust applies the **full** Unicode case folding, where `'ß'` upper-cases to the two
/// characters `"SS"`; SQL Server folds character by character and leaves `'ß'` alone
/// (`SELECT UPPER(N'straße');` is `STRAßE`; `upper_lower_fold_one_char_at_a_time`). Keeping the original
/// character whenever the folding is not one-to-one reproduces that.
fn single(mut folded: impl Iterator<Item = char>, original: char) -> char {
    match (folded.next(), folded.next()) {
        (Some(one), None) => one,
        _ => original,
    }
}

/// Evaluates `UPPER(s)`, character by character ([`single`]).
fn upper_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(text) = text_arg(args, 0)? else {
        return Ok(Value::Null);
    };
    Ok(text_value(
        text.chars().map(|c| single(c.to_uppercase(), c)).collect(),
    ))
}

/// Evaluates `LOWER(s)`, character by character ([`single`]).
fn lower_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(text) = text_arg(args, 0)? else {
        return Ok(Value::Null);
    };
    Ok(text_value(
        text.chars().map(|c| single(c.to_lowercase(), c)).collect(),
    ))
}

/// Evaluates `LTRIM(s)`: removes the leading **spaces** (U+0020), nothing else.
///
/// Not `str::trim_start`, which also removes tabulations and line breaks: SQL Server keeps
/// them (`ltrim_rtrim_only_remove_spaces`). The two-argument form of SQL Server 2022, which
/// removes the characters of a given set, is a later version.
fn ltrim_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(text) = text_arg(args, 0)? else {
        return Ok(Value::Null);
    };
    Ok(text_value(text.trim_start_matches(' ').to_owned()))
}

/// Evaluates `RTRIM(s)`: removes the trailing **spaces** (U+0020), nothing else, same rule
/// as [`ltrim_eval`].
fn rtrim_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(text) = text_arg(args, 0)? else {
        return Ok(Value::Null);
    };
    Ok(text_value(text.trim_end_matches(' ').to_owned()))
}

/// `LEN`: Microsoft Learn, "LEN (Transact-SQL)".
const LEN_DEF: FunctionDef = FunctionDef {
    name: "LEN",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: length_return_type,
    eval: len_eval,
    aggregate: None,
};

/// `DATALENGTH`: Microsoft Learn, "DATALENGTH (Transact-SQL)".
const DATALENGTH_DEF: FunctionDef = FunctionDef {
    name: "DATALENGTH",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: length_return_type,
    eval: datalength_eval,
    aggregate: None,
};

/// `LEFT`: Microsoft Learn, "LEFT (Transact-SQL)".
const LEFT_DEF: FunctionDef = FunctionDef {
    name: "LEFT",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: left_right_return_type,
    eval: left_eval,
    aggregate: None,
};

/// `RIGHT`: Microsoft Learn, "RIGHT (Transact-SQL)".
const RIGHT_DEF: FunctionDef = FunctionDef {
    name: "RIGHT",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: left_right_return_type,
    eval: right_eval,
    aggregate: None,
};

/// `SUBSTRING`: Microsoft Learn, "SUBSTRING (Transact-SQL)". The three-argument form is the
/// only one T-SQL has.
const SUBSTRING_DEF: FunctionDef = FunctionDef {
    name: "SUBSTRING",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(3),
    return_type: substring_return_type,
    eval: substring_eval,
    aggregate: None,
};

/// `UPPER`: Microsoft Learn, "UPPER (Transact-SQL)".
const UPPER_DEF: FunctionDef = FunctionDef {
    name: "UPPER",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: one_string_return_type,
    eval: upper_eval,
    aggregate: None,
};

/// `LOWER`: Microsoft Learn, "LOWER (Transact-SQL)".
const LOWER_DEF: FunctionDef = FunctionDef {
    name: "LOWER",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: one_string_return_type,
    eval: lower_eval,
    aggregate: None,
};

/// `LTRIM`: Microsoft Learn, "LTRIM (Transact-SQL)". One argument: the form that takes the
/// characters to remove belongs to a later version.
const LTRIM_DEF: FunctionDef = FunctionDef {
    name: "LTRIM",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: one_string_return_type,
    eval: ltrim_eval,
    aggregate: None,
};

/// `RTRIM`: Microsoft Learn, "RTRIM (Transact-SQL)", same restriction as [`LTRIM_DEF`].
const RTRIM_DEF: FunctionDef = FunctionDef {
    name: "RTRIM",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: one_string_return_type,
    eval: rtrim_eval,
    aggregate: None,
};

/// Registers the nine length-and-case string functions in the global registry.
pub(crate) fn register_all() {
    register(LEN_DEF);
    register(DATALENGTH_DEF);
    register(LEFT_DEF);
    register(RIGHT_DEF);
    register(SUBSTRING_DEF);
    register(UPPER_DEF);
    register(LOWER_DEF);
    register(LTRIM_DEF);
    register(RTRIM_DEF);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::StaticContext;
    use vauban_types::Decimal;

    /// `10^38`, one past the widest `decimal` T-SQL declares.
    const TEN_POW_38: i128 = 10i128.pow(38);

    fn varchar(length: u16) -> TypeInfo {
        TypeInfo::new(SqlType::VarChar(Len::Fixed(length)), true)
    }

    fn nvarchar(length: u16) -> TypeInfo {
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(length)), true)
    }

    fn char_n(length: u16) -> TypeInfo {
        TypeInfo::new(SqlType::Char(Len::Fixed(length)), true)
    }

    fn int() -> TypeInfo {
        TypeInfo::new(SqlType::Int, true)
    }

    fn text(value: &str) -> Value {
        Value::String(SqlString {
            text: value.to_owned(),
        })
    }

    /// Evaluates `def` the way the `executor` does: the result type of the call comes from
    /// `return_type`, so a test never has to guess it.
    fn eval(def: &FunctionDef, values: &[Value], types: &[TypeInfo]) -> SqlResult<Value> {
        let result = (def.return_type)(types)?;
        let args = EvalArgs {
            values,
            types,
            result: &result,
        };
        (def.eval)(&args, &StaticContext::default())
    }

    /// Evaluates a one-argument string function on a `varchar(50)` argument.
    fn eval_string(def: &FunctionDef, value: &str) -> SqlResult<Value> {
        eval(def, &[text(value)], &[varchar(50)])
    }

    /// Evaluates a two-argument function on a `varchar(50)` and an `int`.
    fn eval_string_int(def: &FunctionDef, value: &str, count: i32) -> SqlResult<Value> {
        eval(
            def,
            &[text(value), Value::I32(count)],
            &[varchar(50), int()],
        )
    }

    /// Evaluates `DATALENGTH` on the `decimal(precision, scale)` value `mantissa / 10^scale`,
    /// the way a `CAST` builds it: the mantissa carried by the value is already scaled to the
    /// declared scale (`vauban_types::convert`).
    fn datalength_of(mantissa: i128, precision: u8, scale: u8) -> SqlResult<Value> {
        let ty = TypeInfo::new(SqlType::Decimal { precision, scale }, true);
        eval(
            &DATALENGTH_DEF,
            &[Value::Decimal(Decimal {
                mantissa,
                precision,
                scale,
            })],
            std::slice::from_ref(&ty),
        )
    }

    /// Evaluates `SUBSTRING` on a `varchar(50)` and two `int`s.
    fn eval_substring(value: &str, start: i32, length: i32) -> SqlResult<Value> {
        eval(
            &SUBSTRING_DEF,
            &[text(value), Value::I32(start), Value::I32(length)],
            &[varchar(50), int(), int()],
        )
    }

    #[test]
    fn len_ignores_trailing_spaces() {
        assert_eq!(eval_string(&LEN_DEF, "abc  "), Ok(Value::I32(3)));
        assert_eq!(eval_string(&LEN_DEF, "  abc"), Ok(Value::I32(5)));
        assert_eq!(eval_string(&LEN_DEF, ""), Ok(Value::I32(0)));
        assert_eq!(eval_string(&LEN_DEF, "   "), Ok(Value::I32(0)));
        assert_eq!(
            eval(&LEN_DEF, &[Value::Null], &[varchar(50)]),
            Ok(Value::Null)
        );
        // Only U+0020 is a trailing space: the tabulation counts.
        assert_eq!(eval_string(&LEN_DEF, "ab\t"), Ok(Value::I32(3)));
    }

    #[test]
    fn len_counts_characters_not_bytes() {
        assert_eq!(
            eval(&LEN_DEF, &[text("é")], &[nvarchar(10)]),
            Ok(Value::I32(1))
        );
        assert_eq!(
            eval(&LEN_DEF, &[text("日本")], &[nvarchar(10)]),
            Ok(Value::I32(2))
        );
    }

    #[test]
    fn len_of_a_non_string_converts_first() {
        // `LEN(12345)` is 5: the argument is rendered before being counted.
        assert_eq!(
            eval(&LEN_DEF, &[Value::I32(12345)], &[int()]),
            Ok(Value::I32(5))
        );
    }

    #[test]
    fn len_return_type_is_bigint_for_max() {
        let int_result = length_return_type(&[varchar(10)]).expect("varchar(10) is accepted");
        assert_eq!(int_result.ty, SqlType::Int);

        for max in [
            SqlType::VarChar(Len::Max),
            SqlType::NVarChar(Len::Max),
            SqlType::VarBinary(Len::Max),
        ] {
            let result = length_return_type(&[TypeInfo::new(max, true)])
                .expect("a (max) argument is accepted");
            assert_eq!(result.ty, SqlType::BigInt, "for {}", max.declaration());
        }
    }

    #[test]
    fn len_of_varbinary_max_is_bigint() {
        let result = length_return_type(&[TypeInfo::new(SqlType::VarBinary(Len::Max), true)])
            .expect("varbinary(max) is accepted");
        assert_eq!(result.ty, SqlType::BigInt);
        // And it counts bytes, not the characters of a `0x01` rendering.
        assert_eq!(
            eval(
                &LEN_DEF,
                &[Value::Bytes(vec![0x01])],
                &[TypeInfo::new(SqlType::VarBinary(Len::Max), true)]
            ),
            Ok(Value::I64(1))
        );
    }

    /// `SELECT LEN(CAST('12:00' AS time));` answers 16, not 8116: the date and time types
    /// convert implicitly to a string, so `LEN` accepts them like the other types.
    #[test]
    fn len_accepts_time_and_the_other_date_types() {
        for ty in [
            SqlType::Time(7),
            SqlType::Date,
            SqlType::DateTime2(7),
            SqlType::DateTimeOffset(7),
        ] {
            let result = length_return_type(&[TypeInfo::new(ty, true)])
                .unwrap_or_else(|e| panic!("{} must be accepted: {}", ty.declaration(), e.message));
            assert_eq!(result.ty, SqlType::Int);
        }
    }

    #[test]
    fn len_accepts_uniqueidentifier() {
        let result = length_return_type(&[TypeInfo::new(SqlType::UniqueIdentifier, true)])
            .expect("uniqueidentifier is accepted: LEN(NEWID()) is 36");
        assert_eq!(result.ty, SqlType::Int);
        // And the value is the 36 characters of the rendered GUID.
        assert_eq!(
            eval(
                &LEN_DEF,
                &[Value::Guid([0x11; 16])],
                &[TypeInfo::new(SqlType::UniqueIdentifier, true)]
            ),
            Ok(Value::I32(36))
        );
    }

    #[test]
    fn datalength_counts_bytes() {
        // The declared type, and it alone, departs `varchar` from `nvarchar`: the same
        // `Value::String` is counted three ways below.
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("abc ")], &[varchar(10)]),
            Ok(Value::I32(4))
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("abc")], &[nvarchar(10)]),
            Ok(Value::I32(6))
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("abc")], &[char_n(5)]),
            Ok(Value::I32(5))
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[Value::I32(1)], &[int()]),
            Ok(Value::I32(4))
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[Value::Null], &[varchar(10)]),
            Ok(Value::Null)
        );
    }

    /// The same call built by hand, without `return_type`: the proof that nothing but
    /// `args.types[0]` is consulted, the values and the result type being identical in the
    /// two calls.
    #[test]
    fn datalength_reads_the_declared_type_only() {
        let values = [text("é")];
        let result = TypeInfo::new(SqlType::Int, true);
        let narrow = [varchar(10)];
        let wide = [nvarchar(10)];
        let ctx = StaticContext::default();
        assert_eq!(
            datalength_eval(
                &EvalArgs {
                    values: &values,
                    types: &narrow,
                    result: &result
                },
                &ctx
            ),
            Ok(Value::I32(1))
        );
        assert_eq!(
            datalength_eval(
                &EvalArgs {
                    values: &values,
                    types: &wide,
                    result: &result
                },
                &ctx
            ),
            Ok(Value::I32(2))
        );
    }

    #[test]
    fn datalength_of_max_is_bigint() {
        let ty = TypeInfo::new(SqlType::VarChar(Len::Max), true);
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("abc")], std::slice::from_ref(&ty)),
            Ok(Value::I64(3))
        );
    }

    #[test]
    fn datalength_of_the_fixed_size_types() {
        // Microsoft Learn, "Data types (Transact-SQL)", storage sizes.
        let sizes = [
            (SqlType::Bit, 1),
            (SqlType::TinyInt, 1),
            (SqlType::SmallInt, 2),
            (SqlType::Int, 4),
            (SqlType::BigInt, 8),
            (SqlType::Float, 8),
            (SqlType::Real, 4),
            (SqlType::Money, 8),
            (SqlType::SmallMoney, 4),
            (SqlType::Date, 3),
            (SqlType::Time(0), 3),
            (SqlType::Time(3), 4),
            (SqlType::Time(7), 5),
            (SqlType::DateTime, 8),
            (SqlType::SmallDateTime, 4),
            (SqlType::DateTime2(0), 6),
            (SqlType::DateTime2(7), 8),
            (SqlType::DateTimeOffset(0), 8),
            (SqlType::DateTimeOffset(7), 10),
            (SqlType::UniqueIdentifier, 16),
        ];
        for (ty, expected) in sizes {
            assert_eq!(
                byte_length(&ty, &Value::Bit(true)),
                expected,
                "for {}",
                ty.declaration()
            );
        }
        // The variable-length types are counted on the value, the fixed ones on the type.
        assert_eq!(
            byte_length(&SqlType::Binary(Len::Fixed(8)), &Value::Bytes(vec![1, 2])),
            8
        );
        assert_eq!(
            byte_length(&SqlType::VarBinary(Len::Max), &Value::Bytes(vec![1, 2])),
            2
        );
    }

    /// A `decimal(p, s)` is billed on its scaled mantissa and not on its declared
    /// precision.
    ///
    /// The first three vectors are the ones that **discriminate** the two rules: the three
    /// live in a `decimal(38,0)`, where a rule on the precision answers 17 each time and
    /// SQL Server answers 5, 9 and 5. The two that follow give the same answer under both
    /// rules and are here as the ends of the ladder, not as proof.
    #[test]
    fn datalength_of_a_decimal_follows_the_mantissa() {
        assert_eq!(datalength_of(4_294_967_295, 38, 0), Ok(Value::I32(5)));
        assert_eq!(datalength_of(4_294_967_296, 38, 0), Ok(Value::I32(9)));
        assert_eq!(datalength_of(1, 38, 0), Ok(Value::I32(5)));
        assert_eq!(datalength_of(1, 9, 0), Ok(Value::I32(5)));
        assert_eq!(datalength_of(TEN_POW_38 - 1, 38, 0), Ok(Value::I32(17)));
    }

    /// The four steps of the ladder are the powers of two, not the digits of the number:
    /// 2^32, 2^64 and 2^96 each cost a 32-bit word more.
    #[test]
    fn datalength_of_a_decimal_steps_on_the_powers_of_two() {
        let ladder = [
            (u64::MAX as i128, 9),
            (1i128 << 64, 13),
            ((1i128 << 96) - 1, 13),
            (1i128 << 96, 17),
        ];
        for (mantissa, expected) in ladder {
            assert_eq!(
                datalength_of(mantissa, 38, 0),
                Ok(Value::I32(expected)),
                "for a mantissa of {mantissa}"
            );
        }
    }

    /// The vector that establishes the rule: the **value is constant** at `1` and only the
    /// scale moves, so neither the printed value nor the precision can explain the answer —
    /// the scaled mantissa can.
    #[test]
    fn datalength_of_a_decimal_follows_the_scale_at_a_constant_value() {
        assert_eq!(datalength_of(1, 38, 0), Ok(Value::I32(5)));
        assert_eq!(datalength_of(10i128.pow(10), 38, 10), Ok(Value::I32(9)));
        assert_eq!(datalength_of(10i128.pow(20), 38, 20), Ok(Value::I32(13)));
        assert_eq!(datalength_of(10i128.pow(30), 38, 30), Ok(Value::I32(17)));
    }

    /// The sign leaves the answer alone, on `-1` and `-4294967296` in a `decimal(38,0)`,
    /// and a value below 1 is billed on its mantissa too: `0.4294967295` and
    /// `0.4294967296` in a `decimal(38,10)` are 5 and 9, though both print as a zero and a
    /// fraction.
    #[test]
    fn datalength_of_a_decimal_ignores_the_sign_and_the_decimal_point() {
        assert_eq!(datalength_of(0, 38, 0), Ok(Value::I32(5)));
        assert_eq!(datalength_of(-1, 38, 0), Ok(Value::I32(5)));
        assert_eq!(datalength_of(-4_294_967_296, 38, 0), Ok(Value::I32(9)));
        assert_eq!(datalength_of(4_294_967_295, 38, 10), Ok(Value::I32(5)));
        assert_eq!(datalength_of(4_294_967_296, 38, 10), Ok(Value::I32(9)));
    }

    /// `numeric` answers as `decimal` does, and a `NULL` of either stays `NULL`: the branch
    /// reads the mantissa of the value, and a `NULL` has no mantissa.
    #[test]
    fn datalength_of_a_numeric_matches_the_decimal_rule() {
        let ty = TypeInfo::new(
            SqlType::Numeric {
                precision: 20,
                scale: 0,
            },
            true,
        );
        assert_eq!(
            eval(
                &DATALENGTH_DEF,
                &[Value::Decimal(Decimal {
                    mantissa: 1,
                    precision: 20,
                    scale: 0,
                })],
                std::slice::from_ref(&ty),
            ),
            Ok(Value::I32(5))
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[Value::Null], std::slice::from_ref(&ty)),
            Ok(Value::Null)
        );
        let decimal_38_0 = TypeInfo::new(
            SqlType::Decimal {
                precision: 38,
                scale: 0,
            },
            true,
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[Value::Null], &[decimal_38_0]),
            Ok(Value::Null)
        );
    }

    /// `DATALENGTH` counts UTF-16 code units, so a character outside the BMP is charged
    /// twice in the two variable-length families.
    #[test]
    fn datalength_counts_utf16_code_units() {
        // U+1D11E: one character, two code units.
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("𝄞")], &[nvarchar(10)]),
            Ok(Value::I32(4))
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("𝄞")], &[varchar(10)]),
            Ok(Value::I32(2))
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("a𝄞")], &[nvarchar(10)]),
            Ok(Value::I32(6))
        );
        // A character of the BMP is one unit in the two families, whether code page 1252
        // can represent it or not.
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("日本")], &[nvarchar(10)]),
            Ok(Value::I32(4))
        );
        assert_eq!(
            eval(&DATALENGTH_DEF, &[text("日本")], &[varchar(10)]),
            Ok(Value::I32(2))
        );
    }

    #[test]
    fn left_and_right_basics() {
        assert_eq!(eval_string_int(&LEFT_DEF, "abcdef", 3), Ok(text("abc")));
        assert_eq!(eval_string_int(&RIGHT_DEF, "abcdef", 3), Ok(text("def")));
        assert_eq!(eval_string_int(&LEFT_DEF, "abc", 10), Ok(text("abc")));
        assert_eq!(eval_string_int(&RIGHT_DEF, "abc", 10), Ok(text("abc")));
        assert_eq!(eval_string_int(&LEFT_DEF, "abc", 0), Ok(text("")));
        assert_eq!(eval_string_int(&RIGHT_DEF, "abc", 0), Ok(text("")));
        // A `NULL` string, or a `NULL` length, gives `NULL`.
        assert_eq!(
            eval(
                &LEFT_DEF,
                &[Value::Null, Value::I32(2)],
                &[varchar(50), int()]
            ),
            Ok(Value::Null)
        );
        assert_eq!(
            eval(
                &LEFT_DEF,
                &[text("abc"), Value::Null],
                &[varchar(50), int()]
            ),
            Ok(Value::Null)
        );
    }

    #[test]
    fn runtime_negative_lengths_have_their_errors() {
        let source = text("abc");
        for (def, number, message) in [
            (
                &LEFT_DEF,
                537,
                "The length given to LEFT or SUBSTRING is not valid.",
            ),
            (
                &RIGHT_DEF,
                536,
                "The length given to the RIGHT function is not valid.",
            ),
            (
                &SUBSTRING_DEF,
                537,
                "The length given to LEFT or SUBSTRING is not valid.",
            ),
        ] {
            let (values, types) = if def.name == "SUBSTRING" {
                (
                    vec![source.clone(), Value::I32(1), Value::I32(-1)],
                    vec![varchar(3), int(), int()],
                )
            } else {
                (
                    vec![source.clone(), Value::I32(-1)],
                    vec![varchar(3), int()],
                )
            };
            let err = eval(def, &values, &types).expect_err("negative runtime length");
            assert_eq!(
                (err.number, err.severity, err.state, err.message.as_str()),
                (number, 16, 2, message)
            );
        }
    }

    #[test]
    fn runtime_unicode_length_states_follow_the_result_family() {
        for (def, number, state) in [
            (&LEFT_DEF, 537, 3),
            (&RIGHT_DEF, 536, 4),
            (&SUBSTRING_DEF, 537, 3),
        ] {
            for ty in [
                SqlType::NChar(Len::Fixed(3)),
                SqlType::NVarChar(Len::Fixed(3)),
            ] {
                let wide = TypeInfo::new(ty, true);
                let (values, types) = if def.name == "SUBSTRING" {
                    (
                        vec![text("abc"), Value::I32(1), Value::I32(-1)],
                        vec![wide, int(), int()],
                    )
                } else {
                    (vec![text("abc"), Value::I32(-1)], vec![wide, int()])
                };
                let error = eval(def, &values, &types).expect_err("negative Unicode length");
                assert_eq!(
                    (error.number, error.severity, error.state),
                    (number, 16, state)
                );
            }
        }
    }

    #[test]
    fn runtime_null_string_masks_negative_length() {
        for def in [&LEFT_DEF, &RIGHT_DEF] {
            assert_eq!(
                eval(def, &[Value::Null, Value::I32(-1)], &[varchar(3), int()]),
                Ok(Value::Null)
            );
        }
        assert_eq!(
            eval(
                &SUBSTRING_DEF,
                &[Value::Null, Value::I32(1), Value::I32(-1)],
                &[varchar(3), int(), int()]
            ),
            Ok(Value::Null)
        );
    }

    #[test]
    fn runtime_null_start_masks_negative_length() {
        assert_eq!(
            eval(
                &SUBSTRING_DEF,
                &[text("abc"), Value::Null, Value::I32(-1)],
                &[varchar(3), int(), int()]
            ),
            Ok(Value::Null)
        );
    }

    #[test]
    fn substring_clamps_start() {
        assert_eq!(eval_substring("abcdef", 2, 3), Ok(text("bcd")));
        assert_eq!(eval_substring("abcdef", 0, 3), Ok(text("ab")));
        assert_eq!(eval_substring("abcdef", -1, 3), Ok(text("a")));
        assert_eq!(eval_substring("abcdef", -5, 3), Ok(text("")));
        assert_eq!(eval_substring("abcdef", 10, 3), Ok(text("")));
        assert_eq!(eval_substring("abcdef", 2, 100), Ok(text("bcdef")));
        assert_eq!(eval_substring("abcdef", 1, 0), Ok(text("")));
    }

    #[test]
    fn substring_rejects_a_non_string_argument() {
        let err = substring_return_type(&[int(), int(), int()])
            .expect_err("an int argument is rejected by SUBSTRING");
        assert_eq!(err.number, 8116);
        assert_eq!(
            err.message,
            "Data type int is not accepted for argument 1 of the substring function."
        );
        // The same type is accepted by LEN and by LEFT, which convert instead.
        assert!(length_return_type(&[int()]).is_ok());
        assert!(left_right_return_type(&[int(), int()]).is_ok());
    }

    /// The fourteen types `SUBSTRING` refuses as a start or as a length, and the six it
    /// takes. The counter-test is the second half: `bit` and `money` are numbers and are
    /// refused, `decimal` is not an integer and is taken, so "a number" and "an integer"
    /// both fail to draw this line.
    #[test]
    fn substring_rejects_a_start_or_a_length_of_the_wrong_type() {
        let refused: [(SqlType, &str); 14] = [
            (SqlType::Bit, "bit"),
            (SqlType::Float, "float"),
            (SqlType::Real, "real"),
            (SqlType::Money, "money"),
            (SqlType::SmallMoney, "smallmoney"),
            (SqlType::Char(Len::Fixed(1)), "char"),
            (SqlType::VarChar(Len::Fixed(5)), "varchar"),
            (SqlType::NChar(Len::Fixed(1)), "nchar"),
            (SqlType::NVarChar(Len::Fixed(5)), "nvarchar"),
            (SqlType::Date, "date"),
            (SqlType::DateTime, "datetime"),
            (SqlType::Time(7), "time"),
            (SqlType::UniqueIdentifier, "uniqueidentifier"),
            (SqlType::Binary(Len::Fixed(1)), "binary"),
        ];
        for (ty, name) in refused {
            let wrong = TypeInfo::new(ty, true);
            for position in [2usize, 3] {
                let mut args = [varchar(10), int(), int()];
                args[position - 1] = wrong.clone();
                let Err(err) = substring_return_type(&args) else {
                    panic!("{name} must be refused at position {position}");
                };
                assert_eq!(err.number, 8116);
                assert_eq!(
                    err.message,
                    format!(
                        "Data type {name} is not accepted for argument {position} of the substring function."
                    )
                );
            }
        }
    }

    /// The six types `SUBSTRING` takes as a start or as a length, and the message of the
    /// fourteen it refuses.
    #[test]
    fn substring_offset_types_follow_the_engine() {
        for ty in [
            SqlType::TinyInt,
            SqlType::SmallInt,
            SqlType::Int,
            SqlType::BigInt,
            SqlType::Decimal {
                precision: 5,
                scale: 1,
            },
            SqlType::Numeric {
                precision: 5,
                scale: 0,
            },
        ] {
            let accepted = TypeInfo::new(ty, true);
            assert!(
                substring_return_type(&[varchar(10), accepted.clone(), accepted]).is_ok(),
                "{ty:?} is accepted as a start and as a length"
            );
        }

        let float = TypeInfo::new(SqlType::Float, true);
        let err = substring_return_type(&[varchar(10), int(), float.clone()])
            .expect_err("a float length is 8116");
        assert_eq!(err.number, 8116);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "Data type float is not accepted for argument 3 of the substring function."
        );

        // The start is named with its own position, and is read before the length.
        let err = substring_return_type(&[varchar(10), float.clone(), float])
            .expect_err("a float start is 8116");
        assert_eq!(
            err.message,
            "Data type float is not accepted for argument 2 of the substring function."
        );

        // Argument 1 is read before both of them.
        let err = substring_return_type(&[int(), int(), TypeInfo::new(SqlType::Float, true)])
            .expect_err("an int text argument is 8116");
        assert_eq!(
            err.message,
            "Data type int is not accepted for argument 1 of the substring function."
        );

        // LEFT and RIGHT keep taking what SUBSTRING refuses: SQL Server fails them at run
        // time instead.
        assert!(
            left_right_return_type(&[varchar(10), TypeInfo::new(SqlType::Float, true)]).is_ok()
        );
        assert!(left_right_return_type(&[varchar(10), varchar(5)]).is_ok());
    }

    #[test]
    fn substring_of_binary_cuts_bytes() {
        let ty = TypeInfo::new(SqlType::VarBinary(Len::Fixed(10)), true);
        let types = [ty.clone(), int(), int()];
        assert_eq!(
            eval(
                &SUBSTRING_DEF,
                &[Value::Bytes(vec![1, 2, 3, 4]), Value::I32(2), Value::I32(2)],
                &types
            ),
            Ok(Value::Bytes(vec![2, 3]))
        );
        // And the result stays binary.
        assert_eq!(
            substring_return_type(&types)
                .expect("binary is accepted")
                .ty,
            ty.ty
        );
    }

    #[test]
    fn upper_lower_fold_one_char_at_a_time() {
        assert_eq!(eval_string(&UPPER_DEF, "abcé"), Ok(text("ABCÉ")));
        assert_eq!(eval_string(&LOWER_DEF, "ABCÉ"), Ok(text("abcé")));
        // `ß` upper-cases to two characters in Unicode, to itself in SQL Server.
        assert_eq!(eval_string(&UPPER_DEF, "straße"), Ok(text("STRAßE")));
        assert_eq!(
            eval(&UPPER_DEF, &[Value::Null], &[varchar(50)]),
            Ok(Value::Null)
        );
        assert_eq!(
            eval(&LOWER_DEF, &[Value::Null], &[varchar(50)]),
            Ok(Value::Null)
        );
    }

    #[test]
    fn ltrim_rtrim_only_remove_spaces() {
        assert_eq!(eval_string(&LTRIM_DEF, "  ab "), Ok(text("ab ")));
        assert_eq!(eval_string(&RTRIM_DEF, " ab  "), Ok(text(" ab")));
        assert_eq!(eval_string(&RTRIM_DEF, "ab\t"), Ok(text("ab\t")));
        assert_eq!(eval_string(&LTRIM_DEF, "\tab"), Ok(text("\tab")));
        assert_eq!(eval_string(&LTRIM_DEF, "   "), Ok(text("")));
        assert_eq!(eval_string(&RTRIM_DEF, "   "), Ok(text("")));
        assert_eq!(
            eval(&LTRIM_DEF, &[Value::Null], &[varchar(50)]),
            Ok(Value::Null)
        );
    }

    #[test]
    fn return_type_keeps_the_string_family() {
        let one_argument = [&UPPER_DEF, &LOWER_DEF, &LTRIM_DEF, &RTRIM_DEF];
        for def in one_argument {
            let narrow = (def.return_type)(&[varchar(10)]).expect("varchar is accepted");
            assert_eq!(narrow.ty, SqlType::VarChar(Len::Fixed(10)), "{}", def.name);
            let wide = (def.return_type)(&[nvarchar(10)]).expect("nvarchar is accepted");
            assert_eq!(wide.ty, SqlType::NVarChar(Len::Fixed(10)), "{}", def.name);
        }

        for def in [&LEFT_DEF, &RIGHT_DEF] {
            let narrow = (def.return_type)(&[varchar(10), int()]).expect("varchar is accepted");
            assert_eq!(narrow.ty, SqlType::VarChar(Len::Fixed(10)), "{}", def.name);
            let wide = (def.return_type)(&[nvarchar(10), int()]).expect("nvarchar is accepted");
            assert_eq!(wide.ty, SqlType::NVarChar(Len::Fixed(10)), "{}", def.name);
        }

        let narrow =
            substring_return_type(&[varchar(10), int(), int()]).expect("varchar is accepted");
        assert_eq!(narrow.ty, SqlType::VarChar(Len::Fixed(10)));
        let wide =
            substring_return_type(&[nvarchar(10), int(), int()]).expect("nvarchar is accepted");
        assert_eq!(wide.ty, SqlType::NVarChar(Len::Fixed(10)));

        // A `char(n)` gives a `varchar(n)`.
        let from_char = (UPPER_DEF.return_type)(&[char_n(5)]).expect("char is accepted");
        assert_eq!(from_char.ty, SqlType::VarChar(Len::Fixed(5)));
        // A non-character argument gives a `varchar` too.
        let from_int = (LEFT_DEF.return_type)(&[int(), int()]).expect("int is accepted");
        assert_eq!(from_int.ty, SqlType::VarChar(Len::Fixed(WIDEST_VARCHAR)));
    }

    #[test]
    fn return_type_keeps_the_collation_of_the_argument() {
        let collation = Collation::parse("Latin1_General_BIN2").expect("a supported collation");
        let argument = TypeInfo {
            ty: SqlType::VarChar(Len::Fixed(10)),
            nullable: true,
            collation: Some(collation),
        };
        for result in [
            (UPPER_DEF.return_type)(std::slice::from_ref(&argument)),
            (LEFT_DEF.return_type)(&[argument.clone(), int()]),
            substring_return_type(&[argument.clone(), int(), int()]),
        ] {
            let result = result.expect("varchar is accepted");
            assert_eq!(result.collation, Some(collation));
        }
    }

    #[test]
    fn the_nine_functions_are_registered_as_scalars() {
        crate::builtins::register_builtins();
        for name in [
            "len",
            "DATALENGTH",
            "Left",
            "RIGHT",
            "substring",
            "UPPER",
            "lower",
            "LTRIM",
            "rtrim",
        ] {
            let def = crate::lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
            assert_eq!(def.kind, FunctionKind::Scalar);
            assert!(def.aggregate.is_none());
            assert!(def.deterministic);
        }
    }
}
