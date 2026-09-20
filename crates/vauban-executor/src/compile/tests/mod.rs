use vauban_binder::{BindContext, SessionOptions, bind};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{NoIndexes, PlanContext, plan};
use vauban_sysfn::{StaticContext, register_builtins};

use super::*;
use crate::statement::execute_collect;

/// Runs the compile-time checks of `text`, going through the whole chain: a hand-built
/// `BoundExpr` would say nothing about what a client's text folds to.
fn compiled(text: &str) -> SqlResult<()> {
    with_chain(text, compile)
}

/// Runs `text` in full, compile-time checks included, and answers how many rows it
/// produced.
fn executed(text: &str) -> SqlResult<usize> {
    with_chain(text, |stmt, ctx| {
        execute_collect(stmt, ctx).map(|(_, set)| set.rows.len())
    })
}

/// Parses, binds, plans and hands the single statement of `text` to `run`.
fn with_chain<T>(
    text: &str,
    run: impl FnOnce(&PhysicalStatement, &mut ExecContext<'_>) -> SqlResult<T>,
) -> SqlResult<T> {
    // The registry is global and idempotent, like in the test files of the crate.
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let bind_ctx = BindContext::scalar(text, SessionOptions::default());
    let bound = bind(&batch.statements[0], &bind_ctx).expect("the statement binds");
    let physical = plan(
        bound,
        &PlanContext {
            catalog: &NoIndexes,
        },
    )
    .expect("the statement plans");
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    run(&physical, &mut ctx)
}

/// A DDL statement and a `USE` have nothing to compile: `compile` answers `Ok(())`
/// for them in a context that has neither engine nor catalogue, so the checks of the
/// catalogue are not silently moved here (`ddl.rs` runs them at execution).
#[test]
fn ddl_and_use_compile_without_checks() {
    for text in [
        "CREATE TABLE dbo.t (a int);",
        "DROP TABLE dbo.t;",
        "USE master;",
    ] {
        compiled(text).expect("nothing to check at compile time");
    }
}

/// The number of the error `text` raises while it is compiled.
fn compile_error(text: &str) -> SqlError {
    compiled(text).expect_err("the statement does not compile")
}

#[test]
fn conditional_folding_follows_the_selected_branch() {
    for expression in [
        "CASE WHEN 1 = 1 THEN -1 ELSE @@SPID END",
        "CASE WHEN 1 = 0 THEN @@SPID ELSE -1 END",
        "CASE 1 WHEN 1 THEN -1 ELSE @@SPID END",
        "CASE 2 WHEN 1 THEN @@SPID ELSE -1 END",
        "CASE WHEN 1 = 1 THEN -1 WHEN @@SPID > 0 THEN 1 ELSE 1 END",
        "CASE WHEN NULL = 1 THEN @@SPID ELSE -1 END",
        "CASE WHEN 1 = 1 THEN -1 ELSE 1 / 0 END",
        "CASE WHEN 1 = 1 THEN -1 ELSE CAST('bad' AS int) END",
        "CASE WHEN 1 = 1 THEN -1 ELSE ISNULL(1, 0) END",
        "CASE WHEN 1 = 1 THEN CASE WHEN 1 = 0 THEN @@SPID ELSE -1 END ELSE @@SPID END",
        "CASE WHEN CASE WHEN 1 = 1 THEN 1 ELSE @@SPID END = 1 THEN -1 ELSE @@SPID END",
    ] {
        let length = format!("SELECT RIGHT('abc', {expression})");
        assert_eq!(compile_error(&length).number, 536, "{length}");
        let top = format!("SELECT TOP ({expression}) 1");
        assert_eq!(compile_error(&top).number, 127, "{top}");
    }
}

#[test]
fn unresolved_or_failing_conditions_do_not_fold() {
    for expression in [
        "CASE WHEN @@SPID = 0 THEN 1 ELSE -1 END",
        "CASE @@SPID WHEN 0 THEN 1 ELSE -1 END",
        "CASE WHEN 1 / 0 = 1 THEN -1 ELSE @@SPID END",
        "CASE WHEN 1 = 1 THEN @@SPID * 0 - 1 ELSE -1 END",
    ] {
        assert!(compiled(&format!("SELECT RIGHT('abc', {expression})")).is_ok());
        assert!(compiled(&format!("SELECT TOP ({expression}) 1")).is_ok());
    }
}

#[test]
fn coalesce_selects_its_first_known_non_null_argument() {
    for expression in [
        "COALESCE(-1, @@SPID)",
        "COALESCE(CAST(NULL AS int), -1, @@SPID)",
        "COALESCE(-1, 1 / 0)",
        "COALESCE(-1, ISNULL(1, 0))",
    ] {
        assert_eq!(
            compile_error(&format!("SELECT RIGHT('abc', {expression})")).number,
            536
        );
        assert_eq!(
            compile_error(&format!("SELECT TOP ({expression}) 1")).number,
            127
        );
    }
}

#[test]
fn selected_isnull_keeps_the_length_gate() {
    for expression in [
        "ISNULL(-1, @@SPID)",
        "ISNULL(CAST(NULL AS int), -1)",
        "ISNULL(-1, 1 / 0)",
        "CASE WHEN 1 = 1 THEN ISNULL(-1, 0) ELSE @@SPID END",
    ] {
        assert!(compiled(&format!("SELECT RIGHT('abc', {expression})")).is_ok());
        assert_eq!(
            compile_error(&format!("SELECT TOP ({expression}) 1")).number,
            127
        );
    }
}

#[test]
fn conditional_folds_keep_null_and_selected_errors() {
    let absent = "CASE WHEN 1 = 0 THEN @@SPID END";
    assert!(compiled(&format!("SELECT RIGHT('abc', {absent})")).is_ok());
    assert_eq!(
        compile_error(&format!("SELECT TOP ({absent}) 1")).number,
        1060
    );
    let failing = "CASE WHEN 1 = 1 THEN 1 / 0 ELSE @@SPID END";
    for text in [
        format!("SELECT RIGHT('abc', {failing})"),
        format!("SELECT TOP ({failing}) 1"),
    ] {
        assert!(compiled(&text).is_ok());
        assert_eq!(
            executed(&text)
                .expect_err("the selected expression raises")
                .number,
            8134
        );
    }
    assert_eq!(
        executed("SELECT CASE WHEN 1 = 1 THEN 1 ELSE 1 / 0 END"),
        Ok(1)
    );
}

#[test]
fn a_constant_length_is_checked_before_the_plan_runs() {
    // SQL Server sends no result set for these three.
    assert_eq!(compile_error("SELECT SUBSTRING('abc', 1, -1)").number, 536);
    assert_eq!(compile_error("SELECT LEFT('abc', -1)").number, 536);
    assert_eq!(compile_error("SELECT RIGHT('abc', -1)").number, 536);
    // The state follows the function, as `errors` builds it (6 for `left` and
    // `right`, 8 for `substring`), and the compile-time check keeps that.
    assert_eq!(compile_error("SELECT LEFT('abc', -1)").state, 6);
    assert_eq!(compile_error("SELECT SUBSTRING('abc', 1, -1)").state, 8);
}

#[test]
fn a_length_the_compiler_computes_is_not_only_a_literal() {
    // Each of these folds on SQL Server and answers 536 with no result set (module
    // header, second table).
    for text in [
        "SELECT SUBSTRING('abc', 1, 0 - 1)",
        "SELECT SUBSTRING('abc', 1, CAST(-1 AS int))",
        "SELECT SUBSTRING('abc', 1, ABS(-1) * -1)",
        "SELECT SUBSTRING('abc', 1, LEN('a') - 2)",
        "SELECT SUBSTRING('abc', 1, CASE WHEN 1 = 1 THEN -1 ELSE 1 END)",
        // Typed `int` although the value travels through a wider type, and folded on
        // SQL Server for that reason (module header, third table, rows 6 and 7).
        "SELECT SUBSTRING('abc', 1, CAST(-1 AS smallint) * 1)",
        "SELECT SUBSTRING('abc', 1, CAST(CAST(-1 AS bigint) AS int))",
        "SELECT SUBSTRING('abc', 1, COALESCE(-1, 0))",
    ] {
        assert_eq!(compile_error(text).number, 536, "{text}");
    }
}

#[test]
fn a_length_not_typed_int_is_left_to_execution() {
    // The gate that has no equivalent on the `TOP` side: SQL Server checks a length
    // argument at compile time when the argument is typed `int`, and sends its result
    // set for the other integral or numeric types — with 537, not 536 (module header,
    // third table). Without the gate each of these would compute -1 at compile time
    // and suppress a result set the server sends.
    for text in [
        "SELECT SUBSTRING('abc', 1, CAST(-1 AS bigint))",
        "SELECT SUBSTRING('abc', 1, CAST(-1 AS smallint))",
        "SELECT SUBSTRING('abc', 1, CAST(-1 AS decimal(10, 0)))",
        "SELECT SUBSTRING('abc', 1, -1.0)",
        "SELECT SUBSTRING('abc', 1, CAST(-1 AS int) + CAST(0 AS bigint))",
        "SELECT SUBSTRING('abc', 1, CONVERT(bigint, '-1'))",
        "SELECT LEFT('abc', CAST(-1 AS bigint))",
        "SELECT RIGHT('abc', CAST(-1 AS bigint))",
    ] {
        assert!(compiled(text).is_ok(), "{text}");
        // The check still exists, one phase later: what moved is the moment, not the
        // threshold. The number is 536 here and 537 there.
        assert!(executed(text).is_err(), "{text}");
    }
}

#[test]
fn a_length_that_calls_isnull_or_nullif_is_left_to_execution() {
    // `ISNULL` follows SQL Server — `SELECT SUBSTRING('abc', 1, ISNULL(-1, 0));` answers
    // 537 with a result set where `COALESCE(-1, 0)`, same type and same value, answers
    // 536 with no result set — and it is opaque at any depth, the root included. `NULLIF` is
    // there because SQL Server narrows its result type below `int` and `sysfn` does
    // not (module header).
    for text in [
        "SELECT SUBSTRING('abc', 1, ISNULL(-1, 0))",
        "SELECT SUBSTRING('abc', 1, ISNULL(-1, 0) * 1)",
        "SELECT SUBSTRING('abc', 1, 0 + ISNULL(-1, 0))",
        "SELECT SUBSTRING('abc', 1, CAST(ISNULL(-1, 0) AS int))",
        "SELECT SUBSTRING('abc', 1, ABS(ISNULL(-1, 0)) * -1)",
        "SELECT LEFT('abc', ISNULL(-1, 0))",
        "SELECT RIGHT('abc', ISNULL(-1, 0))",
        "SELECT SUBSTRING('abc', 1, NULLIF(-1, 0))",
        "SELECT SUBSTRING('abc', 1, -NULLIF(1, 0))",
    ] {
        assert!(compiled(text).is_ok(), "{text}");
        assert!(executed(text).is_err(), "{text}");
    }
}

#[test]
fn a_row_count_is_folded_whatever_its_type() {
    // The mirror of `a_length_not_typed_int_is_left_to_execution`, and what makes the
    // two gates two facts rather than a symmetry: the very same arguments are checked
    // at compile time under a `TOP`. `ISNULL` is transparent here too.
    for text in [
        "SELECT TOP (CAST(-1 AS bigint)) 1",
        "SELECT TOP (CAST(-1 AS smallint)) 1",
        "SELECT TOP (ISNULL(-1, 0)) 1",
        "SELECT TOP (NULLIF(-1, 0)) 1",
    ] {
        assert_eq!(compile_error(text).number, 127, "{text}");
    }
    // A row count that is not integral answers 1060 — with no result set on both
    // sides, `binder` refusing the clause before there is a plan to compile, which is
    // why these two are not `compile_error`.
    for text in [
        "SELECT TOP (CAST(-1 AS decimal(10, 0))) 1",
        "SELECT TOP (-1.0) 1",
    ] {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let bind_ctx = BindContext::scalar(text, SessionOptions::default());
        let err = bind(&batch.statements[0], &bind_ctx).expect_err("the clause is refused");
        assert_eq!(err.number, 1060, "{text}");
    }
}

#[test]
fn a_length_that_does_not_fold_is_left_to_execution() {
    // `@@SPID` is non-deterministic, so the compiler cannot know the length: SQL
    // Server sends the result set and raises 537 at run time. This test also pins
    // that compilation lets the expression through.
    let text = "SELECT SUBSTRING('abc', 1, @@SPID * 0 - 1)";
    assert!(compiled(text).is_ok());
    assert_eq!(
        executed(text).expect_err("the plan raises it").number,
        537,
        "the check still exists at run time"
    );
}

#[test]
fn a_null_length_is_not_a_negative_length() {
    // `SELECT SUBSTRING('abc', 1, NULL);` answers `NULL` and no error at all.
    assert!(compiled("SELECT SUBSTRING('abc', 1, NULL)").is_ok());
    assert_eq!(executed("SELECT SUBSTRING('abc', 1, NULL)"), Ok(1));
}

#[test]
fn a_constant_row_count_is_checked_before_the_plan_runs() {
    assert_eq!(compile_error("SELECT TOP (-1) 1").number, 127);
    assert_eq!(compile_error("SELECT TOP (0 - 1) 1").number, 127);
    assert_eq!(
        compile_error("SELECT TOP (CAST(NULL AS int)) 1").number,
        1060
    );
}

#[test]
fn a_row_count_that_does_not_fold_is_left_to_execution() {
    // The vector that separates the rule from a list of numbers: **the same 127**,
    // once before the metadata and once after.
    let text = "SELECT TOP (@@SPID * 0 - 1) 1";
    assert!(compiled(text).is_ok());
    assert_eq!(executed(text).expect_err("the plan raises it").number, 127);
}

#[test]
fn a_fold_that_raises_is_not_a_compilation_error() {
    // SQL Server drops the fold and lets the plan raise, result set included.
    for text in [
        "SELECT TOP (1 / 0) 1",
        "SELECT LEFT('abc', 1 / 0)",
        "SELECT SUBSTRING('abc', 1, CAST('a' AS int))",
    ] {
        assert!(compiled(text).is_ok(), "{text}");
        assert!(executed(text).is_err(), "{text}");
    }
    // The other argument failing to fold does not spare the length its check.
    assert_eq!(
        compile_error("SELECT LEFT(CAST('abc' AS int), -1)").number,
        536
    );
}

#[test]
fn the_check_fires_where_execution_would_never_reach() {
    // No row is ever computed, and the error comes out all the same.
    assert_eq!(
        compile_error("SELECT SUBSTRING('abc', 1, -1) WHERE 1 = 0").number,
        536
    );
    assert_eq!(
        compile_error("SELECT CASE WHEN 1 = 0 THEN SUBSTRING('abc', 1, -1) ELSE 'z' END").number,
        536
    );
    assert_eq!(
        compile_error("SELECT TOP (0) SUBSTRING('abc', 1, -1)").number,
        536
    );
}

#[test]
fn compilation_comes_before_execution() {
    // A run-time error written first in the select list does not get its turn.
    assert_eq!(
        compile_error("SELECT 1 / 0, SUBSTRING('abc', 1, -1)").number,
        536
    );
    // And between two compile-time checks, the select list answers before the `TOP`.
    assert_eq!(
        compile_error("SELECT TOP (-1) SUBSTRING('abc', 1, -1)").number,
        536
    );
}

#[test]
fn a_statement_without_a_constrained_argument_compiles() {
    for text in [
        "SELECT 1",
        "SELECT 1 / 0",
        "SELECT SUBSTRING('abc', -5, 3)",
        "SELECT TOP (1) LEFT('abc', 2)",
        "SELECT 1 WHERE 'a' LIKE 'a'",
    ] {
        assert!(compiled(text).is_ok(), "{text}");
    }
}

#[test]
fn the_registry_still_names_the_functions_this_module_checks() {
    // `LENGTH_ARGUMENT` matches on the name `sysfn` publishes: a renamed built-in, or
    // one whose length argument moves, must break here rather than stop being checked
    // in silence.
    register_builtins();
    for (name, index, spelling) in LENGTH_ARGUMENT {
        let def = vauban_sysfn::lookup(name).unwrap_or_else(|| panic!("{name} is registered"));
        assert!(
            def.arity.accepts(index + 1),
            "{name} has an argument {index}"
        );
        assert!(def.deterministic, "{name} folds when its arguments do");
        assert_eq!(spelling, name.to_ascii_lowercase(), "message spelling");
    }
    // Same reason for `LENGTH_OPAQUE`: a built-in that stops being registered under
    // that name would stop being opaque without a word, and the length check would go
    // back to suppressing result sets the server sends.
    for name in LENGTH_OPAQUE {
        let def = vauban_sysfn::lookup(name).unwrap_or_else(|| panic!("{name} is registered"));
        assert!(
            def.deterministic,
            "{name} is opaque to the fold although `folds` accepts it, which is the \
             whole point of the list"
        );
    }
}
