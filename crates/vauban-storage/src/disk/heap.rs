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
//! and releases them before it answers: [`Heap::insert`], [`Heap::get`], [`Heap::delete`] and
//! [`Heap::scan`] answer a [`Rid`], a `Vec<u8>`, a `bool` and a `Vec<Rid>`, not a
//! [`super::buffer::PinGuard`]. A page a call changed is marked
//! dirty at `Lsn(0)` — the journal record that describes the change is written by the caller,
//! [`super::version::HeapTable`] — and the page that **took the record** has its `in_progress`
//! flag cleared: the heap carries no transaction of its own, so the caller sets the flag back
//! on the page it wrote. [`Heap::link_page`] is the exception: it changes the page that comes
//! before a fresh one, which the caller does not visit afterwards, so it leaves that flag as
//! it stands (`a_transaction_over_two_heap_pages_keeps_the_flag_on_both_of_them`).

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
    /// # Errors
    ///
    /// The errors of [`Heap::get`] while the slot is read, and those of
    /// [`super::alloc::free`] while the overflow pages are handed back.
    pub(crate) fn delete(&self, rid: Rid) -> Result<bool, InternalError> {
        let pin = self.storage.pool.pin(rid.page)?;
        self.check_heap(&pin)?;
        let found = pin.with_page(|page| read_record(page, rid.slot))?;
        let Some(record) = found? else {
            drop(pin);
            return Ok(false);
        };
        pin.with_page_mut(|page| write_slot(page, rid.slot, 0, 0))?;
        self.storage.pool.mark_dirty(rid.page, Lsn(0))?;
        self.storage.pool.set_in_progress(rid.page, false)?;
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
mod tests {
    use super::super::control::Control;
    use super::super::temp::TempDir;
    use super::super::{DATA_FILE_NAME, DiskOptions};
    use super::*;

    /// The table the tests of this file work on; the heap reads it for its error messages.
    const TABLE: TableId = TableId(7);

    /// An empty instance in a temporary directory, with the guard that removes it.
    fn instance(label: &str) -> (TempDir, DiskStorage) {
        let dir = TempDir::created(label);
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        (dir, storage)
    }

    /// A fresh heap over a page the allocator just handed out.
    fn heap_of(storage: &DiskStorage) -> Heap<'_> {
        let head = alloc::allocate(storage).expect("allocate the head page");
        Heap::create(storage, TABLE, head).expect("format the head page")
    }

    /// The control block of the instance.
    fn control_of(storage: &DiskStorage) -> Control {
        storage.control().expect("read the control block")
    }

    /// The pages of the heap, head first.
    fn pages_of(heap: &Heap<'_>) -> Vec<PageId> {
        let mut ids = Vec::new();
        let mut current = Some(heap.first_page());
        while let Some(id) = current {
            ids.push(id);
            current = heap.next_page(id).expect("read the link of a heap page");
        }
        ids
    }

    /// The slot entry behind a `Rid`, read through the pool.
    fn slot_of(storage: &DiskStorage, rid: Rid) -> Slot {
        let pin = storage.pool.pin(rid.page).expect("pin the page of the rid");
        pin.with_page(|page| read_slot(page, rid.slot))
            .expect("read the slot")
    }

    /// The kind of each page of the data file, from page 0 to `next_page_id`.
    fn kinds_of(storage: &DiskStorage) -> Vec<PageKind> {
        let count = control_of(storage).next_page_id.0;
        (0..count)
            .map(|id| {
                storage
                    .pool
                    .pin(PageId(id))
                    .expect("pin a page of the file")
                    .with_page(Page::kind)
                    .expect("read the page")
                    .expect("a known kind")
            })
            .collect()
    }

    /// A buffer of `len` bytes whose content depends on `seed` and on the position.
    fn payload(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn insert_get_small_row() {
        let (_dir, storage) = instance("heap-small-row");
        let heap = heap_of(&storage);
        let row = payload(16, 3);

        let rid = heap.insert(&row).expect("insert a row of 16 bytes");
        assert_eq!(rid.page, heap.first_page());
        assert_eq!(rid.slot, 0);
        assert_eq!(heap.get(rid).expect("read the row back"), Some(row));
        assert_eq!(heap.scan().expect("scan the heap"), vec![rid]);
        assert!(!slot_of(&storage, rid).is_overflow());
        assert_eq!(slot_of(&storage, rid).len(), 16);
    }

    /// A fresh heap page carries the layout the module documents.
    #[test]
    fn a_fresh_heap_page_carries_lower_42_upper_8192_and_no_link() {
        let (_dir, storage) = instance("heap-fresh-page");
        let heap = heap_of(&storage);
        let pin = storage
            .pool
            .pin(heap.first_page())
            .expect("pin the head page");
        pin.with_page(|page| {
            assert_eq!(page.kind().expect("a known kind"), PageKind::Heap);
            assert_eq!(page.slot_count(), 0);
            assert_eq!(page.lower(), 42);
            assert_eq!(page.lower(), HEAP_PAYLOAD_START);
            assert_eq!(heap_upper(page), 8192);
            assert_eq!(heap_next(page), None);
            assert_eq!(read_u64(page, OFF_HEAP_NEXT), END_OF_CHAIN);
            assert_eq!(free_span(page), 8150);
        })
        .expect("read the head page");
    }

    #[test]
    fn rid_displays_page_colon_slot() {
        assert_eq!(
            Rid {
                page: PageId(12),
                slot: 5,
            }
            .to_string(),
            "12:5"
        );
        assert_eq!(
            Rid {
                page: PageId(0),
                slot: 0,
            }
            .to_string(),
            "0:0"
        );
    }

    #[test]
    fn delete_makes_get_none() {
        let (_dir, storage) = instance("heap-delete");
        let heap = heap_of(&storage);
        let first = heap.insert(&payload(20, 1)).expect("insert the first row");
        let second = heap.insert(&payload(24, 2)).expect("insert the second row");

        assert!(heap.delete(first).expect("delete the first row"));
        assert_eq!(heap.get(first).expect("read the freed slot"), None);
        assert_eq!(
            heap.get(second).expect("read the row that stays"),
            Some(payload(24, 2))
        );
        assert_eq!(heap.scan().expect("scan the heap"), vec![second]);
        assert!(!heap.delete(first).expect("delete the freed slot again"));
    }

    #[test]
    fn get_of_a_slot_past_the_directory_is_none() {
        let (_dir, storage) = instance("heap-past-directory");
        let heap = heap_of(&storage);
        let rid = heap.insert(&payload(8, 4)).expect("insert a row");
        let past = Rid {
            page: rid.page,
            slot: 9,
        };
        assert_eq!(heap.get(past).expect("read past the directory"), None);
        assert!(!heap.delete(past).expect("delete past the directory"));
    }

    /// A record of length 0 is not a free slot: its offset is where it was placed.
    #[test]
    fn insert_empty_row_is_not_a_free_slot() {
        let (_dir, storage) = instance("heap-empty-row");
        let heap = heap_of(&storage);
        let rid = heap.insert(&[]).expect("insert an empty row");
        let slot = slot_of(&storage, rid);
        assert_eq!(slot.len(), 0);
        assert!(!slot.is_free(), "{slot:?}");
        assert_eq!(slot.offset, HEAP_PAYLOAD_START);
        assert_eq!(heap.get(rid).expect("read it back"), Some(Vec::new()));
        assert_eq!(heap.scan().expect("scan the heap"), vec![rid]);
    }

    /// The hole a `delete` leaves is used again by a smaller record, in the same page.
    ///
    /// The page is filled until its free span is exactly 0, so the insert that follows the
    /// delete fits in the page just when the hole is reused. With the `compact` call of
    /// `place` replaced by `return None`, this test reads `PageId(1)` where it asserts
    /// `PageId(0)`.
    #[test]
    fn allocate_reuses_space_on_same_page() {
        let (_dir, storage) = instance("heap-reuse-space");
        let heap = heap_of(&storage);
        let head = heap.first_page();

        // 81 records of 96 bytes cost 81 × 100 = 8 100 of the 8 150 free bytes; one of 46 bytes
        // then costs the 50 that are left.
        let mut rids = Vec::new();
        for index in 0..81u8 {
            rids.push(heap.insert(&payload(96, index)).expect("fill the page"));
        }
        let last = heap
            .insert(&payload(46, 200))
            .expect("fill the page exactly");
        rids.push(last);
        assert_eq!(
            control_of(&storage).next_page_id,
            PageId(1),
            "the 82 records went in the head page"
        );
        let pin = storage.pool.pin(head).expect("pin the head page");
        pin.with_page(|page| {
            assert_eq!(free_span(page), 0, "the page holds no free byte");
            assert_eq!(page.slot_count(), 82);
        })
        .expect("read the head page");
        drop(pin);

        let freed = rids[0];
        assert!(heap.delete(freed).expect("free 96 bytes"));
        let smaller = heap
            .insert(&payload(32, 9))
            .expect("insert a smaller record");
        assert_eq!(smaller.page, head, "the smaller record stayed in the page");
        assert_eq!(
            control_of(&storage).next_page_id,
            PageId(1),
            "no page was allocated for it"
        );
        assert_eq!(smaller.slot, freed.slot, "the freed slot was reused");
        assert_eq!(
            heap.get(smaller).expect("read the smaller record"),
            Some(payload(32, 9))
        );
        // Compacting moved the other records; each one reads as it was written.
        for (index, rid) in rids.iter().enumerate().skip(1) {
            let expected = if *rid == last {
                payload(46, 200)
            } else {
                payload(96, index as u8)
            };
            assert_eq!(heap.get(*rid).expect("read a moved record"), Some(expected));
        }
        assert_eq!(heap.scan().expect("scan the heap").len(), 82);
    }

    #[test]
    fn many_rows_span_pages() {
        let (_dir, storage) = instance("heap-many-rows");
        let heap = heap_of(&storage);

        let mut rids = Vec::new();
        for index in 0..500usize {
            let row = payload(64, index as u8);
            rids.push(heap.insert(&row).expect("insert one of 500 rows"));
        }

        let pages = pages_of(&heap);
        assert!(pages.len() >= 2, "500 rows of 64 bytes need {pages:?}");
        assert_eq!(pages.len(), 5, "119 rows of 64 bytes fit in a page");
        assert_eq!(pages[0], heap.first_page());

        let scanned = heap.scan().expect("scan the heap");
        assert_eq!(scanned.len(), 500);
        assert_eq!(scanned, rids, "scan answers the order of the insertions");
        for (index, rid) in scanned.iter().enumerate() {
            assert_eq!(
                heap.get(*rid).expect("read one of 500 rows"),
                Some(payload(64, index as u8)),
                "row {index} at {rid}"
            );
        }
        assert_eq!(kinds_of(&storage), vec![PageKind::Heap; 5]);
    }

    #[test]
    fn overflow_roundtrip() {
        let (_dir, storage) = instance("heap-overflow");
        let heap = heap_of(&storage);
        let row = payload(PAGE_SIZE, 5);
        assert!(row.len() > MAX_INLINE_LEN);

        let rid = heap.insert(&row).expect("insert a row of 8 192 bytes");
        assert_eq!(
            control_of(&storage).next_page_id,
            PageId(3),
            "the head page and the two pages of the chain"
        );
        assert!(slot_of(&storage, rid).is_overflow());
        assert_eq!(slot_of(&storage, rid).len(), STUB_LEN);
        assert_eq!(
            kinds_of(&storage),
            vec![PageKind::Heap, PageKind::Overflow, PageKind::Overflow]
        );

        let read = heap.get(rid).expect("read the row back");
        assert_eq!(read.as_deref(), Some(row.as_slice()), "byte for byte");
        assert_eq!(heap.scan().expect("scan the heap"), vec![rid]);

        // Deleting hands the overflow pages back to the allocator.
        assert!(heap.delete(rid).expect("delete the overflow row"));
        assert_eq!(heap.get(rid).expect("read the freed slot"), None);
        let overflow_pages = [PageId(1), PageId(2)];
        let free_head = control_of(&storage)
            .free_head
            .expect("a non-empty free list");
        assert!(overflow_pages.contains(&free_head), "{free_head}");
        let recycled = alloc::allocate(&storage).expect("allocate after the delete");
        assert!(overflow_pages.contains(&recycled), "{recycled}");
        assert_eq!(
            control_of(&storage).next_page_id,
            PageId(3),
            "the page came from the free list, the file did not grow"
        );
    }

    #[test]
    fn overflow_not_used_when_fits() {
        let (_dir, storage) = instance("heap-no-overflow");
        let heap = heap_of(&storage);
        let before = control_of(&storage);
        assert_eq!(before.next_page_id, PageId(1));

        let rid = heap
            .insert(&payload(64, 8))
            .expect("insert a row of 64 bytes");

        let after = control_of(&storage);
        assert_eq!(after.next_page_id, before.next_page_id, "no page allocated");
        assert_eq!(after.free_head, before.free_head);
        assert_eq!(kinds_of(&storage), vec![PageKind::Heap], "no Overflow page");
        assert!(!slot_of(&storage, rid).is_overflow());
        assert_eq!(heap.get(rid).expect("read it back"), Some(payload(64, 8)));
    }

    /// The threshold is the one the module documents: `PAGE_SIZE - 64` bytes stay in the page,
    /// one byte more goes to an overflow chain.
    #[test]
    fn overflow_threshold_is_the_documented_one() {
        assert_eq!(MAX_INLINE_LEN, 8128);

        let (_dir, inline_storage) = instance("heap-threshold-inline");
        let inline_heap = heap_of(&inline_storage);
        let inline_row = payload(MAX_INLINE_LEN, 1);
        let inline_rid = inline_heap.insert(&inline_row).expect("insert 8 128 bytes");
        assert!(!slot_of(&inline_storage, inline_rid).is_overflow());
        assert_eq!(
            control_of(&inline_storage).next_page_id,
            PageId(1),
            "8 128 bytes stayed in the head page"
        );
        assert_eq!(
            inline_heap.get(inline_rid).expect("read 8 128 bytes back"),
            Some(inline_row)
        );

        let (_dir, over_storage) = instance("heap-threshold-overflow");
        let over_heap = heap_of(&over_storage);
        let over_row = payload(MAX_INLINE_LEN + 1, 1);
        let over_rid = over_heap.insert(&over_row).expect("insert 8 129 bytes");
        assert!(slot_of(&over_storage, over_rid).is_overflow());
        assert_eq!(
            control_of(&over_storage).next_page_id,
            PageId(2),
            "8 129 bytes took one overflow page"
        );
        assert_eq!(
            over_heap.get(over_rid).expect("read 8 129 bytes back"),
            Some(over_row)
        );
    }

    /// A record of 32 bytes stays in the page, the other end of the overflow threshold.
    #[test]
    fn a_row_of_32_bytes_stays_in_the_page() {
        let (_dir, storage) = instance("heap-32-bytes");
        let heap = heap_of(&storage);
        let rid = heap.insert(&payload(32, 2)).expect("insert 32 bytes");
        assert!(!slot_of(&storage, rid).is_overflow());
        assert_eq!(control_of(&storage).next_page_id, PageId(1));
    }

    /// The heap is read back from the file after a flush, a close and a reopen.
    ///
    /// Nothing of the layout lives outside the pages: they carry `lower`, `upper`,
    /// `slot_count` and the links, so [`Heap::open`] finds the records where `insert` put
    /// them.
    #[test]
    fn records_read_back_after_a_reopen() {
        let dir = TempDir::created("heap-reopen");
        let mut rids = Vec::new();
        let head;
        let large;
        {
            let storage =
                DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
            let heap = heap_of(&storage);
            head = heap.first_page();
            for index in 0..200usize {
                rids.push(
                    heap.insert(&payload(64, index as u8))
                        .expect("insert a row"),
                );
            }
            large = heap
                .insert(&payload(PAGE_SIZE, 6))
                .expect("insert a big row");
            storage.pool.flush_all().expect("flush the pool");
        }

        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let heap = Heap::open(&storage, TABLE, head);
        assert_eq!(heap.table(), TABLE);
        assert_eq!(heap.first_page(), head);
        assert_eq!(heap.scan().expect("scan the reopened heap").len(), 201);
        for (index, rid) in rids.iter().enumerate() {
            assert_eq!(
                heap.get(*rid).expect("read a row back after the reopen"),
                Some(payload(64, index as u8))
            );
        }
        assert_eq!(
            heap.get(large).expect("read the big row back"),
            Some(payload(PAGE_SIZE, 6))
        );
        assert!(
            std::fs::metadata(dir.child(DATA_FILE_NAME))
                .expect("the data file exists")
                .len()
                >= 8192,
            "the pages were written"
        );
    }

    /// A page that is not a heap page is corruption, not a silent `None`.
    #[test]
    fn get_on_a_page_that_is_not_a_heap_page_is_corruption() {
        let (_dir, storage) = instance("heap-wrong-kind");
        let heap = heap_of(&storage);
        let other = alloc::allocate(&storage).expect("allocate a second page");
        let err = heap
            .get(Rid {
                page: other,
                slot: 0,
            })
            .expect_err("a free page is not a heap page");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("Free"), "{err}");
    }

    /// A slot that points outside the records of its page is corruption.
    #[test]
    fn a_slot_pointing_past_lower_is_corruption() {
        let (_dir, storage) = instance("heap-bad-slot");
        let heap = heap_of(&storage);
        let rid = heap.insert(&payload(16, 1)).expect("insert a row");
        let pin = storage.pool.pin(rid.page).expect("pin the page");
        pin.with_page_mut(|page| write_slot(page, rid.slot, 8000, 16))
            .expect("move the slot past the records");
        drop(pin);

        let err = heap.get(rid).expect_err("the slot points outside");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("outside the records"), "{err}");
    }

    #[test]
    fn a_slot_tells_free_from_empty_and_carries_the_overflow_bit() {
        assert!(
            Slot {
                offset: 0,
                stored: 0,
            }
            .is_free()
        );
        assert!(
            !Slot {
                offset: 42,
                stored: 0,
            }
            .is_free(),
            "a record of length 0 is not a free slot"
        );
        let stub = Slot {
            offset: 42,
            stored: STUB_LEN as u16 | OVERFLOW_FLAG,
        };
        assert!(stub.is_overflow());
        assert_eq!(stub.len(), STUB_LEN);
        assert_eq!(slot_position(0), PAGE_SIZE - 4);
        assert_eq!(slot_position(1), PAGE_SIZE - 8);
        assert_eq!(OVERFLOW_CAPACITY, 8152);
        assert_eq!(SLOT_SIZE, 4);
        assert_eq!(OFF_UPPER, 32);
        assert_eq!(OFF_OVERFLOW_NEXT, 32);
        assert_eq!(OFF_HEAP_NEXT, 34);
    }
}
