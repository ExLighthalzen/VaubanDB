//! [MS-TDS] 2.2.6.9 Transaction Manager Request: begin, commit, rollback, save.
//!
//! A driver does not send `BEGIN TRAN` as text when the application calls
//! `BeginTransaction()`: it sends a TRANSACTION_MANAGER message (packet type 0x0E). The
//! server answers with an ENVCHANGE of type 8 (begin), 9 (commit) or 10 (rollback)
//! carrying the transaction descriptor, and the client echoes that descriptor in the
//! ALL_HEADERS of every following SQL_BATCH, RPC or TRANSACTION_MANAGER request
//! ([MS-TDS] 2.2.5.3.1). This file only decodes the request and hands the descriptor
//! over; `session` and `txn` decide what to do with it.
//!
//! Wire layout, integers little-endian:
//!
//! ```text
//! TRANSACTION_MANAGER = ALL_HEADERS RequestType RequestPayload
//! RequestType         = USHORT
//! TM_BEGIN_XACT (5)   = ISOLATION_LEVEL BEGIN_XACT_NAME
//! TM_COMMIT_XACT (7)  = XACT_NAME fBeginXact [ISOLATION_LEVEL BEGIN_XACT_NAME]
//! TM_ROLLBACK_XACT (8)= XACT_NAME fBeginXact [ISOLATION_LEVEL BEGIN_XACT_NAME]
//! TM_SAVE_XACT (9)    = SAVE_POINT_NAME
//! ISOLATION_LEVEL     = BYTE   ; 0 no change, 1 READ UNCOMMITTED, 2 READ COMMITTED,
//!                              ; 3 REPEATABLE READ, 4 SERIALIZABLE, 5 SNAPSHOT
//! fBeginXact          = BYTE   ; bit 0 set: a new transaction starts right after
//! *_NAME              = B_VARCHAR ([MS-TDS] 2.2.5.1: length in UTF-16 code units, then
//!                              ; the UTF-16LE text)
//! ```
//!
//! The optional tail of TM_COMMIT_XACT and TM_ROLLBACK_XACT is present only when
//! `fBeginXact` is set. The field order of these two payloads (name before flag) follows
//! the spec's reading.
//! TM_GET_DTC_ADDRESS (0) and TM_PROPAGATE_XACT (1) belong to DTC, out of V1 scope: they
//! come back as `Unsupported` with their payload unread.

use crate::error::TdsError;
use crate::headers::decode_all_headers;

/// `RequestType` TM_GET_DTC_ADDRESS: DTC, refused in V1.
const TM_GET_DTC_ADDRESS: u16 = 0;
/// `RequestType` TM_PROPAGATE_XACT: DTC, refused in V1.
const TM_PROPAGATE_XACT: u16 = 1;
/// `RequestType` TM_BEGIN_XACT.
const TM_BEGIN_XACT: u16 = 5;
/// `RequestType` TM_COMMIT_XACT.
const TM_COMMIT_XACT: u16 = 7;
/// `RequestType` TM_ROLLBACK_XACT.
const TM_ROLLBACK_XACT: u16 = 8;
/// `RequestType` TM_SAVE_XACT.
const TM_SAVE_XACT: u16 = 9;

/// Decoded TRANSACTION_MANAGER request ([MS-TDS] 2.2.6.9, RequestType).
///
/// `transaction_descriptor` is the `TransactionDescriptor` of the ALL_HEADERS block
/// ([MS-TDS] 2.2.5.3.1), 0 when the client is outside any explicit transaction. The
/// session matches it against the transaction it handed out in its ENVCHANGE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmRequest {
    /// TM_BEGIN_XACT (5).
    Begin {
        /// `TransactionDescriptor` of the ALL_HEADERS; 0 outside a transaction.
        transaction_descriptor: u64,
        /// `ISOLATION_LEVEL` byte, delivered as sent (0 means "no change").
        isolation: u8,
        /// `BEGIN_XACT_NAME`: optional transaction name (empty when absent).
        name: String,
    },
    /// TM_COMMIT_XACT (7).
    Commit {
        /// `TransactionDescriptor` of the ALL_HEADERS: the transaction to commit.
        transaction_descriptor: u64,
        /// `XACT_NAME`: optional transaction name (empty when absent).
        name: String,
        /// `Some(isolation)` when `fBeginXact` is set: the driver chains a new
        /// transaction with that `ISOLATION_LEVEL` right after the commit.
        begin_next: Option<u8>,
    },
    /// TM_ROLLBACK_XACT (8).
    Rollback {
        /// `TransactionDescriptor` of the ALL_HEADERS: the transaction to roll back.
        transaction_descriptor: u64,
        /// `XACT_NAME`: optional transaction or savepoint name (empty when absent).
        name: String,
        /// `Some(isolation)` when `fBeginXact` is set: the driver chains a new
        /// transaction with that `ISOLATION_LEVEL` right after the rollback.
        begin_next: Option<u8>,
    },
    /// TM_SAVE_XACT (9).
    Save {
        /// `TransactionDescriptor` of the ALL_HEADERS: the transaction to mark.
        transaction_descriptor: u64,
        /// `SAVE_POINT_NAME`: savepoint name.
        name: String,
    },
    /// Any other `RequestType` (TM_GET_DTC_ADDRESS, TM_PROPAGATE_XACT, undefined values).
    Unsupported(u16),
}

/// Forward-only reader over the request payload; every read fails with `Malformed` when
/// the bytes run out, with the spec name of the missing field as the label.
struct Cursor<'a> {
    /// Bytes not consumed yet.
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    /// Starts at the beginning of `bytes`.
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Takes `len` bytes, or fails naming the field that is truncated.
    fn take(&mut self, len: usize, field: &'static str) -> Result<&'a [u8], TdsError> {
        if self.bytes.len() < len {
            return Err(TdsError::Malformed(field));
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(head)
    }

    /// Reads one BYTE.
    fn u8(&mut self, field: &'static str) -> Result<u8, TdsError> {
        Ok(self.take(1, field)?[0])
    }

    /// Reads one little-endian USHORT.
    fn u16_le(&mut self, field: &'static str) -> Result<u16, TdsError> {
        let bytes = self.take(2, field)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Reads a B_VARCHAR ([MS-TDS] 2.2.5.1): a length in UTF-16 code units, then the
    /// UTF-16LE text. An unpaired surrogate is a protocol error, not replaced.
    fn b_varchar(&mut self, field: &'static str) -> Result<String, TdsError> {
        let units = usize::from(self.u8(field)?);
        let bytes = self.take(units * 2, field)?;
        // The length is even by construction: the remainder of `as_chunks` is empty.
        let (pairs, _) = bytes.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();
        String::from_utf16(&units).map_err(|_| TdsError::Malformed(field))
    }

    /// Fails unless every byte was consumed: the spec defines each payload completely,
    /// so leftovers mean the request was not read the way the client wrote it.
    fn finish(self, field: &'static str) -> Result<(), TdsError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(TdsError::Malformed(field))
        }
    }
}

/// Decodes the payload of a TRANSACTION_MANAGER packet ([MS-TDS] 2.2.6.9): ALL_HEADERS,
/// `RequestType`, then the request payload of that type.
///
/// Fails with `Malformed` when the ALL_HEADERS block is invalid, when a field of a
/// supported request is truncated, when a name is not valid UTF-16, or when bytes are
/// left over after a supported request. DTC and undefined request types are delivered as
/// `Unsupported` without reading their payload.
pub(crate) fn decode(payload: &[u8]) -> Result<TmRequest, TdsError> {
    let (headers, body) = decode_all_headers(payload)?;
    let transaction_descriptor = headers.transaction_descriptor;
    let mut cursor = Cursor::new(body);
    let request_type = cursor.u16_le("TRANSACTION_MANAGER RequestType")?;
    let request = match request_type {
        TM_BEGIN_XACT => {
            let isolation = cursor.u8("TM_BEGIN_XACT ISOLATION_LEVEL")?;
            let name = cursor.b_varchar("TM_BEGIN_XACT BEGIN_XACT_NAME")?;
            TmRequest::Begin {
                transaction_descriptor,
                isolation,
                name,
            }
        }
        TM_COMMIT_XACT => {
            let name = cursor.b_varchar("TM_COMMIT_XACT XACT_NAME")?;
            let begin_next = decode_begin_next(&mut cursor)?;
            TmRequest::Commit {
                transaction_descriptor,
                name,
                begin_next,
            }
        }
        TM_ROLLBACK_XACT => {
            let name = cursor.b_varchar("TM_ROLLBACK_XACT XACT_NAME")?;
            let begin_next = decode_begin_next(&mut cursor)?;
            TmRequest::Rollback {
                transaction_descriptor,
                name,
                begin_next,
            }
        }
        TM_SAVE_XACT => {
            let name = cursor.b_varchar("TM_SAVE_XACT SAVE_POINT_NAME")?;
            TmRequest::Save {
                transaction_descriptor,
                name,
            }
        }
        // DTC is out of V1 scope; the session refuses these with an error. Their payload
        // is not inspected, so nothing about it can be malformed here.
        TM_GET_DTC_ADDRESS | TM_PROPAGATE_XACT => return Ok(TmRequest::Unsupported(request_type)),
        other => return Ok(TmRequest::Unsupported(other)),
    };
    cursor.finish("TRANSACTION_MANAGER bytes after the request payload")?;
    Ok(request)
}

/// Reads `fBeginXact` and, when its bit 0 is set, the `ISOLATION_LEVEL` and
/// `BEGIN_XACT_NAME` of the chained transaction ([MS-TDS] 2.2.6.9, TM_COMMIT_XACT and
/// TM_ROLLBACK_XACT). Returns the isolation level of that next transaction. Its name is
/// read for framing but not delivered: `TmRequest` carries the flag and the level only.
/// The 7 other bits of the flag byte are reserved and ignored.
fn decode_begin_next(cursor: &mut Cursor<'_>) -> Result<Option<u8>, TdsError> {
    let flag = cursor.u8("TM_COMMIT_XACT/TM_ROLLBACK_XACT fBeginXact")?;
    if flag & 0x01 == 0 {
        return Ok(None);
    }
    let isolation = cursor.u8("TM_COMMIT_XACT/TM_ROLLBACK_XACT ISOLATION_LEVEL")?;
    cursor.b_varchar("TM_COMMIT_XACT/TM_ROLLBACK_XACT BEGIN_XACT_NAME")?;
    Ok(Some(isolation))
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::*;
    use crate::message::{ClientMessage, decode_client_message};
    use crate::packet::PacketType;
    use crate::tokens::{DoneStatus, EncodeContext, EnvChange, Token, encode_tokens};

    /// The 22-byte minimal ALL_HEADERS block: Transaction Descriptor 0, one request.
    const HEADERS: [u8; 22] = [
        0x16, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    ];

    /// A descriptor the server could have handed out, and its little-endian bytes.
    const DESCRIPTOR: u64 = 0x0102_0304_0506_0708;
    const DESCRIPTOR_LE: [u8; 8] = [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];

    /// ALL_HEADERS with `descriptor`, followed by `request`.
    fn payload(descriptor: u64, request: &[u8]) -> Vec<u8> {
        let mut out = HEADERS.to_vec();
        out[10..18].copy_from_slice(&descriptor.to_le_bytes());
        out.extend_from_slice(request);
        out
    }

    /// Decodes `request` behind a minimal ALL_HEADERS (descriptor 0).
    fn decode_request(request: &[u8]) -> Result<TmRequest, TdsError> {
        decode(&payload(0, request))
    }

    #[test]
    fn decode_begin_default() {
        let bytes = payload(0, &[0x05, 0x00, 0x00, 0x00]);
        assert_eq!(bytes.len(), 26);
        assert_eq!(
            decode(&bytes).unwrap(),
            TmRequest::Begin {
                transaction_descriptor: 0,
                isolation: 0,
                name: String::new(),
            }
        );
    }

    #[test]
    fn decode_begin_named_snapshot() {
        assert_eq!(
            decode_request(&[0x05, 0x00, 0x05, 0x02, 0x74, 0x00, 0x31, 0x00]).unwrap(),
            TmRequest::Begin {
                transaction_descriptor: 0,
                isolation: 5,
                name: "t1".into(),
            }
        );
    }

    #[test]
    fn decode_commit_with_descriptor() {
        let bytes = payload(DESCRIPTOR, &[0x07, 0x00, 0x00, 0x00]);
        assert_eq!(&bytes[10..18], &DESCRIPTOR_LE);
        assert_eq!(
            decode(&bytes).unwrap(),
            TmRequest::Commit {
                transaction_descriptor: DESCRIPTOR,
                name: String::new(),
                begin_next: None,
            }
        );

        // fBeginXact = 1: ISOLATION_LEVEL 2 and an empty BEGIN_XACT_NAME follow.
        let bytes = payload(DESCRIPTOR, &[0x07, 0x00, 0x00, 0x01, 0x02, 0x00]);
        assert_eq!(
            decode(&bytes).unwrap(),
            TmRequest::Commit {
                transaction_descriptor: DESCRIPTOR,
                name: String::new(),
                begin_next: Some(2),
            }
        );
    }

    #[test]
    fn decode_commit_named_with_chained_named_begin() {
        // XACT_NAME "t1", fBeginXact 1, ISOLATION_LEVEL 4, BEGIN_XACT_NAME "t2" (read,
        // not delivered).
        let request = [
            0x07, 0x00, // TM_COMMIT_XACT
            0x02, 0x74, 0x00, 0x31, 0x00, // "t1"
            0x01, // fBeginXact
            0x04, // SERIALIZABLE
            0x02, 0x74, 0x00, 0x32, 0x00, // "t2"
        ];
        assert_eq!(
            decode(&payload(DESCRIPTOR, &request)).unwrap(),
            TmRequest::Commit {
                transaction_descriptor: DESCRIPTOR,
                name: "t1".into(),
                begin_next: Some(4),
            }
        );

        // Reserved bits of fBeginXact are ignored: only bit 0 counts.
        assert_eq!(
            decode_request(&[0x07, 0x00, 0x00, 0x02]).unwrap(),
            TmRequest::Commit {
                transaction_descriptor: 0,
                name: String::new(),
                begin_next: None,
            }
        );
        assert_eq!(
            decode_request(&[0x07, 0x00, 0x00, 0x03, 0x01, 0x00]).unwrap(),
            TmRequest::Commit {
                transaction_descriptor: 0,
                name: String::new(),
                begin_next: Some(1),
            }
        );
    }

    #[test]
    fn decode_rollback_and_save() {
        assert_eq!(
            decode(&payload(DESCRIPTOR, &[0x08, 0x00, 0x00, 0x00])).unwrap(),
            TmRequest::Rollback {
                transaction_descriptor: DESCRIPTOR,
                name: String::new(),
                begin_next: None,
            }
        );
        // Rollback to a savepoint name, then a chained READ COMMITTED transaction.
        assert_eq!(
            decode_request(&[0x08, 0x00, 0x02, 0x73, 0x00, 0x31, 0x00, 0x01, 0x02, 0x00]).unwrap(),
            TmRequest::Rollback {
                transaction_descriptor: 0,
                name: "s1".into(),
                begin_next: Some(2),
            }
        );
        assert_eq!(
            decode(&payload(
                DESCRIPTOR,
                &[0x09, 0x00, 0x02, 0x73, 0x00, 0x31, 0x00]
            ))
            .unwrap(),
            TmRequest::Save {
                transaction_descriptor: DESCRIPTOR,
                name: "s1".into(),
            }
        );
    }

    #[test]
    fn decode_unsupported_types() {
        assert_eq!(
            decode_request(&[0x00, 0x00]).unwrap(),
            TmRequest::Unsupported(0)
        );
        assert_eq!(
            decode_request(&[0x01, 0x00]).unwrap(),
            TmRequest::Unsupported(1)
        );
        assert_eq!(
            decode_request(&[0x2A, 0x00]).unwrap(),
            TmRequest::Unsupported(42)
        );
        // The payload of an unsupported request is not inspected.
        assert_eq!(
            decode_request(&[0x01, 0x00, 0xDE, 0xAD, 0xBE, 0xEF]).unwrap(),
            TmRequest::Unsupported(1)
        );
        // RequestType is little-endian.
        assert_eq!(
            decode_request(&[0x00, 0x01]).unwrap(),
            TmRequest::Unsupported(256)
        );
    }

    #[test]
    fn decode_truncated_is_malformed() {
        let vectors: [&[u8]; 8] = [
            &[0x05, 0x00, 0x00, 0x00],
            &[0x05, 0x00, 0x05, 0x02, 0x74, 0x00, 0x31, 0x00],
            &[0x07, 0x00, 0x00, 0x00],
            &[0x07, 0x00, 0x00, 0x01, 0x02, 0x00],
            &[0x08, 0x00, 0x00, 0x00],
            &[0x09, 0x00, 0x02, 0x73, 0x00, 0x31, 0x00],
            &[0x00, 0x00],
            &[0x2A, 0x00],
        ];
        for request in vectors {
            let full = payload(DESCRIPTOR, request);
            assert!(decode(&full).is_ok(), "{request:02X?} must decode whole");
            // Every shorter prefix is truncated somewhere: inside the ALL_HEADERS block,
            // inside RequestType, or inside the request payload.
            for len in 0..full.len() {
                assert!(
                    matches!(decode(&full[..len]), Err(TdsError::Malformed(_))),
                    "{request:02X?} truncated to {len} bytes must be Malformed"
                );
            }
        }
    }

    #[test]
    fn decode_rejects_trailing_bytes_and_bad_utf16() {
        // One byte too many after a complete BEGIN.
        assert!(matches!(
            decode_request(&[0x05, 0x00, 0x00, 0x00, 0x00]),
            Err(TdsError::Malformed(_))
        ));
        // A lone high surrogate in a name is a protocol error, not replaced.
        assert!(matches!(
            decode_request(&[0x09, 0x00, 0x01, 0x3D, 0xD8]),
            Err(TdsError::Malformed(_))
        ));
        // Bare RequestType with no ALL_HEADERS at all.
        assert!(matches!(
            decode(&[0x05, 0x00, 0x00, 0x00]),
            Err(TdsError::Malformed(_))
        ));
    }

    #[test]
    fn decode_names_beyond_ascii() {
        let name = "été 😀";
        let mut request = vec![0x09, 0x00, name.encode_utf16().count() as u8];
        request.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(
            decode_request(&request).unwrap(),
            TmRequest::Save {
                transaction_descriptor: 0,
                name: name.into(),
            }
        );
    }

    #[test]
    fn dispatch_reaches_tm_decoder() {
        let bytes = payload(0, &[0x05, 0x00, 0x00, 0x00]);
        match decode_client_message(PacketType::TransactionManager, &bytes) {
            Ok(ClientMessage::TransactionManager(TmRequest::Begin { isolation: 0, .. })) => {}
            other => panic!("expected TransactionManager(Begin), got {other:?}"),
        }
    }

    /// End to end: a driver's BEGIN request decodes, and the response the session owes
    /// ([MS-TDS] 2.2.7, ENVCHANGE type 8 then DONE) has exactly these bytes.
    #[test]
    fn begin_response_bytes() {
        let bytes = payload(0, &[0x05, 0x00, 0x00, 0x00]);
        assert_eq!(
            decode(&bytes).unwrap(),
            TmRequest::Begin {
                transaction_descriptor: 0,
                isolation: 0,
                name: String::new(),
            }
        );

        let mut out = BytesMut::new();
        encode_tokens(
            &[
                Token::EnvChange(EnvChange::BeginTransaction(DESCRIPTOR)),
                Token::Done {
                    status: DoneStatus::FINAL,
                    cur_cmd: 0,
                    row_count: None,
                },
            ],
            &mut EncodeContext::default(),
            &mut out,
        )
        .unwrap();
        assert_eq!(
            &out[..],
            &[
                0xE3, 0x0B, 0x00, 0x08, // ENVCHANGE, Length = 11, Type = 8
                0x08, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // NewValue: descriptor
                0x00, // OldValue: empty
                0xFD, 0x00, 0x00, 0x00, 0x00, // DONE, Status = FINAL, CurCmd = 0
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // DoneRowCount = 0
            ]
        );

        // The client then echoes the descriptor in its ALL_HEADERS: the COMMIT that
        // follows carries it back.
        assert_eq!(
            decode(&payload(DESCRIPTOR, &[0x07, 0x00, 0x00, 0x00])).unwrap(),
            TmRequest::Commit {
                transaction_descriptor: DESCRIPTOR,
                name: String::new(),
                begin_next: None,
            }
        );
    }
}
