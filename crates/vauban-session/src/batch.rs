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
//! this layer sends them, until [`ResultSink`] implements `executor::RowSink` itself.
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
//!
//! The three frontiers:
//!
//! - a **syntax** error kills the batch, because `parse_batch` reads the whole text before
//!   the first statement runs (`syntax_error_stops_the_batch`);
//! - a **binding or compile-time** error kills the batch too: `prepare_batch` binds and checks
//!   each statement before execution emits any row
//!   (`a_compilation_error_silences_the_whole_batch`);
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
//! `executor::compile` for every statement; only after they all succeed does
//! `run_prepared` send COLMETADATA and call `executor::execute`. A compilation error reaches
//! the client with **no** result set (`a_bind_error_sends_no_metadata_at_all`), a run-time
//! error with a result set that carries the columns and no row.
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

use std::sync::{Arc, OnceLock};

use vauban_binder::{BatchVariables, BindContext, BoundStatement, OutputSchema};
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{BatchErrorScope, InternalError, SqlError, SqlResult};
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, RowSink};
use vauban_parser::{Statement, parse_batch};
use vauban_planner::{PhysicalStatement, PlanContext, StorageIndexes};
use vauban_tds::{ColumnFlags, ColumnMeta, EnvChange, Rpc, RpcProc};
use vauban_txn::IsolationLevel;
use vauban_types::{TypeInfo, Value};

use crate::cancel::CancelHandle;
use crate::eval_context::SessionEvalContext;
use crate::fake_engine;
use crate::login::{DATABASE_CONTEXT_STATE_USE, changed_database_context};
use crate::server::Engine;
use crate::set_options::{SetOutcome, apply_set_statement};
use crate::sink::{ResultSink, RowSinkAdapter};
use crate::state::SessionState;
use crate::txn_session::{self, StatementTxn};

/// Number of the generic internal error (`errors`, `InternalError` → `SqlError`): the
/// binder answers it for each statement that is not implemented yet.
const INTERNAL_ERROR: u32 = 50000;

/// Default schema of a login, `dbo` until `catalog` knows better.
const DEFAULT_SCHEMA: &str = "dbo";

/// One column returned by a registered system procedure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemProcedureColumn {
    /// Column name sent in COLMETADATA.
    pub name: String,
    /// SQL type, nullability and collation sent in COLMETADATA.
    pub ty: TypeInfo,
}

/// Result of a system procedure, independent of TDS and [`ResultSink`].
#[derive(Debug, Clone, PartialEq)]
pub struct SystemProcedureResult {
    /// Metadata of the procedure's single result set.
    pub columns: Vec<SystemProcedureColumn>,
    /// Rows of that result set.
    pub rows: Vec<Vec<Value>>,
    /// RETURNSTATUS value emitted after the rows.
    pub return_status: i32,
}

/// Function registered by `vauban-compat` at process start-up.
pub type SystemProcedureDispatcher =
    fn(&str, &[(Option<&str>, &Value)]) -> Option<SystemProcedureResult>;

static SYSTEM_PROCEDURE_DISPATCHER: OnceLock<SystemProcedureDispatcher> = OnceLock::new();

/// Registers the compatibility-layer dispatcher without introducing a `session → compat`
/// dependency cycle. Repeated registration is harmless, like the function registry.
pub fn register_system_procedure_dispatcher(dispatcher: SystemProcedureDispatcher) {
    let _ = SYSTEM_PROCEDURE_DISPATCHER.set(dispatcher);
}

/// The numbers that come out of `executor::execute` with a line of their own to keep.
///
/// 127 (`TOP` row count below zero) and 1060 (`TOP` row count `NULL`) are the two checks
/// the executor performs whose line SQL Server counts on the **row-count expression** of
/// the `TOP` rather than on the statement (module header). `executor` already puts that
/// line on them, so [`at_statement`] steps aside.
///
/// These two are not a class: 1062 and 1031, raised while the very same `TOP` clause is
/// compiled, answer the last line of the statement and the first line of the select list
/// respectively on SQL Server, and neither of those is the row count (module header).
const COMPILED_WITH_THE_STATEMENT: [u32; 2] = [127, 1060];

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
    /// Rolls back a transaction the connection left open. The full release (locks given
    /// back, a waiting request cut) is the connection-teardown work; this one cancels the
    /// open transaction and clears its descriptor.
    fn drop(&mut self) {
        let _ = txn_session::rollback_all(&mut self.state, &self.engine);
    }
}

/// What one statement of a batch decided about the rest of it.
enum Flow {
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
        let prepared = match self.prepare_batch(text, &batch.statements) {
            Ok(prepared) => prepared,
            Err(err) if self.cancel.is_cancelled() => return Err(err),
            Err(err) => return self.fail(&err, sink),
        };
        let last = prepared.len() - 1;
        for (index, statement) in prepared.iter().enumerate() {
            // Between two statements, as `fake_engine` did before its own work: an
            // ATTENTION that arrived during the previous one stops the batch here.
            self.cancel.check()?;
            let more = index < last;
            if let Flow::Stop = self.run_prepared(statement, more, sink)? {
                break;
            }
        }
        // A batch may end with a transaction still open; what the end of a batch does with
        // it is `txn_session::end_of_batch`'s decision.
        txn_session::end_of_batch(&self.state, sink)?;
        Ok(())
    }

    /// Parses, binds and compile-checks every statement without changing live session state.
    /// A failure therefore reaches the sink before an earlier statement can emit a result set.
    ///
    /// # The transaction of the binding, and the one of each statement
    ///
    /// The binder needs a [`CatalogSnapshot`], and a snapshot is taken through a
    /// transaction. This one is opened here, read from, and closed **before** the first
    /// statement runs, so that no transaction stays open for the length of a batch: the
    /// transaction a statement writes in is the one
    /// [`Session::execute_in_a_transaction`] opens for it.
    ///
    /// Closing it makes the whole batch bind against the catalogue as it stood **before**
    /// the batch, which is a deliberate difference from SQL Server: a batch that creates
    /// a table and reads it needs deferred name resolution, and this engine has none.
    /// `CREATE TABLE dbo.s16b (a int); SELECT * FROM dbo.s16b;` in one batch answers one
    /// result set with the column `a` and no row on SQL Server, where the same text here
    /// answers 208 at binding time and runs neither statement (unit test
    /// `create_table_then_select_in_the_same_batch_is_208`). Two batches on one connection
    /// answer the same thing here as on SQL Server
    /// (`create_table_then_select_star_same_connection`).
    ///
    /// The vector that separates "bound before the batch" from "bound after the previous
    /// statement" the other way round is a name the batch **creates twice**:
    /// `CREATE TABLE t; CREATE TABLE t;`. Both statements bind here, because the snapshot
    /// shows neither, and the second one raises 2714 while it runs, which is where SQL
    /// Server raises it too: on
    /// `SELECT 1; CREATE TABLE dbo.s16d (a int); CREATE TABLE dbo.s16d (a int); SELECT 2;`,
    /// one result set (the `SELECT 1`), then 2714 severity 16 state 6 on the line of the
    /// second `CREATE TABLE`, and no result set for the `SELECT 2`, 2714 being a
    /// batch-scoped number ([`BatchErrorScope`], `errors/catalog.rs`). Pinned by
    /// `create_table_twice_in_one_batch_is_2714_at_run_time`.
    fn prepare_batch(
        &self,
        text: &str,
        statements: &[Statement],
    ) -> SqlResult<Vec<PreparedStatement>> {
        let binding = self.engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.engine.catalog.snapshot(&binding);
        let prepared = self.bind_batch(text, statements, &snapshot);
        // The binding transaction wrote nothing, so it is committed rather than rolled
        // back in both branches of the binding, the successful one and the failing one: a
        // rollback would run the compensations of a transaction that created nothing. Its
        // own failure is internal, and it does not hide the error the binding already found.
        let closed = self.engine.txn.commit(binding);
        match (prepared, closed) {
            (Ok(prepared), Ok(())) => Ok(prepared),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    /// Binds and compile-checks the statements of the batch against `snapshot`, the
    /// catalogue as it
    /// stood when [`Session::prepare_batch`] opened its transaction.
    ///
    /// # Where the 911 of a `USE` belongs, and why it is here
    ///
    /// On SQL Server a `USE` that names a database which is not there is a
    /// **compilation** error: nothing of the batch runs, not even the statements written
    /// before it. The same here, each batch carrying a statement after the failing one so
    /// that "the batch stops" and "the batch goes on" answer differently (unit tests
    /// `use_unknown_is_911`, `a_database_created_by_the_batch_is_not_usable_by_it`):
    ///
    /// | batch | result |
    /// |---|---|
    /// | `SELECT 1; USE nosuchdb; SELECT 2;` | 911, **not one** result set |
    /// | `CREATE DATABASE d; USE d; SELECT DB_NAME();` (one batch) | 911, and `d` is **not** created |
    /// | `USE master; USE nosuchdb; SELECT 2;` | 911, no result set |
    /// | `SELECT 1; USE d; SELECT 2;` (`d` exists) | row `1`, 5701, row `2` |
    ///
    /// The second row is the one that separates a compile-time check from a run-time one:
    /// a 911 raised while the statement ran would have let the `CREATE DATABASE` before it
    /// through. Raising it here, where the batch is bound, is the same frontier as SQL
    /// Server's.
    ///
    /// The line is the one of the **name**, not of the statement
    /// ([`use_name_line`]), and the batch is left alone by
    /// [`BatchErrorScope`]: `prepare_batch` fails, so `run_batch` sends one ERROR and one
    /// DONE.
    ///
    /// What this layer does **not** reproduce is the order between a 911 and a **syntax**
    /// error of a later statement: `USE nosuchdb; SELEC 2;` answers 911 on SQL Server,
    /// which compiles statement by statement, and 102 here, because `parse_batch` reads
    /// the whole text first (module header). A binding error of a later statement does
    /// fall on the same side as SQL Server (911 both times).
    fn bind_batch(
        &self,
        text: &str,
        statements: &[Statement],
        snapshot: &CatalogSnapshot,
    ) -> SqlResult<Vec<PreparedStatement>> {
        let mut state = self.state.clone();
        let mut batch_variables = BatchVariables::new();
        let mut prepared = Vec::with_capacity(statements.len());

        for (index, original) in statements.iter().enumerate() {
            self.cancel.check()?;
            let raw = statement_text(text, original);
            if matches!(original, Statement::SetOption(_)) {
                // SET options affect compilation of following statements but are not committed
                // to the connection unless the whole batch compiles and execution reaches them.
                apply_set_statement(&mut state, &raw);
                prepared.push(PreparedStatement::Set {
                    text: raw,
                    line: statement_span(original).line,
                });
                continue;
            }

            // The first parse establishes safe statement boundaries for the whole batch. Reparse
            // each statement under the options produced by preceding SETs. Leading newlines keep
            // client-visible error lines anchored in the original batch.
            let line = statement_line(original);
            let source = statement_fragment(text, original, statements.get(index + 1));
            let padded = format!("{}{}", "\n".repeat(line.saturating_sub(1) as usize), source);
            let reparsed = parse_batch(&padded, &state.options.parse_options())?;
            let [statement] = reparsed.statements.as_slice() else {
                return Err(SqlError::from(vauban_errors::InternalError::Bug(
                    "one statement reparsed as a different statement count".to_owned(),
                )));
            };

            let options = state.binder_options();
            let ctx = BindContext {
                text: &padded,
                catalog: Some(snapshot),
                database: &state.database,
                default_schema: DEFAULT_SCHEMA,
                variables: &batch_variables,
                options,
            };
            let bound = match vauban_binder::bind(statement, &ctx) {
                Ok(bound) => bound,
                Err(error)
                    if error.number == INTERNAL_ERROR
                        && matches!(statement, Statement::Waitfor(_)) =>
                {
                    prepared.push(PreparedStatement::Fallback { text: raw, error });
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let BoundStatement::Declare(declarations) = &bound {
                for declaration in declarations {
                    batch_variables.declare(&declaration.name, declaration.ty.clone())?;
                }
            }

            // The planner reads the indexes of the storage; the rules that would choose
            // one are not written, so a read is planned as a scan.
            let indexes = StorageIndexes(self.engine.storage.as_ref());
            let physical = vauban_planner::plan(bound, &PlanContext { catalog: &indexes })
                .map_err(|error| at_statement(error, statement_line(statement)))?;

            // The compiler folds what it can, so the context it gets reads the same
            // catalogue as the binder: `snapshot` (`eval_context.rs`).
            let eval = SessionEvalContext::new(&state, Some(snapshot));
            let mut exec = ExecContext::scalar(&eval, state.options.to_binder());
            vauban_executor::compile(&physical, &mut exec)
                .map_err(|error| at_statement(error, statement_line(statement)))?;

            if let PhysicalStatement::Use { database } = &physical {
                let found = snapshot
                    .database(database)
                    .map(|target| target.name.clone())
                    .ok_or_else(|| {
                        SqlError::database_not_found(database)
                            .with_line(use_name_line(&padded, statement))
                    })?;
                // The statements that follow bind in the new database, as they do on SQL
                // Server: `USE d; SELECT 1 FROM dbo.only_in_master;` answers 208
                // (`use_moves_the_binding_of_the_rest_of_the_batch`).
                state.database = found.clone();
                prepared.push(PreparedStatement::Use {
                    statement: physical,
                    database: found,
                    line,
                });
                continue;
            }

            prepared.push(PreparedStatement::Bound {
                statement: physical,
                line: statement_line(statement),
            });
        }
        Ok(prepared)
    }

    /// Executes one already compiled statement and says whether the batch goes on.
    fn run_prepared(
        &mut self,
        prepared: &PreparedStatement,
        more: bool,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<Flow> {
        if let PreparedStatement::Set { text, line } = prepared {
            match apply_set_statement(&mut self.state, text) {
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
            return Ok(Flow::Continue);
        }

        if let PreparedStatement::Fallback { text, error } = prepared {
            return self.fall_back(text, error, more, sink);
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
            PreparedStatement::Set { .. } | PreparedStatement::Fallback { .. } => {
                unreachable!("SET and fallback were handled above")
            }
        };

        // The adapter wraps the caller's sink and counts rows for the DONE. The
        // executor calls `RowSink::columns` during execution, which the adapter
        // converts to `ResultSink::columns` — COLMETADATA therefore goes out when
        // the statement starts to run, not while it is prepared.
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
                self.state.rowcount = 0;
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
            Ok(outcome @ (ExecOutcome::Cancelled | ExecOutcome::Break | ExecOutcome::Continue)) => {
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

    /// Runs an RPC ([MS-TDS] 2.2.6.6). Same contract as [`run_batch`](Self::run_batch).
    ///
    /// The compatibility dispatcher serves known system procedures. Every other RPC is
    /// answered with error 2812 then a DONEPROC carrying `DoneStatus::ERROR`. A `ProcID`
    /// is named after the special procedure it stands for ([MS-TDS] 2.2.6.6, e.g. 10 →
    /// `sp_executesql`).
    pub fn run_rpc(&mut self, rpc: &Rpc, sink: &mut dyn ResultSink) -> SqlResult<()> {
        let name = match &rpc.proc {
            RpcProc::Name(name) => name.clone(),
            RpcProc::Id(id) => rpc
                .proc
                .well_known_name()
                .map_or_else(|| id.to_string(), str::to_owned),
        };

        let params: Vec<_> = rpc
            .params
            .iter()
            .map(|param| {
                let name = (!param.name.is_empty()).then_some(param.name.as_str());
                (name, &param.value)
            })
            .collect();
        if let Some(result) = SYSTEM_PROCEDURE_DISPATCHER
            .get()
            .and_then(|dispatcher| dispatcher(&name, &params))
        {
            let columns: Vec<_> = result
                .columns
                .into_iter()
                .map(|column| ColumnMeta {
                    flags: ColumnFlags {
                        nullable: column.ty.nullable,
                        ..ColumnFlags::default()
                    },
                    name: column.name,
                    ty: column.ty,
                })
                .collect();
            sink.columns(&columns)?;
            let rowcount = result.rows.len() as u64;
            for row in &result.rows {
                sink.row(row)?;
            }
            sink.return_status(result.return_status)?;
            return sink.done(Some(rowcount), false);
        }

        self.fail(&SqlError::procedure_not_found(&name), sink)
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

/// The 1-based line the statement starts on, in the text of the batch, or 0 for a variant
/// this function has no case for.
///
/// # Which variants have a case, and why
///
/// A `SELECT` ([`PhysicalStatement::Query`]), the DDL of databases, tables and indexes
/// ([`PhysicalStatement::Ddl`]), `USE` ([`PhysicalStatement::Use`]) and `INSERT`
/// ([`PhysicalStatement::Insert`]). These are the statements
/// whose run-time errors reach [`at_statement`], and whose fragment
/// [`Session::bind_batch`] pads with `line - 1` newlines before rebinding it, so that a
/// binding error lands on its line of the batch instead of line 1. A variant without a
/// case answers 0, which means "no line to offer" and makes [`at_statement`] leave the
/// error alone.
///
/// # The line answered for these numbers is the **statement**'s
///
/// As on SQL Server, with the failing node on a line below the one the statement starts
/// on, so that the two hypotheses answer differently
/// (`a_run_time_ddl_error_carries_the_line_of_its_statement`,
/// `a_binding_error_of_a_ddl_statement_carries_the_line_of_its_statement`):
///
/// | batch | statement | failing node | answer |
/// |---|:-:|:-:|:-:|
/// | `SELECT 1;` / `DROP TABLE` / `dbo.s16nosuch6;` | 2 | 3 | **2** (3701) |
/// | `CREATE TABLE dbo.s16v` / `(a int);`, twice | 3 | 3 | **3** (2714) |
/// | `SELECT 1;` / `CREATE TABLE dbo.s16w` / `(a nosuchtype);` | 2 | 3 | **2** (2715) |
/// | `SELECT 1;` / `CREATE TABLE dbo.s16x` / `(a int,` / `a int);` | 2 | 4 | **2** (2705) |
/// | `SELECT 1;` / `USE` / `nosuchdatabase_s16;` | 2 | 3 | **3** (911) |
///
/// The last row is the exception, and the reason `USE` keeps a case here rather than being
/// left at 0: 911 answers the line of the **name**. It stays out of [`at_statement`], the
/// way [`COMPILED_WITH_THE_STATEMENT`] keeps 127 and 1060 out of it, but for a different
/// reason: 911 is raised while the batch is **bound**
/// ([`Session::bind_batch`]), with the line [`use_name_line`] counts, and the execution of
/// a `USE` raises nothing (`executor` answers `NoRows`). What the case here buys for a
/// `USE` is the line of its INFO 5701, which is the **statement**'s (line 2 where 911
/// answers 3, rustdoc of [`Session::switch_database`]), and the padding of the fragment,
/// which is what puts 2715 and 2705 on their own line
/// (`a_binding_error_of_a_ddl_statement_carries_the_line_of_its_statement`).
fn statement_line(stmt: &Statement) -> u32 {
    match stmt {
        Statement::Select(_)
        | Statement::CreateDatabase(_)
        | Statement::DropDatabase { .. }
        | Statement::CreateTable(_)
        | Statement::DropTable { .. }
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::AlterDatabase(_)
        | Statement::Use { .. }
        | Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)
        | Statement::BeginTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Save { .. } => statement_span(stmt).line,
        _ => 0,
    }
}

/// Number of bytes of the `USE` keyword, the first token of the statement
/// [`use_name_line`] walks.
const USE_KEYWORD_LEN: usize = "USE".len();

/// The 1-based line the **name** of a `USE` statement starts on, in `text`, which is the
/// line SQL Server puts on the 911 of a database that is not there.
///
/// `Ident` carries no span (`parser`, `ast/expr.rs`), so the line is counted here, from the
/// span of the statement: past the `USE` keyword, then through the trivia — spaces, line
/// comments, block comments, nested or not — up to the first byte of the name.
///
/// # The rule, on the shapes that separate it from its neighbours
///
/// Each shape puts the name on a different line from the statement, so "the name's line"
/// and "the statement's line" answer differently; the first also separates it from "the
/// line the statement ends on":
///
/// | batch | statement | name | end | answer |
/// |---|:-:|:-:|:-:|:-:|
/// | `SELECT 1;` / `USE` / `  d` / `;` | 2 | 3 | 4 | **3** |
/// | `SELECT 1;` / `USE /* c` / `  c */ d;` | 2 | 3 | 3 | **3** |
/// | `SELECT 1;` / `USE` / `-- a comment` / `  d;` | 2 | 4 | 4 | **4** |
/// | `SELECT 1;` / `USE [d` / `e];` | 2 | 2 | 3 | **2** |
/// | `SELECT 1;` / `USE /* a /* b` / `b */ a */` / `  d;` | 2 | 4 | 4 | **4** |
/// | `SELECT 1;` / `USE` / `` / `` / `  d;` | 2 | 5 | 5 | **5** |
/// | `USE d;` | 1 | 1 | 1 | **1** |
///
/// The fourth shape is the one that says the answer is where the name **starts** and not
/// where it ends: a delimited name spread over two lines answers the line of its `[`. The
/// fifth is why the depth of a block comment is counted rather than its first `*/`: a
/// scan that stopped at `b */` would have answered 3. The last is the witness, where the
/// three hypotheses agree. Pinned by
/// `use_name_line_is_the_line_the_name_starts_on` and, end to end,
/// `use_of_an_unknown_database_is_911_on_the_line_of_the_name`.
fn use_name_line(text: &str, stmt: &Statement) -> u32 {
    let span = statement_span(stmt);
    let start = span.offset as usize;
    let Some(body) = text
        .get(start..start + span.len as usize)
        .and_then(|body| body.get(USE_KEYWORD_LEN..))
    else {
        return span.line;
    };
    let bytes = body.as_bytes();
    let mut line = span.line;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\n' => {
                line += 1;
                at += 1;
            }
            byte if byte.is_ascii_whitespace() => at += 1,
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                // A line comment runs to the end of its line; the newline itself is counted
                // by the arm above, on the next turn.
                at += 2;
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                // Block comments nest in T-SQL, as the parser reads them (module header of
                // `parser`, `lexer.rs`): the depth is counted, not the first `*/`.
                let mut depth = 1usize;
                at += 2;
                while at < bytes.len() && depth > 0 {
                    match bytes[at] {
                        b'\n' => line += 1,
                        b'/' if bytes.get(at + 1) == Some(&b'*') => {
                            depth += 1;
                            at += 1;
                        }
                        b'*' if bytes.get(at + 1) == Some(&b'/') => {
                            depth -= 1;
                            at += 1;
                        }
                        _ => {}
                    }
                    at += 1;
                }
            }
            // The first byte that is neither space nor comment opens the name.
            _ => return line,
        }
    }
    line
}

/// Puts the line of the failing statement on a run-time error, over the line of the
/// sub-expression the executor put there.
///
/// Overriding rather than filling is the point: the executor answers a line for every
/// error it raises, and that line is the right one only when the statement fits on one
/// line. Three errors are left as they are:
///
/// - the internal error 50000, which names a hole in this engine and not a place in the
///   query (`executor`, `errors.rs`, keeps it at line 0);
/// - 127 and 1060, the two compilation checks the executor performs late, whose line SQL
///   Server counts on the `TOP` row count and not on the statement
///   ([`COMPILED_WITH_THE_STATEMENT`]);
/// - any error at all when `line` is 0, which means the caller has no line to offer.
fn at_statement(err: SqlError, line: u32) -> SqlError {
    if line == 0
        || err.number == INTERNAL_ERROR
        || COMPILED_WITH_THE_STATEMENT.contains(&err.number)
    {
        return err;
    }
    err.with_line(line)
}

/// The text of `stmt` as the client wrote it, cut out of the batch by the span of the
/// statement, or re-rendered from the AST when the variant carries no span this function
/// knows.
///
/// Only the two variants that need their text have a case here: a `SET` option, which
/// `set_options.rs` recognises from the text, and `WAITFOR`, which the fake engine still serves.
/// `Display` is the fall-back rather than an error because it round-trips through the
/// parser (`parser`, `display`), so a statement rendered this way is still the same one.
fn statement_text(batch: &str, stmt: &Statement) -> String {
    let span = statement_span(stmt);
    let start = span.offset as usize;
    batch
        .get(start..start + span.len as usize)
        .map_or_else(|| stmt.to_string(), str::to_owned)
}

/// Exact source from this statement through its separator and trailing trivia.
/// Keeping that suffix preserves errors which SQL Server locates on a semicolon below the
/// expression rather than on the expression itself.
fn statement_fragment<'a>(batch: &'a str, stmt: &Statement, next: Option<&Statement>) -> &'a str {
    let span = statement_span(stmt);
    let start = span.offset as usize;
    let end = next
        .map(statement_span)
        .map_or(batch.len(), |next_span| next_span.offset as usize);
    batch.get(start..end).unwrap_or(batch)
}

/// Position of every statement variant. Keeping this match exhaustive makes a new parser
/// construct break compilation instead of silently rebuilding the whole batch as one statement.
fn statement_span(stmt: &Statement) -> vauban_parser::Span {
    match stmt {
        Statement::Select(statement) => statement.span,
        Statement::Insert(statement) => statement.span,
        Statement::Update(statement) => statement.span,
        Statement::Delete(statement) => statement.span,
        Statement::Merge(statement) => statement.span,
        Statement::CreateDatabase(statement) => statement.span,
        Statement::AlterDatabase(statement) => statement.span,
        Statement::CreateTable(statement) => statement.span,
        Statement::AlterTable(statement) => statement.span,
        Statement::CreateIndex(statement) => statement.span,
        Statement::DropIndex(statement) => statement.span,
        Statement::CreateProcedure(statement) => statement.span,
        Statement::CreateFunction(statement) => statement.span,
        Statement::CreateView(statement) => statement.span,
        Statement::CreateTrigger(statement) => statement.span,
        Statement::CreateSequence(statement) => statement.span,
        Statement::Declare(statement) => statement.span,
        Statement::Set(statement) => statement.span,
        Statement::SetOption(statement) => statement.span,
        Statement::Execute(statement) => statement.span,
        Statement::Waitfor(statement) => statement.span,
        Statement::RaiseError(statement) => statement.span,
        Statement::Cursor(statement) => statement.span,
        Statement::Grant(statement) => statement.span,
        Statement::Break(span) | Statement::Continue(span) => *span,
        Statement::Truncate { span, .. }
        | Statement::DropDatabase { span, .. }
        | Statement::Use { span, .. }
        | Statement::DropTable { span, .. }
        | Statement::DropProcedure { span, .. }
        | Statement::DropFunction { span, .. }
        | Statement::DropView { span, .. }
        | Statement::DropTrigger { span, .. }
        | Statement::DropSequence { span, .. }
        | Statement::If { span, .. }
        | Statement::While { span, .. }
        | Statement::Block { span, .. }
        | Statement::Return { span, .. }
        | Statement::Print { span, .. }
        | Statement::BeginTransaction { span, .. }
        | Statement::Commit { span, .. }
        | Statement::Rollback { span, .. }
        | Statement::Save { span, .. }
        | Statement::TryCatch { span, .. }
        | Statement::Throw { span, .. }
        | Statement::Goto { span, .. }
        | Statement::Label { span, .. } => *span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_errors::InfoMessage;
    use vauban_parser::ParseOptions;
    use vauban_storage::MemoryStorage;
    use vauban_tds::EnvChange;
    use vauban_types::{SqlType, TypeInfo};

    fn parse(text: &str) -> Vec<Statement> {
        parse_batch(text, &ParseOptions::default())
            .expect("the text parses")
            .statements
    }

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
    fn statement_text_is_the_text_the_client_wrote() {
        let text = "SET nocount on;\nWAITFOR DELAY '00:00:01'";
        let statements = parse(text);
        assert_eq!(statement_text(text, &statements[0]), "SET nocount on");
        assert_eq!(
            statement_text(text, &statements[1]),
            "WAITFOR DELAY '00:00:01'"
        );
    }

    #[test]
    fn statement_text_preserves_the_select_source() {
        let text = "select  1";
        let statements = parse(text);
        assert_eq!(statement_text(text, &statements[0]), text);
    }

    #[test]
    fn statement_fragment_keeps_the_separator_and_its_line() {
        let text = "SELECT 1\n;\nSELECT 2";
        let statements = parse(text);
        assert_eq!(
            statement_fragment(text, &statements[0], statements.get(1)),
            "SELECT 1\n;\n"
        );
    }

    #[test]
    fn statement_line_is_the_line_of_the_first_token() {
        // The `SELECT` of the second statement is on line 3, its select list on line 4.
        let text = "SELECT 1;\n\n SELECT\n 1 / 0;";
        let statements = parse(text);
        assert_eq!(statement_line(&statements[0]), 1);
        assert_eq!(statement_line(&statements[1]), 3);
        // A comment before the statement is trivia: the line is the keyword's.
        let text = "SELECT 1;\n-- a comment on its own line\nSELECT\n1 / 0;";
        assert_eq!(statement_line(&parse(text)[1]), 3);
        // Two statements on one line share it.
        assert_eq!(statement_line(&parse("SELECT 1; SELECT\n1 / 0;")[1]), 1);
    }

    #[test]
    fn statement_line_of_a_variant_without_a_case_is_zero() {
        assert_eq!(statement_line(&parse("SET nocount on")[0]), 0);
    }

    /// The DDL variants, `USE`, `INSERT`, `UPDATE` and `DELETE` answer the line of their
    /// first token, like a `SELECT`.
    ///
    /// Each statement below starts on line 2 and spreads over line 3: a case that read the
    /// last line of the statement would answer 3, and a missing case 0, which is what the
    /// three tests of the DDL lines answer with `_ => 0` put back.
    #[test]
    fn statement_line_covers_the_ddl_variants_and_use() {
        for text in [
            "SELECT 1;\nCREATE TABLE dbo.t\n  (a int);",
            "SELECT 1;\nDROP TABLE\n  dbo.t;",
            "SELECT 1;\nCREATE DATABASE\n  d;",
            "SELECT 1;\nDROP DATABASE\n  d;",
            "SELECT 1;\nCREATE INDEX ix ON dbo.t\n  (a);",
            "SELECT 1;\nDROP INDEX ix ON\n  dbo.t;",
            "SELECT 1;\nUSE\n  d;",
            "SELECT 1;\nINSERT INTO dbo.t\n  (a) VALUES (1);",
            "SELECT 1;\nUPDATE dbo.t\n  SET a = 1;",
            "SELECT 1;\nDELETE FROM\n  dbo.t;",
            "SELECT 1;\nDELETE\n  dbo.t WHERE a = 1;",
        ] {
            assert_eq!(statement_line(&parse(text)[1]), 2, "{text}");
        }
    }

    #[test]
    fn at_statement_overrides_the_line_of_the_sub_expression() {
        let raised_on_line_4 = SqlError::divide_by_zero().with_line(4);
        assert_eq!(at_statement(raised_on_line_4, 3).line, 3);
    }

    #[test]
    fn at_statement_without_a_line_changes_nothing() {
        let raised_on_line_4 = SqlError::divide_by_zero().with_line(4);
        assert_eq!(at_statement(raised_on_line_4, 0).line, 4);
    }

    #[test]
    fn an_internal_error_still_carries_no_line() {
        let bug = SqlError::from(vauban_errors::InternalError::Bug("x".to_owned()));
        let wrapped = at_statement(bug, 3);
        assert_eq!(wrapped.number, INTERNAL_ERROR);
        assert_eq!(wrapped.line, 0);
    }

    #[test]
    fn the_two_compilation_checks_keep_their_own_line() {
        // `SELECT` on line 3, `TOP (-1)` on line 4: SQL Server answers 4 for both numbers.
        assert_eq!(
            at_statement(SqlError::top_negative().with_line(4), 3).line,
            4
        );
        assert_eq!(at_statement(SqlError::top_null().with_line(4), 3).line, 4);
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
    /// columns, in the order declared, no row, and a DONE whose count is 0, which is what
    /// SQL Server answers to `CREATE TABLE dbo.s16y (a int, b bigint); SELECT * FROM
    /// dbo.s16y;` in one batch, where this engine needs two
    /// (`create_table_then_select_in_the_same_batch_is_208`).
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

    /// The same two statements in **one** batch answer 208 and run neither of them: the
    /// whole batch is bound before its first statement runs, so the `SELECT` is bound
    /// against a catalogue that does not hold the table yet.
    ///
    /// This is the deliberate difference with SQL Server, which resolves the name when the
    /// statement runs and answers one result set with the column `a` and no row to
    /// `CREATE TABLE dbo.s16b (a int); SELECT * FROM dbo.s16b;` in one batch. Here the
    /// batch answers one error and no result set, and the table is **not** created, the
    /// assertion that separates "bound before the batch" from "bound but run anyway".
    #[test]
    fn create_table_then_select_in_the_same_batch_is_208() {
        let engine = engine();
        let mut session = session(&engine);
        let batch = run(
            &mut session,
            "CREATE TABLE dbo.s16c (a int); SELECT 1 FROM dbo.s16c;",
        );
        assert_eq!(batch.errors.len(), 1);
        assert_eq!(batch.errors[0].number, 208);
        assert!(batch.columns.is_empty());
        assert_eq!(tables_of_master(&engine), Vec::<String>::new());
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
    /// which [`at_statement`] puts there from the case [`statement_line`] has for the DDL
    /// variants.
    ///
    /// Both batches open with a comment on line 1. Each puts the failing node below the
    /// first line of its statement, which is what tells "the statement's line" from "the
    /// node's" and from "line 1":
    ///
    /// - `DROP TABLE` on line 3, `dbo.s16nosuch5` on line 4 -> **3** (3701);
    /// - the second `CREATE TABLE dbo.s16v` on line 4, its column list on line 5 -> **4**
    ///   (2714).
    ///
    /// Without the DDL cases of [`statement_line`], both answer 0, the "no line to offer"
    /// of [`at_statement`], and a client reads `Line 0`.
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
    /// the `SELECT` is bound in `d`. SQL Server resolves the name when the statement runs
    /// and therefore answers 208 as well, after the 5701 of the `USE`, where this engine
    /// answers before it (the batch is bound before its first statement, `bind_batch`).
    /// The counter-proof is the first batch: without the `USE`, the very same `SELECT`
    /// finds its table.
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
        assert!(
            after_use.databases.is_empty(),
            "the binding error stops the batch before the ENVCHANGE"
        );
    }

    /// [`use_name_line`] on the shapes of its table, without the chain around it: the
    /// trivia between `USE` and its name — spaces, a line comment, a block comment, a
    /// nested block comment — is counted, and the answer is where the name starts.
    #[test]
    fn use_name_line_is_the_line_the_name_starts_on() {
        for (text, line) in [
            ("USE d;", 1),
            ("SELECT 1;\nUSE\n  d\n;", 3),
            ("SELECT 1;\nUSE /* c\n  c */ d;", 3),
            ("SELECT 1;\nUSE\n-- a comment\n  d;", 4),
            ("SELECT 1;\nUSE [d\ne];", 2),
            ("SELECT 1;\nUSE /* a /* b\nb */ a */\n d;", 4),
            ("SELECT 1;\nUSE\n\n\n  d;", 5),
        ] {
            let statements = parse(text);
            let stmt = statements.last().expect("the batch has a statement");
            assert!(matches!(stmt, Statement::Use { .. }), "{text}");
            assert_eq!(use_name_line(text, stmt), line, "{text}");
        }
    }
}
