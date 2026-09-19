//! Integration tests of batch `EXEC` routed through procedure dispatch.

use std::sync::{Arc, Once};

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{
    Engine, ProcAction, ProcParam, ResultSink, Session, SessionState,
    register_system_procedure_resolver,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

const SPID: i16 = 72;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns,
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    DoneInProc,
    Error(u32),
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
        self.0.push(Event::Columns);
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
    fn done_in_proc(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        self.0.push(Event::DoneInProc);
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.number));
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

fn register_test_resolver() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        register_system_procedure_resolver(|name, args| {
            if !name.eq_ignore_ascii_case("sp_executesql")
                && !name.to_ascii_lowercase().ends_with(".sp_executesql")
            {
                return None;
            }
            let mut params = Vec::new();
            for arg in args {
                let Some(name) = arg.name.filter(|name| name.starts_with('@')) else {
                    continue;
                };
                if name == "@statement" || name == "@stmt" || name == "@params" || name == "params"
                {
                    continue;
                }
                params.push(ProcParam {
                    name: name.to_owned(),
                    ty: arg.ty.clone(),
                    value: arg.value.clone(),
                    output: arg.output,
                });
            }
            let statement = args
                .iter()
                .find_map(|arg| match (arg.name, arg.value) {
                    (None | Some("@statement") | Some("@stmt"), Value::String(text)) => {
                        Some(text.text.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "SET @o = @a + 1".into());
            Some(Ok(ProcAction::ExecuteSql { statement, params }))
        });
    });
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    register_test_resolver();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(SPID),
    )
}

fn run(text: &str) -> Recording {
    let mut sink = Recording::default();
    session()
        .run_batch(text, &mut sink)
        .expect("batch completes");
    sink
}

#[test]
fn batch_sp_executesql_with_output_writes_the_caller_variable() {
    let events = run(
        "DECLARE @v int; EXEC sp_executesql N'SET @o = @a + 1', N'@a int, @o int OUTPUT', @a = 6, @o = @v OUTPUT; SELECT @v AS n;",
    );
    assert!(
        events
            .0
            .iter()
            .any(|event| matches!(event, Event::Row(values) if values == &[Value::I32(7)])),
        "{:?}",
        events.0
    );
}

#[test]
fn exec_literal_select_returns_one_row() {
    let events = run("EXEC('SELECT 1 AS n');");
    assert!(events.0.contains(&Event::Columns));
    assert!(
        events
            .0
            .iter()
            .any(|event| matches!(event, Event::Row(values) if values == &[Value::I32(1)]))
    );
}

#[test]
fn exec_null_text_emits_no_row() {
    let events = run("DECLARE @t nvarchar(100) = NULL; EXEC(@t);");
    assert!(
        !events.0.iter().any(|event| matches!(event, Event::Row(_))),
        "{:?}",
        events.0
    );
}
