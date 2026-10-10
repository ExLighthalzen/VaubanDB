//! A `vauban serve --data` instance stopped and started again on the same directory keeps
//! its tables: their rows, their keys, their constraints, their defaults and their identity.
//! Two ways of stopping are covered: the SIGINT shutdown, and a `SIGKILL`, which the process
//! cannot catch.
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

/// How long the port has to accept a connection after the start, and the process to exit
/// after a SIGINT.
const WAIT: Duration = Duration::from_secs(30);

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
                started.elapsed() < WAIT,
                "port {port} did not accept a connection within {WAIT:?}\n{}",
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
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                return;
            }
            assert!(
                started.elapsed() < WAIT,
                "no exit within {WAIT:?} of SIGINT"
            );
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

#[test]
fn a_foreign_key_left_without_its_index_does_not_stop_the_instance() {
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
             DROP INDEX ux_r ON dbo.r;
             CREATE TABLE dbo.g (a int NOT NULL);
             INSERT dbo.g VALUES (1);
             CREATE TABLE dbo.w (a int, b int, c int);
             ALTER TABLE dbo.w DROP COLUMN c;
             ALTER TABLE dbo.w ADD d int;
             ALTER TABLE dbo.w DROP COLUMN d;
             CREATE TABLE dbo.zlast (a int);",
        )
        .await;
        let dropped = ints(&mut client, "SELECT OBJECT_ID('dbo.zlast')").await[0][0];
        ok(&mut client, "DROP TABLE dbo.zlast").await;
        (dropped, largest_object_id(&mut client).await)
    });
    server.interrupt();

    let server = Running::start(52, &dir.data(), &dir.log(2));
    runtime.block_on(async {
        let mut client = connect(server.port).await;
        // The instance opens, and each table reads, the one left without its key included.
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
        ok(&mut client, "INSERT dbo.f VALUES (20)").await;
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
    let log = fs::read_to_string(dir.log(2)).unwrap_or_default();
    assert!(log.contains("is left out"), "no warning in the log:\n{log}");
}
