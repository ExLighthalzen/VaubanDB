//! Integration tests of `Session::run_batch` on the real chain:
//! `parser::parse_batch` -> `binder::bind` -> `executor::execute`, seen through a
//! `ResultSink` that keeps every call.
//!
//! No socket and no TDS here: `tests/batch.rs` is what drives the same path through the
//! wire. What this file fixes is the sequence of `columns` / `row` / `done` / `error` calls
//! one batch produces, which is what a client sees as result sets and errors.
//!
//! `vauban_sysfn::register_builtins()` is called by every test that needs a function:
//! the registry is a process-wide table, filled by the binary at start-up, and the call is
//! idempotent. `vauban_compat::register_functions()` is **not** callable from here
//! (`compat` depends on `session`, so it cannot be a dev-dependency of it), which is why
//! nothing below asks for `@@VERSION` or `SERVERPROPERTY`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange, ResetConnection, Rpc, RpcProc};
use vauban_types::{SqlString, SqlType, TypeInfo, Value};

/// SPID of every session built here.
const SPID: i16 = 51;

// ---------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------

/// Everything a sink receives, in order.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns(Vec<ColumnMeta>),
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    Info(InfoMessage),
    Error(SqlError),
    EnvChange(EnvChange),
    ReturnValue(String, TypeInfo, Value),
    ReturnStatus(i32),
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.0.push(Event::Columns(cols.to_vec()));
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.0.push(Event::Row(row.to_vec()));
        Ok(())
    }
    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.0.push(Event::Done(rowcount, more));
        Ok(())
    }
    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
        self.0.push(Event::Info(msg.clone()));
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.clone()));
        Ok(())
    }
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.0.push(Event::EnvChange(change.clone()));
        Ok(())
    }
    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.0.push(Event::ReturnValue(
            name.to_owned(),
            ty.clone(),
            value.clone(),
        ));
        Ok(())
    }
    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.0.push(Event::ReturnStatus(status));
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------

/// A session on an in-memory engine, with the built-in functions registered.
fn session() -> Session {
    vauban_sysfn::register_builtins();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(SPID),
    )
}

/// Runs `text` as one batch on a fresh session and returns what the sink received.
fn run(text: &str) -> Vec<Event> {
    let (events, result) = run_on(&mut session(), text);
    result.expect("a batch reports its errors through the sink, not through its result");
    events
}

/// Runs `text` on an existing session, keeping its state.
fn run_on(session: &mut Session, text: &str) -> (Vec<Event>, SqlResult<()>) {
    let mut sink = Recording::default();
    let result = session.run_batch(text, &mut sink);
    (sink.0, result)
}

/// The single error of `events`; panics when there is none or more than one.
fn only_error(events: &[Event]) -> SqlError {
    let errors: Vec<&SqlError> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some(err),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "exactly one error expected: {events:#?}");
    errors[0].clone()
}

/// The values of the rows of `events`, in order, whatever the result set.
fn rows(events: &[Event]) -> Vec<Vec<Value>> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => Some(row.clone()),
            _ => None,
        })
        .collect()
}

/// The column metadata of the `n`-th result set (0-based).
fn columns(events: &[Event], n: usize) -> Vec<ColumnMeta> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Columns(cols) => Some(cols.clone()),
            _ => None,
        })
        .nth(n)
        .unwrap_or_else(|| panic!("no result set number {n}: {events:#?}"))
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

// ---------------------------------------------------------------------------------------
// The chain: one statement, one result set, one DONE
// ---------------------------------------------------------------------------------------

#[test]
fn select_one() {
    let events = run("SELECT 1");
    assert_eq!(events.len(), 3, "{events:#?}");
    let cols = columns(&events, 0);
    assert_eq!(cols.len(), 1);
    assert_eq!(
        cols[0].name, "",
        "an expression without an alias is unnamed"
    );
    assert_eq!(cols[0].ty.ty, SqlType::Int);
    assert!(!cols[0].ty.nullable, "an int literal is NOT NULL");
    assert!(!cols[0].flags.nullable);
    assert_eq!(events[1], Event::Row(vec![Value::I32(1)]));
    assert_eq!(events[2], Event::Done(Some(1), false));
}

#[test]
fn scalar_functions_and_casts_in_one_select() {
    // Four expressions of four kinds in one select list.
    let events = run("SELECT 1 + 1, LEN('abc'), CAST(1.5 AS int), ISNULL(NULL, 'x')");
    let cols = columns(&events, 0);
    assert_eq!(cols.len(), 4, "{cols:#?}");
    assert!(
        cols.iter().all(|col| col.name.is_empty()),
        "four unnamed columns: {cols:#?}"
    );
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(2), Value::I32(3), Value::I32(1), text("x"),]]
    );
    assert_eq!(events.last(), Some(&Event::Done(Some(1), false)));
}

#[test]
fn two_statements_two_result_sets() {
    let events = run("SELECT 1; SELECT 2");
    assert_eq!(events.len(), 6, "{events:#?}");
    assert_eq!(events[1], Event::Row(vec![Value::I32(1)]));
    assert_eq!(
        events[2],
        Event::Done(Some(1), true),
        "MORE on all but the last"
    );
    assert_eq!(events[4], Event::Row(vec![Value::I32(2)]));
    assert_eq!(events[5], Event::Done(Some(1), false));
}

#[test]
fn select_no_rows() {
    // Metadata and a DONE with a count of 0: "no row", not "no result set".
    let events = run("SELECT 1 WHERE 1 = 0");
    assert_eq!(events.len(), 2, "{events:#?}");
    assert_eq!(columns(&events, 0).len(), 1);
    assert!(rows(&events).is_empty());
    assert_eq!(events[1], Event::Done(Some(0), false));
}

#[test]
fn rowcount_is_updated() {
    // `@@ROWCOUNT` reports the previous statement, not the batch.
    let events = run("SELECT 1; SELECT @@ROWCOUNT");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(1)]]
    );
    // Then the `SELECT @@ROWCOUNT` posts its own count.
    let mut session = session();
    let _ = run_on(&mut session, "SELECT 1; SELECT 2 WHERE 1 = 0");
    assert_eq!(session.state().rowcount, 0);
    let (events, _) = run_on(&mut session, "SELECT @@ROWCOUNT");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(0)]],
        "the count survives from one batch to the next"
    );
}

#[test]
fn spid_comes_from_the_session() {
    let events = run("SELECT @@SPID");
    assert_eq!(columns(&events, 0)[0].ty.ty, SqlType::SmallInt);
    assert_eq!(rows(&events), vec![vec![Value::I16(SPID)]]);
    // T-SQL names are case-insensitive, and so is the registry of `sysfn`: the driver of a
    // client sends what its author typed.
    assert_eq!(rows(&run("select @@spid")), vec![vec![Value::I16(SPID)]]);
    assert_eq!(rows(&run("select len('abc')")), vec![vec![Value::I32(3)]]);
}

// ---------------------------------------------------------------------------------------
// Compilation errors: the batch never starts
// ---------------------------------------------------------------------------------------

#[test]
fn syntax_error_stops_the_batch() {
    let events = run("SELECT 1; SELEC 1;");
    let err = only_error(&events);
    assert_eq!(err.number, 102);
    assert_eq!(
        err.message,
        SqlError::incorrect_syntax_near("SELEC", 1).message
    );
    assert_eq!(err.line, 1);
    assert!(
        !events.iter().any(|e| matches!(e, Event::Columns(_))),
        "not one statement of the batch ran: {events:#?}"
    );
    assert_eq!(events.len(), 2, "one ERROR and one DONE: {events:#?}");
    assert_eq!(events[1], Event::Done(None, false));
}

#[test]
fn syntax_error_line() {
    let events = run("SELECT 1;\nSELECT 2;\nSELECT 3;\nSELEC 4;");
    assert_eq!(only_error(&events).line, 4);
}

#[test]
fn a_failed_batch_leaves_the_session_usable() {
    let mut session = session();
    // `SELECT 1;` first: a batch whose **first** word is a bare name is an implicit
    // `EXECUTE`, not a syntax error, and answers something else entirely.
    let (events, result) = run_on(&mut session, "SELECT 1; SELEC 1");
    assert!(result.is_ok());
    assert_eq!(only_error(&events).number, 102);
    // `@@ERROR` keeps the number of the last error (the session does not reset it).
    assert_eq!(session.state().last_error, 102);
    let (events, _) = run_on(&mut session, "SELECT 1");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]]);
}

// ---------------------------------------------------------------------------------------
// Binding and run-time errors
// ---------------------------------------------------------------------------------------

#[test]
fn bind_error_reaches_the_client() {
    let events = run("SELECT NO_SUCH_FN(1)");
    let err = only_error(&events);
    assert_eq!(err.number, 195, "{err:?}");
    assert_eq!(
        err.message,
        SqlError::not_a_recognized_name("NO_SUCH_FN", "built-in function").message
    );
    assert_eq!(events.last(), Some(&Event::Done(None, false)));

    let events = run("SELECT 1 WHERE 1");
    assert_eq!(only_error(&events).number, 4145);
}

#[test]
fn a_statement_scoped_runtime_error_lets_the_batch_continue() {
    // 8134 stops this statement, but not the batch, as on SQL Server.
    let events = run("SELECT 1; SELECT 1 / 0; SELECT 2");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "{events:#?}"
    );
    assert_eq!(only_error(&events).number, 8134);
    assert_eq!(events.last(), Some(&Event::Done(Some(1), false)));
    let error = events
        .iter()
        .position(|event| matches!(event, Event::Error(_)))
        .expect("the statement raises its error");
    assert_eq!(
        events.get(error + 1),
        Some(&Event::Done(None, true)),
        "ERROR is closed by a DONE carrying MORE: {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Columns(_)))
            .count(),
        3,
        "the failing statement sends empty metadata and the third one runs: {events:#?}"
    );
}

#[test]
fn a_batch_scoped_runtime_error_stops_the_batch() {
    // Same severity and state as 8134, opposite scope.
    let events = run("SELECT 1; SELECT CAST('abc' AS int); SELECT 2");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
    assert_eq!(only_error(&events).number, 245);
    assert_eq!(events.last(), Some(&Event::Done(None, false)));
}

#[test]
fn xact_abort_stops_a_statement_scoped_runtime_error() {
    let events = run("SET XACT_ABORT ON; SELECT 1; SELECT 1 / 0; SELECT 2");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
    assert_eq!(only_error(&events).number, 8134);
    assert_eq!(events.last(), Some(&Event::Done(None, false)));
}

#[test]
fn datepart_9810_is_statement_scoped_unless_xact_abort_is_on() {
    let batch = "SELECT 1; SELECT DATEADD(hour, 1, CAST('2020-01-01' AS date)); SELECT 2";
    let events = run(batch);
    assert_eq!(only_error(&events).number, 9810);
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "{events:#?}"
    );

    let events = run(&format!("SET XACT_ABORT ON; {batch}"));
    assert_eq!(only_error(&events).number, 9810);
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
}

// ---------------------------------------------------------------------------------------
// The line of a run-time error
// ---------------------------------------------------------------------------------------

/// The line of the single error of `batch`, run on a fresh session.
fn error_line(batch: &str) -> u32 {
    only_error(&run(batch)).line
}

#[test]
fn a_run_time_error_carries_the_line_its_statement_starts_on() {
    // Six vectors (`batch.rs`, module header). Each pair is (batch, line of the `SELECT`
    // that fails); in all but the last, the failing node sits on a **different** line, so
    // the assertion tells the two rules apart.
    for (batch, line) in [
        ("SELECT 1;\nSELECT CAST(\n1 / 0\nAS int);", 2),
        ("SELECT\nCASE WHEN 1 = 1\nTHEN 1 / 0\nELSE 0 END;", 1),
        ("SELECT\n(1 / 0)\n+\n(2 / 0);", 1),
        ("SELECT 1;\nSELECT\n1 + (1 / 0);", 2),
        ("SELECT\n1 +\n(1 / 0)\n+ 2;", 1),
        // Witness of one line: the two rules agree, which is why it proves nothing alone.
        ("SELECT 1 / 0;", 1),
    ] {
        assert_eq!(error_line(batch), line, "batch {batch:?}");
    }
}

#[test]
fn a_statement_that_starts_on_the_line_the_previous_one_ends_on() {
    // The case where "the line of the statement" and "the line of the batch" separate the
    // least. The failing statement starts on line 1 and its division is on line 2.
    assert_eq!(error_line("SELECT 1; SELECT\n1 / 0;"), 1);
    // The same shape two families further: 245 and 536 answer it identically.
    assert_eq!(error_line("SELECT 1; SELECT\nCAST('abc' AS int);"), 1);
    assert_eq!(error_line("SELECT 1; SELECT LEFT(\n'abc',\n-1);"), 1);
}

#[test]
fn the_line_rule_holds_across_error_families() {
    // A rule established on division by zero alone would not hold. Five families, each on a
    // statement whose `SELECT` is on line 1 and whose failing node is three or more lines
    // below; every one answers 1 on SQL Server.
    for (batch, number) in [
        ("SELECT\n1\n%\n0;", 8134),
        ("SELECT\nCAST(\n'abc'\nAS int);", 245),
        ("SELECT\nCAST(\n300\nAS tinyint);", 220),
        ("SELECT\nCAST(\n3000000000\nAS int);", 8115),
        ("SELECT\nSUBSTRING(\n'abc',\n1,\n-1);", 536),
    ] {
        let err = only_error(&run(batch));
        assert_eq!(err.number, number, "batch {batch:?}");
        assert_eq!(err.line, 1, "batch {batch:?}: {err:?}");
    }
}

#[test]
fn a_statement_over_more_than_three_lines_still_answers_its_first_line() {
    // Seven lines, the failing node five lines below the answer.
    assert_eq!(error_line("SELECT\n1\n+\n2\n+\n(1 /\n0);"), 1);
    // And the line is the **failing** statement's, not the batch's: two statements of two
    // lines come first, so the third starts on line 5.
    assert_eq!(error_line("SELECT\n1;\nSELECT\n2;\nSELECT\n1 / 0;"), 5);
}

#[test]
fn the_statement_starts_where_its_first_token_is() {
    // Leading trivia is not part of the statement: a comment line, a blank line and an
    // inline comment all leave the answer on the line of the `SELECT` keyword, as on SQL
    // Server.
    assert_eq!(error_line("SELECT 1;\n-- a comment\nSELECT\n1 / 0;"), 3);
    assert_eq!(error_line("SELECT 1;\n\nSELECT\n1 / 0;"), 3);
    assert_eq!(error_line("SELECT 1; /* c */ SELECT\n1 / 0;"), 1);
}

#[test]
fn the_two_top_row_count_errors_keep_the_line_of_their_clause() {
    // 127 and 1060 are raised while SQL Server **compiles** the statement, and a
    // compilation error names the offending clause. As on SQL Server:
    // `SELECT` / `TOP (-1)` / `1;` answers the line of the row count, one below the
    // `SELECT`, and `SELECT TOP` / `(-1)` / `1;` answers the row count's line too — so it is
    // the expression's line, not the `TOP` keyword's. `executor` already puts it there and
    // `batch.rs` leaves it alone (this is the vector that says the override is not blind).
    let err = only_error(&run("SELECT\nTOP (-1)\n1;"));
    assert_eq!(err.number, 127, "{err:?}");
    assert_eq!(err.line, 2, "{err:?}");
    let err = only_error(&run("SELECT\nTOP (NULL)\n1;"));
    assert_eq!(err.number, 1060, "{err:?}");
    assert_eq!(err.line, 2, "{err:?}");
    let err = only_error(&run("SELECT TOP\n(-1)\n1;"));
    assert_eq!(err.number, 127, "{err:?}");
    assert_eq!(err.line, 2, "{err:?}");
    // A **run-time** error inside the same clause is not one of those two and does follow
    // the statement: `SELECT` / `TOP (1 /` / `0)` / `1;` answers 1 on SQL Server.
    let err = only_error(&run("SELECT\nTOP (1 /\n0)\n1;"));
    assert_eq!(err.number, 8134, "{err:?}");
    assert_eq!(err.line, 1, "{err:?}");
}

#[test]
fn an_internal_error_raised_at_run_time_still_carries_no_line() {
    // 50000 names a hole in this engine, not a place in the client's query, so it gets no
    // line — neither from `executor` nor from the override above. `TOP (150) PERCENT` is a
    // run-time internal error (error 1031 is not raised by the executor); SQL Server
    // answers 1031 there, which is the deviation, not this line.
    let err = only_error(&run("SELECT\nTOP (150) PERCENT\n1;"));
    assert_eq!(err.number, 50000, "{err:?}");
    assert_eq!(err.line, 0, "{err:?}");
}

#[test]
fn a_bind_error_sends_no_metadata_at_all() {
    // The local half of the frontier: a binding error sends no metadata for its statement.
    // `a_compilation_error_silences_the_whole_batch` below covers the surrounding statements.
    for batch in ["SELECT NO_SUCH_FN(1)", "SELECT \"a\"", "SELECT 1 WHERE 1"] {
        let events = run(batch);
        assert!(
            !events.iter().any(|e| matches!(e, Event::Columns(_))),
            "batch {batch:?}: {events:#?}"
        );
    }
}

#[test]
fn a_compilation_error_silences_the_whole_batch() {
    for (batch, number) in [
        ("SELECT 1; SELECT NO_SUCH_FN(1); SELECT 2", 195),
        ("SELECT 1; SELECT \"a\"; SELECT 2", 207),
        ("SELECT 1; SELECT 1 WHERE 1; SELECT 2", 4145),
        ("SELECT 1; SELECT TOP (-1) 1; SELECT 2", 127),
    ] {
        let events = run(batch);
        assert_eq!(only_error(&events).number, number, "{events:#?}");
        assert!(rows(&events).is_empty(), "batch {batch:?}: {events:#?}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Columns(_))),
            "batch {batch:?}: {events:#?}"
        );
    }
}

#[test]
fn an_unsupported_statement_keeps_its_precise_binding_error() {
    let events = run("SELECT 1; DECLARE @t TABLE (a int); SELECT 2");
    let error = only_error(&events);
    assert_eq!(error.number, 50000, "{error:?}");
    // The binder refuses a table variable with an error that names the statement; the
    // number and the silence of the batch are what this test guards.
    assert!(error.message.contains("DECLARE"), "{error:?}");
    assert!(rows(&events).is_empty(), "{events:#?}");
}

// ---------------------------------------------------------------------------------------
// `SET` statements
// ---------------------------------------------------------------------------------------

#[test]
fn set_statements_still_work() {
    let mut session = session();
    assert!(!session.state().options.nocount);
    let (events, result) = run_on(&mut session, "SET NOCOUNT ON");
    assert!(result.is_ok());
    assert!(session.state().options.nocount, "the state changed");
    assert_eq!(events, vec![Event::Done(None, false)], "no result set");

    let events = run("SET ANSI_NULLS ON; SELECT 1");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Done(..)))
            .count(),
        2,
        "one DONE per statement: {events:#?}"
    );
    assert_eq!(events[0], Event::Done(None, true));
}

#[test]
fn a_set_puts_the_row_count_back_to_zero() {
    // As on SQL Server:
    // `SELECT 1; SET NOCOUNT OFF; SELECT @@ROWCOUNT;` answers 1 then **0**, while
    // `SELECT 1; SELECT @@ROWCOUNT;` answers 1 then 1. The second batch is the vector that
    // tells the reset apart from a count that simply never moved.
    let events = run("SELECT 1; SET NOCOUNT OFF; SELECT @@ROWCOUNT");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(0)]],
        "{events:#?}"
    );
    assert_eq!(
        rows(&run("SELECT 1; SELECT @@ROWCOUNT")),
        vec![vec![Value::I32(1)], vec![Value::I32(1)]]
    );
}

#[test]
fn set_transaction_isolation_level_still_reaches_the_state() {
    // A three-word option name: the AST spells it back through the span of the statement,
    // not through a rendering that could lose a word.
    let mut session = session();
    let (events, result) = run_on(&mut session, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    assert!(result.is_ok());
    assert_eq!(events, vec![Event::Done(None, false)]);
    assert_eq!(
        format!("{:?}", session.state().isolation),
        "Serializable",
        "the isolation level of the session changed"
    );
}

#[test]
fn quoted_identifier_reaches_the_parser() {
    // ON (the default of every driver): `"a"` names a column, and no table is in scope.
    let events = run("SELECT \"a\"");
    assert_eq!(only_error(&events).number, 207);

    // OFF: the same text is a character string literal.
    vauban_sysfn::register_builtins();
    let mut state = SessionState::new(SPID);
    state.options.quoted_identifier = false;
    let mut session = Session::new(Arc::new(Engine::new(Arc::new(MemoryStorage::new()))), state);
    let (events, result) = run_on(&mut session, "SELECT \"a\"");
    assert!(result.is_ok());
    assert_eq!(rows(&events), vec![vec![text("a")]], "{events:#?}");
}

#[test]
fn quoted_identifier_takes_effect_in_its_own_batch_both_ways() {
    // As on SQL Server: the option changes parsing from the following statement, first
    // OFF (`"a"` is a string) and then ON again.
    let mut session = session();
    let (events, result) = run_on(
        &mut session,
        "SET QUOTED_IDENTIFIER OFF; SELECT \"a\"; SET QUOTED_IDENTIFIER ON; SELECT 'b'",
    );
    assert!(result.is_ok());
    assert_eq!(
        rows(&events),
        vec![vec![text("a")], vec![text("b")]],
        "{events:#?}"
    );
    assert!(session.state().options.quoted_identifier);
}

#[test]
fn a_set_in_a_batch_that_does_not_compile_never_reaches_the_session() {
    let mut session = session();
    let (events, result) = run_on(
        &mut session,
        "SET QUOTED_IDENTIFIER OFF; SELECT NO_SUCH_FN(1)",
    );
    assert!(result.is_ok());
    assert_eq!(only_error(&events).number, 195, "{events:#?}");
    assert!(session.state().options.quoted_identifier);

    let mut state = SessionState::new(SPID);
    state.options.quoted_identifier = false;
    let mut session = Session::new(Arc::new(Engine::new(Arc::new(MemoryStorage::new()))), state);
    let (events, result) = run_on(&mut session, "SET QUOTED_IDENTIFIER ON; SELECT \"a\"");
    assert!(result.is_ok());
    assert_eq!(only_error(&events).number, 207, "{events:#?}");
    assert!(
        !session.state().options.quoted_identifier,
        "the SET was used for compilation but the failed batch was never executed"
    );
}

// ---------------------------------------------------------------------------------------
// The empty batch
// ---------------------------------------------------------------------------------------

#[test]
fn empty_batch() {
    for batch in ["", "   ", "-- rien", "/* rien */", ";;", "\n\n"] {
        assert_eq!(
            run(batch),
            vec![Event::Done(None, false)],
            "batch {batch:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// The fall-back on the fake engine: `WAITFOR DELAY` and nothing else
// ---------------------------------------------------------------------------------------

#[test]
fn waitfor_delay_still_goes_through_the_fall_back() {
    let start = Instant::now();
    let events = run("WAITFOR DELAY '00:00:00.100'");
    assert_eq!(events, vec![Event::Done(None, false)]);
    assert!(
        start.elapsed() >= Duration::from_millis(100),
        "waited {:?}",
        start.elapsed()
    );
}

#[test]
fn waitfor_delay_keeps_the_text_the_client_wrote() {
    // Error 148 prints the time string unchanged, so the fall-back must receive the text
    // of the statement and not a rendering of the AST.
    let events = run("WAITFOR DELAY 'abc'; SELECT 1");
    let err = only_error(&events);
    assert_eq!(err.number, 148);
    assert!(err.message.contains("'abc'"), "{err:?}");
    assert_eq!(events.len(), 2, "the batch stopped: {events:#?}");
}

#[test]
fn a_statement_no_task_implements_yet_answers_the_internal_error() {
    // `WAITFOR TIME` parses, the binder does not bind it, and the fake engine does not know
    // it either: what reaches the client is the internal error that names it, not an
    // invented 102.
    let events = run("WAITFOR TIME '22:00'");
    let err = only_error(&events);
    assert_eq!(err.number, 50000, "{err:?}");
    assert!(err.message.contains("WAITFOR"), "{err:?}");
    assert_eq!(events.last(), Some(&Event::Done(None, false)));
}

#[test]
fn a_cancelled_batch_sends_nothing_and_fails() {
    let mut session = session();
    let cancel = session.cancel_handle();
    cancel.cancel();
    let (events, result) = run_on(&mut session, "SELECT 1");
    let err = result.expect_err("a cancelled batch returns the internal error");
    assert_eq!(err.number, 50000);
    assert!(events.is_empty(), "{events:#?}");
}

// ---------------------------------------------------------------------------------------
// RPC: 2812 for a procedure the dispatcher does not serve
// ---------------------------------------------------------------------------------------

#[test]
fn rpc_by_id_gets_2812_under_the_name_of_the_special_procedure() {
    let rpc = Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::SP_EXECUTESQL,
        options: 0,
        params: Vec::new(),
        transaction_descriptor: 0,
    };
    let mut sink = Recording::default();
    session().run_rpc(&rpc, &mut sink).unwrap();
    assert_eq!(sink.0.len(), 2, "{:#?}", sink.0);
    let err = only_error(&sink.0);
    assert_eq!(err.number, 2812);
    assert_eq!(
        err.message,
        SqlError::procedure_not_found("sp_executesql").message
    );
    assert_eq!(sink.0[1], Event::Done(None, false));
}

#[test]
fn rpc_by_name_gets_2812_with_the_name_as_sent() {
    let rpc = Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name("dbo.p".into()),
        options: 0,
        params: Vec::new(),
        transaction_descriptor: 0,
    };
    let mut sink = Recording::default();
    session().run_rpc(&rpc, &mut sink).unwrap();
    assert_eq!(
        only_error(&sink.0).message,
        SqlError::procedure_not_found("dbo.p").message
    );
}

// ---------------------------------------------------------------------------------------
// The frozen clock
// ---------------------------------------------------------------------------------------

/// `SELECT GETDATE(), GETDATE()` returns twice the same value: the clock is read once per
/// statement (`SessionEvalContext`, `eval_context.rs`). The freeze itself is also tested
/// where it lives, by `eval_context::tests::the_clock_is_read_once_and_never_again`.
#[test]
fn getdate_is_frozen_within_a_statement() {
    let events = run("SELECT GETDATE(), GETDATE()");
    let row = rows(&events).into_iter().next().expect("one row");
    assert_eq!(row[0], row[1], "one clock read per statement: {row:?}");
}
