//! Payload of the row records of the journal: what [`super::wal::WalRecordKind::Insert`],
//! `Update` and `Delete` carry after the fixed fields [`super::wal`] writes.
//!
//! A record of the journal is logical: it names the table, the logical row and the version
//! the write produced, not the page it landed on. The address of the version ([`Rid`], the
//! page and slot) is left out on purpose: a version moves when its `xmax` is set
//! ([`super::version`]), and the redo ([`super::recover`]) replays the write through the heap rather
//! than copying bytes back to a position.
//!
//! [`Rid`]: super::heap::Rid
//!
//! # Layout, little-endian
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | `table` ([`TableId`]) |
//! | 4 | 32 | the prefix of the version, as [`super::version::VersionHeader`] writes it: `row`, `xmin`, `xmax`, `seq` |
//! | 36 | rest | the columns, as [`super::encode::encode_row`] writes them; empty in a `Delete` |
//!
//! The four bytes of `table` then the whole heap record is the shape chosen so that the redo
//! of an `Insert` hands [`super::heap::Heap::insert`] the bytes of the payload from offset 4
//! on, without rebuilding them.
//!
//! # What each kind carries
//!
//! | Kind | `xmin` | `xmax` | `seq` | Columns |
//! |---|---|---|---|---|
//! | `Insert` | the writing transaction | unset ([`super::version::NO_XMAX`]) | the version created | the row inserted |
//! | `Update` | the writing transaction | unset | the version created | the new content of the row |
//! | `Delete` | the transaction that created the version being hidden | the writing transaction | the version hidden | empty |
//!
//! An `Update` names the version it creates; the version it replaces is the current one of
//! the chain when the record is replayed, so its `seq` is not repeated here. A `Delete` names
//! the version it hides by its `seq`, which is what the replay looks up.
//!
//! `Begin`, `Commit` and `Abort` carry an empty payload: their `txn` field is in the fixed
//! part of the record.

use vauban_errors::InternalError;

use super::version::{VERSION_PREFIX_LEN, VersionHeader};
use crate::TableId;

/// Offset of the version prefix in the payload: after the four bytes of the table.
const OFF_HEADER: usize = 4;

/// Bytes a payload carries before its columns.
const PREFIX_LEN: usize = OFF_HEADER + VERSION_PREFIX_LEN;

/// The payload of an `Insert`, `Update` or `Delete` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowChange {
    /// The table the row belongs to.
    pub(crate) table: TableId,
    /// The version the write produced, as its prefix carries it on the heap.
    pub(crate) header: VersionHeader,
    /// The columns of that version, as [`super::encode::encode_row`] writes them; empty for a
    /// `Delete`.
    pub(crate) columns: Vec<u8>,
}

impl RowChange {
    /// The bytes of the payload, as the layout table of the module describes them.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PREFIX_LEN + self.columns.len());
        bytes.extend_from_slice(&self.table.0.to_le_bytes());
        self.header.write_to(&mut bytes);
        bytes.extend_from_slice(&self.columns);
        bytes
    }

    /// Reads a payload back.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for fewer than the [`PREFIX_LEN`] bytes of the fixed
    /// part (`decode_refuses_a_payload_shorter_than_the_prefix`).
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        let Some(table) = bytes.get(..OFF_HEADER) else {
            return Err(short(bytes.len()));
        };
        // The slice is four bytes long, checked just above.
        let table = TableId(u32::from_le_bytes([table[0], table[1], table[2], table[3]]));
        let (Some(prefix), Some(columns)) =
            (bytes.get(OFF_HEADER..PREFIX_LEN), bytes.get(PREFIX_LEN..))
        else {
            return Err(short(bytes.len()));
        };
        Ok(Self {
            table,
            header: VersionHeader::read_from(prefix)?,
            columns: columns.to_vec(),
        })
    }
}

/// The error of a payload that does not hold its fixed part.
fn short(len: usize) -> InternalError {
    InternalError::Corruption(format!(
        "a row record of the journal carries {len} payload bytes, fewer than the {PREFIX_LEN} \
         of its table and version prefix"
    ))
}

#[cfg(test)]
mod tests {
    use vauban_types::Value;

    use super::super::encode::{decode_row, encode_row};
    use super::super::version::NO_XMAX;
    use super::*;
    use crate::{Row, RowId, TxnId};

    /// The columns of a row of one `int`, encoded as a payload carries them.
    fn columns(value: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        encode_row(&Row(vec![Value::I32(value)]), &mut bytes);
        bytes
    }

    #[test]
    fn an_insert_payload_is_the_table_then_the_version_record() {
        let change = RowChange {
            table: TableId(9),
            header: VersionHeader {
                row: RowId(3),
                xmin: TxnId(4),
                xmax: None,
                seq: 7,
            },
            columns: columns(11),
        };
        let bytes = change.encode();

        assert_eq!(OFF_HEADER, 4);
        assert_eq!(PREFIX_LEN, 36);
        assert_eq!(bytes.len(), PREFIX_LEN + change.columns.len());
        assert_eq!(&bytes[0..4], &9u32.to_le_bytes());
        assert_eq!(&bytes[4..12], &3u64.to_le_bytes());
        assert_eq!(&bytes[12..20], &4u64.to_le_bytes());
        assert_eq!(&bytes[20..28], &NO_XMAX.to_le_bytes());
        assert_eq!(&bytes[28..36], &7u64.to_le_bytes());
        assert_eq!(&bytes[36..], change.columns.as_slice());
        assert_eq!(RowChange::decode(&bytes).expect("decode"), change);
        // The columns read back as the row that was encoded, which is what a redo inserts.
        assert_eq!(
            decode_row(&bytes[36..]).expect("decode the columns"),
            Row(vec![Value::I32(11)])
        );
    }

    #[test]
    fn a_delete_payload_carries_its_xmax_and_no_column() {
        let change = RowChange {
            table: TableId(1),
            header: VersionHeader {
                row: RowId(2),
                xmin: TxnId(4),
                xmax: Some(TxnId(5)),
                seq: 8,
            },
            columns: Vec::new(),
        };
        let bytes = change.encode();

        assert_eq!(bytes.len(), PREFIX_LEN);
        assert_eq!(&bytes[20..28], &5u64.to_le_bytes());
        let read = RowChange::decode(&bytes).expect("decode");
        assert_eq!(read, change);
        assert_eq!(read.header.xmax, Some(TxnId(5)));
        assert!(read.columns.is_empty());
    }

    #[test]
    fn decode_refuses_a_payload_shorter_than_the_prefix() {
        for len in [0, 4, PREFIX_LEN - 1] {
            let err = RowChange::decode(&vec![0u8; len])
                .expect_err("a payload shorter than its prefix is refused");
            assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
            assert!(
                err.to_string().contains(&format!("{len} payload bytes")),
                "{err}"
            );
        }
        // The shortest payload this module reads is the one of a `Delete`.
        assert!(RowChange::decode(&[0u8; PREFIX_LEN]).is_ok());
    }
}
