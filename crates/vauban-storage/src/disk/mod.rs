//! On-disk implementation of the storage layer: paged data file, buffer pool, heap, WAL and
//! B+tree, with its DDL persisted in a catalogue and the whole put behind [`crate::Storage`],
//! the trait the in-memory implementation already answers ([`storage_impl`]).
//!
//! The file format is VaubanDB's own and versioned from its first byte. Going through a trait
//! is what lets the catalogue, the transaction manager and the executor be written against
//! the in-memory implementation and switch to this one without changing a line.
//!
//! # Layout of the module
//!
//! [`page`] fixes the page size, the common header and the checksum rule; [`crc`] holds the
//! checksum algorithm; [`file`] moves pages between the data file and memory, [`control`]
//! holds the format of `vauban.ctl`, [`buffer`] caches the pages the engine works on and
//! [`alloc`] hands pages out and grows the file. [`wal`] holds the journal and
//! [`wal_payload`] the payload of its row records; [`heap`] and [`version`] store the rows of
//! a heap table, [`btree`] and [`clustered`] those of a clustered table, [`index`] the
//! secondary indexes, [`meta`] the catalogue and [`recover`] the replay of the journal at
//! `open`. The re-exports in `lib.rs` are the two public types below.
//!
//! [`DiskStorage`] and [`DiskOptions`] are re-exported by the crate root; [`page::Page`],
//! [`page::PageId`], [`page::Lsn`] and [`page::PageKind`] stay inside the crate, because the
//! trait does not mention them.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{Collation, Len, SqlType, Value};

use buffer::BufferPool;
use clustered::ClusteredTable;
use control::Control;
use file::DataFile;
use heap::Heap;
use index::DiskIndex;
use meta::{Catalogue, IndexEntry, TableEntry};
use page::{Lsn, PageId, PageKind};
use recover::Recovery;
use version::{HeapTable, TableState};
use wal::{WalHandle, WalRecordKind};

use crate::{
    DbId, IndexId, IndexShape, KeyColumn, SavepointId, Snapshot, TableId, TableShape, TxnId,
    TxnStatus,
};

// The page header carries accessors reached from the tests alone: `flags`, `set_kind`, the pair
// around `slot_count` and the pair around `lower`.
#[allow(dead_code)]
pub(crate) mod crc;
#[allow(dead_code)]
pub(crate) mod page;

// The allocator is called by its own tests, which go through `allocate` and `free`; its
// callers in the engine are the heap and the B+tree.
#[allow(dead_code)]
mod alloc;
#[allow(dead_code)] // the page layer is reached from the tests alone
mod btree;
// The pool is built by `open`, and its `pin`, `mark_dirty`, `set_in_progress` and `flush` are
// called by the allocator (`alloc`) and by the versioned heap (`version`). `flush_all`,
// `capacity` and `Fixed` are read by the tests of this module and of `buffer.rs`; the caller
// of `flush_all` in the engine is the checkpoint.
#[allow(dead_code)]
mod buffer;
#[allow(dead_code)] // some accessors are reached from the tests alone
mod clustered;
// The counters of the control file are moved forward and rewritten by the allocator (`alloc`);
// `seal` and `from_bytes` are reached by the tests of this module and of `control.rs`.
#[allow(dead_code)]
mod control;
#[allow(dead_code)] // some codecs are reached from the tests alone
mod encode;
#[allow(dead_code)]
mod file;
#[allow(dead_code)] // some accessors are reached from the tests alone
mod heap;
#[allow(dead_code)] // some accessors are reached from the tests alone
mod index;
// The catalogue of the instance: `storage_impl` reads `TableEntry::next_row_id` and the
// roots of a clustered table, which this module writes.
#[allow(dead_code)]
mod meta;
// `install_single_heap` and the fields of `Recovery` are read by the tests of this module;
// `recover` is called by `open`, and `storage_impl` reads the outcome in the engine.
#[allow(dead_code)]
mod recover;
#[allow(dead_code)] // some accessors are reached from the tests alone
mod version;
// The rows behind the trait: the `impl Storage for DiskStorage` and the store it dispatches
// each call to.
mod storage_impl;
// `Wal::open`, `append` and `flush` are reached through `WalHandle` by the versioned heap;
// `iter`, `records`, `last_lsn`, `path` and `append_offset` are read by the tests of the
// module and by the recovery.
#[allow(dead_code)]
mod wal;
#[allow(dead_code)] // `RowChange::decode` is read by the tests and by the recovery
mod wal_payload;

/// Default number of pages held by the buffer pool.
const DEFAULT_BUFFER_PAGES: usize = 1024;

/// Name of the control file inside the directory of an instance.
pub(crate) const CONTROL_FILE_NAME: &str = "vauban.ctl";

/// Name of the data file inside the directory of an instance.
pub(crate) const DATA_FILE_NAME: &str = "data";

/// Name of the journal file inside the directory of an instance.
pub(crate) const WAL_FILE_NAME: &str = "wal";

/// Options of an on-disk instance, passed to [`DiskStorage::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskOptions {
    /// Number of frames the buffer pool holds, that is `buffer_pages * 8192` bytes of cache.
    /// 0 is refused by [`DiskStorage::open`] as [`InternalError::Bug`].
    pub buffer_pages: usize,
}

impl Default for DiskOptions {
    /// 1 024 pages, that is 8 MiB of cache.
    fn default() -> Self {
        Self {
            buffer_pages: DEFAULT_BUFFER_PAGES,
        }
    }
}

/// Durable implementation of [`crate::Storage`] over a directory of paged files.
///
/// The structure holds the directory, the open files, the control block read from
/// `vauban.ctl` and the buffer pool over the data file. On the path of the engine, pages of
/// `data` are read and written through the pool: a caller `pin`s a page rather than reading
/// it, and the `seek` and the `read_exact` / `write_all` of the file layer are issued from
/// there. [`DiskStorage::data`] is a second `pub(crate)` handle on the same file, which the
/// tests of this module use to write a page behind the pool, and which the allocator
/// ([`alloc`]) grows. The `impl Storage` is in `storage_impl`.
// `options` is read by the tests of this module and by nothing else in the engine.
#[allow(dead_code)]
#[derive(Debug)]
pub struct DiskStorage {
    /// Directory of the instance, the `dir` given to [`DiskStorage::open`].
    pub(crate) dir: PathBuf,
    /// The data file, `<dir>/data`. [`DiskStorage::pool`] holds the same handle and
    /// serialises the page reads and writes it issues itself; a call made through this field
    /// goes to the file without passing that lock.
    pub(crate) data: Arc<DataFile>,
    /// Cache of the pages of `data`, of `options.buffer_pages` frames.
    pub(crate) pool: BufferPool,
    /// The journal, `<dir>/wal`, shared with the buffer pool: a write is appended here before
    /// the page it changes is touched, and `commit` `sync_all`s it ([`version::HeapTable`]).
    pub(crate) wal: WalHandle,
    /// Control block read from `<dir>/vauban.ctl`, or the fresh one written at creation.
    ///
    /// Behind a lock because the allocator moves `next_page_id` and `free_head` forward
    /// through `&self` and rewrites the file ([`alloc`]); [`DiskStorage::lock_control`] is how
    /// that lock is taken, [`DiskStorage::control`] copies the block out of it.
    pub(crate) control: Mutex<Control>,
    /// Options the instance was opened with.
    pub(crate) options: DiskOptions,
    /// What the recovery of [`DiskStorage::open`] found in the journal and did with it
    /// ([`recover`]), read through [`DiskStorage::recovery`].
    recovery: Recovery,
    /// The databases, tables and indexes of the instance, read from the chain of
    /// [`page::PageKind::Meta`] pages at the `open` and rewritten at each DDL statement
    /// ([`meta`]).
    ///
    /// Behind a lock because the DDL methods take it through `&self`;
    /// [`DiskStorage::lock_catalogue`] is how that lock is taken. A caller that also needs the
    /// control block takes **this one first**: the DDL allocates pages under it, and
    /// [`alloc::allocate`] takes the control block itself.
    catalogue: Mutex<Catalogue>,
    /// The mutable state of each table: its counters, its directory, the undo logs of the
    /// transactions that wrote in it and its index maintenance log ([`TableState`]).
    ///
    /// The map is behind an [`RwLock`] — a lookup takes the read lock, the first call on a
    /// table takes the write lock to add its cell — and each cell behind a [`Mutex`], which
    /// [`DiskStorage::with_rows`] holds for the length of one call: one writer at a time on a
    /// table. Holding the state here rather than in [`version::HeapTable`] is what keeps
    /// `pub struct DiskStorage` free of a lifetime parameter.
    tables: RwLock<HashMap<TableId, Arc<Mutex<TableState>>>>,
    /// The transactions of the instance: their status, whether their `Begin` reached the
    /// journal and which tables they wrote in ([`TxnRegistry`]).
    txns: RwLock<TxnRegistry>,
    /// Keeps the directory to this instance for as long as the structure lives
    /// ([`InstanceGuard`]).
    // Read by nobody: what it does, it does in its `Drop`.
    #[allow(dead_code)]
    instance: InstanceGuard,
}

impl DiskStorage {
    /// Opens the instance held by `dir`, or creates an empty one.
    ///
    /// # Layout of an instance
    ///
    /// An instance is one directory holding three files:
    ///
    /// | File | Contents |
    /// |---|---|
    /// | `vauban.ctl` | versioned header and persisted counters, 128 bytes ([`control`]) |
    /// | `data` | the pages, a whole number of 8 192-byte pages, 0 at creation ([`file`]) |
    /// | `wal` | the journal, empty at creation; its record format is that of the `wal` module |
    ///
    /// # Creation
    ///
    /// `dir` is created when it is missing (`create_dir_all`, parents included). A directory
    /// that holds no `vauban.ctl`, no `data` and no `wal` is taken for an empty one and the
    /// three files are created in it; anything else it may hold is left alone, so that the
    /// `tls/` directory the server writes does not make it look like an instance.
    ///
    /// `data` and `wal` are created first, then `vauban.ctl` is written and `sync_all`ed. A
    /// crash between the two steps therefore leaves `data` or `wal` without a `vauban.ctl`,
    /// which the next `open` reports as corruption rather than creating an instance over the
    /// files already there; a crash before them leaves a directory this `open` treats as the
    /// empty one it is.
    ///
    /// # Reopening
    ///
    /// A `vauban.ctl` that is there is read and checked: its size, its magic, its
    /// `format_version` and its checksum. A `format_version` other than the 2 this build
    /// writes is
    /// [`InternalError::Corruption`], and this build does not migrate it. `data` and `wal`
    /// are then opened as they stand; either of the two missing is
    /// [`InternalError::Corruption`] naming the file, and so is a `data` whose size is not a
    /// whole number of pages. A directory that holds `data` or `wal` without a `vauban.ctl`
    /// is corruption too: it holds part of an instance, and this build does not complete it.
    ///
    /// The journal is opened by [`wal::Wal::open`], which scans its records and truncates a
    /// tail an unfinished append left behind.
    ///
    /// # Recovery
    ///
    /// Once the three files are open and the pool is built, [`recover::recover`] replays the
    /// journal: the row records of the transactions that hold a `Commit` are applied to the
    /// heaps, the ones of the transactions that do not are left out, and the counters of
    /// `vauban.ctl` take the identifiers the journal names. The replay starts at the first
    /// record of the journal rather than at [`control::Control::latest_checkpoint_lsn`], and
    /// is idempotent, both of which [`recover`] explains and
    /// `recover::tests::txn_straddling_checkpoint_is_redone` asserts. What it found is kept
    /// in [`DiskStorage::recovery`], which is where the head page of a heap and the list of
    /// the interrupted transactions are read from
    /// ([`version::HeapTable::resume_after_recovery`]).
    ///
    /// # Buffer pool
    ///
    /// The instance opens with a pool of `opts.buffer_pages` frames over `data`, empty: no
    /// page is read at open time. The pool reads the durable LSN of the journal through the
    /// [`WalHandle`] this `open` builds, so a page it holds waits for the record that
    /// describes it to be flushed ([`buffer::BufferPool::flush`]). `buffer_pages == 0` is
    /// [`InternalError::Bug`], checked before any file is touched, so a refused option leaves
    /// no half-created instance.
    ///
    /// # Errors
    ///
    /// A failure reported by the operating system is [`InternalError::Io`]; a file that is
    /// there but does not hold what this build reads is [`InternalError::Corruption`]; an
    /// option this build refuses is [`InternalError::Bug`].
    pub fn open(dir: &Path, opts: DiskOptions) -> Result<Self, InternalError> {
        buffer::validate_buffer_pages(opts.buffer_pages)?;
        let instance = InstanceGuard::acquire(dir)?;
        let control_path = dir.join(CONTROL_FILE_NAME);
        let data_path = dir.join(DATA_FILE_NAME);
        let wal_path = dir.join(WAL_FILE_NAME);

        let mut storage = if control_path.exists() {
            Self::reopen(dir, opts, instance, &control_path, &data_path, &wal_path)?
        } else if data_path.exists() || wal_path.exists() {
            return Err(InternalError::Corruption(format!(
                "directory {} holds {DATA_FILE_NAME} or {WAL_FILE_NAME} but no \
                 {CONTROL_FILE_NAME}",
                dir.display()
            )));
        } else {
            Self::create(dir, opts, instance, &control_path, &data_path, &wal_path)?
        };
        let root = storage.control()?.meta_root;
        storage.catalogue = Mutex::new(meta::load(&storage, root)?);
        storage.recovery = recover::recover(&storage)?;
        storage.rebuild_indexes()?;
        Ok(storage)
    }

    /// Rebuilds the B+tree of each index the catalogue names with a root of its own.
    ///
    /// The journal names no index page: the tree of an index comes back through the
    /// pages that reached `data` at a checkpoint, and the pages a running transaction dirtied
    /// are lost with its pool. The contract of the trait ("the indexes are maintained by the
    /// implementation") lets the caller rebuild them from the versions that survive the redo,
    /// which is what [`index::DiskIndex::create`] does at `create_index`; this pass runs
    /// here over each index whose root the catalogue names, so a reopen that took no
    /// checkpoint answers `seek` the way an instance that ran through to the close would
    /// (`indexes_are_rebuilt_at_open`).
    ///
    /// An index that shares the tree of its clustered table ([`DiskStorage::create_index`]) is
    /// left alone: its entries **are** the versions, and the redo brings the tree itself back.
    ///
    /// # Errors
    ///
    /// The errors of [`index::DiskIndex::create`] on a table whose pages read back damaged.
    fn rebuild_indexes(&self) -> Result<(), InternalError> {
        let entries: Vec<IndexEntry> = {
            let catalogue = self.lock_catalogue()?;
            catalogue.all_indexes().into_iter().cloned().collect()
        };
        for entry in entries {
            let Some(old_root) = entry.root else {
                continue;
            };
            // Free the tree the crashed instance left behind, unless the root reads back as
            // a free page — the pages the pool held when the process ended did not reach `data`, and
            // `allocate` already gave them back. Freeing a free page would list it twice.
            let kind = {
                let pin = self.pool.pin(old_root)?;
                let k = pin.with_page(|page| page.kind())?;
                drop(pin);
                k?
            };
            if kind != PageKind::Free {
                index::free_tree(self, old_root)?;
            }
            let new_root = self
                .with_rows(entry.table, |store| {
                    let fresh =
                        index::DiskIndex::create(self, entry.index, &entry.shape, store.source())?;
                    Ok(fresh.root())
                })
                .map_err(|e: SqlError| {
                    InternalError::Corruption(format!(
                        "the index rebuild at the open of instance {} failed: {}",
                        self.dir.display(),
                        e.message
                    ))
                })?;
            let mut catalogue = self.lock_catalogue()?;
            catalogue.add_index(IndexEntry {
                root: Some(new_root),
                ..entry
            });
        }
        Ok(())
    }

    /// What the recovery of this `open` found in the journal and did with it.
    ///
    /// A journal with no record answers `Recovery::default()`
    /// (`recover::tests::an_empty_instance_recovers_to_the_default`).
    pub(crate) fn recovery(&self) -> &Recovery {
        &self.recovery
    }

    /// Creates the three files of an empty instance in `dir`, creating `dir` if needed.
    fn create(
        dir: &Path,
        opts: DiskOptions,
        instance: InstanceGuard,
        control_path: &Path,
        data_path: &Path,
        wal_path: &Path,
    ) -> Result<Self, InternalError> {
        fs::create_dir_all(dir)?;
        let data = Arc::new(DataFile::create(data_path)?);
        let wal = WalHandle::open(wal_path)?;
        let control = Control::default();
        control.write(control_path)?;
        let pool = BufferPool::with_durable_lsn(
            Arc::clone(&data),
            opts.buffer_pages,
            Arc::new(wal.clone()),
        )?;
        Ok(Self {
            dir: dir.to_path_buf(),
            data,
            pool,
            wal,
            control: Mutex::new(control),
            options: opts,
            recovery: Recovery::default(),
            catalogue: Mutex::new(Catalogue::default()),
            tables: RwLock::new(HashMap::new()),
            txns: RwLock::new(TxnRegistry::default()),
            instance,
        })
    }

    /// Reads the control file of an existing instance and opens its two other files.
    fn reopen(
        dir: &Path,
        opts: DiskOptions,
        instance: InstanceGuard,
        control_path: &Path,
        data_path: &Path,
        wal_path: &Path,
    ) -> Result<Self, InternalError> {
        let control = Control::read(control_path)?;
        for required in [data_path, wal_path] {
            if !required.exists() {
                return Err(InternalError::Corruption(format!(
                    "instance {} has a {CONTROL_FILE_NAME} but no {}",
                    dir.display(),
                    required.display()
                )));
            }
        }
        let data = Arc::new(DataFile::open(data_path)?);
        let wal = WalHandle::open(wal_path)?;
        let pool = BufferPool::with_durable_lsn(
            Arc::clone(&data),
            opts.buffer_pages,
            Arc::new(wal.clone()),
        )?;
        Ok(Self {
            dir: dir.to_path_buf(),
            data,
            pool,
            wal,
            control: Mutex::new(control),
            options: opts,
            recovery: Recovery::default(),
            catalogue: Mutex::new(Catalogue::default()),
            tables: RwLock::new(HashMap::new()),
            txns: RwLock::new(TxnRegistry::default()),
            instance,
        })
    }

    /// The control block of the instance, its lock taken.
    ///
    /// The allocator holds this guard from the moment it reads `next_page_id` or `free_head` to
    /// the moment `vauban.ctl` has been rewritten ([`alloc`]). A caller that also needs the
    /// buffer pool takes this lock first, which is the order [`alloc`] documents.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] "control block lock poisoned" for a lock a panicking
    /// thread left behind, as the buffer pool reports its own.
    pub(crate) fn lock_control(&self) -> Result<MutexGuard<'_, Control>, InternalError> {
        self.control
            .lock()
            .map_err(|_| InternalError::Corruption("control block lock poisoned".to_string()))
    }

    /// A copy of the control block as it stands, the lock released at the return.
    pub(crate) fn control(&self) -> Result<Control, InternalError> {
        Ok(*self.lock_control()?)
    }

    /// Makes durable what is committed: the pages of the transactions that have committed
    /// reach `data`, a [`wal::WalRecordKind::Checkpoint`] record reaches the journal, and its
    /// LSN reaches `vauban.ctl`, where the recovery reads it.
    ///
    /// # Steps
    ///
    /// 1. [`buffer::BufferPool::flush_all`] writes the dirty frames, skipping the ones a
    ///    running transaction holds (`in_progress`, no-steal): a version that has not
    ///    committed stays in the pool (`checkpoint_skips_in_progress`).
    /// 2. `data` is `sync_all`ed, so the pages just written are on the medium rather than in
    ///    the cache of the operating system. A `sync_all` is invisible from the process that
    ///    issues it, so what that call saves is not asserted here: showing it takes a test
    ///    that kills the process.
    /// 3. the checkpoint record is appended and the journal `sync_all`ed
    ///    ([`wal::WalHandle::append_checkpoint`]); its payload carries its own LSN and the two
    ///    counters of the control block.
    /// 4. the control block takes that LSN ([`control::Control::note_checkpoint`]) and is
    ///    rewritten and `sync_all`ed, the file first and the block in memory after it, as the
    ///    allocator does.
    ///
    /// The journal keeps the records it held: this build does not truncate it
    /// (`two_checkpoints_leave_two_records` reads the records of the two checkpoints and the
    /// ones written before them).
    ///
    /// A second call appends a second record and rewrites `vauban.ctl` with its LSN
    /// (`two_checkpoints_leave_two_records`); an instance nothing was written on is an
    /// ordinary case of the four steps (`checkpoint_on_empty_is_ok`). What the pool holds and
    /// what `vauban.ctl` carries are read under their own locks, the control block first and
    /// the journal under it, which keeps the order of [`alloc`] (control block, then pool,
    /// the pool taking the journal under its own lock).
    ///
    /// The call is `pub(crate)`: [`storage_impl`] answers [`crate::Storage::checkpoint`] from
    /// it, and adds nothing to it.
    ///
    /// # What the payload carries, and what it does not
    ///
    /// The payload is 24 bytes: the LSN of the record and the two counters of the control
    /// block (`wal::CHECKPOINT_PAYLOAD_LEN`,
    /// `wal::tests::a_checkpoint_payload_of_another_length_is_corruption`). The LSN of the
    /// oldest transaction still running is not among them: the register ([`TxnRegistry`])
    /// holds a status, a flag and a list of tables per transaction, not the LSN of the first
    /// record of each of them, so adding that field means keeping it at
    /// [`DiskStorage::begin_txn`] **and** changing the length of the payload, its layout and
    /// the test that freezes it. The payload is therefore left as it is, and a redo starts at
    /// the first record of the journal.
    ///
    /// # What the LSN of the record says, and what it does not
    ///
    /// It says where the checkpoint happened. It does not say by itself where a redo may
    /// start. A transaction that began before the record and committed after it has its row
    /// records **behind** that LSN while its page stayed in the pool: step 1 skipped the page,
    /// the transaction being in progress, and the commit that followed synced the journal
    /// rather than `data` ([`version::HeapTable::commit`]). One insert astride one checkpoint
    /// gives `Insert` at `Lsn(2)`, `Checkpoint` at `Lsn(3)`, `Commit` at `Lsn(4)`, and
    /// the page of that insert out of `data` after the commit
    /// (`a_transaction_astride_the_checkpoint_leaves_its_insert_before_redo_lsn`). Replaying
    /// the records after `Lsn(3)` alone would leave that row out; the records before the
    /// checkpoint are still in the journal, which a checkpoint does not truncate, and where a
    /// recovery starts its scan is fixed by [`recover`].
    ///
    /// # One instance per directory
    ///
    /// Two [`DiskStorage`] opened on one directory would hold a journal and a control block
    /// each and overwrite what the other wrote: the second `checkpoint` of such a pair would
    /// leave a journal of one record and a `vauban.ctl` naming it. The register of directories
    /// of the process refuses the second [`DiskStorage::open`] of one directory
    /// (`tests::a_second_open_of_the_same_directory_is_refused`). Two processes are not
    /// covered by that register.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] from the pool for a dirty page carrying an LSN the journal has
    /// not made durable; [`InternalError::Io`] for a failure reported by the operating system;
    /// [`InternalError::Corruption`] for a lock a panicking thread left poisoned. A failure of
    /// one of the first three steps leaves `vauban.ctl` naming the previous checkpoint.
    // Called by the tests of this module and of `version.rs`; its caller in the engine is
    // `storage_impl`, which answers `Storage::checkpoint` from it.
    #[allow(dead_code)]
    pub(crate) fn checkpoint(&self) -> Result<(), InternalError> {
        self.pool.flush_all()?;
        self.sync_data()?;
        let mut control = self.lock_control()?;
        let lsn = self
            .wal
            .append_checkpoint(control.next_page_id, control.next_row_id)?;
        let mut updated = *control;
        updated.note_checkpoint(lsn);
        updated.write(&self.control_path())?;
        *control = updated;
        Ok(())
    }

    /// `sync_all`s `data`, which is what puts the pages [`buffer::BufferPool::flush_all`]
    /// wrote on the medium.
    ///
    /// The call opens a second handle on `<dir>/data` instead of adding a method to
    /// [`file::DataFile`]; `sync_all` flushes the dirty bytes of the file itself, whichever
    /// descriptor the writes went through. A checkpoint being rare, the `open` it costs is
    /// paid once per call.
    fn sync_data(&self) -> Result<(), InternalError> {
        fs::OpenOptions::new()
            .write(true)
            .open(self.data.path())?
            .sync_all()?;
        Ok(())
    }

    /// Path of `vauban.ctl` in the directory of the instance.
    ///
    /// [`control::Control::write`] rewrites that file in full and `sync_all`s it, which is how
    /// the allocator persists the counters it moved.
    pub(crate) fn control_path(&self) -> PathBuf {
        self.dir.join(CONTROL_FILE_NAME)
    }

    /// Runs `vacuum(horizon)` on each table of each database the catalogue holds.
    ///
    /// The catalogue locks are taken and released for each table, so two passes do not deadlock.
    ///
    /// # Errors
    ///
    /// The errors of the tables and of the indexes.
    pub(crate) fn vacuum(&self, horizon: TxnId) -> SqlResult<()> {
        let catalogue = self.lock_catalogue()?;
        let dbs: Vec<DbId> = catalogue
            .databases()
            .into_iter()
            .map(|(db, _)| db)
            .collect();
        drop(catalogue);
        for db in dbs {
            let catalogue = self.lock_catalogue()?;
            let tables: Vec<TableId> = catalogue
                .tables_of(db)
                .into_iter()
                .map(|e| e.table)
                .collect();
            drop(catalogue);
            for table in tables {
                let changes: Vec<crate::disk::index::IndexChange> =
                    self.with_rows(table, |store| store.vacuum(horizon).map_err(SqlError::from))?;
                self.maintain_indexes(table, &changes)?;
            }
        }
        Ok(())
    }

    /// The catalogue of the instance, its lock taken.
    ///
    /// The DDL methods hold this guard from the moment they read the catalogue to the moment
    /// the journal record and the meta pages are written, which is what keeps two DDL
    /// statements from handing out one identifier twice or writing one chain of meta pages at
    /// the same time. The control block is taken **under** it, the DDL allocating pages
    /// through [`alloc::allocate`], which takes that block itself.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] "catalogue lock poisoned" for a lock a panicking thread
    /// left behind, as the buffer pool and the control block report their own.
    pub(crate) fn lock_catalogue(&self) -> Result<MutexGuard<'_, Catalogue>, InternalError> {
        self.catalogue
            .lock()
            .map_err(|_| InternalError::Corruption("catalogue lock poisoned".to_string()))
    }

    /// The cell holding the state of `table`, added to the map when this is the first call on
    /// that table.
    ///
    /// The lookup takes the read lock of the map; a call that finds no cell for that table
    /// takes the write lock to insert one. The cell is an [`Arc`] so that the caller may drop the lock of the map
    /// before it takes the one of the cell, which is what keeps a call on one table from
    /// blocking a call on another.
    pub(crate) fn table_state(&self, table: TableId) -> Arc<Mutex<TableState>> {
        // A poisoned lock still holds the map, and the map is what this needs; the panic is
        // reported by the test that caused it.
        if let Some(cell) = read_lock(&self.tables).get(&table) {
            return Arc::clone(cell);
        }
        Arc::clone(
            write_lock(&self.tables)
                .entry(table)
                .or_insert_with(|| Arc::new(Mutex::new(TableState::default()))),
        )
    }

    /// The status of `txn` as [`Snapshot::is_visible`] must see it: the registered status, or
    /// `Committed` for a transaction this instance has not seen (the rule of
    /// [`crate::MemoryStorage`]).
    ///
    /// The register is per instance: a transaction that wrote in two tables
    /// has one status, so a row of the second table is visible as soon as the `commit` of the
    /// first is registered (`tests::one_begin_one_end_per_txn_over_two_tables`).
    pub(crate) fn txn_status(&self, txn: TxnId) -> TxnStatus {
        read_lock(&self.txns)
            .txns
            .get(&txn)
            .map_or(TxnStatus::Committed, |record| record.status)
    }

    /// Refuses a transaction that already committed or rolled back, registering nothing.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] naming the status the register holds.
    pub(crate) fn check_writable(&self, txn: TxnId) -> Result<(), InternalError> {
        match read_lock(&self.txns).txns.get(&txn) {
            Some(record) if record.status != TxnStatus::InProgress => Err(InternalError::Bug(
                format!("transaction {txn} is already {:?}", record.status),
            )),
            _ => Ok(()),
        }
    }

    /// Registers the first write of `txn` in `table`, appending **one**
    /// [`WalRecordKind::Begin`] record per transaction and per instance.
    ///
    /// The journal discovers a transaction at its first write, not when the `txn` module hands
    /// out its identifier. A transaction that writes in a second table adds that
    /// table to its list and appends no second `Begin`
    /// (`tests::one_begin_one_end_per_txn_over_two_tables`).
    ///
    /// # Errors
    ///
    /// Those of [`wal::WalHandle::append`].
    pub(crate) fn begin_txn(&self, txn: TxnId, table: TableId) -> Result<(), InternalError> {
        let mut register = write_lock(&self.txns);
        let record = register.txns.entry(txn).or_default();
        record.tables.insert(table);
        if record.began {
            return Ok(());
        }
        self.wal.append(WalRecordKind::Begin, txn, &[])?;
        record.began = true;
        Ok(())
    }

    /// Ends `txn` with `kind`, appending **one** record per transaction and per instance, and
    /// answers the LSN of that record when one was appended.
    ///
    /// A transaction that wrote nothing has no `Begin` in the journal, so it gets no `Commit`
    /// and no `Abort` either (`version::tests::a_commit_without_a_write_appends_no_record`);
    /// its status is registered all the same, so that a second end is reported.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a transaction that already committed or rolled back
    /// (`version::tests::a_second_commit_is_a_bug`); the errors of
    /// [`wal::WalHandle::append_durable`] otherwise.
    pub(crate) fn end_txn(
        &self,
        txn: TxnId,
        kind: WalRecordKind,
    ) -> Result<Option<Lsn>, InternalError> {
        let mut register = write_lock(&self.txns);
        let record = register.txns.entry(txn).or_default();
        if record.status != TxnStatus::InProgress {
            return Err(InternalError::Bug(format!(
                "transaction {txn} is already {:?}",
                record.status
            )));
        }
        let lsn = if record.began {
            Some(self.wal.append_durable(kind, txn, &[])?)
        } else {
            None
        };
        record.status = match kind {
            WalRecordKind::Abort => TxnStatus::Aborted,
            _ => TxnStatus::Committed,
        };
        Ok(lsn)
    }

    /// The tables `txn` wrote in, by increasing identifier.
    pub(crate) fn txn_tables(&self, txn: TxnId) -> Vec<TableId> {
        read_lock(&self.txns)
            .txns
            .get(&txn)
            .map(|record| record.tables.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Registers the outcome the recovery of the `open` read in the journal: `winners`
    /// `Committed`, `losers` `Aborted`.
    ///
    /// A `loser` registered here is what hides a version of its that reached `data` through a
    /// page shared with a committing transaction
    /// (`recover::tests::a_loser_whose_page_reached_data_is_invisible`); a `winner` is what
    /// refuses a second write under an identifier the journal has already ended
    /// (`recover::tests::a_winner_is_registered_committed_and_may_not_write_again`).
    ///
    /// # Errors
    ///
    /// This build answers `Ok` here; the signature carries a `Result` because the callers are
    /// on the error path of the recovery.
    pub(crate) fn register_recovered(
        &self,
        winners: &[TxnId],
        losers: &[TxnId],
    ) -> Result<(), InternalError> {
        let mut register = write_lock(&self.txns);
        for (txns, status) in [
            (winners, TxnStatus::Committed),
            (losers, TxnStatus::Aborted),
        ] {
            for &txn in txns {
                register.txns.entry(txn).or_default().status = status;
            }
        }
        Ok(())
    }

    /// Appends a [`WalRecordKind::Savepoint`] record and records the write position of `txn`
    /// in each table it has written in, for a later [`DiskStorage::rollback_to`].
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a finished transaction; the errors of the journal.
    pub(crate) fn savepoint(&self, txn: TxnId) -> SqlResult<SavepointId> {
        let mut register = write_lock(&self.txns);
        if register
            .txns
            .get(&txn)
            .is_some_and(|r| r.status != TxnStatus::InProgress)
        {
            let status = register.txns[&txn].status;
            return Err(
                InternalError::Bug(format!("transaction {txn} is already {status:?}")).into(),
            );
        }
        let id = SavepointId(register.next_savepoint_id);
        let Some(next) = register.next_savepoint_id.checked_add(1) else {
            return Err(InternalError::Bug("savepoint id space exhausted".to_string()).into());
        };
        register.next_savepoint_id = next;
        let record = register.txns.entry(txn).or_default();
        record.began = true;
        let tables: Vec<TableId> = record.tables.iter().copied().collect();
        self.wal
            .append(WalRecordKind::Savepoint, txn, &id.0.to_le_bytes())?;
        // Record the current writes length per table. Tables are locked one by one, so this
        // is a best-effort snapshot: the caller must hold the per-table lock for the whole
        // savepoint + writes + rollback_to sequence.
        drop(register);
        let mut marks = HashMap::new();
        for table in tables {
            let mark = self.with_rows(table, |store| Ok(store.writes_len(txn)))?;
            marks.insert(table, mark);
        }
        let mut register = write_lock(&self.txns);
        let record = register.txns.get_mut(&txn).ok_or_else(|| {
            SqlError::from(InternalError::Bug(format!(
                "transaction {txn} vanished under the lock"
            )))
        })?;
        record.savepoints.push((id, marks));
        Ok(id)
    }

    /// Appends a [`WalRecordKind::RollbackTo`] record, flushes the journal, and rolls back
    /// the writes of `txn` after the savepoint `sp` in each table.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for an unknown savepoint or a finished transaction; the errors
    /// of the journal and of the undo path.
    pub(crate) fn rollback_to(&self, txn: TxnId, sp: SavepointId) -> SqlResult<()> {
        let (marks, pos) = {
            let register = read_lock(&self.txns);
            let Some(record) = register.txns.get(&txn) else {
                return Err(InternalError::Bug(format!(
                    "unknown transaction {txn} for savepoint {sp}"
                ))
                .into());
            };
            if record.status != TxnStatus::InProgress {
                return Err(InternalError::Bug(format!(
                    "transaction {txn} is already {:?}",
                    record.status
                ))
                .into());
            }
            let found = record
                .savepoints
                .iter()
                .enumerate()
                .find(|(_, (id, _))| *id == sp)
                .map(|(pos, (_, marks))| (marks.clone(), pos));
            let Some((marks, pos)) = found else {
                return Err(InternalError::Bug(format!(
                    "savepoint {sp} is unknown, invalidated or not owned by transaction {txn}"
                ))
                .into());
            };
            (marks, pos)
        };
        // Append the RollbackTo record with the savepoint id, before the undo. The recovery
        // reads this record to know which rows of the transaction the undo took away, and
        // leaves them out of the redo.
        self.wal
            .append_durable(WalRecordKind::RollbackTo, txn, &sp.0.to_le_bytes())?;
        // Rollback per table. Each table the transaction wrote in — including those that
        // appeared after the savepoint — undoes its writes back to the position the savepoint
        // named when it was taken. A table the savepoint did not name yet has its whole undo
        // log to replay, so its position is `0`
        // (`savepoint_invalidated_after_rollback_to`). The undo of a write pushes an [`IndexChange::Removed`] into the store's
        // maintenance log; that log is drained here and applied to the trees, as `commit`
        // and `rollback` do at the end of a transaction.
        for table in self.txn_tables(txn) {
            let mark = marks.get(&table).copied().unwrap_or(0);
            let changes = self.with_rows(table, |store| {
                store.rollback_to_savepoint(txn, mark)?;
                Ok(store.take_index_changes())
            })?;
            self.maintain_indexes(table, &changes)?;
        }
        // Truncate savepoints after this one.
        let mut register = write_lock(&self.txns);
        if let Some(record) = register.txns.get_mut(&txn) {
            record.savepoints.truncate(pos + 1);
        }
        Ok(())
    }
}

/// What one instance knows about one transaction.
#[derive(Debug)]
struct TxnRecord {
    /// `InProgress` from the first write until `commit` or `rollback`.
    status: TxnStatus,
    /// Whether the journal holds the [`WalRecordKind::Begin`] of this transaction.
    began: bool,
    /// The tables the transaction wrote in, which `commit` and `rollback` walk.
    tables: BTreeSet<TableId>,
    /// The savepoints of this transaction: each entry holds its id and the write position
    /// per table at the moment the savepoint was taken.
    savepoints: Vec<(SavepointId, HashMap<TableId, usize>)>,
}

impl Default for TxnRecord {
    /// A transaction discovered at its first write: in progress, no record yet, no table.
    fn default() -> Self {
        Self {
            status: TxnStatus::InProgress,
            began: false,
            tables: BTreeSet::new(),
            savepoints: Vec::new(),
        }
    }
}

/// The transactions of one instance, by [`TxnId`].
///
/// One register per instance rather than one per table: that is what makes a transaction
/// spanning two tables one `Begin` and one end record
/// (`tests::one_begin_one_end_per_txn_over_two_tables`).
#[derive(Debug, Default)]
pub(crate) struct TxnRegistry {
    /// What the instance knows about each transaction it has seen.
    txns: HashMap<TxnId, TxnRecord>,
    /// The next savepoint id to hand out.
    pub(crate) next_savepoint_id: u64,
}

/// The read guard of `lock`, taking the contents back from a thread that panicked while it
/// held the write guard.
///
/// The maps these locks hold are still the maps the instance needs; the panic itself is
/// reported by the test that caused it, as [`InstanceGuard`] does for its own set.
fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The write guard of `lock`, with the same rule as [`read_lock`].
fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The DDL of an on-disk instance: the nine methods of [`crate::Storage`] that create, drop
/// and list databases, tables and indexes.
///
/// They are inherent methods rather than an `impl Storage`: [`storage_impl`] holds that block
/// and calls these. Each of them is **immediate and not transactional**, as the trait
/// documents: no [`crate::TxnId`] is passed, and the record the journal takes carries
/// `TxnId(0)`.
///
/// # What one DDL statement does, in order
///
/// 1. the catalogue is locked and what the caller named is checked against it;
/// 2. the identifier is taken from the counter of `vauban.ctl`, which is rewritten and
///    `sync_all`ed there and then, so an identifier survives the statement that took it and
///    is not handed out again;
/// 3. the pages of the new object are allocated and formatted (a `create`), which the
///    allocator persists in `vauban.ctl` as well;
/// 4. the record reaches the journal and the journal is `sync_all`ed
///    ([`wal::WalHandle::append_durable`]): from that point the statement is durable, the
///    pages aside;
/// 5. the catalogue in memory takes the change, and the chain of meta pages is rewritten
///    through the buffer pool at the LSN of that record ([`meta::store`]) — so the page may
///    reach `data` at the next checkpoint or eviction, not before its record;
/// 6. a `drop` hands the pages of the object back to the allocator, after the journal record
///    and the meta pages, so a crash in between leaks pages instead of leaving the catalogue
///    naming a page the free list also holds.
///
/// A crash between steps 3 and 4 leaves the pages allocated and no object: the identifiers and
/// the pages are lost, nothing is named twice.
impl DiskStorage {
    /// Creates a database named `name` and answers its fresh identifier.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when the counter of `vauban.ctl` has passed [`u32::MAX`]; the
    /// errors of the journal, of the allocator and of the buffer pool otherwise.
    pub(crate) fn create_database(&self, name: &str) -> SqlResult<DbId> {
        let mut catalogue = self.lock_catalogue()?;
        let db = DbId(self.take_id(Counter::Database)?);
        let lsn = self.wal.append_durable(
            WalRecordKind::CreateDatabase,
            TxnId(0),
            &meta::create_database_payload(db, name),
        )?;
        catalogue.add_database(db, name);
        self.write_catalogue(&catalogue, lsn)?;
        Ok(db)
    }

    /// Drops the database `db`, its tables and their indexes, and hands their pages back.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when `db` is unknown; the errors of the journal, of the buffer
    /// pool and of the allocator otherwise.
    pub(crate) fn drop_database(&self, db: DbId) -> SqlResult<()> {
        let mut catalogue = self.lock_catalogue()?;
        if !catalogue.holds_database(db) {
            return Err(self.unknown("database", db.0).into());
        }
        let doomed: Vec<(TableEntry, Vec<PageId>)> = catalogue
            .tables_of(db)
            .into_iter()
            .map(|entry| (entry.clone(), catalogue.index_roots_of(entry.table)))
            .collect();
        let lsn = self.wal.append_durable(
            WalRecordKind::DropDatabase,
            TxnId(0),
            &meta::identifier_payload(db.0),
        )?;
        catalogue.remove_database(db);
        self.write_catalogue(&catalogue, lsn)?;
        for (entry, index_roots) in doomed {
            self.free_table_pages(&entry, &index_roots)?;
        }
        Ok(())
    }

    /// The databases of the instance, by increasing identifier.
    pub(crate) fn databases(&self) -> SqlResult<Vec<(DbId, String)>> {
        Ok(self.lock_catalogue()?.databases())
    }

    /// Creates a table of shape `shape` in `db` and answers its fresh identifier.
    ///
    /// A `shape` without a clustered key is a heap ([`version::HeapTable`]); a `shape` with one
    /// is a clustered table ([`clustered::ClusteredTable`]), whose two tree roots the
    /// catalogue keeps beside the head page of its overflow heap.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when `db` is unknown, when `shape.columns` is empty, when a
    /// clustered key is empty or names a column outside the shape, and when the counter of
    /// `vauban.ctl` has passed [`u32::MAX`]. [`SqlError`] 1946 for a clustered key whose
    /// declared width is past [`MAX_CLUSTERED_KEY_BYTES`]
    /// (`tests::clustered_key_over_900_bytes_is_1946`). The errors of the journal, of the
    /// allocator and of the buffer pool otherwise.
    pub(crate) fn create_table(&self, db: DbId, shape: &TableShape) -> SqlResult<TableId> {
        let mut catalogue = self.lock_catalogue()?;
        if !catalogue.holds_database(db) {
            return Err(self.unknown("database", db.0).into());
        }
        check_shape(shape)?;
        check_clustered_key_width(shape)?;
        let table = TableId(self.take_id(Counter::Table)?);
        let first_page = alloc::allocate(self)?;
        let (tree_root, directory_root) = if shape.clustered_key.is_some() {
            let clustered = ClusteredTable::create(self, table, first_page, shape.clone())?;
            (
                Some(clustered.tree_root()),
                Some(clustered.directory_root()),
            )
        } else {
            HeapTable::create(self, table, first_page, shape.clone())?;
            (None, None)
        };
        let entry = TableEntry {
            table,
            db,
            shape: shape.clone(),
            first_page,
            tree_root,
            directory_root,
            next_row_id: 1,
        };
        let lsn = self
            .wal
            .append_durable(WalRecordKind::CreateTable, TxnId(0), &entry.encode())?;
        catalogue.add_table(entry);
        self.write_catalogue(&catalogue, lsn)?;
        Ok(table)
    }

    /// Drops the table `table` and its indexes, and hands their pages back.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when `table` is unknown; the errors of the journal, of the
    /// buffer pool and of the allocator otherwise.
    pub(crate) fn drop_table(&self, table: TableId) -> SqlResult<()> {
        let mut catalogue = self.lock_catalogue()?;
        let Some(entry) = catalogue.table(table).cloned() else {
            return Err(self.unknown("table", table.0).into());
        };
        let index_roots = catalogue.index_roots_of(table);
        let lsn = self.wal.append_durable(
            WalRecordKind::DropTable,
            TxnId(0),
            &meta::identifier_payload(table.0),
        )?;
        catalogue.remove_table(table);
        self.write_catalogue(&catalogue, lsn)?;
        self.free_table_pages(&entry, &index_roots)?;
        Ok(())
    }

    /// The tables of `db` with their shape, by increasing identifier.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when `db` is unknown.
    pub(crate) fn tables(&self, db: DbId) -> SqlResult<Vec<(TableId, TableShape)>> {
        let catalogue = self.lock_catalogue()?;
        if !catalogue.holds_database(db) {
            return Err(self.unknown("database", db.0).into());
        }
        Ok(catalogue
            .tables_of(db)
            .into_iter()
            .map(|entry| (entry.table, entry.shape.clone()))
            .collect())
    }

    /// Creates the index `def` on `table` and answers its fresh identifier.
    ///
    /// # The index of a clustered key shares its tree
    ///
    /// An index whose `columns` are the `clustered_key` of its table — the same columns in the
    /// same order, each with the same `descending` — takes no page: the rows are already in
    /// that order in the tree of [`clustered::ClusteredTable`], and the entry the catalogue
    /// keeps carries no root of its own
    /// (`clustered_index_shares_the_tree` reads `next_page_id` back unchanged). Any other
    /// shape gets a B+tree of its own ([`index::DiskIndex::create`]), which indexes the
    /// versions already there and answers 2601 when a `unique` shape finds a duplicate.
    ///
    /// A `unique` shape that shares the tree is checked against the rows already there before
    /// it is created, as [`index::DiskIndex::create`] checks the ones of a heap: the scan of
    /// [`clustered::ClusteredTable`] follows the clustered key, so two live rows that share a
    /// key come out side by side and the second of them is 2601
    /// (`a_unique_index_on_the_clustered_key_refuses_a_duplicate`). `NULL` equals `NULL` here,
    /// the rule of [`IndexShape::unique`].
    ///
    /// # An index of another shape on a clustered table
    ///
    /// It gets a B+tree of its own, like an index of a heap, and is kept up to date by
    /// [`storage_impl`] at each write: the tree holds one entry per version, and `seek` reads
    /// the rows through it (`tests::index_seek_on_clustered_table`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when `table` is unknown, when `def.columns` is empty or names a
    /// column outside the table, when an entry of `def.included` does, and when the counter of
    /// `vauban.ctl` has passed [`u32::MAX`].
    /// `SqlError` 2601 when `def.unique` and two live rows share a key.
    pub(crate) fn create_index(&self, table: TableId, def: &IndexShape) -> SqlResult<IndexId> {
        let mut catalogue = self.lock_catalogue()?;
        let Some(entry) = catalogue.table(table).cloned() else {
            return Err(self.unknown("table", table.0).into());
        };
        check_index_shape(def, &entry.shape)?;
        let clustered = entry.shape.clustered_key.as_deref();
        let shares_the_tree = clustered == Some(def.columns.as_slice());
        let index = IndexId(self.take_id(Counter::Index)?);
        let root = if shares_the_tree {
            if def.unique {
                self.refuse_a_duplicate_clustered_key(&entry, index, def)?;
            }
            None
        } else {
            // The lock of the catalogue is held here, so the table is reached without
            // taking it again (`storage_impl::DiskStorage::with_rows_of`).
            self.with_rows_of(&entry, |store| {
                Ok(Some(
                    DiskIndex::create(self, index, def, store.source())?.root(),
                ))
            })?
            .0?
        };
        let entry = IndexEntry {
            index,
            table,
            shape: def.clone(),
            root,
        };
        let lsn = self
            .wal
            .append_durable(WalRecordKind::CreateIndex, TxnId(0), &entry.encode())?;
        catalogue.add_index(entry);
        self.write_catalogue(&catalogue, lsn)?;
        Ok(index)
    }

    /// Drops the index `index` and hands its pages back; the rows are untouched.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when `index` is unknown; the errors of the journal, of the
    /// buffer pool and of the allocator otherwise.
    pub(crate) fn drop_index(&self, index: IndexId) -> SqlResult<()> {
        let mut catalogue = self.lock_catalogue()?;
        let Some(entry) = catalogue.index(index).cloned() else {
            return Err(self.unknown("index", index.0).into());
        };
        let lsn = self.wal.append_durable(
            WalRecordKind::DropIndex,
            TxnId(0),
            &meta::identifier_payload(index.0),
        )?;
        catalogue.remove_index(index);
        self.write_catalogue(&catalogue, lsn)?;
        if let Some(root) = entry.root {
            index::free_tree(self, root)?;
        }
        Ok(())
    }

    /// The indexes of `table` with their shape, by increasing identifier.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when `table` is unknown.
    pub(crate) fn indexes(&self, table: TableId) -> SqlResult<Vec<(IndexId, IndexShape)>> {
        let catalogue = self.lock_catalogue()?;
        if catalogue.table(table).is_none() {
            return Err(self.unknown("table", table.0).into());
        }
        Ok(catalogue
            .indexes_of(table)
            .into_iter()
            .map(|entry| (entry.index, entry.shape.clone()))
            .collect())
    }

    /// Answers 2601 when two live rows of the clustered table `entry` already share a key.
    ///
    /// The table is attached as it stands and walked in the order of its tree
    /// ([`clustered::ClusteredTable::scan`]), under a snapshot that settles the transactions
    /// it meets: the table is opened here, so its register of transactions is empty and
    /// [`clustered::ClusteredTable::status`] answers `Committed` for each of them — the same
    /// state [`index::DiskIndex::create`] reads a heap in, so the two paths call the same rows
    /// live. Two rows that share a key are adjacent in that walk, so one comparison per step
    /// is enough.
    ///
    /// # Errors
    ///
    /// [`SqlError`] 2601 naming `index` and the key, as an index of a heap does;
    /// [`InternalError::Bug`] for a catalogue entry of a clustered table that carries no tree
    /// root; the errors of the buffer pool otherwise.
    fn refuse_a_duplicate_clustered_key(
        &self,
        entry: &TableEntry,
        index: IndexId,
        def: &IndexShape,
    ) -> SqlResult<()> {
        let (Some(tree), Some(directory)) = (entry.tree_root, entry.directory_root) else {
            return Err(InternalError::Bug(format!(
                "table {} carries a clustered key and no tree in the catalogue of instance {}",
                entry.table,
                self.dir.display()
            ))
            .into());
        };
        let table = ClusteredTable::open(
            self,
            entry.table,
            entry.first_page,
            (tree, directory),
            entry.shape.clone(),
        )?;
        let settled = Snapshot {
            xmin: TxnId(u64::MAX),
            xmax: TxnId(u64::MAX),
            active: Vec::new(),
            own: TxnId(0),
        };
        let mut previous: Option<Vec<Value>> = None;
        for (_, row) in table.scan(&settled)? {
            let key = key_of(&row, &def.columns);
            if let Some(before) = &previous
                && same_key(before, &key, &def.columns, &entry.shape)?
            {
                return Err(SqlError::duplicate_key_index(
                    &entry.table.to_string(),
                    &index.to_string(),
                    &index::key_text(&key),
                ));
            }
            previous = Some(key);
        }
        Ok(())
    }

    /// Takes the next identifier of `counter` from `vauban.ctl` and rewrites the file.
    ///
    /// The counter moves forward before the statement goes on, so an identifier a refused
    /// statement took is not handed out a second time, as the trait asks
    /// (`an_identifier_is_not_handed_out_twice_after_a_refused_statement`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when the counter has passed [`u32::MAX`], which is the width of
    /// the identifiers of the trait; the errors of [`control::Control::write`] otherwise.
    fn take_id(&self, counter: Counter) -> Result<u32, InternalError> {
        let mut control = self.lock_control()?;
        let mut updated = *control;
        let next = match counter {
            Counter::Database => &mut updated.next_db_id,
            Counter::Table => &mut updated.next_table_id,
            Counter::Index => &mut updated.next_index_id,
        };
        let taken = *next;
        let Ok(id) = u32::try_from(taken) else {
            return Err(InternalError::Bug(format!(
                "instance {} has handed out {taken} identifiers of {counter:?}, past the \
                 {} of the trait",
                self.dir.display(),
                u32::MAX
            )));
        };
        *next = taken + 1;
        updated.write(&self.control_path())?;
        *control = updated;
        Ok(id)
    }

    /// Writes the catalogue to its chain of meta pages, at the LSN of the record that has just
    /// been made durable.
    fn write_catalogue(&self, catalogue: &Catalogue, lsn: Lsn) -> Result<(), InternalError> {
        let root = meta::ensure_root(self)?;
        meta::store(self, root, catalogue, lsn)
    }

    /// Hands back the pages of a table: the trees of its indexes, its own trees when it is
    /// clustered, then its heap and the overflow pages that heap points at.
    fn free_table_pages(
        &self,
        entry: &TableEntry,
        index_roots: &[PageId],
    ) -> Result<(), InternalError> {
        for root in index_roots {
            index::free_tree(self, *root)?;
        }
        for root in [entry.tree_root, entry.directory_root]
            .into_iter()
            .flatten()
        {
            index::free_tree(self, root)?;
        }
        Heap::open(self, entry.table, entry.first_page).free_pages()
    }

    /// The error of an object the catalogue does not hold.
    fn unknown(&self, what: &str, id: u32) -> InternalError {
        InternalError::Bug(format!(
            "{what} {id} is unknown to instance {}",
            self.dir.display()
        ))
    }
}

/// Which counter of `vauban.ctl` [`DiskStorage::take_id`] moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counter {
    /// `next_db_id`.
    Database,
    /// `next_table_id`.
    Table,
    /// `next_index_id`.
    Index,
}

/// Refuses a table shape this build does not store.
fn check_shape(shape: &TableShape) -> Result<(), InternalError> {
    if shape.columns.is_empty() {
        return Err(InternalError::Bug(
            "a table shape carries no column".to_string(),
        ));
    }
    let Some(key) = &shape.clustered_key else {
        return Ok(());
    };
    if key.is_empty() {
        return Err(InternalError::Bug(
            "the clustered key of a table shape carries no column".to_string(),
        ));
    }
    outside(key.iter().map(|column| column.column), shape.columns.len()).map_or(Ok(()), |column| {
        Err(InternalError::Bug(format!(
            "the clustered key of a table shape names column {column}, outside its {} \
                 columns",
            shape.columns.len()
        )))
    })
}

/// Largest declared width, in bytes, this build takes for the clustered key of a table.
///
/// The ceiling of SQL Server for the key of a clustered index
/// ([Learn, "Maximum capacity specifications for SQL Server"]). This build has its own
/// reason to stop there: an `update` of a clustered table whose key is around 980 bytes
/// would answer [`InternalError::Bug`], the entry of the
/// B+tree being past `btree::MAX_INSERT_BYTES` once the version prefix, the link to the
/// previous version and the two tie columns ride with it. Refusing the table is what turns
/// that late `Bug` into an error at `CREATE TABLE`.
///
/// [Learn, "Maximum capacity specifications for SQL Server"]: https://learn.microsoft.com/sql/sql-server/maximum-capacity-specifications-for-sql-server
pub(crate) const MAX_CLUSTERED_KEY_BYTES: usize = 900;

/// Refuses a clustered key whose declared width is past [`MAX_CLUSTERED_KEY_BYTES`].
///
/// The width counted is the **declared** one, the storage size of each key column, not the
/// size of a value: that is what makes the refusal a property of the
/// shape rather than of the first row. A `varchar(max)` or an `nvarchar(max)` in a key has no
/// declared width and is refused for that reason (`tests::clustered_key_over_900_bytes_is_1946`
/// carries the fixed-width case, `tests::a_max_column_in_a_clustered_key_is_1946` the other).
///
/// # Errors
///
/// [`SqlError`] 1946, severity 16, state 3, naming the width the key asks for and the one this
/// build takes. The message is built here rather than taken from the catalogue of
/// `vauban-errors`, which carries no 1946 template.
fn check_clustered_key_width(shape: &TableShape) -> SqlResult<()> {
    let Some(key) = &shape.clustered_key else {
        return Ok(());
    };
    let mut width: usize = 0;
    for column in key {
        let Some(info) = shape.columns.get(usize::from(column.column)) else {
            continue;
        };
        match declared_width(&info.ty) {
            Some(bytes) => width = width.saturating_add(bytes),
            None => width = usize::MAX,
        }
    }
    if width <= MAX_CLUSTERED_KEY_BYTES {
        return Ok(());
    }
    let asked = if width == usize::MAX {
        "an unbounded number of".to_string()
    } else {
        width.to_string()
    };
    Err(SqlError::new(
        1946,
        16,
        3,
        format!(
            "Operation failed. The index entry of length {asked} bytes for the clustered key \
             exceeds the maximum length of {MAX_CLUSTERED_KEY_BYTES} bytes."
        ),
    ))
}

/// The declared width of a column of type `ty`, in bytes, `None` for a `max` type, whose
/// width is not declared.
///
/// The sizes are the storage sizes of each type (what the 900-byte rule of a key counts),
/// not what one value weighs in the encoding of [`encode`].
fn declared_width(ty: &SqlType) -> Option<usize> {
    let bounded = |len: &Len, per_character: usize| match len {
        Len::Fixed(count) => Some(usize::from(*count) * per_character),
        Len::Max => None,
    };
    match ty {
        SqlType::Bit | SqlType::TinyInt => Some(1),
        SqlType::SmallInt => Some(2),
        SqlType::Int | SqlType::Real | SqlType::SmallMoney | SqlType::SmallDateTime => Some(4),
        SqlType::BigInt | SqlType::Float | SqlType::Money | SqlType::DateTime => Some(8),
        SqlType::Date => Some(3),
        SqlType::Time(scale) => Some(fractional_width(*scale)),
        SqlType::DateTime2(scale) => Some(3 + fractional_width(*scale)),
        SqlType::DateTimeOffset(scale) => Some(5 + fractional_width(*scale)),
        SqlType::UniqueIdentifier => Some(16),
        SqlType::Decimal { precision, .. } | SqlType::Numeric { precision, .. } => {
            Some(match precision {
                0..=9 => 5,
                10..=19 => 9,
                20..=28 => 13,
                _ => 17,
            })
        }
        SqlType::Char(len) | SqlType::VarChar(len) => bounded(len, 1),
        SqlType::Binary(len) | SqlType::VarBinary(len) => bounded(len, 1),
        SqlType::NChar(len) | SqlType::NVarChar(len) => bounded(len, 2),
    }
}

/// The bytes the time part of a `time`, a `datetime2` or a `datetimeoffset` of that scale
/// takes: 3 up to scale 2, 4 up to scale 4, 5 beyond (Learn, "time (Transact-SQL)").
fn fractional_width(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

/// Refuses an index shape this build does not store.
fn check_index_shape(def: &IndexShape, shape: &TableShape) -> Result<(), InternalError> {
    if def.columns.is_empty() {
        return Err(InternalError::Bug(
            "an index shape carries no key column".to_string(),
        ));
    }
    let arity = shape.columns.len();
    let named = def
        .columns
        .iter()
        .map(|column| column.column)
        .chain(def.included.iter().copied());
    outside(named, arity).map_or(Ok(()), |column| {
        Err(InternalError::Bug(format!(
            "an index shape names column {column}, outside the {arity} columns of its table"
        )))
    })
}

/// The first column of `named` that is not a column of a table of `arity` columns.
fn outside(named: impl Iterator<Item = u16>, arity: usize) -> Option<u16> {
    named
        .into_iter()
        .find(|column| usize::from(*column) >= arity)
}

/// The values of `columns` in `row`, in key order.
///
/// A column outside the row is read as `NULL`: the shapes are checked by
/// [`check_index_shape`] before this is called, so that case is a row of another arity than
/// its table.
fn key_of(row: &crate::Row, columns: &[KeyColumn]) -> Vec<Value> {
    columns
        .iter()
        .map(|column| {
            row.0
                .get(usize::from(column.column))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect()
}

/// Whether two keys of the shape `columns` are the same key for a `unique` index.
///
/// `NULL` equals `NULL` column by column, which is what [`IndexShape::unique`] asks and what
/// the `=` predicate of SQL does not do. The collation of a column comes from its
/// [`vauban_types::TypeInfo`], or [`Collation::DEFAULT`] for a column that carries no
/// collation of its own.
fn same_key(
    a: &[Value],
    b: &[Value],
    columns: &[KeyColumn],
    shape: &TableShape,
) -> SqlResult<bool> {
    for (position, column) in columns.iter().enumerate() {
        let (Some(left), Some(right)) = (a.get(position), b.get(position)) else {
            return Ok(false);
        };
        if matches!(left, Value::Null) && matches!(right, Value::Null) {
            continue;
        }
        let collation = shape
            .columns
            .get(usize::from(column.column))
            .and_then(|info| info.collation)
            .unwrap_or(Collation::DEFAULT);
        if vauban_types::compare(left, right, &collation)? != Some(Ordering::Equal) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Keeps one directory to one [`DiskStorage`] for as long as that structure lives.
///
/// Two instances opened on one directory hold a control block and a journal each and overwrite
/// what the other wrote: a pair of checkpoints on such a pair leaves a journal of one record
/// and a `vauban.ctl` naming it. The second
/// [`DiskStorage::open`] of a directory is therefore refused
/// (`a_second_open_of_the_same_directory_is_refused`), and the directory is free again when
/// the first instance is dropped.
///
/// # What this guard covers
///
/// The set is a `static` of this process: what is refused is a second `open` **in the process
/// that holds the first**, which is the shape a test can observe. Two processes are not
/// caught — that would be a lock held by the operating system, and `flock` needs a dependency
/// this workspace does not carry. A process that dies takes its entries with it, which a
/// marker written in `vauban.ctl` or a lock file would not: after a `kill -9` the directory
/// opens again.
#[derive(Debug)]
struct InstanceGuard {
    /// The directory, as [`fs::canonicalize`] answers it, so that two spellings of one
    /// directory are one entry.
    key: PathBuf,
}

/// The directories this process holds an instance on.
static OPEN_INSTANCES: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

impl InstanceGuard {
    /// Takes `dir` for this process, creating the directory when it is missing.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] naming the directory when this process already holds an instance
    /// on it; [`InternalError::Io`] when the directory cannot be created or read.
    fn acquire(dir: &Path) -> Result<Self, InternalError> {
        fs::create_dir_all(dir)?;
        let key = fs::canonicalize(dir)?;
        // A lock a panicking thread poisoned still holds the set it was given, and the set is
        // what this guard needs; the panic is reported by the test that caused it.
        let mut open = OPEN_INSTANCES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !open.insert(key.clone()) {
            return Err(InternalError::Bug(format!(
                "directory {} is already open in this process; one instance holds it until it \
                 is dropped",
                key.display()
            )));
        }
        Ok(Self { key })
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let mut open = OPEN_INSTANCES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        open.remove(&self.key);
    }
}

/// Temporary directories for the tests of this module, removed when their guard is dropped.
///
/// The dependency list of the workspace carries no `tempfile` crate, so the tests build their
/// own guard over
/// [`std::env::temp_dir`].
#[cfg(test)]
pub(crate) mod temp {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Distinguishes two directories asked for in the same nanosecond by the same process.
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// A path under [`std::env::temp_dir`], removed with its contents on drop.
    #[derive(Debug)]
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        /// A unique path, not created on disk: for the tests that ask `open` to create it.
        pub(crate) fn reserved(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0);
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "vauban-{label}-{pid}-{nanos}-{sequence}",
                pid = std::process::id()
            );
            Self(std::env::temp_dir().join(name))
        }

        /// A unique path, created on disk and empty.
        pub(crate) fn created(label: &str) -> Self {
            let dir = Self::reserved(label);
            std::fs::create_dir_all(dir.path()).expect("create a temporary directory");
            dir
        }

        /// The path itself.
        pub(crate) fn path(&self) -> &Path {
            &self.0
        }

        /// The path of an entry inside the directory.
        pub(crate) fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // A test that failed before creating the directory leaves nothing to remove.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use vauban_types::{SqlType, TypeInfo, Value};

    use super::encode::encode_row;
    use super::page::{Page, PageKind};
    use super::temp::TempDir;
    use super::version::{HeapTable, VersionHeader};
    use super::wal::{CheckpointPayload, WalRecord, WalRecordKind};
    use super::*;
    use crate::{
        Direction, KeyColumn, KeyRange, Row, RowId, Storage, TableId, TableShape, TxnId, TxnStatus,
    };

    /// Size in bytes of a file of the instance.
    fn file_size(path: &Path) -> u64 {
        fs::metadata(path)
            .unwrap_or_else(|err| panic!("{} should exist: {err}", path.display()))
            .len()
    }

    /// The control block of the instance, copied out of its lock.
    fn control_of(storage: &DiskStorage) -> Control {
        storage.control().expect("read the control block")
    }

    #[test]
    fn open_creates_ctl_data_wal() {
        let dir = TempDir::created("open-creates");
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");

        let control_path = dir.child(CONTROL_FILE_NAME);
        let data_path = dir.child(DATA_FILE_NAME);
        let wal_path = dir.child(WAL_FILE_NAME);
        assert!(control_path.is_file(), "vauban.ctl");
        assert!(data_path.is_file(), "data");
        assert!(wal_path.is_file(), "wal");

        let control_bytes = fs::read(&control_path).expect("read vauban.ctl");
        assert_eq!(control_bytes.len(), 128);
        assert_eq!(&control_bytes[0..8], b"VAUBANDB");
        assert_eq!(
            u32::from_le_bytes([
                control_bytes[8],
                control_bytes[9],
                control_bytes[10],
                control_bytes[11]
            ]),
            2,
            "format_version"
        );
        assert_eq!(file_size(&data_path), 0, "data holds no page at creation");
        assert_eq!(file_size(&wal_path), 0, "wal is empty at creation");

        assert_eq!(storage.dir, dir.path());
        assert_eq!(storage.data.path(), data_path);
        assert_eq!(storage.options, DiskOptions::default());
        assert_eq!(storage.data.page_count().expect("page count"), 0);
        assert_eq!(control_of(&storage), Control::default());
    }

    #[test]
    fn open_creates_a_missing_directory() {
        let dir = TempDir::reserved("open-mkdir");
        let nested = dir.child("a").join("b");
        assert!(!nested.exists());
        let storage =
            DiskStorage::open(&nested, DiskOptions::default()).expect("create an instance");
        assert!(nested.is_dir());
        assert!(nested.join(CONTROL_FILE_NAME).is_file());
        assert_eq!(storage.dir, nested);
    }

    #[test]
    fn open_is_idempotent_on_empty() {
        let dir = TempDir::created("open-twice");
        let first = DiskStorage::open(dir.path(), DiskOptions::default()).expect("create");
        let control = control_of(&first);
        drop(first);

        let second = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        assert_eq!(control_of(&second), control);
        assert_eq!(control_of(&second), Control::default());
        assert_eq!(second.data.page_count().expect("page count"), 0);
        assert_eq!(file_size(&dir.child(WAL_FILE_NAME)), 0);
        assert_eq!(file_size(&dir.child(CONTROL_FILE_NAME)), 128);
        drop(second);

        let third = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen again");
        assert_eq!(control_of(&third), control);
    }

    /// The counters of `vauban.ctl` come back through a close and a reopen.
    ///
    /// The test above cannot show it: a fresh block holds `0`, `1` and `u64::MAX`, the values
    /// [`Control::default`] carries, so a `reopen` that threw away the file it read would
    /// answer the same. Here the control file of an instance is replaced by one whose eight
    /// counters are distinct and none of them is the default, and `open` is asked for them.
    ///
    /// Handing [`Control::default`] to the structure built by [`DiskStorage::reopen`] instead
    /// of the block it read turns this test red.
    #[test]
    fn open_returns_the_counters_of_the_ctl_it_read() {
        use super::page::Lsn;

        let dir = TempDir::created("counters-survive");
        drop(DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance"));

        let moved = Control {
            next_page_id: PageId(7),
            free_head: Some(PageId(9)),
            next_db_id: 12,
            next_table_id: 34,
            next_index_id: 56,
            next_row_id: 78,
            latest_checkpoint_lsn: Lsn(90),
            durable_lsn: Lsn(91),
            meta_root: None,
        };
        assert_ne!(moved, Control::default());
        moved
            .write(&dir.child(CONTROL_FILE_NAME))
            .expect("write the counters of a used instance");

        let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let read_back = control_of(&reopened);
        assert_eq!(read_back, moved);
        // Field by field as well, so that a failure names the counter that was lost.
        assert_eq!(read_back.next_page_id, PageId(7));
        assert_eq!(read_back.free_head, Some(PageId(9)));
        assert_eq!(read_back.next_db_id, 12);
        assert_eq!(read_back.next_table_id, 34);
        assert_eq!(read_back.next_index_id, 56);
        assert_eq!(read_back.next_row_id, 78);
        assert_eq!(read_back.latest_checkpoint_lsn, Lsn(90));
        assert_eq!(read_back.durable_lsn, Lsn(91));
    }

    #[test]
    fn open_rejects_unknown_version() {
        let dir = TempDir::created("bad-version");
        // The three files are there and the ctl is sealed, so the version is what `open`
        // refuses here.
        let mut bytes = Control::default().to_bytes();
        bytes[8..12].copy_from_slice(&3u32.to_le_bytes());
        control::seal(&mut bytes);
        fs::write(dir.child(CONTROL_FILE_NAME), bytes).expect("write a ctl of version 3");
        fs::write(dir.child(DATA_FILE_NAME), []).expect("write an empty data file");
        fs::write(dir.child(WAL_FILE_NAME), []).expect("write an empty wal");

        let err = DiskStorage::open(dir.path(), DiskOptions::default())
            .expect_err("version 3 is not read by this build");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("format version 3"), "{err}");
    }

    #[test]
    fn read_page_past_end_is_corruption() {
        let dir = TempDir::created("read-past-end");
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        assert_eq!(storage.data.page_count().expect("page count"), 0);
        let err = storage
            .data
            .read_page(PageId(0))
            .expect_err("a fresh instance holds no page");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("page 0"), "{err}");
    }

    #[test]
    fn tls_dir_does_not_block_create() {
        let dir = TempDir::created("with-tls");
        let tls = dir.child("tls");
        fs::create_dir_all(&tls).expect("create tls/");
        fs::write(tls.join("server.pem"), b"not a certificate").expect("write in tls/");

        let storage = DiskStorage::open(dir.path(), DiskOptions::default())
            .expect("tls/ is not an instance, the instance is created");
        assert!(dir.child(CONTROL_FILE_NAME).is_file());
        assert_eq!(control_of(&storage), Control::default());
        // The directory that was already there is left as it was.
        assert_eq!(
            fs::read(tls.join("server.pem")).expect("read the file back"),
            b"not a certificate"
        );
    }

    #[test]
    fn open_rejects_an_instance_whose_data_file_is_missing() {
        let dir = TempDir::created("no-data");
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        let data_path = dir.child(DATA_FILE_NAME);
        fs::remove_file(&data_path).expect("remove the data file");

        let err = DiskStorage::open(dir.path(), DiskOptions::default())
            .expect_err("an instance without its data file");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(
            err.to_string().contains(&data_path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn open_rejects_an_instance_whose_wal_is_missing() {
        let dir = TempDir::created("no-wal");
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        let wal_path = dir.child(WAL_FILE_NAME);
        fs::remove_file(&wal_path).expect("remove the wal");

        let err = DiskStorage::open(dir.path(), DiskOptions::default())
            .expect_err("an instance without its wal");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(
            err.to_string().contains(&wal_path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn open_rejects_data_without_a_control_file() {
        let dir = TempDir::created("no-ctl");
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        fs::remove_file(dir.child(CONTROL_FILE_NAME)).expect("remove the ctl");

        let err = DiskStorage::open(dir.path(), DiskOptions::default())
            .expect_err("data and wal without a ctl is not an empty directory");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains(CONTROL_FILE_NAME), "{err}");
    }

    #[test]
    fn a_page_written_through_the_instance_survives_a_reopen() {
        use super::page::{Lsn, Page, PageKind};

        let dir = TempDir::created("survives-reopen");
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        // The allocator owns the growth of the file; the test extends it by hand.
        storage.data.extend_to(1).expect("extend to one page");
        let mut page = Page::empty(PageKind::Heap, PageId(0));
        page.set_lsn(Lsn(5));
        page.seal();
        storage
            .data
            .write_page(PageId(0), &page)
            .expect("write page 0");
        drop(storage);

        let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        assert_eq!(reopened.data.page_count().expect("page count"), 1);
        let read = reopened.data.read_page(PageId(0)).expect("read page 0");
        assert_eq!(read, page);
        assert_eq!(read.lsn(), Lsn(5));
        // The control file was not rewritten by the page write.
        assert_eq!(control_of(&reopened), Control::default());
    }

    #[test]
    fn open_builds_a_pool_of_the_option_size_over_the_data_file() {
        use super::page::{Lsn, Page, PageKind};

        let dir = TempDir::created("pool-of-open");
        let storage = DiskStorage::open(dir.path(), DiskOptions { buffer_pages: 2 })
            .expect("create an instance");
        assert_eq!(storage.pool.capacity(), 2);

        // The allocator owns the growth of the file; the test extends it by hand.
        storage.data.extend_to(1).expect("extend to one page");
        let mut page = Page::empty(PageKind::Heap, PageId(0));
        page.set_lsn(Lsn(0));
        page.seal();
        storage
            .data
            .write_page(PageId(0), &page)
            .expect("write page 0");

        let pin = storage.pool.pin(PageId(0)).expect("pin page 0");
        assert_eq!(
            pin.with_page(|cached| cached.clone())
                .expect("read the cached page"),
            page,
            "the pool of the instance reads through its data file"
        );
    }

    #[test]
    fn open_rejects_a_pool_of_zero_frames_before_creating_anything() {
        let dir = TempDir::created("zero-frames");
        let err = DiskStorage::open(dir.path(), DiskOptions { buffer_pages: 0 })
            .expect_err("a pool of 0 frames is refused");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("0 frames"), "{err}");
        assert!(
            !dir.child(CONTROL_FILE_NAME).exists(),
            "the option is checked before the instance is created"
        );
        assert!(!dir.child(DATA_FILE_NAME).exists());
        assert!(!dir.child(WAL_FILE_NAME).exists());

        // The same directory opens with a usable option.
        DiskStorage::open(dir.path(), DiskOptions { buffer_pages: 1 }).expect("create an instance");
        // And an existing instance is refused the same way.
        let err = DiskStorage::open(dir.path(), DiskOptions { buffer_pages: 0 })
            .expect_err("reopening with 0 frames is refused too");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    }

    #[test]
    fn disk_options_default_holds_1024_pages() {
        assert_eq!(DiskOptions::default().buffer_pages, 1024);
        assert_eq!(
            DiskOptions { buffer_pages: 2 },
            DiskOptions { buffer_pages: 2 }
        );
    }

    /// An empty instance in a temporary directory, with the guard that removes it.
    fn empty_instance(label: &str) -> (TempDir, DiskStorage) {
        let dir = TempDir::created(label);
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        (dir, storage)
    }

    /// The kind of each record of the journal, in order.
    fn kinds(records: &[WalRecord]) -> Vec<WalRecordKind> {
        records.iter().map(|record| record.kind).collect()
    }

    #[test]
    fn checkpoint_on_empty_is_ok() {
        let (dir, storage) = empty_instance("checkpoint-empty");

        storage
            .checkpoint()
            .expect("checkpoint an instance nothing was written on");

        let records = storage.wal.records().expect("read the journal");
        assert_eq!(kinds(&records), vec![WalRecordKind::Checkpoint]);
        assert_eq!(records[0].lsn, Lsn(1));
        assert_eq!(
            CheckpointPayload::decode(&records[0].payload).expect("decode the payload"),
            CheckpointPayload {
                redo_lsn: Lsn(1),
                next_page_id: PageId(0),
                next_row_id: 1,
            },
            "the payload copies the counters of a fresh control block"
        );
        let control = control_of(&storage);
        assert_eq!(control.latest_checkpoint_lsn, Lsn(1));
        assert_eq!(control.durable_lsn, Lsn(1));
        assert_eq!(
            Control::read(&dir.child(CONTROL_FILE_NAME)).expect("read vauban.ctl"),
            control
        );
        assert_eq!(
            file_size(&dir.child(DATA_FILE_NAME)),
            0,
            "data holds 0 pages, the instance having been written on by nothing"
        );
    }

    #[test]
    fn two_checkpoints_leave_two_records() {
        let (dir, storage) = empty_instance("checkpoint-twice");

        storage.checkpoint().expect("first checkpoint");
        storage.checkpoint().expect("second checkpoint");

        let records = storage.wal.records().expect("read the journal");
        assert_eq!(
            kinds(&records),
            vec![WalRecordKind::Checkpoint, WalRecordKind::Checkpoint],
            "the journal keeps the record of the first checkpoint"
        );
        for (record, lsn) in records.iter().zip([Lsn(1), Lsn(2)]) {
            assert_eq!(record.lsn, lsn);
            assert_eq!(
                CheckpointPayload::decode(&record.payload)
                    .expect("decode the payload")
                    .redo_lsn,
                lsn,
                "each of the two payloads names its own record"
            );
        }
        let control = Control::read(&dir.child(CONTROL_FILE_NAME)).expect("read vauban.ctl");
        assert_eq!(control.latest_checkpoint_lsn, Lsn(2));
        assert_eq!(control.durable_lsn, Lsn(2));
        assert_eq!(control, control_of(&storage));
    }

    // --- Checkpoint ---------------------------------------------------------------------

    /// The table the checkpoint tests write on.
    const TABLE: TableId = TableId(9);

    /// A table of one nullable `int` column over a heap page of its own.
    fn int_table(storage: &DiskStorage) -> HeapTable<'_> {
        let head = alloc::allocate(storage).expect("allocate the head page");
        let shape = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, true)],
            clustered_key: None,
        };
        HeapTable::create(storage, TABLE, head, shape).expect("create the table")
    }

    /// A row of one `int`.
    fn row(value: i32) -> Row {
        Row(vec![Value::I32(value)])
    }

    /// The bytes the first version of the first row of a table occupies on the heap: the
    /// 32-byte prefix of [`version::VersionHeader`] then the columns.
    fn first_version_of(txn: u64, value: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        VersionHeader {
            row: RowId(1),
            xmin: TxnId(txn),
            xmax: None,
            seq: 1,
        }
        .write_to(&mut bytes);
        encode_row(&row(value), &mut bytes);
        bytes
    }

    /// Whether the 8 192 bytes of `page` carry `needle`.
    fn page_carries(page: &Page, needle: &[u8]) -> bool {
        page.0.windows(needle.len()).any(|window| window == needle)
    }

    /// The page of `data`, read behind the pool.
    fn page_on_data(storage: &DiskStorage, id: PageId) -> Page {
        storage.data.read_page(id).expect("read a page of `data`")
    }

    #[test]
    fn checkpoint_flushes_committed_heap_page() {
        let (_dir, storage) = empty_instance("checkpoint-flush");
        let mut table = int_table(&storage);
        let page = table.heap().first_page();
        table.insert(TxnId(4), &row(7)).expect("insert");
        table.commit(TxnId(4)).expect("commit");
        let version = first_version_of(4, 7);

        // The pool was not flushed by hand: the commit synced the journal, not `data`.
        assert!(
            !page_carries(&page_on_data(&storage, page), &version),
            "the committed row reached `data` before the checkpoint"
        );

        storage.checkpoint().expect("checkpoint");

        assert!(
            page_carries(&page_on_data(&storage, page), &version),
            "the committed row is missing from `data` after the checkpoint"
        );
    }

    #[test]
    fn checkpoint_skips_in_progress() {
        let (_dir, storage) = empty_instance("checkpoint-in-progress");
        let mut running = int_table(&storage);
        let mut committed = int_table(&storage);
        let hidden_page = running.heap().first_page();
        let seen_page = committed.heap().first_page();
        assert_ne!(
            hidden_page, seen_page,
            "the two tables were given heap pages of their own"
        );
        running
            .insert(TxnId(4), &row(41))
            .expect("insert of the running transaction");
        committed
            .insert(TxnId(5), &row(52))
            .expect("insert of the committing transaction");
        committed.commit(TxnId(5)).expect("commit");

        storage.checkpoint().expect("checkpoint");

        assert!(
            page_carries(&page_on_data(&storage, seen_page), &first_version_of(5, 52)),
            "the row of the committed transaction is missing from `data`"
        );
        assert!(
            !page_carries(
                &page_on_data(&storage, hidden_page),
                &first_version_of(4, 41)
            ),
            "the row of the running transaction reached `data`"
        );
        // Transaction 4 is still in progress: its page waits in the pool, flag set.
        assert_eq!(running.status(TxnId(4)), TxnStatus::InProgress);
    }

    #[test]
    fn checkpoint_record_in_wal_and_ctl() {
        let (dir, storage) = empty_instance("checkpoint-record");
        let mut table = int_table(&storage);
        table.insert(TxnId(4), &row(1)).expect("insert");
        table.commit(TxnId(4)).expect("commit");

        storage.checkpoint().expect("checkpoint");

        let records = storage.wal.records().expect("read the journal");
        assert_eq!(
            kinds(&records),
            vec![
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Commit,
                WalRecordKind::Checkpoint,
            ]
        );
        let record = records.last().expect("the checkpoint record");
        assert_eq!(record.lsn, Lsn(4));
        assert_eq!(record.txn, TxnId(0));
        let payload = CheckpointPayload::decode(&record.payload).expect("decode the payload");
        assert_eq!(
            payload.redo_lsn, record.lsn,
            "the payload names the record itself"
        );

        let control = control_of(&storage);
        assert_eq!(control.latest_checkpoint_lsn, record.lsn);
        assert_eq!(control.durable_lsn, record.lsn);
        assert_eq!(payload.next_page_id, control.next_page_id);
        assert_eq!(payload.next_row_id, control.next_row_id);
        // The head page of the table was handed out by the allocator, so the counter moved.
        assert_eq!(control.next_page_id, PageId(1));
        // The file read on its own by `control.rs` says the same.
        let reread = Control::read(&dir.child(CONTROL_FILE_NAME)).expect("read vauban.ctl");
        assert_eq!(reread, control);
        assert_eq!(reread.latest_checkpoint_lsn, Lsn(4));
    }

    #[test]
    fn a_transaction_astride_the_checkpoint_leaves_its_insert_before_redo_lsn() {
        let (_dir, storage) = empty_instance("checkpoint-astride");
        let mut table = int_table(&storage);
        let page = table.heap().first_page();
        table.insert(TxnId(4), &row(7)).expect("insert");

        storage.checkpoint().expect("checkpoint while 4 runs");
        table.commit(TxnId(4)).expect("commit after the checkpoint");

        let records = storage.wal.records().expect("read the journal");
        assert_eq!(
            kinds(&records),
            vec![
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Checkpoint,
                WalRecordKind::Commit,
            ]
        );
        let redo = CheckpointPayload::decode(&records[2].payload)
            .expect("decode the payload")
            .redo_lsn;
        assert_eq!(
            (records[1].lsn, redo, records[3].lsn),
            (Lsn(2), Lsn(3), Lsn(4)),
            "the insert is behind the checkpoint record, the commit ahead of it"
        );
        // The write was in progress when the checkpoint flushed the pool, and the commit that
        // follows syncs the journal, not `data`: the page of the row stayed in the pool.
        assert!(
            !page_carries(&page_on_data(&storage, page), &first_version_of(4, 7)),
            "the row of the transaction astride the checkpoint reached `data`"
        );
        // A redo of the records after `redo_lsn` would replay the `Commit` alone, whose
        // payload is empty: where a recovery starts is decided by `recover`, and this LSN
        // does not answer it on its own.
        assert!(records[3].payload.is_empty());
    }

    // ------------------------------------------------------------------- DDL

    /// The instance of `dir`, created or reopened.
    fn reopen(dir: &TempDir) -> DiskStorage {
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("open the instance")
    }

    /// A table of two nullable `int`, the first of them clustered when `clustered` is true.
    fn two_ints(clustered: bool) -> TableShape {
        TableShape {
            columns: vec![
                TypeInfo::new(SqlType::Int, true),
                TypeInfo::new(SqlType::Int, true),
            ],
            clustered_key: clustered.then(|| {
                vec![KeyColumn {
                    column: 0,
                    descending: false,
                }]
            }),
        }
    }

    /// A unique index on the column `column`, ascending, with no included column.
    fn unique_on(column: u16) -> IndexShape {
        IndexShape {
            columns: vec![KeyColumn {
                column,
                descending: false,
            }],
            unique: true,
            included: Vec::new(),
        }
    }

    /// Creates one database, one table of [`two_ints`] and one index on its first column.
    fn create_the_three(storage: &DiskStorage, clustered: bool) -> (DbId, TableId, IndexId) {
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(clustered))
            .expect("create the table");
        let index = storage
            .create_index(table, &unique_on(0))
            .expect("create the index");
        (db, table, index)
    }

    /// The three lists of the catalogue, as the reopen tests assert them.
    type Listed = (
        Vec<(DbId, String)>,
        Vec<(TableId, TableShape)>,
        Vec<(IndexId, IndexShape)>,
    );

    /// What the three lists of the catalogue answer, for the assertions of the reopen tests.
    fn catalogue_of(storage: &DiskStorage, db: DbId, table: TableId) -> Listed {
        (
            storage.databases().expect("databases"),
            storage.tables(db).expect("tables"),
            storage.indexes(table).expect("indexes"),
        )
    }

    #[test]
    fn ddl_survives_reopen() {
        let dir = TempDir::created("ddl-reopen");
        let (db, table, index) = {
            let storage = reopen(&dir);
            let ids = create_the_three(&storage, false);
            storage.checkpoint().expect("checkpoint");
            ids
        };

        let storage = reopen(&dir);
        assert_eq!((db, table, index), (DbId(1), TableId(1), IndexId(1)));
        assert_eq!(
            catalogue_of(&storage, db, table),
            (
                vec![(db, "a".to_string())],
                vec![(table, two_ints(false))],
                vec![(index, unique_on(0))],
            )
        );
        // The identifiers are not handed out a second time.
        let (second_db, second_table, second_index) = create_the_three(&storage, false);
        assert_eq!(
            (second_db, second_table, second_index),
            (DbId(2), TableId(2), IndexId(2))
        );
    }

    #[test]
    fn ddl_survives_reopen_without_checkpoint() {
        let dir = TempDir::created("ddl-reopen-no-checkpoint");
        let (db, table, index, root) = {
            let storage = reopen(&dir);
            let (db, table, index) = create_the_three(&storage, false);
            let root = control_of(&storage)
                .meta_root
                .expect("the root of the chain");
            // No checkpoint and no flush: the page of the catalogue is still in the pool, and
            // `data` holds the page the allocator wrote there.
            assert_eq!(
                page_on_data(&storage, root).kind().expect("the kind"),
                PageKind::Free,
                "the catalogue has not reached `data`"
            );
            (db, table, index, root)
        };

        let storage = reopen(&dir);
        assert_eq!(control_of(&storage).meta_root, Some(root));
        assert_eq!(
            catalogue_of(&storage, db, table),
            (
                vec![(db, "a".to_string())],
                vec![(table, two_ints(false))],
                vec![(index, unique_on(0))],
            ),
            "the DDL records of the journal rebuild the catalogue"
        );
    }

    #[test]
    fn a_checkpointed_catalogue_reads_back_with_an_emptied_journal() {
        let dir = TempDir::created("ddl-pages-alone");
        let (db, table, index) = {
            let storage = reopen(&dir);
            let ids = create_the_three(&storage, false);
            storage.checkpoint().expect("checkpoint");
            ids
        };
        // The counterpart of `ddl_survives_reopen_without_checkpoint`: with the journal gone,
        // what answers is the chain of meta pages the checkpoint wrote. Emptying the file
        // rather than removing it, an instance without a journal being corruption.
        fs::File::create(dir.child(WAL_FILE_NAME)).expect("empty the journal");

        let storage = reopen(&dir);
        assert_eq!(storage.wal.records().expect("records").len(), 0);
        assert_eq!(
            catalogue_of(&storage, db, table),
            (
                vec![(db, "a".to_string())],
                vec![(table, two_ints(false))],
                vec![(index, unique_on(0))],
            )
        );
    }

    #[test]
    fn a_root_whose_formatting_was_lost_is_written_again() {
        let dir = TempDir::created("ddl-root-lost");
        let (db, first) = {
            let storage = reopen(&dir);
            let db = storage.create_database("a").expect("create the database");
            let first = storage
                .create_table(db, &two_ints(false))
                .expect("create the table");
            // Dropped without a checkpoint: `data` holds the page the allocator wrote where
            // the root of the chain is, and the catalogue that reaches the reopen comes from
            // the journal.
            (db, first)
        };

        let second = {
            let storage = reopen(&dir);
            let root = control_of(&storage).meta_root.expect("the root");
            assert_eq!(
                page_on_data(&storage, root).kind().expect("the kind"),
                PageKind::Free
            );
            let second = storage
                .create_table(db, &two_ints(true))
                .expect("create a second table");
            storage.checkpoint().expect("checkpoint");
            assert_eq!(
                page_on_data(&storage, root).kind().expect("the kind"),
                PageKind::Meta,
                "the DDL wrote the root again"
            );
            second
        };

        // With the journal emptied, the chain of meta pages answers for the two tables.
        fs::File::create(dir.child(WAL_FILE_NAME)).expect("empty the journal");
        let storage = reopen(&dir);
        assert_eq!(
            storage.tables(db).expect("tables"),
            vec![(first, two_ints(false)), (second, two_ints(true))]
        );
    }

    #[test]
    fn a_ctl_behind_the_journal_takes_the_identifiers_of_the_catalogue() {
        let dir = TempDir::created("ddl-ctl-behind");
        let (db, table) = {
            let storage = reopen(&dir);
            let db = storage.create_database("a").expect("create the database");
            let table = storage
                .create_table(db, &two_ints(false))
                .expect("create the table");
            storage.checkpoint().expect("checkpoint");
            (db, table)
        };
        // A `vauban.ctl` whose three object counters are the ones of a fresh instance, its
        // page counter and its root left alone: the order of a DDL statement rules this out,
        // the file being rewritten before the record is appended, so this is what the reopen
        // reads when that file was lost.
        let path = dir.child(CONTROL_FILE_NAME);
        let mut control = Control::read(&path).expect("read the control block");
        control.next_db_id = 1;
        control.next_table_id = 1;
        control.next_index_id = 1;
        control.write(&path).expect("write the control block");

        let storage = reopen(&dir);
        let read_back = control_of(&storage);
        assert_eq!(
            (
                read_back.next_db_id,
                read_back.next_table_id,
                read_back.next_index_id
            ),
            (2, 2, 1),
            "the counters take the largest identifier of the catalogue, plus one"
        );
        assert_eq!(
            storage.create_database("b").expect("create a database"),
            DbId(2)
        );
        assert_eq!(
            storage
                .create_table(db, &two_ints(false))
                .expect("create a table"),
            TableId(2)
        );
        assert_eq!(
            storage.tables(db).expect("tables").first().map(|t| t.0),
            Some(table)
        );
    }

    #[test]
    fn drop_table_frees_its_pages() {
        let (dir, storage) = empty_instance("ddl-drop-table");
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(false))
            .expect("create the table");
        let entry_page = storage
            .lock_catalogue()
            .expect("the catalogue")
            .table(table)
            .expect("the table")
            .first_page;
        // 500 rows of two `int` take more than one heap page.
        {
            let mut rows = HeapTable::open(&storage, table, entry_page, two_ints(false));
            for value in 0..500 {
                rows.insert(TxnId(1), &Row(vec![Value::I32(value), Value::Null]))
                    .expect("insert");
            }
            rows.commit(TxnId(1)).expect("commit");
        }
        let before = control_of(&storage).next_page_id;
        assert!(before.0 > 3, "the heap took more than one page: {before}");

        storage.drop_table(table).expect("drop the table");
        assert_eq!(storage.tables(db).expect("tables"), Vec::new());
        assert_eq!(storage.indexes(table).unwrap_err().number, 50000);

        // The pages of the heap are in the free list: the next allocation hands one of them
        // back and the file does not grow.
        let handed = alloc::allocate(&storage).expect("allocate");
        assert_eq!(control_of(&storage).next_page_id, before);
        assert!(
            handed.0 < before.0,
            "{handed} is a page the dropped table held"
        );
        drop(dir);
    }

    #[test]
    fn clustered_index_shares_the_tree() {
        let (dir, storage) = empty_instance("ddl-clustered-index");
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(true))
            .expect("create the table");
        let before = control_of(&storage).next_page_id;

        let index = storage
            .create_index(table, &unique_on(0))
            .expect("the index of the clustered key");
        assert_eq!(
            control_of(&storage).next_page_id,
            before,
            "an index on the clustered key allocates no tree page"
        );
        assert_eq!(
            storage.indexes(table).expect("indexes"),
            vec![(index, unique_on(0))]
        );
        assert_eq!(
            storage
                .lock_catalogue()
                .expect("the catalogue")
                .index(index)
                .expect("the index")
                .root,
            None,
            "the entry carries no root of its own"
        );
        drop(dir);
    }

    /// The clustered table `table` of the catalogue, attached as it stands.
    fn clustered_of<'a>(storage: &'a DiskStorage, table: TableId) -> ClusteredTable<'a> {
        let entry = storage
            .lock_catalogue()
            .expect("the catalogue")
            .table(table)
            .expect("the table")
            .clone();
        ClusteredTable::open(
            storage,
            table,
            entry.first_page,
            (
                entry.tree_root.expect("the tree"),
                entry.directory_root.expect("the directory"),
            ),
            entry.shape,
        )
        .expect("attach to the clustered table")
    }

    #[test]
    fn a_unique_index_on_the_clustered_key_refuses_a_duplicate() {
        let (dir, storage) = empty_instance("ddl-clustered-duplicate");
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(true))
            .expect("create the table");
        {
            let mut rows = clustered_of(&storage, table);
            for second in [11, 12] {
                rows.insert(TxnId(1), &Row(vec![Value::I32(7), Value::I32(second)]))
                    .expect("insert a row of key 7");
            }
            rows.commit(TxnId(1)).expect("commit");
        }
        let before = control_of(&storage).next_page_id;

        let err = storage
            .create_index(table, &unique_on(0))
            .expect_err("two live rows of key 7 are a duplicate");
        assert_eq!(err.number, 2601);
        assert!(err.message.contains("I32(7)"), "{}", err.message);
        assert_eq!(storage.indexes(table).expect("indexes"), Vec::new());
        assert_eq!(
            control_of(&storage).next_page_id,
            before,
            "nothing allocated"
        );

        // The same shape without `unique` passes, and shares the tree.
        let permissive = IndexShape {
            columns: vec![KeyColumn {
                column: 0,
                descending: false,
            }],
            unique: false,
            included: Vec::new(),
        };
        let index = storage
            .create_index(table, &permissive)
            .expect("a non-unique index takes the duplicate");
        assert_eq!(
            index,
            IndexId(2),
            "the refused statement kept its identifier"
        );
        assert_eq!(control_of(&storage).next_page_id, before);

        // One row of key 7 deleted: the key is live once, and the unique index passes.
        {
            let mut rows = clustered_of(&storage, table);
            let visible = rows
                .scan(&Snapshot {
                    xmin: TxnId(9),
                    xmax: TxnId(9),
                    active: Vec::new(),
                    own: TxnId(2),
                })
                .expect("scan");
            assert_eq!(visible.len(), 2);
            rows.delete(TxnId(2), visible[0].0).expect("delete");
            rows.commit(TxnId(2)).expect("commit");
        }
        assert_eq!(
            storage
                .create_index(table, &unique_on(0))
                .expect("one live row of key 7 is no duplicate"),
            IndexId(3)
        );
        drop(dir);
    }

    /// A snapshot whose `xmin` and `xmax` are `TxnId(9)`, so a transaction of a smaller
    /// identifier reads as settled.
    fn settled(own: u64) -> Snapshot {
        Snapshot {
            xmin: TxnId(9),
            xmax: TxnId(9),
            active: Vec::new(),
            own: TxnId(own),
        }
    }

    /// A row of two `int` columns.
    fn pair(a: i32, b: i32) -> Row {
        Row(vec![Value::I32(a), Value::I32(b)])
    }

    /// An index on the column `column`, ascending, not `unique`, with no included column.
    fn index_on(column: u16) -> IndexShape {
        IndexShape {
            unique: false,
            ..unique_on(column)
        }
    }

    /// A clustered shape of one column of type `ty`, key on that column.
    fn keyed_by(ty: SqlType) -> TableShape {
        TableShape {
            columns: vec![TypeInfo::new(ty, true)],
            clustered_key: Some(vec![KeyColumn {
                column: 0,
                descending: false,
            }]),
        }
    }

    #[test]
    fn assert_send_sync() {
        fn takes<T: Storage + Send + Sync + 'static>() {}
        takes::<DiskStorage>();
        let (dir, storage) = empty_instance("trait-send-sync");
        let behind: Box<dyn Storage> = Box::new(storage);
        assert_eq!(behind.databases().expect("databases"), Vec::new());
        drop(dir);
    }

    #[test]
    fn insert_commit_reopen_get() {
        for clustered in [false, true] {
            let dir = TempDir::created(if clustered {
                "trait-reopen-clustered"
            } else {
                "trait-reopen-heap"
            });
            let (table, ids) = {
                let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
                let db = storage.create_database("a").expect("create the database");
                let table = storage
                    .create_table(db, &two_ints(clustered))
                    .expect("create the table");
                // Inserted in an order the clustered key does not follow, so that the order of
                // the `scan` below tells the two stores apart.
                let mut ids = Vec::new();
                for key in [30, 10, 20] {
                    ids.push(
                        storage
                            .insert(TxnId(1), table, &pair(key, key + 1))
                            .expect("insert"),
                    );
                }
                storage.commit(TxnId(1)).expect("commit");
                // No checkpoint either: the clustered tree is now replayed
                // (`a_clustered_table_reopened_without_a_checkpoint_is_redone`): the two halves
                // of this loop both come back from the journal.
                (table, ids)
            };

            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
            for (position, key) in [30, 10, 20].into_iter().enumerate() {
                assert_eq!(
                    storage.get(&settled(9), table, ids[position]).expect("get"),
                    Some(pair(key, key + 1)),
                    "clustered: {clustered}"
                );
            }
            let rows: Vec<_> = storage
                .scan(&settled(9), table)
                .expect("scan")
                .collect::<SqlResult<Vec<_>>>()
                .expect("the rows");
            // A heap answers in `RowId` order, a clustered table in clustered-key order: that
            // is what says the rows came back from the store the shape asked for.
            let expected = if clustered {
                vec![
                    (ids[1], pair(10, 11)),
                    (ids[2], pair(20, 21)),
                    (ids[0], pair(30, 31)),
                ]
            } else {
                vec![
                    (ids[0], pair(30, 31)),
                    (ids[1], pair(10, 11)),
                    (ids[2], pair(20, 21)),
                ]
            };
            assert_eq!(rows, expected, "clustered: {clustered}");
            drop(storage);
            drop(dir);
        }
    }

    #[test]
    fn unique_2601_on_disk() {
        for clustered in [false, true] {
            let (dir, storage) = empty_instance(if clustered {
                "trait-unique-clustered"
            } else {
                "trait-unique-heap"
            });
            let db = storage.create_database("a").expect("create the database");
            let table = storage
                .create_table(db, &two_ints(clustered))
                .expect("create the table");
            // On the second column, so that the index is one of its own on both stores.
            let index = storage
                .create_index(table, &unique_on(1))
                .expect("create the index");

            // Two rows whose first column, the clustered key of the clustered half, is
            // inserted in decreasing order: the `scan` below reads back in one order or the
            // other and says which store answered.
            let first = storage
                .insert(TxnId(1), table, &pair(3, 5))
                .expect("insert");
            let second = storage
                .insert(TxnId(1), table, &pair(1, 7))
                .expect("insert");
            storage.commit(TxnId(1)).expect("commit");

            let err = storage
                .insert(TxnId(2), table, &pair(2, 5))
                .expect_err("the second row holds the key of the first");
            assert_eq!(err.number, 2601, "clustered: {clustered}");
            assert!(err.message.contains(&index.to_string()), "{}", err.message);
            // The refused insert left no row: the check runs before the write.
            let rows: Vec<_> = storage
                .scan(&settled(9), table)
                .expect("scan")
                .collect::<SqlResult<Vec<_>>>()
                .expect("the rows");
            let expected = if clustered {
                vec![(second, pair(1, 7)), (first, pair(3, 5))]
            } else {
                vec![(first, pair(3, 5)), (second, pair(1, 7))]
            };
            assert_eq!(rows, expected, "clustered: {clustered}");
            drop(dir);
        }
    }

    #[test]
    fn one_begin_one_end_per_txn_over_two_tables() {
        let (dir, storage) = empty_instance("trait-one-begin");
        let db = storage.create_database("a").expect("create the database");
        let first = storage
            .create_table(db, &two_ints(false))
            .expect("create the first table");
        let second = storage
            .create_table(db, &two_ints(false))
            .expect("create the second table");

        storage
            .insert(TxnId(3), first, &pair(1, 1))
            .expect("insert in the first table");
        storage
            .insert(TxnId(3), second, &pair(2, 2))
            .expect("insert in the second table");
        storage.commit(TxnId(3)).expect("commit");

        let records = storage.wal.records().expect("read the journal");
        let mine: Vec<WalRecordKind> = records
            .iter()
            .filter(|record| record.txn == TxnId(3))
            .map(|record| record.kind)
            .collect();
        assert_eq!(
            mine,
            vec![
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Insert,
                WalRecordKind::Commit
            ]
        );
        // Both rows are visible to a later snapshot: one status for the two tables.
        for table in [first, second] {
            let rows: Vec<_> = storage
                .scan(&settled(9), table)
                .expect("scan")
                .collect::<SqlResult<Vec<_>>>()
                .expect("the rows");
            assert_eq!(rows.len(), 1);
        }
        drop(dir);
    }

    #[test]
    fn stale_version_is_refused_before_uniqueness() {
        let (dir, storage) = empty_instance("trait-stale-first");
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(false))
            .expect("create the table");
        storage
            .create_index(table, &unique_on(1))
            .expect("create the index");

        let live = storage
            .insert(TxnId(1), table, &pair(1, 5))
            .expect("insert the row whose key 5 stays live");
        let stale = storage
            .insert(TxnId(1), table, &pair(2, 6))
            .expect("insert the row that goes stale");
        storage.commit(TxnId(1)).expect("commit");
        storage.delete(TxnId(2), table, stale).expect("delete");
        storage.commit(TxnId(2)).expect("commit");

        // The update would break the unique index too: the key 5 is held by a live row.
        let err = storage
            .update(TxnId(3), table, stale, &pair(2, 5))
            .expect_err("a stale row is refused");
        assert_ne!(err.number, 2601, "{}", err.message);
        assert!(err.message.contains("is stale"), "{}", err.message);
        // The same update on a row that is not stale does answer 2601, which is what makes the
        // order of the two checks visible.
        let other = storage
            .insert(TxnId(4), table, &pair(3, 7))
            .expect("insert a third row");
        storage.commit(TxnId(4)).expect("commit");
        let err = storage
            .update(TxnId(5), table, other, &pair(3, 5))
            .expect_err("the key 5 is taken");
        assert_eq!(err.number, 2601, "{}", err.message);
        assert_eq!(
            storage.get(&settled(9), table, live).expect("get"),
            Some(pair(1, 5))
        );
        drop(dir);
    }

    /// A database, a table of [`two_ints`] and a `unique` index on its second column, on
    /// `storage`, with one committed row of key 5 and one transaction already committed.
    ///
    /// `TxnId(1)` holds the live row `(1, 5)`; `TxnId(2)` wrote `(3, 7)` and committed, so it
    /// is a finished transaction; `TxnId(5)` has written nothing yet.
    fn table_with_a_unique_index(storage: &dyn Storage, clustered: bool) -> TableId {
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(clustered))
            .expect("create the table");
        storage
            .create_index(table, &unique_on(1))
            .expect("create the index");
        storage
            .insert(TxnId(1), table, &pair(1, 5))
            .expect("the row whose key 5 stays live");
        storage.commit(TxnId(1)).expect("commit");
        storage
            .insert(TxnId(2), table, &pair(3, 7))
            .expect("the write of the transaction that then ends");
        storage.commit(TxnId(2)).expect("commit");
        table
    }

    /// The error of `answer`, as the two implementations are compared on it.
    fn failure<T: std::fmt::Debug>(answer: SqlResult<T>) -> SqlError {
        answer.expect_err("the call is refused")
    }

    #[test]
    fn a_finished_txn_is_refused_before_uniqueness() {
        for clustered in [false, true] {
            let (dir, disk) = empty_instance(if clustered {
                "trait-finished-clustered"
            } else {
                "trait-finished-heap"
            });
            let memory = crate::MemoryStorage::new();
            let mut answers = Vec::new();
            for storage in [&disk as &dyn Storage, &memory as &dyn Storage] {
                let table = table_with_a_unique_index(storage, clustered);
                // `TxnId(2)` has committed, and the row would also take the key 5 of a live
                // row: the precondition of the store is what must be reported.
                let err = failure(storage.insert(TxnId(2), table, &pair(9, 5)));
                assert_ne!(err.number, 2601, "clustered: {clustered}, {}", err.message);
                assert!(
                    err.message.contains("already Committed"),
                    "clustered: {clustered}, {}",
                    err.message
                );
                answers.push(err);
            }
            assert_eq!(
                (answers[0].number, &answers[0].message),
                (answers[1].number, &answers[1].message),
                "disk and memory answer the same, clustered: {clustered}"
            );
            drop(dir);
        }
    }

    #[test]
    fn a_row_of_the_wrong_arity_is_refused_before_uniqueness() {
        for clustered in [false, true] {
            let (dir, disk) = empty_instance(if clustered {
                "trait-arity-clustered"
            } else {
                "trait-arity-heap"
            });
            let memory = crate::MemoryStorage::new();
            let mut answers = Vec::new();
            for storage in [&disk as &dyn Storage, &memory as &dyn Storage] {
                let table = table_with_a_unique_index(storage, clustered);
                // One value where the shape asks for two, and that value holds the key 5 of a
                // live row: the arity is what must be reported, as an `InternalError::Bug`.
                let err = failure(storage.insert(TxnId(5), table, &Row(vec![Value::I32(5)])));
                assert_eq!(err.number, 50000, "clustered: {clustered}, {}", err.message);
                assert!(
                    err.message.contains("row has 1 values"),
                    "clustered: {clustered}, {}",
                    err.message
                );
                assert!(
                    !err.message.contains("data corruption"),
                    "clustered: {clustered}, {}",
                    err.message
                );
                answers.push(err);
            }
            assert_eq!(
                (answers[0].number, &answers[0].message),
                (answers[1].number, &answers[1].message),
                "disk and memory answer the same, clustered: {clustered}"
            );
            drop(dir);
        }
    }

    #[test]
    fn a_failed_store_of_leaves_the_state_of_the_table_in_place() {
        let (dir, storage) = empty_instance("trait-store-of-error");
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(true))
            .expect("create the clustered table");
        assert_eq!(
            storage
                .insert(TxnId(1), table, &pair(1, 1))
                .expect("insert"),
            RowId(1)
        );

        // The catalogue loses the roots of the tree, so `store_of` fails after `with_rows_of`
        // has taken the state of the table out of its cell.
        let entry = storage
            .lock_catalogue()
            .expect("the catalogue")
            .table(table)
            .expect("the entry")
            .clone();
        let broken = TableEntry {
            tree_root: None,
            ..entry.clone()
        };
        storage
            .lock_catalogue()
            .expect("the catalogue")
            .add_table(broken);
        let err = storage
            .insert(TxnId(1), table, &pair(2, 2))
            .expect_err("the entry carries no tree");
        assert!(err.message.contains("no tree"), "{}", err.message);

        // The roots come back: the state of the table must be the one the first insert left,
        // not a `TableState::default()`.
        storage
            .lock_catalogue()
            .expect("the catalogue")
            .add_table(entry);
        assert_eq!(
            storage
                .insert(TxnId(1), table, &pair(3, 3))
                .expect("insert"),
            RowId(2),
            "the counter of the table survived the failed call"
        );
        storage.rollback(TxnId(1)).expect("rollback");
        for id in [RowId(1), RowId(2)] {
            assert_eq!(
                storage.latest_version(table, id).expect("latest_version"),
                None,
                "the undo log of the transaction survived the failed call"
            );
        }
        drop(dir);
    }

    #[test]
    fn clustered_key_over_900_bytes_is_1946() {
        let (dir, storage) = empty_instance("trait-key-width");
        let db = storage.create_database("a").expect("create the database");

        let err = storage
            .create_table(db, &keyed_by(SqlType::Char(Len::Fixed(901))))
            .expect_err("a key of 901 declared bytes is refused");
        assert_eq!(err.number, 1946);
        assert_eq!(err.severity, 16);
        assert!(err.message.contains("901"), "{}", err.message);
        assert!(err.message.contains("900"), "{}", err.message);
        // The same shape one byte shorter is created, so 900 is the bound and not a guess.
        storage
            .create_table(db, &keyed_by(SqlType::Char(Len::Fixed(900))))
            .expect("a key of 900 declared bytes is taken");
        // `nchar` counts two bytes per character: 451 characters are 902 bytes.
        let err = storage
            .create_table(db, &keyed_by(SqlType::NChar(Len::Fixed(451))))
            .expect_err("451 characters of nchar are 902 bytes");
        assert_eq!(err.number, 1946);
        assert!(err.message.contains("902"), "{}", err.message);
        assert_eq!(storage.tables(db).expect("tables").len(), 1);
        drop(dir);
    }

    #[test]
    fn a_max_column_in_a_clustered_key_is_1946() {
        let (dir, storage) = empty_instance("trait-key-max");
        let db = storage.create_database("a").expect("create the database");

        let err = storage
            .create_table(db, &keyed_by(SqlType::VarChar(Len::Max)))
            .expect_err("a max column has no declared width");
        assert_eq!(err.number, 1946);
        assert!(err.message.contains("unbounded"), "{}", err.message);
        assert_eq!(storage.tables(db).expect("tables"), Vec::new());
        drop(dir);
    }

    #[test]
    fn index_seek_on_clustered_table() {
        let (dir, storage) = empty_instance("trait-index-clustered");
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(true))
            .expect("create the clustered table");
        // A shape other than the clustered key, kept up to date at each write.
        let index = storage
            .create_index(table, &index_on(1))
            .expect("a non-clustered index on a clustered table");

        let mut ids = Vec::new();
        for (key, indexed) in [(30, 5), (10, 7), (20, 5)] {
            ids.push(
                storage
                    .insert(TxnId(1), table, &pair(key, indexed))
                    .expect("insert"),
            );
        }
        storage.commit(TxnId(1)).expect("commit");

        let found: Vec<_> = storage
            .seek(
                &settled(9),
                index,
                &KeyRange::Point(vec![Value::I32(5)]),
                Direction::Forward,
            )
            .expect("seek")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(
            found,
            vec![(ids[0], pair(30, 5)), (ids[2], pair(20, 5))],
            "the two rows of key 5, by increasing RowId"
        );
        // The table answering the `seek` is the clustered one: its `scan` is in clustered-key
        // order, where a heap would answer in the insertion order 30, 10, 20.
        let rows: Vec<_> = storage
            .scan(&settled(9), table)
            .expect("scan")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(
            rows,
            vec![
                (ids[1], pair(10, 7)),
                (ids[2], pair(20, 5)),
                (ids[0], pair(30, 5)),
            ]
        );

        // The maintenance follows an update: the row leaves the key 5 for the key 9.
        storage
            .update(TxnId(2), table, ids[0], &pair(30, 9))
            .expect("update");
        storage.commit(TxnId(2)).expect("commit");
        let found: Vec<_> = storage
            .seek(
                &settled(9),
                index,
                &KeyRange::Point(vec![Value::I32(5)]),
                Direction::Forward,
            )
            .expect("seek")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(found, vec![(ids[2], pair(20, 5))]);
        drop(dir);
    }

    #[test]
    fn a_clustered_table_reopened_without_a_checkpoint_is_redone() {
        let dir = TempDir::created("trait-reopen-no-checkpoint");
        let (heap, clustered, id) = {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
            let db = storage.create_database("a").expect("create the database");
            let heap = storage
                .create_table(db, &two_ints(false))
                .expect("create the heap");
            let clustered = storage
                .create_table(db, &two_ints(true))
                .expect("create the clustered table");
            storage
                .insert(TxnId(1), heap, &pair(7, 11))
                .expect("insert in the heap");
            let id = storage
                .insert(TxnId(1), clustered, &pair(7, 11))
                .expect("insert in the clustered table");
            storage.commit(TxnId(1)).expect("commit");
            (heap, clustered, id)
        };

        // No checkpoint: the rows of the heap and of the clustered table come back from the
        // journal, and the tree of the clustered table is replayed on the pages
        // [`ClusteredTable::redo_open`] formats as empty leaves.
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let rows: Vec<_> = storage
            .scan(&settled(9), heap)
            .expect("scan the heap")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows of the heap");
        assert_eq!(rows.len(), 1, "the heap comes back from the journal");
        let rows: Vec<_> = storage
            .scan(&settled(9), clustered)
            .expect("scan the clustered table")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows of the clustered table");
        assert_eq!(
            rows,
            vec![(id, pair(7, 11))],
            "the tree is replayed from the journal"
        );
        assert_eq!(
            storage.get(&settled(9), clustered, id).expect("get"),
            Some(pair(7, 11))
        );
        drop(storage);
        drop(dir);
    }

    #[test]
    fn a_reopen_hands_out_row_ids_past_the_ones_the_journal_names() {
        for clustered in [false, true] {
            let dir = TempDir::created(if clustered {
                "trait-resume-clustered"
            } else {
                "trait-resume-heap"
            });
            let table = {
                let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
                let db = storage.create_database("a").expect("create the database");
                let table = storage
                    .create_table(db, &two_ints(clustered))
                    .expect("create the table");
                for key in [1, 2, 3] {
                    assert_eq!(
                        storage
                            .insert(TxnId(1), table, &pair(key, key))
                            .expect("insert"),
                        RowId(u64::try_from(key).expect("a small key"))
                    );
                }
                storage.commit(TxnId(1)).expect("commit");
                if clustered {
                    storage.checkpoint().expect("checkpoint");
                }
                table
            };

            // The state of the table is rebuilt from what the recovery of the `open` found:
            // the fourth row takes `RowId(4)`, not the `RowId(1)` a counter starting over
            // would hand out a second time.
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
            assert_eq!(
                storage
                    .insert(TxnId(2), table, &pair(4, 4))
                    .expect("insert after the reopen"),
                RowId(4),
                "clustered: {clustered}"
            );
            storage.commit(TxnId(2)).expect("commit");
            let rows: Vec<_> = storage
                .scan(&settled(9), table)
                .expect("scan")
                .collect::<SqlResult<Vec<_>>>()
                .expect("the rows");
            assert_eq!(rows.len(), 4, "clustered: {clustered}");
            // A transaction the journal has already ended may not write again: the register of
            // the instance took the winners of the recovery.
            let err = storage
                .insert(TxnId(1), table, &pair(5, 5))
                .expect_err("TxnId(1) is a winner of the recovery");
            assert_eq!(err.number, 50000, "{}", err.message);
            assert!(err.message.contains("Committed"), "{}", err.message);
            drop(storage);
            drop(dir);
        }
    }

    #[test]
    fn savepoint_rollback_to_partial() {
        let dir = TempDir::created("trait-savepoint");
        let (db, table, first_two) = {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
            let db = storage.create_database("a").expect("create the database");
            let table = storage
                .create_table(db, &two_ints(false))
                .expect("create the table");
            let a = storage
                .insert(TxnId(1), table, &pair(1, 1))
                .expect("insert a");
            let b = storage
                .insert(TxnId(1), table, &pair(2, 2))
                .expect("insert b");
            let sp = storage.savepoint(TxnId(1)).expect("savepoint");
            let c = storage
                .insert(TxnId(1), table, &pair(3, 3))
                .expect("insert c");
            assert_eq!(c, RowId(3));
            storage
                .rollback_to(TxnId(1), sp)
                .expect("rollback to savepoint");
            storage.commit(TxnId(1)).expect("commit");
            (db, table, vec![a, b])
        };
        let _ = db;

        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let rows: Vec<_> = storage
            .scan(&settled(9), table)
            .expect("scan")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0], (first_two[0], pair(1, 1)));
        assert_eq!(rows[1], (first_two[1], pair(2, 2)));
        assert!(
            storage
                .wal
                .records()
                .expect("journal")
                .iter()
                .any(|r| r.kind == WalRecordKind::Savepoint),
            "the savepoint is journalled"
        );
        assert!(
            storage
                .wal
                .records()
                .expect("journal")
                .iter()
                .any(|r| r.kind == WalRecordKind::RollbackTo),
            "the rollback to savepoint is journalled"
        );
        drop(storage);
        drop(dir);
    }

    #[test]
    fn vacuum_removes_dead_keeps_horizon() {
        let (dir, storage) = empty_instance("trait-vacuum-heap");
        let db = storage.create_database("a").expect("database");
        let table = storage.create_table(db, &two_ints(false)).expect("table");
        let a = storage.insert(TxnId(1), table, &pair(1, 1)).expect("a");
        let b = storage.insert(TxnId(1), table, &pair(2, 2)).expect("b");
        storage.commit(TxnId(1)).expect("commit 1");
        storage.delete(TxnId(2), table, a).expect("delete a");
        storage.commit(TxnId(2)).expect("commit 2");
        // A snapshot taken while TxnId(3) still runs: `xmax` does not settle 3, `active`
        // holds it, so 3's own write of b is not visible to it.
        let held = Snapshot {
            xmin: TxnId(1),
            xmax: TxnId(4),
            active: vec![TxnId(3)],
            own: TxnId(2),
        };
        storage
            .update(TxnId(3), table, b, &pair(20, 2))
            .expect("update b");
        storage.commit(TxnId(3)).expect("commit 3");

        storage.vacuum(TxnId(3)).expect("vacuum at horizon 3");
        assert_eq!(
            storage.get(&held, table, b).expect("old b visible"),
            Some(pair(2, 2)),
            "the version a snapshot still holds is kept"
        );
        storage.vacuum(TxnId(4)).expect("vacuum at horizon 4");
        assert_eq!(storage.get(&settled(9), table, a).expect("a gone"), None);
        assert_eq!(
            storage.get(&settled(9), table, b).expect("new b"),
            Some(pair(20, 2))
        );
        let c = storage
            .insert(TxnId(5), table, &pair(3, 3))
            .expect("row ids are not reused");
        assert_eq!(c, RowId(3));
        drop(storage);
        drop(dir);
    }

    #[test]
    fn vacuum_drops_index_entries_of_dead_versions() {
        let (dir, storage) = empty_instance("trait-vacuum-index");
        let db = storage.create_database("a").expect("database");
        let table = storage.create_table(db, &two_ints(false)).expect("table");
        // Indexed on column 0, the one the update moves from 2 to 20.
        let index = storage.create_index(table, &index_on(0)).expect("index");
        let b = storage
            .insert(TxnId(1), table, &pair(2, 2))
            .expect("b at key 2");
        storage.commit(TxnId(1)).expect("commit");
        storage
            .update(TxnId(2), table, b, &pair(20, 2))
            .expect("move b to 20");
        storage.commit(TxnId(2)).expect("commit");
        // A snapshot taken while TxnId(2) still runs: it reads b under its old key.
        let older = Snapshot {
            xmin: TxnId(1),
            xmax: TxnId(3),
            active: vec![TxnId(2)],
            own: TxnId(1),
        };
        let before: Vec<_> = storage
            .seek(
                &older,
                index,
                &KeyRange::Point(vec![vauban_types::Value::I32(2)]),
                Direction::Forward,
            )
            .expect("seek the old key")
            .collect::<SqlResult<Vec<_>>>()
            .expect("rows");
        assert_eq!(before, vec![(b, pair(2, 2))]);
        storage.vacuum(TxnId(3)).expect("vacuum");
        let after: Vec<_> = storage
            .seek(
                &settled(9),
                index,
                &KeyRange::Point(vec![vauban_types::Value::I32(2)]),
                Direction::Forward,
            )
            .expect("seek")
            .collect::<SqlResult<Vec<_>>>()
            .expect("rows");
        assert!(
            after.is_empty(),
            "the index lost the dead version: {after:?}"
        );
        drop(storage);
        drop(dir);
    }

    #[test]
    fn indexes_are_rebuilt_at_open() {
        let dir = TempDir::created("trait-index-reopen");
        let (table, index, ids) = {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
            let db = storage.create_database("a").expect("database");
            let table = storage.create_table(db, &two_ints(false)).expect("table");
            let index = storage.create_index(table, &index_on(0)).expect("index");
            let mut ids = Vec::new();
            for key in [1i32, 2, 3] {
                ids.push(
                    storage
                        .insert(TxnId(1), table, &pair(key, 0))
                        .expect("insert"),
                );
            }
            storage.commit(TxnId(1)).expect("commit");
            (table, index, ids)
        };
        let _ = table;
        // No checkpoint: the tree pages of the index stayed in the pool and are gone; the row
        // pages themselves come back from the heap and the journal.
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let rows: Vec<_> = storage
            .seek(
                &settled(9),
                index,
                &KeyRange::Point(vec![vauban_types::Value::I32(2)]),
                Direction::Forward,
            )
            .expect("seek")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(rows, vec![(ids[1], pair(2, 0))], "the index is back");
        drop(storage);
        drop(dir);
    }

    #[test]
    fn a_loser_xmax_on_data_is_cleared() {
        let dir = TempDir::created("trait-loser-xmax");
        let (table, id) = {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
            let db = storage.create_database("a").expect("database");
            let table = storage.create_table(db, &two_ints(false)).expect("table");
            let id = storage
                .insert(TxnId(1), table, &pair(1, 1))
                .expect("insert");
            storage.commit(TxnId(1)).expect("commit 1");
            // TxnId(2) deletes the row; its page is shared with a later committed write of
            // TxnId(3) that reaches `data`, which carries the loser's `xmax`.
            storage.delete(TxnId(2), table, id).expect("delete");
            storage
                .insert(TxnId(3), table, &pair(9, 9))
                .expect("second write");
            storage.commit(TxnId(3)).expect("commit 3");
            storage.checkpoint().expect("checkpoint");
            storage.rollback(TxnId(2)).expect("rollback 2");
            // The rollback replays the undo and clears the xmax on `data`'s copy too; a build
            // that left it there is what this test would catch through the reopen below.
            (table, id)
        };
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        assert_eq!(
            storage.get(&settled(9), table, id).expect("get"),
            Some(pair(1, 1)),
            "the row of a rolled-back delete is back"
        );
        storage
            .update(TxnId(4), table, id, &pair(2, 2))
            .expect("update a row whose loser xmax is cleared");
        storage.commit(TxnId(4)).expect("commit");
        drop(storage);
        drop(dir);
    }

    #[test]
    fn rollback_writes_one_abort_and_takes_back_both_tables() {
        let (dir, storage) = empty_instance("trait-rollback");
        let db = storage.create_database("a").expect("create the database");
        let heap = storage
            .create_table(db, &two_ints(false))
            .expect("create the heap");
        let clustered = storage
            .create_table(db, &two_ints(true))
            .expect("create the clustered table");
        let index = storage
            .create_index(heap, &index_on(1))
            .expect("create the index");

        storage
            .insert(TxnId(3), heap, &pair(1, 5))
            .expect("insert in the heap");
        storage
            .insert(TxnId(3), clustered, &pair(2, 5))
            .expect("insert in the clustered table");
        storage.rollback(TxnId(3)).expect("rollback");

        let mine: Vec<WalRecordKind> = storage
            .wal
            .records()
            .expect("read the journal")
            .iter()
            .filter(|record| record.txn == TxnId(3))
            .map(|record| record.kind)
            .collect();
        assert_eq!(
            mine,
            vec![
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Insert,
                WalRecordKind::Abort
            ]
        );
        for table in [heap, clustered] {
            let rows: Vec<_> = storage
                .scan(&settled(9), table)
                .expect("scan")
                .collect::<SqlResult<Vec<_>>>()
                .expect("the rows");
            assert_eq!(rows, Vec::new());
        }
        // The versions themselves are gone, not merely hidden: `latest_version` reads the
        // directory of the store and no longer finds the row.
        for (table, id) in [(heap, RowId(1)), (clustered, RowId(2))] {
            assert_eq!(
                storage.latest_version(table, id).expect("latest_version"),
                None
            );
        }
        // The index of the heap lost the entry of the undone insert too.
        let found: Vec<_> = storage
            .seek(
                &settled(9),
                index,
                &KeyRange::Point(vec![Value::I32(5)]),
                Direction::Forward,
            )
            .expect("seek")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(found, Vec::new());
        drop(dir);
    }

    #[test]
    fn a_second_open_of_the_same_directory_is_refused() {
        let (dir, storage) = empty_instance("ddl-second-open");
        let db = storage.create_database("a").expect("create the database");

        let err = DiskStorage::open(dir.path(), DiskOptions::default())
            .expect_err("a second instance on one directory is refused");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("already open"), "{err}");

        // The first instance goes on writing.
        let table = storage
            .create_table(db, &two_ints(false))
            .expect("the first instance writes on");
        assert_eq!(storage.tables(db).expect("tables").len(), 1);
        drop(storage);

        // Once it is dropped, the directory opens again, with what it held.
        let reopened = reopen(&dir);
        assert_eq!(
            reopened.tables(db).expect("tables"),
            vec![(table, two_ints(false))]
        );
    }

    #[test]
    fn drop_database_takes_its_tables_and_indexes() {
        let (dir, storage) = empty_instance("ddl-drop-database");
        let (db, table, _index) = create_the_three(&storage, false);
        let other = storage.create_database("b").expect("create the database");
        let kept = storage
            .create_table(other, &two_ints(false))
            .expect("create the table");

        storage.drop_database(db).expect("drop the database");
        assert_eq!(
            storage.databases().expect("databases"),
            vec![(other, "b".to_string())]
        );
        assert_eq!(
            storage.tables(other).expect("tables"),
            vec![(kept, two_ints(false))]
        );
        // The dropped database, its table and the index of that table have left the
        // catalogue.
        for message in [
            storage.tables(db).unwrap_err().message,
            storage.indexes(table).unwrap_err().message,
            storage.drop_table(table).unwrap_err().message,
            storage.drop_database(db).unwrap_err().message,
        ] {
            assert!(message.contains("unknown to instance"), "{message}");
        }
        drop(dir);
    }

    #[test]
    fn drop_index_frees_its_tree_and_leaves_the_rows() {
        let (dir, storage) = empty_instance("ddl-drop-index");
        let (db, table, index) = create_the_three(&storage, false);
        let before = control_of(&storage).next_page_id;
        assert_eq!(
            storage.indexes(table).expect("indexes"),
            vec![(index, unique_on(0))]
        );

        storage.drop_index(index).expect("drop the index");
        assert_eq!(storage.indexes(table).expect("indexes"), Vec::new());
        assert_eq!(
            storage.tables(db).expect("tables"),
            vec![(table, two_ints(false))],
            "the table is untouched"
        );
        // The page of the tree went back to the free list.
        assert_eq!(control_of(&storage).next_page_id, before);
        assert!(control_of(&storage).free_head.is_some());
        assert!(
            storage.drop_index(index).is_err(),
            "the index is unknown now"
        );
        drop(dir);
    }

    #[test]
    fn an_identifier_is_not_handed_out_twice_after_a_refused_statement() {
        let (dir, storage) = empty_instance("ddl-identifiers");
        let db = storage.create_database("a").expect("create the database");
        assert_eq!(db, DbId(1));
        // A shape this build refuses, checked before an identifier is taken.
        let empty = TableShape {
            columns: Vec::new(),
            clustered_key: None,
        };
        assert!(storage.create_table(db, &empty).is_err());
        assert_eq!(control_of(&storage).next_table_id, 1, "no identifier taken");

        let table = storage
            .create_table(db, &two_ints(false))
            .expect("create the table");
        assert_eq!(table, TableId(1));
        // An index shape naming a column outside the table is refused before the identifier.
        assert!(storage.create_index(table, &unique_on(9)).is_err());
        assert_eq!(control_of(&storage).next_index_id, 1);
        // A duplicate key takes the identifier and creates nothing (2601).
        let entry_page = storage
            .lock_catalogue()
            .expect("the catalogue")
            .table(table)
            .expect("the table")
            .first_page;
        {
            let mut rows = HeapTable::open(&storage, table, entry_page, two_ints(false));
            for _ in 0..2 {
                rows.insert(TxnId(1), &Row(vec![Value::I32(7), Value::Null]))
                    .expect("insert");
            }
            rows.commit(TxnId(1)).expect("commit");
        }
        assert_eq!(
            storage
                .create_index(table, &unique_on(0))
                .unwrap_err()
                .number,
            2601
        );
        assert_eq!(storage.indexes(table).expect("indexes"), Vec::new());
        assert_eq!(
            storage
                .create_index(
                    table,
                    &IndexShape {
                        columns: vec![KeyColumn {
                            column: 0,
                            descending: false,
                        }],
                        unique: false,
                        included: Vec::new(),
                    }
                )
                .expect("a non-unique index passes"),
            IndexId(2),
            "the identifier the refused statement took is not handed out again"
        );
        drop(dir);
    }

    #[test]
    fn a_table_of_an_unknown_database_is_refused() {
        let (dir, storage) = empty_instance("ddl-unknown-db");
        let err = storage
            .create_table(DbId(4), &two_ints(false))
            .expect_err("an unknown database is refused");
        assert!(
            err.message.contains("database 4 is unknown"),
            "{}",
            err.message
        );
        assert_eq!(control_of(&storage).next_table_id, 1);

        let db = storage.create_database("a").expect("create the database");
        let clustered = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, true)],
            clustered_key: Some(vec![KeyColumn {
                column: 3,
                descending: false,
            }]),
        };
        let err = storage
            .create_table(db, &clustered)
            .expect_err("a key outside the shape is refused");
        assert!(err.message.contains("column 3"), "{}", err.message);
        drop(dir);
    }
}
