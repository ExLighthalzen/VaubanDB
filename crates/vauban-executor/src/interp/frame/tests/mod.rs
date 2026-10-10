//! Call frames: the variables and the options of a caller, set aside while its callee
//! runs and given back when it returns.

use vauban_binder::SessionOptions;
use vauban_errors::{SqlError, SqlResult};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, LockTimeout};
use vauban_types::Value;

use super::*;
use crate::context::{ExecContext, ExecSession};
use crate::interp::tests::planned;
use crate::interp::{Call, CallHandler, Program, ProgramEnd};
use crate::sink::{CollectSink, SinkEvent, StatementSink};

/// What the handler saw of the level a call opened.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    level: u32,
    names: Vec<String>,
    nocount: bool,
}

/// Runs each dynamic text as a program of its own, in the level the interpreter opened,
/// after `prelude`: a stand-in for the `SET` statements of the callee, which change the
/// options of its level.
struct Callee {
    prelude: fn(&mut ExecContext<'_>),
    seen: Vec<Seen>,
    restored: Vec<String>,
    fails: bool,
    restore_fails: bool,
}

impl Callee {
    fn new(prelude: fn(&mut ExecContext<'_>)) -> Self {
        Self {
            prelude,
            seen: Vec::new(),
            restored: Vec::new(),
            fails: false,
            restore_fails: false,
        }
    }
}

impl CallHandler for Callee {
    fn call(
        &mut self,
        call: Call,
        ctx: &mut ExecContext<'_>,
        sink: &mut dyn StatementSink,
    ) -> SqlResult<ProgramEnd> {
        let Call::Dynamic { text, .. } = call else {
            panic!("a dynamic text: {call:?}");
        };
        let session = ctx.session()?;
        self.seen.push(Seen {
            level: session.nest_level,
            names: session.variables.keys().cloned().collect(),
            nocount: session.nocount,
        });
        (self.prelude)(ctx);
        let statements = planned(&text, None, None);
        let program = Program::compile(&statements)?;
        let end = program.run(ctx, sink, self)?;
        if self.fails {
            return Ok(ProgramEnd::Aborted(SqlError::new(
                8134,
                16,
                1,
                "the callee failed",
            )));
        }
        Ok(end)
    }

    fn restore_database(&mut self, database: &str, _ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        self.restored.push(database.to_owned());
        if self.restore_fails {
            return Err(SqlError::new(911, 16, 1, "the database is gone"));
        }
        Ok(())
    }
}

/// Runs `text` as one program in a scalar context, with `session` and `calls`.
fn run(text: &str, session: &mut ExecSession, calls: &mut Callee) -> (ProgramEnd, CollectSink) {
    let statements = planned(text, None, None);
    let program = Program::compile(&statements).expect("the program compiles");
    let eval = StaticContext {
        database: "callerdb".to_owned(),
        ..StaticContext::default()
    };
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(session);
    let mut sink = CollectSink::new();
    let end = program
        .run(&mut ctx, &mut sink, calls)
        .expect("the sink takes everything");
    (end, sink)
}

/// The row counts the sink received at the end of each statement.
fn done_counts(sink: &CollectSink) -> Vec<Option<u64>> {
    sink.events
        .iter()
        .filter_map(|event| match event {
            SinkEvent::Done(count) => Some(*count),
            _ => None,
        })
        .collect()
}

/// The first column of each row the sink received.
fn first_column(sink: &CollectSink) -> Vec<Value> {
    sink.rows.iter().map(|row| row[0].clone()).collect()
}

/// Leaves the options of the level as they are.
fn nothing(_: &mut ExecContext<'_>) {}

/// `@x` and `@X` are one entry, kept under its first spelling, at the position it took.
#[test]
fn names_fold_as_the_binder_folds_them() {
    let mut map = NameMap::new();
    assert_eq!(map.insert("@Nom".to_owned(), 1), None);
    assert_eq!(map.insert("@autre".to_owned(), 2), None);
    assert_eq!(map.insert("@NOM".to_owned(), 3), Some(1));
    assert_eq!(map.len(), 2);
    assert_eq!(map.get("@nom"), Some(&3));
    assert_eq!(map.get("@É"), None);
    assert_eq!(
        (map.position("@nOm"), map.position("@AUTRE")),
        (Some(0), Some(1))
    );
    assert_eq!(map.keys().collect::<Vec<_>>(), ["@Nom", "@autre"]);
    assert_eq!(*map.entry("@NOM".to_owned()).or_insert(9), 3);
    assert_eq!(*map.entry("@ß".to_owned()).or_insert_with(|| 4), 4);
    assert_eq!(map.get("@SS"), Some(&4));
    assert!(map.contains_key("@Autre"));
    map.clear();
    assert!(map.is_empty());
    assert_eq!(map.get("@nom"), None);
}

/// The callee starts with no variable, declares its own `@x` beside the caller's, and its
/// variables are gone when it returns; the caller reads its own `@x`, at one level.
#[test]
fn a_variable_declared_by_a_callee_is_not_visible_to_its_caller() {
    let mut session = ExecSession::default();
    let mut calls = Callee::new(nothing);
    let (end, sink) = run(
        "DECLARE @x int = 1; \
         EXEC('DECLARE @x int = 5; DECLARE @y int = 2; SELECT @x + @y'); \
         SELECT @x;",
        &mut session,
        &mut calls,
    );
    assert_eq!(end, ProgramEnd::Completed, "{sink:?}");
    assert_eq!(first_column(&sink), [Value::I32(7), Value::I32(1)]);
    assert_eq!(
        calls.seen,
        [Seen {
            level: 1,
            names: Vec::new(),
            nocount: false,
        }]
    );
    assert_eq!(session.variables.keys().collect::<Vec<_>>(), ["@x"]);
    assert_eq!(session.variables.get("@y"), None);
    assert_eq!(session.nest_level, 0);
}

/// `SET NOCOUNT ON` in the callee drops the row count of its own statements; the caller's
/// next statement has its count again. The other way round, `SET NOCOUNT OFF` in the
/// callee ends with it too.
#[test]
fn nocount_set_by_a_callee_is_restored_on_return() {
    let mut session = ExecSession::default();
    let mut calls = Callee::new(|ctx| ctx.session().expect("a session").nocount = true);
    let (_, sink) = run(
        "SELECT 1; EXEC('SELECT 2'); SELECT 3;",
        &mut session,
        &mut calls,
    );
    assert_eq!(done_counts(&sink), [Some(1), None, Some(1)]);
    assert!(!session.nocount);

    let mut session = ExecSession {
        nocount: true,
        ..ExecSession::default()
    };
    let mut calls = Callee::new(|ctx| ctx.session().expect("a session").nocount = false);
    let (_, sink) = run(
        "SELECT 1; EXEC('SELECT 2'); SELECT 3;",
        &mut session,
        &mut calls,
    );
    assert_eq!(done_counts(&sink), [None, Some(1), None]);
    assert!(session.nocount);
    assert!(
        calls.seen[0].nocount,
        "the callee starts with the caller's options"
    );
}

/// Changes the options the executor holds, each to a value its default is not.
fn change_every_option(ctx: &mut ExecContext<'_>) {
    ctx.options.ansi_nulls = false;
    ctx.options.ansi_warnings = false;
    ctx.options.arithabort = false;
    ctx.options.concat_null_yields_null = false;
    ctx.options.numeric_roundabort = true;
    ctx.options = ctx.options.with_identity_insert("db", "dbo", "t");
    let session = ctx.session().expect("a session");
    session.xact_abort = true;
    session.isolation = IsolationLevel::Serializable;
    session.lock_timeout = LockTimeout::Millis(1234);
    session.nocount = true;
}

/// The options the callee changed are back at the caller's values once it returns, and
/// the caller's database goes to the handler.
#[test]
fn the_options_a_callee_changes_are_restored_on_return() {
    let mut session = ExecSession::default();
    let mut calls = Callee::new(change_every_option);
    let statements = planned("EXEC('SELECT 1');", None, None);
    let program = Program::compile(&statements).expect("the program compiles");
    let eval = StaticContext {
        database: "callerdb".to_owned(),
        ..StaticContext::default()
    };
    let before = SessionOptions::default();
    let mut ctx = ExecContext::scalar(&eval, before).with_session(&mut session);
    let mut sink = CollectSink::new();
    let end = program
        .run(&mut ctx, &mut sink, &mut calls)
        .expect("the sink takes everything");
    assert_eq!(end, ProgramEnd::Completed);
    assert_eq!(ctx.options, before);
    drop(ctx);
    let fresh = ExecSession::default();
    assert_eq!(
        (
            session.xact_abort,
            session.isolation,
            session.lock_timeout,
            session.nocount,
        ),
        (
            fresh.xact_abort,
            fresh.isolation,
            fresh.lock_timeout,
            fresh.nocount,
        )
    );
    assert_eq!(calls.restored, ["callerdb"]);
}

/// A callee that ends by an error gives its caller its state back too: the caller's
/// variables and options, at level 0.
#[test]
fn a_callee_that_fails_still_gives_the_caller_its_state_back() {
    let mut session = ExecSession::default();
    let mut calls = Callee::new(change_every_option);
    calls.fails = true;
    let (end, _) = run(
        "DECLARE @x int = 1; EXEC('DECLARE @y int = 2'); SELECT @x;",
        &mut session,
        &mut calls,
    );
    assert!(matches!(end, ProgramEnd::Aborted(_)), "{end:?}");
    assert_eq!(session.variables.keys().collect::<Vec<_>>(), ["@x"]);
    assert!(!session.xact_abort && !session.nocount);
    assert_eq!(session.nest_level, 0);
    assert_eq!(calls.restored, ["callerdb"]);
}

/// Each call opens a level one deeper than its caller, and a call past the limit is
/// refused before its callee runs: its text is not run, the program that made it ends,
/// and each level is closed on the way out.
#[test]
fn a_call_past_the_nesting_limit_is_refused() {
    let mut session = ExecSession {
        max_nest_level: Some(2),
        ..ExecSession::default()
    };
    let mut calls = Callee::new(nothing);
    let (end, sink) = run(
        "EXEC('SELECT 1; EXEC(''SELECT 2; EXEC(''''SELECT 3'''')'')'); SELECT 4;",
        &mut session,
        &mut calls,
    );
    let levels: Vec<u32> = calls.seen.iter().map(|seen| seen.level).collect();
    assert_eq!(levels, [1, 2]);
    assert_eq!(first_column(&sink), [Value::I32(1), Value::I32(2)]);
    let numbers: Vec<u32> = sink.errors().iter().map(|error| error.number).collect();
    assert_eq!(numbers, [217]);
    assert!(matches!(end, ProgramEnd::Aborted(_)), "{end:?}");
    assert_eq!(session.nest_level, 0);

    let mut session = ExecSession::default();
    let mut calls = Callee::new(nothing);
    let (end, sink) = run(
        "EXEC('SELECT 1; EXEC(''SELECT 2; EXEC(''''SELECT 3'''')'')'); SELECT 4;",
        &mut session,
        &mut calls,
    );
    assert_eq!(end, ProgramEnd::Completed);
    let levels: Vec<u32> = calls.seen.iter().map(|seen| seen.level).collect();
    assert_eq!(levels, [1, 2, 3]);
    assert_eq!(first_column(&sink), [1, 2, 3, 4].map(Value::I32));
}

/// An error of the change of database back to the caller's reaches the sink after the
/// error that ended the callee, and the program still ends as the callee decided.
#[test]
fn an_error_restoring_the_database_after_a_failed_callee_reaches_the_sink() {
    let mut session = ExecSession::default();
    let mut calls = Callee::new(nothing);
    calls.fails = true;
    calls.restore_fails = true;
    let (end, sink) = run("EXEC('SELECT 1'); SELECT 2;", &mut session, &mut calls);
    assert!(
        matches!(&end, ProgramEnd::Aborted(error) if error.number == 8134),
        "{end:?}"
    );
    let numbers: Vec<u32> = sink.errors().iter().map(|error| error.number).collect();
    assert_eq!(numbers, [911]);
    assert_eq!(first_column(&sink), [Value::I32(1)]);
}
