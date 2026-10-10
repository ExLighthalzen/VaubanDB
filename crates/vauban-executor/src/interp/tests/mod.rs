//! Running a program: the program counter, the errors a statement raises, the calls, the
//! jumps and the cancellation token.

use std::collections::HashMap;
use std::sync::Arc;

use vauban_binder::{BindContext, CatalogView, SessionOptions, VariableScope, bind};
use vauban_catalog::Catalog;
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{NoIndexes, PhysicalStatement, PlanCatalog, PlanContext, StorageIndexes};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{TypeInfo, Value};

use super::*;
use crate::context::{CancelToken, ExecSession};
use crate::sink::{CollectSink, SinkEvent};

/// The declared variables a statement of the batch is bound against: the `DECLARE`s
/// written before it.
#[derive(Default)]
struct Declared(HashMap<String, TypeInfo>);

impl VariableScope for Declared {
    fn type_of(&self, name: &str) -> Option<TypeInfo> {
        self.0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, ty)| ty.clone())
    }
}

/// Parses, binds and plans each statement of `text`, against `catalog` when one is given.
pub(crate) fn planned(
    text: &str,
    catalog: Option<&dyn CatalogView>,
    storage: Option<&dyn Storage>,
) -> Vec<PhysicalStatement> {
    vauban_sysfn::register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let mut declared = Declared::default();
    let mut out = Vec::new();
    for statement in &batch.statements {
        let mut bind_ctx = BindContext::scalar(text, SessionOptions::default());
        bind_ctx.catalog = catalog;
        bind_ctx.variables = &declared;
        let bound = bind(statement, &bind_ctx).expect("the statement binds");
        let indexes = storage.map(StorageIndexes);
        let plan_catalog: &dyn PlanCatalog = match &indexes {
            Some(indexes) => indexes,
            None => &NoIndexes,
        };
        let physical = vauban_planner::plan(
            bound,
            &PlanContext {
                catalog: plan_catalog,
            },
        )
        .expect("the statement plans");
        if let PhysicalStatement::Declare(declarations) = &physical {
            for declaration in declarations {
                declared
                    .0
                    .insert(declaration.name.clone(), declaration.ty.clone());
            }
        }
        out.push(physical);
    }
    out
}

/// The calls a program asked for, answered `Completed` after one row handed to the sink.
#[derive(Default)]
struct Recorder {
    calls: Vec<Call>,
}

impl CallHandler for Recorder {
    fn call(
        &mut self,
        call: Call,
        _ctx: &mut ExecContext<'_>,
        sink: &mut dyn StatementSink,
    ) -> SqlResult<ProgramEnd> {
        self.calls.push(call);
        sink.columns(&[])?;
        sink.row(&[])?;
        sink.done(Some(1))?;
        Ok(ProgramEnd::Completed)
    }

    fn restore_database(&mut self, _database: &str, _ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        Ok(())
    }
}

/// Answers each call with the error it holds.
struct Failing(SqlError);

impl CallHandler for Failing {
    fn call(
        &mut self,
        _call: Call,
        _ctx: &mut ExecContext<'_>,
        _sink: &mut dyn StatementSink,
    ) -> SqlResult<ProgramEnd> {
        Err(self.0.clone())
    }

    fn restore_database(&mut self, _database: &str, _ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        Ok(())
    }
}

/// Runs `text` as one program in a scalar context, with `session` and `calls`.
fn run_scalar(
    text: &str,
    session: &mut ExecSession,
    calls: &mut dyn CallHandler,
) -> (ProgramEnd, CollectSink) {
    let statements = planned(text, None, None);
    let program = Program::compile(&statements).expect("the program compiles");
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(session);
    let mut sink = CollectSink::new();
    let end = program
        .run(&mut ctx, &mut sink, calls)
        .expect("the sink takes everything");
    (end, sink)
}

/// The value of the last row the sink received, first column.
fn last_value(sink: &CollectSink) -> Value {
    sink.rows
        .last()
        .and_then(|row| row.first())
        .cloned()
        .expect("a row was received")
}

/// The first column of each row the sink received.
fn first_column(sink: &CollectSink) -> Vec<Value> {
    sink.rows.iter().map(|row| row[0].clone()).collect()
}

/// A bootstrapped catalogue over an in-memory storage.
struct Fixture {
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
    catalog: Catalog,
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
            .expect("the catalogue boots on a fresh storage");
        Self {
            storage,
            txn,
            catalog,
        }
    }

    /// Binds `text` against the catalogue, runs it as one program in a transaction of its
    /// own, and commits.
    fn run(&self, text: &str, session: &mut ExecSession) -> (ProgramEnd, CollectSink) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.catalog.snapshot(&handle);
        let statements = planned(text, Some(&snapshot), Some(self.storage.as_ref()));
        let program = Program::compile(&statements).expect("the program compiles");
        let snap = self.txn.statement_snapshot(&handle);
        let eval = StaticContext::default();
        let mut sink = CollectSink::new();
        let end = {
            let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
                .with_engine(self.storage.as_ref(), &self.txn, &snap)
                .with_catalog(&self.catalog)
                .with_handle(&handle)
                .with_session(session);
            program
                .run(&mut ctx, &mut sink, &mut Recorder::default())
                .expect("the sink takes everything")
        };
        self.txn.commit(handle).expect("the transaction commits");
        (end, sink)
    }
}

/// A `WHILE` whose body calls `EXEC` runs each of its three turns, and the statement
/// written after the loop runs too: the call runs where it is written and the program
/// counter moves on.
#[test]
fn a_loop_that_calls_three_times_runs_three_turns() {
    let mut session = ExecSession::default();
    let mut calls = Recorder::default();
    let (end, sink) = run_scalar(
        "DECLARE @i int = 0; WHILE @i < 3 BEGIN EXEC('SELECT 1'); SET @i += 1 END; SELECT @i;",
        &mut session,
        &mut calls,
    );
    assert_eq!(end, ProgramEnd::Completed);
    let texts: Vec<&str> = calls
        .calls
        .iter()
        .map(|call| match call {
            Call::Dynamic { text, .. } => text.as_str(),
            Call::Procedure { name, .. } => name.as_str(),
        })
        .collect();
    assert_eq!(texts, ["SELECT 1", "SELECT 1", "SELECT 1"]);
    assert_eq!(last_value(&sink), Value::I32(3));
}

/// A procedure call reaches the handler with its name and its arguments evaluated.
#[test]
fn a_procedure_call_reaches_the_handler_with_its_arguments() {
    let mut session = ExecSession::default();
    let mut calls = Recorder::default();
    let (end, _) = run_scalar(
        "DECLARE @n int = 2; EXEC sp_who @n;",
        &mut session,
        &mut calls,
    );
    assert_eq!(end, ProgramEnd::Completed);
    let [Call::Procedure { name, args, .. }] = calls.calls.as_slice() else {
        panic!("one procedure call: {:?}", calls.calls);
    };
    assert_eq!(name, "sp_who");
    assert_eq!(
        args[0].value.as_ref().map(|(value, _)| value.clone()),
        Some(Value::I32(2))
    );
}

/// An `INSERT` that fails with 2627 on each of three turns hands three errors to the sink,
/// and the loop still runs its three turns: the error of a statement ends that statement,
/// not the loop that holds it.
#[test]
fn a_duplicate_key_in_a_loop_of_three_turns_answers_three_errors() {
    let fixture = Fixture::new();
    let mut session = ExecSession::default();
    fixture.run("CREATE TABLE dbo.t (a int PRIMARY KEY);", &mut session);
    fixture.run("INSERT INTO dbo.t VALUES (1);", &mut session);
    let (end, sink) = fixture.run(
        "DECLARE @i int = 0; \
         WHILE @i < 3 BEGIN INSERT INTO dbo.t VALUES (1); SET @i += 1 END; \
         SELECT @i;",
        &mut session,
    );
    assert_eq!(end, ProgramEnd::Completed);
    let numbers: Vec<u32> = sink.errors().iter().map(|error| error.number).collect();
    assert_eq!(numbers, vec![2627, 2627, 2627], "{sink:?}");
    assert_eq!(last_value(&sink), Value::I32(3));
}

/// Under `SET XACT_ABORT ON` the first 2627 ends the program: the statement after the
/// loop does not run.
#[test]
fn a_duplicate_key_under_xact_abort_ends_the_program() {
    let fixture = Fixture::new();
    let mut session = ExecSession::default();
    fixture.run("CREATE TABLE dbo.t (a int PRIMARY KEY);", &mut session);
    fixture.run("INSERT INTO dbo.t VALUES (1);", &mut session);
    session.xact_abort = true;
    let (end, sink) = fixture.run(
        "DECLARE @i int = 0; \
         WHILE @i < 3 BEGIN INSERT INTO dbo.t VALUES (1); SET @i += 1 END; \
         SELECT @i;",
        &mut session,
    );
    assert!(
        matches!(&end, ProgramEnd::Aborted(error) if error.number == 2627),
        "{end:?}"
    );
    assert_eq!(sink.errors().len(), 1, "{sink:?}");
    assert!(sink.rows.is_empty(), "{sink:?}");
}

/// The error goes to the sink, then the end of its statement; a result set is closed by
/// its row count.
#[test]
fn each_statement_is_closed_in_the_sink() {
    let mut session = ExecSession::default();
    let (end, sink) = run_scalar(
        "SELECT 1; SELECT 1 / 0; SELECT 2;",
        &mut session,
        &mut Recorder::default(),
    );
    assert_eq!(end, ProgramEnd::Completed);
    let shape: Vec<String> = sink
        .events
        .iter()
        .map(|event| match event {
            SinkEvent::Columns(_) => "columns".to_owned(),
            SinkEvent::Row(_) => "row".to_owned(),
            SinkEvent::Done(count) => format!("done {count:?}"),
            SinkEvent::Error(error) => format!("error {}", error.number),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "columns",
            "row",
            "done Some(1)",
            "columns",
            "error 8134",
            "done None",
            "columns",
            "row",
            "done Some(1)",
        ]
    );
    assert_eq!(session.rowcount, 1);
    assert_eq!(session.last_error, 8134);
}

/// An error whose scope is the batch ends the program where it is raised.
#[test]
fn a_call_that_fails_with_an_error_of_batch_scope_ends_the_program() {
    let mut session = ExecSession::default();
    let mut calls = Failing(SqlError::new(50000, 16, 1, "the call failed"));
    let (end, sink) = run_scalar("EXEC('x'); SELECT 1;", &mut session, &mut calls);
    assert!(
        matches!(&end, ProgramEnd::Aborted(error) if error.number == 50000),
        "{end:?}"
    );
    assert_eq!(sink.errors().len(), 1);
    assert!(sink.rows.is_empty(), "{sink:?}");
}

/// A `GOTO` written in an `IF` lands on a label of the `BEGIN … END` that holds that `IF`:
/// the label is the place of the statement after it, inside the block.
#[test]
fn a_goto_lands_on_a_label_of_an_enclosing_block() {
    let mut session = ExecSession::default();
    let (end, sink) = run_scalar(
        "DECLARE @i int = 0; \
         BEGIN SET @i = 1; again: SET @i += 10; IF @i < 30 GOTO again; END; \
         SELECT @i;",
        &mut session,
        &mut Recorder::default(),
    );
    assert_eq!(end, ProgramEnd::Completed);
    assert_eq!(last_value(&sink), Value::I32(31));
}

/// A `GOTO` written before a loop lands inside its body: the rest of the body runs, then
/// the loop goes on from its test. Four rows, 0 to 3, then the row after the loop.
#[test]
fn a_goto_into_a_loop_body_runs_the_rest_of_the_body() {
    let mut session = ExecSession::default();
    let (_, sink) = run_scalar(
        "DECLARE @i int = 0; GOTO inside; \
         WHILE @i < 3 BEGIN SET @i += 1; inside: SELECT @i; END; \
         SELECT @i + 100;",
        &mut session,
        &mut Recorder::default(),
    );
    assert_eq!(first_column(&sink), [0, 1, 2, 3, 103].map(Value::I32));
}

/// A `GOTO` leaves a loop for a label written after it.
#[test]
fn a_goto_leaves_a_loop() {
    let mut session = ExecSession::default();
    let (_, sink) = run_scalar(
        "DECLARE @i int = 0; \
         WHILE @i < 2 BEGIN SET @i += 1; IF @i = 1 GOTO done; SELECT @i; END; \
         done: SELECT @i + 100;",
        &mut session,
        &mut Recorder::default(),
    );
    assert_eq!(first_column(&sink), [Value::I32(101)]);
}

/// A label no statement declares is refused when the program is compiled, so the
/// statement written before the `GOTO` never runs.
#[test]
fn a_goto_to_an_unknown_label_answers_133_before_anything_runs() {
    let statements = planned("SELECT 1; GOTO nowhere;", None, None);
    let error = Program::compile(&statements).expect_err("no label is named nowhere");
    assert_eq!((error.number, error.severity, error.state), (133, 15, 1));
}

/// `BREAK` jumps past the loop and `CONTINUE` back to its test.
#[test]
fn break_and_continue_jump_out_of_and_back_to_the_loop() {
    let mut session = ExecSession::default();
    let (_, sink) = run_scalar(
        "DECLARE @i int = 0; \
         WHILE @i < 9 BEGIN SET @i += 1; IF @i = 2 CONTINUE; IF @i = 4 BREAK; SELECT @i; END; \
         SELECT @i + 100;",
        &mut session,
        &mut Recorder::default(),
    );
    assert_eq!(first_column(&sink), [1, 3, 104].map(Value::I32));
}

/// An error of the condition of an `IF` goes to the sink and the `ELSE` runs; an error of
/// the condition of a `WHILE` ends the loop before its first turn.
#[test]
fn an_error_in_a_condition_counts_as_a_condition_that_is_not_true() {
    let mut session = ExecSession::default();
    let (end, sink) = run_scalar(
        "DECLARE @i int = 0; \
         IF 1 / @i = 1 SELECT 1; ELSE SELECT 2; \
         WHILE 1 / @i = 1 BEGIN SELECT 3; SET @i = 5; END; \
         SELECT @i + 100;",
        &mut session,
        &mut Recorder::default(),
    );
    assert_eq!(end, ProgramEnd::Completed);
    assert_eq!(first_column(&sink), [2, 100].map(Value::I32));
    let numbers: Vec<u32> = sink.errors().iter().map(|error| error.number).collect();
    assert_eq!(numbers, vec![8134, 8134]);
}

/// `RETURN` ends the program with its status; what follows does not run.
#[test]
fn return_ends_the_program() {
    let mut session = ExecSession::default();
    let (end, sink) = run_scalar(
        "SELECT 1; RETURN; SELECT 2;",
        &mut session,
        &mut Recorder::default(),
    );
    assert_eq!(end, ProgramEnd::Returned(0));
    assert_eq!(sink.rows.len(), 1);
}

/// A loop with no end stops once the token is raised, whether it is written with a
/// `WHILE` or with a `GOTO` that jumps backward.
#[test]
fn a_raised_token_stops_a_loop_at_its_next_jump_backward() {
    for text in [
        "DECLARE @i int = 0; WHILE 1 = 1 SET @i += 1;",
        "DECLARE @i int = 0; again: SET @i += 1; GOTO again;",
    ] {
        let statements = planned(text, None, None);
        let program = Program::compile(&statements).expect("the program compiles");
        let eval = StaticContext::default();
        let token = CancelToken::new();
        token.cancel();
        let mut session = ExecSession::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_cancel(&token)
            .with_session(&mut session);
        let mut sink = CollectSink::new();
        let end = program
            .run(&mut ctx, &mut sink, &mut Recorder::default())
            .expect("the sink takes everything");
        assert_eq!(end, ProgramEnd::Cancelled, "{text}");
    }
}
