//! Network integration tests of the live session registry.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tokio_util::sync::CancellationToken;
use vauban_session::{Engine, NoAuth, Server, ServerConfig};
use vauban_storage::{DiskOptions, DiskStorage, MemoryStorage};
use vauban_tds::EncryptPolicy;

const DEFAULT_PACKET_SIZE: u16 = 4096;

fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
}

struct Running {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), vauban_errors::InternalError>>,
}

/// A fresh directory under the temporary directory, removed when dropped.
struct Instance(PathBuf);

impl Instance {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vauban-registry-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("instance directory");
        Instance(dir)
    }

    /// Length in bytes of the write-ahead journal of the instance.
    fn journal_len(&self) -> u64 {
        std::fs::metadata(self.0.join("wal"))
            .expect("the journal exists")
            .len()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tiberius_config(addr: SocketAddr) -> tiberius::Config {
    use tiberius::{AuthMethod, Config, EncryptionLevel};
    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(addr.port());
    config.authentication(AuthMethod::sql_server("sa", "ignored"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();
    config
}

async fn start_server(engine: Arc<Engine>) -> Running {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = Server::new(
        engine,
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(NoAuth),
            server_name: "registry-tiberius".into(),
            default_packet_size: DEFAULT_PACKET_SIZE,
            program_name: None,
            version_banner: None,
            edition: None,
        },
    );
    let task = tokio::spawn(server.serve(listener, shutdown.clone()));
    Running {
        addr,
        shutdown,
        task,
    }
}

impl Running {
    async fn stop(self) {
        self.shutdown.cancel();
        timeout(Duration::from_secs(6), self.task)
            .await
            .expect("serve returns")
            .expect("serve task")
            .expect("serve ok");
    }
}

#[tokio::test]
#[ignore = "network integration test with a real tiberius client"]
async fn tiberius_two_connections_then_one_close() {
    use tiberius::{AuthMethod, Client, Config, EncryptionLevel};

    let running = start_server(engine()).await;

    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(running.addr.port());
    config.authentication(AuthMethod::sql_server("sa", "ignored"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();

    let tcp_a = TcpStream::connect(running.addr).await.unwrap();
    let mut client_a = Client::connect(config.clone(), tcp_a.compat_write())
        .await
        .expect("first client connects");
    let tcp_b = TcpStream::connect(running.addr).await.unwrap();
    let mut client_b = Client::connect(config, tcp_b.compat_write())
        .await
        .expect("second client connects");

    let count_two = client_a
        .simple_query("SELECT COUNT(*) FROM master.dbo.vauban_sys_sessions")
        .await
        .unwrap()
        .into_row()
        .await
        .unwrap()
        .expect("count row")
        .get::<i32, _>(0)
        .expect("count");
    assert_eq!(count_two, 2);

    client_a.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let count_one = client_b
        .simple_query("SELECT COUNT(*) FROM master.dbo.vauban_sys_sessions")
        .await
        .unwrap()
        .into_row()
        .await
        .unwrap()
        .expect("count row")
        .get::<i32, _>(0)
        .expect("count");
    assert_eq!(count_one, 1);

    client_b.close().await.unwrap();
    running.stop().await;
}

async fn one_int(
    client: &mut tiberius::Client<tokio_util::compat::Compat<TcpStream>>,
    text: &str,
) -> i32 {
    client
        .simple_query(text)
        .await
        .expect("query")
        .into_row()
        .await
        .expect("row")
        .expect("one row")
        .get::<i32, _>(0)
        .expect("an int")
}

/// A thousand `SELECT 1` batches on a disk instance leave the journal at the length it had
/// before them: keeping the session registry up to date writes nothing to storage.
#[tokio::test]
async fn a_thousand_select_one_append_nothing_to_the_journal() {
    use tiberius::Client;

    let instance = Instance::new("journal");
    vauban_sysfn::register_builtins();
    let storage = DiskStorage::open(&instance.0, DiskOptions::default()).expect("open");
    let engine = Arc::new(Engine::new(Arc::new(storage)));
    let running = start_server(engine).await;

    let tcp = TcpStream::connect(running.addr).await.unwrap();
    let mut client = Client::connect(tiberius_config(running.addr), tcp.compat_write())
        .await
        .expect("client connects");
    assert_eq!(one_int(&mut client, "SELECT 1").await, 1);
    let before = instance.journal_len();
    for _ in 0..1000 {
        assert_eq!(one_int(&mut client, "SELECT 1").await, 1);
    }
    assert_eq!(instance.journal_len(), before);

    // The registry still follows the session: it is listed, and running while it asks.
    assert_eq!(
        one_int(
            &mut client,
            "SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE session_id = @@SPID",
        )
        .await,
        1
    );
    assert_eq!(
        one_int(
            &mut client,
            "SELECT COUNT(*) FROM sys.dm_exec_requests WHERE session_id = @@SPID",
        )
        .await,
        1
    );
    assert_eq!(instance.journal_len(), before);

    client.close().await.unwrap();
    running.stop().await;
}
