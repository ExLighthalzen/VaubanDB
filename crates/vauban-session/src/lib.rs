//! Crate `vauban-session`: connection life cycle. The TCP acceptance loop, one tokio
//! task per connection, the login sequence, the per-session state and `SET` options, the
//! batch and RPC dispatch, the `ResultSink` that turns engine results into TDS tokens, and
//! the ATTENTION handling. This is the async/sync boundary of the engine: the engine
//! crates are synchronous and run on the blocking pool.
//!
//! # File plan
//!
//! | File(s) | Contents |
//! |---|---|
//! | `server.rs`, `auth.rs` | acceptance loop, connection task, authentication contract |
//! | `login.rs` | LOGIN7 handling and the login response |
//! | `state.rs`, `set_options.rs` | per-session state and `SET` options |
//! | `sink.rs` | `ResultSink` and its TDS implementation |
//! | `batch.rs`, `fake_engine.rs` | batch and RPC dispatch, the `WAITFOR DELAY` fallback |
//! | `cancel.rs` | ATTENTION handling |
//! | `disconnect.rs` | connection teardown and ATTENTION cleanup |
//! | `eval_context.rs` | `EvalContext` of the session for `sysfn` |
//! | `txn_request.rs` | TRANSACTION_MANAGER requests |
//! | `txn_session.rs` | the session transaction: opening, holding across batches, closing, descriptor |

mod auth;
mod batch;
mod cancel;
mod disconnect;
mod eval_context;
mod fake_engine;
mod login;
mod nested;
mod server;
mod set_options;
mod sink;
mod state;
mod txn_request;
mod txn_session;

pub use auth::*;
pub use server::*;

// A glob re-export of a module without public items imports nothing, which
// `unused_imports` reports; the allows are kept so that emptying a module stays harmless.
#[allow(unused_imports)]
pub use batch::*;
#[allow(unused_imports)]
pub use cancel::*;
#[allow(unused_imports)]
pub use fake_engine::*;
#[allow(unused_imports)]
pub use login::*;
#[allow(unused_imports)]
pub use nested::*;
#[allow(unused_imports)]
pub use set_options::*;
#[allow(unused_imports)]
pub use sink::*;
#[allow(unused_imports)]
pub use state::*;

// Re-export: `cli` configures the encryption policy without depending on `tds` directly.
pub use vauban_tds::EncryptPolicy;
