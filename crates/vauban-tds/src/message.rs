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
    /// A message read to its last packet, framed and laid out as [MS-TDS] describes, whose
    /// content this version does not handle: an RPC with a parameter type it does not
    /// decode, a batch of several RPCs, an encrypted parameter, or a BULK_LOAD message. The
    /// whole message has left the socket, so the session can answer it with an error and
    /// serve the next one; a malformed message is an `Err` instead.
    Refused {
        /// Header byte of the packet type ([MS-TDS] 2.2.3.1.1): `0x03` RPC, `0x07` BULK_LOAD.
        packet_type: u8,
        /// What was refused, for the log and the error message.
        reason: &'static str,
    },
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
            Self::Refused {
                packet_type,
                reason,
            } => write!(f, "Refused(0x{packet_type:02X}, {reason})"),
        }
    }
}

/// Decodes the payload of a reassembled message according to its packet type. ATTENTION
/// carries no payload; a packet type not listed in `ClientMessage` gives `Unsupported`.
///
/// `payload` is the whole message, so nothing of it is left on the socket, decoded or not. An RPC whose decoder answers `TdsError::Unsupported`, and a BULK_LOAD message,
/// give `Refused`: the stream is sound up to the refused part and the connection can go
/// on. Any other decoding failure stays an `Err`, which ends the connection.
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
        PacketType::Rpc => match crate::rpc::decode(payload) {
            Ok(mut r) => {
                r.reset = reset;
                Ok(ClientMessage::Rpc(r))
            }
            Err(TdsError::Unsupported(reason)) => Ok(ClientMessage::Refused {
                packet_type: kind.to_u8(),
                reason,
            }),
            Err(err) => Err(err),
        },
        // Row data sent without the INSERT BULK statement that announces its columns
        // ([MS-TDS] 2.2.6.1) cannot be read; the message is refused as a whole.
        PacketType::BulkLoad => Ok(ClientMessage::Refused {
            packet_type: kind.to_u8(),
            reason: "bulk copy of rows",
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
