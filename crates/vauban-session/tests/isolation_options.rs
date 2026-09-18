//! Session isolation and lock options: the level, lock timeout, snapshot refusal and
//! `XACT_ABORT` reach the transaction the next statement opens.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use vauban_errors::SqlError;
use vauban_executor::ExecSession;
use vauban_session::{Engine, Session, SessionState, lock_timeout_from_ms, sync_exec_session};
use vauban_storage::{MemoryStorage, RowId, testsuite::int_table_shape};
use vauban_txn::{IsolationLevel, LockTimeout};
use vauban_types::Value;

const SPID: i16 = 71;

fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
}

fn session(engine: &Arc<Engine>) -> Session {
    Session::new(Arc::clone(engine), SessionState::new(SPID))
}

fn run(session: &mut Session, text: &str) {
    struct Sink;
    impl vauban_session::ResultSink for Sink {
        fn columns(&mut self, _cols: &[vauban_tds::ColumnMeta]) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn row(&mut self, _row: &[Value]) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn info(&mut self, _msg: &vauban_errors::InfoMessage) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn error(&mut self, _err: &vauban_errors::SqlError) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn env_change(&mut self, _change: &vauban_tds::EnvChange) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn return_value(
            &mut self,
            _name: &str,
            _ty: &vauban_types::TypeInfo,
            _value: &Value,
        ) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn return_status(&mut self, _status: i32) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
    }
    let mut sink = Sink;
    session
        .run_batch(text, &mut sink)
        .expect("errors go through the sink");
}

fn one_int(session: &mut Session, text: &str) -> i32 {
    struct Collect(Vec<Value>);
    impl vauban_session::ResultSink for Collect {
        fn columns(&mut self, _cols: &[vauban_tds::ColumnMeta]) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn row(&mut self, row: &[Value]) -> vauban_errors::SqlResult<()> {
            self.0 = row.to_vec();
            Ok(())
        }
        fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn info(&mut self, _msg: &vauban_errors::InfoMessage) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn error(&mut self, err: &vauban_errors::SqlError) -> vauban_errors::SqlResult<()> {
            panic!("unexpected error {}: {}", err.number, err.message);
        }
        fn env_change(&mut self, _change: &vauban_tds::EnvChange) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn return_value(
            &mut self,
            _name: &str,
            _ty: &vauban_types::TypeInfo,
            _value: &Value,
        ) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn return_status(&mut self, _status: i32) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
    }
    let mut sink = Collect(Vec::new());
    session
        .run_batch(text, &mut sink)
        .expect("the batch succeeds");
    match sink.0.as_slice() {
        [Value::I32(n)] => *n,
        other => panic!("expected one int, got {other:?}"),
    }
}

#[test]
fn session_level_reaches_the_handle() {
    let engine = engine();
    let mut session = session(&engine);
    run(
        &mut session,
        "CREATE TABLE dbo.iso (id int NOT NULL PRIMARY KEY, v int NOT NULL)",
    );

    for (stmt, level) in [
        (
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            IsolationLevel::ReadUncommitted,
        ),
        (
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
            IsolationLevel::ReadCommitted,
        ),
        (
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            IsolationLevel::RepeatableRead,
        ),
        (
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            IsolationLevel::Serializable,
        ),
        (
            "SET TRANSACTION ISOLATION LEVEL SNAPSHOT",
            IsolationLevel::Snapshot,
        ),
    ] {
        run(&mut session, stmt);
        run(&mut session, "BEGIN TRAN");
        let open = engine.txn.active_sessions();
        assert_eq!(
            open.last().map(|info| info.isolation),
            Some(level),
            "after `{stmt}`"
        );
        run(&mut session, "COMMIT");
    }
}

#[test]
fn lock_timeout_reaches_the_lock_manager() {
    let engine = engine();
    let storage = Arc::clone(&engine.storage);
    let mgr = Arc::clone(&engine.txn);
    let db = storage
        .databases()
        .expect("databases")
        .into_iter()
        .find(|(_, name)| name == "master")
        .expect("master")
        .0;
    let table = storage
        .create_table(db, &int_table_shape(1))
        .expect("create_table");
    let row = RowId(1);

    let holder = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.lock_row(&holder, table, row, LockTimeout::NoWait)
        .expect("the row is free");

    let mut session = session(&engine);
    run(&mut session, "SET LOCK_TIMEOUT 100");
    let mut exec = ExecSession::default();
    sync_exec_session(session.state(), &mut exec);
    assert_eq!(exec.lock_timeout, LockTimeout::Millis(100));

    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    let started = Instant::now();
    let err = mgr
        .lock_row(&reader, table, row, exec.lock_timeout)
        .expect_err("the exclusive lock blocks");
    assert_eq!((err.number, err.severity), (1222, 16));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "1222 came back in {:?}",
        started.elapsed()
    );
    mgr.rollback(holder).expect("rollback");
    mgr.rollback(reader).expect("rollback");

    run(&mut session, "SET LOCK_TIMEOUT -1");
    sync_exec_session(session.state(), &mut exec);
    assert_eq!(exec.lock_timeout, LockTimeout::Infinite);

    let holder = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.lock_row(&holder, table, row, LockTimeout::NoWait)
        .expect("the row is free");
    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    let mgr_bg = Arc::clone(&mgr);
    let reader_bg = reader.clone();
    let wait = thread::spawn(move || {
        mgr_bg
            .lock_row(&reader_bg, table, row, LockTimeout::Infinite)
            .expect("waits until the lock is released")
    });
    thread::sleep(Duration::from_millis(50));
    assert!(
        !wait.is_finished(),
        "LOCK_TIMEOUT -1 waits for the holder to release"
    );
    mgr.rollback(holder).expect("rollback");
    wait.join().expect("join");
    mgr.rollback(reader).expect("rollback");
}

#[test]
fn lock_timeout_zero_is_no_wait() {
    assert_eq!(lock_timeout_from_ms(0), LockTimeout::NoWait);
    assert_eq!(lock_timeout_from_ms(-1), LockTimeout::Infinite);
    assert_eq!(lock_timeout_from_ms(100), LockTimeout::Millis(100));
}

fn run_expect_error(session: &mut Session, text: &str) -> SqlError {
    struct ErrorRecording(Option<SqlError>);
    impl vauban_session::ResultSink for ErrorRecording {
        fn columns(&mut self, _cols: &[vauban_tds::ColumnMeta]) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn row(&mut self, _row: &[Value]) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn info(&mut self, _msg: &vauban_errors::InfoMessage) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn error(&mut self, err: &SqlError) -> vauban_errors::SqlResult<()> {
            self.0 = Some(err.clone());
            Ok(())
        }
        fn env_change(&mut self, _change: &vauban_tds::EnvChange) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn return_value(
            &mut self,
            _name: &str,
            _ty: &vauban_types::TypeInfo,
            _value: &Value,
        ) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn return_status(&mut self, _status: i32) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
    }
    let mut sink = ErrorRecording(None);
    session
        .run_batch(text, &mut sink)
        .expect("errors go through the sink");
    sink.0.expect("the batch raised an error")
}

#[test]
fn snapshot_without_the_database_option() {
    let engine = engine();
    let mut session = session(&engine);
    run(
        &mut session,
        "CREATE TABLE dbo.snap (id int NOT NULL PRIMARY KEY, v int NOT NULL)",
    );
    run(&mut session, "SET TRANSACTION ISOLATION LEVEL SNAPSHOT");
    run(&mut session, "SELECT 1");
    let err = run_expect_error(
        &mut session,
        "SET TRANSACTION ISOLATION LEVEL SNAPSHOT; SELECT v FROM dbo.snap",
    );
    assert_eq!((err.number, err.severity, err.state), (3952, 16, 1));
}

#[test]
fn xact_abort_aborts_the_transaction() {
    let engine = engine();
    let mut session = session(&engine);
    run(
        &mut session,
        "CREATE TABLE dbo.xa (id int NOT NULL PRIMARY KEY, v int NOT NULL)",
    );
    run(
        &mut session,
        "BEGIN TRAN; INSERT INTO dbo.xa (id, v) VALUES (1, 10); SET XACT_ABORT ON; SELECT 1 / 0",
    );
    assert_eq!(one_int(&mut session, "SELECT @@TRANCOUNT"), 0);
    assert_eq!(
        one_int(&mut session, "SELECT COUNT(*) FROM dbo.xa"),
        0,
        "the insert rolled back with the transaction"
    );

    run(
        &mut session,
        "BEGIN TRAN; INSERT INTO dbo.xa (id, v) VALUES (2, 20); SET XACT_ABORT OFF; SELECT CAST('x' AS int)",
    );
    assert_eq!(one_int(&mut session, "SELECT @@TRANCOUNT"), 1);
    assert_eq!(one_int(&mut session, "SELECT COUNT(*) FROM dbo.xa"), 1);
    run(&mut session, "ROLLBACK");
}

#[test]
fn deadlock_priority_reaches_the_lock_manager() {
    let engine = engine();
    let mut session = session(&engine);
    run(&mut session, "SET DEADLOCK_PRIORITY LOW");
    run(&mut session, "BEGIN TRAN");
    let open = engine.txn.active_sessions();
    let info = open
        .last()
        .expect("a transaction was open for the statement");
    assert_eq!(info.deadlock_priority, -5);
    run(&mut session, "COMMIT");
}
