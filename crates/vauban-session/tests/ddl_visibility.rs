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

/// Runs `text` on `session` on a thread of its own, and checks that it is still waiting
/// after 300 ms; [`Waiting::finish`] hands back the session and what the sink received.
struct Waiting {
    done: mpsc::Receiver<()>,
    thread: thread::JoinHandle<(Session, Vec<Event>)>,
}

impl Waiting {
    fn start(mut session: Session, text: &'static str) -> Self {
        let (sender, done) = mpsc::channel();
        let thread = thread::spawn(move || {
            let events = run_on(&mut session, text);
            let _ = sender.send(());
            (session, events)
        });
        assert!(
            done.recv_timeout(Duration::from_millis(300)).is_err(),
            "`{text}` waits for the transaction of the other session"
        );
        Waiting { done, thread }
    }

    fn still_waits(&self) -> bool {
        self.done.recv_timeout(Duration::from_millis(300)).is_err()
    }

    fn finish(self) -> (Session, Vec<Event>) {
        let _ = self.done.recv_timeout(Duration::from_secs(10));
        self.thread.join().expect("thread of the waiting session")
    }
}

/// The number and the state of each error of `events`.
fn numbers_and_states(events: &[Event]) -> Vec<(u32, u8)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some((err.number, err.state)),
            Event::Row(_) => None,
        })
        .collect()
}

/// A `CREATE TABLE` another session has not committed: a `SELECT` and an `INSERT` of the
/// table wait for its end, and answer 208 once it is rolled back; the rows of another table
/// are left alone.
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
    assert_eq!(
        ints(&run(&mut b, "SELECT a FROM dbo.b_own ORDER BY a;")),
        vec![1, 2],
        "another table answers at once"
    );
    let waiting = Waiting::start(b, "SELECT a FROM dbo.x; INSERT INTO dbo.x VALUES (9);");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert_eq!(errors(&events), vec![208]);

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
    let b = session(&engine, 62);
    run(
        &mut a,
        "BEGIN TRANSACTION; CREATE TABLE dbo.x (a int); INSERT INTO dbo.x VALUES (5);",
    );
    let waiting = Waiting::start(b, "SELECT a FROM dbo.x;");
    run(&mut a, "COMMIT TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert_eq!(ints(&events), vec![5], "{events:#?}");
}

/// An `ALTER TABLE` another session has not committed: an `INSERT` waits for it, then writes
/// its row in the table the `ALTER` left, where it stays.
#[test]
fn an_insert_waits_for_an_uncommitted_alter_and_its_row_stays() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
    );
    run(&mut a, "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD c int;");
    let waiting = Waiting::start(b, "INSERT INTO dbo.t (a) VALUES (2);");
    run(&mut a, "COMMIT TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        ints(&run(&mut a, "SELECT a FROM dbo.t ORDER BY a;")),
        vec![1, 2],
        "the row of the waiting INSERT is in the altered table"
    );
    assert_eq!(
        run(&mut a, "SELECT c FROM dbo.t WHERE a = 2;"),
        vec![Event::Row(vec![Value::Null])]
    );
}

/// A `DROP TABLE` another session has not committed: an `INSERT` waits for it, then answers
/// 208 once it is committed, and writes its row once it is rolled back.
#[test]
fn an_insert_waits_for_an_uncommitted_drop_then_answers_208_or_writes() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.y (a int); INSERT INTO dbo.y VALUES (1);",
    );

    run(&mut a, "BEGIN TRANSACTION; DROP TABLE dbo.y;");
    let waiting = Waiting::start(b, "INSERT INTO dbo.y VALUES (2);");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        ints(&run(&mut a, "SELECT a FROM dbo.y ORDER BY a;")),
        vec![1, 2]
    );

    run(&mut a, "BEGIN TRANSACTION; DROP TABLE dbo.y;");
    let waiting = Waiting::start(b, "INSERT INTO dbo.y VALUES (3);");
    run(&mut a, "COMMIT TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert_eq!(errors(&events), vec![208], "{events:#?}");
}

/// Under `SET LOCK_TIMEOUT 100`, set by an earlier batch, a statement that names a table
/// another session is dropping answers 1222 state 56, and a `CREATE TABLE` of a name another
/// session is creating 1222 state 47. The transaction of the dropping session goes on.
#[test]
fn a_wait_at_binding_under_a_lock_timeout_is_1222() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.y (a int); INSERT INTO dbo.y VALUES (1);",
    );
    run(&mut b, "SET LOCK_TIMEOUT 100;");

    run(&mut a, "BEGIN TRANSACTION; DROP TABLE dbo.y;");
    for text in [
        "SELECT a FROM dbo.y;",
        "SELECT a FROM dbo.y WITH (NOLOCK);",
        "INSERT INTO dbo.y VALUES (2);",
    ] {
        let events = run_on(&mut b, text);
        assert_eq!(numbers_and_states(&events), vec![(1222, 56)], "{text}");
    }
    run(&mut a, "ROLLBACK TRANSACTION;");
    assert_eq!(ints(&run(&mut b, "SELECT a FROM dbo.y;")), vec![1]);

    run(&mut a, "BEGIN TRANSACTION; CREATE TABLE dbo.x (a int);");
    let events = run_on(&mut b, "CREATE TABLE dbo.x (a int);");
    assert_eq!(numbers_and_states(&events), vec![(1222, 47)]);
    run(&mut a, "COMMIT TRANSACTION;");
    assert!(errors(&run_on(&mut b, "SELECT a FROM dbo.x;")).is_empty());
}

/// A `CREATE TABLE` of a name another session is dropping waits for that `DROP`, then
/// creates the table once it is committed.
#[test]
fn a_create_of_a_name_another_session_drops_waits_then_creates() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    run(&mut a, "CREATE TABLE dbo.x (a int);");
    run(&mut a, "BEGIN TRANSACTION; DROP TABLE dbo.x;");
    let waiting = Waiting::start(
        b,
        "CREATE TABLE dbo.x (b int); INSERT INTO dbo.x VALUES (4);",
    );
    run(&mut a, "COMMIT TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(ints(&run(&mut a, "SELECT b FROM dbo.x;")), vec![4]);
}

/// An `ALTER TABLE` of a referenced table another session has not committed: an `INSERT`
/// into the referencing table with a valid key waits, then succeeds once the `ALTER` is
/// committed or rolled back, and answers 1222 under a lock timeout; a `SELECT` of the
/// altered table waits too, then reads its rows.
#[test]
fn a_write_checked_against_a_table_being_altered_waits() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.x (id int PRIMARY KEY); INSERT INTO dbo.x VALUES (1), (2);          CREATE TABLE dbo.y (id int, x_id int REFERENCES dbo.x (id));",
    );

    for (alter, insert, outcome) in [
        (
            "BEGIN TRANSACTION; ALTER TABLE dbo.x ADD c int;",
            "INSERT INTO dbo.y VALUES (1, 1);",
            "COMMIT",
        ),
        (
            "BEGIN TRANSACTION; ALTER TABLE dbo.x ADD c2 int;",
            "INSERT INTO dbo.y VALUES (2, 2);",
            "ROLLBACK",
        ),
    ] {
        run(&mut a, alter);
        let waiting = Waiting::start(b, insert);
        run(&mut a, &format!("{outcome} TRANSACTION;"));
        let (back, events) = waiting.finish();
        b = back;
        assert!(errors(&events).is_empty(), "{outcome}: {events:#?}");
    }
    assert_eq!(
        ints(&run(&mut a, "SELECT x_id FROM dbo.y ORDER BY x_id;")),
        vec![1, 2]
    );

    run(&mut a, "ALTER TABLE dbo.x DROP COLUMN c;");
    run(&mut a, "BEGIN TRANSACTION; ALTER TABLE dbo.x ADD d int;");
    let waiting = Waiting::start(b, "SELECT id FROM dbo.x ORDER BY id;");
    run(&mut a, "COMMIT TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert_eq!(ints(&events), vec![1, 2]);

    run(&mut b, "SET LOCK_TIMEOUT 100;");
    run(&mut a, "BEGIN TRANSACTION; ALTER TABLE dbo.x ADD e int;");
    let events = run_on(&mut b, "INSERT INTO dbo.y VALUES (3, 1);");
    assert_eq!(numbers_and_states(&events), vec![(1222, 56)]);
    run(&mut a, "ROLLBACK TRANSACTION;");
    assert_eq!(
        ints(&run(&mut b, "SELECT x_id FROM dbo.y ORDER BY x_id;")),
        vec![1, 2]
    );
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

/// The tables a write checks are waited for, those a read does not check are not: during an
/// uncommitted `ALTER TABLE` of the referenced table, a `SELECT` of the referencing table
/// answers at once; during one of the referencing table, a `DELETE` of the referenced table
/// waits.
#[test]
fn a_read_does_not_wait_for_the_tables_a_write_would_check() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.x (id int PRIMARY KEY); INSERT INTO dbo.x VALUES (1), (2); \
         CREATE TABLE dbo.y (id int, x_id int REFERENCES dbo.x (id)); \
         INSERT INTO dbo.y VALUES (1, 1);",
    );

    run(&mut a, "BEGIN TRANSACTION; ALTER TABLE dbo.x ADD c int;");
    assert_eq!(ints(&run(&mut b, "SELECT x_id FROM dbo.y;")), vec![1]);
    run(&mut a, "ROLLBACK TRANSACTION;");

    run(&mut a, "BEGIN TRANSACTION; ALTER TABLE dbo.y ADD c int;");
    let waiting = Waiting::start(b, "DELETE FROM dbo.x WHERE id = 2;");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(ints(&run(&mut a, "SELECT id FROM dbo.x;")), vec![1]);
}

/// Runs `text` on `session` on a thread of its own, without checking that it waits; the
/// batches below open with a `WAITFOR` that keeps their statements bound and not yet run.
struct Delayed {
    done: mpsc::Receiver<()>,
    thread: thread::JoinHandle<(Session, Vec<Event>)>,
}

impl Delayed {
    fn start(mut session: Session, text: &'static str) -> Self {
        let (sender, done) = mpsc::channel();
        let thread = thread::spawn(move || {
            let events = run_on(&mut session, text);
            let _ = sender.send(());
            (session, events)
        });
        // The batch is bound by now and sits in its `WAITFOR`.
        thread::sleep(Duration::from_millis(150));
        Delayed { done, thread }
    }

    /// `true` when the batch has not answered within `ms` milliseconds.
    fn still_running_after(&self, ms: u64) -> bool {
        self.done.recv_timeout(Duration::from_millis(ms)).is_err()
    }

    fn finish(self) -> (Session, Vec<Event>) {
        let _ = self.done.recv_timeout(Duration::from_secs(10));
        self.thread.join().expect("thread of the delayed session")
    }
}

/// An `INSERT` bound before another session starts an `ALTER TABLE` of its table, and run
/// while that `ALTER` is not committed, waits for it, then writes its row in the table the
/// `ALTER` left; once the `ALTER` is rolled back, in the table as it was.
#[test]
fn an_insert_bound_before_an_alter_waits_and_keeps_its_row() {
    for (end, expected) in [("COMMIT", vec![1, 2]), ("ROLLBACK", vec![1, 2])] {
        let engine = engine();
        let mut a = session(&engine, 61);
        let b = session(&engine, 62);
        run(
            &mut a,
            "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
        );
        let delayed = Delayed::start(
            b,
            "WAITFOR DELAY '00:00:00.400'; INSERT INTO dbo.t (a) VALUES (2);",
        );
        run(&mut a, "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD c int;");
        assert!(
            delayed.still_running_after(900),
            "{end}: the INSERT waits for the ALTER"
        );
        run(&mut a, &format!("{end} TRANSACTION;"));
        let (_b, events) = delayed.finish();
        assert!(errors(&events).is_empty(), "{end}: {events:#?}");
        assert_eq!(
            ints(&run(&mut a, "SELECT a FROM dbo.t ORDER BY a;")),
            expected,
            "{end}"
        );
    }
}

/// A statement bound before another session commits an `ALTER TABLE` or a `DROP TABLE` of
/// its table, and run after that commit, is bound again: the `INSERT` writes its row in the
/// altered table, and a read of the dropped table answers 208.
#[test]
fn an_insert_bound_before_a_committed_alter_is_bound_again() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1); \
         CREATE TABLE dbo.y (a int); INSERT INTO dbo.y VALUES (1);",
    );
    let delayed = Delayed::start(
        b,
        "WAITFOR DELAY '00:00:00.400'; INSERT INTO dbo.t (a) VALUES (2); SELECT a FROM dbo.t ORDER BY a;",
    );
    run(&mut a, "ALTER TABLE dbo.t ADD c int;");
    let (b, events) = delayed.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(ints(&events), vec![1, 2]);
    assert_eq!(
        run(&mut a, "SELECT c FROM dbo.t WHERE a = 2;"),
        vec![Event::Row(vec![Value::Null])]
    );

    let delayed = Delayed::start(
        b,
        "WAITFOR DELAY '00:00:00.400'; SELECT a FROM dbo.y; SELECT 7;",
    );
    run(&mut a, "DROP TABLE dbo.y;");
    let (_b, events) = delayed.finish();
    assert_eq!(errors(&events), vec![208], "{events:#?}");
}

/// A block whose `INSERT`, on line 5, runs after a read of `dbo.w` that waits for the
/// other session, written alone, then after a `CREATE TABLE` that makes the batch bind one
/// statement at a time. `SELECT 0` and the read (5) run before the `INSERT`, `SELECT 3` and
/// `SELECT 4` after it.
const HELD_BY_AN_IF: [&str; 2] = [
    "IF 1 = 1\nBEGIN\n  SELECT 0;\n  SELECT a FROM dbo.w;\n  \
     INSERT INTO dbo.t (a) VALUES (2);\n  SELECT 3;\nEND\nSELECT 4;",
    "CREATE TABLE dbo.o (a int);\nIF 1 = 1\nBEGIN\n  SELECT 0;\n  SELECT a FROM dbo.w;\n  \
     INSERT INTO dbo.t (a) VALUES (2);\n  SELECT 3;\nEND\nSELECT 4;",
];

/// A loop of two turns whose `INSERT`, on line 5, writes `10 + @i`: the first turn ends on
/// a read of `dbo.w` that waits for the other session, the second runs after it.
const HELD_BY_A_WHILE: &str = "DECLARE @i int = 0; DECLARE @n int;\nWHILE @i < 2\nBEGIN\n  \
     SELECT @i;\n  INSERT INTO dbo.t (a) VALUES (10 + @i);\n  SET @i = @i + 1;\n  \
     IF @i = 1 SELECT @n = a FROM dbo.w;\nEND\nSELECT 9;";

/// Runs `text` on a session of `engine` while another one, in a transaction, holds the row
/// of `dbo.w` that `text` reads, runs `ddl` while `text` waits for that row, then commits;
/// answers what the first session received.
fn held_while_another_session_commits(
    engine: &Arc<Engine>,
    text: &'static str,
    ddl: &str,
) -> Vec<Event> {
    let mut a = session(engine, 61);
    let b = session(engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.w (a int); INSERT INTO dbo.w VALUES (5); \
         BEGIN TRANSACTION; UPDATE dbo.w SET a = 5;",
    );
    let waiting = Waiting::start(b, text);
    run(&mut a, ddl);
    run(&mut a, "COMMIT TRANSACTION;");
    waiting.finish().1
}

/// The number and the line of each error of `events`.
fn numbers_and_lines(events: &[Event]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some((err.number, err.line)),
            Event::Row(_) => None,
        })
        .collect()
}

/// A statement held by an `IF` or by the second turn of a `WHILE`, whose table another
/// session rebuilds by an `ALTER TABLE` it commits while the statement waits, is bound
/// again with the statement that holds it and writes its row in the altered table. The
/// statements that ran before it, the first turn of the loop included, are not run again.
#[test]
fn a_held_statement_whose_table_was_rebuilt_is_bound_again() {
    for text in HELD_BY_AN_IF {
        let engine = engine();
        let mut a = session(&engine, 61);
        run(
            &mut a,
            "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
        );
        let events =
            held_while_another_session_commits(&engine, text, "ALTER TABLE dbo.t ADD c int;");
        assert!(errors(&events).is_empty(), "{text}: {events:#?}");
        assert_eq!(ints(&events), vec![0, 5, 3, 4], "{text}");
        assert_eq!(
            run(&mut a, "SELECT a, c FROM dbo.t ORDER BY a;"),
            vec![
                Event::Row(vec![Value::I32(1), Value::Null]),
                Event::Row(vec![Value::I32(2), Value::Null]),
            ],
            "{text}"
        );
    }

    let other = engine();
    let mut a = session(&other, 61);
    run(&mut a, "CREATE TABLE dbo.t (a int);");
    let events =
        held_while_another_session_commits(&other, HELD_BY_A_WHILE, "ALTER TABLE dbo.t ADD c int;");
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(ints(&events), vec![0, 1, 9]);
    assert_eq!(
        run(&mut a, "SELECT a, c FROM dbo.t ORDER BY a;"),
        vec![
            Event::Row(vec![Value::I32(10), Value::Null]),
            Event::Row(vec![Value::I32(11), Value::Null]),
        ]
    );
}

/// The same statements, when the other session drops the table: the new binding of the
/// statement that holds them answers 208 on the line of the `INSERT`, and the batch stops
/// there, after what ran before it.
#[test]
fn a_held_statement_whose_table_was_dropped_is_208() {
    let first = engine();
    let mut a = session(&first, 61);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
    );
    let events = held_while_another_session_commits(&first, HELD_BY_AN_IF[0], "DROP TABLE dbo.t;");
    assert_eq!(numbers_and_lines(&events), vec![(208, 5)], "{events:#?}");
    assert_eq!(ints(&events), vec![0, 5]);

    let other = engine();
    let mut a = session(&other, 61);
    run(&mut a, "CREATE TABLE dbo.t (a int);");
    let events = held_while_another_session_commits(&other, HELD_BY_A_WHILE, "DROP TABLE dbo.t;");
    assert_eq!(numbers_and_lines(&events), vec![(208, 5)], "{events:#?}");
    assert_eq!(ints(&events), vec![0, 1]);
}

/// A block whose `IF`, on line 4, tests the rows of `dbo.t` after a read of `dbo.w` that
/// waits for the other session: `SELECT 1` when `dbo.t` holds one row, `SELECT 2` otherwise.
const CONDITION_HELD_BY_AN_IF: &str = "IF 1 = 1\nBEGIN\n  SELECT a FROM dbo.w;\n  \
     IF (SELECT COUNT(*) FROM dbo.t) = 1 SELECT 1; ELSE SELECT 2;\nEND\nSELECT 4;";

/// A loop whose condition, on line 2, counts the rows of `dbo.t`: its first test runs before
/// a read of `dbo.w` that waits for the other session, its second test right after it.
const CONDITION_OF_A_WHILE: &str = "DECLARE @i int = 0;\nWHILE (SELECT COUNT(*) FROM dbo.t) > @i\n\
     BEGIN\n  SET @i = @i + 1;\n  IF @i = 1 SELECT a FROM dbo.w;\nEND\nSELECT @i;";

/// The condition of an `IF` held by a block, or of a `WHILE`, tested after another session
/// rebuilt its table by an `ALTER TABLE` and wrote a second row, reads the rebuilt table:
/// the statement that holds it is bound again.
#[test]
fn a_condition_whose_table_was_rebuilt_reads_the_new_table() {
    for (text, expected) in [
        (CONDITION_HELD_BY_AN_IF, vec![5, 2, 4]),
        (CONDITION_OF_A_WHILE, vec![5, 2]),
    ] {
        let engine = engine();
        let mut a = session(&engine, 61);
        run(
            &mut a,
            "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
        );
        let events = held_while_another_session_commits(
            &engine,
            text,
            "ALTER TABLE dbo.t ADD c int; INSERT INTO dbo.t (a) VALUES (2);",
        );
        assert!(errors(&events).is_empty(), "{text}: {events:#?}");
        assert_eq!(ints(&events), expected, "{text}");
    }
}

/// The same conditions, when the other session drops the table: 208 on the line of the
/// statement that holds the condition, and the batch stops there, after what ran before.
#[test]
fn a_condition_whose_table_was_dropped_is_208() {
    for (text, line) in [(CONDITION_HELD_BY_AN_IF, 4), (CONDITION_OF_A_WHILE, 2)] {
        let engine = engine();
        let mut a = session(&engine, 61);
        run(
            &mut a,
            "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
        );
        let events = held_while_another_session_commits(&engine, text, "DROP TABLE dbo.t;");
        assert_eq!(
            numbers_and_lines(&events),
            vec![(208, line)],
            "{text}: {events:#?}"
        );
        assert_eq!(ints(&events), vec![5], "{text}");
    }
}

/// A block that reads `dbo.t1`, waiting for the row another session holds, then `dbo.t2`:
/// that session alters `dbo.t2` meanwhile without waiting, since the read of `dbo.t1` holds
/// the schema of `dbo.t1` alone, then commits; the block reads both tables, the second
/// one as altered.
#[test]
fn a_held_statement_holds_the_schema_of_its_own_tables_only() {
    for text in [
        "IF 1 = 1\nBEGIN\n  SELECT a FROM dbo.t1;\n  SELECT a FROM dbo.t2;\nEND",
        "DECLARE @i int = 0;\nWHILE @i < 1\nBEGIN\n  SELECT a FROM dbo.t1;\n  \
         SELECT a FROM dbo.t2;\n  SET @i = @i + 1;\nEND",
    ] {
        let engine = engine();
        let mut a = session(&engine, 61);
        let b = session(&engine, 62);
        run(
            &mut a,
            "CREATE TABLE dbo.t1 (a int); INSERT INTO dbo.t1 VALUES (1); \
             CREATE TABLE dbo.t2 (a int); INSERT INTO dbo.t2 VALUES (2); \
             BEGIN TRANSACTION; UPDATE dbo.t1 SET a = 1;",
        );
        let waiting = Waiting::start(b, text);
        run(&mut a, "ALTER TABLE dbo.t2 ADD c int;");
        run(&mut a, "COMMIT TRANSACTION;");
        let (_b, events) = waiting.finish();
        assert!(errors(&events).is_empty(), "{text}: {events:#?}");
        assert_eq!(ints(&events), vec![1, 2], "{text}");
    }
}

/// A write held by a block waits, as it would written alone, for an `ALTER TABLE` another
/// session has not committed on the table its `FOREIGN KEY` checks, then writes its row.
#[test]
fn a_held_write_waits_for_the_table_its_foreign_key_checks() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.x (id int PRIMARY KEY); INSERT INTO dbo.x VALUES (1); \
         CREATE TABLE dbo.y (id int, x_id int REFERENCES dbo.x (id));",
    );
    run(&mut a, "BEGIN TRANSACTION; ALTER TABLE dbo.x ADD c int;");
    let waiting = Waiting::start(
        b,
        "IF 1 = 1\nBEGIN\n  SELECT 0;\n  INSERT INTO dbo.y VALUES (1, 1);\nEND",
    );
    run(&mut a, "COMMIT TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(ints(&run(&mut a, "SELECT x_id FROM dbo.y;")), vec![1]);
}

/// A condition that reads `dbo.w`, waiting for the row a first session holds, then
/// `dbo.t` under `NOLOCK`, which takes no lock of its rows: an `ALTER TABLE dbo.t` of a
/// third session waits for the end of the condition, which reads `dbo.t` as it was, then
/// runs.
#[test]
fn a_condition_holds_the_schema_of_the_tables_it_reads() {
    for text in [
        "IF 1 = 1\nBEGIN\n  SELECT 0;\n  \
         IF (SELECT COUNT(*) FROM dbo.w) + (SELECT COUNT(*) FROM dbo.t WITH (NOLOCK)) = 2 SELECT 1; \
         ELSE SELECT 2;\nEND",
        "IF (SELECT COUNT(*) FROM dbo.w) + (SELECT COUNT(*) FROM dbo.t WITH (NOLOCK)) = 2 SELECT 1; \
         ELSE SELECT 2;",
    ] {
        let engine = engine();
        let mut a = session(&engine, 61);
        let b = session(&engine, 62);
        let c = session(&engine, 63);
        run(
            &mut a,
            "CREATE TABLE dbo.w (a int); INSERT INTO dbo.w VALUES (5); \
             CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1); \
             BEGIN TRANSACTION; UPDATE dbo.w SET a = 5;",
        );
        let condition = Waiting::start(b, text);
        let alter = Waiting::start(c, "ALTER TABLE dbo.t ADD c int;");
        run(&mut a, "COMMIT TRANSACTION;");
        let (_b, events) = condition.finish();
        assert!(errors(&events).is_empty(), "{text}: {events:#?}");
        assert_eq!(ints(&events).last(), Some(&1), "{text}: {events:#?}");
        let (_c, events) = alter.finish();
        assert!(errors(&events).is_empty(), "{text}: {events:#?}");
        assert_eq!(
            run(&mut a, "SELECT a, c FROM dbo.t;"),
            vec![Event::Row(vec![Value::I32(1), Value::Null])]
        );
    }
}

/// A condition held by a block, reached while a third session has altered its table
/// without committing, waits for that session, then reads the table as altered: the block
/// is bound again.
#[test]
fn a_condition_waits_for_the_ddl_of_its_table_then_reads_the_new_table() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    let mut c = session(&engine, 63);
    run(
        &mut a,
        "CREATE TABLE dbo.w (a int); INSERT INTO dbo.w VALUES (5); \
         CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1); \
         BEGIN TRANSACTION; UPDATE dbo.w SET a = 5;",
    );
    let waiting = Waiting::start(b, CONDITION_HELD_BY_AN_IF);
    run(
        &mut c,
        "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD c int; INSERT INTO dbo.t (a) VALUES (2);",
    );
    run(&mut a, "COMMIT TRANSACTION;");
    assert!(
        waiting.still_waits(),
        "the condition waits for the transaction of the third session"
    );
    run(&mut c, "COMMIT TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(ints(&events), vec![5, 2, 4]);
}

/// A transaction that has read a table in `READ COMMITTED`, its statement over, does not
/// hold back an `ALTER TABLE` of another session; one that read it in `REPEATABLE READ`, or
/// wrote a row of it, does, until it ends. The reading transaction goes on, and reads the
/// altered table.
#[test]
fn a_read_committed_read_does_not_hold_back_a_ddl_once_its_statement_ended() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
    );

    run(&mut a, "BEGIN TRANSACTION; SELECT a FROM dbo.t;");
    let (done, finished) = mpsc::channel();
    let altering = thread::spawn(move || {
        let mut b = b;
        let events = run_on(&mut b, "ALTER TABLE dbo.t ADD c int;");
        let _ = done.send(());
        (b, events)
    });
    assert!(
        finished.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the ALTER goes through while the reader is open"
    );
    let (mut b, events) = altering.join().expect("the altering session");
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        run(&mut a, "SELECT c FROM dbo.t; COMMIT TRANSACTION;"),
        vec![Event::Row(vec![Value::Null])]
    );

    for (level, statement, ddl) in [
        (
            "REPEATABLE READ",
            "SELECT a FROM dbo.t;",
            "ALTER TABLE dbo.t ADD d int;",
        ),
        (
            "READ COMMITTED",
            "INSERT INTO dbo.t (a) VALUES (2);",
            "ALTER TABLE dbo.t ADD e int;",
        ),
    ] {
        run(
            &mut a,
            &format!("SET TRANSACTION ISOLATION LEVEL {level}; BEGIN TRANSACTION; {statement}"),
        );
        let waiting = Waiting::start(b, ddl);
        run(&mut a, "ROLLBACK TRANSACTION;");
        let (back, events) = waiting.finish();
        assert!(errors(&events).is_empty(), "{level}: {events:#?}");
        b = back;
    }
}

/// The 1222 of a wait at binding carries the line its statement starts on.
#[test]
fn a_wait_at_binding_that_times_out_carries_the_line_of_its_statement() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(&mut a, "CREATE TABLE dbo.y (a int);");
    run(&mut b, "SET LOCK_TIMEOUT 100;");
    run(&mut a, "BEGIN TRANSACTION; DROP TABLE dbo.y;");
    let events = run_on(&mut b, "SELECT 1;\n\nSELECT a FROM dbo.y;");
    let lines: Vec<(u32, u32)> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some((err.number, err.line)),
            Event::Row(_) => None,
        })
        .collect();
    assert_eq!(lines, vec![(1222, 3)]);
    run(&mut a, "ROLLBACK TRANSACTION;");
}

/// A dynamic text, the body `sp_executesql` and `sp_execute` run, waits as a batch does for
/// an `ALTER TABLE` another session has not committed, then writes its row; under
/// `SET LOCK_TIMEOUT 100` it answers 1222 state 56.
#[test]
fn a_prepared_or_dynamic_statement_waits_for_an_uncommitted_alter() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1);",
    );
    run(&mut a, "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD c int;");
    run(&mut b, "SET LOCK_TIMEOUT 100;");
    let mut sink = Recording::default();
    b.run_nested("INSERT INTO dbo.t (a) VALUES (2);", &[], &mut sink)
        .expect("the text answers");
    assert_eq!(numbers_and_states(&sink.0), vec![(1222, 56)]);

    run(&mut b, "SET LOCK_TIMEOUT -1;");
    let (done, finished) = mpsc::channel();
    let waiting = thread::spawn(move || {
        let mut sink = Recording::default();
        b.run_nested("INSERT INTO dbo.t (a) VALUES (3);", &[], &mut sink)
            .expect("the text answers");
        let _ = done.send(());
        sink.0
    });
    assert!(
        finished.recv_timeout(Duration::from_millis(300)).is_err(),
        "the text waits for the ALTER"
    );
    run(&mut a, "COMMIT TRANSACTION;");
    let events = waiting.join().expect("the waiting session");
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        ints(&run(&mut a, "SELECT a FROM dbo.t ORDER BY a;")),
        vec![1, 3]
    );
}

/// `OBJECT_ID` of a table another session creates or drops without having committed waits
/// for that session, then answers what it left: `NULL` after a rolled back `CREATE`, the
/// identifier after a committed one, the identifier after a rolled back `DROP`. Under
/// `SET LOCK_TIMEOUT 100` it answers 1222 state 51; `OBJECT_ID` of another table answers at
/// once.
#[test]
fn object_id_waits_for_an_uncommitted_create_or_drop_of_its_table() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.y (a int); CREATE TABLE dbo.other (a int);",
    );
    let id_of_y = run(&mut a, "SELECT OBJECT_ID('dbo.y');");

    run(&mut a, "BEGIN TRANSACTION; CREATE TABLE dbo.x (a int);");
    let waiting = Waiting::start(b, "SELECT OBJECT_ID('dbo.x');");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (b, events) = waiting.finish();
    assert_eq!(events, vec![Event::Row(vec![Value::Null])]);

    run(&mut a, "BEGIN TRANSACTION; CREATE TABLE dbo.x (a int);");
    let created = run(&mut a, "SELECT OBJECT_ID('dbo.x');");
    let waiting = Waiting::start(b, "SELECT OBJECT_ID('dbo.x');");
    run(&mut a, "COMMIT TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert_eq!(events, created);

    run(&mut b, "SET LOCK_TIMEOUT 100;");
    run(&mut a, "BEGIN TRANSACTION; DROP TABLE dbo.y;");
    let events = run_on(&mut b, "SELECT OBJECT_ID('dbo.y');");
    assert_eq!(numbers_and_states(&events), vec![(1222, 51)]);
    assert_eq!(
        run(
            &mut b,
            "SELECT CASE WHEN OBJECT_ID('dbo.other') IS NULL THEN 0 ELSE 1 END;"
        ),
        vec![Event::Row(vec![Value::I32(1)])]
    );
    run(&mut b, "SET LOCK_TIMEOUT -1;");
    let waiting = Waiting::start(b, "SELECT OBJECT_ID('dbo.y');");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert_eq!(events, id_of_y);
}

/// A block that alters or drops a table goes on with the statements it holds after the
/// `DDL`, in autocommit and in a transaction the block commits: a table the block itself
/// dropped or rebuilt does not make them fail.
#[test]
fn a_block_goes_on_after_its_own_alter() {
    let engine = engine();
    let mut a = session(&engine, 61);
    run(
        &mut a,
        "CREATE TABLE dbo.n (a int); CREATE TABLE dbo.t (a int); INSERT INTO dbo.t VALUES (1); \
         CREATE TABLE dbo.u (a int); CREATE TABLE dbo.v (a int);",
    );
    run(
        &mut a,
        "IF 1 = 1 BEGIN ALTER TABLE dbo.n ADD c int; PRINT 'altered'; END",
    );
    assert_eq!(
        ints(&run(
            &mut a,
            "IF 1 = 1 BEGIN ALTER TABLE dbo.t ADD c int; SELECT 7; END"
        )),
        vec![7]
    );
    assert_eq!(
        ints(&run(
            &mut a,
            "IF 1 = 1 BEGIN BEGIN TRANSACTION; ALTER TABLE dbo.v ADD c int; \
             COMMIT TRANSACTION; SELECT 8; END"
        )),
        vec![8]
    );
    assert_eq!(
        ints(&run(&mut a, "BEGIN DROP TABLE dbo.u; SELECT 9; END")),
        vec![9]
    );
    assert_eq!(
        run(&mut a, "SELECT c FROM dbo.t;"),
        vec![Event::Row(vec![Value::Null])]
    );
}

/// The names of the indexes of `dbo.t` in `sys.indexes`, by `index_id`, the heap left out.
///
/// The table is found by its name in `sys.tables` rather than by `OBJECT_ID`, which waits
/// for a `DDL` another session has not committed on the table
/// (`object_id_waits_for_an_uncommitted_create_or_drop_of_its_table`).
const INDEX_NAMES: &str = "SELECT i.name FROM sys.indexes AS i \
     JOIN sys.tables AS t ON t.object_id = i.object_id \
     WHERE t.name = 't' AND i.index_id > 0 ORDER BY i.index_id;";

/// The indexes of `dbo.t` a plan may seek through, as a transaction of its own sees them,
/// next to the ones `storage` holds.
fn plan_indexes(engine: &Arc<Engine>) -> (usize, usize) {
    use vauban_planner::{PlanCatalog, SnapshotIndexes, StorageIndexes};

    let handle = engine.txn.begin(vauban_txn::IsolationLevel::ReadCommitted);
    let snapshot = engine.catalog.snapshot(&handle);
    let table = snapshot
        .resolve_object("master", Some("dbo"), "t", "dbo")
        .map(|object| object.id)
        .and_then(|id| snapshot.table(id))
        .map(|meta| meta.storage_id)
        .expect("dbo.t");
    let seen = SnapshotIndexes {
        storage: engine.storage.as_ref(),
        snapshot: &snapshot,
    }
    .indexes_of(table)
    .len();
    let held = StorageIndexes(engine.storage.as_ref())
        .indexes_of(table)
        .len();
    engine.txn.commit(handle).expect("commit");
    (seen, held)
}

/// A `CREATE INDEX` another session has not committed: `sys.indexes` does not list it, a plan
/// does not seek through it although `storage` holds it, and a read of the table waits for
/// the end of that session; a `ROLLBACK` leaves the index out, a `COMMIT` puts it in.
#[test]
fn an_uncommitted_create_index_is_waited_for_then_left_out() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int, b int); INSERT INTO dbo.t VALUES (1, 2);",
    );

    run(&mut a, "BEGIN TRANSACTION; CREATE INDEX ix ON dbo.t (a);");
    assert_eq!(names(&run(&mut a, INDEX_NAMES)), vec!["ix"]);
    assert!(names(&run(&mut b, INDEX_NAMES)).is_empty());
    assert_eq!(plan_indexes(&engine), (0, 1));
    let waiting = Waiting::start(b, "SELECT b FROM dbo.t WHERE a = 1;");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert_eq!(ints(&events), vec![2], "{events:#?}");
    assert!(names(&run(&mut b, INDEX_NAMES)).is_empty());
    assert_eq!(plan_indexes(&engine), (0, 0));

    run(&mut a, "BEGIN TRANSACTION; CREATE INDEX ix ON dbo.t (a);");
    let waiting = Waiting::start(b, "SELECT b FROM dbo.t WHERE a = 1;");
    run(&mut a, "COMMIT TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert_eq!(ints(&events), vec![2], "{events:#?}");
    assert_eq!(names(&run(&mut b, INDEX_NAMES)), vec!["ix"]);
    assert_eq!(plan_indexes(&engine), (1, 1));
}

/// The name of an index the transaction dropped is free for that transaction; after a
/// `ROLLBACK TRANSACTION` to a savepoint taken before the `DROP INDEX`, the index is back and
/// a second `CREATE INDEX` of its name is refused, naming 1913.
#[test]
fn a_dropped_index_name_is_free_for_its_transaction_until_a_savepoint_gives_it_back() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int, b int); CREATE INDEX ix ON dbo.t (a);",
    );
    let key_column = "SELECT c.name FROM sys.index_columns AS ic \
         JOIN sys.columns AS c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE ic.object_id = OBJECT_ID('dbo.t');";

    run(
        &mut a,
        "BEGIN TRANSACTION; DROP INDEX ix ON dbo.t; CREATE INDEX ix ON dbo.t (b); \
         COMMIT TRANSACTION;",
    );
    assert_eq!(names(&run(&mut b, INDEX_NAMES)), vec!["ix"]);
    assert_eq!(names(&run(&mut b, key_column)), vec!["b"]);

    let events = run_on(
        &mut a,
        "BEGIN TRANSACTION; SAVE TRANSACTION s; DROP INDEX ix ON dbo.t; \
         ROLLBACK TRANSACTION s; CREATE INDEX ix ON dbo.t (a);",
    );
    let refused: Vec<&SqlError> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some(err),
            Event::Row(_) => None,
        })
        .collect();
    assert_eq!(refused.len(), 1, "{events:#?}");
    assert!(
        refused[0].message.contains("1913"),
        "{}",
        refused[0].message
    );
    assert_eq!(names(&run(&mut a, INDEX_NAMES)), vec!["ix"]);
    run(&mut a, "ROLLBACK TRANSACTION;");

    run(
        &mut a,
        "BEGIN TRANSACTION; SAVE TRANSACTION s; DROP INDEX ix ON dbo.t; \
         ROLLBACK TRANSACTION s; DROP INDEX ix ON dbo.t; CREATE INDEX ix ON dbo.t (a); \
         COMMIT TRANSACTION;",
    );
    assert_eq!(names(&run(&mut b, INDEX_NAMES)), vec!["ix"]);
    assert_eq!(names(&run(&mut b, key_column)), vec!["a"]);
}

/// A `CHECK` another session has added and not committed: `sys.check_constraints` does not
/// list it, and an `INSERT` that breaks it waits for that session, then writes its row after
/// a `ROLLBACK` and answers 547 after a `COMMIT`.
#[test]
fn an_uncommitted_check_is_not_enforced_on_another_session() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int, b int); INSERT INTO dbo.t VALUES (1, 2);",
    );
    let checks = "SELECT name FROM sys.check_constraints;";

    run(
        &mut a,
        "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD CONSTRAINT ck CHECK (a > 0);",
    );
    assert!(names(&run(&mut b, checks)).is_empty());
    let waiting = Waiting::start(b, "INSERT INTO dbo.t VALUES (-1, 2);");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        ints(&run(&mut b, "SELECT a FROM dbo.t ORDER BY a;")),
        vec![-1, 1]
    );

    run(&mut a, "DELETE FROM dbo.t WHERE a < 0;");
    run(
        &mut a,
        "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD CONSTRAINT ck CHECK (a > 0);",
    );
    let waiting = Waiting::start(b, "INSERT INTO dbo.t VALUES (-1, 2);");
    run(&mut a, "COMMIT TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert_eq!(errors(&events), vec![547], "{events:#?}");
    assert_eq!(names(&run(&mut b, checks)), vec!["ck"]);
}

/// The same for a `UNIQUE` constraint: `sys.key_constraints` does not list it, and an
/// `INSERT` of a duplicate waits, then writes after a `ROLLBACK` and answers 2627 after a
/// `COMMIT`.
#[test]
fn an_uncommitted_unique_is_not_enforced_on_another_session() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let mut b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.t (a int, b int); INSERT INTO dbo.t VALUES (1, 2);",
    );
    let keys = "SELECT name FROM sys.key_constraints;";

    run(
        &mut a,
        "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD CONSTRAINT uq UNIQUE (b);",
    );
    assert!(names(&run(&mut b, keys)).is_empty());
    let waiting = Waiting::start(b, "INSERT INTO dbo.t VALUES (5, 2);");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        ints(&run(&mut b, "SELECT a FROM dbo.t ORDER BY a;")),
        vec![1, 5]
    );

    run(&mut a, "DELETE FROM dbo.t WHERE a = 5;");
    run(
        &mut a,
        "BEGIN TRANSACTION; ALTER TABLE dbo.t ADD CONSTRAINT uq UNIQUE (b);",
    );
    let waiting = Waiting::start(b, "INSERT INTO dbo.t VALUES (5, 2);");
    run(&mut a, "COMMIT TRANSACTION;");
    let (mut b, events) = waiting.finish();
    assert_eq!(errors(&events), vec![2627], "{events:#?}");
    assert_eq!(names(&run(&mut b, keys)), vec!["uq"]);
}

/// A `FOREIGN KEY` another session has declared and not committed — with the table it
/// created, or added to a table that exists — makes a `DELETE` of the referenced table wait
/// for that session, then delete once it has rolled back.
#[test]
fn a_delete_waits_for_a_foreign_key_another_session_has_not_committed() {
    let engine = engine();
    let mut a = session(&engine, 61);
    let b = session(&engine, 62);
    run(
        &mut a,
        "CREATE TABLE dbo.p (a int PRIMARY KEY); INSERT INTO dbo.p VALUES (1), (2), (3);",
    );

    run(
        &mut a,
        "BEGIN TRANSACTION; CREATE TABLE dbo.c (a int REFERENCES dbo.p (a)); \
         INSERT INTO dbo.c VALUES (2);",
    );
    let waiting = Waiting::start(b, "DELETE FROM dbo.p WHERE a = 2;");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        ints(&run(&mut a, "SELECT a FROM dbo.p ORDER BY a;")),
        vec![1, 3]
    );

    run(
        &mut a,
        "CREATE TABLE dbo.c (a int); INSERT INTO dbo.c VALUES (1);",
    );
    run(
        &mut a,
        "BEGIN TRANSACTION; \
         ALTER TABLE dbo.c ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES dbo.p (a);",
    );
    let waiting = Waiting::start(b, "DELETE FROM dbo.p WHERE a = 1;");
    run(&mut a, "ROLLBACK TRANSACTION;");
    let (_b, events) = waiting.finish();
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        ints(&run(&mut a, "SELECT a FROM dbo.p ORDER BY a;")),
        vec![3]
    );
}
