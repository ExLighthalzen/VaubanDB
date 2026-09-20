//! Recovery at [`super::DiskStorage::open`]: the journal is read, the transactions that hold a
//! [`WalRecordKind::Commit`] are replayed on the heaps, and the others are left out.
//!
//! # What a crash leaves behind
//!
//! Under no-steal a page a running transaction wrote stays in the pool
//! ([`super::buffer::BufferPool::flush_all`] skips it), and `commit` syncs the journal rather
//! than `data` ([`super::version::HeapTable::commit`]). So a committed row may sit in the
//! journal while its page is out of `data`, and the recovery is what puts it back on the heap
//! (`reopen_after_commit_without_checkpoint`: one insert, one commit, a drop without a
//! checkpoint, and the row read back after the reopen).
//!
//! # Two passes
//!
//! 1. **Scan.** The records are read from the first one. A `Commit` makes its transaction a
//!    winner, an `Abort` a loser, and a transaction that holds neither is a loser too: its
//!    `Begin` was written and the process went away before the end of it. The row records
//!    give the largest [`RowId`] and the largest [`super::version::VersionHeader::seq`] the
//!    journal names, winners and losers together, so that an identifier a rolled-back insert
//!    took is not handed out again (`a_row_id_taken_by_a_loser_is_not_handed_out_again`). The
//!    DDL records are replayed on the catalogue, which gives the head page of each heap
//!    ([`redo_ddl`]).
//! 2. **Redo.** The row records of the winners are applied to the heaps, in journal order.
//!
//! # Where the redo starts, and why not at the checkpoint
//!
//! The second pass starts at the **first record of the journal**, not at
//! [`super::control::Control::latest_checkpoint_lsn`]. A transaction that began before a
//! checkpoint record and committed after it has its row records behind that LSN and its page
//! out of `data`: the flush of the checkpoint skipped the page, the transaction being in
//! progress then. One insert astride one checkpoint gives `Insert` at `Lsn(2)`, `Checkpoint`
//! at `Lsn(3)`, `Commit` at `Lsn(4)`, page out of `data`
//! (`a_transaction_astride_the_checkpoint_leaves_its_insert_before_redo_lsn` in
//! [`super`]). A row committed before a checkpoint is not assumed to be on `data` either: a
//! transaction still running on the page of a committed row holds that page back at the
//! checkpoint. The redo therefore repeats what a checkpoint already wrote, which it may do
//! because it is idempotent: a version the heap already holds under its `(RowId, seq)` is
//! skipped ([`Recovery::skipped`], `double_open_is_idempotent`).
//!
//! The journal is not truncated by this build, so its first record is the first record of the
//! instance. A starting point bounded by the oldest running transaction — the payload of the
//! checkpoint carrying it — is not implemented. Reading the whole journal also means holding
//! it in memory ([`super::wal::WalHandle::records`]); a streaming pass is not implemented
//! either.
//!
//! # Losers
//!
//! A loser is not replayed, and this build runs no undo: under no-steal the page of a running
//! transaction is held back, so what it wrote is usually left in the pool a crash threw away.
//! Usually, not in one case: two transactions that write the same page share it, and the
//! commit of the first clears the `in_progress` flag of that page
//! (`a_page_shared_by_two_txns_loses_its_flag_at_the_first_commit` in
//! [`super::version`]), after which a flush may write the version of the second one to `data`.
//! That leftover is hidden by the register rather than taken away:
//! [`super::version::HeapTable::resume_after_recovery`] marks the losers `Aborted`, which
//! [`crate::Snapshot::is_visible`] reads (`a_loser_whose_page_reached_data_is_invisible`).
//! Taking the bytes back belongs to a vacuum, which is not implemented.
//!
//! The `in_progress` flag itself is a field of a frame of the pool
//! ([`super::buffer::BufferPool::set_in_progress`] writes it on the frame of a pinned page),
//! not a bit of the page [`super::page::Page`] lays out, so it does not outlive the process
//! that set it and a page read back from `data` carries no trace of it. A page left in
//! progress is therefore unobservable here, and this module reports nothing of the sort.
//!
//! # The DDL is redone first
//!
//! The catalogue of the instance — which tables exist, in which database, of which shape and
//! rooted at which page — is read from the meta pages at the `open` and brought up to date
//! here, **before** the row records are replayed ([`redo_ddl`]): a row record names a
//! [`TableId`], and the table it names has to exist before its rows go anywhere. The DDL
//! records carry the identifiers and the shapes the statement used, so a reopen rebuilds the
//! very entries the instance had rather than new ones
//! (`super::tests::ddl_survives_reopen_without_checkpoint`), and replaying them over a
//! catalogue the meta pages already carry changes nothing ([`super::meta::Catalogue::apply_ddl`]).
//!
//! A table the journal drops leaves the catalogue, so the row records of a dropped table have
//! no heap to go to: they are counted in [`Recovery::unplaced`] and skipped, as are the row
//! records of a table no `CreateTable` record names
//! (`a_row_record_without_its_table_is_counted_unplaced`). That is what the trait asks of a
//! `drop_table`, and it is also what keeps the redo off pages the free list has handed to
//! someone else since.
//!
//! A test that works on a heap of its own installs it with [`install_single_heap`], which
//! appends the [`WalRecordKind::CreateTable`] record of a table of the shape it gives.

use std::collections::{BTreeMap, BTreeSet};

use vauban_errors::InternalError;

use super::DiskStorage;
use super::clustered::ClusteredTable;
use super::heap::{Heap, Rid};
use super::meta::TableEntry;
use super::page::{Lsn, PageId, PageKind};
use super::version::{VERSION_PREFIX_LEN, VersionHeader};
use super::wal::{WalRecord, WalRecordKind};
use super::wal_payload::RowChange;
use crate::{DbId, RowId, TableId, TableShape, TxnId};

/// What the recovery found and did, kept by [`super::DiskStorage`] for the caller that rebuilds
/// a table on top of the heaps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recovery {
    /// The transactions the journal holds a [`WalRecordKind::Commit`] for, sorted.
    pub(crate) winners: Vec<TxnId>,
    /// The transactions the journal holds an [`WalRecordKind::Abort`] for, and the ones whose
    /// `Begin` is followed by neither end, sorted.
    pub(crate) losers: Vec<TxnId>,
    /// The identifier the next insert takes: one past the largest the journal names, and at
    /// least the counter of `vauban.ctl`.
    pub(crate) next_row_id: u64,
    /// The serial the next version takes, one past the largest the journal names.
    pub(crate) next_seq: u64,
    /// Head page of the heap of each table the catalogue holds once the DDL of the journal
    /// has been replayed, by table ([`redo_ddl`]). A table the journal drops is not there.
    pub(crate) tables: BTreeMap<TableId, PageId>,
    /// Row records of winners the redo wrote to a heap.
    pub(crate) applied: usize,
    /// Row records of winners the heap already held, `(RowId, seq)` for `(RowId, seq)`.
    pub(crate) skipped: usize,
    /// Row records of winners naming a table no [`WalRecordKind::CreateTable`] record names.
    pub(crate) unplaced: usize,
    /// Row records of a committed transaction that an undone savepoint took back before the
    /// commit ([`Scan::cancelled`]). The redo leaves them out.
    pub(crate) cancelled: BTreeSet<Lsn>,
}

impl Default for Recovery {
    /// What an empty journal answers: no transaction, the two counters at 1.
    fn default() -> Self {
        Self {
            winners: Vec::new(),
            losers: Vec::new(),
            next_row_id: 1,
            next_seq: 1,
            tables: BTreeMap::new(),
            applied: 0,
            skipped: 0,
            unplaced: 0,
            cancelled: BTreeSet::new(),
        }
    }
}

impl Recovery {
    /// Head page of the heap of `table`, as a [`WalRecordKind::CreateTable`] record named it.
    pub(crate) fn heap_of(&self, table: TableId) -> Option<PageId> {
        self.tables.get(&table).copied()
    }
}

/// Replays the journal of `storage` on its heaps and answers what it found.
///
/// The two passes of the module documentation, then the counter of `vauban.ctl`:
/// `next_row_id` takes the larger of what the block held and one past the largest [`RowId`] the
/// journal names, and the block is rewritten when that moves it. The control lock is released
/// before the redo, which allocates pages and therefore takes that lock itself
/// ([`super::alloc::allocate`]).
///
/// # Errors
///
/// [`InternalError::Corruption`] for a journal whose records do not read back, for a payload
/// shorter than its fixed part, and for a delete record naming a version no heap holds; the
/// errors of the buffer pool, of the heap and of the allocator otherwise.
pub(crate) fn recover(storage: &DiskStorage) -> Result<Recovery, InternalError> {
    let records = storage.wal.records()?;
    let scan = Scan::of(&records)?;
    let tables = redo_ddl(storage, &records)?;
    let mut recovery = scan.into_recovery(storage, tables)?;
    redo(storage, &records, &mut recovery)?;
    Ok(recovery)
}

/// Replays the DDL records on the catalogue of the instance and answers the head page of the
/// heap of each table it holds afterwards.
///
/// The counters of `vauban.ctl` then take the largest identifier of each kind the catalogue
/// holds, plus one, so an identifier the journal names is not handed out a second time. They
/// are already there in the ordinary case — a DDL statement rewrites `vauban.ctl` before it
/// appends its record — and this is what puts them back when that file was lost.
///
/// The catalogue is locked first and the control block under it, the order
/// [`super::DiskStorage::lock_catalogue`] documents.
///
/// # Errors
///
/// [`InternalError::Corruption`] for a DDL payload this build does not read, and the errors of
/// [`super::control::Control::write`].
fn redo_ddl(
    storage: &DiskStorage,
    records: &[WalRecord],
) -> Result<BTreeMap<TableId, PageId>, InternalError> {
    let mut catalogue = storage.lock_catalogue()?;
    for record in records {
        catalogue.apply_ddl(record)?;
    }
    let (db, table, index) = catalogue.largest_ids();
    let mut control = storage.lock_control()?;
    let mut updated = *control;
    updated.next_db_id = updated.next_db_id.max(db + 1);
    updated.next_table_id = updated.next_table_id.max(table + 1);
    updated.next_index_id = updated.next_index_id.max(index + 1);
    if updated != *control {
        updated.write(&storage.control_path())?;
        *control = updated;
    }
    Ok(catalogue.heap_heads())
}

/// Appends the [`WalRecordKind::CreateTable`] record that tells the recovery where the heap of
/// `table` starts, and syncs the journal.
///
/// A test that builds a heap by hand — [`super::alloc::allocate`] then
/// [`super::version::HeapTable::create`] — calls this once, before the writes, instead of
/// going through [`super::DiskStorage::create_table`]. The record names database
/// [`PLACEHOLDER_DB`], which the `CreateDatabase` records of such a test do not create: the
/// redo applies a table whose database the catalogue does not hold, the DDL of an unknown one
/// having been refused when it happened.
///
/// # Errors
///
/// Those of [`super::wal::WalHandle::append_durable`].
// Called by the tests of this module; the engine goes through `DiskStorage::create_table`.
#[allow(dead_code)]
pub(crate) fn install_single_heap(
    storage: &DiskStorage,
    table: TableId,
    first_page: PageId,
    shape: TableShape,
) -> Result<(), InternalError> {
    let entry = TableEntry {
        table,
        db: PLACEHOLDER_DB,
        shape,
        first_page,
        tree_root: None,
        directory_root: None,
        next_row_id: 1,
    };
    storage
        .wal
        .append_durable(WalRecordKind::CreateTable, TxnId(0), &entry.encode())?;
    Ok(())
}

/// The database [`install_single_heap`] names, which the catalogue of such an instance does
/// not hold.
pub(crate) const PLACEHOLDER_DB: DbId = DbId(0);

/// The [`crate::SavepointId`] the payload of a [`WalRecordKind::Savepoint`] or a
/// [`WalRecordKind::RollbackTo`] carries.
fn savepoint_id(record: &WalRecord) -> Result<u64, InternalError> {
    let bytes: [u8; 8] = record.payload[..8].try_into().map_err(|_| {
        InternalError::Corruption(format!(
            "the {} record at lsn {} carries {} payload bytes, fewer than the 8 of a savepoint \
             id",
            record.kind.as_byte(),
            record.lsn,
            record.payload.len()
        ))
    })?;
    Ok(u64::from_le_bytes(bytes))
}

/// What the first pass read: the outcome of each transaction and the largest identifiers.
#[derive(Debug, Default)]
struct Scan {
    /// Transactions with a `Commit`.
    winners: BTreeSet<TxnId>,
    /// Transactions with an `Abort`.
    aborted: BTreeSet<TxnId>,
    /// Transactions the journal names in a `Begin` or a row record.
    seen: BTreeSet<TxnId>,
    /// Largest [`RowId`] a row record names, winners and losers together.
    max_row_id: u64,
    /// Largest version serial a row record names.
    max_seq: u64,
    /// Row records the redo must leave out. A [`WalRecordKind::RollbackTo`] record cancels the
    /// row records of its transaction between the [`WalRecordKind::Savepoint`] its payload
    /// names and itself: the undo of a rollback to a savepoint writes no record of
    /// its own; a redo that replayed the writes the savepoint protected would put back
    /// what the running transaction had taken away.
    cancelled: BTreeSet<Lsn>,
}

impl Scan {
    /// Reads the records once, from the first one.
    fn of(records: &[WalRecord]) -> Result<Self, InternalError> {
        let mut scan = Self::default();
        // The savepoints the scan has seen and not yet rolled back: the id the payload
        // names and the LSN of the record that wrote it, per transaction.
        let mut open: BTreeMap<(TxnId, u64), Lsn> = BTreeMap::new();
        // The LSN of the last record seen, so the range of records a rollback takes back can
        // be looked up in the journal without a second pass.
        for record in records {
            match record.kind {
                WalRecordKind::Commit => {
                    scan.seen.insert(record.txn);
                    scan.winners.insert(record.txn);
                }
                WalRecordKind::Abort => {
                    scan.seen.insert(record.txn);
                    scan.aborted.insert(record.txn);
                }
                WalRecordKind::Begin => {
                    scan.seen.insert(record.txn);
                }
                WalRecordKind::Savepoint => {
                    let id = savepoint_id(record)?;
                    open.insert((record.txn, id), record.lsn);
                }
                WalRecordKind::RollbackTo => {
                    let id = savepoint_id(record)?;
                    let Some(at) = open.remove(&(record.txn, id)) else {
                        continue;
                    };
                    for r in records {
                        if r.txn == record.txn && r.lsn > at && r.lsn < record.lsn {
                            scan.cancelled.insert(r.lsn);
                        }
                    }
                }
                WalRecordKind::Insert | WalRecordKind::Update | WalRecordKind::Delete => {
                    scan.seen.insert(record.txn);
                    let change = RowChange::decode(&record.payload)?;
                    scan.max_row_id = scan.max_row_id.max(change.header.row.0);
                    scan.max_seq = scan.max_seq.max(change.header.seq);
                }
                _ => {}
            }
        }
        Ok(scan)
    }

    /// The outcome of the scan, the counter of `vauban.ctl` moved forward when the journal
    /// names a larger [`RowId`] than the block held.
    ///
    /// `tables` is what [`redo_ddl`] left in the catalogue, not what the scan saw: a table the
    /// journal creates and then drops is not there.
    fn into_recovery(
        self,
        storage: &DiskStorage,
        tables: BTreeMap<TableId, PageId>,
    ) -> Result<Recovery, InternalError> {
        let losers: Vec<TxnId> = self
            .seen
            .iter()
            .copied()
            .filter(|txn| !self.winners.contains(txn))
            .collect();
        let next_row_id = {
            let mut control = storage.lock_control()?;
            let wanted = self.max_row_id.saturating_add(1).max(control.next_row_id);
            if wanted != control.next_row_id {
                let mut updated = *control;
                updated.next_row_id = wanted;
                updated.write(&storage.control_path())?;
                *control = updated;
            }
            wanted
        };
        Ok(Recovery {
            winners: self.winners.into_iter().collect(),
            losers,
            next_row_id,
            next_seq: self.max_seq.saturating_add(1),
            tables,
            applied: 0,
            skipped: 0,
            unplaced: 0,
            cancelled: self.cancelled,
        })
    }
}

/// Second pass: the row records of the winners, in journal order, applied to the heaps.
///
/// The heap of each table the journal names is attached first, whether a record of a winner
/// reaches it or not: a head page that stayed in the pool of the instance that crashed is
/// formatted there, so that the table reads back as the empty heap it was
/// (`uncommitted_insert_gone_after_reopen`).
///
/// A clustered table is left out of this pass for the row redo: the pages of its tree reach
/// `data` at a checkpoint. The recovery rebuilds the trees at the `open` by walking the
/// versions the DDL phase installed on the catalogue: each winner record is inserted into the
/// tree of its clustered table in a separate pass.
fn redo(
    storage: &DiskStorage,
    records: &[WalRecord],
    recovery: &mut Recovery,
) -> Result<(), InternalError> {
    let winners: BTreeSet<TxnId> = recovery.winners.iter().copied().collect();
    let mut heaps: BTreeMap<TableId, RedoHeap<'_>> = BTreeMap::new();
    let mut clustered_tables: BTreeSet<TableId> = BTreeSet::new();
    {
        let catalogue = storage.lock_catalogue()?;
        for (&table, &first_page) in &recovery.tables {
            let clustered = catalogue
                .table(table)
                .is_some_and(|entry| entry.shape.clustered_key.is_some());
            if clustered {
                clustered_tables.insert(table);
                continue;
            }
            heaps.insert(table, RedoHeap::over(storage, table, first_page)?);
        }
    }
    let mut clustered_changes: BTreeMap<TableId, Vec<RowChange>> = BTreeMap::new();
    for record in records {
        let kind = record.kind;
        if !matches!(
            kind,
            WalRecordKind::Insert | WalRecordKind::Update | WalRecordKind::Delete
        ) || !winners.contains(&record.txn)
            || recovery.cancelled.contains(&record.lsn)
        {
            continue;
        }
        let change = RowChange::decode(&record.payload)?;
        if clustered_tables.contains(&change.table) {
            clustered_changes
                .entry(change.table)
                .or_default()
                .push(change);
            continue;
        }
        let Some(redo) = heaps.get_mut(&change.table) else {
            recovery.unplaced += 1;
            continue;
        };
        let written = match kind {
            WalRecordKind::Insert => redo.insert(&change)?,
            WalRecordKind::Update => {
                redo.hide_previous(change.header.row, change.header.seq, record.txn)?;
                redo.insert(&change)?
            }
            // The header of a delete record carries the version it hides, its `xmax` set.
            _ => redo.hide(change.header.row, change.header.seq, record.txn)?,
        };
        if written {
            recovery.applied += 1;
        } else {
            recovery.skipped += 1;
        }
    }
    // Apply clustered records: each winner version is inserted into the tree. The roots may
    // hold free pages when the pages a split or an insert left dirty stayed in the pool of the
    // instance that crashed: [`ClusteredTable::redo_open`] formats them before the replay, as
    // [`RedoHeap::over`] does for the head page of a heap.
    for (&table, changes) in &clustered_changes {
        let Some(&first_page) = recovery.tables.get(&table) else {
            continue;
        };
        let entry = storage.lock_catalogue()?.table(table).cloned();
        let Some(entry) = entry else {
            continue;
        };
        let (Some(tree), Some(directory)) = (entry.tree_root, entry.directory_root) else {
            continue;
        };
        let mut clustered = ClusteredTable::redo_open(
            storage,
            table,
            first_page,
            (tree, directory),
            entry.shape.clone(),
        )?;
        for change in changes {
            if clustered.replay_one(change)? {
                recovery.applied += 1;
            } else {
                recovery.skipped += 1;
            }
        }
    }
    Ok(())
}

/// Where one version sits and what its prefix says, as the redo tracks it.
#[derive(Debug, Clone, Copy)]
struct Placed {
    /// Address of the version in the heap.
    rid: Rid,
    /// Its prefix, `xmax` included.
    header: VersionHeader,
}

/// The redo of one heap table: its heap and the versions the heap holds, by `(RowId, seq)`.
///
/// The directory is built by walking the heap before the first record is applied and kept up to
/// date by the redo itself, so that the heap is walked once per table rather than once per
/// record. It is thrown away at the end of
/// the recovery; the directory [`super::version::HeapTable`] works from is rebuilt by
/// [`super::version::HeapTable::resume_after_recovery`], which walks the heap after the redo.
#[derive(Debug)]
struct RedoHeap<'storage> {
    /// The heap the versions go to.
    heap: Heap<'storage>,
    /// The versions the heap holds, by row then serial.
    versions: BTreeMap<(RowId, u64), Placed>,
}

impl<'storage> RedoHeap<'storage> {
    /// Attaches to the heap of `table`, formatting its head page when that page is not one.
    ///
    /// A head page whose formatting stayed in the pool of the instance that crashed reads back
    /// as the [`PageKind::Free`] page [`super::alloc::allocate`] wrote when it grew the file,
    /// and the redo formats it as the empty heap the writes of the journal are replayed on
    /// (`reopen_after_commit_without_checkpoint`).
    fn over(
        storage: &'storage DiskStorage,
        table: TableId,
        first_page: PageId,
    ) -> Result<Self, InternalError> {
        let pin = storage.pool.pin(first_page)?;
        let kind = pin.with_page(|page| page.kind())?;
        drop(pin);
        let heap = if kind? == PageKind::Heap {
            Heap::open(storage, table, first_page)
        } else {
            Heap::create(storage, table, first_page)?
        };
        let mut versions = BTreeMap::new();
        for rid in heap.scan()? {
            let Some(bytes) = heap.get(rid)? else {
                continue;
            };
            let header = VersionHeader::read_from(&bytes)?;
            versions.insert((header.row, header.seq), Placed { rid, header });
        }
        Ok(Self { heap, versions })
    }

    /// Writes the version of `change` when the heap does not hold it yet, and answers whether
    /// it wrote.
    fn insert(&mut self, change: &RowChange) -> Result<bool, InternalError> {
        let key = (change.header.row, change.header.seq);
        if self.versions.contains_key(&key) {
            return Ok(false);
        }
        let mut bytes = Vec::with_capacity(VERSION_PREFIX_LEN + change.columns.len());
        change.header.write_to(&mut bytes);
        bytes.extend_from_slice(&change.columns);
        let rid = self.heap.insert(&bytes)?;
        self.versions.insert(
            key,
            Placed {
                rid,
                header: change.header,
            },
        );
        Ok(true)
    }

    /// Sets `xmax` on the version `(row, seq)`, and answers whether it wrote.
    ///
    /// A version that already carries an `xmax` is left as it stands: the write it records had
    /// reached `data` before the crash. The heap has no rewrite in place, so the version is
    /// deleted and the patched bytes inserted, as [`super::version::HeapTable`] does; its
    /// [`Rid`] may change, its `(RowId, seq)` does not.
    fn hide(&mut self, row: RowId, seq: u64, txn: TxnId) -> Result<bool, InternalError> {
        let Some(placed) = self.versions.get(&(row, seq)).copied() else {
            return Err(InternalError::Corruption(format!(
                "the journal hides version {seq} of row {row} of table {}, which the heap does \
                 not hold",
                self.heap.table()
            )));
        };
        self.rewrite(placed, txn)
    }

    /// Sets `xmax` on the version of `row` that comes before `seq`: the one an update replaces.
    ///
    /// The record of an update names the version it creates, not the one it replaces, which is
    /// the current one of the chain when the record is replayed ([`super::wal_payload`]): here,
    /// the largest serial of `row` below `seq`. A row whose earlier version is out of the heap
    /// — its insert belonging to a transaction that is no winner — leaves nothing to hide.
    fn hide_previous(&mut self, row: RowId, seq: u64, txn: TxnId) -> Result<bool, InternalError> {
        let previous = self
            .versions
            .range((row, 0)..(row, seq))
            .next_back()
            .map(|(_, placed)| *placed);
        match previous {
            Some(placed) => self.rewrite(placed, txn),
            None => Ok(false),
        }
    }

    /// Rewrites `placed` with `xmax` set to `txn`, when it does not carry one already.
    fn rewrite(&mut self, placed: Placed, txn: TxnId) -> Result<bool, InternalError> {
        if placed.header.xmax.is_some() {
            return Ok(false);
        }
        let Some(bytes) = self.heap.get(placed.rid)? else {
            return Err(InternalError::Corruption(format!(
                "version {} of row {} of table {} is not at {} any more",
                placed.header.seq,
                placed.header.row,
                self.heap.table(),
                placed.rid
            )));
        };
        let Some(columns) = bytes.get(VERSION_PREFIX_LEN..) else {
            return Err(InternalError::Corruption(format!(
                "version {} of row {} of table {} is {} bytes, shorter than the \
                 {VERSION_PREFIX_LEN}-byte version prefix",
                placed.header.seq,
                placed.header.row,
                self.heap.table(),
                bytes.len()
            )));
        };
        let header = VersionHeader {
            xmax: Some(txn),
            ..placed.header
        };
        let mut patched = Vec::with_capacity(bytes.len());
        header.write_to(&mut patched);
        patched.extend_from_slice(columns);
        self.heap.delete(placed.rid)?;
        let rid = self.heap.insert(&patched)?;
        self.versions
            .insert((header.row, header.seq), Placed { rid, header });
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
