//! What a `SELECT` without `FROM` answers — schema, values, number of rows.
//!
//! Each vector starts from SQL text, parsed, bound and executed by [`run`]: this is the
//! whole chain, and the last link `session` needs before a client sees a row.
//!
//! `RowSet.rows.len()` is deliberately asserted on each vector: it is the number
//! `session` puts in the `DONE` token and in `@@ROWCOUNT`, and the executor's only say in
//! either.

use vauban_binder::{BindContext, SessionOptions, bind};
use vauban_errors::SqlError;
use vauban_executor::{ExecContext, ExecOutcome, RowSet, execute};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_sysfn::{StaticContext, register_builtins};
use vauban_types::{Len, SqlString, SqlType, Value};

// ---------------------------------------------------------------------------------------
// The chain: parse, bind, execute
// ---------------------------------------------------------------------------------------

/// The `RowSet` of the **first** statement of `text`.
///
/// A batch of several statements is several `RowSet`s, one per call of `execute`: the
/// executor knows nothing of a batch, `session` loops over it. Use [`run_batch`] to see
/// them all.
fn run(text: &str) -> RowSet {
    let mut sets = run_batch(text);
    assert!(!sets.is_empty(), "the batch has at least one statement");
    sets.remove(0)
}

/// The `RowSet` of **every** statement of `text`, in order.
fn run_batch(text: &str) -> Vec<RowSet> {
    outcomes(text).expect("the batch parses, binds and executes")
}

/// The error the first failing statement of `text` raises.
fn err(text: &str) -> SqlError {
    outcomes(text).expect_err("the batch does not execute")
}

/// Parses, binds and executes each statement of `text`, stopping at the first error.
fn outcomes(text: &str) -> Result<Vec<RowSet>, SqlError> {
    // The registry is global and idempotent; every test that reaches a function call
    // registers, like the tests of `binder` do.
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())?;
    let bind_ctx = BindContext::scalar(text, SessionOptions::default());
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let mut sets = Vec::with_capacity(batch.statements.len());
    for statement in &batch.statements {
        let bound = bind(statement, &bind_ctx)?;
        match execute(&bound, &mut ctx)? {
            ExecOutcome::Rows(set) => sets.push(set),
            // This file runs nothing but `SELECT`s, which always have a result set.
            ExecOutcome::NoRows => panic!("a `SELECT` produces a result set"),
        }
    }
    Ok(sets)
}

/// The `(name, type, nullable)` of every column of the result set of `text`.
fn cols(text: &str) -> Vec<(String, SqlType, bool)> {
    run(text)
        .schema
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.ty.ty, column.ty.nullable))
        .collect()
}

/// The `varchar(n)` of the expected schemas.
fn varchar(n: u16) -> SqlType {
    SqlType::VarChar(Len::Fixed(n))
}

/// A `varchar`/`nvarchar` value, the shape `types` stores a string in.
fn string(value: &str) -> SqlString {
    SqlString {
        text: value.to_owned(),
    }
}

/// The `nvarchar(n)` of the expected schemas.
fn nvarchar(n: u16) -> SqlType {
    SqlType::NVarChar(Len::Fixed(n))
}

// ---------------------------------------------------------------------------------------
// One row, one value
// ---------------------------------------------------------------------------------------

/// `SELECT 1`: one unnamed `int` column and one row holding 1.
#[test]
fn select_constant() {
    let set = run("SELECT 1");
    assert_eq!(cols("SELECT 1"), [(String::new(), SqlType::Int, false)]);
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.rows, [[Value::I32(1)]]);
}

/// Four columns, one row.
///
/// If one of the four ever diverges from SQL Server, the module at fault is `types` or
/// `sysfn`, not this test: the values below are what
/// `SELECT 1 + 1, LEN('abc'), CAST(1.5 AS int), ISNULL(NULL, 'x');` answers on SQL
/// Server. `CAST(1.5 AS int)` is 1 and not 2: converting `numeric` to an integer
/// **truncates** towards zero, it does not round.
#[test]
fn four_expressions_in_one_select() {
    let text = "SELECT 1 + 1, LEN('abc'), CAST(1.5 AS int), ISNULL(NULL, 'x')";
    let set = run(text);
    assert_eq!(set.schema.columns.len(), 4);
    assert_eq!(set.rows.len(), 1);
    assert_eq!(
        set.rows[0],
        [
            Value::I32(2),
            Value::I32(3),
            Value::I32(1),
            Value::String(string("x")),
        ]
    );
    // The schema the binder computed travels untouched: `execute` copies it, it does not
    // rebuild one from the values. The first three columns are nullable, as they are on
    // the wire (`INTNTYPE`, `fNullable = 1`).
    assert_eq!(
        cols(text),
        [
            (String::new(), SqlType::Int, true),
            (String::new(), SqlType::Int, true),
            (String::new(), SqlType::Int, true),
            (String::new(), varchar(1), false),
        ]
    );
}

/// Five literals: five columns `int`, `varchar`, `nvarchar`, `numeric`, `int`, and a row
/// whose last value is `NULL`.
#[test]
fn select_literals() {
    let text = "SELECT 1, 'a', N'é', 1.5, NULL";
    let set = run(text);
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.rows[0].len(), 5);
    assert_eq!(set.rows[0][0], Value::I32(1));
    assert_eq!(set.rows[0][1], Value::String(string("a")));
    assert_eq!(set.rows[0][2], Value::String(string("é")));
    assert_eq!(set.rows[0][4], Value::Null);
    assert_eq!(
        cols(text),
        [
            (String::new(), SqlType::Int, false),
            (String::new(), varchar(1), false),
            (String::new(), nvarchar(1), false),
            (
                String::new(),
                SqlType::Numeric {
                    precision: 2,
                    scale: 1,
                },
                false,
            ),
            // The untyped `NULL` literal is `int`, and it is the only nullable column.
            (String::new(), SqlType::Int, true),
        ]
    );
}

/// An alias names the column and changes nothing else.
#[test]
fn an_alias_names_the_column() {
    assert_eq!(
        cols("SELECT 1 AS n"),
        [("n".to_owned(), SqlType::Int, false)]
    );
    assert_eq!(run("SELECT 1 AS n").rows, [[Value::I32(1)]]);
}

// ---------------------------------------------------------------------------------------
// `WHERE`
// ---------------------------------------------------------------------------------------

/// A `WHERE` that is false answers the column metadata and **no row**, not "no result
/// set".
#[test]
fn where_false_gives_no_row() {
    let set = run("SELECT 1 WHERE 1 = 0");
    assert_eq!(set.schema.columns.len(), 1);
    assert_eq!(set.schema.columns[0].ty.ty, SqlType::Int);
    assert!(set.rows.is_empty());
}

#[test]
fn where_true_gives_one_row() {
    let set = run("SELECT 1 WHERE 1 = 1");
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.rows, [[Value::I32(1)]]);
}

/// The third truth value is not true, so the row goes.
///
/// This is what tells `WHERE` apart from a two-valued filter: `SET ANSI_NULLS OFF; SELECT
/// 1 WHERE NULL = NULL;` answers **one** row on SQL Server where `SET ANSI_NULLS ON;`
/// answers **none**. The option here is `SessionOptions::default()`, that is `ANSI_NULLS
/// ON`, so zero rows is the answer.
#[test]
fn where_unknown_gives_no_row() {
    let set = run("SELECT 1 WHERE NULL = NULL");
    assert_eq!(set.schema.columns.len(), 1);
    assert!(set.rows.is_empty());
}

/// `WHERE` runs **under** the select list, so a row it drops never evaluates it.
///
/// `SELECT 1 / 0 WHERE 1 = 0;` answers zero rows and no error on SQL Server: projecting
/// first would raise 8134 instead. The vector distinguishes the two orders, which is why
/// it is here and not only in the plan's rustdoc.
#[test]
fn where_false_skips_the_select_list() {
    let set = run("SELECT 1 / 0 WHERE 1 = 0");
    assert!(set.rows.is_empty());
    // The same select list under a `WHERE` that keeps the row does raise.
    assert_eq!(err("SELECT 1 / 0 WHERE 1 = 1").number, 8134);
}

// ---------------------------------------------------------------------------------------
// `TOP`
// ---------------------------------------------------------------------------------------

/// `TOP 0`, and the two counts around it.
#[test]
fn top_zero_gives_no_row() {
    let set = run("SELECT TOP 0 1");
    // Zero rows, but still one column: the client is sent the metadata of the result set.
    assert_eq!(set.schema.columns.len(), 1);
    assert!(set.rows.is_empty());
    assert_eq!(run("SELECT TOP 1 1").rows.len(), 1);
    // More rows asked for than there are: `TOP` is a maximum, not a promise.
    assert_eq!(run("SELECT TOP 5 1").rows.len(), 1);
}

/// `TOP 100 PERCENT`, `TOP 0 PERCENT` and `TOP (50) PERCENT`.
///
/// The last one is the one that proves the rounding: over a single row,
/// `ceil(1 × 50 / 100)` is **1** and truncation would be 0. SQL Server answers one row,
/// so `PERCENT` rounds up.
#[test]
fn top_percent() {
    assert_eq!(run("SELECT TOP 100 PERCENT 1").rows.len(), 1);
    assert!(run("SELECT TOP 0 PERCENT 1").rows.is_empty());
    assert_eq!(run("SELECT TOP (50) PERCENT 1").rows.len(), 1);
    // Any share above zero keeps the row, however small.
    assert_eq!(run("SELECT TOP (0.0001) PERCENT 1").rows.len(), 1);
    // A percentage of zero still sends the column metadata.
    assert_eq!(run("SELECT TOP 0 PERCENT 1").schema.columns.len(), 1);
}

/// A `TOP` of zero rows does not read its input at all.
///
/// `SELECT TOP (0) 1 / 0;` and `SELECT TOP (0) 1 WHERE 1 / 0 = 1;` answer zero rows and no
/// error on SQL Server; truncating an already computed row would raise 8134. The same
/// select list under `TOP (1)` does raise, which is what makes the pair a vector.
#[test]
fn top_zero_skips_the_input() {
    assert!(run("SELECT TOP 0 1 / 0").rows.is_empty());
    assert!(run("SELECT TOP 0 1 WHERE 1 / 0 = 1").rows.is_empty());
    assert!(run("SELECT TOP 0 PERCENT 1 / 0").rows.is_empty());
    assert_eq!(err("SELECT TOP 1 1 / 0").number, 8134);
}

/// The row count is evaluated before anything is read, so its own errors win.
///
/// `SELECT TOP (-1) 1;` answers 127, and it still answers 127 — not 8134 — when the
/// select list or the `WHERE` divides by zero. A `NULL` reaching the executor behind a
/// `CAST` answers 1060; the bare `NULL` literal never gets here, `bind_top` refuses it.
#[test]
fn a_bad_row_count_is_raised_before_the_input() {
    assert_eq!(err("SELECT TOP (-1) 1").number, 127);
    assert_eq!(err("SELECT TOP (-1) 1 / 0").number, 127);
    assert_eq!(err("SELECT TOP (-1) 1 WHERE 1 / 0 = 1").number, 127);
    assert_eq!(err("SELECT TOP (CAST(NULL AS int)) 1").number, 1060);
    // Bound, not executed: the node *is* the constant, so the binder sees it.
    assert_eq!(err("SELECT TOP (NULL) 1").number, 1060);
    // An error inside the row count itself is raised as it is.
    assert_eq!(err("SELECT TOP (1 / 0) 1").number, 8134);
}

/// A percentage outside `0..=100` and a `NULL` percentage are 1031 and 1014 on SQL Server
/// — `plan::check_budget` raises neither yet, so both are the internal 50000 here.
///
/// The statements are in the rustdoc of `plan::execute_limit`. The assertion is on 50000
/// on purpose: it fails the day the two numbers are raised, which is when this test must
/// be rewritten to the real number rather than left agreeing by accident.
#[test]
fn an_out_of_range_percentage_is_still_an_internal_error() {
    assert_eq!(err("SELECT TOP (150) PERCENT 1").number, 50000);
    assert_eq!(err("SELECT TOP (100.5) PERCENT 1").number, 50000);
    assert_eq!(err("SELECT TOP (-1) PERCENT 1").number, 50000);
    assert_eq!(err("SELECT TOP (NULL) PERCENT 1").number, 50000);
    assert_eq!(
        err("SELECT TOP (CAST(NULL AS float)) PERCENT 1").number,
        50000
    );
}

/// `TOP` and `WHERE` in the same statement: the filter runs first, the truncation second.
#[test]
fn top_over_a_filtered_row() {
    assert!(run("SELECT TOP 1 1 WHERE 1 = 0").rows.is_empty());
    assert_eq!(run("SELECT TOP 1 1 WHERE 1 = 1").rows.len(), 1);
    assert!(run("SELECT TOP 0 1 WHERE 1 = 1").rows.is_empty());
}

// ---------------------------------------------------------------------------------------
// `DISTINCT`, and a batch of several statements
// ---------------------------------------------------------------------------------------

/// Over one row `DISTINCT` removes nothing, and the binder accepts and drops it. A real
/// operator with a real input under it is not written.
#[test]
fn distinct_is_transparent() {
    let set = run("SELECT DISTINCT 1");
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.rows, [[Value::I32(1)]]);
    assert!(run("SELECT DISTINCT TOP 0 1").rows.is_empty());
}

/// `SELECT 1; SELECT 2;` is **two** statements, so two calls of `execute` and two
/// `RowSet`s. The executor never loops over a batch — `session` does.
#[test]
fn two_statements_are_two_row_sets() {
    let sets = run_batch("SELECT 1; SELECT 2;");
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].rows, [[Value::I32(1)]]);
    assert_eq!(sets[1].rows, [[Value::I32(2)]]);
    assert_eq!(sets[0].schema.columns.len(), 1);
    assert_eq!(sets[1].schema.columns.len(), 1);
}

// ---------------------------------------------------------------------------------------
// The number of rows, which `session` posts as `@@ROWCOUNT`
// ---------------------------------------------------------------------------------------

/// `RowSet.rows.len()` for each vector of this file, against the number of rows it
/// produces.
///
/// The executor does not touch `@@ROWCOUNT`: it counts, and `session` posts the count. The
/// table below is therefore the whole of the executor's contribution to `@@ROWCOUNT`.
#[test]
fn row_count_is_the_length() {
    for (text, rows) in [
        ("SELECT 1", 1),
        (
            "SELECT 1 + 1, LEN('abc'), CAST(1.5 AS int), ISNULL(NULL, 'x')",
            1,
        ),
        ("SELECT 1, 'a', N'é', 1.5, NULL", 1),
        ("SELECT 1 WHERE 1 = 0", 0),
        ("SELECT 1 WHERE 1 = 1", 1),
        ("SELECT 1 WHERE NULL = NULL", 0),
        ("SELECT TOP 0 1", 0),
        ("SELECT TOP 1 1", 1),
        ("SELECT TOP 5 1", 1),
        ("SELECT TOP 100 PERCENT 1", 1),
        ("SELECT TOP (50) PERCENT 1", 1),
        ("SELECT TOP 0 PERCENT 1", 0),
        ("SELECT DISTINCT 1", 1),
    ] {
        assert_eq!(run(text).rows.len(), rows, "{text}");
    }
}

/// Every row is exactly as wide as the schema that describes it, on every vector above:
/// `session` reads the two side by side to write `COLMETADATA` and `ROW`.
#[test]
fn every_row_matches_its_schema() {
    for text in [
        "SELECT 1",
        "SELECT 1 + 1, LEN('abc'), CAST(1.5 AS int), ISNULL(NULL, 'x')",
        "SELECT 1, 'a', N'é', 1.5, NULL",
        "SELECT 1 WHERE 1 = 1",
        "SELECT TOP 1 1",
        "SELECT DISTINCT 1",
    ] {
        let set = run(text);
        for row in &set.rows {
            assert_eq!(row.len(), set.schema.columns.len(), "{text}");
        }
    }
}
