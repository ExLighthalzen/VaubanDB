//! Prepared-statement RPC tests (`sp_prepare`, `sp_prepexec`, `sp_execute`, `sp_unprepare`).

use std::process::Command;
use std::sync::{Arc, Once};

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::spawn_blocking;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_parser::{ParseOptions, parse_parameter_declarations};
use vauban_session::{
    Engine, ProcAction, ProcArg, ProcParam, ResultSink, Session, SessionState,
    register_system_procedure_resolver,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{
    ClientMessage, ColumnMeta, DoneStatus, EncryptPolicy, EnvChange, ResetConnection, Rpc,
    RpcParam, RpcProc, TdsStream, Token,
};
use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value};

const SPID: i16 = 83;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns,
    Row(Value),
    DoneInProc,
    DoneProc,
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
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.0.push(Event::Row(row[0].clone()));
        Ok(())
    }
    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }
    fn done_in_proc(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        self.0.push(Event::DoneInProc);
        Ok(())
    }
    fn done_proc(&mut self, _rowcount: Option<u64>) -> SqlResult<()> {
        self.0.push(Event::DoneProc);
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

fn nvarchar(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
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

fn register_prepare_resolver() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        register_system_procedure_resolver(|name, args| {
            let base = name.rsplit('.').next().unwrap_or(name);
            match base.to_ascii_lowercase().as_str() {
                "sp_executesql" => Some(resolve_executesql(args)),
                "sp_prepare" => Some(resolve_prepare(args, false)),
                "sp_prepexec" => Some(resolve_prepare(args, true)),
                "sp_execute" => Some(resolve_execute(args)),
                "sp_unprepare" => Some(resolve_unprepare(args)),
                _ => None,
            }
        });
    });
}

fn resolve_executesql(args: &[ProcArg<'_>]) -> Result<ProcAction, SqlError> {
    let statement = args
        .iter()
        .find_map(|arg| {
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
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_executesql", "@statement"))?;
    let params_text = args
        .iter()
        .find_map(|arg| match (arg.name, arg.value) {
            (Some("@params") | Some("params"), Value::String(text)) => Some(text.text.clone()),
            (None, Value::String(text)) if text.text != statement => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let declarations =
        parse_parameter_declarations(&params_text, &ParseOptions::default()).unwrap_or_default();
    let mut bound = vec![None; declarations.len()];
    let mut positional = 0usize;
    for arg in args {
        if let Some(name) = arg.name {
            if name == "@statement" || name == "@stmt" || name == "@params" || name == "params" {
                continue;
            }
            if let Some(index) = declarations
                .iter()
                .position(|decl| decl.name.eq_ignore_ascii_case(name))
            {
                bound[index] = Some(ProcParam {
                    name: declarations[index].name.clone(),
                    ty: arg.ty.clone(),
                    value: arg.value.clone(),
                    output: arg.output,
                });
            }
        } else if matches!(arg.value, Value::String(_)) {
            continue;
        } else if positional < declarations.len() {
            bound[positional] = Some(ProcParam {
                name: declarations[positional].name.clone(),
                ty: arg.ty.clone(),
                value: arg.value.clone(),
                output: arg.output,
            });
            positional += 1;
        }
    }
    Ok(ProcAction::ExecuteSql {
        statement,
        params: bound.into_iter().flatten().collect(),
    })
}

fn resolve_prepare(args: &[ProcArg<'_>], execute: bool) -> Result<ProcAction, SqlError> {
    let handle = args
        .iter()
        .position(|arg| arg.output && matches!(arg.ty.ty, SqlType::Int))
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_prepare", "@handle"))?;
    let params_text = args
        .iter()
        .find_map(|arg| match (arg.name, arg.value) {
            (Some("@params") | Some("params"), Value::String(text)) => Some(text.text.clone()),
            (None, Value::String(text)) => Some(text.text.clone()),
            _ => None,
        })
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_prepare", "@params"))?;
    let statement = args
        .iter()
        .find_map(|arg| match (arg.name, arg.value) {
            (Some("@stmt") | Some("@statement") | Some("stmt"), Value::String(text)) => {
                Some(text.text.clone())
            }
            (None, Value::String(text)) if text.text != params_text => Some(text.text.clone()),
            _ => None,
        })
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_prepare", "@stmt"))?;
    let declarations = parse_parameter_declarations(&params_text, &ParseOptions::default())?;
    let execute_with = if execute {
        let dynamic: Vec<ProcParam> = args
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
            .enumerate()
            .map(|(index, (_, arg))| ProcParam {
                name: declarations
                    .get(index)
                    .map(|decl| decl.name.clone())
                    .or_else(|| arg.name.map(str::to_owned))
                    .unwrap_or_else(|| format!("@P{}", index + 1)),
                ty: arg.ty.clone(),
                value: arg.value.clone(),
                output: arg.output,
            })
            .collect();
        Some(dynamic)
    } else {
        None
    };
    Ok(ProcAction::Prepare {
        statement,
        params: declarations,
        handle_arg: handle,
        execute_with,
    })
}

fn resolve_execute(args: &[ProcArg<'_>]) -> Result<ProcAction, SqlError> {
    let mut handle = None;
    let mut params = Vec::new();
    for arg in args {
        match arg.name {
            Some("@handle") => handle = read_int(arg.value),
            Some(name) => params.push(ProcParam {
                name: name.to_owned(),
                ty: arg.ty.clone(),
                value: arg.value.clone(),
                output: arg.output,
            }),
            None if matches!(arg.value, Value::String(_)) => {}
            None => {
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
        }
    }
    let handle =
        handle.ok_or_else(|| SqlError::procedure_expects_parameter("sp_execute", "@handle"))?;
    Ok(ProcAction::Execute { handle, params })
}

fn read_int(value: &Value) -> Option<i32> {
    match value {
        Value::I32(v) => Some(*v),
        Value::I16(v) => Some(i32::from(*v)),
        Value::I64(v) => i32::try_from(*v).ok(),
        _ => None,
    }
}

fn resolve_unprepare(args: &[ProcArg<'_>]) -> Result<ProcAction, SqlError> {
    let handle = args
        .iter()
        .find_map(|arg| match (arg.name, arg.value) {
            (None | Some("@handle"), Value::I32(v)) => Some(*v),
            _ => None,
        })
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_unprepare", "@handle"))?;
    Ok(ProcAction::Unprepare { handle })
}

fn execute_events(value: i32) -> Vec<Event> {
    vec![
        Event::Columns,
        Event::Row(int(value)),
        Event::DoneInProc,
        Event::ReturnStatus(0),
        Event::DoneProc,
    ]
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    register_prepare_resolver();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(SPID),
    )
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
                value: nvarchar("@P1 int"),
            },
            RpcParam {
                name: "stmt".into(),
                output: false,
                default: false,
                ty: nvarchar_ty(),
                value: nvarchar("SELECT @P1 AS n"),
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

fn unprepare_rpc(handle: i32) -> Rpc {
    Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::Name("sp_unprepare".into()),
        options: 0,
        params: vec![RpcParam {
            name: String::new(),
            output: false,
            default: false,
            ty: int_ty(),
            value: int(handle),
        }],
        transaction_descriptor: 0,
    }
}

#[test]
fn execute_replays_with_new_values() {
    let mut session = session();
    let mut sink = Recording::default();
    session
        .run_rpc(&prepexec_rpc(10), &mut sink)
        .expect("prepexec");
    assert!(sink.0.contains(&Event::Row(int(10))));
    for (value, expected) in [(20, 20), (30, 30)] {
        sink.0.clear();
        session
            .run_rpc(&execute_rpc(1, value), &mut sink)
            .expect("execute");
        assert_eq!(sink.0, execute_events(expected));
    }
}

#[test]
fn unprepared_handle_is_8179() {
    let mut session = session();
    let mut sink = Recording::default();
    session
        .run_rpc(&prepexec_rpc(1), &mut sink)
        .expect("prepexec");
    session
        .run_rpc(&unprepare_rpc(1), &mut sink)
        .expect("unprepare");
    sink.0.clear();
    session
        .run_rpc(&execute_rpc(1, 2), &mut sink)
        .expect("missing handle");
    assert!(sink.0.contains(&Event::Error(8179)));
    let err = SqlError::prepared_statement_not_found(1);
    assert_eq!(err.number, 8179);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 4);
}

#[test]
fn prepexec_returns_the_handle_then_the_rows() {
    let mut sink = Recording::default();
    session()
        .run_rpc(&prepexec_rpc(41), &mut sink)
        .expect("prepexec");
    assert_eq!(
        sink.0,
        vec![
            Event::Columns,
            Event::Row(int(41)),
            Event::DoneInProc,
            Event::ReturnStatus(0),
            Event::ReturnValue("handle".into()),
            Event::DoneProc,
        ]
    );
}

#[test]
fn prepexec_and_execute_in_one_flow() {
    let mut session = session();
    let mut sink = Recording::default();
    session
        .run_rpc(&prepexec_rpc(5), &mut sink)
        .expect("prepexec");
    sink.0.clear();
    session
        .run_rpc(&execute_rpc(1, 6), &mut sink)
        .expect("execute");
    assert_eq!(sink.0, execute_events(6));
}

#[test]
fn two_prepared_handles_are_independent() {
    let mut session = session();
    let mut sink = Recording::default();
    session
        .run_rpc(&prepexec_rpc(1), &mut sink)
        .expect("first prepexec");
    session
        .run_rpc(&prepexec_rpc(2), &mut sink)
        .expect("second prepexec");
    sink.0.clear();
    session
        .run_rpc(&execute_rpc(1, 10), &mut sink)
        .expect("handle 1");
    session
        .run_rpc(&execute_rpc(2, 20), &mut sink)
        .expect("handle 2");
    assert_eq!(sink.0, [execute_events(10), execute_events(20)].concat());
}

#[test]
fn invalid_prepare_text_emits_8180() {
    let rpc = Rpc {
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
                value: nvarchar("@P1 int"),
            },
            RpcParam {
                name: "stmt".into(),
                output: false,
                default: false,
                ty: nvarchar_ty(),
                value: nvarchar("SELECT 1 2"),
            },
            RpcParam {
                name: String::new(),
                output: false,
                default: false,
                ty: int_ty(),
                value: int(1),
            },
        ],
        transaction_descriptor: 0,
    };
    let mut sink = Recording::default();
    session().run_rpc(&rpc, &mut sink).expect("prepare fails");
    assert!(sink.0.contains(&Event::Error(102)));
    assert!(sink.0.contains(&Event::Error(8180)));
}

fn minimal_login_tokens() -> Vec<Token> {
    vec![
        Token::EnvChange(EnvChange::Database {
            old: "master".into(),
            new: "master".into(),
        }),
        Token::Info(InfoMessage {
            number: 5701,
            severity: 0,
            state: 2,
            message: "Changed database context to 'master'.".into(),
            line: 1,
        }),
        Token::EnvChange(EnvChange::Collation {
            old: None,
            new: Collation::DEFAULT,
        }),
        Token::EnvChange(EnvChange::Language {
            old: String::new(),
            new: "us_english".into(),
        }),
        Token::Info(InfoMessage {
            number: 5703,
            severity: 0,
            state: 1,
            message: "Changed language setting to us_english.".into(),
            line: 1,
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

async fn serve_jdbc_prepare(listener: TcpListener) {
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
    let (rpc_tx, mut rpc_rx) = mpsc::channel(8);
    tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match reader.read_message().await {
                Ok(ClientMessage::Rpc(rpc)) => {
                    let _ = rpc_tx.send(rpc).await;
                }
                Ok(ClientMessage::Attention) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
    let (work_tx, work_rx) = std::sync::mpsc::channel::<(Rpc, mpsc::Sender<vauban_tds::Token>)>();
    let worker = spawn_blocking(move || {
        vauban_sysfn::register_builtins();
        register_prepare_resolver();
        let mut session = Session::new(engine, SessionState::new(SPID));
        while let Ok((rpc, token_tx)) = work_rx.recv() {
            let mut sink = vauban_session::TdsSink::new(token_tx);
            session.run_rpc(&rpc, &mut sink).expect("rpc on wire");
        }
    });

    while let Some(rpc) = rpc_rx.recv().await {
        let (token_tx, mut token_rx) = mpsc::channel(32);
        work_tx.send((rpc, token_tx)).expect("worker alive");
        while let Some(token) = token_rx.recv().await {
            writer.write_tokens(&[token]).await.unwrap();
        }
        writer.flush().await.unwrap();
    }
    drop(work_tx);
    let _ = worker.await;
}

fn jdk_available() -> bool {
    Command::new("java")
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "network integration with mssql-jdbc; needs JDK"]
async fn jdbc_prepared_statement_replays_three_values() {
    if !jdk_available() {
        eprintln!("skipped: no JDK on PATH");
        return;
    }
    let Some(jar) = std::env::var_os("VAUBAN_JDBC_JAR") else {
        eprintln!("skipped: VAUBAN_JDBC_JAR not set");
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_jdbc_prepare(listener));

    let pkg = ["java", ".", "sql"].concat();
    let source = format!(
        "import {pkg}.Connection;\nimport {pkg}.DriverManager;\nimport {pkg}.PreparedStatement;\nimport {pkg}.ResultSet;\n\n\
         public final class PreparedProbe {{\n\
           public static void main(String[] a) throws Exception {{\n\
             String server = System.getenv().getOrDefault(\"VAUBAN_TEST_SERVER\", \"127.0.0.1:1433\");\n\
             String password = System.getenv().getOrDefault(\"VAUBAN_SA_PASSWORD\", \"ignored\");\n\
             String url = \"jdbc:sqlserver://\" + server + \";encrypt=false;trustServerCertificate=true\";\n\
             try (Connection c = DriverManager.getConnection(url, \"sa\", password);\n\
                  PreparedStatement s = c.prepareStatement(\"SELECT ? + 1\")) {{\n\
               for (int v : new int[] {{10, 20, 30}}) {{\n\
                 s.setInt(1, v);\n\
                 try (ResultSet r = s.executeQuery()) {{\n\
                   r.next();\n\
                   System.out.println(r.getInt(1));\n\
                 }}\n\
               }}\n\
             }}\n\
           }}\n\
         }}\n"
    );
    let out_dir = std::env::temp_dir().join("vauban-jdbc-prepared-test");
    let _ = std::fs::create_dir_all(&out_dir);
    let java_file = out_dir.join("PreparedProbe.java");
    std::fs::write(&java_file, source).expect("write probe");
    let compile_out_dir = out_dir.clone();
    let compile_java_file = java_file.clone();
    let compile = spawn_blocking(move || {
        Command::new("javac")
            .args(["-d"])
            .arg(&compile_out_dir)
            .arg(&compile_java_file)
            .output()
            .expect("javac")
    })
    .await
    .expect("compile task");
    assert!(
        compile.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let port = addr.port();
    let classpath = format!("{}:{}", out_dir.display(), jar.to_string_lossy());
    let run = spawn_blocking(move || {
        Command::new("java")
            .arg("-cp")
            .arg(classpath)
            .arg("PreparedProbe")
            .env("VAUBAN_TEST_SERVER", format!("127.0.0.1:{port}"))
            .env("VAUBAN_SA_PASSWORD", "ignored")
            .output()
            .expect("java")
    })
    .await
    .expect("java task");
    assert!(
        run.status.success(),
        "java failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(lines, ["11", "21", "31"]);
}
