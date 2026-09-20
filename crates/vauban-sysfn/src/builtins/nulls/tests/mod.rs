use super::*;
use crate::context::StaticContext;
use vauban_types::{Len, SqlString, SqlType};

fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

fn bigint(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::BigInt, nullable)
}

fn varchar(length: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(length)), nullable)
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
fn isnull_returns_the_first_non_null() {
    let types = [int(true), int(true)];
    assert_eq!(
        eval(&ISNULL_DEF, &[Value::I32(1), Value::I32(2)], &types),
        Ok(Value::I32(1))
    );
    assert_eq!(
        eval(&ISNULL_DEF, &[Value::Null, Value::I32(2)], &types),
        Ok(Value::I32(2))
    );
    assert_eq!(
        eval(&ISNULL_DEF, &[Value::Null, Value::Null], &types),
        Ok(Value::Null)
    );
}

/// The half of `isnull_returns_the_first_non_null` that needs no conversion: the
/// short-circuit on a non-`NULL` `check`.
#[test]
fn isnull_returns_check_untouched_when_it_is_not_null() {
    let types = [int(true), int(true)];
    assert_eq!(
        eval(&ISNULL_DEF, &[Value::I32(1), Value::I32(2)], &types),
        Ok(Value::I32(1))
    );
    assert_eq!(
        eval(&ISNULL_DEF, &[Value::Null, Value::Null], &types),
        Ok(Value::Null)
    );
}

#[test]
fn isnull_return_type_is_the_first_argument() {
    let strict = isnull_return_type(&[varchar(3, true), varchar(10, false)])
        .expect("two character arguments are valid");
    assert_eq!(strict.ty, SqlType::VarChar(Len::Fixed(3)));
    // Learn, "ISNULL": the result is not nullable as soon as the replacement is not.
    assert!(!strict.nullable);

    let nullable = isnull_return_type(&[varchar(3, true), varchar(10, true)])
        .expect("two character arguments are valid");
    assert_eq!(nullable.ty, SqlType::VarChar(Len::Fixed(3)));
    assert!(nullable.nullable);
}

#[test]
fn isnull_truncates_to_the_type_of_check() {
    let values = [Value::Null, text("abcdef")];
    let types = [varchar(3, true), varchar(10, true)];
    let result = varchar(3, true);
    let args = EvalArgs {
        values: &values,
        types: &types,
        result: &result,
    };
    assert_eq!(
        isnull_eval(&args, &StaticContext::default()),
        Ok(text("abc"))
    );
}

#[test]
fn coalesce_returns_the_first_non_null() {
    let types = [int(true), int(true), int(true)];
    assert_eq!(
        eval(
            &COALESCE_DEF,
            &[Value::Null, Value::Null, Value::I32(3)],
            &types
        ),
        Ok(Value::I32(3))
    );
    assert_eq!(
        eval(
            &COALESCE_DEF,
            &[Value::Null, Value::Null, Value::Null],
            &types
        ),
        Ok(Value::Null)
    );
}

/// The half of `coalesce_returns_the_first_non_null` that needs no conversion.
#[test]
fn coalesce_returns_null_when_every_argument_is_null() {
    let types = [int(true), int(true), int(true)];
    assert_eq!(
        eval(
            &COALESCE_DEF,
            &[Value::Null, Value::Null, Value::Null],
            &types
        ),
        Ok(Value::Null)
    );
}

#[test]
fn coalesce_return_type_follows_precedence() {
    // Learn, "Data type precedence (Transact-SQL)": bigint outranks int, and both
    // outrank every character type.
    let widened = coalesce_return_type(&[int(true), bigint(true)]).expect("compatible types");
    assert_eq!(widened.ty, SqlType::BigInt);

    let longest =
        coalesce_return_type(&[varchar(3, true), varchar(10, true)]).expect("compatible types");
    assert_eq!(longest.ty, SqlType::VarChar(Len::Fixed(10)));

    // The typing succeeds even though `COALESCE(CAST(NULL AS int), 'abc')` would fail
    // at evaluation time with 245: converting the string is `types`' business.
    let numeric = coalesce_return_type(&[int(true), varchar(10, true)]).expect("compatible types");
    assert_eq!(numeric.ty, SqlType::Int);
}

#[test]
fn coalesce_return_type_is_nullable_if_any_argument_is() {
    let arguments = [int(true), int(false)];
    let coalesce = coalesce_return_type(&arguments).expect("compatible types");
    assert_eq!(coalesce.ty, SqlType::Int);
    assert!(coalesce.nullable);

    // The very same arguments, the opposite answer: Learn, "ISNULL", section comparing
    // the two functions.
    let isnull = isnull_return_type(&arguments).expect("compatible types");
    assert_eq!(isnull.ty, SqlType::Int);
    assert!(!isnull.nullable);
}

#[test]
fn coalesce_without_typed_argument_is_4127() {
    let err = coalesce_return_type(&[]).expect_err("an empty argument list is 4127");
    assert_eq!(err.number, 4127);
}

#[test]
fn nullif_returns_null_on_equality() {
    let types = [int(true), int(true)];
    assert_eq!(
        eval(&NULLIF_DEF, &[Value::I32(1), Value::I32(1)], &types),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&NULLIF_DEF, &[Value::I32(1), Value::I32(2)], &types),
        Ok(Value::I32(1))
    );
    assert_eq!(
        eval(&NULLIF_DEF, &[Value::Null, Value::I32(1)], &types),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&NULLIF_DEF, &[Value::I32(1), Value::Null], &types),
        Ok(Value::I32(1))
    );
}

/// The half of `nullif_returns_null_on_equality` that needs no conversion: a `NULL`
/// operand short-circuits both the conversion and the comparison.
#[test]
fn nullif_returns_a_when_an_argument_is_null() {
    let types = [int(true), int(true)];
    assert_eq!(
        eval(&NULLIF_DEF, &[Value::Null, Value::I32(1)], &types),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&NULLIF_DEF, &[Value::I32(1), Value::Null], &types),
        Ok(Value::I32(1))
    );
}

#[test]
fn nullif_uses_the_collation_for_strings() {
    let types = [varchar(10, true), varchar(10, true)];
    assert_eq!(
        eval(&NULLIF_DEF, &[text("abc"), text("ABC")], &types),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&NULLIF_DEF, &[text("abc"), text("abd")], &types),
        Ok(text("abc"))
    );
}

/// The value comes back in the type of the call, which the `binder` may have narrowed
/// below the type of the first argument. Without the conversion an `int` 1
/// would travel in a `tinyint` column.
#[test]
fn nullif_returns_the_value_in_the_result_type() {
    let values = [Value::I32(1), Value::I32(2)];
    let types = [int(true), int(true)];
    let narrowed = TypeInfo::new(SqlType::TinyInt, true);
    let args = EvalArgs {
        values: &values,
        types: &types,
        result: &narrowed,
    };
    assert_eq!(
        nullif_eval(&args, &StaticContext::default()),
        Ok(Value::I8(1))
    );

    // A NULL second argument takes the short-circuit branch, and converts too.
    let with_null = [Value::I32(1), Value::Null];
    let args = EvalArgs {
        values: &with_null,
        types: &types,
        result: &narrowed,
    };
    assert_eq!(
        nullif_eval(&args, &StaticContext::default()),
        Ok(Value::I8(1))
    );

    // Equal operands answer NULL, which carries no type to convert.
    let equal = [Value::I32(1), Value::I32(1)];
    let args = EvalArgs {
        values: &equal,
        types: &types,
        result: &narrowed,
    };
    assert_eq!(
        nullif_eval(&args, &StaticContext::default()),
        Ok(Value::Null)
    );
}

#[test]
fn nullif_return_type_is_always_nullable() {
    let result =
        nullif_return_type(&[int(false), int(false)]).expect("two int arguments are valid");
    assert_eq!(result.ty, SqlType::Int);
    assert!(result.nullable);
}

#[test]
fn the_three_functions_are_registered_as_scalars() {
    crate::builtins::register_builtins();
    for name in ["isnull", "COALESCE", "NullIf"] {
        let def = crate::lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
        assert_eq!(def.kind, FunctionKind::Scalar);
        assert!(def.aggregate.is_none());
        assert!(def.deterministic);
    }
}
