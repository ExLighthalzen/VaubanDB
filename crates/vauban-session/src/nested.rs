//! Nested batch execution: a T-SQL text with pre-declared parameters, its own variable
//! scope, and procedure-style DONE tokens ([MS-TDS] 2.2.7 DONEINPROC / DONEPROC).

use std::sync::Arc;

use vauban_binder::{BatchVariables, BindContext, BoundStatement, DdlStatement};
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{BatchErrorScope, InternalError, SqlError, SqlResult};
use vauban_executor::{EvaluatedExecArg, ExecContext, ExecOutcome, ExecSession, RowSink};
use vauban_parser::{Statement, parse_batch};
use vauban_planner::{PhysicalStatement, PlanContext, StorageIndexes};
use vauban_tds::{ColumnFlags, ColumnMeta, EnvChange};
use vauban_txn::IsolationLevel;
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::batch::Session;
use crate::eval_context::{SessionEvalContext, apply_exec_session};
use crate::fake_engine;
use crate::login::{DATABASE_CONTEXT_STATE_USE, changed_database_context};
use crate::rpc::{ProcAction, ProcArg, ProcParam, StaticResultColumn};
use crate::set_options::{SetOutcome, apply_set_statement, sync_exec_session};
use crate::sink::{ResultSink, RowSinkAdapter};
use crate::state::SessionState;
use crate::statements::{
    INTERNAL_ERROR, INVALID_OBJECT_NAME, at_statement, next_rowcount, statement_clears_last_error,
    statement_fragment, statement_line, statement_span, statement_text, use_name_line,
};
use crate::txn_session::{self, StatementTxn};

/// Default schema of a login, `dbo` until `catalog` knows better.
const DEFAULT_SCHEMA: &str = "dbo";

/// One parameter of a nested batch, declared before the text is bound.
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

/// What a nested batch leaves behind once its tokens are sent.
#[derive(Debug, Clone, PartialEq)]
pub struct NestedOutcome {
    /// Final values of the `output` parameters, in declaration order.
    pub outputs: Vec<(String, Value)>,
    /// `RETURN` status, `0` when the text did not `RETURN`.
    pub return_status: i32,
    /// `@@ROWCOUNT` at the end of the nested text.
    pub rowcount: i64,
    /// `true` when an error stopped the text before it finished.
    pub failed: bool,
    /// `true` when the nested text emitted at least one result set.
    pub had_result_set: bool,
}

/// Row count for the closing DONEPROC of a nested batch or RPC when it is known.
pub(crate) fn nested_done_proc_rowcount(
    failed: bool,
    had_result_set: bool,
    rowcount: i64,
) -> Option<u64> {
    if failed || had_result_set {
        return None;
    }
    if rowcount > 0 {
        Some(rowcount as u64)
    } else {
        None
    }
}

/// What one statement of a nested batch decided about the rest of it.
enum Flow {
    Continue,
    Stop,
}

/// One statement after compilation-stage checks have succeeded for the nested text.
enum PreparedStatement {
    Set {
        text: String,
        line: u32,
    },
    Fallback {
        text: String,
        error: SqlError,
    },
    Bound {
        statement: PhysicalStatement,
        line: u32,
    },
    Use {
        statement: PhysicalStatement,
        database: String,
        line: u32,
    },
    /// A statement whose binding answered 208 while the text was prepared, bound again
    /// when the text reaches it (`batch.rs`, `PreparedStatement::Deferred`, for the
    /// rule; here it is what makes `EXEC('CREATE TABLE dbo.t (n int); INSERT dbo.t VALUES
    /// (1); SELECT n FROM dbo.t;')` answer a row).
    Deferred {
        padded: String,
        raw: String,
        variables: BatchVariables,
    },
}

impl Session {
    /// Runs `text` like the body of `sp_executesql`: a fresh variable scope filled from
    /// `params`, procedure-style DONE tokens, and the caller [`ExecSession`] restored
    /// afterward so its `@@ROWCOUNT` is unchanged by the call.
    pub fn run_nested(
        &mut self,
        text: &str,
        params: &[NestedParam],
        sink: &mut dyn ResultSink,
    ) -> SqlResult<NestedOutcome> {
        let trancount_before = self.state().trancount;
        let saved_exec = std::mem::take(self.exec_mut());
        {
            let session_state = self.state().clone();
            let exec = self.exec_mut();
            sync_exec_session(&session_state, exec);
            seed_params(params, exec)?;
        }

        let mut nested_state = self.state().clone();
        let mut outcome = NestedOutcome {
            outputs: Vec::new(),
            return_status: 0,
            rowcount: 0,
            failed: false,
            had_result_set: false,
        };

        let result = self.run_nested_body(text, params, &mut nested_state, sink, &mut outcome);

        outcome.outputs = collect_outputs(params, self.exec());
        outcome.rowcount = self.exec().rowcount;

        if nested_state.trancount != trancount_before {
            let err = SqlError::transaction_count_after_execute(
                i64::from(trancount_before),
                i64::from(nested_state.trancount),
            );
            if !outcome.failed {
                sink.error(&err)?;
                outcome.failed = true;
            }
            txn_session::rollback_all(&mut nested_state, &Arc::clone(self.engine()))?;
        }

        if !outcome.failed {
            sink.return_status(outcome.return_status)?;
        }
        sink.done_proc(nested_done_proc_rowcount(
            outcome.failed,
            outcome.had_result_set,
            outcome.rowcount,
        ))?;

        *self.exec_mut() = saved_exec;
        result?;
        Ok(outcome)
    }

    fn run_nested_body(
        &mut self,
        text: &str,
        params: &[NestedParam],
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<()> {
        let batch = match parse_batch(text, &nested_state.options.parse_options()) {
            Ok(batch) => batch,
            Err(err) => return self.fail_nested(&err, sink, outcome),
        };
        if batch.statements.is_empty() {
            return Ok(());
        }

        let (mut prepared, mut bind_state, mut batch_variables) =
            match self.prepare_nested(text, &batch.statements, params, nested_state) {
                Ok(prepared) => prepared,
                Err(err) if self.cancel_handle().is_cancelled() => return Err(err),
                Err(err) => return self.fail_nested(&err, sink, outcome),
            };

        let mut index = 0;
        while index < prepared.len() {
            self.cancel_handle().check()?;
            let more = index + 1 < prepared.len();
            let current = &prepared[index];
            if let Flow::Stop =
                self.run_nested_prepared(current, more, nested_state, sink, outcome)?
            {
                break;
            }
            let tail_start = index + 1;
            if crate::batch::should_reprepare_tail(
                tail_start,
                prepared.len(),
                batch.statements.len(),
                catalog_changed(current, &batch.statements[tail_start..]),
            ) {
                let tail = self.reprepare_nested_tail(
                    text,
                    &batch.statements[tail_start..],
                    params,
                    &mut bind_state,
                    &mut batch_variables,
                )?;
                prepared.truncate(tail_start);
                prepared.extend(tail);
            }
            index += 1;
        }
        Ok(())
    }

    fn prepare_nested(
        &self,
        text: &str,
        statements: &[Statement],
        params: &[NestedParam],
        nested_state: &SessionState,
    ) -> SqlResult<(Vec<PreparedStatement>, SessionState, BatchVariables)> {
        let mut state = nested_state.clone();
        let mut batch_variables = BatchVariables::new();
        seed_batch_variables(params, &mut batch_variables)?;
        let binding = self.engine().txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.engine().catalog.snapshot(&binding);
        let prepared = self.bind_nested(
            text,
            statements,
            &snapshot,
            &mut state,
            &mut batch_variables,
            crate::batch::bind_limit_for(statements),
        );
        let closed = self.engine().txn.commit(binding);
        match (prepared, closed) {
            (Ok(prepared), Ok(())) => Ok((prepared, state, batch_variables)),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    fn reprepare_nested_tail(
        &self,
        text: &str,
        statements: &[Statement],
        params: &[NestedParam],
        state: &mut SessionState,
        batch_variables: &mut BatchVariables,
    ) -> SqlResult<Vec<PreparedStatement>> {
        let binding = self.engine().txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.engine().catalog.snapshot(&binding);
        let prepared = self.bind_nested(
            text,
            statements,
            &snapshot,
            state,
            batch_variables,
            crate::batch::bind_limit_for(statements),
        );
        self.engine().txn.commit(binding)?;
        let _ = params;
        prepared
    }

    fn bind_nested(
        &self,
        text: &str,
        statements: &[Statement],
        snapshot: &CatalogSnapshot,
        state: &mut SessionState,
        batch_variables: &mut BatchVariables,
        mut limit: Option<usize>,
    ) -> SqlResult<Vec<PreparedStatement>> {
        let mut prepared = Vec::with_capacity(statements.len());

        for (index, original) in statements.iter().enumerate() {
            if limit == Some(0) {
                break;
            }
            self.cancel_handle().check()?;
            let raw = statement_text(text, original);
            if matches!(original, Statement::SetOption(_)) {
                apply_set_statement(state, &raw, Some(snapshot));
                prepared.push(PreparedStatement::Set {
                    text: raw,
                    line: statement_span(original).line,
                });
                continue;
            }

            let line = statement_line(original);
            let source = statement_fragment(text, original, statements.get(index + 1));
            let padded = format!("{}{}", "\n".repeat(line.saturating_sub(1) as usize), source);
            let one = match self.bind_one_nested(&padded, raw, snapshot, state, batch_variables) {
                Ok(one) => one,
                Err(error) if error.number == INVALID_OBJECT_NAME => PreparedStatement::Deferred {
                    padded,
                    raw: statement_text(text, original),
                    variables: batch_variables.clone(),
                },
                Err(error) => return Err(error),
            };
            let counts = matches!(
                one,
                PreparedStatement::Bound { .. } | PreparedStatement::Deferred { .. }
            );
            prepared.push(one);
            if !counts {
                continue;
            }
            if let Some(remaining) = &mut limit {
                *remaining = remaining.saturating_sub(1);
                if *remaining == 0 {
                    break;
                }
            }
        }
        Ok(prepared)
    }

    /// Reparses, binds, plans and compile-checks one statement of a nested text against
    /// `snapshot` (`batch.rs`, `Session::bind_one`, for the rule).
    fn bind_one_nested(
        &self,
        padded: &str,
        raw: String,
        snapshot: &CatalogSnapshot,
        state: &mut SessionState,
        batch_variables: &mut BatchVariables,
    ) -> SqlResult<PreparedStatement> {
        let reparsed = parse_batch(padded, &state.options.parse_options())?;
        let [statement] = reparsed.statements.as_slice() else {
            return Err(SqlError::from(InternalError::Bug(
                "one statement reparsed as a different statement count".to_owned(),
            )));
        };
        let line = statement_line(statement);

        let options = state.binder_options();
        let ctx = BindContext {
            text: padded,
            catalog: Some(snapshot),
            database: &state.database,
            default_schema: DEFAULT_SCHEMA,
            variables: batch_variables,
            options,
        };
        let bound = match vauban_binder::bind(statement, &ctx) {
            Ok(bound) => bound,
            Err(error)
                if error.number == INTERNAL_ERROR && matches!(statement, Statement::Waitfor(_)) =>
            {
                return Ok(PreparedStatement::Fallback { text: raw, error });
            }
            Err(error) => return Err(error),
        };
        if let BoundStatement::Declare(declarations) = &bound {
            for declaration in declarations {
                batch_variables.declare(&declaration.name, declaration.ty.clone())?;
            }
        }

        let indexes = StorageIndexes(self.engine().storage.as_ref());
        let physical = vauban_planner::plan(bound, &PlanContext { catalog: &indexes })
            .map_err(|error| at_statement(error, line))?;

        let eval = SessionEvalContext::new(state, Some(snapshot));
        let mut exec = ExecContext::scalar(&eval, state.options.to_binder());
        vauban_executor::compile(&physical, &mut exec)
            .map_err(|error| at_statement(error, line))?;

        if let PhysicalStatement::Use { database } = &physical {
            let found = snapshot
                .database(database)
                .map(|target| target.name.clone())
                .ok_or_else(|| {
                    SqlError::database_not_found(database)
                        .with_line(use_name_line(padded, statement))
                })?;
            state.database = found.clone();
            return Ok(PreparedStatement::Use {
                statement: physical,
                database: found,
                line,
            });
        }

        Ok(PreparedStatement::Bound {
            statement: physical,
            line,
        })
    }

    /// Binds a deferred statement of a nested text against the catalogue of this moment,
    /// through the transaction of the nested state when one is open and through one of its
    /// own otherwise (`batch.rs`, `Session::bind_deferred`, for the rule).
    fn bind_nested_deferred(
        &self,
        padded: &str,
        raw: &str,
        variables: &BatchVariables,
        nested_state: &SessionState,
    ) -> SqlResult<PreparedStatement> {
        let mut state = nested_state.clone();
        let mut variables = variables.clone();
        if let Some(session_txn) = &nested_state.txn {
            let snapshot = self.engine().catalog.snapshot(&session_txn.handle);
            return self.bind_one_nested(
                padded,
                raw.to_owned(),
                &snapshot,
                &mut state,
                &mut variables,
            );
        }
        let binding = self.engine().txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.engine().catalog.snapshot(&binding);
        let bound = self.bind_one_nested(
            padded,
            raw.to_owned(),
            &snapshot,
            &mut state,
            &mut variables,
        );
        self.engine().txn.commit(binding)?;
        bound
    }

    fn run_nested_prepared(
        &mut self,
        prepared: &PreparedStatement,
        more: bool,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
        if let PreparedStatement::Set { text, line } = prepared {
            let snapshot = if let Some(session_txn) = &nested_state.txn {
                self.engine().catalog.snapshot(&session_txn.handle)
            } else {
                let binding = crate::set_options::begin_autocommit_txn(
                    nested_state,
                    &self.engine().storage,
                    &self.engine().txn,
                )?;
                let snap = self.engine().catalog.snapshot(&binding);
                self.engine().txn.commit(binding)?;
                snap
            };
            match apply_set_statement(nested_state, text, Some(&snapshot)) {
                SetOutcome::Applied | SetOutcome::Ignored(_) => {}
                SetOutcome::Failed(err) => {
                    let err = at_statement(err, *line);
                    return self
                        .fail_nested_statement(&err, sink, outcome)
                        .map(|()| Flow::Stop);
                }
            }
            sink.done_in_proc(None, true)?;
            nested_state.rowcount = 0;
            apply_exec_session(nested_state, self.exec(), true, true);
            self.exec_mut().identity_updated = false;
            return Ok(Flow::Continue);
        }

        if let PreparedStatement::Fallback { text, error } = prepared {
            return self.fall_back_nested(text, error, sink, outcome);
        }

        // A deferred statement is bound now and run like the statement it turned out to
        // be. A binding that fails now is a run-time error of the statement, batch-scoped
        // or not according to its number (`batch.rs`, `Session::run_deferred`).
        if let PreparedStatement::Deferred {
            padded,
            raw,
            variables,
        } = prepared
        {
            return match self.bind_nested_deferred(padded, raw, variables, nested_state) {
                Ok(bound) => self.run_nested_prepared(&bound, more, nested_state, sink, outcome),
                Err(err) if self.cancel_handle().is_cancelled() => Err(err),
                Err(err) => {
                    nested_state.last_error = err.number;
                    sink.error(&err)?;
                    outcome.failed = true;
                    let continues = more
                        && !nested_state.options.xact_abort
                        && err.batch_scope() == BatchErrorScope::Statement;
                    if continues {
                        sink.done_in_proc(None, true)?;
                        Ok(Flow::Continue)
                    } else {
                        Ok(Flow::Stop)
                    }
                }
            };
        }

        let (bound, line, target) = match prepared {
            PreparedStatement::Bound { statement, line } => (statement, *line, None),
            PreparedStatement::Use {
                statement,
                database,
                line,
            } => (statement, *line, Some(database.as_str())),
            PreparedStatement::Set { .. }
            | PreparedStatement::Fallback { .. }
            | PreparedStatement::Deferred { .. } => {
                unreachable!("SET, fallback and deferred were handled above")
            }
        };

        self.exec_mut().identity_updated = false;
        self.exec_mut().rowcount = 0;
        let mut adapter = RowSinkAdapter::new(sink);
        let (result, txn) = self.execute_nested_in_a_transaction(bound, nested_state, &mut adapter);
        let succeeded = result.is_ok();
        {
            let engine = Arc::clone(self.engine());
            let exec = self.exec_mut();
            txn_session::finish_statement(
                nested_state,
                &engine,
                exec,
                txn,
                txn_session::kind_of(bound),
                succeeded,
                sink,
            )?;
        }
        match result {
            Ok(ExecOutcome::Rows(count)) => {
                outcome.had_result_set = true;
                let reported = if nested_state.options.nocount {
                    None
                } else {
                    Some(count)
                };
                sink.done_in_proc(reported, true)?;
                nested_state.rowcount = count as i64;
                apply_exec_session(nested_state, self.exec(), true, true);
                self.exec_mut().identity_updated = false;
                Ok(Flow::Continue)
            }
            Ok(ExecOutcome::NoRows) => {
                if let Some(database) = target {
                    self.switch_nested_database(database, line, nested_state, sink)?;
                }
                sink.done_in_proc(None, true)?;
                // Same rule as the top-level batch (`batch.rs`, `run_prepared`): `next_rowcount`
                // decides `@@ROWCOUNT` for a `DECLARE` or a `WHILE`.
                nested_state.rowcount =
                    next_rowcount(bound, nested_state.rowcount, self.exec().rowcount);
                apply_exec_session(
                    nested_state,
                    self.exec(),
                    true,
                    statement_clears_last_error(bound),
                );
                self.exec_mut().identity_updated = false;
                Ok(Flow::Continue)
            }
            Ok(ExecOutcome::Return(code)) => {
                outcome.return_status = code;
                Ok(Flow::Stop)
            }
            Ok(ExecOutcome::Cancelled) => Err(SqlError::from(InternalError::Bug(
                crate::cancel::CANCELLED.to_owned(),
            ))),
            Ok(ExecOutcome::CallProcedure {
                name,
                args,
                return_into,
                line: exec_line,
            }) => self.run_nested_procedure(
                &name,
                &args,
                return_into.as_deref(),
                exec_line,
                more,
                nested_state,
                sink,
                outcome,
            ),
            Ok(ExecOutcome::RunDynamic {
                text,
                line: exec_line,
            }) => self.run_nested_dynamic(&text, exec_line, more, nested_state, sink, outcome),
            Ok(flow @ (ExecOutcome::Break | ExecOutcome::Continue)) => {
                let err = SqlError::from(InternalError::Bug(format!(
                    "run_nested_prepared: the executor answered {flow:?}, which this layer does \
                     not handle"
                )));
                self.fail_nested_statement(&err, sink, outcome)
                    .map(|()| Flow::Stop)
            }
            Ok(ExecOutcome::BatchAbort(err)) => {
                let err = at_statement(err, line);
                nested_state.last_error = err.number;
                sink.error(&err)?;
                outcome.failed = true;
                Ok(Flow::Stop)
            }
            Err(err) => {
                let err = at_statement(err, line);
                nested_state.last_error = err.number;
                sink.error(&err)?;
                outcome.failed = true;
                let continues = more
                    && !nested_state.options.xact_abort
                    && err.batch_scope() == BatchErrorScope::Statement;
                if continues {
                    sink.done_in_proc(None, true)?;
                    Ok(Flow::Continue)
                } else {
                    Ok(Flow::Stop)
                }
            }
        }
    }

    fn execute_nested_in_a_transaction(
        &mut self,
        bound: &PhysicalStatement,
        nested_state: &SessionState,
        sink: &mut dyn RowSink,
    ) -> (SqlResult<ExecOutcome>, StatementTxn) {
        let engine = Arc::clone(self.engine());
        let cancel = self.cancel_handle();
        let exec = self.exec_mut();
        txn_session::execute_in_a_transaction(nested_state, &engine, exec, &cancel, bound, sink)
    }

    fn switch_nested_database(
        &mut self,
        database: &str,
        line: u32,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<()> {
        let old = std::mem::replace(&mut nested_state.database, database.to_owned());
        sink.env_change(&EnvChange::Database {
            old,
            new: database.to_owned(),
        })?;
        sink.info(&changed_database_context(
            database,
            DATABASE_CONTEXT_STATE_USE,
            line,
        ))
    }

    fn fail_nested(
        &mut self,
        err: &SqlError,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<()> {
        outcome.failed = true;
        sink.error(err)?;
        Ok(())
    }

    fn fail_nested_statement(
        &mut self,
        err: &SqlError,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<()> {
        outcome.failed = true;
        sink.error(err)?;
        Ok(())
    }

    fn fall_back_nested(
        &mut self,
        stmt: &str,
        err: &SqlError,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
        match fake_engine::answer(&self.cancel_handle(), stmt, true, sink)? {
            Some(true) => Ok(Flow::Continue),
            Some(false) => Ok(Flow::Stop),
            None => self
                .fail_nested_statement(err, sink, outcome)
                .map(|()| Flow::Stop),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_nested_procedure(
        &mut self,
        name: &str,
        exec_args: &[EvaluatedExecArg],
        return_into: Option<&str>,
        line: u32,
        more: bool,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
        let proc_args = exec_args_to_proc_args(exec_args);
        let resolver_args: Vec<ProcArg<'_>> = proc_args
            .iter()
            .map(|arg| ProcArg {
                name: arg.name.as_deref(),
                ty: &arg.ty,
                value: &arg.value,
                output: arg.output,
                default: arg.default,
            })
            .collect();
        let Some(resolver) = crate::rpc::system_procedure_resolver() else {
            let err = SqlError::procedure_not_found(name).with_line(line);
            return self.finish_nested_procedure_error(err, more, nested_state, sink, outcome);
        };
        match resolver(name, &resolver_args) {
            None => {
                let err = SqlError::procedure_not_found(name).with_line(line);
                self.finish_nested_procedure_error(err, more, nested_state, sink, outcome)
            }
            Some(Err(err)) => self.finish_nested_procedure_error(
                at_procedure_line(err, line),
                more,
                nested_state,
                sink,
                outcome,
            ),
            Some(Ok(action)) => self.execute_nested_proc_action(
                action,
                exec_args,
                return_into,
                more,
                nested_state,
                sink,
                outcome,
            ),
        }
    }

    fn run_nested_dynamic(
        &mut self,
        text: &str,
        line: u32,
        more: bool,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
        let _ = line;
        let mut wrapper = NestedProcedureSink::new(sink, more, nested_state.options.xact_abort);
        let inner = self.run_nested(text, &[], &mut wrapper)?;
        self.finish_nested_procedure_flow(inner, &wrapper, more, nested_state, outcome, &[], None)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_nested_proc_action(
        &mut self,
        action: ProcAction,
        exec_args: &[EvaluatedExecArg],
        return_into: Option<&str>,
        more: bool,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
        match action {
            ProcAction::ExecuteSql { statement, params } => {
                let nested_params = proc_params_to_nested(&params);
                let mut wrapper =
                    NestedProcedureSink::new(sink, more, nested_state.options.xact_abort);
                let inner = self.run_nested(&statement, &nested_params, &mut wrapper)?;
                self.finish_nested_procedure_flow(
                    inner,
                    &wrapper,
                    more,
                    nested_state,
                    outcome,
                    exec_args,
                    return_into,
                )
            }
            ProcAction::Template { sql, params } => {
                let nested_params = proc_params_to_nested(&params);
                let mut wrapper =
                    NestedProcedureSink::new(sink, more, nested_state.options.xact_abort);
                let inner = self.run_nested(&sql, &nested_params, &mut wrapper)?;
                self.finish_nested_procedure_flow(
                    inner,
                    &wrapper,
                    more,
                    nested_state,
                    outcome,
                    exec_args,
                    return_into,
                )
            }
            ProcAction::Static { columns, rows } => {
                self.run_nested_static(&columns, &rows, more, nested_state, sink, outcome)
            }
            ProcAction::Describe {
                statement,
                kind,
                columns,
            } => match crate::prepared::describe_batch(self, &statement, kind) {
                Ok(rows) => {
                    self.run_nested_static(&columns, &rows, more, nested_state, sink, outcome)
                }
                Err(err) => {
                    self.finish_nested_procedure_error(err, more, nested_state, sink, outcome)
                }
            },
            ProcAction::Refuse(err) => {
                self.finish_nested_procedure_error(err, more, nested_state, sink, outcome)
            }
            ProcAction::Prepare { .. } => self.finish_nested_procedure_error(
                SqlError::procedure_not_found("sp_prepare"),
                more,
                nested_state,
                sink,
                outcome,
            ),
            ProcAction::Execute { .. } => self.finish_nested_procedure_error(
                SqlError::procedure_not_found("sp_execute"),
                more,
                nested_state,
                sink,
                outcome,
            ),
            ProcAction::Unprepare { .. } => self.finish_nested_procedure_error(
                SqlError::procedure_not_found("sp_unprepare"),
                more,
                nested_state,
                sink,
                outcome,
            ),
        }
    }

    fn run_nested_static(
        &mut self,
        columns: &[StaticResultColumn],
        rows: &[Vec<Value>],
        more: bool,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
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
        outcome.had_result_set = true;
        let reported = if nested_state.options.nocount {
            None
        } else {
            Some(rowcount)
        };
        sink.done_in_proc(reported, more)?;
        nested_state.rowcount = rowcount as i64;
        apply_exec_session(nested_state, self.exec(), true, true);
        self.exec_mut().identity_updated = false;
        Ok(Flow::Continue)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_nested_procedure_flow(
        &mut self,
        inner: NestedOutcome,
        wrapper: &NestedProcedureSink<'_>,
        more: bool,
        nested_state: &mut SessionState,
        outcome: &mut NestedOutcome,
        exec_args: &[EvaluatedExecArg],
        return_into: Option<&str>,
    ) -> SqlResult<Flow> {
        apply_nested_procedure_outputs(self, exec_args, return_into, &inner);
        if inner.failed {
            outcome.failed = true;
            nested_state.last_error = wrapper.last_error();
        } else {
            nested_state.last_error = 0;
        }
        if inner.had_result_set {
            outcome.had_result_set = true;
        }
        nested_state.rowcount = inner.rowcount;
        let exec = self.exec().clone();
        apply_exec_session(nested_state, &exec, true, !inner.failed);
        self.exec_mut().identity_updated = false;
        let continues = crate::procedures::continues_after_nested(
            more,
            nested_state.options.xact_abort,
            wrapper.last_error(),
        );
        Ok(if continues {
            Flow::Continue
        } else {
            Flow::Stop
        })
    }

    fn finish_nested_procedure_error(
        &mut self,
        err: SqlError,
        more: bool,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
        nested_state.last_error = err.number;
        outcome.failed = true;
        sink.error(&err)?;
        let continues = nested_procedure_continues(nested_state, more, true, err.number);
        if continues {
            sink.done_in_proc(None, true)?;
            Ok(Flow::Continue)
        } else {
            Ok(Flow::Stop)
        }
    }
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

fn coerce_unicode_literal_ty(ty: &TypeInfo, value: &Value) -> TypeInfo {
    if matches!(value, Value::String(_)) && matches!(ty.ty, SqlType::VarChar(_)) {
        TypeInfo::new(SqlType::NVarChar(Len::Max), ty.nullable)
    } else {
        ty.clone()
    }
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

fn apply_nested_procedure_outputs(
    session: &mut Session,
    exec_args: &[EvaluatedExecArg],
    return_into: Option<&str>,
    nested: &NestedOutcome,
) {
    if let Some(var) = return_into {
        session
            .exec_mut()
            .variables
            .insert(var.to_owned(), Value::I32(nested.return_status));
    }
    for arg in exec_args {
        if !arg.output {
            continue;
        }
        let Some(var) = arg.output_variable.as_ref() else {
            continue;
        };
        let key = arg.name.as_deref().unwrap_or(var);
        let value = nested
            .outputs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.clone())
            .or_else(|| session.exec().variables.get(key).cloned());
        if let Some(value) = value {
            session.exec_mut().variables.insert(var.clone(), value);
        }
    }
}

fn nested_procedure_continues(
    nested_state: &SessionState,
    more: bool,
    failed: bool,
    error_number: u32,
) -> bool {
    if !failed {
        return more;
    }
    more && !nested_state.options.xact_abort
        && error_number != 0
        && SqlError::new(error_number, 16, 1, "x").batch_scope() == BatchErrorScope::Statement
}

fn at_procedure_line(err: SqlError, line: u32) -> SqlError {
    if line == 0 { err } else { err.with_line(line) }
}

struct NestedProcedureSink<'a> {
    inner: &'a mut dyn ResultSink,
    more: bool,
    xact_abort: bool,
    last_error: u32,
    /// See `procedures.rs`, `TrackedSink::pending_error`.
    pending_error: bool,
}

impl<'a> NestedProcedureSink<'a> {
    fn new(inner: &'a mut dyn ResultSink, more: bool, xact_abort: bool) -> Self {
        Self {
            inner,
            more,
            xact_abort,
            last_error: 0,
            pending_error: false,
        }
    }

    fn last_error(&self) -> u32 {
        self.last_error
    }

    /// See `procedures.rs`, `TrackedSink::close_statement`.
    fn close_statement(&mut self) {
        if !self.pending_error {
            self.last_error = 0;
        }
        self.pending_error = false;
    }
}

impl ResultSink for NestedProcedureSink<'_> {
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
        let continues =
            crate::procedures::continues_after_nested(self.more, self.xact_abort, self.last_error);
        self.inner.done_in_proc(rowcount, continues)
    }

    fn info(&mut self, msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
        self.inner.info(msg)
    }

    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.last_error = err.number;
        self.pending_error = true;
        self.inner.error(err)
    }

    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.inner.env_change(change)
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.inner.return_value(name, ty, value)
    }

    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

fn seed_batch_variables(
    params: &[NestedParam],
    batch_variables: &mut BatchVariables,
) -> SqlResult<()> {
    for param in params {
        batch_variables.declare(&param.name, param.ty.clone())?;
    }
    Ok(())
}

fn seed_params(params: &[NestedParam], exec: &mut ExecSession) -> SqlResult<()> {
    for param in params {
        exec.variables
            .insert(param.name.clone(), param.value.clone());
        exec.variable_types
            .insert(param.name.clone(), param.ty.clone());
    }
    Ok(())
}

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

fn catalog_changed(prepared: &PreparedStatement, tail: &[Statement]) -> bool {
    match prepared {
        PreparedStatement::Bound {
            statement: PhysicalStatement::Ddl(DdlStatement::AlterTable { .. }),
            ..
        } => true,
        PreparedStatement::Bound {
            statement: PhysicalStatement::Ddl(DdlStatement::CreateTable { .. }),
            ..
        } => crate::batch::batch_has_alter_table(tail),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vauban_errors::{InfoMessage, SqlError, SqlResult};
    use vauban_storage::MemoryStorage;
    use vauban_tds::{ColumnMeta, EnvChange};
    use vauban_txn::IsolationLevel;
    use vauban_types::{TypeInfo, Value};

    use crate::state::SessionState;
    use crate::{Engine, ResultSink, Session};

    const SPID: i16 = 84;

    #[derive(Default)]
    struct Recording {
        rows: Vec<Vec<Value>>,
        errors: Vec<SqlError>,
    }

    impl ResultSink for Recording {
        fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
            Ok(())
        }
        fn row(&mut self, row: &[Value]) -> SqlResult<()> {
            self.rows.push(row.to_vec());
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

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
    }

    fn session(engine: &Arc<Engine>) -> Session {
        vauban_sysfn::register_builtins();
        Session::new(Arc::clone(engine), SessionState::new(SPID))
    }

    /// The body of `EXEC sp_executesql N'SELECT 1; CREATE TABLE t …; ALTER TABLE t ADD c int;'`
    /// creates the table and adds the column (`nested_select_create_alter_runs_all_statements`).
    #[test]
    fn nested_select_create_alter_runs_all_statements() {
        let engine = engine();
        let mut session = session(&engine);
        let mut sink = Recording::default();
        let outcome = session
            .run_nested(
                "SELECT 1; CREATE TABLE t (id int); ALTER TABLE t ADD c int;",
                &[],
                &mut sink,
            )
            .expect("nested completes");
        assert!(!outcome.failed, "{:?}", sink.errors);
        assert_eq!(
            sink.rows.first().and_then(|row| match row.first() {
                Some(Value::I32(n)) => Some(*n),
                _ => None,
            }),
            Some(1)
        );
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = engine.catalog.snapshot(&handle);
        let object = snapshot
            .resolve_object("master", Some("dbo"), "t", "dbo")
            .expect("the table was created");
        assert_eq!(
            snapshot.table(object.id).expect("table meta").columns.len(),
            2
        );
        engine.txn.commit(handle).expect("commit");
    }
}
