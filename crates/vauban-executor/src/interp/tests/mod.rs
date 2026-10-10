//! Running a program: the program counter, the errors a statement raises, the calls, the
//! jumps and the cancellation token.

use std::collections::HashMap;
use std::sync::Arc;

use vauban_binder::{BindContext, BoundExpr, CatalogView, SessionOptions, VariableScope, bind};
use vauban_catalog::Catalog;
use vauban_errors::{ErrorScope, SqlError, SqlResult};
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

/// Answers each call with the end it holds, after the error of that end in the sink.
struct Ending(ProgramEnd);

impl CallHandler for Ending {
    fn call(
        &mut self,
        _call: Call,
        _ctx: &mut ExecContext<'_>,
        sink: &mut dyn StatementSink,
    ) -> SqlResult<ProgramEnd> {
        if let ProgramEnd::Aborted(err) | ProgramEnd::LevelAborted(err) = &self.0 {
            sink.error(err)?;
        }
        Ok(self.0.clone())
    }

    fn restore_database(&mut self, _database: &str, _ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        Ok(())
    }
}

/// A callee an error of its level ended (208 on a table its text names) lets its caller go
/// on with the statement after the `EXECUTE`; a callee an error of its batch ended (245)
/// ends its caller too.
#[test]
fn a_callee_ended_by_an_error_of_its_level_lets_the_caller_go_on() {
    let level = SqlError::new(208, 16, 1, "the callee names a missing table");
    let mut session = ExecSession::default();
    let mut calls = Ending(ProgramEnd::ended_by(level, false));
    assert!(
        matches!(&calls.0, ProgramEnd::LevelAborted(_)),
        "{:?}",
        calls.0
    );
    let (end, sink) = run_scalar("EXEC('x'); SELECT 1;", &mut session, &mut calls);
    assert_eq!(end, ProgramEnd::Completed);
    assert_eq!(sink.rows, vec![vec![Value::I32(1)]]);

    let batch = SqlError::new(245, 16, 1, "the callee failed a conversion");
    let mut session = ExecSession::default();
    let mut calls = Ending(ProgramEnd::ended_by(batch, false));
    let (end, sink) = run_scalar("EXEC('x'); SELECT 1;", &mut session, &mut calls);
    assert!(
        matches!(&end, ProgramEnd::Aborted(error) if error.number == 245),
        "{end:?}"
    );
    assert!(sink.rows.is_empty(), "{sink:?}");
}

/// Under `SET XACT_ABORT ON`, an error that stops its level alone ends the batch: the end
/// of the program reaches its callers.
#[test]
fn under_xact_abort_an_error_of_a_level_ends_the_batch() {
    let level = SqlError::new(208, 16, 1, "the callee names a missing table");
    assert!(matches!(
        ProgramEnd::ended_by(level.clone(), true),
        ProgramEnd::Aborted(_)
    ));
    assert!(matches!(
        ProgramEnd::ended_by(level, false),
        ProgramEnd::LevelAborted(_)
    ));
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

/// Under [`Program::run`], an error of a condition carries the line the condition starts
/// on, not line 0: here each `IF` and each `WHILE` is written on the line of its condition,
/// and the error answers the line of the statement that holds the condition.
#[test]
fn an_error_of_a_condition_carries_the_line_the_condition_starts_on() {
    let mut session = ExecSession::default();
    let (end, sink) = run_scalar(
        "DECLARE @i int = 0;\nIF 1 / @i = 1\n  SELECT 1;\n\nWHILE 1 / @i = 1\n  SELECT 2;",
        &mut session,
        &mut Recorder::default(),
    );
    assert_eq!(end, ProgramEnd::Completed);
    let lines: Vec<(u32, u32)> = sink
        .errors()
        .iter()
        .map(|error| (error.number, error.line))
        .collect();
    assert_eq!(lines, vec![(8134, 2), (8134, 5)]);
}

/// A [`Host`] over one context that writes down the reach the interpreter gave each error
/// it raised.
struct Reaches<'r, 'a> {
    inner: SingleContext<'r, 'a>,
    raised: Vec<(u32, ErrorScope)>,
}

impl Host for Reaches<'_, '_> {
    fn statement(&mut self, stmt: Option<&PhysicalStatement>, site: Site) -> SqlResult<Ran> {
        self.inner.statement(stmt, site)
    }

    fn condition(
        &mut self,
        condition: &BoundExpr,
        site: Site,
    ) -> SqlResult<Result<bool, SqlError>> {
        self.inner.condition(condition, site)
    }

    fn within(
        &mut self,
        site: Site,
        step: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<()>,
    ) -> SqlResult<()> {
        self.inner.within(site, step)
    }

    fn locate(&self, err: SqlError, site: Site) -> SqlError {
        self.inner.locate(err, site)
    }

    fn raise(&mut self, err: &SqlError, reach: ErrorScope) -> SqlResult<()> {
        self.raised.push((err.number, reach));
        self.inner.raise(err, reach)
    }

    fn call(&mut self, call: Call, site: Site) -> SqlResult<Result<ProgramEnd, SqlError>> {
        self.inner.call(call, site)
    }

    fn leave_call(&mut self, database: &str) -> SqlResult<()> {
        self.inner.leave_call(database)
    }

    fn control(&mut self, site: Site) -> SqlResult<()> {
        self.inner.control(site)
    }

    fn cancelled(&self) -> bool {
        self.inner.cancelled()
    }

    fn xact_abort(&self) -> bool {
        self.inner.xact_abort()
    }
}

/// Runs `text` through a [`Reaches`] host, `SET XACT_ABORT ON` held when `xact_abort`, and
/// answers how it ended, the reaches written down and the number of errors in the sink.
fn reaches(
    text: &str,
    xact_abort: bool,
    calls: &mut dyn CallHandler,
) -> (ProgramEnd, Vec<(u32, ErrorScope)>, usize) {
    let statements = planned(text, None, None);
    let program = Program::compile(&statements).expect("the program compiles");
    let eval = StaticContext::default();
    let mut session = ExecSession {
        xact_abort,
        ..ExecSession::default()
    };
    let mut sink = CollectSink::new();
    let (end, raised) = {
        let mut ctx =
            ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
        let mut host = Reaches {
            inner: SingleContext::new(&mut ctx, &mut sink, calls),
            raised: Vec::new(),
        };
        let end = program
            .run_hosted(&mut host)
            .expect("the sink takes everything");
        (end, host.raised)
    };
    (end, raised, sink.errors().len())
}

/// Each error reaches the host once, with the reach the interpreter decided from its scope:
/// 8134 its statement, 245 the batch; under `SET XACT_ABORT ON` the 8134 reaches the batch.
/// The error of an `EXECUTE` refused before a callee ran is decided the same way, and an
/// error that ends the program is not raised twice.
#[test]
fn an_error_reaches_the_host_with_the_reach_the_interpreter_decided() {
    let text = "SELECT 1 / 0; SELECT CAST('x' AS int); SELECT 2;";
    let (end, raised, sent) = reaches(text, false, &mut Recorder::default());
    assert!(
        matches!(&end, ProgramEnd::Aborted(err) if err.number == 245),
        "{end:?}"
    );
    assert_eq!(
        raised,
        vec![(8134, ErrorScope::Statement), (245, ErrorScope::Batch)]
    );
    assert_eq!(sent, 2);

    let (end, raised, sent) = reaches(text, true, &mut Recorder::default());
    assert!(
        matches!(&end, ProgramEnd::Aborted(err) if err.number == 8134),
        "{end:?}"
    );
    assert_eq!(raised, vec![(8134, ErrorScope::Batch)]);
    assert_eq!(sent, 1);

    let mut refused = Failing(SqlError::procedure_not_found("p"));
    let (end, raised, sent) = reaches("EXEC p; SELECT 1;", false, &mut refused);
    assert_eq!(end, ProgramEnd::Completed);
    assert_eq!(raised, vec![(2812, ErrorScope::Statement)]);
    assert_eq!(sent, 1);
}

/// Answers each call with the body refused before it ran, by the error it holds, unsent.
struct Refusing(SqlError);

impl CallHandler for Refusing {
    fn call(
        &mut self,
        _call: Call,
        _ctx: &mut ExecContext<'_>,
        _sink: &mut dyn StatementSink,
    ) -> SqlResult<ProgramEnd> {
        Ok(ProgramEnd::Refused(self.0.clone()))
    }

    fn restore_database(&mut self, _database: &str, _ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        Ok(())
    }
}

/// The error of a body refused before it ran reaches the host once, through the decision of
/// the program that made the call: an error that stops its level stops the callee, which
/// is the `EXECUTE` of the caller, so the caller goes on (102, 208); an error of the batch,
/// or any error under `SET XACT_ABORT ON`, ends the caller too.
#[test]
fn a_callee_refused_before_it_ran_is_decided_by_the_caller() {
    let text = "EXEC('x'); SELECT 1;";
    for number in [102, 208] {
        let error = SqlError::new(number, 16, 1, "refused");
        let (end, raised, sent) = reaches(text, false, &mut Refusing(error.clone()));
        assert_eq!(end, ProgramEnd::Completed, "{number}");
        assert_eq!(raised, vec![(number, ErrorScope::Statement)]);
        assert_eq!(sent, 1);

        let (end, raised, sent) = reaches(text, true, &mut Refusing(error));
        assert!(
            matches!(&end, ProgramEnd::Aborted(err) if err.number == number),
            "{end:?}"
        );
        assert_eq!(raised, vec![(number, ErrorScope::Batch)]);
        assert_eq!(sent, 1);
    }
    let batch = SqlError::new(245, 16, 1, "conversion");
    let (end, raised, sent) = reaches(text, false, &mut Refusing(batch));
    assert!(
        matches!(&end, ProgramEnd::Aborted(err) if err.number == 245),
        "{end:?}"
    );
    assert_eq!(raised, vec![(245, ErrorScope::Batch)]);
    assert_eq!(sent, 1);
}

/// The reach of an error read from its scope: `XACT_ABORT` widens a statement or a level to
/// the batch, and leaves a batch or a connection as it is; an error that ends its program
/// reaches its level at least.
#[test]
fn the_reach_of_an_error_reads_its_scope_and_xact_abort() {
    let statement = SqlError::new(8134, 16, 1, "divide");
    let level = SqlError::new(208, 16, 1, "missing");
    let batch = SqlError::new(245, 16, 1, "conversion");
    assert_eq!(run::reach(&statement, false), ErrorScope::Statement);
    assert_eq!(run::reach(&level, false), ErrorScope::Level);
    assert_eq!(run::reach(&batch, false), ErrorScope::Batch);
    assert_eq!(run::reach(&statement, true), ErrorScope::Batch);
    assert_eq!(run::reach(&level, true), ErrorScope::Batch);
    assert_eq!(run::reach(&batch, true), ErrorScope::Batch);
    assert_eq!(run::ending_reach(ErrorScope::Statement), ErrorScope::Level);
    assert_eq!(run::ending_reach(ErrorScope::Batch), ErrorScope::Batch);
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
