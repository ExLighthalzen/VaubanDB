//! The lock manager: lock modes, row and table resources, the compatibility matrix, the
//! fair queue, conversion and the wait.
//!
//! This file carries no SQL meaning: nothing here knows an isolation level, a table hint or
//! an error number. A lock is taken, waited for, converted and given back; who takes which
//! mode belongs to `isolation.rs`, `schema_lock.rs` and `table_lock.rs`, and what a refusal
//! reports (errors 1222 and 1205) to `deadlock.rs`.
//!
//! # Modes
//!
//! The seven modes of [`LockMode`] and their compatibility are those of SQL Server.
//! [`LockMode::compatible_with`] is the matrix; `compatibility_matrix_matches_the_table`
//! (`tests/lock.rs`) holds the 49 couples as data and checks each couple in both
//! directions.
//!
//! Range locks, `PAGLOCK` and the `SIX` mode of SQL Server are **not modelled** here. A row
//! lock and a table lock on the same table are two unrelated resources in this file — the
//! intent lock a row lock implies on its table is taken by the caller
//! (`tests/lock.rs`, `row_and_table_are_distinct_resources`).
//!
//! # Strength, re-entrance and conversion
//!
//! A transaction holds at most one **data** mode (`IS`, `S`, `U`, `IX`, `X`) and at most one
//! **schema** mode (`Sch-S`, `Sch-M`) per resource, because the two families never convert
//! into each other. Inside a family the strength order is `IS < S`, `IS < IX`, `S < U < X`,
//! `IX < X`, `Sch-S < Sch-M` ([`LockMode::covers`]). Asking for a mode already covered by
//! the one held is granted at once, without touching the queue
//! (`tests/lock.rs`, `reentrant_lock_is_free`). Asking for a mode that is not covered is a
//! **conversion** to [`LockMode::join`] of the two — `S` and `IX`, which do not compare,
//! convert to `X` — and a conversion waits only for the **other** holders: it is placed
//! ahead of the plain candidates of the queue (`tests/lock.rs`,
//! `conversion_jumps_the_queue`).
//!
//! # The queue
//!
//! One FIFO queue per resource. Only the **front** of a queue is ever granted, and a grant
//! pass stops at the first candidate the holders refuse: a candidate compatible with the
//! holders but standing behind an older candidate they refuse waits behind it
//! (`tests/lock.rs`, `a_compatible_latecomer_waits_behind_an_older_waiter`, and the order
//! `fifo_has_no_overtaking` asserts). The one exception is the conversion above,
//! marked at the line that inserts it in [`LockTable::enqueue`].
//!
//! # The wait
//!
//! One [`Mutex`] over the whole table and one [`Condvar`]: a state change notifies the
//! waiters of the manager, each of them re-reads its own ticket. Waiting is
//! [`Condvar::wait_timeout`], not a `sleep` loop; each call waits at most
//! [`POLL_SLICE`], so that a cancellation flag raised by a thread that cannot notify this
//! condvar is seen within one slice (the session passes the token of its ATTENTION
//! handling). Between two slices the thread is parked by the condvar, not running.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{RowId, TableId, TxnId};

use crate::deadlock::{self, DeadlockMonitor};
use crate::{LockInfo, LockStatus, LockTimeout};

/// Longest single [`Condvar::wait_timeout`] a waiter performs, so that a raised
/// cancellation flag is seen within one slice.
///
/// Fifty milliseconds is the slice the session already uses to make `WAITFOR`
/// interruptible (`crates/vauban-session/src/cancel.rs`).
const POLL_SLICE: Duration = Duration::from_millis(50);

/// What a lock is taken on: one row, or one whole table.
///
/// The two are **unrelated** resources for this file: locking `Row(t, 1)` leaves
/// `Table(t)` free (`tests/lock.rs`, `row_and_table_are_distinct_resources`). The intent
/// lock a row lock implies on its table is not derived here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockResource {
    /// One row of one table.
    Row(TableId, RowId),
    /// One whole table.
    Table(TableId),
}

/// A lock mode of SQL Server.
///
/// `SIX`, the range modes and the bulk-update mode of SQL Server are not modelled (module
/// documentation above).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockMode {
    /// Shared: read the resource.
    S,
    /// Exclusive: change the resource.
    X,
    /// Update: read with the intention of changing, so that two readers do not both
    /// convert to `X` at once.
    U,
    /// Intent shared: a shared lock is held, or will be, lower down.
    IS,
    /// Intent exclusive: an exclusive lock is held, or will be, lower down.
    IX,
    /// Schema stability: the schema of the object must not change while it is read.
    SchS,
    /// Schema modification: the schema of the object is being changed.
    SchM,
}

/// How a [`LockManager::try_acquire`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockOutcome {
    /// The lock is held by the transaction that asked for it.
    Granted,
    /// The wait reached the end of its [`LockTimeout`] — what `deadlock.rs` turns into 1222.
    TimedOut,
    /// The wait was refused because it would close a cycle — what `deadlock.rs` turns into
    /// 1205.
    Deadlock {
        /// The transaction chosen to be rolled back.
        victim: TxnId,
    },
    /// The wait was interrupted by the cancellation token of the caller ([`LockWait`]).
    ///
    /// Distinct from [`LockOutcome::TimedOut`]: an ATTENTION cancels the request instead of
    /// reporting 1222; what the client is told belongs to the session.
    Cancelled,
}

/// The cancellation token a caller hands to a wait, so that another thread may cut it
/// short.
///
/// [`LockWait::none`] is the token of a caller that does not cancel — what
/// [`crate::TransactionManager::lock_row`] passes, since its signature has no parameter
/// for one. A session clones the flag it already raises on ATTENTION into
/// [`LockWait::from_flag`]; the cancellation token of the execution context is what should
/// replace this type when it exists.
#[derive(Debug, Clone, Default)]
pub struct LockWait {
    /// Raised by [`LockWait::cancel`] on this token or any clone of it.
    cancelled: Arc<AtomicBool>,
}

impl LockWait {
    /// A token nothing has cancelled yet.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A token over a flag the caller already owns, shared with it.
    #[must_use]
    pub fn from_flag(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }

    /// Asks the wait started with this token to give up at its next slice.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// `true` once [`LockWait::cancel`] was called on this token or a clone of it.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl LockMode {
    /// The modes this one is at least as strong as, itself included.
    ///
    /// The order written in the module documentation: `IS < S`, `IS < IX`, `S < U < X`,
    /// `IX < X`, `Sch-S < Sch-M`. `S` and `IX` are not comparable, and no data mode
    /// compares with a schema mode.
    const fn below(self) -> &'static [LockMode] {
        match self {
            LockMode::IS => &[LockMode::IS],
            LockMode::S => &[LockMode::IS, LockMode::S],
            LockMode::U => &[LockMode::IS, LockMode::S, LockMode::U],
            LockMode::IX => &[LockMode::IS, LockMode::IX],
            LockMode::X => &[
                LockMode::IS,
                LockMode::S,
                LockMode::U,
                LockMode::IX,
                LockMode::X,
            ],
            LockMode::SchS => &[LockMode::SchS],
            LockMode::SchM => &[LockMode::SchS, LockMode::SchM],
        }
    }

    /// `true` when holding `self` already gives what `other` asks for.
    ///
    /// `X` covers the four other data modes, `U` covers `S` and `IS`, `Sch-M` covers
    /// `Sch-S` (`tests/lock.rs`, `strength_order_is_the_one_documented`).
    #[must_use]
    pub fn covers(self, other: LockMode) -> bool {
        self.below().contains(&other)
    }

    /// `true` when this mode is one of `IS`, `S`, `U`, `IX`, `X`.
    ///
    /// A data mode and a schema mode held by one transaction on one resource are two
    /// independent locks: they do not convert into each other (module documentation).
    #[must_use]
    pub fn is_data(self) -> bool {
        !matches!(self, LockMode::SchS | LockMode::SchM)
    }

    /// The weakest mode that covers both, inside one family.
    ///
    /// `S` and `IX`, `U` and `IX`, which do not compare, join on `X`
    /// (`tests/lock.rs`, `strength_order_is_the_one_documented`). The two families never
    /// meet here: a request of the other family is a separate lock, not a conversion.
    #[must_use]
    pub fn join(self, other: LockMode) -> LockMode {
        if self.covers(other) {
            self
        } else if other.covers(self) {
            other
        } else if self.is_data() && other.is_data() {
            LockMode::X
        } else {
            // Unreachable: the schema family is the chain Sch-S < Sch-M, so two schema
            // modes compare, and a cross-family pair is not joined (callers check `is_data`
            // first).
            LockMode::SchM
        }
    }

    /// `true` when a transaction may hold `self` while **another** transaction holds
    /// `held`.
    ///
    /// The lock compatibility matrix of SQL Server. It is symmetric, which
    /// `compatibility_matrix_matches_the_table` (`tests/lock.rs`) checks on the 49 couples.
    #[must_use]
    pub fn compatible_with(self, held: LockMode) -> bool {
        use LockMode::{IS, IX, S, SchM, SchS, U, X};
        match (self, held) {
            // Sch-M refuses everything, and everything refuses Sch-M.
            (SchM, _) | (_, SchM) => false,
            // What Sch-S stands in the way of is Sch-M, handled above.
            (SchS, _) | (_, SchS) => true,
            // X refuses the five data modes.
            (X, _) | (_, X) => false,
            (IS, _) | (_, IS) => true,
            (U, U) => false,
            (S, U) | (U, S) => true,
            (S, S) => true,
            (IX, IX) => true,
            // The remaining data couples: IX against S or U.
            (IX, _) | (_, IX) => false,
        }
    }
}

/// One lock held by one transaction on one resource.
#[derive(Debug, Clone, Copy)]
struct Holder {
    /// The transaction that holds it.
    txn: TxnId,
    /// The mode it holds, the strongest it converted to.
    mode: LockMode,
}

/// One candidate waiting in the queue of one resource.
#[derive(Debug, Clone, Copy)]
struct Waiter {
    /// The ticket its own thread watches, unique over the manager.
    ticket: u64,
    /// The transaction that asked.
    txn: TxnId,
    /// The mode asked for; for a conversion, the joined mode
    /// ([`LockMode::join`]).
    mode: LockMode,
    /// `true` when the transaction already holds a weaker mode of the same family here.
    conversion: bool,
    /// When the request joined the queue, what `waiting_ms` of
    /// [`crate::LockInfo`] counts from.
    since: Instant,
}

/// The holders and the queue of one resource.
#[derive(Debug, Default)]
struct Entry {
    /// The locks held, at most one data mode and one schema mode per transaction.
    holders: Vec<Holder>,
    /// The candidates, in the order they may be granted: conversions first, then plain
    /// candidates in arrival order.
    queue: VecDeque<Waiter>,
}

impl Entry {
    /// The mode this transaction holds here in the family of `mode`, with its position.
    fn holder_of(&self, txn: TxnId, mode: LockMode) -> Option<(usize, LockMode)> {
        self.holders
            .iter()
            .position(|h| h.txn == txn && h.mode.is_data() == mode.is_data())
            .map(|pos| (pos, self.holders[pos].mode))
    }

    /// `true` when `mode` asked by `txn` stands against no holder of another transaction.
    fn holders_allow(&self, txn: TxnId, mode: LockMode) -> bool {
        self.holders
            .iter()
            .all(|h| h.txn == txn || mode.compatible_with(h.mode))
    }

    /// Marks the lock as held, replacing the weaker mode of the same family if any.
    fn grant_to(&mut self, txn: TxnId, mode: LockMode) {
        match self.holder_of(txn, mode) {
            Some((pos, _)) => self.holders[pos].mode = mode,
            None => self.holders.push(Holder { txn, mode }),
        }
    }

    /// `true` when nothing is held here and nobody waits, so the entry may be forgotten.
    fn is_empty(&self) -> bool {
        self.holders.is_empty() && self.queue.is_empty()
    }
}

/// What a request found when it reached the table.
enum Request {
    /// Already covered by a mode the transaction holds: nothing was queued.
    Reentrant,
    /// Queued under this ticket, waiting for the holders.
    Queued(u64),
}

/// The resources the manager knows about, plus the tickets granted since their waiter
/// last looked.
#[derive(Debug, Default)]
struct LockTable {
    /// Resources with at least one holder or one waiter.
    entries: HashMap<LockResource, Entry>,
    /// Ticket of the next candidate to queue.
    next_ticket: u64,
    /// Tickets a grant pass has honoured, each removed by the thread that owns it.
    granted: Vec<u64>,
}

impl LockTable {
    /// Places a request in the table, or reports that the transaction already covers it.
    ///
    /// A conversion — a mode of a family the transaction already holds here, not covered
    /// by what it holds — enters the queue **ahead** of the plain candidates.
    fn enqueue(&mut self, txn: TxnId, resource: LockResource, mode: LockMode) -> Request {
        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.saturating_add(1);
        let entry = self.entries.entry(resource).or_default();
        let mut asked = mode;
        let mut conversion = false;
        if let Some((_, held)) = entry.holder_of(txn, mode) {
            if held.covers(mode) {
                return Request::Reentrant;
            }
            asked = held.join(mode);
            conversion = true;
        }
        let waiter = Waiter {
            ticket,
            txn,
            mode: asked,
            conversion,
            since: Instant::now(),
        };
        if conversion {
            // The one exception to the fair queue: a conversion waits for the other
            // holders, not for the candidates that arrived before it, because those
            // candidates are themselves waiting for the mode this transaction already
            // holds (`tests/lock.rs`, `conversion_jumps_the_queue`).
            let at = entry.queue.iter().take_while(|w| w.conversion).count();
            entry.queue.insert(at, waiter);
        } else {
            entry.queue.push_back(waiter);
        }
        Request::Queued(ticket)
    }

    /// Grants the front of the queue of `resource` as far as the holders allow.
    ///
    /// Stops at the first candidate a holder refuses: nothing behind it is looked at, which
    /// is what keeps a compatible latecomer behind an older candidate.
    fn grant_pass(&mut self, resource: LockResource) {
        let Some(entry) = self.entries.get_mut(&resource) else {
            return;
        };
        while let Some(front) = entry.queue.front().copied() {
            if !entry.holders_allow(front.txn, front.mode) {
                break;
            }
            entry.queue.pop_front();
            entry.grant_to(front.txn, front.mode);
            self.granted.push(front.ticket);
        }
        if entry.is_empty() {
            self.entries.remove(&resource);
        }
    }

    /// Takes the ticket out of the queue it sits in, if it is still there.
    ///
    /// Returns `true` when the ticket was found, so the caller knows a grant pass is worth
    /// running on the resource it left.
    fn drop_ticket(&mut self, resource: LockResource, ticket: u64) -> bool {
        let Some(entry) = self.entries.get_mut(&resource) else {
            return false;
        };
        let Some(pos) = entry.queue.iter().position(|w| w.ticket == ticket) else {
            return false;
        };
        entry.queue.remove(pos);
        true
    }

    /// Removes the ticket from the list of honoured ones, reporting whether it was there.
    fn take_granted(&mut self, ticket: u64) -> bool {
        match self.granted.iter().position(|&t| t == ticket) {
            Some(pos) => {
                self.granted.remove(pos);
                true
            }
            None => false,
        }
    }

    /// Everything held, resource by resource in the order [`LockTable::waiters`] uses.
    ///
    /// The other half of the wait-for graph of `deadlock.rs`: an edge runs from a waiter of
    /// a resource to each holder of that resource whose mode refuses it.
    fn holders(&self) -> Vec<(TxnId, LockResource, LockMode)> {
        let mut resources: Vec<&LockResource> = self.entries.keys().collect();
        resources.sort_unstable();
        resources
            .into_iter()
            .flat_map(|res| {
                self.entries[res]
                    .holders
                    .iter()
                    .map(move |h| (h.txn, *res, h.mode))
            })
            .collect()
    }

    /// Everything queued, resource by resource in a stable order, each queue in grant
    /// order.
    fn waiters(&self) -> Vec<(TxnId, LockResource, LockMode)> {
        let mut resources: Vec<&LockResource> = self.entries.keys().collect();
        resources.sort_unstable();
        resources
            .into_iter()
            .flat_map(|res| {
                self.entries[res]
                    .queue
                    .iter()
                    .map(move |w| (w.txn, *res, w.mode))
            })
            .collect()
    }
}

/// The lock manager of one server: modes, resources, compatibility, fair queue, conversion
/// and wait.
///
/// Shared between threads behind an [`Arc`]; each method takes `&self`. See the module
/// documentation for the rules it applies.
#[derive(Debug, Default)]
pub struct LockManager {
    /// The resources and their queues.
    table: Mutex<LockTable>,
    /// Raised on each change of [`LockManager::table`]; a woken waiter re-reads its ticket.
    signal: Condvar,
    /// The deadlock hook: consulted before a thread goes to sleep.
    monitor: DeadlockMonitor,
}

impl LockManager {
    /// A manager holding nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            table: Mutex::default(),
            signal: Condvar::new(),
            monitor: DeadlockMonitor::new(),
        }
    }

    /// The deadlock monitor of this manager, the holder of the `DEADLOCK_PRIORITY` map
    /// `TransactionManager::set_deadlock_priority` writes to (`deadlock.rs`).
    pub(crate) fn monitor(&self) -> &DeadlockMonitor {
        &self.monitor
    }

    /// The table, recovering from a poisoned lock instead of panicking, as
    /// `TransactionManager::lock` does: the manager sits on the execution path of a query,
    /// which must not panic, and three of its methods return no
    /// `Result` to carry the poisoning to the caller. No code holding this guard panics:
    /// the block between `lock()` and its drop contains no `unwrap`, no `expect` and no
    /// indexing outside a length just read.
    fn state(&self) -> MutexGuard<'_, LockTable> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes `mode` on `resource` for `txn`, waiting as `timeout` allows.
    ///
    /// Calls [`LockManager::try_acquire`] and turns an outcome other than
    /// [`LockOutcome::Granted`] into the error of `deadlock::to_error` — the
    /// numbers 1222 and 1205.
    ///
    /// # Errors
    ///
    /// The error of `deadlock::to_error` when the lock was not granted: the timeout ran
    /// out, the wait was cancelled, or a cycle was found.
    pub fn lock(
        &self,
        txn: TxnId,
        resource: LockResource,
        mode: LockMode,
        timeout: LockTimeout,
        wait: &LockWait,
    ) -> SqlResult<()> {
        match self.try_acquire(txn, resource, mode, timeout, wait) {
            LockOutcome::Granted => Ok(()),
            other => Err(deadlock::to_error(self, other, resource)),
        }
    }

    /// Takes `mode` on `resource` for `txn` and reports how it ended, without turning the
    /// refusal into an error. The probe of `tests/lock.rs` and of `deadlock.rs`.
    ///
    /// - a mode the transaction already covers here returns [`LockOutcome::Granted`] at
    ///   once, queue untouched (`tests/lock.rs`, `reentrant_lock_is_free`);
    /// - a stronger mode is a conversion, placed ahead of the plain candidates;
    /// - otherwise the request joins the back of the FIFO queue of the resource and is
    ///   granted when it reaches the front and the holders allow it.
    ///
    /// [`LockTimeout::NoWait`] never parks the thread (`tests/lock.rs`,
    /// `nowait_returns_at_once`); [`LockTimeout::Millis`] parks it until the deadline
    /// (`tests/lock.rs`, `millis_times_out`); [`LockTimeout::Infinite`] parks it until the
    /// lock is granted or `wait` is cancelled.
    pub fn try_acquire(
        &self,
        txn: TxnId,
        resource: LockResource,
        mode: LockMode,
        timeout: LockTimeout,
        wait: &LockWait,
    ) -> LockOutcome {
        let mut state = self.state();
        let ticket = match state.enqueue(txn, resource, mode) {
            Request::Reentrant => return LockOutcome::Granted,
            Request::Queued(ticket) => ticket,
        };
        state.grant_pass(resource);
        if state.take_granted(ticket) {
            self.signal.notify_all();
            return LockOutcome::Granted;
        }
        let deadline = match timeout {
            LockTimeout::NoWait => {
                self.give_up(&mut state, resource, ticket);
                return LockOutcome::TimedOut;
            }
            LockTimeout::Millis(ms) => Some(Instant::now() + Duration::from_millis(u64::from(ms))),
            LockTimeout::Infinite => None,
        };
        loop {
            if wait.is_cancelled() {
                self.give_up(&mut state, resource, ticket);
                return LockOutcome::Cancelled;
            }
            // The deadlock hook: the cycle is looked for before the thread parks, on
            // the queues and the holders as they stand. It answers `Some` to the thread of
            // the victim it picked, and `None` to the other branches of the cycle.
            if let Some(victim) = self.monitor.detect(&state.waiters(), &state.holders(), txn) {
                self.give_up(&mut state, resource, ticket);
                return LockOutcome::Deadlock { victim };
            }
            let slice = match deadline {
                None => POLL_SLICE,
                Some(end) => {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        self.give_up(&mut state, resource, ticket);
                        return LockOutcome::TimedOut;
                    }
                    left.min(POLL_SLICE)
                }
            };
            let (guard, _) = self
                .signal
                .wait_timeout(state, slice)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
            if state.take_granted(ticket) {
                return LockOutcome::Granted;
            }
        }
    }

    /// Leaves the queue, then lets the candidates behind take their turn.
    ///
    /// A ticket honoured between the last look and this call is honoured for good: the
    /// caller has already read it out of [`LockTable::granted`], so `drop_ticket` finds
    /// nothing and the holder stays. The guard stays with the caller, which returns right
    /// after; the notification is read by the waiters once they get the lock back.
    fn give_up(&self, state: &mut LockTable, resource: LockResource, ticket: u64) {
        if state.drop_ticket(resource, ticket) {
            state.grant_pass(resource);
        }
        self.signal.notify_all();
    }

    /// Gives back what `txn` holds on `resource` and hands it to the candidates it was
    /// blocking.
    ///
    /// Both families go: a transaction that held `IX` and `Sch-S` on the same resource
    /// holds neither afterwards. A queued request of `txn` on this resource — a conversion
    /// another thread of the same transaction is waiting for — leaves the queue too.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `txn` holds nothing on `resource`: giving back a lock
    /// that was never taken is a caller bug, as a second `commit` of one handle is for
    /// `TransactionManager::commit` (`tests/lock.rs`, `unlock_without_a_lock_is_a_bug`).
    pub fn unlock(&self, txn: TxnId, resource: LockResource) -> SqlResult<()> {
        let mut state = self.state();
        let Some(entry) = state.entries.get_mut(&resource) else {
            return Err(not_held(txn, resource));
        };
        let before = entry.holders.len();
        entry.holders.retain(|h| h.txn != txn);
        if entry.holders.len() == before {
            return Err(not_held(txn, resource));
        }
        entry.queue.retain(|w| w.txn != txn);
        state.grant_pass(resource);
        drop(state);
        self.signal.notify_all();
        Ok(())
    }

    /// Gives back the locks of `txn` and wakes the candidates it was blocking.
    ///
    /// Called by `TransactionManager::commit` and `TransactionManager::rollback` once
    /// `storage` has accepted the outcome. A transaction that holds nothing is accepted, so
    /// the call is idempotent (`tests/lock.rs`, `release_all_of_an_unknown_txn_is_quiet`).
    /// Every candidate the transaction was blocking is looked at in one pass
    /// (`tests/lock.rs`, `release_all_wakes_every_waiter`, three threads).
    pub fn release_all(&self, txn: TxnId) {
        let mut state = self.state();
        let touched: Vec<LockResource> = state
            .entries
            .iter()
            .filter(|(_, e)| {
                e.holders.iter().any(|h| h.txn == txn) || e.queue.iter().any(|w| w.txn == txn)
            })
            .map(|(res, _)| *res)
            .collect();
        for resource in &touched {
            if let Some(entry) = state.entries.get_mut(resource) {
                entry.holders.retain(|h| h.txn != txn);
                entry.queue.retain(|w| w.txn != txn);
            }
            state.grant_pass(*resource);
        }
        drop(state);
        self.signal.notify_all();
    }

    /// What `txn` holds right now, resource by resource in a stable order.
    ///
    /// A transaction holding two modes of two families on one resource appears twice, once
    /// per mode. Read by the table hints and the `sys.dm_tran_*` views.
    #[must_use]
    pub fn held(&self, txn: TxnId) -> Vec<(LockResource, LockMode)> {
        let state = self.state();
        let mut resources: Vec<&LockResource> = state.entries.keys().collect();
        resources.sort_unstable();
        resources
            .into_iter()
            .flat_map(|res| {
                state.entries[res]
                    .holders
                    .iter()
                    .filter(|h| h.txn == txn)
                    .map(move |h| (*res, h.mode))
            })
            .collect()
    }

    /// Who is waiting, for what, in what mode: resource by resource in a stable order,
    /// each queue in the order it will be granted.
    ///
    /// A conversion is reported under the mode it converts **to**. Read by `deadlock.rs`
    /// to build its wait-for graph and by the `sys.dm_tran_*` views.
    #[must_use]
    pub fn waiters(&self) -> Vec<(TxnId, LockResource, LockMode)> {
        self.state().waiters()
    }

    /// Holders and waiters together, read under **one** guard of the table: resource by
    /// resource in the order of [`LockManager::held`], the holders of a resource before
    /// its queue, the queue in grant order. The body of
    /// [`crate::TransactionManager::active_locks`].
    ///
    /// A holder is [`LockStatus::Grant`]; a queued request is [`LockStatus::Convert`]
    /// when the transaction already holds a weaker mode of the same family here, and
    /// [`LockStatus::Wait`] otherwise. `waiting_ms` is counted from the moment the request
    /// joined the queue, and is `0` for a holder.
    pub(crate) fn lines(&self) -> Vec<LockInfo> {
        let state = self.state();
        let now = Instant::now();
        let mut resources: Vec<&LockResource> = state.entries.keys().collect();
        resources.sort_unstable();
        resources
            .into_iter()
            .flat_map(|res| {
                let entry = &state.entries[res];
                let holders = entry.holders.iter().map(move |h| LockInfo {
                    txn: h.txn,
                    resource: *res,
                    mode: h.mode,
                    status: LockStatus::Grant,
                    waiting_ms: 0,
                });
                let waiters = entry.queue.iter().map(move |w| LockInfo {
                    txn: w.txn,
                    resource: *res,
                    mode: w.mode,
                    status: if w.conversion {
                        LockStatus::Convert
                    } else {
                        LockStatus::Wait
                    },
                    waiting_ms: u64::try_from(now.saturating_duration_since(w.since).as_millis())
                        .unwrap_or(u64::MAX),
                });
                holders.chain(waiters)
            })
            .collect()
    }
}

/// The error of an `unlock` on a resource the transaction holds nothing on.
fn not_held(txn: TxnId, resource: LockResource) -> SqlError {
    InternalError::Bug(format!(
        "LockManager::unlock: transaction {txn} holds no lock on {resource:?}"
    ))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue of a resource is forgotten once nothing is held and nobody waits, so a
    /// server that took and gave back many row locks does not keep one entry per row.
    #[test]
    fn an_emptied_resource_leaves_no_entry() {
        let mgr = LockManager::new();
        let res = LockResource::Row(TableId(1), RowId(7));
        let wait = LockWait::none();
        assert_eq!(
            mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait),
            LockOutcome::Granted
        );
        assert_eq!(mgr.state().entries.len(), 1);
        mgr.release_all(TxnId(1));
        assert!(mgr.state().entries.is_empty(), "entry dropped when empty");
    }

    /// A refused `NoWait` leaves nothing behind either: the candidate took itself out of
    /// the queue before returning.
    #[test]
    fn a_refused_nowait_leaves_the_queue_empty() {
        let mgr = LockManager::new();
        let res = LockResource::Table(TableId(3));
        let wait = LockWait::none();
        mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);
        assert_eq!(
            mgr.try_acquire(TxnId(2), res, LockMode::S, LockTimeout::NoWait, &wait),
            LockOutcome::TimedOut
        );
        assert!(mgr.waiters().is_empty());
        assert_eq!(mgr.held(TxnId(1)), vec![(res, LockMode::X)]);
    }

    /// The ticket counter saturates instead of wrapping round to a ticket in use, the
    /// bound `TransactionManager::begin` already takes on its identifiers.
    #[test]
    fn the_ticket_counter_saturates() {
        let mgr = LockManager::new();
        mgr.state().next_ticket = u64::MAX;
        let res = LockResource::Table(TableId(1));
        let wait = LockWait::none();
        mgr.try_acquire(TxnId(1), res, LockMode::X, LockTimeout::NoWait, &wait);
        mgr.try_acquire(TxnId(2), res, LockMode::X, LockTimeout::NoWait, &wait);
        assert_eq!(mgr.state().next_ticket, u64::MAX);
    }
}
