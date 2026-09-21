//! Execute a resolved system-procedure [`ProcAction`] through the session.

use crate::rpc::{DescribeKind, ProcAction, ProcArg, ProcParam, StaticResultColumn};
use vauban_errors::{BatchErrorScope, SqlError, SqlResult};
use vauban_executor::EvaluatedExecArg;
use vauban_tds::{ColumnFlags, ColumnMeta, RpcParam};
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::batch::{Flow, Session};
use crate::eval_context::apply_exec_session;
use crate::nested::{NestedOutcome, NestedParam, nested_done_proc_rowcount};
use crate::sink::ResultSink;

/// How a procedure call closes on the wire.
pub(crate) enum ProcFinish<'a> {
    /// RPC request: RETURNVALUE tokens, RETURNSTATUS, then DONEPROC.
    Rpc {
        /// Original RPC parameters, in wire order.
        rpc_params: &'a [RpcParam],
    },
    /// `EXEC` inside a SQL batch: inner DONEINPROC tokens, then one batch DONE.
    Batch {
        /// Whether another batch statement follows.
        more: bool,
    },
}

/// What a batch `EXEC` leaves in session state after the call.
pub(crate) struct BatchProcOutcome {
    /// Final values of nested `OUTPUT` parameters.
    pub outputs: Vec<(String, Value)>,
    /// `@@ROWCOUNT` after the call.
    pub rowcount: i64,
    /// `RETURN` status of the nested text, when applicable.
    pub return_status: i32,
    /// `true` when the call failed before finishing cleanly.
    pub failed: bool,
    /// Client error number, `0` on success.
    pub error_number: u32,
    /// Whether the batch may continue after this statement.
    pub continues: bool,
}

/// Runs `action` for an RPC or a batch `EXEC`.
pub(crate) fn execute_proc_action(
    session: &mut Session,
    action: ProcAction,
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    match action {
        ProcAction::ExecuteSql { statement, params } => {
            execute_nested_sql(session, &statement, &params, finish, sink)
        }
        ProcAction::Template { sql, params } => {
            execute_nested_sql(session, &sql, &params, finish, sink)
        }
        ProcAction::Static { columns, rows } => execute_static(&columns, &rows, finish, sink),
        ProcAction::Describe {
            statement,
            kind,
            columns,
        } => execute_describe(session, &statement, kind, &columns, finish, sink),
        ProcAction::Refuse(err) => fail_proc(session, err, finish, sink),
        ProcAction::Prepare { .. } => fail_proc(
            session,
            SqlError::procedure_not_found("sp_prepare"),
            finish,
            sink,
        ),
        ProcAction::Execute { .. } => fail_proc(
            session,
            SqlError::procedure_not_found("sp_execute"),
            finish,
            sink,
        ),
        ProcAction::Unprepare { .. } => fail_proc(
            session,
            SqlError::procedure_not_found("sp_unprepare"),
            finish,
            sink,
        ),
    }
}

/// Resolves and runs a batch `EXEC` of a stored procedure.
pub(crate) fn run_batch_procedure(
    session: &mut Session,
    name: &str,
    exec_args: &[EvaluatedExecArg],
    return_into: Option<&str>,
    line: u32,
    more: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<(Flow, BatchProcOutcome)> {
    let stored = exec_args_to_proc_args(exec_args);
    let proc_args: Vec<ProcArg<'_>> = stored
        .iter()
        .map(|arg| ProcArg {
            name: arg.name.as_deref(),
            ty: &arg.ty,
            value: &arg.value,
            output: arg.output,
            default: arg.default,
        })
        .collect();
    let Some(resolver) = super::rpc::system_procedure_resolver() else {
        let err = SqlError::procedure_not_found(name).with_line(line);
        return finish_batch_exec_error(session, err, more, sink);
    };
    match resolver(name, &proc_args) {
        None => {
            let err = SqlError::procedure_not_found(name).with_line(line);
            finish_batch_exec_error(session, err, more, sink)
        }
        Some(Err(err)) => finish_batch_exec_error(session, at_line(err, line), more, sink),
        Some(Ok(action)) => {
            let outcome = execute_proc_action(session, action, ProcFinish::Batch { more }, sink)?;
            apply_batch_outputs(session, exec_args, return_into, &outcome);
            Ok((flow_from_outcome(&outcome), outcome))
        }
    }
}

/// Runs `EXEC('…')` dynamic SQL inside a batch.
pub(crate) fn run_batch_dynamic(
    session: &mut Session,
    text: &str,
    line: u32,
    more: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<(Flow, BatchProcOutcome)> {
    let _ = line;
    let mut wrapper = BatchExecSink::new(sink, more, session.state().options.xact_abort);
    let nested = session.run_nested(text, &[], &mut wrapper)?;
    let outcome = BatchProcOutcome {
        outputs: nested.outputs.clone(),
        rowcount: nested.rowcount,
        return_status: nested.return_status,
        failed: nested.failed,
        error_number: wrapper.last_error(),
        continues: continues_after_nested(
            more,
            session.state().options.xact_abort,
            wrapper.last_error(),
        ),
    };
    if outcome.failed {
        session.state_mut().last_error = outcome.error_number;
    } else {
        session.state_mut().last_error = 0;
    }
    session.state_mut().rowcount = outcome.rowcount;
    let exec = session.exec().clone();
    apply_exec_session(session.state_mut(), &exec, true, !outcome.failed);
    Ok((flow_from_outcome(&outcome), outcome))
}

fn execute_nested_sql(
    session: &mut Session,
    statement: &str,
    params: &[ProcParam],
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    let nested_params = proc_params_to_nested(params);
    let mut body_sink = match finish {
        ProcFinish::Rpc { .. } => BodySink::Rpc(RpcBodySink::new(sink)),
        ProcFinish::Batch { more } => BodySink::Batch(BatchExecSink::new(
            sink,
            more,
            session.state().options.xact_abort,
        )),
    };
    let nested = session.run_nested(statement, &nested_params, &mut body_sink)?;
    let last_error = body_sink.last_error();
    if let ProcFinish::Rpc { rpc_params } = finish {
        if !nested.failed {
            emit_rpc_return_values(sink, rpc_params, &nested)?;
            sink.return_status(nested.return_status)?;
        }
        sink.done_proc(nested_done_proc_rowcount(
            nested.failed,
            nested.had_result_set,
            nested.rowcount,
        ))?;
    }
    let continues = match finish {
        ProcFinish::Batch { more } => {
            continues_after_nested(more, session.state().options.xact_abort, last_error)
        }
        ProcFinish::Rpc { .. } => false,
    };
    if nested.failed {
        session.state_mut().last_error = last_error;
    } else {
        session.state_mut().last_error = 0;
    }
    Ok(BatchProcOutcome {
        outputs: nested.outputs.clone(),
        rowcount: nested.rowcount,
        return_status: nested.return_status,
        failed: nested.failed,
        error_number: last_error,
        continues,
    })
}

/// Describes a batch without running it, and answers one row per described item.
///
/// A batch the binder refuses answers that binding error instead of a result set
/// (`tests/describe.rs`, `describe_of_an_unknown_table_fails`).
fn execute_describe(
    session: &mut Session,
    statement: &str,
    kind: DescribeKind,
    columns: &[StaticResultColumn],
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    match crate::prepared::describe_batch(session, statement, kind) {
        Ok(rows) => execute_static(columns, &rows, finish, sink),
        Err(err) => fail_proc(session, err, finish, sink),
    }
}

fn execute_static(
    columns: &[StaticResultColumn],
    rows: &[Vec<Value>],
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    let meta: Vec<ColumnMeta> = columns
        .iter()
        .map(|column| ColumnMeta {
            flags: ColumnFlags {
                nullable: column.ty.nullable,
                ..ColumnFlags::default()
            },
            name: column.name.clone(),
            ty: column.ty.clone(),
        })
        .collect();
    sink.columns(&meta)?;
    for row in rows {
        sink.row(row)?;
    }
    let rowcount = rows.len() as u64;
    match finish {
        ProcFinish::Rpc { .. } => {
            sink.return_status(0)?;
            sink.done_proc(Some(rowcount))?;
        }
        ProcFinish::Batch { more } => {
            sink.done(Some(rowcount), more)?;
        }
    }
    Ok(BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: rowcount as i64,
        return_status: 0,
        failed: false,
        error_number: 0,
        continues: true,
    })
}

fn fail_proc(
    session: &mut Session,
    err: SqlError,
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    session.state_mut().last_error = err.number;
    sink.error(&err)?;
    let continues = match finish {
        ProcFinish::Batch { more } => batch_continues(session, more, true, err.number),
        ProcFinish::Rpc { .. } => false,
    };
    match finish {
        ProcFinish::Rpc { .. } => sink.done_proc(None)?,
        ProcFinish::Batch { more: _ } => sink.done(None, continues)?,
    }
    Ok(BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: 0,
        return_status: 0,
        failed: true,
        error_number: err.number,
        continues,
    })
}

fn finish_batch_exec_error(
    session: &mut Session,
    err: SqlError,
    more: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<(Flow, BatchProcOutcome)> {
    session.state_mut().last_error = err.number;
    sink.error(&err)?;
    let continues = batch_continues(session, more, true, err.number);
    sink.done(None, continues)?;
    let outcome = BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: 0,
        return_status: 0,
        failed: true,
        error_number: err.number,
        continues,
    };
    Ok((flow_from_outcome(&outcome), outcome))
}

fn flow_from_outcome(outcome: &BatchProcOutcome) -> Flow {
    if outcome.continues {
        Flow::Continue
    } else {
        Flow::Stop
    }
}

fn batch_continues(session: &Session, more: bool, failed: bool, error_number: u32) -> bool {
    if !failed {
        return more;
    }
    more && !session.state().options.xact_abort
        && error_number != 0
        && SqlError::new(error_number, 16, 1, "x").batch_scope() == BatchErrorScope::Statement
}

/// A statement of a nested text that fails to compile stops the whole text before any of it
/// runs: on the shapes checked, none of these numbers stops the caller
/// (`tests/nested.rs`, `a_208_in_an_exec_lets_the_caller_continue` and its siblings for the
/// other numbers of this list).
const NESTED_TEXT_COMPILATION_ERRORS: [u32; 7] = [102, 137, 206, 207, 208, 2812, 4104];

/// Whether the batch that issued `EXEC('…')` or `sp_executesql` runs its next statement once
/// the nested text this call ran is done.
///
/// `XACT_ABORT ON` on the caller stops it after any failure. Otherwise, an error the nested
/// text fails to *compile* on (`NESTED_TEXT_COMPILATION_ERRORS`) does not stop the caller: no
/// statement of the text ran, so nothing there needed the caller to stop either. An error
/// raised while a statement of the nested text *runs* stops the caller exactly when its
/// catalogued scope (`vauban_errors::SqlError::batch_scope`) already stops the batch that
/// raised it — `BatchErrorScope::Batch` — and lets the caller go on when it is
/// `BatchErrorScope::Statement` (`tests/nested.rs`,
/// `nested_batch_scope_family_decides_caller_continuation`,
/// `a_515_in_an_exec_lets_the_caller_continue` and its siblings for 547, 2627 and 3701).
pub(crate) fn continues_after_nested(more: bool, xact_abort: bool, error_number: u32) -> bool {
    if error_number == 0 {
        return more;
    }
    if xact_abort {
        return false;
    }
    if NESTED_TEXT_COMPILATION_ERRORS.contains(&error_number) {
        return more;
    }
    more && SqlError::new(error_number, 16, 1, "x").batch_scope() == BatchErrorScope::Statement
}

fn apply_batch_outputs(
    session: &mut Session,
    exec_args: &[EvaluatedExecArg],
    return_into: Option<&str>,
    outcome: &BatchProcOutcome,
) {
    session.state_mut().rowcount = outcome.rowcount;
    session.state_mut().last_error = outcome.error_number;
    if let Some(var) = return_into {
        session
            .exec_mut()
            .variables
            .insert(var.to_owned(), Value::I32(outcome.return_status));
    }
    for arg in exec_args {
        if !arg.output {
            continue;
        }
        let Some(var) = arg.output_variable.as_ref() else {
            continue;
        };
        let key = arg.name.as_deref().unwrap_or(var);
        let value = outcome
            .outputs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.clone())
            .or_else(|| session.exec().variables.get(key).cloned());
        if let Some(value) = value {
            session.exec_mut().variables.insert(var.clone(), value);
        }
    }
    let exec = session.exec().clone();
    apply_exec_session(session.state_mut(), &exec, true, !outcome.failed);
}

fn emit_rpc_return_values(
    sink: &mut dyn ResultSink,
    rpc_params: &[RpcParam],
    nested: &NestedOutcome,
) -> SqlResult<()> {
    for param in rpc_params {
        if !param.output {
            continue;
        }
        if param.name.is_empty() {
            continue;
        }
        let Some((_, value)) = nested
            .outputs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&param.name))
        else {
            continue;
        };
        sink.return_value(&param.name, &param.ty, value)?;
    }
    Ok(())
}

fn proc_params_to_nested(params: &[ProcParam]) -> Vec<NestedParam> {
    params
        .iter()
        .map(|param| NestedParam {
            name: param.name.clone(),
            ty: param.ty.clone(),
            value: param.value.clone(),
            output: param.output,
        })
        .collect()
}

struct StoredProcArg {
    name: Option<String>,
    ty: TypeInfo,
    value: Value,
    output: bool,
    default: bool,
}

fn exec_args_to_proc_args(args: &[EvaluatedExecArg]) -> Vec<StoredProcArg> {
    args.iter()
        .map(|arg| {
            let (ty, value, default) = match &arg.value {
                None => (TypeInfo::new(SqlType::Int, true), Value::Null, true),
                Some((value, ty)) => (coerce_unicode_literal_ty(ty, value), value.clone(), false),
            };
            StoredProcArg {
                name: arg.name.clone(),
                ty,
                value,
                output: arg.output,
                default,
            }
        })
        .collect()
}

/// The executor types string literals as `varchar`; system procedures expect `nvarchar`.
fn coerce_unicode_literal_ty(ty: &TypeInfo, value: &Value) -> TypeInfo {
    if matches!(value, Value::String(_)) && matches!(ty.ty, SqlType::VarChar(_)) {
        TypeInfo::new(SqlType::NVarChar(Len::Max), ty.nullable)
    } else {
        ty.clone()
    }
}

fn at_line(err: SqlError, line: u32) -> SqlError {
    if line == 0 { err } else { err.with_line(line) }
}

enum BodySink<'a> {
    Rpc(RpcBodySink<'a>),
    Batch(BatchExecSink<'a>),
}

impl BodySink<'_> {
    fn last_error(&self) -> u32 {
        match self {
            Self::Rpc(inner) => inner.last_error(),
            Self::Batch(inner) => inner.last_error(),
        }
    }
}

impl ResultSink for BodySink<'_> {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.columns(cols),
            Self::Batch(inner) => inner.columns(cols),
        }
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.row(row),
            Self::Batch(inner) => inner.row(row),
        }
    }

    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.done(rowcount, more),
            Self::Batch(inner) => inner.done(rowcount, more),
        }
    }

    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.done_in_proc(rowcount, more),
            Self::Batch(inner) => inner.done_in_proc(rowcount, more),
        }
    }

    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.done_proc(rowcount),
            Self::Batch(inner) => inner.done_proc(rowcount),
        }
    }

    fn info(&mut self, msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.info(msg),
            Self::Batch(inner) => inner.info(msg),
        }
    }

    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.error(err),
            Self::Batch(inner) => inner.error(err),
        }
    }

    fn env_change(&mut self, change: &vauban_tds::EnvChange) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.env_change(change),
            Self::Batch(inner) => inner.env_change(change),
        }
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.return_value(name, ty, value),
            Self::Batch(inner) => inner.return_value(name, ty, value),
        }
    }

    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        match self {
            Self::Rpc(inner) => inner.return_status(status),
            Self::Batch(inner) => inner.return_status(status),
        }
    }
}

/// Forwards nested tokens and records the error number of the nested statement most
/// recently closed, `0` once one finishes without one: the same rule `@@ERROR` follows
/// between two statements of a batch (`eval_context.rs`, `apply_exec_session`), carried
/// here for the statement the caller sees once the whole call returns.
struct TrackedSink<'a> {
    inner: &'a mut dyn ResultSink,
    last_error: u32,
    /// `true` from an `error()` call until the `done_in_proc`/`done_proc` that closes the
    /// statement it belongs to; tells that closing call whether to keep `last_error` or
    /// zero it.
    pending_error: bool,
}

impl<'a> TrackedSink<'a> {
    fn new(inner: &'a mut dyn ResultSink) -> Self {
        Self {
            inner,
            last_error: 0,
            pending_error: false,
        }
    }

    /// Closes the statement `error()` last opened: keeps `last_error` when it just failed,
    /// zeroes it when it ran to completion instead (`tests/nested.rs`,
    /// `an_8134_in_an_exec_lets_the_caller_continue_and_error_is_cleared_after` for the
    /// success case, `a_2812_in_an_exec_lets_the_caller_continue` for a failure that still
    /// closes a statement this way).
    fn close_statement(&mut self) {
        if !self.pending_error {
            self.last_error = 0;
        }
        self.pending_error = false;
    }
}

impl ResultSink for TrackedSink<'_> {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.inner.columns(cols)
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.inner.row(row)
    }

    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.inner.done(rowcount, more)
    }

    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.close_statement();
        self.inner.done_in_proc(rowcount, more)
    }

    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.inner.done_proc(rowcount)
    }

    fn info(&mut self, msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
        self.inner.info(msg)
    }

    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.last_error = err.number;
        self.pending_error = true;
        self.inner.error(err)
    }

    fn env_change(&mut self, change: &vauban_tds::EnvChange) -> SqlResult<()> {
        self.inner.env_change(change)
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.inner.return_value(name, ty, value)
    }

    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.inner.return_status(status)
    }
}

/// Swallows the nested procedure trailer for an RPC.
struct RpcBodySink<'a> {
    tracked: TrackedSink<'a>,
}

impl<'a> RpcBodySink<'a> {
    fn new(inner: &'a mut dyn ResultSink) -> Self {
        Self {
            tracked: TrackedSink::new(inner),
        }
    }
}

impl ResultSink for RpcBodySink<'_> {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.tracked.columns(cols)
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.tracked.row(row)
    }

    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.tracked.done(rowcount, more)
    }

    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.tracked.done_in_proc(rowcount, more)
    }

    fn done_proc(&mut self, _rowcount: Option<u64>) -> SqlResult<()> {
        Ok(())
    }

    fn info(&mut self, msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
        self.tracked.info(msg)
    }

    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.tracked.error(err)
    }

    fn env_change(&mut self, change: &vauban_tds::EnvChange) -> SqlResult<()> {
        self.tracked.env_change(change)
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.tracked.return_value(name, ty, value)
    }

    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

impl RpcBodySink<'_> {
    fn last_error(&self) -> u32 {
        self.tracked.last_error
    }
}

/// Swallows the nested procedure trailer and emits one batch DONE instead.
struct BatchExecSink<'a> {
    tracked: TrackedSink<'a>,
    more: bool,
    xact_abort: bool,
}

impl<'a> BatchExecSink<'a> {
    fn new(inner: &'a mut dyn ResultSink, more: bool, xact_abort: bool) -> Self {
        Self {
            tracked: TrackedSink::new(inner),
            more,
            xact_abort,
        }
    }
}

impl ResultSink for BatchExecSink<'_> {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.tracked.columns(cols)
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.tracked.row(row)
    }

    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.tracked.done(rowcount, more)
    }

    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.tracked.done_in_proc(rowcount, more)
    }

    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        let more = continues_after_nested(self.more, self.xact_abort, self.tracked.last_error);
        self.tracked.done(rowcount, more)
    }

    fn info(&mut self, msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
        self.tracked.info(msg)
    }

    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.tracked.error(err)
    }

    fn env_change(&mut self, change: &vauban_tds::EnvChange) -> SqlResult<()> {
        self.tracked.env_change(change)
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.tracked.return_value(name, ty, value)
    }

    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

impl BatchExecSink<'_> {
    fn last_error(&self) -> u32 {
        self.tracked.last_error
    }
}
