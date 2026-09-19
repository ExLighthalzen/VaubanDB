//! The ODBC type-info procedure answers the rows of SQL Server 2022.

use std::sync::Arc;

use vauban_compat::{
    ProcAction, ProcArg, call_system_procedure, register_functions, resolve_system_procedure,
    sp_datatype_info_100,
};
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange, ResetConnection, Rpc, RpcParam, RpcProc};
use vauban_types::{SqlType, TypeInfo, Value};

const COLUMN_NAMES: [&str; 20] = [
    "TYPE_NAME",
    "DATA_TYPE",
    "PRECISION",
    "LITERAL_PREFIX",
    "LITERAL_SUFFIX",
    "CREATE_PARAMS",
    "NULLABLE",
    "CASE_SENSITIVE",
    "SEARCHABLE",
    "UNSIGNED_ATTRIBUTE",
    "MONEY",
    "AUTO_INCREMENT",
    "LOCAL_TYPE_NAME",
    "MINIMUM_SCALE",
    "MAXIMUM_SCALE",
    "SQL_DATA_TYPE",
    "SQL_DATETIME_SUB",
    "NUM_PREC_RADIX",
    "INTERVAL_PRECISION",
    "USERTYPE",
];

const COLUMN_TYPES: [&str; 20] = [
    "nvarchar", "smallint", "int", "varchar", "varchar", "varchar", "smallint", "smallint",
    "smallint", "smallint", "smallint", "smallint", "nvarchar", "smallint", "smallint", "smallint",
    "smallint", "int", "smallint", "smallint",
];

const COLUMN_NULLABILITY: [bool; 20] = [
    true, false, true, true, true, true, true, true, false, true, false, true, true, true, true,
    false, true, true, true, true,
];

fn render(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::I16(value) => Some(value.to_string()),
        Value::I32(value) => Some(value.to_string()),
        Value::String(value) => Some(value.text.clone()),
        other => panic!("unexpected value: {other:?}"),
    }
}

fn rendered_rows(data_type: i32) -> Vec<Vec<Option<String>>> {
    sp_datatype_info_100(data_type, 4)
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect())
        .collect()
}

#[test]
fn columns_are_the_twenty_odbc_columns_for_each_data_type() {
    for data_type in [12, -9, -3, 93] {
        let result = sp_datatype_info_100(data_type, 4);
        assert_eq!(result.columns.len(), 20);
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            COLUMN_NAMES
        );
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| column.ty.ty.name())
                .collect::<Vec<_>>(),
            COLUMN_TYPES
        );
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| column.ty.nullable)
                .collect::<Vec<_>>(),
            COLUMN_NULLABILITY
        );
    }
}

#[test]
fn rows_of_each_data_type() {
    let s = |value: &str| Some(value.to_owned());
    let n = || None;

    assert_eq!(
        rendered_rows(12),
        vec![vec![
            s("varchar"),
            s("12"),
            s("8000"),
            s("'"),
            s("'"),
            s("max length"),
            s("1"),
            s("0"),
            s("3"),
            n(),
            s("0"),
            n(),
            s("varchar"),
            n(),
            n(),
            s("12"),
            n(),
            n(),
            n(),
            s("2"),
        ]]
    );
    assert_eq!(
        rendered_rows(-9),
        vec![
            vec![
                s("nvarchar"),
                s("-9"),
                s("4000"),
                s("N'"),
                s("'"),
                s("max length"),
                s("1"),
                s("0"),
                s("3"),
                n(),
                s("0"),
                n(),
                s("nvarchar"),
                n(),
                n(),
                s("-9"),
                n(),
                n(),
                n(),
                s("0"),
            ],
            vec![
                s("sysname"),
                s("-9"),
                s("128"),
                s("N'"),
                s("'"),
                n(),
                s("0"),
                s("0"),
                s("3"),
                n(),
                s("0"),
                n(),
                s("sysname"),
                n(),
                n(),
                s("-9"),
                n(),
                n(),
                n(),
                s("18"),
            ],
        ]
    );
    assert_eq!(
        rendered_rows(-3),
        vec![vec![
            s("varbinary"),
            s("-3"),
            s("8000"),
            s("0x"),
            n(),
            s("max length"),
            s("1"),
            s("0"),
            s("2"),
            n(),
            s("0"),
            n(),
            s("varbinary"),
            n(),
            n(),
            s("-3"),
            n(),
            n(),
            n(),
            s("4"),
        ]]
    );
    assert_eq!(
        rendered_rows(93),
        vec![
            vec![
                s("datetime2"),
                s("93"),
                s("27"),
                s("'"),
                s("'"),
                s("scale"),
                s("1"),
                s("0"),
                s("3"),
                n(),
                s("0"),
                n(),
                s("datetime2"),
                s("0"),
                s("7"),
                s("9"),
                s("3"),
                n(),
                n(),
                s("0"),
            ],
            vec![
                s("datetime"),
                s("93"),
                s("23"),
                s("'"),
                s("'"),
                n(),
                s("1"),
                s("0"),
                s("3"),
                n(),
                s("0"),
                n(),
                s("datetime"),
                s("3"),
                s("3"),
                s("9"),
                s("3"),
                n(),
                n(),
                s("12"),
            ],
            vec![
                s("smalldatetime"),
                s("93"),
                s("16"),
                s("'"),
                s("'"),
                n(),
                s("1"),
                s("0"),
                s("3"),
                n(),
                s("0"),
                n(),
                s("smalldatetime"),
                s("0"),
                s("0"),
                s("9"),
                s("3"),
                n(),
                n(),
                s("22"),
            ],
        ]
    );
}

#[test]
fn name_normalisation() {
    let data_type = Value::I32(12);
    let odbc_ver = Value::I32(4);
    let params = [(None, &data_type), (Some("@ODBCVer"), &odbc_ver)];
    for name in [
        "[sys].sp_datatype_info_100",
        "sys.sp_datatype_info_100",
        "SP_DATATYPE_INFO_100",
        "sp_datatype_info_100",
    ] {
        let result = call_system_procedure(name, &params).expect(name);
        assert_eq!(result.result_set.rows.len(), 1, "{name}");
        assert_eq!(result.return_status, 0, "{name}");
    }
}

#[derive(Default)]
struct Recording {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    errors: Vec<SqlError>,
    statuses: Vec<i32>,
    dones: Vec<(Option<u64>, bool)>,
}

impl ResultSink for Recording {
    fn columns(&mut self, columns: &[ColumnMeta]) -> SqlResult<()> {
        self.columns = columns.to_vec();
        Ok(())
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.rows.push(row.to_vec());
        Ok(())
    }

    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.dones.push((rowcount, more));
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

    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.statuses.push(status);
        Ok(())
    }
}

fn session() -> Session {
    register_functions();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(51),
    )
}

fn rpc(name: &str, data_type: i32) -> Rpc {
    Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name(name.to_owned()),
        options: 0,
        params: vec![
            RpcParam {
                name: String::new(),
                output: false,
                default: false,
                ty: TypeInfo::new(SqlType::Int, false),
                value: Value::I32(data_type),
            },
            RpcParam {
                name: "@ODBCVer".to_owned(),
                output: false,
                default: false,
                ty: TypeInfo::new(SqlType::Int, false),
                value: Value::I32(4),
            },
        ],
        transaction_descriptor: 0,
    }
}

#[test]
fn run_rpc_emits_the_rows_the_status_then_doneproc() {
    let mut sink = Recording::default();
    session()
        .run_rpc(&rpc("[sys].sp_datatype_info_100", -9), &mut sink)
        .unwrap();
    assert_eq!(sink.columns.len(), 20);
    assert_eq!(sink.rows.len(), 2);
    assert_eq!(sink.statuses, [0]);
    assert_eq!(sink.dones, [(Some(2), false)]);
    assert!(sink.errors.is_empty());
}

#[test]
fn unknown_procedure_still_gives_2812() {
    let mut sink = Recording::default();
    session()
        .run_rpc(&rpc("sp_no_such_proc", 12), &mut sink)
        .unwrap();
    assert_eq!(sink.errors.len(), 1);
    assert_eq!(sink.errors[0].number, 2812);
    assert_eq!(sink.dones, [(None, false)]);
    assert!(sink.columns.is_empty());
    // `TdsSink::for_rpc` turns this `error` then `done` pair into DONEPROC|ERROR; that
    // token-state rule is covered in `vauban-session::sink`.
}

#[test]
fn resolve_returns_static_rows_for_sp_datatype_info_100() {
    let ty = TypeInfo::new(SqlType::Int, false);
    let data_type = Value::I32(-9);
    let odbc_ver = Value::I32(4);
    let args = [
        ProcArg {
            name: None,
            ty: &ty,
            value: &data_type,
            output: false,
            default: false,
        },
        ProcArg {
            name: Some("@ODBCVer"),
            ty: &ty,
            value: &odbc_ver,
            output: false,
            default: false,
        },
    ];
    let action = resolve_system_procedure("sp_datatype_info_100", &args)
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Static { columns, rows } => {
            assert_eq!(columns.len(), 20);
            assert_eq!(rows.len(), 2);
        }
        other => panic!("expected Static, got {other:?}"),
    }
}
