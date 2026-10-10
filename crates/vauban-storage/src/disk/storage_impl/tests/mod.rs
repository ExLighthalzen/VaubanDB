//! Tests of the end of a transaction on disk: the group commit of the journal, and the rule
//! that a commit returns once its record is durable.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use vauban_types::{SqlType, TypeInfo, Value};

use super::super::buffer::DurableLsn;
use super::super::temp::TempDir;
use super::super::wal::{SyncGate, WalRecordKind};
use super::super::{DiskOptions, DiskStorage};
use crate::{Row, Snapshot, Storage, TableId, TableShape, TxnId};

/// How many transactions end at the same time in these tests.
const WRITERS: u64 = 16;

/// The identifier of the first of them.
const FIRST: u64 = 10;

/// A table of one nullable `int`.
fn one_int() -> TableShape {
    TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: None,
    }
}

/// A snapshot that sees what committed below `TxnId(1000)`.
fn settled() -> Snapshot {
    Snapshot {
        xmin: TxnId(1000),
        xmax: TxnId(1000),
        active: Vec::new(),
        own: TxnId(0),
    }
}

/// The values of the rows of `table` a settled snapshot sees, sorted.
fn values(storage: &DiskStorage, table: TableId) -> Vec<i32> {
    let mut seen: Vec<i32> = storage
        .scan(&settled(), table)
        .expect("scan")
        .map(|item| match item.expect("a row").1.0.as_slice() {
            [Value::I32(value)] => *value,
            other => panic!("unexpected row {other:?}"),
        })
        .collect();
    seen.sort_unstable();
    seen
}

/// Polls `done` until it holds or ten seconds have passed, and answers whether it held.
fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > Duration::from_secs(10) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    true
}

/// The number of `Commit` records the journal of `storage` holds.
fn commits_in_journal(storage: &DiskStorage) -> usize {
    storage
        .wal
        .records()
        .expect("read the journal")
        .iter()
        .filter(|record| record.kind == WalRecordKind::Commit)
        .count()
}

/// Copies the files of `storage` to a fresh directory and cuts the journal of the copy after
/// its last durable record: what a crash at this point may leave on disk.
fn crash_image(dir: &TempDir, storage: &DiskStorage, label: &str) -> TempDir {
    let durable = storage.wal.durable_lsn();
    let kept: u64 = storage
        .wal
        .records()
        .expect("read the journal")
        .iter()
        .filter(|record| record.lsn <= durable)
        .map(|record| 34 + record.payload.len() as u64)
        .sum();
    let image = TempDir::created(label);
    for entry in std::fs::read_dir(dir.path()).expect("list the instance") {
        let entry = entry.expect("an entry");
        std::fs::copy(entry.path(), image.path().join(entry.file_name())).expect("copy a file");
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(image.child("wal"))
        .expect("open the copied journal")
        .set_len(kept)
        .expect("cut the copied journal");
    image
}

/// An instance holding one table, and a gate in front of its next group sync.
fn gated_instance(label: &str) -> (TempDir, DiskStorage, TableId, Arc<SyncGate>) {
    let dir = TempDir::created(label);
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &one_int())
        .expect("create the table");
    let gate = Arc::new(SyncGate::default());
    storage.wal.set_sync_gate(Some(Arc::clone(&gate)));
    (dir, storage, table, gate)
}

/// Runs `WRITERS` transactions that each insert one row and commit at the same time, holds
/// the first group sync back until each `Commit` record is in the journal, calls `held` at
/// that point, then lets the syncs go. Answers the LSN of each `Commit` record together with
/// the durable LSN its commit saw at its return, and whether a leader reached the gate.
fn commit_together(
    storage: &DiskStorage,
    table: TableId,
    gate: &SyncGate,
    held: impl FnOnce(usize),
) -> (Vec<(u64, u64)>, bool) {
    let returned = AtomicUsize::new(0);
    let seen = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WRITERS)
            .map(|i| {
                let returned = &returned;
                scope.spawn(move || {
                    let txn = TxnId(FIRST + i);
                    let value = i32::try_from(i).expect("small");
                    storage
                        .insert(txn, table, &Row(vec![Value::I32(value)]))
                        .expect("insert");
                    storage.commit(txn).expect("commit");
                    let durable = storage.wal.durable_lsn();
                    returned.fetch_add(1, Ordering::SeqCst);
                    (txn.0, durable.0)
                })
            })
            .collect();
        assert!(
            wait_until(|| commits_in_journal(storage) == WRITERS as usize),
            "the commit records did not reach the journal"
        );
        // A commit that did not wait for its sync would return here.
        std::thread::sleep(Duration::from_millis(50));
        held(returned.load(Ordering::SeqCst));
        let leader = wait_until(|| gate.reached() == 1);
        gate.open();
        let seen = workers
            .into_iter()
            .map(|worker| worker.join().expect("a writer"))
            .collect::<Vec<_>>();
        (seen, leader)
    });
    let (seen, leader) = seen;
    let records = storage.wal.records().expect("read the journal");
    let seen = seen
        .into_iter()
        .map(|(txn, durable)| {
            let lsn = records
                .iter()
                .find(|record| record.kind == WalRecordKind::Commit && record.txn == TxnId(txn))
                .expect("the commit record")
                .lsn;
            (lsn.0, durable)
        })
        .collect();
    (seen, leader)
}

#[test]
fn sixteen_concurrent_commits_share_two_syncs() {
    let (_dir, storage, table, gate) = gated_instance("group-commit-syncs");
    let before = storage.wal.syncs();
    let (_, leader) = commit_together(&storage, table, &gate, |_| {});
    let syncs = storage.wal.syncs() - before;
    assert!(
        (1..=2).contains(&syncs),
        "{WRITERS} commits made {syncs} syncs; the first sync covers what was appended \
         before it and the second the rest"
    );
    assert!(leader, "no leader reached the gate");
    let expected: Vec<i32> = (0..16).collect();
    assert_eq!(values(&storage, table), expected);
}

#[test]
fn no_commit_returns_before_its_record_is_synced() {
    let (dir, storage, table, gate) = gated_instance("group-commit-durable");
    let mut held_image = None;
    let (seen, leader) = commit_together(&storage, table, &gate, |returned| {
        assert_eq!(
            returned, 0,
            "a commit returned while its record was not synced"
        );
        held_image = Some(crash_image(&dir, &storage, "group-commit-held"));
    });
    for (lsn, durable) in &seen {
        assert!(
            lsn <= durable,
            "commit record {lsn} returned with {durable} durable"
        );
    }
    assert!(leader, "no leader reached the gate");

    // A crash while the sync was held back loses the sixteen transactions, none of which had
    // returned; a crash after the last return keeps them all.
    let held_image = held_image.expect("the image taken while the sync was held");
    let reopened = DiskStorage::open(held_image.path(), DiskOptions::default()).expect("open");
    assert_eq!(values(&reopened, table), Vec::<i32>::new());
    let after = crash_image(&dir, &storage, "group-commit-after");
    let reopened = DiskStorage::open(after.path(), DiskOptions::default()).expect("open");
    let expected: Vec<i32> = (0..16).collect();
    assert_eq!(values(&reopened, table), expected);
}

#[test]
fn a_commit_without_a_write_waits_for_no_sync() {
    let (_dir, storage, _table, gate) = gated_instance("group-commit-no-write");
    let before = storage.wal.syncs();
    storage.commit(TxnId(5)).expect("commit");
    assert_eq!(storage.wal.syncs(), before);
    assert_eq!(gate.reached(), 0);
    let err = storage.commit(TxnId(5)).expect_err("a second commit");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);
}

#[test]
fn a_failed_group_sync_fails_each_commit_it_was_to_cover() {
    let (_dir, storage, table, gate) = gated_instance("group-commit-failed");
    // One sync fails, the one the first leader makes once the sixteen records are appended;
    // a later sync would succeed.
    gate.fail_next(1);
    let durable = storage.wal.durable_lsn();
    let syncs = storage.wal.syncs();
    let outcomes: Vec<bool> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WRITERS)
            .map(|i| {
                let storage = &storage;
                scope.spawn(move || {
                    let txn = TxnId(FIRST + i);
                    let value = i32::try_from(i).expect("small");
                    storage
                        .insert(txn, table, &Row(vec![Value::I32(value)]))
                        .expect("insert");
                    storage.commit(txn).is_ok()
                })
            })
            .collect();
        assert!(
            wait_until(|| commits_in_journal(&storage) == WRITERS as usize),
            "the commit records did not reach the journal"
        );
        assert!(
            wait_until(|| gate.reached() == 1),
            "no leader reached the gate"
        );
        gate.open();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("a writer"))
            .collect()
    });
    assert_eq!(
        outcomes,
        vec![false; 16],
        "each commit reports the failed sync"
    );
    assert_eq!(storage.wal.durable_lsn(), durable);
    assert_eq!(storage.wal.syncs(), syncs);
    assert_eq!(values(&storage, table), Vec::<i32>::new());

    // The sixteen transactions stay in progress, so each of them rolls back.
    for i in 0..WRITERS {
        storage.rollback(TxnId(FIRST + i)).expect("rollback");
    }
    assert_eq!(values(&storage, table), Vec::<i32>::new());
}

#[test]
fn a_failed_commit_rolls_back_and_stays_rolled_back_after_a_reopen() {
    let (dir, storage, table, gate) = gated_instance("group-commit-failed-reopen");
    gate.open();
    storage
        .insert(TxnId(10), table, &Row(vec![Value::I32(1)]))
        .expect("insert");
    gate.fail_next(1);
    storage.commit(TxnId(10)).expect_err("the sync fails");
    assert_eq!(values(&storage, table), Vec::<i32>::new());

    storage
        .rollback(TxnId(10))
        .expect("rollback after a failed commit");
    storage
        .insert(TxnId(11), table, &Row(vec![Value::I32(2)]))
        .expect("insert");
    storage.commit(TxnId(11)).expect("commit");
    assert_eq!(values(&storage, table), vec![2]);
    // The sync of the second commit covers the `Commit` record of the first one too.
    let first_commit = storage
        .wal
        .records()
        .expect("read the journal")
        .iter()
        .find(|record| record.kind == WalRecordKind::Commit && record.txn == TxnId(10))
        .expect("the commit record of the failed commit")
        .lsn;
    assert!(first_commit <= storage.wal.durable_lsn());
    drop(storage);

    let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    assert_eq!(values(&reopened, table), vec![2]);
}
