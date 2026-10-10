//! Slotted heap of the on-disk layout: opaque records addressed by `(PageId, slot)`, chained
//! heap pages, and a chain of [`PageKind::Overflow`] pages for a record too large for one page.
//!
//! A table without a clustered key puts its rows here. This module stores opaque bytes: no
//! `xmin`, no logical [`crate::RowId`], no encoding of values; [`super::version`] puts the
//! row versions on top of it and calls the encoder of [`super::encode`].
//!
//! # Layout of a heap page
//!
//! ```text
//! 0                                                                            8191
//! +--------+-------+--------+-----------------------+---------+-----------------+
//! | header | upper | next   | records ->            |  free   |  <- slot dir    |
//! | 0..32  | 32..34| 34..42 | 42..lower             |         | upper..8192     |
//! +--------+-------+--------+-----------------------+---------+-----------------+
//! ```
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 32 | common header ([`super::page::Page`]); `slot_count` at 28, `lower` at 30 |
//! | 32 | 2 | `upper`: first byte of the slot directory, 8192 on a page with no slot |
//! | 34 | 8 | `next`: [`PageId`] of the next heap page, [`END_OF_CHAIN`] on the last page of the chain (`a_fresh_heap_page_carries_lower_42_upper_8192_and_no_link`) |
//! | 42 | | the records, growing up from `lower` |
//! | `upper` | 4 × `slot_count` | the slot directory, growing down from byte 8191 |
//!
//! `lower` and `upper` are the two ends of the free span: the records grow up from
//! [`HEAP_PAYLOAD_START`], the directory grows down from the last byte of the page. Slot `i`
//! sits at `8192 - 4 * (i + 1)`, so slot 0 is the last four bytes of the page and a slot keeps
//! its index for the whole life of the page.
//!
//! # A slot
//!
//! | Offset in the slot | Size | Field |
//! |---|---|---|
//! | 0 | 2 | `offset` of the record in the page |
//! | 2 | 2 | `length` of the record, bit 15 set when the record is a stub |
//!
//! A slot whose `offset` and `length` are both 0 is **free**: [`Heap::get`] answers `None` for
//! it and [`Heap::scan`] skips it. A record placed by [`Heap::insert`] starts at
//! [`HEAP_PAYLOAD_START`] or past it, so a record of length 0 is told apart from a free slot by
//! its non-zero offset (`insert_empty_row_is_not_a_free_slot`).
//!
//! # Overflow
//!
//! A record longer than [`MAX_INLINE_LEN`] (`PAGE_SIZE - 64`: 32 of common header, 10 of
//! `upper` and `next`, 4 of slot, and margin) is written to a chain of [`PageKind::Overflow`]
//! pages, and what the heap page holds is a **stub** of [`STUB_LEN`] bytes: a `u32` total
//! length then the [`PageId`] of the head of that chain, its slot carrying [`OVERFLOW_FLAG`].
//! A record of [`MAX_INLINE_LEN`] bytes or fewer is written in the page itself
//! (`overflow_threshold_is_the_documented_one`).
//!
//! An overflow page holds its `next` at bytes 32..40 and its share of the record from byte 40
//! to `lower`:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 32 | common header; `lower` is the first byte after the payload |
//! | 32 | 8 | `next` overflow page, [`END_OF_CHAIN`] on the last one |
//! | 40 | up to [`OVERFLOW_CAPACITY`] | the payload, `40..lower` |
//!
//! # Free space
//!
//! [`Heap::insert`] looks for a free slot to reuse before it adds one, so a record costs
//! `length` bytes of free span, or `length + 4` when it needs a new slot. When the free span is
//! too small, the page is compacted in place — the live records are moved down against
//! [`HEAP_PAYLOAD_START`] and the offsets of their slots rewritten — and the placement tried
//! once more; a page that still has no room sends the record to the next page of the chain, or
//! to a page [`super::alloc::allocate`] hands out (`allocate_reuses_space_on_same_page`,
//! `many_rows_span_pages`). Compaction stays inside one page: moving a record to another page
//! would change its [`Rid`], which this heap does not do.
//!
//! # Pins and the no-steal rule
//!
//! Each entry point pins the pages it reads or changes through [`super::buffer::BufferPool`]
//! and releases them before it answers: [`Heap::insert`], [`Heap::get`], [`Heap::delete`],
//! [`Heap::free_slot`] and [`Heap::scan`] answer a [`Rid`], a `Vec<u8>`, a `bool` for each of
//! the two frees and a `Vec<Rid>`, not a [`super::buffer::PinGuard`]. A page [`Heap::insert`] or [`Heap::delete`] changed is marked
//! dirty at `Lsn(0)` — the journal record that describes the change is written by the caller,
//! [`super::version::HeapTable`] — and the page that **took the record** has its `in_progress`
//! flag cleared: the heap carries no transaction of its own, so the caller sets the flag back
//! on the page it wrote. [`Heap::free_slot`] differs: it marks the page at the LSN its caller
//! hands it and leaves the flag as it stands, so that a freed slot does not reach `data` ahead
//! of the record that frees it. [`Heap::link_page`] leaves the flag too: it changes the page
//! that comes before a fresh one, which the caller does not visit afterwards
//! (`a_transaction_over_two_heap_pages_keeps_the_flag_on_both_of_them`).

use std::fmt;

use vauban_errors::InternalError;

use super::DiskStorage;
use super::alloc;
use super::buffer::PinGuard;
use super::page::{HEADER_SIZE, Lsn, PAGE_SIZE, Page, PageId, PageKind};
use crate::TableId;

/// Offset, in a heap page, of `upper`: the first byte of the slot directory.
pub(crate) const OFF_UPPER: usize = HEADER_SIZE;

/// Offset, in a heap page, of the identifier of the next page of the heap.
pub(crate) const OFF_HEAP_NEXT: usize = HEADER_SIZE + 2;

/// First byte of a heap page a record may occupy: after the header, `upper` and `next`.
pub(crate) const HEAP_PAYLOAD_START: u16 = (HEADER_SIZE + 10) as u16;

/// Offset, in an overflow page, of the identifier of the next page of the chain.
pub(crate) const OFF_OVERFLOW_NEXT: usize = HEADER_SIZE;

/// First byte of an overflow page the payload occupies: after the header and `next`.
pub(crate) const OVERFLOW_PAYLOAD_START: u16 = (HEADER_SIZE + 8) as u16;

/// Number of payload bytes one overflow page holds.
pub(crate) const OVERFLOW_CAPACITY: usize = PAGE_SIZE - OVERFLOW_PAYLOAD_START as usize;

/// Value written in `next` by the page that ends a chain, heap or overflow.
///
/// It is the same sentinel as [`super::alloc::END_OF_FREE_LIST`], read here through this name
/// because a heap chain is not the free list.
pub(crate) const END_OF_CHAIN: u64 = u64::MAX;

/// Size of one entry of the slot directory: a `u16` offset and a `u16` length.
pub(crate) const SLOT_SIZE: u16 = 4;

/// Longest record written inside a heap page; past it, the record goes to overflow pages.
///
/// `PAGE_SIZE - 64` = 8 128 bytes: 32 of common header, 10 of `upper` and `next`, 4 of slot,
/// and 18 bytes of margin over the 8 146 a fresh page could hold. The margin is what makes the
/// threshold a constant rather than the exact capacity of a page.
pub(crate) const MAX_INLINE_LEN: usize = PAGE_SIZE - 64;

/// Bit set in the length of a slot whose record is a stub pointing at an overflow chain.
///
/// A record written inside a page is at most [`MAX_INLINE_LEN`] bytes, well under `0x8000`, so
/// the bit is free for this use.
pub(crate) const OVERFLOW_FLAG: u16 = 0x8000;

/// Size of the stub a heap page holds for an overflow record: a `u32` total length then the
/// `u64` identifier of the head of the chain.
pub(crate) const STUB_LEN: usize = 12;

/// Address of a record in a heap: the page that holds it and its slot in that page.
///
/// `Display` writes `page:slot`, the two bare decimal integers
/// (`rid_displays_page_colon_slot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Rid {
    /// The heap page that holds the record or its stub.
    pub(crate) page: PageId,
    /// Index of the slot in the directory of that page.
    pub(crate) slot: u16,
}

impl fmt::Display for Rid {
    /// Writes `page:slot`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.page, self.slot)
    }
}

/// One entry of the slot directory, as it stands in the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    /// Offset of the record in the page.
    offset: u16,
    /// Length of the record, [`OVERFLOW_FLAG`] included.
    stored: u16,
}

impl Slot {
    /// Whether the slot holds no record: offset and length both 0.
    fn is_free(self) -> bool {
        self.offset == 0 && self.stored == 0
    }

    /// Whether the record of the slot is a stub pointing at an overflow chain.
    fn is_overflow(self) -> bool {
        self.stored & OVERFLOW_FLAG != 0
    }

    /// Length of the bytes the record occupies in the page, the flag bit left out.
    fn len(self) -> usize {
        usize::from(self.stored & !OVERFLOW_FLAG)
    }
}

/// What a slot points at, once read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    /// The record, held in the heap page itself.
    Inline(Vec<u8>),
    /// A stub: the total length of the record and the head of its overflow chain.
    Overflow {
        /// Length of the whole record, in bytes.
        total: usize,
        /// First page of the overflow chain.
        head: PageId,
    },
}

/// The heap of one table: a chain of slotted pages starting at `first_page`.
///
/// The structure borrows the instance and holds no page: each call pins what it needs and
/// releases it before it answers. Which page is the head of which table is the business of
/// [`super::meta`]; here the caller hands the page it got from [`super::alloc::allocate`].
#[derive(Debug)]
pub(crate) struct Heap<'storage> {
    /// The instance the pages belong to.
    storage: &'storage DiskStorage,
    /// The table this heap holds the rows of; it names the table in the errors.
    table: TableId,
    /// Head of the chain of heap pages.
    first_page: PageId,
}

impl<'storage> Heap<'storage> {
    /// Formats `first_page` as the empty head page of the heap of `table`.
    ///
    /// `first_page` is a page [`super::alloc::allocate`] handed out. Its payload is zeroed and
    /// its header written: kind [`PageKind::Heap`], `slot_count` 0, `lower`
    /// [`HEAP_PAYLOAD_START`], `upper` `PAGE_SIZE`, `next` [`END_OF_CHAIN`]
    /// (`a_fresh_heap_page_carries_lower_42_upper_8192_and_no_link`).
    ///
    /// # Errors
    ///
    /// The errors of [`super::buffer::BufferPool::pin`] for a page that cannot be read.
    pub(crate) fn create(
        storage: &'storage DiskStorage,
        table: TableId,
        first_page: PageId,
    ) -> Result<Self, InternalError> {
        let heap = Self {
            storage,
            table,
            first_page,
        };
        heap.format_page(first_page)?;
        Ok(heap)
    }

    /// Attaches to the heap of `table` whose head page is `first_page`, leaving the pages as
    /// they stand.
    pub(crate) fn open(storage: &'storage DiskStorage, table: TableId, first_page: PageId) -> Self {
        Self {
            storage,
            table,
            first_page,
        }
    }

    /// The table this heap holds the rows of.
    pub(crate) fn table(&self) -> TableId {
        self.table
    }

    /// The head of the chain of heap pages.
    pub(crate) fn first_page(&self) -> PageId {
        self.first_page
    }

    /// Writes `bytes` in the heap and answers where it went.
    ///
    /// A record of [`MAX_INLINE_LEN`] bytes or fewer is written in a heap page; a longer one
    /// goes to a chain of overflow pages and the heap page holds its stub. The pages are
    /// walked from [`Heap::first_page`] and the first one with room takes the record; a walk
    /// that reaches the end of the chain without placing it grows the heap by one page,
    /// chained there (`many_rows_span_pages` reaches 5 pages for 500 rows of 64 bytes).
    ///
    /// # Errors
    ///
    /// The errors of the buffer pool and of [`super::alloc::allocate`], and
    /// [`InternalError::Bug`] for a record that does not fit in a page this call has just
    /// formatted, which [`MAX_INLINE_LEN`] rules out.
    pub(crate) fn insert(&self, bytes: &[u8]) -> Result<Rid, InternalError> {
        let (payload, overflow) = if bytes.len() > MAX_INLINE_LEN {
            let head = self.write_overflow_chain(bytes)?;
            (stub(bytes.len(), head)?, true)
        } else {
            (bytes.to_vec(), false)
        };

        let mut page_id = self.first_page;
        loop {
            if let Some(slot) = self.try_place(page_id, &payload, overflow)? {
                return Ok(Rid {
                    page: page_id,
                    slot,
                });
            }
            match self.next_page(page_id)? {
                Some(next) => page_id = next,
                None => break,
            }
        }

        let fresh = alloc::allocate(self.storage)?;
        self.format_page(fresh)?;
        self.link_page(page_id, fresh)?;
        match self.try_place(fresh, &payload, overflow)? {
            Some(slot) => Ok(Rid { page: fresh, slot }),
            None => Err(InternalError::Bug(format!(
                "record of {} bytes does not fit in the fresh heap page {fresh} of table {}",
                payload.len(),
                self.table
            ))),
        }
    }

    /// The record at `rid`, or `None` when its slot is free or past the directory of its page.
    ///
    /// An overflow record is reassembled from its chain, and the assembled length is checked
    /// against the total its stub carries.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the page of `rid` is not a heap page, when a slot
    /// points outside the records of its page, when a stub is not [`STUB_LEN`] bytes long, and
    /// when an overflow chain does not hold the length its stub announces. The errors of the
    /// buffer pool otherwise.
    pub(crate) fn get(&self, rid: Rid) -> Result<Option<Vec<u8>>, InternalError> {
        let pin = self.storage.pool.pin(rid.page)?;
        self.check_heap(&pin)?;
        let found = pin.with_page(|page| read_record(page, rid.slot))?;
        drop(pin);
        match found? {
            None => Ok(None),
            Some(Record::Inline(bytes)) => Ok(Some(bytes)),
            Some(Record::Overflow { total, head }) => {
                Ok(Some(self.read_overflow_chain(head, total)?))
            }
        }
    }

    /// Frees the slot of `rid`, and the overflow pages of its record when it had some.
    ///
    /// Answers whether a record was there: a slot already free and a slot past the directory of
    /// its page both answer `false` and change nothing. The bytes the record occupied stay in
    /// the page until a later [`Heap::insert`] compacts it.
    ///
    /// The page is marked dirty at `Lsn(0)` and its `in_progress` flag cleared, as
    /// [`Heap::insert`] does. This is the call of the overflow heap of
    /// [`super::clustered::ClusteredTable`]; a heap table frees its slots through
    /// [`Heap::free_slot`], which ties the page to the record that describes the release.
    ///
    /// # Errors
    ///
    /// The errors of [`Heap::get`] while the slot is read, and those of
    /// [`super::alloc::free`] while the overflow pages are handed back.
    pub(crate) fn delete(&self, rid: Rid) -> Result<bool, InternalError> {
        self.free(rid, Lsn(0), true)
    }

    /// Frees the slot of `rid` as [`Heap::delete`] does, the page tied to the journal record of
    /// LSN `lsn` under the same pin and its `in_progress` flag left as it stands.
    ///
    /// `lsn` is the record that describes the release, appended by the caller before this call:
    /// the record of the write that moves the version ([`super::version::HeapTable::update`],
    /// [`super::version::HeapTable::delete`]), the compensation of an undo or the purge of a
    /// vacuum. The pool then keeps the page out of `data` until that record is durable
    /// (`a_freed_slot_ties_the_page_to_the_lsn_it_is_given`). `Lsn(0)` is what the recovery
    /// passes, which replays the journal and appends nothing. The flag is left as it stands
    /// because the caller that flags its pages has set it before the call, and clearing it here
    /// would let the pool write a page whose record is not durable yet.
    ///
    /// # Errors
    ///
    /// Those of [`Heap::delete`].
    pub(crate) fn free_slot(&self, rid: Rid, lsn: Lsn) -> Result<bool, InternalError> {
        self.free(rid, lsn, false)
    }

    /// The body of [`Heap::delete`] and [`Heap::free_slot`]: the slot freed, the page marked
    /// dirty at `lsn`, its flag cleared when `clear_flag` says so, then the overflow pages
    /// handed back.
    fn free(&self, rid: Rid, lsn: Lsn, clear_flag: bool) -> Result<bool, InternalError> {
        let pin = self.storage.pool.pin(rid.page)?;
        self.check_heap(&pin)?;
        let found = pin.with_page(|page| read_record(page, rid.slot))?;
        let Some(record) = found? else {
            drop(pin);
            return Ok(false);
        };
        pin.with_page_mut(|page| write_slot(page, rid.slot, 0, 0))?;
        self.storage.pool.mark_dirty(rid.page, lsn)?;
        if clear_flag {
            self.storage.pool.set_in_progress(rid.page, false)?;
        }
        drop(pin);

        if let Record::Overflow { head, .. } = record {
            for id in self.overflow_chain(head)? {
                alloc::free(self.storage, id)?;
            }
        }
        Ok(true)
    }

    /// Hands the pages of the heap back to the allocator: its chain of heap pages and the
    /// overflow chains their records point at (`super::tests::drop_table_frees_its_pages`
    /// reads a page of a dropped heap back out of the free list).
    ///
    /// Called by the `drop_table` and `drop_database` of [`super::DiskStorage`], after the
    /// journal record of the statement. The pages are read first and freed after,
    /// because [`super::alloc::free`] refuses a page that carries a pin; the overflow pages go
    /// back before the heap pages that name them, so a walk interrupted by an error leaves
    /// heap pages allocated rather than a freed heap page pointing at an allocated one.
    ///
    /// The heap is left behind: the structure holds no state of its own, and its head page has
    /// gone back to the free list, so a later call answers the error of a page that is not a
    /// heap page.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the chain reaches a page that is not a heap page,
    /// and the errors of the buffer pool and of [`super::alloc::free`].
    pub(crate) fn free_pages(&self) -> Result<(), InternalError> {
        let mut pages = Vec::new();
        let mut overflow = Vec::new();
        let mut current = Some(self.first_page);
        while let Some(id) = current {
            let pin = self.storage.pool.pin(id)?;
            self.check_heap(&pin)?;
            let (heads, next) = pin.with_page(|page| {
                let mut heads = Vec::new();
                for slot in 0..page.slot_count() {
                    if let Some(Record::Overflow { head, .. }) = read_record(page, slot)? {
                        heads.push(head);
                    }
                }
                Ok::<_, InternalError>((heads, heap_next(page)))
            })??;
            drop(pin);
            for head in heads {
                overflow.extend(self.overflow_chain(head)?);
            }
            pages.push(id);
            current = next;
        }
        for id in overflow.into_iter().chain(pages) {
            alloc::free(self.storage, id)?;
        }
        Ok(())
    }

    /// The addresses of the records the heap holds, page by page from [`Heap::first_page`] and
    /// slot by slot in each page, free slots skipped.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] when the chain reaches a page that is not a heap page, and
    /// the errors of the buffer pool.
    pub(crate) fn scan(&self) -> Result<Vec<Rid>, InternalError> {
        let mut rids = Vec::new();
        let mut current = Some(self.first_page);
        while let Some(id) = current {
            let pin = self.storage.pool.pin(id)?;
            self.check_heap(&pin)?;
            let (found, next) = pin.with_page(|page| {
                let mut found = Vec::new();
                for slot in 0..page.slot_count() {
                    if !read_slot(page, slot).is_free() {
                        found.push(Rid { page: id, slot });
                    }
                }
                (found, heap_next(page))
            })?;
            drop(pin);
            rids.extend(found);
            current = next;
        }
        Ok(rids)
    }

    /// Writes an empty heap page over `id`, zeroing the bytes past its common header so that a
    /// page recycled by [`super::alloc::allocate`] starts with an empty directory
    /// (`a_fresh_heap_page_carries_lower_42_upper_8192_and_no_link`).
    fn format_page(&self, id: PageId) -> Result<(), InternalError> {
        let pin = self.storage.pool.pin(id)?;
        pin.with_page_mut(format_heap_page)?;
        self.storage.pool.mark_dirty(id, Lsn(0))?;
        self.storage.pool.set_in_progress(id, false)?;
        Ok(())
    }

    /// Tries to put `payload` in the page `id`, answering the slot it took.
    fn try_place(
        &self,
        id: PageId,
        payload: &[u8],
        overflow: bool,
    ) -> Result<Option<u16>, InternalError> {
        let pin = self.storage.pool.pin(id)?;
        self.check_heap(&pin)?;
        let placed = pin.with_page_mut(|page| place(page, payload, overflow))?;
        if placed.is_some() {
            self.storage.pool.mark_dirty(id, Lsn(0))?;
            self.storage.pool.set_in_progress(id, false)?;
        }
        Ok(placed)
    }

    /// The page that follows `id` in the chain of heap pages.
    fn next_page(&self, id: PageId) -> Result<Option<PageId>, InternalError> {
        let pin = self.storage.pool.pin(id)?;
        let next = pin.with_page(heap_next)?;
        drop(pin);
        Ok(next)
    }

    /// Chains `to` after `from` in the chain of heap pages.
    ///
    /// The `in_progress` flag of `from` is left as it stands, unlike the other writes of this
    /// file: `from` is the page the walk of [`Heap::insert`] left behind, and the caller flags
    /// the page the record went to, not that one. Clearing it here took the flag off a page a
    /// running transaction had written, and its rows reached `data` once another transaction
    /// had flushed the journal (`a_transaction_over_two_heap_pages_keeps_the_flag_on_both_of_them`,
    /// 400 rows of one `int` over three pages).
    fn link_page(&self, from: PageId, to: PageId) -> Result<(), InternalError> {
        let pin = self.storage.pool.pin(from)?;
        pin.with_page_mut(|page| set_heap_next(page, Some(to)))?;
        self.storage.pool.mark_dirty(from, Lsn(0))?;
        Ok(())
    }

    /// Writes `bytes` to a fresh chain of overflow pages and answers its head.
    ///
    /// The chunks are written from the last to the first, so that each page is written with the
    /// identifier of the one that follows it already known.
    fn write_overflow_chain(&self, bytes: &[u8]) -> Result<PageId, InternalError> {
        let mut next: Option<PageId> = None;
        for chunk in bytes.chunks(OVERFLOW_CAPACITY).rev() {
            let id = alloc::allocate(self.storage)?;
            let pin = self.storage.pool.pin(id)?;
            pin.with_page_mut(|page| {
                page.0[HEADER_SIZE..].fill(0);
                page.set_kind(PageKind::Overflow);
                page.set_slot_count(0);
                let start = usize::from(OVERFLOW_PAYLOAD_START);
                page.0[start..start + chunk.len()].copy_from_slice(chunk);
                // `chunk.len()` is at most `OVERFLOW_CAPACITY`, that is 8 152.
                page.set_lower(OVERFLOW_PAYLOAD_START + chunk.len() as u16);
                write_u64(
                    page,
                    OFF_OVERFLOW_NEXT,
                    next.map_or(END_OF_CHAIN, |id| id.0),
                );
            })?;
            self.storage.pool.mark_dirty(id, Lsn(0))?;
            self.storage.pool.set_in_progress(id, false)?;
            drop(pin);
            next = Some(id);
        }
        next.ok_or_else(|| {
            InternalError::Bug(format!(
                "a record of the heap of table {} went to overflow with no byte to write",
                self.table
            ))
        })
    }

    /// Reads back the `total` bytes of the overflow chain that starts at `head`.
    fn read_overflow_chain(&self, head: PageId, total: usize) -> Result<Vec<u8>, InternalError> {
        let mut out = Vec::with_capacity(total);
        let mut current = Some(head);
        while let Some(id) = current {
            let pin = self.storage.pool.pin(id)?;
            let step = pin.with_page(|page| read_overflow_page(page, id))?;
            drop(pin);
            let (chunk, next) = step?;
            out.extend_from_slice(&chunk);
            if out.len() > total {
                return Err(InternalError::Corruption(format!(
                    "overflow chain {head} of table {} holds more than the {total} bytes its \
                     stub announces",
                    self.table
                )));
            }
            current = next;
        }
        if out.len() != total {
            return Err(InternalError::Corruption(format!(
                "overflow chain {head} of table {} holds {} bytes, its stub announces {total}",
                self.table,
                out.len()
            )));
        }
        Ok(out)
    }

    /// The identifiers of the overflow chain that starts at `head`, head first.
    fn overflow_chain(&self, head: PageId) -> Result<Vec<PageId>, InternalError> {
        let mut ids = Vec::new();
        let mut current = Some(head);
        while let Some(id) = current {
            let pin = self.storage.pool.pin(id)?;
            let step = pin.with_page(|page| read_overflow_page(page, id))?;
            drop(pin);
            let next = step?.1;
            ids.push(id);
            current = next;
        }
        Ok(ids)
    }

    /// Checks that the pinned page is a heap page.
    fn check_heap(&self, pin: &PinGuard<'_>) -> Result<(), InternalError> {
        let kind = pin.with_page(Page::kind)??;
        if kind != PageKind::Heap {
            return Err(InternalError::Corruption(format!(
                "page {} of the heap of table {} carries kind {kind:?}",
                pin.page_id(),
                self.table
            )));
        }
        Ok(())
    }
}

/// The stub a heap page holds for a record written to the overflow chain `head`.
fn stub(total: usize, head: PageId) -> Result<Vec<u8>, InternalError> {
    let length = u32::try_from(total).map_err(|_| {
        InternalError::Bug(format!(
            "a record of {total} bytes is past the {} an overflow stub records",
            u32::MAX
        ))
    })?;
    let mut bytes = Vec::with_capacity(STUB_LEN);
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&head.0.to_le_bytes());
    Ok(bytes)
}

/// Writes an empty heap page over the payload of `page`, keeping its common header.
fn format_heap_page(page: &mut Page) {
    page.0[HEADER_SIZE..].fill(0);
    page.set_kind(PageKind::Heap);
    page.set_slot_count(0);
    page.set_lower(HEAP_PAYLOAD_START);
    set_heap_upper(page, PAGE_SIZE as u16);
    set_heap_next(page, None);
}

/// Puts `payload` in `page` if it fits, compacting the page once if it has to, and answers the
/// slot it took.
fn place(page: &mut Page, payload: &[u8], overflow: bool) -> Option<u16> {
    let len = u16::try_from(payload.len()).ok()?;
    let slot_count = page.slot_count();
    let reuse = (0..slot_count).find(|&index| read_slot(page, index).is_free());
    let needed = if reuse.is_some() {
        len
    } else {
        len.saturating_add(SLOT_SIZE)
    };

    if free_span(page) < needed {
        compact(page);
        if free_span(page) < needed {
            return None;
        }
    }

    let at = page.lower();
    page.0[usize::from(at)..usize::from(at) + payload.len()].copy_from_slice(payload);
    page.set_lower(at + len);
    let stored = if overflow { len | OVERFLOW_FLAG } else { len };
    let index = match reuse {
        Some(index) => index,
        None => {
            page.set_slot_count(slot_count + 1);
            set_heap_upper(page, heap_upper(page) - SLOT_SIZE);
            slot_count
        }
    };
    write_slot(page, index, at, stored);
    Some(index)
}

/// Moves the live records of `page` down against [`HEAP_PAYLOAD_START`], rewriting the offsets
/// of their slots, so that the bytes the deleted records left become free span.
///
/// The records are moved in the order of their current offsets, so the destination of a move is
/// at or below the offset it comes from and the records that have yet to move are left where
/// they are (`allocate_reuses_space_on_same_page` reads its 81 remaining records back after a
/// compaction). The directory keeps its size: a freed slot is reused by index, not removed,
/// which is what keeps the [`Rid`] of the records that stay.
fn compact(page: &mut Page) {
    let slot_count = page.slot_count();
    let mut live: Vec<(u16, Slot)> = (0..slot_count)
        .map(|index| (index, read_slot(page, index)))
        .filter(|(_, slot)| !slot.is_free())
        .collect();
    live.sort_by_key(|(_, slot)| slot.offset);

    let mut cursor = HEAP_PAYLOAD_START;
    for (index, slot) in live {
        let from = usize::from(slot.offset);
        let len = slot.len();
        if slot.offset != cursor {
            page.0.copy_within(from..from + len, usize::from(cursor));
        }
        write_slot(page, index, cursor, slot.stored);
        // `len` counts bytes the page already holds, so the sum stays inside a `u16`.
        cursor += len as u16;
    }
    page.set_lower(cursor);
}

/// Bytes between the records and the slot directory of a heap page.
fn free_span(page: &Page) -> u16 {
    heap_upper(page).saturating_sub(page.lower())
}

/// Reads the record of `slot`, or `None` when the slot is free or past the directory.
fn read_record(page: &Page, slot: u16) -> Result<Option<Record>, InternalError> {
    if slot >= page.slot_count() {
        return Ok(None);
    }
    let entry = read_slot(page, slot);
    if entry.is_free() {
        return Ok(None);
    }
    let start = usize::from(entry.offset);
    let end = start + entry.len();
    if entry.offset < HEAP_PAYLOAD_START || end > usize::from(page.lower()) {
        return Err(InternalError::Corruption(format!(
            "slot {slot} of heap page {} points at bytes {start}..{end}, outside the records of \
             the page, which end at {}",
            page.page_id(),
            page.lower()
        )));
    }
    if !entry.is_overflow() {
        return Ok(Some(Record::Inline(page.0[start..end].to_vec())));
    }
    if entry.len() != STUB_LEN {
        return Err(InternalError::Corruption(format!(
            "slot {slot} of heap page {} is an overflow stub of {} bytes, not {STUB_LEN}",
            page.page_id(),
            entry.len()
        )));
    }
    Ok(Some(Record::Overflow {
        total: read_u32(page, start) as usize,
        head: PageId(read_u64(page, start + 4)),
    }))
}

/// Reads the payload and the link of an overflow page.
fn read_overflow_page(page: &Page, id: PageId) -> Result<(Vec<u8>, Option<PageId>), InternalError> {
    let kind = page.kind()?;
    if kind != PageKind::Overflow {
        return Err(InternalError::Corruption(format!(
            "page {id} of an overflow chain carries kind {kind:?}"
        )));
    }
    let end = usize::from(page.lower());
    if end < usize::from(OVERFLOW_PAYLOAD_START) || end > PAGE_SIZE {
        return Err(InternalError::Corruption(format!(
            "overflow page {id} says its payload ends at {end}, outside \
             {OVERFLOW_PAYLOAD_START}..{PAGE_SIZE}"
        )));
    }
    let payload = page.0[usize::from(OVERFLOW_PAYLOAD_START)..end].to_vec();
    let next = match read_u64(page, OFF_OVERFLOW_NEXT) {
        END_OF_CHAIN => None,
        next => Some(PageId(next)),
    };
    Ok((payload, next))
}

/// Offset, in the page, of the entry of slot `index`.
fn slot_position(index: u16) -> usize {
    PAGE_SIZE - usize::from(SLOT_SIZE) * (usize::from(index) + 1)
}

/// The entry of slot `index`, as it stands in the page.
fn read_slot(page: &Page, index: u16) -> Slot {
    let at = slot_position(index);
    Slot {
        offset: read_u16(page, at),
        stored: read_u16(page, at + 2),
    }
}

/// Writes the entry of slot `index`.
fn write_slot(page: &mut Page, index: u16, offset: u16, stored: u16) {
    let at = slot_position(index);
    write_u16(page, at, offset);
    write_u16(page, at + 2, stored);
}

/// The first byte of the slot directory of a heap page.
fn heap_upper(page: &Page) -> u16 {
    read_u16(page, OFF_UPPER)
}

/// Overwrites the first byte of the slot directory of a heap page.
fn set_heap_upper(page: &mut Page, upper: u16) {
    write_u16(page, OFF_UPPER, upper);
}

/// The page that follows this heap page, `None` at the end of the chain.
fn heap_next(page: &Page) -> Option<PageId> {
    match read_u64(page, OFF_HEAP_NEXT) {
        END_OF_CHAIN => None,
        next => Some(PageId(next)),
    }
}

/// Writes the link of a heap page.
fn set_heap_next(page: &mut Page, next: Option<PageId>) {
    write_u64(page, OFF_HEAP_NEXT, next.map_or(END_OF_CHAIN, |id| id.0));
}

/// Reads the little-endian `u16` at `at`.
fn read_u16(page: &Page, at: usize) -> u16 {
    u16::from_le_bytes([page.0[at], page.0[at + 1]])
}

/// Writes the little-endian `u16` at `at`.
fn write_u16(page: &mut Page, at: usize, value: u16) {
    page.0[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

/// Reads the little-endian `u32` at `at`.
fn read_u32(page: &Page, at: usize) -> u32 {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(&page.0[at..at + 4]);
    u32::from_le_bytes(raw)
}

/// Reads the little-endian `u64` at `at`.
fn read_u64(page: &Page, at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&page.0[at..at + 8]);
    u64::from_le_bytes(raw)
}

/// Writes the little-endian `u64` at `at`.
fn write_u64(page: &mut Page, at: usize, value: u64) {
    page.0[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests;
