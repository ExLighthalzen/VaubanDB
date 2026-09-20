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
