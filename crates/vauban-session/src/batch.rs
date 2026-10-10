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
//! this layer sends them through the adapter of `executor::StatementSink`. A
//! statement the binder refuses with 208 is the exception: it is kept as text and goes
//! through the same three steps when the program reaches it (`PreparedStatement::Deferred`,
//! `BatchHost::run_deferred`).
//!
//! # One program for the batch
//!
//! The prepared statements are compiled into one `vauban_executor::Program` and run by
//! the interpreter of the executor, which holds the program counter: the `IF`, the
//! `WHILE`, the `BEGIN … END`, the jumps and the labels at any depth, and the decision that
//! an error lets the batch go on. This layer is the `vauban_executor::Host` of that program
//! ([`BatchHost`]): it runs each statement that holds no other one in a transaction of its
//! own and closes it with its DONE, evaluates each condition, puts the line of its
//! statement on each error, and runs each call, `EXEC` of a procedure or of a dynamic text,
//! where it is written, so an `EXEC` in the body of a `WHILE` runs once per turn
//! (`tests/run_batch_pipeline.rs`, `a_loop_runs_its_dynamic_text_on_each_turn`). A batch
//! that holds an `ALTER TABLE`, a `DROP TABLE`, a `DROP INDEX`, a `ROLLBACK` or an `EXECUTE`
//! is bound one statement at a time, when the program reaches each ([`bind_limit_for`]).
//!
//! Whether a DONE carries `MORE` depends on what follows it in the response, which a jump
//! decides: the DONE is held back until the next token, and the last one goes out without
//! it ([`HeldDone`]).
//!
//! The body of a call, a dynamic text or the text of `sp_executesql`, runs through the
//! same preparation, the same program and the same host ([`Session::run_body`]), in a
//! level its call opened (`procedures.rs`): its statements end with DONEINPROC
//! ([`Level::Called`]), an error that ends the level is closed by the call rather than by
//! a DONEINPROC of its own, and a call it makes opens a level below it, so a text that runs
//! a text that runs a third one counts three levels
//! (`procedures::tests::a_call_past_the_nesting_limit_is_refused`).
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
//! - a **run-time** error stops its statement. Its catalogued [`vauban_errors::ErrorScope`] then
//!   decides whether the following statement runs. This is not a severity/state rule: 8134
//!   and 245 are both severity 16, state 1, but 8134 alone lets the batch continue
//!   (`a_statement_scoped_runtime_error_lets_the_batch_continue`,
//!   `a_batch_scoped_runtime_error_stops_the_batch`). With `XACT_ABORT ON`, a run-time
//!   error of either scope stops the batch
//!   (`xact_abort_stops_a_statement_scoped_runtime_error`).
//!
//! # A binding waits for the schema of what it names
//!
//! A statement that names a table another session is creating, altering or dropping in a
//! transaction it has not ended waits, from its binding, for the end of that transaction,
//! then answers what the catalogue holds by then: the rows of a table whose `DROP` was rolled
//! back, 208 for one whose `CREATE` was, and an `INSERT` that waited for an `ALTER TABLE`
//! writes its row in the table the `ALTER` left (`tests/ddl_visibility.rs`). The catalogue
//! snapshot keeps the user tables each binding reached
//! (`vauban_catalog::CatalogSnapshot::take_resolved_tables`); [`Session::bind_one_stable`]
//! waits for the `Sch-M` of each one and binds the statement again after a wait. The limit is
//! the `SET LOCK_TIMEOUT` the session had when the batch arrived: 1222 state 56, state 47 for
//! a `CREATE TABLE`. A wait that closes a cycle with the open transaction of the session is a
//! deadlock, and its victim is rolled back as at run time.
//!
//! The `Sch-S` is given back once the binding is over: a `DROP` or an `ALTER TABLE` that
//! another session starts after a statement was bound and before it runs is met by the
//! statement when it locks a row, and not before.
//!
//! # Where the column metadata of a statement goes out
//!
//! **Between compilation and execution**, which is what decides whether a failing
//! statement shows the client a result set at all. `prepare_batch` calls
//! `executor::compile` for the statements it binds, deferred ones excepted; once they have
//! succeeded, `BatchHost::run_bound` sends COLMETADATA and calls `executor::execute`. A compilation
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
//! `PhysicalPlan`. So it answers the line of the failing sub-expression, and
//! `BatchHost::place` puts the statement's over it. This is the layer that holds the datum: the `&Statement`
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
use crate::procedures::NestedParam;
use crate::server::Engine;
use crate::set_options::{SetOutcome, apply_set_option, lock_timeout_from_ms};
use crate::sink::{
    CUR_CMD_DELETE, CUR_CMD_INSERT, CUR_CMD_SELECT, CUR_CMD_SELECT_INTO, CUR_CMD_UPDATE,
    ResultSink, StatementSinkAdapter,
};
use crate::state::SessionState;
use crate::statements::{
    INTERNAL_ERROR, INVALID_OBJECT_NAME, at_statement, declare_variables_held_by,
    first_held_statement_line, first_undeclared_goto, labels_at_any_depth, last_line,
    next_rowcount, statement_clears_last_error, statement_fragment, statement_line, statement_text,
    use_name_line,
};
use crate::txn_session::{self, StatementTxn};
use vauban_binder::{
    BatchVariables, BindContext, BoundExecute, BoundStatement, DdlStatement, OutputSchema,
};
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{ErrorScope, InfoMessage, InternalError, SqlError, SqlResult, error_behavior};
use vauban_executor::{
    Call, EnvironmentChange, ExecContext, ExecOutcome, ExecSession, Host, Program, ProgramEnd, Ran,
    Site, StatementSink,
};
use vauban_parser::{SetOptionStatement, Statement, parse_batch};
use vauban_planner::{PhysicalStatement, PlanContext, SnapshotIndexes};
use vauban_storage::TableId;
use vauban_tds::{ColumnFlags, ColumnMeta, EnvChange};
use vauban_txn::{IsolationLevel, TxnHandle};
use vauban_types::{TypeInfo, Value};

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

/// Number of the error a lock wait answers once `SET LOCK_TIMEOUT` has run out.
const LOCK_REQUEST_TIMEOUT: u32 = 1222;

/// State of the 1222 a `CREATE TABLE` answers when the wait of its binding runs out, where
/// the other statements answer state 56.
const LOCK_TIMEOUT_STATE_OF_A_CREATE: u8 = 47;

/// Number of the error a `GOTO` answers when no label of the text carries its name.
const GOTO_LABEL_NOT_DECLARED: u32 = 133;

/// The numbers a batch defers instead of treating as a compilation error: a table the
/// catalogue does not hold yet may be created by an earlier statement of the batch, in
/// this batch or in the nested one of an `EXEC`.
const DEFERRED_UNTIL_EXECUTION: [u32; 3] = [
    INVALID_OBJECT_NAME,
    CREATE_INDEX_MISSING_TABLE,
    ALTER_TABLE_MISSING_TABLE,
];

/// The errors a batch defers when a name it creates is **taken** rather than missing: an
/// earlier statement of the batch may drop what holds it, the shape of a migration script
/// that writes `IF OBJECT_ID('t') IS NOT NULL DROP TABLE t; CREATE TABLE t (…);`
/// (`tests/recreate_in_one_batch.rs`). Each entry is a number and the state that picks it
/// out: 2714 state 6 for a `CREATE TABLE` or a `SELECT … INTO`, 2714 state 5 for an
/// `ADD CONSTRAINT`, 2705 state 4 for an `ALTER TABLE … ADD` of a column the table has.
/// Under state 3, 2705 is a column list that writes one name twice, a fault of the text that
/// stays a compilation error. The same entries are deferred in a branch by the binder.
const DEFERRED_WHEN_TAKEN: [(u32, u8); 3] = [(2714, 6), (2714, 5), (2705, 4)];

/// Whether `error`, answered while the batch is bound, waits for the statement to be
/// reached ([`DEFERRED_UNTIL_EXECUTION`], [`DEFERRED_WHEN_TAKEN`]).
fn deferred_until_execution(error: &SqlError) -> bool {
    DEFERRED_UNTIL_EXECUTION.contains(&error.number)
        || DEFERRED_WHEN_TAKEN
            .iter()
            .any(|&(number, state)| error.number == number && error.state == state)
}

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

/// One statement after every compilation-stage check has succeeded for the whole batch.
enum PreparedStatement {
    /// A statement the binder does not bind, still served by the deliberately narrow fallback.
    Fallback { text: String, error: SqlError },
    /// A bound, planned and compile-checked statement, ready to execute without another
    /// compilation pass.
    Bound { statement: PhysicalStatement },
    /// A `USE` whose target was **found** in the catalogue while the batch was bound: a
    /// name that is not there raises 911 at that moment and no statement of the batch
    /// runs, which is where SQL Server raises it too ([`Session::bind_batch`]).
    Use {
        statement: PhysicalStatement,
        /// Name of the target as the **catalogue** spells it, which is the one the
        /// ENVCHANGE and the INFO 5701 carry (unit test
        /// `use_existing_database_changes_state_and_sends_5701`).
        database: String,
    },
    /// A bound `EXECUTE`, evaluated at run time without going through the planner.
    Execute { stmt: BoundExecute, line: u32 },
    /// A statement whose binding answered one of [`deferred_until_execution`] while the
    /// batch was prepared: the table it names may be created by a statement before it, in
    /// this batch or in a nested one, so it is bound, planned and compile-checked again
    /// when the program reaches it, against the catalogue of that moment
    /// ([`BatchHost::run_deferred`]).
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

/// What the batch holds at one position of its text.
enum Slot {
    /// Not bound yet: a batch that moves the catalogue binds each statement when the
    /// program reaches it ([`bind_limit_for`]).
    Unbound,
    /// Bound, planned and checked, run by the host when the program reaches it.
    Ready(Box<PreparedStatement>),
    /// Compiled into the program itself: a statement that holds others, or a jump.
    InProgram,
    /// Taken out while the host runs it.
    Running,
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
        let mut held = HeldDone::new(sink);
        let ended = match self.run_body(text, Level::Batch, &[], &mut held) {
            Ok((ProgramEnd::Cancelled, _)) => Err(cancelled()),
            Ok(_) => Ok(()),
            Err(err) => Err(err),
        };
        if let Err(err) = ended {
            held.release(true)?;
            return Err(err);
        }
        // A batch may end with a transaction still open; what the end of a batch does with
        // it is `txn_session::end_of_batch`'s decision.
        txn_session::end_of_batch(&self.state, &mut held)?;
        held.finish()
    }

    /// Parses, prepares, compiles and runs `text` as the body of `level`, its statements
    /// closed in `sink` (module documentation, "One program for the batch"). `params` are
    /// the variables the body starts with, declared for the binder; the caller entered
    /// their values in the running level.
    ///
    /// An error raised before the first statement runs (a syntax error, a `GOTO` to a label
    /// the text does not declare, a binding error) goes to `sink` and ends the body: the
    /// batch is closed by its DONE, a called level is left to its caller to close.
    ///
    /// # Errors
    ///
    /// An internal error that ends the request at once: a sink that cannot take what it is
    /// handed, a cancelled request.
    pub(crate) fn run_body(
        &mut self,
        text: &str,
        level: Level,
        params: &[NestedParam],
        sink: &mut dyn ResultSink,
    ) -> SqlResult<(ProgramEnd, LevelTrack)> {
        let mut track = LevelTrack::default();
        let batch = match parse_batch(text, &self.state.options.parse_options()) {
            Ok(batch) => batch,
            // A syntax error is a compilation error: SQL Server compiles no batch partly,
            // so not one statement of this text runs. One ERROR, one DONE.
            Err(err) => return self.fail_body(err, level, track, sink),
        };
        if batch.statements.is_empty() {
            // A response always ends with a DONE, even for a batch with no statement
            // (blank text, comments only): without it the client would wait forever.
            if level == Level::Batch {
                sink.done(None, false)?;
            }
            return Ok((ProgramEnd::Completed, track));
        }
        // Where a `GOTO` lands is read from the statements of this text, at any depth, so a
        // name that no label of the text declares is known before the first statement runs:
        // 133 is a compilation error, and the batch produces no result set
        // (`tests/run_batch_pipeline.rs`, `a_goto_to_an_unknown_label_is_133_and_stops_the_batch`).
        // The labels of a dynamic text are its own, and so is its 133
        // (`a_goto_to_an_unknown_label_of_a_dynamic_text_is_133`).
        let statements = batch.statements.as_slice();
        let labels = labels_at_any_depth(statements);
        if let Some(label) = first_undeclared_goto(statements, &labels) {
            let err = SqlError::goto_label_not_declared(label).with_line(last_line(text));
            return self.fail_body(err, level, track, sink);
        }
        let prepared = if bind_limit_for(statements).is_some() {
            // Without an `ALTER TABLE` at the top level, the whole batch is bound once
            // against the catalogue it starts from, so that a compilation error still stops
            // it before its first statement runs, and the result is set aside: each
            // statement is bound again when the program reaches it
            // (`tests/recreate_in_one_batch.rs`,
            // `a_compilation_error_after_a_drop_still_stops_the_batch_before_the_drop`).
            let checked = if batch_has_alter_table(statements) {
                Ok(())
            } else {
                self.prepare_batch(text, statements, params).map(|_| ())
            };
            checked
                .and_then(|()| declared_parameters(params))
                .map(|variables| {
                    let slots = statements.iter().map(|_| Slot::Unbound).collect();
                    (slots, self.state.clone(), variables)
                })
        } else {
            self.prepare_batch(text, statements, params)
                .map(|(prepared, state, variables)| {
                    let slots = prepared
                        .into_iter()
                        .map(|prepared| Slot::Ready(Box::new(prepared)))
                        .collect();
                    (slots, state, variables)
                })
        };
        let (slots, bind_state, batch_variables) = match prepared {
            Ok(prepared) => prepared,
            Err(err) if self.cancel.is_cancelled() => return Err(err),
            Err(err) => return self.fail_body(err, level, track, sink),
        };
        let batch = PreparedBatch {
            text,
            statements,
            slots,
            bind_state,
            batch_variables,
        };
        let end = self.run_program(batch, level, &mut track, sink)?;
        Ok((end, track))
    }

    /// Answers how a body ends when `err` was raised before its first statement ran.
    ///
    /// The batch hands the error to `sink` and closes the response with its DONE: no level
    /// encloses it, so no program is left to decide anything. A called level sends nothing:
    /// it answers [`ProgramEnd::Refused`], and the program that made the call decides the
    /// error where the `EXECUTE` is written (`vauban_executor::Host::raise`); a level that
    /// no program called, the text of an RPC, is closed by its caller
    /// (`procedures.rs`, `run_level`).
    fn fail_body(
        &mut self,
        err: SqlError,
        level: Level,
        mut track: LevelTrack,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<(ProgramEnd, LevelTrack)> {
        match level {
            Level::Batch => {
                self.fail(&err, sink)?;
                let xact_abort = self.state.options.xact_abort;
                Ok((ProgramEnd::ended_by(err, xact_abort), track))
            }
            Level::Called => {
                track.failed = true;
                track.status = error_status(&err);
                Ok((ProgramEnd::Refused(err), track))
            }
        }
    }

    /// Compiles the statements of a body into one program and runs it through a
    /// [`BatchHost`] (module documentation, "One program for the batch").
    fn run_program(
        &mut self,
        batch: PreparedBatch<'_>,
        level: Level,
        track: &mut LevelTrack,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<ProgramEnd> {
        let PreparedBatch {
            text,
            statements,
            mut slots,
            bind_state,
            batch_variables,
        } = batch;
        let lazy = slots.iter().any(|slot| matches!(slot, Slot::Unbound));
        let mut in_program: Vec<Option<PhysicalStatement>> = Vec::with_capacity(slots.len());
        for (slot, statement) in slots.iter_mut().zip(statements) {
            let mut in_place = false;
            if let Slot::Ready(prepared) = &*slot
                && let PreparedStatement::Bound { statement } = prepared.as_ref()
            {
                declare_variables_held_by(statement, &mut self.exec);
                in_place = compiled_in_place(statement);
            }
            let compiled = if in_place {
                match std::mem::replace(slot, Slot::InProgram) {
                    Slot::Ready(prepared) => match *prepared {
                        PreparedStatement::Bound { statement } => Some(statement),
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
                let (end, failed) = self.fail_body(err, level, std::mem::take(track), sink)?;
                *track = failed;
                return Ok(end);
            }
        };
        let bound = if lazy { 0 } else { slots.len() };
        let end = {
            let mut host = BatchHost {
                session: self,
                sink: &mut *sink,
                level,
                track: &mut *track,
                text,
                statements,
                slots,
                bound,
                bind_state,
                batch_variables,
                outputs: Vec::new(),
                refused_rowcount: None,
            };
            program.run_hosted(&mut host)?
        };
        // A `RETURN` ends the body and counts no row: `@@ROWCOUNT` leaves it at 0 whatever
        // the statement before it counted (`tests/nested.rs`,
        // `rowcount_is_zero_after_a_text_that_ends_with_return`); its DONE is already out.
        if let ProgramEnd::Returned(_) = end {
            self.state.rowcount = 0;
        }
        Ok(end)
    }

    /// The catalogue a batch is bound against: the one the open transaction of the session
    /// sees when there is one, so that a table that transaction has created, altered or
    /// dropped is bound as it left it (`tests/ddl_visibility.rs`,
    /// `an_alter_in_an_open_transaction_is_bound_by_the_next_statements`); the one `binding`
    /// sees otherwise.
    fn binding_snapshot(&self, binding: &TxnHandle) -> CatalogSnapshot {
        binding_snapshot(&self.engine, &self.state, binding)
    }

    /// Parses, binds and compile-checks every statement without changing live session state.
    /// `params` are declared before the first statement is bound.
    fn prepare_batch(
        &self,
        text: &str,
        statements: &[Statement],
        params: &[NestedParam],
    ) -> SqlResult<(Vec<PreparedStatement>, SessionState, BatchVariables)> {
        let mut state = self.state.clone();
        let mut batch_variables = declared_parameters(params)?;
        let binding = self.engine.txn.begin(IsolationLevel::ReadCommitted);
        let prepared = self.bind_batch(
            text,
            statements,
            &binding,
            &mut state,
            &mut batch_variables,
            None,
        );
        let closed = self.engine.txn.commit(binding);
        match (prepared, closed) {
            (Ok(prepared), Ok(())) => Ok((prepared, state, batch_variables)),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    /// Binds the statements of a batch that moves the catalogue from the first of
    /// `statements`, against the catalogue of this moment, up to the first one that is not
    /// a `SET` ([`bind_limit_for`]).
    fn reprepare_tail(
        &self,
        text: &str,
        statements: &[Statement],
        state: &mut SessionState,
        batch_variables: &mut BatchVariables,
    ) -> SqlResult<Vec<PreparedStatement>> {
        let binding = self.engine.txn.begin(IsolationLevel::ReadCommitted);
        let prepared = self.bind_batch(
            text,
            statements,
            &binding,
            state,
            batch_variables,
            bind_limit_for(statements),
        );
        self.engine.txn.commit(binding)?;
        prepared
    }

    /// Binds the statements of `statements` against the catalogue `binding` reads
    /// ([`Session::binding_snapshot`]), each through [`Session::bind_one_stable`].
    fn bind_batch(
        &self,
        text: &str,
        statements: &[Statement],
        binding: &TxnHandle,
        state: &mut SessionState,
        batch_variables: &mut BatchVariables,
        mut limit: Option<usize>,
    ) -> SqlResult<Vec<PreparedStatement>> {
        let mut prepared = Vec::with_capacity(statements.len());
        let mut snapshot = self.binding_snapshot(binding);

        for (index, original) in statements.iter().enumerate() {
            if limit == Some(0) {
                break;
            }
            self.cancel.check()?;
            let raw = statement_text(text, original);
            let line = statement_line(original);
            let source = statement_fragment(text, original, statements.get(index + 1));
            let padded = format!("{}{}", "\n".repeat(line.saturating_sub(1) as usize), source);
            let one = match self.bind_one_stable(
                &padded,
                raw,
                &mut snapshot,
                binding,
                state,
                batch_variables,
            ) {
                Ok(one) => one,
                // A table the catalogue does not hold yet, or a name an earlier statement
                // may free, is not a compilation error: the statement is kept as text and
                // bound when the program reaches it.
                Err(error) if deferred_until_execution(&error) => PreparedStatement::Deferred {
                    padded,
                    raw: statement_text(text, original),
                    variables: batch_variables.clone(),
                },
                Err(error) => return Err(error),
            };
            // A `SET` binds with the statement after it: a batch bound one statement at a
            // time reads the options it sets. An `EXECUTE` counts, so the statement after it
            // is bound once its text has run.
            let counts = match &one {
                PreparedStatement::Bound { statement } => {
                    !matches!(statement, PhysicalStatement::SetOption(_))
                }
                PreparedStatement::Deferred { .. } | PreparedStatement::Execute { .. } => true,
                _ => false,
            };
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
    /// A `USE` moves `state.database`, a `DECLARE` enters its variables and a `SET <option>`
    /// changes the options of `state`, which is what the statements bound after this one
    /// read: `SET QUOTED_IDENTIFIER OFF; SELECT "a";` reparses its `SELECT` under `OFF`. A
    /// `SET` the statement holds, in a branch or a block, changes nothing here: it applies
    /// when the program reaches it.
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

        let indexes = SnapshotIndexes {
            storage: self.engine.storage.as_ref(),
            snapshot,
        };
        let physical = vauban_planner::plan(bound, &PlanContext { catalog: &indexes })
            .map_err(|error| at_statement(error, line))?;

        let eval = SessionEvalContext::new(state, Some(snapshot));
        let mut exec = ExecContext::scalar(&eval, state.options.to_binder());
        vauban_executor::compile(&physical, &mut exec)
            .map_err(|error| at_statement(error, line))?;

        if let PhysicalStatement::SetOption(set) = &physical {
            // The live options change when the program reaches the statement; a refusal
            // (8107) is raised then, on its line.
            let _ = apply_set_option(state, set, Some(snapshot));
        }

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
            });
        }

        Ok(PreparedStatement::Bound {
            statement: physical,
        })
    }

    /// Binds a deferred statement against the catalogue as it stands when the program
    /// reaches it: a snapshot of the catalogue of this moment, a clone of the live state
    /// and of the variables the statement can see.
    ///
    /// The catalogue is read through the session transaction when one is open, so a
    /// `CREATE TABLE` of that transaction is seen before its `COMMIT`
    /// (`a_deferred_statement_reads_the_catalogue_of_its_own_transaction`), and through a
    /// transaction of its own otherwise, closed before the statement runs. The live
    /// session state is the one bound against: the `SET` options before the statement
    /// have been applied to it by then.
    fn bind_deferred(
        &self,
        padded: &str,
        raw: &str,
        variables: &BatchVariables,
    ) -> SqlResult<PreparedStatement> {
        let mut state = self.state.clone();
        let mut variables = variables.clone();
        let binding = self.engine.txn.begin(IsolationLevel::ReadCommitted);
        let mut snapshot = self.binding_snapshot(&binding);
        let prepared = self.bind_one_stable(
            padded,
            raw.to_owned(),
            &mut snapshot,
            &binding,
            &mut state,
            &mut variables,
        );
        self.engine.txn.commit(binding)?;
        prepared
    }

    /// Binds one statement with [`Session::bind_one`], then waits for the `Sch-M` another
    /// transaction holds on a user table the binding named, and binds the statement again,
    /// against a new `snapshot`, once a wait is over (module documentation, "A binding
    /// waits for the schema of what it names").
    ///
    /// A write also waits on the tables its `FOREIGN KEY` checks read
    /// (`CatalogSnapshot::take_resolved_tables`). `state` and `batch_variables` are put back
    /// as they were before a binding that is done again.
    ///
    /// # Errors
    ///
    /// The error of the binding, or the one of the wait: 1222 state 56 under the
    /// `SET LOCK_TIMEOUT` of the session, state 47 for a `CREATE TABLE`
    /// (`tests/ddl_visibility.rs`, `a_wait_at_binding_under_a_lock_timeout_is_1222`), 1205
    /// for a deadlock victim (`a_crossed_create_deadlock_leaves_one_table_per_name`).
    fn bind_one_stable(
        &self,
        padded: &str,
        raw: String,
        snapshot: &mut CatalogSnapshot,
        binding: &TxnHandle,
        state: &mut SessionState,
        batch_variables: &mut BatchVariables,
    ) -> SqlResult<PreparedStatement> {
        loop {
            let state_before = state.clone();
            let variables_before = batch_variables.clone();
            let _ = snapshot.take_resolved_tables(false);
            let bound = self.bind_one(padded, raw.clone(), snapshot, state, batch_variables);
            let writes = matches!(
                &bound,
                Ok(PreparedStatement::Bound {
                    statement: PhysicalStatement::Insert(_)
                        | PhysicalStatement::Update(_)
                        | PhysicalStatement::Delete(_)
                })
            );
            let tables = snapshot.take_resolved_tables(writes);
            match self.wait_for_schema(binding, &tables) {
                Ok(false) => return bound,
                Ok(true) => {}
                Err(mut err) => {
                    if err.number == LOCK_REQUEST_TIMEOUT && creates_a_table(&bound) {
                        err.state = LOCK_TIMEOUT_STATE_OF_A_CREATE;
                    }
                    return Err(err);
                }
            }
            *state = state_before;
            *batch_variables = variables_before;
            *snapshot = self.binding_snapshot(binding);
        }
    }

    /// Waits, table by table, until no other transaction holds the `Sch-M` of `tables`,
    /// and answers whether one of them had to be waited for.
    ///
    /// The wait is made in the open transaction of the session when there is one, so that a
    /// cycle it closes is a deadlock the lock manager sees, and in `binding` otherwise,
    /// which holds nothing else. Its limit is the `SET LOCK_TIMEOUT` of the live session,
    /// not the one a `SET` earlier in the same batch has given the binding state.
    fn wait_for_schema(&self, binding: &TxnHandle, tables: &[TableId]) -> SqlResult<bool> {
        let owner = self
            .state
            .txn
            .as_ref()
            .map_or(binding, |session_txn| &session_txn.handle);
        let timeout = lock_timeout_from_ms(self.state.options.lock_timeout);
        let wait = self.cancel.token().lock_wait();
        let mut waited = false;
        for table in tables {
            match self
                .engine
                .txn
                .wait_for_schema_stability(owner, *table, timeout, &wait)
            {
                Ok(this_one) => waited |= this_one,
                Err(_) if self.cancel.is_cancelled() => return Err(cancelled()),
                Err(err) => return Err(err),
            }
        }
        Ok(waited)
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
        sink: &mut dyn StatementSink,
    ) -> SqlResult<()> {
        let old = std::mem::replace(&mut self.state.database, database.to_owned());
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
        sink: &mut dyn StatementSink,
    ) -> (SqlResult<ExecOutcome>, StatementTxn) {
        self.exec.nocount = self.state.options.nocount;
        txn_session::execute_in_a_transaction(
            &self.state,
            &self.engine,
            &mut self.exec,
            &self.cancel,
            bound,
            sink,
        )
    }

    /// Runs `step` in a context opened like the one of a statement, in the transaction of
    /// the session when one is open and in one of its own otherwise, then closes it.
    fn within_a_transaction(
        &mut self,
        step: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<()>,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<()> {
        let engine = Arc::clone(&self.engine);
        within_a_transaction(
            &mut self.state,
            &engine,
            &mut self.exec,
            &self.cancel,
            step,
            sink,
        )
    }

    /// Runs one bound `EXECUTE` in a transaction, like [`execute_in_a_transaction`] but
    /// without going through the planner.
    fn execute_bound_in_a_transaction(
        &mut self,
        stmt: &BoundExecute,
        sink: &mut dyn StatementSink,
    ) -> (SqlResult<ExecOutcome>, StatementTxn) {
        self.exec.nocount = self.state.options.nocount;
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
    /// path of the program instead; this helper closes preparation and RPC errors.
    ///
    /// An error whose catalogue entry rolls the transaction back, the 1205 of a binding
    /// chosen as a deadlock victim, does so after its ERROR token, as at run time
    /// ([`BatchHost`], `raise`).
    fn fail(&mut self, err: &SqlError, sink: &mut dyn ResultSink) -> SqlResult<()> {
        self.state.last_error = err.number;
        sink.error(err)?;
        if error_behavior(err.number).rolls_back_transaction {
            txn_session::roll_back_after_error(
                &mut self.state,
                &self.engine,
                &mut self.exec,
                sink,
            )?;
        }
        sink.done(None, false)
    }
}

/// Runs `step` in a context opened for `state` like the one of a statement, in the
/// transaction of the session when one is open and in one of its own otherwise, then
/// closes it. `SET NOCOUNT` of the running level is the one of `state`, which a call sets
/// aside with the rest of the level (`interp/frame.rs` of the executor).
fn within_a_transaction(
    state: &mut SessionState,
    engine: &Arc<Engine>,
    exec: &mut ExecSession,
    cancel: &CancelHandle,
    step: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<()>,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    exec.nocount = state.options.nocount;
    let txn = txn_session::statement_txn(state, engine, exec)?;
    let handle = match &txn {
        StatementTxn::Explicit => state
            .txn
            .as_ref()
            .map(|session_txn| session_txn.handle.clone()),
        StatementTxn::Autocommit(handle) => Some(handle.clone()),
        StatementTxn::Skipped => None,
    };
    let Some(handle) = handle else {
        return Err(SqlError::from(InternalError::Bug(
            "within_a_transaction: the session transaction has no handle".to_owned(),
        )));
    };
    let result = {
        let snap = engine.txn.statement_snapshot(&handle);
        let eval = SessionEvalContext::deferred(state, &engine.catalog, &handle);
        let token = cancel.token();
        let mut ctx = ExecContext::scalar(&eval, state.options.to_binder())
            .with_engine(engine.storage.as_ref(), &engine.txn, &snap)
            .with_catalog(&engine.catalog)
            .with_handle(&handle)
            .with_cancel(&token)
            .with_session(exec);
        step(&mut ctx)
    };
    txn_session::finish_statement(
        state,
        engine,
        exec,
        txn,
        txn_session::TxnKind::Other,
        result.is_ok(),
        sink,
    )?;
    result
}

/// The statements of a batch once prepared, handed to [`Session::run_program`].
struct PreparedBatch<'b> {
    text: &'b str,
    statements: &'b [Statement],
    slots: Vec<Slot>,
    /// The session state the binding has reached, read by a statement bound when the
    /// program reaches it.
    bind_state: SessionState,
    /// The variables the binding has declared, read the same way.
    batch_variables: BatchVariables,
}

/// Whether the program of the batch holds `statement` itself rather than handing it to the
/// host: the statements that hold others, and the jumps.
pub(crate) fn compiled_in_place(statement: &PhysicalStatement) -> bool {
    matches!(
        statement,
        PhysicalStatement::If { .. }
            | PhysicalStatement::While { .. }
            | PhysicalStatement::Block(_)
            | PhysicalStatement::Goto { .. }
            | PhysicalStatement::Label { .. }
            | PhysicalStatement::Return(_)
            | PhysicalStatement::Break
            | PhysicalStatement::Continue
    )
}

/// Where the program a [`BatchHost`] runs sits, which decides the token that closes each of
/// its statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    /// The batch the client sent: a statement ends with a DONE.
    Batch,
    /// The body a call runs, a dynamic text or the text of `sp_executesql`: a statement ends
    /// with a DONEINPROC ([MS-TDS] 2.2.7.7), and the call that opened the level closes it.
    Called,
}

/// What the run of a called level leaves for the call that opened it, besides its end.
#[derive(Debug, Default)]
pub(crate) struct LevelTrack {
    /// An error of the level, or of a level it called, reached the client.
    pub(crate) failed: bool,
    /// A statement of the level sent a result set.
    pub(crate) had_result_set: bool,
    /// The status of a level that runs past its last statement: the number of the error the
    /// last statement that ran ended with, 0 when it ended without one
    /// (`tests/nested.rs`, `return_status_is_the_error_number_after_a_runtime_error` and
    /// `return_status_is_zero_after_a_statement_scoped_error_the_text_recovers_from`).
    pub(crate) status: i32,
}

/// The status an error leaves to the level it ended: its number.
pub(crate) fn error_status(err: &SqlError) -> i32 {
    i32::try_from(err.number).unwrap_or(i32::MAX)
}

/// The variables of a body declared for the binder before its first statement: the
/// parameters of `sp_executesql`, an empty list for a batch.
fn declared_parameters(params: &[NestedParam]) -> SqlResult<BatchVariables> {
    let mut variables = BatchVariables::new();
    for param in params {
        variables.declare(&param.name, param.ty.clone())?;
    }
    Ok(variables)
}

/// The internal error a cancelled request ends with.
pub(crate) fn cancelled() -> SqlError {
    SqlError::from(InternalError::Bug(crate::cancel::CANCELLED.to_owned()))
}

/// The adapter that turns what the executor hands a statement of `level` into tokens: the
/// end of the statement is a DONE in the batch, a DONEINPROC in a called level, with `more`
/// in both, since what follows it is decided later ([`HeldDone`], or the call that closes
/// the level).
fn statement_adapter(sink: &mut dyn ResultSink, level: Level) -> StatementSinkAdapter<'_> {
    match level {
        Level::Batch => {
            let mut adapter = StatementSinkAdapter::new(sink);
            adapter.set_more(true);
            adapter
        }
        Level::Called => StatementSinkAdapter::in_proc(sink),
    }
}

/// The [`Host`] of the program of a body, the batch or a called level (module
/// documentation, "One program for the batch"): each statement runs in a transaction of its
/// own and ends with its DONE or its DONEINPROC, each error goes to the client on the line
/// of its statement, and each call goes through the dispatch of `procedures.rs`, which opens
/// the called level and runs its body through a host of its own.
struct BatchHost<'h> {
    session: &'h mut Session,
    sink: &'h mut dyn ResultSink,
    level: Level,
    track: &'h mut LevelTrack,
    text: &'h str,
    statements: &'h [Statement],
    slots: Vec<Slot>,
    /// How many statements, from the first, are bound: the length of the text, except in a
    /// body that moves the catalogue ([`batch_moves_the_catalogue`]).
    bound: usize,
    bind_state: SessionState,
    batch_variables: BatchVariables,
    /// The values a call wrote for its caller, written into the caller's level once the
    /// level of the call is closed ([`Host::leave_call`]).
    outputs: Vec<(String, Value)>,
    /// `@@ROWCOUNT` as a call whose body was refused before it ran found it: that body ran
    /// no statement, so the count outlives its error, which [`Host::raise`] sends next
    /// (`tests/nested.rs`, `a_dynamic_text_refused_before_it_ran_keeps_its_tokens_at_each_depth`).
    refused_rowcount: Option<i64>,
}

impl Host for BatchHost<'_> {
    fn statement(&mut self, stmt: Option<&PhysicalStatement>, site: Site) -> SqlResult<Ran> {
        // Between two statements: an ATTENTION that arrived during the previous one stops
        // the batch here. A `GOTO` that jumps backwards comes through this check on every
        // turn, so a batch that loops over itself answers the ATTENTION instead of running
        // on (`tests/run_batch_pipeline.rs`, `a_batch_that_loops_on_a_goto_answers_the_cancellation`).
        self.session.cancel.check()?;
        match stmt {
            Some(stmt) => self.run_bound(stmt, site, None),
            None => self.run_root(site),
        }
    }

    fn condition(
        &mut self,
        site: Site,
        evaluate: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<Value>,
    ) -> SqlResult<Result<bool, SqlError>> {
        let mut value = None;
        let opened = self.session.within_a_transaction(
            &mut |ctx: &mut ExecContext<'_>| {
                value = Some(evaluate(ctx));
                Ok(())
            },
            &mut *self.sink,
        );
        let evaluated = match (opened, value) {
            (Ok(()), Some(value)) => value,
            (Err(err), _) => Err(err),
            (Ok(()), None) => Err(SqlError::from(InternalError::Bug(
                "condition: nothing was evaluated".to_owned(),
            ))),
        };
        match evaluated {
            // The condition is a statement of its own: it leaves `@@ROWCOUNT` and `@@ERROR`
            // at 0 (`tests/run_batch_pipeline.rs`,
            // `a_condition_puts_the_row_count_and_the_last_error_back_to_zero`).
            Ok(value) => {
                self.session.state.rowcount = 0;
                self.session.state.last_error = 0;
                Ok(Ok(matches!(value, Value::Bit(true))))
            }
            // An error of the condition belongs to the statement that holds it, not to the
            // sub-expression that failed (`statements.rs`, `first_held_statement_line`).
            Err(err) => Ok(Err(self.place(err.with_line(0), site))),
        }
    }

    fn within(
        &mut self,
        _site: Site,
        step: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<()>,
    ) -> SqlResult<()> {
        self.session.within_a_transaction(step, &mut *self.sink)
    }

    fn locate(&self, err: SqlError, site: Site) -> SqlError {
        self.place(err, site)
    }

    // The error goes to the client, and `@@ERROR` takes its number while `@@ROWCOUNT` goes
    // back to 0. In the batch its statement ends with a DONE. In a called level it ends with
    // a DONEINPROC when the level goes on after it, and with nothing when the error ends
    // the level, whose end the call that opened it sends. An error whose catalogue entry
    // rolls the transaction back (the victim of a deadlock) does so here, after its ERROR
    // token and before the DONE (`tests/deadlock_victim.rs`).
    fn raise(&mut self, err: &SqlError, reach: ErrorScope) -> SqlResult<()> {
        self.session.state.last_error = err.number;
        self.session.state.rowcount = self.refused_rowcount.take().unwrap_or(0);
        self.sink.error(err)?;
        if error_behavior(err.number).rolls_back_transaction {
            let session = &mut *self.session;
            txn_session::roll_back_after_error(
                &mut session.state,
                &session.engine,
                &mut session.exec,
                &mut *self.sink,
            )?;
        }
        match self.level {
            Level::Batch => self.sink.done(None, true),
            Level::Called => {
                self.track.failed = true;
                self.track.status = error_status(err);
                if reach == ErrorScope::Statement {
                    self.sink.done_in_proc(None, true)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn call(&mut self, call: Call, _site: Site) -> SqlResult<Result<ProgramEnd, SqlError>> {
        let in_proc = self.level == Level::Called;
        let (end, outcome) = match call {
            Call::Procedure {
                name,
                database,
                args,
                return_into,
                line,
            } => crate::procedures::run_batch_procedure(
                self.session,
                &crate::procedures::BatchCall {
                    name: &name,
                    qualifier: database.as_deref(),
                    args: &args,
                    return_into: return_into.as_deref(),
                    line,
                    in_proc,
                },
                &mut *self.sink,
            )?,
            Call::Dynamic { text, .. } => {
                crate::procedures::run_batch_dynamic(self.session, &text, in_proc, &mut *self.sink)?
            }
        };
        if let Ok(ProgramEnd::Refused(_)) = &end {
            self.refused_rowcount = Some(self.session.state.rowcount);
        }
        if !outcome.failed {
            self.session.state.rowcount = outcome.rowcount;
        }
        // What the callee leaves to the status of this level: the error that refused the
        // call, the status of a callee that failed, nothing otherwise.
        self.track.failed |= outcome.failed;
        self.track.had_result_set |= outcome.had_result_set;
        self.track.status = match &end {
            Err(err) => error_status(err),
            Ok(_) if outcome.failed => outcome.return_status,
            Ok(_) => 0,
        };
        // What the dispatch wrote for the caller (`@rc =`, `OUTPUT`) went into the level
        // of the call, which is closed before the caller reads it.
        self.outputs = self
            .session
            .exec
            .variables
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        Ok(end)
    }

    fn leave_call(&mut self, database: &str) -> SqlResult<()> {
        database.clone_into(&mut self.session.state.database);
        for (name, value) in std::mem::take(&mut self.outputs) {
            self.session.exec.variables.insert(name, value);
        }
        Ok(())
    }

    // The test of an `IF` or a `WHILE`, a `GOTO`, a `BREAK`, a `CONTINUE` and a `RETURN`
    // each end with a DONE that carries no count (`tests/run_batch_pipeline.rs`,
    // `a_statement_of_the_control_of_flow_ends_with_a_done`), a DONEINPROC in a called
    // level.
    fn control(&mut self, _site: Site) -> SqlResult<()> {
        self.end_statement(None)
    }

    fn cancelled(&self) -> bool {
        self.session.cancel.is_cancelled()
    }

    fn xact_abort(&self) -> bool {
        self.session.state.options.xact_abort
    }
}

impl BatchHost<'_> {
    /// The error `err` raised at `site`, on the line the client reads it at.
    ///
    /// A statement written at the top level answers its own line ([`at_statement`]). A
    /// statement an `IF`, a `WHILE` or a `BEGIN … END` holds keeps the line the executor
    /// gave it when that line is at or past the first statement held, and answers the line
    /// of the statement written at the top level otherwise: an error of a condition comes
    /// with no line of its own ([`first_held_statement_line`]). A called level reads the
    /// lines of its own text (`tests/nested.rs`, `error_line_is_relative_to_the_nested_text`).
    fn place(&self, err: SqlError, site: Site) -> SqlError {
        let Some(root) = self.statements.get(site.root) else {
            return err;
        };
        if site.nested
            && let Some(first) = first_held_statement_line(root)
            && err.line >= first
        {
            return err;
        }
        at_statement(err, statement_line(root))
    }

    /// The line the statement written at `site` starts on.
    fn line_of(&self, site: Site) -> u32 {
        self.statements.get(site.root).map_or(0, statement_line)
    }

    /// Closes a statement that ended without an error, with a DONE in the batch and a
    /// DONEINPROC in a called level, which `SET NOCOUNT ON` leaves out
    /// ([`StatementEnd::Silent`]).
    fn end_statement(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.track.status = 0;
        match self.level {
            Level::Batch => self.sink.done(rowcount, true),
            Level::Called if self.session.state.options.nocount => Ok(()),
            Level::Called => self.sink.done_in_proc(rowcount, true),
        }
    }

    /// Runs the statement written at the top level at `site`, bound now when the batch is
    /// bound one statement at a time.
    fn run_root(&mut self, site: Site) -> SqlResult<Ran> {
        let index = site.root;
        if let Err(err) = self.bind_up_to(index)? {
            return Ok(Ran::Stopped(err));
        }
        let Some(slot) = self.slots.get_mut(index) else {
            return Err(SqlError::from(InternalError::Bug(format!(
                "run_root: the batch holds no statement {index}"
            ))));
        };
        let slot = std::mem::replace(slot, Slot::Running);
        let ran = match &slot {
            Slot::Ready(prepared) => self.run_prepared(prepared, site),
            Slot::Unbound | Slot::InProgram | Slot::Running => Err(SqlError::from(
                InternalError::Bug(format!("run_root: statement {index} is not ready")),
            )),
        };
        self.slots[index] = slot;
        ran
    }

    /// Binds the statements up to `index` that are not bound yet, in the order of the
    /// text, against the catalogue of this moment. A `DECLARE` among them enters its names,
    /// whether the program runs it or jumps over it. The inner `Err` is a binding error.
    fn bind_up_to(&mut self, index: usize) -> SqlResult<Result<(), SqlError>> {
        while self.bound <= index {
            let from = self.bound;
            let tail = match self.session.reprepare_tail(
                self.text,
                &self.statements[from..],
                &mut self.bind_state,
                &mut self.batch_variables,
            ) {
                Ok(tail) => tail,
                Err(err) if self.session.cancel.is_cancelled() => return Err(err),
                Err(err) => return Ok(Err(err)),
            };
            if tail.is_empty() {
                break;
            }
            for prepared in tail {
                if let PreparedStatement::Bound { statement } = &prepared {
                    declare_variables_held_by(statement, &mut self.session.exec);
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

    /// Runs one prepared statement written at the top level.
    fn run_prepared(&mut self, prepared: &PreparedStatement, site: Site) -> SqlResult<Ran> {
        match prepared {
            PreparedStatement::Fallback { text, error } => self.run_fallback(text, error),
            PreparedStatement::Deferred {
                padded,
                raw,
                variables,
            } => self.run_deferred(padded, raw, variables, site),
            // A `USE` is a bound statement with one thing more: the name its ENVCHANGE and
            // its INFO 5701 carry, resolved against the catalogue while the batch was bound.
            PreparedStatement::Use {
                statement,
                database,
            } => self.run_bound(statement, site, Some(database)),
            PreparedStatement::Execute { stmt, line } => self.run_execute(stmt, *line),
            PreparedStatement::Bound { statement } if compiled_in_place(statement) => {
                Ok(Ran::Holds(Box::new(statement.clone())))
            }
            PreparedStatement::Bound { statement } => self.run_bound(statement, site, None),
        }
    }

    /// A `SET <option>` applied to the options of the running level, written at the top
    /// level of the body or held by a statement of the program at `site`
    /// (`tests/set_options_effects.rs`, `a_set_held_by_a_block_or_a_branch_changes_the_options`).
    /// A `SET` that is refused ends the program, on its own line.
    fn run_set(&mut self, set: &SetOptionStatement, site: Site) -> SqlResult<Ran> {
        let session = &mut *self.session;
        let snapshot = if let Some(session_txn) = &session.state.txn {
            session.engine.catalog.snapshot(&session_txn.handle)
        } else {
            let binding = crate::set_options::begin_autocommit_txn(
                &session.state,
                &session.engine.storage,
                &session.engine.txn,
            )?;
            let snap = session.engine.catalog.snapshot(&binding);
            session.engine.txn.commit(binding)?;
            snap
        };
        match apply_set_option(&mut session.state, set, Some(&snapshot)) {
            SetOutcome::Applied | SetOutcome::Ignored => {}
            SetOutcome::Failed(err) => {
                return Ok(Ran::Stopped(self.place(err.with_line(set.span.line), site)));
            }
        }
        // A `SET` carries no row count, under both values of `NOCOUNT`: its DONE is
        // `0x0001` (MORE) before a statement and `0x0000` last, without `DONE_COUNT`.
        self.end_statement(None)?;
        // A `SET` puts `@@ROWCOUNT` back to 0: `SELECT 1; SET NOCOUNT OFF; SELECT
        // @@ROWCOUNT;` answers 1 then **0**, where `SELECT 1; SELECT @@ROWCOUNT;`
        // answers 1 then 1, the vector that tells "a `SET` resets the count" from "a
        // `SET` leaves it alone" (`tests/run_batch_pipeline.rs`,
        // `a_set_puts_the_row_count_back_to_zero`).
        let session = &mut *self.session;
        session.state.rowcount = 0;
        apply_exec_session(&mut session.state, &session.exec, true, true);
        session.exec.identity_updated = false;
        Ok(Ran::Outcome(ExecOutcome::NoRows))
    }

    /// Last resort for a statement the binder does not bind: the fake engine, then the
    /// internal error itself when the fake engine does not know it either. Either error
    /// ends the program.
    fn run_fallback(&mut self, text: &str, error: &SqlError) -> SqlResult<Ran> {
        match fake_engine::answer(&self.session.cancel, text, true, &mut *self.sink)? {
            Some(true) => Ok(Ran::Outcome(ExecOutcome::NoRows)),
            Some(false) => Ok(Ran::Reported(SqlError::from(InternalError::Bug(
                "the fallback answered an error".to_owned(),
            )))),
            None => Ok(Ran::Stopped(error.clone())),
        }
    }

    /// Binds a deferred statement against the catalogue as it stands when the program
    /// reaches it, then runs it like the statement it turned out to be.
    ///
    /// A binding that fails now is a run-time error of the statement: what came before it
    /// has produced its results, and the catalogued scope of the number decides whether
    /// the batch goes on. 1088 stops the batch, and 208, 207 and 4902 stop their level,
    /// which is the batch here, so
    /// `SELECT 1; SELECT * FROM dbo.t; CREATE TABLE dbo.t (a int); SELECT 2;` answers the
    /// row of the first `SELECT`, then 208 on the line of the second, and no `2`
    /// (`tests/run_batch_pipeline.rs`, `a_select_before_the_create_is_208_at_run_time`).
    fn run_deferred(
        &mut self,
        padded: &str,
        raw: &str,
        variables: &BatchVariables,
        site: Site,
    ) -> SqlResult<Ran> {
        match self.session.bind_deferred(padded, raw, variables) {
            Ok(PreparedStatement::Bound { statement }) if compiled_in_place(&statement) => {
                Ok(Ran::Holds(Box::new(statement)))
            }
            Ok(prepared) => self.run_prepared(&prepared, site),
            Err(err) if self.session.cancel.is_cancelled() => Err(err),
            Err(err) => Ok(Ran::Failed(err)),
        }
    }

    /// Runs a bound `EXECUTE` written at the top level: what it answers is the call the
    /// program runs in place.
    fn run_execute(&mut self, stmt: &BoundExecute, line: u32) -> SqlResult<Ran> {
        let session = &mut *self.session;
        session.exec.identity_updated = false;
        session.exec.rowcount = 0;
        let mut adapter = statement_adapter(&mut *self.sink, self.level);
        let (result, txn) = session.execute_bound_in_a_transaction(stmt, &mut adapter);
        let succeeded = result.is_ok();
        txn_session::finish_statement(
            &mut session.state,
            &session.engine,
            &mut session.exec,
            txn,
            txn_session::TxnKind::Other,
            succeeded,
            adapter.tds(),
        )?;
        match result {
            Ok(outcome) => Ok(Ran::Outcome(outcome)),
            Err(err) => {
                let err = at_statement(err, line);
                let session = &mut *self.session;
                apply_exec_session(&mut session.state, &session.exec, true, false);
                session.exec.identity_updated = false;
                Ok(Ran::Failed(err))
            }
        }
    }

    /// Runs one bound statement that holds no other one, written at the top level or held
    /// by a statement of the program, and closes it with its DONE. `target` is the database
    /// of a `USE`.
    fn run_bound(
        &mut self,
        bound: &PhysicalStatement,
        site: Site,
        target: Option<&str>,
    ) -> SqlResult<Ran> {
        if let PhysicalStatement::SetOption(set) = bound {
            return self.run_set(set, site);
        }
        let line = self.line_of(site);
        // The adapter wraps the caller's sink: the executor calls
        // `StatementSink::columns` during execution, which the adapter converts to
        // `ResultSink::columns`, so COLMETADATA goes out when the statement starts to
        // run, not while it is prepared.
        let session = &mut *self.session;
        session.exec.identity_updated = false;
        session.exec.rowcount = 0;
        let mut adapter = statement_adapter(&mut *self.sink, self.level);
        let (result, txn) = session.execute_in_a_transaction(bound, &mut adapter);
        let succeeded = result.is_ok();
        // The ENVCHANGE of an opening or a closing transaction goes out before the DONE of
        // the statement that caused it.
        txn_session::finish_statement(
            &mut session.state,
            &session.engine,
            &mut session.exec,
            txn,
            txn_session::kind_of(bound),
            succeeded,
            adapter.tds(),
        )?;
        match result {
            Ok(ExecOutcome::Rows(count)) => {
                // The metadata was already sent through the adapter during execution.
                // `SET NOCOUNT ON` clears `DONE_COUNT` on the DONE of a `SELECT`
                // ([MS-TDS] 2.2.7.6): `SET NOCOUNT ON; SELECT 1;` answers a DONE with
                // status `0x0000` where `SET NOCOUNT OFF; SELECT 1;` answers `0x0010`, and
                // the `SET` reaches the `SELECT` of its own batch in both directions
                // (`tests/set_options_effects.rs`). SQL Server still writes the count in
                // `DoneRowCount` with the flag clear; the spec says the field is then not
                // valid, the sink has no way to write it, and a client following [MS-TDS]
                // does not read it, so `None` it is.
                let reported = if session.state.options.nocount {
                    None
                } else {
                    Some(count)
                };
                adapter.done(reported)?;
                self.track.status = 0;
                self.track.had_result_set = true;
                // `@@ROWCOUNT` is posted **after** the statement and read by the next one:
                // `SELECT 1; SELECT @@ROWCOUNT` answers 1, and so it does under `NOCOUNT
                // ON`: the option touches the DONE, not the variable.
                session.state.rowcount = i64::try_from(count).unwrap_or(i64::MAX);
                apply_exec_session(&mut session.state, &session.exec, true, true);
                session.exec.identity_updated = false;
                Ok(Ran::Outcome(ExecOutcome::Rows(count)))
            }
            // The DDL and `USE` answer `NoRows` (`executor`, `ddl.rs`); a `SELECT`
            // answers `Rows`. A `NoRows` sent no COLMETADATA, so the DONE closes a
            // statement with no result set, and it carries no row count, the shape of
            // `CREATE TABLE`, `DROP TABLE` and `USE`.
            Ok(ExecOutcome::NoRows) => {
                if let Some(database) = target {
                    session.switch_database(database, line, &mut adapter)?;
                }
                // A `DECLARE` sets `@@ROWCOUNT` by its own rule, over what the statement
                // left in `exec.rowcount` (`next_rowcount`).
                let rowcount = next_rowcount(bound, session.state.rowcount, session.exec.rowcount);
                match quiet_end(bound, rowcount, session.state.options.nocount, self.level) {
                    StatementEnd::Silent => {}
                    StatementEnd::Done(reported) => {
                        if let Some(cur_cmd) = cur_cmd_of(bound) {
                            adapter.tds().next_cur_cmd(cur_cmd)?;
                        }
                        adapter.done(reported)?;
                    }
                }
                self.track.status = 0;
                session.state.rowcount = rowcount;
                apply_exec_session(
                    &mut session.state,
                    &session.exec,
                    true,
                    statement_clears_last_error(bound),
                );
                session.exec.identity_updated = false;
                Ok(Ran::Outcome(ExecOutcome::NoRows))
            }
            // A call, a cancellation, an error of the whole batch: the program acts on it.
            Ok(outcome) => Ok(Ran::Outcome(outcome)),
            // A run-time error stops its statement; its catalogued scope and XACT_ABORT
            // decide whether the program goes on. It is not handed to the fake engine even
            // when it is internal: a cancelled request raises one, and running it again
            // would ignore the ATTENTION.
            Err(err) => Ok(Ran::Failed(self.place(err, site))),
        }
    }
}

/// How a statement that ended without an error and without a result set closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementEnd {
    /// No token.
    Silent,
    /// A DONE in the batch, a DONEINPROC in a called level, with this row count.
    Done(Option<u64>),
}

/// The end of `stmt`, a statement that sent no result set, run at `level` and leaving
/// `rowcount` in `@@ROWCOUNT`.
///
/// - A `DECLARE` that gives no variable a value sends nothing: `DECLARE @x int; SELECT 1`
///   answers the result set and its DONE alone.
/// - In a called level, `SET NOCOUNT ON` silences the statement; in the batch it clears
///   the count and keeps the DONE.
/// - Under `SET NOCOUNT OFF`, the statements that write rows or variables carry the count
///   they leave in `@@ROWCOUNT`: `INSERT`, `UPDATE`, `DELETE` and `SELECT … INTO` the rows
///   they wrote, `0` included; `SET @v`, `SELECT @v = …` and a `DECLARE` that gives a value
///   their own count. The other statements (DDL, `TRUNCATE TABLE`, `USE`, `PRINT`, a
///   transaction statement) carry no count.
///
/// `tests/row_counts.rs` pins each of these shapes, in the batch, in `EXEC('…')` and in
/// `sp_executesql`.
fn quiet_end(stmt: &PhysicalStatement, rowcount: i64, nocount: bool, level: Level) -> StatementEnd {
    if let PhysicalStatement::Declare(declarations) = stmt
        && declarations
            .iter()
            .all(|declaration| declaration.value.is_none())
    {
        return StatementEnd::Silent;
    }
    if nocount {
        return match level {
            Level::Batch => StatementEnd::Done(None),
            Level::Called => StatementEnd::Silent,
        };
    }
    let counted = matches!(
        stmt,
        PhysicalStatement::Insert(_)
            | PhysicalStatement::Update(_)
            | PhysicalStatement::Delete(_)
            | PhysicalStatement::SelectInto(_)
            | PhysicalStatement::SetVariable { .. }
            | PhysicalStatement::SelectAssign { .. }
            | PhysicalStatement::Declare(_)
    );
    StatementEnd::Done(counted.then(|| u64::try_from(rowcount).unwrap_or(0)))
}

/// The `CurCmd` of the DONE of `stmt` when it is not 0: `INSERT`, `UPDATE`, `DELETE` and
/// `SELECT … INTO` have their own, and `SET @v`, `SELECT @v = …` and a `DECLARE` that
/// gives a value close as a `SELECT` does, so a driver does not add their count to the
/// rows a batch affected (`tests/row_counts.rs`,
/// `the_done_of_each_statement_names_its_kind`).
fn cur_cmd_of(stmt: &PhysicalStatement) -> Option<u16> {
    match stmt {
        PhysicalStatement::Insert(_) => Some(CUR_CMD_INSERT),
        PhysicalStatement::Update(_) => Some(CUR_CMD_UPDATE),
        PhysicalStatement::Delete(_) => Some(CUR_CMD_DELETE),
        PhysicalStatement::SelectInto(_) => Some(CUR_CMD_SELECT_INTO),
        PhysicalStatement::SetVariable { .. }
        | PhysicalStatement::SelectAssign { .. }
        | PhysicalStatement::Declare(_) => Some(CUR_CMD_SELECT),
        _ => None,
    }
}

/// A [`ResultSink`] that holds the last DONE back until the next token, so the DONE
/// carries `MORE` exactly when something follows it in the response, wherever the program
/// jumps: the last DONE of the response goes out without it ([`HeldDone::finish`]).
struct HeldDone<'s> {
    sink: &'s mut dyn ResultSink,
    /// The row count and the `CurCmd` of the DONE held back.
    held: Option<(Option<u64>, Option<u16>)>,
    /// The `CurCmd` named for the next DONE or DONEINPROC, not passed on yet.
    cur_cmd: Option<u16>,
    /// A DONE went out.
    sent: bool,
}

impl<'s> HeldDone<'s> {
    fn new(sink: &'s mut dyn ResultSink) -> Self {
        Self {
            sink,
            held: None,
            cur_cmd: None,
            sent: false,
        }
    }

    /// Sends the DONE held back, with `more`.
    fn release(&mut self, more: bool) -> SqlResult<()> {
        match self.held.take() {
            Some((rowcount, cur_cmd)) => {
                self.sent = true;
                if let Some(cur_cmd) = cur_cmd {
                    self.sink.next_cur_cmd(cur_cmd)?;
                }
                self.sink.done(rowcount, more)
            }
            None => Ok(()),
        }
    }

    /// Closes the response: the DONE held back goes out without `MORE`, and a response
    /// that sent no DONE gets one, without which the client would wait for ever.
    fn finish(mut self) -> SqlResult<()> {
        if self.held.is_some() {
            self.release(false)
        } else if self.sent {
            Ok(())
        } else {
            self.sink.done(None, false)
        }
    }
}

impl ResultSink for HeldDone<'_> {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.release(true)?;
        self.sink.columns(cols)
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.release(true)?;
        self.sink.row(row)
    }

    fn done(&mut self, rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        self.release(true)?;
        self.held = Some((rowcount, self.cur_cmd.take()));
        Ok(())
    }

    fn next_cur_cmd(&mut self, cur_cmd: u16) -> SqlResult<()> {
        self.cur_cmd = Some(cur_cmd);
        Ok(())
    }

    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
        self.release(true)?;
        self.sink.info(msg)
    }

    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.release(true)?;
        self.sink.error(err)
    }

    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.release(true)?;
        self.sink.env_change(change)
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.release(true)?;
        self.sink.return_value(name, ty, value)
    }

    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.release(true)?;
        self.sink.return_status(status)
    }

    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.release(true)?;
        if let Some(cur_cmd) = self.cur_cmd.take() {
            self.sink.next_cur_cmd(cur_cmd)?;
        }
        self.sink.done_in_proc(rowcount, more)
    }

    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.release(true)?;
        self.sink.done_proc(rowcount)
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
/// [`StatementSinkAdapter`].
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

/// Whether the batch still holds, at any depth, a statement that changes or removes a
/// table or an index the catalogue holds, or that undoes what its transaction did to them:
/// `ALTER TABLE`, `DROP TABLE`, `DROP INDEX`, `ROLLBACK`, and an `EXECUTE`, whose text or
/// procedure may run one of them. A statement bound before it has run would read the shape
/// it leaves behind as it was before: the `CREATE TABLE` of a name a `DROP TABLE` frees, the
/// `SELECT` after a `ROLLBACK` of an `ALTER TABLE`, or the `SELECT` after an `EXEC` that
/// recreates its table (`tests/recreate_in_one_batch.rs`).
pub(crate) fn batch_moves_the_catalogue(statements: &[Statement]) -> bool {
    statements.iter().any(moves_the_catalogue)
}

/// [`batch_moves_the_catalogue`] for one statement and the statements it holds.
fn moves_the_catalogue(statement: &Statement) -> bool {
    match statement {
        Statement::AlterTable(_)
        | Statement::DropTable { .. }
        | Statement::DropIndex(_)
        | Statement::Rollback { .. }
        | Statement::Execute(_) => true,
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            moves_the_catalogue(then_branch)
                || else_branch.as_deref().is_some_and(moves_the_catalogue)
        }
        Statement::While { body, .. } => moves_the_catalogue(body),
        Statement::Block { statements, .. } => batch_moves_the_catalogue(statements),
        Statement::TryCatch {
            try_block,
            catch_block,
            ..
        } => batch_moves_the_catalogue(try_block) || batch_moves_the_catalogue(catch_block),
        _ => false,
    }
}

/// How many statements to bind at once when the batch will reach a statement that moves
/// the catalogue ([`batch_moves_the_catalogue`]).
///
/// A `CREATE TABLE` before an `ALTER` must run before the `ALTER` is bound, and a
/// `DROP TABLE`, a `ROLLBACK` or an `EXECUTE` before the statements after it, so the tail is
/// prepared one statement at a time up to the last of them.
pub(crate) fn bind_limit_for(statements: &[Statement]) -> Option<usize> {
    batch_moves_the_catalogue(statements).then_some(1)
}

/// Whether `bound` is a `CREATE TABLE`: a statement bound as one, or refused with the 2714
/// state 6 of a name already taken.
fn creates_a_table(bound: &SqlResult<PreparedStatement>) -> bool {
    match bound {
        Ok(PreparedStatement::Bound {
            statement: PhysicalStatement::Ddl(DdlStatement::CreateTable { .. }),
        }) => true,
        Err(err) => err.number == 2714 && err.state == 6,
        Ok(_) => false,
    }
}

/// The catalogue a statement is bound against, for a session in `state`: what the open
/// transaction of the session sees when there is one, what `binding` sees otherwise.
pub(crate) fn binding_snapshot(
    engine: &Engine,
    state: &SessionState,
    binding: &TxnHandle,
) -> CatalogSnapshot {
    match &state.txn {
        Some(session_txn) => engine.catalog.snapshot(&session_txn.handle),
        None => engine.catalog.snapshot(binding),
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
    /// (comment of the `match` in [`BatchHost::run_bound`]).
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
    /// column stops the batch with 207 after the DDL committed, the error sent to the
    /// client like any other.
    #[test]
    fn create_alter_foreign_key_then_bad_select_in_one_batch() {
        let engine = engine();
        let mut session = session(&engine);
        let mut sink = Received::default();
        session
            .run_batch(
                "CREATE TABLE dbo.s16g_p (id int NOT NULL PRIMARY KEY);\
                 CREATE TABLE dbo.s16g_c (id int NOT NULL, pid int);\
                 ALTER TABLE dbo.s16g_c ADD CONSTRAINT fk_s16g FOREIGN KEY (pid)\
                 REFERENCES dbo.s16g_p(id);\
                 SELECT nosuch FROM dbo.s16g_c;",
                &mut sink,
            )
            .expect("a bind error on the tail reaches the client, not the connection");
        let numbers: Vec<u32> = sink.errors.iter().map(|err| err.number).collect();
        assert_eq!(numbers, [207]);
        assert!(sink.columns.is_empty(), "the SELECT did not run");
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
    /// result set for the `SELECT 2`, 2714 being batch-scoped ([`vauban_errors::ErrorScope`]). The
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
