//! Integration tests of `Session::run_nested`: parameter scope, procedure-style DONE
//! tokens, and output parameters.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::spawn_blocking;
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_parser::{ParseOptions, parse_parameter_declarations};
use vauban_session::{
    Engine, NestedOutcome, NestedParam, ProcAction, ProcArg, ProcParam, ResultSink, Session,
    SessionState, SystemProcedureResolver, TdsSink,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{
    ClientMessage, ColumnMeta, DoneStatus, EncryptPolicy, EnvChange, TdsStream, Token,
};
use vauban_types::{SqlType, TypeInfo, Value};

const SPID: i16 = 62;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns(Vec<ColumnMeta>),
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    DoneInProc(Option<u64>, bool),
    DoneProc(Option<u64>),
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
    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.0.push(Event::DoneInProc(rowcount, more));
        Ok(())
    }
    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.0.push(Event::DoneProc(rowcount));
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

#[derive(Default)]
struct TokenCounts {
    done: u32,
    done_in_proc: u32,
    done_proc: u32,
}

struct CountingSink {
    inner: Recording,
    counts: TokenCounts,
}

impl CountingSink {
    fn new() -> Self {
        Self {
            inner: Recording::default(),
            counts: TokenCounts::default(),
        }
    }
}

impl ResultSink for CountingSink {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.inner.columns(cols)
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.inner.row(row)
    }
    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.counts.done += 1;
        self.inner.done(rowcount, more)
    }
    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.counts.done_in_proc += 1;
        self.inner.done_in_proc(rowcount, more)
    }
    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.counts.done_proc += 1;
        self.inner.done_proc(rowcount)
    }
    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
        self.inner.info(msg)
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.inner.error(err)
    }
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.inner.env_change(change)
    }
    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.inner.return_value(name, ty, value)
    }
    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.inner.return_status(status)
    }
}

fn test_resolver() -> Arc<SystemProcedureResolver> {
    fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>> {
        let base = name.rsplit('.').next().unwrap_or(name);
        if base.eq_ignore_ascii_case("sp_executesql") {
            Some(resolve_executesql(args))
        } else {
            None
        }
    }
    Arc::new(resolve)
}

fn resolve_executesql(args: &[ProcArg<'_>]) -> Result<ProcAction, SqlError> {
    let statement = args
        .iter()
        .find_map(|arg| {
            let named = matches!(
                arg.name,
                None | Some("@statement") | Some("@stmt") | Some("stmt")
            );
            if !named {
                return None;
            }
            match arg.value {
                Value::String(text) => Some(text.text.clone()),
                _ => None,
            }
        })
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_executesql", "@statement"))?;
    let params_text = args
        .iter()
        .find_map(|arg| match (arg.name, arg.value) {
            (Some("@params") | Some("params"), Value::String(text)) => Some(text.text.clone()),
            (None, Value::String(text)) if text.text != statement => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let declarations =
        parse_parameter_declarations(&params_text, &ParseOptions::default()).unwrap_or_default();
    let mut bound = vec![None; declarations.len()];
    let mut positional = 0usize;
    for arg in args {
        if let Some(name) = arg.name {
            if name == "@statement" || name == "@stmt" || name == "@params" || name == "params" {
                continue;
            }
            if let Some(index) = declarations
                .iter()
                .position(|decl| decl.name.eq_ignore_ascii_case(name))
            {
                bound[index] = Some(ProcParam {
                    name: declarations[index].name.clone(),
                    ty: arg.ty.clone(),
                    value: arg.value.clone(),
                    output: arg.output,
                });
            }
        } else if matches!(arg.value, Value::String(_)) {
            continue;
        } else if positional < declarations.len() {
            bound[positional] = Some(ProcParam {
                name: declarations[positional].name.clone(),
                ty: arg.ty.clone(),
                value: arg.value.clone(),
                output: arg.output,
            });
            positional += 1;
        }
    }
    Ok(ProcAction::ExecuteSql {
        statement,
        params: bound.into_iter().flatten().collect(),
    })
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    Session::new(
        Arc::new(Engine::with_system_procedures(
            Arc::new(MemoryStorage::new()),
            test_resolver(),
        )),
        SessionState::new(SPID),
    )
}

fn int_param(name: &str, value: i32) -> NestedParam {
    NestedParam {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, true),
        value: Value::I32(value),
        output: false,
    }
}

fn run_nested(
    session: &mut Session,
    text: &str,
    params: &[NestedParam],
) -> (NestedOutcome, Recording) {
    let mut sink = Recording::default();
    let outcome = session
        .run_nested(text, params, &mut sink)
        .expect("run_nested reports client errors through the sink");
    (outcome, sink)
}

fn only_row(events: &[Event]) -> i32 {
    events
        .iter()
        .find_map(|event| match event {
            Event::Row(row) => match row.first() {
                Some(Value::I32(n)) => Some(*n),
                _ => None,
            },
            _ => None,
        })
        .expect("one row with an int")
}

fn only_error(events: &[Event]) -> SqlError {
    events
        .iter()
        .find_map(|event| match event {
            Event::Error(err) => Some(err.clone()),
            _ => None,
        })
        .expect("one error")
}

/// Every error number the events carry, in order.
fn error_numbers(events: &[Event]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some(err.number),
            _ => None,
        })
        .collect()
}

/// `true` once a row whose first column is the int `n` appears.
fn has_int_row(events: &[Event], n: i32) -> bool {
    events
        .iter()
        .any(|event| matches!(event, Event::Row(row) if row.first() == Some(&Value::I32(n))))
}

/// The last row of the events, as plain values.
fn last_row(events: &[Event]) -> &[Value] {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::Row(row) => Some(row.as_slice()),
            _ => None,
        })
        .expect("at least one row")
}

fn run_batch(session: &mut Session, text: &str) -> Recording {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("run_batch reports client errors through the sink");
    sink
}

#[test]
fn parameter_is_visible_in_the_text() {
    let (outcome, events) = run_nested(&mut session(), "SELECT @a + 1", &[int_param("@a", 41)]);
    assert!(!outcome.failed);
    assert_eq!(only_row(&events.0), 42);
}

#[test]
fn caller_variables_are_out_of_scope() {
    let mut session = session();
    session
        .run_batch("DECLARE @v int = 1", &mut Recording::default())
        .expect("declare runs");
    let mut sink = Recording::default();
    let outcome = session
        .run_nested("SELECT @v", &[], &mut sink)
        .expect("nested completes");
    assert!(outcome.failed);
    let err = only_error(&sink.0);
    assert_eq!(err.number, 137);
}

#[test]
fn nested_exec_sp_executesql_writes_output() {
    let (outcome, events) = run_nested(
        &mut session(),
        "EXEC sp_executesql N'SET @o = 7', N'@o int OUTPUT', @o = @v OUTPUT; SELECT @v",
        &[NestedParam {
            name: "@v".into(),
            ty: TypeInfo::new(SqlType::Int, true),
            value: Value::Null,
            output: true,
        }],
    );
    assert!(!outcome.failed, "{:?}", events.0);
    assert_eq!(outcome.outputs, vec![("@v".to_owned(), Value::I32(7))]);
    assert_eq!(only_row(&events.0), 7);
    assert!(
        !events.0.iter().any(|event| matches!(
            event,
            Event::Error(err) if err.number == 50000
                && err.message.contains("does not handle")
        )),
        "counter-proof: no internal does-not-handle error, got {:?}",
        events.0
    );
}

#[test]
fn output_parameter_is_written_back() {
    let (outcome, _) = run_nested(
        &mut session(),
        "SET @o = 7",
        &[NestedParam {
            name: "@o".into(),
            ty: TypeInfo::new(SqlType::Int, true),
            value: Value::Null,
            output: true,
        }],
    );
    assert!(!outcome.failed);
    assert_eq!(outcome.outputs, vec![("@o".to_owned(), Value::I32(7))]);
}

#[test]
fn three_statements_give_three_done_in_proc_and_one_done_proc() {
    let mut session = session();
    let mut sink = CountingSink::new();
    session
        .run_nested("SELECT 1; SELECT 2; SELECT 3", &[], &mut sink)
        .expect("nested completes");
    assert_eq!(sink.counts.done_in_proc, 3);
    assert_eq!(sink.counts.done_proc, 1);
    assert_eq!(sink.counts.done, 0);

    let mut batch_sink = CountingSink::new();
    session
        .run_batch("SELECT 1; SELECT 2; SELECT 3", &mut batch_sink)
        .expect("batch completes");
    assert_eq!(batch_sink.counts.done, 3);
    assert_eq!(batch_sink.counts.done_in_proc, 0);
    assert_eq!(batch_sink.counts.done_proc, 0);
}

#[test]
fn error_line_is_relative_to_the_nested_text() {
    let text = "SELECT 1\nSELECT 1/0";
    let mut sink = Recording::default();
    let outcome = session()
        .run_nested(text, &[], &mut sink)
        .expect("nested completes");
    assert!(outcome.failed);
    let err = only_error(&sink.0);
    assert_eq!(err.number, 8134);
    assert_eq!(err.line, 2);
}

/// A `DECLARE` of a type the binder does not know fails while the nested text is bound,
/// and the number leaves on the line the `DECLARE` was written on, not on the first line
/// of that text.
#[test]
fn a_binding_error_of_a_declare_carries_the_line_of_its_statement() {
    let text = "SELECT 1;\nDECLARE @x nosuchtype;";
    let mut sink = Recording::default();
    let outcome = session()
        .run_nested(text, &[], &mut sink)
        .expect("nested completes");
    assert!(outcome.failed);
    let err = only_error(&sink.0);
    assert_eq!(err.number, 2715);
    assert_eq!(err.line, 2);
}

/// What a call raises is located in the text the call ran: an `EXEC sp_executesql` on
/// the third line of the text, whose parameterized text reads a parameter the call
/// leaves unbound, answers on line **1**, the first line of the text the call ran, and
/// not on the line the `EXEC` sits on.
#[test]
fn an_error_of_a_call_keeps_the_line_of_the_text_it_ran() {
    let text = "SELECT 1;\n\nEXEC sp_executesql N'SELECT @a AS n', N'@a int';";
    let mut sink = Recording::default();
    let outcome = session()
        .run_nested(text, &[], &mut sink)
        .expect("nested completes");
    assert!(outcome.failed);
    let err = only_error(&sink.0);
    assert_eq!(err.number, 137);
    assert_eq!(err.line, 1);
}

/// `@@ROWCOUNT` at the end of the call is the row count of the nested text's own last
/// statement: one row, no row, or three, not the `0` a fresh statement starts on.
#[test]
fn rowcount_reflects_the_last_row_count_of_the_nested_text() {
    let (one, _) = run_nested(&mut session(), "SELECT 1 AS n", &[]);
    assert_eq!(one.rowcount, 1, "{one:?}");

    let (none, _) = run_nested(&mut session(), "SELECT 1 AS n WHERE 1 = 0", &[]);
    assert_eq!(none.rowcount, 0, "{none:?}");

    let (three, _) = run_nested(
        &mut session(),
        "CREATE TABLE dbo.n5 (n int); INSERT dbo.n5 VALUES (1), (2), (3); SELECT n FROM dbo.n5;",
        &[],
    );
    assert_eq!(three.rowcount, 3, "{three:?}");
}

/// A text that ends with `RETURN` leaves `@@ROWCOUNT` at 0: the `RETURN` is the last statement
/// that ran, and it counts no row, whatever the `SELECT` before it counted.
#[test]
fn rowcount_is_zero_after_a_text_that_ends_with_return() {
    let (bare, _) = run_nested(&mut session(), "RETURN", &[]);
    assert_eq!(bare.rowcount, 0, "{bare:?}");

    let (after_rows, _) = run_nested(
        &mut session(),
        "CREATE TABLE dbo.n7 (n int); INSERT dbo.n7 VALUES (1), (2), (3); SELECT n FROM dbo.n7; RETURN;",
        &[],
    );
    assert_eq!(after_rows.rowcount, 0, "{after_rows:?}");
    assert_eq!(after_rows.return_status, 0, "{after_rows:?}");
}

/// A statement that never runs because its text failed to compile leaves `@@ROWCOUNT` at
/// whatever the caller already held; a statement that started running and then raised a
/// run-time error resets it to `0` instead.
#[test]
fn rowcount_is_zero_after_a_runtime_error_and_unchanged_after_a_compile_error() {
    let seed = "CREATE TABLE dbo.n6 (n int); INSERT dbo.n6 VALUES (1), (2), (3);";

    let mut runtime = session();
    run_batch(&mut runtime, seed);
    assert_eq!(runtime.state().rowcount, 3);
    let (after_runtime_error, _) = run_nested(&mut runtime, "SELECT 1/0", &[]);
    assert!(after_runtime_error.failed);
    assert_eq!(after_runtime_error.rowcount, 0, "{after_runtime_error:?}");

    let mut compile = session();
    run_batch(&mut compile, seed);
    assert_eq!(compile.state().rowcount, 3);
    let (after_compile_error, _) = run_nested(&mut compile, "SELECT @nope", &[]);
    assert!(after_compile_error.failed);
    assert_eq!(after_compile_error.rowcount, 3, "{after_compile_error:?}");
}

/// A run-time error that stops the whole nested text leaves its own number as the return
/// status of the call, read back by the caller through `EXEC @r = sp_executesql`.
#[test]
fn return_status_is_the_error_number_after_a_runtime_error() {
    let (outcome, _) = run_nested(&mut session(), "SELECT 1/0", &[]);
    assert!(outcome.failed);
    assert_eq!(outcome.return_status, 8134);
}

/// A text that never compiles answers the same way: the return status is the number of the
/// error that kept it from running, not `0`.
#[test]
fn return_status_is_the_error_number_after_a_compile_error() {
    let (outcome, _) = run_nested(&mut session(), "SELECT @nope", &[]);
    assert!(outcome.failed);
    assert_eq!(outcome.return_status, 137);
}

/// A statement-scoped error the text recovers from does not leave its number behind: once
/// a later statement of the same text runs to completion, the return status reads `0`, as
/// if the error had not happened.
#[test]
fn return_status_is_zero_after_a_statement_scoped_error_the_text_recovers_from() {
    let (outcome, events) = run_nested(&mut session(), "SELECT 1/0; SELECT 5;", &[]);
    assert_eq!(error_numbers(&events.0), vec![8134]);
    assert_eq!(outcome.return_status, 0, "{outcome:?}");
}

/// A nested text that never compiles because of a syntax error still lets the caller batch
/// run its next statement, the same family as `a_208_in_an_exec_lets_the_caller_continue`:
/// nothing of the text ran, so nothing there needed the caller to stop.
#[test]
fn a_156_syntax_error_in_a_nested_text_lets_the_caller_continue() {
    let events = run_batch(
        &mut session(),
        "DECLARE @r int; EXEC @r = sp_executesql N'SELECT FROM', N''; SELECT @r AS n;",
    );
    assert_eq!(only_row(&events.0), 156);
}

/// A nested text is a batch, not a procedure: `RETURN` with a value inside one fails to
/// compile (178), and the caller still runs its next statement.
#[test]
fn a_178_return_with_a_value_in_a_nested_text_lets_the_caller_continue() {
    let events = run_batch(
        &mut session(),
        "DECLARE @r int; EXEC @r = sp_executesql N'RETURN 5', N''; SELECT @r AS n;",
    );
    assert_eq!(only_row(&events.0), 178);
}

/// A text whose only statement is a call to another `sp_executesql` that fails leaves that
/// call's own status as its own: `outcome.return_status` used to stay at its default `0`
/// (`finish_nested_procedure_flow` copied `rowcount` from the inner call but not
/// `return_status`), even though the caller already read the right `@@ROWCOUNT` and the
/// right error.
#[test]
fn return_status_of_a_two_level_call_propagates_up() {
    let (outcome, _) = run_nested(&mut session(), "EXEC sp_executesql N'SELECT 1/0', N''", &[]);
    assert!(outcome.failed);
    assert_eq!(outcome.return_status, 8134, "{outcome:?}");
}

/// The same failing call, followed by a statement of the outer text that runs to
/// completion: the final status is `0`, as if the inner failure had not happened, the same
/// rule `return_status_is_zero_after_a_statement_scoped_error_the_text_recovers_from`
/// applies to a plain statement.
#[test]
fn return_status_of_a_two_level_call_is_zero_when_the_text_recovers_after_it() {
    let (outcome, events) = run_nested(
        &mut session(),
        "EXEC sp_executesql N'SELECT 1/0', N''; SELECT 5;",
        &[],
    );
    assert_eq!(error_numbers(&events.0), vec![8134]);
    assert_eq!(outcome.return_status, 0, "{outcome:?}");
}

#[test]
fn empty_text_emits_done_proc_and_return_status() {
    let mut sink = Recording::default();
    let outcome = session()
        .run_nested("", &[], &mut sink)
        .expect("empty nested completes");
    assert!(!outcome.failed);
    assert_eq!(outcome.return_status, 0);
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::ReturnStatus(0)))
    );
    assert_eq!(
        sink.0
            .iter()
            .filter(|event| matches!(event, Event::DoneInProc(_, _)))
            .count(),
        0
    );
    assert_eq!(
        sink.0
            .iter()
            .filter(|event| matches!(event, Event::DoneProc(_)))
            .count(),
        1
    );
}

#[test]
fn two_level_nesting_keeps_scopes_separate() {
    let mut session = session();
    let (outer, _) = run_nested(&mut session, "SELECT @a + 1", &[int_param("@a", 1)]);
    assert!(!outer.failed);

    let mut inner_sink = Recording::default();
    let inner = session
        .run_nested("SELECT @a + 1", &[int_param("@a", 2)], &mut inner_sink)
        .expect("inner nested completes");
    assert!(!inner.failed);
    assert_eq!(only_row(&inner_sink.0), 3);

    let mut missing_sink = Recording::default();
    let missing = session
        .run_nested("SELECT @a", &[], &mut missing_sink)
        .expect("missing param nested completes");
    assert!(missing.failed);
    assert_eq!(only_error(&missing_sink.0).number, 137);
}

/// A table created earlier in the nested text is read by the statements after it: the
/// `INSERT` and the `SELECT` are deferred when the text is bound and resolved when the text
/// reaches them. Three DONEINPROC and one row.
#[test]
fn a_table_created_earlier_in_the_nested_text_is_read_by_the_rest_of_it() {
    let (outcome, events) = run_nested(
        &mut session(),
        "CREATE TABLE dbo.n1 (n int); INSERT dbo.n1 VALUES (7); SELECT n FROM dbo.n1;",
        &[],
    );
    assert!(!outcome.failed, "{:?}", events.0);
    assert_eq!(only_row(&events.0), 7);
    assert_eq!(
        events
            .0
            .iter()
            .filter(|event| matches!(event, Event::DoneInProc(_, _)))
            .count(),
        3,
        "{:?}",
        events.0
    );
}

/// The counter-proof: the same `SELECT` written before the `CREATE TABLE` answers 208 when
/// the text reaches it, on its own line, after the row of the `SELECT 1`; the text stops
/// there.
#[test]
fn a_select_before_the_create_in_a_nested_text_is_208_at_run_time() {
    let (outcome, events) = run_nested(
        &mut session(),
        "SELECT 1;\nSELECT * FROM dbo.n2;\nCREATE TABLE dbo.n2 (a int);\nSELECT 2;",
        &[],
    );
    assert!(outcome.failed);
    let err = only_error(&events.0);
    assert_eq!(err.number, 208);
    assert_eq!(err.line, 2);
    let rows: Vec<&Event> = events
        .0
        .iter()
        .filter(|event| matches!(event, Event::Row(_)))
        .collect();
    assert_eq!(rows.len(), 1, "{:?}", events.0);
    assert_eq!(only_row(&events.0), 1);
}

/// Through `sp_executesql` from a batch: the nested text creates, inserts and reads in one
/// go, and a table the nested text created is read by the calling batch afterwards.
#[test]
fn a_table_created_by_sp_executesql_is_read_by_the_calling_batch() {
    let mut session = session();
    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC sp_executesql N'CREATE TABLE dbo.n3 (n int); INSERT dbo.n3 VALUES (9); SELECT n FROM dbo.n3;';",
            &mut sink,
        )
        .expect("the batch completes");
    assert!(
        !sink.0.iter().any(|event| matches!(event, Event::Error(_))),
        "{:?}",
        sink.0
    );
    assert_eq!(only_row(&sink.0), 9);

    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC sp_executesql N'CREATE TABLE dbo.n4 (n int)'; INSERT dbo.n4 VALUES (4); SELECT n FROM dbo.n4;",
            &mut sink,
        )
        .expect("the batch completes");
    assert!(
        !sink.0.iter().any(|event| matches!(event, Event::Error(_))),
        "{:?}",
        sink.0
    );
    assert_eq!(only_row(&sink.0), 4);
}

/// The batch that issued `EXEC('…')` goes on past a `208` its nested text raised: only the
/// nested text stops at the missing name, not the batch that called it.
#[test]
fn a_208_in_an_exec_lets_the_caller_continue() {
    let events = run_batch(
        &mut session(),
        "EXEC('SELECT 1; SELECT * FROM dbo.nested_missing; SELECT 2;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![208]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// An unknown column of a table that already exists is a compile error for the whole
/// nested text (its `SELECT 1` never runs), and the caller still goes on afterward.
#[test]
fn a_207_compile_error_in_an_exec_lets_the_caller_continue() {
    let mut session = session();
    run_batch(&mut session, "CREATE TABLE dbo.nested_pre (a int);");
    let events = run_batch(
        &mut session,
        "EXEC('SELECT 1; SELECT nosuch FROM dbo.nested_pre; SELECT 2;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![207]);
    assert!(!has_int_row(&events.0, 1), "{:?}", events.0);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// A missing procedure called from inside an `EXEC('…')` also lets the caller continue.
#[test]
fn a_2812_in_an_exec_lets_the_caller_continue() {
    let events = run_batch(
        &mut session(),
        "EXEC('EXEC dbo.nested_no_such_proc;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![2812]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// 2812 stops the `EXEC` that names the missing procedure, and nothing more: the next
/// statement of the batch runs.
#[test]
fn a_2812_lets_the_batch_go_on() {
    let events = run_batch(
        &mut session(),
        "SELECT 1; EXEC dbo.nested_no_such_proc; SELECT 2;",
    );
    assert_eq!(error_numbers(&events.0), vec![2812]);
    assert!(has_int_row(&events.0, 2), "{:?}", events.0);
}

/// Inside a nested text too, 2812 stops its own statement: the text goes on, then the
/// caller.
#[test]
fn a_2812_inside_an_exec_lets_the_text_go_on() {
    let events = run_batch(
        &mut session(),
        "EXEC('EXEC dbo.nested_no_such_proc; SELECT 2;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![2812]);
    assert!(has_int_row(&events.0, 2), "{:?}", events.0);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// A syntax error stops the nested text it was found in, and the caller goes on: 105 is
/// severity 15, whose errors stop their level.
#[test]
fn a_105_in_an_exec_lets_the_caller_continue() {
    let events = run_batch(&mut session(), "EXEC('SELECT ''abc'); SELECT 3;");
    assert_eq!(error_numbers(&events.0), vec![105]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// `ALTER TABLE` on a table that does not exist stops its nested text, like 208, and the
/// caller goes on.
#[test]
fn a_4902_in_an_exec_lets_the_caller_continue() {
    let events = run_batch(
        &mut session(),
        "EXEC('ALTER TABLE dbo.nested_missing ADD b int;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![4902]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// The other side of the two tests above: a negative row count read at run time (127) and
/// an index on a table that does not exist (1088) stop the caller as well, although their
/// severity is that of the syntax errors.
#[test]
fn a_127_or_a_1088_in_an_exec_stops_the_caller() {
    for (text, number) in [
        (
            "EXEC('DECLARE @n int = -1; SELECT TOP (@n) 1;'); SELECT 3;",
            127,
        ),
        (
            "EXEC('CREATE INDEX ix ON dbo.nested_missing (a);'); SELECT 3;",
            1088,
        ),
    ] {
        let events = run_batch(&mut session(), text);
        assert_eq!(error_numbers(&events.0), vec![number], "{text}");
        assert!(!has_int_row(&events.0, 3), "{text}: {:?}", events.0);
    }
}

/// `8134` only stops the statement that raised it: the nested text goes on to its own next
/// statement, which succeeds, so the caller sees the call as a whole succeed, and `@@ERROR`
/// right after the call is `0`, not the division by zero in the middle of the text.
#[test]
fn an_8134_in_an_exec_lets_the_caller_continue_and_error_is_cleared_after() {
    let events = run_batch(
        &mut session(),
        "EXEC('SELECT 1; SELECT 1/0; SELECT 2;'); SELECT @@ERROR;",
    );
    assert_eq!(error_numbers(&events.0), vec![8134]);
    assert_eq!(last_row(&events.0), &[Value::I32(0)]);
}

/// A `NOT NULL` violation inside an `EXEC('…')` lets the caller continue.
#[test]
fn a_515_in_an_exec_lets_the_caller_continue() {
    let mut session = session();
    run_batch(
        &mut session,
        "CREATE TABLE dbo.nested_notnull (a int NOT NULL);",
    );
    let events = run_batch(
        &mut session,
        "EXEC('INSERT dbo.nested_notnull (a) VALUES (NULL);'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![515]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// A foreign-key violation inside an `EXEC('…')` lets the caller continue.
#[test]
fn a_547_in_an_exec_lets_the_caller_continue() {
    let mut session = session();
    run_batch(
        &mut session,
        "CREATE TABLE dbo.nested_parent (a int PRIMARY KEY); \
         CREATE TABLE dbo.nested_child (a int REFERENCES dbo.nested_parent(a));",
    );
    let events = run_batch(
        &mut session,
        "EXEC('INSERT dbo.nested_child (a) VALUES (9);'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![547]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// A unique-key violation inside an `EXEC('…')` lets the caller continue.
#[test]
fn a_2627_in_an_exec_lets_the_caller_continue() {
    let mut session = session();
    run_batch(
        &mut session,
        "CREATE TABLE dbo.nested_uniq (a int UNIQUE); INSERT dbo.nested_uniq (a) VALUES (1);",
    );
    let events = run_batch(
        &mut session,
        "EXEC('INSERT dbo.nested_uniq (a) VALUES (1);'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![2627]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// `515` is statement-scoped: the nested text goes on to its own next statement too, next
/// to the caller. `@@ERROR` right after the call is `0`, cleared by that statement, not
/// the `NOT NULL` violation in the middle of the text (complements
/// `a_515_in_an_exec_lets_the_caller_continue`, which checks the caller alone).
#[test]
fn a_515_in_an_exec_lets_the_nested_texts_own_next_statement_run_too() {
    let mut session = session();
    run_batch(
        &mut session,
        "CREATE TABLE dbo.nested_notnull_own (a int NOT NULL);",
    );
    let events = run_batch(
        &mut session,
        "EXEC('SELECT 1; INSERT dbo.nested_notnull_own (a) VALUES (NULL); SELECT 2;'); \
         SELECT @@ERROR;",
    );
    assert_eq!(error_numbers(&events.0), vec![515]);
    assert!(has_int_row(&events.0, 1), "{:?}", events.0);
    assert!(has_int_row(&events.0, 2), "{:?}", events.0);
    assert_eq!(last_row(&events.0), &[Value::I32(0)], "{:?}", events.0);
}

/// One of a family of checked exceptions: a conversion error raised while a nested statement
/// runs ends the caller's batch too, unlike the numbers checked above, even without
/// `XACT_ABORT`.
#[test]
fn a_245_in_an_exec_also_ends_the_caller_batch() {
    let events = run_batch(
        &mut session(),
        "EXEC('SELECT 1; SELECT CAST(''abc'' AS int); SELECT 2;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![245]);
    assert!(!has_int_row(&events.0, 3), "{:?}", events.0);
}

/// Same family as `245`: a date conversion that fails while a nested statement runs.
#[test]
fn a_241_in_an_exec_also_ends_the_caller_batch() {
    let events = run_batch(
        &mut session(),
        "EXEC('SELECT 1; SELECT CAST(''zz'' AS datetime); SELECT 2;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![241]);
    assert!(!has_int_row(&events.0, 3), "{:?}", events.0);
}

/// Same family as `245`: a numeric conversion that fails while a nested statement runs.
#[test]
fn an_8114_in_an_exec_also_ends_the_caller_batch() {
    let events = run_batch(
        &mut session(),
        "EXEC('SELECT 1; SELECT CAST(''abc'' AS decimal(5,2)); SELECT 2;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![8114]);
    assert!(!has_int_row(&events.0, 3), "{:?}", events.0);
}

/// Counter-proof: an operand type clash does not join the family above on every shape.
/// `NEWID() + 1` fails to compile the whole nested text (no row from its own `SELECT 1`),
/// like `207`, and the caller still continues on this one.
#[test]
fn a_206_operand_type_clash_lets_the_caller_continue_on_this_shape() {
    let events = run_batch(
        &mut session(),
        "EXEC('SELECT 1; SELECT NEWID() + 1; SELECT 2;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![206]);
    assert!(!has_int_row(&events.0, 1), "{:?}", events.0);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// Same family as `245`: an object that already exists, found while a nested statement of
/// `EXEC('…')` runs.
#[test]
fn a_2714_in_an_exec_also_ends_the_caller_batch() {
    let mut session = session();
    run_batch(&mut session, "CREATE TABLE dbo.nested_dup (x int);");
    let events = run_batch(
        &mut session,
        "EXEC('CREATE TABLE dbo.nested_dup (x int);'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![2714]);
    assert!(!has_int_row(&events.0, 3), "{:?}", events.0);
}

/// `XACT_ABORT ON` overrides the default: the caller stops after the `EXEC('…')` fails, even
/// on a number (`208`) that lets it continue when `XACT_ABORT` is `OFF`.
#[test]
fn xact_abort_on_ends_the_caller_batch_even_after_a_208() {
    let events = run_batch(
        &mut session(),
        "SET XACT_ABORT ON; EXEC('SELECT 1; SELECT * FROM dbo.nested_xact; SELECT 2;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![208]);
    assert!(!has_int_row(&events.0, 3), "{:?}", events.0);
}

/// The same rule one level deeper: a `sp_executesql` called from inside a nested text lets
/// that nested text continue to its own next statement after a `208`.
#[test]
fn a_208_from_sp_executesql_inside_a_nested_text_lets_it_continue() {
    let (outcome, events) = run_nested(
        &mut session(),
        "EXEC sp_executesql N'SELECT 1; SELECT * FROM dbo.nested_deep; SELECT 2;'; SELECT 30;",
        &[],
    );
    assert!(outcome.failed);
    assert_eq!(error_numbers(&events.0), vec![208]);
    assert!(has_int_row(&events.0, 30), "{:?}", events.0);
}

/// A missing procedure found by an `sp_executesql` called from inside an `EXEC('…')` lets
/// both the nested text (its own `SELECT 22`) and the caller continue: no `DONEINPROC`
/// carrying `more = false` goes out for the failing `EXEC dbo.nope;`, which a client can
/// read as the end of the whole response and stop there, short of the caller's own rows.
#[test]
fn a_2812_from_sp_executesql_inside_an_exec_lets_both_levels_continue() {
    let events = run_batch(
        &mut session(),
        "EXEC('EXEC sp_executesql N''EXEC dbo.nope;''; SELECT 22;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![2812]);
    assert!(has_int_row(&events.0, 22), "{:?}", events.0);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
    assert!(
        !events
            .0
            .iter()
            .any(|event| matches!(event, Event::DoneInProc(_, false))),
        "{:?}",
        events.0
    );
}

/// An open transaction around the `EXEC('…')` does not change the rule: the caller still
/// continues past the nested `208`, and the transaction commits.
#[test]
fn a_208_in_an_exec_inside_an_open_transaction_lets_the_caller_continue() {
    let events = run_batch(
        &mut session(),
        "BEGIN TRAN; EXEC('SELECT * FROM dbo.nested_tx;'); SELECT 3; COMMIT;",
    );
    assert_eq!(error_numbers(&events.0), vec![208]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

fn minimal_login_tokens() -> Vec<Token> {
    vec![
        Token::EnvChange(EnvChange::Database {
            old: "master".into(),
            new: "master".into(),
        }),
        Token::EnvChange(EnvChange::PacketSize {
            old: 4096,
            new: 4096,
        }),
        Token::LoginAck {
            tds_version: 0x7400_0004,
            program_name: "VaubanDB".into(),
            version: [0x10, 0x00, 0x03, 0xE8],
        },
        Token::Done {
            status: DoneStatus::FINAL,
            cur_cmd: 0,
            row_count: None,
        },
    ]
}

async fn serve_one_nested_query(listener: TcpListener) {
    let (tcp, _) = listener.accept().await.unwrap();
    let mut stream = TdsStream::accept(tcp, None, EncryptPolicy::Off)
        .await
        .expect("handshake");
    match stream.read_message().await.expect("login read") {
        ClientMessage::Login7(_) => {}
        other => panic!("unexpected pre-login message: {other:?}"),
    }
    stream.write_tokens(&minimal_login_tokens()).await.unwrap();
    stream.flush().await.unwrap();

    let (reader, mut writer) = stream.split();
    let (msg_tx, mut msg_rx) = mpsc::channel(8);
    tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match reader.read_message().await {
                Ok(ClientMessage::SqlBatch(_)) => {
                    let _ = msg_tx.send(()).await;
                    break;
                }
                Ok(ClientMessage::Attention) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    msg_rx.recv().await.unwrap();

    let (token_tx, mut token_rx) = mpsc::channel(32);
    let engine = Arc::new(Engine::with_system_procedures(
        Arc::new(MemoryStorage::new()),
        test_resolver(),
    ));
    let handle = spawn_blocking(move || {
        vauban_sysfn::register_builtins();
        let mut session = Session::new(engine, SessionState::new(SPID));
        let mut sink = TdsSink::new(token_tx);
        session
            .run_nested(
                "SELECT @a + 1",
                &[NestedParam {
                    name: "@a".into(),
                    ty: TypeInfo::new(SqlType::Int, true),
                    value: Value::I32(41),
                    output: false,
                }],
                &mut sink,
            )
            .expect("nested on wire");
    });

    while let Some(token) = token_rx.recv().await {
        writer.write_tokens(&[token]).await.unwrap();
    }
    writer.flush().await.unwrap();
    let _ = handle.await;
}

#[tokio::test]
#[ignore = "network integration test with a real tiberius client"]
async fn tiberius_reads_run_nested_doneinproc_and_doneproc() {
    use tiberius::{AuthMethod, Client, Config, EncryptionLevel};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_one_nested_query(listener));

    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(addr.port());
    config.authentication(AuthMethod::sql_server("sa", "ignored"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();

    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut client = Client::connect(config, tcp.compat_write())
        .await
        .expect("tiberius connects");

    let row = timeout(Duration::from_secs(10), client.simple_query("SELECT 1"))
        .await
        .expect("query finishes")
        .expect("query succeeds")
        .into_row()
        .await
        .expect("row read")
        .expect("one row returned");
    assert_eq!(row.get::<i32, _>(0), Some(42));
}

/// The rule this project applies to decide whether the caller of an `EXEC('…')` goes on: an
/// error raised while a statement of the nested text *runs* stops the caller exactly when
/// its catalogued scope (`vauban_errors::error_behavior`) stops the batch that raised it.
/// Parameterized over the numbers this project checks that scope on.
#[test]
fn nested_batch_scope_family_decides_caller_continuation() {
    let stops: &[(&str, u32)] = &[
        ("SELECT CAST(''abc'' AS uniqueidentifier)", 8169),
        ("SELECT DATEFROMPARTS(2020, 13, 1)", 289),
        ("SELECT CAST(''zz'' AS smalldatetime)", 295),
        ("SELECT CAST(''abc'' AS money)", 235),
        ("SELECT CAST(NEWID() AS char(3))", 8170),
        ("SELECT CONVERT(varchar(30), GETDATE(), 999)", 281),
        ("SELECT CAST($12345678 AS varchar(3))", 234),
    ];
    for (stmt, number) in stops {
        let text = format!("EXEC('SELECT 1; {stmt}; SELECT 2;'); SELECT 3;");
        let events = run_batch(&mut session(), &text);
        assert_eq!(
            error_numbers(&events.0),
            vec![*number],
            "{stmt}: {:?}",
            events.0
        );
        assert!(
            !has_int_row(&events.0, 3),
            "{stmt} is batch-scoped, should stop the caller: {:?}",
            events.0
        );
    }

    let continues: &[String] = &[
        "EXEC('SELECT 1; SELECT 1/0; SELECT 2;'); SELECT 3;".to_owned(),
        "EXEC('SELECT 1; SELECT CAST(100000 AS smallint); SELECT 2;'); SELECT 3;".to_owned(),
    ];
    for text in continues {
        let events = run_batch(&mut session(), text);
        assert!(
            has_int_row(&events.0, 3),
            "statement-scoped, should let the caller continue: {:?}",
            events.0
        );
    }

    {
        let mut nn_session = session();
        run_batch(
            &mut nn_session,
            "CREATE TABLE dbo.nested_scope_nn (a int NOT NULL);",
        );
        let events = run_batch(
            &mut nn_session,
            "EXEC('INSERT dbo.nested_scope_nn (a) VALUES (NULL);'); SELECT 3;",
        );
        assert_eq!(error_numbers(&events.0), vec![515]);
        assert!(has_int_row(&events.0, 3), "{:?}", events.0);
    }

    {
        let mut fk_session = session();
        run_batch(
            &mut fk_session,
            "CREATE TABLE dbo.nested_scope_parent (a int PRIMARY KEY); \
             CREATE TABLE dbo.nested_scope_child (a int REFERENCES dbo.nested_scope_parent(a));",
        );
        let events = run_batch(
            &mut fk_session,
            "EXEC('INSERT dbo.nested_scope_child (a) VALUES (9);'); SELECT 3;",
        );
        assert_eq!(error_numbers(&events.0), vec![547]);
        assert!(has_int_row(&events.0, 3), "{:?}", events.0);
    }

    {
        let mut uniq_session = session();
        run_batch(
            &mut uniq_session,
            "CREATE TABLE dbo.nested_scope_uniq (a int UNIQUE); \
             INSERT dbo.nested_scope_uniq (a) VALUES (1);",
        );
        let events = run_batch(
            &mut uniq_session,
            "EXEC('INSERT dbo.nested_scope_uniq (a) VALUES (1);'); SELECT 3;",
        );
        assert_eq!(error_numbers(&events.0), vec![2627]);
        assert!(has_int_row(&events.0, 3), "{:?}", events.0);
    }

    let events = run_batch(
        &mut session(),
        "EXEC('DROP TABLE dbo.nested_scope_no_such_table;'); SELECT 3;",
    );
    assert_eq!(error_numbers(&events.0), vec![3701]);
    assert!(has_int_row(&events.0, 3), "{:?}", events.0);
}

/// The `more` flag of every DONEINPROC of the events, in order.
fn done_in_proc_more(events: &[Event]) -> Vec<bool> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::DoneInProc(_, more) => Some(*more),
            _ => None,
        })
        .collect()
}

/// A call made from a text that another call runs: the token that closes the inner call is
/// a DONEINPROC with `more`, like the one that closes the `SELECT` it ran, and the batch
/// plays the statement that follows the outer call.
#[test]
fn the_token_that_closes_a_two_level_call_carries_more() {
    let events = run_batch(
        &mut session(),
        "EXEC sp_executesql N'EXEC sp_executesql N''SELECT 1 AS n''';\nSELECT 13 AS alive;",
    );
    assert_eq!(
        done_in_proc_more(&events.0),
        vec![true, true],
        "{:?}",
        events.0
    );
    assert!(has_int_row(&events.0, 13), "{:?}", events.0);
}

/// The same two levels with nothing after the outer call: the token that closes the inner
/// call still carries `more`, since the outer call has its own closing token to send, and
/// that last one carries none.
#[test]
fn the_token_that_closes_a_call_at_the_end_of_a_batch_carries_more() {
    let events = run_batch(
        &mut session(),
        "EXEC sp_executesql N'EXEC sp_executesql N''SELECT 1 AS n''';",
    );
    assert_eq!(
        done_in_proc_more(&events.0),
        vec![true, true],
        "{:?}",
        events.0
    );
    assert_eq!(
        events.0.last(),
        Some(&Event::Done(None, false)),
        "{:?}",
        events.0
    );
}

/// Three levels of `sp_executesql`: the two tokens that close the two inner calls carry
/// `more`, and the batch plays the statement that follows the outermost call.
#[test]
fn the_tokens_that_close_a_three_level_call_carry_more() {
    let events = run_batch(
        &mut session(),
        "EXEC sp_executesql N'EXEC sp_executesql N''EXEC sp_executesql N''''SELECT 1 AS n'''''''\
         ;\nSELECT 13 AS alive;",
    );
    assert_eq!(
        done_in_proc_more(&events.0),
        vec![true, true, true],
        "{:?}",
        events.0
    );
    assert!(has_int_row(&events.0, 13), "{:?}", events.0);
}

/// A two level call whose deepest text raises 8134: the failed statement ends with its
/// DONEINPROC, since an error of its statement lets its level go on, and the token that
/// closes the failed call carries `more` too; the batch plays the statement that follows
/// the outer call.
#[test]
fn the_token_that_closes_a_failed_two_level_call_carries_more() {
    let events = run_batch(
        &mut session(),
        "EXEC sp_executesql N'EXEC sp_executesql N''SELECT 1/0 AS n''';\nSELECT 13 AS alive;",
    );
    assert_eq!(error_numbers(&events.0), vec![8134], "{:?}", events.0);
    assert_eq!(
        done_in_proc_more(&events.0),
        vec![true, true],
        "{:?}",
        events.0
    );
    assert!(has_int_row(&events.0, 13), "{:?}", events.0);
}

/// The ints of the first column of each row of the events, in order.
fn int_rows(events: &[Event]) -> Vec<i32> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => match row.first() {
                Some(Value::I32(n)) => Some(*n),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A dynamic text that runs a dynamic text that runs a third one: each level runs its own
/// statements around the call it makes, in the order they are written, and the batch goes
/// on after the outermost call.
#[test]
fn a_dynamic_text_runs_its_own_dynamic_texts_three_deep() {
    let events = run_batch(
        &mut session(),
        "EXEC('SELECT 1; EXEC(''SELECT 2; EXEC(''''SELECT 3'''') ; SELECT 4''); SELECT 5'); \
         SELECT 6;",
    );
    assert_eq!(
        error_numbers(&events.0),
        Vec::<u32>::new(),
        "{:?}",
        events.0
    );
    assert_eq!(int_rows(&events.0), [1, 2, 3, 4, 5, 6], "{:?}", events.0);
}

/// An `EXEC` in the body of a `WHILE` of a dynamic text runs once per turn, and the loop
/// reads its counter after each call: three result sets of one row, then the counter.
#[test]
fn an_exec_in_a_loop_of_a_dynamic_text_runs_on_each_turn() {
    let events = run_batch(
        &mut session(),
        "EXEC('DECLARE @i int = 0; WHILE @i < 3 BEGIN EXEC(''SELECT 7''); SET @i += 1 END; \
         SELECT @i'); SELECT 9;",
    );
    assert_eq!(
        error_numbers(&events.0),
        Vec::<u32>::new(),
        "{:?}",
        events.0
    );
    assert_eq!(int_rows(&events.0), [7, 7, 7, 3, 9], "{:?}", events.0);
}

/// The same loop through `sp_executesql`, its bound taken from a parameter, and a procedure
/// call of the loop body answering 2812 on each turn without ending the loop.
#[test]
fn a_loop_of_sp_executesql_calls_on_each_turn_and_reads_its_parameter() {
    let events = run_batch(
        &mut session(),
        "EXEC sp_executesql N'DECLARE @i int = 0; WHILE @i < @n BEGIN \
         EXEC(''SELECT @@ROWCOUNT''); EXEC nested_no_such_procedure; SET @i += 1 END; \
         SELECT @i', N'@n int', 2;",
    );
    assert_eq!(error_numbers(&events.0), vec![2812, 2812], "{:?}", events.0);
    assert_eq!(int_rows(&events.0).last(), Some(&2), "{:?}", events.0);
}

/// A parameter of `sp_executesql` is a variable of the level the call opens: the text it
/// calls does not see it (137), and the text itself reads it again once the call returned.
#[test]
fn a_parameter_is_a_variable_of_its_own_level_alone() {
    let events = run_batch(
        &mut session(),
        "EXEC sp_executesql N'EXEC(''SELECT @a''); SELECT @a', N'@a int', 5;",
    );
    assert_eq!(error_numbers(&events.0), vec![137], "{:?}", events.0);
    assert_eq!(int_rows(&events.0), [5], "{:?}", events.0);
}

/// The `SET` options and the `USE` of a callee end with it: the caller's next batch is
/// parsed, bound and run with its own `QUOTED_IDENTIFIER`, `NOCOUNT` and database.
#[test]
fn the_options_and_the_database_of_a_callee_end_with_it() {
    let mut session = session();
    let changed = run_batch(
        &mut session,
        "EXEC('SET QUOTED_IDENTIFIER OFF; SET NOCOUNT ON; USE tempdb; SELECT DB_NAME()');",
    );
    assert_eq!(
        error_numbers(&changed.0),
        Vec::<u32>::new(),
        "{:?}",
        changed.0
    );
    assert!(
        changed.0.iter().any(
            |event| matches!(event, Event::Row(row) if row.first().is_some_and(
                |value| matches!(value, Value::String(text) if text.text == "tempdb")
            ))
        ),
        "{:?}",
        changed.0
    );

    let after = run_batch(&mut session, "SELECT DB_NAME();");
    assert_eq!(error_numbers(&after.0), Vec::<u32>::new(), "{:?}", after.0);
    assert!(
        after.0.iter().any(
            |event| matches!(event, Event::Row(row) if row.first().is_some_and(
                |value| matches!(value, Value::String(text) if text.text == "master")
            ))
        ),
        "{:?}",
        after.0
    );
    // NOCOUNT is off again: the DONE of the SELECT carries its count.
    assert!(
        after.0.contains(&Event::Done(Some(1), false)),
        "{:?}",
        after.0
    );

    // QUOTED_IDENTIFIER is on again: a double-quoted name is a column, which does not exist.
    let quoted = run_batch(&mut session, "SELECT \"a\";");
    assert_eq!(error_numbers(&quoted.0), vec![207], "{:?}", quoted.0);
}

/// An error of the whole batch raised two levels down ends every level up to the batch:
/// neither the statement after the inner call nor the one after the outer call runs, and
/// the inner level sends no DONEINPROC of its own.
#[test]
fn an_error_of_the_batch_two_levels_down_ends_the_response_with_one_done() {
    let events = run_batch(
        &mut session(),
        "EXEC('EXEC(''SELECT CAST(''''a'''' AS int)''); SELECT 3'); SELECT 2;",
    );
    assert_eq!(error_numbers(&events.0), vec![245], "{:?}", events.0);
    assert_eq!(int_rows(&events.0), Vec::<i32>::new(), "{:?}", events.0);
    assert_eq!(
        done_in_proc_more(&events.0),
        Vec::<bool>::new(),
        "{:?}",
        events.0
    );
}

/// The same two levels with an error of a statement: the inner level closes that statement
/// with a DONEINPROC and goes on, each level above it goes on too, and the statement after
/// the outer call runs. The error comes once, before the DONEINPROC that closes its
/// statement, and the inner call is closed by its own DONEINPROC.
#[test]
fn an_error_of_a_statement_two_levels_down_lets_each_level_go_on() {
    let events = run_batch(
        &mut session(),
        "EXEC('EXEC(''SELECT 1/0''); SELECT 3'); SELECT 2;",
    );
    assert_eq!(
        token_shape(&events.0),
        [
            "columns",
            "error 8134",
            "doneinproc None true",
            "doneinproc None true",
            "columns",
            "row Some(I32(3))",
            "doneinproc Some(1) true",
            "done None true",
            "columns",
            "row Some(I32(2))",
            "done Some(1) false",
        ],
        "{:?}",
        events.0
    );
}

/// The tokens of `events`, one word each, with the row count and `more` of each DONE.
fn token_shape(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            Event::Columns(_) => "columns".to_owned(),
            Event::Row(row) => format!("row {:?}", row.first()),
            Event::Done(count, more) => format!("done {count:?} {more}"),
            Event::DoneInProc(count, more) => format!("doneinproc {count:?} {more}"),
            Event::Error(err) => format!("error {}", err.number),
            other => format!("{other:?}"),
        })
        .collect()
}

/// A dynamic text refused before it ran (a syntax error) is decided by the program that
/// made the call: its error comes once, closed by the DONE of the `EXEC` in the batch and
/// by a DONEINPROC one or two levels down, and each caller goes on; under
/// `SET XACT_ABORT ON` it ends the batch at each depth. `@@ROWCOUNT` keeps the count the
/// caller had, since the text ran no statement, and `@@ERROR` takes the number.
#[test]
fn a_dynamic_text_refused_before_it_ran_keeps_its_tokens_at_each_depth() {
    let shapes = [
        (
            "EXEC('SELECT FROM'); SELECT 2;",
            vec![
                "error 156",
                "done None true",
                "columns",
                "row Some(I32(2))",
                "done Some(1) false",
            ],
        ),
        (
            "EXEC('EXEC(''SELECT FROM''); SELECT 3'); SELECT 2;",
            vec![
                "error 156",
                "doneinproc None true",
                "columns",
                "row Some(I32(3))",
                "doneinproc Some(1) true",
                "done None true",
                "columns",
                "row Some(I32(2))",
                "done Some(1) false",
            ],
        ),
        (
            "EXEC('EXEC(''EXEC(''''SELECT FROM''''); SELECT 4''); SELECT 3'); SELECT 2;",
            vec![
                "error 156",
                "doneinproc None true",
                "columns",
                "row Some(I32(4))",
                "doneinproc Some(1) true",
                "doneinproc None true",
                "columns",
                "row Some(I32(3))",
                "doneinproc Some(1) true",
                "done None true",
                "columns",
                "row Some(I32(2))",
                "done Some(1) false",
            ],
        ),
    ];
    for (text, expected) in shapes {
        let events = run_batch(&mut session(), text);
        assert_eq!(token_shape(&events.0), expected, "{text}");
        let events = run_batch(&mut session(), &format!("SET XACT_ABORT ON; {text}"));
        assert_eq!(
            token_shape(&events.0),
            ["done None true", "error 156", "done None false"],
            "XACT_ABORT ON; {text}"
        );
    }
    let events = run_batch(
        &mut session(),
        "SELECT 1; EXEC('SELECT FROM'); SELECT @@ROWCOUNT, @@ERROR;",
    );
    let last = events.0.iter().rev().find_map(|event| match event {
        Event::Row(row) => Some(row.clone()),
        _ => None,
    });
    assert_eq!(
        last,
        Some(vec![Value::I32(1), Value::I32(156)]),
        "{:?}",
        events.0
    );
}

/// A call refused before a callee ran (2812) ends its statement as an error of any
/// statement does: `@@ROWCOUNT` goes back to 0, in the batch and in a dynamic text.
#[test]
fn a_refused_call_puts_the_row_count_back_to_zero() {
    let events = run_batch(
        &mut session(),
        "SELECT 1; EXEC nested_no_such_procedure; SELECT @@ROWCOUNT;",
    );
    assert_eq!(error_numbers(&events.0), vec![2812], "{:?}", events.0);
    assert_eq!(int_rows(&events.0), [1, 0], "{:?}", events.0);

    let events = run_batch(
        &mut session(),
        "SELECT 1; EXEC('EXEC nested_no_such_procedure; SELECT @@ROWCOUNT');",
    );
    assert_eq!(error_numbers(&events.0), vec![2812], "{:?}", events.0);
    assert_eq!(int_rows(&events.0), [1, 0], "{:?}", events.0);
}

/// `SET XACT_ABORT ON` written in a callee holds for the errors of that callee: a 2812
/// there ends the batch, where without it the caller would go on.
#[test]
fn xact_abort_set_by_a_callee_ends_the_batch_on_its_error() {
    let events = run_batch(
        &mut session(),
        "EXEC('SET XACT_ABORT ON; EXEC nested_no_such_procedure; SELECT 3'); SELECT 2;",
    );
    assert_eq!(error_numbers(&events.0), vec![2812], "{:?}", events.0);
    assert_eq!(int_rows(&events.0), Vec::<i32>::new(), "{:?}", events.0);

    let events = run_batch(
        &mut session(),
        "EXEC('EXEC nested_no_such_procedure; SELECT 3'); SELECT 2;",
    );
    assert_eq!(error_numbers(&events.0), vec![2812], "{:?}", events.0);
    assert_eq!(int_rows(&events.0), [3, 2], "{:?}", events.0);
}

/// An error of a statement a block of a dynamic text holds answers the line of that
/// statement in the text, not the line of the `IF` that holds the block.
#[test]
fn an_error_held_by_a_block_of_a_dynamic_text_keeps_the_line_of_its_statement() {
    let events = run_batch(
        &mut session(),
        "EXEC('IF 1 = 1\nBEGIN\n  SELECT 1\n  SELECT 1/0\nEND');",
    );
    let err = only_error(&events.0);
    assert_eq!((err.number, err.line), (8134, 4), "{:?}", events.0);
}

/// The body of `EXEC sp_executesql N'SELECT 1; CREATE TABLE t …; ALTER TABLE t ADD c int;'`
/// is bound one statement at a time, since it holds an `ALTER TABLE`: the table is created
/// and the column added.
#[test]
fn nested_select_create_alter_runs_all_statements() {
    let mut session = session();
    let (outcome, events) = run_nested(
        &mut session,
        "SELECT 1; CREATE TABLE dbo.nested_alter (id int); ALTER TABLE dbo.nested_alter ADD c int; \
         SELECT c FROM dbo.nested_alter;",
        &[],
    );
    assert!(!outcome.failed, "{:?}", events.0);
    assert_eq!(int_rows(&events.0), [1], "{:?}", events.0);
    assert_eq!(
        events
            .0
            .iter()
            .filter(|event| matches!(event, Event::Columns(_)))
            .count(),
        2,
        "{:?}",
        events.0
    );
}
