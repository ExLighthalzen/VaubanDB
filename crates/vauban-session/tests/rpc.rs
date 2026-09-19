//! Integration tests of `Session::run_rpc` and procedure token order.

use std::sync::Arc;
use std::time::Duration;

use std::sync::Once;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::spawn_blocking;
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{
    Engine, ProcAction, ProcArg, ProcParam, ResultSink, Session, SessionState, TdsSink,
    register_system_procedure_resolver,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{
    ClientMessage, ColumnMeta, DoneStatus, EncryptPolicy, EnvChange, ResetConnection, Rpc,
    RpcParam, RpcProc, TdsStream, Token,
};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

const SPID: i16 = 71;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns,
    Row,
    DoneInProc,
    DoneProc(Option<u64>),
    ReturnValue(String),
    ReturnStatus(i32),
    Error(u32),
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
        self.0.push(Event::Columns);
        Ok(())
    }
    fn row(&mut self, _row: &[Value]) -> SqlResult<()> {
        self.0.push(Event::Row);
        Ok(())
    }
    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }
    fn done_in_proc(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        self.0.push(Event::DoneInProc);
        Ok(())
    }
    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.0.push(Event::DoneProc(rowcount));
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.number));
        Ok(())
    }
    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        Ok(())
    }
    fn return_value(&mut self, name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        self.0.push(Event::ReturnValue(name.to_owned()));
        Ok(())
    }
    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.0.push(Event::ReturnStatus(status));
        Ok(())
    }
}

fn register_test_resolver() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        register_system_procedure_resolver(|name, args| {
            if !name.eq_ignore_ascii_case("sp_executesql")
                && !name.to_ascii_lowercase().ends_with(".sp_executesql")
            {
                return None;
            }
            Some(Ok(ProcAction::ExecuteSql {
                statement: stmt_arg(args).unwrap_or_else(|| "SELECT @a + 1 AS n".into()),
                params: bound_params(args),
            }))
        });
    });
}

fn stmt_arg(args: &[ProcArg<'_>]) -> Option<String> {
    args.iter().find_map(|arg| {
        let named = matches!(
            arg.name,
            None | Some("@statement") | Some("@stmt") | Some("stmt")
        );
        if !named {
            return None;
        }
        match arg.value {
            Value::String(text) => Some(text.text.clone()),
            _ => None,
        }
    })
}

fn bound_params(args: &[ProcArg<'_>]) -> Vec<ProcParam> {
    args.iter()
        .filter(|arg| match arg.name {
            Some(name)
                if name.starts_with('@')
                    && name != "@statement"
                    && name != "@stmt"
                    && name != "@params" =>
            {
                true
            }
            Some("params") | Some("stmt") => false,
            Some(name) if name.starts_with('@') => true,
            None => false,
            _ => false,
        })
        .map(|arg| ProcParam {
            name: arg.name.expect("named parameter").to_owned(),
            ty: arg.ty.clone(),
            value: arg.value.clone(),
            output: arg.output,
        })
        .collect()
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    register_test_resolver();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(SPID),
    )
}

fn nvarchar(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

fn int(value: i32) -> Value {
    Value::I32(value)
}

fn sp_executesql_rpc(params: Vec<RpcParam>) -> Rpc {
    Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::SP_EXECUTESQL,
        options: 0,
        params,
        transaction_descriptor: 0,
    }
}

#[test]
fn output_parameter_comes_back_as_return_value() {
    let nvarchar_type = TypeInfo::new(SqlType::NVarChar(Len::Max), false);
    let int_type = TypeInfo::new(SqlType::Int, false);
    let rpc = sp_executesql_rpc(vec![
        RpcParam {
            name: "stmt".into(),
            output: false,
            default: false,
            ty: nvarchar_type.clone(),
            value: nvarchar("SELECT @a + 1 AS n"),
        },
        RpcParam {
            name: "params".into(),
            output: false,
            default: false,
            ty: nvarchar_type.clone(),
            value: nvarchar("@a int, @o int OUTPUT"),
        },
        RpcParam {
            name: "@a".into(),
            output: false,
            default: false,
            ty: int_type.clone(),
            value: int(41),
        },
        RpcParam {
            name: "@o".into(),
            output: true,
            default: false,
            ty: int_type,
            value: Value::Null,
        },
    ]);

    let mut sink = Recording::default();
    session().run_rpc(&rpc, &mut sink).expect("rpc completes");
    assert_eq!(
        sink.0,
        vec![
            Event::Columns,
            Event::Row,
            Event::DoneInProc,
            Event::ReturnValue("@o".into()),
            Event::ReturnStatus(0),
            Event::DoneProc(None),
        ]
    );
}

#[test]
fn executesql_insert_reports_done_proc_count() {
    let mut session = session();
    session
        .run_batch(
            "CREATE TABLE dbo.rpc_dml_ins (id int NOT NULL)",
            &mut Recording::default(),
        )
        .expect("create table");

    let rpc = sp_executesql_rpc(vec![RpcParam {
        name: "stmt".into(),
        output: false,
        default: false,
        ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
        value: nvarchar("INSERT INTO dbo.rpc_dml_ins (id) VALUES (1)"),
    }]);

    let mut sink = Recording::default();
    session.run_rpc(&rpc, &mut sink).expect("rpc completes");
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::DoneProc(Some(1)))),
        "expected done_proc(Some(1)), got {:?}",
        sink.0
    );
}

#[test]
fn unknown_rpc_name_gets_2812_then_doneproc() {
    let rpc = Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name("dbo.p".into()),
        options: 0,
        params: Vec::new(),
        transaction_descriptor: 0,
    };
    let mut sink = Recording::default();
    session().run_rpc(&rpc, &mut sink).expect("rpc completes");
    assert_eq!(sink.0, vec![Event::Error(2812), Event::DoneProc(None)]);
}

fn minimal_login_tokens() -> Vec<Token> {
    vec![
        Token::EnvChange(EnvChange::Database {
            old: "master".into(),
            new: "master".into(),
        }),
        Token::EnvChange(EnvChange::PacketSize {
            old: 4096,
            new: 4096,
        }),
        Token::LoginAck {
            tds_version: 0x7400_0004,
            program_name: "VaubanDB".into(),
            version: [0x10, 0x00, 0x03, 0xE8],
        },
        Token::Done {
            status: DoneStatus::FINAL,
            cur_cmd: 0,
            row_count: None,
        },
    ]
}

async fn serve_tiberius_parameterized_query(listener: TcpListener) {
    let (tcp, _) = listener.accept().await.unwrap();
    let mut stream = TdsStream::accept(tcp, None, EncryptPolicy::Off)
        .await
        .expect("handshake");
    match stream.read_message().await.expect("login read") {
        ClientMessage::Login7(_) => {}
        other => panic!("unexpected pre-login message: {other:?}"),
    }
    stream.write_tokens(&minimal_login_tokens()).await.unwrap();
    stream.flush().await.unwrap();

    let (reader, mut writer) = stream.split();
    let (msg_tx, mut msg_rx) = mpsc::channel(8);
    tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match reader.read_message().await {
                Ok(ClientMessage::Rpc(_)) => {
                    let _ = msg_tx.send(()).await;
                    break;
                }
                Ok(ClientMessage::Attention) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    msg_rx.recv().await.unwrap();

    let (token_tx, mut token_rx) = mpsc::channel(32);
    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
    let handle = spawn_blocking(move || {
        vauban_sysfn::register_builtins();
        register_test_resolver();
        let mut session = Session::new(engine, SessionState::new(SPID));
        let mut sink = TdsSink::new(token_tx);
        session
            .run_rpc(
                &sp_executesql_rpc(vec![
                    RpcParam {
                        name: "stmt".into(),
                        output: false,
                        default: false,
                        ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
                        value: nvarchar("SELECT @P1 + 1"),
                    },
                    RpcParam {
                        name: "params".into(),
                        output: false,
                        default: false,
                        ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
                        value: nvarchar("@P1 int"),
                    },
                    RpcParam {
                        name: "@P1".into(),
                        output: false,
                        default: false,
                        ty: TypeInfo::new(SqlType::Int, false),
                        value: int(41),
                    },
                ]),
                &mut sink,
            )
            .expect("rpc on wire");
    });

    while let Some(token) = token_rx.recv().await {
        writer.write_tokens(&[token]).await.unwrap();
    }
    writer.flush().await.unwrap();
    let _ = handle.await;
}

#[tokio::test]
#[ignore = "network integration test with a real tiberius client"]
async fn tiberius_parameterized_query_reads_sp_executesql_result() {
    use tiberius::{AuthMethod, Client, Config, EncryptionLevel};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_tiberius_parameterized_query(listener));

    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(addr.port());
    config.authentication(AuthMethod::sql_server("sa", "ignored"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();

    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut client = Client::connect(config, tcp.compat_write())
        .await
        .expect("tiberius connects");

    let row = timeout(
        Duration::from_secs(10),
        client.query("SELECT @P1 + 1", &[&41i32]),
    )
    .await
    .expect("query finishes")
    .expect("query succeeds")
    .into_row()
    .await
    .expect("row read")
    .expect("one row returned");
    assert_eq!(row.get::<i32, _>(0), Some(42));
}
