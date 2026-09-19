//! RPC dispatch: decode parameters, resolve a system procedure, execute its action.

use std::sync::OnceLock;

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::ParameterDeclaration;
use vauban_tds::{Rpc, RpcProc};
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::batch::Session;
use crate::prepared::execute_prepared_action;
use crate::procedures::{ProcFinish, execute_proc_action};
use crate::sink::ResultSink;

/// One argument of a system procedure call, as the session received it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcArg<'a> {
    /// Parameter name when the call used `@name = value`.
    pub name: Option<&'a str>,
    /// Declared type of the argument.
    pub ty: &'a TypeInfo,
    /// Argument value.
    pub value: &'a Value,
    /// True when the RPC marked the parameter as output.
    pub output: bool,
    /// True when the RPC marked the parameter as default.
    pub default: bool,
}

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

/// One column of a static procedure result set.
#[derive(Debug, Clone, PartialEq)]
pub struct StaticResultColumn {
    /// Column name sent in COLMETADATA.
    pub name: String,
    /// SQL type, nullability and collation sent in COLMETADATA.
    pub ty: TypeInfo,
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
        columns: Vec<StaticResultColumn>,
        /// Result rows.
        rows: Vec<Vec<Value>>,
    },
    /// Known procedure deliberately refused (for example cursor RPCs).
    Refuse(SqlError),
}

/// Function registered by `vauban-compat` at process start-up.
pub type SystemProcedureResolver = fn(&str, &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>>;

static SYSTEM_PROCEDURE_RESOLVER: OnceLock<SystemProcedureResolver> = OnceLock::new();

/// Registers the compatibility-layer resolver without introducing a `session → compat`
/// dependency cycle. Repeated registration is harmless, like the function registry.
pub fn register_system_procedure_resolver(resolver: SystemProcedureResolver) {
    let _ = SYSTEM_PROCEDURE_RESOLVER.set(resolver);
}

pub(crate) fn system_procedure_resolver() -> Option<SystemProcedureResolver> {
    SYSTEM_PROCEDURE_RESOLVER.get().copied()
}

impl Session {
    /// Runs an RPC ([MS-TDS] 2.2.6.6). Same contract as [`super::batch::Session::run_batch`].
    ///
    /// The registered resolver serves known system procedures. An unresolved RPC name is
    /// answered with error 2812 then a DONEPROC carrying `DoneStatus::ERROR`. A `ProcID` is named
    /// after the special procedure it stands for ([MS-TDS] 2.2.6.6, e.g. 10 →
    /// `sp_executesql`).
    pub fn run_rpc(&mut self, rpc: &Rpc, sink: &mut dyn ResultSink) -> SqlResult<()> {
        let name = rpc_procedure_name(rpc);
        let stored = rpc_params_storage(&rpc.params);
        let params: Vec<ProcArg<'_>> = stored
            .iter()
            .map(|arg| ProcArg {
                name: arg.name.as_deref(),
                ty: &arg.ty,
                value: &arg.value,
                output: arg.output,
                default: arg.default,
            })
            .collect();
        let Some(resolver) = SYSTEM_PROCEDURE_RESOLVER.get() else {
            return fail_rpc(&name, sink);
        };
        match resolver(&name, &params) {
            None => fail_rpc(&name, sink),
            Some(Err(err)) => fail_rpc_error(self, &err, sink),
            Some(Ok(action)) => {
                let finish = ProcFinish::Rpc {
                    rpc_params: &rpc.params,
                };
                let outcome = match &action {
                    ProcAction::Prepare { .. }
                    | ProcAction::Execute { .. }
                    | ProcAction::Unprepare { .. } => {
                        execute_prepared_action(self, action, finish, &rpc.params, sink)?
                    }
                    _ => execute_proc_action(self, action, finish, sink)?,
                };
                if outcome.failed {
                    self.state_mut().last_error = outcome.error_number;
                } else {
                    self.state_mut().rowcount = outcome.rowcount;
                    self.state_mut().last_error = 0;
                }
                Ok(())
            }
        }
    }
}

fn rpc_procedure_name(rpc: &Rpc) -> String {
    match &rpc.proc {
        RpcProc::Name(name) => name.clone(),
        RpcProc::Id(id) => rpc
            .proc
            .well_known_name()
            .map_or_else(|| id.to_string(), str::to_owned),
    }
}

struct StoredRpcArg {
    name: Option<String>,
    ty: TypeInfo,
    value: Value,
    output: bool,
    default: bool,
}

fn rpc_params_storage(params: &[vauban_tds::RpcParam]) -> Vec<StoredRpcArg> {
    params
        .iter()
        .map(|param| StoredRpcArg {
            name: rpc_param_name(&param.name).map(str::to_owned),
            ty: coerce_unicode_literal_ty(&param.ty, &param.value),
            value: param.value.clone(),
            output: param.output,
            default: param.default,
        })
        .collect()
}

fn coerce_unicode_literal_ty(ty: &TypeInfo, value: &Value) -> TypeInfo {
    if matches!(value, Value::String(_)) && matches!(ty.ty, SqlType::VarChar(_)) {
        TypeInfo::new(SqlType::NVarChar(Len::Max), ty.nullable)
    } else {
        ty.clone()
    }
}

/// Maps wire parameter names to the names the compatibility resolver expects.
fn rpc_param_name(name: &str) -> Option<&str> {
    if name.is_empty() {
        return None;
    }
    match name {
        "stmt" => Some("@stmt"),
        "params" => Some("@params"),
        other => Some(other),
    }
}

fn fail_rpc(name: &str, sink: &mut dyn ResultSink) -> SqlResult<()> {
    let err = SqlError::procedure_not_found(name);
    sink.error(&err)?;
    sink.done_proc(None)
}

fn fail_rpc_error(
    session: &mut Session,
    err: &SqlError,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    session.state_mut().last_error = err.number;
    sink.error(err)?;
    sink.done_proc(None)
}
