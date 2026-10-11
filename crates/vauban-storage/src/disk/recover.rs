//! Recovery at [`super::DiskStorage::open`]: the journal is read, the pages it describes are
//! brought back to the last state it records, and what the transactions without a `Commit`
//! left behind is then taken out.
//!
//! # What a crash leaves behind
//!
//! Under no-steal a page a running transaction wrote stays in the pool
//! ([`super::buffer::BufferPool::flush_all`] skips it), and `commit` syncs the journal rather
//! than `data` ([`super::version::HeapTable::commit`]). So a committed row may sit in the
//! journal while its page is out of `data`, and the recovery is what puts it back on its page
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
//!    DDL records are replayed on the catalogue, which gives the head page of each heap, and
//!    the moves of the roots of the clustered trees the page records carry are applied after
//!    them ([`redo_ddl`]).
//! 2. **Redo.** Each record that describes a change of a page is replayed in journal order on
//!    the page it names, when the `page_lsn` of that page is below its number, **for a winner
//!    and a loser alike**: the page records ([`WalRecordKind::PageWrite`],
//!    [`super::wal::PageWrite::redo`]), the row records of a heap table
//!    ([`RedoHeap::apply`]) and the records that free a heap slot
//!    ([`WalRecordKind::Compensation`], [`WalRecordKind::Purge`], [`RedoHeap::release`]).
//!    The pages then hold what the instance held when the last record reached the journal,
//!    the writes of the losers included; those are taken out afterwards (section "Losers",
//!    `tests::uncommitted_insert_gone_after_reopen`).
//!
//! # Page numbers
//!
//! A record that changes a page sets the `page_lsn` of that page to its number in the change
//! itself ([`super::heap::Heap::put`], [`super::wal::PageSet::commit`]), and the pool does not
//! write a page whose number the journal has not made durable. A page on `data` therefore holds
//! the changes of the records up to its `page_lsn` and none after: the redo leaves it as it
//! stands for the records at or below that number and replays the ones past it, in order. A
//! page on `data` newer than the records is not changed by them: a version a vacuum took away
//! after its insert reached `data` is not put back by the insert record
//! (`tests::a_page_on_data_before_the_crash_is_left_as_it_stands_by_the_redo`), and a second
//! redo over the pages of the first gives the same pages
//! (`tests::a_redo_played_twice_gives_the_same_pages`).
//!
//! The changes of each page being replayed from the state the page had before them, each
//! finds the page as the instance had it: a `Put` finds the room its write found, a `SetXmax`
//! and a release find the version they name at their slot. One that does not is a page its
//! journal does not describe, and the redo refuses it ([`InternalError::Corruption`],
//! `tests::a_put_without_room_at_its_slot_is_corruption`).
//!
//! A heap grows by a page through one page record that formats the fresh page and links it
//! from the last one, and an overflow chain and the stub that names it are page records too
//! ([`super::heap`], "Journal"): the redo of the row record of a long version leaves the page
//! of its stub to them, and so does the `SetXmax` of a long version, whose `xmax` sits in the
//! first page of its chain and is replayed by the `page_lsn` of that page
//! (`tests::a_delete_of_a_long_row_whose_stub_page_alone_reached_data_is_redone`).
//!
//! # Records that free a slot
//!
//! The undo of a rollback, of a rollback to a savepoint, and a vacuum journal each version they
//! take out of a heap slot, or whose `xmax` they unset ([`super::wal_payload::SlotRelease`]).
//! The redo replays them at their place in the journal, on the slot they name, when the
//! `page_lsn` of its page is behind them, as it does the row records: the version the slot holds
//! is taken out, or has its `xmax` unset, and the page takes the number of the record
//! ([`Recovery::released`] counts them). A version a rollback to a savepoint took back is thus
//! put back by its record and taken out again by the compensation of that rollback, in the
//! order the instance did it (`a_compensation_inside_an_outer_rolled_back_range_is_replayed`).
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
//! [`super`]). The redo therefore reads records a checkpoint already wrote, and skips them
//! by the number of their page ([`Recovery::skipped`], `double_open_is_idempotent`).
//!
//! The journal is not truncated by this build, so its first record is the first record of the
//! instance. A starting point bounded by the oldest running transaction — the payload of the
//! checkpoint carrying it — is not implemented. Reading the whole journal also means holding
//! it in memory ([`super::wal::WalHandle::records`]); a streaming pass is not implemented
//! either.
//!
//! # Losers
//!
//! The redo replays the writes of a loser with the others, so a loser is taken back after it,
//! each change journalled by a [`WalRecordKind::Compensation`] it appends before the change.
//!
//! On a heap, the writes of a loser are taken back when the table is first reached:
//! [`super::version::HeapTable::resume_after_recovery`] marks the losers `Aborted`, which
//! [`crate::Snapshot::is_visible`] reads (`a_loser_whose_page_reached_data_is_invisible`), then
//! takes out each version a loser created and unsets each `xmax` a loser set. A later write
//! of the row, and its rollback, find the chain the winners left
//! (`a_row_a_loser_updated_then_deleted_takes_an_update_and_its_rollback`), and the next
//! recovery replays those compensations as it replays the ones of a rollback
//! (`a_loser_taken_back_by_a_recovery_cut_short_is_taken_back_by_the_next`).
//!
//! A clustered table is taken back here rather than at its first call, once the redo is done:
//! [`super::clustered::ClusteredTable::take_back_losers`] takes out of the tree each version a
//! loser created and unsets each `xmax` a loser set, and makes the directory agree with the
//! tree (`a_clustered_row_a_loser_wrote_takes_writes_and_rollbacks_after_recovery`). A change
//! of a loser that a compensation already in the journal describes is made under that record,
//! so a recovery cut short adds no record the next time
//! (`a_clustered_take_back_lost_with_the_pool_is_made_again_under_its_records`).
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
//! rooted at which page — is read from the meta pages at the `open` ([`super::meta::load`],
//! which replays the page records of those pages first) and brought up to date here, **before**
//! the row records are replayed ([`redo_ddl`]): a row record names a [`TableId`], and the table
//! it names has to exist before its rows go anywhere. The DDL records carry the identifiers and
//! the shapes the statement used, so a reopen rebuilds the very entries the instance had
//! rather than new ones (`super::tests::ddl_survives_reopen_without_checkpoint`). Replaying them
//! over a catalogue the meta pages already carry keeps the roots and the row counter those
//! pages hold ([`keep_the_roots_the_meta_pages_held`]), and the root a split moved since is the
//! one the page record of that split names (`super::wal::RootMove`).
//!
//! A table the journal drops leaves the catalogue, so the row records of a dropped table have
//! no heap to go to: they are counted in [`Recovery::unplaced`] and skipped, as are the row
//! records of a table no `CreateTable` record names
//! (`a_row_record_without_its_table_is_counted_unplaced`). That is what the trait asks of a
//! `drop_table`, and it is also what keeps the redo off pages the free list has handed to
//! someone else since; the page records of those pages are replayed by their `page_lsn`, which
//! the release of the page by the allocator moved past them ([`super::alloc::free`]).
//!
//! A test that works on a heap of its own installs it with [`install_single_heap`], which
//! appends the [`WalRecordKind::CreateTable`] record of a table of the shape it gives.

use std::collections::{BTreeMap, BTreeSet};

use vauban_errors::InternalError;

use super::DiskStorage;
use super::clustered::{ClusteredTable, TakenBack};
use super::heap::{Heap, MAX_INLINE_LEN, Rid, Slotted};
use super::meta::{Catalogue, IndexEntry, TableEntry};
use super::page::{Lsn, PageId};
use super::version::{NO_XMAX, OFF_XMAX, VERSION_PREFIX_LEN, VersionHeader};
use super::wal::{PageWrite, TreeRole, WalRecord, WalRecordKind};
use super::wal_payload::{PageOp, Release, RowChange, SlotRelease};
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
    /// The largest transaction identifier a record of the journal carries, winner, loser or
    /// neither (a record of the instance carries 0). [`super::DiskStorage::next_txn_id`]
    /// hands out identifiers past it, so a later `Commit` cannot turn a loser into a winner
    /// (`tests::the_identifier_of_a_loser_is_not_handed_out_again`).
    pub(crate) max_txn: u64,
    /// The identifier the next insert takes: one past the largest the journal names, and at
    /// least the counter of `vauban.ctl`.
    pub(crate) next_row_id: u64,
    /// The serial the next version takes, one past the largest the journal names.
    pub(crate) next_seq: u64,
    /// Head page of the heap of each table the catalogue holds once the DDL of the journal
    /// has been replayed, by table ([`redo_ddl`]). A table the journal drops is not there.
    pub(crate) tables: BTreeMap<TableId, PageId>,
    /// Row records of a heap table whose operations the redo applied to a page behind them.
    pub(crate) applied: usize,
    /// Row records of a heap table whose pages already carried their number.
    pub(crate) skipped: usize,
    /// Row records naming a table no [`WalRecordKind::CreateTable`] record names, or one the
    /// journal dropped.
    pub(crate) unplaced: usize,
    /// [`WalRecordKind::Compensation`] and [`WalRecordKind::Purge`] records that changed a
    /// heap: the page of their slot was behind them.
    pub(crate) released: usize,
    /// Pages the page records rewrote, a page counted once per record.
    pub(crate) rewritten: usize,
}

impl Default for Recovery {
    /// What an empty journal answers: no transaction, the two counters at 1.
    fn default() -> Self {
        Self {
            winners: Vec::new(),
            losers: Vec::new(),
            max_txn: 0,
            next_row_id: 1,
            next_seq: 1,
            tables: BTreeMap::new(),
            applied: 0,
            skipped: 0,
            unplaced: 0,
            released: 0,
            rewritten: 0,
        }
    }
}

impl Recovery {
    /// Head page of the heap of `table`, as a [`WalRecordKind::CreateTable`] record named it.
    pub(crate) fn heap_of(&self, table: TableId) -> Option<PageId> {
        self.tables.get(&table).copied()
    }
}

/// Replays the journal of `storage` on its pages and answers what it found.
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
/// shorter than its fixed part, and for a record a page behind it does not take; the errors of
/// the buffer pool, of the heap and of the allocator otherwise.
pub(crate) fn recover(storage: &DiskStorage) -> Result<Recovery, InternalError> {
    let records = storage.wal.records()?;
    let scan = Scan::of(&records)?;
    let tables = redo_ddl(storage, &records)?;
    let mut recovery = scan.into_recovery(storage, tables)?;
    redo(storage, &records, &mut recovery)?;
    Ok(recovery)
}

/// Replays the DDL records on the catalogue of the instance, then the moves of roots the page
/// records carry, and answers the head page of the heap of each table it holds afterwards.
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
    let loaded_tables: Vec<TableEntry> = catalogue
        .heap_heads()
        .keys()
        .filter_map(|table| catalogue.table(*table).cloned())
        .collect();
    let loaded_indexes: Vec<IndexEntry> = catalogue.all_indexes().into_iter().cloned().collect();
    for record in records {
        catalogue.apply_ddl(record)?;
    }
    keep_the_roots_the_meta_pages_held(&mut catalogue, loaded_tables, loaded_indexes);
    apply_root_moves(&mut catalogue, records)?;
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

/// Puts back, over what the DDL records of the journal wrote, the roots and the row counter
/// the meta pages held for a table or an index that the journal leaves in place.
///
/// A `CreateTable` or `CreateIndex` record carries the entry as the statement created it: the
/// roots it allocated then and a row counter at 1. The meta pages carry the entry as the last
/// commit before the reopen left it, with the root a split moved
/// (`a_clustered_root_moved_by_a_split_survives_a_checkpointed_reopen`). Replaying the record
/// over it would hand the tree back its first root, which a split turned into one page among
/// others. The entry of the meta pages is kept when the table still starts at the same
/// `first_page`, which is what tells the same table from another one of the same identifier,
/// and when the index is still on the same table.
fn keep_the_roots_the_meta_pages_held(
    catalogue: &mut Catalogue,
    tables: Vec<TableEntry>,
    indexes: Vec<IndexEntry>,
) {
    for loaded in tables {
        let Some(now) = catalogue.table(loaded.table) else {
            continue;
        };
        if now.first_page != loaded.first_page || *now == loaded {
            continue;
        }
        let kept = TableEntry {
            tree_root: loaded.tree_root,
            directory_root: loaded.directory_root,
            next_row_id: now.next_row_id.max(loaded.next_row_id),
            ..now.clone()
        };
        catalogue.add_table(kept);
    }
    for loaded in indexes {
        let Some(now) = catalogue.index(loaded.index) else {
            continue;
        };
        if now.table != loaded.table || *now == loaded {
            continue;
        }
        let kept = IndexEntry {
            root: loaded.root,
            ..now.clone()
        };
        catalogue.add_index(kept);
    }
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

/// Gives each clustered table of `catalogue` the roots the page records of the journal moved
/// its trees to, in journal order, the last move of a tree being the one it keeps.
///
/// A split that reaches the root of a tree writes a new root in the record that carries the
/// split ([`super::wal::RootMove`]); the meta pages take that root at the next commit only, so
/// a crash in between leaves them naming the old one, a page the split turned into the left
/// child (`tests::a_root_moved_by_a_split_is_found_again_without_the_meta_pages`). A move of a
/// table the catalogue no longer holds is left out.
///
/// # Errors
///
/// [`InternalError::Corruption`] for a page record this build does not read.
fn apply_root_moves(catalogue: &mut Catalogue, records: &[WalRecord]) -> Result<(), InternalError> {
    for record in records {
        if record.kind != WalRecordKind::PageWrite {
            continue;
        }
        let Some(moved) = PageWrite::decode(&record.payload)?.root else {
            continue;
        };
        let Some(entry) = catalogue.table(moved.table).cloned() else {
            continue;
        };
        let entry = match moved.role {
            TreeRole::Versions => TableEntry {
                tree_root: Some(moved.root),
                ..entry
            },
            TreeRole::Directory => TableEntry {
                directory_root: Some(moved.root),
                ..entry
            },
        };
        catalogue.add_table(entry);
    }
    Ok(())
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
    /// Largest transaction identifier a record carries, row, end or instance record.
    max_txn: u64,
    /// Largest [`RowId`] a row record names, winners and losers together.
    max_row_id: u64,
    /// Largest version serial a row record names.
    max_seq: u64,
}

impl Scan {
    /// Reads the records once, from the first one.
    fn of(records: &[WalRecord]) -> Result<Self, InternalError> {
        let mut scan = Self::default();
        for record in records {
            scan.max_txn = scan.max_txn.max(record.txn.0);
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
            max_txn: self.max_txn,
            next_row_id,
            next_seq: self.max_seq.saturating_add(1),
            tables,
            ..Recovery::default()
        })
    }
}

/// Second pass: the records that change a page, in journal order, each replayed on a page
/// behind it; then the losers taken out of the clustered tables.
///
/// A page record goes through [`PageWrite::redo`]; a row record or a release of a heap table
/// through [`RedoHeap`]. A row record of a clustered table changes no page by itself: the pages
/// of its trees come back through the page records that follow it, and its compensations are
/// noted for [`ClusteredTable::take_back_losers`].
///
/// Each clustered table is then opened over the trees the redo brought back
/// ([`ClusteredTable::redo_open`]) and the versions of the losers are taken out of it, a
/// table no record reaches included, before the first call of the engine
/// (`a_clustered_table_whose_writes_all_rolled_back_reopens`).
fn redo(
    storage: &DiskStorage,
    records: &[WalRecord],
    recovery: &mut Recovery,
) -> Result<(), InternalError> {
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
            heaps.insert(
                table,
                RedoHeap {
                    heap: Heap::open(storage, table, first_page),
                },
            );
        }
    }
    let mut taken_back: BTreeMap<TableId, TakenBack> = BTreeMap::new();
    let mut on_data = BTreeMap::new();
    for record in records {
        match record.kind {
            WalRecordKind::CreateTable => {
                // The record describes the formatting of the head page of the heap of the
                // table, the overflow heap of a clustered one included ([`Heap::create`]). It
                // is replayed by the `page_lsn` of that page like a page record, the table
                // dropped since or not: the page records that follow it on that page were
                // computed from the page it formatted
                // (`tests::a_dropped_table_whose_head_page_data_lost_reopens`).
                let entry = TableEntry::decode(&record.payload)?;
                Heap::open(storage, entry.table, entry.first_page).redo_format(record.lsn)?;
            }
            WalRecordKind::PageWrite => {
                let write = PageWrite::decode(&record.payload)?;
                recovery.rewritten += write.redo(storage, record.lsn, None, &mut on_data)?.0;
            }
            WalRecordKind::Compensation | WalRecordKind::Purge => {
                let release = SlotRelease::decode(&record.payload)?;
                // A clustered table journals the versions of losers an earlier recovery took
                // back, and what the undo of a `ROLLBACK TO` took out
                // ([`ClusteredTable::rollback_to_savepoint`]); [`ClusteredTable::take_back_losers`]
                // takes them back again under these records. A heap the catalogue no longer
                // holds has nothing to take out.
                if clustered_tables.contains(&release.table) {
                    taken_back
                        .entry(release.table)
                        .or_default()
                        .note(&release, record.lsn);
                } else if let Some(redo) = heaps.get(&release.table)
                    && redo.release(record, &release)?
                {
                    recovery.released += 1;
                }
            }
            WalRecordKind::Insert | WalRecordKind::Update | WalRecordKind::Delete => {
                let (change, ops) = RowChange::decode_with_ops(&record.payload)?;
                if clustered_tables.contains(&change.table) {
                    continue;
                }
                let Some(redo) = heaps.get(&change.table) else {
                    recovery.unplaced += 1;
                    continue;
                };
                if redo.apply(record, &change, &ops)? {
                    recovery.applied += 1;
                } else {
                    recovery.skipped += 1;
                }
            }
            _ => {}
        }
    }
    for &table in &clustered_tables {
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
        let journalled = taken_back.remove(&table).unwrap_or_default();
        clustered.take_back_losers(&recovery.losers, &journalled)?;
        // A split the taking back caused moves a root; the catalogue takes it, and the meta
        // pages take the catalogue at the next commit.
        let roots = (
            Some(clustered.tree_root()),
            Some(clustered.directory_root()),
        );
        if roots != (entry.tree_root, entry.directory_root) {
            storage.lock_catalogue()?.add_table(TableEntry {
                tree_root: roots.0,
                directory_root: roots.1,
                ..entry
            });
        }
    }
    Ok(())
}

/// The redo of one heap table.
///
/// A row record names the heap pages it changed ([`PageOp`]); the redo applies the operations
/// of a page when the `page_lsn` of that page is below the number of the record, and each
/// page it changes takes that number. The numbers are read for each page of a record before
/// the first of its operations is applied, so the two operations of an update that landed on
/// one page are decided by one comparison. The records that free a slot are decided the same
/// way ([`RedoHeap::release`]).
#[derive(Debug)]
struct RedoHeap<'storage> {
    /// The heap the versions go to.
    heap: Heap<'storage>,
}

impl RedoHeap<'_> {
    /// Applies the operations of `record` whose page is behind it, and answers whether one was.
    ///
    /// A `Put` of a version longer than [`MAX_INLINE_LEN`], and a `SetXmax` on the stub of such
    /// a version, change nothing here: the page records that follow the row record write the
    /// chain, the stub and the `xmax` in the first page of the chain ([`super::heap::Heap::put`],
    /// [`super::heap::Heap::overwrite`]).
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a record that names no page, for a page that is not a
    /// heap page, for a `Put` that does not fit at its slot and for a `SetXmax` on a slot that
    /// does not hold the version it names; the errors of the heap otherwise.
    fn apply(
        &self,
        record: &WalRecord,
        change: &RowChange,
        ops: &[PageOp],
    ) -> Result<bool, InternalError> {
        if ops.is_empty() {
            return Err(InternalError::Corruption(format!(
                "the row record at lsn {} names table {} and no page of its heap",
                record.lsn,
                self.heap.table()
            )));
        }
        let mut behind = BTreeSet::new();
        for op in ops {
            let page = op.page();
            if self.is_behind(page, record.lsn)? {
                behind.insert(page);
            }
        }
        for op in ops {
            if !behind.contains(&op.page()) {
                continue;
            }
            match *op {
                PageOp::Put { rid } => self.put(change, rid, record.lsn)?,
                PageOp::SetXmax { rid, seq } => {
                    self.set_xmax((change.header.row, seq), rid, Some(record.txn), record.lsn)?;
                }
            }
        }
        Ok(!behind.is_empty())
    }

    /// Replays a record that frees a heap slot, and answers whether it changed the heap.
    ///
    /// The record is applied when the `page_lsn` of the page of its slot is behind it, as a
    /// row record is: a [`Release::Removed`] frees the slot, a [`Release::XmaxCleared`] unsets
    /// the `xmax` in place. The slot of a long version is freed alike; the unset of its `xmax`
    /// is the page record that follows ([`RedoHeap::apply`]).
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a slot that does not hold the version the record names.
    fn release(&self, record: &WalRecord, release: &SlotRelease) -> Result<bool, InternalError> {
        let key = (release.header.row, release.header.seq);
        if !self.is_behind(release.at.page, record.lsn)? {
            return Ok(false);
        }
        match self.heap.peek(release.at)? {
            Some(Slotted::Inline(bytes)) if self.holds(&bytes, key)? => {}
            Some(Slotted::Stub) => {}
            _ => {
                return Err(InternalError::Corruption(format!(
                    "the record at lsn {} frees version {} of row {} of table {} at {}, which the \
                     heap does not hold there",
                    record.lsn,
                    key.1,
                    key.0,
                    self.heap.table(),
                    release.at
                )));
            }
        }
        match release.release {
            Release::Removed => self.heap.redo_release(release.at, record.lsn),
            Release::XmaxCleared => {
                self.set_xmax(key, release.at, None, record.lsn)?;
                Ok(true)
            }
        }
    }

    /// Whether `page` is behind the record of number `lsn`.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the page is not a heap page: the records before
    /// this one, replayed first, format each page a row record names.
    fn is_behind(&self, page: PageId, lsn: Lsn) -> Result<bool, InternalError> {
        match self.heap.page_lsn(page)? {
            Some(at) => Ok(at < lsn),
            None => Err(InternalError::Corruption(format!(
                "the record at lsn {lsn} names page {page} of the heap of table {}, which is not \
                 a heap page once the records before it are replayed",
                self.heap.table()
            ))),
        }
    }

    /// Writes the version of `change` at `rid`; a version longer than [`MAX_INLINE_LEN`] is
    /// left to the page records that follow.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the slot has no room for it: the page holds versions
    /// its journal does not account for.
    fn put(&self, change: &RowChange, rid: Rid, lsn: Lsn) -> Result<(), InternalError> {
        let mut bytes = Vec::with_capacity(VERSION_PREFIX_LEN + change.columns.len());
        change.header.write_to(&mut bytes);
        bytes.extend_from_slice(&change.columns);
        if bytes.len() > MAX_INLINE_LEN || self.heap.put(rid, &bytes, lsn)? {
            return Ok(());
        }
        Err(InternalError::Corruption(format!(
            "the record at lsn {lsn} puts version {} of row {} of table {} at {rid}, where the \
             page has no room for its {} bytes",
            change.header.seq,
            change.header.row,
            self.heap.table(),
            bytes.len()
        )))
    }

    /// Writes `xmax` in the `xmax` field of the version `key`, which sits at `rid`; the stub of
    /// a long version is left to the page record that follows.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the slot does not hold that version.
    fn set_xmax(
        &self,
        key: (RowId, u64),
        rid: Rid,
        xmax: Option<TxnId>,
        lsn: Lsn,
    ) -> Result<(), InternalError> {
        match self.heap.peek(rid)? {
            Some(Slotted::Stub) => return Ok(()),
            Some(Slotted::Inline(bytes)) if self.holds(&bytes, key)? => {}
            _ => {
                return Err(InternalError::Corruption(format!(
                    "the record at lsn {lsn} sets the xmax of version {} of row {} of table {} \
                     at {rid}, which the heap does not hold there",
                    key.1,
                    key.0,
                    self.heap.table()
                )));
            }
        }
        let field = xmax.map_or(NO_XMAX, |txn| txn.0).to_le_bytes();
        self.heap.overwrite(rid, OFF_XMAX, &field, lsn)?;
        Ok(())
    }

    /// Whether the version `bytes` is the version `key`.
    fn holds(&self, bytes: &[u8], key: (RowId, u64)) -> Result<bool, InternalError> {
        let header = VersionHeader::read_from(bytes)?;
        Ok((header.row, header.seq) == key)
    }
}

#[cfg(test)]
mod tests;
