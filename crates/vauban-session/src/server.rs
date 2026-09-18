//! `Server`: TCP acceptance loop, one tokio task per connection, graceful shutdown, and
//! the `Engine` the sessions share.
//!
//! # Connection life cycle ([MS-TDS] 3.3.5)
//!
//! 1. `serve` accepts a socket, assigns a SPID and spawns a task under the span
//!    `connection{spid, peer}`.
//! 2. The task runs `TdsStream::accept`: PRELOGIN exchange and, if negotiated, the TLS
//!    handshake ([MS-TDS] 2.2.6.5, 3.3.5.1).
//! 3. The next client message must be the LOGIN7 ([MS-TDS] 3.3.5.2); anything else closes
//!    the connection. The login is then checked in this order: TDS version (older
//!    than 7.2 closes without a response), SSPI request (18452), `Authenticator` (18456),
//!    requested database (4060 then 18456 when the catalogue holds no database of that
//!    name). A refusal is answered by the errors and a DONE `ERROR`, then the
//!    connection is closed.
//! 4. Whatever the verdict, the stream drops TLS before the response when only the login
//!    was encrypted ([MS-TDS] 3.3.5): the login response *and* the refusal both go out in
//!    clear text. On success the SPID also goes into the packet headers, the response is
//!    sent at the previous packet size, and the negotiated size applies from then on.
//! 5. The stream is then split (`TdsStream::split`): a reader task owns the read half and
//!    pushes every message into a bounded channel, this task owns the write half. Client
//!    messages are served until the peer leaves. A SQL_BATCH or an RPC moves the `Session`
//!    onto the blocking pool, where it runs synchronously and pushes its tokens into a
//!    bounded channel; this task drains the channel into the writer and ends the response
//!    with one `flush`, while still watching the client channel for an ATTENTION.
//!    TRANSACTION_MANAGER runs through the same blocking request path and shares the
//!    session transaction with T-SQL batches.
//!
//! # ATTENTION and the reader task ([MS-TDS] 2.2.1.7)
//!
//! A client that gives up on a request sends an ATTENTION and waits for its
//! acknowledgement — a lone DONE carrying `DoneStatus::ATTN` — before reusing the
//! connection. Reading it therefore has to happen *while* a response is being written,
//! which a single `TdsStream` cannot do (one `&mut` for both halves, and `read_message`
//! is not cancellation-safe inside a `select!`): hence the reader task.
//!
//! Invariants:
//! - one request at a time per connection (no MARS): any client message other than an
//!   ATTENTION received while a request runs is a protocol violation and closes the
//!   connection;
//! - the reader task never runs more than [`CLIENT_CHANNEL_CAPACITY`] messages ahead of
//!   this task, and stops as soon as a read fails or the channel is closed;
//! - the reader task is aborted when the connection ends ([`AbortOnDrop`]), otherwise it
//!   would keep the read half — and the socket — alive;
//! - the tokens already written before an ATTENTION stay on the wire (SQL Server does the
//!   same); only those still in the channel are dropped, unread, before the DONE `ATTN`;
//! - the cancelled request is never reported to the client: its result, `Ok` or the
//!   internal "cancelled" error, is dropped with the DONE `ATTN` sent in its place.
//!
//! # A token that cannot be encoded is not a broken socket
//!
//! `write_tokens` fails for two unrelated reasons, told apart by
//! [`TdsError::is_encoding_failure`]. When the transport is gone there is nobody left to
//! talk to: the request is cancelled, the tokens it still had are dropped unread and the
//! connection closes. When the codec refuses a value, the socket is untouched and the
//! client is still waiting for an answer: the response then ends with an ERROR and a DONE
//! carrying `DoneStatus::ERROR` ([`report_unencodable`]), and the connection serves the
//! next request. The COLMETADATA and the rows already written stay where they are, the
//! error coming after them ([MS-TDS] 2.2.7.9).
//!
//! No client error reaches `serve`: a `TdsError` is logged at `warn` and closes its own
//! connection only. The password of a LOGIN7 is never logged: only named fields of the
//! login appear in the journal, never the whole message.

use std::sync::Arc;
use std::sync::atomic::{AtomicI16, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet, spawn_blocking};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span, warn};
use vauban_catalog::Catalog;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::Storage;
use vauban_tds::{
    ClientMessage, DoneStatus, EncryptPolicy, Login7, TdsError, TdsReader, TdsStream, TdsWriter,
    Token,
};
use vauban_txn::TransactionManager;

use crate::auth::Authenticator;
use crate::batch::Session;
use crate::cancel::CancelHandle;
use crate::disconnect::{Release, release};
use crate::login::{self, EDITION, MASTER, VERSION_BANNER};
use crate::sink::{CHANNEL_CAPACITY, ResultSink, TdsSink};
use crate::state::SessionState;
use crate::txn_request;

/// First SPID handed out: SQL Server reserves the lower values for system tasks.
const SPID_FIRST: i16 = 51;
/// Last SPID before the counter goes back to [`SPID_FIRST`] (`@@SPID` is a `smallint`).
const SPID_MAX: i16 = i16::MAX;
/// How long `serve` waits for open connections after `shutdown` is cancelled.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Pause after a failed `accept` (out of descriptors, aborted handshake) before retrying,
/// so that a persistent failure does not spin the loop.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Capacity of the channel between the reader task and the connection task: how far ahead
/// of the served request the reader may go. Small on purpose (a client sends at most an
/// ATTENTION while it waits), and never zero: `mpsc::channel` forbids it.
const CLIENT_CHANNEL_CAPACITY: usize = 8;
/// `CurCmd` of the DONE that closes a TRANSACTION_MANAGER response.
const CUR_CMD_TRANSACTION_MANAGER: u16 = 0x00FD;

/// Stack of each thread of the tokio runtime that serves a request.
///
/// `session` runs a batch on the blocking pool (`spawn_blocking`, [`run_request`]), where
/// the parse, the binding and the execution recurse over one tree on one thread. A stack
/// overflow in Rust aborts the **process**, so the depth guards of `parser`
/// (`MAX_NESTING_DEPTH`) and of `binder` (`MAX_BIND_DEPTH`) are set against the stack this
/// constant fixes: narrowing it without lowering those two guards moves the abort back
/// within reach.
///
/// `cli` hands this number to `Builder::thread_stack_size`; tokio applies it to its
/// workers and to its blocking pool alike.
///
/// The cost is address space first, resident memory second: a thread reserves its stack and
/// touches the pages it uses. The blocking pool of tokio is capped at 512 threads, so the
/// reservation it can reach is 512 x 16 MiB = 8 GiB of virtual size; 100 simultaneous
/// connections reserve 100 x 16 MiB = 1.6 GiB while their requests run.
pub const REQUEST_THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;
/// What the sessions share: the storage engine, the transaction manager and the
/// catalogue.
///
/// `session` does not construct a storage: `cli` chooses `MemoryStorage` and
/// hands it over. The three fields are shared behind `Arc`s because a session keeps the
/// engine alive while it runs a statement (the binder takes a snapshot through them).
///
/// [`Engine::new`] builds the [`TransactionManager`] over the storage and runs
/// [`Catalog::bootstrap`], so that a fresh instance holds its system databases before the
/// first login.
pub struct Engine {
    /// The storage engine behind every table.
    pub storage: Arc<dyn Storage>,
    /// The transaction manager a session begins its transactions with.
    pub txn: Arc<TransactionManager>,
    /// The metadata of the instance, bootstrapped by [`Engine::new`].
    pub catalog: Arc<Catalog>,
}

impl Engine {
    /// Builds the engine over the storage the caller chose: a [`TransactionManager`] on
    /// that storage, then [`Catalog::bootstrap`], which creates the system databases when
    /// they are not there yet and finds them when they are.
    ///
    /// The signature stays infallible so that `cli` keeps writing `Engine::new(storage)`.
    /// Two engines may be built over one storage: the second bootstrap
    /// creates nothing (unit test
    /// `engine_new_is_idempotent_enough_for_two_engines_on_same_storage`).
    ///
    /// # Panics
    ///
    /// When the bootstrap fails — a storage error, or a transaction the manager refuses to
    /// commit. An instance without a catalogue serves nothing, so this is a failure of
    /// start-up, where a panic is allowed; the message carries the
    /// [`InternalError`] the catalogue returned (unit test
    /// `engine_new_panics_with_the_bootstrap_error_when_the_storage_refuses`).
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = match Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)) {
            Ok(catalog) => Arc::new(catalog),
            Err(err) => panic!("{BOOTSTRAP_PANIC}: {err}"),
        };
        Self {
            storage,
            txn,
            catalog,
        }
    }
}

/// Head of the panic message of [`Engine::new`] when the bootstrap fails.
///
/// A constant so that the test that provokes the failure asserts on the same text the
/// binary prints (unit test
/// `engine_new_panics_with_the_bootstrap_error_when_the_storage_refuses`).
const BOOTSTRAP_PANIC: &str = "catalogue bootstrap failed at start-up";

/// Per-server settings, shared by every connection.
pub struct ServerConfig {
    /// Encryption policy applied at PRELOGIN. `Copy`: passed by
    /// value to each `TdsStream::accept`.
    pub encrypt: EncryptPolicy,
    /// TLS configuration; required as soon as `encrypt` may lead to encryption.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Checks SQL logins.
    pub authenticator: Arc<dyn Authenticator>,
    /// Server name announced at login (`@@SERVERNAME`).
    pub server_name: String,
    /// Packet size granted when the client requests no size.
    pub default_packet_size: u16,
    /// LOGINACK `ProgName`. `None` uses [`crate::PROGRAM_NAME`].
    pub program_name: Option<String>,
    /// `@@VERSION` text. `None` uses [`crate::VERSION_BANNER`].
    pub version_banner: Option<String>,
    /// `SERVERPROPERTY('Edition')`. `None` uses [`crate::EDITION`].
    pub edition: Option<String>,
}

/// The TCP front of the engine: accepts connections and runs one task per connection.
pub struct Server {
    engine: Arc<Engine>,
    cfg: Arc<ServerConfig>,
    /// Next SPID to hand out; see [`Server::next_spid`].
    next_spid: AtomicI16,
}

impl Server {
    /// Builds a server; nothing is bound until [`Server::serve`].
    pub fn new(engine: Arc<Engine>, cfg: ServerConfig) -> Self {
        Self {
            engine,
            cfg: Arc::new(cfg),
            next_spid: AtomicI16::new(SPID_FIRST),
        }
    }

    /// Accepts connections on `listener` until `shutdown` is cancelled, then stops
    /// accepting, cancels every connection (child token), waits at most five seconds for
    /// them to finish and returns `Ok(())`.
    ///
    /// A failed `accept` is logged and retried after a short pause; a client error never
    /// ends the loop.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: CancellationToken,
    ) -> Result<(), InternalError> {
        let connections = shutdown.child_token();
        let mut tasks = JoinSet::new();

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((tcp, addr)) => {
                        let spid = self.next_spid();
                        let span = info_span!("connection", spid, peer = %addr);
                        let task = handle_connection(
                            Arc::clone(&self.engine),
                            Arc::clone(&self.cfg),
                            tcp,
                            spid,
                            connections.child_token(),
                        );
                        tasks.spawn(task.instrument(span));
                    }
                    Err(err) => {
                        warn!(error = %err, "accept failed");
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                },
                // Reap finished tasks so that the set does not grow with every connection.
                Some(finished) = tasks.join_next(), if !tasks.is_empty() => {
                    log_task_end(finished);
                }
            }
        }

        info!("shutdown requested, closing connections");
        connections.cancel();
        let drain = async {
            while let Some(finished) = tasks.join_next().await {
                log_task_end(finished);
            }
        };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            warn!(
                remaining = tasks.len(),
                "connections still open after the shutdown grace period, aborting them"
            );
            tasks.abort_all();
        }
        Ok(())
    }

    /// Hands out the next SPID: 51, 52, … 32767, then 51 again.
    fn next_spid(&self) -> i16 {
        self.next_spid
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(if current == SPID_MAX {
                    SPID_FIRST
                } else {
                    current + 1
                })
            })
            .unwrap_or_else(|previous| previous)
    }
}

/// Logs the outcome of a finished connection task; a panic is reported, not propagated.
fn log_task_end(finished: Result<(), tokio::task::JoinError>) {
    if let Err(err) = finished
        && err.is_panic()
    {
        warn!(error = %err, "connection task panicked");
    }
}

/// One connection: runs the TDS dialogue until the client leaves, an error occurs or the
/// server shuts down. Dropping the `TdsStream` closes the socket.
async fn handle_connection(
    engine: Arc<Engine>,
    cfg: Arc<ServerConfig>,
    tcp: TcpStream,
    spid: i16,
    shutdown: CancellationToken,
) {
    let outcome = tokio::select! {
        _ = shutdown.cancelled() => {
            info!("connection closed by server shutdown");
            return;
        }
        outcome = run_connection(engine, &cfg, tcp, spid) => outcome,
    };
    match outcome {
        Ok(()) => info!("connection closed"),
        // The peer leaving is the normal end of a connection, not a fault.
        Err(TdsError::ConnectionClosed) => info!("connection closed by peer"),
        Err(err) => warn!(error = %err, "connection closed on error"),
    }
}

/// PRELOGIN (and TLS) through `TdsStream::accept`, the LOGIN7 and its response, then the
/// client messages until the peer leaves.
async fn run_connection(
    engine: Arc<Engine>,
    cfg: &ServerConfig,
    tcp: TcpStream,
    spid: i16,
) -> Result<(), TdsError> {
    let mut stream = TdsStream::accept(tcp, cfg.tls.clone(), cfg.encrypt).await?;
    info!("PRELOGIN negotiated");

    // [MS-TDS] 3.3.5.2: the message that follows the PRELOGIN exchange is the LOGIN7.
    let login = match stream.read_message().await? {
        ClientMessage::Login7(login) => login,
        other => {
            warn!(
                message = message_kind(&other),
                "expected a LOGIN7 after PRELOGIN, closing"
            );
            return Ok(());
        }
    };

    // The verdict is taken first, the response written afterwards: `downgrade_encryption`
    // sits between the two so that it covers the refusal as much as the acceptance.
    let checked = match check_login(&login, &engine, cfg, spid) {
        Ok(state) => Ok(state),
        Err(LoginRefused::Silently) => return Ok(()),
        Err(LoginRefused::With(errors)) => Err(errors),
    };
    if checked.is_ok() {
        // Refusals keep the SPID 0 of the packets they arrived with: an accepted
        // login is what puts one in the packet headers.
        stream.set_spid(spid);
    }
    // Unconditional: a no-op unless only the login was encrypted (ENCRYPT_OFF), in which
    // case each response to the LOGIN7 goes in clear text ([MS-TDS] 3.3.5; tiberius
    // expects it so). A refusal written before the
    // downgrade reaches a client already back in clear text as TLS records, which it reads
    // as a truncated connection instead of the error.
    let mut stream = stream.downgrade_encryption().await;

    let state = match checked {
        Ok(state) => state,
        Err(errors) => {
            stream
                .write_tokens(&login::failure_response(errors))
                .await?;
            return stream.flush().await;
        }
    };

    stream
        .write_tokens(&login::login_response(&login, &state, cfg))
        .await?;
    stream.flush().await?;
    // The response above still travelled at the previous size; the negotiated one
    // applies from the next packet ([MS-TDS] 2.2.6.4 PacketSize).
    stream.set_packet_size(state.packet_size);
    info!(
        login = %state.login,
        app_name = %state.app_name,
        hostname = %state.hostname,
        tds_version = %format!("0x{:08X}", login.tds_version),
        packet_size = state.packet_size,
        "login succeeded"
    );

    // From here on the halves are independent: the reader task feeds `client_rx` while
    // this task writes the responses (module documentation, ATTENTION).
    let (reader, mut writer) = stream.split();
    let (client_tx, mut client_rx) = mpsc::channel(CLIENT_CHANNEL_CAPACITY);
    let _reader = AbortOnDrop(tokio::spawn(
        read_messages(reader, client_tx).in_current_span(),
    ));

    // The session lives here and is moved onto the blocking pool for each request, then
    // taken back: no mutex on the path of a request.
    let transaction_engine = Arc::clone(&engine);
    let mut session = Session::new(engine, state);
    loop {
        // `None`: the reader task is gone, which only happens once its message reached us.
        let Some(message) = client_rx.recv().await else {
            let _ = release(&mut session, Release::Disconnect);
            return Ok(());
        };
        match message? {
            ClientMessage::SqlBatch(batch) => {
                let descriptor = batch.transaction_descriptor;
                let text = batch.text;
                let outcome = run_request(
                    &mut writer,
                    &mut client_rx,
                    session,
                    false,
                    None,
                    move |session, sink| {
                        let mut state = session.state().clone();
                        if !txn_request::reject_mismatched_descriptor(descriptor, &mut state, sink)?
                        {
                            session.replace_state(state);
                            return Ok(());
                        }
                        session.run_batch(&text, sink)
                    },
                )
                .await?;
                match outcome {
                    RequestOutcome::Served(back) => session = back,
                    RequestOutcome::Close => return Ok(()),
                }
            }
            ClientMessage::Rpc(rpc) => {
                let descriptor = rpc.transaction_descriptor;
                let outcome = run_request(
                    &mut writer,
                    &mut client_rx,
                    session,
                    true,
                    None,
                    move |session, sink| {
                        let mut state = session.state().clone();
                        if !txn_request::reject_mismatched_descriptor(descriptor, &mut state, sink)?
                        {
                            session.replace_state(state);
                            return Ok(());
                        }
                        session.run_rpc(&rpc, sink)
                    },
                )
                .await?;
                match outcome {
                    RequestOutcome::Served(back) => session = back,
                    RequestOutcome::Close => return Ok(()),
                }
            }
            // No request is running here: nothing to cancel, but the acknowledgement is
            // owed all the same ([MS-TDS] 2.2.1.7).
            ClientMessage::Attention => {
                info!("ATTENTION received outside a request, acknowledged");
                let _ = release(&mut session, Release::Attention);
                acknowledge_attention(&mut writer).await?;
            }
            ClientMessage::TransactionManager(request) => {
                let engine = Arc::clone(&transaction_engine);
                let outcome = run_request(
                    &mut writer,
                    &mut client_rx,
                    session,
                    false,
                    Some(CUR_CMD_TRANSACTION_MANAGER),
                    move |session, sink| {
                        let mut state = session.state().clone();
                        let result = txn_request::handle(&request, &engine, &mut state, sink);
                        session.replace_state(state);
                        result
                    },
                )
                .await?;
                match outcome {
                    RequestOutcome::Served(back) => session = back,
                    RequestOutcome::Close => return Ok(()),
                }
            }
            other @ (ClientMessage::Login7(_) | ClientMessage::Unsupported(_)) => {
                warn!(
                    message = message_kind(&other),
                    packet_type = ?other,
                    "unexpected client message after login, closing"
                );
                return Ok(());
            }
        }
    }
}

/// Reads client messages and forwards them, in order, to the connection task.
///
/// Stops after forwarding a failed read (a closed connection is one of them) and when the
/// receiver is gone. The channel bound is the whole flow control: the task blocks on
/// `send` instead of reading ahead.
async fn read_messages(mut reader: TdsReader, tx: mpsc::Sender<Result<ClientMessage, TdsError>>) {
    loop {
        let message = reader.read_message().await;
        let failed = message.is_err();
        if tx.send(message).await.is_err() || failed {
            return;
        }
    }
}

/// Aborts the reader task when the connection ends, whatever the reason: a normal return,
/// an error, or this future being dropped by the shutdown token. Without it the task would
/// stay blocked on `read_message`, holding the read half and thus the socket open.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What became of the connection after one request.
///
/// `Session` holds the resolved product-identity strings; boxing it would scatter
/// every match site for a clippy threshold.
#[allow(clippy::large_enum_variant)]
enum RequestOutcome {
    /// The response is on the wire (or the ATTENTION was acknowledged): the session comes
    /// back for the next request.
    Served(Session),
    /// The connection must close: protocol violation, panic of the request task, or the
    /// client left.
    Close,
}

/// How the relay of a response ended.
enum Relayed {
    /// The blocking task dropped its sender: every token was written.
    Complete,
    /// An ATTENTION arrived: the request is cancelled, the remaining tokens are dropped
    /// and the DONE `ATTN` is owed to the client.
    Attention,
    /// The codec refused a token of the response: the socket is intact, so the client is
    /// owed an ERROR and a DONE carrying `ERROR` and the connection stays open. Carries the
    /// refusal for the log and for the message.
    Unencodable(TdsError),
    /// The connection must close, with the error to report if there is one (a write
    /// failure, a failed read); `None` for a protocol violation, already logged.
    Aborted(Option<TdsError>),
}

/// Runs one request on the blocking pool, relays its tokens to the client and watches the
/// client channel for an ATTENTION.
///
/// The `TdsSink` and its `Sender` live in the blocking closure, so `rx` yields `None`
/// exactly when the request is over. The session comes back with the result: `Ok(())`
/// ends the response with a `flush`; an `Err` (internal error, response incomplete) is
/// sent as an ERROR token and a DONE `ERROR` before the `flush`. An ATTENTION replaces
/// the whole answer with a lone DONE `ATTN`, once the blocking task has handed the
/// session back — a pool thread cannot be killed, it is asked to stop and awaited. A token
/// the codec refused ends the response the same way as an internal error, by
/// [`report_unencodable`], and the connection is kept.
async fn run_request<F>(
    writer: &mut TdsWriter,
    client_rx: &mut mpsc::Receiver<Result<ClientMessage, TdsError>>,
    mut session: Session,
    rpc: bool,
    done_cur_cmd: Option<u16>,
    run: F,
) -> Result<RequestOutcome, TdsError>
where
    F: FnOnce(&mut Session, &mut dyn ResultSink) -> SqlResult<()> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Token>(CHANNEL_CAPACITY);
    // A handle outlives the requests of its session: an ATTENTION cancels this one only.
    let cancel = session.cancel_handle();
    cancel.reset();
    let handle = spawn_blocking(move || {
        let mut sink = if rpc {
            TdsSink::for_rpc(tx)
        } else {
            TdsSink::new(tx)
        };
        let result = run(&mut session, &mut sink);
        (session, result)
    });

    let relayed = relay(writer, &mut rx, client_rx, &cancel, done_cur_cmd).await;
    // The relay left the channel empty and closed: whatever happens next, no `blocking_send`
    // is holding a pool thread and the request task is on its way out.
    drop(rx);
    let (mut session, result) = match handle.await {
        Ok(outcome) => outcome,
        Err(err) => {
            error!(error = %err, "request task panicked, closing the connection");
            return Ok(RequestOutcome::Close);
        }
    };

    match relayed {
        Relayed::Complete => {}
        // The result of the cancelled request is dropped here: `Ok` or the internal
        // "cancelled" error, the client only sees the acknowledgement.
        Relayed::Attention => {
            let _ = release(&mut session, Release::Attention);
            acknowledge_attention(writer).await?;
            return Ok(RequestOutcome::Served(session));
        }
        // The result of the request is dropped here as well: the relay cancelled it to let
        // the pool thread go, and what the client is owed is the refusal, not the
        // "cancelled" error that unwound the blocking task.
        Relayed::Unencodable(err) => {
            report_unencodable(writer, &err).await?;
            return Ok(RequestOutcome::Served(session));
        }
        Relayed::Aborted(Some(err)) => return Err(err),
        Relayed::Aborted(None) => return Ok(RequestOutcome::Close),
    }

    match result {
        Ok(()) => writer.flush().await?,
        Err(err) => {
            warn!(
                error = err.number,
                message = %err.message,
                "internal error during the request, response incomplete"
            );
            writer
                .write_tokens(&[
                    Token::Error(err),
                    Token::Done {
                        status: DoneStatus::ERROR,
                        cur_cmd: 0,
                        row_count: None,
                    },
                ])
                .await?;
            writer.flush().await?;
        }
    }
    Ok(RequestOutcome::Served(session))
}

/// Writes every token of the channel to the client until the sender is dropped, while
/// serving the client channel: an ATTENTION cancels the request, anything else ends the
/// connection (no MARS, see the module documentation).
///
/// Both `recv` are cancellation-safe, which the `select!` requires. Every exit but
/// `Complete` cancels the request and drains the token channel **without writing**: the
/// blocking task must be able to finish, and what it still had to say is dropped.
async fn relay(
    writer: &mut TdsWriter,
    rx: &mut mpsc::Receiver<Token>,
    client_rx: &mut mpsc::Receiver<Result<ClientMessage, TdsError>>,
    cancel: &CancelHandle,
    done_cur_cmd: Option<u16>,
) -> Relayed {
    loop {
        tokio::select! {
            token = rx.recv() => match token {
                Some(mut token) => {
                    if let (
                        Some(cur_cmd_override),
                        Token::Done {
                            status, cur_cmd, ..
                        },
                    ) = (done_cur_cmd, &mut token)
                        && !status.contains(DoneStatus::ERROR)
                    {
                        *cur_cmd = cur_cmd_override;
                    }
                    if let Err(err) = writer.write_tokens(&[token]).await {
                        cancel.cancel();
                        drain(rx).await;
                        return after_write_failure(err);
                    }
                }
                None => return Relayed::Complete,
            },
            message = client_rx.recv() => {
                let aborted = match message {
                    Some(Ok(ClientMessage::Attention)) => {
                        info!("ATTENTION received during a request, cancelling it");
                        cancel.cancel();
                        drain(rx).await;
                        return Relayed::Attention;
                    }
                    Some(Ok(other)) => {
                        warn!(
                            message = message_kind(&other),
                            "client message during a request (no MARS), closing"
                        );
                        Relayed::Aborted(None)
                    }
                    Some(Err(err)) => Relayed::Aborted(Some(err)),
                    // The reader task ended without a message: it cannot, but closing is
                    // the safe reading of it.
                    None => Relayed::Aborted(None),
                };
                cancel.cancel();
                drain(rx).await;
                return aborted;
            }
        }
    }
}

/// What a failed `write_tokens` means for the connection, from the error alone
/// ([`TdsError::is_encoding_failure`]): a token the codec refused leaves a working socket
/// and a client waiting for an answer, while the rest is the transport going away.
///
/// The request has already been cancelled and its channel drained by the caller; what is
/// left to pick here is the ending (unit test
/// `a_refused_token_is_reported_while_a_broken_socket_closes`).
fn after_write_failure(err: TdsError) -> Relayed {
    if err.is_encoding_failure() {
        Relayed::Unencodable(err)
    } else {
        Relayed::Aborted(Some(err))
    }
}

/// Head of the message sent when a token of the response cannot be encoded.
///
/// The column is not named: the refusal of the codec does not say which value it looked at.
const UNENCODABLE: &str = "a value of the response cannot be sent to the client";

/// Ends a response whose next token the codec refused: an ERROR then a DONE carrying
/// `ERROR`, followed by the `flush` that closes the message.
///
/// Nothing of the refused token reached the buffer or the wire, so this lands right after
/// the COLMETADATA and the rows that did, which [MS-TDS] 2.2.7.9 allows. The number is the
/// one of any internal failure of the engine, 50000, since the client asked for a state the
/// engine cannot serve.
async fn report_unencodable(writer: &mut TdsWriter, err: &TdsError) -> Result<(), TdsError> {
    warn!(
        error = %err,
        "a token of the response cannot be encoded, answering an error instead of closing"
    );
    let reported = SqlError::from(InternalError::Bug(format!("{UNENCODABLE}: {err}")));
    writer
        .write_tokens(&[
            Token::Error(reported),
            Token::Done {
                status: DoneStatus::ERROR,
                cur_cmd: 0,
                row_count: None,
            },
        ])
        .await?;
    writer.flush().await
}

/// Drops every token the cancelled request still had to send, until it lets go of the
/// channel.
async fn drain(rx: &mut mpsc::Receiver<Token>) {
    while rx.recv().await.is_some() {}
}

/// Acknowledges an ATTENTION ([MS-TDS] 2.2.1.7): a lone DONE with `ATTN` and `FINAL`, no
/// count, `CurCmd` 0 ([MS-TDS] 2.2.7.6). The client waits for it before reusing the
/// connection.
async fn acknowledge_attention(writer: &mut TdsWriter) -> Result<(), TdsError> {
    writer
        .write_tokens(&[Token::Done {
            status: DoneStatus::ATTN | DoneStatus::FINAL,
            cur_cmd: 0,
            row_count: None,
        }])
        .await?;
    writer.flush().await
}

/// Why a LOGIN7 is refused: with a response (errors then DONE `ERROR`) or without one.
enum LoginRefused {
    /// TDS version older than 7.2: the client could not decode a response ([MS-TDS]
    /// 2.2.6.4 TDSVersion), the connection is simply closed.
    Silently,
    /// The errors to send, in order, before the DONE `ERROR` and the close.
    With(Vec<SqlError>),
}

/// Checks a LOGIN7 in the order of the module documentation and, when accepted, builds
/// the state of the session. Every refusal is logged at `warn` with the error number and
/// the detailed state; the state sent to the client is the generic one.
///
/// The database of the LOGIN7 is resolved against the catalogue of `engine`, through a
/// transaction of its own opened and closed here: a login is not a batch, and the
/// session must not start with a transaction already open.
fn check_login(
    login: &Login7,
    engine: &Engine,
    cfg: &ServerConfig,
    spid: i16,
) -> Result<SessionState, LoginRefused> {
    if login::negotiate_tds_version(login.tds_version).is_none() {
        warn!(
            tds_version = %format!("0x{:08X}", login.tds_version),
            login = %login.username,
            "TDS version older than 7.2, closing without a response"
        );
        return Err(LoginRefused::Silently);
    }

    // Integrated authentication is not offered.
    if login.sspi {
        let err = login::sspi_refused();
        warn!(
            error = err.number,
            state = err.state,
            login = %login.username,
            "integrated authentication requested, refused"
        );
        return Err(LoginRefused::With(vec![err]));
    }

    let principal = match cfg
        .authenticator
        .authenticate(&login.username, &login.password)
    {
        Ok(principal) => principal,
        Err(err) => {
            warn!(
                error = err.number,
                state = err.state,
                login = %login.username,
                "login refused"
            );
            return Err(LoginRefused::With(vec![login::for_client(err)]));
        }
    };

    let opened = match login.database.as_deref() {
        None | Some("") => MASTER.to_owned(),
        Some(database) => match resolve_login_database(engine, database) {
            Some(canonical) => canonical,
            None => {
                let cannot_open = SqlError::cannot_open_database(database);
                warn!(
                    error = cannot_open.number,
                    state = cannot_open.state,
                    login = %principal.login,
                    database,
                    "database requested at login does not exist"
                );
                return Err(LoginRefused::With(vec![
                    cannot_open,
                    SqlError::login_failed(&login.username),
                ]));
            }
        },
    };

    let mut state = SessionState::new(spid);
    state.database = opened;
    state.login = principal.login;
    state.app_name = login.app_name.clone();
    state.hostname = login.hostname.clone();
    state.packet_size = login::negotiate_packet_size(login.packet_size, cfg.default_packet_size);
    state.version_banner = cfg
        .version_banner
        .clone()
        .unwrap_or_else(|| VERSION_BANNER.to_owned());
    state.edition = cfg.edition.clone().unwrap_or_else(|| EDITION.to_owned());
    // `@@SERVERNAME` and `SERVERPROPERTY('ServerName')` both read this one field through
    // `SessionEvalContext::server_name`: the name the instance was started under
    // travels from here rather than from a constant of `eval_context.rs`.
    state.server_name = cfg.server_name.clone();
    Ok(state)
}

/// The database of a LOGIN7 as the **catalogue** spells it, `None` when no database of the
/// instance bears that name.
///
/// The comparison and the spelling both come from
/// [`CatalogSnapshot::database`](vauban_catalog::CatalogSnapshot::database), which folds the
/// case under the default collation: `MASTER` opens `master`, and a login on
/// `VAUBAN_mixed` opens a database created as `Vauban_Mixed`. The name the session keeps
/// is the catalogue's and not the client's, as `SELECT DB_NAME()` then shows (unit test
/// `login_to_existing_user_database`).
///
/// The transaction is opened and committed here: it reads the catalogue and writes nothing,
/// like the binding transaction of a batch (`batch.rs`), and a failure to commit is no
/// reason to refuse a login that the catalogue accepted — the snapshot has already been
/// read.
fn resolve_login_database(engine: &Engine, requested: &str) -> Option<String> {
    let handle = engine.txn.begin(vauban_txn::IsolationLevel::ReadCommitted);
    let found = engine
        .catalog
        .snapshot(&handle)
        .database(requested)
        .map(|database| database.name.clone());
    if let Err(err) = engine.txn.commit(handle) {
        warn!(error = %err, "the transaction that read the catalogue at login did not commit");
    }
    found
}

/// Name of a client message for the log ([MS-TDS] 2.2.3.1.1 packet types).
fn message_kind(message: &ClientMessage) -> &'static str {
    match message {
        ClientMessage::Login7(_) => "LOGIN7",
        ClientMessage::SqlBatch(_) => "SQL Batch",
        ClientMessage::Rpc(_) => "RPC Request",
        ClientMessage::Attention => "Attention",
        ClientMessage::TransactionManager(_) => "Transaction Manager Request",
        ClientMessage::Unsupported(_) => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vauban_errors::SqlResult;
    use vauban_storage::{
        DbId, Direction, IndexId, IndexShape, KeyRange, MemoryStorage, Row, RowId, RowIter,
        SavepointId, Snapshot, TableId, TableShape, TxnId,
    };
    use vauban_txn::IsolationLevel;

    /// Storage that answers an internal bug to each call: what the bootstrap of
    /// `Engine::new` meets when the storage handed to it serves nothing (unit test
    /// `engine_new_panics_with_the_bootstrap_error_when_the_storage_refuses`).
    struct NopStorage;

    fn bug<T>() -> SqlResult<T> {
        Err(InternalError::Bug("NopStorage".into()).into())
    }

    impl Storage for NopStorage {
        fn create_database(&self, _name: &str) -> SqlResult<DbId> {
            bug()
        }
        fn drop_database(&self, _db: DbId) -> SqlResult<()> {
            bug()
        }
        fn databases(&self) -> SqlResult<Vec<(DbId, String)>> {
            bug()
        }
        fn create_table(&self, _db: DbId, _shape: &TableShape) -> SqlResult<TableId> {
            bug()
        }
        fn drop_table(&self, _table: TableId) -> SqlResult<()> {
            bug()
        }
        fn tables(&self, _db: DbId) -> SqlResult<Vec<(TableId, TableShape)>> {
            bug()
        }
        fn create_index(&self, _table: TableId, _def: &IndexShape) -> SqlResult<IndexId> {
            bug()
        }
        fn drop_index(&self, _index: IndexId) -> SqlResult<()> {
            bug()
        }
        fn indexes(&self, _table: TableId) -> SqlResult<Vec<(IndexId, IndexShape)>> {
            bug()
        }
        fn insert(&self, _txn: TxnId, _table: TableId, _row: &Row) -> SqlResult<RowId> {
            bug()
        }
        fn update(&self, _txn: TxnId, _table: TableId, _id: RowId, _row: &Row) -> SqlResult<()> {
            bug()
        }
        fn delete(&self, _txn: TxnId, _table: TableId, _id: RowId) -> SqlResult<()> {
            bug()
        }
        fn get(&self, _snap: &Snapshot, _table: TableId, _id: RowId) -> SqlResult<Option<Row>> {
            bug()
        }
        fn scan(&self, _snap: &Snapshot, _table: TableId) -> SqlResult<Box<dyn RowIter + '_>> {
            bug()
        }
        fn seek(
            &self,
            _snap: &Snapshot,
            _index: IndexId,
            _range: &KeyRange,
            _dir: Direction,
        ) -> SqlResult<Box<dyn RowIter + '_>> {
            bug()
        }
        fn latest_version(&self, _table: TableId, _id: RowId) -> SqlResult<Option<(TxnId, Row)>> {
            bug()
        }
        fn commit(&self, _txn: TxnId) -> SqlResult<()> {
            bug()
        }
        fn rollback(&self, _txn: TxnId) -> SqlResult<()> {
            bug()
        }
        fn savepoint(&self, _txn: TxnId) -> SqlResult<SavepointId> {
            bug()
        }
        fn rollback_to(&self, _txn: TxnId, _sp: SavepointId) -> SqlResult<()> {
            bug()
        }
        fn checkpoint(&self) -> SqlResult<()> {
            bug()
        }
        fn vacuum(&self, _horizon: TxnId) -> SqlResult<()> {
            bug()
        }
    }

    struct NoLogin;

    impl Authenticator for NoLogin {
        fn authenticate(&self, _user: &str, _password: &str) -> SqlResult<crate::Principal> {
            bug()
        }
    }

    fn server() -> Server {
        Server::new(
            Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
            ServerConfig {
                encrypt: EncryptPolicy::Off,
                tls: None,
                authenticator: Arc::new(NoLogin),
                server_name: "test".into(),
                default_packet_size: 4096,
                program_name: None,
                version_banner: None,
                edition: None,
            },
        )
    }

    #[test]
    fn spids_start_at_51_and_increase() {
        let server = server();
        assert_eq!(server.next_spid(), 51);
        assert_eq!(server.next_spid(), 52);
        assert_eq!(server.next_spid(), 53);
    }

    /// The names of the databases `storage` holds, sorted, as the bootstrap wrote them.
    fn database_names(storage: &Arc<dyn Storage>) -> Vec<String> {
        let mut names: Vec<String> = storage
            .databases()
            .expect("MemoryStorage lists its databases")
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        names.sort();
        names
    }

    /// The `DbId` of the database named `name` in `storage`.
    fn database_id(storage: &Arc<dyn Storage>, name: &str) -> DbId {
        storage
            .databases()
            .expect("MemoryStorage lists its databases")
            .into_iter()
            .find(|(_, db)| db == name)
            .unwrap_or_else(|| panic!("no database named {name}"))
            .0
    }

    /// `Engine::new` bootstraps the catalogue, read through the rows `storage` holds.
    #[test]
    fn engine_new_bootstraps_master() {
        // Counter-proof of the two assertions below: a `MemoryStorage` no engine was built
        // over holds nothing, so what the engine holds came from the bootstrap.
        let untouched: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        assert!(database_names(&untouched).is_empty());

        let engine = Engine::new(Arc::new(MemoryStorage::new()));

        // What `Catalog::bootstrap` wrote: the four system databases, and the internal
        // tables in `master`.
        assert_eq!(
            database_names(&engine.storage),
            vec!["master", "model", "msdb", "tempdb"]
        );
        let master = database_id(&engine.storage, MASTER);
        // The internal tables of `master`, read by their width: `Storage::tables` hands out
        // shapes, not names. `vauban_sys_databases` holds 3 columns, `vauban_sys_schemas` 4
        // and `vauban_sys_types` 15; the other files of `views/` add their own tables, so
        // the assertion states the presence of the three rather than a count.
        let widths: Vec<usize> = engine
            .storage
            .tables(master)
            .expect("MemoryStorage lists the tables of master")
            .into_iter()
            .map(|(_, shape)| shape.columns.len())
            .collect();
        for width in [3, 4, 15] {
            assert!(
                widths.contains(&width),
                "a table of {width} columns is in master: {widths:?}"
            );
        }

        // The path the binder takes: a transaction of `engine.txn`, a snapshot of
        // `engine.catalog` on it. What this test reads of the bootstrap is the rows above,
        // which the snapshot resolves against.
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = engine.catalog.snapshot(&handle);
        let _ = snapshot.database(MASTER);

        // The transaction of the bootstrap was closed: `active_sessions` reports one
        // transaction, the one just begun.
        let active = engine.txn.active_sessions();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, handle.id);
        engine.txn.commit(handle).expect("commit of an empty txn");
    }

    #[test]
    fn engine_new_is_idempotent_enough_for_two_engines_on_same_storage() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());

        let first = Engine::new(Arc::clone(&storage));
        let names = database_names(&storage);
        let master = database_id(&storage, MASTER);
        let tables = storage.tables(master).expect("tables of master").len();

        // Second bootstrap on the same storage: it returns instead of panicking, and the
        // three counts below are the ones the first bootstrap left.
        let second = Engine::new(Arc::clone(&storage));
        assert_eq!(database_names(&storage), names);
        assert_eq!(database_id(&storage, MASTER), master);
        assert_eq!(
            storage.tables(master).expect("tables of master").len(),
            tables
        );

        // The second bootstrap left `TxnId(1)` unused: its manager hands that identifier
        // out, while the manager of the first engine, whose bootstrap consumed it, hands out
        // `TxnId(2)`. Two engines over one storage is a shape of this test; `cli` builds one
        // engine per storage, so the two numberings do not meet there.
        assert_eq!(second.txn.begin(IsolationLevel::ReadCommitted).id, TxnId(1));
        assert_eq!(first.txn.begin(IsolationLevel::ReadCommitted).id, TxnId(2));
    }

    #[test]
    #[should_panic(expected = "catalogue bootstrap failed at start-up")]
    fn engine_new_panics_with_the_bootstrap_error_when_the_storage_refuses() {
        Engine::new(Arc::new(NopStorage));
    }

    /// A LOGIN7 of `sa` with no password check, naming `database`.
    fn login7(database: Option<&str>) -> Login7 {
        Login7 {
            username: "sa".into(),
            password: String::new(),
            database: database.map(str::to_owned),
            app_name: "app".into(),
            hostname: "host".into(),
            server_name: "server".into(),
            tds_version: crate::login::TDS_VERSION_7_4,
            packet_size: 4096,
            client_lcid: 0x0409,
            sspi: false,
            features: Vec::new(),
            language: None,
            client_interface_name: "ODBC".into(),
            client_pid: 1,
            read_only_intent: false,
            option_flags: [0; 4],
        }
    }

    /// A configuration whose `Authenticator` accepts the login it is handed, so that what the
    /// tests below exercise is the database branch alone.
    fn cfg_no_auth() -> ServerConfig {
        ServerConfig {
            encrypt: EncryptPolicy::Off,
            tls: None,
            authenticator: Arc::new(crate::NoAuth),
            server_name: "test".into(),
            default_packet_size: 4096,
            program_name: None,
            version_banner: None,
            edition: None,
        }
    }

    /// The name the server was started under reaches the evaluation context of its
    /// sessions, which is what `@@SERVERNAME` and `SERVERPROPERTY('ServerName')` read
    /// (`eval_context.rs`, `fn server_name`).
    ///
    /// The two names are neither the default of `SessionState::new` nor each other, so a
    /// context that answered a constant `vauban`, the configuration notwithstanding, would
    /// fail here (unit test `server_name_comes_from_the_session_state`).
    #[test]
    fn the_configured_server_name_reaches_the_evaluation_context() {
        let engine = Engine::new(Arc::new(MemoryStorage::new()));
        for name in ["VAUBAN-NODE-2", "sql-01\\INSTANCE"] {
            let cfg = ServerConfig {
                server_name: name.into(),
                ..cfg_no_auth()
            };
            let state = check_login(&login7(None), &engine, &cfg, 51)
                .unwrap_or_else(|_| panic!("the login under {name} is accepted"));
            assert_eq!(state.server_name, name);

            let ctx = crate::eval_context::SessionEvalContext::new(&state, None);
            let ctx: &dyn vauban_sysfn::EvalContext = &ctx;
            assert_eq!(ctx.server_name(), name);
            assert_ne!(ctx.server_name(), "vauban");
        }
    }

    /// A LOGIN7 that names a database of the instance opens it, under the spelling of the
    /// **catalogue**, which is not necessarily the one the client wrote.
    ///
    /// The four names below are the answers a login can meet: a system database of the
    /// bootstrap, `master` in another case, a user database created after the bootstrap,
    /// and that user database in another case. A branch that compared with `master` alone
    /// would answer 4060 to the last two (unit test
    /// `login_to_unknown_database_is_4060_then_18456` shows what that answer looks like).
    /// The canonical spelling is the catalogue's (rustdoc of [`resolve_login_database`]).
    #[test]
    fn login_to_existing_user_database() {
        let engine = Engine::new(Arc::new(MemoryStorage::new()));
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        engine
            .catalog
            .create_database(&handle, "Vauban_Mixed", None)
            .expect("the catalogue creates the database");
        engine.txn.commit(handle).expect("the creation commits");

        for (requested, opened) in [
            ("tempdb", "tempdb"),
            ("MASTER", "master"),
            ("Vauban_Mixed", "Vauban_Mixed"),
            ("VAUBAN_mixed", "Vauban_Mixed"),
        ] {
            let state = check_login(&login7(Some(requested)), &engine, &cfg_no_auth(), 51)
                .unwrap_or_else(|_| panic!("the login on {requested} is accepted"));
            assert_eq!(state.database, opened, "requested {requested}");
        }

        // A LOGIN7 without a database opens `master`, and so does one with an empty name:
        // that is the shape the other tests of this crate log in with.
        for none in [None, Some("")] {
            let state = check_login(&login7(none), &engine, &cfg_no_auth(), 51)
                .unwrap_or_else(|_| panic!("the login on {none:?} is accepted"));
            assert_eq!(state.database, "master");
        }

        // The transactions this branch opened were closed: no transaction of a login
        // survives into the session (leak detector of `batch.rs`).
        assert!(engine.txn.active_sessions().is_empty());
    }

    /// A LOGIN7 that names a database the catalogue does not hold is refused with 4060 then
    /// 18456, the sequence this engine keeps.
    ///
    /// Deliberate difference from SQL Server, which answers 4063 (severity 11) followed by
    /// the sequence of an accepted login and lands in `master`: falling back to the default
    /// database of the login is not implemented (rustdoc of `login::failure_response`).
    #[test]
    fn login_to_unknown_database_is_4060_then_18456() {
        let engine = Engine::new(Arc::new(MemoryStorage::new()));
        let refused = check_login(&login7(Some("nosuchdb")), &engine, &cfg_no_auth(), 51)
            .expect_err("the login on an unknown database is refused");
        let LoginRefused::With(errors) = refused else {
            panic!("a refusal carries its response");
        };
        let numbers: Vec<u32> = errors.iter().map(|err| err.number).collect();
        assert_eq!(numbers, vec![4060, 18456]);
        assert_eq!((errors[0].severity, errors[0].state), (11, 1));
        assert_eq!(
            errors[0].message,
            SqlError::cannot_open_database("nosuchdb").message
        );
        assert!(engine.txn.active_sessions().is_empty());
    }

    /// The branch that decides what a failed write means: a token the codec refused is
    /// reported to the client, anything else closes the connection.
    ///
    /// Counter-proof of the fix: were the transport failures routed to
    /// [`Relayed::Unencodable`] too, the two socket failures below would answer instead of
    /// closing, and a genuine breakage would be swallowed.
    #[test]
    fn a_refused_token_is_reported_while_a_broken_socket_closes() {
        for err in [
            TdsError::NullInNotNullable,
            TdsError::ValueTypeMismatch { expected: "int" },
            TdsError::RowWithoutMetadata,
            TdsError::ColumnCountMismatch {
                expected: 2,
                got: 1,
            },
        ] {
            let text = err.to_string();
            assert!(
                matches!(after_write_failure(err), Relayed::Unencodable(_)),
                "{text}"
            );
        }
        for err in [
            TdsError::Io(std::io::Error::other("broken pipe")),
            TdsError::ConnectionClosed,
            TdsError::Malformed("token longer than 65535 bytes"),
        ] {
            let text = err.to_string();
            assert!(
                matches!(after_write_failure(err), Relayed::Aborted(Some(_))),
                "{text}"
            );
        }
    }

    /// The message the client gets for a refused token: number 50000, severity 16, state 1,
    /// and a text that says a value could not be sent without naming a column.
    #[test]
    fn the_reported_refusal_is_the_internal_error_number() {
        let reported = SqlError::from(InternalError::Bug(format!(
            "{UNENCODABLE}: {}",
            TdsError::NullInNotNullable
        )));
        assert_eq!(
            (reported.number, reported.severity, reported.state),
            (50000, 16, 1)
        );
        assert_eq!(
            reported.message,
            "Internal error: internal bug: a value of the response cannot be sent to the \
             client: NULL value in a non-nullable column"
        );
    }

    #[test]
    fn spids_wrap_to_51_after_32767() {
        let server = server();
        server.next_spid.store(SPID_MAX - 1, Ordering::Release);
        assert_eq!(server.next_spid(), 32766);
        assert_eq!(server.next_spid(), 32767);
        assert_eq!(server.next_spid(), 51);
    }
}
