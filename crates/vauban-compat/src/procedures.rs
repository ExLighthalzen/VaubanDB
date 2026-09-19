//! Resolving a system procedure name and its arguments into a session action.

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::ParameterDeclaration;
use vauban_types::{TypeInfo, Value};

use crate::sp_datatype_info::{ResultColumn, sp_datatype_info_100_static};
use crate::special_procs;

pub use special_procs::ProcArg;

/// One bound parameter of dynamic SQL, ready for the session to execute.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcParam {
    /// Parameter name, `@` included.
    pub name: String,
    /// Declared type.
    pub ty: TypeInfo,
    /// Bound value.
    pub value: Value,
    /// True when the declaration or the call marked the parameter `OUTPUT`.
    pub output: bool,
}

/// What the session should do after a system procedure has been resolved.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcAction {
    /// `sp_executesql`: run a statement with bound parameters.
    ExecuteSql {
        /// Dynamic SQL text.
        statement: String,
        /// Values for the parameters declared in `@params`.
        params: Vec<ProcParam>,
    },
    /// `sp_prepare` or `sp_prepexec`: prepare a handle, optionally executing once.
    Prepare {
        /// Statement text to prepare.
        statement: String,
        /// Declarations read from the `@params` argument.
        params: Vec<ParameterDeclaration>,
        /// Index of the output handle argument in the original call.
        handle_arg: usize,
        /// Values supplied by `sp_prepexec` after the fixed arguments.
        execute_with: Option<Vec<ProcParam>>,
    },
    /// `sp_execute`: run a prepared handle with new parameter values.
    Execute {
        /// Prepared-statement handle.
        handle: i32,
        /// Bound parameter values.
        params: Vec<ProcParam>,
    },
    /// `sp_unprepare`: drop a prepared handle.
    Unprepare {
        /// Prepared-statement handle.
        handle: i32,
    },
    /// Catalog procedure template (T-SQL batch on the current database).
    Template {
        /// T-SQL batch to run on the current database.
        sql: String,
        /// Named arguments of the catalog call.
        params: Vec<ProcParam>,
    },
    /// Static result set, such as `sp_datatype_info_100`.
    Static {
        /// Result metadata.
        columns: Vec<ResultColumn>,
        /// Result rows.
        rows: Vec<Vec<Value>>,
    },
    /// Known procedure deliberately refused (for example cursor RPCs).
    Refuse(SqlError),
}

/// Catalog procedure entry placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SystemProc {
    /// Procedure name without qualification.
    pub name: &'static str,
}

const CURSOR_PROCS: &[&str] = &[
    "sp_cursoropen",
    "sp_cursorfetch",
    "sp_cursorclose",
    "sp_cursor",
    "sp_cursorprepare",
    "sp_cursorexecute",
    "sp_cursorprepexec",
    "sp_cursorunprepare",
    "sp_cursoroption",
    "sp_prepexecrpc",
];

/// Normalises a procedure name: strips brackets, keeps the last name part, lowercases.
pub fn normalize_procedure_name(name: &str) -> String {
    let stripped = name.replace(['[', ']'], "");
    stripped
        .rsplit('.')
        .next()
        .unwrap_or(stripped.as_str())
        .to_ascii_lowercase()
}

/// Resolves a system procedure call into an action, an argument error, or `None` when the
/// name is unknown (the session answers 2812).
pub fn resolve_system_procedure(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    let normalized = normalize_procedure_name(name);
    if CURSOR_PROCS.contains(&normalized.as_str()) {
        return Some(Ok(ProcAction::Refuse(SqlError::procedure_not_found(name))));
    }

    match normalized.as_str() {
        "sp_executesql" => Some(special_procs::resolve_sp_executesql(args)),
        "sp_prepare" => Some(special_procs::resolve_sp_prepare(args, false)),
        "sp_prepexec" => Some(special_procs::resolve_sp_prepare(args, true)),
        "sp_execute" => Some(special_procs::resolve_sp_execute(args)),
        "sp_unprepare" => Some(special_procs::resolve_sp_unprepare(args)),
        "sp_datatype_info_100" => Some(resolve_sp_datatype_info_100(args)),
        name => resolve_catalog_procedure(name, args),
    }
}

fn resolve_sp_datatype_info_100(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let integer = |value: &Value| match value {
        Value::I16(value) => Some(i32::from(*value)),
        Value::I32(value) => Some(*value),
        Value::I64(value) => i32::try_from(*value).ok(),
        _ => None,
    };
    let data_type = args
        .first()
        .and_then(|arg| integer(arg.value))
        .ok_or_else(|| {
            SqlError::procedure_expects_parameter("sp_datatype_info_100", "@data_type")
        })?;
    let odbc_ver = args.get(1).and_then(|arg| integer(arg.value)).unwrap_or(4);
    let (columns, rows) = sp_datatype_info_100_static(data_type, odbc_ver);
    Ok(ProcAction::Static { columns, rows })
}

fn resolve_catalog_procedure(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    if let Some(result) = crate::catalog_procs::resolve(name, args) {
        return Some(result);
    }
    if let Some(result) = crate::catalog_keys::resolve(name, args) {
        return Some(result);
    }
    let stub_tables: &[&[SystemProc]] = &[
        crate::help_procs::PROCS,
        crate::who_procs::PROCS,
    ];
    for table in stub_tables {
        if table.iter().any(|proc| proc.name == name) {
            return Some(Err(SqlError::procedure_not_found(name)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::{Len, SqlString, SqlType};

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

    fn arg<'a>(
        name: Option<&'a str>,
        ty: &'a TypeInfo,
        value: &'a Value,
        output: bool,
    ) -> ProcArg<'a> {
        ProcArg {
            name,
            ty,
            value,
            output,
            default: false,
        }
    }

    #[test]
    fn unknown_name_returns_none() {
        assert!(resolve_system_procedure("sp_no_such", &[]).is_none());
    }

    #[test]
    fn sp_cursoropen_is_refused_with_2812() {
        let action = resolve_system_procedure("sp_cursoropen", &[])
            .expect("known")
            .expect("action");
        match action {
            ProcAction::Refuse(err) => assert_eq!(err.number, 2812),
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn sp_executesql_positional_and_named_forms() {
        let stmt = nvarchar("SELECT @a");
        let params = nvarchar("@a int");
        let value = int(1);
        let nvarchar_type = nvarchar_ty();
        let int_type = int_ty();
        let positional = [
            arg(None, &nvarchar_type, &stmt, false),
            arg(None, &nvarchar_type, &params, false),
            arg(Some("@a"), &int_type, &value, false),
        ];
        let action = resolve_system_procedure("sp_executesql", &positional)
            .unwrap()
            .unwrap();
        match action {
            ProcAction::ExecuteSql { statement, params } => {
                assert_eq!(statement, "SELECT @a");
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "@a");
                assert_eq!(params[0].value, int(1));
            }
            other => panic!("expected ExecuteSql, got {other:?}"),
        }

        let named = [
            arg(Some("@stmt"), &nvarchar_type, &stmt, false),
            arg(Some("@params"), &nvarchar_type, &params, false),
            arg(Some("@a"), &int_type, &value, false),
        ];
        let action = resolve_system_procedure("sp_executesql", &named)
            .unwrap()
            .unwrap();
        assert!(matches!(action, ProcAction::ExecuteSql { .. }));
    }

    #[test]
    fn sp_prepexec_binds_handle_and_execute_values() {
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
            .unwrap()
            .unwrap();
        match action {
            ProcAction::Prepare {
                statement,
                handle_arg,
                execute_with,
                ..
            } => {
                assert_eq!(statement, "SELECT @P1");
                assert_eq!(handle_arg, 0);
                let values = execute_with.expect("prepexec values");
                assert_eq!(values.len(), 1);
                assert_eq!(values[0].name, "@P1");
                assert_eq!(values[0].value, int(5));
            }
            other => panic!("expected Prepare, got {other:?}"),
        }
    }

    #[test]
    fn sp_executesql_errors_match_err_022() {
        let stmt = nvarchar("SELECT 1");
        let params = nvarchar("@a int");
        let value = int(1);
        let nvarchar_type = nvarchar_ty();
        let varchar_type = varchar_ty();
        let int_type = int_ty();

        let err = resolve_system_procedure("sp_executesql", &[])
            .unwrap()
            .unwrap_err();
        assert_eq!(err.number, 201);

        let varchar_stmt = nvarchar("SELECT 1");
        let err = resolve_system_procedure(
            "sp_executesql",
            &[
                arg(None, &varchar_type, &varchar_stmt, false),
                arg(None, &nvarchar_type, &nvarchar(""), false),
            ],
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(err.number, 214);

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
        .unwrap()
        .unwrap_err();
        assert_eq!(err.number, 8144);
    }
}
