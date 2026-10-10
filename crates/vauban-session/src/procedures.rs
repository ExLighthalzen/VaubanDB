//! Calls: a system procedure resolved to a [`ProcAction`], a dynamic text (`EXEC('…')`)
//! and the text of `sp_executesql`, each run in a level of its own.
//!
//! # A called level
//!
//! A call that runs T-SQL opens a level ([`run_level`]): its text is parsed, prepared,
//! compiled and run by the program of `batch.rs`, as a batch is, each of its statements
//! closed by a DONEINPROC. The level starts with the `SET` options, the current database
//! and the transaction of its caller, with no variable but the parameters of
//! `sp_executesql`, which are its first variables (`tests/nested.rs`,
//! `caller_variables_are_out_of_scope`, `parameter_is_visible_in_the_text`). When it
//! returns, however it ended, the caller gets its own state back: its variables, the `SET`
//! options the callee changed and its current database
//! (`tests/nested.rs`, `the_options_and_the_database_of_a_callee_end_with_it`, on
//! `QUOTED_IDENTIFIER`, `NOCOUNT` and `USE`).
//!
//! A call written in a program runs inside the level the interpreter of the executor
//! opened for it (`interp/frame.rs`), one level deeper than the statement that made it; a
//! call an RPC makes opens its level here. `sp_executesql` is a procedure that runs its
//! text one level further down: its text runs two levels below the statement that called
//! it, where `EXEC('…')` runs its text one level below (`Body::levels`). The depth
//! therefore grows with each text that calls another (`tests/nested.rs`,
//! `a_dynamic_text_runs_its_own_dynamic_texts_three_deep`), and
//! `ExecSession::max_nest_level` refuses a call that would go past it with 217, which ends
//! the batch and rolls its transaction back (`procedures::tests::a_call_past_the_nesting_limit_is_refused`,
//! `procedures::tests::sp_executesql_runs_its_text_two_levels_down`).
//!
//! How the level ended is the interpreter's answer to the program that made the call
//! (`vauban_executor::ProgramEnd`): an error that stops its level alone lets the caller go
//! on (`tests/nested.rs`, `a_208_in_an_exec_lets_the_caller_continue`), an error that stops
//! the batch, or an error under `SET XACT_ABORT ON`, stops it too (`tests/nested.rs`,
//! `an_error_of_the_batch_two_levels_down_ends_the_response_with_one_done`,
//! `xact_abort_set_by_a_callee_ends_the_batch_on_its_error`).

use crate::rpc::{DescribeKind, ProcAction, ProcArg, ProcParam, StaticResultColumn};
use vauban_errors::{SqlError, SqlResult};
use vauban_executor::{EvaluatedExecArg, ExecSession, ProgramEnd};
use vauban_tds::{ColumnFlags, ColumnMeta, RpcParam};
use vauban_txn::IsolationLevel;
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::batch::{Level, Session, cancelled, error_status};
use crate::eval_context::apply_exec_session;
use crate::prepared::returned_output;
use crate::sink::ResultSink;

/// One parameter of a called level, declared before its text is bound.
#[derive(Debug, Clone, PartialEq)]
pub struct NestedParam {
    /// Variable name, `@` included.
    pub name: String,
    /// Declared type sent in metadata and used for binding.
    pub ty: TypeInfo,
    /// Initial value before the text runs.
    pub value: Value,
    /// When `true`, the final value is returned in [`NestedOutcome::outputs`].
    pub output: bool,
}

/// What a called level leaves behind once its tokens are sent.
#[derive(Debug, Clone, PartialEq)]
pub struct NestedOutcome {
    /// Final values of the `output` parameters, in declaration order.
    pub outputs: Vec<(String, Value)>,
    /// `RETURN` status: the value of an explicit `RETURN`, `0` on a text that ran to its end
    /// without one, and the number of the error that stopped the text otherwise, whether
    /// that error kept the text from compiling at all or stopped it partway through
    /// (`tests/nested.rs`, `return_status_is_the_error_number_after_a_runtime_error` and
    /// `return_status_is_the_error_number_after_a_compile_error`).
    pub return_status: i32,
    /// `@@ROWCOUNT` once the text is done: what its last executed statement left, unchanged
    /// from the value the caller held when a compile-time error kept every statement of the
    /// text from running, `0` when a statement started running and then raised an error
    /// (`tests/nested.rs`, `rowcount_reflects_the_last_row_count_of_the_nested_text` and
    /// `rowcount_is_zero_after_a_runtime_error_and_unchanged_after_a_compile_error`).
    pub rowcount: i64,
    /// `true` when an error of the text, or of a text it called, reached the client.
    pub failed: bool,
    /// `true` when the nested text emitted at least one result set.
    pub had_result_set: bool,
}

impl Session {
    /// Runs `text` like the body of `sp_executesql` an RPC calls: a level of its own whose
    /// variables are `params` (module documentation, "A called level"), then RETURNSTATUS
    /// when no error reached the client, and the DONEPROC that closes the call.
    ///
    /// # Errors
    ///
    /// An internal error that ends the request: a sink that cannot take what it is
    /// handed, a cancelled request.
    pub fn run_nested(
        &mut self,
        text: &str,
        params: &[NestedParam],
        sink: &mut dyn ResultSink,
    ) -> SqlResult<NestedOutcome> {
        let (outcome, _) = run_level(self, text, params, None, Body::Procedure, false, sink)?;
        if !outcome.failed {
            sink.return_status(outcome.return_status)?;
        }
        // The statements of the text carried their counts; the DONEPROC that closes the
        // call carries none (`tests/row_counts.rs`).
        sink.done_proc(None)?;
        Ok(outcome)
    }
}

/// What runs a called text, which says how deep below its caller the text runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    /// `EXEC('…')`: the text runs one level below the statement that wrote it.
    Dynamic,
    /// `sp_executesql`: the procedure runs one level below its caller, and its text one
    /// level below the procedure.
    ExecuteSql,
    /// A system procedure whose body is a text, or a prepared statement: one level below
    /// the caller.
    Procedure,
}

impl Body {
    /// How many levels below its caller the text runs.
    const fn levels(self) -> u32 {
        match self {
            Self::Dynamic | Self::Procedure => 1,
            Self::ExecuteSql => 2,
        }
    }

    /// The procedure an error of the level it opens names, when it is a procedure.
    const fn procedure(self) -> Option<&'static str> {
        match self {
            Self::ExecuteSql => Some("sp_executesql"),
            Self::Dynamic | Self::Procedure => None,
        }
    }
}

/// Runs `text` in a level of its own (module documentation, "A called level"), on
/// `database` when the call named one and on the caller's otherwise, its statements closed
/// in `sink`; the end of the level is the caller's to send. `in_program` tells that the
/// interpreter already opened the first level of `body`, its variables set aside and its
/// depth counted; the levels of `body` past that one are counted here.
///
/// A text that would run deeper than `ExecSession::max_nest_level` does not run: its call
/// is refused with 217, line 1, naming `sp_executesql` when that procedure opened the level
/// (`procedures::tests::sp_executesql_runs_its_text_two_levels_down`).
///
/// A `database` named by the call is the current database of the text while it runs,
/// without the 5701 a `USE` sends (`compat/tests/catalog_procs.rs`,
/// `a_qualified_call_leaves_the_caller_database_and_sends_no_context_change`).
///
/// A transaction count the text left different from the one it found is 266, sent after
/// the text when no other error was; the transaction the text ended in stays open for the
/// caller, its count included (`tests/exec_reopens_transaction.rs`,
/// `a_callee_that_opens_one_more_transaction_leaves_it_to_its_caller`).
///
/// # Errors
///
/// An internal error that ends the request: a sink that cannot take what it is handed, a
/// cancelled request.
fn run_level(
    session: &mut Session,
    text: &str,
    params: &[NestedParam],
    database: Option<&str>,
    body: Body,
    in_program: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<(NestedOutcome, ProgramEnd)> {
    let added = if in_program {
        body.levels() - 1
    } else {
        body.levels()
    };
    if let Some(err) = past_the_nesting_limit(session.exec(), added, body) {
        return refuse_level(session, err, in_program, sink);
    }
    let trancount_before = session.state().trancount;
    let mut level_state = session.state().clone();
    if let Some(database) = database {
        database.clone_into(&mut level_state.database);
    }
    let caller_state = std::mem::replace(session.state_mut(), level_state);
    let exec = session.exec_mut();
    let variables = std::mem::take(&mut exec.variables);
    let variable_types = std::mem::take(&mut exec.variable_types);
    exec.nest_level += added;
    for param in params {
        exec.variables
            .insert(param.name.clone(), param.value.clone());
        exec.variable_types
            .insert(param.name.clone(), param.ty.clone());
    }

    let ran = session.run_body(text, Level::Called, params, sink);

    let outputs = collect_outputs(params, session.exec());
    let exec = session.exec_mut();
    exec.variables = variables;
    exec.variable_types = variable_types;
    exec.nest_level = exec.nest_level.saturating_sub(added);
    let level_state = std::mem::replace(session.state_mut(), caller_state);
    // The transaction belongs to the connection, not to a level: the caller goes on in the
    // one its callee ended with. A transaction the called level closed is closed for its
    // caller too (the victim of a deadlock inside `EXEC('…')` leaves `@@TRANCOUNT` at 0,
    // `tests/deadlock_victim.rs`), and one it committed and opened again is the caller's,
    // with its locks, for the caller's `ROLLBACK` or `COMMIT` to end
    // (`tests/exec_reopens_transaction.rs`).
    session.state_mut().take_transaction_of(&level_state);
    let (mut end, track) = ran?;
    // A body refused before it ran leaves its error unsent for the program that called it
    // (`vauban_executor::ProgramEnd::Refused`); the text of an RPC has no such program, and
    // sends it here.
    if !in_program && let ProgramEnd::Refused(err) = end {
        sink.error(&err)?;
        end = ProgramEnd::ended_by(err, level_state.options.xact_abort);
    }
    if end == ProgramEnd::Cancelled {
        return Err(cancelled());
    }
    let mut outcome = NestedOutcome {
        outputs,
        return_status: track.status,
        rowcount: level_state.rowcount,
        failed: track.failed,
        had_result_set: track.had_result_set,
    };
    if let ProgramEnd::Returned(code) = end {
        outcome.return_status = code;
    }
    if level_state.trancount != trancount_before {
        let err = SqlError::transaction_count_after_execute(
            i64::from(trancount_before),
            i64::from(level_state.trancount),
        );
        // The transaction stays open as the callee left it, its count included
        // (`tests/exec_reopens_transaction.rs`,
        // `a_callee_that_opens_one_more_transaction_leaves_it_to_its_caller`).
        if !outcome.failed {
            sink.error(&err)?;
            outcome.failed = true;
        }
    }
    Ok((outcome, end))
}

/// The 217 of a text that would run `added` levels below the running one, past
/// `ExecSession::max_nest_level`; `None` when it stays within the limit.
fn past_the_nesting_limit(exec: &ExecSession, added: u32, body: Body) -> Option<SqlError> {
    let limit = exec.max_nest_level?;
    if exec.nest_level.saturating_add(added) <= limit {
        return None;
    }
    let err = SqlError::nesting_level_exceeded(limit).with_line(1);
    Some(match body.procedure() {
        Some(procedure) => err.with_procedure(procedure),
        None => err,
    })
}

/// A level refused before its text ran: the error stays unsent for the program that made
/// the call, which decides its reach as for the `EXECUTE` it wrote; the text of an RPC has
/// no such program, and its error is sent here.
fn refuse_level(
    session: &Session,
    err: SqlError,
    in_program: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<(NestedOutcome, ProgramEnd)> {
    let outcome = NestedOutcome {
        outputs: Vec::new(),
        return_status: error_status(&err),
        rowcount: session.state().rowcount,
        failed: true,
        had_result_set: false,
    };
    if in_program {
        return Ok((outcome, ProgramEnd::Refused(err)));
    }
    sink.error(&err)?;
    let end = ProgramEnd::ended_by(err, session.state().options.xact_abort);
    Ok((outcome, end))
}

/// The final values of the `output` parameters among `params`, read from the level that
/// ran them.
fn collect_outputs(params: &[NestedParam], exec: &ExecSession) -> Vec<(String, Value)> {
    params
        .iter()
        .filter(|param| param.output)
        .filter_map(|param| {
            exec.variables
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&param.name))
                .map(|(name, value)| (name.clone(), value.clone()))
        })
        .collect()
}

/// Closes a level a call written in a program opened: with a DONE that ends the `EXECUTE`
/// in the batch, with a DONEINPROC in a called level, where a further DONE follows. A level
/// that an error of the whole batch ended is not closed in a called level: that error ends
/// each level up to the batch, which sends the last DONE (`tests/nested.rs`,
/// `an_error_of_the_batch_two_levels_down_ends_the_response_with_one_done`).
fn close_level(
    sink: &mut dyn ResultSink,
    in_proc: bool,
    nocount: bool,
    end: &ProgramEnd,
) -> SqlResult<()> {
    match (in_proc, end) {
        // The program that made the call sends the error and the token that closes the
        // `EXECUTE` once it decided the reach of that error.
        (_, ProgramEnd::Refused(_)) | (true, ProgramEnd::Aborted(_)) => Ok(()),
        // In a called level, `SET NOCOUNT ON` of the caller silences the end of the call as
        // it does the end of a statement, except for a level an error ended: that error
        // sent no DONEINPROC of its own, and the end of the call is the one that carries it
        // (`tests/row_counts.rs`).
        (true, ProgramEnd::LevelAborted(_)) => sink.done_in_proc(None, true),
        (true, _) if nocount => Ok(()),
        // The statements of the text carried their counts; the end of the call carries no
        // count.
        (true, _) => sink.done_in_proc(None, true),
        (false, _) => sink.done(None, true),
    }
}

/// How a procedure call closes on the wire.
pub(crate) enum ProcFinish<'a> {
    /// RPC request: RETURNVALUE tokens, RETURNSTATUS, then DONEPROC.
    Rpc {
        /// Original RPC parameters, in wire order.
        rpc_params: &'a [RpcParam],
    },
    /// `EXEC` written in a program: the statements of the callee end with DONEINPROC, and
    /// the call ends with a DONE in the batch, with a DONEINPROC in a called level.
    Batch {
        /// The `EXEC` is written in a called level rather than in the batch.
        in_proc: bool,
    },
}

/// How a call ended, for the program that made it: how the callee ended, or the error of
/// the `EXECUTE` itself, not sent yet (`vauban_executor::Host::call`).
pub(crate) type CallEnd = Result<ProgramEnd, SqlError>;

/// An `EXEC` of a procedure written in a program, its arguments evaluated.
pub(crate) struct BatchCall<'c> {
    /// The procedure name, as the `EXEC` names it.
    pub(crate) name: &'c str,
    /// The database part written left of the name.
    pub(crate) qualifier: Option<&'c str>,
    /// The arguments, in written order.
    pub(crate) args: &'c [EvaluatedExecArg],
    /// The `@rc =` variable that receives the status.
    pub(crate) return_into: Option<&'c str>,
    /// The line of the `EXEC`.
    pub(crate) line: u32,
    /// The `EXEC` is written in a called level rather than in the batch.
    pub(crate) in_proc: bool,
}

/// The database a call qualified by a name runs its body on, `None` when the call qualified
/// nothing.
///
/// The name is answered with the spelling the catalogue holds. A name the catalogue holds no
/// database for answers 911 before the body runs, so the call produces no result set
/// (`compat/tests/catalog_procs.rs`,
/// `a_call_qualified_by_a_missing_database_fails_before_the_body_runs`).
pub(crate) fn resolve_call_database(
    session: &Session,
    database: Option<&str>,
) -> SqlResult<Option<String>> {
    let Some(database) = database else {
        return Ok(None);
    };
    match catalogued_database_name(session, database)? {
        Some(found) => Ok(Some(found)),
        None => Err(SqlError {
            state: QUALIFIED_CALL_MISSING_DATABASE_STATE,
            ..SqlError::database_not_found(database)
        }),
    }
}

/// State of the 911 an `EXEC` qualified by an unknown database answers, where
/// [`SqlError::database_not_found`] builds state 1 (`compat/tests/catalog_procs.rs`,
/// `a_call_qualified_by_a_missing_database_fails_before_the_body_runs`).
const QUALIFIED_CALL_MISSING_DATABASE_STATE: u8 = 4;

/// The name the catalogue holds for `database`, `None` when the lookup finds nothing.
fn catalogued_database_name(session: &Session, database: &str) -> SqlResult<Option<String>> {
    let engine = session.engine();
    if let Some(session_txn) = &session.state().txn {
        let snapshot = engine.catalog.snapshot(&session_txn.handle);
        return Ok(snapshot.database(database).map(|found| found.name.clone()));
    }
    let binding = engine.txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = engine.catalog.snapshot(&binding);
    let found = snapshot.database(database).map(|found| found.name.clone());
    engine.txn.commit(binding)?;
    Ok(found)
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
    /// `true` when the call sent at least one result set.
    pub had_result_set: bool,
}

/// Runs `action` for an RPC or a batch `EXEC`.
///
/// `database` is the database the body runs on, `None` for the caller's own. It reaches the
/// two actions that run T-SQL; a static result set and a description are built without one.
pub(crate) fn execute_proc_action(
    session: &mut Session,
    action: ProcAction,
    database: Option<&str>,
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    run_action(session, action, database, finish, sink).map(|(_, outcome)| outcome)
}

/// [`execute_proc_action`], with how the call ended for a program that made it.
fn run_action(
    session: &mut Session,
    action: ProcAction,
    database: Option<&str>,
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<(CallEnd, BatchProcOutcome)> {
    match action {
        ProcAction::ExecuteSql { statement, params } => {
            let call = NestedSql {
                statement: &statement,
                params: &params,
                database,
                body: Body::ExecuteSql,
            };
            execute_nested_sql(session, &call, finish, sink)
        }
        ProcAction::Template { sql, params } => {
            let call = NestedSql {
                statement: &sql,
                params: &params,
                database,
                body: Body::Procedure,
            };
            execute_nested_sql(session, &call, finish, sink)
        }
        ProcAction::Static { columns, rows } => {
            execute_static(session, &columns, &rows, finish, sink)
        }
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

/// Resolves and runs an `EXEC` of a stored procedure written in a program.
///
/// The database the call names is resolved before the procedure name is: a call qualified
/// by a database the catalogue does not hold fails whether or not the procedure exists
/// (`compat/tests/catalog_procs.rs`,
/// `a_call_qualified_by_a_missing_database_fails_before_the_body_runs`).
pub(crate) fn run_batch_procedure(
    session: &mut Session,
    call: &BatchCall<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<(CallEnd, BatchProcOutcome)> {
    let database = match resolve_call_database(session, call.qualifier) {
        Ok(database) => database,
        Err(err) => {
            return Ok(refuse_call(at_line(err, call.line)));
        }
    };
    let stored = exec_args_to_proc_args(call.name, call.args);
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
    let Some(resolver) = super::rpc::system_procedure_resolver(session) else {
        let err = SqlError::procedure_not_found(call.name).with_line(call.line);
        return Ok(refuse_call(err));
    };
    match resolver(call.name, &proc_args) {
        None => {
            let err = SqlError::procedure_not_found(call.name).with_line(call.line);
            Ok(refuse_call(err))
        }
        Some(Err(err)) if ARGUMENT_CONVERSION_ERRORS.contains(&err.number) => {
            argument_not_converted(session, at_call_line(err), call.in_proc, sink)
        }
        Some(Err(err)) => Ok(refuse_call(at_call_line(err))),
        Some(Ok(action)) => {
            let finish = ProcFinish::Batch {
                in_proc: call.in_proc,
            };
            let declared = declared_output_types(&action);
            let (end, outcome) = run_action(session, action, database.as_deref(), finish, sink)?;
            match apply_batch_outputs(session, call.args, call.return_into, &outcome, &declared) {
                Ok(()) => Ok((end, outcome)),
                // A value its receiver cannot hold ends the batch (8114, state 2).
                Err(err) => {
                    sink.error(&err)?;
                    let mut outcome = outcome;
                    outcome.failed = true;
                    outcome.error_number = err.number;
                    session.state_mut().last_error = err.number;
                    Ok((Ok(ProgramEnd::Aborted(err)), outcome))
                }
            }
        }
    }
}

/// Runs `EXEC('…')` written in a program: the text in a level of its own, closed as the
/// `EXEC` is ([`close_level`]).
pub(crate) fn run_batch_dynamic(
    session: &mut Session,
    text: &str,
    in_proc: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<(CallEnd, BatchProcOutcome)> {
    let mut tracked = TrackedSink::new(sink);
    let (nested, end) = run_level(session, text, &[], None, Body::Dynamic, true, &mut tracked)?;
    let error_number = tracked.last_error;
    let nocount = session.state().options.nocount;
    close_level(sink, in_proc, nocount, &end)?;
    let outcome = BatchProcOutcome {
        outputs: nested.outputs,
        rowcount: nested.rowcount,
        return_status: nested.return_status,
        failed: nested.failed,
        error_number,
        had_result_set: nested.had_result_set,
    };
    session.state_mut().last_error = if outcome.failed { error_number } else { 0 };
    session.state_mut().rowcount = outcome.rowcount;
    let exec = session.exec().clone();
    apply_exec_session(session.state_mut(), &exec, true, !outcome.failed);
    Ok((Ok(end), outcome))
}

/// The text of `sp_executesql`, or of a procedure written in T-SQL, with what it runs on.
struct NestedSql<'a> {
    /// The text.
    statement: &'a str,
    /// Its parameters, its first variables.
    params: &'a [ProcParam],
    /// The database the call named, `None` for the caller's.
    database: Option<&'a str>,
    /// What runs it.
    body: Body,
}

/// Runs the text of `call` in a level of its own whose variables are its parameters, and
/// closes the call as `finish` says.
fn execute_nested_sql(
    session: &mut Session,
    call: &NestedSql<'_>,
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<(CallEnd, BatchProcOutcome)> {
    let params = call.params;
    let nested_params = proc_params_to_nested(params);
    let in_program = matches!(finish, ProcFinish::Batch { .. });
    let mut tracked = TrackedSink::new(sink);
    let (nested, end) = run_level(
        session,
        call.statement,
        &nested_params,
        call.database,
        call.body,
        in_program,
        &mut tracked,
    )?;
    let last_error = tracked.last_error;
    match finish {
        ProcFinish::Rpc { rpc_params } => {
            if !nested.failed {
                emit_rpc_return_values(sink, rpc_params, params, &nested)?;
                sink.return_status(nested.return_status)?;
            }
            sink.done_proc(None)?;
        }
        ProcFinish::Batch { in_proc } => {
            let nocount = session.state().options.nocount;
            close_level(sink, in_proc, nocount, &end)?;
        }
    }
    session.state_mut().last_error = if nested.failed { last_error } else { 0 };
    Ok((
        Ok(end),
        BatchProcOutcome {
            outputs: nested.outputs,
            rowcount: nested.rowcount,
            return_status: nested.return_status,
            failed: nested.failed,
            error_number: last_error,
            had_result_set: nested.had_result_set,
        },
    ))
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
) -> SqlResult<(CallEnd, BatchProcOutcome)> {
    match crate::prepared::describe_batch(session, statement, kind) {
        Ok(rows) => execute_static(session, columns, &rows, finish, sink),
        Err(err) => fail_proc(session, err, finish, sink),
    }
}

/// Sends a result set the procedure answers without running T-SQL. Its DONEINPROC in a
/// called level follows `SET NOCOUNT`.
fn execute_static(
    session: &Session,
    columns: &[StaticResultColumn],
    rows: &[Vec<Value>],
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<(CallEnd, BatchProcOutcome)> {
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
        ProcFinish::Batch { in_proc: false } => sink.done(Some(rowcount), true)?,
        ProcFinish::Batch { in_proc: true } => {
            let reported = (!session.state().options.nocount).then_some(rowcount);
            sink.done_in_proc(reported, true)?;
        }
    }
    Ok((
        Ok(ProgramEnd::Completed),
        BatchProcOutcome {
            outputs: Vec::new(),
            rowcount: i64::try_from(rowcount).unwrap_or(i64::MAX),
            return_status: 0,
            failed: false,
            error_number: 0,
            had_result_set: true,
        },
    ))
}

/// Refuses a resolved call with `err`: for an RPC, the error goes to the client and closes
/// the call; for an `EXEC` written in a program, it is the error of that statement
/// ([`refuse_call`]).
fn fail_proc(
    session: &mut Session,
    err: SqlError,
    finish: ProcFinish<'_>,
    sink: &mut dyn ResultSink,
) -> SqlResult<(CallEnd, BatchProcOutcome)> {
    match finish {
        ProcFinish::Rpc { .. } => {
            session.state_mut().last_error = err.number;
            sink.error(&err)?;
            sink.done_proc(None)?;
            Ok((Err(err.clone()), refused_outcome(&err)))
        }
        ProcFinish::Batch { .. } => Ok(refuse_call(err)),
    }
}

/// The `EXECUTE` written in a program failed with `err` before a callee ran: the error is
/// the error of that statement, which the interpreter acts on and hands to the client
/// (`vauban_executor::Host::call`).
fn refuse_call(err: SqlError) -> (CallEnd, BatchProcOutcome) {
    let outcome = refused_outcome(&err);
    (Err(err), outcome)
}

/// The numbers an argument that does not convert to the type of its parameter raises.
const ARGUMENT_CONVERSION_ERRORS: [u32; 3] = [206, 257, 8114];

/// An `EXECUTE` written in a program whose argument `err` refused to convert to the type
/// of its parameter: the error ends that statement alone, and the program goes on with the
/// next one, whatever reach the number has elsewhere
/// (`compat/tests/bound_parameters.rs`, `an_argument_that_does_not_convert_ends_its_statement_alone`).
fn argument_not_converted(
    session: &mut Session,
    err: SqlError,
    in_proc: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<(CallEnd, BatchProcOutcome)> {
    session.state_mut().last_error = err.number;
    sink.error(&err)?;
    let end = ProgramEnd::LevelAborted(err.clone());
    let nocount = session.state().options.nocount;
    close_level(sink, in_proc, nocount, &end)?;
    Ok((Ok(end), refused_outcome(&err)))
}

/// What a call refused with `err` leaves to its caller.
fn refused_outcome(err: &SqlError) -> BatchProcOutcome {
    BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: 0,
        return_status: 0,
        failed: true,
        error_number: err.number,
        had_result_set: false,
    }
}

/// The declared type of each `OUTPUT` parameter `action` binds, by parameter name.
fn declared_output_types(action: &ProcAction) -> Vec<(String, TypeInfo)> {
    match action {
        ProcAction::ExecuteSql { params, .. } | ProcAction::Template { params, .. } => params
            .iter()
            .filter(|param| param.output)
            .map(|param| (param.name.clone(), param.ty.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Copies the final `OUTPUT` values and the return status of a call into the variables of
/// the program that made it, each value converted from the declared type of its parameter
/// (`declared`) to the type of its variable.
///
/// # Errors
///
/// 8114, state 2, for a value its variable cannot hold; the values before it are copied.
fn apply_batch_outputs(
    session: &mut Session,
    exec_args: &[EvaluatedExecArg],
    return_into: Option<&str>,
    outcome: &BatchProcOutcome,
    declared: &[(String, TypeInfo)],
) -> SqlResult<()> {
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
            let from = declared
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, ty)| ty);
            // The caller's level is set aside while the call runs: the type of the variable
            // is the one its value was passed under.
            let to = arg.value.as_ref().map(|(_, ty)| ty);
            let value = match (from, to) {
                (Some(from), Some(to)) => returned_output(&value, from, to)?,
                _ => value,
            };
            session.exec_mut().variables.insert(var.clone(), value);
        }
    }
    let exec = session.exec().clone();
    apply_exec_session(session.state_mut(), &exec, true, !outcome.failed);
    Ok(())
}

/// Sends the final value of each `OUTPUT` parameter of an RPC, converted from the declared
/// type of its parameter (`declared`) to the type the RPC gave it.
fn emit_rpc_return_values(
    sink: &mut dyn ResultSink,
    rpc_params: &[RpcParam],
    declared: &[ProcParam],
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
        let value = match declared
            .iter()
            .find(|decl| decl.name.eq_ignore_ascii_case(&param.name))
        {
            Some(decl) => returned_output(value, &decl.ty, &param.ty)?,
            None => value.clone(),
        };
        sink.return_value(&param.name, &param.ty, &value)?;
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

/// One argument of a system procedure call, kept alive while the resolver borrows it.
pub(crate) struct StoredProcArg {
    pub(crate) name: Option<String>,
    pub(crate) ty: TypeInfo,
    pub(crate) value: Value,
    pub(crate) output: bool,
    pub(crate) default: bool,
}

/// The arguments of an `EXEC` of `proc`, under the types the procedure reads them with.
pub(crate) fn exec_args_to_proc_args(proc: &str, args: &[EvaluatedExecArg]) -> Vec<StoredProcArg> {
    args.iter()
        .map(|arg| {
            let (ty, value, default) = match &arg.value {
                None => (TypeInfo::new(SqlType::Int, true), Value::Null, true),
                Some((value, ty)) => (
                    coerce_unicode_literal_ty(proc, ty, value),
                    value.clone(),
                    false,
                ),
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

/// The three procedures that read their text parameter under the type the caller wrote, so
/// that a `varchar` one is refused instead of converted
/// (`tests/execute.rs`, `a_varchar_statement_is_refused_by_executesql`; `tests/describe.rs`,
/// `a_varchar_batch_is_refused_by_describe_first_result_set`,
/// `a_varchar_batch_is_refused_by_describe_undeclared_parameters`).
const TEXT_ARGUMENTS_READ_UNCONVERTED: [&str; 3] = [
    "sp_executesql",
    "sp_describe_first_result_set",
    "sp_describe_undeclared_parameters",
];

/// The type `proc` reads a text argument under.
///
/// A catalogue procedure declares its text parameters as `sysname` and a `varchar`
/// argument reaches its body converted, written as a literal or read from a variable alike
/// (`tests/execute.rs`, `a_catalogue_procedure_still_reads_a_varchar_argument`); a `char`
/// argument is not widened here. The procedures of
/// [`TEXT_ARGUMENTS_READ_UNCONVERTED`] do not convert it: the type stays what the caller
/// gave (`tests/execute.rs`, `a_varchar_statement_is_refused_by_executesql`).
pub(crate) fn coerce_unicode_literal_ty(proc: &str, ty: &TypeInfo, value: &Value) -> TypeInfo {
    let converts = !TEXT_ARGUMENTS_READ_UNCONVERTED.contains(&object_part(proc).as_str());
    if converts && matches!(value, Value::String(_)) && matches!(ty.ty, SqlType::VarChar(_)) {
        TypeInfo::new(SqlType::NVarChar(Len::Max), ty.nullable)
    } else {
        ty.clone()
    }
}

/// The object part of a procedure name, unbracketed and lower-cased: `[sys].[sp_tables]`,
/// `sys.sp_tables` and `sp_tables` answer `sp_tables`.
fn object_part(name: &str) -> String {
    let stripped = name.replace(['[', ']'], "");
    stripped
        .rsplit('.')
        .next()
        .unwrap_or(stripped.as_str())
        .to_ascii_lowercase()
}

fn at_line(err: SqlError, line: u32) -> SqlError {
    if line == 0 { err } else { err.with_line(line) }
}

/// The numbers a call answers without a line. The argument list holds more values
/// than the procedure takes (8144), or the parameterized text declares no parameter and the
/// call passed one anyway (8146). Both count arguments instead of pointing at one
/// (`tests/execute.rs`, `argument_errors_of_a_call_leave_with_the_line_their_number_calls_for`).
///
/// An argument that does not convert to the type of its parameter leaves with no line
/// either ([`ARGUMENT_CONVERSION_ERRORS`]; `compat/tests/bound_parameters.rs`,
/// `argument_conversion_errors_leave_without_a_line`).
const COUNTED_WITHOUT_A_LINE: [u32; 5] = [
    8144,
    8146,
    ARGUMENT_CONVERSION_ERRORS[0],
    ARGUMENT_CONVERSION_ERRORS[1],
    ARGUMENT_CONVERSION_ERRORS[2],
];

/// First line of the parameterized text a call carries, where an argument error the
/// resolution raised without a line of its own is read.
const FIRST_LINE_OF_THE_CALL: u32 = 1;

/// The line an error raised while the arguments of a system procedure are bound leaves with.
///
/// The line of the `EXEC` is not it: what the resolution raises is located in the
/// parameterized text the call carries, so a declaration the parser refuses on the second
/// line of `@params` keeps that line, and an error with no line of its own leaves on the
/// first, wherever in the batch the `EXEC` sits. The numbers of
/// [`COUNTED_WITHOUT_A_LINE`] leave with no line at all (`tests/execute.rs`,
/// `argument_errors_of_a_call_leave_with_the_line_their_number_calls_for`).
fn at_call_line(err: SqlError) -> SqlError {
    if COUNTED_WITHOUT_A_LINE.contains(&err.number) {
        return err.with_line(0);
    }
    if err.line == 0 {
        return err.with_line(FIRST_LINE_OF_THE_CALL);
    }
    err
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
    fn next_cur_cmd(&mut self, cur_cmd: u16) -> SqlResult<()> {
        self.inner.next_cur_cmd(cur_cmd)
    }
}

#[cfg(test)]
mod tests;
