//! Integration tests for catalog procedure resolution.

use std::sync::Arc;

use vauban_compat::{ProcAction, ProcArg, register_functions, resolve_system_procedure};
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange, ResetConnection, Rpc, RpcParam, RpcProc};
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
    column_names: Vec<Vec<String>>,
    values: Vec<Vec<Value>>,
    env_changes: usize,
    infos: Vec<u32>,
}

impl ResultSink for Recording {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.columns += 1;
        self.column_names
            .push(cols.iter().map(|col| col.name.clone()).collect());
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.rows += 1;
        self.values.push(row.to_vec());
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
    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
        self.infos.push(msg.number);
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.errors.push(err.clone());
        Ok(())
    }
    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        self.env_changes += 1;
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
fn batch_exec_sp_columns_returns_rows() {
    vauban_sysfn::register_builtins();
    register_functions();
    let mut session = Session::new(
        Arc::new(Engine::with_system_procedures(
            Arc::new(MemoryStorage::new()),
            vauban_compat::system_procedures(),
        )),
        SessionState::new(91),
    );
    let mut sink = Recording::default();
    session
        .run_batch(
            "CREATE TABLE dbo.t (id int NOT NULL); EXEC sys.sp_columns @table_name = 't';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(
        sink.errors.is_empty(),
        "errors: {:?}",
        sink.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert_eq!(sink.columns, 1);
    assert_eq!(sink.rows, 1);
}

#[test]
fn batch_exec_sp_server_info_returns_rows() {
    vauban_sysfn::register_builtins();
    register_functions();
    let mut session = Session::new(
        Arc::new(Engine::with_system_procedures(
            Arc::new(MemoryStorage::new()),
            vauban_compat::system_procedures(),
        )),
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

/// A session holding two user databases, each with one table of its own, and left on
/// `qualified_home`.
fn two_database_session() -> Session {
    vauban_sysfn::register_builtins();
    register_functions();
    let mut session = Session::new(
        Arc::new(Engine::with_system_procedures(
            Arc::new(MemoryStorage::new()),
            vauban_compat::system_procedures(),
        )),
        SessionState::new(91),
    );
    for text in [
        "CREATE DATABASE qualified_other;",
        "USE qualified_other; CREATE TABLE dbo.other_table (id int NOT NULL, label nvarchar(20) NULL);",
        "USE master; CREATE DATABASE qualified_home;",
        "USE qualified_home; CREATE TABLE dbo.home_table (id int NOT NULL);",
    ] {
        let mut sink = Recording::default();
        session.run_batch(text, &mut sink).expect("batch completes");
        assert!(sink.errors.is_empty(), "setup `{text}`: {:?}", sink.errors);
    }
    session
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.text.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn a_three_part_exec_of_sp_tables_reads_the_named_database() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC qualified_other.sys.sp_tables @table_owner = 'dbo', @table_name = 'other_table';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 1);
    let row = &sink.values[0];
    assert_eq!(text_of(&row[0]), "qualified_other");
    assert_eq!(text_of(&row[1]), "dbo");
    assert_eq!(text_of(&row[2]), "other_table");
}

#[test]
fn a_three_part_exec_of_sp_columns_reads_the_named_database() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC qualified_other.sys.sp_columns @table_owner = 'dbo', @table_name = 'other_table';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 2);
    assert_eq!(text_of(&sink.values[0][0]), "qualified_other");
    assert_eq!(text_of(&sink.values[0][3]), "id");
    assert_eq!(text_of(&sink.values[1][3]), "label");
}

#[test]
fn an_unqualified_exec_of_sp_tables_reads_the_caller_database() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC sys.sp_tables @table_owner = 'dbo', @table_name = 'home_table';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 1);
    assert_eq!(text_of(&sink.values[0][0]), "qualified_home");
    assert_eq!(text_of(&sink.values[0][2]), "home_table");
}

#[test]
fn a_qualified_call_leaves_the_caller_database_and_sends_no_context_change() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC qualified_other.sys.sp_tables @table_owner = 'dbo', @table_name = 'other_table'; \
             SELECT DB_NAME();",
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 2);
    assert_eq!(text_of(&sink.values[1][0]), "qualified_home");
    assert_eq!(sink.env_changes, 0);
    assert!(!sink.infos.contains(&5701), "infos: {:?}", sink.infos);
    assert_eq!(session.state().database, "qualified_home");
}

#[test]
fn a_call_qualified_by_a_missing_database_fails_before_the_body_runs() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC qualified_absent.sys.sp_tables @table_owner = 'dbo';",
            &mut sink,
        )
        .expect("batch completes");
    assert_eq!(sink.errors.len(), 1);
    let err = &sink.errors[0];
    assert_eq!((err.number, err.severity, err.state), (911, 16, 4));
    assert_eq!(err.line, 1);
    assert_eq!(sink.columns, 0);
    assert_eq!(sink.rows, 0);
}

#[test]
fn a_call_qualified_by_an_unknown_server_fails_before_the_body_runs() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC elsewhere.qualified_other.sys.sp_tables @table_owner = 'dbo';",
            &mut sink,
        )
        .expect("batch completes");
    assert_eq!(sink.errors.len(), 1);
    let err = &sink.errors[0];
    assert_eq!((err.number, err.severity, err.state), (7202, 11, 2));
    assert_eq!(err.line, 1);
    assert_eq!(sink.columns, 0);
    assert_eq!(sink.rows, 0);
}

#[test]
fn a_four_part_call_naming_this_server_reads_the_named_database() {
    let mut session = two_database_session();
    let server = session.state().server_name.clone();
    let mut sink = Recording::default();
    session
        .run_batch(
            &format!(
                "EXEC {server}.qualified_other.sys.sp_tables @table_owner = 'dbo', \
                 @table_name = 'other_table';"
            ),
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 1);
    assert_eq!(text_of(&sink.values[0][0]), "qualified_other");
}

#[test]
fn a_qualified_call_accepts_a_table_qualifier_naming_the_same_database() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_batch(
            "EXEC qualified_other.sys.sp_tables @table_qualifier = 'qualified_other', \
             @table_owner = 'dbo', @table_name = 'other_table';",
            &mut sink,
        )
        .expect("batch completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 1);
    assert_eq!(text_of(&sink.values[0][2]), "other_table");
}

/// An RPC naming `procedure` and passing `@table_owner` and `@table_name`.
fn catalog_rpc(procedure: &str, owner: &str, table: &str) -> Rpc {
    let param = |name: &str, value: &str| RpcParam {
        name: name.to_owned(),
        output: false,
        default: false,
        ty: nvarchar_ty(),
        value: nvarchar(value),
    };
    Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name(procedure.to_owned()),
        options: 0,
        params: vec![param("@table_owner", owner), param("@table_name", table)],
        transaction_descriptor: 0,
    }
}

#[test]
fn a_three_part_rpc_name_reads_the_named_database() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_rpc(
            &catalog_rpc("qualified_other.sys.sp_tables", "dbo", "other_table"),
            &mut sink,
        )
        .expect("rpc completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 1);
    assert_eq!(text_of(&sink.values[0][0]), "qualified_other");
    assert_eq!(text_of(&sink.values[0][2]), "other_table");
}

#[test]
fn a_bracketed_three_part_rpc_name_reads_the_named_database() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_rpc(
            &catalog_rpc("[qualified_other].[sys].[sp_tables]", "dbo", "other_table"),
            &mut sink,
        )
        .expect("rpc completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 1);
    assert_eq!(text_of(&sink.values[0][0]), "qualified_other");
}

#[test]
fn a_two_part_rpc_name_reads_the_caller_database() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_rpc(
            &catalog_rpc("sys.sp_tables", "dbo", "home_table"),
            &mut sink,
        )
        .expect("rpc completes");
    assert!(sink.errors.is_empty(), "errors: {:?}", sink.errors);
    assert_eq!(sink.rows, 1);
    assert_eq!(text_of(&sink.values[0][0]), "qualified_home");
}

#[test]
fn an_rpc_qualified_by_a_missing_database_fails_before_the_body_runs() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_rpc(
            &catalog_rpc("qualified_absent.sys.sp_tables", "dbo", "other_table"),
            &mut sink,
        )
        .expect("rpc completes");
    assert_eq!(sink.errors.len(), 1);
    let err = &sink.errors[0];
    assert_eq!((err.number, err.severity, err.state), (911, 16, 4));
    assert_eq!(sink.columns, 0);
    assert_eq!(sink.rows, 0);
}

#[test]
fn a_four_part_rpc_name_on_an_unknown_server_fails_before_the_body_runs() {
    let mut session = two_database_session();
    let mut sink = Recording::default();
    session
        .run_rpc(
            &catalog_rpc(
                "elsewhere.qualified_other.sys.sp_tables",
                "dbo",
                "other_table",
            ),
            &mut sink,
        )
        .expect("rpc completes");
    assert_eq!(sink.errors.len(), 1);
    let err = &sink.errors[0];
    assert_eq!((err.number, err.severity, err.state), (7202, 11, 2));
    assert_eq!(sink.columns, 0);
    assert_eq!(sink.rows, 0);
}
