//! ALL_HEADERS of SQL_BATCH, RPC and TRANSACTION_MANAGER ([MS-TDS] 2.2.5.3 Packet Data
//! Stream Headers): transaction descriptor, query notifications, trace activity.
//!
//! Wire layout, every integer little-endian:
//!
//! ```text
//! ALL_HEADERS = TotalLength (DWORD, counts itself) Header*
//! Header      = HeaderLength (DWORD, counts itself) HeaderType (USHORT) HeaderData
//! ```
//!
//! Since TDS 7.2 the block is mandatory in front of a SQL_BATCH, an RPC and a
//! TRANSACTION_MANAGER request. Only the Transaction Descriptor header (type 2,
//! [MS-TDS] 2.2.5.3.1) is decoded; Query Notifications (type 1, 2.2.5.3.2) and Trace
//! Activity (type 3, 2.2.5.3.3) are skipped by their length.

use crate::error::TdsError;

/// `HeaderType` of the Query Notifications header ([MS-TDS] 2.2.5.3.2).
const HEADER_TYPE_QUERY_NOTIFICATIONS: u16 = 0x0001;
/// `HeaderType` of the Transaction Descriptor header ([MS-TDS] 2.2.5.3.1).
const HEADER_TYPE_TRANSACTION_DESCRIPTOR: u16 = 0x0002;
/// `HeaderType` of the Trace Activity header ([MS-TDS] 2.2.5.3.3).
const HEADER_TYPE_TRACE_ACTIVITY: u16 = 0x0003;

/// Size of the `TotalLength` field.
const TOTAL_LENGTH_LEN: usize = 4;
/// Size of `HeaderLength` + `HeaderType`: the smallest legal `HeaderLength`.
const HEADER_PREFIX_LEN: usize = 6;
/// `HeaderLength` of a Transaction Descriptor header: prefix + 8 + 4.
const TRANSACTION_DESCRIPTOR_HEADER_LEN: usize = HEADER_PREFIX_LEN + 8 + 4;

/// Contents of the Transaction Descriptor header ([MS-TDS] 2.2.5.3.1), or zeroes when
/// the block carries none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct AllHeaders {
    /// `TransactionDescriptor`: the descriptor the server handed out in an ENVCHANGE of
    /// type Begin Transaction; 0 outside any explicit transaction.
    pub(crate) transaction_descriptor: u64,
    /// `OutstandingRequestCount`: number of requests in flight on the connection,
    /// always 1 without MARS.
    // Not read by SQL_BATCH; the TRANSACTION_MANAGER and RPC decoders consume it.
    #[allow(dead_code)]
    pub(crate) outstanding_request_count: u32,
}

/// Reads a little-endian `u32` at the start of `bytes`, if there are enough of them.
fn read_u32_le(bytes: &[u8]) -> Option<u32> {
    let head: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(head))
}

/// Decodes an ALL_HEADERS block ([MS-TDS] 2.2.5.3) at the start of `payload`. Returns
/// the decoded headers and the remainder of the payload, which starts exactly
/// `TotalLength` bytes in.
///
/// Fails with `Malformed` when `TotalLength` does not fit in the payload or cannot even
/// count itself, when a `HeaderLength` is below 6 or overruns the block, when the headers
/// do not add up exactly to `TotalLength`, when a Transaction Descriptor header does not
/// have its fixed length of 18, or when two of them are present. Header types other
/// than 2 are skipped by their length, whether the spec defines them or not.
pub(crate) fn decode_all_headers(payload: &[u8]) -> Result<(AllHeaders, &[u8]), TdsError> {
    let total_length =
        read_u32_le(payload).ok_or(TdsError::Malformed("ALL_HEADERS shorter than TotalLength"))?;
    // `u32` to `usize` is lossless on every target the crate builds for.
    let total_length = total_length as usize;
    if total_length < TOTAL_LENGTH_LEN {
        return Err(TdsError::Malformed(
            "ALL_HEADERS TotalLength smaller than its own size",
        ));
    }
    if total_length > payload.len() {
        return Err(TdsError::Malformed(
            "ALL_HEADERS TotalLength larger than the payload",
        ));
    }
    let (block, rest) = payload.split_at(total_length);
    let mut headers = &block[TOTAL_LENGTH_LEN..];
    let mut decoded = AllHeaders::default();
    let mut seen_transaction_descriptor = false;
    while !headers.is_empty() {
        let header_length = read_u32_le(headers).ok_or(TdsError::Malformed(
            "ALL_HEADERS header shorter than HeaderLength",
        ))?;
        let header_length = header_length as usize;
        if header_length < HEADER_PREFIX_LEN {
            return Err(TdsError::Malformed(
                "ALL_HEADERS HeaderLength smaller than its prefix",
            ));
        }
        if header_length > headers.len() {
            // Also covers a sum of header lengths that exceeds TotalLength.
            return Err(TdsError::Malformed(
                "ALL_HEADERS HeaderLength overruns TotalLength",
            ));
        }
        let (header, next) = headers.split_at(header_length);
        let header_type = u16::from_le_bytes([header[4], header[5]]);
        let data = &header[HEADER_PREFIX_LEN..];
        match header_type {
            HEADER_TYPE_TRANSACTION_DESCRIPTOR => {
                if header_length != TRANSACTION_DESCRIPTOR_HEADER_LEN {
                    return Err(TdsError::Malformed(
                        "ALL_HEADERS Transaction Descriptor header is not 18 bytes",
                    ));
                }
                if seen_transaction_descriptor {
                    return Err(TdsError::Malformed(
                        "ALL_HEADERS carries two Transaction Descriptor headers",
                    ));
                }
                seen_transaction_descriptor = true;
                // Lengths were checked just above: 12 bytes of data are present.
                let descriptor: [u8; 8] = data[..8]
                    .try_into()
                    .map_err(|_| TdsError::Malformed("ALL_HEADERS TransactionDescriptor"))?;
                let count: [u8; 4] = data[8..12]
                    .try_into()
                    .map_err(|_| TdsError::Malformed("ALL_HEADERS OutstandingRequestCount"))?;
                decoded.transaction_descriptor = u64::from_le_bytes(descriptor);
                decoded.outstanding_request_count = u32::from_le_bytes(count);
            }
            // Query Notifications and Trace Activity carry nothing the server needs in V1.
            HEADER_TYPE_QUERY_NOTIFICATIONS | HEADER_TYPE_TRACE_ACTIVITY => {}
            // An undefined type is skipped the same way rather than refused, so that a
            // newer client with an extra header still gets its batch executed.
            _ => {}
        }
        headers = next;
    }
    Ok((decoded, rest))
}

#[cfg(test)]
mod tests;
