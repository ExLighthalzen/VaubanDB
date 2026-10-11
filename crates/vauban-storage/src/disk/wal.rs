//! The write-ahead journal of an on-disk instance: `<dir>/wal`, an append-only file of
//! records.
//!
//! A record names what changed (a row inserted in a table, a transaction committed); a row
//! record of a heap table also names the pages it changed and the operation it made on each,
//! which is what the redo compares with their `page_lsn`. What the payload of a row record holds
//! is [`super::wal_payload`], and [`super::version::HeapTable`] is what appends a record before
//! it changes a page. The changes of structure — the pages of a tree, the formatting and the
//! link of a heap page, an overflow chain, a freed page, the catalogue — are carried as bytes
//! instead, by a [`WalRecordKind::PageWrite`] (section "Page writes" below). Here the file, the
//! record format, the log sequence numbers and `sync_all` are what is written.
//!
//! # Record format, little-endian
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | `length`: bytes of the record **after** this field, checksum included |
//! | 4 | 1 | `version` = 1 |
//! | 5 | 1 | [`WalRecordKind`] |
//! | 6 | 8 | `lsn` ([`Lsn`]) |
//! | 14 | 8 | `prev_lsn`, `0` in the first record of the journal |
//! | 22 | 8 | `txn` ([`crate::TxnId`]), `0` when the record belongs to no transaction |
//! | 30 | `length` − 30 | payload, opaque to this file |
//! | end − 4 | 4 | CRC-32 of the record but these four bytes, `length` included |
//!
//! A record therefore occupies `34 + payload` bytes on disk. [`Wal::append`] refuses a payload
//! that would push a record past [`MAX_RECORD_LEN`], and a `length` field announcing more than
//! that frames no record for the scan below. The checksum is the CRC-32 of [`super::crc`], the
//! one the pages use.
//!
//! # Log sequence numbers
//!
//! The `lsn` is a counter carried by the record, not its offset: the first record of a fresh
//! journal is `Lsn(1)`, and each record that follows adds 1. `prev_lsn` repeats the number of
//! the record before it, per record and not per transaction, so that a scan can check the
//! chain without holding a map of transactions; the recovery ([`super::recover`]) sorts the
//! winners out by their `Commit` records. Nothing about the numbering is persisted beside the records: [`Wal::open`]
//! reads the last one and carries on from there.
//!
//! # Durability
//!
//! [`Wal::append`] writes the record and returns; [`Wal::flush`] calls `sync_all` and moves
//! [`Wal::durable_lsn`]. `sync_all` (data and metadata) rather than `sync_data`, on the ground
//! that the size the appends grew the file to is metadata; what a given file system loses
//! when the data is synced without its metadata is not asserted in this crate.
//!
//! Dropping a `Wal` does not flush it: a `Drop` that called `sync_all` would make a crash
//! impossible to simulate from a test, and a crash test needs to kill a process holding
//! unflushed records. What a reader gets back after such a crash is what [`Wal::open`]
//! decides, below.
//!
//! # What `open` does with a damaged file
//!
//! An append can be cut short by a crash, which leaves at the end of the file fewer bytes than
//! the last `length` announces, or a `length` that frames nothing at all. [`Wal::open`] scans
//! the records from the start, and what it does with a record it cannot read depends on **what
//! lies after that record**, in all three forms the failure takes:
//!
//! - the bytes of the record are not all there, its `length` frames no record (four zero bytes,
//!   a length outside the bounds of a record), or its checksum disagrees — **and no record with
//!   an agreeing checksum starts anywhere after it**: the tail was written by an append that did
//!   not finish, and the file is truncated to the end of the last record that checks out. A file
//!   in which the walk below finds no agreeing checksum at all is emptied by that same rule,
//!   without an error (`tests::a_file_holding_no_agreeing_checksum_is_emptied_without_an_error`,
//!   500 bytes carrying no record);
//! - the same three failures, but **a record with an agreeing checksum starts after it**:
//!   [`InternalError::Corruption`] naming the offset of the unreadable record and that later
//!   offset, and the file is left as it stands. Sealed bytes behind it make the record an
//!   interior one for each of the three failures, and this build repairs none of them; the
//!   sweeps of `open` over one journal of two records and one of three, one **bit** flipped at a
//!   time over each byte of those two files, cover the three together
//!   (`tests::the_two_bit_sweeps_stop_on_frames_of_the_three_failing_kinds`): of the 1 840
//!   frames the scan stops on there, 1 694 fail their checksum, 72 are short of the bytes they
//!   announce and 74 frame nothing at all.
//! - a record whose bytes are all there and whose checksum agrees but which this build will not
//!   read — an unknown `version`, a kind outside [`WalRecordKind`], an `lsn` that does not
//!   continue the chain: `Corruption` naming the offset. An intact checksum means the record was
//!   written whole, so what is wrong with it was not lost in a torn append and its position in
//!   the file does not change the answer (`tests::an_unknown_kind_is_corruption_even_at_the_end`,
//!   `tests::another_format_version_is_corruption`,
//!   `tests::a_number_that_does_not_continue_the_chain_is_corruption`: a lone record carrying
//!   kind 99, a lone record carrying version 2, a lone record numbered 4, and a second record
//!   that names 7 as the record before it).
//!
//! Because an intact checksum is what tells a record written whole from one an unfinished
//! append left behind, it is checked before the `version` and the kind byte, unlike
//! [`super::page::Page::verify`] which reads its header first.
//!
//! "After it" is looked for the same way for the three failures, because none of them leaves a
//! boundary worth trusting. The `length` field is covered by the checksum, so a record whose
//! checksum disagrees announces a size that may be anything: on a journal of two records of 34
//! and 64 bytes, flipping one bit of the first `length` field turns 30 into 94 and makes that
//! first frame span the file to its last byte. Stepping over such a frame would carry the scan
//! past records that are whole. So the scan stops at the first frame it cannot read, and
//! [`Wal::sealed_record_after`] walks the offsets one by one from the byte after it, stopping at
//! the first that frames a record whose checksum agrees — whether this build reads that record
//! or not, an agreeing checksum being what says the bytes were written whole.
//!
//! Rejecting one offset costs the four bytes of a length field when that length frames no
//! record, and as much as the record it announces when it frames one: an offset announcing 1 000
//! bytes has them read, 1 004 in all, before its checksum is found to disagree. The walk runs
//! once a frame the scan cannot read has been met — that is, on a file whose tail an append left
//! half-written or whose bytes were altered, not on a journal read through to its end. Like the
//! sequential scan, it holds at most one record in memory.
//!
//! A record boundary is not the only place an agreeing checksum can sit: a payload holding the
//! bytes of an encoded record carries one too, and the walk stops on it. A torn tail whose
//! payload carries an encoded record is therefore reported as an interior record
//! (`tests::a_torn_tail_whose_payload_holds_a_record_reads_as_an_interior_record`: one such
//! tail, appended behind one whole record); what makes the case unlikely on payloads
//! not built to hold a record is the CRC-32, nothing in this file.
//!
//! `open` ends with a `sync_all`, so that a truncation it performed is covered by that
//! `sync_all` and so that [`Wal::durable_lsn`] names a record the file holds after the call.
//!
//! There is one journal per instance and it is not rotated; it is not mapped into memory
//! either, for the reason [`super::file`] gives about pages.
//!
//! # The handle an instance holds
//!
//! [`Wal`] appends through `&mut self`; [`WalHandle`] is the same journal behind a mutex,
//! which is what [`super::DiskStorage`] holds so that the tables writing their records and the
//! buffer pool reading [`Wal::durable_lsn`] share one journal.
//!
//! # Group commit
//!
//! [`WalHandle::wait_durable`] is how the end of a transaction becomes durable without holding
//! the journal lock across `sync_all`. A caller appends its record, then waits until
//! [`Wal::durable_lsn`] reaches it. The first waiter that finds no sync running becomes the
//! leader: it reads the number of the last record appended so far, drops the journal lock,
//! syncs the file through a second handle on it, then moves the durable LSN to that number.
//! The callers that appended while it was syncing wait for it, and one of them leads the next
//! sync, which covers the records of all of them. A sync that fails fails each caller whose
//! record it was to cover, and none of them tries again
//! (`storage_impl::tests::a_failed_group_sync_fails_each_commit_it_was_to_cover`). Sixteen threads that end a transaction
//! while one sync is held back share at most two syncs
//! (`storage_impl::tests::sixteen_concurrent_commits_share_two_syncs`), and a commit whose
//! record is not covered yet has not returned
//! (`storage_impl::tests::no_commit_returns_before_its_record_is_synced`).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use vauban_errors::InternalError;

use crate::{TableId, TxnId};

use super::DiskStorage;
use super::buffer::DurableLsn;
use super::crc::crc32;
use super::page::{Lsn, PAGE_SIZE, Page, PageId, PageKind};

/// Value written in the `version` field of a record by this build.
pub(crate) const RECORD_VERSION: u8 = 1;

/// Size of the `length` field that opens a record, in bytes.
const LENGTH_LEN: usize = 4;

/// Size of the fixed fields that follow `length`: version, kind, `lsn`, `prev_lsn`, `txn`.
const HEADER_LEN: usize = 1 + 1 + 8 + 8 + 8;

/// Size of the checksum that closes a record, in bytes.
const CHECKSUM_LEN: usize = 4;

/// Bytes a record occupies on disk beside its payload: `length`, fixed fields and checksum.
const FRAME_LEN: usize = LENGTH_LEN + HEADER_LEN + CHECKSUM_LEN;

/// Largest record this build writes or reads, in bytes on disk, payload included: 1 MiB.
pub(crate) const MAX_RECORD_LEN: usize = 1 << 20;

/// Largest payload a record may carry: [`MAX_RECORD_LEN`] less the frame around it.
pub(crate) const MAX_PAYLOAD_LEN: usize = MAX_RECORD_LEN - FRAME_LEN;

/// Smallest value the `length` field may carry: a record with an empty payload.
const MIN_LENGTH: u64 = (HEADER_LEN + CHECKSUM_LEN) as u64;

/// What a record says happened, stored as one byte at offset 5.
///
/// The byte values are part of the on-disk format: a new kind takes a free value, an existing
/// one keeps its own. `0` is not a kind, so a run of zero bytes does not read as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub(crate) enum WalRecordKind {
    /// First record of a transaction.
    Begin = 1,
    /// A row version was inserted.
    Insert = 2,
    /// A row version was replaced.
    Update = 3,
    /// A row version was deleted.
    Delete = 4,
    /// The transaction committed; the record before which `commit` does not return.
    Commit = 5,
    /// The transaction was rolled back.
    Abort = 6,
    /// Checkpoint: the pages of the committed transactions are on disk.
    Checkpoint = 7,
    /// A database was created.
    CreateDatabase = 8,
    /// A database was dropped.
    DropDatabase = 9,
    /// A table was created.
    CreateTable = 10,
    /// A table was dropped.
    DropTable = 11,
    /// An index was created.
    CreateIndex = 12,
    /// An index was dropped.
    DropIndex = 13,
    /// A savepoint was taken.
    Savepoint = 14,
    /// The transaction was rolled back to a savepoint.
    RollbackTo = 15,
    /// The undo of a rollback, or of a rollback to a savepoint, took a version out of a heap
    /// slot: the version left the heap, or had its `xmax` unset in place. The
    /// payload is a [`super::wal_payload::SlotRelease`].
    Compensation = 16,
    /// A vacuum took a version out of a heap slot. The payload is a
    /// [`super::wal_payload::SlotRelease`].
    Purge = 17,
    /// Bytes of one or more pages, as a write left them: the pages of a tree a write changed,
    /// the formatting and the link of a page a heap grows by, the pages of an overflow chain,
    /// a page the allocator frees, the pages of the catalogue. The payload is a [`PageWrite`].
    PageWrite = 18,
}

impl WalRecordKind {
    /// Reads a kind from its stored byte.
    ///
    /// A byte outside the enum is data corruption, not a panic: the error names the value
    /// that was read.
    pub(crate) fn from_byte(raw: u8) -> Result<Self, InternalError> {
        match raw {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Insert),
            3 => Ok(Self::Update),
            4 => Ok(Self::Delete),
            5 => Ok(Self::Commit),
            6 => Ok(Self::Abort),
            7 => Ok(Self::Checkpoint),
            8 => Ok(Self::CreateDatabase),
            9 => Ok(Self::DropDatabase),
            10 => Ok(Self::CreateTable),
            11 => Ok(Self::DropTable),
            12 => Ok(Self::CreateIndex),
            13 => Ok(Self::DropIndex),
            14 => Ok(Self::Savepoint),
            15 => Ok(Self::RollbackTo),
            16 => Ok(Self::Compensation),
            17 => Ok(Self::Purge),
            18 => Ok(Self::PageWrite),
            other => Err(InternalError::Corruption(format!(
                "unknown wal record kind {other}"
            ))),
        }
    }

    /// The byte written at offset 5 of a record.
    pub(crate) fn as_byte(self) -> u8 {
        self as u8
    }
}

/// One record of the journal, as [`Wal::append`] wrote it or as [`Wal::iter`] read it back.
///
/// The payload is opaque here: what a kind puts in it is written by the module that emits
/// that kind ([`super::wal_payload`] for the row records, [`super::meta`] for the DDL ones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WalRecord {
    /// What happened.
    pub(crate) kind: WalRecordKind,
    /// Number of this record, from 1 upwards.
    pub(crate) lsn: Lsn,
    /// Number of the record before it, `Lsn(0)` in the first record of the journal.
    pub(crate) prev_lsn: Lsn,
    /// Transaction the record belongs to, `TxnId(0)` when it belongs to none.
    pub(crate) txn: TxnId,
    /// Payload, `length` − 30 bytes on disk.
    pub(crate) payload: Vec<u8>,
}

impl WalRecord {
    /// The bytes of the record, checksum included, as the format table describes them.
    fn encode(&self) -> Vec<u8> {
        let length = HEADER_LEN + self.payload.len() + CHECKSUM_LEN;
        let mut bytes = Vec::with_capacity(LENGTH_LEN + length);
        // The caller checked the payload against `MAX_PAYLOAD_LEN`, so `length` fits.
        bytes.extend_from_slice(&(length as u32).to_le_bytes());
        bytes.push(RECORD_VERSION);
        bytes.push(self.kind.as_byte());
        bytes.extend_from_slice(&self.lsn.0.to_le_bytes());
        bytes.extend_from_slice(&self.prev_lsn.0.to_le_bytes());
        bytes.extend_from_slice(&self.txn.0.to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        let checksum = crc32(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes
    }
}

/// Bytes the payload of a [`WalRecordKind::Checkpoint`] record occupies: three `u64`.
pub(crate) const CHECKPOINT_PAYLOAD_LEN: usize = 24;

/// The payload of a [`WalRecordKind::Checkpoint`] record.
///
/// # Layout, little-endian
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0 | 8 | `redo_lsn` |
/// | 8 | 8 | `next_page_id`, copied from `vauban.ctl` |
/// | 16 | 8 | `next_row_id`, copied from `vauban.ctl` |
///
/// `redo_lsn` is the number of the checkpoint record itself: at the moment the record is
/// written, what the checkpoint flushed to
/// `data` is the pages of the transactions that had committed ([`super::DiskStorage::checkpoint`]).
/// The two counters are the copy of the control block the checkpoint then writes, so a
/// recovery that reads the journal reads the counters as they stood at that record rather
/// than as `vauban.ctl` holds them (`checkpoint_record_in_wal_and_ctl` reads the two back and
/// compares them to the control block).
///
/// # What this number does not say
///
/// `redo_lsn` marks where the checkpoint happened; it is not by itself the number a redo may
/// start from. A transaction that began before the record and committed after it has its row
/// records **behind** `redo_lsn` while its page stayed in the pool: one insert
/// astride one checkpoint gives `Insert` at `Lsn(2)`, `Checkpoint` at `Lsn(3)`, `Commit` at
/// `Lsn(4)`, and the page of that insert out of `data` after the commit
/// (`a_transaction_astride_the_checkpoint_leaves_its_insert_before_redo_lsn`, in
/// [`super`]). Replaying the records after `Lsn(3)` alone would leave that row out. The
/// journal is kept whole — this build does not truncate it — so the records before the
/// checkpoint are still there to be read, and where a recovery begins its scan is fixed by
/// [`super::recover`], not by this field.
///
/// The list of the transactions in progress is left out: under no-steal a page a running
/// transaction wrote stays in the pool ([`super::buffer::BufferPool::flush_all`] skips it),
/// so what the checkpoint put on `data` was written by transactions that had committed
/// (`checkpoint_skips_in_progress`, in [`super`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckpointPayload {
    /// Number of the checkpoint record itself; where a redo starts is fixed by
    /// [`super::recover`], which reads this number rather than takes it for its floor (see
    /// above).
    pub(crate) redo_lsn: Lsn,
    /// `next_page_id` of the control block when the record was written.
    pub(crate) next_page_id: PageId,
    /// `next_row_id` of the control block when the record was written.
    pub(crate) next_row_id: u64,
}

impl CheckpointPayload {
    /// The 24 bytes of the payload, as the layout table describes them.
    pub(crate) fn encode(&self) -> [u8; CHECKPOINT_PAYLOAD_LEN] {
        let mut bytes = [0u8; CHECKPOINT_PAYLOAD_LEN];
        bytes[0..8].copy_from_slice(&self.redo_lsn.0.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.next_page_id.0.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.next_row_id.to_le_bytes());
        bytes
    }

    /// Reads a payload back.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] naming the length that was read for a payload that is
    /// not [`CHECKPOINT_PAYLOAD_LEN`] bytes long
    /// (`a_checkpoint_payload_of_another_length_is_corruption`).
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        let bytes: [u8; CHECKPOINT_PAYLOAD_LEN] = bytes.try_into().map_err(|_| {
            InternalError::Corruption(format!(
                "a checkpoint record carries {} payload bytes, expected {CHECKPOINT_PAYLOAD_LEN}",
                bytes.len()
            ))
        })?;
        Ok(Self {
            redo_lsn: Lsn(read_u64(&bytes, 0)),
            next_page_id: PageId(read_u64(&bytes, 8)),
            next_row_id: read_u64(&bytes, 16),
        })
    }
}

// ---------------------------------------------------------------------------- Page writes
//
// A change of structure is logged as the bytes it leaves in its pages. The section below holds
// the payload of such a record ([`PageWrite`]), the set of page copies a change is computed on
// before anything reaches the pool ([`PageSet`]), and the redo of the record by the `page_lsn`
// of each page it names ([`PageWrite::redo`]).

/// Bytes of the common header a page write leaves out: the `page_lsn` (16..24), which the
/// record that carries the write sets, and the checksum (24..28), which the pool computes when
/// it seals the page ([`Page`] lays the header out).
const UNLOGGED_HEADER: std::ops::Range<usize> = 16..28;

/// Gap, in bytes, below which two changed ranges of one page are logged as one range: a range
/// costs four bytes of offset and length, so a shorter gap costs less carried than cut.
const MERGE_GAP: usize = 4;

/// Largest number of pages one [`PageSet`] of an overflow chain logs in one record, so that
/// the record stays under [`MAX_PAYLOAD_LEN`]: 64 whole pages of 8 180 bytes each.
pub(crate) const PAGES_PER_RECORD: usize = 64;

/// Which tree of a clustered table a [`RootMove`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreeRole {
    /// The tree of the versions.
    Versions = 1,
    /// The directory from a row to the key of its newest version.
    Directory = 2,
}

/// The root of a tree of a clustered table moved: the split that reached the old root gave
/// the tree a new one. The record of that split carries the move, and the recovery hands it to
/// the catalogue ([`super::recover`]), so the tree is reached from the root its pages describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RootMove {
    /// The table the tree belongs to.
    pub(crate) table: TableId,
    /// Which of its two trees.
    pub(crate) role: TreeRole,
    /// The new root.
    pub(crate) root: PageId,
}

/// The bytes one page of a [`PageWrite`] changed, by range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageDelta {
    /// The page written.
    pub(crate) page: PageId,
    /// The kind byte the page carries once written, which [`super::meta::load`] reads to redo
    /// the pages of the catalogue ahead of the others.
    pub(crate) kind: u8,
    /// Whether the ranges cover the page, its `page_lsn` and its checksum apart, the bytes they
    /// leave out being zeros: the page was a free one, and its redo writes it without reading
    /// what `data` holds there.
    pub(crate) whole: bool,
    /// The changed bytes: an offset in the page and the bytes found there after the write.
    pub(crate) ranges: Vec<(u16, Vec<u8>)>,
}

/// The payload of a [`WalRecordKind::PageWrite`] record.
///
/// # Layout, little-endian
///
/// | Size | Field |
/// |---|---|
/// | 2 | number of pages, then for each page: |
/// | 8 | the [`PageId`] |
/// | 1 | the kind byte of the page once written |
/// | 1 | 1 when the ranges cover the whole page, 0 otherwise |
/// | 2 | number of ranges, then for each range a `u16` offset, a `u16` length and the bytes |
/// | 1 | 1 when a [`RootMove`] follows, 0 otherwise |
/// | 13 | the move: `u32` table, `u8` role, `u64` root |
///
/// The ranges of a page leave out its `page_lsn` and its checksum: the redo sets the first to
/// the number of the record and the pool computes the second
/// (`tests::a_page_write_reads_back_and_leaves_the_lsn_and_the_checksum_out`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PageWrite {
    /// The pages, in the order the write changed them first.
    pub(crate) pages: Vec<PageDelta>,
    /// The root a split moved, when it moved one.
    pub(crate) root: Option<RootMove>,
}

impl PageWrite {
    /// The bytes of the payload, in the layout above.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // A write changes a handful of pages; `PageSet` bounds an overflow chain to
        // `PAGES_PER_RECORD` of them per record.
        out.extend_from_slice(&(self.pages.len() as u16).to_le_bytes());
        for delta in &self.pages {
            out.extend_from_slice(&delta.page.0.to_le_bytes());
            out.push(delta.kind);
            out.push(u8::from(delta.whole));
            // A page holds 8 192 bytes, so a page has fewer ranges than a `u16` counts, and
            // an offset and a length fit one.
            out.extend_from_slice(&(delta.ranges.len() as u16).to_le_bytes());
            for (at, bytes) in &delta.ranges {
                out.extend_from_slice(&at.to_le_bytes());
                out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
        match self.root {
            None => out.push(0),
            Some(moved) => {
                out.push(1);
                out.extend_from_slice(&moved.table.0.to_le_bytes());
                out.push(moved.role as u8);
                out.extend_from_slice(&moved.root.0.to_le_bytes());
            }
        }
        out
    }

    /// Reads a payload back.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a payload cut short of what it announces, for a range
    /// that leaves the page, for bytes left past the end, and for a role byte other than 1
    /// and 2 (`tests::a_page_write_cut_short_is_corruption`).
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        let mut at = 0usize;
        let pages = usize::from(take_u16(bytes, &mut at)?);
        let mut out = Self::default();
        for _ in 0..pages {
            let page = PageId(take_u64(bytes, &mut at)?);
            let kind = take(bytes, &mut at, 1)?[0];
            let whole = take(bytes, &mut at, 1)?[0] == 1;
            let count = usize::from(take_u16(bytes, &mut at)?);
            let mut ranges = Vec::with_capacity(count);
            for _ in 0..count {
                let offset = take_u16(bytes, &mut at)?;
                let len = usize::from(take_u16(bytes, &mut at)?);
                if usize::from(offset) + len > PAGE_SIZE {
                    return Err(InternalError::Corruption(format!(
                        "a page write names bytes {offset}..{} of page {page}, past the \
                         {PAGE_SIZE} of a page",
                        usize::from(offset) + len
                    )));
                }
                ranges.push((offset, take(bytes, &mut at, len)?.to_vec()));
            }
            out.pages.push(PageDelta {
                page,
                kind,
                whole,
                ranges,
            });
        }
        if take(bytes, &mut at, 1)?[0] == 1 {
            let table = TableId(take_u32(bytes, &mut at)?);
            let role = match take(bytes, &mut at, 1)?[0] {
                1 => TreeRole::Versions,
                2 => TreeRole::Directory,
                other => {
                    return Err(InternalError::Corruption(format!(
                        "a page write names the root of tree role {other}, this build reads 1 \
                         and 2"
                    )));
                }
            };
            let root = PageId(take_u64(bytes, &mut at)?);
            out.root = Some(RootMove { table, role, root });
        }
        if at != bytes.len() {
            return Err(InternalError::Corruption(format!(
                "a page write carries {} payload bytes past the {at} it reads",
                bytes.len() - at
            )));
        }
        Ok(out)
    }

    /// Replays the record of number `lsn` on the pages it names whose `page_lsn` is below
    /// it, and answers how many pages it wrote and how many it left as they stood.
    ///
    /// A page behind the record takes its ranges and the number `lsn`; a page at or past it
    /// already holds what the record wrote, and what came after. `only` restricts the redo to
    /// the pages of that kind once written, which is how the catalogue is brought up to date
    /// before it is read ([`super::meta::load`]); the others are left to the redo of
    /// [`super::recover`], which passes `None`.
    ///
    /// A page that does not read back — the zeros of a file grown by the allocator, a page
    /// whose checksum does not hold — is written whole from the record when the record covers
    /// it whole ([`PageDelta::whole`]) and is an error otherwise.
    ///
    /// `on_data` holds the `page_lsn` each page carries in `data`, read once per page and
    /// kept by the caller across the records of one pass: a page whose number there covers the
    /// record is left alone without being read into the pool, so a redo over pages a
    /// checkpoint wrote leaves the pool as cold as it found it
    /// (`super::index::tests::shared_tree_uniqueness_reads_a_bounded_number_of_pages`). The
    /// pool holds a page at that number or past it, the redo only moving it forward.
    ///
    /// # Errors
    ///
    /// The errors of the buffer pool and of the data file.
    pub(crate) fn redo(
        &self,
        storage: &DiskStorage,
        lsn: Lsn,
        only: Option<PageKind>,
        on_data: &mut BTreeMap<PageId, Lsn>,
    ) -> Result<(usize, usize), InternalError> {
        let (mut applied, mut skipped) = (0, 0);
        for delta in &self.pages {
            if only.is_some_and(|kind| kind.as_byte() != delta.kind) {
                continue;
            }
            let stored = *on_data.entry(delta.page).or_insert_with(|| {
                storage
                    .data
                    .read_page(delta.page)
                    .map_or(Lsn(0), |page| page.lsn())
            });
            if stored >= lsn {
                skipped += 1;
                continue;
            }
            let pin = match storage.pool.pin(delta.page) {
                Ok(pin) => pin,
                Err(_) if delta.whole => {
                    write_whole(storage, delta, lsn)?;
                    applied += 1;
                    continue;
                }
                Err(err) => return Err(err),
            };
            if pin.with_page(Page::lsn)? >= lsn {
                skipped += 1;
                continue;
            }
            pin.with_page_mut(|page| {
                apply_delta(page, delta);
                page.set_lsn(lsn);
            })?;
            storage.pool.mark_dirty(delta.page, lsn)?;
            applied += 1;
        }
        Ok((applied, skipped))
    }
}

/// Writes the page of `delta` to `data` from the record alone, `page_lsn` `lsn`, growing the
/// file when it does not reach the page.
fn write_whole(storage: &DiskStorage, delta: &PageDelta, lsn: Lsn) -> Result<(), InternalError> {
    let mut page = Page::empty(PageKind::Free, delta.page);
    apply_delta(&mut page, delta);
    page.set_lsn(lsn);
    page.seal();
    if storage.data.page_count()? <= delta.page.0 {
        storage.data.extend_to(delta.page.0 + 1)?;
    }
    storage.data.write_page(delta.page, &page)
}

/// Copies the bytes of `ranges` into `page`.
fn apply_ranges(page: &mut Page, ranges: &[(u16, Vec<u8>)]) {
    for (at, bytes) in ranges {
        let at = usize::from(*at);
        page.0[at..at + bytes.len()].copy_from_slice(bytes);
    }
}

/// The ranges of `page` a whole write carries: the bytes of the page that are not zero, its
/// unlogged header bytes apart. The redo zeroes the page before it copies them
/// ([`apply_delta`]), so a fresh page of a few records costs a few records.
fn whole_ranges(page: &Page) -> Vec<(u16, Vec<u8>)> {
    changed_ranges(&Page([0u8; PAGE_SIZE]), page)
}

/// Writes `delta` into `page`: the page zeroed first, its unlogged header bytes apart, when
/// the delta covers it whole, then the bytes of its ranges.
fn apply_delta(page: &mut Page, delta: &PageDelta) {
    if delta.whole {
        page.0[..UNLOGGED_HEADER.start].fill(0);
        page.0[UNLOGGED_HEADER.end..].fill(0);
    }
    apply_ranges(page, &delta.ranges);
}

/// The ranges where `after` differs from `before`, the unlogged header bytes apart, two ranges
/// closer than [`MERGE_GAP`] carried as one.
fn changed_ranges(before: &Page, after: &Page) -> Vec<(u16, Vec<u8>)> {
    let differs = |at: usize| !UNLOGGED_HEADER.contains(&at) && before.0[at] != after.0[at];
    let mut out = Vec::new();
    let mut at = 0;
    while at < PAGE_SIZE {
        if !differs(at) {
            at += 1;
            continue;
        }
        let start = at;
        let mut end = at + 1;
        let mut probe = end;
        while probe < PAGE_SIZE && probe - end < MERGE_GAP && !UNLOGGED_HEADER.contains(&probe) {
            if differs(probe) {
                end = probe + 1;
            }
            probe += 1;
        }
        // `start` is below `PAGE_SIZE`, which fits a `u16`.
        out.push((start as u16, after.0[start..end].to_vec()));
        at = end;
    }
    out
}

/// The `len` bytes of `bytes` at `*at`, the cursor moved past them.
fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8], InternalError> {
    let slice = bytes.get(*at..at.saturating_add(len)).ok_or_else(|| {
        InternalError::Corruption(format!(
            "a page write carries {} payload bytes, too few for the {len} it announces at {}",
            bytes.len(),
            *at
        ))
    })?;
    *at += len;
    Ok(slice)
}

/// A little-endian `u16` read by [`take`].
fn take_u16(bytes: &[u8], at: &mut usize) -> Result<u16, InternalError> {
    let raw = take(bytes, at, 2)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

/// A little-endian `u32` read by [`take`].
fn take_u32(bytes: &[u8], at: &mut usize) -> Result<u32, InternalError> {
    Ok(read_u32(take(bytes, at, 4)?, 0))
}

/// A little-endian `u64` read by [`take`].
fn take_u64(bytes: &[u8], at: &mut usize) -> Result<u64, InternalError> {
    Ok(read_u64(take(bytes, at, 8)?, 0))
}

/// One page a [`PageSet`] holds: what the pool held when the set first wrote it, and what the
/// set made of it since.
#[derive(Debug)]
struct Staged {
    /// The page.
    id: PageId,
    /// Its bytes when the set first wrote it.
    before: Box<Page>,
    /// Its bytes as the set left them.
    after: Box<Page>,
}

/// The pages one change of structure writes, held as copies until the change is complete.
///
/// A split rewrites several pages, and a tree whose pages reached `data` from the middle of a
/// split is not a tree. The set therefore takes a copy of each page the first time it writes it
/// ([`PageSet::write`]), answers the reads of the change from those copies
/// ([`PageSet::read`]), and only then, at [`PageSet::commit`], logs what changed in one
/// [`WalRecordKind::PageWrite`] record and puts the bytes in the pool, each page taking the
/// number of that record in the same change as its bytes. Until that call the pool holds the
/// pages as they were before the change, so a flush in between writes nothing of it
/// (`tests::a_page_set_leaves_the_pool_alone_until_its_commit`); after it the record precedes
/// each page, which the pool does not write before the record is durable.
///
/// A crash keeps such a record in the journal or leaves it out, a torn tail being truncated
/// by [`Wal::open`]; the redo replays its pages by their `page_lsn` ([`PageWrite::redo`]).
///
/// A set built with `logged` false writes no record and leaves the `page_lsn` of its pages
/// where it stands: the trees of a non-clustered index, which the instance builds again from the
/// rows at each `open` (`DiskStorage::rebuild_indexes`).
///
/// The pages are written under the lock of the table they belong to, or of the catalogue for
/// its pages, which keeps another writer off them between the copy and the commit; a reader
/// of the pool reads the pages as they were until the commit.
#[derive(Debug)]
pub(crate) struct PageSet<'storage> {
    /// The instance the pages belong to.
    storage: &'storage DiskStorage,
    /// Whether the commit writes a record.
    logged: bool,
    /// The pages, in the order the set first wrote them.
    staged: Vec<Staged>,
    /// The root a split moved, carried by the record.
    root: Option<RootMove>,
}

impl<'storage> PageSet<'storage> {
    /// An empty set over the pages of `storage`, logged or not.
    pub(crate) fn new(storage: &'storage DiskStorage, logged: bool) -> Self {
        Self {
            storage,
            logged,
            staged: Vec::new(),
            root: None,
        }
    }

    /// The instance the pages belong to.
    pub(crate) fn storage(&self) -> &'storage DiskStorage {
        self.storage
    }

    /// Applies `read` to the page `id` as the set left it: its copy when the set wrote it, the
    /// page of the pool otherwise.
    ///
    /// # Errors
    ///
    /// The errors of the buffer pool.
    pub(crate) fn read<R>(
        &self,
        id: PageId,
        read: impl FnOnce(&Page) -> R,
    ) -> Result<R, InternalError> {
        if let Some(staged) = self.staged.iter().find(|staged| staged.id == id) {
            return Ok(read(&staged.after));
        }
        let pin = self.storage.pool.pin(id)?;
        pin.with_page(read)
    }

    /// Applies `change` to the copy of the page `id`, taking the copy from the pool the first
    /// time the set writes the page.
    ///
    /// # Errors
    ///
    /// The errors of the buffer pool.
    pub(crate) fn write<R>(
        &mut self,
        id: PageId,
        change: impl FnOnce(&mut Page) -> R,
    ) -> Result<R, InternalError> {
        let at = match self.staged.iter().position(|staged| staged.id == id) {
            Some(at) => at,
            None => {
                let pin = self.storage.pool.pin(id)?;
                let before = Box::new(pin.with_page(Page::clone)?);
                drop(pin);
                self.staged.push(Staged {
                    id,
                    after: before.clone(),
                    before,
                });
                self.staged.len() - 1
            }
        };
        Ok(change(&mut self.staged[at].after))
    }

    /// Carries `moved` in the record of the commit.
    pub(crate) fn note_root(&mut self, moved: RootMove) {
        self.root = Some(moved);
    }

    /// Logs what the set changed and puts it in the pool, then hands each page it changed to
    /// `on_page`, the page still pinned; answers the number of the record, `None` when the set
    /// changed nothing or is not logged.
    ///
    /// A page the set copied from a free page is logged whole, so that its redo does not depend
    /// on what `data` holds there; any other page by the ranges it changed. Each page takes the
    /// bytes of those ranges and the number of the record under one change of the pool, and is
    /// marked dirty at that number (at `Lsn(0)` in a set that is not logged). The pages logged
    /// whole are put in the pool first, then the others in the order the set first wrote them.
    ///
    /// # Errors
    ///
    /// The errors of the journal — a record past [`MAX_PAYLOAD_LEN`] among them — of the
    /// buffer pool and of `on_page`.
    pub(crate) fn commit(
        self,
        on_page: impl FnMut(PageId) -> Result<(), InternalError>,
    ) -> Result<Option<Lsn>, InternalError> {
        self.install(None, on_page)
    }

    /// Logs what the set changed as [`PageSet::commit`] does, but leaves each page it changed
    /// at the number `behind`, a record the journal holds durably, rather than at the number
    /// of its own record.
    ///
    /// The pool may then write the pages before their record is durable, so this is for a
    /// change a crash may leave on `data` without its record: the counters and the roots a
    /// commit rewrites in place in the pages of the catalogue, eight bytes each, which the
    /// recovery takes from the journal (`super::meta::store`). The record keeps the pages
    /// described for the changes that follow them: a later record is computed from the pages
    /// this one left, and the redo replays the two in order
    /// (`super::meta::tests::a_catalogue_of_the_same_length_is_rewritten_in_place_without_a_sync`).
    ///
    /// # Errors
    ///
    /// Those of [`PageSet::commit`].
    pub(crate) fn commit_behind(
        self,
        behind: Lsn,
        on_page: impl FnMut(PageId) -> Result<(), InternalError>,
    ) -> Result<Option<Lsn>, InternalError> {
        self.install(Some(behind), on_page)
    }

    /// The body of [`PageSet::commit`] and [`PageSet::commit_behind`]: the pages take
    /// `behind` when it is given, the number of the record otherwise.
    fn install(
        self,
        behind: Option<Lsn>,
        mut on_page: impl FnMut(PageId) -> Result<(), InternalError>,
    ) -> Result<Option<Lsn>, InternalError> {
        let mut deltas = Vec::new();
        for staged in &self.staged {
            let whole = matches!(staged.before.kind(), Ok(PageKind::Free))
                && !matches!(staged.after.kind(), Ok(PageKind::Free));
            let ranges = if whole {
                whole_ranges(&staged.after)
            } else {
                changed_ranges(&staged.before, &staged.after)
            };
            if ranges.is_empty() {
                continue;
            }
            deltas.push(PageDelta {
                page: staged.id,
                kind: staged.after.0[OFF_KIND],
                whole,
                ranges,
            });
        }
        if deltas.is_empty() {
            return Ok(None);
        }
        // A page the change cannot take — one the calling thread holds a second pin on — is
        // refused before the record exists, so the journal holds no change the pool did not
        // take ([`super::buffer::PinGuard::with_page_mut`]).
        for delta in &deltas {
            let pin = self.storage.pool.pin(delta.page)?;
            pin.with_page_mut(|_| ())?;
        }
        // The pages written from a free one go first: a page that names one of them reaches
        // the pool after it.
        deltas.sort_by_key(|delta| !delta.whole);
        let lsn = if self.logged {
            let payload = PageWrite {
                pages: deltas.clone(),
                root: self.root,
            }
            .encode();
            Some(
                self.storage
                    .wal
                    .append(WalRecordKind::PageWrite, TxnId(0), &payload)?,
            )
        } else {
            None
        };
        let page_lsn = behind.or(lsn).unwrap_or(Lsn(0));
        for delta in &deltas {
            let pin = self.storage.pool.pin(delta.page)?;
            pin.with_page_mut(|page| {
                apply_delta(page, delta);
                if page_lsn > page.lsn() {
                    page.set_lsn(page_lsn);
                }
            })?;
            self.storage.pool.mark_dirty(delta.page, page_lsn)?;
            on_page(delta.page)?;
            drop(pin);
        }
        Ok(lsn)
    }
}

/// Offset of the kind byte in the common header of a page ([`Page`]).
const OFF_KIND: usize = 5;

/// What the bytes at a given offset amount to, as [`frame`] read them.
///
/// The four failing variants carry the sentence that describes the failure rather than a built
/// [`InternalError`]: the caller decides whether it becomes a [`InternalError::Corruption`] on
/// its own, gets the offset of the sealed record found after it appended to it, or is dropped
/// along with the tail it describes.
#[derive(Debug)]
enum Framed {
    /// A whole record whose checksum, version and kind check out; `len` is its size on disk.
    Whole {
        /// The record itself.
        record: WalRecord,
        /// Bytes it occupies, `length` field included.
        len: u64,
    },
    /// A record whose bytes are all there but whose checksum disagrees. Its `length` is covered
    /// by that checksum, so it is no boundary to step over and is not carried out of here.
    Damaged {
        /// The mismatch, as a sentence.
        reason: String,
    },
    /// A whole record with an agreeing checksum that this build will not read.
    Rejected {
        /// What this build refused, as a sentence.
        reason: String,
    },
    /// Fewer bytes than the record announces. Whether that is a tail an append left behind or
    /// an interior record is decided by what lies after it, by the caller.
    Torn {
        /// What was announced against what is there, as a sentence.
        reason: String,
    },
    /// A `length` field that frames no record, so there is nothing to step over. Which of the
    /// two cases it is, again, is decided by what lies after it.
    Unframed {
        /// The length that was read, as a sentence.
        reason: String,
    },
}

/// Reads the record that starts at `offset`, given the `remaining` bytes of the region.
///
/// `remaining` bounds the read: the caller passes the bytes between `offset` and the end of
/// the file ([`Wal::open`]) or the end of the last record it knows about ([`Wal::iter`]).
fn frame<R: Read>(reader: &mut R, offset: u64, remaining: u64) -> Result<Framed, InternalError> {
    if remaining < LENGTH_LEN as u64 {
        return Ok(Framed::Torn {
            reason: format!(
                "offset {offset} of the journal holds {remaining} bytes, fewer than the \
                 {LENGTH_LEN} of a length field"
            ),
        });
    }
    let mut length_bytes = [0u8; LENGTH_LEN];
    reader.read_exact(&mut length_bytes)?;
    let length = u64::from(u32::from_le_bytes(length_bytes));
    let len = LENGTH_LEN as u64 + length;
    if length < MIN_LENGTH || len > MAX_RECORD_LEN as u64 {
        return Ok(Framed::Unframed {
            reason: format!(
                "the record at offset {offset} of the journal announces {length} bytes, \
                 outside the {MIN_LENGTH}..={} a record carries",
                MAX_RECORD_LEN - LENGTH_LEN
            ),
        });
    }
    if remaining < len {
        return Ok(Framed::Torn {
            reason: format!(
                "the record at offset {offset} of the journal announces {len} bytes, the \
                 journal holds {remaining} from there"
            ),
        });
    }
    // `len` is at most `MAX_RECORD_LEN`, checked just above.
    let mut bytes = vec![0u8; len as usize];
    bytes[..LENGTH_LEN].copy_from_slice(&length_bytes);
    reader.read_exact(&mut bytes[LENGTH_LEN..])?;

    let split = bytes.len() - CHECKSUM_LEN;
    let stored = read_u32(&bytes, split);
    let computed = crc32(&bytes[..split]);
    if stored != computed {
        return Ok(Framed::Damaged {
            reason: format!(
                "the record at offset {offset} of the journal carries checksum \
                 {stored:#010x}, its bytes hash to {computed:#010x}"
            ),
        });
    }
    let version = bytes[LENGTH_LEN];
    if version != RECORD_VERSION {
        return Ok(Framed::Rejected {
            reason: format!(
                "the record at offset {offset} of the journal declares format version \
                 {version}, this build reads {RECORD_VERSION}"
            ),
        });
    }
    let kind_byte = bytes[LENGTH_LEN + 1];
    let Ok(kind) = WalRecordKind::from_byte(kind_byte) else {
        return Ok(Framed::Rejected {
            reason: format!(
                "the record at offset {offset} of the journal carries unknown wal record \
                 kind {kind_byte}"
            ),
        });
    };
    Ok(Framed::Whole {
        record: WalRecord {
            kind,
            lsn: Lsn(read_u64(&bytes, LENGTH_LEN + 2)),
            prev_lsn: Lsn(read_u64(&bytes, LENGTH_LEN + 10)),
            txn: TxnId(read_u64(&bytes, LENGTH_LEN + 18)),
            payload: bytes[LENGTH_LEN + HEADER_LEN..split].to_vec(),
        },
        len,
    })
}

/// The error of a record at `at` that cannot be read while a record whose checksum agrees
/// starts at `sealed`.
///
/// Names both offsets: the one that cannot be read, and the one that makes it an interior record
/// rather than the tail of an append that did not finish.
fn interior(at: u64, reason: &str, sealed: u64) -> InternalError {
    InternalError::Corruption(format!(
        "{reason}; the record at offset {sealed} of the journal carries a checksum that agrees, \
         so offset {at} holds an interior record and not the tail of an append that did not \
         finish"
    ))
}

/// Reads a little-endian `u32` at `at`.
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Reads a little-endian `u64` at `at`.
fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
        bytes[at + 4],
        bytes[at + 5],
        bytes[at + 6],
        bytes[at + 7],
    ])
}

/// What the scan of [`Wal::open`] found at the end of the file.
#[derive(Debug)]
struct Tail {
    /// Offset just past the last record that checks out, where the next append goes.
    end: u64,
    /// Number of that record, `Lsn(0)` when the journal holds none.
    last_lsn: Lsn,
    /// How many records the scan read.
    records: u64,
}

/// The journal of an on-disk instance, `<dir>/wal`.
///
/// The file is append-only: a record is written at the end and is not rewritten afterwards.
/// The structure holds the offset where the next record goes, the number of the last record
/// and the number of the last record made durable by [`Wal::flush`].
///
/// Dropping a `Wal` does not flush it, on purpose: see the durability section of the module.
///
/// One `Wal` is written through `&mut self`, so two appends cannot interleave through the same
/// handle; sharing the journal between threads is the business of [`WalHandle`].
#[derive(Debug)]
pub(crate) struct Wal {
    /// Path of the file, kept for the error messages and for [`Wal::iter`].
    path: PathBuf,
    /// Open handle, readable and writable.
    file: File,
    /// Offset where the next record goes: past the last record that checks out.
    end: u64,
    /// Number of the last record, `Lsn(0)` when the journal holds none.
    last_lsn: Lsn,
    /// Number of the last record a `sync_all` covered, `Lsn(0)` when none.
    durable_lsn: Lsn,
    /// How many records the file holds.
    records: u64,
    /// How many `sync_all` calls made records durable since the journal was opened.
    #[cfg(test)]
    syncs: u64,
}

impl Wal {
    /// Opens the journal at `path`, creating an empty file when there is none.
    ///
    /// An existing file is scanned from its first byte: the records are read in order, their
    /// checksum, version, kind and numbering are checked, and the handle is left ready to
    /// append past the last record that checks out. A tail that an unfinished append left
    /// behind is truncated away; a record that cannot be read while a record whose checksum
    /// agrees starts after it, and a record this build will not read, are both
    /// [`InternalError::Corruption`]. Which is which is the subject of the module
    /// documentation.
    ///
    /// The call ends with a `sync_all`, so a truncation it performed is covered by that
    /// `sync_all` before it returns.
    ///
    /// # Errors
    ///
    /// A failure reported by the operating system is [`InternalError::Io`]. A record that is
    /// whole and checksummed but that this build will not read is
    /// [`InternalError::Corruption`], and so is a record that cannot be read at all while a
    /// record whose checksum agrees starts after it.
    pub(crate) fn open(path: &Path) -> Result<Self, InternalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let tail = Self::scan(&file)?;
        let size = file.metadata()?.len();
        if tail.end < size {
            file.set_len(tail.end)?;
        }
        file.sync_all()?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            end: tail.end,
            last_lsn: tail.last_lsn,
            durable_lsn: tail.last_lsn,
            records: tail.records,
            #[cfg(test)]
            syncs: 0,
        })
    }

    /// Reads the records of `file` in order and reports where the good ones end.
    ///
    /// A failure is held back rather than returned at once, because a record that cannot be
    /// read is an interior record when sealed bytes follow it and the tail of an unfinished
    /// append when none do. The three failures are looked after the same way: the sequential
    /// reading stops on the frame that failed, because none of the three leaves a boundary the
    /// scan may trust — a `length` that frames no record and one that overshoots the file leave
    /// none at all, and a `length` whose record fails its checksum is covered by that very
    /// checksum, so stepping over it can land past whole records. What follows is then looked
    /// for offset by offset, by [`Wal::sealed_record_after`].
    fn scan(file: &File) -> Result<Tail, InternalError> {
        let size = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(0))?;
        let mut offset = 0u64;
        let mut tail = Tail {
            end: 0,
            last_lsn: Lsn(0),
            records: 0,
        };
        let mut unreadable: Option<(u64, String)> = None;
        while offset < size {
            match frame(&mut reader, offset, size - offset)? {
                Framed::Whole { record, len } => {
                    if record.lsn.0 != tail.last_lsn.0 + 1 || record.prev_lsn != tail.last_lsn {
                        return Err(InternalError::Corruption(format!(
                            "the record at offset {offset} of the journal carries lsn \
                             {} after lsn {} and names {} as the record before it; the \
                             journal numbers its records from 1 upwards, each naming the one \
                             before it",
                            record.lsn, tail.last_lsn, record.prev_lsn
                        )));
                    }
                    tail.last_lsn = record.lsn;
                    tail.records += 1;
                    offset += len;
                    tail.end = offset;
                }
                Framed::Rejected { reason } => return Err(InternalError::Corruption(reason)),
                // None of the three leaves a boundary to resume on, so the sequential reading
                // stops here and what follows is looked for offset by offset, below.
                Framed::Damaged { reason }
                | Framed::Torn { reason }
                | Framed::Unframed { reason } => {
                    unreadable = Some((offset, reason));
                    break;
                }
            }
        }
        // The reader is done with: the walk below moves the position of the shared handle.
        drop(reader);
        if let Some((at, reason)) = unreadable
            && let Some(sealed) = Self::sealed_record_after(file, at, size)?
        {
            return Err(interior(at, &reason, sealed));
        }
        Ok(tail)
    }

    /// Offset of the first record whose checksum agrees that starts after `at`, if the file
    /// holds one.
    ///
    /// Called when the record at `at` could not be read: the scan has no boundary left it may
    /// trust, so each offset from `at + 1` to the end of the file is tried in turn and the first
    /// one that frames a record whose checksum agrees is returned. A record this build will not
    /// read ([`Framed::Rejected`]: another version, a kind outside the enum) counts, because the
    /// question asked here is whether something was written whole behind `at`, not whether this
    /// build can use it; truncating over such bytes would throw away a record that reached the
    /// disk entire. Rejecting one offset costs the four bytes of a length field when that length
    /// frames no record, and as much as the record it announces when it frames one.
    ///
    /// Bytes found this way make the record at `at` an interior one. The other way round, a
    /// false find would take a checksum that agrees over bytes that are not a record boundary —
    /// a payload holding an encoded record does exactly that; outside that case it is what the
    /// CRC-32 makes unlikely, which nothing in this crate quantifies.
    fn sealed_record_after(file: &File, at: u64, size: u64) -> Result<Option<u64>, InternalError> {
        let mut handle = file;
        let mut candidate = at + 1;
        while candidate < size {
            handle.seek(SeekFrom::Start(candidate))?;
            match frame(&mut handle, candidate, size - candidate)? {
                Framed::Whole { .. } | Framed::Rejected { .. } => return Ok(Some(candidate)),
                Framed::Damaged { .. } | Framed::Torn { .. } | Framed::Unframed { .. } => {}
            }
            candidate += 1;
        }
        Ok(None)
    }

    /// Path of the journal.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Offset where the next record goes: the end of the last record this `Wal` knows about.
    ///
    /// It is the size of the file for a journal only this `Wal` writes to. Bytes that reached the
    /// file another way sit past it, and the test `iter_stops_at_the_records_the_wal_knows_about`
    /// holds such a file: 68 bytes long for an `append_offset` of 34.
    pub(crate) fn append_offset(&self) -> u64 {
        self.end
    }

    /// Number of the last record, `Lsn(0)` when the journal holds none.
    pub(crate) fn last_lsn(&self) -> Lsn {
        self.last_lsn
    }

    /// Number of the last record a `sync_all` covered, `Lsn(0)` when none.
    ///
    /// A `Wal` that was just opened reports the last record of the file: [`Wal::open`] ends
    /// with a `sync_all`.
    pub(crate) fn durable_lsn(&self) -> Lsn {
        self.durable_lsn
    }

    /// How many records the journal holds.
    pub(crate) fn records(&self) -> u64 {
        self.records
    }

    /// Appends a record of kind `kind` for the transaction `txn` and returns its number.
    ///
    /// `txn` is `TxnId(0)` for a record that belongs to no transaction. The record is written
    /// where the last one ended; this call does not `sync_all`, so what it wrote can be lost
    /// by a crash until [`Wal::flush`] returns.
    ///
    /// # Errors
    ///
    /// A payload longer than [`MAX_PAYLOAD_LEN`] is [`InternalError::Bug`]: the caller is
    /// expected to split what it logs, and a record is bounded so that a scan can size its
    /// buffer from the `length` field. A failure reported by the operating system is
    /// [`InternalError::Io`], and it may leave a partial record at the end of the file; the
    /// next [`Wal::open`] truncates it away.
    pub(crate) fn append(
        &mut self,
        kind: WalRecordKind,
        txn: TxnId,
        payload: &[u8],
    ) -> Result<Lsn, InternalError> {
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(InternalError::Bug(format!(
                "a wal record of kind {kind:?} carries {} payload bytes, more than the \
                 {MAX_PAYLOAD_LEN} that fit in a record of {MAX_RECORD_LEN} bytes",
                payload.len()
            )));
        }
        let lsn = Lsn(self.last_lsn.0.checked_add(1).ok_or_else(|| {
            InternalError::Bug(format!(
                "the journal {} has handed out lsn {}, the largest a u64 holds",
                self.path.display(),
                u64::MAX
            ))
        })?);
        let record = WalRecord {
            kind,
            lsn,
            prev_lsn: self.last_lsn,
            txn,
            payload: payload.to_vec(),
        };
        let bytes = record.encode();
        let mut handle = &self.file;
        handle.seek(SeekFrom::Start(self.end))?;
        handle.write_all(&bytes)?;
        self.end += bytes.len() as u64;
        self.last_lsn = lsn;
        self.records += 1;
        Ok(lsn)
    }

    /// Appends a [`WalRecordKind::Checkpoint`] record whose payload names the number of that
    /// very record, along with the two counters, and answers the number.
    ///
    /// The number is taken from the one of the last record before the payload is built; the
    /// `&mut self` of this call is what keeps a second append from slipping in between. The
    /// record is checked against that prediction and a disagreement is [`InternalError::Bug`],
    /// the record already being in the file at that point
    /// (`the_payload_of_a_checkpoint_names_the_record_itself`).
    ///
    /// The call does not `sync_all`; [`WalHandle::append_checkpoint`] is the one that does.
    ///
    /// # Errors
    ///
    /// Those of [`Wal::append`], the overflow of the numbering included.
    fn append_checkpoint(
        &mut self,
        next_page_id: PageId,
        next_row_id: u64,
    ) -> Result<Lsn, InternalError> {
        // Saturating: a journal sitting at `u64::MAX` is refused by `append` below, which is
        // where the numbering overflow is reported.
        let expected = Lsn(self.last_lsn.0.saturating_add(1));
        let payload = CheckpointPayload {
            redo_lsn: expected,
            next_page_id,
            next_row_id,
        };
        let lsn = self.append(WalRecordKind::Checkpoint, TxnId(0), &payload.encode())?;
        if lsn != expected {
            return Err(InternalError::Bug(format!(
                "the checkpoint record of journal {} was numbered {lsn} while its payload \
                 names {expected}",
                self.path.display()
            )));
        }
        Ok(lsn)
    }

    /// Makes the records written so far durable and moves [`Wal::durable_lsn`] to the last of
    /// them; this is the call the contract "durable when `flush` returns" rests on.
    ///
    /// `sync_all` covers the data and the metadata of the file, the size the appends grew it to
    /// being metadata. `sync_all` is used rather than `sync_data`; what a given file system
    /// loses with `sync_data` alone is not asserted anywhere in this crate.
    pub(crate) fn flush(&mut self) -> Result<(), InternalError> {
        self.file.sync_all()?;
        self.note_sync(self.last_lsn);
        Ok(())
    }

    /// Records that a `sync_all` covered the records up to `covered`.
    ///
    /// The durable LSN does not go backwards: a group sync that read its target before a
    /// [`Wal::flush`] moved the LSN further leaves it where the flush put it.
    fn note_sync(&mut self, covered: Lsn) {
        if covered > self.durable_lsn {
            self.durable_lsn = covered;
        }
        #[cfg(test)]
        {
            self.syncs += 1;
        }
    }

    /// How many `sync_all` calls made records durable since the journal was opened, the one
    /// of [`Wal::open`] left out.
    #[cfg(test)]
    pub(crate) fn syncs(&self) -> u64 {
        self.syncs
    }

    /// Reads the records of the journal in the order they were appended, which is the order
    /// of their `lsn`.
    ///
    /// The iterator reads the file through a handle of its own, from the first byte to the end
    /// of the last record this `Wal` knows about, so a record appended after the call is not
    /// yielded. The numbering was checked by [`Wal::open`] and is not checked again here.
    pub(crate) fn iter(&self) -> Result<WalIter, InternalError> {
        let file = File::open(&self.path)?;
        Ok(WalIter {
            reader: BufReader::new(file),
            offset: 0,
            end: self.end,
            stopped: false,
        })
    }
}

/// The journal of one instance, shared by what writes it and what reads its durable LSN.
///
/// [`Wal`] appends through `&mut self`, while [`super::DiskStorage`] hands the journal both to
/// the tables that log their writes ([`super::version::HeapTable`]) and to the buffer pool,
/// which reads [`Wal::durable_lsn`] through [`DurableLsn`]. The handle is that sharing: a
/// clone names the same journal.
///
/// # Lock order
///
/// [`BufferPool::flush`](super::buffer::BufferPool::flush) and the eviction path read the
/// durable LSN while they hold the lock on the frames, so the lock of the journal is taken
/// under the lock of the pool. Each method below takes the journal lock, does its file work
/// and drops it, and the bodies of this file issue no call to the pool, so the pair is taken
/// in that order here.
///
/// [`WalHandle::wait_durable`] takes the lock of the group, then the journal lock under it;
/// [`WalHandle::sync_appended`] takes the journal lock and drops it before it takes the lock
/// of the group. These two methods are where this file takes the lock of the group, and
/// neither of them calls the pool.
#[derive(Debug, Clone)]
pub(crate) struct WalHandle {
    /// The journal itself, behind the lock that serialises the appends.
    wal: Arc<Mutex<Wal>>,
    /// What the callers of [`WalHandle::wait_durable`] share to elect the leader of a sync.
    group: Arc<Group>,
}

/// The group commit of one journal (module documentation, "Group commit").
#[derive(Debug)]
struct Group {
    /// A second handle on the journal file, which the leader syncs without the journal lock.
    file: File,
    /// Whether a leader is syncing at the moment, and the records whose sync failed.
    state: Mutex<GroupState>,
    /// Where the callers wait for the leader to finish.
    done: Condvar,
    /// A gate the leader passes before it syncs, set by the tests that hold a sync back.
    #[cfg(test)]
    gate: Mutex<Option<Arc<SyncGate>>>,
}

/// What the callers of [`WalHandle::wait_durable`] share, behind the lock of the group.
#[derive(Debug, Default)]
struct GroupState {
    /// Whether a leader is syncing at the moment.
    syncing: bool,
    /// The records a failed group sync was to cover: from the record after the durable LSN
    /// read before that sync to its target, both ends included.
    failed: Vec<(Lsn, Lsn)>,
}

impl GroupState {
    /// Whether a failed group sync was to cover `lsn`.
    fn failed_on(&self, lsn: Lsn) -> bool {
        self.failed
            .iter()
            .any(|&(first, last)| first <= lsn && lsn <= last)
    }
}

/// A gate that holds the leader of a group sync back until a test opens it.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct SyncGate {
    /// Whether the gate is open, and how many leaders reached it.
    state: Mutex<(bool, u64)>,
    /// Where a leader waits for the gate to open.
    opened: Condvar,
    /// How many of the next leaders that pass the gate fail their sync instead of making it.
    fail: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl SyncGate {
    /// Counts the caller, then waits until the gate is open.
    fn pass(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.1 += 1;
        while !state.0 {
            state = self.opened.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Opens the gate and lets the leaders waiting at it go on.
    pub(crate) fn open(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0 = true;
        self.opened.notify_all();
    }

    /// Makes the next `count` leaders that pass the gate fail their sync.
    pub(crate) fn fail_next(&self, count: u64) {
        self.fail.store(count, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the leader passing the gate fails its sync, counting it down.
    fn fails(&self) -> bool {
        self.fail
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |left| left.checked_sub(1),
            )
            .is_ok()
    }

    /// How many leaders reached the gate.
    pub(crate) fn reached(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1
    }
}

impl WalHandle {
    /// Opens the journal at `path` as [`Wal::open`] does, and shares it.
    ///
    /// # Errors
    ///
    /// Those of [`Wal::open`].
    pub(crate) fn open(path: &Path) -> Result<Self, InternalError> {
        let wal = Wal::open(path)?;
        let file = wal.file.try_clone()?;
        Ok(Self {
            wal: Arc::new(Mutex::new(wal)),
            group: Arc::new(Group {
                file,
                state: Mutex::new(GroupState::default()),
                done: Condvar::new(),
                #[cfg(test)]
                gate: Mutex::new(None),
            }),
        })
    }

    /// Appends a record of kind `kind` for `txn` and answers its number, without `sync_all`.
    ///
    /// This is what a write logs before it changes a page: the record is in the file, and a
    /// crash before the next [`WalHandle::append_durable`] may take it away.
    ///
    /// # Errors
    ///
    /// Those of [`Wal::append`], and [`InternalError::Corruption`] for a lock a panicking
    /// thread left poisoned.
    pub(crate) fn append(
        &self,
        kind: WalRecordKind,
        txn: TxnId,
        payload: &[u8],
    ) -> Result<Lsn, InternalError> {
        self.lock()?.append(kind, txn, payload)
    }

    /// Appends a record and `sync_all`s the journal before returning its number.
    ///
    /// At the return, the record and the ones appended before it are on disk, and
    /// [`WalHandle::durable_lsn`] names the new one: this is the call `commit` and `rollback`
    /// rest on, and it moves the durable LSN past the records of the other transactions the
    /// journal already held ([`super::version::HeapTable::commit`]).
    ///
    /// # Errors
    ///
    /// Those of [`Wal::append`] and [`Wal::flush`], and [`InternalError::Corruption`] for a
    /// poisoned lock.
    pub(crate) fn append_durable(
        &self,
        kind: WalRecordKind,
        txn: TxnId,
        payload: &[u8],
    ) -> Result<Lsn, InternalError> {
        let mut wal = self.lock()?;
        let lsn = wal.append(kind, txn, payload)?;
        wal.flush()?;
        Ok(lsn)
    }

    /// Appends the [`WalRecordKind::Checkpoint`] record of a checkpoint and `sync_all`s the
    /// journal before answering its number.
    ///
    /// The payload carries the number of the record itself and the two counters
    /// ([`CheckpointPayload`]); `next_page_id` and `next_row_id` are the ones of the control
    /// block the caller then writes ([`super::DiskStorage::checkpoint`]). At the return the
    /// record is on disk and [`WalHandle::durable_lsn`] names it.
    ///
    /// # Errors
    ///
    /// Those of [`Wal::append_checkpoint`] and [`Wal::flush`], and
    /// [`InternalError::Corruption`] for a poisoned lock.
    pub(crate) fn append_checkpoint(
        &self,
        next_page_id: PageId,
        next_row_id: u64,
    ) -> Result<Lsn, InternalError> {
        let mut wal = self.lock()?;
        let lsn = wal.append_checkpoint(next_page_id, next_row_id)?;
        wal.flush()?;
        Ok(lsn)
    }

    /// `sync_all`s the journal, so that [`WalHandle::durable_lsn`] names the last record
    /// appended so far.
    ///
    /// This is what a write that frees room in a heap page calls once its records are
    /// appended, before a page carrying their LSN may be handed to the pool without the
    /// `in_progress` flag ([`super::version::HeapTable::vacuum`],
    /// [`super::version::HeapTable::finish`],
    /// [`super::version::HeapTable::rollback_to_savepoint`]).
    ///
    /// # Errors
    ///
    /// Those of [`Wal::flush`], and [`InternalError::Corruption`] for a poisoned lock.
    pub(crate) fn flush(&self) -> Result<(), InternalError> {
        self.lock()?.flush()
    }

    /// Waits until the record `lsn` is durable, syncing the journal as the leader of a group
    /// when no other caller is (module documentation, "Group commit").
    ///
    /// `lsn` is a record the caller has appended. At an `Ok` return,
    /// [`WalHandle::durable_lsn`] names `lsn` or a later record. The journal lock is not held
    /// during the sync, so the other callers append their records meanwhile, and the next
    /// sync covers them.
    ///
    /// # Errors
    ///
    /// When the group sync that was to cover `lsn` fails, [`InternalError::Io`] for each
    /// caller whose record lies between the durable LSN read before that sync and its target,
    /// the leader included; the waiters among them do not lead a second sync for their
    /// record, since a sync that succeeds after a failed one does not say the pages of the
    /// first are on disk. The durable LSN stays where it was
    /// (`storage_impl::tests::a_failed_group_sync_fails_each_commit_it_was_to_cover`).
    /// [`InternalError::Corruption`] for a poisoned lock.
    pub(crate) fn wait_durable(&self, lsn: Lsn) -> Result<(), InternalError> {
        let mut group = self.group_lock()?;
        loop {
            if group.failed_on(lsn) {
                return Err(failed_sync(lsn));
            }
            if self.lock()?.durable_lsn() >= lsn {
                return Ok(());
            }
            if !group.syncing {
                break;
            }
            group = self
                .group
                .done
                .wait(group)
                .map_err(|_| poisoned("group commit"))?;
        }
        group.syncing = true;
        drop(group);
        let outcome = self.sync_appended();
        let mut group = self.group_lock()?;
        group.syncing = false;
        if let Err((first, last)) = outcome {
            group.failed.push((first, last));
        }
        drop(group);
        self.group.done.notify_all();
        match outcome {
            Ok(()) => Ok(()),
            Err(_) => Err(failed_sync(lsn)),
        }
    }

    /// Syncs the records appended so far, without holding the journal lock during the sync,
    /// and moves the durable LSN to the last of them.
    ///
    /// A failure answers the records the sync was to cover, from the one after the durable
    /// LSN read before the sync to the target; the durable LSN is left alone.
    fn sync_appended(&self) -> Result<(), (Lsn, Lsn)> {
        #[cfg(test)]
        let gate = self
            .group
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        #[cfg(test)]
        if let Some(gate) = &gate {
            gate.pass();
        }
        let (durable, target) = match self.lock() {
            Ok(wal) => (wal.durable_lsn(), wal.last_lsn()),
            Err(_) => return Err((Lsn(1), Lsn(u64::MAX))),
        };
        let first = Lsn(durable.0.saturating_add(1));
        #[cfg(test)]
        if gate.as_ref().is_some_and(|gate| gate.fails()) {
            return Err((first, target));
        }
        if self.group.file.sync_all().is_err() {
            return Err((first, target));
        }
        match self.lock() {
            Ok(mut wal) => {
                wal.note_sync(target);
                Ok(())
            }
            Err(_) => Err((first, target)),
        }
    }

    /// The state of the group, a poison reported as corruption.
    fn group_lock(&self) -> Result<MutexGuard<'_, GroupState>, InternalError> {
        self.group
            .state
            .lock()
            .map_err(|_| poisoned("group commit"))
    }

    /// Puts `gate` in front of the next group syncs of this journal.
    #[cfg(test)]
    pub(crate) fn set_sync_gate(&self, gate: Option<Arc<SyncGate>>) {
        *self.group.gate.lock().unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// How many `sync_all` calls made records durable since the journal was opened.
    #[cfg(test)]
    pub(crate) fn syncs(&self) -> u64 {
        self.lock().map_or(0, |wal| wal.syncs())
    }

    /// The records of the journal, read from its first byte.
    ///
    /// # Errors
    ///
    /// Those of [`Wal::iter`], and [`InternalError::Corruption`] for a poisoned lock.
    pub(crate) fn records(&self) -> Result<Vec<WalRecord>, InternalError> {
        self.lock()?.iter()?.collect()
    }

    /// Number of the last record, whether it is durable or not.
    pub(crate) fn last_lsn(&self) -> Result<Lsn, InternalError> {
        Ok(self.lock()?.last_lsn())
    }

    /// The lock around the journal, a poison reported as corruption, as the buffer pool
    /// reports its own.
    fn lock(&self) -> Result<MutexGuard<'_, Wal>, InternalError> {
        self.wal.lock().map_err(|_| poisoned("journal"))
    }
}

/// The error of a record whose group sync failed.
fn failed_sync(lsn: Lsn) -> InternalError {
    InternalError::Io(std::io::Error::other(format!(
        "the sync of the journal that was to make record {lsn} durable failed"
    )))
}

/// The error of a lock a panicking thread left poisoned.
fn poisoned(what: &str) -> InternalError {
    InternalError::Corruption(format!("{what} lock poisoned"))
}

impl DurableLsn for WalHandle {
    /// The LSN the last [`Wal::flush`] covered, `Lsn(0)` for a journal nothing was flushed on.
    ///
    /// A poisoned lock answers `Lsn(0)`, the value that holds back the most: the trait hands
    /// the pool no error to report, and `Lsn(0)` makes it refuse to write a page that carries
    /// an LSN ([`InternalError::Bug`], `flush_of_a_logged_page_is_a_bug_when_the_lock_is_poisoned`)
    /// rather than put that page ahead of the log.
    fn durable_lsn(&self) -> Lsn {
        self.wal.lock().map_or(Lsn(0), |wal| wal.durable_lsn())
    }
}

/// Iterator over the records of a [`Wal`], in `lsn` order.
///
/// An item is a [`WalRecord`] or the error that stopped the reading; after an error the
/// iterator yields `None`.
#[derive(Debug)]
pub(crate) struct WalIter {
    /// Read-only handle on the journal, positioned at `offset`.
    reader: BufReader<File>,
    /// Offset of the next record.
    offset: u64,
    /// Offset past the last record to yield.
    end: u64,
    /// Set once an error was yielded, so the reading does not resume after it.
    stopped: bool,
}

impl Iterator for WalIter {
    type Item = Result<WalRecord, InternalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped || self.offset >= self.end {
            return None;
        }
        let framed = match frame(&mut self.reader, self.offset, self.end - self.offset) {
            Ok(framed) => framed,
            Err(error) => {
                self.stopped = true;
                return Some(Err(error));
            }
        };
        match framed {
            Framed::Whole { record, len } => {
                self.offset += len;
                Some(Ok(record))
            }
            // `Wal::open` read the region up to `end` and found whole records in it, and what
            // `Wal::append` added to that region it encoded itself, so getting here means the
            // file changed under the iterator.
            Framed::Damaged { reason }
            | Framed::Rejected { reason }
            | Framed::Torn { reason }
            | Framed::Unframed { reason } => {
                self.stopped = true;
                Some(Err(InternalError::Corruption(reason)))
            }
        }
    }
}

#[cfg(test)]
mod tests;
