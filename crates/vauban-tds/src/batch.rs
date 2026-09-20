//! [MS-TDS] 2.2.6.7 SQL Batch: ALL_HEADERS then the UCS-2 text of the batch.
//!
//! # SQL_BATCH
//!
//! The payload of a SQL_BATCH message (packet type 0x01) is an ALL_HEADERS block
//! ([MS-TDS] 2.2.5.3, mandatory since TDS 7.2) followed by `SQLText`: the raw T-SQL of
//! the batch in UCS-2 little-endian, with neither a length prefix nor a terminator, up
//! to the end of the message. The client has already stripped the `GO` separators of
//! `sqlcmd`. A message with no text after the headers is an empty batch, not an error.
//!
//! # ATTENTION
//!
//! An ATTENTION message ([MS-TDS] 2.2.1.7 Attention; 2.2.3.1.1 Type 0x06) is a single
//! header-only packet with the EOM bit set and no payload. `decode_client_message`
//! turns it into `ClientMessage::Attention`; there is nothing to decode here.
//!
//! What the session owes in return ([MS-TDS] 3.3.5.2, Attention handling): it stops
//! the request in progress, discards whatever response it had not yet flushed, and
//! sends **one** DONE token whose status carries `DoneStatus::ATTN` (`DONE_ATTN`,
//! 0x0020), with `CurCmd` 0 and no row count. Nothing else goes out for that request:
//! no ERROR, no INFO, no further result. The client itself ignores everything it
//! receives until that DONE. An ATTENTION that arrives while the server is still
//! writing a response is handled by `session`, not here.

use crate::ResetConnection;
use crate::error::TdsError;
use crate::headers::decode_all_headers;

/// Decoded SQL_BATCH message ([MS-TDS] 2.2.6.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBatch {
    /// `SQLText`, decoded from UCS-2.
    pub text: String,
    /// `TransactionDescriptor` of the Transaction Descriptor header ([MS-TDS] 2.2.5.3.1);
    /// 0 when absent.
    pub transaction_descriptor: u64,
    /// RESETCONNECTION or RESETCONNECTIONSKIPTRAN from the first packet's status
    /// ([MS-TDS] 2.2.3.1.2).
    pub reset: ResetConnection,
}

/// Decodes the payload of a SQL_BATCH packet ([MS-TDS] 2.2.6.7): ALL_HEADERS, then
/// `SQLText` in UTF-16LE up to the end of the payload.
///
/// An odd number of text bytes is a protocol error (`Malformed`). An invalid surrogate
/// pair is not: it is replaced by U+FFFD and left to the parser, which reports the
/// error the way SQL Server would. A payload with no ALL_HEADERS (TDS 7.1 and older)
/// is refused by `decode_all_headers`.
pub(crate) fn decode(payload: &[u8]) -> Result<SqlBatch, TdsError> {
    let (headers, text) = decode_all_headers(payload)?;
    let (pairs, odd_byte) = text.as_chunks::<2>();
    if !odd_byte.is_empty() {
        return Err(TdsError::Malformed(
            "SQL_BATCH text has an odd number of bytes",
        ));
    }
    let units: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();
    Ok(SqlBatch {
        text: String::from_utf16_lossy(&units),
        transaction_descriptor: headers.transaction_descriptor,
        reset: ResetConnection::None,
    })
}

#[cfg(test)]
mod tests;
