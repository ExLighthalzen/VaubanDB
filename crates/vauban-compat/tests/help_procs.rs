//! Integration tests for `sp_help`, `sp_helpdb` and `sp_spaceused`.

use std::sync::Arc;

use vauban_compat::{ProcAction, ProcArg, register_functions, resolve_system_procedure};
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

fn nvarchar(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

fn nvarchar_ty() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Max), true)
}

fn arg<'a>(name: Option<&'a str>, ty: &'a TypeInfo, value: &'a Value) -> ProcArg<'a> {
    ProcArg {
        name,
        ty,
        value,
        output: false,
        default: false,
    }
}

#[derive(Default)]
struct Recording {
    columns: usize,
    rows: usize,
    errors: Vec<SqlError>,
}

impl ResultSink for Recording {
    fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
        self.columns += 1;
        Ok(())
    }

    fn row(&mut self, _row: &[Value]) -> SqlResult<()> {
        self.rows += 1;
        Ok(())
    }

    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }

    fn info(&mut self, _message: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }

    fn error(&mut self, error: &SqlError) -> SqlResult<()> {
        self.errors.push(error.clone());
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

#[test]
fn resolve_sp_help_returns_a_template_with_null_objname() {
    register_functions();
    let action = resolve_system_procedure("sp_help", &[])
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Template { params, sql } => {
            assert!(sql.contains("sys.all_objects"));
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "@objname");
            assert_eq!(params[0].value, Value::Null);
        }
        other => panic!("expected Template, got {other:?}"),
    }
}

#[test]
fn resolve_sp_helpdb_requires_dbname() {
    register_functions();
    let err = resolve_system_procedure("sp_helpdb", &[])
        .expect("known")
        .unwrap_err();
    assert_eq!(err.number, 201);
}

#[test]
fn resolve_sp_spaceused_accepts_positional_objname() {
    register_functions();
    let name = nvarchar("dbo.t");
    let action = resolve_system_procedure("sp_spaceused", &[arg(None, &nvarchar_ty(), &name)])
        .expect("known")
        .expect("ok");
    assert!(matches!(action, ProcAction::Template { .. }));
}

#[test]
fn sp_spaceused_executes_on_an_empty_table() {
    vauban_sysfn::register_builtins();
    register_functions();
    let mut session = Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(91),
    );
    let mut sink = Recording::default();
    session
        .run_batch(
            "CREATE TABLE dbo.t (id int NOT NULL); EXEC sys.sp_spaceused 'dbo.t';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert_eq!(sink.columns, 1);
    assert_eq!(sink.rows, 1);
}
