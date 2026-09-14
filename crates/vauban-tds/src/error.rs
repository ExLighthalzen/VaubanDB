//! Internal error type of the `tds` crate.
//!
//! `TdsError` describes protocol-level failures (framing, malformed streams, TLS, I/O).
//! It is never sent to a client as such: errors meant for the client are `SqlError`
//! values carried by `Token::Error`. The variant list is kept short: a decoder that has
//! no dedicated variant uses `Malformed` or `Unsupported` with a precise label.

/// Protocol-level error of the TDS codec.
#[derive(Debug, thiserror::Error)]
pub enum TdsError {
    /// Underlying socket error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// TLS handshake or record error ([MS-TDS] 2.2.6.5 PRELOGIN, ENCRYPTION option).
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    /// The byte stream violates [MS-TDS]; the label says where.
    #[error("malformed TDS stream: {0}")]
    Malformed(&'static str),
    /// A client message exceeds the size limit given to `read_message`; carries the
    /// size that would have been reached.
    #[error("TDS message too large: {0} bytes")]
    MessageTooLarge(usize),
    /// A packet type ([MS-TDS] 2.2.3.1.1) that is valid but not expected at this point
    /// of the conversation (e.g. SQL_BATCH before LOGIN7).
    #[error("unexpected packet type 0x{0:02X}")]
    UnexpectedPacketType(u8),
    /// A feature the V1 refuses on purpose (MARS, TDS 8.0, SSPI, BULK_LOAD…).
    #[error("unsupported TDS feature: {0}")]
    Unsupported(&'static str),
    /// A code path that is declared but not implemented yet.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    /// The encryption negotiation failed: the client and the `EncryptPolicy` disagree.
    #[error("encryption negotiation refused")]
    EncryptionRefused,
    /// The peer closed the connection.
    #[error("connection closed by peer")]
    ConnectionClosed,
    /// A `Value` does not match the `TypeInfo` it is encoded with.
    #[error("value type mismatch: expected {expected}")]
    ValueTypeMismatch {
        /// Name of the expected `SqlType`.
        expected: &'static str,
    },
    /// A `NULL` value in a column whose metadata says it is not nullable.
    #[error("NULL value in a non-nullable column")]
    NullInNotNullable,
    /// A `Token::Row` emitted before any `Token::ColMetaData`.
    #[error("ROW token without preceding COLMETADATA")]
    RowWithoutMetadata,
    /// A `Token::Row` whose value count differs from the last `Token::ColMetaData`.
    #[error("column count mismatch: expected {expected}, got {got}")]
    ColumnCountMismatch {
        /// Number of columns declared by the last COLMETADATA.
        expected: usize,
        /// Number of values in the row.
        got: usize,
    },
}
