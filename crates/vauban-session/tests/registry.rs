//! Network integration tests of the live session registry.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tokio_util::sync::CancellationToken;
use vauban_session::{Engine, NoAuth, Server, ServerConfig};
use vauban_storage::MemoryStorage;
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
