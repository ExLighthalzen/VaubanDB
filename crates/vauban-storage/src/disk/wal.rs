//! The write-ahead journal of an on-disk instance: `<dir>/wal`, an append-only file of
//! logical records.
//!
//! The journal is **logical**: a record names what changed (a row inserted in a table, a
//! transaction committed), not the bytes of a page. What the payload of a row record holds is
//! [`super::wal_payload`], and [`super::version::HeapTable`] is what appends a record before it
//! changes a page; here the file, the record format, the log sequence numbers and `sync_all`
//! are what is written.
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

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use vauban_errors::InternalError;

use crate::TxnId;

use super::buffer::DurableLsn;
use super::crc::crc32;
use super::page::{Lsn, PageId};

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
        self.durable_lsn = self.last_lsn;
        Ok(())
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
#[derive(Debug, Clone)]
pub(crate) struct WalHandle {
    /// The journal itself, behind the lock that serialises the appends.
    wal: Arc<Mutex<Wal>>,
}

impl WalHandle {
    /// Opens the journal at `path` as [`Wal::open`] does, and shares it.
    ///
    /// # Errors
    ///
    /// Those of [`Wal::open`].
    pub(crate) fn open(path: &Path) -> Result<Self, InternalError> {
        Ok(Self {
            wal: Arc::new(Mutex::new(Wal::open(path)?)),
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
        self.wal
            .lock()
            .map_err(|_| InternalError::Corruption("journal lock poisoned".to_string()))
    }
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
mod tests {
    use super::super::temp::TempDir;
    use super::*;

    /// A journal in a temporary directory, with the guard that removes it.
    fn journal(label: &str) -> (TempDir, Wal) {
        let dir = TempDir::created(label);
        let wal = Wal::open(&dir.child("wal")).expect("open a journal");
        (dir, wal)
    }

    /// The records of a journal, or the error that stopped the reading.
    fn records(wal: &Wal) -> Vec<WalRecord> {
        wal.iter()
            .expect("open the journal for reading")
            .collect::<Result<Vec<_>, _>>()
            .expect("read the records back")
    }

    /// Size of the journal on disk.
    fn size(path: &Path) -> u64 {
        std::fs::metadata(path).expect("the journal exists").len()
    }

    /// Appends raw bytes past the end of the journal, as a torn append would leave them.
    fn append_bytes(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open the journal to append raw bytes");
        file.write_all(bytes).expect("append raw bytes");
    }

    /// Flips the byte at `at` in the journal.
    fn flip(path: &Path, at: usize) {
        let mut bytes = std::fs::read(path).expect("read the journal");
        bytes[at] ^= 0xFF;
        std::fs::write(path, bytes).expect("write the journal back");
    }

    /// A record built field by field, checksum sealed over what the fields say.
    ///
    /// Lets the tests write a record that [`WalRecord::encode`] would not: another version,
    /// a kind outside the enum, a number that does not continue the chain.
    fn raw_record(
        version: u8,
        kind: u8,
        lsn: u64,
        prev_lsn: u64,
        txn: u64,
        payload: &[u8],
    ) -> Vec<u8> {
        let length = HEADER_LEN + payload.len() + CHECKSUM_LEN;
        let mut bytes = Vec::with_capacity(LENGTH_LEN + length);
        bytes.extend_from_slice(&(length as u32).to_le_bytes());
        bytes.push(version);
        bytes.push(kind);
        bytes.extend_from_slice(&lsn.to_le_bytes());
        bytes.extend_from_slice(&prev_lsn.to_le_bytes());
        bytes.extend_from_slice(&txn.to_le_bytes());
        bytes.extend_from_slice(payload);
        let checksum = crc32(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes
    }

    // --- Checkpoint payload -------------------------------------------------------------

    #[test]
    fn a_checkpoint_payload_is_three_little_endian_u64() {
        let payload = CheckpointPayload {
            redo_lsn: Lsn(12),
            next_page_id: PageId(34),
            next_row_id: 56,
        };
        let bytes = payload.encode();

        assert_eq!(CHECKPOINT_PAYLOAD_LEN, 24);
        assert_eq!(bytes.len(), CHECKPOINT_PAYLOAD_LEN);
        assert_eq!(&bytes[0..8], &12u64.to_le_bytes());
        assert_eq!(&bytes[8..16], &34u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &56u64.to_le_bytes());
        assert_eq!(CheckpointPayload::decode(&bytes).expect("decode"), payload);
    }

    #[test]
    fn a_checkpoint_payload_of_another_length_is_corruption() {
        for len in [0, CHECKPOINT_PAYLOAD_LEN - 1, CHECKPOINT_PAYLOAD_LEN + 1] {
            let err = CheckpointPayload::decode(&vec![0u8; len])
                .expect_err("a payload of another length is refused");
            assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
            assert!(
                err.to_string().contains(&format!("{len} payload bytes")),
                "{err}"
            );
        }
    }

    #[test]
    fn the_payload_of_a_checkpoint_names_the_record_itself() {
        let (dir, mut wal) = journal("wal-checkpoint-names-itself");
        wal.append(WalRecordKind::Begin, TxnId(3), &[])
            .expect("append a begin");

        let lsn = wal
            .append_checkpoint(PageId(7), 9)
            .expect("append the checkpoint record");

        assert_eq!(lsn, Lsn(2), "the record follows the begin");
        let written = records(&wal);
        assert_eq!(written.len(), 2);
        let record = &written[1];
        assert_eq!(record.kind, WalRecordKind::Checkpoint);
        assert_eq!(record.lsn, lsn);
        assert_eq!(
            record.txn,
            TxnId(0),
            "a checkpoint belongs to no transaction"
        );
        let payload = CheckpointPayload::decode(&record.payload).expect("decode the payload");
        assert_eq!(
            payload,
            CheckpointPayload {
                redo_lsn: lsn,
                next_page_id: PageId(7),
                next_row_id: 9,
            }
        );
        // The record is in the file; `Wal::append_checkpoint` leaves the `sync_all` to the
        // handle, so the durable LSN is still the one of before.
        assert_eq!(wal.durable_lsn(), Lsn(0));
        assert_eq!(size(&dir.child("wal")), wal.append_offset());
    }

    #[test]
    fn the_handle_syncs_the_checkpoint_record_it_appends() {
        let dir = TempDir::created("wal-checkpoint-handle");
        let handle = WalHandle::open(&dir.child("wal")).expect("open the journal");

        let lsn = handle
            .append_checkpoint(PageId(1), 2)
            .expect("append the checkpoint record");

        assert_eq!(lsn, Lsn(1));
        assert_eq!(handle.durable_lsn(), lsn, "the record was synced");
        let written = handle.records().expect("read the journal");
        assert_eq!(kinds_of(&written), vec![WalRecordKind::Checkpoint]);
        assert_eq!(
            CheckpointPayload::decode(&written[0].payload)
                .expect("decode the payload")
                .redo_lsn,
            lsn
        );
    }

    /// The kind of each record, in order.
    fn kinds_of(records: &[WalRecord]) -> Vec<WalRecordKind> {
        records.iter().map(|record| record.kind).collect()
    }

    #[test]
    fn open_creates_an_empty_journal() {
        let dir = TempDir::created("wal-create");
        let path = dir.child("wal");
        assert!(!path.exists());
        let wal = Wal::open(&path).expect("create a journal");
        assert!(path.is_file());
        assert_eq!(size(&path), 0);
        assert_eq!(wal.path(), path);
        assert_eq!(wal.append_offset(), 0);
        assert_eq!(wal.last_lsn(), Lsn(0));
        assert_eq!(wal.durable_lsn(), Lsn(0));
        assert_eq!(wal.records(), 0);
        assert!(records(&wal).is_empty());
    }

    #[test]
    fn append_then_iter() {
        let (dir, mut wal) = journal("append-then-iter");
        assert_eq!(
            wal.append(WalRecordKind::Begin, TxnId(9), &[])
                .expect("append Begin"),
            Lsn(1)
        );
        assert_eq!(
            wal.append(WalRecordKind::Insert, TxnId(9), &[1, 2, 3])
                .expect("append Insert"),
            Lsn(2)
        );
        assert_eq!(
            wal.append(WalRecordKind::Commit, TxnId(9), &[])
                .expect("append Commit"),
            Lsn(3)
        );
        assert_eq!(wal.last_lsn(), Lsn(3));
        assert_eq!(wal.records(), 3);

        let read = records(&wal);
        assert_eq!(read.len(), 3);
        assert_eq!(
            read.iter().map(|record| record.kind).collect::<Vec<_>>(),
            vec![
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Commit
            ]
        );
        assert_eq!(
            read.iter().map(|record| record.lsn).collect::<Vec<_>>(),
            vec![Lsn(1), Lsn(2), Lsn(3)]
        );
        // Chained: each record names the one before it, the first names `Lsn(0)`.
        assert_eq!(
            read.iter()
                .map(|record| record.prev_lsn)
                .collect::<Vec<_>>(),
            vec![Lsn(0), Lsn(1), Lsn(2)]
        );
        assert_eq!(
            read.iter().map(|record| record.txn).collect::<Vec<_>>(),
            vec![TxnId(9), TxnId(9), TxnId(9)]
        );
        assert_eq!(read[0].payload, Vec::<u8>::new());
        assert_eq!(read[1].payload, vec![1, 2, 3]);
        assert_eq!(read[2].payload, Vec::<u8>::new());

        // Two records with an empty payload and one with three bytes: 34 + 37 + 34.
        assert_eq!(wal.append_offset(), 105);
        assert_eq!(size(&dir.child("wal")), 105);
    }

    #[test]
    fn record_bytes_follow_the_documented_layout() {
        let (dir, mut wal) = journal("wal-layout");
        wal.append(WalRecordKind::Insert, TxnId(0x0102_0304_0506_0708), &[7, 8])
            .expect("append Insert");
        wal.flush().expect("flush");
        let bytes = std::fs::read(dir.child("wal")).expect("read the journal");

        assert_eq!(bytes.len(), 36, "34 bytes of frame and two of payload");
        // `length`: the bytes after the field itself, checksum included.
        assert_eq!(read_u32(&bytes, 0), 32);
        assert_eq!(bytes[4], 1, "version");
        assert_eq!(bytes[5], WalRecordKind::Insert.as_byte());
        assert_eq!(bytes[5], 2);
        assert_eq!(read_u64(&bytes, 6), 1, "lsn");
        assert_eq!(read_u64(&bytes, 14), 0, "prev_lsn of the first record");
        assert_eq!(read_u64(&bytes, 22), 0x0102_0304_0506_0708, "txn");
        // Little-endian: the low byte of the transaction comes first.
        assert_eq!(bytes[22], 0x08);
        assert_eq!(&bytes[30..32], &[7, 8], "payload");
        assert_eq!(
            read_u32(&bytes, 32),
            crc32(&bytes[..32]),
            "the checksum covers the record but its own four bytes"
        );
    }

    #[test]
    fn flush_is_durable_across_reopen() {
        let (dir, mut wal) = journal("wal-flush");
        wal.append(WalRecordKind::Begin, TxnId(1), &[])
            .expect("append Begin");
        wal.append(WalRecordKind::Insert, TxnId(1), &[4, 5])
            .expect("append Insert");
        assert_eq!(wal.durable_lsn(), Lsn(0), "nothing was flushed yet");
        wal.flush().expect("flush");
        assert_eq!(wal.durable_lsn(), Lsn(2));
        let written = records(&wal);
        drop(wal);

        let mut reopened = Wal::open(&dir.child("wal")).expect("reopen the journal");
        assert_eq!(records(&reopened), written);
        assert_eq!(reopened.last_lsn(), Lsn(2));
        assert_eq!(reopened.records(), 2);
        // `open` ends with a `sync_all`, so it reports the record it read as durable.
        assert_eq!(reopened.durable_lsn(), Lsn(2));
        assert_eq!(reopened.append_offset(), size(&dir.child("wal")));

        // The numbering carries on from the file rather than starting over.
        assert_eq!(
            reopened
                .append(WalRecordKind::Commit, TxnId(1), &[])
                .expect("append after the reopen"),
            Lsn(3)
        );
        let after = records(&reopened);
        assert_eq!(after.len(), 3);
        assert_eq!(after[2].prev_lsn, Lsn(2));
    }

    #[test]
    fn unflushed_tail_may_vanish() {
        // `Drop` does not flush, so that a crash test can kill a process holding records that
        // were never synced. The source is asserted here rather than only in the rustdoc: a
        // `Drop` that synced would make this file lie.
        let source = include_str!("wal.rs");
        let drop_impl = format!("impl {} for Wal", "Drop");
        assert!(
            !source.contains(&drop_impl),
            "wal.rs should carry no {drop_impl}"
        );
        assert!(
            !source.contains(&format!("impl {} for WalIter", "Drop")),
            "the iterator should not sync either"
        );

        let (dir, mut wal) = journal("wal-unflushed");
        let first = wal
            .append(WalRecordKind::Begin, TxnId(2), &[])
            .expect("append Begin");
        let second = wal
            .append(WalRecordKind::Insert, TxnId(2), &[6])
            .expect("append Insert");
        assert_eq!((first, second), (Lsn(1), Lsn(2)));
        assert_eq!(wal.durable_lsn(), Lsn(0), "no flush, nothing durable");
        let written = records(&wal);
        drop(wal);

        // Without a flush the two records are not promised to be there. What is asserted is
        // that the journal reopens on a prefix of what was appended: this process wrote the
        // bytes through `write_all`, so the operating system usually hands them back whole,
        // and the test does not require the loss it allows.
        let reopened = Wal::open(&dir.child("wal")).expect("reopen the journal");
        let read = records(&reopened);
        assert!(read.len() <= written.len(), "{} records", read.len());
        assert_eq!(read, written[..read.len()]);
        assert_eq!(reopened.last_lsn(), Lsn(read.len() as u64));
        assert_eq!(reopened.records(), read.len() as u64);
    }

    #[test]
    fn drop_does_not_flush_says_the_rustdoc_of_wal() {
        // The sentence is required in the rustdoc of `Wal`, not merely somewhere in the file:
        // look at the contiguous block of `///` lines that precedes the declaration.
        let source = include_str!("wal.rs");
        let (before_declaration, _) = source
            .split_once("pub(crate) struct Wal {")
            .expect("the declaration of Wal");
        let doc_block: Vec<&str> = before_declaration
            .lines()
            .rev()
            .take_while(|line| {
                let line = line.trim_start();
                line.starts_with("///") || line.starts_with("#[")
            })
            .filter(|line| line.trim_start().starts_with("///"))
            .collect();
        assert!(!doc_block.is_empty(), "Wal should carry a rustdoc block");
        assert!(
            doc_block
                .iter()
                .any(|line| line.contains("does not flush it")),
            "the rustdoc of Wal should say that dropping it does not flush"
        );
    }

    #[test]
    fn truncated_last_record_is_discarded() {
        let (dir, mut wal) = journal("wal-torn");
        wal.append(WalRecordKind::Begin, TxnId(3), &[])
            .expect("append Begin");
        wal.flush().expect("flush");
        let written = records(&wal);
        let whole = wal.append_offset();
        drop(wal);

        // Four bytes announcing a record of 100, and not one byte of it.
        append_bytes(&dir.child("wal"), &100u32.to_le_bytes());
        assert_eq!(size(&dir.child("wal")), whole + 4);

        let mut reopened = Wal::open(&dir.child("wal")).expect("the torn tail is not an error");
        assert_eq!(records(&reopened), written);
        assert_eq!(reopened.last_lsn(), Lsn(1));
        assert_eq!(reopened.records(), 1);
        assert_eq!(
            size(&dir.child("wal")),
            whole,
            "the file is truncated to the end of the last whole record"
        );
        assert_eq!(reopened.append_offset(), whole);
        // The next record goes where the torn bytes were.
        assert_eq!(
            reopened
                .append(WalRecordKind::Commit, TxnId(3), &[])
                .expect("append over the truncated tail"),
            Lsn(2)
        );
        assert_eq!(records(&reopened).len(), 2);
    }

    #[test]
    fn a_zero_filled_tail_is_discarded() {
        let (dir, mut wal) = journal("wal-zeros");
        wal.append(WalRecordKind::Begin, TxnId(3), &[])
            .expect("append Begin");
        wal.flush().expect("flush");
        let whole = wal.append_offset();
        drop(wal);

        // Eight zero bytes: the length field frames no record, and 0 is not a kind either.
        append_bytes(&dir.child("wal"), &[0u8; 8]);
        let reopened = Wal::open(&dir.child("wal")).expect("a zero-filled tail is not an error");
        assert_eq!(reopened.records(), 1);
        assert_eq!(size(&dir.child("wal")), whole);
    }

    #[test]
    fn crc_mismatch_interior_is_corruption() {
        let (dir, mut wal) = journal("wal-interior-crc");
        wal.append(WalRecordKind::Insert, TxnId(4), &[1, 2, 3])
            .expect("append Insert");
        wal.append(WalRecordKind::Commit, TxnId(4), &[])
            .expect("append Commit");
        wal.flush().expect("flush");
        let full = wal.append_offset();
        drop(wal);

        // Offset 30 is the first payload byte of the first record.
        flip(&dir.child("wal"), 30);
        let err = Wal::open(&dir.child("wal")).expect_err("a damaged interior record");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("checksum"), "{err}");
        assert!(err.to_string().contains("offset 0"), "{err}");
        assert_eq!(
            size(&dir.child("wal")),
            full,
            "a refused journal is left as it stands"
        );
    }

    #[test]
    fn crc_mismatch_of_the_last_record_is_discarded() {
        // The counterpart of the test above, and what makes the two cases part on the position
        // rather than on the checksum alone: a byte flipped inside the last record leaves a
        // tail this build drops, where a byte flipped inside a record that has a whole record
        // after it is an error.
        let (dir, mut wal) = journal("wal-tail-crc");
        wal.append(WalRecordKind::Insert, TxnId(4), &[1, 2, 3])
            .expect("append Insert");
        let first = wal.append_offset();
        wal.append(WalRecordKind::Commit, TxnId(4), &[])
            .expect("append Commit");
        wal.flush().expect("flush");
        let written = records(&wal);
        drop(wal);

        // Offset `first + 6` is the first byte of the `lsn` of the second record.
        flip(&dir.child("wal"), first as usize + 6);
        let reopened = Wal::open(&dir.child("wal")).expect("a damaged tail record is dropped");
        assert_eq!(records(&reopened), written[..1]);
        assert_eq!(reopened.last_lsn(), Lsn(1));
        assert_eq!(size(&dir.child("wal")), first);
    }

    /// A journal of three records of 34 bytes each, flushed, in a temporary directory.
    ///
    /// The three records carry an empty payload, so each of them is a frame of 34 bytes and the
    /// file is 102 bytes long: the offsets of the records are 0, 34 and 68, which is what the
    /// tests below flip bytes at.
    fn journal_of_three_records(label: &str) -> (TempDir, Vec<WalRecord>, Vec<u8>) {
        let dir = TempDir::created(label);
        let mut wal = Wal::open(&dir.child("wal")).expect("open a journal");
        for kind in [
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Commit,
        ] {
            wal.append(kind, TxnId(8), &[]).expect("append a record");
        }
        wal.flush().expect("flush");
        assert_eq!(wal.append_offset(), 102);
        let written = records(&wal);
        drop(wal);
        let bytes = std::fs::read(dir.child("wal")).expect("read the journal");
        assert_eq!(bytes.len(), 102);
        (dir, written, bytes)
    }

    #[test]
    fn an_interior_length_that_overshoots_the_file_is_corruption() {
        // The `length` of the second record of three goes from 30
        // to 65 310 by the flip of its second byte. The frame then announces more bytes than
        // the file holds, which is not something the scan can step over — and the third record
        // is whole and its checksum agrees, so the second is an interior record, not the tail
        // of an append that did not finish.
        let (dir, _written, bytes) = journal_of_three_records("wal-interior-length");
        let path = dir.child("wal");
        assert_eq!(read_u32(&bytes, 34), 30, "the length of the second record");
        flip(&path, 35);
        assert_eq!(
            read_u32(&std::fs::read(&path).expect("read the journal"), 34),
            65_310
        );

        let err = Wal::open(&path).expect_err("an interior record that cannot be framed");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("offset 34"), "{err}");
        assert!(err.to_string().contains("offset 68"), "{err}");
        assert!(err.to_string().contains("interior record"), "{err}");
        assert_eq!(
            size(&path),
            102,
            "a refused journal keeps the records that follow the damage"
        );
    }

    #[test]
    fn an_interior_length_that_frames_no_record_is_corruption() {
        // The other unusable frame: four zero bytes in place of the `length` of the second
        // record of three. Nothing to step over either, and the third record checks out.
        let (dir, _written, _bytes) = journal_of_three_records("wal-interior-zeros");
        let path = dir.child("wal");
        let mut bytes = std::fs::read(&path).expect("read the journal");
        bytes[34..38].fill(0);
        std::fs::write(&path, &bytes).expect("write the journal back");

        let err = Wal::open(&path).expect_err("an interior length that frames no record");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("offset 34"), "{err}");
        assert!(err.to_string().contains("offset 68"), "{err}");
        assert_eq!(size(&path), 102);
    }

    #[test]
    fn one_flipped_byte_of_a_record_that_has_a_whole_record_after_it_is_corruption() {
        // Swept on one shape: a journal of three records of 34 bytes, the eight bits of one
        // byte flipped at a time over the 68 bytes of the first two records, 68 journals in all.
        // Each flip leaves the third record whole with an agreeing checksum, so each of these 68
        // names an interior record, in whichever field of those two records the byte sat — the
        // `length` as much as the body or the checksum. Flipping eight bits is a coarse vector:
        // a `length` mangled that way lands outside the bounds of a record, where one bit of it
        // can leave a frame that still fits in the file. That finer sweep is
        // `one_flipped_bit_does_not_truncate_over_a_sealed_record`, below.
        for at in 0..68usize {
            let (dir, _written, _bytes) =
                journal_of_three_records(&format!("wal-sweep-interior-{at}"));
            let path = dir.child("wal");
            flip(&path, at);
            // The record the flip landed in, named by the error.
            let hit = if at < 34 { "offset 0" } else { "offset 34" };
            match Wal::open(&path) {
                Err(InternalError::Corruption(message)) => {
                    assert!(message.contains(hit), "offset {at}: {message}");
                    assert!(
                        message.contains("interior record"),
                        "offset {at}: {message}"
                    );
                }
                Err(other) => panic!("offset {at}: expected Corruption, got {other:?}"),
                Ok(accepted) => panic!(
                    "offset {at}: open accepted the journal and kept {} of the 3 records",
                    accepted.records()
                ),
            }
            assert_eq!(
                size(&path),
                102,
                "offset {at}: the file is left as it stands"
            );
        }
    }

    #[test]
    fn one_flipped_byte_of_the_last_record_leaves_the_records_before_it() {
        // The counterpart, on the same shape: the 34 bytes of the third record, one flip at a
        // time. Nothing whole follows any of them, so each is the tail of an append this build
        // drops, and the two records before it come back.
        for at in 68..102usize {
            let (dir, written, _bytes) = journal_of_three_records(&format!("wal-sweep-tail-{at}"));
            let path = dir.child("wal");
            flip(&path, at);
            let reopened = Wal::open(&path)
                .unwrap_or_else(|err| panic!("offset {at}: open should truncate, got {err}"));
            assert_eq!(records(&reopened), written[..2], "offset {at}");
            assert_eq!(reopened.last_lsn(), Lsn(2), "offset {at}");
            assert_eq!(size(&path), 68, "offset {at}");
        }
    }

    #[test]
    fn an_unknown_kind_is_corruption_even_at_the_end() {
        // Checksum sealed over the fields, so the record was written whole: nothing was lost
        // in a torn append, this build simply does not know kind 99. This is the vector that
        // separates the rule from "anything the last record carries is dropped".
        let dir = TempDir::created("wal-unknown-kind");
        let path = dir.child("wal");
        std::fs::write(&path, raw_record(RECORD_VERSION, 99, 1, 0, 5, &[])).expect("write");
        let err = Wal::open(&path).expect_err("kind 99 is outside WalRecordKind");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("99"), "{err}");
        assert_eq!(size(&path), 34, "the file is left as it stands");
    }

    #[test]
    fn another_format_version_is_corruption() {
        let dir = TempDir::created("wal-version");
        let path = dir.child("wal");
        std::fs::write(
            &path,
            raw_record(2, WalRecordKind::Begin.as_byte(), 1, 0, 5, &[]),
        )
        .expect("write");
        let err = Wal::open(&path).expect_err("version 2 is not read by this build");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("format version 2"), "{err}");
    }

    #[test]
    fn a_number_that_does_not_continue_the_chain_is_corruption() {
        let dir = TempDir::created("wal-chain");
        let path = dir.child("wal");
        let kind = WalRecordKind::Begin.as_byte();

        // A journal whose first record is numbered 4 instead of 1.
        std::fs::write(&path, raw_record(RECORD_VERSION, kind, 4, 3, 1, &[])).expect("write");
        let err = Wal::open(&path).expect_err("a fresh journal starts at 1");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("lsn 4"), "{err}");

        // A second record that names another record than the one before it.
        let mut bytes = raw_record(RECORD_VERSION, kind, 1, 0, 1, &[]);
        bytes.extend_from_slice(&raw_record(RECORD_VERSION, kind, 2, 7, 1, &[]));
        std::fs::write(&path, bytes).expect("write");
        let err = Wal::open(&path).expect_err("prev_lsn 7 does not name the record before it");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("offset 34"), "{err}");
    }

    #[test]
    fn a_payload_over_the_maximum_is_a_bug() {
        let (dir, mut wal) = journal("wal-too-long");
        let err = wal
            .append(
                WalRecordKind::Insert,
                TxnId(1),
                &vec![0u8; MAX_PAYLOAD_LEN + 1],
            )
            .expect_err("one byte more than a record holds");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(
            err.to_string().contains(&MAX_PAYLOAD_LEN.to_string()),
            "{err}"
        );
        assert_eq!(wal.append_offset(), 0, "nothing was written");
        assert_eq!(wal.last_lsn(), Lsn(0));

        // The payload of that size does fit, and fills a record of exactly 1 MiB.
        wal.append(WalRecordKind::Insert, TxnId(1), &vec![7u8; MAX_PAYLOAD_LEN])
            .expect("the largest payload a record holds");
        wal.flush().expect("flush");
        assert_eq!(wal.append_offset(), MAX_RECORD_LEN as u64);
        assert_eq!(size(&dir.child("wal")), 1_048_576);
        let read = records(&wal);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].payload.len(), MAX_PAYLOAD_LEN);
        assert_eq!(MAX_PAYLOAD_LEN, 1_048_576 - 34);
    }

    #[test]
    fn a_length_field_outside_the_bounds_frames_no_record() {
        // Read through `frame` rather than through `open`: at the end of a file both this and
        // a torn record stop the scan, so the two are told apart here.
        let announced = MAX_RECORD_LEN as u32;
        let mut bytes: &[u8] = &announced.to_le_bytes();
        match frame(&mut bytes, 8, LENGTH_LEN as u64).expect("read the length field") {
            Framed::Unframed { reason } => {
                assert!(reason.contains("offset 8"), "{reason}");
                assert!(reason.contains(&announced.to_string()), "{reason}");
            }
            other => panic!("a length of {announced} frames no record: {other:?}"),
        }

        // One byte less than the frame of a record with an empty payload.
        let mut bytes: &[u8] = &(MIN_LENGTH as u32 - 1).to_le_bytes();
        assert!(matches!(
            frame(&mut bytes, 0, LENGTH_LEN as u64).expect("read the length field"),
            Framed::Unframed { .. }
        ));

        // A length the format allows, with fewer bytes behind it than it announces: torn.
        let record = raw_record(RECORD_VERSION, WalRecordKind::Begin.as_byte(), 1, 0, 1, &[]);
        let mut bytes: &[u8] = &record[..20];
        match frame(&mut bytes, 0, 20).expect("read the length field") {
            Framed::Torn { reason } => {
                assert!(reason.contains("34 bytes"), "{reason}");
                assert!(reason.contains("holds 20"), "{reason}");
            }
            other => panic!("20 bytes of a record of 34: {other:?}"),
        }
    }

    #[test]
    fn record_kind_bytes_are_part_of_the_format() {
        let kinds = [
            (WalRecordKind::Begin, 1u8),
            (WalRecordKind::Insert, 2),
            (WalRecordKind::Update, 3),
            (WalRecordKind::Delete, 4),
            (WalRecordKind::Commit, 5),
            (WalRecordKind::Abort, 6),
            (WalRecordKind::Checkpoint, 7),
            (WalRecordKind::CreateDatabase, 8),
            (WalRecordKind::DropDatabase, 9),
            (WalRecordKind::CreateTable, 10),
            (WalRecordKind::DropTable, 11),
            (WalRecordKind::CreateIndex, 12),
            (WalRecordKind::DropIndex, 13),
            (WalRecordKind::Savepoint, 14),
            (WalRecordKind::RollbackTo, 15),
        ];
        assert_eq!(kinds.len(), 15);
        for (kind, byte) in kinds {
            assert_eq!(kind.as_byte(), byte);
            assert_eq!(
                WalRecordKind::from_byte(byte).expect("byte in the enum"),
                kind
            );
        }
        // 0 is not a kind, so a run of zero bytes does not read as one.
        assert!(WalRecordKind::from_byte(0).is_err());
        assert!(WalRecordKind::from_byte(16).is_err());
        let err = WalRecordKind::from_byte(u8::MAX).expect_err("255 is outside the enum");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("255"), "{err}");
    }

    #[test]
    fn records_of_every_kind_make_the_round_trip() {
        let (_dir, mut wal) = journal("wal-kinds-roundtrip");
        let kinds = [
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Update,
            WalRecordKind::Delete,
            WalRecordKind::Commit,
            WalRecordKind::Abort,
            WalRecordKind::Checkpoint,
            WalRecordKind::CreateDatabase,
            WalRecordKind::DropDatabase,
            WalRecordKind::CreateTable,
            WalRecordKind::DropTable,
            WalRecordKind::CreateIndex,
            WalRecordKind::DropIndex,
            WalRecordKind::Savepoint,
            WalRecordKind::RollbackTo,
        ];
        for (index, kind) in kinds.iter().enumerate() {
            let payload = vec![index as u8; index];
            let lsn = wal
                .append(*kind, TxnId(index as u64), &payload)
                .expect("append a record");
            assert_eq!(lsn, Lsn(index as u64 + 1));
        }
        wal.flush().expect("flush");
        let read = records(&wal);
        assert_eq!(read.len(), kinds.len());
        for (index, (record, kind)) in read.iter().zip(kinds).enumerate() {
            assert_eq!(record.kind, kind);
            assert_eq!(record.txn, TxnId(index as u64));
            assert_eq!(record.payload, vec![index as u8; index]);
        }
    }

    #[test]
    fn iter_stops_at_the_records_the_wal_knows_about() {
        let (dir, mut wal) = journal("wal-iter-bound");
        wal.append(WalRecordKind::Begin, TxnId(1), &[])
            .expect("append Begin");
        wal.flush().expect("flush");
        // Bytes appended behind the back of the structure are past its `append_offset`, so
        // the iterator does not read them.
        append_bytes(
            &dir.child("wal"),
            &raw_record(
                RECORD_VERSION,
                WalRecordKind::Commit.as_byte(),
                2,
                1,
                1,
                &[],
            ),
        );
        assert_eq!(size(&dir.child("wal")), 68);
        assert_eq!(wal.append_offset(), 34);
        assert_eq!(records(&wal).len(), 1);
    }

    /// A flushed journal of one record per entry of `payloads`, each payload a run of `9`.
    ///
    /// Returns the guard of the directory, the bytes of the file, the offsets the records start
    /// at followed by the size of the file, and the records as [`Wal::iter`] read them.
    fn journal_of(payloads: &[usize], label: &str) -> (TempDir, Vec<u8>, Vec<u64>, Vec<WalRecord>) {
        let dir = TempDir::created(label);
        let path = dir.child("wal");
        let mut wal = Wal::open(&path).expect("open a journal");
        let mut bounds = vec![0u64];
        for (index, payload) in payloads.iter().enumerate() {
            let kind = if index + 1 == payloads.len() {
                WalRecordKind::Commit
            } else {
                WalRecordKind::Insert
            };
            wal.append(kind, TxnId(1), &vec![9u8; *payload])
                .expect("append a record");
            bounds.push(wal.append_offset());
        }
        wal.flush().expect("flush");
        let written = records(&wal);
        drop(wal);
        let bytes = std::fs::read(&path).expect("read the journal");
        (dir, bytes, bounds, written)
    }

    /// Opens the journal of `payloads` once per bit of its file, that bit flipped, and asserts
    /// what `open` answered each time.
    ///
    /// A flip that lands in a record other than the last leaves whole records behind it, so the
    /// journal is refused and left untouched; a flip in the last record leaves the tail of an
    /// append that did not finish, so the records before it come back and the file is cut at the
    /// end of the last of them. Returns how many journals fell in each of the two cases, and the
    /// offsets of the shape, so that the caller can assert the shape it swept.
    fn sweep_one_bit(payloads: &[usize], label: &str) -> (usize, usize, Vec<u64>) {
        let (_source, bytes, bounds, written) = journal_of(payloads, &format!("{label}-source"));
        let last = payloads.len() - 1;
        let work = TempDir::created(label);
        let path = work.child("wal");
        let mut refused = 0usize;
        let mut cut = 0usize;
        for at in 0..bytes.len() {
            for bit in 0..8u32 {
                let mut copy = bytes.clone();
                copy[at] ^= 1u8 << bit;
                std::fs::write(&path, &copy).expect("write the flipped journal");
                // Record the flipped byte belongs to.
                let hit = bounds
                    .iter()
                    .rposition(|start| *start <= at as u64)
                    .expect("the byte sits in a record");
                if hit < last {
                    let message = match Wal::open(&path) {
                        Err(InternalError::Corruption(message)) => message,
                        Err(other) => panic!("byte {at} bit {bit}: {other:?}"),
                        Ok(accepted) => panic!(
                            "byte {at} bit {bit}: open kept {} of the {} records and left a \
                             file of {} bytes out of the {} the journal held, over a record \
                             the flip did not touch",
                            accepted.records(),
                            payloads.len(),
                            size(&path),
                            bytes.len()
                        ),
                    };
                    assert!(
                        message.contains(&format!("offset {}", bounds[hit])),
                        "byte {at} bit {bit}: {message}"
                    );
                    assert!(
                        message.contains("interior record"),
                        "byte {at} bit {bit}: {message}"
                    );
                    assert_eq!(
                        size(&path),
                        bytes.len() as u64,
                        "byte {at} bit {bit}: a refused journal is left as it stands"
                    );
                    refused += 1;
                } else {
                    let reopened =
                        Wal::open(&path).unwrap_or_else(|err| panic!("byte {at} bit {bit}: {err}"));
                    assert_eq!(records(&reopened), written[..last], "byte {at} bit {bit}");
                    assert_eq!(reopened.records(), last as u64, "byte {at} bit {bit}");
                    assert_eq!(size(&path), bounds[last], "byte {at} bit {bit}");
                    cut += 1;
                }
            }
        }
        (refused, cut, bounds)
    }

    #[test]
    fn one_flipped_bit_does_not_truncate_over_a_sealed_record() {
        // Two shapes, one bit at a time over each byte of the file. What this sweep reaches and
        // the eight-bit one above does not: a `length` field altered by a single bit can stay
        // inside the bounds of a record and frame bytes the file holds, so a scan that stepped
        // over it on the word of that `length` walked past whole records.
        //
        // 34/64, records at 0 and 34, a file of 98 bytes: bit 6 of byte 0 turns the `length` of
        // the first record from 30 into 94, a frame of 98 bytes — the file itself.
        let (refused, cut, bounds) = sweep_one_bit(&[0, 30], "wal-bit-sweep-98");
        assert_eq!(bounds, vec![0, 34, 98]);
        assert_eq!(
            (refused, cut),
            (34 * 8, 64 * 8),
            "784 journals: the 34 bytes of the first record refused, the 64 of the second cut"
        );

        // 34/64/34, records at 0, 34 and 98, a file of 132 bytes: the same flip of byte 0 ends
        // the first frame at 98, where the third record starts, so a scan that stepped over it
        // read that third record and lost the second one.
        let (refused, cut, bounds) = sweep_one_bit(&[0, 30, 0], "wal-bit-sweep-132");
        assert_eq!(bounds, vec![0, 34, 98, 132]);
        assert_eq!(
            (refused, cut),
            (98 * 8, 34 * 8),
            "1 056 journals: the 98 bytes of the first two records refused, the 34 of the third cut"
        );
    }

    #[test]
    fn the_two_bit_sweeps_stop_on_frames_of_the_three_failing_kinds() {
        // The numbers the module documentation cites for the sweep above: the frame the scan
        // stops on is read here for each of the same 1 840 flipped journals, without opening
        // them, and sorted by the failure it carries. Neither `Whole` nor `Rejected` turns up in
        // these 1 840, which the arm that panics below states: one bit flipped inside a record
        // leaves the checksum of that record disagreeing.
        let mut counts = (0usize, 0usize, 0usize);
        for payloads in [vec![0usize, 30], vec![0usize, 30, 0]] {
            let (_dir, bytes, bounds, _written) = journal_of(&payloads, "wal-bit-sweep-kinds");
            let mut shape = (0usize, 0usize, 0usize);
            for at in 0..bytes.len() {
                for bit in 0..8u32 {
                    let mut copy = bytes.clone();
                    copy[at] ^= 1u8 << bit;
                    let hit = bounds
                        .iter()
                        .rposition(|start| *start <= at as u64)
                        .expect("the byte sits in a record");
                    let start = bounds[hit] as usize;
                    let mut region: &[u8] = &copy[start..];
                    let remaining = (copy.len() - start) as u64;
                    match frame(&mut region, start as u64, remaining).expect("read the frame") {
                        Framed::Damaged { .. } => shape.0 += 1,
                        Framed::Torn { .. } => shape.1 += 1,
                        Framed::Unframed { .. } => shape.2 += 1,
                        other => panic!("byte {at} bit {bit}: {other:?}"),
                    }
                }
            }
            let expected = if payloads.len() == 2 {
                (726, 29, 29)
            } else {
                (968, 43, 45)
            };
            assert_eq!(shape, expected, "shape {payloads:?}");
            counts = (counts.0 + shape.0, counts.1 + shape.1, counts.2 + shape.2);
        }
        assert_eq!(counts, (1694, 72, 74));
        assert_eq!(counts.0 + counts.1 + counts.2, 98 * 8 + 132 * 8);
    }

    #[test]
    fn a_damaged_length_that_spans_the_file_keeps_the_record_behind_it() {
        // One bit of the `length` of the first
        // record of two, 30 into 94, so the frame it announces ends at the last byte of the
        // file. A scan that stepped over that frame landed past the second record and emptied
        // the file without a word; the second record is whole, so this is an interior record.
        let (dir, bytes, bounds, _written) = journal_of(&[0, 30], "wal-spanning-length");
        let path = dir.child("wal");
        assert_eq!(bounds, vec![0, 34, 98]);
        assert_eq!(read_u32(&bytes, 0), 30, "the length of the first record");
        let mut copy = bytes.clone();
        copy[0] ^= 0x40;
        assert_eq!(
            read_u32(&copy, 0),
            94,
            "a frame of 98 bytes, the file itself"
        );
        std::fs::write(&path, &copy).expect("write the journal back");

        let err = Wal::open(&path).expect_err("a damaged record with a whole record after it");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("offset 0"), "{err}");
        assert!(err.to_string().contains("offset 34"), "{err}");
        assert!(err.to_string().contains("interior record"), "{err}");
        assert_eq!(size(&path), 98, "the journal is left as it stands");
    }

    #[test]
    fn a_file_holding_no_agreeing_checksum_is_emptied_without_an_error() {
        // The other end of the truncation rule, and the price it has: 500 bytes in which the
        // walk finds no checksum that agrees are read as one unfinished append and removed, with
        // nothing said. Nothing in this build writes such a file, and `open` has nothing to tell
        // it apart from a first append that stopped halfway with: the case is written down here
        // rather than repaired.
        let dir = TempDir::created("wal-no-record-at-all");
        let path = dir.child("wal");
        let junk: Vec<u8> = (0..500u32).map(|index| (index * 7 + 3) as u8).collect();
        std::fs::write(&path, &junk).expect("write bytes that are no record");
        let opened = Wal::open(&path).expect("bytes framing no record read as a tail");
        assert_eq!(opened.records(), 0);
        assert_eq!(opened.last_lsn(), Lsn(0));
        assert_eq!(size(&path), 0, "the file is emptied");
    }

    #[test]
    fn a_torn_tail_whose_payload_holds_a_record_reads_as_an_interior_record() {
        // The false find the walk can make: the tail is one record whose payload
        // carries the bytes of another, cut two bytes short. Those inner bytes carry a checksum
        // that agrees, so the walk stops on them and reports an interior record where a crash
        // had left a tail. The journal is refused rather than truncated, which is the safe way
        // round of the two. Outside a payload built to hold a record, what the walk leans on is
        // the CRC-32; the case asserted here is the built one.
        let dir = TempDir::created("wal-nested-record");
        let path = dir.child("wal");
        let mut wal = Wal::open(&path).expect("open a journal");
        wal.append(WalRecordKind::Begin, TxnId(1), &[])
            .expect("append Begin");
        wal.flush().expect("flush");
        drop(wal);
        let nested = raw_record(
            RECORD_VERSION,
            WalRecordKind::Commit.as_byte(),
            2,
            1,
            1,
            &[],
        );
        let mut outer = raw_record(
            RECORD_VERSION,
            WalRecordKind::Insert.as_byte(),
            2,
            1,
            1,
            &nested,
        );
        outer.truncate(outer.len() - 2);
        append_bytes(&path, &outer);
        assert_eq!(size(&path), 100, "34 bytes of record and 66 of torn tail");

        let err = Wal::open(&path).expect_err("the nested record is taken for a record behind");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("offset 34"), "{err}");
        // Offset 64: where the payload of the tail starts carrying the nested record.
        assert!(err.to_string().contains("offset 64"), "{err}");
        assert!(err.to_string().contains("interior record"), "{err}");
        assert_eq!(size(&path), 100, "the journal is left as it stands");
    }

    #[test]
    fn a_sealed_record_this_build_will_not_read_stops_the_truncation() {
        // A record sealed over kind 99 has a checksum that agrees, so it reached the disk whole;
        // cutting the file before it would throw away a record that is entire. The walk after an
        // unreadable frame therefore counts it, and the two files below differ by that alone.
        let dir = TempDir::created("wal-sealed-unknown-kind");
        let path = dir.child("wal");
        let begin = WalRecordKind::Begin.as_byte();

        // A `length` of four zero bytes in the second record of three, the third sealed over
        // kind 99: nothing follows with a kind this build reads, and the frame at 34 cannot be
        // walked over, yet offset 68 holds 34 bytes written whole.
        let mut bytes = raw_record(RECORD_VERSION, begin, 1, 0, 1, &[]);
        bytes.extend_from_slice(&raw_record(RECORD_VERSION, begin, 2, 1, 1, &[]));
        bytes.extend_from_slice(&raw_record(RECORD_VERSION, 99, 3, 2, 1, &[]));
        bytes[34..38].fill(0);
        std::fs::write(&path, &bytes).expect("write");
        let err = Wal::open(&path).expect_err("sealed bytes sit behind the unreadable frame");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("offset 34"), "{err}");
        assert!(err.to_string().contains("offset 68"), "{err}");
        assert!(err.to_string().contains("interior record"), "{err}");
        assert_eq!(size(&path), 102, "the journal is left as it stands");

        // The same file without those sealed bytes: the zeroed `length` is then the tail of an
        // append that did not finish, and the first record comes back.
        std::fs::write(&path, &bytes[..68]).expect("write");
        let reopened = Wal::open(&path).expect("a zeroed length at the end is a tail");
        assert_eq!(reopened.records(), 1);
        assert_eq!(size(&path), 34);
    }

    // --- The handle an instance holds ---------------------------------------------------

    #[test]
    fn the_handle_shares_one_journal_and_its_durable_lsn() {
        let dir = TempDir::created("wal-handle");
        let path = dir.child("wal");
        let handle = WalHandle::open(&path).expect("open a journal");
        let shared = handle.clone();

        assert_eq!(handle.durable_lsn(), Lsn(0));
        assert_eq!(
            handle
                .append(WalRecordKind::Begin, TxnId(3), &[])
                .expect("append Begin"),
            Lsn(1)
        );
        // The clone appends to the same journal, and the numbering continues.
        assert_eq!(
            shared
                .append(WalRecordKind::Insert, TxnId(3), &[7])
                .expect("append Insert"),
            Lsn(2)
        );
        // Appending does not make anything durable.
        assert_eq!(handle.durable_lsn(), Lsn(0));
        assert_eq!(handle.last_lsn().expect("last lsn"), Lsn(2));

        assert_eq!(
            shared
                .append_durable(WalRecordKind::Commit, TxnId(3), &[])
                .expect("append Commit and flush"),
            Lsn(3)
        );
        // The flush covers the records appended before the one it was asked for.
        assert_eq!(handle.durable_lsn(), Lsn(3));
        assert_eq!(shared.durable_lsn(), Lsn(3));

        let kinds: Vec<_> = handle
            .records()
            .expect("read the records back")
            .iter()
            .map(|record| record.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Commit
            ]
        );
    }

    #[test]
    fn a_poisoned_journal_lock_answers_a_durable_lsn_of_zero() {
        let dir = TempDir::created("wal-handle-poisoned");
        let handle = WalHandle::open(&dir.child("wal")).expect("open a journal");
        handle
            .append_durable(WalRecordKind::Begin, TxnId(1), &[])
            .expect("append Begin and flush");
        assert_eq!(handle.durable_lsn(), Lsn(1));

        let outcome = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = handle.wal.lock().unwrap();
                    panic!("poisoning the journal lock on purpose");
                })
                .join()
        });
        assert!(outcome.is_err());

        // The pool reads this through a trait that carries no error, so the answer is the one
        // that holds pages back rather than the one that lets them out.
        assert_eq!(handle.durable_lsn(), Lsn(0));
        let err = handle
            .append(WalRecordKind::Commit, TxnId(1), &[])
            .expect_err("the lock is poisoned");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert_eq!(err.to_string(), "data corruption: journal lock poisoned");
        assert!(handle.records().is_err());
        assert!(handle.last_lsn().is_err());
        assert!(
            handle
                .append_durable(WalRecordKind::Commit, TxnId(1), &[])
                .is_err()
        );
    }

    #[test]
    fn wal_handle_is_send_sync() {
        fn assert<T: Send + Sync>() {}
        assert::<WalHandle>();
    }
}
