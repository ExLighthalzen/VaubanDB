//! Server tokens ([MS-TDS] 2.2.7 Packet Data Token Stream Definition): the `Token` enum,
//! its companion structures, the dispatch `encode_tokens`, and the B_VARCHAR / US_VARCHAR /
//! B_VARBYTE helpers ([MS-TDS] 2.2.5.1) shared by the encoders. Each token's encoder
//! lives in its own file.

mod colmetadata;
mod done;
mod env_change;
mod feature_ext_ack;
mod login_ack;
mod message;
mod order;
mod return_status;
mod return_value;
mod row;

use std::ops::{BitOr, BitOrAssign};

use bytes::{BufMut, BytesMut};
use vauban_errors::{InfoMessage, SqlError};
use vauban_types::{Collation, TypeInfo, Value};

use crate::error::TdsError;

/// A server token ([MS-TDS] 2.2.7). `Token::Row` is encoded as ROW or NBCROW depending on
/// the presence of NULLs.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// LOGINACK.
    LoginAck {
        /// `TDSVersion` the server settles on (0x74000004 for TDS 7.4).
        tds_version: u32,
        /// `ProgName`.
        program_name: String,
        /// `MajorVer`, `MinorVer`, `BuildNumHi`, `BuildNumLow`.
        version: [u8; 4],
    },
    /// ENVCHANGE.
    EnvChange(EnvChange),
    /// INFO.
    Info(InfoMessage),
    /// ERROR.
    Error(SqlError),
    /// COLMETADATA.
    ColMetaData(Vec<ColumnMeta>),
    /// ROW or NBCROW; one value per column of the last COLMETADATA.
    Row(Vec<Value>),
    /// DONE.
    Done {
        /// `Status` bit field.
        status: DoneStatus,
        /// `CurCmd`.
        cur_cmd: u16,
        /// `DoneRowCount`; `Some` implies `DoneStatus::COUNT`.
        row_count: Option<u64>,
    },
    /// DONEPROC.
    DoneProc {
        /// `Status` bit field.
        status: DoneStatus,
        /// `CurCmd`.
        cur_cmd: u16,
        /// `DoneRowCount`; `Some` implies `DoneStatus::COUNT`.
        row_count: Option<u64>,
    },
    /// DONEINPROC.
    DoneInProc {
        /// `Status` bit field.
        status: DoneStatus,
        /// `CurCmd`.
        cur_cmd: u16,
        /// `DoneRowCount`; `Some` implies `DoneStatus::COUNT`.
        row_count: Option<u64>,
    },
    /// RETURNSTATUS.
    ReturnStatus(i32),
    /// RETURNVALUE.
    ReturnValue {
        /// `ParamName`.
        name: String,
        /// `ParamOrdinal`.
        ordinal: u16,
        /// `TYPE_INFO` of the value.
        ty: TypeInfo,
        /// The value (`Value::Null` for NULL).
        value: Value,
    },
    /// ORDER: ordinals of the columns of the ORDER BY clause.
    Order(Vec<u16>),
    /// FEATUREEXTACK.
    FeatureExtAck(Vec<FeatureAck>),
}

/// ENVCHANGE payloads ([MS-TDS] 2.2.7, token ENVCHANGE, `Type` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvChange {
    /// Type 1: database.
    Database {
        /// Previous database name.
        old: String,
        /// New database name.
        new: String,
    },
    /// Type 2: language.
    Language {
        /// Previous language.
        old: String,
        /// New language.
        new: String,
    },
    /// Type 4: packet size.
    PacketSize {
        /// Previous packet size.
        old: u16,
        /// New packet size.
        new: u16,
    },
    /// Type 7: SQL collation.
    Collation {
        /// Previous collation, `None` at login.
        old: Option<Collation>,
        /// New collation.
        new: Collation,
    },
    /// Type 8: begin transaction; carries the new transaction descriptor.
    BeginTransaction(u64),
    /// Type 9: commit transaction; carries the descriptor of the ended transaction.
    CommitTransaction(u64),
    /// Type 10: rollback transaction; carries the descriptor of the ended transaction.
    RollbackTransaction(u64),
}

/// `Status` bit field of DONE, DONEPROC and DONEINPROC ([MS-TDS] 2.2.7, token DONE).
/// The spec names (`DONE_MORE`…) live only in the comments; combine with `|`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DoneStatus(pub u16);

impl DoneStatus {
    /// DONE_FINAL (0x0000): last DONE of the response.
    pub const FINAL: Self = Self(0x0000);
    /// DONE_MORE (0x0001): more result sets follow.
    pub const MORE: Self = Self(0x0001);
    /// DONE_ERROR (0x0002): the statement failed.
    pub const ERROR: Self = Self(0x0002);
    /// DONE_INXACT (0x0004): a transaction is in progress.
    pub const INXACT: Self = Self(0x0004);
    /// DONE_COUNT (0x0010): `DoneRowCount` is valid.
    pub const COUNT: Self = Self(0x0010);
    /// DONE_ATTN (0x0020): acknowledges an ATTENTION.
    pub const ATTN: Self = Self(0x0020);
    /// DONE_SRVERROR (0x0100): a severe error, the result set is discarded.
    pub const SRVERROR: Self = Self(0x0100);

    /// `true` when every bit of `other` is set in `self`.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for DoneStatus {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for DoneStatus {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// One column of a COLMETADATA ([MS-TDS] 2.2.7, token COLMETADATA, ColumnData).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    /// `ColName`.
    pub name: String,
    /// `TYPE_INFO`.
    pub ty: TypeInfo,
    /// `Flags`.
    pub flags: ColumnFlags,
}

/// `Flags` of a COLMETADATA column ([MS-TDS] 2.2.7, token COLMETADATA).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColumnFlags {
    /// `fNullable`.
    pub nullable: bool,
    /// `fCaseSen`.
    pub case_sensitive: bool,
    /// `usUpdateable` = 1 (read/write).
    pub updatable: bool,
    /// `fIdentity`.
    pub identity: bool,
    /// `fComputed`.
    pub computed: bool,
}

/// One acknowledged feature of a FEATUREEXTACK ([MS-TDS] 2.2.7, token FEATUREEXTACK).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureAck {
    /// `FeatureId`.
    pub id: u8,
    /// `FeatureAckData`, opaque to this crate.
    pub data: Vec<u8>,
}

/// State carried across tokens of a response: the last COLMETADATA emitted, needed to
/// encode ROW / NBCROW.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EncodeContext {
    /// Columns of the last `Token::ColMetaData`; empty before the first one.
    pub(crate) columns: Vec<ColumnMeta>,
}

/// Encodes `tokens` in order into `out`, dispatching each to the encoder of its file.
/// Stops at the first error; bytes of the preceding tokens stay in `out`.
pub fn encode_tokens(
    tokens: &[Token],
    ctx: &mut EncodeContext,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    for token in tokens {
        match token {
            Token::LoginAck { .. } => login_ack::encode(token, ctx, out)?,
            Token::EnvChange(_) => env_change::encode(token, ctx, out)?,
            Token::Info(_) | Token::Error(_) => message::encode(token, ctx, out)?,
            Token::ColMetaData(_) => colmetadata::encode(token, ctx, out)?,
            Token::Row(_) => row::encode(token, ctx, out)?,
            Token::Done { .. } | Token::DoneProc { .. } | Token::DoneInProc { .. } => {
                done::encode(token, ctx, out)?
            }
            Token::ReturnStatus(_) => return_status::encode(token, ctx, out)?,
            Token::ReturnValue { .. } => return_value::encode(token, ctx, out)?,
            Token::Order(_) => order::encode(token, ctx, out)?,
            Token::FeatureExtAck(_) => feature_ext_ack::encode(token, ctx, out)?,
        }
    }
    Ok(())
}

/// Appends `s` as UTF-16LE code units.
fn put_utf16le(out: &mut BytesMut, s: &str) {
    for unit in s.encode_utf16() {
        out.put_u16_le(unit);
    }
}

/// Writes a B_VARCHAR ([MS-TDS] 2.2.5.1): one byte with the number of UTF-16 code units,
/// then the UTF-16LE text. Fails with `Malformed` beyond 255 code units.
// Shared helper for the token encoders; kept even when no encoder in this build calls it.
#[allow(dead_code)]
pub(crate) fn put_b_varchar(out: &mut BytesMut, s: &str) -> Result<(), TdsError> {
    let units = s.encode_utf16().count();
    let len =
        u8::try_from(units).map_err(|_| TdsError::Malformed("B_VARCHAR longer than 255 chars"))?;
    out.put_u8(len);
    put_utf16le(out, s);
    Ok(())
}

/// Writes a US_VARCHAR ([MS-TDS] 2.2.5.1): two little-endian bytes with the number of UTF-16
/// code units, then the UTF-16LE text. Fails with `Malformed` beyond 65535 code units.
// Shared helper for the token encoders; kept even when no encoder in this build calls it.
#[allow(dead_code)]
pub(crate) fn put_us_varchar(out: &mut BytesMut, s: &str) -> Result<(), TdsError> {
    let units = s.encode_utf16().count();
    let len = u16::try_from(units)
        .map_err(|_| TdsError::Malformed("US_VARCHAR longer than 65535 chars"))?;
    out.put_u16_le(len);
    put_utf16le(out, s);
    Ok(())
}

/// Writes a B_VARBYTE ([MS-TDS] 2.2.5.1): one length byte then the bytes. Fails with
/// `Malformed` beyond 255 bytes.
// Shared helper for the token encoders; kept even when no encoder in this build calls it.
#[allow(dead_code)]
pub(crate) fn put_b_varbyte(out: &mut BytesMut, b: &[u8]) -> Result<(), TdsError> {
    let len = u8::try_from(b.len())
        .map_err(|_| TdsError::Malformed("B_VARBYTE longer than 255 bytes"))?;
    out.put_u8(len);
    out.put_slice(b);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varchar_helpers() {
        let mut out = BytesMut::new();
        put_b_varchar(&mut out, "ab").unwrap();
        assert_eq!(&out[..], &[0x02, 0x61, 0x00, 0x62, 0x00]);

        let mut out = BytesMut::new();
        put_us_varchar(&mut out, "ab").unwrap();
        assert_eq!(&out[..], &[0x02, 0x00, 0x61, 0x00, 0x62, 0x00]);

        let mut out = BytesMut::new();
        put_b_varbyte(&mut out, &[1, 2]).unwrap();
        assert_eq!(&out[..], &[0x02, 0x01, 0x02]);

        let mut out = BytesMut::new();
        let long = "x".repeat(256);
        assert!(matches!(
            put_b_varchar(&mut out, &long),
            Err(TdsError::Malformed(_))
        ));
        assert!(out.is_empty());
        put_b_varchar(&mut out, &long[..255]).unwrap();
        assert_eq!(out[0], 255);

        // Length counts UTF-16 code units, not bytes nor scalar values.
        let mut out = BytesMut::new();
        put_b_varchar(&mut out, "é😀").unwrap();
        assert_eq!(out[0], 3);
        assert_eq!(out.len(), 1 + 3 * 2);

        let mut out = BytesMut::new();
        assert!(matches!(
            put_b_varbyte(&mut out, &[0u8; 256]),
            Err(TdsError::Malformed(_))
        ));
        let mut out = BytesMut::new();
        assert!(matches!(
            put_us_varchar(&mut out, &"y".repeat(65536)),
            Err(TdsError::Malformed(_))
        ));
    }

    #[test]
    fn encode_tokens_dispatch_never_panics() {
        // Each token kind is dispatched to its own encoder.
        // Whether a given encoder is still a stub or already implemented, dispatching must
        // either succeed or fail with a `TdsError`, never panic, and an empty slice writes
        // nothing.
        let mut ctx = EncodeContext::default();
        let mut buf = BytesMut::new();
        assert!(encode_tokens(&[], &mut ctx, &mut buf).is_ok());
        assert!(buf.is_empty());

        let tokens = [
            Token::Done {
                status: DoneStatus::FINAL,
                cur_cmd: 0,
                row_count: None,
            },
            Token::ReturnStatus(0),
            Token::Order(vec![1]),
            Token::ColMetaData(vec![]),
            Token::Row(vec![]),
            Token::FeatureExtAck(vec![]),
            Token::Info(InfoMessage {
                number: 0,
                severity: 0,
                state: 1,
                message: "hello".into(),
                line: 1,
            }),
            Token::Error(SqlError::new(50000, 16, 1, "boom")),
        ];
        for token in tokens {
            let mut out = BytesMut::new();
            match encode_tokens(&[token], &mut ctx, &mut out) {
                Ok(()) => assert!(!out.is_empty(), "an implemented encoder must write bytes"),
                Err(TdsError::NotImplemented(_)) => assert!(out.is_empty()),
                Err(other) => panic!("unexpected error from dispatch: {other:?}"),
            }
        }
    }

    #[test]
    fn done_status_bitor() {
        assert_eq!(DoneStatus::MORE | DoneStatus::ERROR, DoneStatus(0x0003));
        let mut s = DoneStatus::FINAL;
        s |= DoneStatus::COUNT;
        assert_eq!(s, DoneStatus(0x0010));
        assert!((DoneStatus::MORE | DoneStatus::ERROR).contains(DoneStatus::ERROR));
        assert!(!(DoneStatus::MORE | DoneStatus::ERROR).contains(DoneStatus::COUNT));
        assert!(DoneStatus::SRVERROR.contains(DoneStatus::FINAL));
    }
}
