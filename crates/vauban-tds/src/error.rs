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

impl TdsError {
    /// Whether the failure is the codec refusing to turn a token into bytes, rather than
    /// the connection under it going away.
    ///
    /// `true` for the four failures the encoder raises while looking at a value: a value
    /// that does not match the TYPE_INFO it is announced under, a `NULL` under a TYPE_INFO
    /// that is not nullable, a row against no COLMETADATA, a row of another width than the
    /// COLMETADATA before it. `write_tokens` encodes into a scratch buffer, so these leave
    /// the response buffer and the socket as they were (`stream.rs`, unit test
    /// `write_tokens_error_leaves_buffer_and_wire_untouched`): the peer is still there and
    /// still waiting for an answer, which its caller can send.
    ///
    /// `false` for the rest: I/O and TLS failures, a byte stream that violates [MS-TDS],
    /// an oversized message, an unexpected packet type, a feature this version refuses, a
    /// path not implemented, a refused encryption negotiation, a peer that left. A caller
    /// that meets one of those has no working channel to answer through. `Malformed`
    /// carries both a client stream the decoder rejects and a token too large to frame, and
    /// stays on this side: the framing, not the value, is what failed.
    ///
    /// Both arms are written out, so a variant added to the enum stops the build instead of
    /// taking a side by default (unit test `each_variant_has_one_sample_and_its_side`).
    pub fn is_encoding_failure(&self) -> bool {
        match self {
            Self::ValueTypeMismatch { .. }
            | Self::NullInNotNullable
            | Self::RowWithoutMetadata
            | Self::ColumnCountMismatch { .. } => true,
            Self::Io(_)
            | Self::Tls(_)
            | Self::Malformed(_)
            | Self::MessageTooLarge(_)
            | Self::UnexpectedPacketType(_)
            | Self::Unsupported(_)
            | Self::NotImplemented(_)
            | Self::EncryptionRefused
            | Self::ConnectionClosed => false,
        }
    }
}

#[cfg(test)]
mod tests;
