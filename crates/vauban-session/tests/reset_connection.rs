//! RESETCONNECTION: session reset and SqlClient pooling.

use std::process::Command;
use std::sync::{Arc, Once};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_parser::{ParseOptions, parse_parameter_declarations};
use vauban_session::{
    Engine, NoAuth, ProcAction, ProcArg, ProcParam, ResultSink, Server, ServerConfig, Session,
    SessionState, apply_reset, register_system_procedure_resolver,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange, ResetConnection, Rpc, RpcParam, RpcProc};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

const SPID: i16 = 91;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    EnvChange(EnvChange),
    Done(Option<u64>),
    Error(u32),
    Row(Value),
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, _: &[ColumnMeta]) -> SqlResult<()> {
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.0.push(Event::Row(row[0].clone()));
        Ok(())
    }
    fn done(&mut self, rowcount: Option<u64>, _: bool) -> SqlResult<()> {
        self.0.push(Event::Done(rowcount));
        Ok(())
    }
    fn info(&mut self, _: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.number));
        Ok(())
    }
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.0.push(Event::EnvChange(change.clone()));
        Ok(())
    }
    fn return_value(&mut self, _: &str, _: &TypeInfo, _: &Value) -> SqlResult<()> {
        Ok(())
    }
    fn return_status(&mut self, _: i32) -> SqlResult<()> {
        Ok(())
    }
}

fn register_prepare_resolver() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        register_system_procedure_resolver(|name, args| {
            let base = name.rsplit('.').next().unwrap_or(name);
            match base.to_ascii_lowercase().as_str() {
                "sp_prepexec" => Some(resolve_prepexec(args)),
                "sp_execute" => Some(resolve_execute(args)),
                _ => None,
            }
        });
    });
}

fn read_int(value: &Value) -> Option<i32> {
    match value {
        Value::I32(v) => Some(*v),
        Value::I16(v) => Some(i32::from(*v)),
        Value::I64(v) => i32::try_from(*v).ok(),
        _ => None,
    }
}

fn resolve_prepexec(args: &[ProcArg<'_>]) -> Result<ProcAction, SqlError> {
    let handle = args
        .iter()
        .position(|arg| arg.output && matches!(arg.ty.ty, SqlType::Int))
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_prepexec", "@handle"))?;
    let params_text = args
        .iter()
        .find_map(|arg| match (arg.name, arg.value) {
            (Some("@params") | Some("params"), Value::String(text)) => Some(text.text.clone()),
            (None, Value::String(text)) => Some(text.text.clone()),
            _ => None,
        })
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_prepexec", "@params"))?;
    let statement = args
        .iter()
        .find_map(|arg| match (arg.name, arg.value) {
            (Some("@stmt") | Some("@statement") | Some("stmt"), Value::String(text)) => {
                Some(text.text.clone())
            }
            (None, Value::String(text)) if text.text != params_text => Some(text.text.clone()),
            _ => None,
        })
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_prepexec", "@stmt"))?;
    let declarations = parse_parameter_declarations(&params_text, &ParseOptions::default())?;
    let execute_with: Vec<ProcParam> = args
        .iter()
        .enumerate()
        .filter(|(index, arg)| {
            if *index == handle {
                return false;
            }
            match arg.name {
                Some("@params") | Some("params") | Some("@stmt") | Some("@statement")
                | Some("stmt") => false,
                None => !matches!(arg.value, Value::String(_)),
                _ => true,
            }
        })
        .map(|(_, arg)| ProcParam {
            name: String::new(),
            ty: arg.ty.clone(),
            value: arg.value.clone(),
            output: arg.output,
        })
        .collect();
    Ok(ProcAction::Prepare {
        statement,
        params: declarations,
        handle_arg: handle,
        execute_with: Some(execute_with),
    })
}

fn resolve_execute(args: &[ProcArg<'_>]) -> Result<ProcAction, SqlError> {
    let mut handle = None;
    let mut params = Vec::new();
    for arg in args {
        if handle.is_none() {
            handle = read_int(arg.value);
        } else {
            params.push(ProcParam {
                name: format!("@P{}", params.len() + 1),
                ty: arg.ty.clone(),
                value: arg.value.clone(),
                output: arg.output,
            });
        }
    }
    let handle =
        handle.ok_or_else(|| SqlError::procedure_expects_parameter("sp_execute", "@handle"))?;
    Ok(ProcAction::Execute { handle, params })
}

fn session() -> (Session, Arc<Engine>) {
    vauban_sysfn::register_builtins();
    register_prepare_resolver();
    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
    let session = Session::new(Arc::clone(&engine), SessionState::new(SPID));
    (session, engine)
}

fn run_batch(session: &mut Session, text: &str, sink: &mut Recording) {
    session.run_batch(text, sink).expect("batch");
}

fn reset(session: &mut Session, engine: &Arc<Engine>, mode: ResetConnection, sink: &mut Recording) {
    apply_reset(session, engine, "master", mode, sink).expect("reset");
}

fn int(value: i32) -> Value {
    Value::I32(value)
}

fn int_ty() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

fn nvarchar_ty() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Max), false)
}

fn prepexec_rpc(value: i32) -> Rpc {
    Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name("sp_prepexec".into()),
        options: 0,
        params: vec![
            RpcParam {
                name: String::new(),
                output: true,
                default: false,
                ty: int_ty(),
                value: int(0),
            },
            RpcParam {
                name: "params".into(),
                output: false,
                default: false,
                ty: nvarchar_ty(),
                value: Value::String(SqlString {
                    text: "@P1 int".into(),
                }),
            },
            RpcParam {
                name: "stmt".into(),
                output: false,
                default: false,
                ty: nvarchar_ty(),
                value: Value::String(SqlString {
                    text: "SELECT @P1".into(),
                }),
            },
            RpcParam {
                name: String::new(),
                output: false,
                default: false,
                ty: int_ty(),
                value: int(value),
            },
        ],
        transaction_descriptor: 0,
    }
}

fn execute_rpc(handle: i32, value: i32) -> Rpc {
    Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name("sp_execute".into()),
        options: 0,
        params: vec![
            RpcParam {
                name: String::new(),
                output: false,
                default: false,
                ty: int_ty(),
                value: int(handle),
            },
            RpcParam {
                name: String::new(),
                output: false,
                default: false,
                ty: int_ty(),
                value: int(value),
            },
        ],
        transaction_descriptor: 0,
    }
}

#[test]
fn use_then_full_returns_login_database() {
    let (mut session, engine) = session();
    let mut sink = Recording::default();
    run_batch(&mut session, "CREATE DATABASE d;", &mut sink);
    run_batch(&mut session, "USE d;", &mut sink);
    assert_eq!(session.state().database, "d");
    sink.0.clear();
    reset(&mut session, &engine, ResetConnection::Full, &mut sink);
    assert_eq!(session.state().database, "master");
    assert_eq!(
        sink.0,
        vec![Event::EnvChange(EnvChange::Database {
            old: "d".into(),
            new: "master".into(),
        })]
    );
}

#[test]
fn nocoount_is_cleared_by_full_reset() {
    let (mut session, engine) = session();
    let mut sink = Recording::default();
    run_batch(&mut session, "SET NOCOUNT ON;", &mut sink);
    assert!(session.state().options.nocount);
    sink.0.clear();
    reset(&mut session, &engine, ResetConnection::Full, &mut sink);
    assert!(!session.state().options.nocount);
    sink.0.clear();
    run_batch(&mut session, "SELECT 1;", &mut sink);
    assert!(
        matches!(sink.0.last(), Some(Event::Done(Some(1)))),
        "done carries the row count after reset: {:?}",
        sink.0
    );
}

#[test]
fn batch_variables_do_not_survive_reset() {
    let (mut session, engine) = session();
    let mut sink = Recording::default();
    run_batch(&mut session, "DECLARE @x int = 1; SELECT @x;", &mut sink);
    sink.0.clear();
    reset(&mut session, &engine, ResetConnection::Full, &mut sink);
    run_batch(&mut session, "SELECT @x;", &mut sink);
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::Error(137))),
        "undeclared variable after reset: {:?}",
        sink.0
    );
}

#[test]
fn prepared_handle_is_cleared_by_full_reset() {
    let (mut session, engine) = session();
    let mut sink = Recording::default();
    session
        .run_rpc(&prepexec_rpc(1), &mut sink)
        .expect("prepexec");
    sink.0.clear();
    reset(&mut session, &engine, ResetConnection::Full, &mut sink);
    session
        .run_rpc(&execute_rpc(1, 2), &mut sink)
        .expect("execute after reset");
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::Error(8179))),
        "missing prepared handle after reset: {:?}",
        sink.0
    );
}

#[test]
fn full_rolls_back_skip_keeps_transaction() {
    let (mut session, engine) = session();
    let mut sink = Recording::default();
    run_batch(&mut session, "CREATE TABLE dbo.t(c int);", &mut sink);
    run_batch(
        &mut session,
        "BEGIN TRAN; INSERT dbo.t VALUES (1);",
        &mut sink,
    );
    sink.0.clear();
    reset(&mut session, &engine, ResetConnection::Full, &mut sink);
    run_batch(&mut session, "SELECT COUNT(*) FROM dbo.t;", &mut sink);
    assert_eq!(
        sink.0.iter().find_map(|event| match event {
            Event::Row(Value::I32(v)) => Some(*v),
            _ => None,
        }),
        Some(0),
        "full reset rolls the insert back: {:?}",
        sink.0
    );

    run_batch(
        &mut session,
        "BEGIN TRAN; INSERT dbo.t VALUES (2);",
        &mut sink,
    );
    sink.0.clear();
    reset(
        &mut session,
        &engine,
        ResetConnection::SkipTransaction,
        &mut sink,
    );
    run_batch(
        &mut session,
        "SELECT COUNT(*) FROM dbo.t; SELECT @@TRANCOUNT;",
        &mut sink,
    );
    let rows: Vec<i32> = sink
        .0
        .iter()
        .filter_map(|event| match event {
            Event::Row(Value::I32(v)) => Some(*v),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        vec![1, 1],
        "skip reset keeps the open transaction: {:?}",
        sink.0
    );
}

fn dotnet_available() -> bool {
    Command::new("dotnet")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "network integration with Microsoft.Data.SqlClient; needs dotnet"]
async fn sqlclient_pool_reset_returns_login_database() {
    if !dotnet_available() {
        eprintln!("skipped: no dotnet on PATH");
        return;
    }
    vauban_sysfn::register_builtins();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    let shutdown = CancellationToken::new();
    let server = Server::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        ServerConfig {
            encrypt: vauban_tds::EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(NoAuth),
            server_name: "vauban-test".into(),
            default_packet_size: 4096,
            program_name: None,
            version_banner: None,
            edition: None,
        },
    );
    let task = tokio::spawn(server.serve(listener, shutdown.clone()));
    let source = r#"
using Microsoft.Data.SqlClient;

var builder = new SqlConnectionStringBuilder {
    DataSource = $"127.0.0.1,{Environment.GetEnvironmentVariable("VAUBAN_TEST_PORT")}",
    UserID = "sa",
    Password = Environment.GetEnvironmentVariable("VAUBAN_SA_PASSWORD") ?? "ignored",
    InitialCatalog = "master",
    TrustServerCertificate = true,
    Encrypt = false,
    Pooling = true,
    MinPoolSize = 1,
    MaxPoolSize = 1,
};
await using (var conn = new SqlConnection(builder.ConnectionString)) {
    await conn.OpenAsync();
    await using (var cmd = conn.CreateCommand()) {
        cmd.CommandText = "CREATE DATABASE pooldb";
        try { await cmd.ExecuteNonQueryAsync(); } catch { }
    }
    await using (var cmd = conn.CreateCommand()) {
        cmd.CommandText = "USE pooldb";
        await cmd.ExecuteNonQueryAsync();
    }
}
await using (var conn = new SqlConnection(builder.ConnectionString)) {
    await conn.OpenAsync();
    await using (var cmd = conn.CreateCommand()) {
        cmd.CommandText = "SELECT DB_NAME()";
        Console.WriteLine(await cmd.ExecuteScalarAsync());
    }
}
"#;
    let out_dir = std::env::temp_dir().join("vauban-sqlclient-reset-test");
    let _ = std::fs::create_dir_all(&out_dir);
    std::fs::write(out_dir.join("Program.cs"), source).expect("write probe");
    std::fs::write(
        out_dir.join("PoolReset.csproj"),
        r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Exe</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Microsoft.Data.SqlClient" Version="5.2.2" />
  </ItemGroup>
</Project>"#,
    )
    .expect("write csproj");
    let run = spawn_blocking(move || {
        Command::new("dotnet")
            .arg("run")
            .arg("--project")
            .arg(out_dir.join("PoolReset.csproj"))
            .env("VAUBAN_TEST_PORT", port.to_string())
            .env("VAUBAN_SA_PASSWORD", "ignored")
            .output()
            .expect("dotnet run")
    })
    .await
    .expect("dotnet task");
    assert!(
        run.status.success(),
        "dotnet failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.lines().any(|line| line.trim() == "master"),
        "pool reopen reads the login database: {stdout}"
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(6), task)
        .await
        .expect("serve returns")
        .expect("serve task")
        .expect("serve ok");
}

mod wire {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;
    use vauban_session::{Engine, NoAuth, Server, ServerConfig};
    use vauban_storage::MemoryStorage;

    const PACKET_SQL_BATCH: u8 = 0x01;
    const PACKET_TABULAR_RESULT: u8 = 0x04;
    const PACKET_LOGIN7: u8 = 0x10;
    const PACKET_PRELOGIN: u8 = 0x12;
    const STATUS_EOM: u8 = 0x01;
    const STATUS_RESET: u8 = 0x08;
    const HEADER_TRANSACTION_DESCRIPTOR: u16 = 0x0002;
    const TOKEN_COLMETADATA: u8 = 0x81;
    const TOKEN_ERROR: u8 = 0xAA;
    const TOKEN_ROW: u8 = 0xD1;
    const TOKEN_DONE: u8 = 0xFD;
    const INTNTYPE: u8 = 0x26;
    const DEFAULT_PACKET_SIZE: u16 = 4096;
    const TDS_7_4: u32 = 0x7400_0004;
    const OPTION_VERSION: u8 = 0x00;
    const OPTION_ENCRYPTION: u8 = 0x01;
    const OPTION_TERMINATOR: u8 = 0xFF;
    const ENCRYPT_NOT_SUP: u8 = 0x02;

    #[derive(Debug, PartialEq, Eq)]
    enum Tok {
        Row(i64),
        Error(u32),
    }

    fn packet(kind: u8, status: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind, status];
        out.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        out.extend_from_slice(&[0x00, 0x00, 0x01, 0x00]);
        out.extend_from_slice(payload);
        out
    }

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn all_headers(descriptor: u64) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(22);
        bytes.extend_from_slice(&22u32.to_le_bytes());
        bytes.extend_from_slice(&18u32.to_le_bytes());
        bytes.extend_from_slice(&HEADER_TRANSACTION_DESCRIPTOR.to_le_bytes());
        bytes.extend_from_slice(&descriptor.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes
    }

    fn sql_batch_packet(descriptor: u64, text: &str, reset: bool) -> Vec<u8> {
        let mut payload = all_headers(descriptor);
        payload.extend_from_slice(&utf16le(text));
        let status = if reset {
            STATUS_EOM | STATUS_RESET
        } else {
            STATUS_EOM
        };
        packet(PACKET_SQL_BATCH, status, &payload)
    }

    fn prelogin_packet() -> Vec<u8> {
        let version: [u8; 6] = [0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00];
        let data_start = (2 * 5 + 1) as u16;
        let mut payload = Vec::new();
        payload.push(OPTION_VERSION);
        payload.extend_from_slice(&data_start.to_be_bytes());
        payload.extend_from_slice(&(version.len() as u16).to_be_bytes());
        payload.push(OPTION_ENCRYPTION);
        payload.extend_from_slice(&(data_start + version.len() as u16).to_be_bytes());
        payload.extend_from_slice(&1u16.to_be_bytes());
        payload.push(OPTION_TERMINATOR);
        payload.extend_from_slice(&version);
        payload.push(ENCRYPT_NOT_SUP);
        packet(PACKET_PRELOGIN, STATUS_EOM, &payload)
    }

    fn login7_packet() -> Vec<u8> {
        const FIXED_LEN: usize = 94;
        let mut data = Vec::new();
        let mut pairs = Vec::new();
        for field in [
            "testhost",
            "sa",
            "",
            "vauban-server-test",
            "localhost",
            "",
            "hand-made",
            "",
            "",
        ] {
            pairs.push(((FIXED_LEN + data.len()) as u16, field.len() as u16));
            data.extend_from_slice(&utf16le(field));
        }
        let mut fixed = Vec::with_capacity(FIXED_LEN);
        fixed.extend_from_slice(&((FIXED_LEN + data.len()) as u32).to_le_bytes());
        fixed.extend_from_slice(&TDS_7_4.to_le_bytes());
        fixed.extend_from_slice(&u32::from(DEFAULT_PACKET_SIZE).to_le_bytes());
        fixed.extend_from_slice(&0u32.to_le_bytes());
        fixed.extend_from_slice(&4242u32.to_le_bytes());
        fixed.extend_from_slice(&0u32.to_le_bytes());
        fixed.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        fixed.extend_from_slice(&0i32.to_le_bytes());
        fixed.extend_from_slice(&0x0409u32.to_le_bytes());
        for (ib, cch) in &pairs {
            fixed.extend_from_slice(&ib.to_le_bytes());
            fixed.extend_from_slice(&cch.to_le_bytes());
        }
        fixed.extend_from_slice(&[0u8; 6]);
        fixed.extend_from_slice(&[0u8; 4]);
        fixed.extend_from_slice(&[0u8; 4]);
        fixed.extend_from_slice(&[0u8; 4]);
        fixed.extend_from_slice(&0u32.to_le_bytes());
        fixed.extend_from_slice(&data);
        packet(PACKET_LOGIN7, STATUS_EOM, &fixed)
    }

    async fn read_packet(client: &mut TcpStream) -> Vec<u8> {
        let mut header = [0u8; 8];
        client.read_exact(&mut header).await.unwrap();
        let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let mut out = header.to_vec();
        out.resize(len, 0);
        client.read_exact(&mut out[8..]).await.unwrap();
        out
    }

    async fn read_response(client: &mut TcpStream, budget: Duration) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + budget;
        let mut payload = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let packet = timeout(left, read_packet(client))
                .await
                .expect("response within budget");
            assert_eq!(packet[0], PACKET_TABULAR_RESULT);
            payload.extend_from_slice(&packet[8..]);
            if packet[1] & STATUS_EOM == STATUS_EOM {
                return payload;
            }
        }
    }

    fn u16_at(bytes: &[u8], pos: usize) -> u16 {
        u16::from_le_bytes([bytes[pos], bytes[pos + 1]])
    }

    fn tokens(payload: &[u8]) -> Vec<Tok> {
        let mut pos = 0;
        let mut out = Vec::new();
        let mut columns: Vec<u8> = Vec::new();
        while pos < payload.len() {
            match payload[pos] {
                TOKEN_COLMETADATA => {
                    pos += 1;
                    let count = usize::from(u16_at(payload, pos));
                    pos += 2;
                    columns.clear();
                    for _ in 0..count {
                        pos += 4 + 2;
                        let ty = payload[pos];
                        pos += 1;
                        if ty == INTNTYPE {
                            pos += 1;
                        }
                        columns.push(ty);
                        let chars = usize::from(payload[pos]);
                        pos += 1 + 2 * chars;
                    }
                }
                TOKEN_ROW => {
                    pos += 1;
                    let mut value = None;
                    for ty in &columns {
                        if *ty == INTNTYPE {
                            let len = usize::from(payload[pos]);
                            pos += 1;
                            if len == 0 {
                                continue;
                            }
                        }
                        value = Some(i64::from(i32::from_le_bytes(
                            payload[pos..pos + 4].try_into().unwrap(),
                        )));
                        pos += 4;
                    }
                    if let Some(v) = value {
                        out.push(Tok::Row(v));
                    }
                }
                TOKEN_ERROR => {
                    pos += 1;
                    let number = u32::from_le_bytes(payload[pos + 2..pos + 6].try_into().unwrap());
                    pos += 2 + usize::from(u16_at(payload, pos));
                    out.push(Tok::Error(number));
                }
                TOKEN_DONE => pos += 13,
                0xAB | 0xAD | 0xE3 => {
                    pos += 1;
                    let len = usize::from(u16_at(payload, pos));
                    pos += 2 + len;
                }
                other => panic!("unexpected token 0x{other:02X} at {pos}"),
            }
        }
        out
    }

    async fn connect_and_login(addr: SocketAddr) -> TcpStream {
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(&prelogin_packet()).await.unwrap();
        let prelogin = read_packet(&mut client).await;
        assert_eq!(prelogin[0], PACKET_TABULAR_RESULT);
        client.write_all(&login7_packet()).await.unwrap();
        read_response(&mut client, Duration::from_secs(5)).await;
        client
    }

    async fn run_batch(
        client: &mut TcpStream,
        descriptor: u64,
        text: &str,
        reset: bool,
    ) -> Vec<Tok> {
        client
            .write_all(&sql_batch_packet(descriptor, text, reset))
            .await
            .unwrap();
        tokens(&read_response(client, Duration::from_secs(5)).await)
    }

    fn first_int(tokens: &[Tok]) -> Option<i64> {
        tokens.iter().find_map(|tok| match tok {
            Tok::Row(v) => Some(*v),
            _ => None,
        })
    }

    pub(super) async fn full_reset_zero_descriptor_rolls_back() {
        vauban_sysfn::register_builtins();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let server = Server::new(
            Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
            ServerConfig {
                encrypt: vauban_tds::EncryptPolicy::Off,
                tls: None,
                authenticator: Arc::new(NoAuth),
                server_name: "vauban-test".into(),
                default_packet_size: DEFAULT_PACKET_SIZE,
                program_name: None,
                version_banner: None,
                edition: None,
            },
        );
        let task = tokio::spawn(server.serve(listener, shutdown.clone()));
        let mut client = connect_and_login(addr).await;
        run_batch(&mut client, 0, "CREATE TABLE dbo.wt(c int);", false).await;
        run_batch(
            &mut client,
            0,
            "BEGIN TRAN; INSERT dbo.wt VALUES (1);",
            false,
        )
        .await;
        let reset = run_batch(&mut client, 0, "SELECT 1;", true).await;
        assert!(
            !reset.iter().any(|tok| matches!(tok, Tok::Error(3989))),
            "full reset with descriptor 0 must not refuse 3989: {reset:?}"
        );
        let count = run_batch(&mut client, 0, "SELECT COUNT(*) FROM dbo.wt;", false).await;
        assert_eq!(
            first_int(&count),
            Some(0),
            "full reset rolls the insert back: {count:?}"
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(6), task)
            .await
            .expect("serve returns")
            .expect("serve task")
            .expect("serve ok");
    }
}

#[tokio::test]
async fn full_reset_with_zero_descriptor_rolls_back_open_transaction() {
    wire::full_reset_zero_descriptor_rolls_back().await;
}
