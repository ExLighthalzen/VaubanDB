//! The wait-for graph, the choice of a victim and the two errors a refused wait reports —
//! 1222 for a wait that ran out of time, 1205 for a cycle. [`crate::lock::LockManager`]
//! calls the hook of this file before a thread parks.
//!
//! # The graph
//!
//! Two kinds of edge, both leaving a waiter, built from the two halves the lock manager
//! hands over: its queues ([`crate::lock::LockManager::waiters`]) and the holders of the
//! same resources.
//!
//! - **waiter → holder that refuses it**, read through
//!   [`crate::lock::LockMode::compatible_with`];
//! - **waiter → candidate placed ahead of it in the queue of its resource**, which
//!   `waiters` lists in the order it will be granted. A grant pass stops at the first
//!   candidate a holder refuses, so a waiter the holders would let through can still be
//!   held back by the candidate ahead of it, and that wait is its own edge rather than a
//!   copy of the holder edges of the candidate ahead: a mode compatible with the holders
//!   carries no holder edge at all (`tests/deadlock.rs`,
//!   `a_wait_behind_a_blocked_candidate_closes_the_cycle`, where `T3` asks `S` on a row
//!   held in `S`).
//!
//! A cycle over these two edges is a wait the table does not undo by itself while one
//! thread is parked per waiting transaction: each step asks the next transaction to be
//! granted or to give a lock back, and a parked transaction does neither until its own
//! wait ends. The shape is that of `a_wait_behind_a_blocked_candidate_closes_the_cycle`; a
//! long wait that closes no cycle is `waiting_without_cycle_is_not_a_deadlock`.
//!
//! # When the graph is read
//!
//! At the moment a thread is about to park, and again at each wake of the condvar before it
//! parks again — the two points [`crate::lock::LockManager::try_acquire`] calls
//! [`DeadlockMonitor::detect`] from. There is no background thread: the wait that closes
//! the cycle finds it synchronously, without a delay of its own, and the other branches of
//! the cycle find the same graph within one `POLL_SLICE` (50 ms) of their next wake. SQL
//! Server runs a periodic detector instead and reports its 1205 up to a few seconds after
//! the statement that closed the cycle: a difference of moment, not of outcome.
//!
//! # Who answers the 1205
//!
//! [`DeadlockMonitor::detect`] answers `Some` to the thread of the **victim** and `None` to
//! the other threads of the cycle, which keep waiting: the outcome of a wait is reported to
//! the transaction that waited, so a thread reports 1205 for itself and not for another
//! (`tests/deadlock.rs`, `two_way_cycle_picks_one_victim`). The victim being a function of
//! the graph, the other branches agree on it without being told.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use vauban_errors::{InternalError, SqlError};
use vauban_storage::TxnId;

use crate::TransactionManager;
use crate::handle::TxnHandle;
use crate::lock::{LockManager, LockMode, LockOutcome, LockResource};

/// Lowest `DEADLOCK_PRIORITY` a transaction may take, the `LOW` end of the scale of
/// `SET DEADLOCK_PRIORITY`.
const PRIORITY_MIN: i16 = -10;

/// Highest `DEADLOCK_PRIORITY` a transaction may take, the `HIGH` end of that scale.
const PRIORITY_MAX: i16 = 10;

/// `DEADLOCK_PRIORITY` of a transaction that did not ask for one — the `NORMAL` of the same
/// scale.
const PRIORITY_NORMAL: i16 = 0;

/// The kind of resource the deadlock was fought over, the `%.*ls` of message 1205.
///
/// SQL Server writes the word `lock` there for row locks and table locks alike. The other
/// kinds it names (`communication buffer`, `thread`) come from resources this engine does
/// not model.
const DEADLOCK_RESOURCE_KIND: &str = "lock";

/// Looks for a cycle in the waits the lock manager is holding, and holds the
/// `DEADLOCK_PRIORITY` of the transactions that asked for one.
///
/// One per [`crate::lock::LockManager`].
#[derive(Debug, Default)]
pub(crate) struct DeadlockMonitor {
    /// `DEADLOCK_PRIORITY` per transaction, clamped to `PRIORITY_MIN..=PRIORITY_MAX`. A
    /// transaction absent from the map is at [`PRIORITY_NORMAL`].
    priorities: Mutex<HashMap<TxnId, i16>>,
}

impl DeadlockMonitor {
    /// A monitor watching nothing yet, with an empty priority map.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The priority map, recovering from a poisoned lock instead of panicking, as
    /// `LockManager::state` does: this map is read on the execution path of a query, which
    /// must not panic. The block between this call and the drop of the guard holds no
    /// `unwrap`, no `expect` and no indexing.
    fn state(&self) -> MutexGuard<'_, HashMap<TxnId, i16>> {
        self.priorities
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps the `DEADLOCK_PRIORITY` of a transaction, clamped to the documented range.
    pub(crate) fn set_priority(&self, txn: TxnId, priority: i16) {
        self.state()
            .insert(txn, priority.clamp(PRIORITY_MIN, PRIORITY_MAX));
    }

    /// The `DEADLOCK_PRIORITY` of a transaction: [`PRIORITY_NORMAL`] until
    /// [`DeadlockMonitor::set_priority`] sets one for it.
    fn priority(&self, txn: TxnId) -> i16 {
        self.state().get(&txn).copied().unwrap_or(PRIORITY_NORMAL)
    }

    /// The transaction to roll back when `txn` joining the waits would close a cycle, and
    /// `None` when that transaction is not the one to roll back.
    ///
    /// Called by [`crate::lock::LockManager::try_acquire`] before the thread parks and at
    /// each wake, with the queues and the holders as they stand. The answer is `Some(txn)`
    /// — the caller itself — or `None`: a thread reports the 1205 of its own transaction
    /// (module documentation).
    pub(crate) fn detect(
        &self,
        waiters: &[(TxnId, LockResource, LockMode)],
        holders: &[(TxnId, LockResource, LockMode)],
        txn: TxnId,
    ) -> Option<TxnId> {
        let cycle = wait_for_graph(waiters, holders).cycle_from(txn)?;
        let victim = self.victim(&cycle, holders)?;
        (victim == txn).then_some(victim)
    }

    /// The transaction of `cycle` that gives way, by the three ranks of this engine.
    ///
    /// 1. the lowest `DEADLOCK_PRIORITY`;
    /// 2. at equal priority, the **cheapest rollback**, approached by the number of write
    ///    locks held (`X`, `U`, `IX`);
    /// 3. at equal cost, the largest [`TxnId`] — the youngest transaction.
    ///
    /// Ranks 1 and 2 follow SQL Server: the `LOW` side gives way whichever side crossed
    /// first and whichever holds more write locks, and at equal priority the side that
    /// wrote less gives way (`the_lower_priority_beats_the_cost`,
    /// `the_cheaper_rollback_beats_the_age`).
    ///
    /// Rank 3 is an **internal** choice: SQL Server does not tie a symmetric cycle to the
    /// session identifier or to the arrival order, so this engine takes the youngest
    /// transaction so that the answer is reproducible (`tests/deadlock.rs`,
    /// `two_way_cycle_picks_one_victim` asserts one victim, not which one).
    fn victim(
        &self,
        cycle: &[TxnId],
        holders: &[(TxnId, LockResource, LockMode)],
    ) -> Option<TxnId> {
        cycle.iter().copied().min_by_key(|&txn| {
            (
                self.priority(txn),
                writes_held(txn, holders),
                std::cmp::Reverse(txn),
            )
        })
    }
}

impl TransactionManager {
    /// Sets the `DEADLOCK_PRIORITY` of an open transaction, read when a cycle has to pick a
    /// victim.
    ///
    /// `priority` is clamped to `-10..=10`, the range of `SET DEADLOCK_PRIORITY`. The
    /// keywords of that statement are translated by the session: `-5` for `LOW`, `0` for
    /// `NORMAL` and `5` for `HIGH`. A transaction that has not called this sits at `0`
    /// (`tests/deadlock.rs`, `priority_decides_the_victim`).
    pub fn set_deadlock_priority(&self, txn: &TxnHandle, priority: i16) {
        self.locks().monitor().set_priority(txn.id, priority);
    }
}

/// How many write locks a transaction holds: the `X`, `U` and `IX` of `holders`.
///
/// The stand-in for the cost of rolling that transaction back, rank 2 of
/// [`DeadlockMonitor::victim`]. A lock table counts locks where SQL Server counts log
/// records; the two agree when one side wrote one row and the other a hundred.
fn writes_held(txn: TxnId, holders: &[(TxnId, LockResource, LockMode)]) -> usize {
    holders
        .iter()
        .filter(|(t, _, mode)| {
            *t == txn && matches!(mode, LockMode::X | LockMode::U | LockMode::IX)
        })
        .count()
}

/// The wait-for graph: for each transaction, the transactions it waits for.
#[derive(Debug, Default)]
struct Graph {
    /// Successors of each waiting transaction, sorted and deduplicated so that the walk of
    /// [`Graph::cycle_from`] does not depend on the order the lock table listed its entries
    /// in.
    edges: HashMap<TxnId, Vec<TxnId>>,
}

/// Builds the graph from the queues and the holders of one lock table.
///
/// Two edges leave a waiter (module documentation):
///
/// - to a holder of the same resource, when the holder is another transaction and its mode
///   refuses the mode asked for ([`crate::lock::LockMode::compatible_with`]);
/// - to each candidate of another transaction placed ahead of it in the queue of that
///   resource. `waiters` lists a queue in the order it will be granted, so the candidates
///   ahead of a waiter are the ones it has to see granted before its own mode is looked at.
fn wait_for_graph(
    waiters: &[(TxnId, LockResource, LockMode)],
    holders: &[(TxnId, LockResource, LockMode)],
) -> Graph {
    let mut edges: HashMap<TxnId, Vec<TxnId>> = HashMap::new();
    for (rank, (waiter, resource, mode)) in waiters.iter().enumerate() {
        for (holder, held_on, held) in holders {
            if held_on == resource && holder != waiter && !mode.compatible_with(*held) {
                edges.entry(*waiter).or_default().push(*holder);
            }
        }
        for (ahead, queued_on, _) in &waiters[..rank] {
            if queued_on == resource && ahead != waiter {
                edges.entry(*waiter).or_default().push(*ahead);
            }
        }
    }
    for successors in edges.values_mut() {
        successors.sort_unstable();
        successors.dedup();
    }
    Graph { edges }
}

impl Graph {
    /// The transactions of the first cycle a depth-first walk from `start` meets, in the
    /// order the walk entered them, or `None` when the walk ends without closing one.
    ///
    /// The walk carries its own stack rather than the call stack: the number of waiting
    /// transactions bounds its depth, and a recursion that deep sits on the execution path
    /// of a query.
    fn cycle_from(&self, start: TxnId) -> Option<Vec<TxnId>> {
        let mut path: Vec<TxnId> = vec![start];
        let mut next: Vec<usize> = vec![0];
        let mut on_path: HashSet<TxnId> = HashSet::from([start]);
        let mut closed: HashSet<TxnId> = HashSet::new();
        while let (Some(&node), Some(&index)) = (path.last(), next.last()) {
            let successors = self.edges.get(&node).map_or(&[][..], Vec::as_slice);
            match successors.get(index) {
                Some(&successor) => {
                    if let Some(step) = next.last_mut() {
                        *step += 1;
                    }
                    if on_path.contains(&successor) {
                        let from = path.iter().position(|&t| t == successor)?;
                        return Some(path[from..].to_vec());
                    }
                    if !closed.contains(&successor) {
                        path.push(successor);
                        next.push(0);
                        on_path.insert(successor);
                    }
                }
                None => {
                    on_path.remove(&node);
                    closed.insert(node);
                    path.pop();
                    next.pop();
                }
            }
        }
        None
    }
}

/// The error a [`LockOutcome`] other than [`LockOutcome::Granted`] is reported as, and the
/// release of the locks of a victim.
///
/// - [`LockOutcome::TimedOut`] → 1222, in the state of the granularity waited for: 51 on a
///   row, 56 on the object. SQL Server sends state 51 to a statement held back on a row and
///   state 56 to one held back on the object, whether that statement is an `UPDATE`, a
///   `SELECT … WITH (TABLOCKX)` or an `ALTER TABLE … ADD`, which is why the granularity of
///   the resource picks the constructor rather than the kind of statement
///   (`tests/deadlock.rs`, `timeout_state_follows_the_granularity`).
/// - [`LockOutcome::Deadlock`] → 1205 for the victim, whose locks are given back **before**
///   the error goes up, so that the other branch of the cycle is granted without a second
///   pass of detection (`tests/deadlock.rs`, `victim_releases_before_returning`). The `%d`
///   of the message carries the transaction identifier: the session identifier the client
///   sees lives in `session`, and nothing hands it to this crate yet.
/// - [`LockOutcome::Granted`] is an `InternalError::Bug`: the caller asked for the error of
///   a wait that succeeded.
/// - [`LockOutcome::Cancelled`] is an `InternalError::Bug` too: an ATTENTION cancels the
///   request instead of reporting an error, and what the client is told there belongs to
///   the session.
pub(crate) fn to_error(
    locks: &LockManager,
    outcome: LockOutcome,
    resource: LockResource,
) -> SqlError {
    match outcome {
        LockOutcome::Granted => {
            InternalError::Bug("deadlock::to_error: the lock was granted".to_owned()).into()
        }
        LockOutcome::TimedOut => match resource {
            LockResource::Row(..) => SqlError::lock_request_timeout_on_row(),
            LockResource::Table(..) => SqlError::lock_request_timeout_on_object(),
        },
        LockOutcome::Deadlock { victim } => {
            locks.release_all(victim);
            SqlError::deadlock_victim(process_id(victim), DEADLOCK_RESOURCE_KIND)
        }
        LockOutcome::Cancelled => InternalError::Bug(
            "deadlock::to_error: a cancelled wait has no client error".to_owned(),
        )
        .into(),
    }
}

/// The `%d` of message 1205: the transaction identifier, saturated into the signed integer
/// the error constructor takes.
fn process_id(victim: TxnId) -> i64 {
    i64::try_from(victim.0).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_storage::{RowId, TableId};

    /// Row `n` of one table, as a resource.
    fn row(n: u64) -> LockResource {
        LockResource::Row(TableId(1), RowId(n))
    }

    /// A waiter or a holder of an `X` on a row.
    fn x(txn: u64, n: u64) -> (TxnId, LockResource, LockMode) {
        (TxnId(txn), row(n), LockMode::X)
    }

    /// A wait on a resource held by a transaction that is itself running carries an edge,
    /// but the walk from the waiter closes no cycle.
    #[test]
    fn a_wait_on_a_running_holder_closes_no_cycle() {
        let graph = wait_for_graph(&[x(1, 2), x(2, 1)], &[x(3, 1), x(3, 2)]);
        assert_eq!(graph.edges[&TxnId(1)], vec![TxnId(3)]);
        assert_eq!(graph.cycle_from(TxnId(1)), None);
        assert_eq!(graph.cycle_from(TxnId(2)), None);
    }

    /// The same two waits, over holders that are the waiters themselves: the walk closes.
    #[test]
    fn two_crossed_waits_close_a_cycle() {
        let graph = wait_for_graph(&[x(1, 2), x(2, 1)], &[x(1, 1), x(2, 2)]);
        assert_eq!(graph.cycle_from(TxnId(1)), Some(vec![TxnId(1), TxnId(2)]));
        assert_eq!(graph.cycle_from(TxnId(2)), Some(vec![TxnId(2), TxnId(1)]));
    }

    /// A shared lock waiting behind another shared lock is no edge: the two modes are
    /// compatible, and the wait is the queue's doing, not the holder's.
    #[test]
    fn a_compatible_holder_carries_no_edge() {
        let waiters = [(TxnId(1), row(1), LockMode::S)];
        let holders = [(TxnId(2), row(1), LockMode::S)];
        assert!(wait_for_graph(&waiters, &holders).edges.is_empty());
    }

    /// A waiter the holders would let through, standing behind a candidate they refuse,
    /// carries the edge of the fair queue: `T3` asks `S` on a row held in `S` by `T1`, and
    /// waits because `T2` asked `X` on it first. That edge closes `T1 → T3 → T2 → T1`,
    /// which no holder edge closes on its own.
    #[test]
    fn a_candidate_ahead_in_the_queue_carries_an_edge() {
        let waiters = [
            (TxnId(2), row(1), LockMode::X),
            (TxnId(3), row(1), LockMode::S),
            x(1, 2),
        ];
        let holders = [(TxnId(1), row(1), LockMode::S), x(3, 2)];
        let graph = wait_for_graph(&waiters, &holders);
        assert_eq!(graph.edges[&TxnId(3)], vec![TxnId(2)], "the queue edge");
        assert_eq!(graph.edges[&TxnId(2)], vec![TxnId(1)], "the holder edge");
        assert_eq!(
            graph.cycle_from(TxnId(1)),
            Some(vec![TxnId(1), TxnId(3), TxnId(2)])
        );
        assert_eq!(
            DeadlockMonitor::new().victim(&[TxnId(1), TxnId(3), TxnId(2)], &holders),
            Some(TxnId(2)),
            "T3 holds the one write lock of the cycle, and T2 is younger than T1"
        );
    }

    /// The queue edge stops at the transaction itself: a conversion of the same transaction
    /// standing ahead in the queue would otherwise make the walk close on a node of one.
    #[test]
    fn a_candidate_of_the_same_transaction_carries_no_edge() {
        let waiters = [
            (TxnId(1), row(1), LockMode::X),
            (TxnId(1), row(1), LockMode::S),
        ];
        let graph = wait_for_graph(&waiters, &[]);
        assert!(graph.edges.is_empty(), "edges were {:?}", graph.edges);
        assert_eq!(graph.cycle_from(TxnId(1)), None);
    }

    /// Two candidates of one queue, refused by a holder that waits for neither of them: the
    /// queue edge is there and the walk still closes nothing.
    #[test]
    fn a_queue_behind_a_running_holder_closes_no_cycle() {
        let waiters = [x(1, 1), x(2, 1)];
        let holders = [x(3, 1)];
        let graph = wait_for_graph(&waiters, &holders);
        assert_eq!(graph.edges[&TxnId(2)], vec![TxnId(1), TxnId(3)]);
        assert_eq!(graph.cycle_from(TxnId(2)), None);
    }

    /// A cycle of three, entered from each of its three transactions.
    #[test]
    fn a_three_way_cycle_is_walked_from_each_of_its_nodes() {
        let waiters = [x(1, 2), x(2, 3), x(3, 1)];
        let holders = [x(1, 1), x(2, 2), x(3, 3)];
        let graph = wait_for_graph(&waiters, &holders);
        for start in [TxnId(1), TxnId(2), TxnId(3)] {
            let cycle = graph.cycle_from(start).expect("the three waits close");
            assert_eq!(cycle.len(), 3, "cycle from {start}: {cycle:?}");
            assert_eq!(cycle[0], start);
        }
    }

    /// Rank 3 with nothing above it: at equal priority and equal write count, the largest
    /// identifier gives way.
    #[test]
    fn the_youngest_transaction_breaks_a_tie() {
        let monitor = DeadlockMonitor::new();
        let holders = [x(1, 1), x(2, 2)];
        let victim = monitor.victim(&[TxnId(1), TxnId(2)], &holders);
        assert_eq!(victim, Some(TxnId(2)));
    }

    /// Rank 2 beats rank 3: the transaction holding fewer write locks gives way even when
    /// it is the older one.
    #[test]
    fn the_cheaper_rollback_beats_the_age() {
        let monitor = DeadlockMonitor::new();
        let holders = [x(1, 1), x(2, 2), x(2, 3), x(2, 4)];
        assert_eq!(
            monitor.victim(&[TxnId(1), TxnId(2)], &holders),
            Some(TxnId(1))
        );
    }

    /// Rank 1 beats rank 2: the `LOW` side gives way though it holds more write locks.
    #[test]
    fn the_lower_priority_beats_the_cost() {
        let monitor = DeadlockMonitor::new();
        monitor.set_priority(TxnId(2), -5);
        let holders = [x(1, 1), x(2, 2), x(2, 3), x(2, 4)];
        assert_eq!(
            monitor.victim(&[TxnId(1), TxnId(2)], &holders),
            Some(TxnId(2))
        );
    }

    /// A priority outside the documented range is clamped, so a caller cannot invent a
    /// transaction a cycle would refuse to pick.
    #[test]
    fn a_priority_is_clamped_to_the_documented_range() {
        let monitor = DeadlockMonitor::new();
        monitor.set_priority(TxnId(1), i16::MIN);
        monitor.set_priority(TxnId(2), i16::MAX);
        assert_eq!(monitor.priority(TxnId(1)), PRIORITY_MIN);
        assert_eq!(monitor.priority(TxnId(2)), PRIORITY_MAX);
        assert_eq!(monitor.priority(TxnId(3)), PRIORITY_NORMAL);
    }

    /// The identifier that fills the `%d` of 1205 saturates instead of wrapping negative.
    #[test]
    fn the_process_id_saturates() {
        assert_eq!(process_id(TxnId(7)), 7);
        assert_eq!(process_id(TxnId(u64::MAX)), i64::MAX);
    }
}
