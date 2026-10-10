//! The depth of the levels calls open, and its limit.

use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

use crate::state::SessionState;
use crate::{Engine, ResultSink, Session};

/// The rows and the errors a request sent.
#[derive(Default)]
struct Received {
    rows: Vec<Vec<Value>>,
    errors: Vec<SqlError>,
}

impl ResultSink for Received {
    fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.rows.push(row.to_vec());
        Ok(())
    }
    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.errors.push(err.clone());
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

fn session() -> Session {
    vauban_sysfn::register_builtins();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(57),
    )
}

fn run(session: &mut Session, text: &str) -> Received {
    let mut sink = Received::default();
    session
        .run_batch(text, &mut sink)
        .expect("the response is complete");
    sink
}

/// Two dynamic texts, one inside the other, run two levels below the batch: under a limit
/// of two levels they run; a third one inside them is refused before it runs, with the
/// internal error, and ends the batch. The depth is back to the batch's once the request
/// is over.
#[test]
fn a_call_past_the_nesting_limit_is_refused() {
    let mut session = session();
    session.exec_mut().max_nest_level = Some(2);

    let two = run(&mut session, "EXEC('EXEC(''SELECT 2'')'); SELECT 9;");
    assert!(two.errors.is_empty(), "{:?}", two.errors);
    assert_eq!(two.rows, [[Value::I32(2)], [Value::I32(9)]]);
    assert_eq!(session.exec().nest_level, 0);

    let three = run(
        &mut session,
        "EXEC('EXEC(''EXEC(''''SELECT 3'''')'')'); SELECT 9;",
    );
    let numbers: Vec<u32> = three.errors.iter().map(|err| err.number).collect();
    assert_eq!(numbers, [50000], "{:?}", three.errors);
    assert!(three.rows.is_empty(), "{:?}", three.rows);
    assert_eq!(session.exec().nest_level, 0);
}

/// A call made through an RPC opens its level here, one below the batch, so the limit
/// counts it like a call written in a program: `sp_executesql` of a text that calls a text
/// is three levels under a limit of two, and the innermost call is refused.
#[test]
fn a_level_opened_for_a_text_run_alone_counts_toward_the_limit() {
    let mut session = session();
    session.exec_mut().max_nest_level = Some(2);
    let mut sink = Received::default();
    let outcome = session
        .run_nested("EXEC('SELECT 2')", &[], &mut sink)
        .expect("the response is complete");
    assert!(!outcome.failed, "{:?}", sink.errors);
    assert_eq!(sink.rows, [[Value::I32(2)]]);

    let mut sink = Received::default();
    let outcome = session
        .run_nested("EXEC('EXEC(''SELECT 3'')')", &[], &mut sink)
        .expect("the response is complete");
    assert!(outcome.failed);
    let numbers: Vec<u32> = sink.errors.iter().map(|err| err.number).collect();
    assert_eq!(numbers, [50000], "{:?}", sink.errors);
    assert_eq!(session.exec().nest_level, 0);
}
