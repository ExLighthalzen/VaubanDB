//! The catalogue of an on-disk instance — its databases, its tables and its indexes — held in
//! a chain of [`PageKind::Meta`] pages, and the payloads of the DDL records of the journal.
//!
//! The rows of a table live in a heap ([`super::version`]) or in a clustered tree
//! ([`super::clustered`]); what says **which tables exist**, in which database, of which
//! shape and rooted at which page is this file. [`super::DiskStorage`] holds one
//! [`Catalogue`] in memory, answers `databases`, `tables` and `indexes` from it, and writes it
//! back through [`store`] at each DDL statement.
//!
//! # Where the chain starts
//!
//! The walk starts at [`super::control::Control::meta_root`], the head of the chain: a page
//! that another page does not point at, which is why the control block names it. It is `None`
//! until the first DDL statement, which allocates it ([`ensure_root`]); `vauban.ctl` is
//! rewritten and `sync_all`ed at that moment, as the allocator rewrites it when it hands out
//! a page.
//!
//! # Layout of a meta page, little-endian
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 32 | the common header of [`Page`], kind [`PageKind::Meta`] |
//! | 32 | 8 | `next`: the page that follows in the chain, [`NO_NEXT`] at the end |
//! | 40 | 2 | `chunk`: bytes of the catalogue this page carries, at most [`CHUNK_CAPACITY`] |
//! | 42 | `chunk` | that many bytes of the stream below |
//!
//! The catalogue is one byte stream cut into chunks of [`CHUNK_CAPACITY`], one chunk per page:
//! a page is not a record boundary, so an entry may straddle two pages
//! (`a_catalogue_larger_than_one_page_spans_the_chain`). A rewrite that needs fewer pages than
//! the chain holds writes `chunk = 0` in the pages it no longer uses and keeps them chained,
//! which is why [`load`] concatenates the chunks rather than stopping at the first empty one.
//!
//! # Layout of the stream
//!
//! | Size | Field |
//! |---|---|
//! | 1 | [`CATALOGUE_FORMAT_VERSION`] |
//! | 4 | number of databases, then that many database entries |
//! | 4 | number of tables, then that many [`TableEntry`] |
//! | 4 | number of indexes, then that many [`IndexEntry`] |
//!
//! A database entry is a `u32` [`DbId`] then its name, a `u32` length and that many UTF-8
//! bytes. The three lists are written by increasing identifier, which is the order
//! [`crate::Storage::databases`] promises.
//!
//! # The same bytes in the journal
//!
//! A DDL statement appends a record to the journal **before** it touches a page, and the
//! payload of that record is what the redo of [`super::recover`] needs to build the entry
//! again: [`TableEntry::encode`] for [`WalRecordKind::CreateTable`],
//! [`IndexEntry::encode`] for [`WalRecordKind::CreateIndex`], and the identifier alone for the
//! three `Drop` kinds. So a catalogue whose pages stayed in the buffer pool when the process
//! went away is rebuilt from the journal
//! (`super::tests::ddl_survives_reopen_without_checkpoint`), and one whose pages reached
//! `data` is read back from them (`super::tests::ddl_survives_reopen`).

use std::collections::BTreeMap;

use vauban_errors::InternalError;
use vauban_types::{Collation, Len, SqlType, TypeInfo};

use super::DiskStorage;
use super::alloc;
use super::page::{HEADER_SIZE, Lsn, PAGE_SIZE, Page, PageId, PageKind};
use super::wal::{WalRecord, WalRecordKind};
use crate::{DbId, IndexId, IndexShape, KeyColumn, TableId, TableShape};

/// Value written in the first byte of the stream by this build.
pub(crate) const CATALOGUE_FORMAT_VERSION: u8 = 1;

/// Offset, in a meta page, of the identifier of the next page of the chain.
const OFF_NEXT: usize = HEADER_SIZE;

/// Offset, in a meta page, of the length of the chunk it carries.
const OFF_CHUNK_LEN: usize = HEADER_SIZE + 8;

/// Offset, in a meta page, of the first byte of its chunk.
const CHUNK_START: usize = HEADER_SIZE + 10;

/// Bytes of the stream one meta page carries: 8 150.
pub(crate) const CHUNK_CAPACITY: usize = PAGE_SIZE - CHUNK_START;

/// Value written in `next` by the last page of the chain.
const NO_NEXT: u64 = u64::MAX;

/// Value written where a [`PageId`] is optional: the root of a table that is a heap, the root
/// of an index served by the clustered tree of its table.
const NO_PAGE: u64 = u64::MAX;

/// Largest number of pages [`load`] walks before it calls the chain corrupt; 8 150 bytes each,
/// that is 33 MiB of catalogue.
const MAX_CHAIN_PAGES: usize = 4096;

/// What the catalogue knows of one table.
///
/// The roots are the pages the rows are reached through: `first_page` is the head of the heap
/// of a table without a clustered key, and the head of the overflow heap of a clustered one
/// ([`super::clustered::ClusteredTable::first_page`]). `tree_root` and `directory_root` are
/// `None` for a heap table and the two trees of [`super::clustered::ClusteredTable`] for a
/// clustered one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableEntry {
    /// Identifier of the table, unique within the instance.
    pub(crate) table: TableId,
    /// Database the table belongs to.
    pub(crate) db: DbId,
    /// Shape the table was created with, kept verbatim for [`crate::Storage::tables`].
    pub(crate) shape: TableShape,
    /// Head of the heap of the table, or of its overflow heap when it is clustered.
    pub(crate) first_page: PageId,
    /// Root of the tree of the versions, `None` for a table without a clustered key.
    pub(crate) tree_root: Option<PageId>,
    /// Root of the tree that maps a [`crate::RowId`] to its key, `None` for a heap table.
    pub(crate) directory_root: Option<PageId>,
    /// Next [`crate::RowId`] of this table. Written 1 at the creation and moved forward by
    /// [`super::storage_impl`] as the rows are written.
    pub(crate) next_row_id: u64,
}

impl TableEntry {
    /// The bytes of the entry, which are also the payload of a
    /// [`WalRecordKind::CreateTable`] record.
    ///
    /// The four bytes of the table then the eight of `first_page` open the payload, which is
    /// the twelve-byte prefix [`super::recover`] reads to find the heap of a table.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_u32(&mut out, self.table.0);
        put_u64(&mut out, self.first_page.0);
        put_u32(&mut out, self.db.0);
        put_u64(&mut out, self.tree_root.map_or(NO_PAGE, |page| page.0));
        put_u64(&mut out, self.directory_root.map_or(NO_PAGE, |page| page.0));
        put_u64(&mut out, self.next_row_id);
        put_table_shape(&mut out, &self.shape);
        out
    }

    /// Reads an entry back.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for bytes that stop short of a field and for a shape this
    /// build does not read (`a_table_entry_short_of_its_shape_is_corruption`).
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        let mut cursor = Cursor::over(bytes, "a table entry");
        let table = TableId(cursor.u32()?);
        let first_page = PageId(cursor.u64()?);
        let db = DbId(cursor.u32()?);
        let tree_root = optional_page(cursor.u64()?);
        let directory_root = optional_page(cursor.u64()?);
        let next_row_id = cursor.u64()?;
        let shape = take_table_shape(&mut cursor)?;
        Ok(Self {
            table,
            db,
            shape,
            first_page,
            tree_root,
            directory_root,
            next_row_id,
        })
    }
}

/// What the catalogue knows of one index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    /// Identifier of the index, unique within the instance.
    pub(crate) index: IndexId,
    /// Table the index is on.
    pub(crate) table: TableId,
    /// Shape the index was created with, kept verbatim for [`crate::Storage::indexes`].
    pub(crate) shape: IndexShape,
    /// Root of the B+tree of the index, `None` when the index is served by the clustered tree
    /// of its table ([`super::DiskStorage::create_index`]).
    pub(crate) root: Option<PageId>,
}

impl IndexEntry {
    /// The bytes of the entry, which are also the payload of a
    /// [`WalRecordKind::CreateIndex`] record.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_u32(&mut out, self.index.0);
        put_u32(&mut out, self.table.0);
        put_u64(&mut out, self.root.map_or(NO_PAGE, |page| page.0));
        put_index_shape(&mut out, &self.shape);
        out
    }

    /// Reads an entry back.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for bytes that stop short of a field.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        let mut cursor = Cursor::over(bytes, "an index entry");
        let index = IndexId(cursor.u32()?);
        let table = TableId(cursor.u32()?);
        let root = optional_page(cursor.u64()?);
        let shape = take_index_shape(&mut cursor)?;
        Ok(Self {
            index,
            table,
            shape,
            root,
        })
    }
}

/// The databases, tables and indexes of an instance, by identifier.
///
/// The three maps are `BTreeMap`s, so the lists the trait promises sorted by increasing
/// identifier are read in that order without a sort.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalogue {
    /// The databases, by identifier, with the name [`crate::Storage::create_database`] was
    /// given.
    databases: BTreeMap<DbId, String>,
    /// The tables, by identifier.
    tables: BTreeMap<TableId, TableEntry>,
    /// The indexes, by identifier.
    indexes: BTreeMap<IndexId, IndexEntry>,
}

impl Catalogue {
    /// Whether the instance holds `db`.
    pub(crate) fn holds_database(&self, db: DbId) -> bool {
        self.databases.contains_key(&db)
    }

    /// The databases of the instance, by increasing identifier.
    pub(crate) fn databases(&self) -> Vec<(DbId, String)> {
        self.databases
            .iter()
            .map(|(db, name)| (*db, name.clone()))
            .collect()
    }

    /// The entry of `table`, or `None` when the instance does not hold it.
    pub(crate) fn table(&self, table: TableId) -> Option<&TableEntry> {
        self.tables.get(&table)
    }

    /// The entry of `index`, or `None` when the instance does not hold it.
    pub(crate) fn index(&self, index: IndexId) -> Option<&IndexEntry> {
        self.indexes.get(&index)
    }

    /// The tables of `db`, by increasing identifier.
    pub(crate) fn tables_of(&self, db: DbId) -> Vec<&TableEntry> {
        self.tables
            .values()
            .filter(|entry| entry.db == db)
            .collect()
    }

    /// The indexes of `table`, by increasing identifier.
    pub(crate) fn indexes_of(&self, table: TableId) -> Vec<&IndexEntry> {
        self.indexes
            .values()
            .filter(|entry| entry.table == table)
            .collect()
    }

    /// The indexes of the catalogue, by increasing identifier: what the rebuild at the
    /// `open` walks ([`super::DiskStorage::rebuild_indexes`]).
    pub(crate) fn all_indexes(&self) -> Vec<&IndexEntry> {
        self.indexes.values().collect()
    }

    /// The head of the heap of each table, by table: what the redo of [`super::recover`]
    /// attaches its heaps to.
    pub(crate) fn heap_heads(&self) -> BTreeMap<TableId, PageId> {
        self.tables
            .iter()
            .map(|(table, entry)| (*table, entry.first_page))
            .collect()
    }

    /// Adds the database `db` under `name`, replacing an entry of the same identifier.
    pub(crate) fn add_database(&mut self, db: DbId, name: &str) {
        self.databases.insert(db, name.to_string());
    }

    /// Takes the database `db` away, with its tables and their indexes, and answers the
    /// tables that went with it.
    pub(crate) fn remove_database(&mut self, db: DbId) -> Vec<TableEntry> {
        self.databases.remove(&db);
        let tables: Vec<TableId> = self
            .tables
            .values()
            .filter(|entry| entry.db == db)
            .map(|entry| entry.table)
            .collect();
        tables
            .into_iter()
            .filter_map(|table| self.remove_table(table))
            .collect()
    }

    /// Adds a table, replacing an entry of the same identifier.
    pub(crate) fn add_table(&mut self, entry: TableEntry) {
        self.tables.insert(entry.table, entry);
    }

    /// Takes a table away, with its indexes, and answers its entry.
    pub(crate) fn remove_table(&mut self, table: TableId) -> Option<TableEntry> {
        self.indexes.retain(|_, entry| entry.table != table);
        self.tables.remove(&table)
    }

    /// The indexes of `table` that hold a tree of their own, taken away with their table.
    pub(crate) fn index_roots_of(&self, table: TableId) -> Vec<PageId> {
        self.indexes
            .values()
            .filter(|entry| entry.table == table)
            .filter_map(|entry| entry.root)
            .collect()
    }

    /// Adds an index, replacing an entry of the same identifier.
    pub(crate) fn add_index(&mut self, entry: IndexEntry) {
        self.indexes.insert(entry.index, entry);
    }

    /// Takes an index away and answers its entry.
    pub(crate) fn remove_index(&mut self, index: IndexId) -> Option<IndexEntry> {
        self.indexes.remove(&index)
    }

    /// The largest identifier of each kind the catalogue holds: database, table, index.
    ///
    /// [`super::recover`] moves the counters of `vauban.ctl` past them, so that an identifier
    /// the journal names is not handed out a second time.
    pub(crate) fn largest_ids(&self) -> (u64, u64, u64) {
        (
            self.databases.keys().last().map_or(0, |db| u64::from(db.0)),
            self.tables.keys().last().map_or(0, |id| u64::from(id.0)),
            self.indexes.keys().last().map_or(0, |id| u64::from(id.0)),
        )
    }

    /// The byte stream of the module documentation.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![CATALOGUE_FORMAT_VERSION];
        put_u32(&mut out, self.databases.len() as u32);
        for (db, name) in &self.databases {
            put_u32(&mut out, db.0);
            put_text(&mut out, name);
        }
        put_u32(&mut out, self.tables.len() as u32);
        for entry in self.tables.values() {
            let bytes = entry.encode();
            put_u32(&mut out, bytes.len() as u32);
            out.extend_from_slice(&bytes);
        }
        put_u32(&mut out, self.indexes.len() as u32);
        for entry in self.indexes.values() {
            let bytes = entry.encode();
            put_u32(&mut out, bytes.len() as u32);
            out.extend_from_slice(&bytes);
        }
        out
    }

    /// Reads a catalogue back from the stream.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a version other than [`CATALOGUE_FORMAT_VERSION`]
    /// (`a_stream_of_another_version_is_corruption`) and for bytes that stop short of a field.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        let mut cursor = Cursor::over(bytes, "the catalogue");
        let version = cursor.u8()?;
        if version != CATALOGUE_FORMAT_VERSION {
            return Err(InternalError::Corruption(format!(
                "the catalogue of the instance declares format version {version}, this build \
                 reads {CATALOGUE_FORMAT_VERSION}"
            )));
        }
        let mut catalogue = Self::default();
        for _ in 0..cursor.u32()? {
            let db = DbId(cursor.u32()?);
            let name = cursor.text()?;
            catalogue.databases.insert(db, name);
        }
        for _ in 0..cursor.u32()? {
            let len = cursor.u32()? as usize;
            let entry = TableEntry::decode(cursor.bytes(len)?)?;
            catalogue.tables.insert(entry.table, entry);
        }
        for _ in 0..cursor.u32()? {
            let len = cursor.u32()? as usize;
            let entry = IndexEntry::decode(cursor.bytes(len)?)?;
            catalogue.indexes.insert(entry.index, entry);
        }
        Ok(catalogue)
    }

    /// Applies the DDL record `record` and answers whether it changed anything.
    ///
    /// This is the redo of the DDL: it takes the identifiers and the shapes from the record
    /// rather than from the counters, so a reopen rebuilds the very entries the instance had
    /// (`super::tests::ddl_survives_reopen_without_checkpoint`). It is idempotent — a
    /// `CreateTable` replaces the entry of its identifier with the same bytes — which is what
    /// lets the redo run over a catalogue the meta pages already carry.
    ///
    /// A record whose database is not in the catalogue is applied even so: the redo replays
    /// what the instance did, and the DDL of an unknown database was refused when it
    /// happened.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a payload this build does not read.
    pub(crate) fn apply_ddl(&mut self, record: &WalRecord) -> Result<bool, InternalError> {
        match record.kind {
            WalRecordKind::CreateDatabase => {
                let mut cursor = Cursor::over(&record.payload, "a create database record");
                let db = DbId(cursor.u32()?);
                let name = cursor.rest_as_text()?;
                self.add_database(db, &name);
                Ok(true)
            }
            WalRecordKind::DropDatabase => {
                let mut cursor = Cursor::over(&record.payload, "a drop database record");
                let db = DbId(cursor.u32()?);
                self.remove_database(db);
                Ok(true)
            }
            WalRecordKind::CreateTable => {
                self.add_table(TableEntry::decode(&record.payload)?);
                Ok(true)
            }
            WalRecordKind::DropTable => {
                let mut cursor = Cursor::over(&record.payload, "a drop table record");
                let table = TableId(cursor.u32()?);
                self.remove_table(table);
                Ok(true)
            }
            WalRecordKind::CreateIndex => {
                self.add_index(IndexEntry::decode(&record.payload)?);
                Ok(true)
            }
            WalRecordKind::DropIndex => {
                let mut cursor = Cursor::over(&record.payload, "a drop index record");
                let index = IndexId(cursor.u32()?);
                self.remove_index(index);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

/// The payload of a record that names one identifier: the three `Drop` kinds.
pub(crate) fn identifier_payload(id: u32) -> Vec<u8> {
    id.to_le_bytes().to_vec()
}

/// The payload of a [`WalRecordKind::CreateDatabase`] record: the identifier then the name.
pub(crate) fn create_database_payload(db: DbId, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + name.len());
    put_u32(&mut out, db.0);
    out.extend_from_slice(name.as_bytes());
    out
}

/// The head of the chain of meta pages of `storage`, allocated and written to `vauban.ctl`
/// when the instance has not allocated it yet.
///
/// The caller holds the lock of the catalogue, which is what keeps two threads from allocating
/// two roots; the control block is taken under it, the order [`alloc`] documents.
///
/// # Errors
///
/// Those of [`alloc::allocate`], of the buffer pool and of
/// [`super::control::Control::write`].
pub(crate) fn ensure_root(storage: &DiskStorage) -> Result<PageId, InternalError> {
    if let Some(root) = storage.control()?.meta_root {
        return Ok(root);
    }
    let root = alloc::allocate(storage)?;
    format_page(storage, root, Lsn(0))?;
    let mut control = storage.lock_control()?;
    let mut updated = *control;
    updated.meta_root = Some(root);
    updated.write(&storage.control_path())?;
    *control = updated;
    Ok(root)
}

/// Reads the catalogue held by the chain that starts at `root`.
///
/// An instance whose `vauban.ctl` names no root has written no DDL and answers the empty
/// catalogue (`an_instance_without_a_root_loads_an_empty_catalogue`).
///
/// # Errors
///
/// [`InternalError::Corruption`] for a page of the chain that is not a [`PageKind::Meta`] one,
/// for a chain longer than [`MAX_CHAIN_PAGES`] and for a stream this build does not read; the
/// errors of the buffer pool otherwise.
pub(crate) fn load(
    storage: &DiskStorage,
    root: Option<PageId>,
) -> Result<Catalogue, InternalError> {
    let Some(root) = root else {
        return Ok(Catalogue::default());
    };
    let chain = chain(storage, root)?;
    if !chain.whole {
        return Ok(Catalogue::default());
    }
    let mut bytes = Vec::new();
    for id in chain.pages {
        let pin = storage.pool.pin(id)?;
        let chunk = pin.with_page(|page| read_chunk(page, id))?;
        drop(pin);
        bytes.extend_from_slice(&chunk?);
    }
    if bytes.is_empty() {
        return Ok(Catalogue::default());
    }
    Catalogue::decode(&bytes)
}

/// Writes `catalogue` over the chain that starts at `root`, growing the chain when the stream
/// needs more pages than it holds.
///
/// The pages are marked dirty at `lsn`, the record of the DDL statement the caller has just
/// made durable: the page may then leave for `data`, the journal being ahead of it
/// ([`super::buffer::BufferPool::flush`]). The pages the stream no longer fills keep their
/// place in the chain with an empty chunk.
///
/// # Errors
///
/// Those of [`load`] for the walk of the chain, of [`alloc::allocate`] for a page the chain
/// grows by, and of the buffer pool.
pub(crate) fn store(
    storage: &DiskStorage,
    root: PageId,
    catalogue: &Catalogue,
    lsn: Lsn,
) -> Result<(), InternalError> {
    let bytes = catalogue.encode();
    let chunks: Vec<&[u8]> = bytes.chunks(CHUNK_CAPACITY).collect();
    let walk = chain(storage, root)?;
    let mut pages = walk.pages;
    if !walk.whole {
        // The walk stopped on a page the allocator had written and this file had not: its
        // formatting stayed in the buffer pool of an instance that went away. It is written
        // again here, and the pages that used to follow it are lost with its link.
        let unformatted = *pages.last().unwrap_or(&root);
        format_page(storage, unformatted, lsn)?;
    }
    while pages.len() < chunks.len() {
        let fresh = alloc::allocate(storage)?;
        format_page(storage, fresh, lsn)?;
        let previous = *pages.last().unwrap_or(&root);
        link(storage, previous, fresh, lsn)?;
        pages.push(fresh);
    }
    for (position, id) in pages.iter().copied().enumerate() {
        let chunk = chunks.get(position).copied().unwrap_or(&[][..]);
        let pin = storage.pool.pin(id)?;
        pin.with_page_mut(|page| write_chunk(page, chunk))?;
        storage.pool.mark_dirty(id, lsn)?;
        storage.pool.set_in_progress(id, false)?;
        drop(pin);
    }
    Ok(())
}

/// The pages of the chain that starts at `root`, `root` first, and whether each of them reads
/// back as a meta page.
#[derive(Debug)]
struct Chain {
    /// The pages, the root first. The last one is unformatted when `whole` is false.
    pages: Vec<PageId>,
    /// Whether the walk reached the end of the chain without meeting an unformatted page.
    whole: bool,
}

/// What the walk of [`chain`] found on one page.
#[derive(Debug)]
enum Step {
    /// A meta page, and the page it links to.
    Next(Option<PageId>),
    /// A page the allocator wrote and this file did not.
    Unformatted,
}

/// Walks the chain that starts at `root`, stopping at the first page that is not a meta page.
///
/// A page of kind [`PageKind::Free`] is one [`alloc::allocate`] wrote and this file did not:
/// its formatting stayed in the buffer pool of an instance that went away, so the walk stops
/// there with `whole` false. Its link is gone with it, which is why the pages behind it are
/// not walked. Any other kind is [`InternalError::Corruption`]: nothing of this build writes a
/// heap or a tree page where the catalogue says a meta page is.
fn chain(storage: &DiskStorage, root: PageId) -> Result<Chain, InternalError> {
    let mut pages = Vec::new();
    let mut current = Some(root);
    while let Some(id) = current {
        if pages.len() >= MAX_CHAIN_PAGES {
            return Err(InternalError::Corruption(format!(
                "the chain of meta pages rooted at {root} reaches more than {MAX_CHAIN_PAGES} \
                 pages: a link leads back to a page it has already walked"
            )));
        }
        let pin = storage.pool.pin(id)?;
        let step = pin.with_page(|page| match page.kind()? {
            PageKind::Meta => Ok(Step::Next(match read_u64(page, OFF_NEXT) {
                NO_NEXT => None,
                next => Some(PageId(next)),
            })),
            PageKind::Free => Ok(Step::Unformatted),
            other => Err(InternalError::Corruption(format!(
                "page {id} of the chain of the catalogue is a {other:?} page"
            ))),
        })?;
        drop(pin);
        pages.push(id);
        match step? {
            Step::Next(next) => current = next,
            Step::Unformatted => {
                return Ok(Chain {
                    pages,
                    whole: false,
                });
            }
        }
    }
    Ok(Chain { pages, whole: true })
}

/// Writes an empty meta page over `id`, zeroing the bytes past the common header.
fn format_page(storage: &DiskStorage, id: PageId, lsn: Lsn) -> Result<(), InternalError> {
    let pin = storage.pool.pin(id)?;
    pin.with_page_mut(|page| {
        page.0[HEADER_SIZE..].fill(0);
        page.set_kind(PageKind::Meta);
        page.set_slot_count(0);
        page.set_lower(CHUNK_START as u16);
        write_u64(page, OFF_NEXT, NO_NEXT);
    })?;
    storage.pool.mark_dirty(id, lsn)?;
    storage.pool.set_in_progress(id, false)?;
    Ok(())
}

/// Chains `to` after `from`.
fn link(storage: &DiskStorage, from: PageId, to: PageId, lsn: Lsn) -> Result<(), InternalError> {
    let pin = storage.pool.pin(from)?;
    pin.with_page_mut(|page| write_u64(page, OFF_NEXT, to.0))?;
    storage.pool.mark_dirty(from, lsn)?;
    storage.pool.set_in_progress(from, false)?;
    Ok(())
}

/// The chunk the page carries.
fn read_chunk(page: &Page, id: PageId) -> Result<Vec<u8>, InternalError> {
    check_meta(page, id)?;
    let len = usize::from(read_u16(page, OFF_CHUNK_LEN));
    if len > CHUNK_CAPACITY {
        return Err(InternalError::Corruption(format!(
            "meta page {id} announces a chunk of {len} bytes, more than the {CHUNK_CAPACITY} a \
             page carries"
        )));
    }
    Ok(page.0[CHUNK_START..CHUNK_START + len].to_vec())
}

/// Writes `chunk` in the page, its length included.
fn write_chunk(page: &mut Page, chunk: &[u8]) {
    page.0[CHUNK_START..].fill(0);
    // The caller cuts the stream in chunks of `CHUNK_CAPACITY`, so the length fits a `u16`.
    write_u16(page, OFF_CHUNK_LEN, chunk.len() as u16);
    page.0[CHUNK_START..CHUNK_START + chunk.len()].copy_from_slice(chunk);
    page.set_lower(CHUNK_START as u16 + chunk.len() as u16);
}

/// Refuses a page of the chain that is not a meta page.
fn check_meta(page: &Page, id: PageId) -> Result<(), InternalError> {
    let kind = page.kind()?;
    if kind != PageKind::Meta {
        return Err(InternalError::Corruption(format!(
            "page {id} of the chain of the catalogue is a {kind:?} page"
        )));
    }
    Ok(())
}

/// The page identifier a field carries, `None` for [`NO_PAGE`].
fn optional_page(raw: u64) -> Option<PageId> {
    match raw {
        NO_PAGE => None,
        page => Some(PageId(page)),
    }
}

/// Writes the shape of a table: its columns, then its clustered key.
fn put_table_shape(out: &mut Vec<u8>, shape: &TableShape) {
    put_u32(out, shape.columns.len() as u32);
    for column in &shape.columns {
        put_type_info(out, column);
    }
    match &shape.clustered_key {
        None => out.push(0),
        Some(key) => {
            out.push(1);
            put_key(out, key);
        }
    }
}

/// Reads the shape of a table back.
fn take_table_shape(cursor: &mut Cursor<'_>) -> Result<TableShape, InternalError> {
    let count = cursor.u32()? as usize;
    let mut columns = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        columns.push(take_type_info(cursor)?);
    }
    let clustered_key = match cursor.u8()? {
        0 => None,
        1 => Some(take_key(cursor)?),
        other => {
            return Err(InternalError::Corruption(format!(
                "the clustered key of a table entry is flagged {other}, which is neither 0 nor 1"
            )));
        }
    };
    Ok(TableShape {
        columns,
        clustered_key,
    })
}

/// Writes the shape of an index: its key, its uniqueness and its included columns.
fn put_index_shape(out: &mut Vec<u8>, shape: &IndexShape) {
    put_key(out, &shape.columns);
    out.push(u8::from(shape.unique));
    put_u32(out, shape.included.len() as u32);
    for column in &shape.included {
        put_u16(out, *column);
    }
}

/// Reads the shape of an index back.
fn take_index_shape(cursor: &mut Cursor<'_>) -> Result<IndexShape, InternalError> {
    let columns = take_key(cursor)?;
    let unique = match cursor.u8()? {
        0 => false,
        1 => true,
        other => {
            return Err(InternalError::Corruption(format!(
                "the uniqueness of an index entry is flagged {other}, which is neither 0 nor 1"
            )));
        }
    };
    let count = cursor.u32()? as usize;
    let mut included = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        included.push(cursor.u16()?);
    }
    Ok(IndexShape {
        columns,
        unique,
        included,
    })
}

/// Writes a key: its length, then a `u16` column and a direction byte per entry.
fn put_key(out: &mut Vec<u8>, key: &[KeyColumn]) {
    put_u32(out, key.len() as u32);
    for column in key {
        put_u16(out, column.column);
        out.push(u8::from(column.descending));
    }
}

/// Reads a key back.
fn take_key(cursor: &mut Cursor<'_>) -> Result<Vec<KeyColumn>, InternalError> {
    let count = cursor.u32()? as usize;
    let mut key = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let column = cursor.u16()?;
        let descending = match cursor.u8()? {
            0 => false,
            1 => true,
            other => {
                return Err(InternalError::Corruption(format!(
                    "the direction of a key column is {other}, which is neither 0 nor 1"
                )));
            }
        };
        key.push(KeyColumn { column, descending });
    }
    Ok(key)
}

/// Writes a column type: the type itself, the nullability, then the collation.
fn put_type_info(out: &mut Vec<u8>, info: &TypeInfo) {
    put_sql_type(out, info.ty);
    out.push(u8::from(info.nullable));
    match info.collation {
        None => out.push(0),
        Some(collation) => {
            out.push(1);
            put_u32(out, collation.lcid);
            out.push(collation.flags);
            out.push(collation.version);
            out.push(collation.sort_id);
        }
    }
}

/// Reads a column type back.
fn take_type_info(cursor: &mut Cursor<'_>) -> Result<TypeInfo, InternalError> {
    let ty = take_sql_type(cursor)?;
    let nullable = match cursor.u8()? {
        0 => false,
        1 => true,
        other => {
            return Err(InternalError::Corruption(format!(
                "the nullability of a column is flagged {other}, which is neither 0 nor 1"
            )));
        }
    };
    let collation = match cursor.u8()? {
        0 => None,
        1 => Some(Collation {
            lcid: cursor.u32()?,
            flags: cursor.u8()?,
            version: cursor.u8()?,
            sort_id: cursor.u8()?,
        }),
        other => {
            return Err(InternalError::Corruption(format!(
                "the collation of a column is flagged {other}, which is neither 0 nor 1"
            )));
        }
    };
    Ok(TypeInfo {
        ty,
        nullable,
        collation,
    })
}

/// Writes a scalar type: one tag byte, then its declared parameters.
///
/// The tag values are part of the on-disk format: a type added to [`SqlType`] takes a free
/// value, an existing one keeps its own. The `match` is exhaustive, so a variant added to
/// [`SqlType`] stops the build here rather than writing a tag nothing reads.
fn put_sql_type(out: &mut Vec<u8>, ty: SqlType) {
    match ty {
        SqlType::Bit => out.push(1),
        SqlType::TinyInt => out.push(2),
        SqlType::SmallInt => out.push(3),
        SqlType::Int => out.push(4),
        SqlType::BigInt => out.push(5),
        SqlType::Decimal { precision, scale } => {
            out.push(6);
            out.push(precision);
            out.push(scale);
        }
        SqlType::Numeric { precision, scale } => {
            out.push(7);
            out.push(precision);
            out.push(scale);
        }
        SqlType::Float => out.push(8),
        SqlType::Real => out.push(9),
        SqlType::Money => out.push(10),
        SqlType::SmallMoney => out.push(11),
        SqlType::Char(len) => put_len(out, 12, len),
        SqlType::VarChar(len) => put_len(out, 13, len),
        SqlType::NChar(len) => put_len(out, 14, len),
        SqlType::NVarChar(len) => put_len(out, 15, len),
        SqlType::Binary(len) => put_len(out, 16, len),
        SqlType::VarBinary(len) => put_len(out, 17, len),
        SqlType::Date => out.push(18),
        SqlType::Time(scale) => {
            out.push(19);
            out.push(scale);
        }
        SqlType::DateTime => out.push(20),
        SqlType::SmallDateTime => out.push(21),
        SqlType::DateTime2(scale) => {
            out.push(22);
            out.push(scale);
        }
        SqlType::DateTimeOffset(scale) => {
            out.push(23);
            out.push(scale);
        }
        SqlType::UniqueIdentifier => out.push(24),
    }
}

/// Reads a scalar type back.
fn take_sql_type(cursor: &mut Cursor<'_>) -> Result<SqlType, InternalError> {
    let tag = cursor.u8()?;
    Ok(match tag {
        1 => SqlType::Bit,
        2 => SqlType::TinyInt,
        3 => SqlType::SmallInt,
        4 => SqlType::Int,
        5 => SqlType::BigInt,
        6 => SqlType::Decimal {
            precision: cursor.u8()?,
            scale: cursor.u8()?,
        },
        7 => SqlType::Numeric {
            precision: cursor.u8()?,
            scale: cursor.u8()?,
        },
        8 => SqlType::Float,
        9 => SqlType::Real,
        10 => SqlType::Money,
        11 => SqlType::SmallMoney,
        12 => SqlType::Char(take_len(cursor)?),
        13 => SqlType::VarChar(take_len(cursor)?),
        14 => SqlType::NChar(take_len(cursor)?),
        15 => SqlType::NVarChar(take_len(cursor)?),
        16 => SqlType::Binary(take_len(cursor)?),
        17 => SqlType::VarBinary(take_len(cursor)?),
        18 => SqlType::Date,
        19 => SqlType::Time(cursor.u8()?),
        20 => SqlType::DateTime,
        21 => SqlType::SmallDateTime,
        22 => SqlType::DateTime2(cursor.u8()?),
        23 => SqlType::DateTimeOffset(cursor.u8()?),
        24 => SqlType::UniqueIdentifier,
        other => {
            return Err(InternalError::Corruption(format!(
                "a column of the catalogue carries type tag {other}, which this build does not \
                 read"
            )));
        }
    })
}

/// Writes the tag of a type that carries a declared length, then that length.
fn put_len(out: &mut Vec<u8>, tag: u8, len: Len) {
    out.push(tag);
    match len {
        Len::Fixed(n) => {
            out.push(0);
            put_u16(out, n);
        }
        Len::Max => {
            out.push(1);
            put_u16(out, 0);
        }
    }
}

/// Reads a declared length back.
fn take_len(cursor: &mut Cursor<'_>) -> Result<Len, InternalError> {
    let kind = cursor.u8()?;
    let n = cursor.u16()?;
    match kind {
        0 => Ok(Len::Fixed(n)),
        1 => Ok(Len::Max),
        other => Err(InternalError::Corruption(format!(
            "the declared length of a column is flagged {other}, which is neither 0 nor 1"
        ))),
    }
}

/// Appends a little-endian `u16`.
fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a little-endian `u32`.
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a little-endian `u64`.
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a string: its length in bytes, then its UTF-8 bytes.
fn put_text(out: &mut Vec<u8>, text: &str) {
    put_u32(out, text.len() as u32);
    out.extend_from_slice(text.as_bytes());
}

/// Reads the little-endian `u16` at `at` of a page.
fn read_u16(page: &Page, at: usize) -> u16 {
    u16::from_le_bytes([page.0[at], page.0[at + 1]])
}

/// Writes the little-endian `u16` at `at` of a page.
fn write_u16(page: &mut Page, at: usize, value: u16) {
    page.0[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

/// Reads the little-endian `u64` at `at` of a page.
fn read_u64(page: &Page, at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&page.0[at..at + 8]);
    u64::from_le_bytes(raw)
}

/// Writes the little-endian `u64` at `at` of a page.
fn write_u64(page: &mut Page, at: usize, value: u64) {
    page.0[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A read head over a byte stream, which reports what it ran out of.
struct Cursor<'a> {
    /// The bytes being read.
    bytes: &'a [u8],
    /// Position of the next byte.
    at: usize,
    /// What is being read, for the error message.
    what: &'static str,
}

impl<'a> Cursor<'a> {
    /// A head at the first byte of `bytes`.
    fn over(bytes: &'a [u8], what: &'static str) -> Self {
        Self { bytes, at: 0, what }
    }

    /// The next `len` bytes.
    fn bytes(&mut self, len: usize) -> Result<&'a [u8], InternalError> {
        let Some(slice) = self
            .at
            .checked_add(len)
            .and_then(|end| self.bytes.get(self.at..end))
        else {
            return Err(InternalError::Corruption(format!(
                "{} of the instance stops at byte {} of {}, {len} bytes short of a field",
                self.what,
                self.at,
                self.bytes.len()
            )));
        };
        self.at += len;
        Ok(slice)
    }

    /// The next byte.
    fn u8(&mut self) -> Result<u8, InternalError> {
        Ok(self.bytes(1)?[0])
    }

    /// The next little-endian `u16`.
    fn u16(&mut self) -> Result<u16, InternalError> {
        let raw = self.bytes(2)?;
        Ok(u16::from_le_bytes([raw[0], raw[1]]))
    }

    /// The next little-endian `u32`.
    fn u32(&mut self) -> Result<u32, InternalError> {
        let raw = self.bytes(4)?;
        Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    /// The next little-endian `u64`.
    fn u64(&mut self) -> Result<u64, InternalError> {
        let raw = self.bytes(8)?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(raw);
        Ok(u64::from_le_bytes(bytes))
    }

    /// The next string: a `u32` length then that many UTF-8 bytes.
    fn text(&mut self) -> Result<String, InternalError> {
        let len = self.u32()? as usize;
        let raw = self.bytes(len)?;
        Self::utf8(raw, self.what)
    }

    /// The rest of the stream, read as UTF-8.
    fn rest_as_text(&mut self) -> Result<String, InternalError> {
        let rest = self.bytes.get(self.at..).unwrap_or(&[]);
        self.at = self.bytes.len();
        Self::utf8(rest, self.what)
    }

    /// The bytes as a string, a sequence that is not UTF-8 being corruption.
    fn utf8(raw: &[u8], what: &'static str) -> Result<String, InternalError> {
        String::from_utf8(raw.to_vec()).map_err(|error| {
            InternalError::Corruption(format!(
                "{what} of the instance carries a name that is not UTF-8: {error}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use vauban_types::SqlType;

    use super::super::temp::TempDir;
    use super::super::{DiskOptions, DiskStorage};
    use super::*;
    use crate::TxnId;

    /// An instance in a temporary directory, with the guard that removes it.
    fn instance(label: &str) -> (TempDir, DiskStorage) {
        let dir = TempDir::created(label);
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        (dir, storage)
    }

    /// One instance of each of the 24 variants of [`SqlType`], in declaration order.
    ///
    /// The list is written out rather than derived: what forces a variant added to [`SqlType`]
    /// to be handled is the `match` of [`put_sql_type`], which is exhaustive and stops the
    /// build; this list is what checks the tag it takes reads back.
    fn all_types() -> Vec<SqlType> {
        vec![
            SqlType::Bit,
            SqlType::TinyInt,
            SqlType::SmallInt,
            SqlType::Int,
            SqlType::BigInt,
            SqlType::Decimal {
                precision: 18,
                scale: 4,
            },
            SqlType::Numeric {
                precision: 5,
                scale: 0,
            },
            SqlType::Float,
            SqlType::Real,
            SqlType::Money,
            SqlType::SmallMoney,
            SqlType::Char(Len::Fixed(10)),
            SqlType::VarChar(Len::Max),
            SqlType::NChar(Len::Fixed(4000)),
            SqlType::NVarChar(Len::Max),
            SqlType::Binary(Len::Fixed(1)),
            SqlType::VarBinary(Len::Max),
            SqlType::Date,
            SqlType::Time(7),
            SqlType::DateTime,
            SqlType::SmallDateTime,
            SqlType::DateTime2(3),
            SqlType::DateTimeOffset(0),
            SqlType::UniqueIdentifier,
        ]
    }

    /// A table of `columns` columns of type `int`, the first `key` of them clustered.
    fn shape(columns: usize, key: Option<Vec<u16>>) -> TableShape {
        TableShape {
            columns: (0..columns)
                .map(|_| TypeInfo::new(SqlType::Int, true))
                .collect(),
            clustered_key: key.map(|columns| {
                columns
                    .into_iter()
                    .map(|column| KeyColumn {
                        column,
                        descending: column % 2 == 1,
                    })
                    .collect()
            }),
        }
    }

    /// A catalogue of one database, two tables and two indexes.
    fn sample() -> Catalogue {
        let mut catalogue = Catalogue::default();
        catalogue.add_database(DbId(1), "Ålesund");
        catalogue.add_table(TableEntry {
            table: TableId(1),
            db: DbId(1),
            shape: TableShape {
                columns: all_types()
                    .into_iter()
                    .map(|ty| TypeInfo::new(ty, true))
                    .collect(),
                clustered_key: None,
            },
            first_page: PageId(3),
            tree_root: None,
            directory_root: None,
            next_row_id: 1,
        });
        catalogue.add_table(TableEntry {
            table: TableId(2),
            db: DbId(1),
            shape: shape(3, Some(vec![2, 0])),
            first_page: PageId(4),
            tree_root: Some(PageId(5)),
            directory_root: Some(PageId(6)),
            next_row_id: 77,
        });
        catalogue.add_index(IndexEntry {
            index: IndexId(1),
            table: TableId(1),
            shape: IndexShape {
                columns: vec![KeyColumn {
                    column: 0,
                    descending: true,
                }],
                unique: true,
                included: vec![1, 2],
            },
            root: Some(PageId(7)),
        });
        catalogue.add_index(IndexEntry {
            index: IndexId(2),
            table: TableId(2),
            shape: IndexShape {
                columns: vec![
                    KeyColumn {
                        column: 2,
                        descending: false,
                    },
                    KeyColumn {
                        column: 0,
                        descending: true,
                    },
                ],
                unique: false,
                included: Vec::new(),
            },
            root: None,
        });
        catalogue
    }

    #[test]
    fn every_sql_type_reads_back_as_the_column_it_was() {
        let types = all_types();
        assert_eq!(types.len(), 24, "one instance of each variant of SqlType");
        let mut tags = Vec::new();
        for ty in types {
            for nullable in [false, true] {
                let info = TypeInfo::new(ty, nullable);
                let mut bytes = Vec::new();
                put_type_info(&mut bytes, &info);
                tags.push(bytes[0]);
                let mut cursor = Cursor::over(&bytes, "a column");
                assert_eq!(take_type_info(&mut cursor).expect("read back"), info);
                assert_eq!(cursor.at, bytes.len(), "the whole column was read");
            }
        }
        // A tag per variant, and no two variants sharing one: a permutation of two arms of
        // `take_sql_type` is what this catches.
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), 24);
    }

    #[test]
    fn a_collation_reads_back_with_its_four_fields() {
        let info = TypeInfo {
            ty: SqlType::VarChar(Len::Fixed(20)),
            nullable: false,
            collation: Some(Collation {
                lcid: 0x040c,
                flags: 0x1d,
                version: 2,
                sort_id: 52,
            }),
        };
        let mut bytes = Vec::new();
        put_type_info(&mut bytes, &info);
        let read = take_type_info(&mut Cursor::over(&bytes, "a column")).expect("read back");
        assert_eq!(read, info);
        assert_eq!(
            read.collation
                .map(|c| (c.lcid, c.flags, c.version, c.sort_id)),
            Some((0x040c, 0x1d, 2, 52))
        );
        // A column whose collation is `None` reads back the same way.
        let plain = TypeInfo::new(SqlType::Int, true);
        let mut bytes = Vec::new();
        put_type_info(&mut bytes, &plain);
        assert_eq!(
            take_type_info(&mut Cursor::over(&bytes, "a column")).expect("read back"),
            plain
        );
    }

    #[test]
    fn a_catalogue_reads_back_from_its_stream() {
        let catalogue = sample();
        let read = Catalogue::decode(&catalogue.encode()).expect("read the stream back");
        assert_eq!(read, catalogue);
        assert_eq!(read.databases(), vec![(DbId(1), "Ålesund".to_string())]);
        assert_eq!(
            read.tables_of(DbId(1))
                .into_iter()
                .map(|entry| entry.table)
                .collect::<Vec<_>>(),
            vec![TableId(1), TableId(2)]
        );
        assert_eq!(read.indexes_of(TableId(2)).len(), 1);
        assert_eq!(read.index(IndexId(2)).expect("the index").root, None);
        assert_eq!(
            read.table(TableId(2)).expect("the table").tree_root,
            Some(PageId(5))
        );
        assert_eq!(read.largest_ids(), (1, 2, 2));
        assert_eq!(Catalogue::default().largest_ids(), (0, 0, 0));
    }

    #[test]
    fn a_table_entry_opens_with_its_table_and_head_page() {
        let entry = TableEntry {
            table: TableId(9),
            db: DbId(2),
            shape: shape(2, None),
            first_page: PageId(5),
            tree_root: None,
            directory_root: None,
            next_row_id: 1,
        };
        let bytes = entry.encode();
        // The twelve bytes the payload of a `CreateTable` record opens with.
        assert_eq!(&bytes[0..4], &9u32.to_le_bytes());
        assert_eq!(&bytes[4..12], &5u64.to_le_bytes());
        assert_eq!(TableEntry::decode(&bytes).expect("read back"), entry);
    }

    #[test]
    fn a_stream_of_another_version_is_corruption() {
        let mut bytes = sample().encode();
        bytes[0] = 2;
        let err = Catalogue::decode(&bytes).expect_err("version 2 is refused");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("format version 2"), "{err}");
    }

    #[test]
    fn a_table_entry_short_of_its_shape_is_corruption() {
        let bytes = sample().table(TableId(2)).expect("the table").encode();
        for len in [0, 12, bytes.len() - 1] {
            let err = TableEntry::decode(&bytes[..len]).expect_err("a short entry is refused");
            assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
            assert!(err.to_string().contains("short of a field"), "{err}");
        }
        assert!(TableEntry::decode(&bytes).is_ok());
    }

    #[test]
    fn an_instance_without_a_root_loads_an_empty_catalogue() {
        let (_dir, storage) = instance("meta-empty");
        assert_eq!(storage.control().expect("the ctl").meta_root, None);
        assert_eq!(
            load(&storage, None).expect("load"),
            Catalogue::default(),
            "an instance that has written no DDL"
        );
    }

    #[test]
    fn a_catalogue_reads_back_from_the_pages_it_was_written_to() {
        let (_dir, storage) = instance("meta-pages");
        let catalogue = sample();
        let root = ensure_root(&storage).expect("allocate the root");
        assert_eq!(storage.control().expect("the ctl").meta_root, Some(root));
        store(&storage, root, &catalogue, Lsn(0)).expect("store");

        // Read back through the pool, then through the file: the pages are written, not kept.
        assert_eq!(load(&storage, Some(root)).expect("load"), catalogue);
        storage.pool.flush_all().expect("flush the pages");
        assert_eq!(load(&storage, Some(root)).expect("load"), catalogue);
    }

    #[test]
    fn a_catalogue_larger_than_one_page_spans_the_chain() {
        let (_dir, storage) = instance("meta-chain");
        let mut catalogue = Catalogue::default();
        catalogue.add_database(DbId(1), "wide");
        for table in 1..=200u32 {
            catalogue.add_table(TableEntry {
                table: TableId(table),
                db: DbId(1),
                shape: shape(10, None),
                first_page: PageId(u64::from(table)),
                tree_root: None,
                directory_root: None,
                next_row_id: 1,
            });
        }
        let stream = catalogue.encode();
        assert!(
            stream.len() > CHUNK_CAPACITY,
            "{} bytes, one page carries {CHUNK_CAPACITY}",
            stream.len()
        );
        let root = ensure_root(&storage).expect("allocate the root");
        store(&storage, root, &catalogue, Lsn(0)).expect("store");
        let walk = chain(&storage, root).expect("walk the chain");
        assert!(walk.whole);
        assert_eq!(walk.pages.len(), stream.len().div_ceil(CHUNK_CAPACITY));
        assert_eq!(load(&storage, Some(root)).expect("load"), catalogue);

        // A catalogue that shrinks keeps its chain and empties the pages it no longer fills.
        let small = sample();
        store(&storage, root, &small, Lsn(0)).expect("store the smaller one");
        assert_eq!(
            chain(&storage, root).expect("walk the chain").pages.len(),
            walk.pages.len(),
            "the pages stay chained"
        );
        assert_eq!(load(&storage, Some(root)).expect("load"), small);
    }

    #[test]
    fn a_page_of_the_chain_that_is_not_a_meta_page_is_corruption() {
        let (_dir, storage) = instance("meta-kind");
        let root = ensure_root(&storage).expect("allocate the root");
        store(&storage, root, &sample(), Lsn(0)).expect("store");
        let pin = storage.pool.pin(root).expect("pin the root");
        pin.with_page_mut(|page| page.set_kind(PageKind::Heap))
            .expect("change the kind");
        storage.pool.mark_dirty(root, Lsn(0)).expect("mark dirty");
        drop(pin);

        let err = load(&storage, Some(root)).expect_err("a heap page is refused");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("is a Heap page"), "{err}");
    }

    #[test]
    fn the_ddl_records_rebuild_the_entries_they_named() {
        let catalogue = sample();
        let mut replayed = Catalogue::default();
        let records = [
            (
                WalRecordKind::CreateDatabase,
                create_database_payload(DbId(1), "Ålesund"),
            ),
            (
                WalRecordKind::CreateTable,
                catalogue.table(TableId(1)).expect("table 1").encode(),
            ),
            (
                WalRecordKind::CreateTable,
                catalogue.table(TableId(2)).expect("table 2").encode(),
            ),
            (
                WalRecordKind::CreateIndex,
                catalogue.index(IndexId(1)).expect("index 1").encode(),
            ),
            (
                WalRecordKind::CreateIndex,
                catalogue.index(IndexId(2)).expect("index 2").encode(),
            ),
        ];
        for (kind, payload) in &records {
            assert!(
                replayed
                    .apply_ddl(&record(*kind, payload.clone()))
                    .expect("apply")
            );
        }
        assert_eq!(replayed, catalogue, "the DDL records rebuild the catalogue");

        // Replaying them over the catalogue they built changes nothing.
        for (kind, payload) in &records {
            replayed
                .apply_ddl(&record(*kind, payload.clone()))
                .expect("apply again");
        }
        assert_eq!(replayed, catalogue);

        // A row record is not DDL, and a drop takes the indexes of its table with it.
        assert!(
            !replayed
                .apply_ddl(&record(WalRecordKind::Insert, Vec::new()))
                .expect("an insert is no DDL")
        );
        replayed
            .apply_ddl(&record(
                WalRecordKind::DropTable,
                identifier_payload(TableId(1).0),
            ))
            .expect("apply the drop");
        assert_eq!(replayed.table(TableId(1)), None);
        assert_eq!(replayed.index(IndexId(1)), None);
        assert_eq!(replayed.indexes_of(TableId(2)).len(), 1);

        // Dropping the database takes the table that was left and its index.
        replayed
            .apply_ddl(&record(
                WalRecordKind::DropDatabase,
                identifier_payload(DbId(1).0),
            ))
            .expect("apply the drop");
        assert_eq!(replayed, Catalogue::default());
    }

    /// A record of the journal carrying `payload`, numbered as the first one.
    fn record(kind: WalRecordKind, payload: Vec<u8>) -> WalRecord {
        WalRecord {
            kind,
            lsn: Lsn(1),
            prev_lsn: Lsn(0),
            txn: TxnId(0),
            payload,
        }
    }
}
