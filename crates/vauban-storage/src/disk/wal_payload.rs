//! Payload of the row records of the journal: what [`super::wal::WalRecordKind::Insert`],
//! `Update` and `Delete` carry after the fixed fields [`super::wal`] writes.
//!
//! A row record of the journal is logical: it names the table, the logical row and the version
//! the write produced, not the page it landed on. The address of the version ([`Rid`], the
//! page and slot) is left out on purpose: a version moves when its `xmax` is set
//! ([`super::version`]), and the redo ([`super::recover`]) replays the write through the heap rather
//! than copying bytes back to a position. The records that free a heap slot, below, do name
//! the slot.
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
//! | 46 | 1 | [`Release`]: 1 when the version leaves the heap, 2 when it is written again with its `xmax` unset |
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

/// Bytes a [`SlotRelease`] occupies: the table, the version prefix, the page, the slot and the
/// kind of release.
pub(crate) const SLOT_RELEASE_LEN: usize = PREFIX_LEN + 8 + 2 + 1;

/// What became of a version whose slot a [`SlotRelease`] frees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Release {
    /// The version left the heap: the undo of the write that created it, or a vacuum.
    Removed = 1,
    /// The version was written again with its `xmax` unset: the undo of the write that set it.
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
        let header = VersionHeader::read_from(&bytes[OFF_HEADER..PREFIX_LEN])?;
        let mut page = [0u8; 8];
        page.copy_from_slice(&bytes[PREFIX_LEN..PREFIX_LEN + 8]);
        let slot = u16::from_le_bytes([bytes[PREFIX_LEN + 8], bytes[PREFIX_LEN + 9]]);
        let release = match bytes[PREFIX_LEN + 10] {
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
