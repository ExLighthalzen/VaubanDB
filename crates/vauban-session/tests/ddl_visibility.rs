//! Integration tests of a table seen by two sessions while one of them runs a `CREATE TABLE`
//! or a `DROP TABLE` it has not committed: the other session sees the table as it was
//! committed, and a `ROLLBACK TRANSACTION <savepoint>` brings back a table the transaction
//! dropped after the savepoint.
//!
//! No socket and no TDS: the batches run on `Session::run_batch` through a `ResultSink` that
//! keeps the rows and the errors, as `tests/session_transaction.rs` does.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

/// The rows and the errors a sink receives, in order.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Row(Vec<Value>),
    Error(SqlError),
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
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.clone()));
        Ok(())
    }
    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        Ok(())
    }
    fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        Ok(())
    }
    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

/// An in-memory engine with the built-in functions registered.
fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
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
        "`{text}` raised an error: {events:#?}"
    );
    events
}

/// The `int` of the first column of each row of `events`.
fn ints(events: &[Event]) -> Vec<i32> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => match row.first() {
                Some(Value::I32(value)) => Some(*value),
                other => panic!("an int was expected, got {other:?}"),
            },
            Event::Error(_) => None,
        })
        .collect()
}

/// The numbers of the errors of `events`.
fn errors(events: &[Event]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some(err.number),
            Event::Row(_) => None,
        })
        .collect()
}

#[test]
fn a_table_whose_create_is_not_committed_is_not_resolved_by_another_session() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut b,
        "CREATE TABLE dbo.b_own (a int); INSERT INTO dbo.b_own VALUES (1), (2);",
    );

    run(
        &mut a,
        "BEGIN TRANSACTION; CREATE TABLE dbo.x (a int); INSERT INTO dbo.x VALUES (5);",
    );
    assert_eq!(ints(&run(&mut a, "SELECT a FROM dbo.x;")), vec![5]);
    assert_eq!(errors(&run_on(&mut b, "SELECT a FROM dbo.x;")), vec![208]);
    assert_eq!(
        errors(&run_on(&mut b, "INSERT INTO dbo.x VALUES (9);")),
        vec![208]
    );
    let object_id = run(&mut b, "SELECT OBJECT_ID('dbo.x');");
    assert_eq!(object_id, vec![Event::Row(vec![Value::Null])]);

    run(&mut a, "ROLLBACK TRANSACTION;");
    assert_eq!(
        ints(&run(&mut b, "SELECT a FROM dbo.b_own ORDER BY a;")),
        vec![1, 2],
        "the rows of the other session are left alone"
    );
    assert_eq!(errors(&run_on(&mut a, "SELECT a FROM dbo.x;")), vec![208]);
    assert_eq!(errors(&run_on(&mut b, "SELECT a FROM dbo.x;")), vec![208]);
}

#[test]
fn a_table_whose_create_is_committed_is_resolved_by_another_session() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "BEGIN TRANSACTION; CREATE TABLE dbo.x (a int); INSERT INTO dbo.x VALUES (5);",
    );
    assert_eq!(errors(&run_on(&mut b, "SELECT a FROM dbo.x;")), vec![208]);
    run(&mut a, "COMMIT TRANSACTION;");
    assert_eq!(ints(&run(&mut b, "SELECT a FROM dbo.x;")), vec![5]);
}

#[test]
fn a_read_of_a_table_whose_drop_is_not_committed_waits_then_reads_it() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.y (a int); INSERT INTO dbo.y VALUES (1), (2);",
    );
    run(&mut a, "BEGIN TRANSACTION; DROP TABLE dbo.y;");

    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let events = run_on(&mut b, "SELECT a FROM dbo.y ORDER BY a;");
        let _ = sender.send(());
        (b, events)
    });
    assert!(
        receiver.recv_timeout(Duration::from_millis(300)).is_err(),
        "the read waits for the transaction that dropped the table"
    );
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (_b, events) = reader.join().expect("reader thread");
    assert_eq!(ints(&events), vec![1, 2]);
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        ints(&run(&mut a, "SELECT a FROM dbo.y ORDER BY a;")),
        vec![1, 2],
        "the table is intact"
    );
}

#[test]
fn a_savepoint_rollback_brings_a_dropped_table_back() {
    let engine = engine();
    let mut a = session(&engine, 61);
    run(
        &mut a,
        "CREATE TABLE dbo.z (a int); INSERT INTO dbo.z VALUES (3);",
    );
    let events = run(
        &mut a,
        "BEGIN TRANSACTION; SAVE TRANSACTION s; DROP TABLE dbo.z; \
         ROLLBACK TRANSACTION s; SELECT a FROM dbo.z;",
    );
    assert_eq!(ints(&events), vec![3]);
    assert_eq!(ints(&run(&mut a, "SELECT a FROM dbo.z;")), vec![3]);
    run(&mut a, "COMMIT TRANSACTION;");
    assert_eq!(ints(&run(&mut a, "SELECT a FROM dbo.z;")), vec![3]);

    // The table the savepoint brought back can be dropped again in the same transaction.
    run(
        &mut a,
        "BEGIN TRANSACTION; SAVE TRANSACTION s; DROP TABLE dbo.z; \
         ROLLBACK TRANSACTION s;",
    );
    run(&mut a, "DROP TABLE dbo.z;");
    run(&mut a, "COMMIT TRANSACTION;");
    assert_eq!(errors(&run_on(&mut a, "SELECT a FROM dbo.z;")), vec![208]);
}

#[test]
fn an_alter_in_an_open_transaction_is_bound_by_the_next_statements() {
    let engine = engine();
    let mut a = session(&engine, 61);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
    );
    let events = run(
        &mut a,
        "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD c int; SELECT c FROM dbo.t;",
    );
    assert_eq!(events, vec![Event::Row(vec![Value::Null])], "same batch");
    run(&mut a, "UPDATE dbo.t SET c = 7;");
    assert_eq!(
        ints(&run(&mut a, "SELECT c FROM dbo.t;")),
        vec![7],
        "next batch"
    );
    run(&mut a, "COMMIT TRANSACTION;");
    assert_eq!(ints(&run(&mut a, "SELECT c FROM dbo.t;")), vec![7]);
}

#[test]
fn a_table_created_in_an_open_transaction_is_bound_by_the_next_statements() {
    let engine = engine();
    let mut a = session(&engine, 61);
    run(
        &mut a,
        "BEGIN TRANSACTION; CREATE TABLE dbo.n (a int); INSERT INTO dbo.n VALUES (4);",
    );
    run(&mut a, "INSERT INTO dbo.n VALUES (5);");
    assert_eq!(
        ints(&run(&mut a, "SELECT a FROM dbo.n ORDER BY a;")),
        vec![4, 5]
    );
    run(&mut a, "COMMIT TRANSACTION;");
    assert_eq!(
        ints(&run(&mut a, "SELECT a FROM dbo.n ORDER BY a;")),
        vec![4, 5]
    );
}

#[test]
fn a_table_dropped_in_an_open_transaction_is_208_for_the_next_statements() {
    let engine = engine();
    let mut a = session(&engine, 61);
    run(
        &mut a,
        "CREATE TABLE dbo.y (a int); INSERT INTO dbo.y VALUES (1);",
    );
    run(&mut a, "BEGIN TRANSACTION; DROP TABLE dbo.y;");
    assert_eq!(errors(&run_on(&mut a, "SELECT a FROM dbo.y;")), vec![208]);
    run(&mut a, "ROLLBACK TRANSACTION;");
    assert_eq!(ints(&run(&mut a, "SELECT a FROM dbo.y;")), vec![1]);
}

/// The names of `events`, one per row, as text.
fn names(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => match row.first() {
                Some(Value::String(text)) => Some(text.text.clone()),
                other => panic!("a name was expected, got {other:?}"),
            },
            Event::Error(_) => None,
        })
        .collect()
}

/// A creates `p`, B creates `q`, then each creates the other's name: the two waits cross,
/// one of them is chosen as the deadlock victim (1205) and its transaction is rolled back,
/// with the table it created, so the other creates both names without an error. Both
/// sessions then commit what they still hold; `sys.tables` holds each name once.
#[test]
fn a_crossed_create_deadlock_leaves_one_table_per_name() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(&mut a, "BEGIN TRANSACTION; CREATE TABLE dbo.p (a int);");
    run(&mut b, "BEGIN TRANSACTION; CREATE TABLE dbo.q (a int);");

    let (sender, receiver) = mpsc::channel();
    let crossing = thread::spawn(move || {
        let events = run_on(&mut a, "CREATE TABLE dbo.q (a int);");
        let _ = sender.send(());
        (a, events)
    });
    assert!(
        receiver.recv_timeout(Duration::from_millis(300)).is_err(),
        "A waits for the transaction that created q"
    );
    let b_events = run_on(&mut b, "CREATE TABLE dbo.p (a int);");
    let (mut a, a_events) = crossing.join().expect("thread of A");
    let answered = [errors(&a_events), errors(&b_events)].concat();
    assert_eq!(answered, vec![1205], "A: {a_events:#?}, B: {b_events:#?}");
    let victim_is_a = errors(&a_events) == [1205];
    let victim = if victim_is_a { &mut a } else { &mut b };
    assert_eq!(
        ints(&run(victim, "SELECT @@TRANCOUNT;")),
        vec![0],
        "the victim's transaction is rolled back"
    );

    for session in [&mut a, &mut b] {
        let _ = run_on(session, "IF @@TRANCOUNT > 0 COMMIT TRANSACTION;");
    }
    let listed = names(&run(
        &mut a,
        "SELECT name FROM sys.tables WHERE name IN ('p', 'q') ORDER BY name;",
    ));
    assert_eq!(listed, vec!["p", "q"], "one table per name");
}
