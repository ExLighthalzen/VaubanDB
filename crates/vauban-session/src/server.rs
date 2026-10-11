//! `Server`: TCP acceptance loop, one tokio task per connection, graceful shutdown, and
//! the `Engine` the sessions share.
//!
//! # Connection life cycle ([MS-TDS] 3.3.5)
//!
//! 1. `serve` accepts a socket, assigns a SPID and spawns a task under the span
//!    `connection{spid, peer}`.
//!    Beyond [`ConnectionLimits::max_connections`] served connections, the new one is
//!    answered error 17809 once its LOGIN7 is in, and closed.
//! 2. The task runs `TdsStream::accept`: PRELOGIN exchange and, if negotiated, the TLS
//!    handshake ([MS-TDS] 2.2.6.5, 3.3.5.1). Those and the LOGIN7 of step 3 have
//!    [`ConnectionLimits::login_timeout`] to arrive, after which the socket is closed.
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
//! # A message this version does not handle is not a broken stream
//!
//! The reader hands over a message once its last packet is in, so a message whose content
//! the codec refuses (`ClientMessage::Refused`: an RPC with an `xml`, `sql_variant` or
//! table-valued parameter, a batch of RPCs, a BULK_LOAD message) has left nothing on the
//! socket (`tests/unsupported_input.rs`, `refused_rpc_over_several_packets_is_read_to_its_end`). It is answered by error 50000 (severity 16, state 1) naming what
//! was refused and a DONE carrying `DoneStatus::ERROR` ([`report_refused`]), and the
//! connection serves the next request. A stream the codec cannot read (`TdsError::Malformed`)
//! still closes the connection, as do the packet types of `ClientMessage::Unsupported`.
//!
//! No client error reaches `serve`: a `TdsError` is logged at `warn` and closes its own
//! connection only. The password of a LOGIN7 is never logged: only named fields of the
//! login appear in the journal, never the whole message.
//!
//! # A panicked request task
//!
//! A panic in the blocking run is caught, logged with the connection SPID and the request
//! sequence number, and turned into error 50000 (severity 16, state 1) followed by a DONE
//! carrying `DoneStatus::ERROR`, like other internal failures once the token relay has
//! finished. The session stays open when the panic is caught inside the guarded run. A
//! panic that escapes that guard is logged and, when the relay completed, gets the
//! error on the wire before the connection closes.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicI16, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet, spawn_blocking};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};
use vauban_catalog::Catalog;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::Storage;
use vauban_tds::{
    ClientMessage, DoneStatus, EncryptPolicy, Login7, ResetConnection, Rpc, RpcParam, RpcProc,
    TdsError, TdsReader, TdsStream, TdsWriter, Token,
};
use vauban_txn::TransactionManager;
use vauban_types::{SqlType, TypeInfo, Value, default_display};

use crate::auth::Authenticator;
use crate::batch::Session;
use crate::cancel::CancelHandle;
use crate::disconnect::{Release, release};
use crate::login::{self, EDITION, MASTER, VERSION_BANNER};
use crate::rpc::SystemProcedureResolver;
use crate::sink::{CHANNEL_CAPACITY, ResultSink, TdsSink};
use crate::state::SessionState;
use crate::txn_request;

/// SQL batch text read by [`should_inject_panic`] in debug and test builds.
const TEST_PANIC_BATCH: &str = "/* vauban:test-panic */";

/// Client-visible text when a request task panics; the server journal carries the payload.
const REQUEST_PANICKED: &str = "request execution stopped unexpectedly";

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
/// first login. [`Engine::with_system_procedures`] builds it likewise and installs the
/// resolver of system procedures.
pub struct Engine {
    /// The storage engine behind every table.
    pub storage: Arc<dyn Storage>,
    /// The transaction manager a session begins its transactions with.
    pub txn: Arc<TransactionManager>,
    /// The metadata of the instance, bootstrapped by [`Engine::new`].
    pub catalog: Arc<Catalog>,
    /// Resolver of system procedures (`sp_executesql`, `sp_prepare`, catalog procedures).
    /// `None` answers error 2812 to a procedure call
    /// (`an_engine_built_without_the_resolver_answers_2812_next_to_one_built_with_it`).
    system_procedures: Option<Arc<SystemProcedureResolver>>,
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
        let engine = Self {
            storage,
            txn,
            catalog,
            system_procedures: None,
        };
        engine.txn.bump_next_id_above_storage();
        if let Err(err) = engine.clear_session_registry() {
            panic!("{BOOTSTRAP_PANIC}: session registry clear failed: {err}");
        }
        engine
    }

    /// Builds the engine as [`Engine::new`] does, with `resolver` serving the system
    /// procedures a session calls by RPC or by `EXEC`.
    ///
    /// The resolver belongs to this engine: two engines in one process may carry different
    /// resolvers, and an engine built by [`Engine::new`] carries no resolver.
    ///
    /// # Panics
    ///
    /// As [`Engine::new`].
    pub fn with_system_procedures(
        storage: Arc<dyn Storage>,
        resolver: Arc<SystemProcedureResolver>,
    ) -> Self {
        let mut engine = Self::new(storage);
        engine.system_procedures = Some(resolver);
        engine
    }

    /// The resolver of system procedures, `None` when the engine was built without one.
    pub(crate) fn system_procedures(&self) -> Option<&SystemProcedureResolver> {
        self.system_procedures.as_deref()
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

/// Default of [`ConnectionLimits::login_timeout`].
pub const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(60);
/// Default of [`ConnectionLimits::max_connections`]: the largest value the
/// `user connections` server option accepts.
pub const DEFAULT_MAX_CONNECTIONS: usize = 32_767;

/// Error 17809, severity 20: the connection would exceed the connection limit.
const TOO_MANY_CONNECTIONS: u32 = 17809;
/// Error number, severity and state of a client message larger than the size limit: the
/// ones of a message this version does not handle ([`refused_error`]).
const OVERSIZED_MESSAGE: (u32, u8, u8) = (50000, 16, 1);

/// Bounds on what one client may hold of the server before its login and after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// Time a connection has, from its acceptance, to complete the PRELOGIN exchange, the
    /// TLS handshake and to send its LOGIN7; past it the socket is closed without a
    /// response (`tests/resource_limits.rs`, `a_silent_connection_is_closed_after_the_login_timeout`).
    pub login_timeout: Duration,
    /// Connections served at once. The one beyond is answered error 17809 at its LOGIN7
    /// and closed, while the others go on
    /// (`tests/resource_limits.rs`, `the_connection_beyond_the_limit_is_refused_with_17809`).
    pub max_connections: usize,
    /// Largest client message after the login, in payload bytes; `None` keeps the 32 MiB
    /// of the TDS stream. A larger one is read to its end, dropped, and answered by an
    /// error, and the connection serves the next request
    /// (`tests/resource_limits.rs`, `an_oversized_batch_gets_an_error_and_the_connection_goes_on`).
    pub max_message_size: Option<usize>,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            login_timeout: DEFAULT_LOGIN_TIMEOUT,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_message_size: None,
        }
    }
}

/// The TCP front of the engine: accepts connections and runs one task per connection.
pub struct Server {
    engine: Arc<Engine>,
    cfg: Arc<ServerConfig>,
    /// Next SPID to hand out; see [`Server::next_spid`].
    next_spid: AtomicI16,
    limits: ConnectionLimits,
}

impl Server {
    /// Builds a server with [`ConnectionLimits::default`]; nothing is bound until
    /// [`Server::serve`].
    pub fn new(engine: Arc<Engine>, cfg: ServerConfig) -> Self {
        Self {
            engine,
            cfg: Arc::new(cfg),
            next_spid: AtomicI16::new(SPID_FIRST),
            limits: ConnectionLimits::default(),
        }
    }

    /// Replaces the connection limits of the server.
    pub fn with_limits(mut self, limits: ConnectionLimits) -> Self {
        self.limits = limits;
        self
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
        // One permit per served connection, held by its task until it ends.
        let slots = Arc::new(Semaphore::new(
            self.limits.max_connections.min(Semaphore::MAX_PERMITS),
        ));

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((tcp, addr)) => {
                        let spid = self.next_spid();
                        let span = info_span!("connection", spid, peer = %addr);
                        match Arc::clone(&slots).try_acquire_owned() {
                            Ok(slot) => {
                                let task = handle_connection(
                                    Arc::clone(&self.engine),
                                    Arc::clone(&self.cfg),
                                    self.limits,
                                    tcp,
                                    spid,
                                    addr,
                                    connections.child_token(),
                                );
                                tasks.spawn(
                                    async move {
                                        task.await;
                                        drop(slot);
                                    }
                                    .instrument(span),
                                );
                            }
                            Err(_) => {
                                let task = refuse_connection(
                                    Arc::clone(&self.cfg),
                                    self.limits,
                                    tcp,
                                    connections.child_token(),
                                );
                                tasks.spawn(task.instrument(span));
                            }
                        }
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
    limits: ConnectionLimits,
    tcp: TcpStream,
    spid: i16,
    peer: std::net::SocketAddr,
    shutdown: CancellationToken,
) {
    let outcome = tokio::select! {
        _ = shutdown.cancelled() => {
            info!("connection closed by server shutdown");
            return;
        }
        outcome = run_connection(engine, &cfg, limits, tcp, spid, peer) => outcome,
    };
    match outcome {
        Ok(()) => info!("connection closed"),
        // The peer leaving is the normal end of a connection, not a fault.
        Err(TdsError::ConnectionClosed) => info!("connection closed by peer"),
        Err(err) => warn!(error = %err, "connection closed on error"),
    }
}

/// A connection beyond [`ConnectionLimits::max_connections`]: once its LOGIN7 is in,
/// answered error 17809 and a DONE `ERROR`, then closed. The login itself is not checked.
async fn refuse_connection(
    cfg: Arc<ServerConfig>,
    limits: ConnectionLimits,
    tcp: TcpStream,
    shutdown: CancellationToken,
) {
    warn!(
        max_connections = limits.max_connections,
        "connection limit reached, refusing the login"
    );
    let refused = async {
        let Some((stream, _login)) = read_login(&cfg, limits, tcp).await? else {
            return Ok(());
        };
        let mut stream = stream.downgrade_encryption().await;
        stream
            .write_tokens(&login::failure_response(vec![too_many_connections(
                limits.max_connections,
            )]))
            .await?;
        stream.flush().await
    };
    let outcome = tokio::select! {
        _ = shutdown.cancelled() => return,
        outcome = refused => outcome,
    };
    if let Err(err) = outcome {
        info!(error = %err, "refused connection closed");
    }
}

/// Error 17809, severity 20, state 1, with the configured limit in its text.
fn too_many_connections(max: usize) -> SqlError {
    SqlError::new(
        TOO_MANY_CONNECTIONS,
        20,
        1,
        format!(
            "The server already serves its maximum of {max} user connections; \
             this connection has been closed."
        ),
    )
}

/// PRELOGIN (and TLS) through `TdsStream::accept`, then the LOGIN7, both within
/// [`ConnectionLimits::login_timeout`]. `None` when the time runs out or when the message
/// after the PRELOGIN is not a LOGIN7; both are logged and close the connection.
async fn read_login(
    cfg: &ServerConfig,
    limits: ConnectionLimits,
    tcp: TcpStream,
) -> Result<Option<(TdsStream, Login7)>, TdsError> {
    let exchange = async {
        let mut stream = TdsStream::accept(tcp, cfg.tls.clone(), cfg.encrypt).await?;
        info!("PRELOGIN negotiated");
        // [MS-TDS] 3.3.5.2: the message that follows the PRELOGIN exchange is the LOGIN7.
        match stream.read_message().await? {
            ClientMessage::Login7(login) => Ok(Some((stream, login))),
            other => {
                warn!(
                    message = message_kind(&other),
                    "expected a LOGIN7 after PRELOGIN, closing"
                );
                Ok(None)
            }
        }
    };
    match tokio::time::timeout(limits.login_timeout, exchange).await {
        Ok(outcome) => outcome,
        Err(_) => {
            warn!(
                timeout_ms = limits.login_timeout.as_millis(),
                "no LOGIN7 within the login timeout, closing"
            );
            Ok(None)
        }
    }
}

/// PRELOGIN (and TLS) and the LOGIN7 through [`read_login`], the response to the LOGIN7,
/// then the client messages until the peer leaves.
async fn run_connection(
    engine: Arc<Engine>,
    cfg: &ServerConfig,
    limits: ConnectionLimits,
    tcp: TcpStream,
    spid: i16,
    peer: std::net::SocketAddr,
) -> Result<(), TdsError> {
    let Some((mut stream, login)) = read_login(cfg, limits, tcp).await? else {
        return Ok(());
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
    if let Err(err) = login::register_live_session(
        &engine,
        &state,
        &login::connection_info(&login, peer, stream.stays_encrypted_after_login()),
    ) {
        warn!(error = %err, spid, "session registry insert failed after login");
    }
    // The response above still travelled at the previous size; the negotiated one
    // applies from the next packet ([MS-TDS] 2.2.6.4 PacketSize).
    stream.set_packet_size(state.packet_size);
    if let Some(max) = limits.max_message_size {
        stream.set_max_message_size(max);
    }
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
    let login_database = state.database.clone();
    let mut session = Session::new(engine, state);
    let mut request_seq = 0u64;
    loop {
        // `None`: the reader task is gone, which only happens once its message reached us.
        let Some(message) = client_rx.recv().await else {
            let _ = release(&mut session, Release::Disconnect);
            return Ok(());
        };
        let message = match message {
            Ok(message) => message,
            Err(ReadFailure {
                error: TdsError::MessageTooLarge(size),
                reset: ResetConnection::None,
            }) => {
                report_oversized(&mut writer, size).await?;
                continue;
            }
            // The client asked for a reset on the message it is refused: the reset is
            // still applied, otherwise the next user of a pooled connection would
            // inherit the database, options and transaction of the previous one
            // (`tests/resource_limits.rs`, `an_oversized_batch_marked_reset_still_resets_the_session`).
            Err(ReadFailure {
                error: TdsError::MessageTooLarge(size),
                reset,
            }) => {
                request_seq += 1;
                let seq = request_seq;
                let login_db = login_database.clone();
                let engine_for_reset = Arc::clone(&transaction_engine);
                let outcome = run_request(
                    &mut writer,
                    &mut client_rx,
                    session,
                    false,
                    None,
                    0,
                    spid,
                    seq,
                    move |session, sink| {
                        crate::reset::apply_reset(
                            session,
                            &engine_for_reset,
                            &login_db,
                            reset,
                            sink,
                        )?;
                        warn!(
                            size,
                            "client message larger than the size limit, reset applied, \
                             answering an error"
                        );
                        sink.error(&oversized_error(size))?;
                        sink.done(None, false)
                    },
                )
                .await?;
                match outcome {
                    RequestOutcome::Served(back) => session = back,
                    RequestOutcome::Close => return Ok(()),
                }
                continue;
            }
            Err(failure) => return Err(failure.error),
        };
        match message {
            ClientMessage::SqlBatch(batch) => {
                let descriptor = batch.transaction_descriptor;
                let text = batch.text;
                let reset = batch.reset;
                request_seq += 1;
                let seq = request_seq;
                log_received_sql_batch(spid, seq, &text);
                let login_db = login_database.clone();
                let engine_for_reset = Arc::clone(&transaction_engine);
                let text_len = i32::try_from(text.len()).unwrap_or(i32::MAX);
                let outcome = run_request(
                    &mut writer,
                    &mut client_rx,
                    session,
                    false,
                    None,
                    text_len,
                    spid,
                    seq,
                    move |session, sink| {
                        crate::reset::apply_reset(
                            session,
                            &engine_for_reset,
                            &login_db,
                            reset,
                            sink,
                        )?;
                        let mut state = session.state().clone();
                        if !txn_request::reject_mismatched_descriptor(descriptor, &mut state, sink)?
                        {
                            session.replace_state(state);
                            return Ok(());
                        }
                        if should_inject_panic(&text) {
                            panic!("injected test panic");
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
                let reset = rpc.reset;
                request_seq += 1;
                let seq = request_seq;
                log_received_rpc(spid, seq, &rpc);
                let login_db = login_database.clone();
                let engine_for_reset = Arc::clone(&transaction_engine);
                let outcome = run_request(
                    &mut writer,
                    &mut client_rx,
                    session,
                    true,
                    None,
                    0,
                    spid,
                    seq,
                    move |session, sink| {
                        crate::reset::apply_reset(
                            session,
                            &engine_for_reset,
                            &login_db,
                            reset,
                            sink,
                        )?;
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
                request_seq += 1;
                let seq = request_seq;
                let outcome = run_request(
                    &mut writer,
                    &mut client_rx,
                    session,
                    false,
                    Some(CUR_CMD_TRANSACTION_MANAGER),
                    0,
                    spid,
                    seq,
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
            ClientMessage::Refused {
                packet_type,
                reason,
            } => report_refused(&mut writer, packet_type, reason).await?,
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

/// What the reader task hands over: a client message, or the read that failed.
type Incoming = Result<ClientMessage, ReadFailure>;

/// A failed read, with the reset request of the message it dropped when the failure is
/// `MessageTooLarge` (`TdsReader::take_skipped_reset`); `None` otherwise.
struct ReadFailure {
    error: TdsError,
    reset: ResetConnection,
}

/// Reads client messages and forwards them, in order, to the connection task.
///
/// Stops after forwarding a failed read (a closed connection is one of them) and when the
/// receiver is gone. `MessageTooLarge` is not a failed read: the reader skipped the
/// message and stands on the next one. The channel bound is the whole flow control: the
/// task blocks on `send` instead of reading ahead.
async fn read_messages(mut reader: TdsReader, tx: mpsc::Sender<Incoming>) {
    loop {
        let message = reader.read_message().await.map_err(|error| ReadFailure {
            reset: reader.take_skipped_reset(),
            error,
        });
        let failed = matches!(
            &message,
            Err(failure) if !matches!(failure.error, TdsError::MessageTooLarge(_))
        );
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
#[allow(clippy::too_many_arguments)] // relay needs SPID and seq alongside the existing request parameters.
async fn run_request<F>(
    writer: &mut TdsWriter,
    client_rx: &mut mpsc::Receiver<Incoming>,
    mut session: Session,
    rpc: bool,
    done_cur_cmd: Option<u16>,
    sql_text_len: i32,
    spid: i16,
    seq: u64,
    run: F,
) -> Result<RequestOutcome, TdsError>
where
    F: FnOnce(&mut Session, &mut dyn ResultSink) -> SqlResult<()> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Token>(CHANNEL_CAPACITY);
    // A handle outlives the requests of its session: an ATTENTION cancels this one only.
    let cancel = session.cancel_handle();
    cancel.reset();
    let engine = Arc::clone(session.engine());
    let database = session.state().database.clone();
    let handle = spawn_blocking(move || {
        let mut sink = if rpc {
            TdsSink::for_rpc(tx)
        } else {
            TdsSink::new(tx)
        };
        let _ = engine.touch_session(
            spid,
            sql_text_len,
            crate::registry::SessionStatus::Running,
            &database,
        );
        let (result, panic_detail) =
            match catch_unwind(AssertUnwindSafe(|| run(&mut session, &mut sink))) {
                Ok(result) => (result, None),
                Err(payload) => (Err(panic_sql_error()), Some(panic_payload_message(payload))),
            };
        let database = session.state().database.clone();
        let _ = engine.touch_session(
            spid,
            sql_text_len,
            crate::registry::SessionStatus::Sleeping,
            &database,
        );
        (session, result, panic_detail)
    });

    let relayed = relay(writer, &mut rx, client_rx, &cancel, done_cur_cmd, spid, seq).await;
    // The relay left the channel empty and closed: whatever happens next, no `blocking_send`
    // is holding a pool thread and the request task is on its way out.
    drop(rx);
    let (mut session, result, panic_detail) = match handle.await {
        Ok(outcome) => outcome,
        Err(err) if err.is_panic() => {
            error!(
                spid,
                seq,
                error = %err,
                "request task panicked outside the guarded run"
            );
            if matches!(relayed, Relayed::Complete) {
                report_request_panic(writer, spid, seq).await?;
            }
            return Ok(RequestOutcome::Close);
        }
        Err(err) => {
            error!(spid, seq, error = %err, "request blocking task failed");
            return Ok(RequestOutcome::Close);
        }
    };
    if let Some(ref detail) = panic_detail {
        error!(spid, seq, panic = %detail, "request task panicked");
    }

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
            if panic_detail.is_some() {
                log_rendered_client_error(spid, seq, &err);
            } else {
                warn!(
                    error = err.number,
                    message = %err.message,
                    "internal error during the request, response incomplete"
                );
            }
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

/// Whether a batch text is the panic injection hook in debug and test builds.
fn should_inject_panic(text: &str) -> bool {
    cfg!(any(test, debug_assertions)) && text == TEST_PANIC_BATCH
}

/// The error a panicked request task sends to the client: number 50000, severity 16, state 1.
fn panic_sql_error() -> SqlError {
    SqlError::from(InternalError::Bug(REQUEST_PANICKED.into()))
}

/// Turns a panic payload into text for the server journal.
fn panic_payload_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

/// Sends the panic error and a DONE `ERROR` when the session could not be recovered.
async fn report_request_panic(writer: &mut TdsWriter, spid: i16, seq: u64) -> Result<(), TdsError> {
    let reported = panic_sql_error();
    log_rendered_client_error(spid, seq, &reported);
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
    client_rx: &mut mpsc::Receiver<Incoming>,
    cancel: &CancelHandle,
    done_cur_cmd: Option<u16>,
    spid: i16,
    seq: u64,
) -> Relayed {
    loop {
        tokio::select! {
            token = rx.recv() => match token {
                Some(mut token) => {
                    if let Token::Error(ref err) = token {
                        log_rendered_client_error(spid, seq, err);
                    }
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
                    Some(Err(failure)) => Relayed::Aborted(Some(failure.error)),
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

/// The error that answers a refused message: number 50000, severity 16, state 1, naming
/// the message and what in it is not handled (unit test `refused_message_error`).
fn refused_error(packet_type: u8, reason: &str) -> SqlError {
    let message = match packet_type {
        0x03 => "RPC request",
        0x07 => "BULK_LOAD message",
        _ => "client message",
    };
    SqlError::from(InternalError::Bug(format!(
        "{message}: {reason} is not implemented yet"
    )))
}

/// Answers a message the codec read whole but does not handle (`ClientMessage::Refused`):
/// an ERROR then a DONE carrying `ERROR`, and the `flush` that ends the response. Nothing
/// runs, so the open transaction and its uncommitted rows are as the previous request left
/// them (`tests/unsupported_input.rs`, `refused_rpc_leaves_the_open_transaction_as_it_was`).
async fn report_refused(
    writer: &mut TdsWriter,
    packet_type: u8,
    reason: &'static str,
) -> Result<(), TdsError> {
    warn!(
        packet_type = %format!("0x{packet_type:02X}"),
        reason, "client message not handled by this version, answering an error"
    );
    writer
        .write_tokens(&[
            Token::Error(refused_error(packet_type, reason)),
            Token::Done {
                status: DoneStatus::ERROR,
                cur_cmd: 0,
                row_count: None,
            },
        ])
        .await?;
    writer.flush().await
}

/// Answers a client message the reader skipped for its size: error 50000 (severity 16,
/// state 1) naming the size, a DONE carrying `ERROR`, and the `flush` that ends the
/// response. Nothing runs, so the session is as the previous request left it.
async fn report_oversized(writer: &mut TdsWriter, size: usize) -> Result<(), TdsError> {
    warn!(
        size,
        "client message larger than the size limit, answering an error"
    );
    writer
        .write_tokens(&[
            Token::Error(oversized_error(size)),
            Token::Done {
                status: DoneStatus::ERROR,
                cur_cmd: 0,
                row_count: None,
            },
        ])
        .await?;
    writer.flush().await
}

/// Error 50000, severity 16, state 1, naming the size of the dropped message.
fn oversized_error(size: usize) -> SqlError {
    let (number, severity, state) = OVERSIZED_MESSAGE;
    SqlError::new(
        number,
        severity,
        state,
        format!(
            "The client message of {size} bytes is larger than this server accepts; \
             it was not run."
        ),
    )
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

/// Placeholder written instead of a password literal in journal text.
const LOGGED_SECRET_REDACTED: &str = "<redacted>";

/// Replaces `PASSWORD = '…'` literals before SQL text is written to the journal.
fn mask_logged_sql_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(keyword_end) = password_keyword_end(bytes, i) {
            let mut j = keyword_end;
            if skip_ascii_whitespace(bytes, &mut j) && j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                if skip_ascii_whitespace(bytes, &mut j) {
                    if matches!(bytes.get(j), Some(b'N' | b'n')) && bytes.get(j + 1) == Some(&b'\'')
                    {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'\'') {
                        out.push_str(&text[i..=j]);
                        let literal_end =
                            find_sql_string_literal_end(bytes, j).unwrap_or(bytes.len());
                        out.push_str(LOGGED_SECRET_REDACTED);
                        i = literal_end;
                        continue;
                    }
                }
            }
        }
        let ch = text[i..]
            .chars()
            .next()
            .expect("byte index on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Whether `bytes[start..]` begins with `password` as its own keyword.
fn password_keyword_end(bytes: &[u8], start: usize) -> Option<usize> {
    const KEYWORD: &[u8] = b"password";
    if start + KEYWORD.len() > bytes.len() || !is_password_keyword_boundary_before(bytes, start) {
        return None;
    }
    for (offset, &expected) in KEYWORD.iter().enumerate() {
        if bytes[start + offset].to_ascii_lowercase() != expected {
            return None;
        }
    }
    let end = start + KEYWORD.len();
    if is_password_keyword_boundary_after(bytes, end) {
        Some(end)
    } else {
        None
    }
}

fn is_password_keyword_boundary_before(bytes: &[u8], start: usize) -> bool {
    start == 0 || (!bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_')
}

fn is_password_keyword_boundary_after(bytes: &[u8], end: usize) -> bool {
    match bytes.get(end) {
        None => true,
        Some(byte) => byte.is_ascii_whitespace() || *byte == b'=',
    }
}

fn skip_ascii_whitespace(bytes: &[u8], index: &mut usize) -> bool {
    while *index < bytes.len() && bytes[*index].is_ascii_whitespace() {
        *index += 1;
    }
    true
}

/// Index after the closing quote of a single-quoted literal that starts at `quote_start`.
fn find_sql_string_literal_end(bytes: &[u8], quote_start: usize) -> Option<usize> {
    debug_assert_eq!(bytes.get(quote_start), Some(&b'\''));
    let mut index = quote_start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' => {
                if bytes.get(index + 1) == Some(&b'\'') {
                    index += 2;
                } else {
                    return Some(index + 1);
                }
            }
            _ => index += 1,
        }
    }
    None
}

/// Writes the text of a SQL batch at `debug`, with the connection SPID and the request
/// sequence number of this connection.
fn log_received_sql_batch(spid: i16, seq: u64, text: &str) {
    debug!(
        spid,
        seq,
        text = mask_logged_sql_text(text),
        "SQL batch received"
    );
}

/// Writes an RPC at `debug`, with enough detail to replay it as a T-SQL `EXEC`.
fn log_received_rpc(spid: i16, seq: u64, rpc: &Rpc) {
    debug!(
        spid,
        seq,
        text = mask_logged_sql_text(&format_received_rpc(rpc)),
        "RPC received"
    );
}

/// Writes a client-visible error at `debug`, tied to the request that raised it.
fn log_rendered_client_error(spid: i16, seq: u64, err: &SqlError) {
    debug!(
        spid,
        seq,
        error = err.number,
        severity = err.severity,
        state = err.state,
        line = err.line,
        procedure = err.procedure.as_deref().unwrap_or(""),
        message = %err.message,
        "client error rendered"
    );
}

/// Rebuilds an RPC as a T-SQL `EXEC` line, for replay in a script.
fn format_received_rpc(rpc: &Rpc) -> String {
    let proc_name = match &rpc.proc {
        RpcProc::Name(name) => name.clone(),
        RpcProc::Id(id) => rpc
            .proc
            .well_known_name()
            .map_or_else(|| id.to_string(), str::to_owned),
    };
    let args: Vec<String> = rpc.params.iter().map(format_rpc_argument).collect();
    if args.is_empty() {
        format!("EXEC {proc_name}")
    } else {
        format!("EXEC {proc_name} {}", args.join(", "))
    }
}

/// One argument of an RPC in replay form.
fn format_rpc_argument(param: &RpcParam) -> String {
    let value = format_rpc_param_value(&param.ty, &param.value);
    if param.name.is_empty() || param.name == "stmt" || param.name == "params" {
        return value;
    }
    let name = if param.name.starts_with('@') {
        param.name.clone()
    } else {
        format!("@{}", param.name)
    };
    if param.output {
        format!("{name} = {value} OUTPUT")
    } else {
        format!("{name} = {value}")
    }
}

/// Literal form of one RPC parameter value.
fn format_rpc_param_value(ty: &TypeInfo, value: &Value) -> String {
    if matches!(value, Value::Null) {
        return "NULL".to_owned();
    }
    if let Value::String(s) = value {
        let escaped = mask_logged_sql_text(&s.text).replace('\'', "''");
        return if matches!(ty.ty, SqlType::NVarChar(_) | SqlType::NChar(_)) {
            format!("N'{escaped}'")
        } else {
            format!("'{escaped}'")
        };
    }
    default_display(value, ty)
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
        ClientMessage::Refused { .. } => "refused",
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

        // The second manager starts at the storage high-water mark; the first manager,
        // already past bootstrap, hands out the next id.
        assert_eq!(second.txn.begin(IsolationLevel::ReadCommitted).id, TxnId(2));
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

    /// The error of a refused message names the message and the refused part, under the
    /// number, severity and state of the other internal failures.
    #[test]
    fn refused_message_error() {
        let rpc = refused_error(0x03, "XMLTYPE (xml)");
        assert_eq!((rpc.number, rpc.severity, rpc.state), (50000, 16, 1));
        assert!(
            rpc.message
                .ends_with("RPC request: XMLTYPE (xml) is not implemented yet"),
            "{}",
            rpc.message
        );
        let bulk = refused_error(0x07, "bulk copy of rows");
        assert!(
            bulk.message
                .ends_with("BULK_LOAD message: bulk copy of rows is not implemented yet"),
            "{}",
            bulk.message
        );
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

    /// The message the client gets when a request task panics: number 50000, severity 16,
    /// state 1, and a stable text that does not carry the panic payload.
    #[test]
    fn the_reported_panic_is_the_internal_error_number() {
        let reported = panic_sql_error();
        assert_eq!(
            (reported.number, reported.severity, reported.state),
            (50000, 16, 1)
        );
        assert_eq!(
            reported.message,
            "Internal error: internal bug: request execution stopped unexpectedly"
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

    #[test]
    fn debug_logs_sql_batch_text_with_spid_and_sequence() {
        let lines = collect_logs("debug", || {
            log_received_sql_batch(51, 1, "SELECT 1");
            log_received_sql_batch(51, 2, "SELECT 2");
        });
        assert!(lines[0].contains("text=\"SELECT 1\""));
        assert!(lines[0].contains("spid=51"));
        assert!(lines[0].contains("seq=1"));
        assert!(lines[1].contains("text=\"SELECT 2\""));
        assert!(lines[1].contains("seq=2"));
    }

    #[test]
    fn debug_logs_batch_text_and_client_error_on_the_same_request() {
        let err = SqlError::incorrect_syntax_near("SELEC", 1);
        let lines = collect_logs("debug", || {
            log_received_sql_batch(52, 3, "SELECT 1; SELEC 1");
            log_rendered_client_error(52, 3, &err);
        });
        assert!(lines[0].contains("text=\"SELECT 1; SELEC 1\""));
        assert!(lines[1].contains("error=102"));
        assert!(lines[0].contains("spid=52"));
        assert!(lines[1].contains("spid=52"));
        assert!(lines[0].contains("seq=3"));
        assert!(lines[1].contains("seq=3"));
    }

    #[test]
    fn debug_logs_rpc_text_for_replay() {
        use vauban_tds::{ResetConnection, Rpc, RpcParam, RpcProc};
        use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

        let nvarchar = |text: &str| Value::String(SqlString { text: text.into() });
        let rpc = Rpc {
            proc: RpcProc::SP_EXECUTESQL,
            options: 0,
            transaction_descriptor: 0,
            reset: ResetConnection::None,
            params: vec![RpcParam {
                name: String::new(),
                output: false,
                default: false,
                ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
                value: nvarchar("SELECT 1"),
            }],
        };
        let lines = collect_logs("debug", || log_received_rpc(53, 1, &rpc));
        assert!(lines[0].contains("spid=53"));
        assert!(lines[0].contains("seq=1"));
        assert!(lines[0].contains("EXEC sp_executesql"));
        assert!(lines[0].contains("SELECT 1"));
    }

    #[test]
    fn debug_logs_password_literals_redacted_in_sql_batches() {
        for batch in [
            "CREATE LOGIN u WITH PASSWORD = 'Secret1!'",
            "create login u with password='Secret1!'",
            "ALTER LOGIN u WITH PASSWORD  =  N'Secret1!'",
            "CREATE USER u WITH PASSWORD = 'Secret1!'",
        ] {
            let lines = collect_logs("debug", || log_received_sql_batch(55, 1, batch));
            assert!(
                lines[0].to_ascii_lowercase().contains("password"),
                "{batch}: {lines:?}"
            );
            assert!(
                lines[0].contains(LOGGED_SECRET_REDACTED),
                "{batch}: {lines:?}"
            );
            assert!(!lines[0].contains("Secret1!"), "{batch}: {lines:?}");
        }
    }

    #[test]
    fn debug_logs_password_literals_redacted_in_rpc_replay() {
        use vauban_tds::{ResetConnection, Rpc, RpcParam, RpcProc};
        use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

        let nvarchar = |text: &str| Value::String(SqlString { text: text.into() });
        let stmt = "CREATE LOGIN u WITH PASSWORD = 'RpcSecret'";
        let rpc = Rpc {
            proc: RpcProc::SP_EXECUTESQL,
            options: 0,
            transaction_descriptor: 0,
            reset: ResetConnection::None,
            params: vec![RpcParam {
                name: "stmt".into(),
                output: false,
                default: false,
                ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
                value: nvarchar(stmt),
            }],
        };
        let lines = collect_logs("debug", || log_received_rpc(56, 1, &rpc));
        assert!(lines[0].contains(LOGGED_SECRET_REDACTED), "{lines:?}");
        assert!(!lines[0].contains("RpcSecret"), "{lines:?}");
        assert_eq!(
            match &rpc.params[0].value {
                Value::String(s) => s.text.as_str(),
                _ => panic!("expected a string parameter"),
            },
            stmt
        );
    }

    #[test]
    fn logging_a_password_batch_leaves_the_source_text_unchanged() {
        let batch = "CREATE LOGIN u WITH PASSWORD = 'ExecSecret'";
        let mut source = batch.to_owned();
        let lines = collect_logs("debug", || log_received_sql_batch(57, 1, &source));
        assert!(lines[0].contains(LOGGED_SECRET_REDACTED));
        assert_eq!(source, batch);
        source.push(';');
        assert_ne!(source, batch);
    }

    #[test]
    fn batch_with_a_password_literal_still_executes_the_original_text() {
        use vauban_storage::MemoryStorage;
        use vauban_tds::ColumnMeta;

        let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
        let mut session = Session::new(engine, SessionState::new(58));
        let batch = "SELECT 'ExecSecret' AS pwd; /* note PASSWORD = 'Hidden' */";
        struct RowSink {
            rows: Vec<Value>,
            errors: Vec<SqlError>,
        }
        impl ResultSink for RowSink {
            fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
                Ok(())
            }
            fn row(&mut self, row: &[Value]) -> SqlResult<()> {
                self.rows.push(row[0].clone());
                Ok(())
            }
            fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
                Ok(())
            }
            fn info(&mut self, _msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
                Ok(())
            }
            fn error(&mut self, err: &SqlError) -> SqlResult<()> {
                self.errors.push(err.clone());
                Ok(())
            }
            fn env_change(&mut self, _change: &vauban_tds::EnvChange) -> SqlResult<()> {
                Ok(())
            }
            fn return_value(
                &mut self,
                _name: &str,
                _ty: &TypeInfo,
                _value: &Value,
            ) -> SqlResult<()> {
                Ok(())
            }
            fn return_status(&mut self, _status: i32) -> SqlResult<()> {
                Ok(())
            }
        }
        let mut sink = RowSink {
            rows: Vec::new(),
            errors: Vec::new(),
        };
        session.run_batch(batch, &mut sink).expect("batch runs");
        assert!(sink.errors.is_empty(), "{:?}", sink.errors);
        assert_eq!(
            sink.rows,
            vec![Value::String(vauban_types::SqlString {
                text: "ExecSecret".into()
            })]
        );
        let lines = collect_logs("debug", || log_received_sql_batch(58, 1, batch));
        assert!(lines[0].contains(LOGGED_SECRET_REDACTED));
        assert!(!lines[0].contains("Hidden"));
    }

    #[test]
    fn batches_without_secrets_are_logged_unchanged() {
        let batch = "SELECT 1; INSERT INTO t VALUES ('public')";
        let lines = collect_logs("debug", || log_received_sql_batch(59, 1, batch));
        assert!(lines[0].contains("text=\"SELECT 1; INSERT INTO t VALUES ('public')\""));
    }

    #[test]
    fn info_level_leaves_request_logs_silent() {
        use vauban_tds::{ResetConnection, Rpc, RpcProc};

        let lines = collect_logs("info", || {
            log_received_sql_batch(54, 1, "SELECT 1");
            log_received_rpc(
                54,
                1,
                &Rpc {
                    proc: RpcProc::Name("sp_help".into()),
                    options: 0,
                    transaction_descriptor: 0,
                    reset: ResetConnection::None,
                    params: Vec::new(),
                },
            );
            log_rendered_client_error(54, 1, &SqlError::incorrect_syntax_near("x", 1));
        });
        assert!(lines.is_empty(), "{lines:?}");
    }

    fn collect_logs(level: &str, emit: impl FnOnce()) -> Vec<String> {
        use std::fmt::Write as _;
        use std::sync::{Arc, Mutex};

        use tracing::Subscriber;
        use tracing::field::{Field, Visit};
        use tracing_subscriber::EnvFilter;
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::Context;
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::registry::LookupSpan;

        #[derive(Default)]
        struct Fields {
            text: String,
        }

        impl Visit for Fields {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if !self.text.is_empty() {
                    self.text.push(' ');
                }
                let _ = write!(self.text, "{}={value:?}", field.name());
            }

            fn record_str(&mut self, field: &Field, value: &str) {
                if !self.text.is_empty() {
                    self.text.push(' ');
                }
                let _ = write!(self.text, "{}={value:?}", field.name());
            }
        }

        struct LogCollector {
            lines: Arc<Mutex<Vec<String>>>,
        }

        impl<S> Layer<S> for LogCollector
        where
            S: Subscriber + for<'a> LookupSpan<'a>,
        {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                let mut fields = Fields::default();
                event.record(&mut fields);
                self.lines.lock().unwrap().push(format!(
                    "{} {}",
                    event.metadata().name(),
                    fields.text
                ));
            }
        }

        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new(format!("vauban_session={level}")))
            .with(LogCollector {
                lines: Arc::clone(&lines),
            });
        tracing::subscriber::with_default(subscriber, emit);
        Arc::try_unwrap(lines)
            .expect("no stray collector handles")
            .into_inner()
            .unwrap()
    }

    #[test]
    fn format_received_rpc_replays_sp_executesql() {
        use vauban_tds::{ResetConnection, Rpc, RpcParam, RpcProc};
        use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

        let nvarchar = |text: &str| Value::String(SqlString { text: text.into() });
        let rpc = Rpc {
            proc: RpcProc::SP_EXECUTESQL,
            options: 0,
            transaction_descriptor: 0,
            reset: ResetConnection::None,
            params: vec![
                RpcParam {
                    name: "stmt".into(),
                    output: false,
                    default: false,
                    ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
                    value: nvarchar("SELECT @p"),
                },
                RpcParam {
                    name: "params".into(),
                    output: false,
                    default: false,
                    ty: TypeInfo::new(SqlType::NVarChar(Len::Max), false),
                    value: nvarchar("@p int"),
                },
                RpcParam {
                    name: "@p".into(),
                    output: false,
                    default: false,
                    ty: TypeInfo::new(SqlType::Int, false),
                    value: Value::I32(1),
                },
            ],
        };
        assert_eq!(
            format_received_rpc(&rpc),
            "EXEC sp_executesql N'SELECT @p', N'@p int', @p = 1"
        );
    }

    /// Wire-level check that a panicked request yields ERROR 50000 then DONE `ERROR` on the
    /// client (`a_panicked_request_reaches_the_client_as_an_error_then_done`).
    mod panic_wire {
        use super::*;
        use std::net::SocketAddr;
        use std::sync::Arc;
        use std::time::Duration;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::time::timeout;
        use tokio_util::sync::CancellationToken;
        use vauban_storage::MemoryStorage;

        use crate::{NoAuth, Server, ServerConfig};

        const DEFAULT_PACKET_SIZE: u16 = 4096;
        const PACKET_SQL_BATCH: u8 = 0x01;
        const PACKET_TABULAR_RESULT: u8 = 0x04;
        const PACKET_LOGIN7: u8 = 0x10;
        const PACKET_PRELOGIN: u8 = 0x12;
        const STATUS_EOM: u8 = 0x01;
        const OPTION_VERSION: u8 = 0x00;
        const OPTION_ENCRYPTION: u8 = 0x01;
        const OPTION_TERMINATOR: u8 = 0xFF;
        const ENCRYPT_NOT_SUP: u8 = 0x02;
        const TDS_7_4: u32 = 0x7400_0004;
        const HEADER_TRANSACTION_DESCRIPTOR: u16 = 0x0002;
        const TOKEN_ERROR: u8 = 0xAA;
        const TOKEN_DONE: u8 = 0xFD;
        const DONE_ERROR: u16 = 0x0002;
        const RESPONSE_BUDGET: Duration = Duration::from_secs(5);
        const INTERNAL_ERROR: u32 = 50000;

        #[derive(Debug, PartialEq, Eq)]
        enum Tok {
            Error(u32),
            Done {
                status: u16,
                cur_cmd: u16,
                row_count: u64,
            },
        }

        fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
            let mut packet = vec![kind, STATUS_EOM];
            packet.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
            packet.extend_from_slice(&[0x00, 0x00, 0x01, 0x00]);
            packet.extend_from_slice(payload);
            packet
        }

        fn utf16le(s: &str) -> Vec<u8> {
            s.encode_utf16().flat_map(u16::to_le_bytes).collect()
        }

        fn prelogin_packet() -> Vec<u8> {
            let version: [u8; 6] = [0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00];
            let data_start = 11u16;
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
            packet(PACKET_PRELOGIN, &payload)
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
            assert_eq!(fixed.len(), FIXED_LEN);
            fixed.extend_from_slice(&data);
            packet(PACKET_LOGIN7, &fixed)
        }

        fn sql_batch_packet(text: &str) -> Vec<u8> {
            let mut payload = Vec::with_capacity(22 + 2 * text.len());
            payload.extend_from_slice(&22u32.to_le_bytes());
            payload.extend_from_slice(&18u32.to_le_bytes());
            payload.extend_from_slice(&HEADER_TRANSACTION_DESCRIPTOR.to_le_bytes());
            payload.extend_from_slice(&0u64.to_le_bytes());
            payload.extend_from_slice(&1u32.to_le_bytes());
            payload.extend_from_slice(&utf16le(text));
            packet(PACKET_SQL_BATCH, &payload)
        }

        async fn read_packet(client: &mut TcpStream) -> Vec<u8> {
            let mut header = [0u8; 8];
            client.read_exact(&mut header).await.unwrap();
            let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
            let mut packet = header.to_vec();
            packet.resize(len, 0);
            client.read_exact(&mut packet[8..]).await.unwrap();
            packet
        }

        async fn read_response(client: &mut TcpStream, budget: Duration) -> Vec<u8> {
            let deadline = tokio::time::Instant::now() + budget;
            let mut payload = Vec::new();
            loop {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                let packet = timeout(left, read_packet(client))
                    .await
                    .expect("response packet within budget");
                assert_eq!(packet[0], PACKET_TABULAR_RESULT);
                payload.extend_from_slice(&packet[8..]);
                if packet[1] & STATUS_EOM == STATUS_EOM {
                    return payload;
                }
            }
        }

        fn tokens(payload: &[u8]) -> Vec<Tok> {
            let mut pos = 0;
            let mut out = Vec::new();
            while pos < payload.len() {
                match payload[pos] {
                    TOKEN_ERROR => {
                        let len =
                            usize::from(u16::from_le_bytes([payload[pos + 1], payload[pos + 2]]));
                        let number =
                            u32::from_le_bytes(payload[pos + 3..pos + 7].try_into().unwrap());
                        pos += 3 + len;
                        out.push(Tok::Error(number));
                    }
                    TOKEN_DONE => {
                        out.push(Tok::Done {
                            status: u16::from_le_bytes([payload[pos + 1], payload[pos + 2]]),
                            cur_cmd: u16::from_le_bytes([payload[pos + 3], payload[pos + 4]]),
                            row_count: u64::from_le_bytes(
                                payload[pos + 5..pos + 13].try_into().unwrap(),
                            ),
                        });
                        pos += 13;
                    }
                    other => panic!("unexpected token 0x{other:02X} at {pos}"),
                }
            }
            out
        }

        async fn connect_and_login(addr: SocketAddr) -> TcpStream {
            let mut client = TcpStream::connect(addr).await.unwrap();
            client.write_all(&prelogin_packet()).await.unwrap();
            let _ = read_packet(&mut client).await;
            client.write_all(&login7_packet()).await.unwrap();
            let _ = read_response(&mut client, RESPONSE_BUDGET).await;
            client
        }

        async fn run_batch(client: &mut TcpStream, text: &str) -> Vec<Tok> {
            client.write_all(&sql_batch_packet(text)).await.unwrap();
            tokens(&read_response(client, RESPONSE_BUDGET).await)
        }

        #[tokio::test]
        async fn a_panicked_request_reaches_the_client_as_an_error_then_done() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let shutdown = CancellationToken::new();
            let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
            let server = Server::new(
                Arc::clone(&engine),
                ServerConfig {
                    encrypt: EncryptPolicy::Off,
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
            let response = run_batch(&mut client, TEST_PANIC_BATCH).await;
            assert_eq!(
                response,
                vec![
                    Tok::Error(INTERNAL_ERROR),
                    Tok::Done {
                        status: DONE_ERROR,
                        cur_cmd: 0,
                        row_count: 0,
                    },
                ]
            );
            client
                .write_all(&sql_batch_packet("SELECT 1"))
                .await
                .unwrap();
            let after = read_response(&mut client, RESPONSE_BUDGET).await;
            assert!(
                after.contains(&TOKEN_DONE),
                "session still serves after a reported panic"
            );
            drop(client);
            shutdown.cancel();
            timeout(Duration::from_secs(6), task)
                .await
                .expect("serve returns after shutdown")
                .expect("serve task finished")
                .expect("serve succeeded");
        }
    }
}
