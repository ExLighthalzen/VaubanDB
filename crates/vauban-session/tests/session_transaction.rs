//! Integration tests of the session transaction: a transaction a `BEGIN TRANSACTION` opened
//! outlives the statement and the batch, its ENVCHANGE tokens are the ones the server sends,
//! and dropping the session rolls it back.
//!
//! No socket and no TDS: the batches run on `Session::run_batch` through a `ResultSink` that
//! keeps every call, as `tests/run_batch_pipeline.rs` does.

use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

/// SPID of every session built here.
const SPID: i16 = 61;

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

/// An in-memory engine with the built-in functions registered.
fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
}

/// A session on `engine`.
fn session(engine: &Arc<Engine>) -> Session {
    Session::new(Arc::clone(engine), SessionState::new(SPID))
}

/// Runs `text` on `session` and returns what the sink received.
fn run_on(session: &mut Session, text: &str) -> Vec<Event> {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("a batch reports its errors through the sink, not through its result");
    sink.0
}

/// Runs `text` and fails when the batch raised an error event.
fn run(session: &mut Session, text: &str) -> Vec<Event> {
    let events = run_on(session, text);
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "`{text}` raised an error: {events:#?}"
    );
    events
}

/// The rows of `events`.
fn rows(events: &[Event]) -> Vec<&Vec<Value>> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => Some(row),
            _ => None,
        })
        .collect()
}

/// The single `int` of the one row of `events`, or a panic.
fn one_int(events: &[Event]) -> i32 {
    let rows = rows(events);
    let [row] = rows.as_slice() else {
        panic!("expected exactly one row, got {events:#?}");
    };
    match row.as_slice() {
        [Value::I32(n)] => *n,
        other => panic!("expected one int, got {other:?}"),
    }
}

/// The descriptor of the transaction ENVCHANGE `events` carries: `'b'` for the begin one,
/// `'c'` for the commit one, `'r'` for the rollback one.
fn descriptor(events: &[Event], kind: char) -> u64 {
    for event in events {
        if let Event::EnvChange(change) = event {
            let found = match (kind, change) {
                ('b', EnvChange::BeginTransaction(d)) => Some(*d),
                ('c', EnvChange::CommitTransaction(d)) => Some(*d),
                ('r', EnvChange::RollbackTransaction(d)) => Some(*d),
                _ => None,
            };
            if let Some(descriptor) = found {
                return descriptor;
            }
        }
    }
    panic!("no transaction ENVCHANGE of kind {kind:?} in {events:#?}");
}

#[test]
fn transaction_survives_the_batch() {
    let engine = engine();
    let mut s = session(&engine);

    let begin = run(&mut s, "BEGIN TRAN");
    assert_ne!(descriptor(&begin, 'b'), 0);
    assert_eq!(one_int(&run(&mut s, "SELECT @@TRANCOUNT")), 1);

    let commit = run(&mut s, "COMMIT");
    assert!(matches!(
        commit
            .iter()
            .find(|event| matches!(event, Event::EnvChange(_))),
        Some(Event::EnvChange(EnvChange::CommitTransaction(_)))
    ));
    assert_eq!(one_int(&run(&mut s, "SELECT @@TRANCOUNT")), 0);
}

#[test]
fn writes_are_visible_only_after_the_commit() {
    let engine = engine();
    let mut a = session(&engine);
    run(&mut a, "CREATE TABLE dbo.t (a int NOT NULL)");
    run(&mut a, "BEGIN TRAN");
    run(&mut a, "INSERT INTO dbo.t (a) VALUES (1)");

    let mut b = session(&engine);
    assert_eq!(
        rows(&run(&mut b, "SELECT a FROM dbo.t")).len(),
        0,
        "another session reads nothing before the commit"
    );

    run(&mut a, "COMMIT");
    assert_eq!(
        rows(&run(&mut b, "SELECT a FROM dbo.t")).len(),
        1,
        "the committed row is visible"
    );
}

#[test]
fn envchange_tokens_carry_the_transaction_descriptor() {
    let engine = engine();
    let mut s = session(&engine);

    let begin = run(&mut s, "BEGIN TRAN");
    let first = descriptor(&begin, 'b');
    assert_ne!(first, 0);

    let commit = run(&mut s, "COMMIT");
    assert_eq!(descriptor(&commit, 'c'), first);

    let begin = run(&mut s, "BEGIN TRAN");
    let second = descriptor(&begin, 'b');
    assert_ne!(second, 0);

    let rollback = run(&mut s, "ROLLBACK");
    assert_eq!(descriptor(&rollback, 'r'), second);
}

#[test]
fn open_transaction_at_end_of_batch() {
    let engine = engine();
    let mut s = session(&engine);

    let events = run_on(&mut s, "BEGIN TRAN; BEGIN TRAN; BEGIN TRAN;");
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "a batch that ends with a transaction open raises nothing: {events:#?}"
    );
    assert_eq!(
        one_int(&run(&mut s, "SELECT @@TRANCOUNT")),
        3,
        "the transaction outlives the batch"
    );

    run(&mut s, "ROLLBACK");
    assert_eq!(one_int(&run(&mut s, "SELECT @@TRANCOUNT")), 0);
}

#[test]
fn disconnect_rolls_back() {
    let engine = engine();
    {
        let mut a = session(&engine);
        run(&mut a, "CREATE TABLE dbo.t (a int NOT NULL)");
        run(&mut a, "BEGIN TRAN");
        run(&mut a, "INSERT INTO dbo.t (a) VALUES (1)");
        assert_eq!(
            engine.txn.active_sessions().len(),
            1,
            "one transaction open"
        );
    }

    assert!(
        engine.txn.active_sessions().is_empty(),
        "dropping the session rolled the transaction back"
    );
    let mut b = session(&engine);
    assert_eq!(rows(&run(&mut b, "SELECT a FROM dbo.t")).len(), 0);
}
