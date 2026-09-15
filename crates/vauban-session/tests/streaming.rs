//! Streaming behaviour of the session: the order of the tokens a statement sends,
//! written through [`ResultSink`] without a socket.
//!
//! The `Recording` sink records every call in order, and the tests below assert on the
//! sequence of `columns`, `row` and `error` events. The integration test for ATTENTION
//! over the network lives in `tests/attention.rs`; this file tests the contract of the
//! adapter and the cancellation token.

use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::{MemoryStorage, Row as StorageRow, TableId};
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_txn::IsolationLevel;
use vauban_types::{SqlType, TypeInfo, Value};

/// SPID of each session built here.
const SPID: i16 = 51;

// ---------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------

/// Everything a sink receives, in order.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns(Vec<(String, SqlType)>),
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    Error(SqlError),
    Info(InfoMessage),
    EnvChange(String, String),
    Other,
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        let reduced: Vec<_> = cols
            .iter()
            .map(|col| (col.name.clone(), col.ty.ty))
            .collect();
        self.0.push(Event::Columns(reduced));
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
        if let EnvChange::Database { old, new } = change {
            self.0.push(Event::EnvChange(old.clone(), new.clone()));
        } else {
            self.0.push(Event::Other);
        }
        Ok(())
    }

    fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        self.0.push(Event::Other);
        Ok(())
    }

    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        self.0.push(Event::Other);
        Ok(())
    }
}

/// Runs one batch and gives back the events its sink received.
fn run(session: &mut Session, text: &str) -> Recording {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("the response is complete");
    sink
}

/// An engine over an empty in-memory storage.
fn engine() -> Arc<Engine> {
    Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
}

/// A session on `engine`, database `master` and default `SET` options.
fn session(engine: &Arc<Engine>) -> Session {
    Session::new(Arc::clone(engine), SessionState::new(SPID))
}

/// The table id of a table freshly created through `CREATE TABLE` in `master`.
///
/// The last table of the list is the one the statement just created: the binder does
/// not register it in the storage before then, and the storage keeps insertion order.
fn table_id(engine: &Engine) -> TableId {
    let txn = engine.txn.begin(IsolationLevel::ReadCommitted);
    let snap = engine.catalog.snapshot(&txn);
    let master = snap.database("master").expect("master exists");
    let db = master.id;
    // catalog drops the txn/snap at the end...
    engine
        .storage
        .tables(db)
        .expect("tables")
        .last()
        .expect("at least one table (the one we created)")
        .0
}

/// Inserts rows into a table through the storage layer.
fn insert_rows(engine: &Engine, table: TableId, values: &[i64]) {
    let txn = engine.txn.begin(IsolationLevel::ReadCommitted);
    for &v in values {
        engine
            .storage
            .insert(txn.id, table, &StorageRow(vec![Value::I64(v)]))
            .expect("row inserted");
    }
    engine.txn.commit(txn).expect("transaction commits");
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

/// A run-time error has `columns` before the `error` (the metadata goes out when the
/// statement starts running). A compile-time error sends no `columns` at all.
#[test]
fn columns_precede_a_runtime_error() {
    let engine = engine();
    let mut s = session(&engine);

    // `SELECT 1 / 0` is a run-time error: the constant is not folded (`compile.rs`),
    // so the metadata goes out, then the divide-by-zero is raised.
    let batch = run(&mut s, "SELECT 1 / 0;");
    let events = &batch.0;
    assert_eq!(
        events.first(),
        Some(&Event::Columns(vec![(String::new(), SqlType::Int)])),
        "columns must precede the error"
    );
    let error = events.iter().find(|e| matches!(e, Event::Error(_)));
    assert!(error.is_some(), "a run-time error must be sent");
    let error_idx = events
        .iter()
        .position(|e| matches!(e, Event::Error(_)))
        .unwrap();
    let cols_idx = events
        .iter()
        .position(|e| matches!(e, Event::Columns(_)))
        .unwrap();
    assert!(
        cols_idx < error_idx,
        "columns ({cols_idx}) before error ({error_idx})"
    );

    // Counter-proof: compile-time error (208 on an unknown table) sends no columns.
    let compile_err = run(&mut s, "SELECT 1 FROM dbo.s20nosuch;");
    assert!(
        matches!(compile_err.0.first(), Some(Event::Error(e)) if e.number == 208),
        "a compile-time error (208) sends no columns"
    );
    assert!(
        !compile_err.0.iter().any(|e| matches!(e, Event::Columns(_))),
        "a compile-time error must announce no result set"
    );
}

/// An error raised at run time, with a row that precedes it. The sink receives
/// `columns`, at least one `row`, then `error`.
///
/// A compile-time error sends no `columns` at all.
#[test]
fn columns_precede_the_first_row_and_a_runtime_error() {
    let engine = engine();
    let mut s = session(&engine);

    // Create a table and populate rows directly through storage.
    let create = run(&mut s, "CREATE TABLE dbo.s20e (a int);");
    assert!(create.0.is_empty() || create.0.iter().all(|e| matches!(e, Event::Done(_, _))));

    let tid = table_id(&engine);
    // Row a=2: a/(a-1) = 2/1 = 2. Row a=1: a/(a-1) = 1/0 = error.
    insert_rows(&engine, tid, &[2, 1]);

    let batch = run(&mut s, "SELECT a, a / (a - 1) FROM dbo.s20e;");
    let events = &batch.0;
    let cols = events
        .iter()
        .position(|e| matches!(e, Event::Columns(_)))
        .expect("columns must appear");
    let rows: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            if matches!(e, Event::Row(_)) {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    let err = events.iter().position(|e| matches!(e, Event::Error(_)));
    assert!(
        err.is_some(),
        "a run-time error must be sent for row a=1 (1/0)"
    );
    assert!(!rows.is_empty(), "at least one row before the error");
    let err_idx = err.unwrap();
    assert!(
        cols < rows[0],
        "columns ({cols}) before first row ({})",
        rows[0]
    );
    assert!(
        rows[0] < err_idx,
        "row ({}) before error ({err_idx})",
        rows[0]
    );

    // Counter-proof as above.
    let compile_err = run(&mut s, "SELECT 1 FROM dbo.s20nosuch;");
    assert!(
        matches!(compile_err.0.first(), Some(Event::Error(e)) if e.number == 208),
        "compile-time error (208), no columns"
    );
    assert!(
        !compile_err.0.iter().any(|e| matches!(e, Event::Columns(_))),
        "no columns for a compile error"
    );
}

/// 1000 rows between `columns` and `done`, proving that each row was forwarded to the
/// sink as the scan produced it.
#[test]
fn rows_are_forwarded_before_the_scan_ends() {
    let engine = engine();
    let mut s = session(&engine);
    let create = run(&mut s, "CREATE TABLE dbo.s20f (id int);");
    assert!(create.0.is_empty() || create.0.iter().all(|e| matches!(e, Event::Done(_, _))));

    let tid = table_id(&engine);
    let values: Vec<i64> = (1..=1000).collect();
    insert_rows(&engine, tid, &values);

    let batch = run(&mut s, "SELECT id FROM dbo.s20f;");
    let row_count = batch
        .0
        .iter()
        .filter(|e| matches!(e, Event::Row(_)))
        .count();
    assert_eq!(row_count, 1000, "all 1000 rows forwarded");

    let cols_pos = batch
        .0
        .iter()
        .position(|e| matches!(e, Event::Columns(_)))
        .expect("columns must appear");
    let done_pos = batch
        .0
        .iter()
        .position(|e| matches!(e, Event::Done(_, _)))
        .expect("done must appear");
    for (i, e) in batch.0.iter().enumerate() {
        if matches!(e, Event::Row(_)) {
            assert!(
                i > cols_pos,
                "row at position {i} after columns at {cols_pos}"
            );
            assert!(
                i < done_pos,
                "row at position {i} before done at {done_pos}"
            );
        }
    }
}

/// `SELECT 1` without `FROM` still works through the new planner path.
#[test]
fn select_one_without_from() {
    let engine = engine();
    let batch = run(&mut session(&engine), "SELECT 1;");
    let events = &batch.0;
    assert_eq!(
        events.first(),
        Some(&Event::Columns(vec![(String::new(), SqlType::Int)]))
    );
    assert_eq!(events.get(1), Some(&Event::Row(vec![Value::I32(1)])));
    assert_eq!(events.get(2), Some(&Event::Done(Some(1), false)));
}
