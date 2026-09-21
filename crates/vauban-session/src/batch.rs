//! `Session`: batch and RPC execution on the blocking pool, one DONE per statement.
//!
//! Everything here is synchronous: the connection task moves the `Session` into a
//! `spawn_blocking` closure, runs one request, and takes it back.
//!
//! # The chain
//!
//! `run_batch` prepares each statement through `parser::parse_batch` -> `binder::bind` ->
//! `planner::plan` -> `executor::compile` before executing the first one, then turns
//! execution into `ResultSink` calls: the executor collects the rows of a statement and
//! this layer sends them, until [`ResultSink`] implements `executor::RowSink` itself. A
//! statement the binder refuses with 208 is the exception: it is kept as text and goes
//! through the same three steps when the batch reaches it (`PreparedStatement::Deferred`,
//! `Session::run_deferred`).
//!
//! # What is left of the fake engine
//!
//! `fake_engine` no longer answers anything the chain can serve; the single statement it
//! still holds is `WAITFOR DELAY`, which the ATTENTION handling needs as an interruptible
//! request and which the binder does not bind. The fall-back is therefore narrow and
//! explicit: it happens on a **binding** error numbered 50000 (`InternalError::Bug`, that is
//! "this statement is not implemented"), never on an execution error: the cancellation of
//! a request is an internal error too (`cancel.rs`), and re-running a cancelled statement
//! through the fake engine would undo the ATTENTION (`tests/run_batch_pipeline.rs`,
//! `a_cancelled_batch_sends_nothing_and_fails`).
//!
//! # Where compilation ends and execution begins
//!
//! An error raised while the batch is **compiled** kills the whole batch before its first
//! statement runs, while an error raised at **run time** lets everything before it through
//! and, for some numbers, lets what follows it run too. This is what SQL Server does.
//! `tests/run_batch_pipeline.rs` pins each shape with one more statement after the failing
//! one, since a batch whose last statement is the failing one cannot tell "the batch stops"
//! from "the batch goes on":
//!
//! | batch | result |
//! |---|---|
//! | `SELECT 1; SELEC 1; SELECT 2;` | **no** result set, then 102 |
//! | `SELECT 1; SELECT NO_SUCH_FN(1); SELECT 2;` | **no** result set, then 195 |
//! | `SELECT 1; SELECT "a"; SELECT 2;` | **no** result set, then 207 |
//! | `SELECT 1; SELECT 1 WHERE 1; SELECT 2;` | **no** result set, then 4145 |
//! | `SELECT 1; SELECT TOP (-1) 1; SELECT 2;` | **no** result set, then 127 |
//! | `SELECT 1; SELECT 1 / 0; SELECT 2;` | one row, an empty result set, 8134, **then `2`** |
//! | `SELECT 1; SELECT CAST(300 AS tinyint); SELECT 2;` | one row, an empty result set, 220, **then `2`** |
//! | `SELECT 1; SELECT CAST('abc' AS int); SELECT 2;` | one row, an empty result set, then 245, and **no** `2` |
//! | `SELECT 1; SELECT * FROM dbo.t; CREATE TABLE dbo.t (a int); SELECT 2;` | one row, **no** result set, then 208, and **no** `2` |
//! | `SELECT 1; CREATE INDEX ix ON dbo.nosuchtable (a); SELECT 2;` | one row, **no** result set, then 1088, and **no** `2` |
//! | `SELECT 1; ALTER TABLE dbo.nosuchtable ADD b int; SELECT 2;` | one row, **no** result set, then 4902, and **no** `2` |
//! | `CREATE TABLE dbo.t (a int); INSERT dbo.t VALUES (1); SELECT * FROM dbo.t;` | one row, no error |
//!
//! The three frontiers, and the numbers that cross them:
//!
//! - a **syntax** error kills the batch, because `parse_batch` reads the whole text before
//!   the first statement runs (`syntax_error_stops_the_batch`);
//! - a **binding or compile-time** error kills the batch too: `prepare_batch` binds and checks
//!   each statement before execution emits any row
//!   (`a_compilation_error_silences_the_whole_batch`);
//! - **208, 1088 and 4902** are binding errors the batch does not stop on: a table the
//!   catalogue does not hold may be created by a statement before the one that names it, in
//!   this batch or in the nested batch of an `EXEC`. The statement is deferred and bound
//!   again when the batch reaches it; whichever of the three still holds then is a run-time
//!   error of that statement, after the results of the ones before it, and batch-scoped
//!   (`a_select_before_the_create_is_208_at_run_time`,
//!   `a_table_created_by_an_exec_is_read_by_the_rest_of_the_batch`,
//!   `a_create_index_on_a_table_still_missing_is_1088_at_run_time`,
//!   `an_alter_table_on_a_table_still_missing_is_4902_at_run_time`). A deferred statement
//!   whose table exists by then is checked like the rest, so a 207 it raises is a run-time
//!   error too (`a_deferred_statement_raises_207_when_it_runs`);
//! - a **run-time** error stops its statement. Its catalogued [`BatchErrorScope`] then
//!   decides whether the following statement runs. This is not a severity/state rule: 8134
//!   and 245 are both severity 16, state 1, but 8134 alone lets the batch continue
//!   (`a_statement_scoped_runtime_error_lets_the_batch_continue`,
//!   `a_batch_scoped_runtime_error_stops_the_batch`). With `XACT_ABORT ON`, a run-time
//!   error of either scope stops the batch
//!   (`xact_abort_stops_a_statement_scoped_runtime_error`).
//!
//! # Where the column metadata of a statement goes out
//!
//! **Between compilation and execution**, which is what decides whether a failing
//! statement shows the client a result set at all. `prepare_batch` calls
//! `executor::compile` for the statements it binds, deferred ones excepted; once they have
//! succeeded, `run_prepared` sends COLMETADATA and calls `executor::execute`. A compilation
//! error reaches the client with **no** result set (`a_bind_error_sends_no_metadata_at_all`),
//! a run-time error with a result set that carries the columns and no row. A deferred
//! statement whose binding fails when it runs has no columns to send, so its error comes
//! with no result set either, after the result sets of the statements before it.
//!
//! The boundary between the two families lives next to the code that implements it, in
//! the module header of `vauban_executor::compile`. This layer extends that per-statement
//! boundary to the whole batch: a compilation error also silences each statement
//! **before** the failing one (`a_compilation_error_silences_the_whole_batch`).
//!
//! Preparation uses a cloned session state: each `SET` advances that clone before the
//! following statement is reparsed and bound, while live state changes during
//! execution alone. `SET QUOTED_IDENTIFIER OFF; SELECT "a"; SET QUOTED_IDENTIFIER ON; SELECT
//! 'b';` returns `a` then `b` (`quoted_identifier_takes_effect_in_its_own_batch_both_ways`).
//!
//! # The line of a run-time error is the **statement**'s, and this is where it is known
//!
//! The line is the one the failing **statement** starts on, not the line of the
//! sub-expression that raised, as on SQL Server. `executor` cannot know it: a line lives
//! on `binder::BoundExpr` and there alone, and `PhysicalStatement::Query` holds a bare
//! `PhysicalPlan`. So it answers the line of the failing sub-expression, and `run_prepared`
//! puts the statement's over it. This is the layer that holds the datum: the `&Statement`
//! and its `Span`.
//!
//! The rule holds across error families (8134, 245, 220, 8115, 536:
//! `the_line_rule_holds_across_error_families`), for a statement that begins on the line
//! the previous one ends on (`a_statement_that_starts_on_the_line_the_previous_one_ends_on`)
//! and for a statement spread over more than three lines
//! (`a_statement_over_more_than_three_lines_still_answers_its_first_line`).
//!
//! Where the statement starts is where its first **token** is, not where its leading trivia
//! is: `SELECT 1;` / `-- a comment` / `SELECT` / `1 / 0;` answers the line of the second
//! `SELECT`, and so do the same batch with a blank line and the one with `/* c */` between
//! the two statements (`the_statement_starts_where_its_first_token_is`). That is exactly
//! what `Span::line` of the parser holds, comments being trivia the token stream drops.
//!
//! # Two numbers, and these two alone, keep the line the executor gave them
//!
//! 127 (`TOP` row count below zero) and 1060 (`TOP` row count `NULL`) are counted on the
//! **row-count expression** of the `TOP`, not on the statement, as on SQL Server: `SELECT`
//! / `TOP (-1)` / `1;` answers line 2 of the batch, and so does `SELECT TOP` / `(-1)` /
//! `1;`, the expression's line and not the `TOP` keyword's. That is exactly the line
//! `executor` already puts on them (`plan.rs`, `eval_budget`, `top.expr.line`), so these
//! two are left alone here (`the_two_top_row_count_errors_keep_the_line_of_their_clause`).
//!
//! **This is a list of two numbers, not a class of errors.** Other errors of the same `TOP`
//! clause land on other nodes on SQL Server: 1062 (`WITH TIES` without `ORDER BY`) on the
//! last line of the statement, 1031 (`PERCENT` out of range) on the line the select list
//! starts on. Neither is implemented here; whoever implements them has to place the node
//! its own check hangs on, since "a compilation error names its clause" would put both on
//! the line of the `TOP` and be wrong twice.
//!
//! Nor does anything about the way these errors are raised separate them from the ones that
//! *do* follow the statement. Severity does not: 207 is severity **16** and still answers
//! the line of its node. The absence of a result set does not either: 536 sends no result
//! set and answers the line of its statement.
//!
//! The internal error 50000 is left alone too, and for the opposite reason: it names a hole
//! in this engine rather than a place in the client's query, so it carries no line at all
//! (`executor`, `errors.rs`; `an_internal_error_raised_at_run_time_still_carries_no_line`).

use std::sync::Arc;

use crate::cancel::CancelHandle;
use crate::eval_context::{SessionEvalContext, apply_exec_session};
use crate::fake_engine;
use crate::login::{DATABASE_CONTEXT_STATE_USE, changed_database_context};
use crate::server::Engine;
use crate::set_options::{SetOutcome, apply_set_statement};
use crate::sink::{ResultSink, RowSinkAdapter};
use crate::state::SessionState;
use crate::statements::{
    INTERNAL_ERROR, INVALID_OBJECT_NAME, at_statement, next_rowcount, statement_clears_last_error,
    statement_fragment, statement_line, statement_span, statement_text, use_name_line,
};
use crate::txn_session::{self, StatementTxn};
use vauban_binder::{
    BatchVariables, BindContext, BoundExecute, BoundStatement, DdlStatement, OutputSchema,
};
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{BatchErrorScope, InternalError, SqlError, SqlResult};
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, RowSink};
use vauban_parser::{Statement, parse_batch};
use vauban_planner::{PhysicalStatement, PlanContext, StorageIndexes};
use vauban_tds::{ColumnFlags, ColumnMeta, EnvChange};
use vauban_txn::IsolationLevel;

/// Default schema of a login, `dbo` until `catalog` knows better.
const DEFAULT_SCHEMA: &str = "dbo";

/// Number of the error a `CREATE INDEX` answers when its table does not resolve
/// (`errors`, `SqlError::cannot_find_object_to_index`). Deferred the same way as
/// [`INVALID_OBJECT_NAME`].
const CREATE_INDEX_MISSING_TABLE: u32 = 1088;

/// Number of the error an `ALTER TABLE` answers when its table does not resolve
/// (`errors`, `SqlError::cannot_find_object_to_alter_table`). Deferred the same way as
/// [`INVALID_OBJECT_NAME`].
const ALTER_TABLE_MISSING_TABLE: u32 = 4902;

/// The numbers a batch defers instead of treating as a compilation error: a table the
/// catalogue does not hold yet may be created by an earlier statement of the batch, in
/// this batch or in the nested one of an `EXEC`. Shared with `nested.rs`.
pub(crate) const DEFERRED_UNTIL_EXECUTION: [u32; 3] = [
    INVALID_OBJECT_NAME,
    CREATE_INDEX_MISSING_TABLE,
    ALTER_TABLE_MISSING_TABLE,
];

/// The state and the execution of one connection after its login.
pub struct Session {
    state: SessionState,
    /// Storage, transaction manager and catalogue, shared by the connections of one
    /// server. Read by
    /// [`Session::prepare_batch`], which snapshots the catalogue for the binder, and by
    /// [`Session::execute_in_a_transaction`], which opens the transaction of one statement.
    engine: Arc<Engine>,
    cancel: CancelHandle,
    /// The executor's session state: the open transaction, its savepoints, the variables and
    /// `@@ROWCOUNT`. Held here so the transaction a `BEGIN TRANSACTION` opened outlives the
    /// statement that opened it and the batch that contained it (`txn_session.rs`).
    exec: ExecSession,
}

impl Drop for Session {
    /// Rolls back a transaction the connection left open and gives locks back through
    /// [`crate::disconnect::release`].
    fn drop(&mut self) {
        let _ = crate::disconnect::release(self, crate::disconnect::Release::Disconnect);
    }
}

impl Session {
    /// Shared engine of the connection.
    pub(crate) fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Mutable session state.
    pub(crate) fn state_mut(&mut self) -> &mut SessionState {
        &mut self.state
    }

    /// Executor session state shared across statements.
    pub(crate) fn exec(&self) -> &ExecSession {
        &self.exec
    }

    /// Mutable executor session state.
    pub(crate) fn exec_mut(&mut self) -> &mut ExecSession {
        &mut self.exec
    }
}

/// What one statement of a batch decided about the rest of it.
pub(crate) enum Flow {
    /// Go on with the next statement.
    Continue,
    /// Stop the batch here; the DONE has been sent.
    Stop,
}

/// One statement after every compilation-stage check has succeeded for the whole batch.
enum PreparedStatement {
    /// A `SET` is replayed only during execution; preparation applies it to a cloned state.
    Set {
        text: String,
        /// Line the `SET` starts on, which a `SET` that is refused carries
        /// (`tests/set_options_effects.rs`, `identity_insert_8107_carries_the_line_of_its_statement`).
        line: u32,
    },
    /// A statement the binder does not bind, still served by the deliberately narrow fallback.
    Fallback { text: String, error: SqlError },
    /// A bound, planned and compile-checked statement, ready to execute without another
    /// compilation pass.
    Bound {
        statement: PhysicalStatement,
        line: u32,
    },
    /// A `USE` whose target was **found** in the catalogue while the batch was bound: a
    /// name that is not there raises 911 at that moment and no statement of the batch
    /// runs, which is where SQL Server raises it too ([`Session::bind_batch`]).
    Use {
        statement: PhysicalStatement,
        /// Name of the target as the **catalogue** spells it, which is the one the
        /// ENVCHANGE and the INFO 5701 carry (unit test
        /// `use_existing_database_changes_state_and_sends_5701`).
        database: String,
        line: u32,
    },
    /// A bound `EXECUTE`, evaluated at run time without going through the planner.
    Execute { stmt: BoundExecute, line: u32 },
    /// A statement whose binding answered one of [`DEFERRED_UNTIL_EXECUTION`] while the
    /// batch was prepared: the table it names may be created by a statement before it, in
    /// this batch or in a nested one, so it is bound, planned and compile-checked again
    /// when the batch reaches it, against the catalogue of that moment
    /// ([`Session::run_deferred`]).
    Deferred {
        /// The fragment of the statement, padded with newlines up to its line of the batch,
        /// the text [`Session::bind_one`] reparses.
        padded: String,
        /// The text of the statement as the client wrote it.
        raw: String,
        /// The batch variables declared before the statement, and those alone.
        variables: BatchVariables,
    },
}

impl Session {
    /// Wraps the state built at login with the shared engine.
    pub fn new(engine: Arc<Engine>, state: SessionState) -> Self {
        Self {
            state,
            engine,
            cancel: CancelHandle::new(),
            exec: ExecSession::default(),
        }
    }

    /// The state of the session (SPID, database, `SET` options…).
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Replaces [`SessionState`] without reconstructing the wrapper.
    ///
    /// [`Session::new`] runs [`Drop`] on the old value, which rolls back an open transaction;
    /// the TRANSACTION_MANAGER path uses this after `txn_request::handle` instead.
    pub(crate) fn replace_state(&mut self, state: SessionState) {
        self.state = state;
    }

    /// A handle that cancels the running request (ATTENTION).
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// Runs a SQL batch ([MS-TDS] 2.2.6.7): one DONE per statement, in order, `more` set on
    /// every DONE but the last.
    ///
    /// Contract: every error destined to the client goes through `sink.error(..)` and the
    /// method returns `Ok(())`. An `Err(e)` is an **internal** error (channel closed,
    /// cancelled request) and means the response is incomplete; the connection task
    /// (`server.rs`) is what turns it into a DONE `ATTN` or drops the connection.
    pub fn run_batch(&mut self, text: &str, sink: &mut dyn ResultSink) -> SqlResult<()> {
        // Batch variables do not survive the batch: each text starts with an empty scope.
        self.exec.variables.clear();
        self.exec.variable_types.clear();
        let batch = match parse_batch(text, &self.state.options.parse_options()) {
            Ok(batch) => batch,
            // A syntax error is a compilation error: SQL Server compiles no batch partly,
            // so not one statement of this text runs. One ERROR, one DONE.
            Err(err) => return self.fail(&err, sink),
        };
        if batch.statements.is_empty() {
            // A response always ends with a DONE, even for a batch with no statement
            // (blank text, comments only): without it the client would wait forever.
            return sink.done(None, false);
        }
        let (mut prepared, mut bind_state, mut batch_variables) =
            match self.prepare_batch(text, &batch.statements) {
                Ok(prepared) => prepared,
                Err(err) if self.cancel.is_cancelled() => return Err(err),
                Err(err) => return self.fail(&err, sink),
            };
        let mut index = 0;
        while index < prepared.len() {
            // Between two statements, as `fake_engine` did before its own work: an
            // ATTENTION that arrived during the previous one stops the batch here.
            self.cancel.check()?;
            let more = index + 1 < prepared.len();
            let current = &prepared[index];
            if let Flow::Stop = self.run_prepared(current, more, sink)? {
                break;
            }
            let tail_start = index + 1;
            if should_reprepare_tail(
                tail_start,
                prepared.len(),
                batch.statements.len(),
                catalog_changed(current, &batch.statements[tail_start..]),
            ) {
                let tail = self.reprepare_tail(
                    text,
                    &batch.statements[tail_start..],
                    &mut bind_state,
                    &mut batch_variables,
                )?;
                prepared.truncate(tail_start);
                prepared.extend(tail);
            }
            index += 1;
        }
        // A batch may end with a transaction still open; what the end of a batch does with
        // it is `txn_session::end_of_batch`'s decision.
        txn_session::end_of_batch(&self.state, sink)?;
        Ok(())
    }

    /// Parses, binds and compile-checks every statement without changing live session state.
    fn prepare_batch(
        &self,
        text: &str,
        statements: &[Statement],
    ) -> SqlResult<(Vec<PreparedStatement>, SessionState, BatchVariables)> {
        let mut state = self.state.clone();
        let mut batch_variables = BatchVariables::new();
        let binding = self.engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.engine.catalog.snapshot(&binding);
        let prepared = self.bind_batch(
            text,
            statements,
            &snapshot,
            &mut state,
            &mut batch_variables,
            bind_limit_for(statements),
        );
        let closed = self.engine.txn.commit(binding);
        match (prepared, closed) {
            (Ok(prepared), Ok(())) => Ok((prepared, state, batch_variables)),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    /// Re-binds the tail of a batch after a catalogue-changing statement ran.
    fn reprepare_tail(
        &self,
        text: &str,
        statements: &[Statement],
        state: &mut SessionState,
        batch_variables: &mut BatchVariables,
    ) -> SqlResult<Vec<PreparedStatement>> {
        let binding = self.engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.engine.catalog.snapshot(&binding);
        let prepared = self.bind_batch(
            text,
            statements,
            &snapshot,
            state,
            batch_variables,
            bind_limit_for(statements),
        );
        self.engine.txn.commit(binding)?;
        prepared
    }

    fn bind_batch(
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
            self.cancel.check()?;
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
            let one = match self.bind_one(&padded, raw, snapshot, state, batch_variables) {
                Ok(one) => one,
                // A table the catalogue does not hold yet is not a compilation error: the
                // statement is kept as text and bound when the batch reaches it.
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

    /// Reparses, binds, plans and compile-checks one statement against `snapshot`.
    ///
    /// `padded` is the fragment of the statement preceded by as many newlines as it takes
    /// to put it on its line of the batch, so an error of the binder carries that line.
    /// A `USE` moves `state.database` and a `DECLARE` enters its variables, which is what
    /// the statements bound after this one read.
    fn bind_one(
        &self,
        padded: &str,
        raw: String,
        snapshot: &CatalogSnapshot,
        state: &mut SessionState,
        batch_variables: &mut BatchVariables,
    ) -> SqlResult<PreparedStatement> {
        let reparsed = parse_batch(padded, &state.options.parse_options())?;
        let [statement] = reparsed.statements.as_slice() else {
            return Err(SqlError::from(vauban_errors::InternalError::Bug(
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
        if let BoundStatement::Execute(execute) = bound {
            return Ok(PreparedStatement::Execute {
                stmt: execute,
                line,
            });
        }

        let indexes = StorageIndexes(self.engine.storage.as_ref());
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

    /// Binds a deferred statement against the catalogue as it stands when the batch
    /// reaches it, then runs it like the statement it turned out to be.
    ///
    /// The catalogue is read through the session transaction when one is open, so a
    /// `CREATE TABLE` of that transaction is seen before its `COMMIT`
    /// (`a_deferred_statement_reads_the_catalogue_of_its_own_transaction`), and through a
    /// transaction of its own otherwise, closed before the statement runs. The live
    /// session state is the one bound against: the `SET` options before the statement
    /// have been applied to it by then.
    ///
    /// A binding that fails now is a run-time error of the statement: what came before it
    /// has produced its results, and the catalogued [`BatchErrorScope`] of the number
    /// decides whether the batch goes on. 208, 207, 1088 and 4902 are batch-scoped, so
    /// `SELECT 1; SELECT * FROM dbo.t; CREATE TABLE dbo.t (a int); SELECT 2;` answers the
    /// row of the first `SELECT`, then 208 on the line of the second, and no `2`
    /// (`tests/run_batch_pipeline.rs`, `a_select_before_the_create_is_208_at_run_time`).
    fn run_deferred(
        &mut self,
        padded: &str,
        raw: &str,
        variables: &BatchVariables,
        more: bool,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<Flow> {
        match self.bind_deferred(padded, raw, variables) {
            Ok(prepared) => self.run_prepared(&prepared, more, sink),
            Err(err) if self.cancel.is_cancelled() => Err(err),
            Err(err) => {
                self.state.last_error = err.number;
                sink.error(&err)?;
                let continues = more
                    && !self.state.options.xact_abort
                    && err.batch_scope() == BatchErrorScope::Statement;
                sink.done(None, continues)?;
                Ok(if continues {
                    Flow::Continue
                } else {
                    Flow::Stop
                })
            }
        }
    }

    /// The binding half of [`Session::run_deferred`]: a snapshot of the catalogue of this
    /// moment, a clone of the live state and of the variables the statement can see.
    fn bind_deferred(
        &self,
        padded: &str,
        raw: &str,
        variables: &BatchVariables,
    ) -> SqlResult<PreparedStatement> {
        let mut state = self.state.clone();
        let mut variables = variables.clone();
        if let Some(session_txn) = &self.state.txn {
            let snapshot = self.engine.catalog.snapshot(&session_txn.handle);
            return self.bind_one(
                padded,
                raw.to_owned(),
                &snapshot,
                &mut state,
                &mut variables,
            );
        }
        let binding = self.engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.engine.catalog.snapshot(&binding);
        let prepared = self.bind_one(
            padded,
            raw.to_owned(),
            &snapshot,
            &mut state,
            &mut variables,
        );
        self.engine.txn.commit(binding)?;
        prepared
    }

    /// Executes one already compiled statement and says whether the batch goes on.
    fn run_prepared(
        &mut self,
        prepared: &PreparedStatement,
        more: bool,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<Flow> {
        if let PreparedStatement::Set { text, line } = prepared {
            let snapshot = if let Some(session_txn) = &self.state.txn {
                self.engine.catalog.snapshot(&session_txn.handle)
            } else {
                let binding = crate::set_options::begin_autocommit_txn(
                    &self.state,
                    &self.engine.storage,
                    &self.engine.txn,
                )?;
                let snap = self.engine.catalog.snapshot(&binding);
                self.engine.txn.commit(binding)?;
                snap
            };
            match apply_set_statement(&mut self.state, text, Some(&snapshot)) {
                SetOutcome::Applied | SetOutcome::Ignored(_) => {}
                SetOutcome::Failed(err) => {
                    let err = at_statement(err, *line);
                    return self.fail(&err, sink).map(|()| Flow::Stop);
                }
            }
            // A `SET` carries no row count, under both values of `NOCOUNT`: its DONE is
            // `0x0001` (MORE) before a statement and `0x0000` last, without `DONE_COUNT`.
            sink.done(None, more)?;
            // A `SET` puts `@@ROWCOUNT` back to 0: `SELECT 1; SET NOCOUNT OFF; SELECT
            // @@ROWCOUNT;` answers 1 then **0**, where `SELECT 1; SELECT @@ROWCOUNT;`
            // answers 1 then 1, the vector that tells "a `SET` resets the count" from "a
            // `SET` leaves it alone" (`tests/run_batch_pipeline.rs`,
            // `a_set_puts_the_row_count_back_to_zero`).
            self.state.rowcount = 0;
            apply_exec_session(&mut self.state, &self.exec, true, true);
            self.exec.identity_updated = false;
            return Ok(Flow::Continue);
        }

        if let PreparedStatement::Fallback { text, error } = prepared {
            return self.fall_back(text, error, more, sink);
        }

        if let PreparedStatement::Deferred {
            padded,
            raw,
            variables,
        } = prepared
        {
            return self.run_deferred(padded, raw, variables, more, sink);
        }

        if let PreparedStatement::Execute { stmt, line } = prepared {
            self.exec.identity_updated = false;
            self.exec.rowcount = 0;
            let mut adapter = RowSinkAdapter::new(sink);
            let (result, txn) = self.execute_bound_in_a_transaction(stmt, &mut adapter);
            let succeeded = result.is_ok();
            txn_session::finish_statement(
                &mut self.state,
                &self.engine,
                &mut self.exec,
                txn,
                txn_session::TxnKind::Other,
                succeeded,
                sink,
            )?;
            return match result {
                Ok(ExecOutcome::CallProcedure {
                    name,
                    args,
                    return_into,
                    line: exec_line,
                }) => {
                    let (flow, outcome) = crate::procedures::run_batch_procedure(
                        self,
                        &name,
                        &args,
                        return_into.as_deref(),
                        exec_line,
                        more,
                        sink,
                    )?;
                    if !outcome.failed {
                        self.state.rowcount = outcome.rowcount;
                    }
                    Ok(flow)
                }
                Ok(ExecOutcome::RunDynamic {
                    text,
                    line: exec_line,
                }) => {
                    let (flow, outcome) =
                        crate::procedures::run_batch_dynamic(self, &text, exec_line, more, sink)?;
                    if !outcome.failed {
                        self.state.rowcount = outcome.rowcount;
                    }
                    Ok(flow)
                }
                Err(err) => {
                    let err = at_statement(err, *line);
                    self.state.last_error = err.number;
                    sink.error(&err)?;
                    let continues = more
                        && !self.state.options.xact_abort
                        && err.number != 0
                        && err.batch_scope() == BatchErrorScope::Statement;
                    sink.done(None, continues)?;
                    self.state.rowcount = self.exec.rowcount;
                    apply_exec_session(&mut self.state, &self.exec, true, false);
                    self.exec.identity_updated = false;
                    Ok(if continues {
                        Flow::Continue
                    } else {
                        Flow::Stop
                    })
                }
                Ok(outcome) => {
                    let err = SqlError::from(InternalError::Bug(format!(
                        "run_prepared: execute_bound answered {outcome:?}, which this layer does \
                         not handle"
                    )));
                    self.fail(&err, sink).map(|()| Flow::Stop)
                }
            };
        }

        // A `USE` is a `Bound` statement with one thing more: the name its ENVCHANGE and its
        // INFO 5701 carry, resolved against the catalogue while the batch was bound.
        let (bound, line, target) = match prepared {
            PreparedStatement::Bound { statement, line } => (statement, *line, None),
            PreparedStatement::Use {
                statement,
                database,
                line,
            } => (statement, *line, Some(database.as_str())),
            PreparedStatement::Set { .. }
            | PreparedStatement::Fallback { .. }
            | PreparedStatement::Deferred { .. }
            | PreparedStatement::Execute { .. } => {
                unreachable!("SET, fallback, deferred and EXECUTE were handled above")
            }
        };

        // The adapter wraps the caller's sink and counts rows for the DONE. The
        // executor calls `RowSink::columns` during execution, which the adapter
        // converts to `ResultSink::columns` — COLMETADATA therefore goes out when
        // the statement starts to run, not while it is prepared.
        self.exec.identity_updated = false;
        self.exec.rowcount = 0;
        let mut adapter = RowSinkAdapter::new(sink);
        let (result, txn) = self.execute_in_a_transaction(bound, &mut adapter);
        let succeeded = result.is_ok();
        // The ENVCHANGE of an opening or a closing transaction goes out before the DONE of
        // the statement that caused it.
        txn_session::finish_statement(
            &mut self.state,
            &self.engine,
            &mut self.exec,
            txn,
            txn_session::kind_of(bound),
            succeeded,
            sink,
        )?;
        match result {
            Ok(ExecOutcome::Rows(count)) => {
                // The metadata was already sent through the adapter during execution.
                // `SET NOCOUNT ON` clears `DONE_COUNT` on the DONE of a `SELECT`
                // ([MS-TDS] 2.2.7.6): `SET NOCOUNT ON; SELECT 1;` answers a DONE with
                // status `0x0000` where `SET NOCOUNT OFF; SELECT 1;` answers `0x0010`, and
                // the `SET` reaches the `SELECT` of its own batch in both directions (`SET
                // NOCOUNT ON; SELECT 1; SET NOCOUNT OFF; SELECT 2;` answers `0x0001` then
                // `0x0010`), which reading the option here, after `apply_set_statement`
                // ran for the statements before, gives for free
                // (`tests/set_options_effects.rs`). SQL Server still writes the count in
                // `DoneRowCount` with the flag clear; the spec says the field is then not
                // valid, the sink has no way to write it, and a client following [MS-TDS]
                // does not read it, so `None` it is.
                let reported = if self.state.options.nocount {
                    None
                } else {
                    Some(count)
                };
                sink.done(reported, more)?;
                // `@@ROWCOUNT` is posted **after** the statement and read by the next one:
                // `SELECT 1; SELECT @@ROWCOUNT` answers 1, and so it does under `NOCOUNT
                // ON`: the option touches the DONE, not the variable.
                self.state.rowcount = count as i64;
                apply_exec_session(&mut self.state, &self.exec, true, true);
                self.exec.identity_updated = false;
                Ok(Flow::Continue)
            }
            // The DDL and `USE` answer `NoRows` (`executor`, `ddl.rs`); a `SELECT`
            // answers `Rows`. A `NoRows` sent no COLMETADATA (the executor does not call
            // `RowSink::columns` for these), so the DONE closes a statement with no result
            // set, and it carries no row count, the shape of `CREATE TABLE`, `DROP TABLE`
            // and `USE`. `NOCOUNT` is not read here: those DONEs carry no count under `SET
            // NOCOUNT OFF` either.
            Ok(ExecOutcome::NoRows) => {
                if let Some(database) = target {
                    self.switch_database(database, line, sink)?;
                }
                sink.done(None, more)?;
                // A top-level `DECLARE` or `WHILE` sets `@@ROWCOUNT` by its own rule, over
                // what nested execution left in `self.exec.rowcount` (`next_rowcount`).
                self.state.rowcount = next_rowcount(bound, self.state.rowcount, self.exec.rowcount);
                apply_exec_session(
                    &mut self.state,
                    &self.exec,
                    true,
                    statement_clears_last_error(bound),
                );
                self.exec.identity_updated = false;
                Ok(Flow::Continue)
            }
            // The token handed to the executor cannot be raised, and the outcomes of the
            // control of flow have no statement to produce them yet: each is reported as
            // an internal error rather than mapped to a DONE this layer cannot justify.
            Ok(ExecOutcome::Return(_)) => {
                sink.done(None, false)?;
                self.state.rowcount = 0;
                Ok(Flow::Stop)
            }
            Ok(ExecOutcome::Cancelled) => Err(SqlError::from(InternalError::Bug(
                crate::cancel::CANCELLED.to_owned(),
            ))),
            Ok(ExecOutcome::CallProcedure {
                name,
                args,
                return_into,
                line,
            }) => {
                let (flow, outcome) = crate::procedures::run_batch_procedure(
                    self,
                    &name,
                    &args,
                    return_into.as_deref(),
                    line,
                    more,
                    sink,
                )?;
                if !outcome.failed {
                    self.state.rowcount = outcome.rowcount;
                }
                Ok(flow)
            }
            Ok(ExecOutcome::RunDynamic { text, line }) => {
                let (flow, outcome) =
                    crate::procedures::run_batch_dynamic(self, &text, line, more, sink)?;
                if !outcome.failed {
                    self.state.rowcount = outcome.rowcount;
                }
                Ok(flow)
            }
            Ok(outcome @ (ExecOutcome::Break | ExecOutcome::Continue)) => {
                let err = SqlError::from(InternalError::Bug(format!(
                    "run_prepared: the executor answered {outcome:?}, which this layer does \
                     not handle"
                )));
                self.fail(&err, sink).map(|()| Flow::Stop)
            }
            // A batch-scoped error: sent like a run-time error, and the batch stops.
            Ok(ExecOutcome::BatchAbort(err)) => {
                let err = at_statement(err, line);
                self.state.last_error = err.number;
                sink.error(&err)?;
                sink.done(None, false)?;
                Ok(Flow::Stop)
            }
            // A run-time error stops its statement. Its catalogued scope and XACT_ABORT decide
            // whether the rest of the batch runs. It is not handed to the fake engine
            // even when it is internal: a cancelled request raises one, and running it again
            // would ignore the ATTENTION. The line the executor put on it is the failing
            // sub-expression's; SQL Server answers the statement's, and this layer knows it.
            Err(err) => {
                let err = at_statement(err, line);
                self.state.last_error = err.number;
                sink.error(&err)?;
                let continues = more
                    && !self.state.options.xact_abort
                    && err.batch_scope() == BatchErrorScope::Statement;
                sink.done(None, continues)?;
                Ok(if continues {
                    Flow::Continue
                } else {
                    Flow::Stop
                })
            }
        }
    }

    /// Moves the session to `database` and tells the client, which is what a `USE` does.
    ///
    /// Sent in this order, between the execution of the statement and its DONE:
    ///
    /// 1. ENVCHANGE type 1, `old` the database the session was in, `new` this one;
    /// 2. INFO 5701 state 1 (a login sends state 2, [`DATABASE_CONTEXT_STATE_USE`]), on the
    ///    line the `USE` statement starts on: line 2 for `SELECT 1;` / `USE` /
    ///    `  d;`, where the **name** is on line 3 and where 911 would answer 3
    ///    ([`use_name_line`]; unit test `the_5701_of_a_use_carries_the_line_of_its_statement`);
    /// 3. the DONE of the statement, `0x0001` before another statement.
    ///
    /// The ENVCHANGE and the INFO go out even when the session is already in `database`:
    /// `USE master` from `master` answers the same three tokens
    /// (`use_of_the_current_database_still_sends_the_envchange_and_the_5701`). Deliberate
    /// difference from SQL Server, which also sends an ENVCHANGE type 7 (collation) after
    /// the INFO: the collation of a session is not tracked here.
    fn switch_database(
        &mut self,
        database: &str,
        line: u32,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<()> {
        let old = std::mem::replace(&mut self.state.database, database.to_owned());
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

    /// Runs one bound statement in a transaction of its own, committed on success and
    /// rolled back on failure (autocommit: `@@TRANCOUNT` stays 0).
    ///
    /// One transaction per statement and not one per batch: a `CREATE TABLE` of a batch is
    /// visible to the batch that follows it on the same connection, and a statement that
    /// fails undoes nothing but its own writes (unit tests
    /// `create_table_then_select_star_same_connection` and `failed_ddl_rolls_back`).
    ///
    /// A transaction is opened for the statement this method receives, a `SELECT` without
    /// `FROM` that reads nothing included (`select_one_leaves_no_transaction_open`): the
    /// alternative, opening one just for the statements that touch the engine, would need
    /// this layer to know which those are, and the transaction manager accepts a
    /// transaction that wrote nothing
    /// (`vauban_txn::TransactionManager::commit`). Whichever branch runs,
    /// `commit` or `rollback` closes it before the method returns, so no transaction
    /// survives a statement (`select_one_leaves_no_transaction_open`,
    /// `a_failed_statement_leaves_no_transaction_open`).
    ///
    /// # Errors
    ///
    /// The error the statement raised, or, when it succeeded and its transaction refused
    /// to commit, the internal error of the transaction manager. A `rollback` that fails
    /// after a statement error keeps that error: the client is owed the number of what it
    /// asked for, and the manager's own failure is a bug of this engine.
    ///
    /// The transaction is the session one when a `BEGIN TRANSACTION` opened it, and a
    /// one-statement transaction otherwise; what to do with it is
    /// [`txn_session::finish_statement`]'s decision, which is why the [`StatementTxn`] is
    /// handed back with the outcome.
    fn execute_in_a_transaction(
        &mut self,
        bound: &PhysicalStatement,
        sink: &mut dyn RowSink,
    ) -> (SqlResult<ExecOutcome>, StatementTxn) {
        txn_session::execute_in_a_transaction(
            &self.state,
            &self.engine,
            &mut self.exec,
            &self.cancel,
            bound,
            sink,
        )
    }

    /// Runs one bound `EXECUTE` in a transaction, like [`execute_in_a_transaction`] but
    /// without going through the planner.
    fn execute_bound_in_a_transaction(
        &mut self,
        stmt: &BoundExecute,
        sink: &mut dyn RowSink,
    ) -> (SqlResult<ExecOutcome>, StatementTxn) {
        let txn = match txn_session::statement_txn(&self.state, &self.engine, &mut self.exec) {
            Ok(txn) => txn,
            Err(err) => return (Err(err), StatementTxn::Skipped),
        };
        let handle = match &txn {
            StatementTxn::Explicit => self
                .state
                .txn
                .as_ref()
                .map(|session_txn| session_txn.handle.clone()),
            StatementTxn::Autocommit(handle) => Some(handle.clone()),
            StatementTxn::Skipped => None,
        };
        let Some(handle) = handle else {
            return (
                Err(SqlError::from(InternalError::Bug(
                    "execute_bound_in_a_transaction: the session transaction has no handle"
                        .to_owned(),
                ))),
                txn,
            );
        };
        let snap = self.engine.txn.statement_snapshot(&handle);
        let eval = SessionEvalContext::deferred(&self.state, &self.engine.catalog, &handle);
        let token = self.cancel.token();
        let mut exec_ctx = ExecContext::scalar(&eval, self.state.options.to_binder())
            .with_engine(self.engine.storage.as_ref(), &self.engine.txn, &snap)
            .with_catalog(&self.engine.catalog)
            .with_handle(&handle)
            .with_cancel(&token)
            .with_session(&mut self.exec);
        (
            vauban_executor::execute_bound(stmt, &mut exec_ctx, sink),
            txn,
        )
    }

    /// Sends `err` and the DONE that closes the batch, and records `@@ERROR`.
    ///
    /// The DONE carries no row count and no `MORE`. Run-time execution errors take the
    /// catalogued path above instead; this helper closes preparation, fallback and RPC errors.
    fn fail(&mut self, err: &SqlError, sink: &mut dyn ResultSink) -> SqlResult<()> {
        self.state.last_error = err.number;
        sink.error(err)?;
        sink.done(None, false)
    }

    /// Last resort for a statement the binder does not bind: the fake engine, then the internal
    /// error itself when the fake engine does not know it either.
    fn fall_back(
        &mut self,
        stmt: &str,
        err: &SqlError,
        more: bool,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<Flow> {
        match fake_engine::answer(&self.cancel, stmt, more, sink)? {
            Some(true) => Ok(Flow::Continue),
            Some(false) => Ok(Flow::Stop),
            None => self.fail(err, sink).map(|()| Flow::Stop),
        }
    }
}

/// The COLMETADATA of a result set ([MS-TDS] 2.2.7, COLMETADATA).
///
/// The name is the one the binder computed, empty for an expression without an alias
/// (`SELECT 1`). `fNullable` follows the inferred type; every other flag is clear, which is
/// what a driver accepts for a computed column (`column_metadata_follows_the_schema`):
/// `usUpdateable` is 0 (read-only) because an expression is not a column of a table,
/// `fCaseSen`, `fIdentity` and `fComputed` have no meaning outside one.
///
/// Used by the tests and kept for its documentation; the live path goes through
/// [`RowSinkAdapter::columns`].
#[cfg_attr(not(test), allow(dead_code))]
fn column_metadata(schema: &OutputSchema) -> Vec<ColumnMeta> {
    schema
        .columns
        .iter()
        .map(|column| ColumnMeta {
            name: column.name.clone(),
            ty: column.ty.clone(),
            flags: ColumnFlags {
                nullable: column.ty.nullable,
                ..ColumnFlags::default()
            },
        })
        .collect()
}

/// Whether the batch still holds an `ALTER TABLE`.
pub(crate) fn batch_has_alter_table(statements: &[Statement]) -> bool {
    statements
        .iter()
        .any(|statement| matches!(statement, Statement::AlterTable(_)))
}

/// How many statements to bind at once when the batch will reach an `ALTER TABLE`.
///
/// A `CREATE TABLE` before the `ALTER` must run before the `ALTER` is bound, so
/// the tail is prepared one statement at a time when the batch holds an `ALTER TABLE`.
pub(crate) fn bind_limit_for(statements: &[Statement]) -> Option<usize> {
    batch_has_alter_table(statements).then_some(1)
}

/// Whether binding should resume on the statements after index `tail_start`.
///
/// The bind limit says how many statements to bind at once; it does not decide whether
/// binding continues when fewer were prepared than remain in the text.
pub(crate) fn should_reprepare_tail(
    tail_start: usize,
    prepared_len: usize,
    statements_len: usize,
    catalog_changed: bool,
) -> bool {
    tail_start < statements_len && (catalog_changed || tail_start >= prepared_len)
}

/// Whether the batch tail must be rebound after this statement ran.
///
/// The whole batch binds against one catalogue snapshot. A statement that names a table
/// the snapshot does not hold is deferred rather than refused
/// (`PreparedStatement::Deferred`), which is how `CREATE TABLE` followed by a `SELECT` on
/// the new name in the same batch reads it
/// (`create_table_then_select_in_the_same_batch_reads_the_new_table`). An `ALTER TABLE`
/// that changes the shape bound by following statements is the case the deferral does
/// not cover, since those statements bound: the tail is rebound after it commits so a
/// `SELECT` in the same batch reads the new columns. A `CREATE TABLE` before an
/// `ALTER TABLE` in one batch is bound one statement at a time so the `ALTER` sees tables
/// the earlier statements created.
fn catalog_changed(prepared: &PreparedStatement, tail: &[Statement]) -> bool {
    match prepared {
        PreparedStatement::Bound {
            statement: PhysicalStatement::Ddl(DdlStatement::AlterTable { .. }),
            ..
        } => true,
        PreparedStatement::Bound {
            statement: PhysicalStatement::Ddl(DdlStatement::CreateTable { .. }),
            ..
        } => batch_has_alter_table(tail),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_errors::InfoMessage;
    use vauban_storage::MemoryStorage;
    use vauban_tds::EnvChange;
    use vauban_types::{SqlType, TypeInfo, Value};

    /// What a batch sent to its sink, reduced to what the tests below assert on.
    ///
    /// `columns` holds one entry per COLMETADATA, so an empty vector means the statement
    /// sent no metadata, the shape of `CREATE TABLE`, `DROP TABLE` and `USE`
    /// (comment of the `match` in [`Session::run_prepared`]).
    #[derive(Debug, Default)]
    struct Received {
        columns: Vec<Vec<(String, SqlType)>>,
        rows: Vec<Vec<Value>>,
        dones: Vec<(Option<u64>, bool)>,
        errors: Vec<SqlError>,
        /// INFO messages, in order: the 5701 of a `USE` is one of them.
        infos: Vec<InfoMessage>,
        /// ENVCHANGE of the database, `(old, new)`, in order.
        databases: Vec<(String, String)>,
        /// Name of each call, in the order they came: what a test asserting the **order**
        /// of the ENVCHANGE, of the INFO and of the DONE of a `USE` needs.
        order: Vec<&'static str>,
    }

    impl ResultSink for Received {
        fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
            self.order.push("columns");
            self.columns.push(
                cols.iter()
                    .map(|col| (col.name.clone(), col.ty.ty))
                    .collect(),
            );
            Ok(())
        }
        fn row(&mut self, row: &[Value]) -> SqlResult<()> {
            self.order.push("row");
            self.rows.push(row.to_vec());
            Ok(())
        }
        fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
            self.order.push("done");
            self.dones.push((rowcount, more));
            Ok(())
        }
        fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
            self.order.push("info");
            self.infos.push(msg.clone());
            Ok(())
        }
        fn error(&mut self, err: &SqlError) -> SqlResult<()> {
            self.order.push("error");
            self.errors.push(err.clone());
            Ok(())
        }
        fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
            self.order.push("env_change");
            if let EnvChange::Database { old, new } = change {
                self.databases.push((old.clone(), new.clone()));
            }
            Ok(())
        }
        fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
            Ok(())
        }
        fn return_status(&mut self, _status: i32) -> SqlResult<()> {
            Ok(())
        }
    }

    /// An engine over an empty in-memory storage, bootstrapped by `Engine::new`.
    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
    }

    /// A session on `engine`, database `master` and default `SET` options.
    fn session(engine: &Arc<Engine>) -> Session {
        Session::new(Arc::clone(engine), SessionState::new(51))
    }

    /// Runs one batch and gives back what its sink received. The `Ok` is the contract of
    /// [`Session::run_batch`]: an error destined to the client is in `errors`.
    fn run(session: &mut Session, text: &str) -> Received {
        let mut sink = Received::default();
        session
            .run_batch(text, &mut sink)
            .expect("the response is complete");
        sink
    }

    /// The names of the tables of `master` this engine holds, as the catalogue answers them
    /// through a transaction of its own — the assertion of what a batch left behind.
    fn tables_of_master(engine: &Engine) -> Vec<String> {
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = engine.catalog.snapshot(&handle);
        let found: Vec<String> = ["s16a", "s16b", "s16c", "s16d", "t"]
            .into_iter()
            .filter(|name| {
                snapshot
                    .resolve_object("master", Some(DEFAULT_SCHEMA), name, DEFAULT_SCHEMA)
                    .is_some()
            })
            .map(str::to_owned)
            .collect();
        engine
            .txn
            .commit(handle)
            .expect("the reading transaction commits");
        found
    }

    #[test]
    fn column_metadata_follows_the_schema() {
        let schema = OutputSchema {
            columns: vec![
                vauban_binder::OutputColumn {
                    name: String::new(),
                    ty: TypeInfo::new(SqlType::Int, false),
                },
                vauban_binder::OutputColumn {
                    name: "n".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, true),
                },
            ],
        };
        let meta = column_metadata(&schema);
        assert_eq!(meta.len(), 2);
        assert_eq!(meta[0].name, "");
        assert_eq!(meta[0].ty.ty, SqlType::Int);
        assert_eq!(meta[0].flags, ColumnFlags::default());
        assert_eq!(meta[1].name, "n");
        assert!(meta[1].flags.nullable);
        // Everything else stays clear, `usUpdateable` included.
        assert!(!meta[1].flags.updatable);
        assert!(!meta[1].flags.case_sensitive);
        assert!(!meta[1].flags.identity);
        assert!(!meta[1].flags.computed);
    }

    #[test]
    fn column_metadata_of_an_empty_schema_is_empty() {
        assert!(
            column_metadata(&OutputSchema {
                columns: Vec::new()
            })
            .is_empty()
        );
    }

    /// `BindContext.catalog` is `Some` on the `run_batch` path, asserted through what it
    /// changes: a table name resolves.
    ///
    /// `dbo.s16a` is created by a first batch; a second one reads it with a `FROM`. With
    /// `catalog: None`, the binder has
    /// no [`CatalogView`](vauban_binder::CatalogView) to turn `dbo.s16a` into a
    /// `LogicalPlan::Scan` and answers 208 instead, which is what the second half of this
    /// test shows on a name the catalogue does not hold. The pair is the vector: the 208
    /// alone would be the answer under both hypotheses.
    #[test]
    fn bind_context_catalog_is_some() {
        let engine = engine();
        let mut session = session(&engine);
        run(&mut session, "CREATE TABLE dbo.s16a (a int);");

        let known = run(&mut session, "SELECT 1 FROM dbo.s16a;");
        assert!(known.errors.is_empty(), "{:?}", known.errors);
        assert_eq!(known.columns, vec![vec![(String::new(), SqlType::Int)]]);

        let unknown = run(&mut session, "SELECT 1 FROM dbo.s16nosuch;");
        assert_eq!(unknown.errors.len(), 1);
        assert_eq!(unknown.errors[0].number, 208);
    }

    /// A `CREATE TABLE` of one batch is seen by the next batch of the same connection: its
    /// transaction was committed when the statement ended, and the binding of the next
    /// batch snapshots the catalogue again.
    ///
    /// The table is empty, so the `SELECT *` sends the COLMETADATA of the two declared
    /// columns, in the order declared, no row, and a DONE whose count is 0. The same two
    /// statements in one batch answer the same tokens
    /// (`create_table_then_select_in_the_same_batch_reads_the_new_table`): the `SELECT` is
    /// deferred rather than refused.
    #[test]
    fn create_table_then_select_star_same_connection() {
        let engine = engine();
        let mut session = session(&engine);
        let created = run(&mut session, "CREATE TABLE dbo.s16b (a int, b bigint);");
        assert!(created.errors.is_empty(), "{:?}", created.errors);

        let read = run(&mut session, "SELECT * FROM dbo.s16b;");
        assert!(read.errors.is_empty(), "{:?}", read.errors);
        assert_eq!(
            read.columns,
            vec![vec![
                ("a".to_owned(), SqlType::Int),
                ("b".to_owned(), SqlType::BigInt),
            ]]
        );
        assert!(read.rows.is_empty());
        assert_eq!(read.dones, vec![(Some(0), false)]);
    }

    /// The same two statements in **one** batch: the `SELECT` names a table the catalogue
    /// does not hold when the batch is bound, so it is kept as text and bound when the
    /// batch reaches it, after the `CREATE TABLE` committed. One result set with the
    /// column `a`, no row, a DONE with a count of 0, and the table is there afterwards, as
    /// on SQL Server for `CREATE TABLE dbo.s16c (a int); SELECT * FROM dbo.s16c;`.
    ///
    /// The counter-proof is `a_select_before_the_create_is_208_at_run_time` in
    /// `tests/run_batch_pipeline.rs`: the same `SELECT` written **before** the `CREATE`
    /// answers 208 when it runs, so the order of the statements is what the deferral reads.
    #[test]
    fn create_table_then_select_in_the_same_batch_reads_the_new_table() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(
            &mut session,
            "CREATE TABLE dbo.s16c (a int); SELECT * FROM dbo.s16c;",
        );
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        assert_eq!(batch.columns, vec![vec![("a".to_owned(), SqlType::Int)]]);
        assert!(batch.rows.is_empty());
        assert_eq!(batch.dones, vec![(None, true), (Some(0), false)]);
        assert_eq!(tables_of_master(&engine), vec!["s16c".to_owned()]);
    }

    /// A lot whose first statement is not DDL still runs the three statements of
    /// `SELECT 1; CREATE TABLE …; ALTER TABLE … ADD` when the batch holds an `ALTER TABLE`.
    #[test]
    fn select_then_create_then_alter_in_one_batch() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(
            &mut session,
            "SELECT 1; CREATE TABLE dbo.s16e (a int); ALTER TABLE dbo.s16e ADD b int;",
        );
        assert_eq!(batch.rows.len(), 1, "the opening `SELECT 1` answered a row");
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = engine.catalog.snapshot(&handle);
        let object = snapshot
            .resolve_object("master", Some(DEFAULT_SCHEMA), "s16e", DEFAULT_SCHEMA)
            .expect("the table was created");
        assert_eq!(
            snapshot.table(object.id).expect("table meta").columns.len(),
            2
        );
        engine.txn.commit(handle).expect("commit");
    }

    /// An ordinary statement between two DDL statements still runs when the batch ends with
    /// an `ALTER TABLE`.
    #[test]
    fn select_between_create_and_alter_in_one_batch() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(
            &mut session,
            "CREATE TABLE dbo.s16f (a int); SELECT 1; ALTER TABLE dbo.s16f ADD b int;",
        );
        assert_eq!(batch.rows.len(), 1, "the middle `SELECT 1` answered a row");
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = engine.catalog.snapshot(&handle);
        let object = snapshot
            .resolve_object("master", Some(DEFAULT_SCHEMA), "s16f", DEFAULT_SCHEMA)
            .expect("the table was created");
        assert_eq!(
            snapshot.table(object.id).expect("table meta").columns.len(),
            2
        );
        engine.txn.commit(handle).expect("commit");
    }

    /// After an `ALTER TABLE` that adds a column, a `SELECT` on that column in the same batch
    /// is rebound against the updated catalogue.
    #[test]
    fn alter_add_column_then_select_in_one_batch() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(
            &mut session,
            "CREATE TABLE dbo.s16h (a int); ALTER TABLE dbo.s16h ADD b int; SELECT b FROM dbo.s16h;",
        );
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        assert_eq!(
            batch.columns,
            vec![vec![("b".to_owned(), SqlType::Int)]],
            "the `SELECT b` was bound after the `ALTER`"
        );
        assert!(batch.rows.is_empty(), "the empty table sent no row");
    }

    /// When a late statement of a batch fails to bind, the statements before it still run:
    /// two `CREATE TABLE`, an `ALTER TABLE … FOREIGN KEY`, then a `SELECT` on a missing
    /// column stops the batch with 207 after the DDL committed.
    #[test]
    fn create_alter_foreign_key_then_bad_select_in_one_batch() {
        let engine = engine();
        let mut session = session(&engine);
        let mut sink = Received::default();
        let err = session
            .run_batch(
                "CREATE TABLE dbo.s16g_p (id int NOT NULL PRIMARY KEY);\
                 CREATE TABLE dbo.s16g_c (id int NOT NULL, pid int);\
                 ALTER TABLE dbo.s16g_c ADD CONSTRAINT fk_s16g FOREIGN KEY (pid)\
                 REFERENCES dbo.s16g_p(id);\
                 SELECT nosuch FROM dbo.s16g_c;",
                &mut sink,
            )
            .expect_err("a bind error on the tail stops the batch");
        assert_eq!(err.number, 207);
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = engine.catalog.snapshot(&handle);
        assert!(
            snapshot
                .resolve_object("master", Some(DEFAULT_SCHEMA), "s16g_p", DEFAULT_SCHEMA)
                .is_some()
        );
        assert!(
            snapshot
                .resolve_object("master", Some(DEFAULT_SCHEMA), "s16g_c", DEFAULT_SCHEMA)
                .is_some()
        );
        engine.txn.commit(handle).expect("commit");
    }

    /// `CREATE TABLE` twice then `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` in one batch
    /// succeeds when the referenced table carries a primary key.
    #[test]
    fn create_tables_then_alter_add_foreign_key_in_one_batch() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(
            &mut session,
            "CREATE TABLE dbo.s16fk_p (id int NOT NULL PRIMARY KEY);\
             CREATE TABLE dbo.s16fk_c (id int NOT NULL, pid int);\
             ALTER TABLE dbo.s16fk_c ADD CONSTRAINT fk_s16fk FOREIGN KEY (pid)\
             REFERENCES dbo.s16fk_p(id);",
        );
        assert!(
            batch.errors.is_empty(),
            "unexpected errors: {:?}",
            batch.errors
        );
    }

    /// The other half of the vector above: a name the batch creates **twice** binds twice,
    /// because the snapshot of the binding shows neither, and the second statement raises
    /// 2714 while it runs.
    ///
    /// Same place as SQL Server:
    /// `SELECT 1; CREATE TABLE dbo.s16d (a int); CREATE TABLE dbo.s16d (a int); SELECT 2;`
    /// answers the result set of the `SELECT 1`, then 2714 severity 16 state 6, and no
    /// result set for the `SELECT 2`, 2714 being batch-scoped ([`BatchErrorScope`]). The
    /// first table stays: its own transaction was committed before the second statement
    /// ran.
    #[test]
    fn create_table_twice_in_one_batch_is_2714_at_run_time() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(
            &mut session,
            "SELECT 1; CREATE TABLE dbo.s16d (a int); CREATE TABLE dbo.s16d (a int); SELECT 2;",
        );
        assert_eq!(batch.rows.len(), 1, "only the `SELECT 1` answered a row");
        assert_eq!(batch.errors.len(), 1);
        assert_eq!(batch.errors[0].number, 2714);
        assert_eq!(batch.errors[0].severity, 16);
        assert_eq!(tables_of_master(&engine), vec!["s16d".to_owned()]);
    }

    /// A **run-time** error of a DDL statement carries the line its statement starts on,
    /// which [`at_statement`] puts there from [`statement_line`].
    ///
    /// Both batches open with a comment on line 1. Each puts the failing node below the
    /// first line of its statement, which is what tells "the statement's line" from "the
    /// node's" and from "line 1":
    ///
    /// - `DROP TABLE` on line 3, `dbo.s16nosuch5` on line 4 -> **3** (3701);
    /// - the second `CREATE TABLE dbo.s16v` on line 4, its column list on line 5 -> **4**
    ///   (2714).
    ///
    /// With no line to offer, [`at_statement`] leaves the error as the executor raised
    /// it, and a client reads the line of the failing node instead of the statement's.
    #[test]
    fn a_run_time_ddl_error_carries_the_line_of_its_statement() {
        let engine = engine();
        let mut session = session(&engine);

        let dropped = run(
            &mut session,
            "-- a comment on line 1\nSELECT 1;\nDROP TABLE\n  dbo.s16nosuch5;",
        );
        assert_eq!(dropped.errors.len(), 1);
        assert_eq!(dropped.errors[0].number, 3701);
        assert_eq!(dropped.errors[0].line, 3);

        let twice = run(
            &mut session,
            "-- a comment on line 1\nCREATE TABLE dbo.s16v\n  (a int);\nCREATE TABLE dbo.s16v\n  (a int);",
        );
        assert_eq!(twice.errors.len(), 1);
        assert_eq!(twice.errors[0].number, 2714);
        assert_eq!(twice.errors[0].line, 4);
    }

    /// The other half of what [`statement_line`] feeds: the fragment of a DDL statement is
    /// padded with `line - 1` newlines before it is rebound, so a **binding** error of that
    /// statement lands on its line of the batch instead of line 1.
    ///
    /// Same two hypotheses to separate: 2715 with the unknown type on line 4 and the
    /// statement on line 3 answers **3**, and 2705 with the duplicate column on line 5
    /// answers **3**.
    #[test]
    fn a_binding_error_of_a_ddl_statement_carries_the_line_of_its_statement() {
        let engine = engine();
        let mut session = session(&engine);
        for (text, number) in [
            (
                "-- a comment on line 1\nSELECT 1;\nCREATE TABLE dbo.s16w\n  (a nosuchtype);",
                2715,
            ),
            (
                "-- a comment on line 1\nSELECT 1;\nCREATE TABLE dbo.s16x\n  (a int,\n   a int);",
                2705,
            ),
        ] {
            let batch = run(&mut session, text);
            assert_eq!(batch.errors.len(), 1, "{text}");
            assert_eq!(batch.errors[0].number, number, "{text}");
            assert_eq!(batch.errors[0].line, 3, "{text}");
            assert!(
                batch.rows.is_empty(),
                "{text}: a binding error runs nothing"
            );
        }
    }

    /// A DDL statement that fails rolls its transaction back: what it wrote before failing
    /// is not there afterwards.
    ///
    /// `DROP TABLE dbo.s16a, dbo.s16nosuch;` is the vector, because it is a DDL statement
    /// that writes **before** it fails: `executor::ddl` drops the names left to right and
    /// raises 3701 on the second one. The table dropped first is back after the rollback.
    /// Without the rollback it would be gone, which is what `a_dropped_table_stays_dropped`
    /// shows on the same list minus the bad name.
    ///
    /// **Deliberate difference from SQL Server**, which keeps the first drop:
    /// `CREATE TABLE dbo.s16p (a int); DROP TABLE dbo.s16p, dbo.s16nosuch;
    /// SELECT COUNT(*) FROM sys.tables WHERE name = 's16p';` answers 3701 **and** a count
    /// of 0 there, where this engine undoes the drop. Making the statement atomic *and*
    /// keeping the first drop needs a savepoint per statement, which is not implemented.
    #[test]
    fn failed_ddl_rolls_back() {
        let engine = engine();
        let mut session = session(&engine);
        run(&mut session, "CREATE TABLE dbo.s16a (a int);");
        assert_eq!(tables_of_master(&engine), vec!["s16a".to_owned()]);

        let dropped = run(&mut session, "DROP TABLE dbo.s16a, dbo.s16nosuch;");
        assert_eq!(dropped.errors.len(), 1);
        assert_eq!(dropped.errors[0].number, 3701);
        assert_eq!(
            tables_of_master(&engine),
            vec!["s16a".to_owned()],
            "the rollback of the statement put the first table back"
        );
    }

    /// Counter-proof of `failed_ddl_rolls_back`: a `DROP TABLE` that succeeds commits, so
    /// the table is gone for the next batch. Without the commit the assertion below would
    /// read `s16a` again and the test above would pass for the wrong reason.
    #[test]
    fn a_dropped_table_stays_dropped() {
        let engine = engine();
        let mut session = session(&engine);
        run(&mut session, "CREATE TABLE dbo.s16a (a int);");
        let dropped = run(&mut session, "DROP TABLE dbo.s16a;");
        assert!(dropped.errors.is_empty(), "{:?}", dropped.errors);
        assert_eq!(tables_of_master(&engine), Vec::<String>::new());
    }

    /// A `CREATE TABLE` sends no COLMETADATA and a DONE without a row count, and so does a
    /// `DROP TABLE`; the `SELECT 1` of the same batch sends both.
    ///
    /// On the wire, the DONE of the `SELECT 1` is status `0x0011` with `row_count` 1 where
    /// those of the `CREATE TABLE`, of the `DROP TABLE` and of a `USE` are status `0x0001`
    /// with `row_count` 0: `DONE_COUNT` (`0x0010`) clear, hence `None` here and not
    /// `Some(0)`.
    #[test]
    fn a_ddl_statement_sends_no_metadata_and_a_done_without_a_count() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(
            &mut session,
            "SELECT 1; CREATE TABLE dbo.s16a (a int); DROP TABLE dbo.s16a;",
        );
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        assert_eq!(batch.columns.len(), 1, "only the `SELECT 1` sent metadata");
        assert_eq!(
            batch.dones,
            vec![(Some(1), true), (None, true), (None, false)]
        );
    }

    /// `SELECT 1` without `FROM` goes through the same transaction as the rest: it is
    /// opened, committed, and nothing stays open after the batch.
    ///
    /// [`TransactionManager::active_sessions`](vauban_txn::TransactionManager::active_sessions)
    /// is the leak detector: the transaction of the binding and the one of the statement
    /// both appear there while they are open, and neither after.
    #[test]
    fn select_one_leaves_no_transaction_open() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(&mut session, "SELECT 1;");
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        assert_eq!(batch.rows, vec![vec![Value::I32(1)]]);
        assert_eq!(batch.dones, vec![(Some(1), false)]);
        assert!(engine.txn.active_sessions().is_empty());
    }

    /// The same detector on the failing paths: a run-time error, a binding error and a DDL
    /// statement that fails each close their transaction.
    ///
    /// Counter-proof of `select_one_leaves_no_transaction_open`, which exercises the
    /// `commit` branch alone: `1 / 0` and the 2714 take the `rollback` branch, and the 208 fails
    /// while the batch is bound, where the transaction of the binding is the one to close.
    #[test]
    fn a_failed_statement_leaves_no_transaction_open() {
        let engine = engine();
        let mut session = session(&engine);
        for text in [
            "SELECT 1 / 0;",
            "SELECT 1 FROM dbo.s16nosuch;",
            "CREATE TABLE dbo.s16a (a int); CREATE TABLE dbo.s16a (b int);",
            "DROP TABLE dbo.s16nosuch;",
        ] {
            let batch = run(&mut session, text);
            assert_eq!(batch.errors.len(), 1, "{text}: {:?}", batch.errors);
            assert!(
                engine.txn.active_sessions().is_empty(),
                "{text} left a transaction open"
            );
        }
    }

    /// `USE master` from `master` sends its ENVCHANGE and its INFO 5701 just the same, and
    /// no column metadata.
    ///
    /// The response to `SELECT 1;` / `USE master;` / `SELECT 2;` holds the ENVCHANGE
    /// (`master` -> `master`), then the INFO 5701 state 1 line 2, then the DONE: the same
    /// three tokens as a `USE` that changes database, as on SQL Server. A session that
    /// sent nothing when the name did not change would answer one DONE here.
    #[test]
    fn use_of_the_current_database_still_sends_the_envchange_and_the_5701() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(&mut session, "USE master;");
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        assert!(batch.columns.is_empty());
        assert_eq!(batch.order, vec!["env_change", "info", "done"]);
        assert_eq!(
            batch.databases,
            vec![("master".to_owned(), "master".to_owned())]
        );
        assert_eq!(batch.dones, vec![(None, false)]);
        assert_eq!(session.state().database, "master");
    }

    /// A `USE` of a database that exists moves the session and sends the ENVCHANGE then
    /// the INFO 5701, with the name the **catalogue** spells.
    ///
    /// The name is written in another case than the one the `CREATE DATABASE` used, which
    /// is what separates "the name as the client wrote it" from "the name of the
    /// catalogue": `USE vauban_MIXED` on a database created as `Vauban_Mixed` names
    /// `Vauban_Mixed` in the INFO, as on SQL Server. The INFO carries state 1 and the
    /// line of the statement, and the DONE no row count.
    #[test]
    fn use_existing_database_changes_state_and_sends_5701() {
        let engine = engine();
        let mut session = session(&engine);
        let created = run(&mut session, "CREATE DATABASE Vauban_Mixed;");
        assert!(created.errors.is_empty(), "{:?}", created.errors);

        let used = run(&mut session, "USE vauban_MIXED;");
        assert!(used.errors.is_empty(), "{:?}", used.errors);
        assert_eq!(session.state().database, "Vauban_Mixed");
        assert_eq!(used.order, vec!["env_change", "info", "done"]);
        assert_eq!(
            used.databases,
            vec![("master".to_owned(), "Vauban_Mixed".to_owned())]
        );
        assert_eq!(
            used.infos,
            vec![changed_database_context(
                "Vauban_Mixed",
                DATABASE_CONTEXT_STATE_USE,
                1
            )]
        );
        assert!(
            used.infos[0].message.contains("'Vauban_Mixed'"),
            "{}",
            used.infos[0].message
        );
        assert!(used.columns.is_empty(), "a `USE` sends no metadata");
        assert_eq!(used.dones, vec![(None, false)]);

        // And the session stays there for the next batch, which is what `state.database`
        // being session state rather than batch state means.
        let again = run(&mut session, "USE Vauban_Mixed;");
        assert_eq!(
            again.databases,
            vec![("Vauban_Mixed".to_owned(), "Vauban_Mixed".to_owned())]
        );
    }

    /// The INFO 5701 of a `USE` of the middle of a batch sits between the DONE of the
    /// statement before and the DONE of the `USE`, and its line is the **statement**'s.
    ///
    /// On `SELECT 1;` / `USE` / `  Vauban_Mixed;` / `SELECT 2;`: INFO 5701, state 1, line
    /// 2, the name being on line 3, the vector that separates the line of the INFO from
    /// the line 911 answers for the same statement
    /// (`use_of_an_unknown_database_is_911_on_the_line_of_the_name`).
    #[test]
    fn the_5701_of_a_use_carries_the_line_of_its_statement() {
        let engine = engine();
        let mut session = session(&engine);
        run(&mut session, "CREATE DATABASE Vauban_Mixed;");

        let batch = run(&mut session, "SELECT 1;\nUSE\n  Vauban_Mixed;\nSELECT 2;");
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        assert_eq!(
            batch.order,
            vec![
                "columns",
                "row",
                "done",
                "env_change",
                "info",
                "done",
                "columns",
                "row",
                "done",
            ]
        );
        assert_eq!(batch.infos.len(), 1);
        assert_eq!(batch.infos[0].line, 2);
        assert_eq!(batch.infos[0].state, 1);
        assert_eq!(
            batch.dones,
            vec![(Some(1), true), (None, true), (Some(1), false)]
        );
    }

    /// The other criterion: a `USE` of a name the catalogue does not hold answers 911, and
    /// **no statement of the batch runs** — not even the `SELECT 1` written before it.
    ///
    /// `SELECT 1; USE nosuchdb; SELECT 2;` answers one error and not one result set,
    /// where a run-time 911 would have let the row of the `SELECT 1` out (rustdoc of
    /// [`Session::bind_batch`]). The session stays where it was.
    #[test]
    fn use_unknown_is_911() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(&mut session, "SELECT 1; USE nosuchdb; SELECT 2;".trim());
        assert_eq!(batch.errors.len(), 1);
        assert_eq!(batch.errors[0].number, 911);
        assert_eq!(batch.errors[0].severity, 16);
        assert_eq!(batch.errors[0].state, 1);
        assert_eq!(
            batch.errors[0].message,
            SqlError::database_not_found("nosuchdb").message
        );
        assert!(batch.rows.is_empty(), "no statement of the batch ran");
        assert!(batch.columns.is_empty());
        assert_eq!(batch.dones, vec![(None, false)]);
        assert!(batch.databases.is_empty());
        assert_eq!(session.state().database, "master");
    }

    /// The 911 lands on the line the **name** starts on, through the whole chain.
    ///
    /// The shapes are those of the table of [`use_name_line`], each of them putting the
    /// name on a line of its own so that the statement's line would be a different answer.
    #[test]
    fn use_of_an_unknown_database_is_911_on_the_line_of_the_name() {
        let engine = engine();
        let mut session = session(&engine);
        for (text, line) in [
            ("SELECT 1;\nUSE\n  nosuchdb\n;", 3),
            ("SELECT 1;\nUSE /* c\n  c */ nosuchdb;", 3),
            ("SELECT 1;\nUSE\n-- a comment\n  nosuchdb;", 4),
            ("SELECT 1;\nUSE [nosuch\ndb];", 2),
            ("SELECT 1;\nUSE nosuchdb;", 2),
        ] {
            let batch = run(&mut session, text);
            assert_eq!(batch.errors.len(), 1, "{text}");
            assert_eq!(batch.errors[0].number, 911, "{text}");
            assert_eq!(batch.errors[0].line, line, "{text}");
        }
    }

    /// A database the batch **creates** is not a database the `USE` of the same batch can
    /// open: the snapshot of the binding predates the batch, so 911 comes out and the
    /// `CREATE DATABASE` does not run.
    ///
    /// Same answer on SQL Server, which resolves the `USE` while it compiles the batch:
    /// `CREATE DATABASE vauban_c; USE vauban_c; SELECT DB_NAME();` answers 911 on line 1
    /// and leaves no `vauban_c` behind. The counter-proof is the second batch below: two
    /// batches, and the `USE` works.
    #[test]
    fn a_database_created_by_the_batch_is_not_usable_by_it() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(&mut session, "CREATE DATABASE vauban_c; USE vauban_c;");
        assert_eq!(batch.errors.len(), 1);
        assert_eq!(batch.errors[0].number, 911);
        assert_eq!(session.state().database, "master");

        // Nothing ran, so the database is not there: the same `USE` alone answers 911 too.
        let alone = run(&mut session, "USE vauban_c;");
        assert_eq!(alone.errors.len(), 1);
        assert_eq!(alone.errors[0].number, 911);

        // In two batches it works, which is what says the 911 above is about *when* the
        // name is resolved and not about the name itself.
        run(&mut session, "CREATE DATABASE vauban_c;");
        let used = run(&mut session, "USE vauban_c;");
        assert!(used.errors.is_empty(), "{:?}", used.errors);
        assert_eq!(session.state().database, "vauban_c");
    }

    /// A `USE` moves the database the **rest of the batch** binds against, not just the
    /// one the next batch sees.
    ///
    /// `dbo.s17t` exists in `master` alone, so `USE d; SELECT 1 FROM dbo.s17t;` answers 208:
    /// the `SELECT` is bound in `d`. The name is resolved when the statement runs, so the
    /// 208 comes **after** the ENVCHANGE and the 5701 of the `USE`, on the line of the
    /// `SELECT`, as on SQL Server. The counter-proof is the first batch: without the
    /// `USE`, the very same `SELECT` finds its table.
    #[test]
    fn use_moves_the_binding_of_the_rest_of_the_batch() {
        let engine = engine();
        let mut session = session(&engine);
        run(&mut session, "CREATE TABLE dbo.s17t (a int);");
        run(&mut session, "CREATE DATABASE vauban_d;");

        let in_master = run(&mut session, "SELECT 1 FROM dbo.s17t;");
        assert!(in_master.errors.is_empty(), "{:?}", in_master.errors);

        let after_use = run(&mut session, "USE vauban_d;\nSELECT 1 FROM dbo.s17t;");
        assert_eq!(after_use.errors.len(), 1);
        assert_eq!(after_use.errors[0].number, 208);
        assert_eq!(after_use.errors[0].line, 2);
        assert_eq!(
            after_use.order,
            vec!["env_change", "info", "done", "error", "done"],
            "the USE ran before the SELECT was resolved"
        );
        assert_eq!(
            after_use.databases,
            vec![("master".to_owned(), "vauban_d".to_owned())]
        );
    }
}
