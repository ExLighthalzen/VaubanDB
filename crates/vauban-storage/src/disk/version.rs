//! Row versions on the slotted heap: `(xmin, xmax)` chains, an in-memory undo log and the
//! visibility rule of [`Snapshot`], for one table without index.
//!
//! [`HeapTable`] is what [`super::heap::Heap`] becomes once the bytes it ranges carry the
//! MVCC model of the crate root ("Model"): a logical [`RowId`] whose versions form a chain,
//! each version created by `xmin` and replaced or deleted by `xmax`. The in-memory
//! implementation keeps that chain in a `BTreeMap`; here a version is one heap record.
//!
//! # Layout of a version
//!
//! A record is [`VERSION_PREFIX_LEN`] bytes of prefix followed by the columns as
//! [`super::encode::encode_row`] writes them. The prefix is four little-endian `u64`:
//!
//! | Offset | Field | Meaning |
//! |---|---|---|
//! | 0 | `row` | the [`RowId`] this version belongs to, one value for the whole chain of a row |
//! | 8 | `xmin` | the transaction that created the version |
//! | 16 | `xmax` | the transaction that replaced or deleted it, [`NO_XMAX`] while it is current |
//! | 24 | `seq` | serial number of the version in the table, not handed out twice |
//!
//! `seq` is what orders the chain: a version an update creates takes the address the heap
//! hands out (see "Setting `xmax`" below), so the address of a version says nothing about its
//! age.
//!
//! # Directory
//!
//! Two maps held in memory. `RowId -> Rid` names the **most recent** version of each row,
//! which is what a write starts from. `(RowId, seq) -> Rid` names **each** version the heap
//! holds, so that [`HeapTable::get`], [`HeapTable::version`] and the versions of one row are
//! read by a lookup in that map and one heap page per version, not by a walk of the heap
//! (`super::index::tests::point_seek_on_100000_rows_reads_at_most_height_plus_two_pages`).
//! [`HeapTable::scan`] walks the heap. [`HeapTable::open`] attaches to a heap with both maps
//! empty and does not rebuild them ([`HeapTable::resume_after_recovery`] does, after a
//! recovery).
//!
//! # Setting `xmax`
//!
//! Marking a version as replaced or deleted writes the eight bytes of its `xmax` in place
//! ([`HeapTable::set_xmax`], [`super::heap::Heap::overwrite`]): the version keeps its [`Rid`]
//! (`the_records_name_the_pages_they_change_and_the_xmax_is_set_in_place`). The version an
//! update creates is a new record, at another [`Rid`]; the `RowId` is untouched
//! (`update_keeps_row_id_and_old_version`, `rid_of_a_row_may_move_on_update`).
//!
//! # Transactions
//!
//! A transaction is discovered at its first write and registered `InProgress`; a transaction
//! the registry does not know is `Committed` for the visibility rule, as in
//! [`crate::MemoryStorage`]. [`HeapTable::commit`] drops the undo log, [`HeapTable::rollback`]
//! replays it backwards. Index maintenance is driven from here through a log the caller
//! drains ([`HeapTable::take_index_changes`]); savepoints and `vacuum` are not implemented.
//!
//! # Write-ahead logging
//!
//! A write appends its record to the journal ([`super::wal`], payload in
//! [`super::wal_payload`]) **before** it changes the heap. The record names the pages and the
//! slots the write changes, the address of a new version being taken first by
//! [`super::heap::Heap::reserve`], and each page it changes takes the LSN of that record in
//! the change itself; [`HeapTable::note_page`] then flags the page. [`HeapTable::commit`] appends a
//! `Commit` and `sync_all`s the journal before it returns, so what a committed transaction
//! wrote is on disk (`commit_fsyncs_wal`); `data` is not synced, the pages leaving the pool
//! afterwards.
//!
//! A write that frees a heap slot without being a row write of its own — the undo of a
//! rollback or of a rollback to a savepoint, and a vacuum — appends a record of its own too,
//! naming the version, the slot and what became of the version
//! ([`super::wal_payload::SlotRelease`]): a [`WalRecordKind::Compensation`] for each version
//! the undo takes out or whose `xmax` it unsets, a [`WalRecordKind::Purge`] for each version a vacuum takes
//! out. The page is tied to that record before the slot is freed, and the journal is
//! `sync_all`ed before the pages leave the `in_progress` flag, so a page that reaches `data`
//! with a slot freed is a page whose record is durable
//! (`a_rollback_journals_one_compensation_per_version_it_takes_out`,
//! `a_vacuum_journals_one_purge_per_version_and_syncs_before_the_pages_leave`).
//!
//! [`HeapTable::note_page`] also sets `in_progress` on the heap page a write lands on, and
//! [`HeapTable::commit`] and [`HeapTable::rollback`] clear it, so the buffer pool leaves that
//! page in the cache while the transaction runs (no-steal). With one transaction, one
//! page and a row held inside that page, the row stays out of `data` until the commit
//! (`in_progress_keeps_an_uncommitted_row_out_of_the_data_file`). With one transaction
//! of 400 rows over three pages, the pages it filled before the last one keep the flag
//! too (`a_transaction_over_two_heap_pages_keeps_the_flag_on_both_of_them`), which
//! [`super::heap::Heap::link_page`] does not take away.
//!
//! Two shapes fall **outside** those two, each with its test; a shape neither of them
//! names is a shape this paragraph says nothing about. The journal closes neither:
//!
//! - a page two transactions have written loses the flag at the first `commit`, and that
//!   `commit` flushes the journal up to its own record, so the record of the transaction still
//!   running is durable and its page may be written: its row reaches `data` while its
//!   transaction has no `Commit` record
//!   (`a_page_shared_by_two_txns_loses_its_flag_at_the_first_commit`). Redo alone does not take
//!   that row away, so the shape is one the recovery answers by marking the loser `Aborted`
//!   ([`super::recover`]);
//! - the overflow pages of a long row carry no flag, the heap page holding its stub being the
//!   one this module marks, and they are dirtied at `Lsn(0)`, so the payload of an uncommitted
//!   long row does reach `data`
//!   (`the_overflow_pages_of_an_uncommitted_long_row_are_not_flagged`). The stub that names
//!   the chain stays on a flagged page, so those bytes are not reachable as a row.
//!
//! # What one `TxnId` means here, and what it does not
//!
//! The journal is the instance's, and so is the register of the transactions. Two
//! consequences:
//!
//! - a transaction is registered **per instance**: one `Begin` and one
//!   `Commit` or `Abort`, whichever table it writes in
//!   (`super::tests::one_begin_one_end_per_txn_over_two_tables`). The status it carries comes
//!   from [`super::TxnRegistry`], so a table this transaction never touched sees the same
//!   status as the one it wrote in;
//! - a `HeapTable` built by [`HeapTable::open`] starts with an empty directory and counters
//!   at one; [`super::DiskStorage::with_rows`] is what rebuilds them from the recovery before
//!   the first call of the engine on a table ([`HeapTable::resume_after_recovery`]), while a
//!   table attached by hand, as the tests of this file do, keeps what `open` leaves.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use vauban_errors::InternalError;

use super::DiskStorage;
use super::encode::{decode_row, encode_row};
use super::heap::{Heap, Rid};
use super::index::IndexChange;
use super::page::{Lsn, PageId};
use super::wal::WalRecordKind;
use super::wal_payload::{PageOp, Release, RowChange, SlotRelease};
use crate::{Row, RowId, Snapshot, TableId, TableShape, TxnId, TxnStatus};

/// Number of bytes a version carries before its columns: `row`, `xmin`, `xmax`, `seq`.
pub(crate) const VERSION_PREFIX_LEN: usize = 32;

/// Value written in the `xmax` field of a version no transaction has replaced or deleted.
///
/// `TxnId(u64::MAX)` is refused by the entry points of [`HeapTable`] so that the sentinel
/// stays free (`reserved_txn_id_is_a_bug`); the `txn` module hands out increasing ids from 1.
pub(crate) const NO_XMAX: u64 = u64::MAX;

/// Offset of the `xmax` field in the prefix of a version.
pub(crate) const OFF_XMAX: usize = 16;

/// A caller bug: a violated precondition of this module.
fn bug<T>(msg: impl Into<String>) -> Result<T, InternalError> {
    Err(InternalError::Bug(msg.into()))
}

/// A broken invariant of this module: reported rather than repaired blindly.
fn corruption<T>(msg: impl Into<String>) -> Result<T, InternalError> {
    Err(InternalError::Corruption(msg.into()))
}

/// The prefix of one version, as it stands in its heap record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VersionHeader {
    /// The logical row this version belongs to.
    pub(crate) row: RowId,
    /// The transaction that created this version.
    pub(crate) xmin: TxnId,
    /// The transaction that replaced or deleted it, `None` while it is current.
    pub(crate) xmax: Option<TxnId>,
    /// Serial number of the version within its table, unique and not handed out twice.
    pub(crate) seq: u64,
}

impl VersionHeader {
    /// Appends the [`VERSION_PREFIX_LEN`] bytes of the prefix to `out`.
    ///
    /// The journal writes the same prefix in the payload of its row records
    /// ([`super::wal_payload`]), which is why this is not private to the file.
    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.row.0.to_le_bytes());
        out.extend_from_slice(&self.xmin.0.to_le_bytes());
        out.extend_from_slice(&self.xmax.map_or(NO_XMAX, |x| x.0).to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
    }

    /// Reads the prefix at the start of `bytes`.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a record shorter than [`VERSION_PREFIX_LEN`]
    /// (`decode_version_refuses_a_record_shorter_than_the_prefix`).
    pub(crate) fn read_from(bytes: &[u8]) -> Result<Self, InternalError> {
        let xmax = le_u64(bytes, OFF_XMAX)?;
        Ok(Self {
            row: RowId(le_u64(bytes, 0)?),
            xmin: TxnId(le_u64(bytes, 8)?),
            xmax: (xmax != NO_XMAX).then_some(TxnId(xmax)),
            seq: le_u64(bytes, 24)?,
        })
    }
}

/// The little-endian `u64` at `at` in the prefix of a version.
fn le_u64(bytes: &[u8], at: usize) -> Result<u64, InternalError> {
    let field: [u8; 8] = bytes
        .get(at..at + 8)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| {
            InternalError::Corruption(format!(
                "heap record of {} bytes is shorter than the {VERSION_PREFIX_LEN}-byte version \
                 prefix",
                bytes.len()
            ))
        })?;
    Ok(u64::from_le_bytes(field))
}

/// The bytes of one version: its prefix then its columns.
fn encode_version(header: &VersionHeader, row: &Row) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(VERSION_PREFIX_LEN);
    header.write_to(&mut bytes);
    encode_row(row, &mut bytes);
    bytes
}

/// The same record with another `xmax` written in its prefix, the columns untouched.
fn with_xmax(bytes: &[u8], xmax: Option<TxnId>) -> Result<Vec<u8>, InternalError> {
    let field = xmax.map_or(NO_XMAX, |x| x.0).to_le_bytes();
    let mut patched = bytes.to_vec();
    match patched.get_mut(OFF_XMAX..OFF_XMAX + 8) {
        // A record too short for the whole prefix is refused even though the `xmax` field
        // would fit: the columns behind it are not readable either.
        Some(slot) if bytes.len() >= VERSION_PREFIX_LEN => slot.copy_from_slice(&field),
        _ => {
            return corruption(format!(
                "heap record of {} bytes is shorter than the {VERSION_PREFIX_LEN}-byte version \
                 prefix",
                bytes.len()
            ));
        }
    }
    Ok(patched)
}

/// One version as a reader outside this module sees it: what it belongs to, what settles its
/// visibility, and its columns.
///
/// This is what the index maintenance of `super::index` reads: the heap record itself, its
/// address and its encoding stay inside this file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VersionView {
    /// The logical row the version belongs to.
    pub(crate) row: RowId,
    /// The serial number of the version within its table.
    pub(crate) seq: u64,
    /// The transaction that created the version.
    pub(crate) xmin: TxnId,
    /// The transaction that replaced or deleted it, `None` while it is current.
    pub(crate) xmax: Option<TxnId>,
    /// The columns of the version.
    pub(crate) data: Row,
}

/// One version as it was read from the heap: where it sits, its prefix and its bytes.
#[derive(Debug, Clone)]
struct StoredVersion {
    /// Address of the record in the heap.
    rid: Rid,
    /// The prefix, already decoded.
    header: VersionHeader,
    /// The whole record: the prefix then the columns.
    bytes: Vec<u8>,
}

impl StoredVersion {
    /// The columns of the version.
    fn row(&self) -> Result<Row, InternalError> {
        match self.bytes.get(VERSION_PREFIX_LEN..) {
            Some(columns) => decode_row(columns),
            None => corruption(format!(
                "version {} of row {} holds no columns after its prefix",
                self.header.seq, self.header.row
            )),
        }
    }
}

/// One write logged for a transaction, undone in reverse order by [`HeapTable::rollback`].
///
/// Same semantics as the `UndoEntry` of the in-memory implementation, which is not reused
/// here so that `disk` and `memory` stay uncoupled: an entry names the logical row rather than
/// a position in its chain, and the writes of a running transaction on a row are the tail of
/// that chain, so each undo acts on its most recent versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UndoEntry {
    /// A logical row created by the transaction; undoing removes the version it created.
    Insert(RowId),
    /// A version replaced by the transaction followed by the one it created; undoing removes
    /// the new version and resets the `xmax` of the replaced one.
    Update(RowId),
    /// A version deleted by the transaction (`xmax` set on it); undoing resets that `xmax`.
    Delete(RowId),
}

/// What one table holds for one transaction: what to undo, and which of its pages carry the
/// `in_progress` flag.
///
/// The **status** is not here: it lives in the register of the instance
/// ([`super::TxnRegistry`]), so that a transaction that wrote in two tables is one `Begin` and
/// one end record (`super::tests::one_begin_one_end_per_txn_over_two_tables`).
#[derive(Debug, Default)]
pub(crate) struct TxnState {
    /// The writes of the transaction in chronological order, dropped by `commit` and
    /// consumed by `rollback`.
    pub(crate) writes: Vec<UndoEntry>,
    /// The heap pages the transaction wrote, carrying `in_progress` until it finishes.
    pub(crate) pages: BTreeSet<PageId>,
}

/// The mutable state of one table, kept by the instance rather than by the structure that
/// works on it.
///
/// These five fields live outside [`HeapTable`] and [`super::clustered::ClusteredTable`]: a
/// field of [`super::DiskStorage`] holding either of those two types asks for a lifetime
/// parameter that `pub struct DiskStorage` does not carry, while a field holding `TableState`
/// carries no lifetime. The table is built around `&DiskStorage` and this state at each call
/// ([`super::DiskStorage::with_rows`]).
///
/// `directory` and `versions` are read by the heap store: [`super::clustered::ClusteredTable`]
/// keeps its own directory in a B+tree of the instance instead.
#[derive(Debug)]
pub(crate) struct TableState {
    /// The next [`RowId`] to hand out; it grows, so an id taken away by a rollback is not
    /// handed out a second time (`rollback_undoes_insert`).
    pub(crate) next_row_id: u64,
    /// The next [`VersionHeader::seq`] to hand out; it grows, so a serial taken away by a
    /// rollback is not handed out a second time (`seq_is_not_reused_after_a_rollback`).
    pub(crate) next_seq: u64,
    /// Where the most recent version of each logical row sits, for the heap store; the
    /// clustered store keeps that map in a B+tree of the instance.
    pub(crate) directory: HashMap<RowId, Rid>,
    /// Where each version of each row sits, by `(RowId, seq)`, for the heap store: what an
    /// index entry names is looked up here rather than by a walk of the heap.
    pub(crate) versions: BTreeMap<(RowId, u64), Rid>,
    /// The transactions that wrote in this table and have not been forgotten, by [`TxnId`].
    pub(crate) txns: HashMap<TxnId, TxnState>,
    /// What the writes since the last `take_index_changes` mean for an index.
    pub(crate) index_log: Vec<IndexChange>,
    /// Whether the counters and the directory have been rebuilt from what the recovery found
    /// ([`super::DiskStorage::with_rows`] does it once per table and per `open`).
    pub(crate) resumed: bool,
}

impl TableState {
    /// Whether the state still has to be rebuilt from what the recovery of the `open` found.
    pub(crate) fn needs_resume(&self) -> bool {
        !self.resumed
    }

    /// Notes that the rebuild has been done.
    pub(crate) fn resumed(&mut self) {
        self.resumed = true;
    }
}

impl Default for TableState {
    /// An empty table: no version written, the two counters at 1.
    fn default() -> Self {
        Self {
            next_row_id: 1,
            next_seq: 1,
            directory: HashMap::new(),
            versions: BTreeMap::new(),
            txns: HashMap::new(),
            index_log: Vec::new(),
            resumed: false,
        }
    }
}

/// One table stored as a heap of versioned rows, without index.
///
/// The structure borrows the instance and holds the heap of the table, its shape, the
/// directory of the most recent version of each row and the undo logs of the transactions that
/// wrote in it. `super::storage_impl` answers [`crate::Storage`] by calling these methods,
/// which take `&mut self` — one writer at a time on a table, the serialisation being the lock
/// of [`TableState`] that caller holds.
#[derive(Debug)]
pub(crate) struct HeapTable<'storage> {
    /// The instance the pages belong to; the `in_progress` flag is set through its pool.
    storage: &'storage DiskStorage,
    /// The heap holding the versions of this table.
    heap: Heap<'storage>,
    /// Shape of the table: the arity a [`Row`] of this table must have.
    shape: TableShape,
    /// The counters, the directory, the undo logs and the maintenance log, which the instance
    /// holds between two calls ([`TableState`]).
    state: TableState,
}

impl<'storage> HeapTable<'storage> {
    /// Creates the table over `first_page`, a page [`super::alloc::allocate`] handed out.
    ///
    /// # Errors
    ///
    /// The errors of [`Heap::create`].
    pub(crate) fn create(
        storage: &'storage DiskStorage,
        table: TableId,
        first_page: PageId,
        shape: TableShape,
    ) -> Result<Self, InternalError> {
        let heap = Heap::create(storage, table, first_page)?;
        Ok(Self::over(storage, heap, shape))
    }

    /// Attaches to the heap of `table` whose head page is `first_page`, leaving the pages as
    /// they stand.
    ///
    /// The two maps of the directory and the counters start empty and live in memory. What a
    /// table reopened this way answers, with one row committed before the `open`
    /// (`open_serves_the_rows_but_hands_out_row_id_one_again`): [`HeapTable::scan`] serves the
    /// row, because it walks the heap; [`HeapTable::get`] and [`HeapTable::latest_version`]
    /// answer `None` and [`HeapTable::update`] and [`HeapTable::delete`] answer
    /// [`InternalError::Bug`], because they go through the directory; and the next [`HeapTable::insert`] hands out `RowId(1)` a second time, so
    /// the heap then holds two chains under that id and `scan` answers two rows for it.
    /// [`super::DiskStorage::with_rows`] is what rebuilds the directory and the counters
    /// before a call of the engine reaches the table.
    pub(crate) fn open(
        storage: &'storage DiskStorage,
        table: TableId,
        first_page: PageId,
        shape: TableShape,
    ) -> Self {
        Self::over(storage, Heap::open(storage, table, first_page), shape)
    }

    /// Takes the counters and the register of the recovery, and rebuilds the directory by
    /// walking the heap.
    ///
    /// For a table attached by [`HeapTable::open`] after [`super::DiskStorage::open`] replayed
    /// the journal on its heap. The two counters move
    /// forward, so a [`RowId`] an interrupted insert took is not handed out a second time
    /// (`recover::tests::a_row_id_taken_by_a_loser_is_not_handed_out_again`); `losers` are
    /// registered `Aborted`, which is what hides a version of theirs that reached `data`
    /// through a page shared with a committing transaction
    /// (`recover::tests::a_loser_whose_page_reached_data_is_invisible`).
    ///
    /// `winners` are registered `Committed`. [`HeapTable::status`] answers `Committed` for a
    /// transaction the table has not seen, so this changes no visibility; what it changes is
    /// that an identifier the journal has already ended is refused as a writer, as it is on a
    /// table that stayed open: [`HeapTable::insert`] under the identifier of a winner answers
    /// [`InternalError::Bug`] rather than writing a row
    /// (`recover::tests::a_winner_is_registered_committed_and_may_not_write_again`). The two
    /// lists are the ones of the journal of the instance, held per instance rather than per
    /// table in [`super::TxnRegistry`].
    ///
    /// Before the directory is rebuilt, the bytes the losers left on the heap are taken back,
    /// each by a journalled compensation ([`HeapTable::take_back_losers`]). The directory is
    /// then rebuilt from the heap rather than from the records of the redo, each row taking
    /// the address of its largest `seq`, [`HeapTable::stored_versions`] answering in that
    /// order.
    ///
    /// # Errors
    ///
    /// Those of [`Heap::get`] and of [`VersionHeader::read_from`] while the heap is walked,
    /// and those of the journal and of the heap while the losers are taken back.
    pub(crate) fn resume_after_recovery(
        &mut self,
        next_row_id: u64,
        next_seq: u64,
        winners: &[TxnId],
        losers: &[TxnId],
    ) -> Result<(), InternalError> {
        self.state.next_row_id = self.state.next_row_id.max(next_row_id);
        self.state.next_seq = self.state.next_seq.max(next_seq);
        self.state.resumed();
        self.storage.register_recovered(winners, losers)?;
        self.take_back_losers(losers)?;
        self.state.directory.clear();
        self.state.versions.clear();
        for version in self.stored_versions(None)? {
            let (row, seq) = (version.header.row, version.header.seq);
            self.state.versions.insert((row, seq), version.rid);
            if self.status(version.header.xmin) == TxnStatus::Aborted {
                continue;
            }
            self.state.directory.insert(row, version.rid);
        }
        Ok(())
    }

    /// Takes back what the losers left on the heap: the versions they created leave their
    /// slot, and the `xmax` they set on a version of another writer is unset in place.
    ///
    /// Such bytes are on the heap when the page reached `data` through a page shared with a
    /// committing transaction (`super::recover`, "Losers"). Left there, a version of a loser
    /// is hidden by the register, but it stays the last of the chain of its row, and the undo
    /// of a later update of that row would find it where it looks for the version the update
    /// replaced (`recover::tests::a_row_a_loser_updated_then_deleted_takes_an_update_and_its_rollback`);
    /// an `xmax` of a loser makes [`HeapTable::check_current`] refuse an update of the row
    /// (`a_loser_xmax_on_data_is_cleared`).
    ///
    /// Each change is journalled first, as the undo of a rollback journals it: one
    /// [`WalRecordKind::Compensation`] under the loser that wrote the bytes,
    /// [`Release::Removed`] for a version it created and [`Release::XmaxCleared`] for an
    /// `xmax` it set. The journal is `sync_all`ed before the first slot changes, and each page
    /// takes the number of its record in the change, so a page that reaches `data` with the
    /// change is a page whose record is durable, and a recovery cut short replays the records
    /// that were appended and takes back the rest
    /// (`recover::tests::a_loser_taken_back_by_a_recovery_cut_short_is_taken_back_by_the_next`).
    fn take_back_losers(&mut self, losers: &[TxnId]) -> Result<(), InternalError> {
        let losers: BTreeSet<TxnId> = losers.iter().copied().collect();
        let mut doomed = Vec::new();
        for version in self.stored_versions(None)? {
            if losers.contains(&version.header.xmin) {
                doomed.push((version.header.xmin, version, Release::Removed));
            } else if let Some(x) = version.header.xmax
                && losers.contains(&x)
            {
                doomed.push((x, version, Release::XmaxCleared));
            }
        }
        if doomed.is_empty() {
            return Ok(());
        }
        let mut records = Vec::with_capacity(doomed.len());
        for (writer, version, release) in &doomed {
            records.push(self.log_release(
                WalRecordKind::Compensation,
                *writer,
                version,
                *release,
            )?);
        }
        self.storage.wal.flush()?;
        for ((_, version, release), lsn) in doomed.into_iter().zip(records) {
            match release {
                Release::Removed => {
                    self.heap.free_slot(version.rid, lsn)?;
                }
                Release::XmaxCleared => {
                    let patched = with_xmax(&version.bytes, None)?;
                    self.heap.overwrite(
                        version.rid,
                        OFF_XMAX,
                        &patched[OFF_XMAX..OFF_XMAX + 8],
                        lsn,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// The table over `heap`, with an empty directory and counters starting at 1.
    fn over(storage: &'storage DiskStorage, heap: Heap<'storage>, shape: TableShape) -> Self {
        Self::resume(storage, heap, shape, TableState::default())
    }

    /// The table over `heap`, around the state the instance kept for it.
    ///
    /// This is how [`super::DiskStorage::with_rows`] rebuilds the table at each call: the
    /// state is moved in here and moved back out by [`HeapTable::into_state`] when the call
    /// returns, the instance holding it under the lock of that table in between.
    pub(crate) fn resume(
        storage: &'storage DiskStorage,
        heap: Heap<'storage>,
        shape: TableShape,
        state: TableState,
    ) -> Self {
        Self {
            storage,
            heap,
            shape,
            state,
        }
    }

    /// Hands the state of the table back to the instance.
    pub(crate) fn into_state(self) -> TableState {
        self.state
    }

    /// The table these rows belong to.
    pub(crate) fn table(&self) -> TableId {
        self.heap.table()
    }

    /// The heap holding the versions.
    pub(crate) fn heap(&self) -> &Heap<'storage> {
        &self.heap
    }

    /// The shape the rows of this table must have.
    pub(crate) fn shape(&self) -> &TableShape {
        &self.shape
    }

    // ---------------------------------------------------------------------- Rows

    /// Adds a logical row holding `row`, created by `txn`, and answers its fresh [`RowId`].
    ///
    /// The version is written with `xmax` unset and the transaction records an
    /// [`UndoEntry::Insert`]. The row is visible to `txn` at once and to a later snapshot
    /// once `txn` commits (`own_insert_visible_before_commit`,
    /// `commit_makes_row_visible_to_others`).
    ///
    /// The record of the write goes to the journal before the heap is changed, and a
    /// [`WalRecordKind::Begin`] precedes it when this is the first write of `txn`. After an
    /// insert that has not committed, the journal file holds the `Begin` and the
    /// `Insert`, while the page of `data` is the image it carried before the call
    /// (`insert_without_commit_not_on_data`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a `row` whose arity is not the one of [`HeapTable::shape`],
    /// for a transaction that already committed or rolled back and for the reserved
    /// `TxnId(u64::MAX)`; the errors of [`Heap::insert`] and of the journal otherwise.
    ///
    /// Among those journal errors: a row whose encoded version, with the four bytes of the
    /// table and the 32 of the prefix, is past [`super::wal::MAX_PAYLOAD_LEN`] (a record of
    /// 1 MiB, frame included) is [`InternalError::Bug`] from [`super::wal::Wal::append`]. The
    /// record goes first, so the heap is left as it was and the next insert goes through;
    /// the `Begin` of the transaction stays in the journal and the transaction stays in
    /// progress (`a_row_whose_record_is_past_the_payload_limit_is_a_bug`, one `varchar(max)`
    /// of 2 MiB). A row that long is refused rather than split across records: splitting one
    /// write over several records, and reassembling it at redo, is not implemented.
    pub(crate) fn insert(&mut self, txn: TxnId, row: &Row) -> Result<RowId, InternalError> {
        check_txn(txn)?;
        self.check_arity(row)?;
        self.check_writable(txn)?;
        let id = RowId(self.state.next_row_id);
        let Some(next) = self.state.next_row_id.checked_add(1) else {
            return bug(format!("row id space of table {} exhausted", self.table()));
        };
        self.begin_write(txn)?;
        let header = VersionHeader {
            row: id,
            xmin: txn,
            xmax: None,
            seq: self.take_seq()?,
        };
        let bytes = encode_version(&header, row);
        let rid = self.heap.reserve(bytes.len())?;
        let lsn = self.log_row(
            WalRecordKind::Insert,
            txn,
            &header,
            Some(row),
            &[PageOp::Put { rid }],
        )?;
        self.write_version(txn, lsn, rid, &header, &bytes)?;
        self.state.next_row_id = next;
        self.state.directory.insert(id, rid);
        self.note_index_change(IndexChange::Added {
            row: id,
            seq: header.seq,
            data: row.clone(),
        });
        self.log(txn, UndoEntry::Insert(id))?;
        Ok(id)
    }

    /// Replaces the content of `id` with `row` on behalf of `txn`, keeping the [`RowId`].
    ///
    /// The current version takes `xmax = txn` and a new version holding `row` is chained
    /// after it; the transaction records an [`UndoEntry::Update`]. A snapshot that has not
    /// settled `txn` keeps seeing the old content (`update_keeps_row_id_and_old_version`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for the wrong arity, an unknown row, a current version that
    /// already carries an `xmax` or that was created by another transaction still in
    /// progress (`update_on_uncommitted_other_is_bug`, which leaves the heap as it stands), a
    /// finished transaction and the reserved `TxnId(u64::MAX)`.
    pub(crate) fn update(&mut self, txn: TxnId, id: RowId, row: &Row) -> Result<(), InternalError> {
        check_txn(txn)?;
        self.check_arity(row)?;
        let current = self.current(id)?;
        self.check_current(txn, &current)?;
        self.check_writable(txn)?;
        self.begin_write(txn)?;
        let header = VersionHeader {
            row: id,
            xmin: txn,
            xmax: None,
            seq: self.take_seq()?,
        };
        // One record for the pair (`xmax` on the replaced version, the new version), so the
        // two heap writes carry the LSN of the record that describes them both.
        let bytes = encode_version(&header, row);
        let rid = self.heap.reserve(bytes.len())?;
        let ops = [
            PageOp::SetXmax {
                rid: current.rid,
                seq: current.header.seq,
            },
            PageOp::Put { rid },
        ];
        let lsn = self.log_row(WalRecordKind::Update, txn, &header, Some(row), &ops)?;
        self.set_xmax(txn, lsn, &current, Some(txn))?;
        self.write_version(txn, lsn, rid, &header, &bytes)?;
        self.state.directory.insert(id, rid);
        self.note_index_change(IndexChange::Added {
            row: id,
            seq: header.seq,
            data: row.clone(),
        });
        self.log(txn, UndoEntry::Update(id))
    }

    /// Deletes the logical row `id` on behalf of `txn`: `xmax = txn` on its current version.
    ///
    /// The row is hidden from `txn` at once and from a later snapshot once `txn` commits; a
    /// rollback shows it again (`delete_visibility`). The transaction records an
    /// [`UndoEntry::Delete`].
    ///
    /// # Errors
    ///
    /// Those of [`HeapTable::update`], the arity apart.
    pub(crate) fn delete(&mut self, txn: TxnId, id: RowId) -> Result<(), InternalError> {
        check_txn(txn)?;
        let current = self.current(id)?;
        self.check_current(txn, &current)?;
        self.check_writable(txn)?;
        self.begin_write(txn)?;
        // The record names the version being hidden: its `seq` and its `xmin`, with the
        // `xmax` the write sets.
        let hidden = VersionHeader {
            xmax: Some(txn),
            ..current.header
        };
        let ops = [PageOp::SetXmax {
            rid: current.rid,
            seq: current.header.seq,
        }];
        let lsn = self.log_row(WalRecordKind::Delete, txn, &hidden, None, &ops)?;
        let rid = self.set_xmax(txn, lsn, &current, Some(txn))?;
        self.state.directory.insert(id, rid);
        self.log(txn, UndoEntry::Delete(id))
    }

    /// The content of `id` as `snap` sees it, `None` when no version of the row is visible.
    ///
    /// The chain of the row is read through the directory of the versions, from the most
    /// recent version back, and filtered by [`Snapshot::is_visible`]: at most one version
    /// answers, by the chain invariant of the crate root.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a directory entry whose record is gone; the errors of
    /// [`Heap::get`] and [`super::encode::decode_row`].
    pub(crate) fn get(&self, snap: &Snapshot, id: RowId) -> Result<Option<Row>, InternalError> {
        let status = |t| self.status(t);
        for version in self.stored_versions(Some(id))?.iter().rev() {
            if snap.is_visible(version.header.xmin, version.header.xmax, &status) {
                return version.row().map(Some);
            }
        }
        Ok(None)
    }

    /// The version `seq` of the row `id`, `None` when the directory of the versions does not
    /// hold it.
    ///
    /// One lookup in [`TableState::versions`] and one read of the heap record it names.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for an entry whose record is gone or carries another
    /// version; the errors of [`Heap::get`] and [`super::encode::decode_row`].
    pub(crate) fn version(
        &self,
        id: RowId,
        seq: u64,
    ) -> Result<Option<VersionView>, InternalError> {
        let Some(&rid) = self.state.versions.get(&(id, seq)) else {
            return Ok(None);
        };
        let stored = self.stored_at(rid, id, seq)?;
        Ok(Some(VersionView {
            row: id,
            seq,
            xmin: stored.header.xmin,
            xmax: stored.header.xmax,
            data: stored.row()?,
        }))
    }

    /// The rows `snap` sees, by increasing [`RowId`], one entry per visible logical row.
    ///
    /// The rows are copied out of the heap before the call returns: a write made after it
    /// does not reach the caller, which is the isolation the trait asks of
    /// [`crate::Storage::scan`] and what the contract suite checks. A table without clustered
    /// key has
    /// no ordering contract; this implementation answers in `RowId` order, as
    /// [`crate::MemoryStorage`] does (`scan_lists_visible_rows_in_row_id_order`).
    ///
    /// # Errors
    ///
    /// Those of [`HeapTable::get`].
    pub(crate) fn scan(&self, snap: &Snapshot) -> Result<Vec<(RowId, Row)>, InternalError> {
        let status = |t| self.status(t);
        let mut rows = Vec::new();
        for version in self.stored_versions(None)? {
            if snap.is_visible(version.header.xmin, version.header.xmax, &status) {
                rows.push((version.header.row, version.row()?));
            }
        }
        Ok(rows)
    }

    /// The tail of the chain of `id`, regardless of the statuses: the transaction that wrote the
    /// most recent state (the deleter if there is one, else the creator) and its content.
    ///
    /// No snapshot is involved: this is what `txn` reads to detect a write conflict. A row
    /// the directory does not hold answers `None` (`latest_version_is_the_tail_of_the_chain`).
    ///
    /// # Errors
    ///
    /// The errors of [`Heap::get`] and [`super::encode::decode_row`].
    pub(crate) fn latest_version(&self, id: RowId) -> Result<Option<(TxnId, Row)>, InternalError> {
        if !self.state.directory.contains_key(&id) {
            return Ok(None);
        }
        let current = self.current(id)?;
        let writer = current.header.xmax.unwrap_or(current.header.xmin);
        Ok(Some((writer, current.row()?)))
    }

    /// The next [`RowId`] the table hands out, which the catalogue keeps between two opens.
    pub(crate) fn next_row_id(&self) -> u64 {
        self.state.next_row_id
    }

    /// Whether the state of this table still has to be rebuilt from what the recovery found.
    pub(crate) fn state_needs_resume(&self) -> bool {
        self.state.needs_resume()
    }

    /// Checks what [`HeapTable::insert`] checks, without writing anything.
    ///
    /// Called by [`crate::Storage::insert`] **before** the uniqueness of the indexes, so a
    /// precondition of the store is reported as the [`InternalError::Bug`] the trait asks for
    /// even when the row would also break a `unique` index, which is the order
    /// [`crate::MemoryStorage::insert`] follows
    /// (`super::tests::a_finished_txn_is_refused_before_uniqueness`,
    /// `super::tests::a_row_of_the_wrong_arity_is_refused_before_uniqueness`).
    ///
    /// # Errors
    ///
    /// Those of [`HeapTable::insert`], the write apart.
    pub(crate) fn check_insert(&self, txn: TxnId, row: &Row) -> Result<(), InternalError> {
        check_txn(txn)?;
        self.check_arity(row)?;
        self.check_writable(txn)
    }

    /// Checks what [`HeapTable::update`] checks, without writing anything.
    ///
    /// [`crate::Storage::update`] calls this **before** the uniqueness of the indexes, so a
    /// stale version is reported as such even when the new row would also break a `unique`
    /// index: that is the order [`crate::MemoryStorage`] follows
    /// (`super::tests::stale_version_is_refused_before_uniqueness`).
    ///
    /// # Errors
    ///
    /// Those of [`HeapTable::update`], the write apart.
    pub(crate) fn check_update(
        &self,
        txn: TxnId,
        id: RowId,
        row: &Row,
    ) -> Result<(), InternalError> {
        check_txn(txn)?;
        self.check_arity(row)?;
        let current = self.current(id)?;
        self.check_current(txn, &current)?;
        self.check_writable(txn)
    }

    // ------------------------------------------------- Transaction life cycle

    /// Appends a [`WalRecordKind::Commit`] record, `sync_all`s the journal, marks `txn` as
    /// `Committed`, drops its undo log and clears the `in_progress` flag of the pages it
    /// wrote.
    ///
    /// The records of the transaction are on disk when this returns, which is the durability
    /// [`crate::Storage::commit`] promises (`commit_fsyncs_wal`). `data` is **not** synced:
    /// under no-steal the pages of the transaction are still in the pool, and the journal is
    /// what a crash is replayed from ([`super::recover`]). Clearing the flag is what lets the pool
    /// write them from then on (`committed_page_can_evict`).
    ///
    /// A transaction this table has not seen write anything is committed without a record, as
    /// the trait allows (`a_commit_without_a_write_appends_no_record`): the `Begin` is
    /// appended at the first write, not at the first call naming the transaction.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a transaction that already committed or rolled back
    /// (`a_second_commit_is_a_bug`) and for the reserved `TxnId(u64::MAX)`; the errors of the
    /// journal and of the buffer pool while the flag is cleared.
    pub(crate) fn commit(&mut self, txn: TxnId) -> Result<(), InternalError> {
        check_txn(txn)?;
        self.finish_in_journal(txn, WalRecordKind::Commit)?;
        self.finish(txn, TxnStatus::Committed)
    }

    /// Applies to this table an end of transaction the instance has already journalled.
    ///
    /// [`crate::Storage::commit`] and [`crate::Storage::rollback`] append **one** record for
    /// the transaction, then walk the tables it wrote in and call this on each of them: a
    /// `Committed` end drops the undo log, an `Aborted` one replays it backwards, and both
    /// clear the `in_progress` flag of the pages the transaction wrote in this table.
    ///
    /// # Errors
    ///
    /// Those of [`HeapTable::rollback`] for the undo, and of the buffer pool for the flag.
    pub(crate) fn finish(&mut self, txn: TxnId, status: TxnStatus) -> Result<(), InternalError> {
        let writes = self.finish_txn(txn);
        if status == TxnStatus::Aborted && !writes.is_empty() {
            for entry in writes.into_iter().rev() {
                self.undo(txn, entry)?;
            }
            // The compensation records go to disk before the flag of the pages comes down: a
            // page whose flag is down may be written, and it carries their LSN.
            self.storage.wal.flush()?;
        }
        self.release_pages(txn)
    }

    /// Marks `txn` as `Aborted` and replays its undo log backwards.
    ///
    /// An `Insert` loses its version and its directory entry, an `Update` loses the version
    /// it created and gives the replaced one its `xmax` back, a `Delete` gives back the
    /// `xmax` of the version it hid. The [`RowId`] and the `seq` of what is taken away are
    /// not handed out again.
    ///
    /// A [`WalRecordKind::Abort`] record is appended and the journal is `sync_all`ed before
    /// the undo runs (`rollback_appends_abort`), so that a transaction whose `Begin` is
    /// followed by neither a `Commit` nor an `Abort` is one a crash interrupted, which is what
    /// the recovery sorts the losers by. The undo then appends one
    /// [`WalRecordKind::Compensation`] per version it takes out or whose `xmax` it unsets, and `sync_all`s
    /// the journal again before the `in_progress` flag of the pages comes down
    /// ([`HeapTable::finish`]): a page another transaction shares may have reached `data`
    /// holding the versions of this one, and the redo takes them out by these records
    /// (`recover::tests::a_rollback_on_a_page_already_on_data_gives_its_room_back_at_the_redo`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] as in [`HeapTable::commit`]; [`InternalError::Corruption`] when
    /// the chain of a row does not end with the write being undone, which the preconditions
    /// of `update` and `delete` rule out.
    pub(crate) fn rollback(&mut self, txn: TxnId) -> Result<(), InternalError> {
        check_txn(txn)?;
        self.finish_in_journal(txn, WalRecordKind::Abort)?;
        self.finish(txn, TxnStatus::Aborted)
    }

    /// The status of `t` as [`Snapshot::is_visible`] must see it: the registered status, or
    /// `Committed` for a transaction this table has not seen (same rule as
    /// [`crate::MemoryStorage`]).
    pub(crate) fn status(&self, t: TxnId) -> TxnStatus {
        self.storage.txn_status(t)
    }

    // ------------------------------------------------------ Index maintenance

    /// The versions the heap holds, of the row `only` or of the whole table, by increasing
    /// `(RowId, seq)`.
    ///
    /// This is how `super::index` reads the versions of a table: the visibility fields of a
    /// version and its columns, without the heap address and the encoding, which stay here.
    /// The versions of one row are read through the directory of the versions; those of the
    /// whole table by a walk of the heap.
    ///
    /// # Errors
    ///
    /// Those of [`Heap::scan`], of [`Heap::get`] and of [`super::encode::decode_row`].
    pub(crate) fn versions(&self, only: Option<RowId>) -> Result<Vec<VersionView>, InternalError> {
        self.stored_versions(only)?
            .iter()
            .map(|version| {
                Ok(VersionView {
                    row: version.header.row,
                    seq: version.header.seq,
                    xmin: version.header.xmin,
                    xmax: version.header.xmax,
                    data: version.row()?,
                })
            })
            .collect()
    }

    /// Takes the maintenance log of the writes made since the previous call.
    ///
    /// The caller drains it after each write and hands it to `super::index::DiskIndex::apply`,
    /// which is what keeps the trees of the table in step with its heap;
    /// `super::storage_impl` is where the two are tied together behind [`crate::Storage`].
    /// Applying the same log twice would put an entry in a tree twice, so the log is emptied
    /// here.
    ///
    /// What each write records, the shape `super::index` documents:
    ///
    /// | Write | Records |
    /// |---|---|
    /// | [`HeapTable::insert`] | [`IndexChange::Added`] for the version created |
    /// | [`HeapTable::update`] | [`IndexChange::Added`] for the version created; the replaced one keeps its entry, which an older snapshot still reads through |
    /// | [`HeapTable::delete`] | nothing: the version stays and visibility hides it |
    /// | [`HeapTable::rollback`] | [`IndexChange::Removed`] for each version its undo took away |
    pub(crate) fn take_index_changes(&mut self) -> Vec<IndexChange> {
        std::mem::take(&mut self.state.index_log)
    }

    /// Records one entry in the maintenance log, after the heap write it describes went
    /// through.
    fn note_index_change(&mut self, change: IndexChange) {
        self.state.index_log.push(change);
    }

    /// Records the removal of a version an undo took out of the heap.
    ///
    /// # Errors
    ///
    /// Those of [`super::encode::decode_row`] on the columns of the version.
    fn note_undone_version(&mut self, removed: &StoredVersion) -> Result<(), InternalError> {
        let change = IndexChange::Removed {
            row: removed.header.row,
            seq: removed.header.seq,
            data: removed.row()?,
        };
        self.note_index_change(change);
        Ok(())
    }

    // ---------------------------------------------------------------- Internals

    /// Checks the arity of `row` against the shape of the table.
    fn check_arity(&self, row: &Row) -> Result<(), InternalError> {
        let arity = self.shape.columns.len();
        if row.0.len() != arity {
            return bug(format!(
                "row has {} values, table {} has {arity} columns",
                row.0.len(),
                self.table()
            ));
        }
        Ok(())
    }

    /// Refuses a transaction that already committed or rolled back, registering nothing.
    fn check_writable(&self, txn: TxnId) -> Result<(), InternalError> {
        self.storage.check_writable(txn)
    }

    /// Checks the precondition of `update` and `delete` on the current version of a row: it
    /// carries no `xmax`, and it was created by `txn` or by a committed transaction.
    fn check_current(&self, txn: TxnId, current: &StoredVersion) -> Result<(), InternalError> {
        let header = current.header;
        if let Some(x) = header.xmax {
            return bug(format!(
                "row {} of table {} is stale: its latest version was replaced or deleted by \
                 transaction {x}",
                header.row,
                self.table()
            ));
        }
        if header.xmin != txn && self.status(header.xmin) != TxnStatus::Committed {
            return bug(format!(
                "row {} of table {} is being written by transaction {}, which is {:?}",
                header.row,
                self.table(),
                header.xmin,
                self.status(header.xmin)
            ));
        }
        Ok(())
    }

    /// Registers `txn` as `InProgress` if this table had not seen it yet, appending the
    /// [`WalRecordKind::Begin`] record of its first write.
    ///
    /// The journal discovers a transaction at its first write, not when the `txn` module hands
    /// out its identifier: a transaction that reads and commits leaves nothing in
    /// the journal (`a_commit_without_a_write_appends_no_record`). The payload is empty, the
    /// `txn` field of the record naming the transaction.
    fn begin_write(&mut self, txn: TxnId) -> Result<(), InternalError> {
        self.storage.begin_txn(txn, self.heap.table())?;
        self.state.txns.entry(txn).or_default();
        Ok(())
    }

    /// Appends the record that ends `txn` and `sync_all`s the journal, when `txn` wrote.
    ///
    /// A transaction that wrote nothing has no `Begin` in the journal, so it gets no `Commit`
    /// and no `Abort` either. A transaction that already finished is left to
    /// [`HeapTable::finish_txn`], which is what reports the bug: nothing is appended for it.
    fn finish_in_journal(&mut self, txn: TxnId, kind: WalRecordKind) -> Result<(), InternalError> {
        self.storage.end_txn(txn, kind).map(|_| ())
    }

    /// Appends the record of one row write and answers its LSN.
    ///
    /// Called **before** the heap is changed, once the addresses the write changes are known
    /// ([`Heap::reserve`] for a new version): the record of a change reaches the file first,
    /// naming the pages it changes in `ops`, and the LSN it answers is what each of those pages
    /// takes as its `page_lsn` in the change itself. `row` is the content of the version for
    /// an `Insert` and an `Update`, `None` for a `Delete` ([`super::wal_payload`]).
    fn log_row(
        &mut self,
        kind: WalRecordKind,
        txn: TxnId,
        header: &VersionHeader,
        row: Option<&Row>,
        ops: &[PageOp],
    ) -> Result<Lsn, InternalError> {
        let mut columns = Vec::new();
        if let Some(row) = row {
            encode_row(row, &mut columns);
        }
        let payload = RowChange {
            table: self.table(),
            header: *header,
            columns,
        }
        .encode_with(ops);
        self.storage.wal.append(kind, txn, &payload)
    }

    /// Records one write of `txn` in its undo log.
    fn log(&mut self, txn: TxnId, entry: UndoEntry) -> Result<(), InternalError> {
        match self.state.txns.get_mut(&txn) {
            Some(state) => {
                state.writes.push(entry);
                Ok(())
            }
            None => corruption(format!(
                "transaction {txn} is not registered in table {}",
                self.heap.table()
            )),
        }
    }

    /// Hands out the next serial number of a version.
    fn take_seq(&mut self) -> Result<u64, InternalError> {
        let seq = self.state.next_seq;
        match self.state.next_seq.checked_add(1) {
            Some(next) => {
                self.state.next_seq = next;
                Ok(seq)
            }
            None => bug(format!(
                "version serial space of table {} exhausted",
                self.table()
            )),
        }
    }

    /// Writes the version `bytes` at `rid`, the address [`Heap::reserve`] answered before the
    /// record was logged, and marks its page as written by `txn` at the LSN of that record.
    fn write_version(
        &mut self,
        txn: TxnId,
        lsn: Lsn,
        rid: Rid,
        header: &VersionHeader,
        bytes: &[u8],
    ) -> Result<(), InternalError> {
        if !self.heap.put(rid, bytes, lsn)? {
            return bug(format!(
                "version {} of row {} of table {} does not fit at {rid}, the address reserved \
                 for it",
                header.seq,
                header.row,
                self.table()
            ));
        }
        self.state.versions.insert((header.row, header.seq), rid);
        self.note_page(txn, rid.page, lsn)
    }

    /// Writes another `xmax` in the prefix of the version `stored`, in place, and answers
    /// where it sits: at its [`Rid`]
    /// (`the_records_name_the_pages_they_change_and_the_xmax_is_set_in_place`).
    fn set_xmax(
        &mut self,
        txn: TxnId,
        lsn: Lsn,
        stored: &StoredVersion,
        xmax: Option<TxnId>,
    ) -> Result<Rid, InternalError> {
        let patched = with_xmax(&stored.bytes, xmax)?;
        let field = &patched[OFF_XMAX..OFF_XMAX + 8];
        if !self.heap.overwrite(stored.rid, OFF_XMAX, field, lsn)? {
            return corruption(format!(
                "version {} of row {} is not at {} in table {}",
                stored.header.seq,
                stored.header.row,
                stored.rid,
                self.table()
            ));
        }
        self.note_page(txn, stored.rid.page, lsn)?;
        Ok(stored.rid)
    }

    /// Ties `page` to the journal record of LSN `lsn`, flags it as written by a transaction
    /// in progress and records it for `txn`.
    ///
    /// The page is pinned for the call: [`super::buffer::BufferPool::mark_dirty`] and
    /// [`super::buffer::BufferPool::set_in_progress`] ask for a page the pool holds. The LSN
    /// is what keeps the page in the cache until the journal is flushed: the pool refuses to
    /// write a page whose LSN is past the durable one (`flush_page_before_wal_flush_is_bug`).
    fn note_page(&mut self, txn: TxnId, page: PageId, lsn: Lsn) -> Result<(), InternalError> {
        let pin = self.storage.pool.pin(page)?;
        self.storage.pool.mark_dirty(page, lsn)?;
        self.storage.pool.set_in_progress(page, true)?;
        drop(pin);
        match self.state.txns.get_mut(&txn) {
            Some(state) => {
                state.pages.insert(page);
                Ok(())
            }
            None => corruption(format!(
                "transaction {txn} is not registered in table {}",
                self.heap.table()
            )),
        }
    }

    /// Clears the `in_progress` flag of the pages `txn` wrote, so the pool may write them.
    fn release_pages(&mut self, txn: TxnId) -> Result<(), InternalError> {
        let pages = match self.state.txns.get_mut(&txn) {
            Some(state) => std::mem::take(&mut state.pages),
            None => return Ok(()),
        };
        for page in pages {
            self.storage.pool.set_in_progress(page, false)?;
        }
        Ok(())
    }

    /// Hands back the undo log `txn` left in this table and forgets it.
    ///
    /// A transaction this table has not seen wrote nothing here and answers an empty log; the
    /// second `commit` or `rollback` of one transaction is reported by
    /// [`super::DiskStorage::end_txn`], which holds the status of the instance.
    fn finish_txn(&mut self, txn: TxnId) -> Vec<UndoEntry> {
        match self.state.txns.get_mut(&txn) {
            Some(state) => std::mem::take(&mut state.writes),
            None => Vec::new(),
        }
    }

    /// Applies one undo entry of `txn`.
    ///
    /// Each version the undo takes out of its slot is journalled first, by a
    /// [`WalRecordKind::Compensation`] whose LSN the pages of the undo carry: the version
    /// created by an `Insert` or an `Update` leaves the heap ([`Release::Removed`]), the
    /// version whose `xmax` an `Update` or a `Delete` set has it unset in place
    /// ([`Release::XmaxCleared`]). The caller `sync_all`s the journal once the walk is done.
    fn undo(&mut self, txn: TxnId, entry: UndoEntry) -> Result<(), InternalError> {
        match entry {
            UndoEntry::Insert(row) => {
                let created = self.tail_created_by(txn, row)?;
                let lsn =
                    self.log_release(WalRecordKind::Compensation, txn, &created, Release::Removed)?;
                self.remove_version(txn, lsn, &created)?;
                self.note_undone_version(&created)?;
                self.state.directory.remove(&row);
                Ok(())
            }
            UndoEntry::Update(row) => {
                let created = self.tail_created_by(txn, row)?;
                let lsn =
                    self.log_release(WalRecordKind::Compensation, txn, &created, Release::Removed)?;
                self.remove_version(txn, lsn, &created)?;
                self.note_undone_version(&created)?;
                self.state.directory.remove(&row);
                let Some(replaced) = self.stored_versions(Some(row))?.pop() else {
                    return corruption(format!(
                        "row {row} of table {} has no version left under the update of \
                         transaction {txn} being undone",
                        self.table()
                    ));
                };
                if replaced.header.xmax != Some(txn) {
                    return corruption(format!(
                        "version {} of row {row} of table {} does not carry the xmax of \
                         transaction {txn} being undone",
                        replaced.header.seq,
                        self.table()
                    ));
                }
                let lsn = self.log_release(
                    WalRecordKind::Compensation,
                    txn,
                    &replaced,
                    Release::XmaxCleared,
                )?;
                let rid = self.set_xmax(txn, lsn, &replaced, None)?;
                self.state.directory.insert(row, rid);
                Ok(())
            }
            UndoEntry::Delete(row) => {
                let deleted = self.current(row)?;
                if deleted.header.xmax != Some(txn) {
                    return corruption(format!(
                        "row {row} of table {} does not end with the delete of transaction \
                         {txn} being undone",
                        self.table()
                    ));
                }
                let lsn = self.log_release(
                    WalRecordKind::Compensation,
                    txn,
                    &deleted,
                    Release::XmaxCleared,
                )?;
                let rid = self.set_xmax(txn, lsn, &deleted, None)?;
                self.state.directory.insert(row, rid);
                Ok(())
            }
        }
    }

    /// The current version of `row`, checked to be the one `txn` created and left current:
    /// what the undo of an `Insert` or of an `Update` takes away.
    fn tail_created_by(&self, txn: TxnId, row: RowId) -> Result<StoredVersion, InternalError> {
        let current = self.current(row)?;
        if current.header.xmin != txn || current.header.xmax.is_some() {
            return corruption(format!(
                "row {row} of table {} does not end with a version created and left current \
                 by transaction {txn} being undone",
                self.table()
            ));
        }
        Ok(current)
    }

    /// Removes one version from the heap and from the directory of the versions, the page it
    /// sat on flagged for `txn` at `lsn`.
    ///
    /// `lsn` is the record that describes the release, already appended. The page is flagged
    /// and tied to it **before** the slot is freed, so that the pool does not write the freed
    /// slot ahead of that record.
    fn remove_version(
        &mut self,
        txn: TxnId,
        lsn: Lsn,
        stored: &StoredVersion,
    ) -> Result<(), InternalError> {
        self.note_page(txn, stored.rid.page, lsn)?;
        if !self.heap.free_slot(stored.rid, lsn)? {
            return corruption(format!(
                "version {} of row {} is not at {} in table {}",
                stored.header.seq,
                stored.header.row,
                stored.rid,
                self.table()
            ));
        }
        self.state
            .versions
            .remove(&(stored.header.row, stored.header.seq));
        Ok(())
    }

    /// Appends the record of `kind` that frees the slot of `stored`, and answers its LSN.
    ///
    /// `txn` is the transaction whose write is undone for a [`WalRecordKind::Compensation`],
    /// `TxnId(0)` for a [`WalRecordKind::Purge`]. The record is appended, not synced: the
    /// caller syncs once its walk is done.
    fn log_release(
        &self,
        kind: WalRecordKind,
        txn: TxnId,
        stored: &StoredVersion,
        release: Release,
    ) -> Result<Lsn, InternalError> {
        let payload = SlotRelease {
            table: self.table(),
            header: stored.header,
            at: stored.rid,
            release,
        }
        .encode();
        self.storage.wal.append(kind, txn, &payload)
    }

    /// The most recent version of `id`, read through the directory.
    fn current(&self, id: RowId) -> Result<StoredVersion, InternalError> {
        let Some(&rid) = self.state.directory.get(&id) else {
            return bug(format!("unknown row {id} in table {}", self.table()));
        };
        let Some(bytes) = self.heap.get(rid)? else {
            return corruption(format!(
                "row {id} of table {} points at {rid}, which holds no record",
                self.table()
            ));
        };
        Ok(StoredVersion {
            rid,
            header: VersionHeader::read_from(&bytes)?,
            bytes,
        })
    }

    /// The versions the heap holds, of the row `only` or of each row, sorted by increasing
    /// `RowId` then increasing `seq`: the chains, in order.
    ///
    /// The versions of one row are read through [`TableState::versions`]; the versions of each
    /// row by a walk of the heap.
    fn stored_versions(&self, only: Option<RowId>) -> Result<Vec<StoredVersion>, InternalError> {
        if let Some(id) = only {
            return self
                .state
                .versions
                .range((id, 0)..=(id, u64::MAX))
                .map(|(&(row, seq), &rid)| self.stored_at(rid, row, seq))
                .collect();
        }
        let mut versions = Vec::new();
        for rid in self.heap.scan()? {
            let Some(bytes) = self.heap.get(rid)? else {
                continue;
            };
            let header = VersionHeader::read_from(&bytes)?;
            if only.is_none_or(|row| row == header.row) {
                versions.push(StoredVersion { rid, header, bytes });
            }
        }
        versions.sort_by_key(|version| (version.header.row, version.header.seq));
        Ok(versions)
    }

    /// The version the directory of the versions places at `rid`, checked to be the version
    /// `seq` of `id`.
    fn stored_at(&self, rid: Rid, id: RowId, seq: u64) -> Result<StoredVersion, InternalError> {
        let Some(bytes) = self.heap.get(rid)? else {
            return corruption(format!(
                "version {seq} of row {id} of table {} points at {rid}, which holds no record",
                self.table()
            ));
        };
        let header = VersionHeader::read_from(&bytes)?;
        if header.row != id || header.seq != seq {
            return corruption(format!(
                "version {seq} of row {id} of table {} points at {rid}, which holds version {} \
                 of row {}",
                self.table(),
                header.seq,
                header.row
            ));
        }
        Ok(StoredVersion { rid, header, bytes })
    }

    /// Number of undo entries `txn` has written in this table.
    pub(crate) fn writes_len(&self, txn: TxnId) -> usize {
        match self.state.txns.get(&txn) {
            Some(state) => state.writes.len(),
            None => 0,
        }
    }

    /// Rolls back the writes of `txn` after position `mark`, which is what
    /// [`Storage::rollback_to`] calls on each table the transaction wrote in.
    ///
    /// The undo journals what it takes out as [`HeapTable::finish`] does, and `sync_all`s the
    /// journal once it is done.
    ///
    /// # Errors
    ///
    /// Those of [`HeapTable::undo`] and of the journal.
    pub(crate) fn rollback_to_savepoint(
        &mut self,
        txn: TxnId,
        mark: usize,
    ) -> Result<(), InternalError> {
        let Some(state) = self.state.txns.get_mut(&txn) else {
            return Ok(());
        };
        let tail: Vec<UndoEntry> = state.writes.split_off(mark);
        if tail.is_empty() {
            return Ok(());
        }
        for entry in tail.into_iter().rev() {
            self.undo(txn, entry)?;
        }
        self.storage.wal.flush()
    }

    /// Removes the versions a `vacuum` below `horizon` would discard and answers the index
    /// changes the caller passes on to the index trees.
    ///
    /// Passes 1 and 2 of [`crate::MemoryStorage::vacuum`] — versions deleted/replaced by a
    /// committed txn below the horizon, then versions created by an aborted txn — adapted to
    /// the on-disk layout. Callers drop the returned entries from the indexes of the table.
    ///
    /// Each version taken out is journalled first, by a [`WalRecordKind::Purge`] that the
    /// recovery replays; the journal is `sync_all`ed once the records are appended and
    /// before the first slot is freed, so the pages the vacuum changes carry LSNs the journal
    /// already covers and the pool may write them at once
    /// (`a_vacuum_journals_one_purge_per_version_and_syncs_before_the_pages_leave`).
    ///
    /// # Errors
    ///
    /// The errors of reading and writing the heap, and of the journal.
    pub(crate) fn vacuum(&mut self, horizon: TxnId) -> Result<Vec<IndexChange>, InternalError> {
        let status = |t| self.status(t);
        let mut doomed = Vec::new();
        for version in self.stored_versions(None)? {
            let discard = match version.header.xmax {
                Some(x) if x < horizon && status(x) == TxnStatus::Committed => true,
                None => false,
                _ => false,
            } || status(version.header.xmin) == TxnStatus::Aborted;
            if discard {
                doomed.push(version);
            }
        }
        let mut purges = Vec::with_capacity(doomed.len());
        for version in &doomed {
            purges.push(self.log_release(
                WalRecordKind::Purge,
                TxnId(0),
                version,
                Release::Removed,
            )?);
        }
        if !doomed.is_empty() {
            self.storage.wal.flush()?;
        }
        let mut removed = Vec::new();
        for (version, lsn) in doomed.into_iter().zip(purges) {
            self.heap.free_slot(version.rid, lsn)?;
            let row = version.row()?;
            removed.push(IndexChange::Removed {
                row: version.header.row,
                seq: version.header.seq,
                data: row,
            });
        }
        if !removed.is_empty() {
            // The directory names the most recent version of each row: a version an update
            // replaced is dead while its successor is the current one, and a delete is dead
            // and current at once. Rather than tell the two apart entry by entry, the
            // directory is rebuilt from what the heap holds now, as
            // [`HeapTable::resume_after_recovery`] rebuilds it after the redo.
            self.state.directory.clear();
            self.state.versions.clear();
            for version in self.stored_versions(None)? {
                let (row, seq) = (version.header.row, version.header.seq);
                self.state.versions.insert((row, seq), version.rid);
                self.state.directory.insert(row, version.rid);
            }
        }
        Ok(removed)
    }
}

impl super::index::VersionSource for HeapTable<'_> {
    fn table(&self) -> TableId {
        HeapTable::table(self)
    }

    fn shape(&self) -> &TableShape {
        HeapTable::shape(self)
    }

    fn versions(&self, only: Option<RowId>) -> Result<Vec<VersionView>, InternalError> {
        HeapTable::versions(self, only)
    }

    fn version(&self, row: RowId, seq: u64) -> Result<Option<VersionView>, InternalError> {
        HeapTable::version(self, row, seq)
    }

    fn status(&self, t: TxnId) -> TxnStatus {
        HeapTable::status(self, t)
    }
}

/// Refuses the [`TxnId`] reserved by [`NO_XMAX`].
fn check_txn(txn: TxnId) -> Result<(), InternalError> {
    if txn.0 == NO_XMAX {
        return bug(format!(
            "transaction {txn} is the reserved id the xmax field uses for 'no xmax'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
