//! Integration tests of a batch that drops, alters or rolls back a table and then names it
//! again: each statement after the one that moved the catalogue is bound against the
//! catalogue it left, so a `CREATE TABLE` of a name a `DROP TABLE` freed is accepted and a
//! `SELECT` after a `ROLLBACK` reads the shape the `ROLLBACK` brought back.
//!
//! No socket and no TDS: the batches run on `Session::run_batch` through a `ResultSink` that
//! keeps the rows and the errors, as `tests/ddl_visibility.rs` does.

use std::sync::Arc;

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

/// A session on an in-memory engine with the built-in functions registered.
fn session() -> Session {
    vauban_sysfn::register_builtins();
    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
    Session::new(engine, SessionState::new(61))
}

/// Runs `text` and returns what the sink received.
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

/// The rows of `events`, each as its `int` values.
fn rows(events: &[Event]) -> Vec<Vec<i32>> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => Some(
                row.iter()
                    .map(|value| match value {
                        Value::I32(value) => *value,
                        other => panic!("an int was expected, got {other:?}"),
                    })
                    .collect(),
            ),
            Event::Error(_) => None,
        })
        .collect()
}

/// The number and the state of each error of `events`.
fn errors(events: &[Event]) -> Vec<(u32, u8)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some((err.number, err.state)),
            Event::Row(_) => None,
        })
        .collect()
}

const MIGRATION: &str = "IF OBJECT_ID('t') IS NOT NULL DROP TABLE t; \
     CREATE TABLE t (a int, b int); INSERT t VALUES (1, 2); SELECT * FROM t;";

#[test]
fn a_migration_script_that_drops_then_creates_its_table_can_be_replayed() {
    let mut session = session();
    assert_eq!(rows(&run(&mut session, MIGRATION)), vec![vec![1, 2]]);
    assert_eq!(rows(&run(&mut session, MIGRATION)), vec![vec![1, 2]]);
}

#[test]
fn a_drop_table_with_or_without_if_exists_frees_the_name_for_the_next_statement() {
    let mut session = session();
    run(&mut session, "CREATE TABLE t (a int, b int);");
    let plain = run(
        &mut session,
        "DROP TABLE t; CREATE TABLE t (a int, b int); INSERT t VALUES (3, 4); SELECT * FROM t;",
    );
    assert_eq!(rows(&plain), vec![vec![3, 4]]);
    let if_exists = run(
        &mut session,
        "DROP TABLE IF EXISTS t; CREATE TABLE t (a int, b int); INSERT t VALUES (5, 6); \
         SELECT * FROM t;",
    );
    assert_eq!(rows(&if_exists), vec![vec![5, 6]]);
}

#[test]
fn a_table_dropped_and_created_again_in_a_transaction_follows_its_end() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (a int, b int); INSERT t VALUES (1, 1);",
    );
    let committed = run(
        &mut session,
        "BEGIN TRAN; DROP TABLE t; CREATE TABLE t (a int, b int); INSERT t VALUES (7, 8); \
         COMMIT; SELECT * FROM t;",
    );
    assert_eq!(rows(&committed), vec![vec![7, 8]]);
    let rolled_back = run(
        &mut session,
        "BEGIN TRAN; DROP TABLE t; CREATE TABLE t (a int, b int); INSERT t VALUES (9, 10); \
         SELECT * FROM t; ROLLBACK; SELECT * FROM t;",
    );
    assert_eq!(rows(&rolled_back), vec![vec![9, 10], vec![7, 8]]);
}

#[test]
fn a_dynamic_text_that_recreates_a_table_is_read_by_the_rest_of_the_batch() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (a int, b int); INSERT t VALUES (1, 1);",
    );
    let events = run(
        &mut session,
        "EXEC('IF OBJECT_ID(''t'') IS NOT NULL DROP TABLE t; CREATE TABLE t (a int, b int); \
         INSERT t VALUES (11, 12);'); SELECT * FROM t;",
    );
    assert_eq!(rows(&events), vec![vec![11, 12]]);
    let in_a_transaction = run(
        &mut session,
        "BEGIN TRAN; EXEC('DROP TABLE t; CREATE TABLE t (a int, b int); \
         INSERT t VALUES (13, 14);'); SELECT * FROM t; COMMIT;",
    );
    assert_eq!(rows(&in_a_transaction), vec![vec![13, 14]]);
}

#[test]
fn a_constraint_dropped_then_added_again_under_its_name_is_accepted() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (a int); ALTER TABLE t ADD CONSTRAINT ck CHECK (a > 0);",
    );
    run(
        &mut session,
        "ALTER TABLE t DROP CONSTRAINT ck; ALTER TABLE t ADD CONSTRAINT ck CHECK (a > 1);",
    );
    run(
        &mut session,
        "BEGIN TRAN; ALTER TABLE t DROP CONSTRAINT ck; \
         ALTER TABLE t ADD CONSTRAINT ck CHECK (a > 2); COMMIT;",
    );
    let refused = run_on(&mut session, "INSERT t VALUES (2);");
    assert_eq!(errors(&refused), vec![(547, 0)]);
}

#[test]
fn an_index_dropped_then_created_again_under_its_name_is_accepted() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (a int, b int); CREATE INDEX ix ON t (a);",
    );
    let events = run(
        &mut session,
        "DROP INDEX ix ON t; CREATE INDEX ix ON t (b); \
         SELECT COUNT(*) FROM sys.indexes WHERE object_id = OBJECT_ID('t') AND name = 'ix';",
    );
    assert_eq!(rows(&events), vec![vec![1]]);
}

#[test]
fn a_select_after_a_rollback_reads_the_shape_the_rollback_brought_back() {
    let mut session = session();
    run(&mut session, "CREATE TABLE r (a int); INSERT r VALUES (1);");
    let events = run(
        &mut session,
        "BEGIN TRAN; ALTER TABLE r ADD h int; ROLLBACK; SELECT * FROM r;",
    );
    assert_eq!(rows(&events), vec![vec![1]]);
    let in_a_branch = run(
        &mut session,
        "BEGIN TRAN; ALTER TABLE r ADD q int; IF @@TRANCOUNT > 0 ROLLBACK; SELECT * FROM r;",
    );
    assert_eq!(rows(&in_a_branch), vec![vec![1]]);
    let dropped = run(
        &mut session,
        "BEGIN TRAN; DROP TABLE r; ROLLBACK; SELECT * FROM r;",
    );
    assert_eq!(rows(&dropped), vec![vec![1]]);
}

#[test]
fn a_compilation_error_after_a_drop_still_stops_the_batch_before_the_drop() {
    let mut session = session();
    run(&mut session, "CREATE TABLE t (a int); INSERT t VALUES (1);");
    let events = run_on(&mut session, "DROP TABLE t; SELECT nocol FROM t;");
    assert_eq!(errors(&events), vec![(207, 1)]);
    // An `INSERT` that does not fit the table the batch starts from is refused before the
    // `DROP` runs too, whatever shape the `CREATE` before it writes.
    let events = run_on(
        &mut session,
        "DROP TABLE t; CREATE TABLE t (x int, y int); INSERT t VALUES (1, 2);",
    );
    assert_eq!(errors(&events), vec![(213, 1)]);
    assert_eq!(rows(&run(&mut session, "SELECT * FROM t;")), vec![vec![1]]);
}

#[test]
fn a_create_table_of_a_name_still_taken_is_2714_when_it_runs() {
    let mut session = session();
    run(&mut session, "CREATE TABLE t (a int);");
    let events = run_on(&mut session, "SELECT 1; CREATE TABLE t (z int); SELECT 2;");
    assert_eq!(rows(&events), vec![vec![1]]);
    assert_eq!(errors(&events), vec![(2714, 6)]);
    let into = run_on(&mut session, "SELECT 1; SELECT 3 AS k INTO t; SELECT 2;");
    assert_eq!(rows(&into), vec![vec![1]]);
    assert_eq!(errors(&into), vec![(2714, 6)]);
}
