//! A `vauban serve --data` instance stopped and started again on the same directory keeps
//! its tables: their rows, their keys, their constraints, their defaults and their identity.
//! Two ways of stopping are covered: the SIGINT shutdown, and a `SIGKILL`, which the process
//! cannot catch. A directory whose catalogue carries another shape version is refused at
//! start-up, before the port is bound. A first start stopped before its commit is completed
//! by the next start, and an empty control file is written again when the instance beside it
//! is empty too, refused with a way out otherwise.
//!
//! Everything runs on `127.0.0.1` through a real TDS client (`tiberius`).
//!
//! # Ports
//!
//! Each server listens on `VAUBAN_TEST_PORT` (`1433` when unset) plus a fixed offset, one per
//! test: this file uses `50..=59`, apart from the ranges of `tls.rs`, `cli.rs` and
//! `registry.rs`.

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tiberius::error::Error;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream as AsyncTcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use vauban_storage::{DiskOptions, DiskStorage, Row, Snapshot, Storage, TableShape, TxnId};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

/// Cap, in seconds, on every wait for the server when `VAUBAN_TEST_TIMEOUT_SECS` is unset.
/// Each wait ends on the event it observes, so a large cap costs nothing on an idle machine.
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Raises (or lowers) that cap for a loaded machine.
const TIMEOUT_ENV: &str = "VAUBAN_TEST_TIMEOUT_SECS";

/// The cap on the waits of one test.
fn wait_cap() -> Duration {
    let secs = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// The port of the server of one test: `VAUBAN_TEST_PORT` when set, else `1433`, plus
/// `offset`.
fn port(offset: u16) -> u16 {
    let base = std::env::var("VAUBAN_TEST_PORT")
        .ok()
        .and_then(|text| text.trim().parse::<u16>().ok())
        .unwrap_or(1433);
    base + offset
}

/// A directory under the system temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vauban-cli-restart-it-{}-{n}-{label}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("data")).expect("temp dir");
        Self(path)
    }

    /// The `--data` directory of the instance.
    fn data(&self) -> PathBuf {
        self.0.join("data")
    }

    /// The file the server of the `start`-th start writes its output to.
    fn log(&self, start: usize) -> PathBuf {
        self.0.join(format!("server-{start}.log"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A running server, killed on drop.
struct Running {
    child: Option<Child>,
    port: u16,
}

impl Running {
    /// Starts `serve --data <dir> --no-auth --encrypt off` on `port(offset)`, its output in
    /// `log`, and waits for the port to accept a connection.
    fn start(offset: u16, data: &Path, log: &Path) -> Self {
        let port = port(offset);
        let out = fs::File::create(log).expect("log file");
        let err = out.try_clone().expect("log file");
        let mut child = Command::new(env!("CARGO_BIN_EXE_vauban"))
            .env_remove("VAUBAN_SA_PASSWORD")
            .env_remove("RUST_LOG")
            .current_dir(env!("CARGO_TARGET_TMPDIR"))
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .args(["serve", "--data"])
            .arg(data)
            .args([
                "--no-auth",
                "--encrypt",
                "off",
                "--bind",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .spawn()
            .expect("spawn the server");
        let started = Instant::now();
        let cap = wait_cap();
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!(
                    "the server exited with {status} before listening\n{}",
                    fs::read_to_string(log).unwrap_or_default()
                );
            }
            assert!(
                started.elapsed() < cap,
                "port {port} did not accept a connection within {cap:?}\n{}",
                fs::read_to_string(log).unwrap_or_default()
            );
            sleep(Duration::from_millis(50));
        }
        Self {
            child: Some(child),
            port,
        }
    }

    /// Sends SIGINT and waits for the process to exit, which is the shutdown that writes a
    /// checkpoint.
    fn interrupt(mut self) {
        let mut child = self.child.take().expect("still running");
        let sent = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(sent.success(), "kill -INT failed: {sent}");
        let started = Instant::now();
        let cap = wait_cap();
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                return;
            }
            assert!(started.elapsed() < cap, "no exit within {cap:?} of SIGINT");
            sleep(Duration::from_millis(50));
        }
    }

    /// Sends SIGKILL and reaps the process: nothing runs after the signal.
    fn kill(mut self) {
        let mut child = self.child.take().expect("still running");
        child.kill().expect("SIGKILL");
        child.wait().expect("reap");
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

type Session = Client<Compat<AsyncTcpStream>>;

/// Logs in as `sa` on `port`, without encryption.
async fn connect(port: u16) -> Session {
    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(port);
    config.authentication(AuthMethod::sql_server("sa", "any"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();
    let tcp = AsyncTcpStream::connect(config.get_addr())
        .await
        .expect("connect");
    tcp.set_nodelay(true).expect("nodelay");
    Client::connect(config, tcp.compat_write())
        .await
        .expect("login")
}

/// Runs `sql` and gives back the rows of each result set.
async fn run(client: &mut Session, sql: &str) -> Result<Vec<Vec<tiberius::Row>>, Error> {
    client.simple_query(sql).await?.into_results().await
}

/// Runs `sql`, which must succeed.
async fn ok(client: &mut Session, sql: &str) -> Vec<Vec<tiberius::Row>> {
    run(client, sql)
        .await
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
}

/// Runs `sql`, which must fail with the error `number`.
async fn fails_with(client: &mut Session, sql: &str, number: u32) {
    match run(client, sql).await {
        Err(Error::Server(token)) => assert_eq!(token.code(), number, "{sql}: {token}"),
        other => panic!("{sql}: expected error {number}, got {other:?}"),
    }
}

/// The `int` columns of the rows of the first result set of `sql`.
async fn ints(client: &mut Session, sql: &str) -> Vec<Vec<i32>> {
    ok(client, sql).await[0]
        .iter()
        .map(|row| {
            (0..row.len())
                .map(|at| row.get::<i32, _>(at).expect("an int"))
                .collect()
        })
        .collect()
}

/// The largest `object_id` `sys.objects` and `sys.all_objects` publish.
async fn largest_object_id(client: &mut Session) -> i32 {
    ints(
        client,
        "SELECT MAX(object_id) FROM (SELECT object_id FROM sys.objects \
         UNION ALL SELECT object_id FROM sys.all_objects) AS ids",
    )
    .await[0][0]
}

/// The two tables of the first start: a parent with a `PRIMARY KEY` and a `UNIQUE`
/// constraint, a child with an identity key, a `FOREIGN KEY`, a `CHECK`, a `DEFAULT
/// GETDATE()`, a literal `DEFAULT` and an index.
const SCHEMA: &str = "\
CREATE TABLE dbo.parent (id int NOT NULL PRIMARY KEY, code varchar(10) NOT NULL UNIQUE);
INSERT dbo.parent VALUES (1, 'a'), (2, 'b');
CREATE TABLE dbo.child (
  id int IDENTITY(10, 5) NOT NULL PRIMARY KEY,
  parent_id int NOT NULL REFERENCES dbo.parent (id),
  qty int NOT NULL CHECK (qty > 0),
  created datetime NOT NULL DEFAULT GETDATE(),
  status int NOT NULL DEFAULT 7,
  label nvarchar(20) NULL
);
CREATE INDEX ix_child_parent ON dbo.child (parent_id);
INSERT dbo.child (parent_id, qty, created) VALUES (1, 3, '2020-01-01'), (2, 4, '2020-01-02');";

#[test]
fn tables_and_constraints_hold_across_a_clean_restart() {
    let dir = TempDir::new("clean");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    let server = Running::start(50, &dir.data(), &dir.log(1));
    let persisted = runtime.block_on(async {
        let mut client = connect(server.port).await;
        ok(&mut client, SCHEMA).await;
        largest_object_id(&mut client).await
    });
    server.interrupt();

    let server = Running::start(50, &dir.data(), &dir.log(2));
    runtime.block_on(async {
        let mut client = connect(server.port).await;
        assert_eq!(
            ints(
                &mut client,
                "SELECT id, parent_id, qty, status FROM dbo.child ORDER BY id"
            )
            .await,
            vec![vec![10, 1, 3, 7], vec![15, 2, 4, 7]]
        );
        // The `PRIMARY KEY` and the `UNIQUE` of the parent, the `FOREIGN KEY`, the `CHECK`
        // and the `NOT NULL` of the child.
        fails_with(&mut client, "INSERT dbo.parent VALUES (1, 'z')", 2627).await;
        fails_with(&mut client, "INSERT dbo.parent VALUES (3, 'a')", 2627).await;
        fails_with(
            &mut client,
            "INSERT dbo.child (parent_id, qty, created) VALUES (9, 1, '2020-01-03')",
            547,
        )
        .await;
        fails_with(
            &mut client,
            "INSERT dbo.child (parent_id, qty, created) VALUES (1, 0, '2020-01-03')",
            547,
        )
        .await;
        fails_with(
            &mut client,
            "INSERT dbo.child (parent_id, qty, created) VALUES (NULL, 1, '2020-01-03')",
            515,
        )
        .await;
        // A row that satisfies them all takes an identity past the two of the first start,
        // and the literal default of `status`.
        ok(
            &mut client,
            "INSERT dbo.child (parent_id, qty, created) VALUES (2, 5, '2020-01-03')",
        )
        .await;
        let rows = ints(
            &mut client,
            "SELECT id, status FROM dbo.child WHERE qty = 5",
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0][0] > 15, "identity {} reuses a value", rows[0][0]);
        assert_eq!(rows[0][1], 7);
        // The text of the `GETDATE()` default is the one stored before the restart.
        let definitions = ok(
            &mut client,
            "SELECT definition FROM sys.default_constraints \
             WHERE parent_object_id = OBJECT_ID('dbo.child') ORDER BY parent_column_id",
        )
        .await;
        let texts: Vec<&str> = definitions[0]
            .iter()
            .map(|row| row.get::<&str, _>(0).expect("a definition"))
            .collect();
        assert_eq!(texts, vec!["(getdate())", "((7))"]);
        // A table shipped in `master.dbo` reads as well.
        ok(&mut client, "SELECT * FROM master.dbo.spt_monitor").await;
        // An object created now takes an identifier none of the stored ones holds.
        ok(&mut client, "CREATE TABLE dbo.fresh (a int)").await;
        let fresh = ints(&mut client, "SELECT OBJECT_ID('dbo.fresh')").await[0][0];
        assert!(fresh > persisted, "{fresh} is not past {persisted}");
    });
    server.interrupt();
}

#[test]
fn identity_and_object_ids_hold_across_a_killed_process() {
    let dir = TempDir::new("killed");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    let server = Running::start(51, &dir.data(), &dir.log(1));
    let persisted = runtime.block_on(async {
        let mut client = connect(server.port).await;
        ok(
            &mut client,
            "CREATE TABLE dbo.k (id int IDENTITY(1, 1) NOT NULL PRIMARY KEY, \
             v int NOT NULL CHECK (v < 100)); \
             INSERT dbo.k (v) VALUES (1), (2), (3);",
        )
        .await;
        largest_object_id(&mut client).await
    });
    server.kill();

    let server = Running::start(51, &dir.data(), &dir.log(2));
    runtime.block_on(async {
        let mut client = connect(server.port).await;
        assert_eq!(
            ints(&mut client, "SELECT id, v FROM dbo.k ORDER BY id").await,
            vec![vec![1, 1], vec![2, 2], vec![3, 3]]
        );
        fails_with(&mut client, "INSERT dbo.k (v) VALUES (100)", 547).await;
        ok(&mut client, "INSERT dbo.k (v) VALUES (4)").await;
        let ids = ints(&mut client, "SELECT id FROM dbo.k WHERE v = 4").await;
        assert!(ids[0][0] > 3, "identity {} reuses a value", ids[0][0]);
        ok(&mut client, "CREATE TABLE dbo.after_kill (a int)").await;
        let fresh = ints(&mut client, "SELECT OBJECT_ID('dbo.after_kill')").await[0][0];
        assert!(fresh > persisted, "{fresh} is not past {persisted}");
    });
    server.kill();

    // A second kill, after rows written by the restarted process, keeps those rows too.
    let server = Running::start(51, &dir.data(), &dir.log(3));
    runtime.block_on(async {
        let mut client = connect(server.port).await;
        assert_eq!(
            ints(&mut client, "SELECT COUNT(*) FROM dbo.k").await,
            vec![vec![4]]
        );
        ok(&mut client, "SELECT a FROM dbo.after_kill").await;
    });
    server.interrupt();
}

/// `DROP INDEX` of the unique index a foreign key reads is refused (3723), so the key is
/// still there after the restart and the foreign key still applies. A foreign key left
/// without its index on disk is the reopening's to skip (`vauban-catalog`,
/// `a_foreign_key_whose_index_has_gone_is_left_out`).
#[test]
fn a_refused_drop_index_leaves_the_foreign_key_whole_across_a_restart() {
    let dir = TempDir::new("left-out");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    let server = Running::start(52, &dir.data(), &dir.log(1));
    let (dropped, persisted) = runtime.block_on(async {
        let mut client = connect(server.port).await;
        ok(
            &mut client,
            "CREATE TABLE dbo.r (id int NOT NULL, k int NOT NULL);
             CREATE UNIQUE INDEX ux_r ON dbo.r (k);
             INSERT dbo.r VALUES (1, 10);
             CREATE TABLE dbo.f (x int CONSTRAINT fk_f REFERENCES dbo.r (k));
             INSERT dbo.f VALUES (10);
             CREATE TABLE dbo.g (a int NOT NULL);
             INSERT dbo.g VALUES (1);
             CREATE TABLE dbo.w (a int, b int, c int);
             ALTER TABLE dbo.w DROP COLUMN c;
             ALTER TABLE dbo.w ADD d int;
             ALTER TABLE dbo.w DROP COLUMN d;
             CREATE TABLE dbo.zlast (a int);",
        )
        .await;
        fails_with(&mut client, "DROP INDEX ux_r ON dbo.r", 3723).await;
        let dropped = ints(&mut client, "SELECT OBJECT_ID('dbo.zlast')").await[0][0];
        ok(&mut client, "DROP TABLE dbo.zlast").await;
        (dropped, largest_object_id(&mut client).await)
    });
    server.interrupt();

    let server = Running::start(52, &dir.data(), &dir.log(2));
    runtime.block_on(async {
        let mut client = connect(server.port).await;
        // The instance opens, and each table reads.
        assert_eq!(
            ints(&mut client, "SELECT a FROM dbo.g").await,
            vec![vec![1]]
        );
        assert_eq!(
            ints(&mut client, "SELECT k FROM dbo.r").await,
            vec![vec![10]]
        );
        assert_eq!(
            ints(&mut client, "SELECT x FROM dbo.f").await,
            vec![vec![10]]
        );
        // The foreign key still reads its index.
        fails_with(&mut client, "INSERT dbo.f VALUES (20)", 547).await;
        ok(&mut client, "INSERT dbo.f VALUES (10)").await;
        // The column added after the restart takes the number after the last one handed out.
        ok(&mut client, "ALTER TABLE dbo.w ADD e int").await;
        assert_eq!(
            ints(
                &mut client,
                "SELECT column_id FROM sys.columns \
                 WHERE object_id = OBJECT_ID('dbo.w') ORDER BY column_id"
            )
            .await,
            vec![vec![1], vec![2], vec![5]]
        );
        // The identifier of the dropped table is not handed out again.
        ok(&mut client, "CREATE TABLE dbo.znew (a int)").await;
        let fresh = ints(&mut client, "SELECT OBJECT_ID('dbo.znew')").await[0][0];
        assert!(fresh > dropped, "{fresh} is not past the dropped {dropped}");
        assert!(fresh > persisted, "{fresh} is not past {persisted}");
    });
    server.interrupt();
}

/// The name of the row of the registry of internal tables that carries the shape version of
/// the catalogue, and the version written in its place by the test below.
const SHAPE_VERSION_ENTRY: &str = "vauban_catalog_shape_version";
const FOREIGN_SHAPE_VERSION: i64 = 999;

/// Rewrites the shape version the catalogue of `data` carries, through `storage` directly:
/// the registry is the first table of `master`, a row `(name, table_id)`.
fn rewrite_shape_version(data: &Path, version: i64) {
    let storage = DiskStorage::open(data, DiskOptions::default()).expect("open the directory");
    let (master, _) = storage
        .databases()
        .expect("databases")
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case("master"))
        .expect("master");
    let (registry, _) = storage
        .tables(master)
        .expect("tables")
        .into_iter()
        .next()
        .expect("the registry");
    let committed = Snapshot {
        xmin: TxnId(u64::MAX),
        xmax: TxnId(u64::MAX),
        active: Vec::new(),
        own: TxnId(0),
    };
    let target = storage
        .scan(&committed, registry)
        .expect("scan")
        .map(|row| row.expect("row"))
        .find(|(_, row)| {
            matches!(row.0.first(), Some(Value::String(name)) if name.text == SHAPE_VERSION_ENTRY)
        })
        .map(|(id, _)| id)
        .expect("the row of the shape version");
    let txn = TxnId(
        storage
            .next_txn_id()
            .max(storage.committed_txn_high_water() + 1),
    );
    let row = Row(vec![
        Value::String(SqlString {
            text: SHAPE_VERSION_ENTRY.to_owned(),
        }),
        Value::I64(version),
    ]);
    storage.update(txn, registry, target, &row).expect("update");
    storage.commit(txn).expect("commit");
    storage.checkpoint().expect("checkpoint");
}

#[test]
fn a_directory_of_another_catalogue_shape_is_refused_before_listening() {
    let dir = TempDir::new("shape");

    // The directory this binary creates opens again.
    let server = Running::start(53, &dir.data(), &dir.log(1));
    server.interrupt();
    let server = Running::start(53, &dir.data(), &dir.log(2));
    server.interrupt();

    rewrite_shape_version(&dir.data(), FOREIGN_SHAPE_VERSION);

    let port = port(53);
    let log = dir.log(3);
    let out = fs::File::create(&log).expect("log file");
    let err = out.try_clone().expect("log file");
    let mut child = Command::new(env!("CARGO_BIN_EXE_vauban"))
        .env_remove("VAUBAN_SA_PASSWORD")
        .env_remove("RUST_LOG")
        .current_dir(env!("CARGO_TARGET_TMPDIR"))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .args(["serve", "--data"])
        .arg(dir.data())
        .args([
            "--no-auth",
            "--encrypt",
            "off",
            "--bind",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .spawn()
        .expect("spawn the server");
    let started = Instant::now();
    let cap = wait_cap();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() > cap {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the server did not exit within {cap:?}\n{}",
                fs::read_to_string(&log).unwrap_or_default()
            );
        }
        sleep(Duration::from_millis(50));
    };
    let output = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !status.success(),
        "the server exited with {status}\n{output}"
    );
    assert!(
        output.contains(&format!("shape version {FOREIGN_SHAPE_VERSION}"))
            && output.contains("this binary expects version"),
        "{output}"
    );
    assert!(!output.contains("vauban listening"), "{output}");
}

/// Starts `serve --data <data>` on `port(offset)` and waits for the process to exit, which it
/// does when it refuses the directory; gives back its output.
fn refused_start(offset: u16, data: &Path, log: &Path) -> String {
    let port = port(offset);
    let out = fs::File::create(log).expect("log file");
    let err = out.try_clone().expect("log file");
    let mut child = Command::new(env!("CARGO_BIN_EXE_vauban"))
        .env_remove("VAUBAN_SA_PASSWORD")
        .env_remove("RUST_LOG")
        .current_dir(env!("CARGO_TARGET_TMPDIR"))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .args(["serve", "--data"])
        .arg(data)
        .args([
            "--no-auth",
            "--encrypt",
            "off",
            "--bind",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .spawn()
        .expect("spawn the server");
    let started = Instant::now();
    let cap = wait_cap();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() > cap {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the server did not exit within {cap:?}\n{}",
                fs::read_to_string(log).unwrap_or_default()
            );
        }
        sleep(Duration::from_millis(50));
    };
    let output = fs::read_to_string(log).unwrap_or_default();
    assert!(
        !status.success(),
        "the server exited with {status}\n{output}"
    );
    assert!(!output.contains("vauban listening"), "{output}");
    output
}

/// Three counts: the databases of `sys.databases`, the names of `sys.all_objects` that come
/// back more than once in a schema, and the tables of `master.sys.tables`.
async fn system_counts(client: &mut Session) -> Vec<Vec<i32>> {
    ints(
        client,
        "SELECT (SELECT COUNT(*) FROM sys.databases), \
         (SELECT COUNT(*) FROM (SELECT schema_id, name FROM sys.all_objects \
          GROUP BY schema_id, name HAVING COUNT(*) > 1) AS twice), \
         (SELECT COUNT(*) FROM master.sys.tables)",
    )
    .await
}

/// Leaves in `data` what a first start stopped before its commit leaves: `master` with the
/// registry of the internal tables, and a row of it written by a transaction that did not
/// commit.
fn leave_a_stopped_first_start(data: &Path) {
    let storage = DiskStorage::open(data, DiskOptions::default()).expect("a new directory");
    let master = storage.create_database("master").expect("master");
    let name = || TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false);
    let registry = storage
        .create_table(
            master,
            &TableShape {
                columns: vec![name(), TypeInfo::new(SqlType::BigInt, false)],
                clustered_key: None,
            },
        )
        .expect("the registry");
    let txn = TxnId(storage.next_txn_id());
    let text = |text: &str| {
        Value::String(SqlString {
            text: text.to_owned(),
        })
    };
    storage
        .insert(
            txn,
            registry,
            &Row(vec![text("vauban_sys_databases"), Value::I64(2)]),
        )
        .expect("a registry row");
    // Neither commit nor checkpoint: the process stops here.
}

#[test]
fn a_first_start_stopped_before_its_commit_is_completed_by_the_next() {
    let reference = TempDir::new("whole-first-start");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let server = Running::start(54, &reference.data(), &reference.log(1));
    let expected = runtime.block_on(async {
        let mut client = connect(server.port).await;
        system_counts(&mut client).await
    });
    server.interrupt();
    assert_eq!(expected[0][..2], [4, 0], "{expected:?}");

    let dir = TempDir::new("stopped-first-start");
    leave_a_stopped_first_start(&dir.data());
    let server = Running::start(54, &dir.data(), &dir.log(1));
    runtime.block_on(async {
        let mut client = connect(server.port).await;
        assert_eq!(system_counts(&mut client).await, expected);
        ok(&mut client, "CREATE TABLE dbo.after_resume (a int)").await;
    });
    server.interrupt();

    // The completed directory opens as a bootstrapped one.
    let server = Running::start(54, &dir.data(), &dir.log(2));
    runtime.block_on(async {
        let mut client = connect(server.port).await;
        assert_eq!(
            ints(&mut client, "SELECT COUNT(*) FROM dbo.after_resume").await,
            vec![vec![0]]
        );
        let mut counts = expected.clone();
        counts[0][2] += 1;
        assert_eq!(system_counts(&mut client).await, counts);
    });
    server.interrupt();
}

#[test]
fn an_empty_control_file_of_an_unwritten_instance_is_written_again() {
    let dir = TempDir::new("empty-ctl");
    for name in ["vauban.ctl", "data", "wal"] {
        fs::write(dir.data().join(name), []).expect("an empty file");
    }
    let server = Running::start(55, &dir.data(), &dir.log(1));
    server.interrupt();
    assert_eq!(
        fs::metadata(dir.data().join("vauban.ctl"))
            .expect("the control file")
            .len(),
        128
    );
    let server = Running::start(55, &dir.data(), &dir.log(2));
    server.interrupt();
}

#[test]
fn an_empty_control_file_beside_pages_is_refused_with_a_way_out() {
    let dir = TempDir::new("empty-ctl-pages");
    fs::write(dir.data().join("vauban.ctl"), []).expect("an empty control file");
    fs::write(dir.data().join("data"), vec![0u8; 8192]).expect("a page");
    fs::write(dir.data().join("wal"), []).expect("an empty journal");
    let output = refused_start(56, &dir.data(), &dir.log(1));
    assert!(
        output.contains("0 bytes") && output.contains("remove it and start again"),
        "{output}"
    );
}

/// The databases of the instance in `data` and the shape of each of their tables, as
/// `storage` lists them.
fn layout(data: &Path) -> Vec<(String, Vec<String>)> {
    let storage = DiskStorage::open(data, DiskOptions::default()).expect("open the directory");
    storage
        .databases()
        .expect("databases")
        .into_iter()
        .map(|(db, name)| {
            let tables = storage
                .tables(db)
                .expect("tables")
                .into_iter()
                .map(|(id, shape)| format!("{id:?} {shape:?}"))
                .collect();
            (name, tables)
        })
        .collect()
}

#[test]
fn a_bootstrapped_directory_whose_journal_lost_its_commits_is_refused_untouched() {
    let dir = TempDir::new("journal-cut");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let server = Running::start(57, &dir.data(), &dir.log(1));
    runtime.block_on(async {
        let mut client = connect(server.port).await;
        ok(&mut client, "CREATE DATABASE userdb").await;
        ok(
            &mut client,
            "CREATE TABLE dbo.precious (a int NOT NULL); \
             INSERT dbo.precious (a) VALUES (1), (2), (3);",
        )
        .await;
    });
    server.interrupt();
    let before = layout(&dir.data());
    assert!(
        before.iter().any(|(name, _)| name == "userdb"),
        "{before:?}"
    );

    // The journal loses everything past its first thousand bytes, the commit of the first
    // start among it; the pages of the checkpoint still hold the tables.
    fs::OpenOptions::new()
        .write(true)
        .open(dir.data().join("wal"))
        .expect("open the journal")
        .set_len(1000)
        .expect("cut the journal");

    let mut data_seen: Option<Vec<u8>> = None;
    for start in 2..5 {
        let output = refused_start(57, &dir.data(), &dir.log(start));
        assert!(
            output.contains("holds database userdb") && output.contains("left as it is"),
            "{output}"
        );
        // The pages stay as they are from one refused start to the next; the journal is the
        // recovery's to write.
        let data = fs::read(dir.data().join("data")).expect("data");
        if let Some(seen) = &data_seen {
            assert!(&data == seen, "start {start} changed data");
        }
        data_seen = Some(data);
        assert_eq!(layout(&dir.data()), before, "start {start}");
    }
}

#[test]
fn a_second_server_on_a_directory_in_use_is_refused() {
    let dir = TempDir::new("two-servers");
    let first = Running::start(58, &dir.data(), &dir.log(1));
    let output = refused_start(59, &dir.data(), &dir.log(2));
    assert!(output.contains("in use by another process"), "{output}");
    first.interrupt();
    let again = Running::start(58, &dir.data(), &dir.log(3));
    again.interrupt();
}
