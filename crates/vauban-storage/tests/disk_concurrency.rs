//! Several threads writing into one `DiskStorage` at once.
//!
//! Eight threads insert keys that overlap, each in transactions of its own, into a table that
//! carries a `unique` index: each key goes in once, the other inserts of that key are refused
//! with 2601 and with nothing else, and the table and its index agree on the rows, as does a
//! reopen of the clustered one.
//! The structure of the trees themselves is checked from inside the crate
//! (`disk::btree::concurrency::eight_writers_leave_valid_trees`).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use vauban_storage::{
    Direction, DiskOptions, DiskStorage, IndexId, IndexShape, KeyColumn, KeyRange, Row, Snapshot,
    Storage, TableId, TableShape, TxnId,
};
use vauban_types::{SqlType, TypeInfo, Value};

/// Number of writer threads.
const THREADS: u64 = 8;
/// Inserts each writer asks for.
const PER_THREAD: i32 = 10_000;
/// Distinct keys the writers draw from: each key is asked for sixteen times.
const KEYS: i32 = 5_000;
/// Inserts per transaction of a writer.
const PER_TXN: i32 = 500;

/// Distinguishes two instances asked for in the same nanosecond by the same process.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A unique path under `temp_dir()`, not yet created.
fn fresh_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "vauban-concurrency-{label}-{pid}-{nanos}-{sequence}",
        pid = std::process::id()
    ))
}

/// A snapshot that sees every committed row.
fn settled() -> Snapshot {
    Snapshot {
        xmin: TxnId(u64::MAX),
        xmax: TxnId(u64::MAX),
        active: Vec::new(),
        own: TxnId(0),
    }
}

/// Column `column`, ascending.
fn ascending(column: u16) -> KeyColumn {
    KeyColumn {
        column,
        descending: false,
    }
}

/// A table of two nullable `int`, clustered on the first one or not.
fn two_ints(clustered: bool) -> TableShape {
    TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true); 2],
        clustered_key: clustered.then(|| vec![ascending(0)]),
    }
}

/// The key the insert `step` of thread `thread` asks for. The threads start at different keys
/// and walk them with a stride prime to [`KEYS`], so each key is asked for by several threads,
/// in an order that differs from one thread to the next.
fn key_of(thread: u64, step: i32) -> i32 {
    let offset = i32::try_from(thread).expect("a thread number") * 613;
    (offset + step * 7) % KEYS
}

/// Runs the eight writers on `table`, with a reader that seeks `index` while they write, and
/// answers the number of inserts that went through.
fn race(storage: &DiskStorage, table: TableId, index: IndexId) -> usize {
    let writing = AtomicBool::new(true);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut seeks = 0u32;
            while writing.load(Ordering::SeqCst) {
                let rows: Vec<i32> = storage
                    .seek(&settled(), index, &KeyRange::Full, Direction::Forward)
                    .expect("a seek beside the writers")
                    .map(|row| match row.expect("a row").1.0[0] {
                        Value::I32(key) => key,
                        ref other => panic!("a key of another type: {other:?}"),
                    })
                    .collect();
                if let Some(pair) = rows.windows(2).find(|pair| pair[0] >= pair[1]) {
                    panic!(
                        "a seek beside the writers read {} after {}",
                        pair[1], pair[0]
                    );
                }
                seeks += 1;
            }
            seeks
        });
        let writers: Vec<_> = (0..THREADS)
            .map(|thread| {
                scope.spawn(move || {
                    let mut inserted = 0;
                    for batch in 0..PER_THREAD / PER_TXN {
                        let txn = TxnId(1 + thread * 1_000 + u64::try_from(batch).expect("n"));
                        for step in 0..PER_TXN {
                            let key = key_of(thread, batch * PER_TXN + step);
                            let row = Row(vec![Value::I32(key), Value::I32(-key)]);
                            match storage.insert(txn, table, &row) {
                                Ok(_) => inserted += 1,
                                Err(err) => assert_eq!(
                                    (err.number, err.severity, err.state),
                                    (2601, 14, 1),
                                    "{}",
                                    err.message
                                ),
                            }
                        }
                        storage.commit(txn).expect("commit");
                    }
                    inserted
                })
            })
            .collect();
        // Every writer is joined before the reader is stopped, and a writer that panicked is
        // reported after that stop, so that the reader does not keep the scope open.
        let joined: Vec<_> = writers.into_iter().map(|writer| writer.join()).collect();
        writing.store(false, Ordering::SeqCst);
        let inserted = joined
            .into_iter()
            .map(|writer| writer.expect("a writer"))
            .sum();
        assert!(reader.join().expect("the reader") > 0, "the reader ran");
        inserted
    })
}

/// The keys `table` holds, read by a scan and by a seek of `index`, which must agree.
fn keys_of(storage: &DiskStorage, table: TableId, index: IndexId) -> Vec<i32> {
    let key = |row: Row| match row.0[0] {
        Value::I32(key) => key,
        ref other => panic!("a key of another type: {other:?}"),
    };
    let mut scanned: Vec<i32> = storage
        .scan(&settled(), table)
        .expect("scan")
        .map(|row| key(row.expect("a row").1))
        .collect();
    scanned.sort_unstable();
    let sought: Vec<i32> = storage
        .seek(&settled(), index, &KeyRange::Full, Direction::Forward)
        .expect("seek")
        .map(|row| key(row.expect("a row").1))
        .collect();
    assert_eq!(scanned, sought, "the scan and the index agree");
    sought
}

/// Plays the race on a table clustered or not, then, for `reopen`, takes a checkpoint and
/// reads the rows back from a reopened instance.
fn one_insert_per_key(label: &str, clustered: bool, reopen: bool) {
    let dir = fresh_dir(label);
    let (table, index) = {
        let storage = DiskStorage::open(&dir, DiskOptions::default()).expect("open");
        let db = storage.create_database("d").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(clustered))
            .expect("create the table");
        let unique = IndexShape {
            columns: vec![ascending(0)],
            unique: true,
            included: Vec::new(),
        };
        let index = storage.create_index(table, &unique).expect("the index");
        let inserted = race(&storage, table, index);
        assert_eq!(inserted, usize::try_from(KEYS).expect("a count"));
        let expected: Vec<i32> = (0..KEYS).collect();
        assert_eq!(keys_of(&storage, table, index), expected);
        for key in [0, KEYS / 2, KEYS - 1] {
            let point = KeyRange::Point(vec![Value::I32(key)]);
            let rows = storage
                .seek(&settled(), index, &point, Direction::Forward)
                .expect("seek one key")
                .count();
            assert_eq!(rows, 1, "key {key}");
        }
        if reopen {
            storage.checkpoint().expect("checkpoint");
        }
        (table, index)
    };
    if reopen {
        let storage = DiskStorage::open(&dir, DiskOptions::default()).expect("reopen");
        let keys: BTreeSet<i32> = keys_of(&storage, table, index).into_iter().collect();
        assert_eq!(keys, (0..KEYS).collect(), "the rows after a reopen");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A heap table whose `unique` index has a tree of its own: the check and the maintenance of
/// that tree are one section, so a second insert of a key finds the first one.
#[test]
fn concurrent_inserts_of_one_key_let_one_through() {
    one_insert_per_key("heap", false, false);
}

/// A clustered table whose `unique` index shares the tree of the rows, whose root the splits
/// move while the threads write; the roots the catalogue keeps lead to the rows after a
/// checkpoint and a reopen.
#[test]
fn concurrent_inserts_of_one_clustered_key_let_one_through() {
    one_insert_per_key("clustered", true, true);
}
