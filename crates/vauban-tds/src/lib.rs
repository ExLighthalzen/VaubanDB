//! Crate `vauban-tds`: TDS 7.4 protocol codec ([MS-TDS]): packets, client messages,
//! server tokens, type encoding, and the TLS handshake carried in PRELOGIN.
//!
//! This crate knows nothing about tables or queries: it turns bytes into `ClientMessage`
//! values and `Token` values into bytes. Only `session` and `cli` use it.
//!
//! # File plan
//!
//! | File(s) | Content |
//! |---|---|
//! | `error.rs`, `packet.rs`, `message.rs`, `tokens/mod.rs`, this file | error type, framing, dispatch |
//! | `prelogin.rs` | PRELOGIN decoding, response and encryption negotiation |
//! | `stream.rs` | `TdsStream`: connection life cycle and framed message I/O |
//! | `login7.rs` | LOGIN7 decoding and feature extensions |
//! | `headers.rs`, `batch.rs` | ALL_HEADERS and SQL_BATCH decoding |
//! | `tokens/{login_ack,env_change,done,message,feature_ext_ack}.rs` | login and status tokens |
//! | `types/` | TYPE_INFO and value encoding and decoding |
//! | `tokens/{colmetadata,row,order}.rs` | result set token encoders |
//! | `tests/*.rs`, `Cargo.toml` (dev-dependencies) | end-to-end tests with a real client |
//! | `tm.rs` | TRANSACTION_MANAGER decoding |
//! | `rpc.rs`, `tokens/{return_status,return_value}.rs` | RPC decoding and its response tokens |
//! | `tls.rs`, `tests/fixtures/` | TLS handshake carried in PRELOGIN packets |

mod batch;
mod error;
mod headers;
mod login7;
mod message;
mod packet;
mod prelogin;
mod rpc;
mod stream;
mod tls;
mod tm;
mod tokens;
mod types;

pub use batch::SqlBatch;
pub use error::TdsError;
pub use login7::{FeatureExt, Login7};
pub use message::ClientMessage;
pub use packet::PacketType;
pub use prelogin::EncryptPolicy;
pub use rpc::{Rpc, RpcParam, RpcProc};
pub use stream::{TdsReader, TdsStream, TdsWriter};
pub use tm::TmRequest;
pub use tokens::{ColumnFlags, ColumnMeta, DoneStatus, EnvChange, FeatureAck, Token};

#[cfg(test)]
mod api_surface {
    /// The names of the public interface resolve at the crate root (the `use` below is
    /// the list). Written as a unit test, hence `crate::` instead of `vauban_tds::`.
    #[test]
    fn interface_names_resolve_at_root() {
        use crate::{
            ClientMessage, ColumnFlags, ColumnMeta, DoneStatus, EncryptPolicy, EnvChange,
            FeatureAck, FeatureExt, Login7, PacketType, Rpc, RpcParam, RpcProc, SqlBatch, TdsError,
            TdsReader, TdsStream, TdsWriter, TmRequest, Token,
        };

        fn exists<T>() {}
        exists::<EncryptPolicy>();
        exists::<ClientMessage>();
        exists::<Login7>();
        exists::<SqlBatch>();
        exists::<Rpc>();
        exists::<RpcProc>();
        exists::<RpcParam>();
        exists::<Token>();
        exists::<ColumnMeta>();
        exists::<ColumnFlags>();
        exists::<EnvChange>();
        exists::<DoneStatus>();
        exists::<FeatureExt>();
        exists::<FeatureAck>();
        exists::<TmRequest>();
        exists::<TdsError>();
        exists::<TdsStream>();
        exists::<TdsReader>();
        exists::<TdsWriter>();
        exists::<PacketType>();
    }
}
