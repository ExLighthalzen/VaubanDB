//! Client messages ([MS-TDS] 2.2.1 Client Messages) and the dispatch from a packet type to
//! its decoder. The decoders themselves live in the file of each message.

use std::fmt;

use crate::ResetConnection;
use crate::batch::SqlBatch;
use crate::error::TdsError;
use crate::login7::Login7;
use crate::packet::PacketType;
use crate::rpc::Rpc;
use crate::tm::TmRequest;

/// A decoded client message. PRELOGIN is handled by `TdsStream::accept` and never reaches
/// this enum.
pub enum ClientMessage {
    /// LOGIN7 ([MS-TDS] 2.2.6.4).
    Login7(Login7),
    /// SQL Batch ([MS-TDS] 2.2.6.7).
    SqlBatch(SqlBatch),
    /// RPC Request ([MS-TDS] 2.2.6.6).
    Rpc(Rpc),
    /// Attention ([MS-TDS] 2.2.1.7), no payload.
    Attention,
    /// Transaction Manager Request ([MS-TDS] 2.2.6.9).
    TransactionManager(TmRequest),
    /// Any other packet type, carried as its header byte so the session can refuse it.
    Unsupported(u8),
}

// Manual so that `Login7`, which hides its password, needs no `Debug` of its own here.
impl fmt::Debug for ClientMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Login7(_) => f.write_str("Login7(..)"),
            Self::SqlBatch(b) => f.debug_tuple("SqlBatch").field(b).finish(),
            Self::Rpc(r) => f.debug_tuple("Rpc").field(r).finish(),
            Self::Attention => f.write_str("Attention"),
            Self::TransactionManager(t) => f.debug_tuple("TransactionManager").field(t).finish(),
            Self::Unsupported(k) => write!(f, "Unsupported(0x{k:02X})"),
        }
    }
}

/// Decodes the payload of a reassembled message according to its packet type. ATTENTION
/// carries no payload; a packet type not listed in `ClientMessage` gives `Unsupported`.
/// `reset` is the RESETCONNECTION or RESETCONNECTIONSKIPTRAN status from the first packet
/// of the message ([MS-TDS] 2.2.3.1.2), applied to `SqlBatch` and `Rpc`.
pub fn decode_client_message(
    kind: PacketType,
    payload: &[u8],
    reset: ResetConnection,
) -> Result<ClientMessage, TdsError> {
    match kind {
        PacketType::Login7 => crate::login7::decode(payload).map(ClientMessage::Login7),
        PacketType::SqlBatch => crate::batch::decode(payload).map(|mut b| {
            b.reset = reset;
            ClientMessage::SqlBatch(b)
        }),
        PacketType::Rpc => crate::rpc::decode(payload).map(|mut r| {
            r.reset = reset;
            ClientMessage::Rpc(r)
        }),
        PacketType::TransactionManager => {
            crate::tm::decode(payload).map(ClientMessage::TransactionManager)
        }
        PacketType::Attention => {
            if payload.is_empty() {
                Ok(ClientMessage::Attention)
            } else {
                Err(TdsError::Malformed("ATTENTION packet with a payload"))
            }
        }
        other => Ok(ClientMessage::Unsupported(other.to_u8())),
    }
}

#[cfg(test)]
mod tests;
