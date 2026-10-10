//! Nested batch execution: a T-SQL text with pre-declared parameters, its own variable
//! scope, and procedure-style DONE tokens ([MS-TDS] 2.2.7 DONEINPROC / DONEPROC).

use std::sync::Arc;

use vauban_binder::{BatchVariables, BindContext, BoundExpr, BoundStatement};
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{ErrorScope, InternalError, SqlError, SqlResult, error_behavior};
use vauban_executor::{
    Call, EnvironmentChange, EvaluatedExecArg, ExecContext, ExecOutcome, ExecSession, Host,
    Program, ProgramEnd, Ran, Site, StatementSink, eval_expr,
};
use vauban_parser::{Statement, parse_batch};
use vauban_planner::{PhysicalStatement, PlanContext, StorageIndexes};
use vauban_tds::{ColumnFlags, ColumnMeta, EnvChange};
use vauban_txn::IsolationLevel;
use vauban_types::{TypeInfo, Value};

use crate::batch::{DEFERRED_UNTIL_EXECUTION, Session, compiled_in_place, within_a_transaction};
use crate::eval_context::{SessionEvalContext, apply_exec_session};
use crate::fake_engine;
use crate::login::{DATABASE_CONTEXT_STATE_USE, changed_database_context};
use crate::procedures::{exec_args_to_proc_args, resolve_call_database};
use crate::rpc::{ProcAction, ProcArg, ProcParam, StaticResultColumn};
use crate::set_options::{SetOutcome, apply_set_statement, sync_exec_session};
use crate::sink::{ResultSink, StatementSinkAdapter};
use crate::state::SessionState;
use crate::statements::{
    INTERNAL_ERROR, at_statement, declare_variables_held_by, first_undeclared_goto,
    labels_at_any_depth, last_line, next_rowcount, statement_clears_last_error, statement_fragment,
    statement_line, statement_span, statement_text, use_name_line,
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
    /// Go on at the label of this name, which a `GOTO` of this text just named.
    Jump(String),
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
    /// A statement whose binding answered one of `DEFERRED_UNTIL_EXECUTION` while the text
    /// was prepared, bound again when the text reaches it (`batch.rs`,
    /// `PreparedStatement::Deferred`, for the rule; here it is what makes `EXEC('CREATE
    /// TABLE dbo.t (n int); INSERT dbo.t VALUES (1); SELECT n FROM dbo.t;')` answer a row).
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
        self.run_nested_in_database(text, params, None, sink)
    }

    /// [`run_nested`](Self::run_nested) with `database` as the current database of the text.
    ///
    /// The name is placed on the clone of the session state this call already runs on, so
    /// `DB_NAME()`, `DB_ID()` and the objects the text resolves are those of `database`
    /// while it runs, and the caller keeps the database it had when the call returns. No
    /// 5701 message accompanies the change, where a `USE` sends one
    /// (`compat/tests/catalog_procs.rs`,
    /// `a_qualified_call_leaves_the_caller_database_and_sends_no_context_change`). `None`
    /// runs the text on the caller's database.
    pub(crate) fn run_nested_in_database(
        &mut self,
        text: &str,
        params: &[NestedParam],
        database: Option<&str>,
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
        if let Some(database) = database {
            nested_state.database = database.to_owned();
        }
        let mut outcome = NestedOutcome {
            outputs: Vec::new(),
            return_status: 0,
            rowcount: 0,
            failed: false,
            had_result_set: false,
        };

        let result = self.run_nested_body(text, params, &mut nested_state, sink, &mut outcome);

        outcome.outputs = collect_outputs(params, self.exec());
        // `nested_state.rowcount` is the value each statement posts for `@@ROWCOUNT`
        // (`run_nested_prepared`, below); `self.exec().rowcount` is scratch the executor
        // resets ahead of a statement and a bare `SET` writes back to
        // (`vauban_executor::control`), so it does not carry a `SELECT`'s row count out of
        // this call (`tests/nested.rs`, `rowcount_reflects_the_last_row_count_of_the_nested_text`).
        outcome.rowcount = nested_state.rowcount;

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
        // The labels of a nested text are its own, and so is the 133 of a `GOTO` that
        // names none of them: it is raised before the first statement of the text runs,
        // and the caller reads it as the error of the call (`tests/run_batch_pipeline.rs`,
        // `a_goto_to_an_unknown_label_of_a_dynamic_text_is_133`). A label is read at any
        // depth, since the program that runs the text holds a position inside a block.
        let statements = batch.statements.as_slice();
        let labels = labels_at_any_depth(statements);
        if let Some(label) = first_undeclared_goto(statements, &labels) {
            let err = SqlError::goto_label_not_declared(label).with_line(last_line(text));
            return self.fail_nested(&err, sink, outcome);
        }

        let lazy = crate::batch::bind_limit_for(statements).is_some();
        let (mut slots, bind_state, batch_variables) = if lazy {
            let mut variables = BatchVariables::new();
            seed_batch_variables(params, &mut variables)?;
            let slots: Vec<Slot> = statements.iter().map(|_| Slot::Unbound).collect();
            (slots, nested_state.clone(), variables)
        } else {
            match self.prepare_nested(text, statements, params, nested_state) {
                Ok((prepared, state, variables)) => (
                    prepared
                        .into_iter()
                        .map(|prepared| Slot::Ready(Box::new(prepared)))
                        .collect(),
                    state,
                    variables,
                ),
                Err(err) if self.cancel_handle().is_cancelled() => return Err(err),
                Err(err) => return self.fail_nested(&err, sink, outcome),
            }
        };

        // The text runs as one program, as the batch does (`batch.rs`, "One program for the
        // batch"): the statements that hold others and the jumps are compiled into the
        // program, the others are run by [`NestedHost`] from their position.
        let mut in_program: Vec<Option<PhysicalStatement>> = Vec::with_capacity(slots.len());
        for (slot, statement) in slots.iter_mut().zip(statements) {
            let mut in_place = false;
            if let Slot::Ready(prepared) = &*slot
                && let PreparedStatement::Bound { statement, .. } = prepared.as_ref()
            {
                declare_variables_held_by(statement, self.exec_mut());
                in_place = compiled_in_place(statement);
            }
            let compiled = if in_place {
                match std::mem::replace(slot, Slot::InProgram) {
                    Slot::Ready(prepared) => match *prepared {
                        PreparedStatement::Bound { statement, .. } => Some(statement),
                        _ => None,
                    },
                    _ => None,
                }
            } else {
                match (&*slot, statement) {
                    (Slot::Unbound, Statement::Label { name, .. }) => {
                        *slot = Slot::InProgram;
                        Some(PhysicalStatement::Label {
                            name: name.value.clone(),
                        })
                    }
                    (Slot::Unbound, Statement::Goto { label, .. }) => {
                        *slot = Slot::InProgram;
                        Some(PhysicalStatement::Goto {
                            label: label.value.clone(),
                        })
                    }
                    _ => None,
                }
            };
            in_program.push(compiled);
        }
        let roots: Vec<Option<&PhysicalStatement>> =
            in_program.iter().map(Option::as_ref).collect();
        let program = match Program::compile_roots(&roots) {
            Ok(program) => program,
            Err(err) => {
                let err = if err.number == GOTO_LABEL_NOT_DECLARED {
                    err.with_line(last_line(text))
                } else {
                    err
                };
                return self.fail_nested(&err, sink, outcome);
            }
        };
        let bound = if lazy { 0 } else { slots.len() };
        let end = {
            let mut host = NestedHost {
                session: self,
                nested_state: &mut *nested_state,
                sink: &mut *sink,
                outcome: &mut *outcome,
                text,
                statements,
                params,
                slots,
                bound,
                bind_state,
                batch_variables,
            };
            program.run_hosted(&mut host)?
        };
        match end {
            ProgramEnd::Returned(code) => {
                outcome.return_status = code;
                // `RETURN` is the last statement that ran, and it counts no row: `@@ROWCOUNT`
                // leaves the text at 0 whatever the statement before it counted
                // (`tests/nested.rs`, `rowcount_is_zero_after_a_text_that_ends_with_return`).
                nested_state.rowcount = 0;
                Ok(())
            }
            ProgramEnd::Cancelled => Err(SqlError::from(InternalError::Bug(
                crate::cancel::CANCELLED.to_owned(),
            ))),
            ProgramEnd::Completed | ProgramEnd::Aborted(_) => Ok(()),
        }
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
                Err(error) if DEFERRED_UNTIL_EXECUTION.contains(&error.number) => {
                    PreparedStatement::Deferred {
                        padded,
                        raw: statement_text(text, original),
                        variables: batch_variables.clone(),
                    }
                }
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
                Err(err) => nested_failure(&err, more, nested_state, sink, outcome),
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
        self.run_nested_bound(bound, line, target, more, nested_state, sink, outcome)
    }

    /// Runs one bound statement of a nested text that holds no other one, written at the top
    /// level of the text on `line` or held by the statement written there.
    #[allow(clippy::too_many_arguments)]
    fn run_nested_bound(
        &mut self,
        bound: &PhysicalStatement,
        line: u32,
        target: Option<&str>,
        more: bool,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
        self.exec_mut().identity_updated = false;
        self.exec_mut().rowcount = 0;
        let mut adapter = StatementSinkAdapter::in_proc(sink);
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
                adapter.tds(),
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
                adapter.done(reported)?;
                nested_state.rowcount = count as i64;
                apply_exec_session(nested_state, self.exec(), true, true);
                self.exec_mut().identity_updated = false;
                Ok(Flow::Continue)
            }
            Ok(ExecOutcome::NoRows) => {
                if let Some(database) = target {
                    self.switch_nested_database(database, line, nested_state, &mut adapter)?;
                }
                adapter.done(None)?;
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
                // `RETURN` is the last statement that ran, and it counts no row: `@@ROWCOUNT`
                // leaves the text at 0 whatever the statement before it counted
                // (`tests/nested.rs`, `rowcount_is_zero_after_a_text_that_ends_with_return`).
                nested_state.rowcount = 0;
                Ok(Flow::Stop)
            }
            Ok(ExecOutcome::Cancelled) => Err(SqlError::from(InternalError::Bug(
                crate::cancel::CANCELLED.to_owned(),
            ))),
            Ok(ExecOutcome::CallProcedure {
                name,
                database,
                args,
                return_into,
                line: exec_line,
            }) => self.run_nested_procedure(
                &name,
                database.as_deref(),
                &args,
                return_into.as_deref(),
                exec_line,
                more,
                nested_state,
                adapter.tds(),
                outcome,
            ),
            Ok(ExecOutcome::RunDynamic {
                text,
                line: exec_line,
            }) => self.run_nested_dynamic(
                &text,
                exec_line,
                more,
                nested_state,
                adapter.tds(),
                outcome,
            ),
            // As in the top-level batch (`batch.rs`, `run_prepared`): the name travels
            // to the loop that holds the labels of this text.
            Ok(ExecOutcome::Goto(label)) => {
                adapter.done(None)?;
                nested_state.rowcount =
                    next_rowcount(bound, nested_state.rowcount, self.exec().rowcount);
                apply_exec_session(
                    nested_state,
                    self.exec(),
                    true,
                    statement_clears_last_error(bound),
                );
                self.exec_mut().identity_updated = false;
                Ok(Flow::Jump(label))
            }
            Ok(flow @ (ExecOutcome::Break | ExecOutcome::Continue)) => {
                let err = SqlError::from(InternalError::Bug(format!(
                    "run_nested_prepared: the executor answered {flow:?}, which this layer does \
                     not handle"
                )));
                self.fail_nested_statement(&err, adapter.tds(), outcome)
                    .map(|()| Flow::Stop)
            }
            Ok(ExecOutcome::BatchAbort(err)) => {
                let err = at_statement(err, line);
                nested_state.last_error = err.number;
                adapter.error(&err)?;
                outcome.failed = true;
                outcome.return_status = err.number as i32;
                nested_state.rowcount = 0;
                Ok(Flow::Stop)
            }
            Err(err) => {
                let err = at_statement(err, line);
                nested_state.last_error = err.number;
                adapter.error(&err)?;
                outcome.failed = true;
                nested_state.rowcount = 0;
                let continues = more
                    && !nested_state.options.xact_abort
                    && error_behavior(err.number).scope == ErrorScope::Statement;
                if continues {
                    // The rest of the text still runs: a `RETURN` or a later error, not this
                    // one, decides the status this call leaves
                    // (`tests/nested.rs`, `return_status_is_zero_after_a_statement_scoped_error_the_text_recovers_from`).
                    adapter.done(None)?;
                    Ok(Flow::Continue)
                } else {
                    outcome.return_status = err.number as i32;
                    Ok(Flow::Stop)
                }
            }
        }
    }

    fn execute_nested_in_a_transaction(
        &mut self,
        bound: &PhysicalStatement,
        nested_state: &SessionState,
        sink: &mut dyn StatementSink,
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
        sink: &mut dyn StatementSink,
    ) -> SqlResult<()> {
        let old = std::mem::replace(&mut nested_state.database, database.to_owned());
        sink.env_change(&EnvironmentChange::Database {
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
        outcome.return_status = err.number as i32;
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
        outcome.return_status = err.number as i32;
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
        qualifier: Option<&str>,
        exec_args: &[EvaluatedExecArg],
        return_into: Option<&str>,
        line: u32,
        more: bool,
        nested_state: &mut SessionState,
        sink: &mut dyn ResultSink,
        outcome: &mut NestedOutcome,
    ) -> SqlResult<Flow> {
        let database = match resolve_call_database(self, qualifier) {
            Ok(database) => database,
            Err(err) => {
                return self.finish_nested_procedure_error(
                    at_procedure_line(err, line),
                    more,
                    nested_state,
                    sink,
                    outcome,
                );
            }
        };
        let proc_args = exec_args_to_proc_args(name, exec_args);
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
        let Some(resolver) = crate::rpc::system_procedure_resolver(self) else {
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
                database.as_deref(),
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
        let mut wrapper = NestedProcedureSink::new(sink);
        let inner = self.run_nested(text, &[], &mut wrapper)?;
        self.finish_nested_procedure_flow(inner, &wrapper, more, nested_state, outcome, &[], None)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_nested_proc_action(
        &mut self,
        action: ProcAction,
        database: Option<&str>,
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
                let mut wrapper = NestedProcedureSink::new(sink);
                let inner = self.run_nested_in_database(
                    &statement,
                    &nested_params,
                    database,
                    &mut wrapper,
                )?;
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
                let mut wrapper = NestedProcedureSink::new(sink);
                let inner =
                    self.run_nested_in_database(&sql, &nested_params, database, &mut wrapper)?;
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
        if !continues {
            // A `RETURN` or a failure of the call this text made carries out to this text's
            // own status exactly when nothing after it still runs: a call that fails but
            // lets the rest of the text go on leaves the final status to what runs after it
            // instead (`tests/nested.rs`,
            // `return_status_of_a_two_level_call_is_zero_when_the_text_recovers_after_it`).
            outcome.return_status = inner.return_status;
        }
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

/// Hands `err`, raised by a statement of a nested text, to the sink, and says whether the
/// text goes on: the end of the statement goes out with it when it does, and the number
/// becomes the status of the text when it does not.
fn nested_failure(
    err: &SqlError,
    more: bool,
    nested_state: &mut SessionState,
    sink: &mut dyn ResultSink,
    outcome: &mut NestedOutcome,
) -> SqlResult<Flow> {
    nested_state.last_error = err.number;
    sink.error(err)?;
    outcome.failed = true;
    let continues = more
        && !nested_state.options.xact_abort
        && error_behavior(err.number).scope == ErrorScope::Statement;
    if continues {
        sink.done_in_proc(None, true)?;
        Ok(Flow::Continue)
    } else {
        outcome.return_status = err.number as i32;
        Ok(Flow::Stop)
    }
}

/// Number of the error a `GOTO` answers when no label of the text carries its name.
const GOTO_LABEL_NOT_DECLARED: u32 = 133;

/// What a nested text holds at one position (`batch.rs`, `Slot`, for the rule).
enum Slot {
    /// Not bound yet: a text that holds an `ALTER TABLE` binds each statement when the
    /// program reaches it.
    Unbound,
    /// Bound, planned and checked.
    Ready(Box<PreparedStatement>),
    /// Compiled into the program itself.
    InProgram,
    /// Taken out while the host runs it.
    Running,
}

/// The [`Host`] of the program of a nested text: each statement runs as the loop of this
/// file ran it, its end a DONEINPROC, and the program holds the control of flow.
struct NestedHost<'h> {
    session: &'h mut Session,
    nested_state: &'h mut SessionState,
    sink: &'h mut dyn ResultSink,
    outcome: &'h mut NestedOutcome,
    text: &'h str,
    statements: &'h [Statement],
    params: &'h [NestedParam],
    slots: Vec<Slot>,
    bound: usize,
    bind_state: SessionState,
    batch_variables: BatchVariables,
}

impl NestedHost<'_> {
    /// Whether a statement of the text follows the one written at `site`.
    fn more(&self, site: Site) -> bool {
        site.root + 1 < self.statements.len()
    }

    /// The line the statement written at `site` starts on.
    fn line_of(&self, site: Site) -> u32 {
        self.statements.get(site.root).map_or(0, statement_line)
    }

    /// What a [`Flow`] of this file is to the program.
    fn ran(flow: Flow) -> Ran {
        match flow {
            Flow::Continue => Ran::Outcome(ExecOutcome::NoRows),
            Flow::Jump(label) => Ran::Outcome(ExecOutcome::Goto(label)),
            Flow::Stop => Ran::Stopped(SqlError::from(InternalError::Bug(
                "the nested text stopped".to_owned(),
            ))),
        }
    }

    /// Binds the statements up to `index` that are not bound yet (`batch.rs`,
    /// `BatchHost::bind_up_to`). The inner `Err` is a binding error.
    fn bind_up_to(&mut self, index: usize) -> SqlResult<Result<(), SqlError>> {
        while self.bound <= index {
            let from = self.bound;
            let tail = match self.session.reprepare_nested_tail(
                self.text,
                &self.statements[from..],
                self.params,
                &mut self.bind_state,
                &mut self.batch_variables,
            ) {
                Ok(tail) => tail,
                Err(err) if self.session.cancel_handle().is_cancelled() => return Err(err),
                Err(err) => return Ok(Err(err)),
            };
            if tail.is_empty() {
                break;
            }
            for prepared in tail {
                if let PreparedStatement::Bound { statement, .. } = &prepared {
                    declare_variables_held_by(statement, self.session.exec_mut());
                }
                if let Some(slot) = self.slots.get_mut(self.bound)
                    && !matches!(slot, Slot::InProgram)
                {
                    *slot = Slot::Ready(Box::new(prepared));
                }
                self.bound += 1;
            }
        }
        Ok(Ok(()))
    }

    /// Runs one prepared statement written at the top level of the text.
    fn run_root(&mut self, prepared: &PreparedStatement, site: Site) -> SqlResult<Ran> {
        let more = self.more(site);
        match prepared {
            PreparedStatement::Bound { statement, .. } if compiled_in_place(statement) => {
                Ok(Ran::Holds(Box::new(statement.clone())))
            }
            PreparedStatement::Deferred {
                padded,
                raw,
                variables,
            } => match self
                .session
                .bind_nested_deferred(padded, raw, variables, self.nested_state)
            {
                Ok(PreparedStatement::Bound { statement, .. }) if compiled_in_place(&statement) => {
                    Ok(Ran::Holds(Box::new(statement)))
                }
                Ok(bound) => self.run_root(&bound, site),
                Err(err) if self.session.cancel_handle().is_cancelled() => Err(err),
                Err(err) => Ok(Self::ran(nested_failure(
                    &err,
                    more,
                    self.nested_state,
                    self.sink,
                    self.outcome,
                )?)),
            },
            prepared => Ok(Self::ran(self.session.run_nested_prepared(
                prepared,
                more,
                self.nested_state,
                self.sink,
                self.outcome,
            )?)),
        }
    }
}

impl Host for NestedHost<'_> {
    fn statement(&mut self, stmt: Option<&PhysicalStatement>, site: Site) -> SqlResult<Ran> {
        self.session.cancel_handle().check()?;
        if let Some(stmt) = stmt {
            // A statement held by a block, a branch or a loop body has the rest of the
            // program after it, the statement that holds it being last or not
            // (`tests/run_batch_pipeline.rs`, `a_dynamic_text_runs_its_calls_and_its_jumps_in_place`).
            let (line, more) = (self.line_of(site), true);
            let flow = self.session.run_nested_bound(
                stmt,
                line,
                None,
                more,
                self.nested_state,
                self.sink,
                self.outcome,
            )?;
            return Ok(Self::ran(flow));
        }
        let index = site.root;
        if let Err(err) = self.bind_up_to(index)? {
            let flow = nested_failure(&err, false, self.nested_state, self.sink, self.outcome)?;
            return Ok(Self::ran(flow));
        }
        let Some(slot) = self.slots.get_mut(index) else {
            return Err(SqlError::from(InternalError::Bug(format!(
                "nested text: no statement {index}"
            ))));
        };
        let slot = std::mem::replace(slot, Slot::Running);
        let ran = match &slot {
            Slot::Ready(prepared) => self.run_root(prepared, site),
            Slot::Unbound | Slot::InProgram | Slot::Running => Err(SqlError::from(
                InternalError::Bug(format!("nested text: statement {index} is not ready")),
            )),
        };
        self.slots[index] = slot;
        ran
    }

    fn condition(
        &mut self,
        condition: &BoundExpr,
        site: Site,
    ) -> SqlResult<Result<bool, SqlError>> {
        let mut value = None;
        let engine = Arc::clone(self.session.engine());
        let cancel = self.session.cancel_handle();
        let opened = within_a_transaction(
            self.nested_state,
            &engine,
            self.session.exec_mut(),
            &cancel,
            &mut |ctx: &mut ExecContext<'_>| {
                value = Some(eval_expr(condition, None, ctx));
                Ok(())
            },
            self.sink,
        );
        let evaluated = match (opened, value) {
            (Ok(()), Some(value)) => value,
            (Err(err), _) => Err(err),
            (Ok(()), None) => Err(SqlError::from(InternalError::Bug(
                "condition: nothing was evaluated".to_owned(),
            ))),
        };
        match evaluated {
            Ok(value) => {
                self.nested_state.rowcount = 0;
                self.nested_state.last_error = 0;
                Ok(Ok(matches!(value, Value::Bit(true))))
            }
            Err(err) => {
                let err = at_statement(err.with_line(0), self.line_of(site));
                self.report(&err, site)?;
                Ok(Err(err))
            }
        }
    }

    fn within(
        &mut self,
        _site: Site,
        step: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<()>,
    ) -> SqlResult<()> {
        let engine = Arc::clone(self.session.engine());
        let cancel = self.session.cancel_handle();
        within_a_transaction(
            self.nested_state,
            &engine,
            self.session.exec_mut(),
            &cancel,
            step,
            self.sink,
        )
    }

    fn report(&mut self, err: &SqlError, site: Site) -> SqlResult<()> {
        let err = at_statement(err.clone(), self.line_of(site));
        self.nested_state.rowcount = 0;
        let continues = !self.nested_state.options.xact_abort
            && err.number != 0
            && error_behavior(err.number).scope == ErrorScope::Statement;
        nested_failure(&err, continues, self.nested_state, self.sink, self.outcome)?;
        Ok(())
    }

    // A statement of a nested text makes its calls itself (`run_nested_bound`): no call
    // reaches the program.
    fn call(&mut self, _call: Call, _site: Site) -> SqlResult<ProgramEnd> {
        Err(SqlError::from(InternalError::Bug(
            "nested text: a call reached the program".to_owned(),
        )))
    }

    fn leave_call(&mut self, _database: &str) -> SqlResult<()> {
        Ok(())
    }

    fn control(&mut self, _site: Site) -> SqlResult<()> {
        self.sink.done_in_proc(None, true)
    }

    fn cancelled(&self) -> bool {
        self.session.cancel_handle().is_cancelled()
    }

    fn xact_abort(&self) -> bool {
        self.nested_state.options.xact_abort
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
        && error_behavior(error_number).scope == ErrorScope::Statement
}

fn at_procedure_line(err: SqlError, line: u32) -> SqlError {
    if line == 0 { err } else { err.with_line(line) }
}

/// Forwards the tokens of a call made from inside another call: its closing DONEPROC
/// becomes a DONEINPROC, since the call it closes is not the outermost one of the request.
struct NestedProcedureSink<'a> {
    inner: &'a mut dyn ResultSink,
    last_error: u32,
    /// See `procedures.rs`, `TrackedSink::pending_error`.
    pending_error: bool,
}

impl<'a> NestedProcedureSink<'a> {
    fn new(inner: &'a mut dyn ResultSink) -> Self {
        Self {
            inner,
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

    /// The call that made this one still has its own closing token to send, so another
    /// DONE follows this one in the response and this one carries `more`: a client that
    /// stops reading at the first DONE without `more` goes on reading the caller. It
    /// carries `more` for a text that has another statement to run as for one that stops
    /// right there, and for a call that ended on an error as for one that ended on a
    /// result set (`tests/nested.rs`,
    /// `the_token_that_closes_a_two_level_call_carries_more`,
    /// `the_token_that_closes_a_call_at_the_end_of_a_batch_carries_more`,
    /// `the_tokens_that_close_a_three_level_call_carry_more` and
    /// `the_token_that_closes_a_failed_two_level_call_carries_more`).
    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.inner.done_in_proc(rowcount, true)
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
