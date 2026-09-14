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
mod tests {
    use super::*;

    /// The 22-byte minimal block: one Transaction Descriptor header with descriptor 0
    /// and one outstanding request.
    const MINIMAL: [u8; 22] = [
        0x16, 0x00, 0x00, 0x00, // TotalLength = 22
        0x12, 0x00, 0x00, 0x00, // HeaderLength = 18
        0x02, 0x00, // HeaderType = Transaction Descriptor
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // TransactionDescriptor = 0
        0x01, 0x00, 0x00, 0x00, // OutstandingRequestCount = 1
    ];

    /// Builds one header from its type and data, `HeaderLength` computed.
    fn header(header_type: u16, data: &[u8]) -> Vec<u8> {
        let mut out = ((HEADER_PREFIX_LEN + data.len()) as u32)
            .to_le_bytes()
            .to_vec();
        out.extend_from_slice(&header_type.to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    /// Concatenates headers behind a computed `TotalLength`.
    fn block(headers: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = headers.concat();
        let mut out = ((TOTAL_LENGTH_LEN + body.len()) as u32)
            .to_le_bytes()
            .to_vec();
        out.extend_from_slice(&body);
        out
    }

    /// Transaction Descriptor header data for a descriptor and a request count.
    fn transaction_data(descriptor: u64, count: u32) -> Vec<u8> {
        let mut out = descriptor.to_le_bytes().to_vec();
        out.extend_from_slice(&count.to_le_bytes());
        out
    }

    #[test]
    fn minimal_block_yields_zero_descriptor_and_empty_rest() {
        let (headers, rest) = decode_all_headers(&MINIMAL).unwrap();
        assert_eq!(headers.transaction_descriptor, 0);
        assert_eq!(headers.outstanding_request_count, 1);
        assert!(rest.is_empty());
    }

    #[test]
    fn rest_starts_right_after_total_length() {
        let mut payload = MINIMAL.to_vec();
        payload.extend_from_slice(b"tail");
        let (_, rest) = decode_all_headers(&payload).unwrap();
        assert_eq!(rest, b"tail");
        assert_eq!(rest.as_ptr(), payload[22..].as_ptr());
    }

    #[test]
    fn reads_transaction_descriptor_little_endian() {
        let payload = block(&[header(
            HEADER_TYPE_TRANSACTION_DESCRIPTOR,
            &[
                0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01, // descriptor
                0x02, 0x00, 0x00, 0x00, // count = 2
            ],
        )]);
        let (headers, rest) = decode_all_headers(&payload).unwrap();
        assert_eq!(headers.transaction_descriptor, 0x0123_4567_89AB_CDEF);
        assert_eq!(headers.outstanding_request_count, 2);
        assert!(rest.is_empty());
    }

    #[test]
    fn all_headers_skips_unknown_header() {
        // Trace Activity: GUID_ActivityID (16) + ActivitySequence (4) -> HeaderLength 26.
        let trace = header(HEADER_TYPE_TRACE_ACTIVITY, &[0xAA; 20]);
        assert_eq!(trace.len(), 26);
        let transaction = header(
            HEADER_TYPE_TRANSACTION_DESCRIPTOR,
            &transaction_data(0x42, 1),
        );
        let mut payload = block(&[trace, transaction]);
        let total = payload.len();
        payload.extend_from_slice(&[0x53, 0x00]); // "S" in UTF-16LE
        let (headers, rest) = decode_all_headers(&payload).unwrap();
        assert_eq!(headers.transaction_descriptor, 0x42);
        assert_eq!(rest, &[0x53, 0x00]);
        assert_eq!(rest.as_ptr(), payload[total..].as_ptr());
    }

    #[test]
    fn skips_query_notifications_and_undefined_types() {
        let notifications = header(HEADER_TYPE_QUERY_NOTIFICATIONS, b"\x02\x00i\x00d\x00");
        let undefined = header(0x7FFF, &[1, 2, 3]);
        let transaction = header(HEADER_TYPE_TRANSACTION_DESCRIPTOR, &transaction_data(7, 1));
        let payload = block(&[notifications, transaction, undefined]);
        let (headers, rest) = decode_all_headers(&payload).unwrap();
        assert_eq!(headers.transaction_descriptor, 7);
        assert!(rest.is_empty());
    }

    #[test]
    fn missing_transaction_header_gives_zero_descriptor() {
        let payload = block(&[header(HEADER_TYPE_TRACE_ACTIVITY, &[0; 20])]);
        let (headers, _) = decode_all_headers(&payload).unwrap();
        assert_eq!(headers, AllHeaders::default());

        // A block with TotalLength = 4 and no header at all is well-formed too.
        let (headers, rest) = decode_all_headers(&[4, 0, 0, 0, 0xFF]).unwrap();
        assert_eq!(headers.transaction_descriptor, 0);
        assert_eq!(rest, &[0xFF]);
    }

    #[test]
    fn all_headers_rejects_bad_lengths() {
        // TotalLength larger than the payload.
        let mut payload = MINIMAL.to_vec();
        payload[0] = 0x17;
        assert!(matches!(
            decode_all_headers(&payload),
            Err(TdsError::Malformed(_))
        ));

        // HeaderLength < 6.
        let mut payload = MINIMAL.to_vec();
        payload[4] = 0x05;
        assert!(matches!(
            decode_all_headers(&payload),
            Err(TdsError::Malformed(_))
        ));

        // Sum of the headers smaller than TotalLength: 4 trailing bytes are left over,
        // and are not a header (HeaderLength = 0).
        let mut payload = block(&[header(
            HEADER_TYPE_TRANSACTION_DESCRIPTOR,
            &transaction_data(0, 1),
        )]);
        payload.extend_from_slice(&[0, 0, 0, 0]);
        payload[0] += 4;
        assert!(matches!(
            decode_all_headers(&payload),
            Err(TdsError::Malformed(_))
        ));

        // Sum of the headers larger than TotalLength: the header overruns the block.
        let mut payload = MINIMAL.to_vec();
        payload[4] = 0x13;
        payload.push(0);
        assert!(matches!(
            decode_all_headers(&payload),
            Err(TdsError::Malformed(_))
        ));

        // Leftover of 1..=5 bytes inside the block, too short for a HeaderLength.
        let mut payload = MINIMAL.to_vec();
        payload[0] = 0x19;
        payload.extend_from_slice(&[0, 0, 0]);
        assert!(matches!(
            decode_all_headers(&payload),
            Err(TdsError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_total_length_smaller_than_itself_or_missing() {
        assert!(matches!(
            decode_all_headers(&[3, 0, 0, 0]),
            Err(TdsError::Malformed(_))
        ));
        assert!(matches!(
            decode_all_headers(&[0, 0, 0, 0]),
            Err(TdsError::Malformed(_))
        ));
        assert!(matches!(
            decode_all_headers(&[0x16, 0, 0]),
            Err(TdsError::Malformed(_))
        ));
        assert!(matches!(
            decode_all_headers(&[]),
            Err(TdsError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_transaction_header_of_wrong_length_or_repeated() {
        let short = block(&[header(HEADER_TYPE_TRANSACTION_DESCRIPTOR, &[0; 8])]);
        assert!(matches!(
            decode_all_headers(&short),
            Err(TdsError::Malformed(_))
        ));
        let long = block(&[header(HEADER_TYPE_TRANSACTION_DESCRIPTOR, &[0; 16])]);
        assert!(matches!(
            decode_all_headers(&long),
            Err(TdsError::Malformed(_))
        ));
        let twice = block(&[
            header(HEADER_TYPE_TRANSACTION_DESCRIPTOR, &transaction_data(1, 1)),
            header(HEADER_TYPE_TRANSACTION_DESCRIPTOR, &transaction_data(2, 1)),
        ]);
        assert!(matches!(
            decode_all_headers(&twice),
            Err(TdsError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_bare_text_from_pre_tds72_clients() {
        // "SELECT 1" in UTF-16LE with no ALL_HEADERS: the first four bytes read as a
        // TotalLength of 0x00450053, far beyond the payload.
        let text = [0x53, 0x00, 0x45, 0x00, 0x4C, 0x00, 0x45, 0x00];
        assert!(matches!(
            decode_all_headers(&text),
            Err(TdsError::Malformed(_))
        ));
    }
}
