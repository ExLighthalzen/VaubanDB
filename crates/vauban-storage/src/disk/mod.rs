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

/// Name of the file whose lock keeps the directory of an instance to one process
/// ([`InstanceGuard`]). It holds no byte; the lock is what counts.
pub(crate) const LOCK_FILE_NAME: &str = "vauban.lock";

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
/// it, and the reads and writes of the file layer are issued from there.
/// [`DiskStorage::data`] is a second `pub(crate)` handle on the same file, which the tests of
/// this module use to write a page behind the pool, and which the allocator ([`alloc`]) grows;
/// each access of the file layer names its own offset, so the two may move pages at once. The `impl Storage` is in `storage_impl`.
// `options` is read by the tests of this module and by nothing else in the engine.
#[allow(dead_code)]
#[derive(Debug)]
pub struct DiskStorage {
    /// Directory of the instance, the `dir` given to [`DiskStorage::open`].
    pub(crate) dir: PathBuf,
    /// The data file, `<dir>/data`. [`DiskStorage::pool`] holds the same handle; a call made
    /// through this field goes to the file without passing the lock of the pool, which the
    /// positioned accesses of [`file::DataFile`] make safe for the bytes of other pages.
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
    /// crash between the two steps leaves `data` or `wal` without a `vauban.ctl`. When both
    /// are empty or absent, the creation had written nothing yet: the next `open` removes
    /// them and creates the instance again (test
    /// `open_completes_a_creation_stopped_before_its_control_file`). When one of them holds
    /// bytes, the next `open` reports corruption rather than creating an instance over them;
    /// a crash before them leaves a directory this `open` treats as the empty one it is.
    ///
    /// # Reopening
    ///
    /// A `vauban.ctl` that is there is read and checked: its size, its magic, its
    /// `format_version` and its checksum. A `format_version` other than the 2 this build
    /// writes is
    /// [`InternalError::Corruption`], and this build does not migrate it. `data` and `wal`
    /// are then opened as they stand; either of the two missing is
    /// [`InternalError::Corruption`] naming the file, and so is a `data` whose size is not a
    /// whole number of pages. A directory that holds a non-empty `data` or `wal` without a
    /// `vauban.ctl` is corruption too: it holds part of an instance, and this build does not
    /// complete it.
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
            if !(empty_or_absent(&data_path)? && empty_or_absent(&wal_path)?) {
                return Err(InternalError::Corruption(format!(
                    "directory {} holds {DATA_FILE_NAME} or {WAL_FILE_NAME} but no \
                     {CONTROL_FILE_NAME}",
                    dir.display()
                )));
            }
            // A creation stopped before its control file: nothing was written yet.
            for path in [&data_path, &wal_path] {
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
            Self::create(dir, opts, instance, &control_path, &data_path, &wal_path)?
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
    /// (`tests::a_second_open_of_the_same_directory_is_refused`), and the lock of
    /// [`LOCK_FILE_NAME`] refuses it from another process
    /// (`tests::a_directory_locked_by_another_handle_is_refused`).
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

    /// The tables `txn` wrote in that the catalogue still holds, by increasing identifier.
    ///
    /// A table the transaction wrote in and that was dropped since is left out: the copy an
    /// `ALTER TABLE` fills and that the return to a savepoint drops before the writes are
    /// undone, whose rows went with its pages (`vauban-session`, `tests/savepoint_alter_on_disk.rs`). Its writes have
    /// nothing left to undo in, as at the end of the transaction
    /// ([`DiskStorage::end_transaction`]).
    ///
    /// # Errors
    ///
    /// The errors of [`DiskStorage::lock_catalogue`].
    pub(crate) fn live_txn_tables(&self, txn: TxnId) -> SqlResult<Vec<TableId>> {
        let tables = self.txn_tables(txn);
        let catalogue = self.lock_catalogue()?;
        Ok(tables
            .into_iter()
            .filter(|&table| catalogue.table(table).is_some())
            .collect())
    }

    /// One past the highest transaction identifier the register or the journal names.
    ///
    /// The journal counts winners and losers alike ([`Recovery::max_txn`]): a loser the
    /// register does not hold yet, because no table it wrote in has been resumed, still has
    /// its identifier kept out
    /// (`recover::tests::the_identifier_of_a_loser_is_not_handed_out_again`).
    pub(crate) fn next_txn_id(&self) -> u64 {
        let from_register = read_lock(&self.txns)
            .txns
            .keys()
            .map(|id| id.0)
            .max()
            .unwrap_or(0);
        from_register.max(self.recovery.max_txn).saturating_add(1)
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
        self.wal
            .append(WalRecordKind::Savepoint, txn, &id.0.to_le_bytes())?;
        // Record the current writes length per table. Tables are locked one by one, so this
        // is a best-effort snapshot: the caller must hold the per-table lock for the whole
        // savepoint + writes + rollback_to sequence.
        drop(register);
        let mut marks = HashMap::new();
        for table in self.live_txn_tables(txn)? {
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
        for table in self.live_txn_tables(txn)? {
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
/// Two levels. A `static` set of this process refuses a second `open` **in the process that
/// holds the first**, with [`InternalError::Bug`]. Across processes, the guard holds an
/// exclusive lock of the operating system ([`fs::File::try_lock`], `flock` on Unix) on
/// [`LOCK_FILE_NAME`] in the directory, for as long as it lives: a second process that opens
/// the directory meanwhile is refused with [`InternalError::Corruption`] naming the directory,
/// before it reads or writes a file of the instance
/// (`a_directory_locked_by_another_handle_is_refused`). The operating system releases the lock
/// of a process that dies, so after a `kill -9` the directory opens again.
#[derive(Debug)]
struct InstanceGuard {
    /// The directory, as [`fs::canonicalize`] answers it, so that two spellings of one
    /// directory are one entry.
    key: PathBuf,
    /// [`LOCK_FILE_NAME`], opened and locked; closing it releases the lock.
    _lock: fs::File,
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
        if open.contains(&key) {
            return Err(InternalError::Bug(format!(
                "directory {} is already open in this process; one instance holds it until it \
                 is dropped",
                key.display()
            )));
        }
        let lock = lock_directory(&key)?;
        open.insert(key.clone());
        Ok(Self { key, _lock: lock })
    }
}

/// Opens [`LOCK_FILE_NAME`] in `dir`, creating it, and takes its exclusive lock without
/// waiting.
///
/// # Errors
///
/// [`InternalError::Corruption`] naming the directory when another holder has the lock;
/// [`InternalError::Io`] when the file cannot be opened or the lock cannot be asked for.
fn lock_directory(dir: &Path) -> Result<fs::File, InternalError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE_NAME))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(InternalError::Corruption(format!(
            "directory {} is in use by another process (lock on {LOCK_FILE_NAME}); one \
             server opens a data directory at a time",
            dir.display()
        ))),
        Err(fs::TryLockError::Error(err)) => Err(err.into()),
    }
}

/// Whether the file at `path` is absent or holds no byte.
fn empty_or_absent(path: &Path) -> Result<bool, InternalError> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file() && meta.len() == 0),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err.into()),
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
#[path = "tests/temp.rs"]
pub(crate) mod temp;

#[cfg(test)]
mod tests;
