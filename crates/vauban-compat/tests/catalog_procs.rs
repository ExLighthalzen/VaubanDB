//! Integration tests for catalog procedure resolution.

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

fn int(value: i32) -> Value {
    Value::I32(value)
}

fn nvarchar_ty() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Max), false)
}

fn int_ty() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
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

#[test]
fn sp_tables_resolves_to_template_with_five_params() {
    let action = resolve_system_procedure("sp_tables", &[])
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Template { params, .. } => {
            assert_eq!(params.len(), 5);
            assert_eq!(params[0].name, "@table_name");
            assert_eq!(params[4].name, "@fUsePattern");
        }
        other => panic!("expected Template, got {other:?}"),
    }
}

#[test]
fn sp_columns_100_is_an_alias() {
    let name = nvarchar("t");
    let nv = nvarchar_ty();
    let action =
        resolve_system_procedure("sp_columns_100", &[arg(Some("@table_name"), &nv, &name)])
            .expect("known")
            .expect("ok");
    assert!(matches!(action, ProcAction::Template { .. }));
}

#[test]
fn sp_columns_rejects_fusepattern_on_the_base_form() {
    let name = nvarchar("t");
    let nv = nvarchar_ty();
    let err = resolve_system_procedure(
        "sp_columns",
        &[
            arg(Some("@table_name"), &nv, &name),
            arg(Some("@fUsePattern"), &int_ty(), &int(0)),
        ],
    )
    .expect("known")
    .unwrap_err();
    assert_eq!(err.number, 8145);
}

#[test]
fn sp_databases_rejects_any_argument() {
    let err = resolve_system_procedure("sp_databases", &[arg(None, &int_ty(), &int(1))])
        .expect("known")
        .unwrap_err();
    assert_eq!(err.number, 8146);
}

#[test]
fn sys_prefix_normalises_for_sp_tables() {
    let nv = nvarchar_ty();
    assert!(resolve_system_procedure("sys.sp_tables", &[]).is_some());
    assert!(resolve_system_procedure("[sys].[sp_tables]", &[]).is_some());
    let _ = nv;
}

#[derive(Default)]
struct Recording {
    errors: Vec<SqlError>,
    columns: usize,
    rows: usize,
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
    fn done_in_proc(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }
    fn done_proc(&mut self, _rowcount: Option<u64>) -> SqlResult<()> {
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

#[test]
fn batch_exec_sp_server_info_returns_rows() {
    vauban_sysfn::register_builtins();
    register_functions();
    let mut session = Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(91),
    );
    let mut sink = Recording::default();
    session
        .run_batch("EXEC sys.sp_server_info @attribute_id = 1;", &mut sink)
        .expect("batch completes");
    assert!(
        sink.errors.is_empty(),
        "errors: {:?}",
        sink.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert_eq!(sink.columns, 1);
    assert_eq!(sink.rows, 1);
}
