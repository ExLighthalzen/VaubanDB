//! `sp_who` and `sp_who2` session listing procedures.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use vauban_compat::{ProcAction, ProcArg, register_functions, resolve_system_procedure};
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

const SPID_A: i16 = 52;

fn port(offset: u16) -> u16 {
    let base = std::env::var("VAUBAN_TEST_PORT")
        .ok()
        .and_then(|text| text.trim().parse::<u16>().ok())
        .unwrap_or(1433);
    base + offset
}

fn nvarchar(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

fn nvarchar_ty() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Max), true)
}

fn arg<'a>(name: Option<&'a str>, ty: &'a TypeInfo, value: &'a Value) -> ProcArg<'a> {
    ProcArg {
        name,
        ty,
        value,
        output: false,
        default: false,
    }
}

#[derive(Default)]
struct Recording {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    errors: Vec<SqlError>,
}

impl ResultSink for Recording {
    fn columns(&mut self, columns: &[ColumnMeta]) -> SqlResult<()> {
        self.columns = columns.to_vec();
        Ok(())
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.rows.push(row.to_vec());
        Ok(())
    }

    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }

    fn info(&mut self, _message: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }

    fn error(&mut self, error: &SqlError) -> SqlResult<()> {
        self.errors.push(error.clone());
        Ok(())
    }

    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        Ok(())
    }

    fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        Ok(())
    }

    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

fn session(spid: i16) -> Session {
    vauban_sysfn::register_builtins();
    register_functions();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(spid),
    )
}

fn run_batch(session: &mut Session, text: &str) -> Recording {
    let mut sink = Recording::default();
    session.run_batch(text, &mut sink).expect("batch");
    sink
}

#[test]
fn resolve_sp_who_returns_a_template_on_sys_dm_exec_sessions() {
    let action = resolve_system_procedure("sp_who", &[])
        .expect("known")
        .expect("ok");
    match action {
        ProcAction::Template { sql, params } => {
            assert!(sql.contains("sys.dm_exec_sessions"));
            assert!(sql.contains("sys.dm_exec_requests"));
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "@loginame");
        }
        other => panic!("expected Template, got {other:?}"),
    }
}

#[test]
fn resolve_sp_who2_returns_a_template() {
    let action = resolve_system_procedure("sp_who2", &[])
        .expect("known")
        .expect("ok");
    assert!(matches!(action, ProcAction::Template { .. }));
}

#[test]
fn sp_who_by_spid_binds_the_loginame_parameter() {
    let value = nvarchar("52");
    let ty = nvarchar_ty();
    let action = resolve_system_procedure("sp_who", &[arg(None, &ty, &value)])
        .expect("known")
        .expect("ok");
    if let ProcAction::Template { params, .. } = action {
        assert_eq!(params[0].value, value);
    } else {
        panic!("expected Template");
    }
}

#[test]
fn sp_who_executes_with_the_nine_learn_columns() {
    let mut session = session(SPID_A);
    let sink = run_batch(&mut session, "EXEC sp_who;");
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert_eq!(sink.columns.len(), 9);
    let names: Vec<_> = sink.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "spid",
            "ecid",
            "status",
            "loginame",
            "hostname",
            "blk",
            "dbname",
            "cmd",
            "request_id"
        ]
    );
}

struct RunningServe {
    child: Child,
    port: u16,
}

impl RunningServe {
    fn start(offset: u16) -> Self {
        let port = port(offset);
        let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/vauban");
        let port_text = port.to_string();
        let mut child = Command::new(bin)
            .current_dir(env!("CARGO_TARGET_TMPDIR"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args([
                "serve",
                "--in-memory",
                "--no-auth",
                "--encrypt",
                "optional",
                "--bind",
                "127.0.0.1",
                "--port",
                &port_text,
            ])
            .spawn()
            .expect("spawn vauban serve");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Self { child, port };
            }
            if let Some(status) = child.try_wait().expect("try_wait") {
                let output = child.wait_with_output().expect("output");
                panic!(
                    "vauban serve exited with {status}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                Instant::now() < deadline,
                "port {port} did not accept a connection"
            );
            sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for RunningServe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "network integration test with vauban serve and two tiberius clients"]
fn sp_who_lists_two_open_connections() {
    use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
    use tokio::net::TcpStream;
    use tokio::runtime::Runtime;
    use tokio_util::compat::TokioAsyncWriteCompatExt;

    let rt = Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let server = RunningServe::start(40);
        let mut config = Config::new();
        config.host("127.0.0.1");
        config.port(server.port);
        config.authentication(AuthMethod::sql_server("sa", "x"));
        config.encryption(EncryptionLevel::NotSupported);
        config.trust_cert();

        let tcp_a = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .expect("first connect");
        let mut client_a = Client::connect(config.clone(), tcp_a.compat_write())
            .await
            .expect("first client");
        let tcp_b = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .expect("second connect");
        let _client_b = Client::connect(config, tcp_b.compat_write())
            .await
            .expect("second client");

        use futures_util::TryStreamExt;
        use tiberius::QueryItem;

        let mut stream = client_a.simple_query("EXEC sp_who;").await.expect("sp_who");
        let mut rows = 0usize;
        while let Some(item) = stream.try_next().await.expect("row") {
            if matches!(item, QueryItem::Row(_)) {
                rows += 1;
            }
        }
        assert_eq!(rows, 2);
    });
}
