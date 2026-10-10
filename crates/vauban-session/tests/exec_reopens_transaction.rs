//! Integration tests of a called level (`EXEC('…')`, `sp_executesql`) that ends the
//! transaction of its caller and opens another one, with two sessions on one engine.
//!
//! The transaction belongs to the connection: the caller goes on in the transaction its
//! callee opened, with its locks, and its `ROLLBACK` or `COMMIT` ends it. The transaction
//! the callee committed is committed, the one it rolled back is undone, and the other
//! session reads and writes the rows the open transaction does not hold. A callee that
//! leaves another transaction count raises 266 and leaves the transaction open.
//!
//! The other session writes `WITH (NOWAIT)`: a row held by a transaction answers 1222 at
//! once instead of waiting, so a lock left behind fails the test instead of hanging it.
//!
//! No socket and no TDS: the batches run on `Session::run_batch` through a `ResultSink` that
//! keeps the rows, the errors, the transaction ENVCHANGE and the DONE tokens, in order.

use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{
    Engine, ProcAction, ProcArg, ResultSink, Session, SessionState, SystemProcedureResolver,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

/// What a sink receives, in order. A transaction ENVCHANGE keeps its descriptor.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Row(Vec<Value>),
    Error(u32),
    Begin(u64),
    Commit(u64),
    Rollback(u64),
    Done,
    DoneInProc,
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.0.push(Event::Row(row.to_vec()));
        Ok(())
    }
    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        self.0.push(Event::Done);
        Ok(())
    }
    fn done_in_proc(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        self.0.push(Event::DoneInProc);
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.number));
        Ok(())
    }
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        match change {
            EnvChange::BeginTransaction(d) => self.0.push(Event::Begin(*d)),
            EnvChange::CommitTransaction(d) => self.0.push(Event::Commit(*d)),
            EnvChange::RollbackTransaction(d) => self.0.push(Event::Rollback(*d)),
            _ => {}
        }
        Ok(())
    }
    fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        Ok(())
    }
    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

/// Serves `sp_executesql` with its first argument as the text, without parameters.
fn resolver() -> Arc<SystemProcedureResolver> {
    fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>> {
        let base = name.rsplit('.').next().unwrap_or(name);
        if !base.eq_ignore_ascii_case("sp_executesql") {
            return None;
        }
        match args.first().map(|arg| arg.value) {
            Some(Value::String(text)) => Some(Ok(ProcAction::ExecuteSql {
                statement: text.text.clone(),
                params: Vec::new(),
            })),
            _ => Some(Err(SqlError::procedure_expects_parameter(
                "sp_executesql",
                "@statement",
            ))),
        }
    }
    Arc::new(resolve)
}

/// An in-memory engine with the built-in functions and `sp_executesql` registered, and
/// `dbo.t` holding the rows `(1, 0)` and `(2, 0)`.
fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    let engine = Arc::new(Engine::with_system_procedures(
        Arc::new(MemoryStorage::new()),
        resolver(),
    ));
    let mut setup = session(&engine, 60);
    run(
        &mut setup,
        "CREATE TABLE dbo.t (id int PRIMARY KEY, v int); \
         INSERT INTO dbo.t VALUES (1, 0); INSERT INTO dbo.t VALUES (2, 0);",
    );
    engine
}

/// A session of identifier `spid` on `engine`.
fn session(engine: &Arc<Engine>, spid: i16) -> Session {
    Session::new(Arc::clone(engine), SessionState::new(spid))
}

/// Runs `text` on `session` and returns what the sink received.
fn run_on(session: &mut Session, text: &str) -> Vec<Event> {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("a batch reports its errors through the sink, not through its result");
    sink.0
}

/// Runs `text` and fails when the batch raised an error.
fn run(session: &mut Session, text: &str) -> Vec<Event> {
    let events = run_on(session, text);
    assert!(errors(&events).is_empty(), "{text}: {events:#?}");
    events
}

/// The integers of each row of `events`, its columns in order.
fn ints(events: &[Event]) -> Vec<Vec<i32>> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => Some(
                row.iter()
                    .map(|value| match value {
                        Value::I32(value) => *value,
                        Value::I16(value) => i32::from(*value),
                        other => panic!("an int was expected, got {other:?}"),
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

/// The numbers of the errors of `events`.
fn errors(events: &[Event]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Error(number) => Some(*number),
            _ => None,
        })
        .collect()
}

/// The transaction ENVCHANGE of `events`, in order.
fn transactions(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Begin(_) | Event::Commit(_) | Event::Rollback(_)
            )
        })
        .cloned()
        .collect()
}

/// The other session writes row 2 and reads it back without waiting, and finds row 1 held.
fn other_session_is_served_while_row_1_is_held(other: &mut Session) {
    let events = run(
        other,
        "UPDATE dbo.t WITH (NOWAIT) SET v = 7 WHERE id = 2; SELECT v FROM dbo.t WHERE id = 2;",
    );
    assert_eq!(ints(&events), vec![vec![7]]);
    let events = run_on(other, "UPDATE dbo.t WITH (NOWAIT) SET v = 8 WHERE id = 1;");
    assert_eq!(errors(&events), vec![1222], "{events:#?}");
}

/// The other session reads row 1 as the first session left it, then writes it.
fn other_session_writes_row_1(other: &mut Session, committed: i32) {
    let events = run(other, "SELECT v FROM dbo.t WITH (NOWAIT) WHERE id = 1;");
    assert_eq!(ints(&events), vec![vec![committed]]);
    let events = run(
        other,
        "UPDATE dbo.t WITH (NOWAIT) SET v = 9 WHERE id = 1; SELECT @@ROWCOUNT;",
    );
    assert_eq!(ints(&events), vec![vec![1]]);
}

/// The caller's first write, committed by the callee, stays; the callee's write, in the
/// transaction it opened again, is the caller's to roll back.
fn commit_then_reopen_in(call: &str) {
    let engine = engine();
    let mut a = session(&engine, 51);
    let mut b = session(&engine, 52);
    let events = run(
        &mut a,
        &format!(
            "BEGIN TRAN; UPDATE dbo.t SET v = 1 WHERE id = 1; {call}; \
             SELECT @@TRANCOUNT, XACT_STATE();"
        ),
    );
    let Some(Event::Begin(first)) = events.first().cloned() else {
        panic!("BEGIN TRAN announces the transaction first: {events:#?}");
    };
    let [_, Event::Commit(committed), Event::Begin(second)] = transactions(&events)[..] else {
        panic!("the callee commits, then opens another transaction: {events:#?}");
    };
    assert_eq!(committed, first);
    assert_ne!(second, first);
    assert_eq!(ints(&events), vec![vec![1, 1]]);
    other_session_is_served_while_row_1_is_held(&mut b);

    let events = run(&mut a, "SELECT @@TRANCOUNT; ROLLBACK; SELECT @@TRANCOUNT;");
    assert_eq!(ints(&events), vec![vec![1], vec![0]]);
    assert_eq!(transactions(&events), vec![Event::Rollback(second)]);
    other_session_writes_row_1(&mut b, 1);
}

#[test]
fn an_exec_that_commits_and_opens_again_leaves_its_transaction_to_the_caller() {
    commit_then_reopen_in("EXEC('COMMIT; BEGIN TRAN; UPDATE dbo.t SET v = 2 WHERE id = 1')");
}

#[test]
fn sp_executesql_that_commits_and_opens_again_leaves_its_transaction_to_the_caller() {
    commit_then_reopen_in(
        "EXEC sp_executesql N'COMMIT; BEGIN TRAN; UPDATE dbo.t SET v = 2 WHERE id = 1'",
    );
}

#[test]
fn the_reopened_transaction_ends_in_the_batch_of_the_call() {
    let engine = engine();
    let mut a = session(&engine, 51);
    let mut b = session(&engine, 52);
    let events = run(
        &mut a,
        "BEGIN TRAN; EXEC('COMMIT; BEGIN TRAN; UPDATE dbo.t SET v = 2 WHERE id = 1'); \
         SELECT @@TRANCOUNT; ROLLBACK;",
    );
    let Some(Event::Begin(first)) = events.first().cloned() else {
        panic!("BEGIN TRAN announces the transaction first: {events:#?}");
    };
    let second = match events.get(4) {
        Some(Event::Begin(second)) => *second,
        _ => panic!("the callee opens another transaction: {events:#?}"),
    };
    // BEGIN TRAN, the COMMIT, BEGIN TRAN and UPDATE of the callee, the end of the call,
    // the SELECT, the ROLLBACK.
    assert_eq!(
        events,
        vec![
            Event::Begin(first),
            Event::Done,
            Event::Commit(first),
            Event::DoneInProc,
            Event::Begin(second),
            Event::DoneInProc,
            Event::DoneInProc,
            Event::Done,
            Event::Row(vec![Value::I32(1)]),
            Event::Done,
            Event::Rollback(second),
            Event::Done,
        ]
    );
    other_session_writes_row_1(&mut b, 0);
}

#[test]
fn an_exec_that_rolls_back_and_opens_again_leaves_its_transaction_to_the_caller() {
    let engine = engine();
    let mut a = session(&engine, 51);
    let mut b = session(&engine, 52);
    let events = run(
        &mut a,
        "BEGIN TRAN; UPDATE dbo.t SET v = 1 WHERE id = 1; \
         EXEC('ROLLBACK; BEGIN TRAN; UPDATE dbo.t SET v = 2 WHERE id = 1'); \
         SELECT @@TRANCOUNT;",
    );
    let [
        Event::Begin(first),
        Event::Rollback(undone),
        Event::Begin(second),
    ] = transactions(&events)[..]
    else {
        panic!("the callee rolls back, then opens another transaction: {events:#?}");
    };
    assert_eq!(undone, first);
    assert_ne!(second, first);
    assert_eq!(ints(&events), vec![vec![1]]);
    other_session_is_served_while_row_1_is_held(&mut b);

    let events = run(&mut a, "ROLLBACK; SELECT @@TRANCOUNT;");
    assert_eq!(ints(&events), vec![vec![0]]);
    assert_eq!(transactions(&events), vec![Event::Rollback(second)]);
    // Both writes of row 1 are undone.
    other_session_writes_row_1(&mut b, 0);
}

#[test]
fn the_caller_commits_the_transaction_its_callee_opened_again() {
    let engine = engine();
    let mut a = session(&engine, 51);
    let mut b = session(&engine, 52);
    run(
        &mut a,
        "BEGIN TRAN; EXEC('COMMIT; BEGIN TRAN; UPDATE dbo.t SET v = 2 WHERE id = 1'); COMMIT;",
    );
    other_session_writes_row_1(&mut b, 2);
}

#[test]
fn two_levels_down_the_reopened_transaction_reaches_the_batch() {
    let engine = engine();
    let mut a = session(&engine, 51);
    let mut b = session(&engine, 52);
    let events = run(
        &mut a,
        "BEGIN TRAN; \
         EXEC('EXEC(''COMMIT; BEGIN TRAN; UPDATE dbo.t SET v = 2 WHERE id = 1''); \
         SELECT @@TRANCOUNT'); SELECT @@TRANCOUNT;",
    );
    assert_eq!(ints(&events), vec![vec![1], vec![1]]);
    other_session_is_served_while_row_1_is_held(&mut b);
    run(&mut a, "ROLLBACK;");
    other_session_writes_row_1(&mut b, 0);
}

#[test]
fn a_later_transaction_is_announced_with_a_descriptor_of_its_own() {
    let engine = engine();
    let mut a = session(&engine, 51);
    let events = run(
        &mut a,
        "BEGIN TRAN; EXEC('COMMIT; BEGIN TRAN'); COMMIT; BEGIN TRAN; ROLLBACK;",
    );
    let [
        Event::Begin(first),
        Event::Commit(_),
        Event::Begin(second),
        Event::Commit(_),
        Event::Begin(third),
        Event::Rollback(_),
    ] = transactions(&events)[..]
    else {
        panic!("three transactions, one after the other: {events:#?}");
    };
    assert_ne!(first, second);
    assert_ne!(second, third);
    assert_ne!(first, third);
}

#[test]
fn a_callee_that_opens_one_more_transaction_leaves_it_to_its_caller() {
    let engine = engine();
    let mut a = session(&engine, 51);
    let mut b = session(&engine, 52);
    let events = run_on(
        &mut a,
        "EXEC('BEGIN TRAN; UPDATE dbo.t SET v = 3 WHERE id = 1'); SELECT @@TRANCOUNT;",
    );
    assert_eq!(errors(&events), vec![266], "{events:#?}");
    assert_eq!(ints(&events), vec![vec![1]]);
    other_session_is_served_while_row_1_is_held(&mut b);
    let events = run(&mut a, "ROLLBACK; SELECT @@TRANCOUNT;");
    assert_eq!(ints(&events), vec![vec![0]]);
    other_session_writes_row_1(&mut b, 0);

    let events = run_on(
        &mut a,
        "BEGIN TRAN; EXEC('BEGIN TRAN'); SELECT @@TRANCOUNT;",
    );
    assert_eq!(errors(&events), vec![266], "{events:#?}");
    assert_eq!(ints(&events), vec![vec![2]]);
    let events = run(&mut a, "ROLLBACK; SELECT @@TRANCOUNT;");
    assert_eq!(ints(&events), vec![vec![0]]);
}

#[test]
fn a_callee_that_commits_one_level_of_two_leaves_the_rest_to_its_caller() {
    let engine = engine();
    let mut a = session(&engine, 51);
    let mut b = session(&engine, 52);
    let events = run_on(
        &mut a,
        "BEGIN TRAN; BEGIN TRAN; UPDATE dbo.t SET v = 3 WHERE id = 1; EXEC('COMMIT'); \
         SELECT @@TRANCOUNT;",
    );
    assert_eq!(errors(&events), vec![266], "{events:#?}");
    assert_eq!(ints(&events), vec![vec![1]]);
    other_session_is_served_while_row_1_is_held(&mut b);
    run(&mut a, "ROLLBACK;");
    other_session_writes_row_1(&mut b, 0);
}
