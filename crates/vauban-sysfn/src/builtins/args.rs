//! Argument checking shared by every built-in function: how many, and of which types.
//!
//! Two answers: the wrong number of arguments raises 174 or 189 ([`check_call`]), an
//! unacceptable argument type raises 8116 (`invalid_argument_type`). Neither builds a
//! message: the wording belongs to the catalogue of `vauban-errors` and the name of a type
//! to [`vauban_types::SqlType::error_name`].

use vauban_errors::{SqlError, SqlResult};
use vauban_types::{SqlType, TypeInfo};

use crate::registry::{Arity, FunctionDef};

/// Types one call of `def` on arguments of types `args`, or says why it is invalid.
///
/// This is the entry point the `binder` uses to type a function call: it checks the arity
/// first, then delegates the result type to `def.return_type`, which is also where the
/// per-function type constraints live (error 8116, see `invalid_argument_type`). The
/// unknown-function case never reaches here: `lookup` returning `None` is the `binder`'s
/// error 195.
///
/// # Errors
///
/// - 174 (severity 15) when the function takes an exact number of arguments and did not
///   get it, and when a variadic function got fewer than its minimum;
/// - 189 (severity 15) when the function takes a range of arguments and did not get one
///   in it;
/// - whatever `def.return_type` raises, typically 8116, for an argument type the function
///   does not accept.
///
/// **Case of the name.** The message names the function in lower case regardless of the
/// case the user typed: `SELECT LEN();` and `SELECT len();` raise the same 174 naming `len`.
/// `check_call` therefore lower-cases `def.name`, which registrations spell in upper case
/// (`check_call_rejects_wrong_exact_arity`).
pub fn check_call(def: &FunctionDef, args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    if !def.arity.accepts(args.len()) {
        return Err(arity_error(def));
    }
    (def.return_type)(args)
}

/// The error a call of `def` with the wrong number of arguments deserves.
///
/// Exact arity gives 174, a range gives 189. SQL Server's own variadic built-ins are
/// bounded and report a range, `CONCAT` naming 254 as its maximum, so a function
/// registered as [`Arity::Variadic`] falls back to 174 with its minimum: there is no
/// maximum to print.
fn arity_error(def: &FunctionDef) -> SqlError {
    let name = def.name.to_ascii_lowercase();
    match def.arity {
        Arity::Exact(n) => SqlError::function_arg_count(&name, n),
        Arity::Range(min, max) => SqlError::function_arg_count_range(&name, min, max),
        Arity::Variadic(min) => SqlError::function_arg_count(&name, min),
    }
}

/// Error 8116: the argument at `position` (1-based) has a type `function` does not accept.
///
/// The `return_type` of a built-in raises it for the argument types it rejects, e.g.
/// `SELECT SUBSTRING(CAST('12:00' AS time), 1, 2);` raises 8116 for argument 1 of
/// `substring`. The name of the type is [`vauban_types::SqlType::error_name`] (`numeric`
/// for both `decimal` and `numeric`), not a table local to this crate, and the name of
/// the function is lower-cased for the same reason as in [`check_call`].
///
/// 8116 is not the sole answer to a wrong argument type: SQL Server reports an operand
/// type clash (206) when the argument is rejected by an implicit conversion rather than
/// by the function itself (`ABS` on a `time`). Each built-in decides which of the two it
/// raises.
pub(crate) fn invalid_argument_type(ty: &SqlType, position: u8, function: &str) -> SqlError {
    SqlError::invalid_argument_type(ty.error_name(), position, &function.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
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
}
