//! Durability under `kill -9`: the committed rows survive, the uncommitted ones do not.
//!
//! A child process opens an instance in a directory the parent chose, writes N rows with a
//! commit after each — the contract of [`Storage::commit`] is that it returns only once the
//! write-ahead journal is durable — then writes M rows on a transaction it leaves open and
//! sleeps. The parent `kill()`s the child (SIGKILL on Unix, `TerminateProcess` on Windows,
//! which `std::process::Child::kill` maps to on both), waits for the child to die, and
//! reopens the same directory: the scan sees the N committed rows and none of the M
//! uncommitted ones.
//!
//! The child is this same integration test executable, launched with `--exact` and the name
//! of a test function whose body only runs when the environment variable naming the instance
//! directory is present: without the variable the function returns at once, so the ordinary
//! `cargo test --test durability` run sees the parent scenario and skips the child body. The
//! `CARGO_BIN_EXE_…` env that would name a binary does not exist for an integration test,
//! and a `[[bin]]` in the crate would leak into the published crate, so this pattern —
//! the test executable spawning itself — is the one that works here.
//!
//! The child does not share the parent's file handle. It opens its own instance; the parent
//! opens another on the same directory **after** the child is dead, which the instance
//! guard of [`DiskStorage::open`] would otherwise refuse.
//!
//! A second child moves the clustered key of each committed row in a transaction that
//! commits, then again in one it leaves open: after the kill, each row reads back once, under
//! the key of the committed update (`kill9_committed_clustered_key_updates_survive_once`).
//!
//! The last two tests play forty seeded sequences of inserts, updates that move the key,
//! deletes, savepoints, commits, rollbacks and reopens, with or without a checkpoint before
//! each reopen, on a disk instance and on a [`vauban_storage::MemoryStorage`], and compare what
//! the two read after each transaction (`forty_seeds_of_writes_and_reopens_read_as_the_memory_store_reads_clustered`).
//!
//! [`Storage::commit`]: vauban_storage::Storage::commit

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::env;

use vauban_storage::{
    Direction, DiskOptions, DiskStorage, IndexId, IndexShape, KeyColumn, KeyRange, MemoryStorage,
    Row, RowId, Snapshot, Storage, TableId, TableShape, TxnId,
};
use vauban_types::{SqlType, TypeInfo, Value};

/// Environment variable naming the directory of the instance the child writes to.
const DIR_ENV: &str = "VAUBAN_DURABILITY_DIR";
/// Environment variable telling the test executable to run the child body instead of the
/// parent's. Only the value matters, `1` is what the parent writes.
const CHILD_ENV: &str = "VAUBAN_DURABILITY_CHILD";

/// Committed writes: one `insert` and one `commit` per row. The durability contract says the
/// journal is on disk by the time the `commit` returns.
const COMMITTED: usize = 100;
/// Uncommitted writes: the child opens a second transaction, inserts M rows on it, and lets
/// the parent kill it before the `commit`.
const UNCOMMITTED: usize = 50;
/// Environment variable naming what the child writes: `clustered` for the key updates of
/// [`child_update_keys_and_wait`], anything else for the inserts of [`child_write_and_wait`].
const MODE_ENV: &str = "VAUBAN_DURABILITY_MODE";
/// What a committed key update adds to the key of each row of the clustered scenario.
const KEY_SHIFT: i32 = 1_000;
/// Name of the child test function; the parent spawns it with `--exact` so the harness does
/// not run the other tests of this executable in the child.
const CHILD_TEST: &str = "kill9_child_main";

/// Distinguishes two children launched in the same nanosecond by the same parent.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A unique path under `temp_dir()`, not yet created: the child's `DiskStorage::open` makes
/// the directory.
fn fresh_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!(
        "vauban-durability-{label}-{pid}-{nanos}-{sequence}",
        pid = std::process::id()
    ))
}

/// A table of one nullable `int`, no clustered key.
fn int_shape() -> TableShape {
    TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: None,
    }
}

/// A table of one nullable `int`, clustered on it.
fn clustered_shape() -> TableShape {
    TableShape {
        clustered_key: Some(vec![KeyColumn {
            column: 0,
            descending: false,
        }]),
        ..int_shape()
    }
}

/// A one-column row.
fn row(value: i32) -> Row {
    Row(vec![Value::I32(value)])
}

/// The snapshot of a reader that settles every small transaction id and lists none active.
fn settled() -> Snapshot {
    Snapshot {
        xmin: TxnId(u64::MAX),
        xmax: TxnId(u64::MAX),
        active: Vec::new(),
        own: TxnId(0),
    }
}

#[test]
fn kill9_committed_survive() {
    let dir = fresh_dir("survive");
    let mut child = spawn_child(&dir, "heap").expect("spawn the child");
    // The child writes and commits `COMMITTED` rows, opens a second transaction, inserts
    // `UNCOMMITTED` rows on it, then signals readiness on stdout and blocks forever. The
    // parent waits for the signal, then kills it.
    wait_for_ready(&dir);
    // The committed rows and the `Abort`-less `Begin` of the second transaction are in the
    // journal on disk by the time the child reaches its sleep loop. `Child::kill` sends
    // SIGKILL on Unix and `TerminateProcess` on Windows.
    child.kill().expect("kill -9 the child");
    let _ = child.wait();

    let storage = DiskStorage::open(&dir, DiskOptions::default())
        .expect("reopen the instance after the kill");
    let db = storage
        .databases()
        .expect("databases")
        .into_iter()
        .next()
        .map(|(id, _)| id)
        .expect("the database is there");
    let table = storage
        .tables(db)
        .expect("tables")
        .into_iter()
        .map(|(id, _)| id)
        .next()
        .expect("the table is there");
    let rows: Vec<(RowId, Row)> = storage
        .scan(&settled(), table)
        .expect("scan")
        .collect::<Result<Vec<_>, _>>()
        .expect("scan completes");
    let counted_committed: usize = rows
        .iter()
        .filter(|(_, r)| matches!(r.0[0], Value::I32(v) if (1..=COMMITTED as i32).contains(&v)))
        .count();
    let counted_uncommitted: usize = rows.iter().filter(|(_, r)| matches!(r.0[0], Value::I32(v) if (COMMITTED as i32 + 1..=COMMITTED as i32 + UNCOMMITTED as i32).contains(&v))).count();
    assert_eq!(
        counted_committed, COMMITTED,
        "the {COMMITTED} committed rows must be back, {rows:?}"
    );
    assert_eq!(
        counted_uncommitted, 0,
        "none of the {UNCOMMITTED} uncommitted rows may be visible, {rows:?}"
    );
    // The row ids are not reused: a fresh transaction on the reopened instance starts past
    // the largest id the journal names, which includes the ones the killed transaction took.
    let next = storage
        .insert(TxnId(1_000), table, &row(-1))
        .expect("insert past the killed writes");
    assert!(
        next.0 > (COMMITTED + UNCOMMITTED) as u64,
        "a row id a killed transaction took must not be handed out again: got {next:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kill9_committed_clustered_key_updates_survive_once() {
    let dir = fresh_dir("clustered");
    let mut child = spawn_child(&dir, "clustered").expect("spawn the child");
    wait_for_ready(&dir);
    child.kill().expect("kill -9 the child");
    let _ = child.wait();

    let storage = DiskStorage::open(&dir, DiskOptions::default())
        .expect("reopen the instance after the kill");
    let db = storage.databases().expect("databases")[0].0;
    let table = storage.tables(db).expect("tables")[0].0;
    let values: Vec<Value> = storage
        .scan(&settled(), table)
        .expect("scan")
        .map(|row| row.map(|(_, row)| row.0[0].clone()))
        .collect::<Result<Vec<_>, _>>()
        .expect("scan completes");
    // Each row reads back once, under the key its committed update gave it, in key order.
    let expected: Vec<Value> = (1..=COMMITTED as i32)
        .map(|v| Value::I32(v + KEY_SHIFT))
        .collect();
    assert_eq!(values, expected);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The child body: opens the instance in the directory the parent put in the env, writes the
/// committed and the uncommitted rows, prints `READY`, and blocks on stdin until the parent
/// kills it.
#[test]
fn kill9_child_main() {
    let Ok(dir) = env::var(DIR_ENV) else {
        return;
    };
    if env::var(MODE_ENV).is_ok_and(|mode| mode == "clustered") {
        child_update_keys_and_wait(Path::new(&dir));
    } else {
        child_write_and_wait(Path::new(&dir));
    }
    std::process::exit(0);
}

/// Launches the same executable with the child env set and the child test as its filter.
fn spawn_child(dir: &Path, mode: &str) -> Result<Child, std::io::Error> {
    let exe = env::current_exe()?;
    let child = Command::new(exe)
        .arg("--exact")
        .arg(CHILD_TEST)
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, dir)
        .env(MODE_ENV, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(child)
}

/// Waits for the child to create `ready` in `dir`: the child has finished its writes and is
/// blocked on the park loop.
fn wait_for_ready(dir: &Path) {
    let marker = dir.join("ready");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if marker.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the child did not signal readiness within 30 s");
}

/// Body of the child: create the instance, write the two batches, signal, then park.
fn child_write_and_wait(dir: &Path) {
    let storage = DiskStorage::open(dir, DiskOptions::default()).expect("child open");
    let db = storage.create_database("a").expect("child create_database");
    let table = storage
        .create_table(db, &int_shape())
        .expect("child create_table");
    for i in 1..=COMMITTED {
        let v = i32::try_from(i).expect("a small row id");
        let txn = TxnId(u64::try_from(i).expect("a small txn id"));
        storage
            .insert(txn, table, &row(v))
            .expect("child insert committed");
        storage.commit(txn).expect("child commit each row");
    }
    // The uncommitted transaction: opens on the first insert, never commits.
    let open = TxnId((COMMITTED + 1) as u64);
    for i in COMMITTED + 1..=COMMITTED + UNCOMMITTED {
        let v = i32::try_from(i).expect("a small row id");
        storage
            .insert(open, table, &row(v))
            .expect("child insert uncommitted");
    }
    signal_and_park(dir);
}

/// Body of the clustered child: insert the rows, commit, move the key of each of them by
/// [`KEY_SHIFT`] in a second transaction that commits, then move them again in a third that
/// stays open; signal, then park.
fn child_update_keys_and_wait(dir: &Path) {
    let storage = DiskStorage::open(dir, DiskOptions::default()).expect("child open");
    let db = storage.create_database("a").expect("child create_database");
    let table = storage
        .create_table(db, &clustered_shape())
        .expect("child create_table");
    let ids: Vec<(RowId, i32)> = (1..=COMMITTED as i32)
        .map(|v| (storage.insert(TxnId(1), table, &row(v)).expect("insert"), v))
        .collect();
    storage.commit(TxnId(1)).expect("child commit the inserts");
    for &(id, v) in &ids {
        storage
            .update(TxnId(2), table, id, &row(v + KEY_SHIFT))
            .expect("child update the key");
    }
    storage
        .commit(TxnId(2))
        .expect("child commit the key updates");
    for &(id, v) in &ids {
        storage
            .update(TxnId(3), table, id, &row(-v))
            .expect("child update the key again");
    }
    signal_and_park(dir);
}

/// Writes the `ready` marker the parent waits for, then parks until the parent kills this
/// process.
fn signal_and_park(dir: &Path) -> ! {
    std::fs::write(dir.join("ready"), b"").expect("write the ready marker");
    // Park until the parent kills this process: the child must stay alive so the parent
    // sends SIGKILL while its writes are unflushed. A sleep loop keeps the process awake
    // without holding a pipe open.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

// ---- writes and reopens against the memory store ----------------------------------------

/// A table of two nullable `int`, clustered on the first when `clustered` is true.
fn two_ints(clustered: bool) -> TableShape {
    TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::Int, true),
        ],
        clustered_key: clustered.then(|| {
            vec![KeyColumn {
                column: 0,
                descending: false,
            }]
        }),
    }
}

/// An index on the column `column`, ascending, not `unique`, with no included column.
fn index_on(column: u16) -> IndexShape {
    IndexShape {
        columns: vec![KeyColumn {
            column,
            descending: false,
        }],
        unique: false,
        included: Vec::new(),
    }
}

/// A two-column row.
fn pair(a: i32, b: i32) -> Row {
    Row(vec![Value::I32(a), Value::I32(b)])
}

/// A snapshot that settles every transaction below `xmax`, reading as `own`.
fn settled_below(xmax: u64, own: u64) -> Snapshot {
    Snapshot {
        xmin: TxnId(xmax),
        xmax: TxnId(xmax),
        active: Vec::new(),
        own: TxnId(own),
    }
}

/// The rows `snap` sees in `table`, sorted.
fn scanned(storage: &dyn Storage, snap: &Snapshot, table: TableId) -> Vec<Row> {
    let mut rows = storage
        .scan(snap, table)
        .expect("scan")
        .map(|r| r.map(|(_, row)| row))
        .collect::<Result<Vec<_>, _>>()
        .expect("rows of the scan");
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

/// A linear congruential generator, so that a seed names one sequence of operations.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// The rows of `index` whose second column is `b`, sorted, as `snap` sees them.
fn seek_sorted(storage: &dyn Storage, snap: &Snapshot, index: IndexId, b: i32) -> Vec<Row> {
    let point = KeyRange::Point(vec![Value::I32(b)]);
    let mut rows = storage
        .seek(snap, index, &point, Direction::Forward)
        .expect("seek")
        .map(|r| r.map(|(_, row)| row))
        .collect::<Result<Vec<_>, _>>()
        .expect("rows of the seek");
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

/// Plays one seeded sequence of inserts, key updates, deletes, savepoints, commits,
/// rollbacks and reopens on a disk instance and on a memory one, and asserts after each
/// transaction that the two read the same rows; answers the number of comparisons.
fn disk_against_memory(clustered: bool, seed: u64) -> usize {
    let dir = fresh_dir(&format!("reopen-fuzz-{clustered}-{seed}"));
    let reopen = |dir: &Path| DiskStorage::open(dir, DiskOptions::default()).expect("open");
    let mut disk = reopen(&dir);
    let mem = MemoryStorage::new();
    let dt = disk.create_table(disk.create_database("a").expect("db"), &two_ints(clustered));
    let dt = dt.expect("disk table");
    let mt = mem.create_table(mem.create_database("a").expect("db"), &two_ints(clustered));
    let mt = mt.expect("memory table");
    let di = disk.create_index(dt, &index_on(1)).expect("disk index");
    let mi = mem.create_index(mt, &index_on(1)).expect("memory index");
    let mut rng = Lcg(seed);
    let mut live: Vec<(RowId, RowId)> = Vec::new();
    let (mut txn, mut checks) = (10u64, 0);
    let mut old: Option<Snapshot> = None;
    for _ in 0..40 {
        let t = TxnId(txn);
        txn += 1;
        let saved = live.clone();
        let mut mark = None;
        for _ in 0..1 + rng.below(6) {
            let row = pair(rng.below(50) as i32, rng.below(6) as i32);
            match rng.below(10) {
                0..=3 => {
                    let m = mem.insert(t, mt, &row).expect("memory insert");
                    live.push((m, disk.insert(t, dt, &row).expect("disk insert")));
                }
                4..=6 if !live.is_empty() => {
                    let (m, d) = live[rng.below(live.len() as u64) as usize];
                    mem.update(t, mt, m, &row).expect("memory update");
                    disk.update(t, dt, d, &row).expect("disk update");
                }
                7 if !live.is_empty() => {
                    let (m, d) = live.remove(rng.below(live.len() as u64) as usize);
                    mem.delete(t, mt, m).expect("memory delete");
                    disk.delete(t, dt, d).expect("disk delete");
                }
                8 if mark.is_none() => {
                    let m = mem.savepoint(t).expect("memory savepoint");
                    mark = Some((m, disk.savepoint(t).expect("disk savepoint"), live.clone()));
                }
                9 => {
                    if let Some((m, d, before)) = mark.take() {
                        mem.rollback_to(t, m).expect("memory rollback to");
                        disk.rollback_to(t, d).expect("disk rollback to");
                        live = before;
                    }
                }
                _ => {}
            }
        }
        if rng.below(4) == 0 {
            mem.rollback(t).expect("memory rollback");
            disk.rollback(t).expect("disk rollback");
            live = saved;
        } else {
            mem.commit(t).expect("memory commit");
            disk.commit(t).expect("disk commit");
        }
        if rng.below(4) == 0 {
            if rng.below(2) == 0 {
                disk.checkpoint().expect("checkpoint");
            }
            drop(disk);
            disk = reopen(&dir);
            txn = txn.max(disk.next_txn_id());
        }
        let now = settled_below(txn, txn);
        for snap in std::iter::once(&now).chain(old.as_ref()) {
            let (d, m) = (scanned(&disk, snap, dt), scanned(&mem, snap, mt));
            assert_eq!(d, m, "seed {seed} txn {txn} scan of {snap:?}");
            for b in 0..6 {
                let (d, m) = (
                    seek_sorted(&disk, snap, di, b),
                    seek_sorted(&mem, snap, mi, b),
                );
                assert_eq!(d, m, "seed {seed} txn {txn} seek {b} of {snap:?}");
            }
            for &(m, d) in &live {
                let (d, m) = (disk.get(snap, dt, d), mem.get(snap, mt, m));
                assert_eq!(
                    d.expect("disk get"),
                    m.expect("memory get"),
                    "seed {seed} get"
                );
            }
            checks += 1;
        }
        if rng.below(5) == 0 {
            old = Some(now);
        }
    }
    drop(disk);
    let _ = std::fs::remove_dir_all(&dir);
    checks
}

/// The comparisons of forty seeds, each of forty transactions: one or two per transaction.
fn forty_seeds(clustered: bool) {
    let checks: usize = (1..=40)
        .map(|seed| disk_against_memory(clustered, seed))
        .sum();
    assert!(
        checks >= 40 * 40,
        "clustered {clustered}: {checks} comparisons"
    );
}

#[test]
fn forty_seeds_of_writes_and_reopens_read_as_the_memory_store_reads_clustered() {
    forty_seeds(true);
}

#[test]
fn forty_seeds_of_writes_and_reopens_read_as_the_memory_store_reads_heap() {
    forty_seeds(false);
}
