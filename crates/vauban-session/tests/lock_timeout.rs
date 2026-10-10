//! Integration tests of `SET LOCK_TIMEOUT` on a row another session holds, with two sessions
//! on one engine.
//!
//! Session A opens a transaction and writes a row; session B, under `SET LOCK_TIMEOUT 500`,
//! asks for that row through an `UPDATE`, a `DELETE` or a `SELECT ... WITH (UPDLOCK)`. B gets
//! 1222, state 51, once the timeout has run out; the statement stops, the transaction of B
//! stays open, and A commits afterwards. `SET LOCK_TIMEOUT 0` refuses the lock at once and
//! `SET LOCK_TIMEOUT -1` waits for the `COMMIT` of A.
//!
//! No socket and no TDS: the batches run on `Session::run_batch` through a `ResultSink` that
//! keeps the rows, the errors and the transaction ENVCHANGE, in order.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

/// What a sink receives, in order: rows, errors with their state, the transaction ENVCHANGE
/// and the DONE.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Row(Vec<Value>),
    Error(u32, u8),
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
        self.0.push(Event::Error(err.number, err.state));
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

/// An in-memory engine with the built-in functions registered, and `dbo.t` of two rows whose
/// `v` is 0.
fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
    let mut setup = session(&engine, 60);
    run(
        &mut setup,
        "CREATE TABLE dbo.t (id int PRIMARY KEY, v int); \
         INSERT INTO dbo.t VALUES (1, 0), (2, 0);",
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
        !events.iter().any(|event| matches!(event, Event::Error(..))),
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

/// The numbers and states of the errors of `events`.
fn errors(events: &[Event]) -> Vec<(u32, u8)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Error(number, state) => Some((*number, *state)),
            _ => None,
        })
        .collect()
}

/// The committed `v` of each row of `dbo.t`, read by a third session.
fn committed(engine: &Arc<Engine>) -> Vec<Vec<i32>> {
    let mut reader = session(engine, 63);
    ints(&run(&mut reader, "SELECT v FROM dbo.t ORDER BY id;"))
}

/// What `prelude` then `statement` on row 1 answered in B while A held that row, and how long
/// the batch took. A opens a transaction and updates row 1 to 1 beforehand; B runs in a
/// transaction of its own, then reads `@@TRANCOUNT, XACT_STATE(), @@ERROR`. A batch of B
/// that has not answered after five seconds fails the test instead of hanging it.
fn blocked(
    engine: &Arc<Engine>,
    prelude: &str,
    statement: &str,
) -> (Session, Session, Vec<Event>, Duration) {
    let mut a = session(engine, 61);
    let mut b = session(engine, 62);
    run(
        &mut a,
        "BEGIN TRANSACTION; UPDATE dbo.t SET v = 1 WHERE id = 1;",
    );
    run(&mut b, &format!("{prelude} BEGIN TRANSACTION;"));
    let text = format!("{statement}; SELECT @@TRANCOUNT, XACT_STATE(), @@ERROR;");
    let (sender, receiver) = mpsc::channel();
    let waiting = thread::spawn(move || {
        let started = Instant::now();
        let events = run_on(&mut b, &text);
        let elapsed = started.elapsed();
        let _ = sender.send(());
        (b, events, elapsed)
    });
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the batch of B answers within five seconds");
    let (b, events, elapsed) = waiting.join().expect("thread of B");
    (a, b, events, elapsed)
}

/// B answers 1222 after its timeout of 500 ms, its statement stops and its transaction stays
/// open; A then commits its write.
fn times_out_after_500_ms(statement: &str) {
    let engine = engine();
    let (mut a, mut b, events, elapsed) = blocked(&engine, "SET LOCK_TIMEOUT 500;", statement);
    assert_eq!(
        errors(&events),
        vec![(1222, 51)],
        "{statement}: {events:#?}"
    );
    assert!(
        !events.contains(&Event::Rollback),
        "{statement}: {events:#?}"
    );
    assert_eq!(ints(&events).last(), Some(&vec![1, 1, 1222]), "{statement}");
    assert!(
        elapsed >= Duration::from_millis(450) && elapsed < Duration::from_secs(2),
        "{statement}: 1222 came back after {elapsed:?}"
    );
    assert_eq!(
        run(&mut a, "COMMIT TRANSACTION;"),
        vec![Event::Commit, Event::Done]
    );
    run(&mut b, "COMMIT TRANSACTION;");
    assert_eq!(committed(&engine), vec![vec![1], vec![0]]);
}

#[test]
fn an_update_of_a_held_row_times_out() {
    times_out_after_500_ms("UPDATE dbo.t SET v = 2 WHERE id = 1");
}

#[test]
fn a_delete_of_a_held_row_times_out() {
    times_out_after_500_ms("DELETE FROM dbo.t WHERE id = 1");
}

#[test]
fn an_updlock_read_of_a_held_row_times_out() {
    times_out_after_500_ms("SELECT v FROM dbo.t WITH (UPDLOCK) WHERE id = 1");
}

/// `SET LOCK_TIMEOUT 0` refuses the lock without waiting.
#[test]
fn a_zero_timeout_refuses_at_once() {
    let engine = engine();
    let (mut a, mut b, events, elapsed) = blocked(
        &engine,
        "SET LOCK_TIMEOUT 0;",
        "UPDATE dbo.t SET v = 2 WHERE id = 1",
    );
    assert_eq!(errors(&events), vec![(1222, 51)], "{events:#?}");
    assert_eq!(ints(&events), vec![vec![1, 1, 1222]]);
    assert!(
        elapsed < Duration::from_millis(300),
        "1222 came back after {elapsed:?}"
    );
    run(&mut a, "COMMIT TRANSACTION;");
    run(&mut b, "COMMIT TRANSACTION;");
    assert_eq!(committed(&engine), vec![vec![1], vec![0]]);
}

/// `SET LOCK_TIMEOUT -1` waits for the `COMMIT` of A, then the `UPDATE` of B goes through.
#[test]
fn an_infinite_timeout_waits_for_the_commit() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "BEGIN TRANSACTION; UPDATE dbo.t SET v = 1 WHERE id = 1;",
    );
    run(
        &mut b,
        "SET LOCK_TIMEOUT 500; SET LOCK_TIMEOUT -1; BEGIN TRANSACTION;",
    );
    let (sender, receiver) = mpsc::channel();
    let waiting = thread::spawn(move || {
        let events = run_on(
            &mut b,
            "UPDATE dbo.t SET v = 2 WHERE id = 1; SELECT @@TRANCOUNT, @@ERROR;",
        );
        let _ = sender.send(());
        (b, events)
    });
    assert!(
        receiver.recv_timeout(Duration::from_millis(800)).is_err(),
        "B waits past the timeout it first set"
    );
    run(&mut a, "COMMIT TRANSACTION;");
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("B goes through once A has committed");
    let (mut b, events) = waiting.join().expect("thread of B");
    assert_eq!(errors(&events), Vec::<(u32, u8)>::new(), "{events:#?}");
    assert_eq!(ints(&events), vec![vec![1, 0]]);
    run(&mut b, "COMMIT TRANSACTION;");
    assert_eq!(committed(&engine), vec![vec![2], vec![0]]);
}
