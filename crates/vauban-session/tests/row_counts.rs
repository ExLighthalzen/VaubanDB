//! The row count each statement closes with, under `SET NOCOUNT OFF` and `ON`: in the batch
//! (DONE), in a called level (DONEINPROC) and at the end of an RPC (DONEPROC).
//!
//! Under `SET NOCOUNT OFF`, `INSERT`, `UPDATE`, `DELETE` and `SELECT … INTO` carry the
//! rows they wrote, `SET @v`, `SELECT @v = …` and a `DECLARE` that gives a value carry
//! their own count, and the DDL, `TRUNCATE TABLE` and the transaction statements carry
//! none. A `DECLARE` that gives no variable a value sends no token. Under `SET NOCOUNT
//! ON`, the batch keeps a DONE per statement without its count, while a called level sends
//! a DONEINPROC for a result set or an error only. The token that closes a call carries
//! no count.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tokio_util::sync::CancellationToken;
use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{
    EncryptPolicy, Engine, NoAuth, ProcAction, ProcArg, ProcParam, ResultSink, Server,
    ServerConfig, Session, SessionState, SystemProcedureResolver, TdsSink,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange, ResetConnection, Rpc, RpcParam, RpcProc, Token};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

const SPID: i16 = 63;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns,
    Row,
    Done(Option<u64>, bool),
    DoneInProc(Option<u64>, bool),
    DoneProc(Option<u64>),
    Info,
    Error(u32),
    EnvChange,
    ReturnValue,
    ReturnStatus(i32),
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
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        self.0.push(Event::Info);
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.number));
        Ok(())
    }
    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        self.0.push(Event::EnvChange);
        Ok(())
    }
    fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        self.0.push(Event::ReturnValue);
        Ok(())
    }
    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.0.push(Event::ReturnStatus(status));
        Ok(())
    }
}

/// `sp_executesql` with its statement and, for a parameterized call, its named values.
fn resolver() -> Arc<SystemProcedureResolver> {
    fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>> {
        if !name.eq_ignore_ascii_case("sp_executesql") {
            return None;
        }
        let statement = args.first().and_then(|arg| match arg.value {
            Value::String(text) => Some(text.text.clone()),
            _ => None,
        })?;
        let params = args
            .iter()
            .skip(2)
            .filter_map(|arg| {
                arg.name.map(|name| ProcParam {
                    name: name.to_owned(),
                    ty: arg.ty.clone(),
                    value: arg.value.clone(),
                    output: arg.output,
                })
            })
            .collect();
        Some(Ok(ProcAction::ExecuteSql { statement, params }))
    }
    Arc::new(resolve)
}

fn engine() -> Arc<Engine> {
    vauban_sysfn::register_builtins();
    Arc::new(Engine::with_system_procedures(
        Arc::new(MemoryStorage::new()),
        resolver(),
    ))
}

fn session() -> Session {
    Session::new(engine(), SessionState::new(SPID))
}

fn run(session: &mut Session, text: &str) -> Vec<Event> {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("run_batch reports client errors through the sink");
    sink.0
}

/// The batch `text` on a session that already holds `dbo.t (a int)`.
fn run_on_t(text: &str) -> Vec<Event> {
    let mut session = session();
    run(&mut session, "CREATE TABLE dbo.t (a int)");
    run(&mut session, text)
}

/// The DONE, DONEINPROC and DONEPROC of `events`, in order.
fn dones(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Done(..) | Event::DoneInProc(..) | Event::DoneProc(_)
            )
        })
        .cloned()
        .collect()
}

#[test]
fn a_single_insert_closes_with_the_rows_it_wrote() {
    assert_eq!(
        run_on_t("INSERT dbo.t VALUES (1), (2);"),
        vec![Event::Done(Some(2), false)]
    );
}

#[test]
fn each_statement_of_a_mixed_batch_carries_its_count() {
    let events = run(
        &mut session(),
        "CREATE TABLE dbo.t (a int); INSERT dbo.t VALUES (1), (2); \
         UPDATE dbo.t SET a = a + 1; DELETE dbo.t WHERE a = 2; SELECT * FROM dbo.t; \
         DECLARE @x int; DECLARE @y int = 1; SET @x = 3; SELECT @x = a FROM dbo.t; \
         INSERT dbo.t SELECT a FROM dbo.t; INSERT dbo.t DEFAULT VALUES; \
         UPDATE dbo.t SET a = 0 WHERE 1 = 0;",
    );
    assert_eq!(
        dones(&events),
        vec![
            Event::Done(None, true),
            Event::Done(Some(2), true),
            Event::Done(Some(2), true),
            Event::Done(Some(1), true),
            Event::Done(Some(1), true),
            // `DECLARE @x int` sends nothing.
            Event::Done(Some(1), true),
            Event::Done(Some(1), true),
            Event::Done(Some(1), true),
            Event::Done(Some(1), true),
            Event::Done(Some(1), true),
            Event::Done(Some(0), false),
        ],
        "{events:?}"
    );
}

#[test]
fn nocount_on_clears_the_counts_of_the_batch_and_off_brings_them_back() {
    let events = run(
        &mut session(),
        "SET NOCOUNT ON; CREATE TABLE dbo.t (a int); INSERT dbo.t VALUES (1), (2); \
         UPDATE dbo.t SET a = a + 1; SELECT * FROM dbo.t; DECLARE @y int = 1; SET @y = 3; \
         SET NOCOUNT OFF; INSERT dbo.t VALUES (9);",
    );
    let mut expected = vec![Event::Done(None, true); 8];
    expected.push(Event::Done(Some(1), false));
    assert_eq!(dones(&events), expected, "{events:?}");
}

#[test]
fn select_assignments_and_set_carry_their_own_counts() {
    let events = run_on_t(
        "INSERT dbo.t VALUES (1), (2), (3); DECLARE @x int; SELECT @x = a FROM dbo.t; \
         SELECT @x = a FROM dbo.t WHERE 1 = 0; SET @x = (SELECT COUNT(*) FROM dbo.t); \
         PRINT 'p'; SELECT @@ROWCOUNT;",
    );
    assert_eq!(
        dones(&events),
        vec![
            Event::Done(Some(3), true),
            Event::Done(Some(3), true),
            Event::Done(Some(0), true),
            Event::Done(Some(1), true),
            Event::Done(None, true),
            Event::Done(Some(1), false),
        ],
        "{events:?}"
    );
}

#[test]
fn select_into_counts_and_truncate_and_transactions_do_not() {
    let events = run_on_t(
        "INSERT dbo.t VALUES (1); SELECT a INTO dbo.u FROM dbo.t; TRUNCATE TABLE dbo.t; \
         BEGIN TRAN; INSERT dbo.t SELECT a FROM dbo.u; COMMIT; \
         UPDATE dbo.t SET a = 2 WHERE a = 1; DELETE FROM dbo.t;",
    );
    assert_eq!(
        dones(&events),
        vec![
            Event::Done(Some(1), true),
            Event::Done(Some(1), true),
            Event::Done(None, true),
            Event::Done(None, true),
            Event::Done(Some(1), true),
            Event::Done(None, true),
            Event::Done(Some(1), true),
            Event::Done(Some(1), false),
        ],
        "{events:?}"
    );
}

#[test]
fn a_declare_without_a_value_sends_no_token() {
    assert_eq!(
        run(&mut session(), "DECLARE @x int; SET @x = 1; DECLARE @y int"),
        vec![Event::Done(Some(1), false)]
    );
    assert_eq!(
        run(&mut session(), "DECLARE @x int; SELECT 1"),
        vec![Event::Columns, Event::Row, Event::Done(Some(1), false)]
    );
    assert_eq!(
        run(&mut session(), "DECLARE @a int = 1, @b int"),
        vec![Event::Done(Some(1), false)]
    );
    // A batch with nothing else still ends with a DONE.
    assert_eq!(
        run(&mut session(), "DECLARE @x int"),
        vec![Event::Done(None, false)]
    );
}

#[test]
fn the_statements_of_a_dynamic_text_carry_their_counts_and_its_end_carries_none() {
    let events = run_on_t(
        "EXEC('INSERT dbo.t VALUES (1), (2); UPDATE dbo.t SET a = a + 1; \
         DELETE dbo.t WHERE a = 2; SELECT * FROM dbo.t; DECLARE @x int; \
         DECLARE @y int = 1; SET @x = 3; SELECT @x = a FROM dbo.t; \
         INSERT dbo.t DEFAULT VALUES'); SELECT @@ROWCOUNT",
    );
    assert_eq!(
        dones(&events),
        vec![
            Event::DoneInProc(Some(2), true),
            Event::DoneInProc(Some(2), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::Done(None, true),
            Event::Done(Some(1), false),
        ],
        "{events:?}"
    );
}

#[test]
fn nocount_on_in_a_dynamic_text_leaves_a_doneinproc_to_its_result_sets_only() {
    let events = run_on_t(
        "EXEC('SET NOCOUNT ON; INSERT dbo.t VALUES (1), (2); UPDATE dbo.t SET a = a + 1; \
         SELECT * FROM dbo.t; DECLARE @x int = 1; IF @x = 1 SET @x = 3; \
         WHILE @x < 5 SET @x += 1'); INSERT dbo.t VALUES (5)",
    );
    assert_eq!(
        events,
        vec![
            Event::Columns,
            Event::Row,
            Event::Row,
            Event::DoneInProc(None, true),
            Event::Done(None, true),
            // The option does not outlive its level.
            Event::Done(Some(1), false),
        ]
    );
}

#[test]
fn nocount_on_of_the_batch_reaches_the_dynamic_text_until_it_turns_it_off() {
    let events = run_on_t(
        "SET NOCOUNT ON; EXEC('INSERT dbo.t VALUES (1); SELECT 1; SET NOCOUNT OFF; \
         INSERT dbo.t VALUES (2)'); INSERT dbo.t VALUES (3)",
    );
    assert_eq!(
        dones(&events),
        vec![
            Event::Done(None, true),
            Event::DoneInProc(None, true),
            Event::DoneInProc(None, true),
            Event::DoneInProc(Some(1), true),
            Event::Done(None, true),
            Event::Done(None, false),
        ],
        "{events:?}"
    );
}

#[test]
fn under_nocount_on_a_call_closes_without_a_token_unless_an_error_ended_its_level() {
    // 8134 ends its statement, which closes with a DONEINPROC of its own: the call that
    // ran it closes with nothing.
    assert_eq!(
        run(
            &mut session(),
            "EXEC('SET NOCOUNT ON; EXEC(''SELECT 1/0''); SELECT 2')"
        ),
        vec![
            Event::Columns,
            Event::Error(8134),
            Event::DoneInProc(None, true),
            Event::Columns,
            Event::Row,
            Event::DoneInProc(None, true),
            Event::Done(None, false),
        ]
    );
    // 208 ends the level of the inner text: the end of the call is the DONEINPROC that
    // follows the error.
    assert_eq!(
        run(
            &mut session(),
            "EXEC('SET NOCOUNT ON; EXEC(''SELECT * FROM dbo.nosuch''); SELECT 2')"
        ),
        vec![
            Event::Error(208),
            Event::DoneInProc(None, true),
            Event::Columns,
            Event::Row,
            Event::DoneInProc(None, true),
            Event::Done(None, false),
        ]
    );
    // Under `SET NOCOUNT OFF` the call that completed closes with a DONEINPROC of its own.
    assert_eq!(
        dones(&run_on_t(
            "EXEC('EXEC(''INSERT dbo.t VALUES (1), (2); SET NOCOUNT ON; \
             INSERT dbo.t VALUES (3)''); INSERT dbo.t VALUES (4)')"
        )),
        vec![
            Event::DoneInProc(Some(2), true),
            Event::DoneInProc(None, true),
            Event::DoneInProc(Some(1), true),
            Event::Done(None, false),
        ]
    );
}

#[test]
fn sp_executesql_in_a_batch_counts_its_statements() {
    let events = run_on_t(
        "EXEC sp_executesql N'INSERT dbo.t VALUES (1), (2); UPDATE dbo.t SET a = a + 1; \
         DELETE dbo.t WHERE a = 2; DECLARE @x int = 1; SET @x = 2; SELECT * FROM dbo.t'",
    );
    assert_eq!(
        dones(&events),
        vec![
            Event::DoneInProc(Some(2), true),
            Event::DoneInProc(Some(2), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::Done(None, false),
        ],
        "{events:?}"
    );
}

fn nvarchar(text: &str) -> RpcParam {
    RpcParam {
        name: String::new(),
        output: false,
        default: false,
        ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
        value: Value::String(SqlString {
            text: text.to_owned(),
        }),
    }
}

fn rpc_executesql(session: &mut Session, text: &str) -> Vec<Event> {
    let rpc = Rpc {
        reset: ResetConnection::None,
        proc: RpcProc::SP_EXECUTESQL,
        options: 0,
        params: vec![nvarchar(text)],
        transaction_descriptor: 0,
    };
    let mut sink = Recording::default();
    session.run_rpc(&rpc, &mut sink).expect("rpc completes");
    sink.0
}

#[test]
fn an_rpc_counts_its_statements_and_closes_with_a_doneproc_without_a_count() {
    let mut session = session();
    run(&mut session, "CREATE TABLE dbo.t (a int)");
    assert_eq!(
        rpc_executesql(
            &mut session,
            "INSERT dbo.t VALUES (1), (2); UPDATE dbo.t SET a = a + 1; DECLARE @x int; \
             DECLARE @y int = 1; SET @y = 2; DELETE dbo.t WHERE a = 2",
        ),
        vec![
            Event::DoneInProc(Some(2), true),
            Event::DoneInProc(Some(2), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::DoneInProc(Some(1), true),
            Event::ReturnStatus(0),
            Event::DoneProc(None),
        ]
    );
    // `SET NOCOUNT ON` in the text silences it, and ends with it.
    assert_eq!(
        rpc_executesql(
            &mut session,
            "SET NOCOUNT ON; INSERT dbo.t VALUES (1), (2); UPDATE dbo.t SET a = a + 1",
        ),
        vec![Event::ReturnStatus(0), Event::DoneProc(None)]
    );
    assert_eq!(
        run(&mut session, "INSERT dbo.t VALUES (7)"),
        vec![Event::Done(Some(1), false)]
    );
}

/// The DONE and DONEINPROC tokens `text` answers on a TCP-less TDS sink, after the batch
/// `setup`: `(in_proc, cur_cmd, row_count)` each.
fn done_tokens(setup: &str, text: &str) -> Vec<(bool, u16, Option<u64>)> {
    let mut session = session();
    run(&mut session, setup);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    let mut sink = TdsSink::new(tx);
    session.run_batch(text, &mut sink).expect("batch runs");
    drop(sink);
    let mut dones = Vec::new();
    while let Ok(token) = rx.try_recv() {
        match token {
            Token::Done {
                cur_cmd, row_count, ..
            } => dones.push((false, cur_cmd, row_count)),
            Token::DoneInProc {
                cur_cmd, row_count, ..
            } => dones.push((true, cur_cmd, row_count)),
            _ => {}
        }
    }
    dones
}

/// The rows a batch affected as a .NET driver adds them up for `ExecuteNonQuery`: the
/// counts of the DONE and DONEINPROC that carry one, except those whose `CurCmd` is
/// `SELECT` (0xC1), which count the rows of a result set or of an assignment; -1 when
/// none is left.
fn records_affected(dones: &[(bool, u16, Option<u64>)]) -> i64 {
    let counts: Vec<u64> = dones
        .iter()
        .filter(|(_, cur_cmd, _)| *cur_cmd != 0xC1)
        .filter_map(|(_, _, count)| *count)
        .collect();
    if counts.is_empty() {
        -1
    } else {
        counts
            .iter()
            .map(|count| i64::try_from(*count).unwrap())
            .sum()
    }
}

#[test]
fn the_done_of_each_statement_names_its_kind() {
    assert_eq!(
        done_tokens(
            "CREATE TABLE dbo.t (a int)",
            "INSERT dbo.t VALUES (1); SELECT a INTO dbo.u FROM dbo.t; \
             INSERT dbo.t SELECT a FROM dbo.t; DECLARE @x int = 1, @y int; SELECT @x = 5; \
             SET @x += 1; UPDATE dbo.t SET a = 0 WHERE 1 = 0; DELETE dbo.t WHERE 1 = 0; \
             SELECT 1; EXEC('UPDATE dbo.t SET a = 2; DECLARE @z int; SET @z = 1; \
             DELETE dbo.t');",
        ),
        vec![
            (false, 0xC3, Some(1)),
            (false, 0xC2, Some(1)),
            (false, 0xC3, Some(1)),
            (false, 0xC1, Some(1)),
            (false, 0xC1, Some(1)),
            (false, 0xC1, Some(1)),
            (false, 0xC5, Some(0)),
            (false, 0xC4, Some(0)),
            (false, 0xC1, Some(1)),
            (true, 0xC5, Some(2)),
            (true, 0xC1, Some(1)),
            (true, 0xC4, Some(2)),
            (false, 0, None),
        ]
    );
    // `SET NOCOUNT ON` takes the count away, not the kind.
    assert_eq!(
        done_tokens(
            "CREATE TABLE dbo.t (a int)",
            "SET NOCOUNT ON; INSERT dbo.t VALUES (1); UPDATE dbo.t SET a = 2; \
             DECLARE @x int = 1",
        ),
        vec![
            (false, 0, None),
            (false, 0xC3, None),
            (false, 0xC5, None),
            (false, 0xC1, None),
        ]
    );
}

#[test]
fn assignments_do_not_add_to_the_rows_a_batch_affected() {
    let setup = "CREATE TABLE dbo.t (a int, b int); INSERT dbo.t VALUES (1, 1), (2, 2), (3, 3)";
    for (text, expected) in [
        ("DECLARE @x int; SET @x = 1", -1),
        ("DECLARE @x int = 5", -1),
        ("DECLARE @x int; SELECT @x = a FROM dbo.t", -1),
        ("DECLARE @x int = 1; UPDATE dbo.t SET b = b WHERE a = @x", 1),
        (
            "DECLARE @v int; SET @v = 1; UPDATE dbo.t SET b = 7 WHERE a = 1 AND b = 1",
            1,
        ),
        ("EXEC('DECLARE @x int = 1; SET @x = 2')", -1),
        (
            "EXEC sp_executesql N'DECLARE @x int = 1; SET @x = 2; \
             UPDATE dbo.t SET b = 1 WHERE a = 1'",
            1,
        ),
        (
            "SET NOCOUNT ON; DECLARE @x int = 1; UPDATE dbo.t SET b = 1",
            -1,
        ),
        ("INSERT dbo.t VALUES (4, 4); UPDATE dbo.t SET b = 0", 5),
    ] {
        assert_eq!(
            records_affected(&done_tokens(setup, text)),
            expected,
            "{text}"
        );
    }
}

/// A server over a fresh in-memory engine, on a port of its own, stopped on drop.
struct Running {
    port: u16,
    shutdown: CancellationToken,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn start() -> Running {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local address").port();
    let shutdown = CancellationToken::new();
    let server = Server::new(
        engine(),
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(NoAuth),
            server_name: "vauban-test".into(),
            default_packet_size: 4096,
            program_name: None,
            version_banner: None,
            edition: None,
        },
    );
    tokio::spawn(server.serve(listener, shutdown.clone()));
    Running { port, shutdown }
}

#[tokio::test]
async fn a_driver_reads_the_rows_a_statement_affected() {
    use tiberius::{AuthMethod, Client, Config, EncryptionLevel};

    let running = start().await;
    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(running.port);
    config.authentication(AuthMethod::sql_server("sa", "ignored"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();
    let tcp = TcpStream::connect(("127.0.0.1", running.port))
        .await
        .expect("connect");
    let mut client = timeout(
        Duration::from_secs(10),
        Client::connect(config, tcp.compat_write()),
    )
    .await
    .expect("login finishes")
    .expect("tiberius connects");

    client
        .execute("CREATE TABLE dbo.t (a int)", &[])
        .await
        .expect("create");
    let inserted = client
        .execute("INSERT dbo.t VALUES (@P1), (@P2)", &[&1i32, &2i32])
        .await
        .expect("insert");
    assert_eq!(inserted.rows_affected(), &[2]);
    let updated = client
        .execute("UPDATE dbo.t SET a = a + 1 WHERE a > @P1", &[&0i32])
        .await
        .expect("update");
    assert_eq!(updated.rows_affected(), &[2]);
    let deleted = client
        .execute("DELETE dbo.t WHERE a = @P1", &[&3i32])
        .await
        .expect("delete");
    assert_eq!(deleted.rows_affected(), &[1]);
}
