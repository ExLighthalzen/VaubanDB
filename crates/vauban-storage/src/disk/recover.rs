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
//! 2. **Redo.** The row records of the winners are applied to the heaps, in journal order,
//!    each to the pages it names when their `page_lsn` is behind it ([`RedoHeap::apply`]),
//!    and so are the records that free a heap slot ([`WalRecordKind::Compensation`],
//!    [`WalRecordKind::Purge`]) of each transaction, winner or not ([`RedoHeap::release`]).
//!
//! # Page numbers
//!
//! A row record of a heap table names the pages it changed and what it did on each
//! ([`super::wal_payload::PageOp`]); each page took the number of the record in the change
//! itself ([`super::heap::Heap::put`], [`super::heap::Heap::overwrite`]). A page whose
//! `page_lsn` is at or past the number of a record is left as it stands by that record: the
//! page reached `data` after the change of the record. A page behind it gets the operation and
//! the number. A page on `data` newer than the records is therefore not changed by them: a
//! version a vacuum took away after its insert reached `data` is not put back by the insert
//! record (`tests::a_page_on_data_before_the_crash_is_left_as_it_stands_by_the_redo`), and a
//! second redo over the pages of the first gives the same pages
//! (`tests::a_redo_played_twice_gives_the_same_pages`).
//!
//! The operations write at a slot, or the `xmax` of the version at a slot, so applying one to
//! a page that already holds it rewrites the slot with what it held
//! (`super::heap::tests::put_over_a_record_replaces_it`). The number is written in the change
//! that writes the bytes, so a page does not leave the pool holding a record its number does
//! not cover.
//!
//! The writes that free room in a heap page are journalled too (below), so a page behind a
//! record holds the versions it held when the record was written, less the versions of losers
//! the redo leaves out. A `Put` therefore finds the room its record found, and one that does
//! not is a page its journal does not describe: the redo refuses it rather than placing the
//! version elsewhere ([`InternalError::Corruption`],
//! `tests::a_put_without_room_at_its_slot_is_corruption`). A version sits where its record
//! put it, and the records that follow find it at that slot.
//!
//! The chain of the heap is not journalled (`page_lsn` covers the records of a page, not the
//! link that leads to it): a page of the chain that reads back as a free page is formatted
//! again, and a page a record names that the chain on `data` does not reach is chained back
//! ([`super::heap::Heap::redo_chain`],
//! `tests::a_chained_page_whose_formatting_missed_data_is_formatted_again`,
//! `tests::a_page_on_data_out_of_the_chain_on_data_is_chained_back`).
//!
//! # Records that free a slot
//!
//! The undo of a rollback, of a rollback to a savepoint, and a vacuum journal each version they
//! take out of a heap slot, or whose `xmax` they unset ([`super::wal_payload::SlotRelease`]).
//! The redo replays them at their place in the journal, on the slot they name, when the
//! `page_lsn` of its page is behind them, as it does the row records: the version the slot holds
//! is taken out, or has its `xmax` unset when it carries the `xmax` the record names, and the
//! page takes the number of the record. A slot that does not hold the version, a write of a
//! loser the redo left out, leaves the record without effect ([`Recovery::released`] counts
//! the ones that changed the heap). They are replayed for a loser too, and inside a range a
//! rollback to a savepoint cancels: a compensation is what that rollback did, not one of the
//! writes it took back.
//!
//! What this gives back is the room: a page another transaction shares may reach `data`
//! holding the versions a rollback later took out, and a write after the rollback may have
//! used that room. The redo takes those versions out before it replays that write
//! (`a_rollback_on_a_page_already_on_data_gives_its_room_back_at_the_redo`), and a vacuumed
//! version replayed from its `Insert` leaves again at its `Purge`
//! (`a_vacuum_then_a_crash_recovers_the_committed_rows_without_the_purged_versions`).
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
//! checkpoint. The redo therefore reads records a checkpoint already wrote, and skips them
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
//! A loser is not replayed, and on a heap the redo runs no undo of its own beyond the
//! compensation records the journal holds: under no-steal the page of a running
//! transaction is held back, so what it wrote is usually left in the pool a crash threw away.
//! Usually, not in one case: two transactions that write the same page share it, and the
//! commit of the first clears the `in_progress` flag of that page
//! (`a_page_shared_by_two_txns_loses_its_flag_at_the_first_commit` in
//! [`super::version`]), after which a flush may write the version of the second one to `data`.
//! A loser that rolled back before the crash journalled its undo, and the redo takes its
//! versions out (above). On a heap, the writes of a loser that neither a `ROLLBACK TO` nor a rollback the
//! crash cut short had undone are taken back after the redo, when the table is first reached:
//! [`super::version::HeapTable::resume_after_recovery`] marks the losers `Aborted`, which
//! [`crate::Snapshot::is_visible`] reads (`a_loser_whose_page_reached_data_is_invisible`), then
//! takes out each version a loser created and unsets each `xmax` a loser set, each by a
//! [`WalRecordKind::Compensation`] it appends and syncs before the slot changes. A later write
//! of the row, and its rollback, find the chain the winners left
//! (`a_row_a_loser_updated_then_deleted_takes_an_update_and_its_rollback`), and the next
//! recovery replays those compensations as it replays the ones of a rollback
//! (`a_loser_taken_back_by_a_recovery_cut_short_is_taken_back_by_the_next`).
//!
//! A clustered table is taken back here rather than at its first call, in the pass that
//! replays it, and before its winners: [`super::clustered::ClusteredTable::take_back_losers`]
//! takes out of the tree each version a loser created and unsets each `xmax` a loser set, each
//! by a compensation appended and synced before the entry changes, and rebuilds the directory.
//! The order matters: a winner that wrote a row after a loser rolled it back links its version
//! to the newest one of the row, which would be the version of the loser if it were still there
//! (`a_clustered_row_a_loser_wrote_takes_writes_and_rollbacks_after_recovery`). A compensation
//! of a clustered table the journal already holds is made again under its own record, so a
//! recovery whose changes stayed in the pool of the instance that crashed adds no record the
//! next time (`a_clustered_take_back_lost_with_the_pool_is_made_again_under_its_records`).
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
//! (`super::tests::ddl_survives_reopen_without_checkpoint`). Replaying them over a catalogue
//! the meta pages already carry keeps the roots and the row counter those pages hold, which a
//! split or the writes moved since the statement ([`keep_the_roots_the_meta_pages_held`]).
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
use super::clustered::{ClusteredTable, TakenBack};
use super::heap::{Heap, Rid};
use super::meta::{Catalogue, IndexEntry, TableEntry};
use super::page::{Lsn, PageId};
use super::version::{NO_XMAX, OFF_XMAX, VERSION_PREFIX_LEN, VersionHeader};
use super::wal::{WalRecord, WalRecordKind};
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
    /// Row records of winners the redo wrote to a heap or a tree.
    pub(crate) applied: usize,
    /// Row records of winners whose pages already carried their number, or, for a clustered
    /// table, whose version the tree already held.
    pub(crate) skipped: usize,
    /// Row records of winners naming a table no [`WalRecordKind::CreateTable`] record names.
    pub(crate) unplaced: usize,
    /// [`WalRecordKind::Compensation`] and [`WalRecordKind::Purge`] records that changed a
    /// heap: the version they name was there, and was taken out or had its `xmax` unset.
    pub(crate) released: usize,
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
            max_txn: 0,
            next_row_id: 1,
            next_seq: 1,
            tables: BTreeMap::new(),
            applied: 0,
            skipped: 0,
            unplaced: 0,
            released: 0,
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
    /// Largest transaction identifier a record carries, row, end or instance record.
    max_txn: u64,
    /// Largest [`RowId`] a row record names, winners and losers together.
    max_row_id: u64,
    /// Largest version serial a row record names.
    max_seq: u64,
    /// Row records the redo must leave out. A [`WalRecordKind::RollbackTo`] record cancels the
    /// row records of its transaction between the [`WalRecordKind::Savepoint`] its payload
    /// names and itself: a redo that replayed the writes the savepoint protected would put
    /// back what the running transaction had taken away. The compensation records the undo
    /// appends come after the `RollbackTo`, and the redo replays them inside a cancelled range
    /// too (`a_compensation_inside_an_outer_rolled_back_range_is_replayed`).
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
            max_txn: self.max_txn,
            next_row_id,
            next_seq: self.max_seq.saturating_add(1),
            tables,
            applied: 0,
            skipped: 0,
            unplaced: 0,
            released: 0,
            cancelled: self.cancelled,
        })
    }
}

/// Second pass: the row records of the winners, in journal order, applied to the heaps.
///
/// A record of a heap table goes through [`RedoHeap::apply`], which compares the `page_lsn` of
/// each page the record names with its number.
///
/// The heap of each table the journal names is attached first, whether a record of a winner
/// reaches it or not: a head page that stayed in the pool of the instance that crashed is
/// formatted there, so that the table reads back as the empty heap it was
/// (`uncommitted_insert_gone_after_reopen`).
///
/// A clustered table is left out of this pass for the row redo: the pages of its tree reach
/// `data` at a checkpoint. The recovery rebuilds the trees at the `open` by walking the
/// versions the DDL phase installed on the catalogue: each winner record is inserted into the
/// tree of its clustered table in a separate pass, after what the losers left in that tree has
/// been taken out ([`ClusteredTable::take_back_losers`]).
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
    let mut clustered_changes: BTreeMap<TableId, Vec<(WalRecordKind, RowChange)>> = BTreeMap::new();
    let mut taken_back: BTreeMap<TableId, TakenBack> = BTreeMap::new();
    for record in records {
        let kind = record.kind;
        if matches!(kind, WalRecordKind::Compensation | WalRecordKind::Purge) {
            let release = SlotRelease::decode(&record.payload)?;
            // A clustered table journals the versions of losers an earlier recovery took back
            // ([`ClusteredTable::take_back_losers`]), which takes them back again under these
            // records; a heap the catalogue no longer holds has nothing to take out.
            if clustered_tables.contains(&release.table) {
                let change = (
                    release.header.row,
                    release.header.seq,
                    release.release as u8,
                );
                taken_back
                    .entry(release.table)
                    .or_default()
                    .insert(change, record.lsn);
            } else if let Some(redo) = heaps.get_mut(&release.table)
                && redo.release(record, &release)?
            {
                recovery.released += 1;
            }
            continue;
        }
        if !matches!(
            kind,
            WalRecordKind::Insert | WalRecordKind::Update | WalRecordKind::Delete
        ) || !winners.contains(&record.txn)
            || recovery.cancelled.contains(&record.lsn)
        {
            continue;
        }
        let (change, ops) = RowChange::decode_with_ops(&record.payload)?;
        if clustered_tables.contains(&change.table) {
            clustered_changes
                .entry(change.table)
                .or_default()
                .push((kind, change));
            continue;
        }
        let Some(redo) = heaps.get_mut(&change.table) else {
            recovery.unplaced += 1;
            continue;
        };
        if redo.apply(record, &change, &ops)? {
            recovery.applied += 1;
        } else {
            recovery.skipped += 1;
        }
    }
    // Apply clustered records: each winner version is inserted into the tree. The roots may
    // hold free pages when the pages a split or an insert left dirty stayed in the pool of the
    // instance that crashed: [`ClusteredTable::redo_open`] formats them before the replay, as
    // [`RedoHeap::over`] does for the head page of a heap. Each clustered table goes through
    // it, a table no winner wrote in included, so that its roots read back as the empty
    // leaves they were rather than as free pages
    // (`a_clustered_table_whose_writes_all_rolled_back_reopens`).
    let no_change = Vec::new();
    for &table in &clustered_tables {
        let changes = clustered_changes.get(&table).unwrap_or(&no_change);
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
        // What the losers left in the tree leaves it before the winners are replayed: a winner
        // that wrote a row after the rollback of a loser links its version to the one the
        // directory names, which is the version of the loser while it is there
        // (`a_clustered_row_a_loser_wrote_takes_writes_and_rollbacks_after_recovery`).
        let journalled = taken_back.remove(&table).unwrap_or_default();
        clustered.take_back_losers(&recovery.losers, &journalled)?;
        for (kind, change) in changes {
            // The record of an update names the version it creates; the version it replaces
            // takes its `xmax` first, as [`RedoHeap::hide_previous`] does on a heap, so that a
            // key the update moved does not leave its old entry alive beside the new one
            // (`a_committed_clustered_key_update_reads_back_once_after_a_reopen`).
            let hidden = *kind == WalRecordKind::Update
                && clustered.replay_hide_previous(
                    change.header.row,
                    change.header.seq,
                    change.header.xmin,
                )?;
            let created = clustered.replay_one(change)?;
            if hidden || created {
                recovery.applied += 1;
            } else {
                recovery.skipped += 1;
            }
        }
        // A split the replay caused moves a root; the catalogue takes it, and the meta pages
        // take the catalogue at the next commit.
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

/// The redo of one heap table: its heap and the chain of its pages.
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
    /// The pages of its chain, as [`Heap::redo_chain`] found them and the redo extended them.
    chain: BTreeSet<PageId>,
    /// The last page of that chain.
    last: PageId,
}

impl<'storage> RedoHeap<'storage> {
    /// Attaches to the heap of `table` and walks its chain.
    ///
    /// A page of the chain whose formatting stayed in the pool of the instance that crashed
    /// reads back as a [`super::page::PageKind::Free`] page, and the redo formats it as an empty heap page
    /// ([`Heap::redo_chain`]), the head page included (`reopen_after_commit_without_checkpoint`).
    fn over(
        storage: &'storage DiskStorage,
        table: TableId,
        first_page: PageId,
    ) -> Result<Self, InternalError> {
        let heap = Heap::open(storage, table, first_page);
        let pages = heap.redo_chain()?;
        let last = pages.last().copied().unwrap_or(first_page);
        Ok(Self {
            heap,
            chain: pages.into_iter().collect(),
            last,
        })
    }

    /// Applies the operations of `record` whose page is behind it, and answers whether one was.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a record that names no page, for a `Put` that does
    /// not fit at its slot and for a `SetXmax` on a slot that does not hold the version it
    /// names; the errors of the heap otherwise.
    fn apply(
        &mut self,
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
        let mut applied = false;
        for op in ops {
            if !behind.contains(&op.page()) {
                continue;
            }
            applied = true;
            match *op {
                PageOp::Put { rid } => self.put(change, rid, record.lsn)?,
                PageOp::SetXmax { rid, seq } => {
                    self.set_xmax((change.header.row, seq), rid, Some(record.txn), record.lsn)?;
                }
            }
        }
        Ok(applied)
    }

    /// Replays a record that frees a heap slot, and answers whether it changed the heap.
    ///
    /// The record is applied when the `page_lsn` of the page of its slot is behind it, as a
    /// row record is: a [`Release::Removed`] frees the slot, a [`Release::XmaxCleared`] unsets
    /// the `xmax` in place when the version carries the one the record names. A slot that
    /// does not hold the version leaves the record without effect: the write that put it there
    /// was a loser's, which the redo left out.
    fn release(
        &mut self,
        record: &WalRecord,
        release: &SlotRelease,
    ) -> Result<bool, InternalError> {
        let key = (release.header.row, release.header.seq);
        if !self.is_behind(release.at.page, record.lsn)? {
            return Ok(false);
        }
        let Some(held) = self.held(release.at)? else {
            return Ok(false);
        };
        if (held.row, held.seq) != key {
            return Ok(false);
        }
        match release.release {
            Release::Removed => self.heap.free_slot(release.at, record.lsn),
            Release::XmaxCleared => {
                if held.xmax.is_none() || held.xmax != release.header.xmax {
                    return Ok(false);
                }
                self.set_xmax(key, release.at, None, record.lsn)?;
                Ok(true)
            }
        }
    }

    /// Whether `page` is behind the record of number `lsn`, the page chained first when the
    /// walk did not reach it.
    fn is_behind(&mut self, page: PageId, lsn: Lsn) -> Result<bool, InternalError> {
        self.attach(page)?;
        Ok(self.heap.page_lsn(page)?.unwrap_or(Lsn(0)) < lsn)
    }

    /// Puts `page` in the chain when the walk did not reach it.
    fn attach(&mut self, page: PageId) -> Result<(), InternalError> {
        if self.chain.contains(&page) {
            return Ok(());
        }
        self.heap.redo_adopt(self.last, page)?;
        self.chain.insert(page);
        self.last = page;
        Ok(())
    }

    /// Writes the version of `change` at `rid`.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the slot has no room for it: the page holds versions
    /// its journal does not account for.
    fn put(&mut self, change: &RowChange, rid: Rid, lsn: Lsn) -> Result<(), InternalError> {
        let mut bytes = Vec::with_capacity(VERSION_PREFIX_LEN + change.columns.len());
        change.header.write_to(&mut bytes);
        bytes.extend_from_slice(&change.columns);
        if self.heap.put(rid, &bytes, lsn)? {
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

    /// Writes `xmax` in the `xmax` field of the version `key`, which sits at `rid`.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the slot does not hold that version.
    fn set_xmax(
        &mut self,
        key: (RowId, u64),
        rid: Rid,
        xmax: Option<TxnId>,
        lsn: Lsn,
    ) -> Result<(), InternalError> {
        if self
            .held(rid)?
            .is_none_or(|held| (held.row, held.seq) != key)
        {
            return Err(InternalError::Corruption(format!(
                "the record at lsn {lsn} sets the xmax of version {} of row {} of table {} at \
                 {rid}, which the heap does not hold there",
                key.1,
                key.0,
                self.heap.table()
            )));
        }
        let field = xmax.map_or(NO_XMAX, |txn| txn.0).to_le_bytes();
        self.heap.overwrite(rid, OFF_XMAX, &field, lsn)?;
        Ok(())
    }

    /// The prefix of the version at `rid`, `None` for a free slot.
    fn held(&self, rid: Rid) -> Result<Option<VersionHeader>, InternalError> {
        match self.heap.get(rid)? {
            Some(bytes) => Ok(Some(VersionHeader::read_from(&bytes)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests;
