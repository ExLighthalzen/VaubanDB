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
fn a_not_null_violation_lets_the_batch_continue() {
    // 515 is statement-scoped, like 8134 above: a `NOT NULL` violation stops the statement
    // that raised it, not the batch, as on SQL Server.
    let mut session = session();
    let (_, setup) = run_on(&mut session, "CREATE TABLE dbo.t515 (a int NOT NULL)");
    setup.expect("the setup statement succeeds");
    let (events, result) = run_on(
        &mut session,
        "SELECT 1; INSERT dbo.t515 (a) VALUES (NULL); SELECT 2",
    );
    result.expect("a batch reports its errors through the sink, not through its result");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "{events:#?}"
    );
    assert_eq!(only_error(&events).number, 515);
    let error = events
        .iter()
        .position(|event| matches!(event, Event::Error(_)))
        .expect("the statement raises its error");
    assert_eq!(
        events.get(error + 1),
        Some(&Event::Done(None, true)),
        "ERROR is closed by a DONE carrying MORE: {events:#?}"
    );
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
// Deferred name resolution: 208 is resolved when the statement runs
// ---------------------------------------------------------------------------------------

/// The events of `events` that are DONEs, in order.
fn dones(events: &[Event]) -> Vec<(Option<u64>, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Done(count, more) => Some((*count, *more)),
            _ => None,
        })
        .collect()
}

/// The number of result sets of `events`, one per COLMETADATA.
fn result_sets(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::Columns(_)))
        .count()
}

#[test]
fn a_table_created_earlier_in_the_batch_is_read_by_the_rest_of_it() {
    // The shape of a migration script sent as one batch: the `INSERT` and the `SELECT`
    // name a table the catalogue does not hold when the batch is bound. Both are bound when
    // the batch reaches them, after the `CREATE TABLE` committed: one row, no error, and the
    // DONE of each statement in order (the DONE of an `INSERT` carries no count at this
    // layer, in one batch as in two).
    let events =
        run("CREATE TABLE dbo.m1 (a int); INSERT INTO dbo.m1 VALUES (1); SELECT * FROM dbo.m1;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
    assert_eq!(columns(&events, 0)[0].name, "a");
    assert_eq!(
        dones(&events),
        vec![(None, true), (None, true), (Some(1), false)],
        "{events:#?}"
    );
}

#[test]
fn a_table_created_by_an_exec_is_read_by_the_rest_of_the_batch() {
    // The `CREATE TABLE` runs in the nested batch of the `EXEC`; the `INSERT` and the
    // `SELECT` of the calling batch are deferred and see it.
    let events = run(
        "EXEC('CREATE TABLE dbo.s3 (n int)'); INSERT dbo.s3 VALUES (42); SELECT n FROM dbo.s3;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(42)]], "{events:#?}");
    assert_eq!(columns(&events, 0)[0].name, "n");
}

#[test]
fn a_table_created_earlier_in_the_batch_lets_a_create_index_bind() {
    // The `CREATE INDEX` names a table the catalogue does not hold when the batch is
    // bound: deferred like a `SELECT` on the same table, it resolves once the `CREATE
    // TABLE` before it has run. No error, and both statements complete.
    let events = run("CREATE TABLE dbo.s3b (a int); CREATE INDEX ix ON dbo.s3b (a);");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(result_sets(&events), 0, "{events:#?}");
    assert_eq!(
        dones(&events),
        vec![(None, true), (None, false)],
        "{events:#?}"
    );
}

#[test]
fn a_table_created_by_an_exec_lets_an_alter_table_bind() {
    // The `CREATE TABLE` runs in the nested batch of the `EXEC`; the `ALTER TABLE` of the
    // calling batch is deferred and sees it once the `EXEC` has run.
    let events = run("EXEC('CREATE TABLE dbo.s3c (a int)'); ALTER TABLE dbo.s3c ADD b int;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(result_sets(&events), 0, "{events:#?}");
}

#[test]
fn a_create_index_on_a_table_still_missing_is_1088_at_run_time() {
    // No `CREATE TABLE` anywhere: the deferred statement is bound again against the same
    // catalogue and still fails. 1088 is batch-scoped, like 208, so the `SELECT 1` before
    // it has sent its row and the `SELECT 2` after it does not run.
    let events = run("SELECT 1; CREATE INDEX ix ON dbo.s3d (a); SELECT 2;");
    let err = only_error(&events);
    assert_eq!(err.number, 1088, "{err:?}");
    assert_eq!(err.severity, 16, "{err:?}");
    assert_eq!(err.state, 12, "{err:?}");
    assert_eq!(err.line, 1, "{err:?}");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
    assert_eq!(result_sets(&events), 1, "{events:#?}");
    assert_eq!(
        dones(&events),
        vec![(Some(1), true), (None, false)],
        "{events:#?}"
    );
}

#[test]
fn an_alter_table_on_a_table_still_missing_is_4902_at_run_time() {
    // Same shape as the `CREATE INDEX` case above, for `ALTER TABLE`: 4902 is batch-scoped
    // too, so the `SELECT 2` after it does not run. An `ALTER TABLE` anywhere in the batch
    // narrows how many statements are bound at once (`bind_limit_for`), which is why the
    // `more` bit of the `SELECT 1` DONE is not asserted here the way it is above.
    let events = run("SELECT 1; ALTER TABLE dbo.s3e ADD b int; SELECT 2;");
    let err = only_error(&events);
    assert_eq!(err.number, 4902, "{err:?}");
    assert_eq!(err.severity, 16, "{err:?}");
    assert_eq!(err.state, 1, "{err:?}");
    assert_eq!(err.line, 1, "{err:?}");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
    assert_eq!(result_sets(&events), 1, "{events:#?}");
    assert_eq!(events.last(), Some(&Event::Done(None, false)));
}

#[test]
fn a_drop_index_on_a_table_still_missing_is_3701_and_the_batch_continues() {
    // Counter-proof: `DROP INDEX` carries no `ObjectId` and needs no catalogue
    // (`ddl_index.rs`), so this number does not join the deferred set above; it already
    // answers at run time, unaffected by it. 3701 is statement-scoped, so `SELECT 2` runs.
    let events = run("SELECT 1; DROP INDEX ix ON dbo.s3f; SELECT 2;");
    let err = only_error(&events);
    assert_eq!(err.number, 3701, "{err:?}");
    assert_eq!(err.severity, 11, "{err:?}");
    assert_eq!(err.state, 6, "{err:?}");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "{events:#?}"
    );
    assert_eq!(result_sets(&events), 2, "{events:#?}");
}

#[test]
fn a_select_before_the_create_is_208_at_run_time() {
    // The counter-proof of the deferral: the same statement written **before** the
    // `CREATE TABLE` answers 208 when the batch reaches it, on the line the statement
    // starts on (2, the `FROM` being on line 3), with no result set of its own. The
    // `SELECT 1` before it has sent its row, which a compilation error would have
    // silenced; 208 is batch-scoped, so the `CREATE TABLE` and the `SELECT 2` do not run.
    let mut session = session();
    for (batch, name) in [
        (
            "SELECT 1;\nSELECT *\n  FROM dbo.s4;\nCREATE TABLE dbo.s4 (a int);\nSELECT 2;",
            "s4",
        ),
        (
            "SELECT 1;\nINSERT dbo.s4b\n  VALUES (1);\nCREATE TABLE dbo.s4b (a int);\nSELECT 2;",
            "s4b",
        ),
    ] {
        let (events, result) = run_on(&mut session, batch);
        result.expect("the batch reports its error through the sink");
        let err = only_error(&events);
        assert_eq!(err.number, 208, "{batch:?}: {err:?}");
        assert_eq!(err.severity, 16, "{batch:?}: {err:?}");
        assert_eq!(err.line, 2, "{batch:?}: {err:?}");
        assert_eq!(
            rows(&events),
            vec![vec![Value::I32(1)]],
            "{batch:?}: {events:#?}"
        );
        assert_eq!(result_sets(&events), 1, "{batch:?}: {events:#?}");
        assert_eq!(
            dones(&events),
            vec![(Some(1), true), (None, false)],
            "{batch:?}: {events:#?}"
        );
        assert_eq!(session.state().last_error, 208);

        // The `CREATE TABLE` after the failing statement did not run: the table is still
        // missing for the next batch.
        let (again, _) = run_on(&mut session, &format!("SELECT 1 FROM dbo.{name};"));
        assert_eq!(only_error(&again).number, 208, "{name}: {again:#?}");
    }
}

#[test]
fn a_drop_then_a_select_on_the_dropped_table_is_208_at_run_time() {
    // The table is created by the batch, so the `SELECT` is deferred; when the batch
    // reaches it the `DROP TABLE` has run, and the binding of that moment answers 208 on
    // the line of the `SELECT`, after the row of the `SELECT 1`, and no `2`.
    let events = run(
        "CREATE TABLE dbo.s5 (a int);\nSELECT 1;\nDROP TABLE dbo.s5;\nSELECT * FROM dbo.s5;\nSELECT 2;",
    );
    let err = only_error(&events);
    assert_eq!(err.number, 208, "{err:?}");
    assert_eq!(err.line, 4, "{err:?}");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
    assert_eq!(result_sets(&events), 1, "{events:#?}");
    assert_eq!(events.last(), Some(&Event::Done(None, false)));
}

#[test]
fn a_deferred_statement_raises_207_when_it_runs() {
    // The table is missing when the batch is bound, so the whole statement is deferred,
    // column check included: the 207 falls when the batch reaches the statement, after the
    // row of the `SELECT 1`, and stops the batch. The counter-proof is the second batch:
    // once the table exists before the batch, the same 207 is a compilation error and the
    // `SELECT 1` sends nothing (`a_compilation_error_silences_the_whole_batch` on a
    // statement without a table).
    let mut session = session();
    let (events, _) = run_on(
        &mut session,
        "SELECT 1; CREATE TABLE dbo.s6 (a int); SELECT nosuch FROM dbo.s6; SELECT 2;",
    );
    let err = only_error(&events);
    assert_eq!(err.number, 207, "{err:?}");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
    assert_eq!(result_sets(&events), 1, "{events:#?}");
    assert_eq!(events.last(), Some(&Event::Done(None, false)));

    let (events, _) = run_on(
        &mut session,
        "SELECT 1; SELECT nosuch FROM dbo.s6; SELECT 2;",
    );
    assert_eq!(only_error(&events).number, 207, "{events:#?}");
    assert!(rows(&events).is_empty(), "{events:#?}");
    assert_eq!(result_sets(&events), 0, "{events:#?}");
}

#[test]
fn a_deferred_statement_in_a_false_if_raises_nothing() {
    // The `IF` names the table in its branch, so it is deferred as a whole and bound again
    // once the table exists; its false branch never runs. The `SELECT 2` follows.
    let events = run("CREATE TABLE dbo.s8 (a int); IF 1 = 0 SELECT * FROM dbo.s8; SELECT 2;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(2)]], "{events:#?}");
    assert_eq!(result_sets(&events), 1, "{events:#?}");
}

#[test]
fn a_deferred_statement_reads_the_catalogue_of_its_own_transaction() {
    // Inside `BEGIN TRAN`, the `CREATE TABLE` is not committed when the `INSERT` and the
    // `SELECT` are bound: the catalogue is read through the session transaction, which
    // sees it. The `SELECT @@TRANCOUNT` after the `COMMIT` reads 0.
    let events = run(
        "BEGIN TRAN; CREATE TABLE dbo.s9 (a int); INSERT INTO dbo.s9 VALUES (3); \
         SELECT * FROM dbo.s9; COMMIT; SELECT @@TRANCOUNT;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(3)], vec![Value::I32(0)]],
        "{events:#?}"
    );
}

#[test]
fn a_deferred_statement_posts_its_row_count_and_reads_the_variables_before_it() {
    // `@@ROWCOUNT` after a deferred `INSERT` of two rows is 2, and a deferred `INSERT` reads
    // a variable declared before it in the batch.
    let events = run(
        "CREATE TABLE dbo.s10 (a int); INSERT INTO dbo.s10 VALUES (1), (2); SELECT @@ROWCOUNT;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(2)]], "{events:#?}");

    let events = run(
        "DECLARE @v int = 5; CREATE TABLE dbo.s11 (a int); INSERT dbo.s11 VALUES (@v); SELECT a FROM dbo.s11;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(5)]], "{events:#?}");
}

// ---------------------------------------------------------------------------------------
// A statement of a branch or of a loop body waits for the control of flow
// ---------------------------------------------------------------------------------------

#[test]
fn a_branch_that_is_not_taken_does_not_look_for_its_table() {
    // Nothing in this batch creates `dbo.never`, and nothing needs to: the branch is not
    // taken, so the name is never resolved. Both `SELECT`s answer, in order, and no error
    // reaches the sink.
    let events = run("SELECT 1; IF 1 = 0 SELECT * FROM dbo.never; SELECT 2;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "{events:#?}"
    );
    assert_eq!(result_sets(&events), 2, "{events:#?}");
}

#[test]
fn a_block_creates_the_table_it_then_writes_to() {
    // The shape of a migration script: the `INSERT` of the block names a table the
    // `CREATE TABLE` of the same block creates, and the `SELECT` after the `IF` reads it.
    let events = run(
        "IF OBJECT_ID('dbo.b1') IS NULL BEGIN CREATE TABLE dbo.b1 (a int); \
         INSERT dbo.b1 VALUES (1); END; SELECT * FROM dbo.b1;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
}

#[test]
fn a_block_at_the_head_of_the_batch_creates_then_writes() {
    // `BEGIN … END` written first, with no `IF` around it.
    let events = run(
        "BEGIN CREATE TABLE dbo.b3 (a int); INSERT dbo.b3 VALUES (7); END; SELECT a FROM dbo.b3;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(7)]], "{events:#?}");
}

#[test]
fn a_while_body_writes_to_the_table_its_first_turn_created() {
    // The loop creates the table on its first turn and writes on every turn, so the
    // `INSERT` is bound again against the catalogue of each turn.
    let events = run("DECLARE @i int = 0; WHILE @i < 2 BEGIN SET @i = @i + 1; \
         IF @i = 1 CREATE TABLE dbo.b4 (a int); \
         INSERT dbo.b4 VALUES (@i); END; SELECT a FROM dbo.b4 ORDER BY a;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "{events:#?}"
    );
}

#[test]
fn a_nested_if_creates_the_table_the_batch_then_reads() {
    let events = run(
        "IF 1 = 1 BEGIN IF OBJECT_ID('dbo.b5') IS NULL BEGIN CREATE TABLE dbo.b5 (a int); \
         INSERT dbo.b5 VALUES (3); END END; SELECT a FROM dbo.b5;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(3)]], "{events:#?}");
}

#[test]
fn an_else_branch_is_looked_up_only_when_it_is_taken() {
    // The `ELSE` names a table nothing creates. Not taken, the batch runs through; taken,
    // 208 falls on the line of the statement the `ELSE` holds, not on the line of the
    // `IF`, and the batch stops there (208 is batch-scoped).
    let events = run("IF 1 = 1\n  SELECT 10;\nELSE\n  SELECT * FROM dbo.b6;\nSELECT 11;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(10)], vec![Value::I32(11)]],
        "{events:#?}"
    );

    let events = run("IF 1 = 0\n  SELECT 10;\nELSE\n  SELECT * FROM dbo.b6;\nSELECT 11;");
    let error = only_error(&events);
    assert_eq!(error.number, 208, "{events:#?}");
    assert_eq!(error.line, 4, "{events:#?}");
    assert!(rows(&events).is_empty(), "{events:#?}");
}

#[test]
fn a_branch_that_is_taken_over_a_table_nothing_creates_is_208_at_run_time() {
    // The `SELECT 1` before it has produced its result set, so 208 fell while the batch
    // ran; it carries the line of the statement the branch holds, and the `SELECT 2`
    // after it does not run.
    let events = run("SELECT 1;\nIF 1 = 1\n  SELECT * FROM dbo.b7;\nSELECT 2;");
    let error = only_error(&events);
    assert_eq!(error.number, 208, "{events:#?}");
    assert_eq!(error.line, 3, "{events:#?}");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
}

#[test]
fn a_branch_that_fails_on_another_number_still_stops_the_batch_before_it_runs() {
    // The counter-proof of the pair: the table is there and the column is not, so 207
    // falls while the batch is compiled. The branch is not taken and the `SELECT 1`
    // before it produces nothing, which is what separates a compilation error from the
    // 208 above.
    let mut session = session();
    let (_, result) = run_on(&mut session, "CREATE TABLE dbo.b8 (a int);");
    result.expect("the setup batch reports through the sink");
    let (events, result) = run_on(
        &mut session,
        "SELECT 1;\nIF 1 = 0\n  SELECT nosuch FROM dbo.b8;\nSELECT 2;",
    );
    result.expect("the batch reports through the sink");
    let error = only_error(&events);
    assert_eq!(error.number, 207, "{events:#?}");
    assert_eq!(error.line, 3, "{events:#?}");
    assert_eq!(result_sets(&events), 0, "{events:#?}");
}

#[test]
fn a_migration_script_replayed_over_the_table_it_created_does_nothing() {
    // The idiom of a deployment script, run twice on the same session. The second run
    // finds `dbo.m1` there, so the branch is not taken: no error reaches the sink and the
    // single row inserted by the first run is still the only one.
    let script = "IF OBJECT_ID('dbo.m1') IS NULL BEGIN CREATE TABLE dbo.m1 (a int); \
         INSERT dbo.m1 VALUES (1); END;";
    let mut session = session();
    let (first, result) = run_on(&mut session, script);
    result.expect("the batch reports through the sink");
    assert!(
        !first.iter().any(|event| matches!(event, Event::Error(_))),
        "{first:#?}"
    );

    let (second, result) = run_on(&mut session, script);
    result.expect("the batch reports through the sink");
    assert!(
        !second.iter().any(|event| matches!(event, Event::Error(_))),
        "{second:#?}"
    );

    let (events, result) = run_on(&mut session, "SELECT a FROM dbo.m1;");
    result.expect("the batch reports through the sink");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
}

#[test]
fn a_branch_that_is_taken_over_a_table_the_batch_already_holds_is_2714_at_run_time() {
    // The table is there when the batch is bound, so the `CREATE TABLE` of the branch
    // could have failed at compilation; the row of `SELECT 1` says it did not. 2714 is
    // batch-scoped, so `SELECT 2` does not run.
    let mut session = session();
    let (_, result) = run_on(&mut session, "CREATE TABLE dbo.m2 (a int);");
    result.expect("the setup batch reports through the sink");

    let (events, result) = run_on(
        &mut session,
        "SELECT 1;\nIF 1 = 1\n  CREATE TABLE dbo.m2 (a int);\nSELECT 2;",
    );
    result.expect("the batch reports through the sink");
    let error = only_error(&events);
    assert_eq!((error.number, error.state), (2714, 6), "{events:#?}");
    assert_eq!(error.line, 3, "{events:#?}");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
}

#[test]
fn a_branch_that_drops_a_table_nothing_holds_is_3701_and_the_batch_goes_on() {
    // The neighbouring scope: 3701 is statement-scoped, so the same shape lets `SELECT 2`
    // run where the 2714 above stops the batch.
    let events = run("SELECT 1;\nIF 1 = 1 DROP TABLE dbo.m3;\nSELECT 2;");
    let error = only_error(&events);
    assert_eq!(error.number, 3701, "{events:#?}");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "{events:#?}"
    );
}

#[test]
fn a_branch_that_repeats_a_column_name_still_stops_the_batch_before_it_runs() {
    // The counter-proof on the state: 2705 state 3 is a fault of the written column list,
    // answered while the batch is compiled, so `SELECT 1` produces nothing — where the
    // 2714 of the test above produced its row. The branch is not even taken.
    let mut session = session();
    let (_, result) = run_on(&mut session, "CREATE TABLE dbo.m4 (a int);");
    result.expect("the setup batch reports through the sink");

    let (events, result) = run_on(
        &mut session,
        "SELECT 1;\nIF 1 = 0\n  CREATE TABLE dbo.m4 (a int, a int);\nSELECT 2;",
    );
    result.expect("the batch reports through the sink");
    let error = only_error(&events);
    assert_eq!((error.number, error.state), (2705, 3), "{events:#?}");
    assert_eq!(result_sets(&events), 0, "{events:#?}");
}

#[test]
fn an_error_of_a_branch_keeps_the_line_the_executor_gave_it() {
    // The `IF` starts on line 2 and holds a `SELECT` on line 3. The divide by zero is
    // answered on line 3, the line of the statement the branch holds, and the batch goes
    // on (8134 is statement-scoped).
    let events = run("SELECT 1;\nIF 1 = 1\n  SELECT 1 / 0;\nSELECT 2;");
    let error = only_error(&events);
    assert_eq!(error.number, 8134, "{events:#?}");
    assert_eq!(error.line, 3, "{events:#?}");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "{events:#?}"
    );

    // The counter-proof: the same divide by zero written as a statement of its own, over
    // two lines, is answered on the line the **statement** starts on. The two shapes hold
    // the fault on the same line, so the line that changes is the rule.
    let events = run("SELECT 1;\nSELECT\n  1 / 0;\nSELECT 2;");
    let error = only_error(&events);
    assert_eq!(error.number, 8134, "{events:#?}");
    assert_eq!(error.line, 2, "{events:#?}");
}

#[test]
fn an_error_of_a_condition_carries_the_line_of_the_statement_that_holds_it() {
    // The condition is evaluated before the branch, and it belongs to the `IF`: the
    // divide by zero of a condition written on line 3 is answered on line 2, where the
    // `IF` starts, and the batch goes on. The three shapes below are the condition of an
    // `IF`, the condition of a `WHILE` and a conversion that fails in a condition.
    let events = run("SELECT 1;\nIF\n  1 / 0 = 1\n  SELECT 2;\nSELECT 3;");
    let error = only_error(&events);
    assert_eq!(error.number, 8134, "{events:#?}");
    assert_eq!(error.line, 2, "{events:#?}");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(3)]],
        "{events:#?}"
    );

    let events = run("SELECT 1;\nWHILE\n  1 / 0 = 1\n  SELECT 2;\nSELECT 3;");
    let error = only_error(&events);
    assert_eq!(error.number, 8134, "{events:#?}");
    assert_eq!(error.line, 2, "{events:#?}");

    let events = run("SELECT 1;\nIF\n  CONVERT(int, 'abc') = 1\n  SELECT 2;");
    let error = only_error(&events);
    assert_eq!(error.number, 245, "{events:#?}");
    assert_eq!(error.line, 2, "{events:#?}");

    // A condition folded onto the line of the statement it holds is still the condition:
    // the divide by zero written on line 3, next to the `SELECT 2`, is answered on line 2.
    for batch in [
        "SELECT 1;\nIF 1 = 1 AND\n  1 / 0 = 1 SELECT 2;\nSELECT 3;",
        "SELECT 1;\nWHILE 1 = 1 AND\n  1 / 0 = 1 SELECT 2;\nSELECT 3;",
        "SELECT 1;\nIF 1 = 1 AND\n  1 / 0 = 1 BEGIN SELECT 2; END\nSELECT 3;",
    ] {
        let events = run(batch);
        let error = only_error(&events);
        assert_eq!(error.number, 8134, "{batch}: {events:#?}");
        assert_eq!(error.line, 2, "{batch}: {events:#?}");
    }
}

#[test]
fn a_branch_statement_over_two_lines_answers_the_line_of_the_fault() {
    // What the rule above keeps is the line the executor put on the error, which is the
    // line of the failing sub-expression: a branch statement starting on line 3 whose
    // `1 / 0` is written on line 4 is answered on line 4. The line a statement of its own
    // is answered on, line 2 in the counter-proof of
    // `an_error_of_a_branch_keeps_the_line_the_executor_gave_it`, is the statement's.
    let events = run("SELECT 1;\nIF 1 = 1\n  SELECT\n    1 / 0;\nSELECT 3;");
    let error = only_error(&events);
    assert_eq!(error.number, 8134, "{events:#?}");
    assert_eq!(error.line, 4, "{events:#?}");
}

#[test]
fn a_create_index_and_an_alter_table_in_a_block_wait_for_the_table() {
    // 1088 and 4902 name a missing table the way 208 does, in a block as at the head of a
    // batch: both statements of the block are bound after the `CREATE TABLE` before them
    // has run.
    let events = run(
        "BEGIN CREATE TABLE dbo.b9 (a int); CREATE INDEX ix_b9 ON dbo.b9 (a); \
         ALTER TABLE dbo.b9 ADD b int; END; INSERT dbo.b9 (a, b) VALUES (1, 2); \
         SELECT a, b FROM dbo.b9;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1), Value::I32(2)]],
        "{events:#?}"
    );
}

#[test]
fn a_statement_of_a_block_reads_a_variable_declared_before_the_block() {
    let events = run(
        "DECLARE @v int = 42; IF OBJECT_ID('dbo.b10') IS NULL BEGIN \
         CREATE TABLE dbo.b10 (a int); INSERT dbo.b10 VALUES (@v); END; SELECT a FROM dbo.b10;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(42)]], "{events:#?}");
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

// ---------------------------------------------------------------------------------------
// Batch variables: DECLARE, SET, SELECT @x, IF, WHILE, RETURN
// ---------------------------------------------------------------------------------------

/// `bind_batch` must accumulate [`BatchVariables`]: with [`NoVariables`], `SET @i` fails
/// at binding time with 137 and the batch stops before `SELECT @i`.
#[test]
fn declare_set_select_reads_the_value_from_an_earlier_statement() {
    let events = run("DECLARE @i int; SET @i = 3; SELECT @i");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(3)]]);
}

#[test]
fn return_stops_the_batch_without_an_internal_error() {
    let events = run("SELECT 1; RETURN; SELECT 2");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]]);
    assert!(
        !events.iter().any(|event| matches!(event, Event::Info(_))),
        "the statement after RETURN must not run: {events:#?}"
    );
}

#[test]
fn a_variable_declared_in_one_batch_is_not_visible_in_the_next() {
    let mut session = session();
    let (first, _) = run_on(&mut session, "DECLARE @i int; SET @i = 3;");
    assert!(
        !first.iter().any(|event| matches!(event, Event::Error(_))),
        "{first:#?}"
    );
    let (second, _) = run_on(&mut session, "SELECT @i");
    let err = only_error(&second);
    assert_eq!(err.number, 137);
    assert_eq!(err.state, 2);
}

#[test]
fn if_reads_a_variable_set_earlier_in_the_batch() {
    let events = run("DECLARE @n int; SET @n = 1; IF @n = 1 SELECT 10 ELSE SELECT 20");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(10)]]);
}

#[test]
fn while_reads_a_variable_set_earlier_in_the_batch() {
    let events = run("DECLARE @i int = 0; WHILE @i < 2 BEGIN SET @i = @i + 1; END; SELECT @i");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(2)]]);
}

#[test]
fn rowcount_still_tracks_select_after_variable_statements() {
    let events = run("DECLARE @i int; SET @i = 3; SELECT 1; SELECT @@ROWCOUNT");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(1)]],
        "{events:#?}"
    );
}

#[test]
fn declare_with_an_initial_value_sets_rowcount_to_one() {
    let events = run("DECLARE @x int = 1; SELECT @@ROWCOUNT;");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]]);
}

#[test]
fn declare_with_two_initial_values_still_sets_rowcount_to_one() {
    let events = run("DECLARE @a int = 1, @b int = 2; SELECT @@ROWCOUNT;");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]]);
}

#[test]
fn declare_without_a_value_leaves_rowcount_alone() {
    // A prior `INSERT` of two rows sets `@@ROWCOUNT` to 2; the bare `DECLARE` that follows
    // does not touch it, unlike the initialized form above.
    let events = run(
        "CREATE TABLE dbo.sr1 (a int); INSERT INTO dbo.sr1 VALUES (1), (2); \
         DECLARE @a int; SELECT @@ROWCOUNT;",
    );
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(2)]),
        "{events:#?}"
    );
}

#[test]
fn if_with_a_false_condition_sets_rowcount_to_zero() {
    // Same two-row `INSERT` first, so the reset a false `IF` leaves (0) is told apart from a
    // value it merely left alone.
    let events = run(
        "CREATE TABLE dbo.sr2 (a int); INSERT INTO dbo.sr2 VALUES (1), (2); \
         DECLARE @a int; IF 1 = 0 SET @a = 1; SELECT @@ROWCOUNT;",
    );
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(0)]),
        "{events:#?}"
    );
}

#[test]
fn while_ends_with_rowcount_at_zero() {
    let events = run("DECLARE @i int = 0; WHILE @i < 3 SET @i = @i + 1; SELECT @@ROWCOUNT, @i;");
    assert_eq!(rows(&events), vec![vec![Value::I32(0), Value::I32(3)]]);
}

#[test]
fn nested_declare_with_a_value_sets_rowcount_to_one() {
    // `EXEC('…')` runs its text through the same nested pipeline as `sp_executesql` and a
    // procedure call, with its own `@@ROWCOUNT`.
    let events = run("EXEC('DECLARE @x int = 1; SELECT @@ROWCOUNT;');");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]]);
}

#[test]
fn nested_while_ends_with_rowcount_at_zero() {
    let events =
        run("EXEC('DECLARE @i int = 0; WHILE @i < 3 SET @i = @i + 1; SELECT @@ROWCOUNT, @i;');");
    assert_eq!(rows(&events), vec![vec![Value::I32(0), Value::I32(3)]]);
}

#[test]
fn select_assign_from_rows_sets_rowcount_to_the_rows_walked() {
    let events = run(
        "DECLARE @x int; SELECT @x = col FROM (VALUES (1), (2), (3)) AS t(col); \
         SELECT @@ROWCOUNT, @x;",
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(3), Value::I32(3)]]);
}

#[test]
fn select_assign_over_no_row_sets_rowcount_to_zero() {
    // The variable keeps the value it was declared with, and the count of a source that
    // produced nothing is 0, not the 1 an assignment without a `FROM` leaves.
    let events = run(
        "DECLARE @x int = 7; SELECT @x = col FROM (VALUES (1)) AS t(col) WHERE 1 = 0; \
         SELECT @@ROWCOUNT, @x;",
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(0), Value::I32(7)]]);
}

#[test]
fn print_after_a_select_assign_sets_rowcount_to_zero() {
    // `PRINT` is a statement of its own and starts its own count, so what the assignment
    // posted is gone by the time the read runs.
    let events = run(
        "DECLARE @x int; SELECT @x = col FROM (VALUES (1), (2)) AS t(col); PRINT 'x'; \
         SELECT @@ROWCOUNT;",
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(0)]]);
}

#[test]
fn select_assign_over_rows_accumulates_into_the_variable() {
    let events = run("DECLARE @s varchar(20) = ''; \
         SELECT @s = @s + CAST(col AS varchar(2)) FROM (VALUES (1), (2), (3)) AS t(col); \
         SELECT @s;");
    assert_eq!(
        rows(&events),
        vec![vec![Value::String(SqlString {
            text: "123".to_owned()
        })]]
    );
}

#[test]
fn select_assign_sends_no_result_set() {
    // The assignment closes a statement that announced no columns: the only `Columns`
    // event of the batch belongs to the `SELECT` that reads the variable back.
    let events = run("DECLARE @x int; SELECT @x = col FROM (VALUES (1)) AS t(col); SELECT @x;");
    let announced = events
        .iter()
        .filter(|event| matches!(event, Event::Columns(_)))
        .count();
    assert_eq!(announced, 1, "{events:#?}");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]]);
}

// ---------------------------------------------------------------------------------------
// GOTO: the jump, the labels of the batch, and 133
// ---------------------------------------------------------------------------------------

/// A `GOTO` written before its label runs the statement at the label next, so the
/// statements between the two do not run: `@i` keeps the value it was declared with.
#[test]
fn a_goto_jumps_forward_over_the_statements_it_skips() {
    let events = run("DECLARE @i int = 0; GOTO skipit; SET @i = 99; skipit: SELECT @i;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(0)]]);
}

/// A `GOTO` written after its label runs the batch from that label again, which is how a
/// batch loops without a `WHILE`: three turns of the `SET` before the condition is false.
#[test]
fn a_goto_jumps_backward_and_the_batch_loops() {
    let events =
        run("DECLARE @i int = 0; again: SET @i = @i + 1; IF @i < 3 GOTO again; SELECT @i;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(3)]]);
}

/// A `GOTO` written inside a `BEGIN … END` jumps to a label of the batch: the rest of the
/// block does not run, and neither do the statements between the block and the label.
#[test]
fn a_goto_written_in_a_block_jumps_to_a_label_of_the_batch() {
    let events = run(
        "DECLARE @i int = 0;\nBEGIN SET @i = 1; GOTO after; SET @i = 50; END\nSET @i = 99;\nafter: SELECT @i;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]]);
}

/// A `GOTO` written as the body of a `WHILE` leaves the loop and jumps: the label sits
/// before the `WHILE`, so the batch runs the `SET` again and reaches the `WHILE` with a
/// larger value each turn.
#[test]
fn a_goto_in_a_while_body_jumps_out_of_the_loop() {
    let events = run(
        "DECLARE @j int = 0; startover: SET @j = @j + 1; WHILE @j < 3 GOTO startover; SELECT @j;",
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(3)]]);
}

/// A label the batch does not declare is 133, severity 15, state 1, and the batch
/// produces nothing: the statement after the `GOTO` sends no result set, which is what
/// separates a compilation error from a run-time one.
#[test]
fn a_goto_to_an_unknown_label_is_133_and_stops_the_batch() {
    let events = run("GOTO nowhere;\nSELECT 1;");
    let err = only_error(&events);
    assert_eq!(err.number, 133);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "GOTO names the label 'nowhere', which this batch does not declare."
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Columns(_))),
        "no statement of the batch ran: {events:#?}"
    );
    assert_eq!(events.last(), Some(&Event::Done(None, false)));
}

/// A `GOTO` written in a branch that is not taken is checked all the same: the label is
/// read from the text of the batch, before its first statement runs.
#[test]
fn a_goto_in_a_branch_that_is_not_taken_is_still_133() {
    let events = run("IF 1 = 0 GOTO nowhere;\nSELECT 1;");
    let err = only_error(&events);
    assert_eq!(err.number, 133);
    assert!(rows(&events).is_empty(), "{events:#?}");
}

/// The line of 133 is the last line of the batch, not the line of the `GOTO` and not the
/// line of the last statement: a trailing blank line and a trailing comment both push it
/// past the last statement, and a last statement spread over two lines answers its second.
#[test]
fn the_line_of_133_is_the_last_line_of_the_batch() {
    for (text, line) in [
        ("GOTO nowhere;\nSELECT 1;", 2),
        ("GOTO nowhere;\nSELECT 1;\n\n-- a comment", 4),
        ("GOTO nowhere;\nSELECT\n  1;", 3),
        ("GOTO nowhere;", 1),
    ] {
        let err = only_error(&run(text));
        assert_eq!(err.number, 133, "{text:?}");
        assert_eq!(err.line, line, "{text:?}");
    }
}

/// A name declared by two labels keeps the first: the jump lands on it, so the statement
/// of the second declaration runs after the one of the first. SQL Server refuses the
/// batch instead, with a number this engine does not raise.
#[test]
fn a_label_declared_twice_keeps_the_first_declaration() {
    let events =
        run("DECLARE @i int = 0; GOTO l; l: SET @i = @i + 1; l: SET @i = @i + 10; SELECT @i;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(11)]]);
}

/// The DONE of a `GOTO` carries MORE even when the `GOTO` is the last statement of the
/// text: the statement at its label runs after it, so it is not the DONE that closes the
/// response.
#[test]
fn a_goto_written_last_still_leaves_more_on_its_done() {
    let events = run("DECLARE @i int = 0; skip: SET @i = @i + 1; IF @i < 2 GOTO skip;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    let dones: Vec<&Event> = events
        .iter()
        .filter(|event| matches!(event, Event::Done(..)))
        .collect();
    assert_eq!(
        dones.last(),
        Some(&&Event::Done(None, false)),
        "only the last DONE of the response closes it: {events:#?}"
    );
    assert_eq!(
        dones
            .iter()
            .filter(|done| **done == &Event::Done(None, false))
            .count(),
        1,
        "{events:#?}"
    );
}

/// A batch that loops on a `GOTO` for ever answers the ATTENTION: the cancellation token
/// is read between two statements, so the jump comes back to it on every turn.
///
/// The counter-proof is the first half: the same jump written so that it ends by itself
/// runs to its `SELECT` and answers no error, on a session whose token is never raised.
#[test]
fn a_batch_that_loops_on_a_goto_answers_the_cancellation() {
    let bounded =
        run("DECLARE @i int = 0; again: SET @i = @i + 1; IF @i < 3 GOTO again; SELECT @i;");
    assert_eq!(rows(&bounded), vec![vec![Value::I32(3)]]);

    let mut session = session();
    let cancel = session.cancel_handle();
    let raiser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
    });
    let started = Instant::now();
    let (events, result) = run_on(&mut session, "forever: SELECT 1; GOTO forever;");
    raiser.join().expect("the token was raised");
    let err = result.expect_err("a cancelled batch returns the internal error");
    assert_eq!(err.number, 50000, "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the loop ended on the token, not on a timeout"
    );
    assert!(
        events.iter().any(|event| matches!(event, Event::Row(_))),
        "the loop ran before the token was raised: {}",
        events.len()
    );
}

/// A dynamic text jumps inside itself: its labels are its own, read from the text the
/// `EXEC` runs and not from the batch that issued it.
#[test]
fn a_goto_of_a_dynamic_text_jumps_inside_that_text() {
    let events = run("EXEC ('DECLARE @i int = 0; GOTO skipit; SET @i = 99; skipit: SELECT @i;');");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(0)]]);
}

/// A label the dynamic text does not declare is 133 on the last line of **that** text,
/// and the batch that issued the `EXEC` runs its next statement: no statement of the text
/// ran, so nothing there asked the caller to stop.
#[test]
fn a_goto_to_an_unknown_label_of_a_dynamic_text_is_133() {
    let events = run("EXEC ('GOTO nowhere;\nSELECT 1;');\nSELECT 2;");
    let err = only_error(&events);
    assert_eq!(err.number, 133);
    assert_eq!(err.line, 2, "the last line of the dynamic text");
    assert_eq!(rows(&events), vec![vec![Value::I32(2)]]);
}

/// A label written inside a `BEGIN … END` is not a label of the batch: the loop that runs
/// the batch steps over the block as one statement and holds no position inside it, so a
/// `GOTO` that names it answers 133. SQL Server runs the block from that label instead.
#[test]
fn a_goto_to_a_label_inside_a_block_is_133() {
    let events = run("GOTO inside;\nBEGIN inside: SELECT 1; END");
    let err = only_error(&events);
    assert_eq!(err.number, 133);
    assert!(rows(&events).is_empty(), "{events:#?}");
}

/// `@@ROWCOUNT` crosses a `GOTO` and a label: neither counts a row, so the count the
/// statement before them posted is the one the statement after them reads.
#[test]
fn a_goto_and_a_label_leave_the_row_count_alone() {
    let events = run("SELECT 1 UNION ALL SELECT 2;\nlbl: SELECT @@ROWCOUNT;");
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(2)]),
        "{events:#?}"
    );

    let events = run(
        "SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3;\nGOTO l;\nSELECT 9;\nl: SELECT @@ROWCOUNT;",
    );
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(3)]),
        "{events:#?}"
    );

    // A `DECLARE` with an initial value posts 1, and the jump carries it across.
    let events = run("DECLARE @i int = 0; GOTO l; l: SELECT @@ROWCOUNT;");
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
}

/// The counter-proof of the rule above: a `PRINT` written where the label sits puts the
/// count back to 0, so what crosses the jump is the pair of statements and not the batch.
#[test]
fn a_print_between_the_two_puts_the_row_count_back_to_zero() {
    let events = run("SELECT 1 UNION ALL SELECT 2;\nPRINT 'x';\nSELECT @@ROWCOUNT;");
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(0)]),
        "{events:#?}"
    );
}

/// `@@ERROR` crosses a `GOTO` and a label the same way: the number the failing statement
/// left is the one the statement after the jump reads.
#[test]
fn a_goto_and_a_label_leave_the_last_error_alone() {
    let events = run("SELECT 1 / 0;\nlbl: SELECT @@ERROR;");
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(8134)]),
        "{events:#?}"
    );

    let events = run("SELECT 1 / 0;\nGOTO l;\nSELECT 9;\nl: SELECT @@ERROR;");
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(8134)]),
        "{events:#?}"
    );
}

/// The counter-proof: a `PRINT` in the place of the label clears `@@ERROR`.
#[test]
fn a_print_between_the_two_clears_it() {
    let events = run("SELECT 1 / 0;\nPRINT 'x';\nSELECT @@ERROR;");
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(0)]),
        "{events:#?}"
    );
}

/// A variable belongs to the whole batch: a `DECLARE` the jump stepped over leaves its
/// name readable, at `NULL`, since the initial value is written by running the `DECLARE`.
#[test]
fn a_goto_over_a_declare_leaves_the_variable_at_null() {
    let events = run("GOTO l;\nDECLARE @z int = 5;\nl: SELECT @z;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::Null]]);
}

/// A batch that holds an `ALTER TABLE` is bound one statement at a time, so the target of
/// a jump forward may lie past what is bound when the jump runs: the `DECLARE` stepped over
/// is entered once the statements up to the target are bound, in the batch and in the text
/// of an `EXEC` alike, and the variable reads `NULL`.
#[test]
fn a_goto_over_a_declare_in_a_batch_with_an_alter_table_still_declares_it() {
    for batch in [
        "CREATE TABLE dbo.z1 (a int);\nGOTO l1;\nDECLARE @z int = 5;\nALTER TABLE dbo.z1 ADD b int;\nl1: SELECT @z;",
        "CREATE TABLE dbo.z2 (a int);\nGOTO l1;\nALTER TABLE dbo.z2 ADD b int;\nDECLARE @z int = 5;\nl1: SELECT @z;",
        "EXEC ('CREATE TABLE dbo.z3 (a int); GOTO l1; DECLARE @z int = 5; ALTER TABLE dbo.z3 ADD b int; l1: SELECT @z;');",
    ] {
        let events = run(batch);
        assert!(
            !events.iter().any(|event| matches!(event, Event::Error(_))),
            "{batch}: {events:#?}"
        );
        assert_eq!(rows(&events), vec![vec![Value::Null]], "{batch}");
    }
}

/// The same name takes an assignment afterwards, converted to the type its `DECLARE`
/// wrote, which is the counter-proof that the jump entered the declaration and not just a
/// value: a name the session does not hold is stored as it was evaluated.
#[test]
fn a_variable_whose_declare_was_jumped_over_can_still_be_assigned() {
    let events = run("GOTO l;\nDECLARE @w int = 5;\nl: SET @w = 9; SELECT @w;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(9)]]);
}

/// The three rules hold in the text of an `EXEC` too, which runs its own loop over its own
/// statements: the count crosses the jump, the error crosses it, and the `DECLARE` stepped
/// over leaves its name at `NULL`.
#[test]
fn a_dynamic_text_keeps_the_count_the_error_and_the_jumped_declare() {
    let events =
        run("EXEC ('SELECT 1 UNION ALL SELECT 2; GOTO l; SELECT 9; l: SELECT @@ROWCOUNT;');");
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(2)]),
        "{events:#?}"
    );

    let events = run("EXEC ('SELECT 1 / 0; GOTO l; l: SELECT @@ERROR;');");
    assert_eq!(
        rows(&events).last(),
        Some(&vec![Value::I32(8134)]),
        "{events:#?}"
    );

    let events = run("EXEC ('GOTO l; DECLARE @z int = 5; l: SELECT @z;');");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::Null]]);
}
