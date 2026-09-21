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

/// Who pushed the current [`ExecContext::subquery_locals`] frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubqueryLocalsSource {
    /// EXISTS, IN or scalar evaluation over a correlated inner plan.
    CorrelatedPlan,
    /// Inner scan of a cross or semi join: row bindings are scan-local, not join-wide.
    JoinInnerScan,
}

struct SubqueryFrame {
    subplans: Vec<SubPlan>,
    slot: usize,
    cache: Vec<Option<Value>>,
}

/// One row of the outer-row stack.
struct OuterFrame {
    row: Row,
    /// Position `i` of `row` holds the value of `bindings[i]`. Empty when the layout of
    /// the row is not described: a reference is then read at its own index.
    bindings: Vec<ColumnBinding>,
    /// Whether the frame is the row of an enclosing query, which
    /// [`ColumnBinding::depth`] counts, rather than a row pushed for the inner side of a
    /// join or by [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval), which it
    /// does not (`tests/subquery.rs`,
    /// `cross_self_join_inside_a_correlated_scalar_keeps_both_aliases`).
    query: bool,
}

impl OuterFrame {
    /// The value of `binding` in this frame: at `binding.index` when the layout is not
    /// described or agrees with the binding there, else at the position whose
    /// binding carries the same column and name.
    fn read(&self, binding: &ColumnBinding) -> SqlResult<Value> {
        let at_index = self.bindings.get(binding.index);
        let position =
            if at_index.is_none_or(|b| b.column == binding.column && b.name == binding.name) {
                binding.index
            } else {
                self.bindings
                    .iter()
                    .position(|b| b.column == binding.column && b.name == binding.name)
                    .unwrap_or(binding.index)
            };
        self.row.get(position).cloned().ok_or_else(|| {
            bug(&format!(
                "read_outer_column: column `{}` is at index {position} of a row of {} value(s)",
                binding.name,
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
    locals: usize,
}

/// The columns of the plan an expression is evaluated in, pushed by a correlated
/// subquery evaluation or by the inner scan of a join.
struct LocalsFrame {
    source: SubqueryLocalsSource,
    columns: HashSet<ColumnId>,
    bindings: Vec<ColumnBinding>,
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

impl ExecSession {
    /// Reads a session variable; names match with `eq_ignore_ascii_case`.
    pub(crate) fn variable_value(&self, name: &str) -> Option<Value> {
        self.variables
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    /// The declared type of a session variable; names match with `eq_ignore_ascii_case`.
    pub(crate) fn variable_type(&self, name: &str) -> Option<TypeInfo> {
        self.variable_types
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, ty)| ty.clone())
    }

    /// Stores `value` under the spelling already in the session, or under `name` otherwise.
    pub(crate) fn store_variable_value(&mut self, name: &str, value: Value) {
        let key = self
            .variables
            .keys()
            .find(|key| key.eq_ignore_ascii_case(name))
            .cloned()
            .unwrap_or_else(|| name.to_owned());
        self.variables.insert(key, value);
    }
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
    /// The stack of outer rows. A correlated join pushes its outer row before opening
    /// the inner input: an [`IndexSeek`](crate::ops::seek::IndexSeek) evaluates its
    /// bounds with that row through [`eval_expr`](crate::expr::eval_expr), which consults
    /// the top of this stack when the current row is `None`. A subquery evaluation
    /// pushes the row of the query that encloses it, and a reference whose
    /// [`ColumnBinding::depth`] is `d` reads the `d`-th such row from the top
    /// ([`ExecContext::read_outer_column`]).
    outer_frames: Vec<OuterFrame>,
    /// One frame per active [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval)
    /// row, innermost last, so a nested `SubqueryEval` does not clobber its parent.
    subquery_frames: Vec<SubqueryFrame>,
    /// The columns of the plans being evaluated, innermost last: a correlated subquery
    /// evaluated under the inner scan of a join, or the reverse, sees its own frame and
    /// gives the enclosing one back when it pops.
    subquery_locals: Vec<LocalsFrame>,
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
            subquery_locals: Vec::new(),
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

    /// Pushes a row onto the outer-row stack for a correlated join or a
    /// [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval): the row is what
    /// [`eval_expr`](crate::expr::eval_expr) reads when it is given no row. Position `i`
    /// of `row` holds the value of `bindings[i]`.
    ///
    /// The row is not one of an enclosing query: a [`ColumnBinding::depth`] does not
    /// count it. Popped by [`ExecContext::pop_outer`].
    pub fn push_outer(&mut self, row: Row, bindings: Vec<ColumnBinding>) {
        self.outer_frames.push(OuterFrame {
            row,
            bindings,
            query: false,
        });
    }

    /// Pushes the row of the query that encloses a subquery about to be evaluated: the
    /// row a reference with [`ColumnBinding::depth`] 1 reads inside that subquery, and
    /// with depth 2 inside a subquery of it. `None` pushes an empty row, so that the
    /// depths stay aligned with the evaluations that enclose them.
    ///
    /// The layout of the row is the one of the top frame when that frame holds this
    /// same row (the [`SubqueryEval`](vauban_planner::PhysicalPlan::SubqueryEval) that
    /// produced it pushed it with the bindings of its input); otherwise the layout is not
    /// described and a reference is read at its own index. Popped by
    /// [`ExecContext::pop_outer`].
    pub(crate) fn push_query_row(&mut self, row: Option<&Row>) {
        let row = row.cloned().unwrap_or_default();
        let bindings = self
            .outer_frames
            .last()
            .filter(|top| top.row == row)
            .map_or_else(Vec::new, |top| top.bindings.clone());
        self.outer_frames.push(OuterFrame {
            row,
            bindings,
            query: true,
        });
    }

    /// Pops the row that [`ExecContext::push_outer`] or [`ExecContext::push_query_row`]
    /// pushed. The caller must match each push with one pop; the row is returned so that
    /// the caller does not need to clone it to keep it.
    #[must_use]
    pub fn pop_outer(&mut self) -> Option<Row> {
        self.outer_frames.pop().map(|frame| frame.row)
    }

    /// Reads one outer column from the stack.
    ///
    /// A `binding` with a [`ColumnBinding::depth`] of `d` (1 or more) is read from the
    /// `d`-th enclosing-query row from the top, at its index when the layout of that row
    /// agrees, else at the position that carries its column and name
    /// ([`OuterFrame::read`]). Two aliases of one table, whose columns carry the same
    /// identifier and name, are told apart by that depth alone (`tests/subquery.rs`,
    /// `correlated_scalar_over_the_same_table_reads_the_enclosing_row`,
    /// `correlated_exists_two_levels_deep_over_the_same_table`).
    ///
    /// A `binding` at depth 0 is searched by column and name from the innermost frame
    /// outward, the frames of joins included.
    ///
    /// # Errors
    ///
    /// The internal error 50000 when the stack holds fewer enclosing-query rows than
    /// `depth`, or when no frame exposes a depth 0 column.
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
        for frame in self.outer_frames.iter().rev() {
            let by_column_and_name = frame
                .bindings
                .iter()
                .position(|b| b.column == binding.column && b.name == binding.name);
            let Some(pos) = by_column_and_name.or_else(|| {
                frame
                    .bindings
                    .iter()
                    .position(|b| b.column == binding.column)
            }) else {
                continue;
            };
            return frame.row.get(pos).cloned().ok_or_else(|| {
                bug(&format!(
                    "read_outer_column: column `{}` is at index {pos} of a row of {} value(s)",
                    binding.name,
                    frame.row.len()
                ))
            });
        }
        Err(bug(&format!(
            "read_outer_column: column `{}` is evaluated without an outer row",
            binding.name
        )))
    }

    /// Where the correlation stacks stand now; see [`CorrelationMark`].
    #[must_use]
    pub(crate) fn correlation_mark(&self) -> CorrelationMark {
        CorrelationMark {
            outer: self.outer_frames.len(),
            frames: self.subquery_frames.len(),
            locals: self.subquery_locals.len(),
        }
    }

    /// Brings the correlation stacks back to `mark`, dropping what was pushed since.
    pub(crate) fn unwind_to(&mut self, mark: CorrelationMark) {
        self.outer_frames.truncate(mark.outer);
        self.subquery_frames.truncate(mark.frames);
        self.subquery_locals.truncate(mark.locals);
    }

    /// The innermost outer row, for [`eval_expr`](crate::expr::eval_expr) to resolve a
    /// `ColumnRef` whose row is `None`.
    #[must_use]
    pub(crate) fn last_outer_row(&self) -> Option<&Row> {
        self.outer_frames.last().map(|frame| &frame.row)
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
        self.open_subquery_eval(subplans);
        mark
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

    /// Pushes the columns of the plan about to be evaluated: a correlated subquery, or
    /// the inner scan of a join. Popped by [`ExecContext::pop_subquery_locals`], which
    /// gives the enclosing frame back.
    pub(crate) fn push_subquery_locals(
        &mut self,
        source: SubqueryLocalsSource,
        locals: HashSet<ColumnId>,
        bindings: Vec<ColumnBinding>,
    ) {
        self.subquery_locals.push(LocalsFrame {
            source,
            columns: locals,
            bindings,
        });
    }

    /// Pops the frame of [`ExecContext::push_subquery_locals`].
    pub(crate) fn pop_subquery_locals(&mut self) {
        self.subquery_locals.pop();
    }

    #[must_use]
    pub(crate) fn subquery_locals_source(&self) -> Option<SubqueryLocalsSource> {
        self.subquery_locals.last().map(|frame| frame.source)
    }

    #[must_use]
    pub(crate) fn subquery_row_binding_count(&self) -> usize {
        self.subquery_locals
            .last()
            .map_or(0, |frame| frame.bindings.len())
    }

    /// Evaluates a predicate over a concatenated join row.
    ///
    /// When the inner scan of a cross join pushed scan-local bindings, suspend them for
    /// one evaluation so [`ColumnRef`](vauban_binder::BoundExprKind::ColumnRef)s use
    /// join-wide indices on `row`.
    pub(crate) fn eval_over_concatenated_row<R>(
        &mut self,
        f: impl FnOnce(&mut Self) -> SqlResult<R>,
    ) -> SqlResult<R> {
        if self.subquery_locals_source() != Some(SubqueryLocalsSource::JoinInnerScan) {
            return f(self);
        }
        let suspended = self.subquery_locals.pop();
        let result = f(self);
        self.subquery_locals.extend(suspended);
        result
    }

    /// The column identifiers of the innermost plan being evaluated, `None` outside a
    /// correlated subquery and the inner scan of a join.
    #[must_use]
    pub(crate) fn subquery_locals(&self) -> Option<&HashSet<ColumnId>> {
        self.subquery_locals.last().map(|frame| &frame.columns)
    }

    pub(crate) fn read_local_column(&self, row: &Row, binding: &ColumnBinding) -> SqlResult<Value> {
        if let Some(bindings) = self.subquery_locals.last().map(|frame| &frame.bindings) {
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

    /// Whether `binding` is read from the outer-row stack rather than from the current
    /// row.
    ///
    /// A [`ColumnBinding::depth`] of 1 or more says so by itself: the binder wrote on the
    /// reference how many queries up its column lives, and the column identifier and
    /// name it carries may be those of a column of the inner plan too (two aliases of
    /// one table). A depth of 0 with a column identifier that no scan of the inner plan
    /// exposes is read from the stack as well; one the inner plan exposes is local even
    /// under another name, since a second table of the plan may carry the identifier
    /// under that other name (`tests/subquery.rs`,
    /// `uncorrelated_cross_join_of_two_tables_inside_a_scalar_counts_its_pairs`).
    pub(crate) fn column_is_outer(&self, binding: &ColumnBinding) -> bool {
        if binding.column == SLOT_COLUMN_ID {
            return false;
        }
        if binding.depth > 0 {
            return true;
        }
        if self.outer_frames.is_empty() {
            return false;
        }
        self.subquery_locals
            .last()
            .is_some_and(|frame| !frame.columns.contains(&binding.column))
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
