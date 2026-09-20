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
