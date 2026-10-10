//! What the executor needs besides the statement it runs, and what it hands back while
//! it runs: the context, the cancellation token, the session state it writes. The sink
//! the results go to is `sink.rs`.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use vauban_binder::{BatchVariables, ColumnBinding, SessionOptions, SubqueryId};
use vauban_catalog::Catalog;
use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult};
use vauban_planner::SubPlan;
use vauban_storage::{SavepointId, Snapshot, Storage};
use vauban_sysfn::EvalContext;
use vauban_txn::{IsolationLevel, LockTimeout, LockWait, TransactionManager, TxnHandle};
use vauban_types::{Decimal, TypeInfo, Value};

use crate::interp::NameMap;
use crate::row::Row;

/// The subquery plans of one [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval)
/// or of one statement expression, shared with the operator that opened the frame, and
/// the values of the uncorrelated ones, at the same positions.
struct SubqueryFrame {
    subplans: Rc<[SubPlan]>,
    cache: Vec<Option<Value>>,
}

/// Where [`ExecContext::find_subplan`] found the plan of a subquery: the table it is in,
/// shared rather than copied, and its place in the frame stack, which the cache of an
/// uncorrelated subquery is read and written at.
pub(crate) struct FoundSubplan {
    table: Rc<[SubPlan]>,
    frame: usize,
    index: usize,
}

impl FoundSubplan {
    /// The plan found.
    #[must_use]
    pub(crate) fn subplan(&self) -> &SubPlan {
        &self.table[self.index]
    }
}

/// One row of the outer-row stack.
struct OuterFrame {
    row: Row,
    /// Whether the frame is the row of an enclosing query, which
    /// [`ColumnBinding::depth`] counts, rather than the outer row of a join, which it
    /// does not (`tests/subquery.rs`,
    /// `cross_self_join_inside_a_correlated_scalar_keeps_both_aliases`).
    query: bool,
}

impl OuterFrame {
    /// The value at `binding.index` of this frame's row.
    fn read(&self, binding: &ColumnBinding) -> SqlResult<Value> {
        self.row.get(binding.index).cloned().ok_or_else(|| {
            bug(&format!(
                "read_outer_column: column `{}` is at index {} of a row of {} value(s)",
                binding.name,
                binding.index,
                self.row.len()
            ))
        })
    }
}

/// Where the correlation stacks of an [`ExecContext`] stand: taken before a subquery plan
/// is evaluated, given back to [`ExecContext::unwind_to`] once it is. An inner plan
/// closed before its input is exhausted (an `EXISTS` stops at its first row) leaves on
/// the stacks the row and the frame its own `SubqueryEval` pushed; unwinding removes
/// them, so that the enclosing evaluation finds its own frame on top again
/// (`tests/subquery.rs`, `a_second_subquery_after_a_nested_exists_takes_its_own_plan`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct CorrelationMark {
    outer: usize,
    frames: usize,
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
    /// The variables of the running level, keyed by name with the `@`; a read hashes the
    /// name to the position it took when it was declared (`interp/frame.rs`).
    pub variables: NameMap<Value>,
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
    /// `SET NOCOUNT` of the running level: when set, a program hands the end of a
    /// statement to the sink without its row count.
    pub nocount: bool,
    /// How many calls deep the running level is: 0 for the batch, one more for each
    /// `EXECUTE` a program runs (`interp/frame.rs`).
    pub nest_level: u32,
    /// The deepest level a call may open, unbounded when `None`.
    pub max_nest_level: Option<u32>,
}

impl ExecSession {
    /// Reads a variable of the running level; names match as the binder matches them
    /// ([`vauban_binder::BatchVariables::name_key`]).
    pub(crate) fn variable_value(&self, name: &str) -> Option<Value> {
        self.variables.get(name).cloned()
    }

    /// The declared type of a variable of the running level; names match as the binder
    /// matches them.
    pub(crate) fn variable_type(&self, name: &str) -> Option<TypeInfo> {
        if let Some(ty) = self.variable_types.get(name) {
            return Some(ty.clone());
        }
        let key = BatchVariables::name_key(name);
        self.variable_types
            .iter()
            .find(|(declared, _)| BatchVariables::name_key(declared) == key)
            .map(|(_, ty)| ty.clone())
    }

    /// Stores `value` under the spelling already in the session, or under `name` otherwise.
    pub(crate) fn store_variable_value(&mut self, name: &str, value: Value) {
        self.variables.insert(name.to_owned(), value);
    }
}

impl Default for ExecSession {
    fn default() -> Self {
        Self {
            variables: NameMap::new(),
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
            nocount: false,
            nest_level: 0,
            max_nest_level: None,
        }
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
    /// The stack of outer rows. A nested-loop join pushes its outer row before opening
    /// the inner input: an [`IndexSeek`](crate::ops::seek::IndexSeek) evaluates its
    /// bounds with that row through [`eval_expr`](crate::expr::eval_expr), which reads
    /// the innermost join row when the current row is `None`. A subquery evaluation
    /// pushes the row of the query that encloses it, and a reference whose
    /// [`ColumnBinding::depth`] is `d` reads the `d`-th such row from the top, at its
    /// index ([`ExecContext::read_outer_column`]).
    outer_frames: Vec<OuterFrame>,
    /// One frame per active [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval)
    /// row, innermost last, so a nested `SubqueryEval` does not clobber its parent.
    subquery_frames: Vec<SubqueryFrame>,
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
            outer_frames: Vec::new(),
            subquery_frames: Vec::new(),
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

    /// Pushes the outer row of a nested-loop join onto the outer-row stack, before the
    /// join opens its inner input: the row is what [`eval_expr`](crate::expr::eval_expr)
    /// reads a column from when it is given no row, at the index of the reference.
    ///
    /// The row is not one of an enclosing query: a [`ColumnBinding::depth`] does not
    /// count it. Popped by [`ExecContext::pop_outer`].
    pub fn push_outer(&mut self, row: Row) {
        self.outer_frames.push(OuterFrame { row, query: false });
    }

    /// Pushes the row of the query that encloses a subquery about to be evaluated: the
    /// row a reference with [`ColumnBinding::depth`] 1 reads inside that subquery, and
    /// with depth 2 inside a subquery of it. `None` pushes an empty row, so that the
    /// depths stay aligned with the evaluations that enclose them. Popped by
    /// [`ExecContext::pop_outer`].
    pub(crate) fn push_query_row(&mut self, row: Option<&Row>) {
        let row = row.cloned().unwrap_or_default();
        self.outer_frames.push(OuterFrame { row, query: true });
    }

    /// Pops the row that [`ExecContext::push_outer`] or [`ExecContext::push_query_row`]
    /// pushed. The caller must match each push with one pop; the row is returned so that
    /// the caller does not need to clone it to keep it.
    #[must_use]
    pub fn pop_outer(&mut self) -> Option<Row> {
        self.outer_frames.pop().map(|frame| frame.row)
    }

    /// Reads one outer column from the stack, at the index the reference carries.
    ///
    /// A `binding` with a [`ColumnBinding::depth`] of `d` (1 or more) is read from the
    /// `d`-th enclosing-query row from the top. Two aliases of one table, whose columns
    /// carry the same identifier and name, are told apart by that depth alone
    /// (`tests/subquery.rs`, `correlated_scalar_over_the_same_table_reads_the_enclosing_row`,
    /// `correlated_exists_two_levels_deep_over_the_same_table`).
    ///
    /// A `binding` at depth 0 is read from the row on top of the stack, which must be the
    /// outer row of a join: the bound of an [`IndexSeek`](crate::ops::seek::IndexSeek) on
    /// the inner side of that join reads it.
    ///
    /// # Errors
    ///
    /// The internal error 50000 when the stack holds fewer enclosing-query rows than
    /// `depth`, when the top of the stack is not a join row for a depth 0 column, or when
    /// the row is
    /// narrower than the index.
    pub(crate) fn read_outer_column(&self, binding: &ColumnBinding) -> SqlResult<Value> {
        if binding.depth > 0 {
            let hops = usize::from(binding.depth);
            let frame = self
                .outer_frames
                .iter()
                .rev()
                .filter(|frame| frame.query)
                .nth(hops - 1)
                .ok_or_else(|| {
                    bug(&format!(
                        "read_outer_column: column `{}` is {hops} quer(y|ies) up but {} enclosing row(s) are pushed",
                        binding.name,
                        self.outer_frames.iter().filter(|frame| frame.query).count()
                    ))
                })?;
            return frame.read(binding);
        }
        let frame = self
            .outer_frames
            .last()
            .filter(|frame| !frame.query)
            .ok_or_else(|| {
                bug(&format!(
                    "read_outer_column: column `{}` is evaluated without an outer row",
                    binding.name
                ))
            })?;
        frame.read(binding)
    }

    /// Where the correlation stacks stand now; see [`CorrelationMark`].
    #[must_use]
    pub(crate) fn correlation_mark(&self) -> CorrelationMark {
        CorrelationMark {
            outer: self.outer_frames.len(),
            frames: self.subquery_frames.len(),
        }
    }

    /// Brings the correlation stacks back to `mark`, dropping what was pushed since.
    pub(crate) fn unwind_to(&mut self, mark: CorrelationMark) {
        self.outer_frames.truncate(mark.outer);
        self.subquery_frames.truncate(mark.frames);
    }

    /// Opens one [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval) operator over
    /// the plans it shares.
    pub(crate) fn open_subquery_eval(&mut self, subplans: Rc<[SubPlan]>) {
        let cache = vec![None; subplans.len()];
        self.subquery_frames.push(SubqueryFrame { subplans, cache });
    }

    /// Closes the innermost [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval) operator.
    pub(crate) fn close_subquery_eval(&mut self) {
        self.subquery_frames.pop();
    }

    /// Installs the plans an expression evaluated outside a plan carries — the value of a
    /// `SET`, the initial value of a `DECLARE` — where
    /// [`eval_expr`](crate::expr::eval_expr) looks for the plans of a subquery, and
    /// answers where the correlation stacks stood, for [`ExecContext::unwind_to`] to take
    /// them back once the expression is evaluated (`tests/subquery.rs`,
    /// `a_set_from_a_subquery_of_one_row_takes_its_value`).
    ///
    /// A frame is opened for one evaluation and dropped with it, cache included: the
    /// second turn of a `WHILE` runs its subquery again rather than reading what the
    /// first stored (`tests/subquery.rs`, `a_set_run_twice_evaluates_its_subquery_twice`).
    pub(crate) fn open_statement_subplans(&mut self, subplans: &[SubPlan]) -> CorrelationMark {
        let mark = self.correlation_mark();
        self.open_subquery_eval(Rc::from(subplans));
        mark
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

    /// The plan of the subquery whose identity is `id`, looked for from the innermost
    /// frame outwards. Which subqueries were evaluated before this one, or skipped, does
    /// not enter the lookup (`tests/subquery.rs`,
    /// `case_branches_each_take_the_plan_of_their_own_subquery`).
    ///
    /// # Errors
    ///
    /// The internal error 50000 when no open frame holds a plan of that identity.
    pub(crate) fn find_subplan(&self, id: SubqueryId) -> SqlResult<FoundSubplan> {
        for (frame, entry) in self.subquery_frames.iter().enumerate().rev() {
            if let Some(index) = entry.subplans.iter().position(|sub| sub.id == id) {
                return Ok(FoundSubplan {
                    table: Rc::clone(&entry.subplans),
                    frame,
                    index,
                });
            }
        }
        Err(bug("ExecContext: no plan for this subquery"))
    }

    /// A cached uncorrelated subquery value, when one was stored at `found`.
    #[must_use]
    pub(crate) fn subquery_cached(&self, found: &FoundSubplan) -> Option<Value> {
        self.subquery_frames
            .get(found.frame)?
            .cache
            .get(found.index)?
            .clone()
    }

    /// Stores the value of an uncorrelated subquery at `found`.
    pub(crate) fn store_subquery_cache(&mut self, found: &FoundSubplan, value: Value) {
        if let Some(entry) = self
            .subquery_frames
            .get_mut(found.frame)
            .and_then(|frame| frame.cache.get_mut(found.index))
        {
            *entry = Some(value);
        }
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
        crate::ops::subquery::eval_exists_plan(plan, self, None)
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
mod tests;
