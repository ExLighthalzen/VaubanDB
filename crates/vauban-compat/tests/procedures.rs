//! Integration tests for `resolve_system_procedure`.

use vauban_compat::{ProcAction, ProcArg, resolve_system_procedure};
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

fn varchar_ty() -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(100)), false)
}

fn arg<'a>(name: Option<&'a str>, ty: &'a TypeInfo, value: &'a Value, output: bool) -> ProcArg<'a> {
    ProcArg {
        name,
        ty,
        value,
        output,
        default: false,
    }
}

#[test]
fn sp_executesql_named_form_binds_execute_sql() {
    let stmt = nvarchar("SELECT @a AS n");
    let params = nvarchar("@a int");
    let value = int(1);
    let nvarchar_type = nvarchar_ty();
    let int_type = int_ty();
    let args = [
        arg(Some("@stmt"), &nvarchar_type, &stmt, false),
        arg(Some("@params"), &nvarchar_type, &params, false),
        arg(Some("@a"), &int_type, &value, false),
    ];
    let action = resolve_system_procedure("sp_executesql", &args)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::ExecuteSql { statement, params } => {
            assert_eq!(statement, "SELECT @a AS n");
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "@a");
            assert_eq!(params[0].ty.ty, SqlType::Int);
            assert_eq!(params[0].value, int(1));
        }
        other => panic!("expected ExecuteSql, got {other:?}"),
    }
}

#[test]
fn sp_executesql_positional_form_binds_execute_sql() {
    let stmt = nvarchar("SELECT @a AS n");
    let params = nvarchar("@a int");
    let value = int(1);
    let nvarchar_type = nvarchar_ty();
    let int_type = int_ty();
    let args = [
        arg(None, &nvarchar_type, &stmt, false),
        arg(None, &nvarchar_type, &params, false),
        arg(Some("@a"), &int_type, &value, false),
    ];
    let action = resolve_system_procedure("sp_executesql", &args)
        .expect("known")
        .expect("ok");
    assert!(matches!(action, ProcAction::ExecuteSql { .. }));
}

#[test]
fn sp_executesql_user_handle_param_is_not_8145() {
    let stmt = nvarchar("SELECT @handle AS n");
    let params = nvarchar("@handle int");
    let value = int(1);
    let nvarchar_type = nvarchar_ty();
    let int_type = int_ty();
    let args = [
        arg(None, &nvarchar_type, &stmt, false),
        arg(None, &nvarchar_type, &params, false),
        arg(Some("@handle"), &int_type, &value, false),
    ];
    let action = resolve_system_procedure("sp_executesql", &args)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::ExecuteSql { params, .. } => {
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "@handle");
            assert_eq!(params[0].value, int(1));
        }
        other => panic!("expected ExecuteSql, got {other:?}"),
    }
}

#[test]
fn sp_prepare_positional_and_named_forms() {
    let handle = int(0);
    let params = nvarchar("@P1 int");
    let stmt = nvarchar("SELECT @P1");
    let int_type = int_ty();
    let nvarchar_type = nvarchar_ty();
    let positional = [
        arg(None, &int_type, &handle, true),
        arg(None, &nvarchar_type, &params, false),
        arg(None, &nvarchar_type, &stmt, false),
    ];
    let action = resolve_system_procedure("sp_prepare", &positional)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Prepare {
            statement,
            handle_arg,
            execute_with,
            ..
        } => {
            assert_eq!(statement, "SELECT @P1");
            assert_eq!(handle_arg, 0);
            assert!(execute_with.is_none());
        }
        other => panic!("expected Prepare, got {other:?}"),
    }

    let named = [
        arg(Some("@handle"), &int_type, &handle, true),
        arg(Some("@params"), &nvarchar_type, &params, false),
        arg(Some("@stmt"), &nvarchar_type, &stmt, false),
    ];
    let action = resolve_system_procedure("sp_prepare", &named)
        .expect("known")
        .expect("ok");
    assert!(matches!(action, ProcAction::Prepare { .. }));
}

#[test]
fn sp_execute_positional_and_named_forms() {
    let handle = int(5);
    let value = int(1);
    let int_type = int_ty();
    let positional = [
        arg(None, &int_type, &handle, false),
        arg(None, &int_type, &value, false),
    ];
    let action = resolve_system_procedure("sp_execute", &positional)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Execute { handle, params } => {
            assert_eq!(handle, 5);
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "@P1");
            assert_eq!(params[0].value, int(1));
        }
        other => panic!("expected Execute, got {other:?}"),
    }

    let named = [
        arg(Some("@handle"), &int_type, &handle, false),
        arg(Some("@P1"), &int_type, &value, false),
    ];
    let action = resolve_system_procedure("sp_execute", &named)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Execute { handle, params } => {
            assert_eq!(handle, 5);
            assert_eq!(params[0].name, "@P1");
        }
        other => panic!("expected Execute, got {other:?}"),
    }
}

#[test]
fn sp_execute_user_stmt_param_is_not_8145() {
    let handle = int(5);
    let value = int(1);
    let int_type = int_ty();
    let args = [
        arg(Some("@handle"), &int_type, &handle, false),
        arg(Some("@stmt"), &int_type, &value, false),
    ];
    let action = resolve_system_procedure("sp_execute", &args)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Execute { handle, params } => {
            assert_eq!(handle, 5);
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "@stmt");
        }
        other => panic!("expected Execute, got {other:?}"),
    }
}

#[test]
fn sp_unprepare_positional_and_named_forms() {
    let handle = int(7);
    let int_type = int_ty();
    let positional = [arg(None, &int_type, &handle, false)];
    let action = resolve_system_procedure("sp_unprepare", &positional)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Unprepare { handle } => assert_eq!(handle, 7),
        other => panic!("expected Unprepare, got {other:?}"),
    }

    let named = [arg(Some("@handle"), &int_type, &handle, false)];
    let action = resolve_system_procedure("sp_unprepare", &named)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Unprepare { handle } => assert_eq!(handle, 7),
        other => panic!("expected Unprepare, got {other:?}"),
    }
}

#[test]
fn sp_prepexec_prepares_and_executes_once() {
    let handle = int(0);
    let params = nvarchar("@P1 int");
    let stmt = nvarchar("SELECT @P1");
    let value = int(5);
    let int_type = int_ty();
    let nvarchar_type = nvarchar_ty();
    let args = [
        arg(None, &int_type, &handle, true),
        arg(None, &nvarchar_type, &params, false),
        arg(None, &nvarchar_type, &stmt, false),
        arg(None, &int_type, &value, false),
    ];
    let action = resolve_system_procedure("sp_prepexec", &args)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Prepare {
            handle_arg,
            execute_with,
            ..
        } => {
            assert_eq!(handle_arg, 0);
            let values = execute_with.expect("execute_with");
            assert_eq!(values[0].name, "@P1");
            assert_eq!(values[0].value, int(5));
        }
        other => panic!("expected Prepare, got {other:?}"),
    }
}

#[test]
fn sp_executesql_varchar_stmt_is_214() {
    let varchar_type = varchar_ty();
    let nvarchar_type = nvarchar_ty();
    let stmt = nvarchar("SELECT 1");
    let empty = nvarchar("");
    let err = resolve_system_procedure(
        "sp_executesql",
        &[
            arg(None, &varchar_type, &stmt, false),
            arg(None, &nvarchar_type, &empty, false),
        ],
    )
    .expect("known")
    .unwrap_err();
    assert_eq!(err.number, 214);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 2);
}

#[test]
fn sp_executesql_without_arguments_is_201() {
    let err = resolve_system_procedure("sp_executesql", &[])
        .expect("known")
        .unwrap_err();
    assert_eq!(err.number, 201);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 10);
}

#[test]
fn sp_executesql_with_five_arguments_is_8144() {
    let stmt = nvarchar("SELECT @a AS n");
    let params = nvarchar("@a int");
    let value = int(1);
    let nvarchar_type = nvarchar_ty();
    let int_type = int_ty();
    let err = resolve_system_procedure(
        "sp_executesql",
        &[
            arg(None, &nvarchar_type, &stmt, false),
            arg(None, &nvarchar_type, &params, false),
            arg(Some("@a"), &int_type, &value, false),
            arg(Some("@b"), &int_type, &value, false),
            arg(Some("@c"), &int_type, &value, false),
        ],
    )
    .expect("known")
    .unwrap_err();
    assert_eq!(err.number, 8144);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 2);
}

#[test]
fn sp_cursoropen_is_refused_with_2812() {
    let action = resolve_system_procedure("sp_cursoropen", &[])
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Refuse(err) => {
            assert_eq!(err.number, 2812);
            assert_eq!(err.severity, 16);
        }
        other => panic!("expected Refuse, got {other:?}"),
    }
}

#[test]
fn unknown_procedure_returns_none() {
    assert!(resolve_system_procedure("sp_nosuch", &[]).is_none());
}

#[test]
fn sys_prefix_and_brackets_normalise() {
    let stmt = nvarchar("SELECT 1");
    let empty = nvarchar("");
    let nvarchar_type = nvarchar_ty();
    let args = [
        arg(None, &nvarchar_type, &stmt, false),
        arg(None, &nvarchar_type, &empty, false),
    ];
    for name in [
        "sys.sp_executesql",
        "[sys].[sp_executesql]",
        "SP_EXECUTESQL",
    ] {
        assert!(resolve_system_procedure(name, &args).is_some(), "{name}");
    }
}
