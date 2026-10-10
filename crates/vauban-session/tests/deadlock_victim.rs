//! Integration tests of the victim of a deadlock (1205) and of a lock wait that ran out of
//! time (1222), with two sessions on one engine.
//!
//! The victim loses its transaction: the ERROR token is followed by the ENVCHANGE that rolls
//! the transaction back, its batch stops, its writes are undone and `@@TRANCOUNT` reads 0,
//! with `XACT_ABORT` OFF or ON and from inside `EXEC('…')`. The other session goes on and
//! commits. A 1222 stops its statement alone and keeps the transaction open.
//!
//! No socket and no TDS: the batches run on `Session::run_batch` through a `ResultSink` that
//! keeps the rows, the errors and the transaction ENVCHANGE, in order.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

/// What a sink receives, in order: rows, errors, the transaction ENVCHANGE and the DONE.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Row(Vec<Value>),
    Error(u32),
    Begin,
    Commit,
    Rollback,
    Done,
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
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.number));
        Ok(())
    }
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        match change {
            EnvChange::BeginTransaction(_) => self.0.push(Event::Begin),
            EnvChange::CommitTransaction(_) => self.0.push(Event::Commit),
            EnvChange::RollbackTransaction(_) => self.0.push(Event::Rollback),
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

/// An in-memory engine with the built-in functions registered, and two tables of one row
/// each, `dbo.t1` and `dbo.t2`, whose `v` is 0.
fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
    let mut setup = session(&engine, 60);
    run(
        &mut setup,
        "CREATE TABLE dbo.t1 (id int PRIMARY KEY, v int); \
         CREATE TABLE dbo.t2 (id int PRIMARY KEY, v int); \
         INSERT INTO dbo.t1 VALUES (1, 0); INSERT INTO dbo.t2 VALUES (1, 0);",
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
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "{text}: {events:#?}"
    );
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

/// The committed `v` of `dbo.t1` and of `dbo.t2`, read by a third session.
fn committed(engine: &Arc<Engine>) -> Vec<Vec<i32>> {
    let mut reader = session(engine, 63);
    ints(&run(
        &mut reader,
        "SELECT v FROM dbo.t1 WHERE id = 1; SELECT v FROM dbo.t2 WHERE id = 1;",
    ))
}

/// The crossed updates of one deadlock: A, under `DEADLOCK_PRIORITY LOW` and `prelude`,
/// writes `t1` then `t2` through `crossing`; B writes `t2` then `t1`. A is the victim.
/// Returns A's session, what A's crossing batch received, B's session and what B's
/// closing batch received.
fn crossed_deadlock(
    engine: &Arc<Engine>,
    prelude: &str,
    crossing: &'static str,
) -> (Session, Vec<Event>, Session, Vec<Event>) {
    let mut a = session(engine, 61);
    let mut b = session(engine, 62);
    run(
        &mut a,
        &format!(
            "{prelude} SET DEADLOCK_PRIORITY LOW; BEGIN TRANSACTION; \
             UPDATE dbo.t1 SET v = 1 WHERE id = 1;"
        ),
    );
    run(
        &mut b,
        "BEGIN TRANSACTION; UPDATE dbo.t2 SET v = 2 WHERE id = 1;",
    );
    let (sender, receiver) = mpsc::channel();
    let waiting = thread::spawn(move || {
        let events = run_on(&mut a, crossing);
        let _ = sender.send(());
        (a, events)
    });
    assert!(
        receiver.recv_timeout(Duration::from_millis(300)).is_err(),
        "A waits for the row B holds"
    );
    let b_events = run_on(
        &mut b,
        "UPDATE dbo.t1 SET v = 2 WHERE id = 1; SELECT @@TRANCOUNT;",
    );
    let (a, a_events) = waiting.join().expect("thread of A");
    (a, a_events, b, b_events)
}

/// The victim's batch answers its 1205, the ENVCHANGE of the rollback and the DONE, and
/// nothing of the statement after it; `@@TRANCOUNT` then reads 0 and its write to `t1` is
/// undone. B updates `t1` without an error and commits both rows.
#[test]
fn the_victim_of_a_crossed_update_is_rolled_back_and_the_other_commits() {
    let engine = engine();
    let (mut a, a_events, mut b, b_events) = crossed_deadlock(
        &engine,
        "",
        "UPDATE dbo.t2 SET v = 1 WHERE id = 1; SELECT 7;",
    );
    assert_eq!(
        a_events,
        vec![Event::Error(1205), Event::Rollback, Event::Done],
        "A"
    );
    assert_eq!(errors(&b_events), Vec::<u32>::new(), "B: {b_events:#?}");
    assert_eq!(ints(&b_events), vec![vec![1]], "B keeps its transaction");
    assert_eq!(
        ints(&run(&mut a, "SELECT @@TRANCOUNT, XACT_STATE(), @@ERROR;")),
        vec![vec![0, 0, 1205]]
    );
    assert_eq!(
        run(&mut b, "COMMIT TRANSACTION;"),
        vec![Event::Commit, Event::Done]
    );
    assert_eq!(committed(&engine), vec![vec![2], vec![2]]);
}

/// Under `XACT_ABORT ON` the victim is rolled back as well, once.
#[test]
fn the_victim_under_xact_abort_is_rolled_back_once() {
    let engine = engine();
    let (mut a, a_events, mut b, b_events) = crossed_deadlock(
        &engine,
        "SET XACT_ABORT ON;",
        "UPDATE dbo.t2 SET v = 1 WHERE id = 1; SELECT 7;",
    );
    assert_eq!(errors(&a_events), vec![1205], "A: {a_events:#?}");
    assert_eq!(
        a_events
            .iter()
            .filter(|event| **event == Event::Rollback)
            .count(),
        1,
        "A: {a_events:#?}"
    );
    assert!(ints(&a_events).is_empty(), "A: {a_events:#?}");
    assert_eq!(errors(&b_events), Vec::<u32>::new(), "B: {b_events:#?}");
    assert_eq!(ints(&run(&mut a, "SELECT @@TRANCOUNT;")), vec![vec![0]]);
    run(&mut b, "COMMIT TRANSACTION;");
    assert_eq!(committed(&engine), vec![vec![2], vec![2]]);
}

/// A victim inside `EXEC('…')` stops the dynamic text and its caller, and the caller is left
/// with no transaction: `@@TRANCOUNT` reads 0, not the count from before the call.
#[test]
fn the_victim_inside_a_dynamic_text_rolls_back_the_transaction_of_its_caller() {
    let engine = engine();
    let (mut a, a_events, mut b, b_events) = crossed_deadlock(
        &engine,
        "",
        "EXEC('UPDATE dbo.t2 SET v = 1 WHERE id = 1; SELECT 7;'); SELECT 8;",
    );
    assert_eq!(errors(&a_events), vec![1205], "A: {a_events:#?}");
    assert!(a_events.contains(&Event::Rollback), "A: {a_events:#?}");
    assert!(ints(&a_events).is_empty(), "A: {a_events:#?}");
    assert_eq!(errors(&b_events), Vec::<u32>::new(), "B: {b_events:#?}");
    assert_eq!(ints(&run(&mut a, "SELECT @@TRANCOUNT;")), vec![vec![0]]);
    run(&mut b, "COMMIT TRANSACTION;");
    assert_eq!(committed(&engine), vec![vec![2], vec![2]]);
}

/// A lock wait refused by `NOWAIT` (1222) stops its statement alone: the batch goes on, the
/// transaction stays open with its earlier write, and commits it.
#[test]
fn a_lock_timeout_stops_its_statement_and_keeps_the_transaction() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut b,
        "BEGIN TRANSACTION; UPDATE dbo.t1 SET v = 2 WHERE id = 1;",
    );
    let events = run_on(
        &mut a,
        "BEGIN TRANSACTION; UPDATE dbo.t2 SET v = 1 WHERE id = 1; \
         UPDATE dbo.t1 WITH (NOWAIT) SET v = 1 WHERE id = 1; \
         SELECT @@TRANCOUNT, XACT_STATE(), @@ERROR;",
    );
    assert_eq!(errors(&events), vec![1222], "{events:#?}");
    assert!(!events.contains(&Event::Rollback), "{events:#?}");
    assert_eq!(ints(&events), vec![vec![1, 1, 1222]]);

    let nested = run_on(
        &mut a,
        "EXEC('UPDATE dbo.t1 WITH (NOWAIT) SET v = 1 WHERE id = 1; SELECT 7;'); \
         SELECT @@TRANCOUNT;",
    );
    assert_eq!(errors(&nested), vec![1222], "{nested:#?}");
    assert_eq!(ints(&nested), vec![vec![7], vec![1]], "the caller goes on");

    run(&mut a, "COMMIT TRANSACTION;");
    run(&mut b, "COMMIT TRANSACTION;");
    assert_eq!(committed(&engine), vec![vec![2], vec![1]]);
}
