//! The string functions that **search**: `CHARINDEX`, `PATINDEX`, `REPLACE`, `REPLICATE`,
//! `REVERSE` and `STUFF`.
//!
//! # Searching happens under the collation, and only in `types`
//!
//! `CHARINDEX` and `REPLACE` do not compare characters here: they call
//! [`Collation::find`], and `PATINDEX` calls [`Collation::pattern_position`]. Not one
//! collation rule is written in this crate — no lower-casing, no ordinal comparison, no
//! `LIKE` engine — which is why `CHARINDEX('B', 'abcabc')` is `2` under the default
//! `SQL_Latin1_General_CP1_CI_AS` (case insensitive) while `CHARINDEX('E', 'café')` is `0`
//! (accent sensitive), both in `charindex_uses_the_collation_primitive`. The collation is
//! read from the **declared type** of the argument
//! ([`TypeInfo::collation`](vauban_types::TypeInfo)), never from the value: a
//! [`Value::String`] carries no collation.
//!
//! # Everything counts in characters
//!
//! Every position and every length below is a number of characters, never a number of
//! UTF-8 bytes: the strings are turned into `char`s and indexed logically. `REVERSE(N'é日')`
//! is `N'日é'`, not a reversed byte sequence.
//!
//! Sources: Microsoft Learn, "CHARINDEX", "PATINDEX", "REPLACE", "REPLICATE", "REVERSE" and
//! "STUFF (Transact-SQL)".

use vauban_errors::SqlResult;
use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value, convert};

use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind};

/// Characters a non-`(max)` `varchar` result holds: 8 000 bytes of code page 1252.
const VARCHAR_MAX_CHARS: u16 = 8000;
/// Characters a non-`(max)` `nvarchar` result holds: the same 8 000 bytes, UTF-16.
const NVARCHAR_MAX_CHARS: u16 = 4000;

/// `CHARINDEX(needle, haystack [, start])`.
const CHARINDEX: FunctionDef = FunctionDef {
    name: "CHARINDEX",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Range(2, 3),
    return_type: position_return_type,
    eval: eval_charindex,
    aggregate: None,
};

/// `PATINDEX(pattern, s)`.
const PATINDEX: FunctionDef = FunctionDef {
    name: "PATINDEX",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: position_return_type,
    eval: eval_patindex,
    aggregate: None,
};

/// `REPLACE(s, from, to)`.
const REPLACE: FunctionDef = FunctionDef {
    name: "REPLACE",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(3),
    return_type: replace_return_type,
    eval: eval_replace,
    aggregate: None,
};

/// `REPLICATE(s, n)`.
const REPLICATE: FunctionDef = FunctionDef {
    name: "REPLICATE",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: replicate_return_type,
    eval: eval_replicate,
    aggregate: None,
};

/// `REVERSE(s)`.
const REVERSE: FunctionDef = FunctionDef {
    name: "REVERSE",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: reverse_return_type,
    eval: eval_reverse,
    aggregate: None,
};

/// `STUFF(s, start, length, insert)`.
const STUFF: FunctionDef = FunctionDef {
    name: "STUFF",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(4),
    return_type: stuff_return_type,
    eval: eval_stuff,
    aggregate: None,
};

/// Registers the six searching string functions.
pub(crate) fn register_all() {
    for def in [CHARINDEX, PATINDEX, REPLACE, REPLICATE, REVERSE, STUFF] {
        crate::registry::register(def);
    }
}

/// Result type of `CHARINDEX` and `PATINDEX`: `int`, or `bigint` when the searched string
/// is a `(max)` type.
///
/// Microsoft Learn, "CHARINDEX": *bigint if expressionToSearch is of the nvarchar(max),
/// varbinary(max), or varchar(max) data types; int otherwise*, and the same sentence for
/// `PATINDEX` (`charindex_on_a_max_haystack_answers_a_bigint`).
///
/// Both functions take the searched string as their **second** argument, which is why they
/// share this function.
fn position_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let searched = args.get(1).map(|info| info.ty);
    let big = matches!(
        searched,
        Some(
            SqlType::VarChar(Len::Max) | SqlType::NVarChar(Len::Max) | SqlType::VarBinary(Len::Max)
        )
    );
    let ty = if big { SqlType::BigInt } else { SqlType::Int };
    Ok(TypeInfo::new(ty, args.iter().any(|info| info.nullable)))
}

/// Result type of a function that returns a string: the family and the collation of its
/// character arguments.
///
/// Microsoft Learn, "REPLACE": *Returns nvarchar if one of the input arguments is of the
/// nvarchar data type; otherwise, REPLACE returns varchar*, the rule of data type
/// precedence, applied here to every function of this module. The collation is the one of
/// the first character argument (`REPLICATE(s, n)` and `STUFF(s, start, length, insert)`
/// take the collation of `s`), so a result keeps searching and comparing like its input.
///
/// The declared **length** is the widest of the family, `(max)` propagating from any
/// argument: these functions can return more characters than they receive (`REPLACE`,
/// `REPLICATE`, `STUFF`); the declared length matters as the truncation limit of
/// [`limit_chars`].
///
/// `always_nullable` is `true` for the two functions that turn valid arguments into `NULL`
/// (`REPLICATE` on a negative count, `STUFF` outside the bounds of its string).
fn string_return_type(args: &[TypeInfo], always_nullable: bool) -> SqlResult<TypeInfo> {
    let unicode = args
        .iter()
        .any(|info| matches!(info.ty, SqlType::NChar(_) | SqlType::NVarChar(_)));
    let max = args.iter().any(|info| {
        matches!(
            info.ty,
            SqlType::VarChar(Len::Max) | SqlType::NVarChar(Len::Max)
        )
    });
    let len = match (max, unicode) {
        (true, _) => Len::Max,
        (false, true) => Len::Fixed(NVARCHAR_MAX_CHARS),
        (false, false) => Len::Fixed(VARCHAR_MAX_CHARS),
    };
    let ty = if unicode {
        SqlType::NVarChar(len)
    } else {
        SqlType::VarChar(len)
    };
    let nullable = always_nullable || args.iter().any(|info| info.nullable);
    let mut info = TypeInfo::new(ty, nullable);
    if let Some(collation) = args.iter().find_map(|arg| arg.collation) {
        info.collation = Some(collation);
    }
    Ok(info)
}

/// Result type of `REPLACE`: [`string_return_type`], `NULL` only through its arguments.
fn replace_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    string_return_type(args, false)
}

/// Result type of `REPLICATE`: [`string_return_type`], nullable because a negative count
/// gives `NULL`.
fn replicate_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    string_return_type(args, true)
}

/// Result type of `REVERSE`: [`string_return_type`], `NULL` only through its argument.
fn reverse_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    string_return_type(args, false)
}

/// Result type of `STUFF`: [`string_return_type`], nullable because a start outside the
/// string gives `NULL`.
fn stuff_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    string_return_type(args, true)
}

/// `CHARINDEX(needle, haystack [, start])`: 1-based position of the first occurrence of
/// `needle` at or after `start`, `0` when there is none.
///
/// One call to [`Collation::find`] does the whole search, under the collation of the
/// haystack. `start` below `1` searches from the first character and a `start` past the end
/// finds nothing (`0`, `1` and `99` give `2`, `2` and `0`). An empty needle is not found,
/// `CHARINDEX('', 'abc')` being `0`, which is [`Collation::find`]'s own contract. `NULL`
/// in an argument, `start` included, gives `NULL` and not `0`
/// (`charindex_is_one_based_and_case_insensitive`, `charindex_honours_start`).
fn eval_charindex(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(needle), Some(haystack)) = (text_arg(args, 0)?, text_arg(args, 1)?) else {
        return Ok(Value::Null);
    };
    let start = if args.values.len() > 2 {
        match int_arg(args, 2)? {
            Some(start) => start,
            None => return Ok(Value::Null),
        }
    } else {
        1
    };
    // `start.max(1)` is positive, so the conversion never fails; a start beyond `usize`
    // cannot occur on a 64-bit target and would simply find nothing on a smaller one.
    let start = usize::try_from(start.max(1)).unwrap_or(usize::MAX);
    let position = collation_of(args, 1)
        .find(&haystack, &needle, start)
        .unwrap_or(0);
    Ok(position_value(args.result, position))
}

/// `PATINDEX(pattern, s)`: 1-based position of the first match of the `LIKE` pattern in
/// `s`, `0` when there is none.
///
/// One call to [`Collation::pattern_position`], which carries the whole `PATINDEX`
/// semantics: the wildcards `%`, `_`, `[...]` and `[^...]`, a pattern with no leading `%`
/// anchored at the beginning of the string (`PATINDEX('a_c', 'abc')` is `1` and
/// `PATINDEX('cd', 'abcdef')` is `0`), and the trailing blanks of the value forgiven. The
/// pattern is never taken apart here.
fn eval_patindex(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(pattern), Some(text)) = (text_arg(args, 0)?, text_arg(args, 1)?) else {
        return Ok(Value::Null);
    };
    let position = collation_of(args, 1)
        .pattern_position(&text, &pattern)
        .unwrap_or(0);
    Ok(position_value(args.result, position))
}

/// `REPLACE(s, from, to)`: every occurrence of `from` in `s`, found under the collation,
/// replaced by `to`.
///
/// The search is [`Collation::find`], so it ignores the case like the rest of the engine:
/// `REPLACE('ABCabc', 'b', 'X')` is `'AXCaXc'`. After a replacement the search resumes
/// **after the matched text**, not inside it: `REPLACE('aaa', 'aa', 'a')` is `'aa'` and
/// not `'a'`. An empty `from` is not found, so the string comes back unchanged; an empty
/// `to` deletes. `NULL` in one of the three arguments gives `NULL` (`replace_is_case_insensitive`).
fn eval_replace(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(text), Some(from), Some(to)) =
        (text_arg(args, 0)?, text_arg(args, 1)?, text_arg(args, 2)?)
    else {
        return Ok(Value::Null);
    };
    let collation = collation_of(args, 0);
    let chars: Vec<char> = text.chars().collect();
    let from_len = from.chars().count();
    let mut out = String::new();
    let mut cursor = 0;
    // `find` answers `None` on an empty `from`, so the loop stops at once and `out`
    // becomes the whole string: no special case needed.
    while let Some(position) = collation.find(&text, &from, cursor + 1) {
        let at = position - 1;
        out.extend(&chars[cursor..at]);
        out.push_str(&to);
        cursor = at + from_len;
    }
    out.extend(&chars[cursor..]);
    Ok(string_value(truncated(out, args.result)))
}

/// `REPLICATE(s, n)`: `s` repeated `n` times.
///
/// `n = 0` gives the empty string and `n < 0` gives `NULL`, not an error
/// (`replicate_and_reverse`). The result is truncated to the length of the result type, as
/// Microsoft Learn, "REPLICATE", prescribes: *If string_expression is not of type
/// varchar(max) or nvarchar(max), REPLICATE truncates the return value at 8,000 bytes*.
/// That truncation is also what keeps a huge `n` from asking for an impossible allocation.
fn eval_replicate(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(text), Some(count)) = (text_arg(args, 0)?, int_arg(args, 1)?) else {
        return Ok(Value::Null);
    };
    if count < 0 {
        return Ok(Value::Null);
    }
    let unit = text.chars().count();
    // `count` is not negative, so the conversion cannot fail.
    let count = usize::try_from(count).unwrap_or(0);
    let mut total = count.saturating_mul(unit);
    if let Some(limit) = limit_chars(args.result) {
        total = total.min(limit);
    }
    let mut out = String::new();
    if total > 0 {
        let mut written = 0;
        'copies: for _ in 0..count {
            for c in text.chars() {
                if written == total {
                    break 'copies;
                }
                out.push(c);
                written += 1;
            }
        }
    }
    Ok(string_value(out))
}

/// `REVERSE(s)`: the characters of `s` in reverse order.
///
/// Character by character, not byte by byte: `REVERSE(N'é日')` is `N'日é'`
/// (`replicate_and_reverse`). The result is as long as the input, so nothing truncates.
fn eval_reverse(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(text) = text_arg(args, 0)? else {
        return Ok(Value::Null);
    };
    Ok(string_value(text.chars().rev().collect()))
}

/// `STUFF(s, start, length, insert)`: `length` characters of `s` removed from position
/// `start` (1-based) and `insert` put in their place.
///
/// `NULL`, not an error, when `start` is `0` or negative, when `start` is past the end of
/// `s`, or when `length` is negative. A `length` longer than what is left simply deletes
/// up to the end (`STUFF('abcdef', 2, 99, 'X')` is `'aX'`) and a `NULL` `insert` deletes
/// without inserting (`STUFF('abcdef', 2, 3, NULL)` is `'aef'`) (`stuff_bounds`).
fn eval_stuff(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(text), Some(start), Some(length)) =
        (text_arg(args, 0)?, int_arg(args, 1)?, int_arg(args, 2)?)
    else {
        return Ok(Value::Null);
    };
    if start <= 0 || length < 0 {
        return Ok(Value::Null);
    }
    let chars: Vec<char> = text.chars().collect();
    // `start` is positive, so both conversions succeed; a position beyond `usize` is past
    // the end of any string this engine can hold.
    let from = usize::try_from(start - 1).unwrap_or(usize::MAX);
    if from >= chars.len() {
        return Ok(Value::Null);
    }
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    let to = from.saturating_add(length).min(chars.len());
    let mut out: String = chars[..from].iter().collect();
    // A `NULL` insert removes the portion and adds nothing.
    if let Some(insert) = text_arg(args, 3)? {
        out.push_str(&insert);
    }
    out.extend(&chars[to..]);
    Ok(string_value(out))
}

/// The `index`-th argument as text, `None` for `NULL`.
///
/// A character argument is used as it is. Any other type goes through
/// [`vauban_types::convert`] towards `nvarchar(max)`, the implicit conversion SQL
/// Server applies when a number or a date reaches a string function; the target is `(max)`
/// so that nothing is truncated on the way, and the family of a character target does not
/// change the text a value produces. Not one conversion rule is written here: converting
/// towards a character type is the business of the `types` crate.
fn text_arg(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<String>> {
    match &args.values[index] {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.text.clone())),
        other => {
            let to = TypeInfo::new(SqlType::NVarChar(Len::Max), true);
            match convert(other, &args.types[index], &to, None)? {
                Value::String(s) => Ok(Some(s.text)),
                Value::Null => Ok(None),
                // `convert` towards a character type answers a string or fails.
                other => unreachable!("convert to nvarchar(max) returned {other:?}"),
            }
        }
    }
}

/// The `index`-th argument as an integer, `None` for `NULL`.
///
/// The integer families are read directly; anything else — a string start, a `decimal`
/// count — goes through [`vauban_types::convert`] towards `bigint`, which is where the
/// rounding and the overflow rules live.
fn int_arg(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<i64>> {
    match &args.values[index] {
        Value::Null => Ok(None),
        Value::Bit(b) => Ok(Some(i64::from(*b))),
        Value::I8(n) => Ok(Some(i64::from(*n))),
        Value::I16(n) => Ok(Some(i64::from(*n))),
        Value::I32(n) => Ok(Some(i64::from(*n))),
        Value::I64(n) => Ok(Some(*n)),
        other => {
            let to = TypeInfo::new(SqlType::BigInt, true);
            match convert(other, &args.types[index], &to, None)? {
                Value::I64(n) => Ok(Some(n)),
                Value::Null => Ok(None),
                // `convert` towards `bigint` answers `Value::I64` or fails.
                other => unreachable!("convert to bigint returned {other:?}"),
            }
        }
    }
}

/// The collation of the `index`-th argument, the default one when it carries none (a
/// non-character argument converted on the fly).
fn collation_of(args: &EvalArgs<'_>, index: usize) -> Collation {
    args.types[index].collation.unwrap_or(Collation::DEFAULT)
}

/// A position as the value the call was typed for: `bigint` for a `(max)` haystack,
/// `int` otherwise ([`position_return_type`]).
fn position_value(result: &TypeInfo, position: usize) -> Value {
    let position = i64::try_from(position).unwrap_or(i64::MAX);
    match result.ty {
        SqlType::BigInt => Value::I64(position),
        _ => Value::I32(i32::try_from(position).unwrap_or(i32::MAX)),
    }
}

/// Wraps a `String` into the character [`Value`] the engine transports.
fn string_value(text: String) -> Value {
    Value::String(SqlString { text })
}

/// How many characters the result type holds, `None` for a `(max)` type.
fn limit_chars(result: &TypeInfo) -> Option<usize> {
    match result.ty {
        SqlType::Char(Len::Fixed(n))
        | SqlType::VarChar(Len::Fixed(n))
        | SqlType::NChar(Len::Fixed(n))
        | SqlType::NVarChar(Len::Fixed(n)) => Some(usize::from(n)),
        _ => None,
    }
}

/// `text` cut to the [`limit_chars`] of the result type, in characters.
fn truncated(text: String, result: &TypeInfo) -> String {
    match limit_chars(result) {
        Some(limit) if text.chars().count() > limit => text.chars().take(limit).collect(),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::register_builtins;
    use crate::context::StaticContext;
    use crate::{check_call, lookup};

    /// A `varchar` value with the declared type a literal of that text would carry.
    fn varchar(text: &str) -> (Value, TypeInfo) {
        let len = u16::try_from(text.chars().count().max(1)).unwrap_or(u16::MAX);
        (
            Value::String(SqlString {
                text: text.to_owned(),
            }),
            TypeInfo::new(SqlType::VarChar(Len::Fixed(len)), false),
        )
    }

    /// The same for an `nvarchar` literal (`N'…'`).
    fn nvarchar(text: &str) -> (Value, TypeInfo) {
        let len = u16::try_from(text.chars().count().max(1)).unwrap_or(u16::MAX);
        (
            Value::String(SqlString {
                text: text.to_owned(),
            }),
            TypeInfo::new(SqlType::NVarChar(Len::Fixed(len)), false),
        )
    }

    /// An `int` argument.
    fn int(n: i32) -> (Value, TypeInfo) {
        (Value::I32(n), TypeInfo::new(SqlType::Int, false))
    }

    /// A `NULL` argument of the given type, as the binder would hand it over.
    fn null(ty: SqlType) -> (Value, TypeInfo) {
        (Value::Null, TypeInfo::new(ty, true))
    }

    /// Evaluates a call the way the engine does: `lookup`, then `check_call`, then `eval`.
    fn call(name: &str, args: &[(Value, TypeInfo)]) -> Value {
        register_builtins();
        let def = lookup(name).expect("the function must be registered");
        let values: Vec<Value> = args.iter().map(|(v, _)| v.clone()).collect();
        let types: Vec<TypeInfo> = args.iter().map(|(_, t)| t.clone()).collect();
        let result = check_call(def, &types).expect("the call must type");
        (def.eval)(
            &EvalArgs {
                values: &values,
                types: &types,
                result: &result,
            },
            &StaticContext::default(),
        )
        .expect("eval must succeed")
    }

    /// The result type of a call, without evaluating it.
    fn result_type(name: &str, types: &[TypeInfo]) -> TypeInfo {
        register_builtins();
        let def = lookup(name).expect("the function must be registered");
        check_call(def, types).expect("the call must type")
    }

    /// A string result, for a shorter assertion.
    fn text(value: &Value) -> String {
        match value {
            Value::String(s) => s.text.clone(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    #[test]
    fn the_six_functions_are_registered_as_scalars() {
        register_builtins();
        for (name, arity) in [
            ("CHARINDEX", Arity::Range(2, 3)),
            ("PATINDEX", Arity::Exact(2)),
            ("REPLACE", Arity::Exact(3)),
            ("REPLICATE", Arity::Exact(2)),
            ("REVERSE", Arity::Exact(1)),
            ("STUFF", Arity::Exact(4)),
        ] {
            // Lower case on purpose: the registry is case-insensitive.
            let def = lookup(&name.to_ascii_lowercase()).expect("registered");
            assert_eq!(def.name, name);
            assert_eq!(def.kind, FunctionKind::Scalar);
            assert!(def.deterministic, "{name} is deterministic");
            assert_eq!(def.arity, arity);
            assert!(def.aggregate.is_none(), "{name} is not an aggregate");
        }
    }

    #[test]
    fn charindex_is_one_based_and_case_insensitive() {
        assert_eq!(
            call("CHARINDEX", &[varchar("b"), varchar("abcabc")]),
            Value::I32(2)
        );
        // The collation ignores the case: the upper-case needle finds the same position.
        assert_eq!(
            call("CHARINDEX", &[varchar("B"), varchar("abcabc")]),
            Value::I32(2)
        );
        assert_eq!(
            call("CHARINDEX", &[varchar("z"), varchar("abc")]),
            Value::I32(0)
        );
        assert_eq!(
            call(
                "CHARINDEX",
                &[null(SqlType::VarChar(Len::Fixed(1))), varchar("abc")]
            ),
            Value::Null
        );
        assert_eq!(
            call(
                "CHARINDEX",
                &[varchar("b"), null(SqlType::VarChar(Len::Fixed(3)))]
            ),
            Value::Null
        );
        // An empty needle is not found, even in an empty haystack.
        assert_eq!(
            call("CHARINDEX", &[varchar(""), varchar("abc")]),
            Value::I32(0)
        );
        assert_eq!(
            call("CHARINDEX", &[varchar(""), varchar("")]),
            Value::I32(0)
        );
    }

    #[test]
    fn charindex_honours_start() {
        assert_eq!(
            call("CHARINDEX", &[varchar("b"), varchar("abcabc"), int(3)]),
            Value::I32(5)
        );
        // A start below 1 searches from the first character.
        assert_eq!(
            call("CHARINDEX", &[varchar("b"), varchar("abcabc"), int(0)]),
            Value::I32(2)
        );
        assert_eq!(
            call("CHARINDEX", &[varchar("b"), varchar("abcabc"), int(-5)]),
            Value::I32(2)
        );
        assert_eq!(
            call("CHARINDEX", &[varchar("b"), varchar("abcabc"), int(99)]),
            Value::I32(0)
        );
        assert_eq!(
            call(
                "CHARINDEX",
                &[varchar("b"), varchar("abcabc"), null(SqlType::Int)]
            ),
            Value::Null
        );
    }

    #[test]
    fn charindex_uses_the_collation_primitive() {
        // Same answer as a direct call to the primitive, argument by argument: `sysfn`
        // normalises nothing of its own. `CHARINDEX('E', 'café')` is 0 (accent sensitive)
        // and `CHARINDEX('é', 'CAFÉ')` is 4 (case insensitive).
        let latin1 = Collation::DEFAULT;
        for (needle, haystack) in [
            ("E", "café"),
            ("é", "CAFÉ"),
            ("CD", "abcdef"),
            ("b", "abcabc"),
            ("", "abc"),
            ("abcd", "abc"),
        ] {
            let expected = latin1.find(haystack, needle, 1).unwrap_or(0);
            let expected = i32::try_from(expected).unwrap_or(i32::MAX);
            assert_eq!(
                call("CHARINDEX", &[varchar(needle), varchar(haystack)]),
                Value::I32(expected),
                "CHARINDEX({needle:?}, {haystack:?})"
            );
        }
        assert_eq!(
            call("CHARINDEX", &[varchar("E"), varchar("café")]),
            Value::I32(0)
        );
        assert_eq!(
            call("CHARINDEX", &[varchar("é"), varchar("CAFÉ")]),
            Value::I32(4)
        );
    }

    #[test]
    fn replace_is_case_insensitive() {
        assert_eq!(
            text(&call(
                "REPLACE",
                &[varchar("ABCabc"), varchar("b"), varchar("X")]
            )),
            "AXCaXc"
        );
        assert_eq!(
            text(&call(
                "REPLACE",
                &[varchar("abc"), varchar(""), varchar("X")]
            )),
            "abc"
        );
        assert_eq!(
            text(&call(
                "REPLACE",
                &[varchar("abc"), varchar("b"), varchar("")]
            )),
            "ac"
        );
        // The search resumes after the replaced text.
        assert_eq!(
            text(&call(
                "REPLACE",
                &[varchar("aaa"), varchar("aa"), varchar("a")]
            )),
            "aa"
        );
        assert_eq!(
            call(
                "REPLACE",
                &[
                    varchar("abc"),
                    varchar("b"),
                    null(SqlType::VarChar(Len::Fixed(1)))
                ]
            ),
            Value::Null
        );
    }

    #[test]
    fn replicate_and_reverse() {
        assert_eq!(text(&call("REPLICATE", &[varchar("ab"), int(3)])), "ababab");
        assert_eq!(text(&call("REPLICATE", &[varchar("ab"), int(0)])), "");
        assert_eq!(call("REPLICATE", &[varchar("ab"), int(-1)]), Value::Null);
        assert_eq!(
            call("REPLICATE", &[varchar("ab"), null(SqlType::Int)]),
            Value::Null
        );
        assert_eq!(text(&call("REVERSE", &[varchar("abc")])), "cba");
        // Character by character, not byte by byte.
        assert_eq!(text(&call("REVERSE", &[nvarchar("é日")])), "日é");
        assert_eq!(
            call("REVERSE", &[null(SqlType::VarChar(Len::Fixed(3)))]),
            Value::Null
        );
    }

    #[test]
    fn replicate_truncates_at_the_result_length() {
        // Microsoft Learn, "REPLICATE": the result is cut at 8 000 bytes unless the input
        // is a `(max)` type. `varchar` holds 8 000 characters, `nvarchar` 4 000.
        let long = call("REPLICATE", &[varchar("ab"), int(9000)]);
        assert_eq!(text(&long).chars().count(), usize::from(VARCHAR_MAX_CHARS));
        let long = call("REPLICATE", &[nvarchar("ab"), int(9000)]);
        assert_eq!(text(&long).chars().count(), usize::from(NVARCHAR_MAX_CHARS));
        // An empty string repeated a huge number of times stays empty and returns at once.
        assert_eq!(text(&call("REPLICATE", &[varchar(""), int(i32::MAX)])), "");
    }

    #[test]
    fn stuff_bounds() {
        assert_eq!(
            text(&call(
                "STUFF",
                &[varchar("abcdef"), int(2), int(3), varchar("XY")]
            )),
            "aXYef"
        );
        assert_eq!(
            call("STUFF", &[varchar("abcdef"), int(0), int(3), varchar("X")]),
            Value::Null
        );
        assert_eq!(
            call("STUFF", &[varchar("abcdef"), int(7), int(1), varchar("X")]),
            Value::Null
        );
        assert_eq!(
            call("STUFF", &[varchar("abcdef"), int(2), int(-1), varchar("X")]),
            Value::Null
        );
        // A NULL insert deletes the portion.
        assert_eq!(
            text(&call(
                "STUFF",
                &[
                    varchar("abcdef"),
                    int(2),
                    int(3),
                    null(SqlType::VarChar(Len::Fixed(1)))
                ]
            )),
            "aef"
        );
        // The last character, and a length longer than what is left.
        assert_eq!(
            text(&call(
                "STUFF",
                &[varchar("abcdef"), int(6), int(1), varchar("X")]
            )),
            "abcdeX"
        );
        assert_eq!(
            text(&call(
                "STUFF",
                &[varchar("abcdef"), int(2), int(99), varchar("X")]
            )),
            "aX"
        );
    }

    #[test]
    fn patindex_finds_the_pattern() {
        assert_eq!(
            call("PATINDEX", &[varchar("%bc%"), varchar("abcd")]),
            Value::I32(2)
        );
        assert_eq!(
            call("PATINDEX", &[varchar("%z%"), varchar("abcd")]),
            Value::I32(0)
        );
        assert_eq!(
            call("PATINDEX", &[varchar("a_c"), varchar("abc")]),
            Value::I32(1)
        );
        // A pattern with no leading `%` is anchored, and the collation ignores the case.
        assert_eq!(
            call("PATINDEX", &[varchar("cd"), varchar("abcdef")]),
            Value::I32(0)
        );
        assert_eq!(
            call("PATINDEX", &[varchar("%CD%"), varchar("abcdef")]),
            Value::I32(3)
        );
        assert_eq!(
            call(
                "PATINDEX",
                &[varchar("%bc%"), null(SqlType::VarChar(Len::Fixed(10)))]
            ),
            Value::Null
        );
    }

    #[test]
    fn patindex_delegates_to_the_collation_primitive() {
        let latin1 = Collation::DEFAULT;
        for (pattern, s) in [
            ("%cd%", "abcdef"),
            ("", "abc"),
            ("", ""),
            ("%", "abc"),
            ("abc", "abc "),
            ("%c", "abc "),
            ("a[b", "ab"),
        ] {
            let expected = latin1.pattern_position(s, pattern).unwrap_or(0);
            let expected = i32::try_from(expected).unwrap_or(i32::MAX);
            assert_eq!(
                call("PATINDEX", &[varchar(pattern), varchar(s)]),
                Value::I32(expected),
                "PATINDEX({pattern:?}, {s:?})"
            );
        }
    }

    #[test]
    fn return_type_keeps_the_string_family() {
        let varchar10 = TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), false);
        let nvarchar10 = TypeInfo::new(SqlType::NVarChar(Len::Fixed(10)), false);
        let int = TypeInfo::new(SqlType::Int, false);
        for (name, args) in [
            (
                "REPLACE",
                vec![varchar10.clone(), varchar10.clone(), varchar10.clone()],
            ),
            ("REPLICATE", vec![varchar10.clone(), int.clone()]),
            ("REVERSE", vec![varchar10.clone()]),
            (
                "STUFF",
                vec![
                    varchar10.clone(),
                    int.clone(),
                    int.clone(),
                    varchar10.clone(),
                ],
            ),
        ] {
            let info = result_type(name, &args);
            assert!(
                matches!(info.ty, SqlType::VarChar(_)),
                "{name} on varchar gives {:?}",
                info.ty
            );
            assert_eq!(
                info.collation, varchar10.collation,
                "{name} keeps the collation"
            );
        }
        for (name, args) in [
            (
                "REPLACE",
                vec![nvarchar10.clone(), nvarchar10.clone(), nvarchar10.clone()],
            ),
            ("REPLICATE", vec![nvarchar10.clone(), int.clone()]),
            ("REVERSE", vec![nvarchar10.clone()]),
            (
                "STUFF",
                vec![
                    nvarchar10.clone(),
                    int.clone(),
                    int.clone(),
                    nvarchar10.clone(),
                ],
            ),
        ] {
            let info = result_type(name, &args);
            assert!(
                matches!(info.ty, SqlType::NVarChar(_)),
                "{name} on nvarchar gives {:?}",
                info.ty
            );
            assert_eq!(
                info.collation, nvarchar10.collation,
                "{name} keeps the collation"
            );
        }
        // One Unicode argument is enough to make the whole result Unicode.
        assert!(matches!(
            result_type(
                "REPLACE",
                &[varchar10.clone(), varchar10.clone(), nvarchar10.clone()]
            )
            .ty,
            SqlType::NVarChar(_)
        ));

        // CHARINDEX and PATINDEX: `int`, and `bigint` on a `(max)` haystack.
        let varchar_max = TypeInfo::new(SqlType::VarChar(Len::Max), false);
        let nvarchar_max = TypeInfo::new(SqlType::NVarChar(Len::Max), false);
        for name in ["CHARINDEX", "PATINDEX"] {
            assert_eq!(
                result_type(name, &[varchar10.clone(), varchar10.clone()]).ty,
                SqlType::Int
            );
            assert_eq!(
                result_type(name, &[varchar10.clone(), varchar_max.clone()]).ty,
                SqlType::BigInt
            );
            assert_eq!(
                result_type(name, &[varchar10.clone(), nvarchar_max.clone()]).ty,
                SqlType::BigInt
            );
            // The `(max)` needle does not change anything: only the searched string does.
            assert_eq!(
                result_type(name, &[varchar_max.clone(), varchar10.clone()]).ty,
                SqlType::Int
            );
        }
        assert_eq!(
            result_type(
                "CHARINDEX",
                &[varchar10.clone(), varchar10.clone(), int.clone()]
            )
            .ty,
            SqlType::Int
        );
    }

    #[test]
    fn charindex_on_a_max_haystack_answers_a_bigint() {
        let haystack = (
            Value::String(SqlString {
                text: "abcabc".to_owned(),
            }),
            TypeInfo::new(SqlType::VarChar(Len::Max), false),
        );
        assert_eq!(call("CHARINDEX", &[varchar("b"), haystack]), Value::I64(2));
    }

    /// `CHARINDEX(needle, 12345)` converts its argument to a string first, which is
    /// `convert` towards a character type; this module has no conversion of its own.
    #[test]
    fn charindex_converts_a_non_character_argument() {
        assert_eq!(
            call("CHARINDEX", &[varchar("34"), int(12345)]),
            Value::I32(3)
        );
    }
}
