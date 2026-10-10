//! A value bound to a declared parameter of `sp_executesql`, `sp_prepexec` or
//! `sp_execute` takes the declared type of that parameter, and the final value of an
//! `OUTPUT` parameter takes the type of what receives it.

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
    /// Number, state and line of each error.
    errors: Vec<(u32, u8, u32)>,
    returned: Vec<(String, TypeInfo, Value)>,
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
        self.errors.push((error.number, error.state, error.line));
        Ok(())
    }

    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        Ok(())
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.returned
            .push((name.to_owned(), ty.clone(), value.clone()));
        Ok(())
    }

    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    let engine =
        Engine::with_system_procedures(Arc::new(MemoryStorage::new()), system_procedures());
    Session::new(Arc::new(engine), SessionState::new(51))
}

fn batch(session: &mut Session, text: &str) -> Recording {
    let mut sink = Recording::default();
    session.run_batch(text, &mut sink).expect("batch runs");
    sink
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

fn rpc(name: &str, params: Vec<RpcParam>) -> Rpc {
    Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name(name.to_owned()),
        options: 0,
        transaction_descriptor: 0,
        params,
    }
}

#[test]
fn a_numeric_argument_compares_with_an_int_constant() {
    let mut session = session();
    let sink = batch(
        &mut session,
        "EXEC sp_executesql N'SELECT 1 AS one WHERE @p > 1', N'@p numeric(5,2)', @p = 2;",
    );
    assert_eq!(sink.errors, Vec::new());
    assert_eq!(sink.rows, vec![vec![Value::I32(1)]]);
}

#[test]
fn an_output_takes_the_type_of_the_variable_it_returns_to() {
    let mut session = session();
    let sink = batch(
        &mut session,
        "DECLARE @o float; \
         EXEC sp_executesql N'SET @o = 0.5', N'@o float(10) OUTPUT', @o = @o OUTPUT; \
         SELECT @o AS o;",
    );
    assert_eq!(sink.errors, Vec::new());
    assert_eq!(sink.rows, vec![vec![Value::F64(0.5)]]);
}

#[test]
fn a_wider_datetime2_argument_takes_the_declared_scale() {
    let mut session = session();
    let bound = batch(
        &mut session,
        "DECLARE @d datetime2(7) = '2020-01-01 10:00:00.1234567'; \
         EXEC sp_executesql N'SELECT @p AS p', N'@p datetime2(3)', @p = @d;",
    );
    let cast = batch(
        &mut session,
        "SELECT CAST(CAST('2020-01-01 10:00:00.1234567' AS datetime2(7)) AS datetime2(3)) AS p;",
    );
    assert_eq!(bound.errors, Vec::new());
    assert_eq!(bound.rows, cast.rows);
}

#[test]
fn a_wider_float_argument_takes_the_declared_real() {
    let mut session = session();
    let sink = batch(
        &mut session,
        "DECLARE @f float = 0.1; EXEC sp_executesql N'SELECT @p AS p', N'@p float(10)', @p = @f;",
    );
    assert_eq!(sink.errors, Vec::new());
    assert_eq!(sink.rows, vec![vec![Value::F32(0.1)]]);
}

#[test]
fn argument_conversion_errors_leave_without_a_line() {
    for (args, expected) in [
        ("N'@p tinyint', @p = 300", (8114, 5, 0)),
        ("N'@p date', @p = 5", (206, 2, 0)),
        ("N'@p int', @p = @d", (257, 3, 0)),
    ] {
        let mut session = session();
        let sink = batch(
            &mut session,
            &format!(
                "DECLARE @d datetime = '20200102';\n\n\
                 EXEC sp_executesql N'SELECT @p AS p', {args};"
            ),
        );
        assert_eq!(sink.errors, vec![expected], "{args}");
    }
}

#[test]
fn an_argument_that_does_not_convert_ends_its_statement_alone() {
    let mut session = session();
    let sink = batch(
        &mut session,
        "EXEC sp_executesql N'SELECT @p AS p', N'@p int', @p = 'x';\nSELECT @@ERROR AS e;",
    );
    assert_eq!(sink.errors, vec![(8114, 5, 0)]);
    assert_eq!(sink.rows, vec![vec![Value::I32(8114)]]);
}

#[test]
fn an_output_its_variable_cannot_hold_ends_the_batch() {
    let mut session = session();
    let sink = batch(
        &mut session,
        "DECLARE @t tinyint; \
         EXEC sp_executesql N'SET @t = 300', N'@t int OUTPUT', @t = @t OUTPUT;\n\
         SELECT 2 AS two;",
    );
    assert_eq!(sink.errors.len(), 1, "{:?}", sink.errors);
    assert_eq!((sink.errors[0].0, sink.errors[0].1), (8114, 2));
    assert!(sink.rows.is_empty(), "{:?}", sink.rows);
}

#[test]
fn an_rpc_output_takes_the_type_the_rpc_gave_it() {
    let mut session = session();
    let mut sink = Recording::default();
    let float = TypeInfo::new(SqlType::Float, true);
    session
        .run_rpc(
            &rpc(
                "sp_executesql",
                vec![
                    ntext("SET @o = 0.5"),
                    ntext("@o float(10) OUTPUT"),
                    RpcParam {
                        name: "@o".to_owned(),
                        output: true,
                        default: false,
                        ty: float.clone(),
                        value: Value::Null,
                    },
                ],
            ),
            &mut sink,
        )
        .expect("rpc runs");
    assert_eq!(sink.errors, Vec::new());
    assert_eq!(
        sink.returned,
        vec![("@o".to_owned(), float, Value::F64(0.5))]
    );
}

#[test]
fn an_rpc_prepexec_argument_takes_the_declared_type() {
    let mut session = session();
    let mut sink = Recording::default();
    session
        .run_rpc(
            &rpc(
                "sp_prepexec",
                vec![
                    RpcParam {
                        name: String::new(),
                        output: true,
                        default: false,
                        ty: TypeInfo::new(SqlType::Int, true),
                        value: Value::Null,
                    },
                    ntext("@p numeric(5,2)"),
                    ntext("SELECT 1 AS one WHERE @p > 1"),
                    RpcParam {
                        name: String::new(),
                        output: false,
                        default: false,
                        ty: TypeInfo::new(SqlType::Int, false),
                        value: Value::I32(2),
                    },
                ],
            ),
            &mut sink,
        )
        .expect("rpc runs");
    assert_eq!(sink.errors, Vec::new());
    assert_eq!(sink.rows, vec![vec![Value::I32(1)]]);
}
