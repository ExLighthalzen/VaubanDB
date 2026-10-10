//! Integration tests of `ROLLBACK TRANSACTION <savepoint>` after an `ALTER TABLE` that copies
//! the table, on a disk instance: the table reads as it was at the savepoint, the transaction
//! stays open, and the same state reads back after the instance is reopened, with or without a
//! checkpoint before.
//!
//! No socket and no TDS: the batches run on `Session::run_batch` through a `ResultSink` that
//! keeps the rows and the errors, as `tests/ddl_visibility.rs` does.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::{DiskOptions, DiskStorage, Storage};
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

/// A fresh directory under the temporary directory, removed when dropped.
struct Instance(PathBuf);

impl Instance {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vauban-savepoint-alter-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("instance directory");
        Instance(dir)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An engine on the disk instance of `dir`, with the built-in functions registered, and the
/// storage it runs on.
fn open(dir: &Path) -> (Arc<Engine>, Arc<dyn Storage>) {
    vauban_sysfn::register_builtins();
    let storage: Arc<dyn Storage> =
        Arc::new(DiskStorage::open(dir, DiskOptions::default()).expect("open the instance"));
    (Arc::new(Engine::new(Arc::clone(&storage))), storage)
}

/// Runs `text` on `session` and fails when the batch raised an error; returns the rows.
fn run(session: &mut Session, text: &str) -> Vec<Vec<Value>> {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("a batch reports its errors through the sink, not through its result");
    sink.0
        .into_iter()
        .map(|event| match event {
            Event::Row(row) => row,
            Event::Error(err) => panic!("`{text}` raised an error: {err:?}"),
        })
        .collect()
}

/// Rows of `int` values.
fn ints(rows: &[&[i32]]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| row.iter().map(|v| Value::I32(*v)).collect())
        .collect()
}

/// Plays `setup` then `body` on a fresh disk instance, checks that `check` reads `expected`
/// before the commit, after it, and after a reopen (with a checkpoint before it when
/// `checkpoint` is set, as a stop by signal does; without one, as after a `kill -9`).
fn play(name: &str, setup: &str, body: &str, check: &str, expected: &[&[i32]], checkpoint: bool) {
    let instance = Instance::new(name);
    let expected = ints(expected);
    {
        let (engine, storage) = open(&instance.0);
        let mut session = Session::new(Arc::clone(&engine), SessionState::new(51));
        run(&mut session, setup);
        run(&mut session, body);
        assert_eq!(
            run(&mut session, check),
            expected,
            "{name}: before the commit"
        );
        assert_eq!(
            run(&mut session, "SELECT @@TRANCOUNT"),
            ints(&[&[1]]),
            "{name}: the transaction stays open"
        );
        run(&mut session, "COMMIT");
        assert_eq!(
            run(&mut session, check),
            expected,
            "{name}: after the commit"
        );
        if checkpoint {
            storage.checkpoint().expect("checkpoint");
        }
    }
    let (engine, _storage) = open(&instance.0);
    let mut session = Session::new(engine, SessionState::new(51));
    assert_eq!(
        run(&mut session, check),
        expected,
        "{name}: after the reopen"
    );
}

#[test]
fn a_savepoint_rollback_of_an_added_column_keeps_the_table_on_disk() {
    for checkpoint in [true, false] {
        play(
            "add-column",
            "CREATE TABLE w (a int, b int); INSERT w VALUES (1,1);",
            "BEGIN TRAN; SAVE TRAN s; ALTER TABLE w ADD z int; ROLLBACK TRAN s;",
            "SELECT * FROM w",
            &[&[1, 1]],
            checkpoint,
        );
    }
}

#[test]
fn a_savepoint_rollback_undoes_an_insert_made_before_the_alter_on_disk() {
    for checkpoint in [true, false] {
        play(
            "insert-then-add",
            "CREATE TABLE w (a int, b int); INSERT w VALUES (1,1);",
            "BEGIN TRAN; SAVE TRAN s; INSERT w VALUES (2,2); ALTER TABLE w ADD z int; \
             ROLLBACK TRAN s;",
            "SELECT * FROM w",
            &[&[1, 1]],
            checkpoint,
        );
    }
}

#[test]
fn a_savepoint_rollback_keeps_the_alter_made_before_the_savepoint_on_disk() {
    for checkpoint in [true, false] {
        play(
            "add-before-savepoint",
            "CREATE TABLE x (a int, b int); INSERT x VALUES (1,1);",
            "BEGIN TRAN; ALTER TABLE x ADD y int; INSERT x VALUES (2,2,2); SAVE TRAN s; \
             ALTER TABLE x ADD z int; INSERT x VALUES (3,3,3,3); ROLLBACK TRAN s;",
            "SELECT a, b, ISNULL(y, -1) FROM x ORDER BY a",
            &[&[1, 1, -1], &[2, 2, 2]],
            checkpoint,
        );
    }
}

#[test]
fn a_savepoint_rollback_of_an_added_unique_keeps_the_table_on_disk() {
    for checkpoint in [true, false] {
        play(
            "add-unique",
            "CREATE TABLE u (a int, b int); INSERT u VALUES (1,5); INSERT u VALUES (2,6);",
            "BEGIN TRAN; SAVE TRAN s; ALTER TABLE u ADD CONSTRAINT uq UNIQUE (b); \
             ROLLBACK TRAN s;",
            "SELECT * FROM u ORDER BY a",
            &[&[1, 5], &[2, 6]],
            checkpoint,
        );
    }
    // The constraint is gone with the savepoint: a duplicate of `b` goes in after the commit.
    let instance = Instance::new("add-unique-gone");
    let (engine, _storage) = open(&instance.0);
    let mut session = Session::new(engine, SessionState::new(51));
    run(
        &mut session,
        "CREATE TABLE u (a int, b int); INSERT u VALUES (1,5); INSERT u VALUES (2,6);",
    );
    run(
        &mut session,
        "BEGIN TRAN; SAVE TRAN s; ALTER TABLE u ADD CONSTRAINT uq UNIQUE (b); \
         ROLLBACK TRAN s; COMMIT;",
    );
    run(&mut session, "INSERT u VALUES (3,5);");
    assert_eq!(
        run(
            &mut session,
            "SELECT COUNT(*) FROM sys.key_constraints WHERE name = 'uq'"
        ),
        ints(&[&[0]])
    );
    assert_eq!(
        run(&mut session, "SELECT * FROM u ORDER BY a"),
        ints(&[&[1, 5], &[2, 6], &[3, 5]])
    );
}
