use std::sync::Arc;

use crate::{Engine, Session, SessionState};
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

use crate::sink::ResultSink;

use super::*;

#[derive(Default)]
struct Recording(Vec<Event>);

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Done(Option<u64>, bool),
    Error(SqlError),
    EnvChange(EnvChange),
}

impl ResultSink for Recording {
    fn columns(&mut self, _: &[ColumnMeta]) -> SqlResult<()> {
        Ok(())
    }
    fn row(&mut self, _: &[Value]) -> SqlResult<()> {
        Ok(())
    }
    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.0.push(Event::Done(rowcount, more));
        Ok(())
    }
    fn info(&mut self, _: &InfoMessage) -> SqlResult<()> {
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
        SessionState::new(81),
    )
}

#[test]
fn release_is_idempotent() {
    let mut s = session();
    let mut sink = Recording::default();
    s.run_batch("BEGIN TRAN", &mut sink)
        .expect("begin through the sink");
    release(&mut s, Release::Disconnect).expect("first release");
    release(&mut s, Release::Disconnect).expect("second release");
}
