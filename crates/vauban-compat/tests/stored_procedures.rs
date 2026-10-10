//! Integration tests for `sp_stored_procedures` resolution and execution.

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
fn sp_stored_procedures_resolves_to_template() {
    let action = resolve_system_procedure("sp_stored_procedures", &[])
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Template { params, .. } => {
            assert_eq!(params.len(), 4);
            assert_eq!(params[0].name, "@sp_name");
            assert_eq!(params[1].name, "@sp_owner");
            assert_eq!(params[2].name, "@sp_qualifier");
            assert_eq!(params[3].name, "@fUsePattern");
        }
        other => panic!("expected Template, got {other:?}"),
    }
}

#[test]
fn sp_stored_procedures_binds_positional_and_named() {
    let name = nvarchar("sample%");
    let owner = nvarchar("dbo");
    let nv = nvarchar_ty();
    let bit_ty = TypeInfo::new(SqlType::Bit, false);
    let zero = Value::Bit(false);

    let positional = [
        arg(None, &nv, &name),
        arg(None, &nv, &owner),
        arg(None, &nv, &Value::Null),
        arg(None, &bit_ty, &zero),
    ];
    let action = resolve_system_procedure("sp_stored_procedures", &positional)
        .unwrap()
        .unwrap();
    let ProcAction::Template {
        params: positional_params,
        ..
    } = action
    else {
        panic!("expected Template");
    };

    let named = [
        arg(Some("@fUsePattern"), &bit_ty, &zero),
        arg(Some("@sp_name"), &nv, &name),
        arg(Some("@sp_owner"), &nv, &owner),
    ];
    let action = resolve_system_procedure("sp_stored_procedures", &named)
        .unwrap()
        .unwrap();
    let ProcAction::Template {
        params: named_params,
        ..
    } = action
    else {
        panic!("expected Template");
    };
    assert_eq!(positional_params, named_params);

    let after_named = [arg(Some("@sp_name"), &nv, &name), arg(None, &nv, &owner)];
    let err = resolve_system_procedure("sp_stored_procedures", &after_named)
        .unwrap()
        .unwrap_err();
    assert_eq!(err.number, 119);
}

#[test]
fn sp_stored_procedures_rejects_unknown_parameter() {
    let value = int(1);
    let ty = int_ty();
    let err = resolve_system_procedure("sp_stored_procedures", &[arg(Some("@bogus"), &ty, &value)])
        .unwrap()
        .unwrap_err();
    assert_eq!(err.number, 8145);
    assert!(err.message.contains("@bogus"));
}

#[test]
fn sys_prefix_normalises_for_sp_stored_procedures() {
    assert!(resolve_system_procedure("sys.sp_stored_procedures", &[]).is_some());
    assert!(resolve_system_procedure("[sys].[sp_stored_procedures]", &[]).is_some());
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

fn new_session() -> Session {
    vauban_sysfn::register_builtins();
    register_functions();
    Session::new(
        Arc::new(Engine::with_system_procedures(
            Arc::new(MemoryStorage::new()),
            vauban_compat::system_procedures(),
        )),
        SessionState::new(91),
    )
}

/// Every branch of the template batch (no argument, `@fUsePattern = 0`, an empty
/// `@sp_qualifier`, and one equal to the current database) must parse, bind and execute:
/// an unreachable statement the binder cannot bind would fail the whole batch, argument
/// values notwithstanding.
#[test]
fn batch_exec_sp_stored_procedures_returns_rows_on_every_branch() {
    let mut session = new_session();
    let mut sink = Recording::default();
    session
        .run_batch("EXEC sys.sp_stored_procedures;", &mut sink)
        .expect("batch completes");
    assert!(
        sink.errors.is_empty(),
        "errors: {:?}",
        sink.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert_eq!(sink.columns, 1);
    assert_eq!(sink.rows, 0, "sys.procedures is empty in V1");

    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC sys.sp_stored_procedures @sp_name = 'no_such_proc', @fUsePattern = 0;",
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty());
    assert_eq!(sink.columns, 1);
    assert_eq!(sink.rows, 0);

    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC sys.sp_stored_procedures @sp_qualifier = '';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty());
    assert_eq!(sink.columns, 1);
    assert_eq!(sink.rows, 0);

    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC sys.sp_stored_procedures @sp_qualifier = 'master';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(
        sink.errors.is_empty(),
        "errors: {:?}",
        sink.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert_eq!(sink.columns, 1);
    assert_eq!(sink.rows, 0);

    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC sys.sp_stored_procedures @sp_qualifier = 'not_the_current_database';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(
        sink.errors.is_empty(),
        "errors: {:?}",
        sink.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert_eq!(sink.columns, 1);
    assert_eq!(sink.rows, 0);
}
