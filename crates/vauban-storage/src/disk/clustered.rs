//! Row versions in the leaves of a B+tree: a table whose `TableShape::clustered_key` is
//! `Some` puts each version where its key says, instead of at the end of a heap.
//!
//! [`ClusteredTable`] is to [`super::btree::BTree`] what [`super::version::HeapTable`] is to
//! [`super::heap::Heap`]: the MVCC model of the crate root ("Model") over the bytes a page
//! holds. The two differ in where a version lands — the heap takes the first page with room,
//! the tree takes the leaf the key belongs to — and in what `scan` answers: `RowId` order for
//! the heap, key order here, which is the contract of `TableShape::clustered_key`
//! (`scan_follows_clustered_key`).
//!
//! # One entry per version
//!
//! A version of a row is one entry of the tree. Its key is the clustered columns **of that
//! version**, then two tie columns of eight big-endian bytes each: the [`RowId`] of the row
//! and the `seq` of the version. Two rows that share their clustered values are told apart by
//! the `RowId` (`scan_follows_clustered_key`), two versions of one row by the `seq`, so the
//! key of an entry names one version and no other.
//!
//! The payload of the entry carries the version:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 32 | the prefix [`VersionHeader`] writes: `row`, `xmin`, `xmax`, `seq` |
//! | 32 | 4 | `prev`: length of the key of the previous version of the row, `0` when this version starts the chain |
//! | 36 | `prev` | that key, as [`encode_row`] writes its values |
//! | 36 + `prev` | 1 | `0` when the columns follow, `1` when what follows is an overflow stub |
//! | 37 + `prev` | rest | the columns, as [`encode_row`] writes them, or ten bytes of stub |
//!
//! The prefix is the one [`RowChange`] carries, so the journal records of a clustered table
//! are the records of a heap table.
//!
//! # The link to the previous version
//!
//! An entry of the tree ([`super::btree`]) is named by its key, not by an address, and the
//! key of a version holds the clustered columns of **that** version, which an update may have
//! changed; a `seq` alone would therefore not say where to look. The link is the whole key of
//! the previous version,
//! `seq` included as its last column, and [`ClusteredTable::get`] follows it from the current
//! version down to the one the snapshot sees (`get_by_row_id_after_move`). The `seq` of a link
//! is checked to be below the `seq` of the version that carries it, so a chain a damaged page
//! turned into a cycle is [`InternalError::Corruption`] rather than a walk that does not end.
//!
//! # Directory
//!
//! `RowId -> key of its most recent version`, held in a second B+tree of one key column of
//! eight big-endian bytes. The catalogue ([`super::meta`]) is what makes the two roots survive
//! an `open`: this module hands them out ([`ClusteredTable::tree_root`],
//! [`ClusteredTable::directory_root`]) and takes them back ([`ClusteredTable::open`]).
//!
//! The payload of that directory is not the `(PageId, slot)` of the current version, and its
//! pages are not of kind `PageKind::RowIdDir`, for one reason: [`super::btree::split_leaf`]
//! moves entries from one page to another, so an entry of a tree has no address that outlives
//! the next split, and [`super::btree::BTree`] reads and writes the two kinds its
//! `expect_kind` takes, `PageKind::BTreeLeaf` and `PageKind::BTreeInternal`. The payload
//! is the key of the current version, which is what a seek of the clustered tree takes: it is
//! read back after the splits of 400 rows have moved the entries between pages
//! (`splits_keep_the_order_and_the_directory`).
//!
//! # A row too long for a leaf
//!
//! [`MAX_INSERT_BYTES`] is what an entry may weigh, key and payload together. A version whose
//! entry would be heavier puts its columns in a heap of its own — the `overflow` heap this
//! table creates beside its trees — and the payload carries the ten-byte stub of that record
//! instead. A record longer than `super::heap::MAX_INLINE_LEN` then goes to a chain of
//! `PageKind::Overflow` pages, as it does for a heap table, the stub in the leaf naming the
//! heap record that names the chain. With a `varchar(max)` of 4 000 characters and one of
//! 16, the long one is stored as a stub and the short one in the leaf, and both read
//! back (`a_long_row_goes_through_the_overflow_heap_and_reads_back`).
//!
//! # Transactions, journal and pages
//!
//! Same shape as [`super::version::HeapTable`]: a transaction is discovered at its first write
//! and registered `InProgress`, a transaction the registry does not know is `Committed` for
//! the visibility rule, [`ClusteredTable::commit`] appends a `Commit` and `sync_all`s the
//! journal, [`ClusteredTable::rollback`] appends an `Abort` and replays the undo log backwards
//! (`rollback_restore_old_key`). A write appends its journal record before it changes a
//! page (`the_writes_of_a_transaction_are_journalled_in_order`).
//!
//! A write marks the pages **of the two trees** it changes with the LSN of the record that
//! describes it and with the `in_progress` flag ([`BTree::hold_writes`]);
//! [`ClusteredTable::commit`] and [`ClusteredTable::rollback`] lift the flag on the pages the
//! transaction collected, the journal being durable by then. That is the no-steal rule of
//! [`super::version::HeapTable`], brought to the pages of a tree. With one transaction that
//! inserts one row in the leaf which is the root of the tree, the leaf and the leaf of the
//! directory stay out of `data` across a `flush_all` and are written after the commit
//! (`clustered_leaf_stays_out_of_data_until_commit`); likewise for the leaf an update
//! rewrites (`an_updated_leaf_stays_out_of_data_until_commit`) and for the pages a rollback
//! leaves behind (`a_rolled_back_write_lets_its_pages_reach_data`).
//!
//! Two shapes fall **outside** those three, each with its test, as [`super::version`] has the
//! same two on a heap; a shape those tests do not name is a shape this paragraph says nothing
//! about:
//!
//! - a leaf two transactions have written loses the flag at the first `commit`, and that
//!   `commit` makes the journal durable up to its own record, so the entry of the transaction
//!   still running is written along with the other: `data` then holds two entries while one of
//!   the two transactions has no record that ends it
//!   (`a_leaf_shared_by_two_txns_loses_its_flag_at_the_first_commit`). A `rollback` lifts the
//!   flag the same way, leaving in `data` the entry of the transaction that goes on
//!   (`a_rollback_lets_the_leaf_of_a_running_txn_reach_data`). A clustered table holds the
//!   versions of its transactions in the same leaves, so this is the ordinary shape, and
//!   answering it — undo at recovery, or a flag held per transaction — is not implemented;
//! - the pages of the overflow chain of a long row carry no flag: what
//!   [`ClusteredTable::note_page`] marks is the page of the overflow heap that takes the record
//!   of the columns, and a record past [`super::heap::MAX_INLINE_LEN`] is continued on pages
//!   this module does not mark, dirtied at `Lsn(0)`. The payload of an uncommitted long row
//!   therefore reaches `data`, as 40 000 characters show
//!   (`the_overflow_chain_of_an_uncommitted_long_row_is_not_flagged`). The entry that names the
//!   chain stays in a flagged leaf, so those bytes are not reachable as a row.
//!
//! What this build does **not** do:
//!
//! - no redo: the recovery ([`super::recover`]) replays the records of a heap table, not of a
//!   clustered one. [`ClusteredTable::open`] therefore attaches to the pages as they stand,
//!   with an empty transaction registry and counters at 1
//!   (`open_attaches_to_the_trees_that_create_left`). A commit syncs the journal and lifts the
//!   flag; it does not flush the pages, so a crash between the two leaves the record on disk
//!   and the page in the pool, and nothing replays it;
//! - no `vacuum`: a version an update replaced stays in the tree under the key it had, which is
//!   what lets an older snapshot read it (`get_by_row_id_after_move`).
//!
//! An `IndexShape` whose `columns` are the clustered key of the table names the same entries in
//! the same order, so a `seek` on it could be answered from this tree rather than from a
//! second one; this build shares nothing with [`super::index`] yet.

use vauban_errors::InternalError;
use vauban_types::{Len, SqlType, TypeInfo, Value};

use super::DiskStorage;
use super::btree::{BTree, MAX_INSERT_BYTES, TreeEntry, encode_entry_key};
use super::encode::{decode_row, encode_row};
use super::heap::{Heap, Rid};
use super::index::IndexChange;
use super::page::{Lsn, PageId, PageKind};
use super::version::{
    NO_XMAX, TableState, UndoEntry, VERSION_PREFIX_LEN, VersionHeader, VersionView,
};
use super::wal::WalRecordKind;
use super::wal_payload::RowChange;
use crate::{
    Direction, KeyColumn, KeyRange, Row, RowId, Snapshot, TableId, TableShape, TxnId, TxnStatus,
};

/// Offset, in the payload of an entry, of the length of the key of the previous version.
const OFF_PREV_LEN: usize = VERSION_PREFIX_LEN;

/// Size of that length field.
const PREV_LEN_SIZE: usize = 4;

/// Byte that introduces columns written in the entry itself.
const BODY_INLINE: u8 = 0;

/// Byte that introduces the stub of a record of the overflow heap.
const BODY_OVERFLOW: u8 = 1;

/// Size of that stub: the page of the record then its slot.
const STUB_LEN: usize = 10;

/// Number of tie columns the key of an entry carries after the clustered ones: `RowId`, `seq`.
const TIE_COLUMNS: usize = 2;

/// A caller bug: a violated precondition of this module.
fn bug<T>(msg: impl Into<String>) -> Result<T, InternalError> {
    Err(InternalError::Bug(msg.into()))
}

/// A broken invariant of this module: reported rather than repaired blindly.
fn corruption<T>(msg: impl Into<String>) -> Result<T, InternalError> {
    Err(InternalError::Corruption(msg.into()))
}

/// The type of a tie column of a key: eight bytes, not nullable.
///
/// `binary(8)` is compared byte by byte by `vauban_types::compare`, and the big-endian bytes of
/// a `u64` are in the order of the numbers, so the tie columns sort as `RowId` and `seq` do
/// (`scan_follows_clustered_key` reads the two rows of key 1 back in `RowId` order).
fn tie_type() -> TypeInfo {
    TypeInfo::new(SqlType::Binary(Len::Fixed(8)), false)
}

/// The eight big-endian bytes of `value`, as a tie column holds them.
fn tie_value(value: u64) -> Value {
    Value::Bytes(value.to_be_bytes().to_vec())
}

/// Reads back what [`tie_value`] wrote.
fn tie_number(value: &Value) -> Result<u64, InternalError> {
    match value {
        Value::Bytes(bytes) => match <[u8; 8]>::try_from(bytes.as_slice()) {
            Ok(field) => Ok(u64::from_be_bytes(field)),
            Err(_) => corruption(format!(
                "a tie column of a clustered key holds {} bytes, not 8",
                bytes.len()
            )),
        },
        other => corruption(format!(
            "a tie column of a clustered key holds {other:?}, not the eight bytes of a number"
        )),
    }
}

/// Where the columns of a version sit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Body {
    /// In the entry itself, as [`encode_row`] wrote them.
    Inline(Vec<u8>),
    /// In that record of the overflow heap of the table.
    Overflow(Rid),
}

/// The payload of one entry: the prefix of the version, the link to the version before it and
/// its columns.
#[derive(Debug, Clone, PartialEq)]
struct VersionPayload {
    /// The prefix of the version.
    header: VersionHeader,
    /// The key of the previous version of the row, empty when this version starts the chain.
    prev: Vec<Value>,
    /// Where the columns sit.
    body: Body,
}

impl VersionPayload {
    /// The bytes of the payload, in the layout of the module documentation.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.header.write_to(&mut out);
        let mut prev = Vec::new();
        if !self.prev.is_empty() {
            encode_row(&Row(self.prev.clone()), &mut prev);
        }
        let len = u32::try_from(prev.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&prev);
        match &self.body {
            Body::Inline(columns) => {
                out.push(BODY_INLINE);
                out.extend_from_slice(columns);
            }
            Body::Overflow(rid) => {
                out.push(BODY_OVERFLOW);
                out.extend_from_slice(&rid.page.0.to_le_bytes());
                out.extend_from_slice(&rid.slot.to_le_bytes());
            }
        }
        out
    }

    /// Reads back what [`VersionPayload::encode`] wrote.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a payload cut short of one of its fields
    /// (`a_payload_cut_short_of_its_body_is_corruption`) and for a body byte that is neither of
    /// the two this module writes.
    fn decode(bytes: &[u8]) -> Result<Self, InternalError> {
        let header = VersionHeader::read_from(bytes)?;
        let field = field(bytes, OFF_PREV_LEN, PREV_LEN_SIZE)?;
        let len = u32::from_le_bytes([field[0], field[1], field[2], field[3]]);
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let at = OFF_PREV_LEN + PREV_LEN_SIZE;
        let prev = match len {
            0 => Vec::new(),
            _ => decode_row(self::field(bytes, at, len)?)?.0,
        };
        let at = at.saturating_add(len);
        let body = match self::field(bytes, at, 1)?[0] {
            BODY_INLINE => Body::Inline(bytes.get(at + 1..).unwrap_or(&[]).to_vec()),
            BODY_OVERFLOW => {
                let stub = self::field(bytes, at + 1, STUB_LEN)?;
                let page: [u8; 8] = stub[..8].try_into().unwrap_or([0; 8]);
                let slot: [u8; 2] = stub[8..].try_into().unwrap_or([0; 2]);
                Body::Overflow(Rid {
                    page: PageId(u64::from_le_bytes(page)),
                    slot: u16::from_le_bytes(slot),
                })
            }
            other => {
                return corruption(format!(
                    "the payload of a clustered version carries {other} where the byte that \
                     tells its columns from an overflow stub was expected"
                ));
            }
        };
        Ok(Self { header, prev, body })
    }
}

/// The `len` bytes of `bytes` at `at`, or the corruption of a payload cut short.
fn field(bytes: &[u8], at: usize, len: usize) -> Result<&[u8], InternalError> {
    match bytes.get(at..at.saturating_add(len)) {
        Some(slice) => Ok(slice),
        None => corruption(format!(
            "the payload of a clustered version holds {} bytes, too few for the {len} at {at}",
            bytes.len()
        )),
    }
}

/// One version as a seek read it: the key of its entry, its prefix and the whole payload.
#[derive(Debug, Clone)]
struct StoredVersion {
    /// The key of the entry: the clustered columns of this version, the `RowId` and the `seq`.
    key: Vec<Value>,
    /// The prefix, already decoded.
    header: VersionHeader,
    /// The payload of the entry, as the tree holds it.
    payload: Vec<u8>,
}

impl StoredVersion {
    /// The version this entry of the tree carries.
    fn read(entry: TreeEntry) -> Result<Self, InternalError> {
        let (key, payload) = entry;
        Ok(Self {
            header: VersionHeader::read_from(&payload)?,
            key,
            payload,
        })
    }

    /// The payload, decoded.
    fn parsed(&self) -> Result<VersionPayload, InternalError> {
        VersionPayload::decode(&self.payload)
    }
}

/// One table whose rows sit in a B+tree, in the order of its clustered key.
///
/// The structure borrows the instance and holds the tree of the versions, the directory of the
/// most recent version of each row, the heap the columns of a long row go to, the shape of the
/// table and the state the instance kept for it. `super::storage_impl` answers
/// [`crate::Storage`] by calling these methods, which take `&mut self` — one writer at a time
/// on a table, the serialisation being the lock of [`TableState`] that caller holds.
#[derive(Debug)]
pub(crate) struct ClusteredTable<'storage> {
    /// The instance the pages belong to.
    storage: &'storage DiskStorage,
    /// The table these rows belong to.
    table: TableId,
    /// Shape of the table: the arity a [`Row`] must have and the clustered key.
    shape: TableShape,
    /// Positions, in a [`Row`], of the clustered columns, in key order.
    key_columns: Vec<usize>,
    /// The versions, keyed by clustered columns then `RowId` then `seq`.
    tree: BTree<'storage>,
    /// `RowId -> key of its most recent version`.
    directory: BTree<'storage>,
    /// The columns of the versions whose entry would be past [`MAX_INSERT_BYTES`].
    overflow: Heap<'storage>,
    /// The counters, the undo logs and the maintenance log, which the instance holds between
    /// two calls ([`TableState`]). Its `directory` field is the heap store's; this one keeps
    /// its own in [`ClusteredTable::directory`].
    state: TableState,
}

impl<'storage> ClusteredTable<'storage> {
    /// Creates the table: one tree for the versions, one for the directory, and a heap over
    /// `first_page` for the columns of a long row.
    ///
    /// `first_page` comes from [`super::alloc::allocate`]; the roots of the two trees are
    /// allocated here and answered by [`ClusteredTable::tree_root`] and
    /// [`ClusteredTable::directory_root`].
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a `shape` whose `clustered_key` is `None`
    /// (`a_table_without_a_clustered_key_is_a_bug`) or names a column outside the shape; the
    /// errors of [`BTree::create`] and [`Heap::create`] otherwise.
    pub(crate) fn create(
        storage: &'storage DiskStorage,
        table: TableId,
        first_page: PageId,
        shape: TableShape,
    ) -> Result<Self, InternalError> {
        let (columns, types) = Self::tree_key(&shape, table)?;
        let tree = BTree::create(storage, &columns, &types)?;
        let directory = BTree::create(storage, &Self::directory_key(), &[tie_type()])?;
        let heap = Heap::create(storage, table, first_page)?;
        Self::over(storage, table, shape, tree, directory, heap)
    }

    /// Attaches to the trees rooted at `roots` — the tree of the versions then the directory —
    /// and to the heap whose head page is `first_page`, leaving the pages as they stand.
    ///
    /// The counters start at 1: what a table reopened this way answers is asserted in
    /// `open_attaches_to_the_trees_that_create_left`, where a `scan` serves the rows the pages
    /// hold and the next `insert` hands out `RowId(1)` a second time.
    /// [`super::DiskStorage::with_rows`] is what moves them forward from what the recovery
    /// found before a call of the engine reaches the table; the roots this call asks for are
    /// kept by the catalogue ([`super::meta`]).
    ///
    /// # Errors
    ///
    /// Those of [`ClusteredTable::create`], the allocation apart.
    pub(crate) fn open(
        storage: &'storage DiskStorage,
        table: TableId,
        first_page: PageId,
        roots: (PageId, PageId),
        shape: TableShape,
    ) -> Result<Self, InternalError> {
        let (columns, types) = Self::tree_key(&shape, table)?;
        let tree = BTree::open(storage, roots.0, &columns, &types)?;
        let directory = BTree::open(storage, roots.1, &Self::directory_key(), &[tie_type()])?;
        let heap = Heap::open(storage, table, first_page);
        Self::over(storage, table, shape, tree, directory, heap)
    }

    /// The table of the redo: attaches to the two roots and the head page the catalogue names,
    /// formatting each root as the empty leaf [`super::recover::redo`] would have found it when
    /// the page a split or an insert left dirty stayed in the pool of the instance that
    /// crashed. The redo replays the row records of the winners on it with
    /// [`ClusteredTable::replay_one`] before the first call of the engine reaches the table
    /// (`a_clustered_table_reopened_without_a_checkpoint_is_redone`).
    ///
    /// # Errors
    ///
    /// Those of [`BTree::open`] and of [`ClusteredTable::over`] for a root that holds pages of
    /// another kind than a free page or a B+tree page, and of [`Self::tree_key`] for a shape
    /// without a clustered key.
    pub(crate) fn redo_open(
        storage: &'storage DiskStorage,
        table: TableId,
        first_page: PageId,
        roots: (PageId, PageId),
        shape: TableShape,
    ) -> Result<Self, InternalError> {
        let (columns, types) = Self::tree_key(&shape, table)?;
        let tree = Self::redo_tree(storage, roots.0, &columns, &types)?;
        let directory = Self::redo_tree(storage, roots.1, &Self::directory_key(), &[tie_type()])?;
        let heap = Heap::open(storage, table, first_page);
        let mut table = Self::over(storage, table, shape, tree, directory, heap)?;
        table.rebuild_directory_from_tree()?;
        Ok(table)
    }

    /// Rebuilds the RowId directory by walking the tree: the entry of the largest `seq` of
    /// each row becomes its directory entry. A clustered table reopened after a crash without
    /// a checkpoint finds its versions in `data` — the pages a commit flushed — and its
    /// directory lost with the pool; the redo replays what the journal adds on top of it, and
    /// `replay_one` uses the directory to tell a version already in the tree from one the
    /// journal adds.
    fn rebuild_directory_from_tree(&mut self) -> Result<(), InternalError> {
        let mut newest: std::collections::BTreeMap<RowId, (u64, Vec<Value>)> =
            std::collections::BTreeMap::new();
        for (key, payload_bytes) in self.tree.seek(&KeyRange::Full, Direction::Forward)? {
            let payload = VersionPayload::decode(&payload_bytes)?;
            let entry = newest
                .entry(payload.header.row)
                .or_insert((payload.header.seq, key.clone()));
            if payload.header.seq > entry.0 {
                *entry = (payload.header.seq, key.clone());
            }
        }
        for (row, (_, key)) in newest {
            self.set_directory(row, &key)?;
        }
        Ok(())
    }

    /// [`BTree::open`] on `root`, after formatting it as an empty leaf when the page is
    /// [`PageKind::Free`]: a head page whose formatting stayed in the pool of the instance that
    /// crashed reads back as the free page [`super::alloc::allocate`] wrote, and the redo
    /// replays the writes of the journal on it, as [`super::recover::RedoHeap::over`] does for
    /// a heap.
    fn redo_tree(
        storage: &'storage DiskStorage,
        root: PageId,
        columns: &[KeyColumn],
        types: &[TypeInfo],
    ) -> Result<BTree<'storage>, InternalError> {
        let kind = {
            let pin = storage.pool.pin(root)?;
            let kind = pin.with_page(|page| page.kind())?;
            drop(pin);
            kind
        };
        if kind? == PageKind::Free {
            let pin = storage.pool.pin(root)?;
            pin.with_page_mut(|page| {
                super::btree::init_leaf(page);
            })?;
            // The pin's guard drops the page from the frame; a pin that leaves a dirty page
            // behind would hold back an eviction. The redo writes the entries the journal
            // names, and each write dirties the page it lands on, so the formatting reaches
            // `data` with them.
        }
        BTree::open(storage, root, columns, types)
    }

    /// Opens the table over the three structures that the redo of [`super::recover`] has built or
    /// formatted: the tree, the directory and the overflow heap. The counters start at 1 and the
    /// registry is empty, as for a fresh table.
    pub(crate) fn over(
        storage: &'storage DiskStorage,
        table: TableId,
        shape: TableShape,
        tree: BTree<'storage>,
        directory: BTree<'storage>,
        overflow: Heap<'storage>,
    ) -> Result<Self, InternalError> {
        let key_columns = Self::key_columns(&shape, table)?;
        Ok(Self {
            storage,
            table,
            shape,
            key_columns,
            tree,
            directory,
            overflow,
            state: TableState::default(),
        })
    }

    /// Puts the state the instance kept for this table back into it.
    ///
    /// [`super::DiskStorage::with_rows`] moves the state in here and takes it back with
    /// [`ClusteredTable::into_state`] when the call returns, as it does for a heap table.
    pub(crate) fn with_state(mut self, state: TableState) -> Self {
        self.state = state;
        self
    }

    /// Hands the state of the table back to the instance.
    pub(crate) fn into_state(self) -> TableState {
        self.state
    }

    /// Takes the counters of the recovery, so a [`RowId`] or a serial the journal names is not
    /// handed out a second time.
    ///
    /// The two counters are those of the instance, one past the largest the journal names
    /// ([`super::recover::Recovery`]): a table of a reopened instance starts above what any of
    /// its rows carries.
    pub(crate) fn resume_after_recovery(&mut self, next_row_id: u64, next_seq: u64) {
        self.state.next_row_id = self.state.next_row_id.max(next_row_id);
        self.state.next_seq = self.state.next_seq.max(next_seq);
        self.state.resumed();
    }

    /// Whether the state of this table still has to be rebuilt from what the recovery found.
    pub(crate) fn state_needs_resume(&self) -> bool {
        self.state.needs_resume()
    }

    /// The table these rows belong to.
    pub(crate) fn table(&self) -> TableId {
        self.table
    }

    /// The shape the rows of this table must have.
    pub(crate) fn shape(&self) -> &TableShape {
        &self.shape
    }

    /// The root of the tree of the versions; it moves when a split reaches the root.
    pub(crate) fn tree_root(&self) -> PageId {
        self.tree.root()
    }

    /// The root of the directory tree.
    pub(crate) fn directory_root(&self) -> PageId {
        self.directory.root()
    }

    /// The head page of the heap the columns of a long row go to.
    pub(crate) fn first_page(&self) -> PageId {
        self.overflow.first_page()
    }

    // ---------------------------------------------------------------------- Rows

    /// Adds a logical row holding `row`, created by `txn`, and answers its fresh [`RowId`].
    ///
    /// The entry of the version goes to the leaf its clustered columns belong to; the journal
    /// record of the write is appended before the tree is changed. The row is visible to `txn`
    /// at once and to a later snapshot once `txn` commits
    /// (`an_insert_is_visible_to_its_writer_then_to_a_later_snapshot`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a `row` whose arity is not the one of
    /// [`ClusteredTable::shape`], for a transaction that already committed or rolled back and
    /// for the reserved `TxnId(u64::MAX)`; the errors of [`BTree::insert`] — among them an
    /// entry past [`MAX_INSERT_BYTES`] whose weight is in its **key**, the columns having gone
    /// to the overflow heap — of [`Heap::insert`] and of the journal otherwise.
    pub(crate) fn insert(&mut self, txn: TxnId, row: &Row) -> Result<RowId, InternalError> {
        check_txn(txn)?;
        self.check_arity(row)?;
        self.check_writable(txn)?;
        let id = RowId(self.state.next_row_id);
        let Some(next) = self.state.next_row_id.checked_add(1) else {
            return bug(format!("row id space of table {} exhausted", self.table));
        };
        self.begin_write(txn)?;
        let header = VersionHeader {
            row: id,
            xmin: txn,
            xmax: None,
            seq: self.take_seq()?,
        };
        let lsn = self.log_row(WalRecordKind::Insert, txn, &header, Some(row))?;
        self.hold_trees(lsn);
        let key = self.key_of(row, id, header.seq)?;
        self.write_version(txn, lsn, &header, &key, &[], row)?;
        self.state.next_row_id = next;
        self.set_directory(id, &key)?;
        self.note_index_change(IndexChange::Added {
            row: id,
            seq: header.seq,
            data: row.clone(),
        });
        self.log(txn, UndoEntry::Insert(id))?;
        self.claim_tree_pages(txn)?;
        Ok(id)
    }

    /// Replaces the content of `id` with `row` on behalf of `txn`, keeping the [`RowId`].
    ///
    /// The current version takes `xmax = txn` where it sits, and a new version holding `row` is
    /// inserted under the key `row` gives it, which is another leaf when the clustered columns
    /// changed; the directory then names the new entry. A snapshot that has not settled `txn`
    /// keeps reading the old version, under the old key (`update_key_moves_row_same_row_id`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for the wrong arity, an unknown row, a current version that
    /// already carries an `xmax` or that was created by another transaction still in progress,
    /// a finished transaction and the reserved `TxnId(u64::MAX)`.
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
        // One record for the pair (`xmax` on the replaced version, the new version), so the two
        // writes are described by the record that names them both.
        let lsn = self.log_row(WalRecordKind::Update, txn, &header, Some(row))?;
        self.hold_trees(lsn);
        self.set_xmax(&current, Some(txn))?;
        let key = self.key_of(row, id, header.seq)?;
        self.write_version(txn, lsn, &header, &key, &current.key, row)?;
        self.set_directory(id, &key)?;
        self.note_index_change(IndexChange::Added {
            row: id,
            seq: header.seq,
            data: row.clone(),
        });
        self.log(txn, UndoEntry::Update(id))?;
        self.claim_tree_pages(txn)
    }

    /// Deletes the logical row `id` on behalf of `txn`: `xmax = txn` on its current version.
    ///
    /// The row is hidden from `txn` at once and from a later snapshot once `txn` commits; the
    /// entry stays where it is, so a snapshot that has not settled `txn` still reads it
    /// (`a_delete_hides_the_row_from_the_snapshot_that_settles_it`).
    ///
    /// # Errors
    ///
    /// Those of [`ClusteredTable::update`], the arity apart.
    pub(crate) fn delete(&mut self, txn: TxnId, id: RowId) -> Result<(), InternalError> {
        check_txn(txn)?;
        let current = self.current(id)?;
        self.check_current(txn, &current)?;
        self.check_writable(txn)?;
        self.begin_write(txn)?;
        // The record names the version being hidden: its `seq` and its `xmin`, with the `xmax`
        // the write sets.
        let hidden = VersionHeader {
            xmax: Some(txn),
            ..current.header
        };
        let lsn = self.log_row(WalRecordKind::Delete, txn, &hidden, None)?;
        self.hold_trees(lsn);
        self.set_xmax(&current, Some(txn))?;
        self.log(txn, UndoEntry::Delete(id))?;
        self.claim_tree_pages(txn)
    }

    /// The content of `id` as `snap` sees it, `None` when no version of the row is visible.
    ///
    /// The walk starts at the version the directory names and follows the link of each payload
    /// back through the chain, so a snapshot that does not settle the writer of the current
    /// version reads the one before it, wherever the key of that one put it
    /// (`get_by_row_id_after_move`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a link that names an entry the tree does not hold and
    /// for a link whose `seq` is not below the one of the version carrying it; the errors of
    /// [`BTree::seek`] and of [`decode_row`] otherwise.
    pub(crate) fn get(&self, snap: &Snapshot, id: RowId) -> Result<Option<Row>, InternalError> {
        let status = |t| self.status(t);
        let Some(mut key) = self.directory_entry(id)? else {
            return Ok(None);
        };
        loop {
            let stored = self.entry_at(&key, id)?;
            let payload = stored.parsed()?;
            if snap.is_visible(payload.header.xmin, payload.header.xmax, &status) {
                return self.columns_of(&payload).map(Some);
            }
            if payload.prev.is_empty() {
                return Ok(None);
            }
            let previous = seq_of(&payload.prev)?;
            if previous >= payload.header.seq {
                return corruption(format!(
                    "version {} of row {id} of table {} links back to version {previous}, which \
                     is not older than it",
                    payload.header.seq, self.table
                ));
            }
            key = payload.prev;
        }
    }

    /// The rows `snap` sees, in the order of the clustered key, one entry per visible row.
    ///
    /// The order is the contract of `TableShape::clustered_key`: `NULL` first,
    /// `vauban_types::compare` with the collation of each column, reversed on a `descending`
    /// column, and the [`RowId`] as the tie between two rows that share their key
    /// (`scan_follows_clustered_key`, `nulls_first_descending_last`). The rows are copied out of
    /// the pages before the call returns, so a write that follows it does not reach the caller.
    ///
    /// # Errors
    ///
    /// Those of [`BTree::seek`] and of [`decode_row`].
    pub(crate) fn scan(&self, snap: &Snapshot) -> Result<Vec<(RowId, Row)>, InternalError> {
        let status = |t| self.status(t);
        let mut rows = Vec::new();
        for (_, payload) in self.tree.seek(&KeyRange::Full, Direction::Forward)? {
            let payload = VersionPayload::decode(&payload)?;
            if snap.is_visible(payload.header.xmin, payload.header.xmax, &status) {
                rows.push((payload.header.row, self.columns_of(&payload)?));
            }
        }
        Ok(rows)
    }

    /// The rows of `range` that `snap` sees, in the order `dir` asks for, read from the tree
    /// of the table.
    ///
    /// This is what [`crate::Storage::seek`] answers for an index that **shares** the tree of
    /// its clustered table ([`super::DiskStorage::create_index`]): the key of the tree is the
    /// clustered key followed by the two tie columns, so a bound of the clustered key alone is
    /// the prefix [`BTree::seek`] reads.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a bound of more values than the clustered key has columns;
    /// the errors of [`BTree::seek`] and of [`decode_row`] otherwise.
    pub(crate) fn seek(
        &self,
        snap: &Snapshot,
        range: &KeyRange,
        dir: Direction,
    ) -> Result<Vec<(RowId, Row)>, InternalError> {
        let status = |t| self.status(t);
        let mut rows = Vec::new();
        for (_, payload) in self.tree.seek(range, dir)? {
            let payload = VersionPayload::decode(&payload)?;
            if snap.is_visible(payload.header.xmin, payload.header.xmax, &status) {
                rows.push((payload.header.row, self.columns_of(&payload)?));
            }
        }
        Ok(rows)
    }

    /// The next [`RowId`] the table hands out, which the catalogue keeps between two opens.
    pub(crate) fn next_row_id(&self) -> u64 {
        self.state.next_row_id
    }

    /// Checks what [`ClusteredTable::insert`] checks, without writing anything.
    ///
    /// Called by [`crate::Storage::insert`] before the uniqueness of the indexes, the order
    /// [`super::version::HeapTable::check_insert`] documents.
    ///
    /// # Errors
    ///
    /// Those of [`ClusteredTable::insert`], the write apart.
    pub(crate) fn check_insert(&self, txn: TxnId, row: &Row) -> Result<(), InternalError> {
        check_txn(txn)?;
        self.check_arity(row)?;
        self.check_writable(txn)
    }

    /// Checks what [`ClusteredTable::update`] checks, without writing anything.
    ///
    /// Called by [`crate::Storage::update`] before the uniqueness of the indexes, the order
    /// [`super::version::HeapTable::check_update`] documents.
    ///
    /// # Errors
    ///
    /// Those of [`ClusteredTable::update`], the write apart.
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
    /// marked (`clustered_leaf_stays_out_of_data_until_commit`), which are the ones
    /// [`super::version::TxnState::pages`] collected.
    ///
    /// The records of the transaction are on disk when this returns. A transaction this table
    /// has not seen write anything is committed without a record, the `Begin` being appended at
    /// the first write.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a transaction that already committed or rolled back and for
    /// the reserved `TxnId(u64::MAX)`; the errors of the journal and of the buffer pool.
    pub(crate) fn commit(&mut self, txn: TxnId) -> Result<(), InternalError> {
        check_txn(txn)?;
        self.finish_in_journal(txn, WalRecordKind::Commit)?;
        self.finish(txn, TxnStatus::Committed)
    }

    /// Applies to this table an end of transaction the instance has already journalled.
    ///
    /// Same contract as [`super::version::HeapTable::finish`]: one record per transaction and
    /// per instance, then this call on each table the transaction wrote in.
    ///
    /// # Errors
    ///
    /// Those of [`ClusteredTable::rollback`] for the undo, and of the buffer pool for the
    /// `in_progress` flag.
    pub(crate) fn finish(&mut self, txn: TxnId, status: TxnStatus) -> Result<(), InternalError> {
        let writes = self.finish_txn(txn);
        if status == TxnStatus::Aborted {
            for entry in writes.into_iter().rev() {
                self.undo(txn, entry)?;
            }
        }
        self.release_pages(txn)
    }

    /// Marks `txn` as `Aborted`, appends a [`WalRecordKind::Abort`] and replays its undo log
    /// backwards.
    ///
    /// An `Insert` loses its entry, an `Update` loses the entry it created and gives the
    /// replaced version its `xmax` back — which puts the row back under its old key
    /// (`rollback_restore_old_key`) — and a `Delete` gives back the `xmax` of the version
    /// it hid. The [`RowId`] and the `seq` of what is taken away are not handed out again.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] as in [`ClusteredTable::commit`]; [`InternalError::Corruption`]
    /// when the chain of a row does not end with the write being undone, which the
    /// preconditions of `update` and `delete` rule out.
    pub(crate) fn rollback(&mut self, txn: TxnId) -> Result<(), InternalError> {
        check_txn(txn)?;
        self.finish_in_journal(txn, WalRecordKind::Abort)?;
        self.finish(txn, TxnStatus::Aborted)
    }

    /// The status of `t` as [`Snapshot::is_visible`] must see it: the registered status, or
    /// `Committed` for a transaction this table has not seen.
    pub(crate) fn status(&self, t: TxnId) -> TxnStatus {
        self.storage.txn_status(t)
    }

    // ------------------------------------------------------ Index maintenance

    /// The versions the tree holds, of the row `only` or of the whole table, by increasing
    /// `(RowId, seq)`.
    ///
    /// Same view as [`super::version::HeapTable::versions`], so that `super::index` reads a
    /// clustered table through [`super::index::VersionSource`] as it reads a heap: this is
    /// what lets a non-clustered index be created on, and kept up to date over, a clustered
    /// table (`super::tests::index_seek_on_clustered_table`).
    ///
    /// # Errors
    ///
    /// Those of [`BTree::seek`] and of [`decode_row`].
    pub(crate) fn versions(&self, only: Option<RowId>) -> Result<Vec<VersionView>, InternalError> {
        let mut versions = Vec::new();
        for (_, payload) in self.tree.seek(&KeyRange::Full, Direction::Forward)? {
            let payload = VersionPayload::decode(&payload)?;
            if only.is_some_and(|row| row != payload.header.row) {
                continue;
            }
            versions.push(VersionView {
                row: payload.header.row,
                seq: payload.header.seq,
                xmin: payload.header.xmin,
                xmax: payload.header.xmax,
                data: self.columns_of(&payload)?,
            });
        }
        versions.sort_by_key(|version| (version.row, version.seq));
        Ok(versions)
    }

    /// The tail of the chain of `id`, regardless of the statuses: the transaction that wrote
    /// the most recent state and its content, `None` for a row the directory does not hold.
    ///
    /// No snapshot is involved: this is what the caller reads to detect a write conflict, the
    /// contract of [`crate::Storage::latest_version`].
    ///
    /// # Errors
    ///
    /// Those of [`BTree::seek`] and of [`decode_row`].
    pub(crate) fn latest_version(&self, id: RowId) -> Result<Option<(TxnId, Row)>, InternalError> {
        let Some(key) = self.directory_entry(id)? else {
            return Ok(None);
        };
        let stored = self.entry_at(&key, id)?;
        let payload = stored.parsed()?;
        let writer = payload.header.xmax.unwrap_or(payload.header.xmin);
        Ok(Some((writer, self.columns_of(&payload)?)))
    }

    /// Takes the maintenance log of the writes made since the previous call.
    ///
    /// Same shape as [`super::version::HeapTable::take_index_changes`]: an `insert` and an
    /// `update` record the version they create, a `delete` records nothing (visibility hides
    /// it), and the undo of a rollback records what it takes away.
    pub(crate) fn take_index_changes(&mut self) -> Vec<IndexChange> {
        std::mem::take(&mut self.state.index_log)
    }

    /// Records one entry in the maintenance log.
    fn note_index_change(&mut self, change: IndexChange) {
        self.state.index_log.push(change);
    }

    /// Records the removal of a version the undo of a rollback takes out of the tree.
    ///
    /// # Errors
    ///
    /// Those of [`decode_row`] on the columns of the version.
    fn note_undone_version(&mut self, removed: &StoredVersion) -> Result<(), InternalError> {
        let payload = removed.parsed()?;
        let change = IndexChange::Removed {
            row: removed.header.row,
            seq: removed.header.seq,
            data: self.columns_of(&payload)?,
        };
        self.note_index_change(change);
        Ok(())
    }

    // ---------------------------------------------------------------- The key

    /// The key columns and the column types of the tree of the versions: the columns of the
    /// table, then the two tie columns.
    fn tree_key(
        shape: &TableShape,
        table: TableId,
    ) -> Result<(Vec<KeyColumn>, Vec<TypeInfo>), InternalError> {
        let clustered = Self::clustered_key(shape, table)?;
        let mut types = shape.columns.clone();
        let mut columns = clustered.to_vec();
        for _ in 0..TIE_COLUMNS {
            let position = u16::try_from(types.len()).unwrap_or(u16::MAX);
            columns.push(KeyColumn {
                column: position,
                descending: false,
            });
            types.push(tie_type());
        }
        Ok((columns, types))
    }

    /// The key of the directory tree: the eight bytes of a [`RowId`], ascending.
    fn directory_key() -> Vec<KeyColumn> {
        vec![KeyColumn {
            column: 0,
            descending: false,
        }]
    }

    /// The clustered key of `shape`, or the bug of a table without one.
    fn clustered_key(shape: &TableShape, table: TableId) -> Result<&[KeyColumn], InternalError> {
        match &shape.clustered_key {
            Some(key) => Ok(key),
            None => bug(format!(
                "table {table} has no clustered key: a table without one is a heap table"
            )),
        }
    }

    /// Where each clustered column sits in a [`Row`], in key order.
    fn key_columns(shape: &TableShape, table: TableId) -> Result<Vec<usize>, InternalError> {
        let mut out = Vec::new();
        for column in Self::clustered_key(shape, table)? {
            let position = usize::from(column.column);
            if position >= shape.columns.len() {
                return bug(format!(
                    "the clustered key of table {table} names the column {position}, outside \
                     its {} columns",
                    shape.columns.len()
                ));
            }
            out.push(position);
        }
        Ok(out)
    }

    /// The key of the entry of one version: its clustered columns, its [`RowId`], its `seq`.
    fn key_of(&self, row: &Row, id: RowId, seq: u64) -> Result<Vec<Value>, InternalError> {
        let mut key = Vec::with_capacity(self.key_columns.len() + TIE_COLUMNS);
        for &position in &self.key_columns {
            match row.0.get(position) {
                Some(value) => key.push(value.clone()),
                None => {
                    return bug(format!(
                        "a row of table {} holds {} values, too few for its clustered column \
                         {position}",
                        self.table,
                        row.0.len()
                    ));
                }
            }
        }
        key.push(tie_value(id.0));
        key.push(tie_value(seq));
        Ok(key)
    }

    // ---------------------------------------------------------------- Internals

    /// Checks the arity of `row` against the shape of the table.
    fn check_arity(&self, row: &Row) -> Result<(), InternalError> {
        let arity = self.shape.columns.len();
        if row.0.len() != arity {
            return bug(format!(
                "row has {} values, table {} has {arity} columns",
                row.0.len(),
                self.table
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
                header.row, self.table
            ));
        }
        if header.xmin != txn && self.status(header.xmin) != TxnStatus::Committed {
            return bug(format!(
                "row {} of table {} is being written by transaction {}, which is {:?}",
                header.row,
                self.table,
                header.xmin,
                self.status(header.xmin)
            ));
        }
        Ok(())
    }

    /// Registers `txn` as `InProgress` if this table had not seen it yet, appending the
    /// [`WalRecordKind::Begin`] record of its first write.
    fn begin_write(&mut self, txn: TxnId) -> Result<(), InternalError> {
        self.storage.begin_txn(txn, self.table)?;
        self.state.txns.entry(txn).or_default();
        Ok(())
    }

    /// Appends the record that ends `txn` and `sync_all`s the journal, when `txn` wrote.
    fn finish_in_journal(&mut self, txn: TxnId, kind: WalRecordKind) -> Result<(), InternalError> {
        self.storage.end_txn(txn, kind).map(|_| ())
    }

    /// Appends the record of one row write and answers its LSN.
    ///
    /// Called **before** the pages are changed. The payload is the one a heap table writes
    /// ([`RowChange`]): the table, the prefix of the version and its columns, the position left
    /// out.
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
            table: self.table,
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
                self.table
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
                self.table
            )),
        }
    }

    /// Puts one version in the tree under `key`, `prev` naming the version before it.
    ///
    /// The columns ride in the entry when what it weighs is within [`MAX_INSERT_BYTES`]; past
    /// it they go to a record of the overflow heap and the entry carries its stub. The page
    /// that takes that record is tied to `lsn` and flagged for `txn`, so the pool keeps it
    /// until the transaction finishes.
    fn write_version(
        &mut self,
        txn: TxnId,
        lsn: Lsn,
        header: &VersionHeader,
        key: &[Value],
        prev: &[Value],
        row: &Row,
    ) -> Result<(), InternalError> {
        let mut columns = Vec::new();
        encode_row(row, &mut columns);
        let mut payload = VersionPayload {
            header: *header,
            prev: prev.to_vec(),
            body: Body::Inline(columns.clone()),
        };
        let mut bytes = payload.encode();
        if encode_entry_key(key, &bytes).len() > MAX_INSERT_BYTES {
            let rid = self.overflow.insert(&columns)?;
            self.note_page(txn, rid.page, lsn)?;
            payload.body = Body::Overflow(rid);
            bytes = payload.encode();
        }
        self.tree.insert(key, &bytes)
    }

    /// Rewrites the entry of `stored` with another `xmax`, its key and its columns untouched.
    ///
    /// An entry of the tree has no in-place rewrite: it is deleted and the patched payload is
    /// inserted under the same key, so the version stays where the order puts it and the link
    /// another version holds on it stays valid.
    fn set_xmax(
        &mut self,
        stored: &StoredVersion,
        xmax: Option<TxnId>,
    ) -> Result<(), InternalError> {
        let header = VersionHeader {
            xmax,
            ..stored.header
        };
        let mut patched = stored.payload.clone();
        let mut prefix = Vec::new();
        header.write_to(&mut prefix);
        match patched.get_mut(..VERSION_PREFIX_LEN) {
            Some(slot) => slot.copy_from_slice(&prefix),
            None => {
                return corruption(format!(
                    "the entry of version {} of row {} of table {} holds {} bytes, fewer than \
                     the {VERSION_PREFIX_LEN} of its prefix",
                    stored.header.seq,
                    stored.header.row,
                    self.table,
                    patched.len()
                ));
            }
        }
        self.remove_entry(stored, false)?;
        self.tree.insert(&stored.key, &patched)
    }

    /// Takes the entry of `stored` out of the tree, and its columns out of the overflow heap
    /// when `free_columns` says the version goes away for good.
    fn remove_entry(
        &mut self,
        stored: &StoredVersion,
        free_columns: bool,
    ) -> Result<(), InternalError> {
        if !self.tree.delete(&stored.key, &stored.payload)? {
            return corruption(format!(
                "version {} of row {} of table {} is not in the tree under its own key",
                stored.header.seq, stored.header.row, self.table
            ));
        }
        if free_columns && let Body::Overflow(rid) = stored.parsed()?.body {
            self.overflow.delete(rid)?;
        }
        Ok(())
    }

    /// The writes that follow on the two trees carry `lsn` and the `in_progress` flag on the
    /// pages they change ([`BTree::hold_writes`]); [`ClusteredTable::claim_tree_pages`] then
    /// hands those pages to the transaction
    /// (`clustered_leaf_stays_out_of_data_until_commit`).
    fn hold_trees(&mut self, lsn: Lsn) {
        self.tree.hold_writes(lsn);
        self.directory.hold_writes(lsn);
    }

    /// Records for `txn` the pages the two trees flagged since the last call, so
    /// [`ClusteredTable::release_pages`] lifts the flag when the transaction finishes.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a transaction the table has not registered.
    fn claim_tree_pages(&mut self, txn: TxnId) -> Result<(), InternalError> {
        let mut flagged = self.tree.take_held_pages();
        flagged.extend(self.directory.take_held_pages());
        match self.state.txns.get_mut(&txn) {
            Some(state) => {
                state.pages.extend(flagged);
                Ok(())
            }
            None => corruption(format!(
                "transaction {txn} is not registered in table {}",
                self.table
            )),
        }
    }

    /// Ties `page` of the overflow heap to the journal record of LSN `lsn`, flags it as written
    /// by a transaction in progress and records it for `txn`.
    ///
    /// The pool refuses to write a page whose LSN is past the durable one, which is the
    /// write-ahead rule; the flag is what keeps it in the cache until `commit` or `rollback`.
    /// The pages of the two trees take the same two marks, through
    /// [`ClusteredTable::hold_trees`] and [`ClusteredTable::claim_tree_pages`].
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
                self.table
            )),
        }
    }

    /// Clears the `in_progress` flag of the pages `txn` wrote: those of the two trees and
    /// those of the overflow heap.
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
    fn finish_txn(&mut self, txn: TxnId) -> Vec<UndoEntry> {
        match self.state.txns.get_mut(&txn) {
            Some(state) => std::mem::take(&mut state.writes),
            None => Vec::new(),
        }
    }

    /// Applies one undo entry of `txn`, the pages it rewrites flagged for `txn`.
    ///
    /// The undo writes no journal record of its own, as the undo of a heap table does not
    /// ([`super::version::HeapTable`]): `Lsn(0)` leaves each page the LSN it carries
    /// ([`super::buffer::BufferPool::mark_dirty`] does not move one backwards), and the flag
    /// keeps the page in the pool until [`ClusteredTable::release_pages`], which
    /// [`ClusteredTable::rollback`] calls once the undo log is replayed.
    fn undo(&mut self, txn: TxnId, entry: UndoEntry) -> Result<(), InternalError> {
        self.hold_trees(Lsn(0));
        self.undo_entry(txn, entry)?;
        self.claim_tree_pages(txn)
    }

    /// One undo entry, without the marking [`ClusteredTable::undo`] puts around it.
    fn undo_entry(&mut self, txn: TxnId, entry: UndoEntry) -> Result<(), InternalError> {
        match entry {
            UndoEntry::Insert(row) => {
                let created = self.tail_created_by(txn, row)?;
                self.note_undone_version(&created)?;
                self.remove_entry(&created, true)?;
                self.remove_directory(row)
            }
            UndoEntry::Update(row) => {
                let created = self.tail_created_by(txn, row)?;
                let previous = created.parsed()?.prev;
                if previous.is_empty() {
                    return corruption(format!(
                        "version {} of row {row} of table {} links to no version, under the \
                         update of transaction {txn} being undone",
                        created.header.seq, self.table
                    ));
                }
                self.note_undone_version(&created)?;
                self.remove_entry(&created, true)?;
                let replaced = self.entry_at(&previous, row)?;
                if replaced.header.xmax != Some(txn) {
                    return corruption(format!(
                        "version {} of row {row} of table {} does not carry the xmax of \
                         transaction {txn} being undone",
                        replaced.header.seq, self.table
                    ));
                }
                self.set_xmax(&replaced, None)?;
                self.set_directory(row, &previous)
            }
            UndoEntry::Delete(row) => {
                let deleted = self.current(row)?;
                if deleted.header.xmax != Some(txn) {
                    return corruption(format!(
                        "row {row} of table {} does not end with the delete of transaction \
                         {txn} being undone",
                        self.table
                    ));
                }
                self.set_xmax(&deleted, None)
            }
        }
    }

    /// The current version of `row`, checked to be the one `txn` created and left current: what
    /// the undo of an `Insert` or of an `Update` takes away.
    fn tail_created_by(&self, txn: TxnId, row: RowId) -> Result<StoredVersion, InternalError> {
        let current = self.current(row)?;
        if current.header.xmin != txn || current.header.xmax.is_some() {
            return corruption(format!(
                "row {row} of table {} does not end with a version created and left current by \
                 transaction {txn} being undone",
                self.table
            ));
        }
        Ok(current)
    }

    /// The most recent version of `id`, read through the directory.
    fn current(&self, id: RowId) -> Result<StoredVersion, InternalError> {
        let Some(key) = self.directory_entry(id)? else {
            return bug(format!("unknown row {id} in table {}", self.table));
        };
        self.entry_at(&key, id)
    }

    /// The entry of the tree whose key is `key`, said to belong to `id`.
    ///
    /// The key of an entry ends with the `RowId` and the `seq` of its version, so a point seek
    /// on a whole key answers the one entry of that version.
    fn entry_at(&self, key: &[Value], id: RowId) -> Result<StoredVersion, InternalError> {
        let found = self
            .tree
            .seek(&KeyRange::Point(key.to_vec()), Direction::Forward)?;
        let Some(entry) = found.into_iter().next() else {
            return corruption(format!(
                "row {id} of table {} names a version the tree does not hold",
                self.table
            ));
        };
        StoredVersion::read(entry)
    }

    /// The key of the most recent version of `id`, `None` when the directory has no entry.
    fn directory_entry(&self, id: RowId) -> Result<Option<Vec<Value>>, InternalError> {
        let found = self
            .directory
            .seek(&KeyRange::Point(vec![tie_value(id.0)]), Direction::Forward)?;
        match found.into_iter().next() {
            Some((_, payload)) => Ok(Some(decode_row(&payload)?.0)),
            None => Ok(None),
        }
    }

    /// Points the directory entry of `id` at `key`, replacing what it named.
    fn set_directory(&mut self, id: RowId, key: &[Value]) -> Result<(), InternalError> {
        self.remove_directory(id)?;
        let mut payload = Vec::new();
        encode_row(&Row(key.to_vec()), &mut payload);
        self.directory.insert(&[tie_value(id.0)], &payload)
    }

    /// Takes the directory entry of `id` away, if it has one.
    fn remove_directory(&mut self, id: RowId) -> Result<(), InternalError> {
        let Some(key) = self.directory_entry(id)? else {
            return Ok(());
        };
        let mut payload = Vec::new();
        encode_row(&Row(key), &mut payload);
        self.directory.delete(&[tie_value(id.0)], &payload)?;
        Ok(())
    }

    /// The columns of one version, read from the entry or from the overflow heap.
    fn columns_of(&self, payload: &VersionPayload) -> Result<Row, InternalError> {
        match &payload.body {
            Body::Inline(columns) => decode_row(columns),
            Body::Overflow(rid) => match self.overflow.get(*rid)? {
                Some(columns) => decode_row(&columns),
                None => corruption(format!(
                    "version {} of row {} of table {} points at the overflow record {rid}, which \
                     holds nothing",
                    payload.header.seq, payload.header.row, self.table
                )),
            },
        }
    }

    /// Number of undo entries `txn` has written in this table.
    pub(crate) fn writes_len(&self, txn: TxnId) -> usize {
        match self.state.txns.get(&txn) {
            Some(state) => state.writes.len(),
            None => 0,
        }
    }

    /// Rolls back the writes of `txn` after position `mark`.
    ///
    /// # Errors
    ///
    /// Those of the undo path.
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

    /// Replays one row record of a winner at recovery into the tree, without journal or index
    /// maintenance. Answers whether it wrote.
    ///
    /// A record whose version the tree already holds — the page of an earlier redo reached
    /// `data` — is skipped, which is what makes the replay idempotent
    /// (`double_open_is_idempotent` on a clustered table).
    ///
    /// # Errors
    ///
    /// Those of [`BTree::seek`] and of [`BTree::insert`] while the entry is read and written.
    pub(crate) fn replay_one(&mut self, change: &RowChange) -> Result<bool, InternalError> {
        let id = change.header.row;
        let seq = change.header.seq;
        if change.header.xmax.is_some() {
            // A delete, or the version an update replaces: the record names the version it
            // hides by its `seq`, and its `xmax` is what the write set. A version the tree
            // does not hold — its insert belonging to a transaction that is not a winner —
            // leaves nothing to hide.
            let Some(key) = self.current_key_of(id, seq)? else {
                return Ok(false);
            };
            let stored = self.entry_at(&key, id)?;
            if stored.header.xmax.is_some() {
                return Ok(false);
            }
            let mut payload = stored.parsed()?;
            payload.header.xmax = change.header.xmax;
            let encoded = payload.encode();
            self.tree.delete(&key, &stored.payload)?;
            self.tree.insert(&key, &encoded)?;
            return Ok(true);
        }
        // The version a winner created. The chain link it carries is the key of the version
        // that came before it, which the directory names when the replay is ordered — the
        // journal hands the records in the order of their `lsn`, and a version is created
        // after the one it replaces. A version the tree already holds — an earlier
        // open redid it and a checkpoint put the page on `data` — is skipped, which is what
        // makes the replay idempotent.
        let row = decode_row(&change.columns)?;
        let mut current = self.directory_entry(id)?;
        while let Some(key) = current {
            let stored = self.entry_at(&key, id)?;
            if stored.header.seq == seq {
                return Ok(false);
            }
            let payload = stored.parsed()?;
            current = (!payload.prev.is_empty()).then_some(payload.prev);
        }
        let prev = self.directory_entry(id)?.unwrap_or_default();
        let key = self.key_of(&row, id, seq)?;
        let mut columns = Vec::new();
        encode_row(&row, &mut columns);
        let mut payload = VersionPayload {
            header: change.header,
            prev,
            body: Body::Inline(columns.clone()),
        };
        let mut bytes = payload.encode();
        if encode_entry_key(&key, &bytes).len() > MAX_INSERT_BYTES {
            let rid = self.overflow.insert(&columns)?;
            payload.body = Body::Overflow(rid);
            bytes = payload.encode();
        }
        self.tree.insert(&key, &bytes)?;
        self.set_directory(id, &key)?;
        Ok(true)
    }

    /// The key of the entry of the version `(id, seq)` of this table, `None` when the tree
    /// holds this version.
    ///
    /// The tree key of a version ends with its `seq` as a tie column, so a seek on the point
    /// range of the directory's current key does not find an older version whose key the
    /// update moved: the walk starts at the oldest entry and follows the directory's chain
    /// back. This is what makes a delete of a version already replaced a skip rather than a
    /// rewrite of a version that an update already replaced.
    fn current_key_of(&self, id: RowId, seq: u64) -> Result<Option<Vec<Value>>, InternalError> {
        let mut key = match self.directory_entry(id)? {
            Some(key) => key,
            None => return Ok(None),
        };
        loop {
            let stored = self.entry_at(&key, id)?;
            if stored.header.seq == seq {
                return Ok(Some(key));
            }
            let payload = stored.parsed()?;
            if payload.prev.is_empty() {
                return Ok(None);
            }
            key = payload.prev;
        }
    }

    /// Removes the versions a `vacuum` below `horizon` would discard.
    ///
    /// # Errors
    ///
    /// The errors of reading and writing the trees.
    pub(crate) fn vacuum(&mut self, horizon: TxnId) -> Result<Vec<IndexChange>, InternalError> {
        let status = |t| self.status(t);
        let entries = self.tree.seek(&KeyRange::Full, Direction::Forward)?;
        let mut to_delete = Vec::new();
        for (key, payload_bytes) in entries {
            let payload = VersionPayload::decode(&payload_bytes)?;
            let discard = match payload.header.xmax {
                Some(x) if x < horizon && status(x) == TxnStatus::Committed => true,
                None => false,
                _ => false,
            } || status(payload.header.xmin) == TxnStatus::Aborted;
            if !discard {
                continue;
            }
            to_delete.push((key, payload_bytes, payload));
        }
        let mut removed = Vec::new();
        for (key, payload_bytes, payload) in to_delete {
            self.tree.delete(&key, &payload_bytes)?;
            // The directory names the most recent version of the row, and is taken away
            // when the dead version is the one it points at: a version an update replaced is
            // dead while its successor is the current one, and a delete is dead and current
            // at once (`vacuum_removes_dead_keeps_horizon`).
            let current = self.directory_entry(payload.header.row)?;
            if current.is_some_and(|entry| entry == key) {
                self.remove_directory(payload.header.row)?;
            }
            let row = self.columns_of(&payload)?;
            removed.push(IndexChange::Removed {
                row: payload.header.row,
                seq: payload.header.seq,
                data: row,
            });
        }
        Ok(removed)
    }
}

/// The `seq` a key carries in its last column.
fn seq_of(key: &[Value]) -> Result<u64, InternalError> {
    match key.last() {
        Some(value) => tie_number(value),
        None => corruption("a key of a clustered tree carries no column".to_string()),
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

impl super::index::VersionSource for ClusteredTable<'_> {
    fn table(&self) -> TableId {
        ClusteredTable::table(self)
    }

    fn shape(&self) -> &TableShape {
        ClusteredTable::shape(self)
    }

    fn versions(&self, only: Option<RowId>) -> Result<Vec<VersionView>, InternalError> {
        ClusteredTable::versions(self, only)
    }

    fn status(&self, t: TxnId) -> TxnStatus {
        ClusteredTable::status(self, t)
    }
}

#[cfg(test)]
mod tests {
    use vauban_types::{SqlString, SqlType};

    use super::super::temp::TempDir;
    use super::super::wal::{WalRecord, WalRecordKind};
    use super::super::{DiskOptions, alloc};
    use super::*;

    /// The table the tests of this file work on.
    const TABLE: TableId = TableId(4);

    /// An empty instance in a temporary directory, with the guard that removes it.
    fn instance(label: &str) -> (TempDir, DiskStorage) {
        let dir = TempDir::created(label);
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("an instance");
        (dir, storage)
    }

    /// An ascending key on the column 0.
    fn first_column() -> Vec<KeyColumn> {
        vec![KeyColumn {
            column: 0,
            descending: false,
        }]
    }

    /// A table of two nullable `int` columns, clustered on the first one, ascending.
    fn pair_shape() -> TableShape {
        TableShape {
            columns: vec![
                TypeInfo::new(SqlType::Int, true),
                TypeInfo::new(SqlType::Int, true),
            ],
            clustered_key: Some(first_column()),
        }
    }

    /// A table of the given shape over a fresh page.
    fn table_of(storage: &DiskStorage, shape: TableShape) -> ClusteredTable<'_> {
        let head = alloc::allocate(storage).expect("allocate the head page");
        ClusteredTable::create(storage, TABLE, head, shape).expect("create the table")
    }

    /// A row of two `int`s.
    fn pair(first: i32, second: i32) -> Row {
        Row(vec![Value::I32(first), Value::I32(second)])
    }

    /// A snapshot of `own` seeing what is below `xmax` but the transactions of `active`.
    fn snap(own: u64, xmax: u64, active: &[u64]) -> Snapshot {
        Snapshot {
            xmin: TxnId(active.iter().copied().min().unwrap_or(xmax)),
            xmax: TxnId(xmax),
            active: active.iter().map(|t| TxnId(*t)).collect(),
            own: TxnId(own),
        }
    }

    /// The `RowId` of each row a scan answered, in order.
    fn ids(rows: &[(RowId, Row)]) -> Vec<u64> {
        rows.iter().map(|(id, _)| id.0).collect()
    }

    /// The column `at` of each row a scan answered, `None` for a `NULL`.
    fn column(rows: &[(RowId, Row)], at: usize) -> Vec<Option<i32>> {
        rows.iter()
            .map(|(_, row)| match row.0.get(at) {
                Some(Value::I32(number)) => Some(*number),
                Some(Value::Null) => None,
                other => panic!("a row of the scan carries {other:?} at {at}"),
            })
            .collect()
    }

    /// The payload of each entry of the tree of the versions, in key order.
    fn entries(table: &ClusteredTable<'_>) -> Vec<VersionPayload> {
        table
            .tree
            .seek(&KeyRange::Full, Direction::Forward)
            .expect("seek the whole tree")
            .iter()
            .map(|(_, payload)| VersionPayload::decode(payload).expect("decode a payload"))
            .collect()
    }

    /// The records the journal of `storage` holds.
    fn journal(storage: &DiskStorage) -> Vec<WalRecord> {
        storage.wal.records().expect("read the journal")
    }

    /// The number of slots the page `id` holds **in the data file**, which is what a flush of
    /// the pool leaves there.
    fn slots_in_data(storage: &DiskStorage, id: PageId) -> u16 {
        storage
            .data
            .read_page(id)
            .expect("read the page from data")
            .slot_count()
    }

    /// The kind of each record, in order.
    fn kinds(records: &[WalRecord]) -> Vec<WalRecordKind> {
        records.iter().map(|record| record.kind).collect()
    }

    /// The message of an error the test expects to be a caller bug.
    fn bug_message(error: InternalError) -> String {
        match error {
            InternalError::Bug(message) => message,
            other => panic!("expected a Bug, got {other:?}"),
        }
    }

    // --- Placement and order ---------------------------------------------------------------

    #[test]
    fn scan_follows_clustered_key() {
        let (_dir, storage) = instance("clustered-scan");
        let mut table = table_of(&storage, pair_shape());
        let two = table
            .insert(TxnId(1), &pair(2, 20))
            .expect("insert (2, 20)");
        let first_one = table
            .insert(TxnId(1), &pair(1, 11))
            .expect("insert (1, 11)");
        let second_one = table
            .insert(TxnId(1), &pair(1, 12))
            .expect("insert (1, 12)");
        table.commit(TxnId(1)).expect("commit");

        let rows = table.scan(&snap(9, 9, &[])).expect("scan");
        assert_eq!(column(&rows, 0), vec![Some(1), Some(1), Some(2)]);
        assert_eq!(column(&rows, 1), vec![Some(11), Some(12), Some(20)]);
        // The two rows of key 1 come back in `RowId` order, and the row inserted first comes
        // last: the order is the key's, not the one of the writes.
        assert_eq!(ids(&rows), vec![first_one.0, second_one.0, two.0]);
        assert_eq!((two.0, first_one.0, second_one.0), (1, 2, 3));
    }

    #[test]
    fn nulls_first_descending_last() {
        let (_dir, storage) = instance("clustered-nulls");
        let ascending = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, true)],
            clustered_key: Some(first_column()),
        };
        let descending = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, true)],
            clustered_key: Some(vec![KeyColumn {
                column: 0,
                descending: true,
            }]),
        };
        let mut up = table_of(&storage, ascending);
        let mut down = table_of(&storage, descending);
        // One transaction per table: the register of the transactions is per instance, so
        // one identifier takes one `Commit`, whichever table wrote under it
        // (`super::super::tests::one_begin_one_end_per_txn_over_two_tables`).
        for value in [Value::I32(1), Value::Null, Value::I32(2)] {
            let row = Row(vec![value]);
            up.insert(TxnId(1), &row)
                .expect("insert in the ascending table");
            down.insert(TxnId(2), &row)
                .expect("insert in the descending table");
        }
        up.commit(TxnId(1)).expect("commit the ascending table");
        down.commit(TxnId(2)).expect("commit the descending table");

        let reader = snap(9, 9, &[]);
        // `NULL` first on the ascending column, last on the descending one: the flag of the
        // key column is what moves it.
        assert_eq!(
            column(&up.scan(&reader).expect("scan"), 0),
            vec![None, Some(1), Some(2)]
        );
        assert_eq!(
            column(&down.scan(&reader).expect("scan"), 0),
            vec![Some(2), Some(1), None]
        );
    }

    // --- An update that moves a row --------------------------------------------------------

    #[test]
    fn update_key_moves_row_same_row_id() {
        let (_dir, storage) = instance("clustered-move");
        let mut table = table_of(&storage, pair_shape());
        let id = table
            .insert(TxnId(1), &pair(1, 10))
            .expect("insert (1, 10)");
        table
            .insert(TxnId(1), &pair(5, 50))
            .expect("insert (5, 50)");
        table
            .commit(TxnId(1))
            .expect("commit the writer of the rows");

        table
            .update(TxnId(2), id, &pair(9, 10))
            .expect("move the row from key 1 to key 9");
        // A snapshot that does not settle transaction 2 reads the row under its old key.
        let older = table.scan(&snap(9, 9, &[2])).expect("scan");
        assert_eq!(column(&older, 0), vec![Some(1), Some(5)]);
        assert_eq!(ids(&older), vec![id.0, 2]);

        table.commit(TxnId(2)).expect("commit the update");
        let newer = table.scan(&snap(9, 9, &[])).expect("scan");
        assert_eq!(column(&newer, 0), vec![Some(5), Some(9)]);
        // The row kept its `RowId` while its place changed.
        assert_eq!(ids(&newer), vec![2, id.0]);
        assert_eq!(
            table.get(&snap(9, 9, &[]), id).expect("get"),
            Some(pair(9, 10))
        );
    }

    #[test]
    fn get_by_row_id_after_move() {
        let (_dir, storage) = instance("clustered-get-after-move");
        let mut table = table_of(&storage, pair_shape());
        let id = table
            .insert(TxnId(1), &pair(1, 10))
            .expect("insert (1, 10)");
        table.commit(TxnId(1)).expect("commit the insert");
        table
            .update(TxnId(2), id, &pair(9, 10))
            .expect("move the row to key 9");
        table.commit(TxnId(2)).expect("commit the update");

        // The directory names the version of the greatest `seq`, wherever the new key put it.
        assert_eq!(
            table.get(&snap(9, 9, &[]), id).expect("get"),
            Some(pair(9, 10))
        );
        // A snapshot that does not settle transaction 2 walks the link back to the version
        // under the old key.
        assert_eq!(
            table.get(&snap(9, 9, &[2]), id).expect("get"),
            Some(pair(1, 10))
        );
        // The two versions are both in the tree, under the two keys.
        assert_eq!(entries(&table).len(), 2);
        assert_eq!(
            table.get(&snap(9, 9, &[]), RowId(7)).expect("get"),
            None,
            "a row the directory does not hold"
        );
    }

    #[test]
    fn splits_keep_the_order_and_the_directory() {
        let (_dir, storage) = instance("clustered-splits");
        let mut table = table_of(&storage, pair_shape());
        let root = table.tree_root();
        // 400 rows whose keys arrive in a scattered order: enough entries for the leaves to
        // split, which moves them from one page to another.
        let keys: Vec<i32> = (0..400).map(|step| (step * 97) % 400).collect();
        let mut ids = Vec::new();
        for key in &keys {
            ids.push(
                table
                    .insert(TxnId(1), &pair(*key, key * 2))
                    .expect("insert"),
            );
        }
        table.commit(TxnId(1)).expect("commit");
        assert_ne!(
            table.tree_root(),
            root,
            "the root moved, so a split reached it"
        );

        let reader = snap(9, 9, &[]);
        let rows = table.scan(&reader).expect("scan");
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(
            column(&rows, 0),
            sorted.iter().map(|key| Some(*key)).collect::<Vec<_>>()
        );
        // The directory still names each row, the entries having moved between pages.
        for (key, id) in keys.iter().zip(&ids) {
            assert_eq!(
                table.get(&reader, *id).expect("get"),
                Some(pair(*key, key * 2)),
                "row {id}"
            );
        }
    }

    // --- Transactions ----------------------------------------------------------------------

    #[test]
    fn an_insert_is_visible_to_its_writer_then_to_a_later_snapshot() {
        let (_dir, storage) = instance("clustered-insert-visibility");
        let mut table = table_of(&storage, pair_shape());
        let id = table.insert(TxnId(3), &pair(1, 10)).expect("insert");

        let own = snap(3, 3, &[]);
        let other = snap(9, 9, &[3]);
        assert_eq!(table.get(&own, id).expect("get"), Some(pair(1, 10)));
        assert_eq!(table.get(&other, id).expect("get"), None);
        assert_eq!(ids(&table.scan(&own).expect("scan")), vec![id.0]);
        assert!(table.scan(&other).expect("scan").is_empty());

        table.commit(TxnId(3)).expect("commit");
        let later = snap(9, 9, &[]);
        assert_eq!(table.get(&later, id).expect("get"), Some(pair(1, 10)));
    }

    #[test]
    fn a_delete_hides_the_row_from_the_snapshot_that_settles_it() {
        let (_dir, storage) = instance("clustered-delete");
        let mut table = table_of(&storage, pair_shape());
        let id = table.insert(TxnId(1), &pair(1, 10)).expect("insert");
        table.commit(TxnId(1)).expect("commit the insert");
        table.delete(TxnId(2), id).expect("delete");
        table.commit(TxnId(2)).expect("commit the delete");

        assert_eq!(table.get(&snap(9, 9, &[]), id).expect("get"), None);
        assert!(table.scan(&snap(9, 9, &[])).expect("scan").is_empty());
        // The entry stays where it was: a snapshot that does not settle the deleter reads it.
        assert_eq!(entries(&table).len(), 1);
        assert_eq!(
            table.get(&snap(9, 9, &[2]), id).expect("get"),
            Some(pair(1, 10))
        );
    }

    #[test]
    fn rollback_restore_old_key() {
        let (_dir, storage) = instance("clustered-rollback-update");
        let mut table = table_of(&storage, pair_shape());
        let id = table
            .insert(TxnId(1), &pair(1, 10))
            .expect("insert (1, 10)");
        table.commit(TxnId(1)).expect("commit the insert");
        table
            .update(TxnId(2), id, &pair(9, 10))
            .expect("move the row to key 9");
        assert_eq!(entries(&table).len(), 2);

        table.rollback(TxnId(2)).expect("roll the update back");
        let rows = table.scan(&snap(9, 9, &[])).expect("scan");
        assert_eq!(column(&rows, 0), vec![Some(1)]);
        assert_eq!(ids(&rows), vec![id.0]);
        // The version the update created is gone and the one it replaced carries no `xmax`.
        let left = entries(&table);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].header.xmax, None);
        assert_eq!(
            table.get(&snap(9, 9, &[]), id).expect("get"),
            Some(pair(1, 10))
        );
    }

    #[test]
    fn rollback_undoes_an_insert_and_keeps_the_row_id_it_took() {
        let (_dir, storage) = instance("clustered-rollback-insert");
        let mut table = table_of(&storage, pair_shape());
        let id = table.insert(TxnId(2), &pair(1, 10)).expect("insert");
        table.rollback(TxnId(2)).expect("roll the insert back");

        assert!(entries(&table).is_empty());
        assert_eq!(table.get(&snap(9, 9, &[]), id).expect("get"), None);
        let next = table.insert(TxnId(3), &pair(2, 20)).expect("insert again");
        assert_eq!((id.0, next.0), (1, 2));
    }

    #[test]
    fn the_writes_of_a_transaction_are_journalled_in_order() {
        let (_dir, storage) = instance("clustered-journal");
        let mut table = table_of(&storage, pair_shape());
        let id = table.insert(TxnId(1), &pair(1, 10)).expect("insert");
        table.update(TxnId(1), id, &pair(9, 10)).expect("update");
        table.delete(TxnId(1), id).expect("delete");
        table.commit(TxnId(1)).expect("commit");

        assert_eq!(
            kinds(&journal(&storage)),
            vec![
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Update,
                WalRecordKind::Delete,
                WalRecordKind::Commit,
            ]
        );
        // A transaction that writes nothing leaves nothing behind.
        table
            .commit(TxnId(5))
            .expect("commit a transaction of no write");
        assert_eq!(kinds(&journal(&storage)).len(), 5);
    }

    // --- A row too long for a leaf ---------------------------------------------------------

    // --- No-steal on the pages of the trees ---------------------------------------------

    #[test]
    fn clustered_leaf_stays_out_of_data_until_commit() {
        let (_dir, storage) = instance("clustered-no-steal");
        let mut table = table_of(&storage, pair_shape());
        let (leaf, directory) = (table.tree_root(), table.directory_root());
        // The pages the creation left are written first, so what `data` holds next comes from
        // the row.
        storage.pool.flush_all().expect("flush what create left");
        assert_eq!(slots_in_data(&storage, leaf), 0);
        assert_eq!(slots_in_data(&storage, directory), 0);

        table.insert(TxnId(4), &pair(1, 2)).expect("insert");
        storage.pool.flush_all().expect("flush while 4 runs");
        assert_eq!(
            slots_in_data(&storage, leaf),
            0,
            "the leaf of a row transaction 4 has not committed reached data"
        );
        assert_eq!(
            slots_in_data(&storage, directory),
            0,
            "the directory entry of that row reached data"
        );

        table.commit(TxnId(4)).expect("commit");
        storage.pool.flush_all().expect("flush after the commit");
        assert_eq!(slots_in_data(&storage, leaf), 1);
        assert_eq!(slots_in_data(&storage, directory), 1);
        assert_eq!(
            (table.tree_root(), table.directory_root()),
            (leaf, directory),
            "a root moved: the count is not on the page the entry landed on"
        );
    }

    #[test]
    fn an_updated_leaf_stays_out_of_data_until_commit() {
        let (_dir, storage) = instance("clustered-no-steal-update");
        let mut table = table_of(&storage, pair_shape());
        let leaf = table.tree_root();
        let id = table.insert(TxnId(4), &pair(1, 2)).expect("insert");
        table.commit(TxnId(4)).expect("commit the insert");
        storage
            .pool
            .flush_all()
            .expect("flush the committed insert");
        assert_eq!(slots_in_data(&storage, leaf), 1);

        // The update rewrites the version it hides and adds the new one: two entries in the
        // leaf held in the pool, one entry in the leaf `data` holds.
        table.update(TxnId(5), id, &pair(1, 3)).expect("update");
        storage.pool.flush_all().expect("flush while 5 runs");
        assert_eq!(
            slots_in_data(&storage, leaf),
            1,
            "the leaf of a version transaction 5 has not committed reached data"
        );

        table.commit(TxnId(5)).expect("commit the update");
        storage.pool.flush_all().expect("flush after the commit");
        assert_eq!(slots_in_data(&storage, leaf), 2);
        assert_eq!(table.tree_root(), leaf, "the root moved");
    }

    #[test]
    fn a_rolled_back_write_lets_its_pages_reach_data() {
        let (_dir, storage) = instance("clustered-no-steal-rollback");
        let mut table = table_of(&storage, pair_shape());
        let (leaf, directory) = (table.tree_root(), table.directory_root());
        table.insert(TxnId(4), &pair(1, 2)).expect("insert");
        table.rollback(TxnId(4)).expect("roll back");

        // The undo took the entry away and the rollback lifted the flag, so the pages go out.
        // What says they went out is the LSN they carry in `data`: the record of the insert
        // named the leaf, and the undo left that LSN on the page
        // (`BufferPool::mark_dirty` does not move one backwards).
        storage.pool.flush_all().expect("flush after the rollback");
        let records = journal(&storage);
        let insert = records
            .iter()
            .find(|record| record.kind == WalRecordKind::Insert)
            .expect("the record of the insert");
        let page = storage
            .data
            .read_page(leaf)
            .expect("read the leaf from data");
        assert_eq!(page.lsn(), insert.lsn);
        assert_eq!(
            page.slot_count(),
            0,
            "the entry the undo took away is in data"
        );
        assert_eq!(slots_in_data(&storage, directory), 0);
    }

    #[test]
    fn a_leaf_shared_by_two_txns_loses_its_flag_at_the_first_commit() {
        let (_dir, storage) = instance("clustered-shared-leaf");
        let mut table = table_of(&storage, pair_shape());
        let leaf = table.tree_root();
        storage.pool.flush_all().expect("flush what create left");

        table.insert(TxnId(4), &pair(1, 2)).expect("insert by 4");
        table.insert(TxnId(5), &pair(2, 2)).expect("insert by 5");
        table.commit(TxnId(4)).expect("commit 4");
        storage.pool.flush_all().expect("flush while 5 runs");

        // The flag belongs to the page, not to a transaction: the commit of 4 cleared it on the
        // leaf the two share, and the entry of 5 went to `data` with the one of 4. The journal
        // holds it back no longer either, the commit of 4 having made it durable past the
        // record of 5.
        assert_eq!(slots_in_data(&storage, leaf), 2);
        let records = journal(&storage);
        assert_eq!(
            kinds(&records),
            vec![
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Begin,
                WalRecordKind::Insert,
                WalRecordKind::Commit,
            ]
        );
        assert!(
            !records.iter().any(|record| record.txn == TxnId(5)
                && matches!(record.kind, WalRecordKind::Commit | WalRecordKind::Abort)),
            "transaction 5 is ended by a record: the entry in `data` is not the shape asserted"
        );
    }

    #[test]
    fn a_rollback_lets_the_leaf_of_a_running_txn_reach_data() {
        let (_dir, storage) = instance("clustered-shared-leaf-rollback");
        let mut table = table_of(&storage, pair_shape());
        let leaf = table.tree_root();
        storage.pool.flush_all().expect("flush what create left");

        table.insert(TxnId(4), &pair(1, 2)).expect("insert by 4");
        table.insert(TxnId(5), &pair(2, 2)).expect("insert by 5");
        table.rollback(TxnId(5)).expect("roll 5 back");
        storage.pool.flush_all().expect("flush while 4 runs");

        // The undo of 5 took its own entry away and the rollback lifted the flag of the shared
        // leaf, so what `data` holds is the entry of 4, which is still running.
        assert_eq!(slots_in_data(&storage, leaf), 1);
        let records = journal(&storage);
        assert!(
            !records.iter().any(|record| record.txn == TxnId(4)
                && matches!(record.kind, WalRecordKind::Commit | WalRecordKind::Abort)),
            "transaction 4 is ended by a record: the entry in `data` is not the shape asserted"
        );
    }

    #[test]
    fn the_overflow_chain_of_an_uncommitted_long_row_is_not_flagged() {
        let (_dir, storage) = instance("clustered-overflow-no-steal");
        let shape = TableShape {
            columns: vec![
                TypeInfo::new(SqlType::Int, true),
                TypeInfo::new(SqlType::VarChar(Len::Max), true),
            ],
            clustered_key: Some(first_column()),
        };
        let mut table = table_of(&storage, shape);
        let leaf = table.tree_root();
        storage.pool.flush_all().expect("flush what create left");
        let long = Row(vec![
            Value::I32(1),
            Value::String(SqlString {
                text: "Z".repeat(40_000),
            }),
        ]);
        table.insert(TxnId(4), &long).expect("insert the long row");

        storage.pool.flush_all().expect("flush while 4 runs");
        // The leaf carries the flag, so the entry that names the chain stays in the pool; the
        // pages of the chain are left unflagged, so the 40 000 characters are written.
        assert_eq!(slots_in_data(&storage, leaf), 0);
        let pages = storage.control().expect("control block").next_page_id.0;
        let with_payload = (0..pages)
            .filter(|id| {
                storage.data.read_page(PageId(*id)).is_ok_and(|page| {
                    page.0
                        .windows(1_000)
                        .any(|run| run.iter().all(|byte| *byte == b'Z'))
                })
            })
            .count();
        assert!(
            with_payload > 0,
            "the payload of the uncommitted long row stayed out of `data`"
        );
    }

    #[test]
    fn a_long_row_goes_through_the_overflow_heap_and_reads_back() {
        let (_dir, storage) = instance("clustered-overflow");
        let shape = TableShape {
            columns: vec![
                TypeInfo::new(SqlType::Int, true),
                TypeInfo::new(SqlType::VarChar(Len::Max), true),
            ],
            clustered_key: Some(first_column()),
        };
        let mut table = table_of(&storage, shape);
        let text = |len: usize| {
            Row(vec![
                Value::I32(1),
                Value::String(SqlString {
                    text: "v".repeat(len),
                }),
            ])
        };
        let (short, long) = (text(16), text(4_000));
        let mut columns = Vec::new();
        encode_row(&long, &mut columns);
        assert!(
            columns.len() > MAX_INSERT_BYTES,
            "the long row weighs {} bytes, which the entry ceiling of {MAX_INSERT_BYTES} has \
             to turn away",
            columns.len()
        );

        let small = table
            .insert(TxnId(1), &short)
            .expect("insert the short row");
        let big = table.insert(TxnId(1), &long).expect("insert the long row");
        table.commit(TxnId(1)).expect("commit");

        let bodies: Vec<bool> = entries(&table)
            .iter()
            .map(|payload| matches!(payload.body, Body::Overflow(_)))
            .collect();
        assert_eq!(bodies, vec![false, true]);
        let reader = snap(9, 9, &[]);
        assert_eq!(table.get(&reader, small).expect("get"), Some(short));
        assert_eq!(table.get(&reader, big).expect("get"), Some(long.clone()));
        assert_eq!(table.scan(&reader).expect("scan").len(), 2);

        // The record of the overflow heap goes away with the version that named it.
        table.delete(TxnId(2), big).expect("delete the long row");
        table.rollback(TxnId(2)).expect("roll the delete back");
        assert_eq!(table.get(&reader, big).expect("get"), Some(long));
    }

    // --- Preconditions and reopening -------------------------------------------------------

    #[test]
    fn a_table_without_a_clustered_key_is_a_bug() {
        let (_dir, storage) = instance("clustered-no-key");
        let head = alloc::allocate(&storage).expect("allocate the head page");
        let shape = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, true)],
            clustered_key: None,
        };
        let message = bug_message(
            ClusteredTable::create(&storage, TABLE, head, shape).expect_err("no clustered key"),
        );
        assert!(message.contains("no clustered key"), "{message}");

        let outside = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, true)],
            clustered_key: Some(vec![KeyColumn {
                column: 3,
                descending: false,
            }]),
        };
        let message = bug_message(
            ClusteredTable::create(&storage, TABLE, head, outside).expect_err("column 3"),
        );
        assert!(message.contains("outside"), "{message}");
    }

    #[test]
    fn a_wrong_arity_and_an_unknown_row_are_bugs() {
        let (_dir, storage) = instance("clustered-preconditions");
        let mut table = table_of(&storage, pair_shape());
        let narrow = Row(vec![Value::I32(1)]);
        let message = bug_message(table.insert(TxnId(1), &narrow).expect_err("arity"));
        assert!(message.contains("2 columns"), "{message}");
        let message = bug_message(
            table
                .update(TxnId(1), RowId(8), &pair(1, 1))
                .expect_err("unknown row"),
        );
        assert!(message.contains("unknown row 8"), "{message}");
        let message = bug_message(
            table
                .insert(TxnId(u64::MAX), &pair(1, 1))
                .expect_err("the reserved transaction id"),
        );
        assert!(message.contains("reserved"), "{message}");
    }

    #[test]
    fn open_attaches_to_the_trees_that_create_left() {
        let (_dir, storage) = instance("clustered-open");
        let (roots, head) = {
            let mut table = table_of(&storage, pair_shape());
            table
                .insert(TxnId(1), &pair(2, 20))
                .expect("insert (2, 20)");
            table
                .insert(TxnId(1), &pair(1, 10))
                .expect("insert (1, 10)");
            table.commit(TxnId(1)).expect("commit");
            (
                (table.tree_root(), table.directory_root()),
                table.first_page(),
            )
        };

        let mut again = ClusteredTable::open(&storage, TABLE, head, roots, pair_shape())
            .expect("open the table again");
        let rows = again.scan(&snap(9, 9, &[])).expect("scan");
        assert_eq!(column(&rows, 0), vec![Some(1), Some(2)]);
        assert_eq!(ids(&rows), vec![2, 1]);
        // The counters start again at 1, the table holding them in memory: a table reopened
        // by hand hands out `RowId(1)` a second time.
        assert_eq!(
            again.insert(TxnId(2), &pair(3, 30)).expect("insert"),
            RowId(1)
        );
    }

    // --- The payload of an entry -----------------------------------------------------------

    #[test]
    fn a_payload_carries_its_prefix_its_link_and_its_columns() {
        let header = VersionHeader {
            row: RowId(7),
            xmin: TxnId(3),
            xmax: None,
            seq: 5,
        };
        let payload = VersionPayload {
            header,
            prev: vec![Value::I32(1), tie_value(7), tie_value(2)],
            body: Body::Overflow(Rid {
                page: PageId(12),
                slot: 3,
            }),
        };
        let bytes = payload.encode();
        assert_eq!(VersionPayload::decode(&bytes).expect("decode"), payload);
        assert_eq!(seq_of(&payload.prev).expect("the seq of the link"), 2);
    }

    #[test]
    fn a_payload_cut_short_of_its_body_is_corruption() {
        let payload = VersionPayload {
            header: VersionHeader {
                row: RowId(1),
                xmin: TxnId(1),
                xmax: None,
                seq: 1,
            },
            prev: Vec::new(),
            body: Body::Inline(Vec::new()),
        };
        let bytes = payload.encode();
        for cut in [VERSION_PREFIX_LEN - 1, VERSION_PREFIX_LEN + 1] {
            let error = VersionPayload::decode(&bytes[..cut]).expect_err("a payload cut short");
            assert!(
                matches!(error, InternalError::Corruption(_)),
                "cut at {cut}: {error:?}"
            );
        }
    }
}
