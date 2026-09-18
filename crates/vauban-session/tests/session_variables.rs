//! Session variables wired through `SessionEvalContext`: `@@ROWCOUNT`, identity values,
//! `@@ERROR`, `@@TRANCOUNT`, `XACT_STATE()`.

use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{SqlType, TypeInfo, Value};

const SPID: i16 = 71;

#[derive(Default)]
struct Recording(Vec<Event>);

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns(Vec<ColumnMeta>),
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    Error(SqlError),
}

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
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.clone()));
        Ok(())
    }
    fn info(&mut self, _: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn env_change(&mut self, _: &EnvChange) -> SqlResult<()> {
        Ok(())
    }
    fn return_value(&mut self, _: &str, _: &TypeInfo, _: &Value) -> SqlResult<()> {
        Ok(())
    }
    fn return_status(&mut self, _: i32) -> SqlResult<()> {
        Ok(())
    }
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(SPID),
    )
}

fn run(session: &mut Session, text: &str) -> Vec<Event> {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("errors go through the sink");
    sink.0
}

fn rows(events: &[Event]) -> Vec<Vec<Value>> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => Some(row.clone()),
            _ => None,
        })
        .collect()
}

fn columns(events: &[Event], n: usize) -> Vec<ColumnMeta> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Columns(cols) => Some(cols.clone()),
            _ => None,
        })
        .nth(n)
        .unwrap_or_else(|| panic!("no result set {n}: {events:#?}"))
}

fn setup_identity_table(session: &mut Session, name: &str) {
    run(
        session,
        &format!(
            "CREATE TABLE dbo.{name} (id int IDENTITY(1,1) NOT NULL PRIMARY KEY, v int NOT NULL);"
        ),
    );
}

fn setup_plain_table(session: &mut Session, name: &str) {
    run(
        session,
        &format!("CREATE TABLE dbo.{name} (id int NOT NULL PRIMARY KEY, v int NOT NULL);"),
    );
}

#[test]
fn identity_after_an_insert() {
    let mut session = session();
    setup_identity_table(&mut session, "sv_id");
    let events = run(
        &mut session,
        "INSERT INTO dbo.sv_id (v) VALUES (10); SELECT @@IDENTITY, SCOPE_IDENTITY();",
    );
    assert_eq!(
        rows(&events),
        vec![vec![
            Value::Decimal(vauban_types::Decimal {
                mantissa: 1,
                precision: 38,
                scale: 0,
            }),
            Value::Decimal(vauban_types::Decimal {
                mantissa: 1,
                precision: 38,
                scale: 0,
            }),
        ]]
    );

    setup_plain_table(&mut session, "sv_plain");
    let events = run(
        &mut session,
        "INSERT INTO dbo.sv_plain (id, v) VALUES (1, 1); SELECT @@IDENTITY, SCOPE_IDENTITY();",
    );
    assert_eq!(rows(&events), vec![vec![Value::Null, Value::Null]]);
}

#[test]
fn rowcount_follows_the_statement() {
    let mut session = session();
    setup_identity_table(&mut session, "sv_rc");
    let events = run(
        &mut session,
        "INSERT INTO dbo.sv_rc (v) VALUES (1), (2), (3); SELECT @@ROWCOUNT;",
    );
    assert_eq!(rows(&events).last(), Some(&vec![Value::I32(3)]));

    let events = run(
        &mut session,
        "SELECT v FROM dbo.sv_rc WHERE v IN (1, 2); SELECT @@ROWCOUNT;",
    );
    assert_eq!(rows(&events).last(), Some(&vec![Value::I32(2)]));

    let events = run(
        &mut session,
        "SELECT v FROM dbo.sv_rc WHERE 1 = 0; SELECT @@ROWCOUNT;",
    );
    assert_eq!(rows(&events).last(), Some(&vec![Value::I32(0)]));
}

#[test]
fn last_error_is_cleared_by_a_successful_statement() {
    let mut session = session();
    let events = run(&mut session, "SELECT 1/0; SELECT @@ERROR; SELECT @@ERROR;");
    assert_eq!(rows(&events)[0], vec![Value::I32(8134)]);
    assert_eq!(rows(&events)[1], vec![Value::I32(0)]);

    let events = run(&mut session, "SELECT 1/0; DECLARE @x int; SELECT @@ERROR;");
    assert_eq!(rows(&events).last(), Some(&vec![Value::I32(8134)]));

    for stmt in [
        "SELECT 1/0; SET NOCOUNT ON; SELECT @@ERROR;",
        "SELECT 1/0; DECLARE @x int = 1; SELECT @@ERROR;",
        "SELECT 1/0; SELECT 1; SELECT @@ERROR;",
        "SELECT 1/0; PRINT 'x'; SELECT @@ERROR;",
        "SELECT 1/0; USE master; SELECT @@ERROR;",
        "SELECT 1/0; BEGIN TRAN; SELECT @@ERROR; ROLLBACK;",
    ] {
        let events = run(&mut session, stmt);
        let err_rows = rows(&events);
        assert_eq!(
            err_rows.last(),
            Some(&vec![Value::I32(0)]),
            "cleared after success: {stmt}"
        );
    }
}

#[test]
fn ident_current_before_any_insert_returns_seed() {
    let mut session = session();
    setup_identity_table(&mut session, "sv_seed");
    let events = run(&mut session, "SELECT IDENT_CURRENT('dbo.sv_seed');");
    assert_eq!(
        rows(&events),
        vec![vec![Value::Decimal(vauban_types::Decimal {
            mantissa: 1,
            precision: 38,
            scale: 0,
        })]]
    );
}

#[test]
fn trancount_and_xact_state_read_the_session_transaction() {
    let mut session = session();
    let events = run(&mut session, "SELECT @@TRANCOUNT, XACT_STATE();");
    assert_eq!(rows(&events), vec![vec![Value::I32(0), Value::I16(0)]]);

    let events = run(
        &mut session,
        "BEGIN TRAN; SELECT @@TRANCOUNT, XACT_STATE(); COMMIT;",
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(1), Value::I16(1)]]);
}

#[test]
fn ident_current_crosses_sessions() {
    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
    vauban_sysfn::register_builtins();
    let mut writer = Session::new(Arc::clone(&engine), SessionState::new(SPID));
    setup_identity_table(&mut writer, "sv_cross");
    run(&mut writer, "INSERT INTO dbo.sv_cross (v) VALUES (42);");

    let mut reader = Session::new(engine, SessionState::new(SPID + 1));
    let events = run(&mut reader, "SELECT IDENT_CURRENT('dbo.sv_cross');");
    assert_eq!(
        rows(&events),
        vec![vec![Value::Decimal(vauban_types::Decimal {
            mantissa: 1,
            precision: 38,
            scale: 0,
        })]]
    );
    assert_ne!(reader.state().last_identity, writer.state().last_identity);
}

#[test]
fn scope_identity_is_numeric_38_0() {
    let mut session = session();
    setup_identity_table(&mut session, "sv_type");
    run(&mut session, "INSERT INTO dbo.sv_type (v) VALUES (1);");
    let events = run(&mut session, "SELECT SCOPE_IDENTITY() AS c WHERE 1 = 0;");
    let cols = columns(&events, 0);
    assert_eq!(cols.len(), 1);
    assert_eq!(
        cols[0].ty,
        TypeInfo::new(
            SqlType::Numeric {
                precision: 38,
                scale: 0
            },
            true
        )
    );
}
