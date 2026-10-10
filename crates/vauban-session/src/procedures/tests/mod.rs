//! The depth of the levels calls open, and its limit.

use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange, ResetConnection, Rpc, RpcParam, RpcProc};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::rpc::{ProcAction, ProcArg, SystemProcedureResolver};
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

/// `sp_executesql` of a text without parameters, the one procedure these tests call.
fn executesql_resolver() -> Arc<SystemProcedureResolver> {
    fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>> {
        if !name.eq_ignore_ascii_case("sp_executesql") {
            return None;
        }
        let statement = args.iter().find_map(|arg| match arg.value {
            Value::String(text) => Some(text.text.clone()),
            _ => None,
        })?;
        Some(Ok(ProcAction::ExecuteSql {
            statement,
            params: Vec::new(),
        }))
    }
    Arc::new(resolve)
}

/// A session whose engine resolves `sp_executesql`.
fn session_with_executesql() -> Session {
    vauban_sysfn::register_builtins();
    Session::new(
        Arc::new(Engine::with_system_procedures(
            Arc::new(MemoryStorage::new()),
            executesql_resolver(),
        )),
        SessionState::new(57),
    )
}

/// An RPC of `sp_executesql` on `text`.
fn executesql_rpc(text: &str) -> Rpc {
    Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name("sp_executesql".into()),
        options: 0,
        params: vec![RpcParam {
            name: String::new(),
            output: false,
            default: false,
            ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
            value: Value::String(SqlString {
                text: text.to_owned(),
            }),
        }],
        transaction_descriptor: 0,
    }
}

fn numbers(errors: &[SqlError]) -> Vec<u32> {
    errors.iter().map(|err| err.number).collect()
}

fn run(session: &mut Session, text: &str) -> Received {
    let mut sink = Received::default();
    session
        .run_batch(text, &mut sink)
        .expect("the response is complete");
    sink
}

/// Two dynamic texts, one inside the other, run two levels below the batch: under a limit
/// of two levels they run; a third one inside them is refused before it runs, with 217,
/// and ends the batch. The depth is back to the batch's once the request
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
    assert_eq!(numbers(&three.errors), [217], "{:?}", three.errors);
    let refused = &three.errors[0];
    assert_eq!((refused.severity, refused.state), (16, 1));
    assert!(refused.message.contains("limit of 2 levels"), "{refused:?}");
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
    assert_eq!(numbers(&sink.errors), [217], "{:?}", sink.errors);
    assert_eq!(session.exec().nest_level, 0);
}

/// `sp_executesql` runs its text two levels below the statement that calls it, where
/// `EXEC('…')` runs its text one level below. Under a limit of two levels, the text of
/// `sp_executesql` written in the batch runs, and so does a dynamic text inside a dynamic
/// text; `sp_executesql` written in a dynamic text would run its text at level 3, and is
/// refused with 217 at line 1 of `sp_executesql`, which ends the batch. Under a limit of
/// three it runs.
#[test]
fn sp_executesql_runs_its_text_two_levels_down() {
    let mut session = session_with_executesql();
    session.exec_mut().max_nest_level = Some(2);

    let two = run(
        &mut session,
        "EXEC sp_executesql N'SELECT 1'; EXEC('EXEC(''SELECT 2'')'); SELECT 9;",
    );
    assert!(two.errors.is_empty(), "{:?}", two.errors);
    assert_eq!(
        two.rows,
        [[Value::I32(1)], [Value::I32(2)], [Value::I32(9)]]
    );

    let three = run(
        &mut session,
        "EXEC('SELECT 1; EXEC sp_executesql N''SELECT 3'''); SELECT 9;",
    );
    assert_eq!(numbers(&three.errors), [217], "{:?}", three.errors);
    let refused = &three.errors[0];
    assert_eq!(refused.line, 1);
    assert_eq!(refused.procedure.as_deref(), Some("sp_executesql"));
    assert_eq!(three.rows, [[Value::I32(1)]]);
    assert_eq!(session.exec().nest_level, 0);

    session.exec_mut().max_nest_level = Some(3);
    let deeper = run(
        &mut session,
        "EXEC('SELECT 1; EXEC sp_executesql N''SELECT 3'''); SELECT 9;",
    );
    assert!(deeper.errors.is_empty(), "{:?}", deeper.errors);
    assert_eq!(
        deeper.rows,
        [[Value::I32(1)], [Value::I32(3)], [Value::I32(9)]]
    );
}

/// The text of `sp_executesql` an RPC calls runs at level 2: under a limit of two levels a
/// dynamic text inside it is refused with 217, under a limit of three it runs.
#[test]
fn the_text_of_an_rpc_of_sp_executesql_runs_at_level_two() {
    let mut session = session_with_executesql();
    session.exec_mut().max_nest_level = Some(2);
    let rpc = executesql_rpc("SELECT 1; EXEC('SELECT 3'); SELECT 4;");
    let mut sink = Received::default();
    session
        .run_rpc(&rpc, &mut sink)
        .expect("the response is complete");
    assert_eq!(numbers(&sink.errors), [217], "{:?}", sink.errors);
    assert_eq!(sink.rows, [[Value::I32(1)]]);
    assert_eq!(session.exec().nest_level, 0);

    session.exec_mut().max_nest_level = Some(3);
    let mut sink = Received::default();
    session
        .run_rpc(&rpc, &mut sink)
        .expect("the response is complete");
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert_eq!(
        sink.rows,
        [[Value::I32(1)], [Value::I32(3)], [Value::I32(4)]]
    );
}

/// A dynamic text read from a table that runs itself again, with the limit of a new
/// session: 32 levels run, the 33rd is refused with 217, the batch ends, and the session
/// answers the next batch. Inside a transaction, 217 rolls it back: the writes of the 32
/// levels are gone and `@@TRANCOUNT` reads 0.
#[test]
fn a_dynamic_text_that_runs_itself_stops_at_level_33() {
    let mut session = session();
    assert_eq!(session.exec().max_nest_level, Some(32));
    let setup = run(
        &mut session,
        "CREATE TABLE dbo.q (t nvarchar(max)); CREATE TABLE dbo.c (n int); \
         INSERT dbo.c VALUES (0); \
         INSERT dbo.q VALUES (N'DECLARE @t nvarchar(max); SELECT @t = t FROM dbo.q; \
         UPDATE dbo.c SET n = n + 1; EXEC(@t)');",
    );
    assert!(setup.errors.is_empty(), "{:?}", setup.errors);

    let deep = run(
        &mut session,
        "DECLARE @t nvarchar(max); SELECT @t = t FROM dbo.q; EXEC(@t); SELECT 9;",
    );
    assert_eq!(numbers(&deep.errors), [217], "{:?}", deep.errors);
    assert!(deep.errors[0].message.contains("limit of 32 levels"));
    assert!(deep.rows.is_empty(), "{:?}", deep.rows);
    assert_eq!(session.exec().nest_level, 0);
    let after = run(&mut session, "SELECT n, @@TRANCOUNT FROM dbo.c;");
    assert!(after.errors.is_empty(), "{:?}", after.errors);
    assert_eq!(after.rows, [[Value::I32(32), Value::I32(0)]]);

    let in_transaction = run(
        &mut session,
        "BEGIN TRANSACTION; DECLARE @t nvarchar(max); SELECT @t = t FROM dbo.q; EXEC(@t);",
    );
    assert_eq!(numbers(&in_transaction.errors), [217]);
    let after = run(&mut session, "SELECT n, @@TRANCOUNT FROM dbo.c;");
    assert!(after.errors.is_empty(), "{:?}", after.errors);
    assert_eq!(after.rows, [[Value::I32(32), Value::I32(0)]]);
}
