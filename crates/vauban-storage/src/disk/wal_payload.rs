//! Payload of the row records of the journal: what [`super::wal::WalRecordKind::Insert`],
//! `Update` and `Delete` carry after the fixed fields [`super::wal`] writes.
//!
//! A record is physiological for a heap table: it names the table, the logical row and the
//! version the write produced, and the heap pages the write changed with the operation it made
//! on each of them ([`PageOp`]). The redo ([`super::recover`]) applies an operation to its page
//! when the `page_lsn` of that page is below the number of the record, and leaves the page as it
//! stands otherwise. A record of a clustered table names no page: its tree is rebuilt from the
//! logical part of the record ([`super::clustered`]). The records that free a heap slot, below, name
//! the slot too.
//!
//! # Layout, little-endian
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | `table` ([`TableId`]) |
//! | 4 | 32 | the prefix of the version, as [`super::version::VersionHeader`] writes it: `row`, `xmin`, `xmax`, `seq` |
//! | 36 | 2 | `ops`: the number of page operations that follow |
//! | 38 | 19 × `ops` | the page operations, in the order the write made them |
//! | 38 + 19 × `ops` | rest | the columns, as [`super::encode::encode_row`] writes them; empty in a `Delete` |
//!
//! One page operation:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 1 | `tag`: 1 for [`PageOp::Put`], 2 for [`PageOp::SetXmax`] |
//! | 1 | 8 | the heap page ([`super::page::PageId`]) |
//! | 9 | 2 | the slot in that page |
//! | 11 | 8 | `seq` of the version whose `xmax` is set; 0 in a `Put` |
//!
//! # What each kind carries
//!
//! | Kind | `xmin` | `xmax` | `seq` | Page operations | Columns |
//! |---|---|---|---|---|---|
//! | `Insert` | the writing transaction | unset ([`super::version::NO_XMAX`]) | the version created | `Put` | the row inserted |
//! | `Update` | the writing transaction | unset | the version created | `SetXmax` of the replaced version, then `Put` | the new content of the row |
//! | `Delete` | the transaction that created the version being hidden | the writing transaction | the version hidden | `SetXmax` | empty |
//!
//! A `Put` writes, at its slot, the prefix of the record followed by its columns. A `SetXmax`
//! writes the transaction of the record in the `xmax` field of the version at its slot, which
//! is the version `seq` of the row of the record. A `Delete` names the version it hides by its
//! `seq` in both places.
//!
//! `Begin`, `Commit` and `Abort` carry an empty payload: their `txn` field is in the fixed
//! part of the record.
//!
//! # Records that free a heap slot
//!
//! [`super::wal::WalRecordKind::Compensation`] (the undo of a rollback) and
//! [`super::wal::WalRecordKind::Purge`] (a vacuum) carry a [`SlotRelease`]: the version whose
//! slot is freed, the slot itself and what became of the version.
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | `table` ([`TableId`]) |
//! | 4 | 32 | the prefix of the version as it stood in the slot: `row`, `xmin`, `xmax`, `seq` |
//! | 36 | 8 | the page of the freed slot ([`PageId`]) |
//! | 44 | 2 | the slot freed in that page |
//! | 46 | 1 | [`Release`]: 1 when the version leaves the heap, 2 when its `xmax` is unset in place |
//!
//! The payload is 47 bytes long (`a_slot_release_is_47_bytes_and_reads_back`): the columns are
//! left out, the version being one the heap already holds.
//! Its `txn` field names the transaction whose write is undone for a `Compensation`, and is
//! `TxnId(0)` for a `Purge`.

use vauban_errors::InternalError;

use super::heap::Rid;
use super::page::PageId;
use super::version::{VERSION_PREFIX_LEN, VersionHeader};
use crate::TableId;

/// Offset of the version prefix in the payload: after the four bytes of the table.
const OFF_HEADER: usize = 4;

/// Offset of the number of page operations.
const OFF_OPS: usize = OFF_HEADER + VERSION_PREFIX_LEN;

/// Bytes a payload carries before its page operations.
const PREFIX_LEN: usize = OFF_OPS + 2;

/// Bytes one page operation takes.
const OP_LEN: usize = 1 + 8 + 2 + 8;

/// Tag of a [`PageOp::Put`].
const TAG_PUT: u8 = 1;

/// Tag of a [`PageOp::SetXmax`].
const TAG_SET_XMAX: u8 = 2;

/// One change a row record makes to one heap page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageOp {
    /// The version of the record is written at `rid`.
    Put {
        /// Where the version goes.
        rid: Rid,
    },
    /// The version `seq` of the row of the record, at `rid`, takes the transaction of the
    /// record as its `xmax`.
    SetXmax {
        /// Where the version sits.
        rid: Rid,
        /// Which version of the row it is.
        seq: u64,
    },
}

impl PageOp {
    /// The heap page the operation changes, whose `page_lsn` the redo compares.
    pub(crate) fn page(&self) -> PageId {
        match self {
            Self::Put { rid } | Self::SetXmax { rid, .. } => rid.page,
        }
    }

    /// Appends the [`OP_LEN`] bytes of the operation.
    fn write_to(&self, out: &mut Vec<u8>) {
        let (tag, rid, seq) = match *self {
            Self::Put { rid } => (TAG_PUT, rid, 0),
            Self::SetXmax { rid, seq } => (TAG_SET_XMAX, rid, seq),
        };
        out.push(tag);
        out.extend_from_slice(&rid.page.0.to_le_bytes());
        out.extend_from_slice(&rid.slot.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
    }

    /// Reads one operation from the [`OP_LEN`] bytes of `bytes`.
    fn read_from(bytes: &[u8]) -> Result<Self, InternalError> {
        let field = |at: usize| -> [u8; 8] {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes[at..at + 8]);
            raw
        };
        let rid = Rid {
            page: PageId(u64::from_le_bytes(field(1))),
            slot: u16::from_le_bytes([bytes[9], bytes[10]]),
        };
        match bytes[0] {
            TAG_PUT => Ok(Self::Put { rid }),
            TAG_SET_XMAX => Ok(Self::SetXmax {
                rid,
                seq: u64::from_le_bytes(field(11)),
            }),
            other => Err(InternalError::Corruption(format!(
                "a row record of the journal carries page operation {other}, which this build \
                 does not read"
            ))),
        }
    }
}

/// The payload of an `Insert`, `Update` or `Delete` record, page operations apart.
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
    /// The bytes of the payload, without a page operation: what a clustered table writes.
    pub(crate) fn encode(&self) -> Vec<u8> {
        self.encode_with(&[])
    }

    /// The bytes of the payload carrying `ops`, as the layout table of the module describes
    /// them.
    pub(crate) fn encode_with(&self, ops: &[PageOp]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PREFIX_LEN + OP_LEN * ops.len() + self.columns.len());
        bytes.extend_from_slice(&self.table.0.to_le_bytes());
        self.header.write_to(&mut bytes);
        // A write of the version layer carries one or two operations.
        bytes.extend_from_slice(&(ops.len() as u16).to_le_bytes());
        for op in ops {
            op.write_to(&mut bytes);
        }
        bytes.extend_from_slice(&self.columns);
        bytes
    }

    /// Reads a payload back, its page operations skipped.
    ///
    /// # Errors
    ///
    /// Those of [`RowChange::decode_with_ops`].
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        Self::decode_with_ops(bytes).map(|(change, _)| change)
    }

    /// Reads a payload back with its page operations.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for fewer than the [`PREFIX_LEN`] bytes of the fixed
    /// part (`decode_refuses_a_payload_shorter_than_the_prefix`), for fewer bytes than the page
    /// operations it announces, and for an operation tag this build does not read.
    pub(crate) fn decode_with_ops(bytes: &[u8]) -> Result<(Self, Vec<PageOp>), InternalError> {
        let (Some(table), Some(prefix), Some(count)) = (
            bytes.get(..OFF_HEADER),
            bytes.get(OFF_HEADER..OFF_OPS),
            bytes.get(OFF_OPS..PREFIX_LEN),
        ) else {
            return Err(short(bytes.len()));
        };
        // The slices are four and two bytes long, checked just above.
        let table = TableId(u32::from_le_bytes([table[0], table[1], table[2], table[3]]));
        let count = usize::from(u16::from_le_bytes([count[0], count[1]]));
        let columns_at = PREFIX_LEN + OP_LEN * count;
        let (Some(raw_ops), Some(columns)) =
            (bytes.get(PREFIX_LEN..columns_at), bytes.get(columns_at..))
        else {
            return Err(InternalError::Corruption(format!(
                "a row record of the journal announces {count} page operations and carries {} \
                 payload bytes, fewer than the {columns_at} they take",
                bytes.len()
            )));
        };
        let ops = raw_ops
            .chunks(OP_LEN)
            .map(PageOp::read_from)
            .collect::<Result<Vec<_>, _>>()?;
        let change = Self {
            table,
            header: VersionHeader::read_from(prefix)?,
            columns: columns.to_vec(),
        };
        Ok((change, ops))
    }
}

/// The error of a payload that does not hold its fixed part.
fn short(len: usize) -> InternalError {
    InternalError::Corruption(format!(
        "a row record of the journal carries {len} payload bytes, fewer than the {PREFIX_LEN} \
         of its table, version prefix and count of page operations"
    ))
}

/// Bytes a [`SlotRelease`] occupies: the table, the version prefix, the page, the slot and the
/// kind of release.
pub(crate) const SLOT_RELEASE_LEN: usize = OFF_OPS + 8 + 2 + 1;

/// What became of a version whose slot a [`SlotRelease`] frees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Release {
    /// The version left the heap: the undo of the write that created it, or a vacuum.
    Removed = 1,
    /// The version had its `xmax` unset in place: the undo of the write that set it.
    XmaxCleared = 2,
}

/// The payload of a [`super::wal::WalRecordKind::Compensation`] or a
/// [`super::wal::WalRecordKind::Purge`] record: one heap slot freed, as the module describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotRelease {
    /// The table the version belongs to.
    pub(crate) table: TableId,
    /// The prefix of the version as it stood in the slot.
    pub(crate) header: VersionHeader,
    /// The slot freed.
    pub(crate) at: Rid,
    /// What became of the version.
    pub(crate) release: Release,
}

impl SlotRelease {
    /// The [`SLOT_RELEASE_LEN`] bytes of the payload.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(SLOT_RELEASE_LEN);
        bytes.extend_from_slice(&self.table.0.to_le_bytes());
        self.header.write_to(&mut bytes);
        bytes.extend_from_slice(&self.at.page.0.to_le_bytes());
        bytes.extend_from_slice(&self.at.slot.to_le_bytes());
        bytes.push(self.release as u8);
        bytes
    }

    /// Reads a payload back.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a payload that is not [`SLOT_RELEASE_LEN`] bytes long
    /// and for a release byte other than 1 and 2
    /// (`a_slot_release_of_another_length_or_release_is_corruption`).
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        let bytes: &[u8; SLOT_RELEASE_LEN] = bytes.try_into().map_err(|_| {
            InternalError::Corruption(format!(
                "a slot release record of the journal carries {} payload bytes, expected \
                 {SLOT_RELEASE_LEN}",
                bytes.len()
            ))
        })?;
        let table = TableId(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
        let header = VersionHeader::read_from(&bytes[OFF_HEADER..OFF_OPS])?;
        let mut page = [0u8; 8];
        page.copy_from_slice(&bytes[OFF_OPS..OFF_OPS + 8]);
        let slot = u16::from_le_bytes([bytes[OFF_OPS + 8], bytes[OFF_OPS + 9]]);
        let release = match bytes[OFF_OPS + 10] {
            1 => Release::Removed,
            2 => Release::XmaxCleared,
            other => {
                return Err(InternalError::Corruption(format!(
                    "a slot release record of the journal carries release {other}, this build \
                     reads 1 and 2"
                )));
            }
        };
        Ok(Self {
            table,
            header,
            at: Rid {
                page: PageId(u64::from_le_bytes(page)),
                slot,
            },
            release,
        })
    }
}

#[cfg(test)]
mod tests;
