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
//! [`Storage::commit`]: vauban_storage::Storage::commit

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::env;

use vauban_storage::{DiskOptions, DiskStorage, Row, RowId, Snapshot, Storage, TableShape, TxnId};
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
    let mut child = spawn_child(&dir).expect("spawn the child");
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

/// The child body: opens the instance in the directory the parent put in the env, writes the
/// committed and the uncommitted rows, prints `READY`, and blocks on stdin until the parent
/// kills it.
#[test]
fn kill9_child_main() {
    let Ok(dir) = env::var(DIR_ENV) else {
        return;
    };
    child_write_and_wait(Path::new(&dir));
    std::process::exit(0);
}

/// Launches the same executable with the child env set and the child test as its filter.
fn spawn_child(dir: &Path) -> Result<Child, std::io::Error> {
    let exe = env::current_exe()?;
    let child = Command::new(exe)
        .arg("--exact")
        .arg(CHILD_TEST)
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, dir)
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
    std::fs::write(dir.join("ready"), b"").expect("write the ready marker");
    // Park until the parent kills this process: the child must stay alive so the parent
    // sends SIGKILL while its writes are unflushed. A sleep loop keeps the process awake
    // without holding a pipe open.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
