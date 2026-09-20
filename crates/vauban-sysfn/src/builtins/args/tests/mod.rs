use super::*;
use crate::context::EvalContext;
use crate::registry::{EvalArgs, FunctionKind};
use vauban_types::Value;

fn int_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Int, false))
}

/// Returns the number of arguments as a type parameter, so that a test can tell
/// `return_type` really ran and really saw the arguments.
fn count_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let count = u8::try_from(args.len()).unwrap_or(u8::MAX);
    Ok(TypeInfo::new(SqlType::Time(count.min(7)), false))
}

fn eval_null(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::Null)
}

fn def(name: &'static str, arity: Arity) -> FunctionDef {
    FunctionDef {
        name,
        kind: FunctionKind::Scalar,
        deterministic: true,
        arity,
        return_type: int_type,
        eval: eval_null,
        aggregate: None,
    }
}

fn types(n: usize) -> Vec<TypeInfo> {
    vec![TypeInfo::new(SqlType::Int, true); n]
}

#[test]
fn check_call_rejects_wrong_exact_arity() {
    let def = def("TEST_LEN", Arity::Exact(1));
    for count in [0, 2, 3] {
        let err = check_call(&def, &types(count)).expect_err("arity must be rejected");
        assert_eq!(err.number, 174);
        assert_eq!(err.severity, 15);
        // Lower case, regardless of the case of the registration.
        assert_eq!(
            err.message,
            "The function test_len takes exactly 1 argument(s)."
        );
    }
}

#[test]
fn check_call_rejects_wrong_range_arity() {
    let def = def("TEST_ROUND", Arity::Range(2, 3));
    for count in [1, 4] {
        let err = check_call(&def, &types(count)).expect_err("arity must be rejected");
        assert_eq!(err.number, 189);
        assert_eq!(err.severity, 15);
        assert_eq!(
            err.message,
            "The function test_round takes between 2 and 3 arguments."
        );
    }
}

#[test]
fn check_call_rejects_wrong_variadic_arity() {
    let def = def("TEST_CONCAT", Arity::Variadic(2));
    let err = check_call(&def, &types(1)).expect_err("arity must be rejected");
    assert_eq!(err.number, 174);
    assert_eq!(err.severity, 15);
    assert_eq!(
        err.message,
        "The function test_concat takes exactly 2 argument(s)."
    );
}

#[test]
fn check_call_accepts_valid_arity() {
    let variadic = FunctionDef {
        return_type: count_type,
        ..def("TEST_VARIADIC", Arity::Variadic(2))
    };
    for count in [2, 7, 50] {
        let info = check_call(&variadic, &types(count)).expect("arity must be accepted");
        // `count_type` saturates at 7: proof that `return_type` ran on the arguments.
        assert_eq!(
            info.ty,
            SqlType::Time(u8::try_from(count.min(7)).unwrap_or(7))
        );
    }

    let exact = def("TEST_EXACT", Arity::Exact(1));
    assert_eq!(
        check_call(&exact, &types(1))
            .expect("arity must be accepted")
            .ty,
        SqlType::Int
    );

    let range = def("TEST_RANGE", Arity::Range(2, 3));
    for count in [2, 3] {
        assert!(check_call(&range, &types(count)).is_ok());
    }
}

#[test]
fn check_call_propagates_the_return_type_error() {
    fn refuse(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
        Err(invalid_argument_type(&SqlType::Time(7), 1, "TEST_REFUSE"))
    }
    let def = FunctionDef {
        return_type: refuse,
        ..def("TEST_REFUSE", Arity::Exact(1))
    };
    let err = check_call(&def, &types(1)).expect_err("return_type must be consulted");
    assert_eq!(err.number, 8116);
}

#[test]
fn invalid_argument_type_is_8116() {
    let err = invalid_argument_type(&SqlType::Time(7), 1, "len");
    assert_eq!(err.number, 8116);
    assert_eq!(err.severity, 16);
    assert_eq!(
        err.message,
        "Data type time is not accepted for argument 1 of the len function."
    );
}

#[test]
fn invalid_argument_type_uses_error_name() {
    let decimal = SqlType::Decimal {
        precision: 5,
        scale: 2,
    };
    let err = invalid_argument_type(&decimal, 1, "abs");
    assert!(
        err.message.contains("Data type numeric is not accepted"),
        "{}",
        err.message
    );
    // The name comes from `SqlType::error_name`, which spells `decimal` as `numeric`.
    assert_eq!(
        err.message,
        invalid_argument_type(
            &SqlType::Numeric {
                precision: 5,
                scale: 2
            },
            1,
            "abs"
        )
        .message
    );
}

#[test]
fn invalid_argument_type_lower_cases_the_function() {
    let err = invalid_argument_type(&SqlType::Time(7), 1, "SUBSTRING");
    assert_eq!(
        err.message,
        "Data type time is not accepted for argument 1 of the substring function."
    );
}
