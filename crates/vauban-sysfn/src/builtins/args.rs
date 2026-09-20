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
mod tests;
