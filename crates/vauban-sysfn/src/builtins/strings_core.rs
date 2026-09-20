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
mod tests;
