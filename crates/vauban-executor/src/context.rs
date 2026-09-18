//! What the executor needs besides the statement it runs, and what it hands back while
//! it runs: the context, the cancellation token, the session state it writes, the sink
//! the rows go to.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use vauban_binder::{ColumnBinding, OutputSchema, SessionOptions};
use vauban_catalog::{Catalog, ColumnId};
use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult};
use vauban_planner::SubPlan;
use vauban_storage::{SavepointId, Snapshot, Storage};
use vauban_sysfn::EvalContext;
use vauban_txn::{IsolationLevel, LockTimeout, LockWait, TransactionManager, TxnHandle};
use vauban_types::{Decimal, TypeInfo, Value};

use crate::row::Row;

pub(crate) const SLOT_COLUMN_ID: ColumnId = ColumnId(0);

struct SubqueryFrame {
    subplans: Vec<SubPlan>,
    slot: usize,
    cache: Vec<Option<Value>>,
}

/// How many rows the driving loop of a query lets through between two reads of the
/// cancellation token.
///
/// The token is also read before the operator tree is opened and once the tree is
/// exhausted (`statement.rs`), so a cancellation raised while an operator materialises its
/// input is still reported as [`ExecOutcome::Cancelled`](crate::ExecOutcome::Cancelled)
/// (`tests/operator_basics.rs`, `cancel_mid_stream`).
pub(crate) const CANCEL_CHECK_ROWS: u64 = 1024;

/// A flag shared between the connection task and the statement it runs: raised once,
/// read between two batches of rows.
///
/// Cloning a token gives another handle on the same flag. [`CancelToken::never`] is a
/// token without a flag: it cannot be raised (`context::tests::a_never_token_stays_down`),
/// and a context built without a token carries it.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    /// `None` for the token that cannot be raised.
    flag: Option<Arc<AtomicBool>>,
}

/// The token a context is built with when the caller hands no token: it has no flag to
/// raise.
static NEVER: CancelToken = CancelToken { flag: None };

impl CancelToken {
    /// A token that is not raised yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: Some(Arc::new(AtomicBool::new(false))),
        }
    }

    /// A token that cannot be raised: [`CancelToken::cancel`] does nothing on it and
    /// [`CancelToken::is_cancelled`] answers `false` (`context::tests::a_never_token_stays_down`).
    #[must_use]
    pub fn never() -> Self {
        Self { flag: None }
    }

    /// A token backed by a caller-provided flag, so that [`CancelHandle`] and
    /// [`ExecContext`] share the same `AtomicBool`.
    #[must_use]
    pub fn from_shared(flag: Arc<AtomicBool>) -> Self {
        Self { flag: Some(flag) }
    }

    /// Raises the flag: the running statement stops at its next read of the token.
    pub fn cancel(&self) {
        if let Some(flag) = &self.flag {
            flag.store(true, Ordering::Release);
        }
    }

    /// `true` once [`CancelToken::cancel`] was called on this token or on a clone of it.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    /// The wait token passed to a lock the statement takes while it runs.
    #[must_use]
    pub fn lock_wait(&self) -> LockWait {
        self.flag
            .as_ref()
            .map_or_else(LockWait::none, |flag| LockWait::from_flag(Arc::clone(flag)))
    }
}

/// The state of a session that the executor reads and writes.
///
/// Defined here and not in `session`, which depends on this crate: `session` embeds one
/// and lends it to each statement through [`ExecContext::with_session`]. The fields are
/// what the statements of a batch share: the variables, `@@ROWCOUNT`, `@@TRANCOUNT`, the
/// transaction the session keeps open across statements, `XACT_ABORT` and `@@ERROR`.
///
/// Isolation level and lock timeout are the session-wide defaults set by
/// `SET TRANSACTION ISOLATION LEVEL` and `SET LOCK_TIMEOUT`; the level is applied when
/// `BEGIN TRANSACTION` opens a new transaction, the timeout when a lock is taken.
#[derive(Debug, Clone)]
pub struct ExecSession {
    /// The declared variables, keyed by name with the `@`.
    pub variables: HashMap<String, Value>,
    /// The declared type of each variable, keyed by name with the `@`. `DECLARE` writes
    /// it and the `SET` of a value the binder did not convert reads it: the value is
    /// converted to this type before it is stored.
    pub variable_types: HashMap<String, TypeInfo>,
    /// `@@ROWCOUNT`: what the previous statement produced or changed.
    pub rowcount: i64,
    /// `@@TRANCOUNT`: how many `BEGIN TRANSACTION` are open.
    pub trancount: i32,
    /// The transaction the session keeps open across statements, `None` in autocommit.
    pub txn: Option<TxnHandle>,
    /// `SET XACT_ABORT`: whether a run-time error rolls back the whole transaction.
    pub xact_abort: bool,
    /// `@@ERROR`: the number of the last error, `0` after a statement that raised no error.
    pub last_error: u32,
    /// Last identity value generated by an `INSERT` in the current statement, `None` when the
    /// statement generated none. Copied into `SessionState` by `session` after the statement
    /// (`insert.rs`, `tests/session_variables.rs`).
    pub last_identity: Option<Decimal>,
    /// `true` when the current statement was an `INSERT` that ran `insert.rs`, which alone
    /// updates `@@IDENTITY` and `SCOPE_IDENTITY()`.
    pub identity_updated: bool,
    /// Named savepoints created by `SAVE TRANSACTION`, keyed by name as written.
    pub savepoints: HashMap<String, SavepointId>,
    /// The savepoint of the currently running statement, taken by [`begin_statement`] and
    /// rolled back to by [`end_statement`](crate::txn_exec::end_statement) on error.
    pub stmt_savepoint: Option<SavepointId>,
    /// The isolation level of the session, applied when `BEGIN TRANSACTION` opens a new
    /// transaction. Set by `SET TRANSACTION ISOLATION LEVEL`.
    pub isolation: IsolationLevel,
    /// The lock timeout of the session, applied when a lock is taken.
    /// Set by `SET LOCK_TIMEOUT`.
    pub lock_timeout: LockTimeout,
}

impl Default for ExecSession {
    fn default() -> Self {
        Self {
            variables: HashMap::new(),
            variable_types: HashMap::new(),
            rowcount: 0,
            trancount: 0,
            txn: None,
            xact_abort: false,
            last_error: 0,
            last_identity: None,
            identity_updated: false,
            savepoints: HashMap::new(),
            stmt_savepoint: None,
            isolation: IsolationLevel::ReadCommitted,
            lock_timeout: LockTimeout::Infinite,
        }
    }
}

/// Where the rows of a statement go, one call per row.
///
/// Defined here and implemented by `session`, which turns each call into the matching
/// token of the wire. The executor calls [`RowSink::columns`] once before the first row of
/// a result set, then [`RowSink::row`] per row, then [`RowSink::info`] for each message
/// the statement queued through [`ExecContext::emit_info`] (`statement.rs`).
pub trait RowSink {
    /// The columns of the result set that follows, sent once before its first row.
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take the metadata; the statement stops
    /// there.
    fn columns(&mut self, schema: &OutputSchema) -> SqlResult<()>;

    /// One row, of the width of the schema announced by [`RowSink::columns`].
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take the row; the statement stops there.
    fn row(&mut self, row: &[Value]) -> SqlResult<()>;

    /// An informational message the statement produced.
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take the message.
    fn info(&mut self, message: &InfoMessage) -> SqlResult<()>;
}

/// A [`RowSink`] that keeps what it is given, for the tests and for
/// [`execute_collect`](crate::execute_collect).
///
/// A row handed over before the columns is refused with the internal error 50000: the
/// order of the calls is part of what a test reads on this sink
/// (`context::tests::a_row_before_the_columns_is_refused`).
#[derive(Debug, Clone, Default)]
pub struct CollectSink {
    /// The schema announced by [`RowSink::columns`], `None` until then.
    pub schema: Option<OutputSchema>,
    /// The rows, in the order they were handed over.
    pub rows: Vec<Row>,
    /// The messages, in the order they were handed over.
    pub infos: Vec<InfoMessage>,
}

impl CollectSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RowSink for CollectSink {
    fn columns(&mut self, schema: &OutputSchema) -> SqlResult<()> {
        self.schema = Some(schema.clone());
        Ok(())
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        if self.schema.is_none() {
            return Err(bug("CollectSink: a row was handed over before the columns"));
        }
        self.rows.push(row.to_vec());
        Ok(())
    }

    fn info(&mut self, message: &InfoMessage) -> SqlResult<()> {
        self.infos.push(message.clone());
        Ok(())
    }
}

/// Everything the executor consults besides the statement it runs.
///
/// Borrowed, never owned: one context is built per statement by the caller (`session` in
/// production, the tests otherwise) and lives as long as the execution
/// (`tests/exec_shape.rs`). A scalar evaluation needs two things; reading a table or
/// running DDL adds the engine — storage, transaction manager, catalogue, the snapshot
/// the statement reads through and the transaction it writes in. The cancellation token
/// and the session state come on top, through [`ExecContext::with_cancel`] and
/// [`ExecContext::with_session`].
///
/// # Why five engine fields and not one `Engine`
///
/// An `engine: &Engine` grouping storage, transaction manager and catalogue would be the
/// natural shape, but `Engine` is a type of `session`, and `session` depends on
/// `executor`: naming it here would reverse the dependency of the engine. The three
/// references are therefore carried side by side, plus the [`Snapshot`] and the
/// [`TxnHandle`], which are not part of an `Engine`: the snapshot is taken per statement
/// by the caller ([`TransactionManager::statement_snapshot`]) and the handle is the
/// transaction that caller opened; this crate opens no transaction itself.
///
/// # Two shapes, one structure
///
/// [`ExecContext::scalar`] builds the scalar shape: no storage, no transaction manager, no
/// catalogue, no snapshot, no transaction handle, no session state, and a token that
/// cannot be raised. It is what a `SELECT` without `FROM` needs.
/// [`ExecContext::with_engine`] adds what a
/// [`TableScan`](vauban_planner::PhysicalPlan::TableScan) needs,
/// [`ExecContext::with_catalog`] and [`ExecContext::with_handle`] what the DDL needs,
/// [`ExecContext::with_session`] what a statement that writes a variable or
/// `@@ROWCOUNT` needs; running one of those without its field is the internal error
/// 50000 raised by the five accessors, and not a panic
/// (`context::tests::a_scalar_context_has_no_engine`).
pub struct ExecContext<'a> {
    /// Session context for the built-in functions (`GETDATE`, `@@SPID`, `@@ROWCOUNT`…).
    pub eval: &'a dyn EvalContext,
    /// `SET` options that change evaluation (`binder::SessionOptions`).
    pub options: SessionOptions,
    /// Where the rows live. `None` in a scalar context.
    pub storage: Option<&'a dyn Storage>,
    /// The transaction manager of the server. `None` in a scalar context.
    pub txn: Option<&'a TransactionManager>,
    /// The catalogue, for the statements that read or change metadata. `None` in a scalar
    /// context; the DDL reads it through [`ExecContext::with_catalog`].
    pub catalog: Option<&'a Catalog>,
    /// What this statement is allowed to see, taken by the caller before the statement
    /// started. `None` in a scalar context.
    pub snap: Option<&'a Snapshot>,
    /// The transaction the caller opened for this statement, which a mutation of the
    /// catalogue takes part in. `None` in a scalar context.
    ///
    /// Distinct from `txn`, which is the manager:
    /// [`Catalog::create_table`](vauban_catalog::Catalog::create_table) and its neighbours
    /// take the [`TxnHandle`] itself, and this crate opens no transaction of its own.
    /// Filled by [`ExecContext::with_handle`], which `session` calls with the handle it
    /// opened for the statement.
    pub handle: Option<&'a TxnHandle>,
    /// The token the caller raises to stop the statement, read by the driving loop
    /// before the operator tree is opened, then every [`CANCEL_CHECK_ROWS`] rows, then
    /// once the tree is exhausted (`tests/operator_basics.rs`, `cancel_mid_stream`). A
    /// scalar context carries a token that cannot be raised.
    pub cancel: &'a CancelToken,
    /// The state of the session this statement writes: variables, `@@ROWCOUNT`,
    /// `@@TRANCOUNT`, the open transaction. `None` in a scalar context; a statement that
    /// needs it reads it through [`ExecContext::session`].
    pub session: Option<&'a mut ExecSession>,
    /// The informational messages queued by [`ExecContext::emit_info`], drained by the
    /// driving loop into the sink after the last row.
    infos: Vec<InfoMessage>,
    /// The stack of outer rows a correlated join pushes before opening the inner input:
    /// an [`IndexSeek`](crate::ops::seek::IndexSeek) evaluates its bounds with the outer
    /// row through [`eval_expr`](crate::expr::eval_expr), which consults this stack when
    /// the current row is `None`.
    outer_rows: Vec<Row>,
    /// Scan bindings for each [`ExecContext::outer_rows`] entry: position `i` in the row
    /// holds the value of `outer_bindings[i]`.
    outer_bindings: Vec<Vec<ColumnBinding>>,
    /// One frame per active [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval)
    /// row, innermost last, so a nested `SubqueryEval` does not clobber its parent.
    subquery_frames: Vec<SubqueryFrame>,
    /// Column identifiers exposed by the inner plan currently being evaluated.
    subquery_locals: Option<HashSet<ColumnId>>,
    subquery_row_bindings: Option<Vec<ColumnBinding>>,
    /// When set, each `open` of a subquery inner plan increments this counter.
    subquery_open_count: Option<Rc<Cell<usize>>>,
    /// When set, each `next` of a subquery inner plan increments this counter.
    subquery_next_count: Option<Rc<Cell<usize>>>,
}

impl<'a> ExecContext<'a> {
    /// A context for a statement that touches no table: the scalar shape.
    ///
    /// The engine fields are `None`; a plan that needs one answers 50000 instead of
    /// reading it (`context::tests::a_scalar_context_has_no_engine`). The token cannot be
    /// raised.
    #[must_use]
    pub fn scalar(eval: &'a dyn EvalContext, options: SessionOptions) -> Self {
        Self {
            eval,
            options,
            storage: None,
            txn: None,
            catalog: None,
            snap: None,
            handle: None,
            cancel: &NEVER,
            session: None,
            infos: Vec::new(),
            outer_rows: Vec::new(),
            outer_bindings: Vec::new(),
            subquery_frames: Vec::new(),
            subquery_locals: None,
            subquery_row_bindings: None,
            subquery_open_count: None,
            subquery_next_count: None,
        }
    }

    /// The same context, plus what reading a table takes.
    ///
    /// `snap` is built by the caller, once per statement
    /// ([`TransactionManager::statement_snapshot`]): the executor neither opens nor ends a
    /// transaction.
    #[must_use]
    pub fn with_engine(
        mut self,
        storage: &'a dyn Storage,
        txn: &'a TransactionManager,
        snap: &'a Snapshot,
    ) -> Self {
        self.storage = Some(storage);
        self.txn = Some(txn);
        self.snap = Some(snap);
        self
    }

    /// The same context, plus the catalogue the metadata statements read.
    #[must_use]
    pub fn with_catalog(mut self, catalog: &'a Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// The same context, plus the transaction the DDL takes part in.
    ///
    /// Kept apart from [`ExecContext::with_engine`] so that a caller that reads rows without
    /// changing metadata hands over no handle: `snap` is what a read needs, the handle is
    /// what a mutation of
    /// the catalogue needs.
    #[must_use]
    pub fn with_handle(mut self, handle: &'a TxnHandle) -> Self {
        self.handle = Some(handle);
        self
    }

    /// The same context, plus the token the caller raises to stop the statement.
    #[must_use]
    pub fn with_cancel(mut self, cancel: &'a CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// The same context, plus the session state the statement writes.
    #[must_use]
    pub fn with_session(mut self, session: &'a mut ExecSession) -> Self {
        self.session = Some(session);
        self
    }

    /// Whether the caller raised the token: the driving loop and the operators that
    /// materialise their input read it here.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Pushes a row onto the outer-row stack for correlated joins: the row is visible
    /// through [`eval_expr`](crate::expr::eval_expr) for any `ColumnRef` whose
    /// [`ColumnId`](vauban_catalog::ColumnId) appears in `bindings`.
    ///
    /// Popped by [`ExecContext::pop_outer`].
    pub fn push_outer(&mut self, row: Row, bindings: Vec<ColumnBinding>) {
        self.outer_rows.push(row);
        self.outer_bindings.push(bindings);
    }

    /// Pops the outer row that [`ExecContext::push_outer`] pushed. The caller must match
    /// each push with one pop; the row is returned so that the caller does not need to
    /// clone it to keep it.
    #[must_use]
    pub fn pop_outer(&mut self) -> Option<Row> {
        self.outer_bindings.pop();
        self.outer_rows.pop()
    }

    /// Reads one outer column from the stack, searching from the innermost frame outward.
    ///
    /// # Errors
    ///
    /// The internal error 50000 when no frame exposes `column`.
    pub(crate) fn read_outer_column(
        &self,
        column: ColumnId,
        name: &str,
    ) -> SqlResult<vauban_types::Value> {
        for (row, bindings) in self.outer_rows.iter().zip(self.outer_bindings.iter()).rev() {
            if let Some(pos) = bindings
                .iter()
                .position(|b| b.column == column && b.name == name)
            {
                return row.get(pos).cloned().ok_or_else(|| {
                    bug(&format!(
                        "read_outer_column: column `{name}` is at index {pos} of a row of {} value(s)",
                        row.len()
                    ))
                });
            }
            let Some(pos) = bindings.iter().position(|b| b.column == column) else {
                continue;
            };
            return row.get(pos).cloned().ok_or_else(|| {
                bug(&format!(
                    "read_outer_column: column `{name}` is at index {pos} of a row of {} value(s)",
                    row.len()
                ))
            });
        }
        Err(bug(&format!(
            "read_outer_column: column `{name}` is evaluated without an outer row"
        )))
    }

    /// The outer row stack, for [`eval_expr`](crate::expr::eval_expr) to resolve a
    /// `ColumnRef` whose row is `None`.
    #[must_use]
    pub(crate) fn outer_rows(&self) -> &[Row] {
        &self.outer_rows
    }

    /// Opens one [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval) operator.
    pub(crate) fn open_subquery_eval(&mut self, subplans: &[SubPlan]) {
        self.subquery_frames.push(SubqueryFrame {
            subplans: subplans.to_vec(),
            slot: 0,
            cache: vec![None; subplans.len()],
        });
    }

    /// Closes the innermost [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval) operator.
    pub(crate) fn close_subquery_eval(&mut self) {
        self.subquery_frames.pop();
    }

    /// Resets the slot counter for a new row of the innermost
    /// [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval).
    pub(crate) fn begin_subquery_row(&mut self) {
        if let Some(frame) = self.subquery_frames.last_mut() {
            frame.slot = 0;
        }
    }

    /// Clears subquery state once the operator is exhausted.
    pub(crate) fn clear_subquery_state(&mut self) {
        self.close_subquery_eval();
    }

    /// Whether a [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval) row is active.
    #[must_use]
    pub(crate) fn has_active_subplans(&self) -> bool {
        !self.subquery_frames.is_empty()
    }

    /// The next subplan slot for this row.
    ///
    /// # Errors
    ///
    /// The internal error 50000 when the walk order diverges from the planner.
    pub(crate) fn take_subplan(&mut self) -> SqlResult<(SubPlan, usize)> {
        let frame = self
            .subquery_frames
            .last_mut()
            .ok_or_else(|| bug("ExecContext: no active subquery row"))?;
        let slot = frame.slot;
        let Some(subplan) = frame.subplans.get(slot) else {
            return Err(bug("ExecContext: subquery slot out of range"));
        };
        frame.slot += 1;
        Ok((subplan.clone(), slot))
    }

    /// A cached uncorrelated subquery value, when one was stored for `slot`.
    #[must_use]
    pub(crate) fn subquery_cached(&self, slot: usize) -> Option<Value> {
        self.subquery_frames.last()?.cache.get(slot)?.clone()
    }

    /// Stores the value of an uncorrelated subquery for `slot`.
    pub(crate) fn store_subquery_cache(&mut self, slot: usize, value: Value) {
        if let Some(entry) = self
            .subquery_frames
            .last_mut()
            .and_then(|frame| frame.cache.get_mut(slot))
        {
            *entry = Some(value);
        }
    }

    /// Marks which columns belong to the inner plan of a correlated subquery.
    pub(crate) fn push_subquery_locals(
        &mut self,
        locals: HashSet<ColumnId>,
        bindings: Vec<ColumnBinding>,
    ) {
        self.subquery_locals = Some(locals);
        self.subquery_row_bindings = Some(bindings);
    }

    /// Clears [`ExecContext::push_subquery_locals`].
    pub(crate) fn pop_subquery_locals(&mut self) {
        self.subquery_locals = None;
        self.subquery_row_bindings = None;
    }

    #[must_use]
    pub(crate) fn subquery_locals(&self) -> Option<&HashSet<ColumnId>> {
        self.subquery_locals.as_ref()
    }

    pub(crate) fn read_local_column(&self, row: &Row, binding: &ColumnBinding) -> SqlResult<Value> {
        if let Some(bindings) = &self.subquery_row_bindings {
            if let Some(found) = bindings.iter().find(|item| {
                item.column == binding.column
                    && item.name == binding.name
                    && item.index == binding.index
            }) && found.index < row.len()
            {
                return row.get(found.index).cloned().ok_or_else(|| {
                    bug(&format!(
                        "read_local_column: column `{}` is at index {} of a row of {} value(s)",
                        found.name,
                        found.index,
                        row.len()
                    ))
                });
            }
            if let Some(found) = bindings
                .iter()
                .find(|item| item.column == binding.column && item.name == binding.name)
                && found.index < row.len()
            {
                return row.get(found.index).cloned().ok_or_else(|| {
                    bug(&format!(
                        "read_local_column: column `{}` is at index {} of a row of {} value(s)",
                        found.name,
                        found.index,
                        row.len()
                    ))
                });
            }
        }
        if binding.index < row.len() {
            return row.get(binding.index).cloned().ok_or_else(|| {
                bug(&format!(
                    "read_local_column: column `{}` is at index {} of a row of {} value(s)",
                    binding.name,
                    binding.index,
                    row.len()
                ))
            });
        }
        Err(bug(&format!(
            "read_local_column: column `{}` is missing from the inner row",
            binding.name
        )))
    }

    /// Whether `column` is read from the outer row pushed by [`ExecContext::push_outer`].
    ///
    /// [`ColumnId`](vauban_catalog::ColumnId) is scoped to one table; one id can name a
    /// different column on another table. When the id appears in the inner plan,
    /// `name` disambiguates.
    pub(crate) fn column_is_outer(&self, column: ColumnId, name: &str) -> bool {
        if column == SLOT_COLUMN_ID {
            return false;
        }
        if self.outer_rows.is_empty() {
            return false;
        }
        let Some(locals) = self.subquery_locals.as_ref() else {
            return false;
        };
        if !locals.contains(&column) {
            return true;
        }
        self.subquery_row_bindings.as_ref().is_none_or(|bindings| {
            bindings
                .iter()
                .find(|binding| binding.column == column)
                .is_none_or(|binding| binding.name != name)
        })
    }

    /// Hooks used by integration tests to count inner-plan `open` calls.
    #[doc(hidden)]
    pub fn set_subquery_open_count(&mut self, counter: Rc<Cell<usize>>) {
        self.subquery_open_count = Some(counter);
    }

    /// Hooks used by integration tests to count inner-plan `next` calls.
    #[doc(hidden)]
    pub fn set_subquery_next_count(&mut self, counter: Rc<Cell<usize>>) {
        self.subquery_next_count = Some(counter);
    }

    pub(crate) fn note_subquery_open(&self) {
        if let Some(counter) = &self.subquery_open_count {
            counter.set(counter.get() + 1);
        }
    }

    pub(crate) fn note_subquery_next(&self) {
        if let Some(counter) = &self.subquery_next_count {
            counter.set(counter.get() + 1);
        }
    }

    /// Integration tests in `tests/subquery.rs` exercise the production path.
    #[doc(hidden)]
    pub fn test_eval_exists_plan(
        &mut self,
        plan: &vauban_planner::PhysicalPlan,
    ) -> SqlResult<Value> {
        crate::ops::subquery::eval_exists_plan(plan, self, None, Some(plan))
    }

    /// Integration tests in `tests/subquery.rs` exercise the production path.
    #[doc(hidden)]
    pub fn test_eval_in_subquery_plan(
        &mut self,
        plan: &vauban_planner::PhysicalPlan,
        tested: &vauban_binder::BoundExpr,
        row: Option<&Row>,
        line: u32,
    ) -> SqlResult<Value> {
        crate::ops::subquery::eval_in_subquery_plan(plan, tested, row, self, line)
    }

    /// Queues an informational message for the sink. An operator has no sink of its own;
    /// the driving loop hands the queue over after the last row, in the order the
    /// messages were queued (`context::tests::infos_are_queued_in_order`).
    pub fn emit_info(&mut self, message: InfoMessage) {
        self.infos.push(message);
    }

    /// Empties the queue of [`ExecContext::emit_info`], in the order the messages were
    /// queued.
    pub(crate) fn take_infos(&mut self) -> Vec<InfoMessage> {
        std::mem::take(&mut self.infos)
    }

    /// Where the rows live, or the internal error 50000 when the caller built a scalar
    /// context for a statement that reads a table.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `storage` is `None`, which is what a caller that built a
    /// scalar context for a statement reading a table gets
    /// (`a_scalar_context_has_no_engine`); no client input can cause it.
    pub(crate) fn storage(&self) -> SqlResult<&'a dyn Storage> {
        self.storage.ok_or_else(|| {
            bug("ExecContext: reading a table needs `storage`, and this context has none")
        })
    }

    /// What this statement may see, or the internal error 50000 when the caller built a
    /// scalar context for a statement that reads a table.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `snap` is `None`, same cause as [`ExecContext::storage`].
    pub(crate) fn snapshot(&self) -> SqlResult<&'a Snapshot> {
        self.snap.ok_or_else(|| {
            bug("ExecContext: reading a table needs `snap`, and this context has none")
        })
    }

    /// The catalogue, or the internal error 50000 when the caller built a context without
    /// one for a statement that reads or changes metadata.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `catalog` is `None`, same cause as [`ExecContext::storage`]
    /// (`a_scalar_context_has_no_engine`).
    pub(crate) fn catalog(&self) -> SqlResult<&'a Catalog> {
        self.catalog.ok_or_else(|| {
            bug("ExecContext: a DDL statement needs `catalog`, and this context has none")
        })
    }

    /// The transaction of this statement, or the internal error 50000 when the caller built
    /// a context without one for a statement that changes the catalogue.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `handle` is `None`, same cause as [`ExecContext::storage`]
    /// (`a_scalar_context_has_no_engine`).
    pub(crate) fn handle(&self) -> SqlResult<&'a TxnHandle> {
        self.handle.ok_or_else(|| {
            bug("ExecContext: a DDL statement needs `handle`, and this context has none")
        })
    }

    /// The session state, or the internal error 50000 when the caller built a context
    /// without one for a statement that writes a variable, `@@ROWCOUNT` or `@@TRANCOUNT`.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `session` is `None`, same cause as
    /// [`ExecContext::storage`] (`a_scalar_context_has_no_engine`).
    pub fn session(&mut self) -> SqlResult<&mut ExecSession> {
        match self.session.as_deref_mut() {
            Some(session) => Ok(session),
            None => Err(bug(
                "ExecContext: writing the session state needs `session`, and this context \
                 has none",
            )),
        }
    }
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_sysfn::StaticContext;

    /// A scalar context answers 50000 for the things a plan reads from the engine, instead
    /// of panicking or handing out a default.
    #[test]
    fn a_scalar_context_has_no_engine() {
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        assert!(ctx.storage.is_none());
        assert!(ctx.txn.is_none());
        assert!(ctx.catalog.is_none());
        assert!(ctx.snap.is_none());
        assert!(ctx.handle.is_none());
        assert!(ctx.session.is_none());
        assert!(!ctx.cancelled());
        let no_storage = ctx
            .storage()
            .err()
            .expect("a scalar context has no storage");
        assert_eq!(no_storage.number, 50000);
        assert_eq!(ctx.snapshot().unwrap_err().number, 50000);
        assert_eq!(
            ctx.catalog()
                .err()
                .expect("a scalar context has no catalogue")
                .number,
            50000
        );
        assert_eq!(ctx.handle().unwrap_err().number, 50000);
        assert_eq!(ctx.session().unwrap_err().number, 50000);
    }

    /// The counter-proof of the test above: once `with_engine` has been called, the two
    /// accessors answer the references they were given.
    #[test]
    fn with_engine_fills_storage_and_snapshot() {
        use std::sync::Arc;
        use vauban_storage::{MemoryStorage, TxnId};
        use vauban_txn::IsolationLevel;

        let eval = StaticContext::default();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = TransactionManager::new(Arc::clone(&storage));
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let snap = txn.statement_snapshot(&handle);
        let ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            storage.as_ref(),
            &txn,
            &snap,
        );
        assert!(ctx.storage().is_ok());
        assert_eq!(ctx.snapshot().expect("the snapshot is there").own, TxnId(1));
        assert!(ctx.txn.is_some());
        assert!(ctx.catalog.is_none(), "`with_engine` does not set it");
        assert!(ctx.handle.is_none(), "`with_engine` does not set it either");
        let ctx = ctx.with_handle(&handle);
        assert_eq!(ctx.handle().expect("the handle is there").id, TxnId(1));
    }

    /// `with_session` lends the state, and what a statement writes through the accessor
    /// is what the caller reads back once the context is gone.
    #[test]
    fn with_session_lends_the_state() {
        let eval = StaticContext::default();
        let mut session = ExecSession::default();
        {
            let mut ctx =
                ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
            ctx.session().expect("the session is there").rowcount = 7;
        }
        assert_eq!(session.rowcount, 7);
    }

    /// A raised token is read through the context, on the token itself and on a clone.
    #[test]
    fn a_raised_token_is_seen_through_the_context() {
        let eval = StaticContext::default();
        let token = CancelToken::new();
        let ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_cancel(&token);
        assert!(!ctx.cancelled());
        token.clone().cancel();
        assert!(ctx.cancelled());
        assert!(token.is_cancelled());
    }

    /// The token of a scalar context cannot be raised: `cancel` on it is a no-op.
    #[test]
    fn a_never_token_stays_down() {
        let token = CancelToken::never();
        token.cancel();
        assert!(!token.is_cancelled());
        let eval = StaticContext::default();
        let ctx = ExecContext::scalar(&eval, SessionOptions::default());
        ctx.cancel.cancel();
        assert!(!ctx.cancelled());
        // The counter-proof: a token built with `new` does go up.
        let armed = CancelToken::new();
        armed.cancel();
        assert!(armed.is_cancelled());
    }

    /// The queue keeps the order of `emit_info`, and `take_infos` empties it.
    #[test]
    fn infos_are_queued_in_order() {
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        let first = InfoMessage {
            number: 1,
            severity: 0,
            state: 1,
            message: "first".to_owned(),
            line: 0,
        };
        let second = InfoMessage {
            number: 2,
            ..first.clone()
        };
        ctx.emit_info(first.clone());
        ctx.emit_info(second.clone());
        assert_eq!(ctx.take_infos(), vec![first, second]);
        assert!(ctx.take_infos().is_empty());
    }

    /// `CollectSink` refuses a row that comes before the columns, and keeps the rest.
    #[test]
    fn a_row_before_the_columns_is_refused() {
        let mut sink = CollectSink::new();
        let refused = sink.row(&[Value::I32(1)]).unwrap_err();
        assert_eq!(refused.number, 50000);
        assert!(sink.rows.is_empty());
        sink.columns(&OutputSchema {
            columns: Vec::new(),
        })
        .expect("the columns are taken");
        sink.row(&[Value::I32(1)]).expect("a row after the columns");
        assert_eq!(sink.rows, vec![vec![Value::I32(1)]]);
        assert!(sink.schema.is_some());
    }
}
