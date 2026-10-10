//! RPC dispatch: decode parameters, resolve a system procedure, execute its action.

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::ParameterDeclaration;
use vauban_tds::{Rpc, RpcProc};
use vauban_types::{TypeInfo, Value};

use crate::batch::Session;
use crate::prepared::execute_prepared_action;
use crate::procedures::{
    ProcFinish, coerce_unicode_literal_ty, execute_proc_action, resolve_call_database,
};
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticResultColumn {
    /// Column name sent in COLMETADATA.
    pub name: String,
    /// SQL type, nullability and collation sent in COLMETADATA.
    pub ty: TypeInfo,
}

/// Which of the two descriptions a [`ProcAction::Describe`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescribeKind {
    /// Columns of the first result set of the batch.
    FirstResultSet,
    /// Parameters the batch uses without declaring them.
    UndeclaredParameters,
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
    /// A call that asks for the shape of a T-SQL batch instead of running it.
    ///
    /// The batch is described by the same parse and bind the session runs before it prepares
    /// a statement; the caller receives the fixed result set named by `columns`, one row per
    /// described item.
    Describe {
        /// The batch to describe.
        statement: String,
        /// Which description the caller asked for.
        kind: DescribeKind,
        /// Result metadata of the description.
        columns: Vec<StaticResultColumn>,
    },
    /// Known procedure deliberately refused (for example cursor RPCs).
    Refuse(SqlError),
}

/// Resolver of system procedures: maps a procedure name and its arguments to the action the
/// session runs, or `None` when the name is not a system procedure (error 2812).
///
/// `vauban-compat` provides one (`resolve_system_procedure`); the caller that builds the
/// [`crate::Engine`] hands it over with [`crate::Engine::with_system_procedures`], which keeps
/// `session` free of a dependency on `compat`.
pub type SystemProcedureResolver =
    dyn Fn(&str, &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>> + Send + Sync;

/// The resolver of the engine `session` runs on, `None` when the engine was built without one.
pub(crate) fn system_procedure_resolver(session: &Session) -> Option<&SystemProcedureResolver> {
    session.engine().system_procedures()
}

impl Session {
    /// Runs an RPC ([MS-TDS] 2.2.6.6). Same contract as [`super::batch::Session::run_batch`].
    ///
    /// The resolver of the engine serves known system procedures. An unresolved RPC name is
    /// answered with error 2812 then a DONEPROC carrying `DoneStatus::ERROR`. A `ProcID` is named
    /// after the special procedure it stands for ([MS-TDS] 2.2.6.6, e.g. 10 →
    /// `sp_executesql`).
    ///
    /// A name qualified by a database runs the procedure body on that database
    /// (`compat/tests/catalog_procs.rs`, `a_three_part_rpc_name_reads_the_named_database`),
    /// and the qualifier is read before the procedure name is resolved.
    pub fn run_rpc(&mut self, rpc: &Rpc, sink: &mut dyn ResultSink) -> SqlResult<()> {
        let name = rpc_procedure_name(rpc);
        let qualifier = split_procedure_name(&name);
        if let Some(server) = &qualifier.server
            && !server.eq_ignore_ascii_case(&self.state().server_name)
        {
            let err = SqlError::linked_server_not_found(server);
            return fail_rpc_error(self, &err, sink);
        }
        let database = match resolve_call_database(self, qualifier.database.as_deref()) {
            Ok(database) => database,
            Err(err) => return fail_rpc_error(self, &err, sink),
        };
        let stored = rpc_params_storage(&name, &rpc.params);
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
        let Some(resolver) = system_procedure_resolver(self) else {
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
                    _ => execute_proc_action(self, action, database.as_deref(), finish, sink)?,
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

/// The parts written left of a procedure name in an RPC name.
struct QualifiedProcedureName {
    /// Server part, the leftmost of four, `None` when the name has fewer parts.
    server: Option<String>,
    /// Database part, `None` when the name has fewer than three parts.
    database: Option<String>,
}

/// Reads the qualification of an RPC procedure name.
///
/// The wire carries the name the client wrote, in one to four parts separated by dots, a
/// part optionally bracketed: `sp_tables`, `sys.sp_tables`, `d.sys.sp_tables`,
/// `srv.d.sys.sp_tables`. The object part is the rightmost, the schema the one before it,
/// then the database, then the server (`compat/tests/catalog_procs.rs`,
/// `a_three_part_rpc_name_reads_the_named_database`,
/// `a_bracketed_three_part_rpc_name_reads_the_named_database` and
/// `a_four_part_rpc_name_on_an_unknown_server_fails_before_the_body_runs`). A name of more
/// than four parts keeps its four rightmost parts.
fn split_procedure_name(name: &str) -> QualifiedProcedureName {
    let parts: Vec<&str> = name.split('.').collect();
    let unbracket = |part: &str| {
        let trimmed = part.trim();
        trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(trimmed)
            .to_owned()
    };
    let from_end = |offset: usize| {
        parts
            .len()
            .checked_sub(offset)
            .map(|index| unbracket(parts[index]))
            .filter(|part| !part.is_empty())
    };
    QualifiedProcedureName {
        server: from_end(4),
        database: from_end(3),
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

fn rpc_params_storage(proc: &str, params: &[vauban_tds::RpcParam]) -> Vec<StoredRpcArg> {
    params
        .iter()
        .map(|param| StoredRpcArg {
            name: rpc_param_name(&param.name).map(str::to_owned),
            ty: coerce_unicode_literal_ty(proc, &param.ty, &param.value),
            value: param.value.clone(),
            output: param.output,
            default: param.default,
        })
        .collect()
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
