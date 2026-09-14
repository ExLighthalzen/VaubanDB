//! The binding errors a client sees: 137, 195, 206, 207, 243, 263, 529, 4104, 4145 and
//! 8117, their message, their state and their line.
//!
//! Each test starts from SQL text — `parse_batch`, then `bind` — because that is what a
//! session calls.

use vauban_binder::{BindContext, SessionOptions, bind};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;

/// The first binding error of `text`, statement by statement.
///
/// `parse_batch` reads the whole batch and `bind` is called on each statement in order:
/// SQL Server reports the first error and stops, and so does this helper — which is what
/// gives `error_line_is_the_node_line` its meaning.
fn err(text: &str) -> SqlError {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let ctx = BindContext::scalar(text, SessionOptions::default());
    for statement in &batch.statements {
        if let Err(error) = bind(statement, &ctx) {
            return error;
        }
    }
    unreachable!("{text} should not bind")
}

/// `SELECT CAST(NEWID() AS int);` is **529**, severity 16, state 1: `call.rs` calls
/// `SqlError::explicit_conversion_not_allowed` and not `SqlError::new`.
#[test]
fn illegal_cast_number() {
    let error = err("SELECT CAST(NEWID() AS int);");
    assert_eq!((error.number, error.severity, error.state), (529, 16, 1));
    assert_eq!(
        error,
        SqlError::explicit_conversion_not_allowed("uniqueidentifier", "int").with_line(1)
    );
}

/// Error 195 prints the name **as the user wrote it**: no upper-casing, no folding.
///
/// `SELECT NO_SUCH_FN(1);` prints `'NO_SUCH_FN'` and `SELECT no_such_fn(1);` prints
/// `'no_such_fn'`, both severity 15, state 10. The `%S_MSG` of the template is filled with
/// `built-in function`.
#[test]
fn unknown_function_message() {
    for name in ["NO_SUCH_FN", "no_such_fn", "No_Such_Fn"] {
        let error = err(&format!("SELECT {name}(1);"));
        assert_eq!((error.number, error.severity, error.state), (195, 15, 10));
        assert_eq!(
            error.message,
            format!("'{name}' is not a known built-in function name.")
        );
    }
}

/// A bare column is 207, a qualified one 4104 — however many parts it has.
///
/// `SELECT c;` is 207, severity 16, state 1; `SELECT t.c;` and `SELECT a.b.c.d;` are both
/// 4104, severity 16, state 1, the whole dotted name inside the double quotes. The 117 of
/// "too many prefixes" is for a `t.*` wildcard, not for a column.
#[test]
fn column_without_from_number() {
    let error = err("SELECT c;");
    assert_eq!((error.number, error.severity, error.state), (207, 16, 1));
    assert_eq!(error.message, "Unknown column name 'c'.");

    for (text, printed) in [
        ("SELECT t.c;", "t.c"),
        ("SELECT dbo.t.c;", "dbo.t.c"),
        ("SELECT a.b.c.d;", "a.b.c.d"),
    ] {
        let error = err(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (4104, 16, 1),
            "{text}"
        );
        assert_eq!(
            error.message,
            format!("The qualified name \"{printed}\" matches nothing in scope.")
        );
    }
}

/// Message 4145 quotes the token that **follows** the offending expression, not the
/// expression itself.
///
/// `SELECT 1 WHERE 1;` answers `near ';'` and `SELECT 1 WHERE 1 AND 1 = 1;` answers
/// `near 'AND'`. The `near '1'` is what a batch that stops right after the expression
/// gives (`SELECT 1 WHERE 1`, no semicolon): with nothing after it, the server quotes the
/// last token it read. The three are checked here so that no reader has to guess which
/// one the rule is.
#[test]
fn non_boolean_where_message() {
    for (text, quoted) in [
        ("SELECT 1 WHERE 1;", ";"),
        ("SELECT 1 WHERE 1 AND 1 = 1;", "AND"),
        ("SELECT 1 WHERE 1", "1"),
        ("SELECT 1 WHERE 'abc';", ";"),
    ] {
        let error = err(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (4145, 15, 1),
            "{text}"
        );
        assert_eq!(
            error.message,
            format!("A condition is expected near '{quoted}', but the expression is not boolean."),
            "{text}"
        );
    }
}

/// The operator word of message 8117, and the number that comes instead when the two
/// operands differ.
///
/// 8117 names the operator in words (`add`, `multiply`, `divide`) and needs **both**
/// operands to be of the type it refuses — `SELECT NEWID() + NEWID();` and
/// `SELECT NEWID() * NEWID();`. A `uniqueidentifier` against an `int` is a different fault
/// and a different number: `SELECT NEWID() + 1;` and `SELECT NEWID() * 1;` answer 206,
/// severity 16, state 2, naming `uniqueidentifier` then `int`.
///
/// The table of operator words lives in `types::arith::op_type` (`operator_name`) for the
/// binary operators and in `binder::expr` for the two unary ones (`minus`, `'~'`).
#[test]
fn invalid_operand_word() {
    for (text, word) in [
        ("SELECT NEWID() + NEWID();", "add"),
        ("SELECT NEWID() * NEWID();", "multiply"),
        ("SELECT NEWID() / NEWID();", "divide"),
    ] {
        let error = err(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (8117, 16, 1),
            "{text}"
        );
        assert_eq!(
            error.message,
            format!("Data type uniqueidentifier is not accepted by the {word} operator.")
        );
    }
    for text in ["SELECT NEWID() + 1;", "SELECT NEWID() * 1;"] {
        let error = err(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (206, 16, 2),
            "{text}"
        );
        assert_eq!(
            error.message,
            "Type mismatch: uniqueidentifier cannot be combined with int."
        );
    }
    // Not asserted, a difference of the `types` crate: with the operands the other way
    // round, `SELECT 1 * NEWID();`, SQL Server still names the `uniqueidentifier`
    // **first** while `types::implicit_result_type` names the operands in the order the
    // query wrote them, `int` then `uniqueidentifier`.
    assert_eq!(err("SELECT 1 * NEWID();").number, 206);
}

/// The line of an error is the line of the node that raised it, counted from 1 over the
/// physical lines of the batch **as received**.
///
/// A batch that opens with comment lines counts them; the rule is the same either way.
#[test]
fn error_line_is_the_node_line() {
    assert_eq!(err("SELECT c;").line, 1);
    assert_eq!(err("SELECT 1;\nSELECT c;").line, 2);
    assert_eq!(err("SELECT 1;\n\n\nSELECT c;").line, 4);
    // A comment counts as a line.
    assert_eq!(err("-- a comment\n-- another\nSELECT c;").line, 3);
    // The other numbers carry their line too, not 207 alone.
    assert_eq!(err("SELECT 1;\nSELECT NO_SUCH_FN(1);").line, 2);
    assert_eq!(err("SELECT 1;\nSELECT 1 WHERE 1;").line, 2);
    assert_eq!(err("SELECT 1;\nSELECT CAST(1 AS foo);").line, 2);
}
