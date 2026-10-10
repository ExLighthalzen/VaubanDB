//! An engine serves system procedures through the resolver it is built with, and through no
//! process-wide registration.

use std::sync::Arc;

use vauban_compat::system_procedures;
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange, ResetConnection, Rpc, RpcParam, RpcProc};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

#[derive(Default)]
struct Recording {
    rows: Vec<Vec<Value>>,
    errors: Vec<u32>,
}

impl ResultSink for Recording {
    fn columns(&mut self, _columns: &[ColumnMeta]) -> SqlResult<()> {
        Ok(())
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.rows.push(row.to_vec());
        Ok(())
    }

    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }

    fn info(&mut self, _message: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }

    fn error(&mut self, error: &SqlError) -> SqlResult<()> {
        self.errors.push(error.number);
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

fn session(engine: Engine) -> Session {
    vauban_sysfn::register_builtins();
    Session::new(Arc::new(engine), SessionState::new(51))
}

fn with_resolver() -> Session {
    session(Engine::with_system_procedures(
        Arc::new(MemoryStorage::new()),
        system_procedures(),
    ))
}

fn without_resolver() -> Session {
    session(Engine::new(Arc::new(MemoryStorage::new())))
}

fn ntext(text: &str) -> RpcParam {
    RpcParam {
        name: String::new(),
        output: false,
        default: false,
        ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
        value: Value::String(SqlString {
            text: text.to_owned(),
        }),
    }
}

/// `sp_executesql N'SELECT @p + 1', N'@p int', @p = 41`, called by name or by `ProcID`.
fn executesql(proc: RpcProc) -> Rpc {
    Rpc {
        reset: ResetConnection::None,
        proc,
        options: 0,
        transaction_descriptor: 0,
        params: vec![
            ntext("SELECT @p + 1"),
            ntext("@p int"),
            RpcParam {
                name: "@p".to_owned(),
                output: false,
                default: false,
                ty: TypeInfo::new(SqlType::Int, false),
                value: Value::I32(41),
            },
        ],
    }
}

#[test]
fn an_engine_built_with_the_resolver_runs_sp_executesql_by_rpc_name() {
    let mut session = with_resolver();
    let mut sink = Recording::default();
    session
        .run_rpc(
            &executesql(RpcProc::Name("sp_executesql".to_owned())),
            &mut sink,
        )
        .expect("rpc runs");
    assert_eq!(sink.errors, Vec::<u32>::new());
    assert_eq!(sink.rows, vec![vec![Value::I32(42)]]);
}

#[test]
fn an_engine_built_with_the_resolver_runs_sp_executesql_by_proc_id() {
    let mut session = with_resolver();
    let mut sink = Recording::default();
    session
        .run_rpc(&executesql(RpcProc::Id(10)), &mut sink)
        .expect("rpc runs");
    assert_eq!(sink.errors, Vec::<u32>::new());
    assert_eq!(sink.rows, vec![vec![Value::I32(42)]]);
}

#[test]
fn an_engine_built_with_the_resolver_runs_sp_executesql_from_a_batch() {
    let mut session = with_resolver();
    let mut sink = Recording::default();
    session
        .run_batch("EXEC sp_executesql N'SELECT 7';", &mut sink)
        .expect("batch runs");
    assert_eq!(sink.errors, Vec::<u32>::new());
    assert_eq!(sink.rows, vec![vec![Value::I32(7)]]);
}

/// The resolver belongs to the engine: an engine built without one answers 2812, even in a
/// process where another engine carries the resolver.
#[test]
fn an_engine_built_without_the_resolver_answers_2812_next_to_one_built_with_it() {
    let mut served = with_resolver();
    let mut sink = Recording::default();
    served
        .run_batch("EXEC sp_executesql N'SELECT 7';", &mut sink)
        .expect("batch runs");
    assert_eq!(sink.rows, vec![vec![Value::I32(7)]]);

    let mut bare = without_resolver();
    let mut sink = Recording::default();
    bare.run_batch("EXEC sp_executesql N'SELECT 7';", &mut sink)
        .expect("batch runs");
    assert_eq!(sink.errors, vec![2812]);
    assert!(sink.rows.is_empty());

    let mut sink = Recording::default();
    bare.run_rpc(
        &executesql(RpcProc::Name("sp_executesql".to_owned())),
        &mut sink,
    )
    .expect("rpc runs");
    assert_eq!(sink.errors, vec![2812]);
    assert!(sink.rows.is_empty());
}
