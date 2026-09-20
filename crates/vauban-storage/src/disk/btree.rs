//! B+tree of the on-disk layout, in two layers: the pages — a leaf page, an internal page,
//! and the split that turns a full one into two — and the [`BTree`] that walks them from a
//! root ([`BTree::insert`], [`BTree::delete`], [`BTree::seek`]).
//!
//! # Two layers, two comparisons
//!
//! The page layer takes a key as a slice of bytes and compares it with `Ord` on
//! `[u8]`: that is what [`leaf_put`] and [`internal_put`] place a key by, and what a split
//! reads to tell one key from its neighbour. The tree layer does not order its
//! entries that way: it decodes the key of an entry into [`crate::Value`]s and compares them
//! with [`KeyOrder`] — `NULL` first, [`vauban_types::compare`] with the collation of the
//! column, reversed by `descending` — then computes the index the entry takes and writes it
//! there. So [`BTree`] calls neither [`leaf_put`] nor [`internal_put`]; those two, and
//! [`leaf_iter`], stay for the tests of the page layer, and the byte order of the encoding is
//! not the order of the tree (`tests::negative_ints_sort_below_zero_as_compare_orders_them`).
//!
//! What a split reads is still bytes, and two entries it has to tell apart are two entries
//! whose bytes differ: [`encode_entry_key`] therefore writes the payload inside the key of the
//! page entry, and the payload field of a leaf entry is left empty. [`super::clustered`] and
//! [`super::index`] put a [`crate::RowId`] in that payload.
//!
//! # Leaf page
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 32 | the common header of [`Page`], with kind [`PageKind::BTreeLeaf`] |
//! | 32 | 8 | `prev`, the leaf to the left, or [`NO_SIBLING`] |
//! | 40 | 8 | `next`, the leaf to the right, or [`NO_SIBLING`] |
//! | 48 | 4 × `slot_count` | the slot directory, growing up |
//! | … | | free space |
//! | … up to 8 192 | | the entries, growing down from the end of the page |
//!
//! A slot is two little-endian `u16`: the offset of its entry in the page, then the length of
//! that entry. The slots are held **in key order**, so reading the directory from 0 to
//! `slot_count` reads the keys in order ([`leaf_iter`]); the entries themselves sit where
//! there was room (`leaf_insert_ordered_slots` compares the two orders). `lower` of the common
//! header is the first byte after the directory, that is `48 + 4 × slot_count`.
//!
//! An entry is `u16 key_len`, the key, `u16 payload_len`, the payload.
//!
//! # Internal page
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 32 | the common header of [`Page`], with kind [`PageKind::BTreeInternal`] |
//! | 32 | 8 | `P0`, the child to the left of the keys of the page |
//! | 40 | 4 × `slot_count` | the slot directory, growing up |
//! | … | | free space |
//! | … up to 8 192 | | the entries, growing down from the end of the page |
//!
//! The entries carry the same two lengths as a leaf's, their payload being the eight bytes of
//! a [`PageId`]: entry `i` holds `k(i+1)` and `P(i+1)`, so a page of `slot_count` keys points
//! at `slot_count + 1` children and reads `P0, k1, P1, k2, P2, …`. The key `k(i)` separates
//! `P(i-1)` from `P(i)`: a search for a key below `k1` goes to `P0`, and one for a key at or
//! above `k(i)` and below `k(i+1)` goes to `P(i)`.
//!
//! # Splitting
//!
//! [`split_leaf`] and [`split_internal`] take a page that is full, ask
//! [`super::alloc::allocate`] for a second one, share the entries between the two by their
//! byte size, and answer the [`Split`] the caller writes into the parent. They do not touch
//! the parent, and they do not rebalance: merging two thin pages is not implemented.
//!
//! The separator differs between the two, because the two kinds of page hold their keys
//! differently:
//!
//! - a **leaf** split copies its separator: the first key of the right leaf goes up to the
//!   parent and stays in the leaf, because the leaf is where the payload of that key lives
//!   (`split_leaf_separator`);
//! - an **internal** split moves its separator: the promoted key leaves both pages, which is
//!   what keeps each of them at one pointer more than it has keys (`internal_put_and_split`).
//!   It is still the key that separates the right page from the left one, which is what the
//!   parent stores.
//!
//! # A run of equal keys is not cut in two
//!
//! Two entries of one key sit next to each other on a leaf ([`leaf_put`]), and such a run may
//! straddle the point the byte sizes pick. The cut is then moved to the nearest index where
//! the key changes, so a run goes to one side of the separator
//! (`tests::split_leaf_keeps_a_run_of_equal_keys_on_one_side`): a separator equal to a key of
//! the left page would send a search of that key to the right page, past the entries left
//! behind, and two splits of one run would put two equal keys in the parent. The price is a
//! split that is not balanced when the run is long, which
//! `tests::equal_keys_stay_on_one_side_of_a_split` asserts over 200 drawn pages.
//!
//! A run that fills the page has no such index: [`split_leaf`] then answers
//! [`InternalError::Bug`] and leaves the page as it was
//! (`tests::split_leaf_refuses_a_run_it_cannot_cut`). Holding more entries of one key than a
//! page carries asks for a chain of overflow leaves, which this build does not lay out; the
//! refusal reaches the caller of [`BTree::insert`]. An internal page refuses the same way when no
//! key of it differs from both of its neighbours
//! (`tests::split_internal_refuses_a_page_it_cannot_promote_out_of`).
//!
//! # What a split leaves behind
//!
//! The pages a write changes are marked through the [`PageWrites`] the tree carries. A tree
//! starts at [`PageWrites::unlogged`]: the pages come back dirty at `page_lsn` 0 and
//! `in_progress` false, so a caller that wants the split in the data file flushes the pool
//! (`a_split_reaches_the_data_file_when_the_pool_is_flushed`), and a crash before that flush
//! leaves the page as it was, plus the page [`super::alloc::allocate`] took out of the free
//! list. A caller whose writes are described by a journal record calls [`BTree::hold_writes`]
//! with the LSN of that record: each page then carries that LSN and the `in_progress` flag
//! until the caller drains [`BTree::take_held_pages`] and lifts it, which is the no-steal rule
//! of the heap (`a_held_write_marks_its_pages_and_hands_them_back`). The pages the
//! `PageWrites` of a tree marks are the ones its own calls touch; a page another writer of the
//! same pool flags is that writer's business.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ops::Bound;

use vauban_errors::InternalError;
use vauban_types::{Collation, TypeInfo, Value, compare};

use super::DiskStorage;
use super::alloc::allocate;
use super::encode::{decode_row, encode_row};
use super::page::{HEADER_SIZE, Lsn, PAGE_SIZE, Page, PageId, PageKind};
use crate::{Direction, KeyColumn, KeyRange, Row};

/// Offset, in a leaf, of the identifier of the leaf to its left.
pub(crate) const OFF_LEAF_PREV: usize = HEADER_SIZE;

/// Offset, in a leaf, of the identifier of the leaf to its right.
pub(crate) const OFF_LEAF_NEXT: usize = HEADER_SIZE + 8;

/// First byte of the slot directory of a leaf, after the header and the two links.
pub(crate) const LEAF_DIRECTORY: usize = HEADER_SIZE + 16;

/// Offset, in an internal page, of `P0`, the child to the left of the keys of the page.
pub(crate) const OFF_LEFTMOST_CHILD: usize = HEADER_SIZE;

/// First byte of the slot directory of an internal page, after the header and `P0`.
pub(crate) const INTERNAL_DIRECTORY: usize = HEADER_SIZE + 8;

/// Value written in `prev` or `next` of a leaf that has no such neighbour. It is the value
/// [`super::alloc::END_OF_FREE_LIST`] carries, which the allocator refuses to hand out, so it
/// names no leaf.
pub(crate) const NO_SIBLING: u64 = u64::MAX;

/// Bytes of one slot of a directory: the offset of its entry, then its length.
const SLOT_SIZE: usize = 4;

/// Bytes an entry spends on its two lengths, added to the length of its key and of its
/// payload.
const ENTRY_OVERHEAD: usize = 4;

/// Bytes of the payload of an internal entry: the child [`PageId`].
const CHILD_BYTES: usize = 8;

/// Largest `key.len() + payload.len()` this build puts on a page, 8 064 bytes.
///
/// An entry past it is [`InternalError::Bug`] and not an overflow chain: an index key fits,
/// and the clustered row that does not goes through [`super::clustered`]. The 128
/// bytes held back leave room for the header, the two links and the directory, so an entry of
/// this size goes on a page that is empty (`entry_too_large_is_bug` puts one there).
pub(crate) const MAX_ENTRY_BYTES: usize = PAGE_SIZE - 128;

/// A key and its payload, held apart from the page they were copied out of.
pub(crate) type Entry = (Vec<u8>, Vec<u8>);

/// What a `put` did with the entry it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Put {
    /// The entry is on the page, its slot in key order.
    Inserted,
    /// The page has no room for it. The caller splits the page and puts the entry again; the
    /// page was left as it was (`leaf_put_answers_full_without_changing_the_page`).
    Full,
}

/// The outcome of a split: the page that was allocated, and the key the parent stores in front
/// of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Split {
    /// The page [`super::alloc::allocate`] handed out, holding the upper half of the entries.
    pub(crate) right: PageId,
    /// The key that separates [`Split::right`] from the page that was split: the keys of the
    /// left page are below it, and the keys of the right page are at or above it
    /// (`tests::split_leaf_separator`, `tests::internal_put_and_split`, and with equal keys
    /// `tests::split_leaf_keeps_a_run_of_equal_keys_on_one_side`).
    pub(crate) separator: Vec<u8>,
}

/// Writes the header of an empty leaf over `page`: kind, no sibling, no entry.
///
/// The bytes a recycled page carries past the directory are left as they are; they are not
/// read, because `slot_count` is 0 and a read goes through a slot.
pub(crate) fn init_leaf(page: &mut Page) {
    page.set_kind(PageKind::BTreeLeaf);
    set_leaf_prev(page, None);
    set_leaf_next(page, None);
    clear_entries(page, LEAF_DIRECTORY);
}

/// Writes the header of an empty internal page over `page`: kind, `P0`, no key.
pub(crate) fn init_internal(page: &mut Page, leftmost: PageId) {
    page.set_kind(PageKind::BTreeInternal);
    set_leftmost_child(page, leftmost);
    clear_entries(page, INTERNAL_DIRECTORY);
}

/// The leaf to the left of `page`, `None` for the leftmost leaf of the tree.
pub(crate) fn leaf_prev(page: &Page) -> Option<PageId> {
    sibling(page, OFF_LEAF_PREV)
}

/// The leaf to the right of `page`, `None` for the rightmost leaf of the tree.
pub(crate) fn leaf_next(page: &Page) -> Option<PageId> {
    sibling(page, OFF_LEAF_NEXT)
}

/// Writes the link to the leaf on the left.
pub(crate) fn set_leaf_prev(page: &mut Page, prev: Option<PageId>) {
    set_sibling(page, OFF_LEAF_PREV, prev);
}

/// Writes the link to the leaf on the right.
pub(crate) fn set_leaf_next(page: &mut Page, next: Option<PageId>) {
    set_sibling(page, OFF_LEAF_NEXT, next);
}

/// Number of entries of a leaf.
pub(crate) fn leaf_len(page: &Page) -> usize {
    page.slot_count() as usize
}

/// The key and the payload of the entry at `index`, counted in key order from 0.
///
/// # Errors
///
/// [`InternalError::Bug`] for a page whose kind is not [`PageKind::BTreeLeaf`] and for an
/// `index` at or past [`leaf_len`]; [`InternalError::Corruption`] for a slot that points
/// outside the space the directory leaves, or for an entry whose two lengths disagree with the
/// length of its slot (`a_slot_pointing_outside_the_page_is_corruption`).
pub(crate) fn leaf_entry(page: &Page, index: usize) -> Result<(&[u8], &[u8]), InternalError> {
    expect_kind(page, PageKind::BTreeLeaf)?;
    entry_at(page, LEAF_DIRECTORY, index)
}

/// The entries of a leaf, in key order.
///
/// The slots are read and checked before the iterator is handed back, so the iterator itself
/// yields no error: a page whose directory does not hold is refused here, with the errors of
/// [`leaf_entry`].
///
/// # Errors
///
/// Those of [`leaf_entry`].
pub(crate) fn leaf_iter(
    page: &Page,
) -> Result<impl Iterator<Item = (&[u8], &[u8])>, InternalError> {
    expect_kind(page, PageKind::BTreeLeaf)?;
    let mut entries = Vec::with_capacity(leaf_len(page));
    for index in 0..leaf_len(page) {
        entries.push(entry_at(page, LEAF_DIRECTORY, index)?);
    }
    Ok(entries.into_iter())
}

/// Puts `(key, payload)` in the leaf, its slot in key order.
///
/// A key equal to one already on the page is put **after** it, so entries of equal keys come
/// back in the order they were put (`duplicate_keys_keep_insertion_order`). Refusing a
/// duplicate is the business of a unique index ([`super::index`]).
///
/// # Errors
///
/// [`InternalError::Bug`] for a page whose kind is not [`PageKind::BTreeLeaf`], and for a
/// `key.len() + payload.len()` past [`MAX_ENTRY_BYTES`]; [`InternalError::Corruption`] for a
/// directory that does not hold. A page with no room answers `Ok(`[`Put::Full`]`)`, which is
/// not an error: it is what asks the caller for a split.
pub(crate) fn leaf_put(page: &mut Page, key: &[u8], payload: &[u8]) -> Result<Put, InternalError> {
    expect_kind(page, PageKind::BTreeLeaf)?;
    check_entry_size(page.page_id(), key.len(), payload.len())?;
    let index = insert_index(page, LEAF_DIRECTORY, key)?;
    put_entry(page, LEAF_DIRECTORY, index, key, payload)
}

/// The child to the left of the keys of an internal page, `P0`.
pub(crate) fn leftmost_child(page: &Page) -> PageId {
    PageId(read_u64(page, OFF_LEFTMOST_CHILD))
}

/// Writes `P0`.
pub(crate) fn set_leftmost_child(page: &mut Page, child: PageId) {
    write_u64(page, OFF_LEFTMOST_CHILD, child.0);
}

/// Number of keys of an internal page. It points at one child more than that
/// (`internal_put_and_split`).
pub(crate) fn internal_len(page: &Page) -> usize {
    page.slot_count() as usize
}

/// The key `k(index + 1)` of an internal page, counted in key order from 0.
///
/// # Errors
///
/// Those of [`leaf_entry`], read for [`PageKind::BTreeInternal`].
pub(crate) fn internal_key(page: &Page, index: usize) -> Result<&[u8], InternalError> {
    expect_kind(page, PageKind::BTreeInternal)?;
    let (key, _) = entry_at(page, INTERNAL_DIRECTORY, index)?;
    Ok(key)
}

/// The child `P(index)` of an internal page: `P0` for 0, the child of the entry `index - 1`
/// past that. A page of `n` keys answers for an `index` in `0..=n`.
///
/// # Errors
///
/// Those of [`leaf_entry`], read for [`PageKind::BTreeInternal`], plus
/// [`InternalError::Corruption`] for an entry whose payload is not the eight bytes of a
/// [`PageId`].
pub(crate) fn internal_child(page: &Page, index: usize) -> Result<PageId, InternalError> {
    expect_kind(page, PageKind::BTreeInternal)?;
    if index == 0 {
        return Ok(leftmost_child(page));
    }
    let (_, payload) = entry_at(page, INTERNAL_DIRECTORY, index - 1)?;
    child_of(page.page_id(), payload)
}

/// Puts the pair `(key, child)` in an internal page, its slot in key order: `child` becomes the
/// subtree of the keys at or above `key`, up to the next key of the page.
///
/// # Errors
///
/// Those of [`leaf_put`], read for [`PageKind::BTreeInternal`]; the eight bytes of the child
/// count towards [`MAX_ENTRY_BYTES`].
pub(crate) fn internal_put(
    page: &mut Page,
    key: &[u8],
    child: PageId,
) -> Result<Put, InternalError> {
    expect_kind(page, PageKind::BTreeInternal)?;
    check_entry_size(page.page_id(), key.len(), CHILD_BYTES)?;
    let index = insert_index(page, INTERNAL_DIRECTORY, key)?;
    put_entry(page, INTERNAL_DIRECTORY, index, key, &child.0.to_le_bytes())
}

/// Splits the leaf `left` in two and answers the page that took its upper half.
///
/// The entries are shared by their byte size, the cut then moved to the nearest index where
/// the key changes so that a run of equal keys goes to one side of it
/// (`tests::split_leaf_keeps_a_run_of_equal_keys_on_one_side`); the lower half stays on
/// `left`. A leaf carrying a long run therefore comes out of the split lopsided. Both pages are
/// rewritten from their entries, so the free space of each is one block. The chain of the
/// leaves is closed on both sides: `left.next` and `right.prev` name each other, `right.next`
/// takes what `left.next` held, and the leaf that held `left` as its `prev` is rewritten to
/// hold `right` (`split_leaf_relinks_the_old_next_sibling`).
///
/// The separator is the first key of the right leaf, and that key stays in the leaf.
///
/// # Errors
///
/// [`InternalError::Bug`] for a page whose kind is not [`PageKind::BTreeLeaf`], for a leaf of
/// fewer than two entries, which has no split leaving two pages that both hold something
/// (`tests::split_leaf_refuses_a_page_of_one_entry`), and for a leaf whose entries carry one
/// key, where the cut has nowhere to go (`tests::split_leaf_refuses_a_run_it_cannot_cut`); the
/// refusal comes before the allocation. The errors of [`super::alloc::allocate`] and of
/// the buffer pool otherwise. A failure after the allocation leaves the allocated page outside
/// the free list, the orphan the DDL protocol of [`crate::Storage`] describes.
pub(crate) fn split_leaf(
    storage: &DiskStorage,
    left: PageId,
    writes: &mut PageWrites,
) -> Result<Split, InternalError> {
    let left_pin = storage.pool.pin(left)?;
    let (entries, next) = left_pin.with_page(|page| {
        expect_kind(page, PageKind::BTreeLeaf)?;
        let entries = owned_entries(page, LEAF_DIRECTORY)?;
        Ok::<_, InternalError>((entries, leaf_next(page)))
    })??;
    if entries.len() < 2 {
        return Err(InternalError::Bug(format!(
            "leaf {left} holds {} entries; a split asks for at least 2",
            entries.len()
        )));
    }
    let balanced = balance_point(&entry_weights(&entries));
    let middle = cut_between_keys(&entries, balanced).ok_or_else(|| {
        InternalError::Bug(format!(
            "leaf {left} holds {} entries that carry one key: a cut would leave that key on \
             both sides of the separator, and this build has no split for a run of equal keys \
             longer than a page",
            entries.len()
        ))
    })?;
    let separator = entries[middle].0.clone();

    let right = allocate(storage)?;
    let right_pin = storage.pool.pin(right)?;
    right_pin.with_page_mut(|page| {
        init_leaf(page);
        set_leaf_prev(page, Some(left));
        set_leaf_next(page, next);
        fill(page, LEAF_DIRECTORY, &entries[middle..], right)
    })??;
    writes.mark(storage, right)?;
    drop(right_pin);

    left_pin.with_page_mut(|page| {
        clear_entries(page, LEAF_DIRECTORY);
        set_leaf_next(page, Some(right));
        fill(page, LEAF_DIRECTORY, &entries[..middle], left)
    })??;
    writes.mark(storage, left)?;
    drop(left_pin);

    if let Some(far) = next {
        let far_pin = storage.pool.pin(far)?;
        far_pin.with_page_mut(|page| {
            expect_kind(page, PageKind::BTreeLeaf)?;
            set_leaf_prev(page, Some(right));
            Ok::<_, InternalError>(())
        })??;
        writes.mark(storage, far)?;
    }

    Ok(Split { right, separator })
}

/// Splits the internal page `left` in two and answers the page that took its upper half.
///
/// The promoted key leaves both pages: with keys `k1..km` and children `P0..Pm`, promoting
/// `kj` leaves `P0..P(j-1)` and `k1..k(j-1)` on the left, and puts `Pj..Pm` and `k(j+1)..km`
/// on the right, whose `P0` is `Pj`. Each page therefore comes out with one pointer more than
/// it has keys, the shape [`internal_child`] reads.
///
/// The key that goes up is one that differs from the key before it and from the key after it,
/// the nearest such key to the middle by byte size
/// (`tests::split_internal_promotes_a_key_out_of_a_run`): a promoted key equal to a key left
/// behind would sit on one of the two pages as well as in the parent.
///
/// # Errors
///
/// [`InternalError::Bug`] for a page whose kind is not [`PageKind::BTreeInternal`], for a page
/// of fewer than three keys: one key goes up, and a page left with zero key would hold a
/// single child (`tests::split_internal_refuses_a_page_of_two_keys`), and for a page holding no
/// key that differs from both of its neighbours
/// (`tests::split_internal_refuses_a_page_it_cannot_promote_out_of`); the errors of
/// [`super::alloc::allocate`] and of the buffer pool otherwise.
pub(crate) fn split_internal(
    storage: &DiskStorage,
    left: PageId,
    writes: &mut PageWrites,
) -> Result<Split, InternalError> {
    let left_pin = storage.pool.pin(left)?;
    let (entries, leftmost) = left_pin.with_page(|page| {
        expect_kind(page, PageKind::BTreeInternal)?;
        let entries = owned_entries(page, INTERNAL_DIRECTORY)?;
        Ok::<_, InternalError>((entries, leftmost_child(page)))
    })??;
    if entries.len() < 3 {
        return Err(InternalError::Bug(format!(
            "internal page {left} holds {} keys; a split asks for at least 3, one to go up and \
             one to stay on each side",
            entries.len()
        )));
    }
    // The promoted key is taken from the middle by byte size, then moved to the nearest key
    // that differs from the one before it and from the one after it, which also keeps a key on
    // each side of it.
    let balanced = balance_point(&entry_weights(&entries)).min(entries.len() - 2);
    let promoted = key_to_promote(&entries, balanced).ok_or_else(|| {
        InternalError::Bug(format!(
            "internal page {left} holds {} keys, none of which differs from both of its \
             neighbours: promoting one would leave an equal key on one of the two pages",
            entries.len()
        ))
    })?;
    let separator = entries[promoted].0.clone();
    let right_leftmost = child_of(left, &entries[promoted].1)?;

    let right = allocate(storage)?;
    let right_pin = storage.pool.pin(right)?;
    right_pin.with_page_mut(|page| {
        init_internal(page, right_leftmost);
        fill(page, INTERNAL_DIRECTORY, &entries[promoted + 1..], right)
    })??;
    writes.mark(storage, right)?;
    drop(right_pin);

    left_pin.with_page_mut(|page| {
        clear_entries(page, INTERNAL_DIRECTORY);
        set_leftmost_child(page, leftmost);
        fill(page, INTERNAL_DIRECTORY, &entries[..promoted], left)
    })??;
    writes.mark(storage, left)?;

    Ok(Split { right, separator })
}

/// How the pages a write of a tree changes are marked in the buffer pool.
///
/// Two shapes. [`PageWrites::unlogged`] is what the page layer does on its own: a page comes
/// back dirty at `page_lsn` 0 and without the `in_progress` flag, so a flush of the pool writes
/// it. After [`PageWrites::hold`], each page a write touches takes the LSN of the journal
/// record that describes that write and carries `in_progress`: the pool then leaves it out of
/// `data` while the transaction runs ([`super::buffer::BufferPool::flush_all`] skips a page
/// flagged that way, and the eviction refuses it) and writes it once the record is durable and
/// the flag lifted. The pages flagged are collected for [`PageWrites::take_held`], which the
/// owner of the transaction drains to lift the flag at `commit` or `rollback`
/// (`super::clustered::ClusteredTable::release_pages`).
///
/// This is the no-steal rule [`super::version::HeapTable::note_page`] applies to a heap page,
/// brought to the pages of a tree
/// (`super::clustered::tests::clustered_leaf_stays_out_of_data_until_commit`). An index whose
/// writes are not journalled keeps [`PageWrites::unlogged`], which is what [`BTree::create`]
/// and [`BTree::open`] start from.
#[derive(Debug)]
pub(crate) struct PageWrites {
    /// LSN written in the header of each page marked, `Lsn(0)` when no record describes the
    /// write. [`super::buffer::BufferPool::mark_dirty`] does not move a page LSN backwards, so
    /// `Lsn(0)` leaves the LSN the page carries.
    lsn: Lsn,
    /// Whether a page marked carries `in_progress`.
    hold: bool,
    /// The pages marked `in_progress` since the last [`PageWrites::take_held`].
    held: BTreeSet<PageId>,
}

impl PageWrites {
    /// Marks with no journal record behind the write: `Lsn(0)`, no flag, nothing collected.
    pub(crate) fn unlogged() -> Self {
        Self {
            lsn: Lsn(0),
            hold: false,
            held: BTreeSet::new(),
        }
    }

    /// The writes that follow carry `lsn` and the `in_progress` flag, and their pages are
    /// collected for [`PageWrites::take_held`].
    pub(crate) fn hold(&mut self, lsn: Lsn) {
        self.lsn = lsn;
        self.hold = true;
    }

    /// The pages flagged since the last call, handed to the caller that lifts the flag.
    pub(crate) fn take_held(&mut self) -> BTreeSet<PageId> {
        std::mem::take(&mut self.held)
    }

    /// Marks a page a write has just rewritten: dirty at the LSN of this marker, flagged or
    /// not as the marker says.
    ///
    /// The caller holds the pin: [`super::buffer::BufferPool::mark_dirty`] asks for one, and so
    /// does [`super::buffer::BufferPool::set_in_progress`] when it sets the flag.
    ///
    /// # Errors
    ///
    /// Those of the buffer pool.
    fn mark(&mut self, storage: &DiskStorage, id: PageId) -> Result<(), InternalError> {
        storage.pool.mark_dirty(id, self.lsn)?;
        storage.pool.set_in_progress(id, self.hold)?;
        if self.hold {
            self.held.insert(id);
        }
        Ok(())
    }
}

/// Puts `entries` on a page that was just cleared, in the order they are given.
///
/// A [`Put::Full`] here is [`InternalError::Bug`]: the entries come from one page, so a half of
/// them goes on a page of the same size.
fn fill(
    page: &mut Page,
    directory: usize,
    entries: &[Entry],
    id: PageId,
) -> Result<(), InternalError> {
    for (key, payload) in entries {
        let index = page.slot_count() as usize;
        if put_entry(page, directory, index, key, payload)? == Put::Full {
            return Err(InternalError::Bug(format!(
                "page {id} has no room for the {} entries of its half of a split",
                entries.len()
            )));
        }
    }
    Ok(())
}

/// The entries of a page, copied out of it, in key order.
fn owned_entries(page: &Page, directory: usize) -> Result<Vec<Entry>, InternalError> {
    let count = page.slot_count() as usize;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let (key, payload) = entry_at(page, directory, index)?;
        entries.push((key.to_vec(), payload.to_vec()));
    }
    Ok(entries)
}

/// The number of bytes each entry takes on a page, its slot included.
fn entry_weights(entries: &[Entry]) -> Vec<usize> {
    entries
        .iter()
        .map(|(key, payload)| SLOT_SIZE + ENTRY_OVERHEAD + key.len() + payload.len())
        .collect()
}

/// The index the upper half of a leaf starts at, moved out of a run of equal keys.
///
/// `balanced` is where the byte sizes put the cut ([`balance_point`]). The answer is the index
/// nearest to it, among `1..entries.len()`, whose key differs from the key of the entry before
/// it: the entries that share a key then go to one side of the separator
/// (`tests::split_leaf_keeps_a_run_of_equal_keys_on_one_side`). A tie goes to the lower index,
/// so the left page is the lighter one of the two candidates
/// (`tests::a_cut_lands_between_two_different_keys`).
///
/// `None` for a leaf whose entries carry one key: it has no such index, and [`split_leaf`]
/// turns that into [`InternalError::Bug`]
/// (`tests::split_leaf_refuses_a_run_it_cannot_cut`).
fn cut_between_keys(entries: &[Entry], balanced: usize) -> Option<usize> {
    nearest(
        (1..entries.len()).filter(|&index| entries[index - 1].0 != entries[index].0),
        balanced,
    )
}

/// The index of the key an internal split promotes, moved out of a run of equal keys.
///
/// The promoted key leaves both pages, so it separates them when it differs from the key
/// before it **and** from the key after it. The answer is the index of that shape nearest
/// `balanced`, among `1..entries.len() - 1`, which leaves a key on each side
/// (`tests::split_internal_promotes_a_key_out_of_a_run`); `None` when the page holds no such
/// key, which [`split_internal`] turns into [`InternalError::Bug`]
/// (`tests::split_internal_refuses_a_page_it_cannot_promote_out_of`).
fn key_to_promote(entries: &[Entry], balanced: usize) -> Option<usize> {
    nearest(
        (1..entries.len() - 1).filter(|&index| {
            entries[index - 1].0 != entries[index].0 && entries[index].0 != entries[index + 1].0
        }),
        balanced,
    )
}

/// The candidate index nearest `balanced`, the lower one of two at the same distance.
fn nearest(candidates: impl Iterator<Item = usize>, balanced: usize) -> Option<usize> {
    candidates.min_by_key(|&index| (index.abs_diff(balanced), index))
}

/// The index the upper half starts at: the first one that carries the running weight past half
/// of the total.
///
/// The answer is at least 1 and at most `weights.len() - 1`, so neither half is empty
/// (`balance_point_keeps_both_halves`); `weights` holds at least two entries, which the callers
/// check before they call this.
fn balance_point(weights: &[usize]) -> usize {
    let total: usize = weights.iter().sum();
    let mut carried = 0;
    for (index, weight) in weights.iter().enumerate() {
        carried += weight;
        if carried * 2 >= total && index + 1 < weights.len() {
            return index + 1;
        }
    }
    weights.len() - 1
}

/// Reads the eight bytes of a child pointer out of the payload of an internal entry.
fn child_of(id: PageId, payload: &[u8]) -> Result<PageId, InternalError> {
    let raw: [u8; CHILD_BYTES] = payload.try_into().map_err(|_| {
        InternalError::Corruption(format!(
            "internal page {id} holds a child pointer of {} bytes, expected {CHILD_BYTES}",
            payload.len()
        ))
    })?;
    Ok(PageId(u64::from_le_bytes(raw)))
}

/// Refuses a page whose kind is not the one the caller reads it as.
fn expect_kind(page: &Page, expected: PageKind) -> Result<(), InternalError> {
    let kind = page.kind()?;
    if kind != expected {
        return Err(InternalError::Bug(format!(
            "page {} is a {kind:?} page, read here as a {expected:?} one",
            page.page_id()
        )));
    }
    Ok(())
}

/// Refuses an entry this build does not put on a page.
fn check_entry_size(id: PageId, key: usize, payload: usize) -> Result<(), InternalError> {
    if key + payload > MAX_ENTRY_BYTES {
        return Err(InternalError::Bug(format!(
            "entry of {key} key bytes and {payload} payload bytes does not go on page {id}: \
             this build puts at most {MAX_ENTRY_BYTES} bytes in one entry"
        )));
    }
    Ok(())
}

/// Empties the directory of a page: no slot, `lower` back at the start of the directory.
fn clear_entries(page: &mut Page, directory: usize) {
    page.set_slot_count(0);
    page.set_lower(directory as u16);
}

/// The first byte of the entries of a page, that is the end of its free space. A page with no
/// entry answers [`PAGE_SIZE`].
fn upper(page: &Page, directory: usize) -> Result<usize, InternalError> {
    let mut upper = PAGE_SIZE;
    for index in 0..page.slot_count() as usize {
        let (offset, _) = checked_slot(page, directory, index)?;
        upper = upper.min(offset);
    }
    Ok(upper)
}

/// Bytes a page has left between its directory and its entries.
///
/// # Errors
///
/// Those of [`directory_end`] and of [`upper`].
fn free_space(page: &Page, directory: usize) -> Result<usize, InternalError> {
    let used = directory_end(page, directory)?;
    Ok(upper(page, directory)?.saturating_sub(used))
}

/// The first byte after the slot directory of a page, that is the start of its free space.
///
/// # Errors
///
/// [`InternalError::Corruption`] for a `slot_count` whose directory would run past
/// [`PAGE_SIZE`]: the reads that follow it would then leave the page, so they are refused here
/// rather than indexed (`tests::a_slot_count_past_the_page_is_corruption`).
fn directory_end(page: &Page, directory: usize) -> Result<usize, InternalError> {
    let count = page.slot_count() as usize;
    let end = directory + count * SLOT_SIZE;
    if end > PAGE_SIZE {
        return Err(InternalError::Corruption(format!(
            "page {} declares {count} slots, a directory of {end} bytes in a page of \
             {PAGE_SIZE}",
            page.page_id()
        )));
    }
    Ok(end)
}

/// The index `key` takes in the directory: behind the entries whose key is at or below it, so
/// that an equal key goes after the ones already there.
fn insert_index(page: &Page, directory: usize, key: &[u8]) -> Result<usize, InternalError> {
    let (mut low, mut high) = (0, page.slot_count() as usize);
    while low < high {
        let middle = (low + high) / 2;
        let (found, _) = entry_at(page, directory, middle)?;
        if found <= key {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

/// Writes an entry at the end of the free space and its slot at `index`, the slots at and past
/// `index` moving up by one.
///
/// The size of the entry is the caller's to check ([`check_entry_size`]): a page with no room
/// for it answers [`Put::Full`] and is left as it was.
fn put_entry(
    page: &mut Page,
    directory: usize,
    index: usize,
    key: &[u8],
    payload: &[u8],
) -> Result<Put, InternalError> {
    let count = page.slot_count() as usize;
    let length = ENTRY_OVERHEAD + key.len() + payload.len();
    if free_space(page, directory)? < length + SLOT_SIZE {
        return Ok(Put::Full);
    }
    let at = upper(page, directory)? - length;
    write_u16(page, at, key.len())?;
    page.0[at + 2..at + 2 + key.len()].copy_from_slice(key);
    write_u16(page, at + 2 + key.len(), payload.len())?;
    page.0[at + 4 + key.len()..at + length].copy_from_slice(payload);

    let slot = directory + index * SLOT_SIZE;
    let end = directory + count * SLOT_SIZE;
    page.0.copy_within(slot..end, slot + SLOT_SIZE);
    write_u16(page, slot, at)?;
    write_u16(page, slot + 2, length)?;
    let slots = bounded(count + 1)?;
    page.set_slot_count(slots);
    page.set_lower(bounded(directory + (count + 1) * SLOT_SIZE)?);
    Ok(Put::Inserted)
}

/// The offset and the length of the slot `index`, checked against the page it sits in.
fn checked_slot(
    page: &Page,
    directory: usize,
    index: usize,
) -> Result<(usize, usize), InternalError> {
    let count = page.slot_count() as usize;
    if index >= count {
        return Err(InternalError::Bug(format!(
            "page {} holds {count} slots, slot {index} was asked for",
            page.page_id()
        )));
    }
    let end = directory_end(page, directory)?;
    let slot = directory + index * SLOT_SIZE;
    let offset = read_u16(page, slot);
    let length = read_u16(page, slot + 2);
    if length < ENTRY_OVERHEAD || offset < end || offset + length > PAGE_SIZE {
        return Err(InternalError::Corruption(format!(
            "page {} has slot {index} at offset {offset} of length {length}, outside the bytes \
             its directory of {count} slots leaves in {PAGE_SIZE}",
            page.page_id()
        )));
    }
    Ok((offset, length))
}

/// The key and the payload of the entry of the slot `index`.
fn entry_at(page: &Page, directory: usize, index: usize) -> Result<(&[u8], &[u8]), InternalError> {
    let (offset, length) = checked_slot(page, directory, index)?;
    let key_len = read_u16(page, offset);
    if ENTRY_OVERHEAD + key_len > length {
        return Err(InternalError::Corruption(format!(
            "page {} has entry {index} declaring {key_len} key bytes in a slot of {length}",
            page.page_id()
        )));
    }
    let payload_len = read_u16(page, offset + 2 + key_len);
    if ENTRY_OVERHEAD + key_len + payload_len != length {
        return Err(InternalError::Corruption(format!(
            "page {} has entry {index} declaring {key_len} key bytes and {payload_len} payload \
             bytes in a slot of {length}",
            page.page_id()
        )));
    }
    let key = &page.0[offset + 2..offset + 2 + key_len];
    let payload = &page.0[offset + 4 + key_len..offset + length];
    Ok((key, payload))
}

/// The neighbour written at `at`, `None` for [`NO_SIBLING`].
fn sibling(page: &Page, at: usize) -> Option<PageId> {
    match read_u64(page, at) {
        NO_SIBLING => None,
        id => Some(PageId(id)),
    }
}

/// Writes a neighbour at `at`, [`NO_SIBLING`] for `None`.
fn set_sibling(page: &mut Page, at: usize, id: Option<PageId>) {
    write_u64(page, at, id.map_or(NO_SIBLING, |id| id.0));
}

/// Reads a little-endian `u16` of the page as the offset or the length it is.
fn read_u16(page: &Page, at: usize) -> usize {
    u16::from_le_bytes([page.0[at], page.0[at + 1]]) as usize
}

/// Writes an offset or a length as a little-endian `u16`.
fn write_u16(page: &mut Page, at: usize, value: usize) -> Result<(), InternalError> {
    let value = bounded(value)?;
    page.0[at..at + 2].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// A byte count of a page as a `u16`. Past 65 535 it is [`InternalError::Bug`], which the sizes
/// written by this file keep below [`PAGE_SIZE`].
fn bounded(value: usize) -> Result<u16, InternalError> {
    u16::try_from(value).map_err(|_| {
        InternalError::Bug(format!(
            "{value} does not fit the two bytes a page spends on an offset"
        ))
    })
}

/// Reads a little-endian `u64` of the page.
fn read_u64(page: &Page, at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&page.0[at..at + 8]);
    u64::from_le_bytes(raw)
}

/// Writes a little-endian `u64` in the page.
fn write_u64(page: &mut Page, at: usize, value: u64) {
    page.0[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Largest entry [`BTree::insert`] takes: the bytes of [`encode_entry_key`], that is the encoded
/// key columns **and** the payload, 2 028 of the 8 192 bytes of a page
/// (`tests::the_ceiling_of_an_entry_is_a_quarter_of_a_leaf` recomputes it).
///
/// The bound is a **quarter of the space of a leaf**, one slot and one entry header taken off.
/// An entry that big leaves each half of a split with room for the entry that caused it: the
/// cut [`balance_point`] picks leaves the heavier half below three quarters of the page, and a
/// quarter is then what still goes in. The size drawn below the ceiling read back against a
/// model in `tests::sizes_drawn_below_the_ceiling_read_back_against_a_model` (600 insertions,
/// one seed); a page whose halves would not take the entry is refused before anything is
/// allocated, by [`leaf_split_fits`]
/// (`tests::a_half_of_a_split_without_room_is_refused_before_the_allocation`).
///
/// A separator of an internal page carries a whole entry key, payload included; an internal
/// page holds three separators of this size, which is what [`split_internal`] asks for — one to
/// go up and one to stay on each side (same test).
///
/// The refusal of an entry above the ceiling comes before the descent, so a page is neither
/// allocated nor written (`tests::an_entry_above_the_ceiling_is_refused_before_any_write`).
/// [`super::clustered`] takes this constant as the size past which a clustered row goes to an
/// overflow chain instead of into the tree.
pub(crate) const MAX_INSERT_BYTES: usize =
    (PAGE_SIZE - LEAF_DIRECTORY) / 4 - SLOT_SIZE - ENTRY_OVERHEAD;

/// Largest number of internal pages a descent walks through before it calls the tree corrupt.
///
/// A tree of this build is far shorter than that: the two-level tree of
/// `tests::split_grows_root` and the three-level one of
/// `tests::a_third_level_appears_when_the_root_splits` are what the tests reach. The bound is
/// there so that a child pointer that leads back up the tree is [`InternalError::Corruption`]
/// rather than a descent that never ends.
const MAX_DEPTH: usize = 64;

/// One column of the key of a [`BTree`], with what its comparison needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrderColumn {
    /// Reverses the order of the column, `NULL` included.
    descending: bool,
    /// The collation handed to [`compare`]: the column's, or the default one.
    collation: Collation,
}

/// The order the keys of one tree follow: `NULL` first, then [`compare`] with the collation of
/// the column, reversed by `descending`, column by column from left to right.
///
/// This is the order `memory/key_order.rs` gives the in-memory engine, written again here
/// because `disk/` does not import `memory/`. The
/// comparison of two values is not written again: one column goes through [`compare`] in both
/// places, so `I32(-1)` sorts below `I32(0)` here as it does there, where the little-endian
/// bytes of the encoding would have put it above
/// (`tests::negative_ints_sort_below_zero_as_compare_orders_them`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyOrder {
    /// The columns of the key, in key order.
    columns: Vec<OrderColumn>,
}

impl KeyOrder {
    /// Builds the order of the key `columns` over a table whose columns have the types
    /// `types`.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a key of no column and for a [`KeyColumn::column`] outside
    /// `types` (`tests::a_key_that_does_not_match_the_columns_is_a_bug`).
    pub(crate) fn new(columns: &[KeyColumn], types: &[TypeInfo]) -> Result<Self, InternalError> {
        if columns.is_empty() {
            return Err(InternalError::Bug("a B+tree key has no column".to_string()));
        }
        let mut out = Vec::with_capacity(columns.len());
        for column in columns {
            let position = usize::from(column.column);
            let Some(info) = types.get(position) else {
                return Err(InternalError::Bug(format!(
                    "key column {} is outside the {} columns of the table",
                    column.column,
                    types.len()
                )));
            };
            out.push(OrderColumn {
                descending: column.descending,
                collation: info.collation.unwrap_or(Collation::DEFAULT),
            });
        }
        Ok(Self { columns: out })
    }

    /// Number of columns of the key.
    fn arity(&self) -> usize {
        self.columns.len()
    }

    /// Compares two keys, or a key and a prefix of one, over their `min(a.len(), b.len())`
    /// first columns: a prefix is `Equal` to a key that starts with it.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] carrying the message of [`compare`] for two values of
    /// incompatible families, [`InternalError::Corruption`] for a comparison of two non-`NULL`
    /// values that answers UNKNOWN.
    fn compare_prefix(&self, a: &[Value], b: &[Value]) -> Result<Ordering, InternalError> {
        for (column, (x, y)) in self.columns.iter().zip(a.iter().zip(b.iter())) {
            let ordering = compare_column(x, y, &column.collation)?;
            let ordering = if column.descending {
                ordering.reverse()
            } else {
                ordering
            };
            if ordering != Ordering::Equal {
                return Ok(ordering);
            }
        }
        Ok(Ordering::Equal)
    }

    /// Refuses a key that does not carry one value per column: [`BTree::insert`] and
    /// [`BTree::delete`] name one entry, not a set of them
    /// (`tests::a_key_that_does_not_match_the_columns_is_a_bug`).
    fn check_key(&self, key: &[Value]) -> Result<(), InternalError> {
        if key.len() != self.arity() {
            return Err(InternalError::Bug(format!(
                "a B+tree entry carries {} values, the key has {} columns",
                key.len(),
                self.arity()
            )));
        }
        Ok(())
    }

    /// Refuses a bound of a [`BTree::seek`] that carries more values than the key has columns.
    /// A shorter one is a prefix, and names the keys that start with it
    /// (`tests::a_two_column_key_seeks_by_prefix`).
    fn check_prefix(&self, prefix: &[Value]) -> Result<(), InternalError> {
        if prefix.len() > self.arity() {
            return Err(InternalError::Bug(format!(
                "a key bound carries {} values, the key has {} columns",
                prefix.len(),
                self.arity()
            )));
        }
        Ok(())
    }
}

/// Compares one column of two keys, `NULL` first: `NULL` equal to `NULL`, `NULL` below
/// anything else, otherwise [`compare`] with the collation of the column.
fn compare_column(a: &Value, b: &Value, collation: &Collation) -> Result<Ordering, InternalError> {
    match (a, b) {
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        (Value::Null, _) => Ok(Ordering::Less),
        (_, Value::Null) => Ok(Ordering::Greater),
        _ => {
            let ordering = compare(a, b, collation).map_err(|err| {
                InternalError::Bug(format!("comparing two key values of a B+tree: {err}"))
            })?;
            ordering.ok_or_else(|| {
                InternalError::Corruption(format!(
                    "comparing the non-NULL key values {a:?} and {b:?} answered UNKNOWN"
                ))
            })
        }
    }
}

/// Builds the bytes a page holds for the entry `(key, payload)`: the values of the key columns
/// and the payload, written as one row by [`encode_row`].
///
/// # Why the payload rides in the key of the page
///
/// [`split_leaf`] cuts a page where the byte keys of two neighbours differ, and refuses a page
/// whose entries carry one key (`tests::split_leaf_refuses_a_run_it_cannot_cut`). Two entries
/// that share their key columns therefore have to differ by something else, and that something
/// is the payload: [`super::clustered`] and [`super::index`] put a [`crate::RowId`] there,
/// which tells two rows
/// apart. A leaf filled with entries of the key `7` then splits like any other
/// (`tests::a_page_full_of_one_key_splits_because_the_payloads_differ`), and the payload field
/// of the page entry is left empty, the key holding both halves.
///
/// Two entries with the same key **and** the same payload are still equal bytes; a page filled
/// with those is the refusal above.
pub(crate) fn encode_entry_key(key: &[Value], payload: &[u8]) -> Vec<u8> {
    let mut values = Vec::with_capacity(key.len() + 1);
    values.extend_from_slice(key);
    values.push(Value::Bytes(payload.to_vec()));
    let mut out = Vec::new();
    encode_row(&Row(values), &mut out);
    out
}

/// Reads back what [`encode_entry_key`] wrote: the values of the key columns, then the payload.
///
/// # Errors
///
/// Those of [`decode_row`], and [`InternalError::Corruption`] for a row whose last value is not
/// the [`Value::Bytes`] the payload was written as.
pub(crate) fn decode_entry_key(bytes: &[u8]) -> Result<(Vec<Value>, Vec<u8>), InternalError> {
    let Row(mut values) = decode_row(bytes)?;
    match values.pop() {
        Some(Value::Bytes(payload)) => Ok((values, payload)),
        other => Err(InternalError::Corruption(format!(
            "a B+tree entry ends with {other:?} where its payload was expected"
        ))),
    }
}

/// Where a search sits among the entries that share the values it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PayloadBound {
    /// Where a lower bound sits: the search lands on the first entry that carries those values
    /// (the `Included(3)` of `tests::seek_between_and_full_both_directions`).
    Lowest,
    /// At one payload: the entry an [`BTree::insert`] or a [`BTree::delete`] names.
    At(Vec<u8>),
    /// Where an upper bound sits: the search lands past the last entry that carries those
    /// values (the `Excluded(7)` of `tests::seek_between_and_full_both_directions`).
    Highest,
}

/// What a descent and a binary search compare an entry against: the values of the key columns,
/// or a prefix of them, and where the search sits among the payloads of those values.
#[derive(Debug, Clone, PartialEq)]
struct Probe {
    /// The values searched for, `Vec::new()` for a bound that names the whole tree.
    values: Vec<Value>,
    /// Where the search sits among the entries whose values are those.
    payload: PayloadBound,
}

/// One entry of a tree as its caller reads it: the values of the key columns and the payload.
pub(crate) type TreeEntry = (Vec<Value>, Vec<u8>);

/// What a walk reads out of one leaf before it lets its pin go: the entries of the leaf, the
/// leaf to its left and the leaf to its right.
type LeafRead = (Vec<TreeEntry>, Option<PageId>, Option<PageId>);

/// A place in the tree: an entry of a leaf, by the identifier of the leaf and the index of the
/// entry in its directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    /// The leaf the entry sits in.
    leaf: PageId,
    /// The index of the entry in the directory of that leaf.
    index: usize,
}

/// Compares one entry of a page against a probe: the key columns first, the payload after them.
fn compare_entry(
    order: &KeyOrder,
    entry: &TreeEntry,
    probe: &Probe,
) -> Result<Ordering, InternalError> {
    let ordering = order.compare_prefix(&entry.0, &probe.values)?;
    if ordering != Ordering::Equal {
        return Ok(ordering);
    }
    Ok(match &probe.payload {
        PayloadBound::Lowest => Ordering::Greater,
        PayloadBound::At(payload) => entry.1.as_slice().cmp(payload.as_slice()),
        PayloadBound::Highest => Ordering::Less,
    })
}

/// The index of the first entry of a leaf at or past `probe`, [`leaf_len`] when the entries of
/// the leaf are below it.
fn leaf_position(page: &Page, order: &KeyOrder, probe: &Probe) -> Result<usize, InternalError> {
    let (mut low, mut high) = (0, leaf_len(page));
    while low < high {
        let middle = (low + high) / 2;
        let (key, _) = entry_at(page, LEAF_DIRECTORY, middle)?;
        let entry = decode_entry_key(key)?;
        if compare_entry(order, &entry, probe)? == Ordering::Less {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

/// The number of keys of an internal page at or below `probe`.
///
/// That number is both the child to walk down to ([`internal_child`], which reads `P0` for 0)
/// and the index the separator of a split takes in the directory, the keys of an internal page
/// of this build being distinct ([`encode_entry_key`]).
fn internal_position(page: &Page, order: &KeyOrder, probe: &Probe) -> Result<usize, InternalError> {
    let (mut low, mut high) = (0, internal_len(page));
    while low < high {
        let middle = (low + high) / 2;
        let (key, _) = entry_at(page, INTERNAL_DIRECTORY, middle)?;
        let entry = decode_entry_key(key)?;
        if compare_entry(order, &entry, probe)? == Ordering::Greater {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    Ok(low)
}

/// Rewrites a page from the entries its directory names, which closes the holes the entries of
/// removed slots left behind.
///
/// [`remove_slot`] takes a slot out of the directory and leaves the bytes of its entry where
/// they are; the space comes back here, the next time the page has no room for a put
/// (`tests::a_leaf_that_lost_slots_takes_entries_again_after_a_compaction`). `P0` of an
/// internal page sits in the header, which this does not touch.
fn compact(page: &mut Page, directory: usize) -> Result<(), InternalError> {
    let id = page.page_id();
    let entries = owned_entries(page, directory)?;
    clear_entries(page, directory);
    fill(page, directory, &entries, id)
}

/// Takes the slot `index` out of the directory of a page, the slots past it moving down by one.
///
/// # Errors
///
/// [`InternalError::Bug`] for an `index` at or past the number of slots, the errors of
/// [`directory_end`] otherwise.
fn remove_slot(page: &mut Page, directory: usize, index: usize) -> Result<(), InternalError> {
    let count = page.slot_count() as usize;
    if index >= count {
        return Err(InternalError::Bug(format!(
            "page {} holds {count} slots, slot {index} was asked to go",
            page.page_id()
        )));
    }
    let end = directory_end(page, directory)?;
    let slot = directory + index * SLOT_SIZE;
    page.0.copy_within(slot + SLOT_SIZE..end, slot);
    page.set_slot_count(bounded(count - 1)?);
    page.set_lower(bounded(directory + (count - 1) * SLOT_SIZE)?);
    Ok(())
}

/// Puts an entry in a leaf at the index the order gives it, compacting the page once when the
/// first put finds no room.
fn place_in_leaf(
    page: &mut Page,
    order: &KeyOrder,
    probe: &Probe,
    bytes: &[u8],
) -> Result<Put, InternalError> {
    check_entry_size(page.page_id(), bytes.len(), 0)?;
    let index = leaf_position(page, order, probe)?;
    if put_entry(page, LEAF_DIRECTORY, index, bytes, &[])? == Put::Inserted {
        return Ok(Put::Inserted);
    }
    if !compaction_makes_room(
        page,
        LEAF_DIRECTORY,
        SLOT_SIZE + ENTRY_OVERHEAD + bytes.len(),
    )? {
        return Ok(Put::Full);
    }
    compact(page, LEAF_DIRECTORY)?;
    let index = leaf_position(page, order, probe)?;
    put_entry(page, LEAF_DIRECTORY, index, bytes, &[])
}

/// Whether rewriting the page from its entries would leave room for `weight` bytes of entry,
/// its slot included.
///
/// A page that answers `false` is left as it is: the put answers [`Put::Full`] and the caller
/// splits, so the page a refused insert walks over keeps its bytes
/// (`tests::five_payloads_around_the_ceiling_are_refused_with_the_tree_intact`).
fn compaction_makes_room(
    page: &Page,
    directory: usize,
    weight: usize,
) -> Result<bool, InternalError> {
    let held: usize = entry_weights(&owned_entries(page, directory)?).iter().sum();
    Ok(held + weight <= PAGE_SIZE - directory)
}

/// Puts a separator and the child it stands in front of in an internal page, at the index the
/// order gives it, compacting the page once when the first put finds no room.
fn place_in_internal(
    page: &mut Page,
    order: &KeyOrder,
    probe: &Probe,
    bytes: &[u8],
    child: PageId,
) -> Result<Put, InternalError> {
    check_entry_size(page.page_id(), bytes.len(), CHILD_BYTES)?;
    let index = internal_position(page, order, probe)?;
    if put_entry(
        page,
        INTERNAL_DIRECTORY,
        index,
        bytes,
        &child.0.to_le_bytes(),
    )? == Put::Inserted
    {
        return Ok(Put::Inserted);
    }
    if !compaction_makes_room(
        page,
        INTERNAL_DIRECTORY,
        SLOT_SIZE + ENTRY_OVERHEAD + bytes.len() + CHILD_BYTES,
    )? {
        return Ok(Put::Full);
    }
    compact(page, INTERNAL_DIRECTORY)?;
    let index = internal_position(page, order, probe)?;
    put_entry(
        page,
        INTERNAL_DIRECTORY,
        index,
        bytes,
        &child.0.to_le_bytes(),
    )
}

/// Checks, before [`split_leaf`] allocates anything, that the half of the split the entry
/// belongs to will have room for it.
///
/// [`balance_point`] shares the entries **that are on the page**; the entry that caused the
/// split is not among them, so a half may come out of the cut with less free space than that
/// entry needs. The check is made here, on the page as it stands, and the answer is
/// [`InternalError::Bug`] with the tree as it was: no page allocated, no page written, and no
/// split left unrecorded in a parent
/// (`tests::a_half_of_a_split_without_room_is_refused_before_the_allocation`).
///
/// The ceiling of [`MAX_INSERT_BYTES`] keeps the entries this build takes away from that case
/// (`tests::sizes_drawn_below_the_ceiling_read_back_against_a_model` draws 600 of them): what
/// is left for this check is a page whose cut moves, that is a run of entries of equal bytes.
/// A page [`split_leaf`] refuses on its own — fewer than two entries, or a cut it cannot place
/// (`tests::split_leaf_refuses_a_run_it_cannot_cut`) — is left to it, which refuses before it
/// allocates too.
fn leaf_split_fits(
    page: &Page,
    order: &KeyOrder,
    probe: &Probe,
    bytes: &[u8],
) -> Result<(), InternalError> {
    let entries = owned_entries(page, LEAF_DIRECTORY)?;
    if entries.len() < 2 {
        return Ok(());
    }
    let weights = entry_weights(&entries);
    let Some(middle) = cut_between_keys(&entries, balance_point(&weights)) else {
        return Ok(());
    };
    let right = goes_right(order, &entries[middle].0, probe)?;
    let half: usize = if right {
        weights[middle..].iter().sum()
    } else {
        weights[..middle].iter().sum()
    };
    let want = SLOT_SIZE + ENTRY_OVERHEAD + bytes.len();
    if half + want > PAGE_SIZE - LEAF_DIRECTORY {
        return Err(InternalError::Bug(format!(
            "the {} half of the split of leaf {} would hold {half} bytes and has no room for \
             the {want} of the entry that caused the split; this build does not split it \
             again",
            if right { "right" } else { "left" },
            page.page_id()
        )));
    }
    Ok(())
}

/// The same check before [`split_internal`]: the promoted key leaves both halves, so the left
/// one is the keys before it and the right one the keys after it.
fn internal_split_fits(
    page: &Page,
    order: &KeyOrder,
    probe: &Probe,
    bytes: &[u8],
) -> Result<(), InternalError> {
    let entries = owned_entries(page, INTERNAL_DIRECTORY)?;
    if entries.len() < 3 {
        return Ok(());
    }
    let weights = entry_weights(&entries);
    let balanced = balance_point(&weights).min(entries.len() - 2);
    let Some(promoted) = key_to_promote(&entries, balanced) else {
        return Ok(());
    };
    let right = goes_right(order, &entries[promoted].0, probe)?;
    let half: usize = if right {
        weights[promoted + 1..].iter().sum()
    } else {
        weights[..promoted].iter().sum()
    };
    let want = SLOT_SIZE + ENTRY_OVERHEAD + bytes.len() + CHILD_BYTES;
    if half + want > PAGE_SIZE - INTERNAL_DIRECTORY {
        return Err(InternalError::Bug(format!(
            "the {} half of the split of internal page {} would hold {half} bytes and has no \
             room for the {want} of the separator that caused the split",
            if right { "right" } else { "left" },
            page.page_id()
        )));
    }
    Ok(())
}

/// Whether an entry at `probe` belongs to the right half of a split whose separator is
/// `separator`: the keys at or above the separator are the right half's.
fn goes_right(order: &KeyOrder, separator: &[u8], probe: &Probe) -> Result<bool, InternalError> {
    let entry = decode_entry_key(separator)?;
    Ok(compare_entry(order, &entry, probe)? != Ordering::Greater)
}

/// A B+tree over the pages of one instance: a root page, and the order its keys follow.
///
/// The structure borrows the instance and holds no page: each call pins what it needs and
/// lets it go before it answers, as [`super::heap::Heap`] does. The state that outlives a call
/// is [`BTree::root`], which [`BTree::insert`] moves when the root splits (`tests::split_grows_root`);
/// the catalogue ([`super::meta`]) is what writes that identifier in a `Meta` page.
///
/// # What an entry is
///
/// A leaf entry holds its key columns **and** its payload in the key of the page entry
/// ([`encode_entry_key`]); the payload field of the page entry is empty. An internal entry
/// holds a separator of the same shape and the eight bytes of a child.
///
/// # Duplicate keys
///
/// Two entries may share their key columns: [`BTree::insert`] takes them, [`BTree::seek`] gives
/// them back in payload order, and [`BTree::delete`] takes one of them away, the one whose
/// payload it is given (`tests::delete_takes_one_occurrence_of_a_duplicated_key`). Refusing a
/// duplicate is the business of a unique index ([`super::index`]).
///
/// # What this build does not do
///
/// A leaf that loses its last entry stays in the chain, empty: there is no merge and no page
/// given back to the allocator, and a walk steps over it
/// (`tests::an_emptied_leaf_stays_in_the_chain_and_a_seek_steps_over_it`). No journal record is
/// written here either: the tree marks its pages with the LSN its caller gives
/// [`BTree::hold_writes`], and appends nothing to the journal itself. A tree left at
/// [`PageWrites::unlogged`] — what an index ([`super::index`]) uses — reaches the data file at the
/// next flush of the pool, with no record behind it.
#[derive(Debug)]
pub(crate) struct BTree<'storage> {
    /// The instance the pages belong to.
    storage: &'storage DiskStorage,
    /// The root of the tree: a leaf while the tree fits one page, an internal page after the
    /// first split that reaches it.
    root: PageId,
    /// The order the keys follow.
    order: KeyOrder,
    /// How the pages of a write are marked in the pool: [`PageWrites::unlogged`] until a
    /// caller asks for [`BTree::hold_writes`].
    writes: PageWrites,
}

impl<'storage> BTree<'storage> {
    /// Creates an empty tree: one leaf, allocated here, which is its root.
    ///
    /// # Errors
    ///
    /// Those of [`KeyOrder::new`], of [`allocate`] and of the buffer pool.
    pub(crate) fn create(
        storage: &'storage DiskStorage,
        columns: &[KeyColumn],
        types: &[TypeInfo],
    ) -> Result<Self, InternalError> {
        let order = KeyOrder::new(columns, types)?;
        let root = allocate(storage)?;
        let pin = storage.pool.pin(root)?;
        pin.with_page_mut(init_leaf)?;
        let mut writes = PageWrites::unlogged();
        writes.mark(storage, root)?;
        drop(pin);
        Ok(Self {
            storage,
            root,
            order,
            writes,
        })
    }

    /// Attaches to the tree whose root is `root`, leaving the pages as they stand.
    ///
    /// # Errors
    ///
    /// Those of [`KeyOrder::new`].
    pub(crate) fn open(
        storage: &'storage DiskStorage,
        root: PageId,
        columns: &[KeyColumn],
        types: &[TypeInfo],
    ) -> Result<Self, InternalError> {
        Ok(Self {
            storage,
            root,
            order: KeyOrder::new(columns, types)?,
            writes: PageWrites::unlogged(),
        })
    }

    /// The root of the tree. It moves when a split reaches the root
    /// (`tests::split_grows_root`).
    pub(crate) fn root(&self) -> PageId {
        self.root
    }

    /// The writes that follow carry `lsn` and the `in_progress` flag on the pages they change
    /// ([`PageWrites::hold`]), so the pool keeps those pages out of `data` until the flag is
    /// lifted (`tests::a_held_write_marks_its_pages_and_hands_them_back`).
    ///
    /// A tree starts at [`PageWrites::unlogged`]; this call moves it and the next one moves the
    /// LSN again. The caller lifts the flag by draining [`BTree::take_held_pages`] and calling
    /// [`super::buffer::BufferPool::set_in_progress`] with `false`, which
    /// `super::clustered::ClusteredTable` does at `commit` and at `rollback`
    /// (`super::clustered::tests::clustered_leaf_stays_out_of_data_until_commit`).
    pub(crate) fn hold_writes(&mut self, lsn: Lsn) {
        self.writes.hold(lsn);
    }

    /// The pages this tree flagged `in_progress` since the last call; the tree drops them from
    /// its account, the caller owning the lifting of the flag. A second call answers an empty
    /// set (`tests::a_held_write_marks_its_pages_and_hands_them_back`).
    pub(crate) fn take_held_pages(&mut self) -> BTreeSet<PageId> {
        self.writes.take_held()
    }

    /// Puts `(key, payload)` in the tree.
    ///
    /// The leaf the key belongs to takes the entry; a leaf with no room is split, the half the
    /// entry belongs to takes it, and the separator goes up to the parent, which may split in
    /// turn. A split of the root gives the tree a new root of one separator and two children,
    /// and moves [`BTree::root`].
    ///
    /// Each level is visited once on the way down and once on the way up: the put after a split
    /// is not tried again, so a cut that shares the bytes unevenly — [`balance_point`] answers
    /// where the byte sizes fall, which a run of long entries pulls away from the middle — costs
    /// one split and no more (`tests::entries_of_uneven_size_read_back_in_order` puts 1 200 of
    /// them, payloads of 8 to 1 600 bytes).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a `key` that does not carry one value per key column, and for
    /// an entry of more than [`MAX_INSERT_BYTES`] bytes — 2 028, a quarter of a leaf — refused
    /// before the descent, so nothing is allocated and nothing is written
    /// (`tests::an_entry_above_the_ceiling_is_refused_before_any_write`,
    /// `tests::five_payloads_around_the_ceiling_are_refused_with_the_tree_intact`).
    ///
    /// [`InternalError::Bug`] again for a page whose halves would leave no room for the entry
    /// that caused the split ([`leaf_split_fits`], [`internal_split_fits`]): that one is
    /// answered before the split allocates or writes anything, so the tree is the one the call
    /// found (`tests::a_half_of_a_split_without_room_is_refused_before_the_allocation`). The
    /// errors of [`split_leaf`], [`split_internal`], [`allocate`] and of the buffer pool
    /// otherwise.
    pub(crate) fn insert(&mut self, key: &[Value], payload: &[u8]) -> Result<(), InternalError> {
        self.order.check_key(key)?;
        let bytes = encode_entry_key(key, payload);
        if bytes.len() > MAX_INSERT_BYTES {
            return Err(InternalError::Bug(format!(
                "an entry of {} bytes of encoded key and payload does not go in the B+tree \
                 rooted at {}: this build takes {MAX_INSERT_BYTES}, a quarter of what a leaf \
                 holds, and a longer payload asks for the overflow chain of a clustered table",
                bytes.len(),
                self.root
            )));
        }
        let probe = Probe {
            values: key.to_vec(),
            payload: PayloadBound::At(payload.to_vec()),
        };
        let (leaf, mut path) = self.descend(&probe)?;
        let Some(mut split) = self.put_in_leaf(leaf, &bytes, &probe)? else {
            return Ok(());
        };
        while let Some(parent) = path.pop() {
            match self.put_in_internal(parent, &split)? {
                Some(higher) => split = higher,
                None => return Ok(()),
            }
        }
        self.grow_root(&split)
    }

    /// Takes one entry `(key, payload)` out of the tree and answers whether it was there.
    ///
    /// A key that is not in the tree, and a key that is there under other payloads, leave the
    /// tree as it is and answer `false`: the caller that deletes a row it has already read is
    /// not told off (`tests::delete_then_seek_absent`). The slot goes out of the directory of
    /// its leaf; the leaf keeps its place in the chain even when it loses its last entry, and
    /// the bytes of the entry come back at the next compaction of the page ([`compact`]).
    ///
    /// # Errors
    ///
    /// Those of [`BTree::insert`] for the key, and the errors of the buffer pool.
    pub(crate) fn delete(&mut self, key: &[Value], payload: &[u8]) -> Result<bool, InternalError> {
        self.order.check_key(key)?;
        let probe = Probe {
            values: key.to_vec(),
            payload: PayloadBound::At(payload.to_vec()),
        };
        let Some(cursor) = self.find_first(&probe)? else {
            return Ok(false);
        };
        let pin = self.storage.pool.pin(cursor.leaf)?;
        let removed = pin.with_page_mut(|page| {
            expect_kind(page, PageKind::BTreeLeaf)?;
            let entry = {
                let (bytes, _) = entry_at(page, LEAF_DIRECTORY, cursor.index)?;
                decode_entry_key(bytes)?
            };
            if compare_entry(&self.order, &entry, &probe)? != Ordering::Equal {
                return Ok(false);
            }
            remove_slot(page, LEAF_DIRECTORY, cursor.index)?;
            Ok::<_, InternalError>(true)
        })??;
        if removed {
            self.writes.mark(self.storage, cursor.leaf)?;
        }
        Ok(removed)
    }

    /// The entries of `range`, in the order `dir` asks for, materialised in a `Vec`.
    ///
    /// [`Direction::Forward`] starts at the first entry of the range, found from the root, and
    /// follows the `next` links of the leaves; [`Direction::Backward`] starts at the entry just
    /// past the range and follows the `prev` links
    /// (`tests::seek_between_and_full_both_directions`). Both read the entries of a leaf out of
    /// the page before they let its pin go, and hand back a `Vec` that owns them, so a write
    /// that follows the call does not change what the call answered
    /// (`tests::a_seek_answers_a_vec_a_later_insert_does_not_change`).
    ///
    /// A bound of fewer values than the key has columns is a prefix: [`KeyRange::Point`] of one
    /// value on a key of two columns names every entry whose first column is that value
    /// (`tests::a_two_column_key_seeks_by_prefix`).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] for a bound of more values than the key has columns; the errors of
    /// the buffer pool and of the decoding of an entry otherwise.
    pub(crate) fn seek(
        &self,
        range: &KeyRange,
        dir: Direction,
    ) -> Result<Vec<TreeEntry>, InternalError> {
        let (lo, hi) = match range {
            KeyRange::Point(values) => (
                Bound::Included(values.clone()),
                Bound::Included(values.clone()),
            ),
            KeyRange::Between(lo, hi) => (lo.clone(), hi.clone()),
            KeyRange::Full => (Bound::Unbounded, Bound::Unbounded),
        };
        for bound in [&lo, &hi] {
            if let Bound::Included(values) | Bound::Excluded(values) = bound {
                self.order.check_prefix(values)?;
            }
        }
        let (low, high) = (lower_probe(&lo), upper_probe(&hi));
        match dir {
            Direction::Forward => self.collect_forward(&low, &high),
            Direction::Backward => self.collect_backward(&low, &high),
        }
    }

    /// The entries from the first one at or past `low` up to the last one below `high`, in key
    /// order.
    fn collect_forward(&self, low: &Probe, high: &Probe) -> Result<Vec<TreeEntry>, InternalError> {
        let Some(cursor) = self.find_first(low)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let (mut leaf, mut from) = (cursor.leaf, cursor.index);
        loop {
            let (entries, _, next) = self.leaf_entries(leaf)?;
            for entry in entries.into_iter().skip(from) {
                if compare_entry(&self.order, &entry, high)? != Ordering::Less {
                    return Ok(out);
                }
                out.push(entry);
            }
            match next {
                Some(id) => (leaf, from) = (id, 0),
                None => return Ok(out),
            }
        }
    }

    /// The same entries as [`BTree::collect_forward`], read from the last one to the first: the
    /// walk starts at the entry `high` names, or past the last entry of the rightmost leaf when
    /// the tree holds nothing above the range, and follows the `prev` links.
    fn collect_backward(&self, low: &Probe, high: &Probe) -> Result<Vec<TreeEntry>, InternalError> {
        let (mut leaf, mut until) = match self.find_first(high)? {
            Some(cursor) => (cursor.leaf, cursor.index),
            None => (self.edge_leaf(false)?, usize::MAX),
        };
        let mut out = Vec::new();
        loop {
            let (entries, prev, _) = self.leaf_entries(leaf)?;
            let take = until.min(entries.len());
            for entry in entries.into_iter().take(take).rev() {
                if compare_entry(&self.order, &entry, low)? == Ordering::Less {
                    return Ok(out);
                }
                out.push(entry);
            }
            match prev {
                Some(id) => (leaf, until) = (id, usize::MAX),
                None => return Ok(out),
            }
        }
    }

    /// The place of the first entry of the tree at or past `probe`, `None` when the entries of
    /// the tree are below it.
    ///
    /// The descent lands on the leaf whose range holds `probe`; when that leaf has no entry at
    /// or past it — it may have been emptied by [`BTree::delete`] — the `next` links are
    /// followed, and the first entry of the next leaf that holds one is past `probe`, the
    /// separator of that leaf being above it.
    fn find_first(&self, probe: &Probe) -> Result<Option<Cursor>, InternalError> {
        let (leaf, _) = self.descend(probe)?;
        let mut node = leaf;
        loop {
            let pin = self.storage.pool.pin(node)?;
            let (index, len, next) = pin.with_page(|page| {
                expect_kind(page, PageKind::BTreeLeaf)?;
                Ok::<_, InternalError>((
                    leaf_position(page, &self.order, probe)?,
                    leaf_len(page),
                    leaf_next(page),
                ))
            })??;
            drop(pin);
            if index < len {
                return Ok(Some(Cursor { leaf: node, index }));
            }
            match next {
                Some(id) => node = id,
                None => return Ok(None),
            }
        }
    }

    /// The leaf `probe` belongs to, and the internal pages walked through to reach it, the root
    /// first.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a page that is neither a leaf nor an internal page, and
    /// for a descent of more than [`MAX_DEPTH`] internal pages; the errors of the buffer pool
    /// otherwise.
    fn descend(&self, probe: &Probe) -> Result<(PageId, Vec<PageId>), InternalError> {
        let mut path = Vec::new();
        let mut node = self.root;
        loop {
            let pin = self.storage.pool.pin(node)?;
            let child = pin.with_page(|page| match page.kind()? {
                PageKind::BTreeLeaf => Ok(None),
                PageKind::BTreeInternal => {
                    let index = internal_position(page, &self.order, probe)?;
                    internal_child(page, index).map(Some)
                }
                other => Err(InternalError::Corruption(format!(
                    "page {node} of the B+tree rooted at {} is a {other:?} page",
                    self.root
                ))),
            })??;
            drop(pin);
            let Some(child) = child else {
                return Ok((node, path));
            };
            path.push(node);
            if path.len() > MAX_DEPTH {
                return Err(InternalError::Corruption(format!(
                    "the descent of the B+tree rooted at {} has walked through {} internal \
                     pages, more than the {MAX_DEPTH} this build reads as a tree",
                    self.root,
                    path.len()
                )));
            }
            node = child;
        }
    }

    /// The leftmost leaf of the tree for `first`, the rightmost one otherwise.
    fn edge_leaf(&self, first: bool) -> Result<PageId, InternalError> {
        let probe = Probe {
            values: Vec::new(),
            payload: if first {
                PayloadBound::Lowest
            } else {
                PayloadBound::Highest
            },
        };
        let (leaf, _) = self.descend(&probe)?;
        Ok(leaf)
    }

    /// The entries of one leaf, decoded, with the leaves to its left and to its right.
    fn leaf_entries(&self, leaf: PageId) -> Result<LeafRead, InternalError> {
        let pin = self.storage.pool.pin(leaf)?;
        let read = pin.with_page(|page| {
            expect_kind(page, PageKind::BTreeLeaf)?;
            let mut entries = Vec::with_capacity(leaf_len(page));
            for index in 0..leaf_len(page) {
                let (bytes, _) = entry_at(page, LEAF_DIRECTORY, index)?;
                entries.push(decode_entry_key(bytes)?);
            }
            Ok::<_, InternalError>((entries, leaf_prev(page), leaf_next(page)))
        })??;
        Ok(read)
    }

    /// Puts an entry in a leaf, splitting the leaf when it has no room, and answers the split
    /// the parent has to record.
    fn put_in_leaf(
        &mut self,
        leaf: PageId,
        bytes: &[u8],
        probe: &Probe,
    ) -> Result<Option<Split>, InternalError> {
        let pin = self.storage.pool.pin(leaf)?;
        let put = pin.with_page_mut(|page| {
            expect_kind(page, PageKind::BTreeLeaf)?;
            place_in_leaf(page, &self.order, probe, bytes)
        })??;
        if put == Put::Inserted {
            self.writes.mark(self.storage, leaf)?;
            return Ok(None);
        }
        pin.with_page(|page| leaf_split_fits(page, &self.order, probe, bytes))??;
        drop(pin);

        let split = split_leaf(self.storage, leaf, &mut self.writes)?;
        let target = if goes_right(&self.order, &split.separator, probe)? {
            split.right
        } else {
            leaf
        };
        let pin = self.storage.pool.pin(target)?;
        let put = pin.with_page_mut(|page| place_in_leaf(page, &self.order, probe, bytes))??;
        if put == Put::Full {
            return Err(InternalError::Bug(format!(
                "leaf {target}, a half of the split of leaf {leaf}, has no room for an entry of \
                 {} bytes",
                bytes.len()
            )));
        }
        self.writes.mark(self.storage, target)?;
        Ok(Some(split))
    }

    /// Records a split in the parent of the page that split, splitting the parent in turn when
    /// it has no room, and answers the split the grandparent has to record.
    fn put_in_internal(
        &mut self,
        parent: PageId,
        split: &Split,
    ) -> Result<Option<Split>, InternalError> {
        let probe = probe_of(&split.separator)?;
        let pin = self.storage.pool.pin(parent)?;
        let put = pin.with_page_mut(|page| {
            expect_kind(page, PageKind::BTreeInternal)?;
            place_in_internal(page, &self.order, &probe, &split.separator, split.right)
        })??;
        if put == Put::Inserted {
            self.writes.mark(self.storage, parent)?;
            return Ok(None);
        }
        pin.with_page(|page| internal_split_fits(page, &self.order, &probe, &split.separator))??;
        drop(pin);

        let higher = split_internal(self.storage, parent, &mut self.writes)?;
        let target = if goes_right(&self.order, &higher.separator, &probe)? {
            higher.right
        } else {
            parent
        };
        let pin = self.storage.pool.pin(target)?;
        let put = pin.with_page_mut(|page| {
            place_in_internal(page, &self.order, &probe, &split.separator, split.right)
        })??;
        if put == Put::Full {
            return Err(InternalError::Bug(format!(
                "internal page {target}, a half of the split of page {parent}, has no room for a \
                 separator of {} bytes",
                split.separator.len()
            )));
        }
        self.writes.mark(self.storage, target)?;
        Ok(Some(higher))
    }

    /// Gives the tree a new root of one separator and two children, the old root on the left,
    /// and moves [`BTree::root`] to it.
    fn grow_root(&mut self, split: &Split) -> Result<(), InternalError> {
        let root = allocate(self.storage)?;
        let old = self.root;
        let pin = self.storage.pool.pin(root)?;
        pin.with_page_mut(|page| {
            init_internal(page, old);
            check_entry_size(root, split.separator.len(), CHILD_BYTES)?;
            if put_entry(
                page,
                INTERNAL_DIRECTORY,
                0,
                &split.separator,
                &split.right.0.to_le_bytes(),
            )? == Put::Full
            {
                return Err(InternalError::Bug(format!(
                    "the fresh root {root} has no room for the separator of the split of {old}"
                )));
            }
            Ok(())
        })??;
        self.writes.mark(self.storage, root)?;
        drop(pin);
        self.root = root;
        Ok(())
    }
}

/// The probe that names the entry a separator was copied from: its values, at its payload.
fn probe_of(separator: &[u8]) -> Result<Probe, InternalError> {
    let (values, payload) = decode_entry_key(separator)?;
    Ok(Probe {
        values,
        payload: PayloadBound::At(payload),
    })
}

/// The probe a forward walk starts at, that is the first entry the bound lets in:
/// [`Bound::Included`] stops before the entries of the value, [`Bound::Excluded`] past them,
/// and [`Bound::Unbounded`] before the first entry of the tree, an empty prefix comparing
/// `Equal` to the key it is held against (the `KeyRange::Full` of
/// `tests::seek_between_and_full_both_directions`).
fn lower_probe(bound: &Bound<Vec<Value>>) -> Probe {
    match bound {
        Bound::Unbounded => Probe {
            values: Vec::new(),
            payload: PayloadBound::Lowest,
        },
        Bound::Included(values) => Probe {
            values: values.clone(),
            payload: PayloadBound::Lowest,
        },
        Bound::Excluded(values) => Probe {
            values: values.clone(),
            payload: PayloadBound::Highest,
        },
    }
}

/// The probe a walk stops at, that is the first entry the bound leaves out.
fn upper_probe(bound: &Bound<Vec<Value>>) -> Probe {
    match bound {
        Bound::Unbounded => Probe {
            values: Vec::new(),
            payload: PayloadBound::Highest,
        },
        Bound::Included(values) => Probe {
            values: values.clone(),
            payload: PayloadBound::Highest,
        },
        Bound::Excluded(values) => Probe {
            values: values.clone(),
            payload: PayloadBound::Lowest,
        },
    }
}

#[cfg(test)]
mod tests;
