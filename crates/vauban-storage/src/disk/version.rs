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
//! `seq` is what orders the chain: a heap record may move (see "Setting `xmax`" below), so
//! the address of a version says nothing about its age.
//!
//! # Directory
//!
//! `RowId -> Rid` of the **most recent** version of each row, held in memory: [`HeapTable::open`]
//! attaches to a heap with an empty directory and does not rebuild the rows it holds
//! ([`HeapTable::resume_after_recovery`] does, after a recovery). Older versions have
//! no directory entry: [`HeapTable::get`] and [`HeapTable::scan`] read the chains by walking
//! the heap ([`HeapTable::stored_versions`]), which is what "no index" costs here.
//!
//! # Setting `xmax`
//!
//! The heap ([`super::heap`]) writes and deletes records; it has no in-place rewrite. Marking a
//! version as replaced or deleted therefore deletes its record and inserts the patched bytes
//! ([`HeapTable::set_xmax`]), which may put the version at another [`Rid`]; the `RowId` is
//! untouched (`update_keeps_row_id_and_old_version`, `rid_of_a_row_may_move_on_update`).
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
//! [`super::wal_payload`]) **before** it changes the heap, and the page it changes is tied to
//! the LSN of that record by [`HeapTable::note_page`]. [`HeapTable::commit`] appends a
//! `Commit` and `sync_all`s the journal before it returns, so what a committed transaction
//! wrote is on disk (`commit_fsyncs_wal`); `data` is not synced, the pages leaving the pool
//! afterwards.
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

use std::collections::{BTreeSet, HashMap};

use vauban_errors::InternalError;

use super::DiskStorage;
use super::encode::{decode_row, encode_row};
use super::heap::{Heap, Rid};
use super::index::IndexChange;
use super::page::{Lsn, PageId};
use super::wal::WalRecordKind;
use super::wal_payload::RowChange;
use crate::{Row, RowId, Snapshot, TableId, TableShape, TxnId, TxnStatus};

/// Number of bytes a version carries before its columns: `row`, `xmin`, `xmax`, `seq`.
pub(crate) const VERSION_PREFIX_LEN: usize = 32;

/// Value written in the `xmax` field of a version no transaction has replaced or deleted.
///
/// `TxnId(u64::MAX)` is refused by the entry points of [`HeapTable`] so that the sentinel
/// stays free (`reserved_txn_id_is_a_bug`); the `txn` module hands out increasing ids from 1.
pub(crate) const NO_XMAX: u64 = u64::MAX;

/// Offset of the `xmax` field in the prefix of a version.
const OFF_XMAX: usize = 16;

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
/// `directory` is read by the heap store: [`super::clustered::ClusteredTable`] keeps its own
/// directory in a B+tree of the instance instead.
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
    /// The directory and the counters start empty and live in memory. What a table reopened
    /// this way answers, with one row committed before the `open`
    /// (`open_serves_the_rows_but_hands_out_row_id_one_again`): [`HeapTable::get`] and
    /// [`HeapTable::scan`] serve the row, because they walk the heap;
    /// [`HeapTable::latest_version`] answers `None` and [`HeapTable::update`] and
    /// [`HeapTable::delete`] answer [`InternalError::Bug`], because they go through the
    /// directory; and the next [`HeapTable::insert`] hands out `RowId(1)` a second time, so
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
    /// The directory is rebuilt from the heap rather than from the records of the redo: the
    /// versions of the losers are left out of it, and each row takes the address of its
    /// largest `seq`, [`HeapTable::stored_versions`] answering in that order. A row whose
    /// current version was left there by a loser therefore keeps a version the register hides
    /// but the undo of this build does not take away, and an [`HeapTable::update`] on that row
    /// answers [`InternalError::Bug`]; taking those bytes back belongs to a vacuum, which is
    /// not implemented.
    ///
    /// # Errors
    ///
    /// Those of [`Heap::get`] and of [`VersionHeader::read_from`] while the heap is walked.
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
        self.clear_losers_xmax(losers)?;
        self.state.directory.clear();
        for version in self.stored_versions(None)? {
            if self.status(version.header.xmin) == TxnStatus::Aborted {
                continue;
            }
            self.state.directory.insert(version.header.row, version.rid);
        }
        Ok(())
    }

    /// Takes back the `xmax` a losing transaction wrote on a page that reached `data`.
    ///
    /// A version whose `xmax` names a loser reads back as hidden by [`Snapshot::is_visible`]
    /// — the write did not commit — but [`HeapTable::check_current`] refuses an `update` on
    /// it as stale, because the field is set by the reader regardless of its writer's status.
    /// The recovery
    /// rewrites each such version with its `xmax` cleared, so the row is writable again as
    /// soon as the instance opens (`a_loser_xmax_on_data_is_cleared`).
    fn clear_losers_xmax(&mut self, losers: &[TxnId]) -> Result<(), InternalError> {
        let losers: std::collections::BTreeSet<TxnId> = losers.iter().copied().collect();
        for version in self.stored_versions(None)? {
            let Some(x) = version.header.xmax else {
                continue;
            };
            if !losers.contains(&x) {
                continue;
            }
            let Some(columns) = version.bytes.get(VERSION_PREFIX_LEN..) else {
                return Err(InternalError::Corruption(format!(
                    "version {} of row {} of table {} is shorter than its prefix",
                    version.header.seq,
                    version.header.row,
                    self.table()
                )));
            };
            let header = VersionHeader {
                xmax: None,
                ..version.header
            };
            let mut patched = Vec::with_capacity(version.bytes.len());
            header.write_to(&mut patched);
            patched.extend_from_slice(columns);
            self.heap.delete(version.rid)?;
            self.heap.insert(&patched)?;
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
        let lsn = self.log_row(WalRecordKind::Insert, txn, &header, Some(row))?;
        let rid = self.write_version(txn, lsn, &header, row)?;
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
        let lsn = self.log_row(WalRecordKind::Update, txn, &header, Some(row))?;
        self.set_xmax(txn, lsn, &current, Some(txn))?;
        let rid = self.write_version(txn, lsn, &header, row)?;
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
        let lsn = self.log_row(WalRecordKind::Delete, txn, &hidden, None)?;
        let rid = self.set_xmax(txn, lsn, &current, Some(txn))?;
        self.state.directory.insert(id, rid);
        self.log(txn, UndoEntry::Delete(id))
    }

    /// The content of `id` as `snap` sees it, `None` when no version of the row is visible.
    ///
    /// The chain of the row is walked and filtered by [`Snapshot::is_visible`]: at most one
    /// version answers, by the chain invariant of the crate root.
    ///
    /// # Errors
    ///
    /// The errors of [`Heap::get`] and [`super::encode::decode_row`].
    pub(crate) fn get(&self, snap: &Snapshot, id: RowId) -> Result<Option<Row>, InternalError> {
        let status = |t| self.status(t);
        for version in self.stored_versions(Some(id))? {
            if snap.is_visible(version.header.xmin, version.header.xmax, &status) {
                return version.row().map(Some);
            }
        }
        Ok(None)
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
        if status == TxnStatus::Aborted {
            for entry in writes.into_iter().rev() {
                self.undo(txn, entry)?;
            }
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
    /// the undo runs (`rollback_appends_abort`), although under no-steal the writes of the
    /// transaction have not reached `data`: that flush is there so that a transaction whose
    /// `Begin` is followed by neither a `Commit` nor an `Abort` is one a crash interrupted,
    /// which is what the recovery sorts the losers by. The undo itself is journalled
    /// by no record here, the versions it takes away being ones no committed transaction can
    /// see.
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
    /// This is how `super::index` reads what an entry points at: the visibility fields of a
    /// version and its columns, without the heap address and the encoding, which stay here.
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
    /// Called **before** the heap is changed: the record of a change reaches the file first,
    /// and the LSN it answers is what [`HeapTable::note_page`] writes in the header of each
    /// page the change touches. `row` is the content of the version for an `Insert` and an
    /// `Update`, `None` for a `Delete` ([`super::wal_payload`]).
    fn log_row(
        &mut self,
        kind: WalRecordKind,
        txn: TxnId,
        header: &VersionHeader,
        row: Option<&Row>,
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
        .encode();
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

    /// Writes one version in the heap and marks the page it landed on as written by `txn` at
    /// the LSN of the record that describes the write.
    fn write_version(
        &mut self,
        txn: TxnId,
        lsn: Lsn,
        header: &VersionHeader,
        row: &Row,
    ) -> Result<Rid, InternalError> {
        let bytes = encode_version(header, row);
        let rid = self.heap.insert(&bytes)?;
        self.note_page(txn, rid.page, lsn)?;
        Ok(rid)
    }

    /// Rewrites the version `stored` with another `xmax` and answers where it went.
    ///
    /// The heap has no in-place rewrite: the record is deleted and the patched bytes are
    /// inserted, so the [`Rid`] may change while the `RowId` in the prefix does not. The
    /// caller updates the directory when the rewritten version is the current one.
    fn set_xmax(
        &mut self,
        txn: TxnId,
        lsn: Lsn,
        stored: &StoredVersion,
        xmax: Option<TxnId>,
    ) -> Result<Rid, InternalError> {
        let patched = with_xmax(&stored.bytes, xmax)?;
        self.remove_version(txn, lsn, stored)?;
        let rid = self.heap.insert(&patched)?;
        self.note_page(txn, rid.page, lsn)?;
        Ok(rid)
    }

    /// Ties `page` to the journal record of LSN `lsn`, flags it as written by a transaction
    /// in progress and records it for `txn`.
    ///
    /// The page is pinned for the call: [`super::buffer::BufferPool::mark_dirty`] and
    /// [`super::buffer::BufferPool::set_in_progress`] ask for a page the pool holds. The LSN
    /// is what keeps the page in the cache until the journal is flushed: the pool refuses to
    /// write a page whose LSN is past the durable one (`flush_page_before_wal_flush_is_bug`).
    /// `Lsn(0)` leaves the LSN of the page where it is, which is what the undo of a rollback
    /// passes, having no record of its own.
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
    /// The heap writes of the undo carry `Lsn(0)`: the `Abort` record was appended and
    /// flushed before the undo ran, so the pages of the transaction sit at an LSN the journal
    /// already covers, and this walk adds no record to move them past it.
    fn undo(&mut self, txn: TxnId, entry: UndoEntry) -> Result<(), InternalError> {
        match entry {
            UndoEntry::Insert(row) => {
                let created = self.tail_created_by(txn, row)?;
                self.remove_version(txn, Lsn(0), &created)?;
                self.note_undone_version(&created)?;
                self.state.directory.remove(&row);
                Ok(())
            }
            UndoEntry::Update(row) => {
                let created = self.tail_created_by(txn, row)?;
                self.remove_version(txn, Lsn(0), &created)?;
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
                let rid = self.set_xmax(txn, Lsn(0), &replaced, None)?;
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
                let rid = self.set_xmax(txn, Lsn(0), &deleted, None)?;
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

    /// Removes one version from the heap, the page it sat on flagged for `txn` at `lsn`.
    fn remove_version(
        &mut self,
        txn: TxnId,
        lsn: Lsn,
        stored: &StoredVersion,
    ) -> Result<(), InternalError> {
        if !self.heap.delete(stored.rid)? {
            return corruption(format!(
                "version {} of row {} is not at {} in table {}",
                stored.header.seq,
                stored.header.row,
                stored.rid,
                self.table()
            ));
        }
        self.note_page(txn, stored.rid.page, lsn)
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
    /// `RowId` then increasing `seq`: the chains, in order, without index.
    fn stored_versions(&self, only: Option<RowId>) -> Result<Vec<StoredVersion>, InternalError> {
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
    /// # Errors
    ///
    /// Those of [`HeapTable::undo`].
    pub(crate) fn rollback_to_savepoint(
        &mut self,
        txn: TxnId,
        mark: usize,
    ) -> Result<(), InternalError> {
        let Some(state) = self.state.txns.get_mut(&txn) else {
            return Ok(());
        };
        let tail: Vec<UndoEntry> = state.writes.split_off(mark);
        for entry in tail.into_iter().rev() {
            self.undo(txn, entry)?;
        }
        Ok(())
    }

    /// Removes the versions a `vacuum` below `horizon` would discard and answers the index
    /// changes the caller passes on to the index trees.
    ///
    /// Passes 1 and 2 of [`crate::MemoryStorage::vacuum`] — versions deleted/replaced by a
    /// committed txn below the horizon, then versions created by an aborted txn — adapted to
    /// the on-disk layout. Callers drop the returned entries from the indexes of the table.
    ///
    /// # Errors
    ///
    /// The errors of reading and writing the heap.
    pub(crate) fn vacuum(&mut self, horizon: TxnId) -> Result<Vec<IndexChange>, InternalError> {
        let status = |t| self.status(t);
        let mut removed = Vec::new();
        for version in self.stored_versions(None)? {
            let discard = match version.header.xmax {
                Some(x) if x < horizon && status(x) == TxnStatus::Committed => true,
                None => false,
                _ => false,
            } || status(version.header.xmin) == TxnStatus::Aborted;
            if !discard {
                continue;
            }
            self.heap.delete(version.rid)?;
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
            for version in self.stored_versions(None)? {
                self.state.directory.insert(version.header.row, version.rid);
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
