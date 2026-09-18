//! Integration tests of `Session::run_nested`: parameter scope, procedure-style DONE
//! tokens, and output parameters.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::spawn_blocking;
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{
    Engine, NestedOutcome, NestedParam, ResultSink, Session, SessionState, TdsSink,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{
    ClientMessage, ColumnMeta, DoneStatus, EncryptPolicy, EnvChange, TdsStream, Token,
};
use vauban_types::{SqlType, TypeInfo, Value};

const SPID: i16 = 62;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns(Vec<ColumnMeta>),
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    DoneInProc(Option<u64>, bool),
    DoneProc(Option<u64>),
    Info(InfoMessage),
    Error(SqlError),
    EnvChange(EnvChange),
    ReturnValue(String, TypeInfo, Value),
    ReturnStatus(i32),
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.0.push(Event::Columns(cols.to_vec()));
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.0.push(Event::Row(row.to_vec()));
        Ok(())
    }
    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.0.push(Event::Done(rowcount, more));
        Ok(())
    }
    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.0.push(Event::DoneInProc(rowcount, more));
        Ok(())
    }
    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.0.push(Event::DoneProc(rowcount));
        Ok(())
    }
    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
        self.0.push(Event::Info(msg.clone()));
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.clone()));
        Ok(())
    }
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.0.push(Event::EnvChange(change.clone()));
        Ok(())
    }
    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.0.push(Event::ReturnValue(
            name.to_owned(),
            ty.clone(),
            value.clone(),
        ));
        Ok(())
    }
    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.0.push(Event::ReturnStatus(status));
        Ok(())
    }
}

#[derive(Default)]
struct TokenCounts {
    done: u32,
    done_in_proc: u32,
    done_proc: u32,
}

struct CountingSink {
    inner: Recording,
    counts: TokenCounts,
}

impl CountingSink {
    fn new() -> Self {
        Self {
            inner: Recording::default(),
            counts: TokenCounts::default(),
        }
    }
}

impl ResultSink for CountingSink {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.inner.columns(cols)
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.inner.row(row)
    }
    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.counts.done += 1;
        self.inner.done(rowcount, more)
    }
    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.counts.done_in_proc += 1;
        self.inner.done_in_proc(rowcount, more)
    }
    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.counts.done_proc += 1;
        self.inner.done_proc(rowcount)
    }
    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
        self.inner.info(msg)
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.inner.error(err)
    }
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.inner.env_change(change)
    }
    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.inner.return_value(name, ty, value)
    }
    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.inner.return_status(status)
    }
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(SPID),
    )
}

fn int_param(name: &str, value: i32) -> NestedParam {
    NestedParam {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, true),
        value: Value::I32(value),
        output: false,
    }
}

fn run_nested(
    session: &mut Session,
    text: &str,
    params: &[NestedParam],
) -> (NestedOutcome, Recording) {
    let mut sink = Recording::default();
    let outcome = session
        .run_nested(text, params, &mut sink)
        .expect("run_nested reports client errors through the sink");
    (outcome, sink)
}

fn only_row(events: &[Event]) -> i32 {
    events
        .iter()
        .find_map(|event| match event {
            Event::Row(row) => match row.first() {
                Some(Value::I32(n)) => Some(*n),
                _ => None,
            },
            _ => None,
        })
        .expect("one row with an int")
}

fn only_error(events: &[Event]) -> SqlError {
    events
        .iter()
        .find_map(|event| match event {
            Event::Error(err) => Some(err.clone()),
            _ => None,
        })
        .expect("one error")
}

#[test]
fn parameter_is_visible_in_the_text() {
    let (outcome, events) = run_nested(&mut session(), "SELECT @a + 1", &[int_param("@a", 41)]);
    assert!(!outcome.failed);
    assert_eq!(only_row(&events.0), 42);
}

#[test]
fn caller_variables_are_out_of_scope() {
    let mut session = session();
    session
        .run_batch("DECLARE @v int = 1", &mut Recording::default())
        .expect("declare runs");
    let mut sink = Recording::default();
    let outcome = session
        .run_nested("SELECT @v", &[], &mut sink)
        .expect("nested completes");
    assert!(outcome.failed);
    let err = only_error(&sink.0);
    assert_eq!(err.number, 137);
}

#[test]
fn output_parameter_is_written_back() {
    let (outcome, _) = run_nested(
        &mut session(),
        "SET @o = 7",
        &[NestedParam {
            name: "@o".into(),
            ty: TypeInfo::new(SqlType::Int, true),
            value: Value::Null,
            output: true,
        }],
    );
    assert!(!outcome.failed);
    assert_eq!(outcome.outputs, vec![("@o".to_owned(), Value::I32(7))]);
}

#[test]
fn three_statements_give_three_done_in_proc_and_one_done_proc() {
    let mut session = session();
    let mut sink = CountingSink::new();
    session
        .run_nested("SELECT 1; SELECT 2; SELECT 3", &[], &mut sink)
        .expect("nested completes");
    assert_eq!(sink.counts.done_in_proc, 3);
    assert_eq!(sink.counts.done_proc, 1);
    assert_eq!(sink.counts.done, 0);

    let mut batch_sink = CountingSink::new();
    session
        .run_batch("SELECT 1; SELECT 2; SELECT 3", &mut batch_sink)
        .expect("batch completes");
    assert_eq!(batch_sink.counts.done, 3);
    assert_eq!(batch_sink.counts.done_in_proc, 0);
    assert_eq!(batch_sink.counts.done_proc, 0);
}

#[test]
fn error_line_is_relative_to_the_nested_text() {
    let text = "SELECT 1\nSELECT 1/0";
    let mut sink = Recording::default();
    let outcome = session()
        .run_nested(text, &[], &mut sink)
        .expect("nested completes");
    assert!(outcome.failed);
    let err = only_error(&sink.0);
    assert_eq!(err.number, 8134);
    assert_eq!(err.line, 2);
}

#[test]
fn empty_text_emits_done_proc_and_return_status() {
    let mut sink = Recording::default();
    let outcome = session()
        .run_nested("", &[], &mut sink)
        .expect("empty nested completes");
    assert!(!outcome.failed);
    assert_eq!(outcome.return_status, 0);
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::ReturnStatus(0)))
    );
    assert_eq!(
        sink.0
            .iter()
            .filter(|event| matches!(event, Event::DoneInProc(_, _)))
            .count(),
        0
    );
    assert_eq!(
        sink.0
            .iter()
            .filter(|event| matches!(event, Event::DoneProc(_)))
            .count(),
        1
    );
}

#[test]
fn two_level_nesting_keeps_scopes_separate() {
    let mut session = session();
    let (outer, _) = run_nested(&mut session, "SELECT @a + 1", &[int_param("@a", 1)]);
    assert!(!outer.failed);

    let mut inner_sink = Recording::default();
    let inner = session
        .run_nested("SELECT @a + 1", &[int_param("@a", 2)], &mut inner_sink)
        .expect("inner nested completes");
    assert!(!inner.failed);
    assert_eq!(only_row(&inner_sink.0), 3);

    let mut missing_sink = Recording::default();
    let missing = session
        .run_nested("SELECT @a", &[], &mut missing_sink)
        .expect("missing param nested completes");
    assert!(missing.failed);
    assert_eq!(only_error(&missing_sink.0).number, 137);
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

async fn serve_one_nested_query(listener: TcpListener) {
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
                Ok(ClientMessage::SqlBatch(_)) => {
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
        let mut session = Session::new(engine, SessionState::new(SPID));
        let mut sink = TdsSink::new(token_tx);
        session
            .run_nested(
                "SELECT @a + 1",
                &[NestedParam {
                    name: "@a".into(),
                    ty: TypeInfo::new(SqlType::Int, true),
                    value: Value::I32(41),
                    output: false,
                }],
                &mut sink,
            )
            .expect("nested on wire");
    });

    while let Some(token) = token_rx.recv().await {
        writer.write_tokens(&[token]).await.unwrap();
    }
    writer.flush().await.unwrap();
    let _ = handle.await;
}

#[tokio::test]
#[ignore = "network integration test with a real tiberius client"]
async fn tiberius_reads_run_nested_doneinproc_and_doneproc() {
    use tiberius::{AuthMethod, Client, Config, EncryptionLevel};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_one_nested_query(listener));

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

    let row = timeout(Duration::from_secs(10), client.simple_query("SELECT 1"))
        .await
        .expect("query finishes")
        .expect("query succeeds")
        .into_row()
        .await
        .expect("row read")
        .expect("one row returned");
    assert_eq!(row.get::<i32, _>(0), Some(42));
}
