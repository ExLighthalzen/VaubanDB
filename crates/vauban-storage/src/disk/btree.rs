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
mod tests {
    use vauban_types::SqlType;

    use super::super::temp::TempDir;
    use super::super::{DiskOptions, DiskStorage};
    use super::*;

    /// An empty instance in a temporary directory, with the guard that removes it.
    fn instance(label: &str) -> (TempDir, DiskStorage) {
        let dir = TempDir::created(label);
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        (dir, storage)
    }

    /// An empty leaf page, not tied to an instance.
    fn leaf(id: u64) -> Page {
        let mut page = Page::empty(PageKind::Free, PageId(id));
        init_leaf(&mut page);
        page
    }

    /// The entries of a leaf, copied out for a comparison.
    fn entries_of(page: &Page) -> Vec<Entry> {
        leaf_iter(page)
            .expect("the leaf reads back")
            .map(|(key, payload)| (key.to_vec(), payload.to_vec()))
            .collect()
    }

    /// The keys of a leaf, in the order its directory holds them.
    fn keys_of(page: &Page) -> Vec<Vec<u8>> {
        entries_of(page).into_iter().map(|(key, _)| key).collect()
    }

    /// The keys of an internal page, in the order its directory holds them.
    fn internal_keys(page: &Page) -> Vec<Vec<u8>> {
        (0..internal_len(page))
            .map(|index| {
                internal_key(page, index)
                    .expect("the internal page reads back")
                    .to_vec()
            })
            .collect()
    }

    /// The children of an internal page, `P0` first.
    fn internal_children(page: &Page) -> Vec<PageId> {
        (0..=internal_len(page))
            .map(|index| internal_child(page, index).expect("the child reads back"))
            .collect()
    }

    /// A copy of the page `id` of the instance, read through the pool.
    fn page_of(storage: &DiskStorage, id: PageId) -> Page {
        let pin = storage.pool.pin(id).expect("pin the page");
        pin.with_page(Page::clone).expect("read the cached page")
    }

    /// A key of a fixed width, so that the byte order of the keys is their numeric order.
    fn key(number: usize) -> Vec<u8> {
        format!("k{number:04}").into_bytes()
    }

    /// A payload of `width` bytes carrying the number of its entry.
    fn payload(number: usize, width: usize) -> Vec<u8> {
        let mut bytes = format!("p{number:04}").into_bytes();
        bytes.resize(width, b'.');
        bytes
    }

    /// An allocated leaf of the instance, filled until it answers [`Put::Full`]; answers its
    /// identifier and the number of entries it took.
    fn filled_leaf(storage: &DiskStorage) -> (PageId, usize) {
        let id = allocate(storage).expect("allocate a leaf");
        let pin = storage.pool.pin(id).expect("pin the leaf");
        let count = pin
            .with_page_mut(|page| {
                init_leaf(page);
                let mut count = 0;
                while leaf_put(page, &key(count), &payload(count, 100))
                    .expect("put an entry in the leaf")
                    == Put::Inserted
                {
                    count += 1;
                }
                count
            })
            .expect("the test holds the only pin");
        PageWrites::unlogged()
            .mark(storage, id)
            .expect("mark the leaf dirty");
        drop(pin);
        (id, count)
    }

    #[test]
    fn leaf_insert_ordered_slots() {
        let mut page = leaf(1);
        assert_eq!(
            page.kind().expect("kind of a fresh leaf"),
            PageKind::BTreeLeaf
        );
        assert_eq!(leaf_len(&page), 0);
        assert_eq!(page.lower(), LEAF_DIRECTORY as u16);
        assert_eq!(leaf_prev(&page), None);
        assert_eq!(leaf_next(&page), None);

        for (key, payload) in [(b"a", b"1"), (b"c", b"3"), (b"b", b"2")] {
            assert_eq!(
                leaf_put(&mut page, key, payload).expect("put an entry"),
                Put::Inserted
            );
        }

        assert_eq!(leaf_len(&page), 3);
        assert_eq!(
            keys_of(&page),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(
            entries_of(&page),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ],
            "the payload follows its key, so a slot that moved took its entry with it"
        );
        assert_eq!(
            leaf_entry(&page, 1).expect("the middle entry"),
            (&b"b"[..], &b"2"[..])
        );
        assert_eq!(
            page.lower(),
            (LEAF_DIRECTORY + 3 * SLOT_SIZE) as u16,
            "three slots of four bytes, after the header and the two links"
        );
        // The entry of `b` was written after that of `c`, so it sits below it in the page: the
        // order of the directory is not the order of the bytes.
        let (offset_b, _) = checked_slot(&page, LEAF_DIRECTORY, 1).expect("the slot of b");
        let (offset_c, _) = checked_slot(&page, LEAF_DIRECTORY, 2).expect("the slot of c");
        assert!(offset_b < offset_c, "{offset_b} < {offset_c}");
    }

    #[test]
    fn duplicate_keys_keep_insertion_order() {
        let mut page = leaf(2);
        for payload in [b"first", b"secnd", b"third"] {
            leaf_put(&mut page, b"dup", payload).expect("put an entry");
        }
        leaf_put(&mut page, b"after", b"zzzzz").expect("put an entry");
        assert_eq!(
            entries_of(&page),
            vec![
                (b"after".to_vec(), b"zzzzz".to_vec()),
                (b"dup".to_vec(), b"first".to_vec()),
                (b"dup".to_vec(), b"secnd".to_vec()),
                (b"dup".to_vec(), b"third".to_vec()),
            ]
        );
    }

    #[test]
    fn entry_too_large_is_bug() {
        let mut page = leaf(3);
        let oversized = vec![b'x'; MAX_ENTRY_BYTES + 1 - 8];
        let err = leaf_put(&mut page, &[b'k'; 8], &oversized)
            .expect_err("eight key bytes and that payload are one byte too many");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("8064"), "{err}");
        assert_eq!(MAX_ENTRY_BYTES, 8064);
        assert_eq!(leaf_len(&page), 0, "the page was left as it was");

        // One byte less goes on the empty page: the limit is the largest entry that fits, not
        // the smallest one that is refused.
        assert_eq!(
            leaf_put(&mut page, &[b'k'; 8], &oversized[..oversized.len() - 1])
                .expect("an entry of exactly MAX_ENTRY_BYTES bytes"),
            Put::Inserted
        );
        assert_eq!(leaf_len(&page), 1);
        assert_eq!(
            leaf_entry(&page, 0).expect("the entry reads back").1.len(),
            MAX_ENTRY_BYTES - 8
        );

        // The same limit guards an internal page, whose payload is the child pointer.
        let mut internal = Page::empty(PageKind::Free, PageId(4));
        init_internal(&mut internal, PageId(1));
        let err = internal_put(&mut internal, &vec![b'k'; MAX_ENTRY_BYTES - 7], PageId(2))
            .expect_err("that key and the eight bytes of the child are one byte too many");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert_eq!(internal_len(&internal), 0);
        assert_eq!(
            internal_put(&mut internal, &vec![b'k'; MAX_ENTRY_BYTES - 8], PageId(2))
                .expect("one byte less fits"),
            Put::Inserted
        );
    }

    #[test]
    fn leaf_put_answers_full_without_changing_the_page() {
        let mut page = leaf(5);
        let mut count = 0;
        while leaf_put(&mut page, &key(count), &payload(count, 100)).expect("put an entry")
            == Put::Inserted
        {
            count += 1;
        }
        assert!(
            count > 1,
            "the page took {count} entries before it was full"
        );
        let before = entries_of(&page);
        assert_eq!(before.len(), count);
        assert_eq!(
            leaf_put(&mut page, &key(count), &payload(count, 100)).expect("the page is full"),
            Put::Full
        );
        assert_eq!(entries_of(&page), before, "a refused put changes nothing");
        assert!(
            free_space(&page, LEAF_DIRECTORY).expect("free space")
                < SLOT_SIZE + ENTRY_OVERHEAD + 5 + 100,
            "the page answered Full because the entry does not fit, not because of a counter"
        );
    }

    #[test]
    fn split_leaf_separator() {
        let (_dir, storage) = instance("split-leaf");
        let (left, count) = filled_leaf(&storage);
        assert!(count >= 2, "{count} entries to share");
        let before = entries_of(&page_of(&storage, left));

        let split =
            split_leaf(&storage, left, &mut PageWrites::unlogged()).expect("split the leaf");
        let left_page = page_of(&storage, left);
        let right_page = page_of(&storage, split.right);
        let left_entries = entries_of(&left_page);
        let right_entries = entries_of(&right_page);

        assert_ne!(split.right, left);
        assert_eq!(
            right_page.kind().expect("kind of the right page"),
            PageKind::BTreeLeaf
        );
        // Nothing lost, nothing duplicated, and the two halves are still in order.
        assert_eq!(
            [left_entries.clone(), right_entries.clone()].concat(),
            before
        );
        assert!(!left_entries.is_empty() && !right_entries.is_empty());
        assert!(left_entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(right_entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(
            left_entries.last().expect("the left half holds an entry").0
                < right_entries
                    .first()
                    .expect("the right half holds an entry")
                    .0,
            "the halves are disjoint"
        );
        // ~50/50: neither half took less than a third of the entries.
        assert!(
            left_entries.len() * 3 >= count && right_entries.len() * 3 >= count,
            "{} and {} out of {count}",
            left_entries.len(),
            right_entries.len()
        );

        assert_eq!(
            split.separator, right_entries[0].0,
            "the separator is the first key of the right leaf"
        );
        assert!(
            right_entries.iter().any(|(key, _)| *key == split.separator),
            "a leaf split copies its separator, it does not move it"
        );
        assert_eq!(leaf_next(&left_page), Some(split.right));
        assert_eq!(leaf_prev(&right_page), Some(left));
        assert_eq!(leaf_prev(&left_page), None);
        assert_eq!(leaf_next(&right_page), None);
    }

    #[test]
    fn split_leaf_relinks_the_old_next_sibling() {
        let (_dir, storage) = instance("split-relink");
        let (first, _) = filled_leaf(&storage);
        let far = split_leaf(&storage, first, &mut PageWrites::unlogged())
            .expect("a first split")
            .right;
        // `first` is half empty now; filling it again and splitting it puts a page between
        // `first` and `far`. The keys are above those already there, so the new entries land
        // on the right half.
        let pin = storage.pool.pin(first).expect("pin the left leaf");
        pin.with_page_mut(|page| {
            let mut count = 0;
            while leaf_put(page, &key(10_000 + count), &payload(count, 100)).expect("put an entry")
                == Put::Inserted
            {
                count += 1;
            }
        })
        .expect("the test holds the only pin");
        PageWrites::unlogged()
            .mark(&storage, first)
            .expect("mark the leaf dirty");
        drop(pin);

        let middle = split_leaf(&storage, first, &mut PageWrites::unlogged())
            .expect("a second split")
            .right;
        assert_ne!(middle, far);
        assert_eq!(leaf_next(&page_of(&storage, first)), Some(middle));
        assert_eq!(leaf_prev(&page_of(&storage, middle)), Some(first));
        assert_eq!(leaf_next(&page_of(&storage, middle)), Some(far));
        assert_eq!(
            leaf_prev(&page_of(&storage, far)),
            Some(middle),
            "the leaf that was to the right points at the page put in front of it"
        );
    }

    #[test]
    fn split_leaf_refuses_a_page_of_one_entry() {
        let (_dir, storage) = instance("split-too-thin");
        let id = allocate(&storage).expect("allocate a leaf");
        let pin = storage.pool.pin(id).expect("pin the leaf");
        pin.with_page_mut(|page| {
            init_leaf(page);
            leaf_put(page, b"only", b"one").expect("put the single entry");
        })
        .expect("the test holds the only pin");
        drop(pin);

        let err = split_leaf(&storage, id, &mut PageWrites::unlogged())
            .expect_err("one entry cannot be shared");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("1 entries"), "{err}");
        assert_eq!(
            storage.control().expect("the control block").next_page_id,
            PageId(1),
            "the refusal came before the allocation"
        );
    }

    #[test]
    fn internal_put_and_split() {
        let (_dir, storage) = instance("split-internal");
        let left = allocate(&storage).expect("allocate an internal page");
        let pin = storage.pool.pin(left).expect("pin the internal page");
        let keys = pin
            .with_page_mut(|page| {
                init_internal(page, PageId(1_000));
                let mut keys = 0;
                // The children are numbered 1001, 1002, … so that a pointer names its key.
                while internal_put(page, &key(keys + 1), PageId(1_001 + keys as u64))
                    .expect("put a separator")
                    == Put::Inserted
                {
                    keys += 1;
                }
                keys
            })
            .expect("the test holds the only pin");
        PageWrites::unlogged()
            .mark(&storage, left)
            .expect("mark the page dirty");
        drop(pin);

        let before = page_of(&storage, left);
        assert!(keys >= 3, "{keys} keys to share");
        assert_eq!(internal_len(&before), keys);
        assert_eq!(
            internal_children(&before).len(),
            keys + 1,
            "one pointer more than the page has keys"
        );
        assert_eq!(internal_child(&before, 0).expect("P0"), PageId(1_000));
        assert_eq!(internal_child(&before, 1).expect("P1"), PageId(1_001));
        let keys_before = internal_keys(&before);
        let children_before = internal_children(&before);

        let split = split_internal(&storage, left, &mut PageWrites::unlogged())
            .expect("split the internal page");
        let left_page = page_of(&storage, left);
        let right_page = page_of(&storage, split.right);
        assert_eq!(
            right_page.kind().expect("kind of the right page"),
            PageKind::BTreeInternal
        );

        let left_keys = internal_keys(&left_page);
        let right_keys = internal_keys(&right_page);
        let left_children = internal_children(&left_page);
        let right_children = internal_children(&right_page);
        // Both pages are valid: one pointer more than they have keys, and keys in order.
        assert_eq!(left_children.len(), left_keys.len() + 1);
        assert_eq!(right_children.len(), right_keys.len() + 1);
        assert!(!left_keys.is_empty() && !right_keys.is_empty());
        assert!(left_keys.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(right_keys.windows(2).all(|pair| pair[0] < pair[1]));

        // The promoted key left both pages, and the pointers kept their order.
        assert_eq!(
            [
                left_keys.clone(),
                vec![split.separator.clone()],
                right_keys.clone()
            ]
            .concat(),
            keys_before
        );
        assert_eq!([left_children, right_children].concat(), children_before);
        assert!(
            left_keys.last().expect("a key on the left") < &split.separator,
            "the keys of the left page are below the separator"
        );
        assert!(
            right_keys.first().expect("a key on the right") > &split.separator,
            "the keys of the right page are above the separator that went up"
        );
        assert!(!right_keys.contains(&split.separator));
        assert!(!left_keys.contains(&split.separator));
    }

    #[test]
    fn split_internal_refuses_a_page_of_two_keys() {
        let (_dir, storage) = instance("internal-too-thin");
        let id = allocate(&storage).expect("allocate an internal page");
        let pin = storage.pool.pin(id).expect("pin the internal page");
        pin.with_page_mut(|page| {
            init_internal(page, PageId(7));
            internal_put(page, b"m", PageId(8)).expect("first separator");
            internal_put(page, b"s", PageId(9)).expect("second separator");
        })
        .expect("the test holds the only pin");
        drop(pin);

        let err = split_internal(&storage, id, &mut PageWrites::unlogged())
            .expect_err("two keys cannot be shared");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("2 keys"), "{err}");
    }

    #[test]
    fn a_held_write_marks_its_pages_and_hands_them_back() {
        let (_dir, storage) = instance("btree-held-writes");
        let mut tree = int_tree(&storage);
        let root = tree.root();
        storage.pool.flush_all().expect("flush the fresh root");
        assert!(
            tree.take_held_pages().is_empty(),
            "the creation of the tree, left unlogged, held a page"
        );

        tree.hold_writes(Lsn(9));
        tree.insert(&[Value::I32(1)], &rid(1))
            .expect("insert one entry");
        assert_eq!(
            tree.take_held_pages(),
            BTreeSet::from([root]),
            "the leaf the entry landed on is the page handed back"
        );
        assert!(
            tree.take_held_pages().is_empty(),
            "a page was handed out twice"
        );
        assert_eq!(page_of(&storage, root).lsn(), Lsn(9));

        // The flag keeps the leaf in the pool: `flush_all` skips it and `data` still holds the
        // empty root written above.
        storage
            .pool
            .flush_all()
            .expect("flush while the leaf is held");
        assert_eq!(
            storage
                .data
                .read_page(root)
                .expect("read the root from data")
                .slot_count(),
            0
        );

        // The flag lifted, the write-ahead rule is what is left: LSN 9 is past the durable LSN
        // of a journal this test leaves empty.
        storage
            .pool
            .set_in_progress(root, false)
            .expect("lift the flag");
        let err = storage
            .pool
            .flush_all()
            .expect_err("the leaf carries an LSN the journal has not made durable");
        assert!(err.to_string().contains("LSN 9"), "{err}");
    }

    #[test]
    fn a_split_reaches_the_data_file_when_the_pool_is_flushed() {
        let dir = TempDir::created("split-durable");
        let (left, right, separator, left_keys, right_keys) = {
            let storage =
                DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
            let (left, _) = filled_leaf(&storage);
            let split =
                split_leaf(&storage, left, &mut PageWrites::unlogged()).expect("split the leaf");
            let left_keys = keys_of(&page_of(&storage, left));
            let right_keys = keys_of(&page_of(&storage, split.right));
            storage.pool.flush_all().expect("flush the pool");
            (left, split.right, split.separator, left_keys, right_keys)
        };

        let reopened =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen the instance");
        let left_page = reopened.data.read_page(left).expect("read the left leaf");
        let right_page = reopened.data.read_page(right).expect("read the right leaf");
        assert_eq!(keys_of(&left_page), left_keys);
        assert_eq!(keys_of(&right_page), right_keys);
        assert_eq!(right_keys[0], separator);
        assert_eq!(leaf_next(&left_page), Some(right));
        assert_eq!(leaf_prev(&right_page), Some(left));
        assert_eq!(left_page.lsn(), Lsn(0), "a split writes no journal record");
        assert_eq!(right_page.lsn(), Lsn(0));
    }

    #[test]
    fn a_page_is_read_as_the_kind_it_carries() {
        let mut page = leaf(11);
        let err = internal_key(&page, 0).expect_err("a leaf is not an internal page");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("BTreeLeaf"), "{err}");

        init_internal(&mut page, PageId(12));
        let err = leaf_put(&mut page, b"k", b"v").expect_err("an internal page is not a leaf");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("BTreeInternal"), "{err}");
        assert_eq!(leftmost_child(&page), PageId(12));
        assert_eq!(internal_len(&page), 0);
    }

    #[test]
    fn a_slot_pointing_outside_the_page_is_corruption() {
        let mut page = leaf(13);
        leaf_put(&mut page, b"k", b"v").expect("put an entry");
        // The slot is moved onto the directory itself, where no entry sits.
        write_u16(&mut page, LEAF_DIRECTORY, LEAF_DIRECTORY).expect("write the slot");
        let err = leaf_entry(&page, 0).expect_err("the slot is inside the directory");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");

        let mut page = leaf(14);
        leaf_put(&mut page, b"k", b"v").expect("put an entry");
        let (offset, _) = checked_slot(&page, LEAF_DIRECTORY, 0).expect("the slot");
        // The declared key length now runs past the end of the entry.
        write_u16(&mut page, offset, 500).expect("write the key length");
        let err = leaf_entry(&page, 0).expect_err("the key does not fit its slot");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("500"), "{err}");

        // A slot past the directory is a Bug, not a corruption of the page.
        let err = leaf_entry(&page, 1).expect_err("the page holds one entry");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    }

    /// Entries built from their keys, each with a payload naming its rank.
    fn entries_from(keys: &[&str]) -> Vec<Entry> {
        keys.iter()
            .enumerate()
            .map(|(rank, key)| (key.as_bytes().to_vec(), format!("p{rank}").into_bytes()))
            .collect()
    }

    /// A leaf of the instance filled with `runs` entries of one key at a time; answers its
    /// identifier and the number of entries it took.
    fn leaf_of_runs(storage: &DiskStorage, runs: usize, width: usize) -> (PageId, usize) {
        let id = allocate(storage).expect("allocate a leaf");
        let pin = storage.pool.pin(id).expect("pin the leaf");
        let count = pin
            .with_page_mut(|page| {
                init_leaf(page);
                let mut count = 0;
                while leaf_put(page, &key(count / runs), &payload(count, width))
                    .expect("put an entry in the leaf")
                    == Put::Inserted
                {
                    count += 1;
                }
                count
            })
            .expect("the test holds the only pin");
        PageWrites::unlogged()
            .mark(storage, id)
            .expect("mark the leaf dirty");
        drop(pin);
        (id, count)
    }

    /// A small deterministic generator, so that the drawn pages are the same at each run.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 11) % bound as u64) as usize
        }
    }

    #[test]
    fn a_cut_lands_between_two_different_keys() {
        // The cut of a leaf moves to the nearest index where the key changes.
        let run_then_one = entries_from(&["a", "a", "a", "b"]);
        assert_eq!(cut_between_keys(&run_then_one, 2), Some(3));
        let one_then_run = entries_from(&["a", "b", "b", "b"]);
        assert_eq!(cut_between_keys(&one_then_run, 2), Some(1));
        // A cut already between two different keys stays where the byte sizes put it.
        let two_runs = entries_from(&["a", "a", "b", "b"]);
        assert_eq!(cut_between_keys(&two_runs, 2), Some(2));
        // Two candidates at the same distance: the lower index wins.
        let tie = entries_from(&["a", "b", "b", "c"]);
        assert_eq!(cut_between_keys(&tie, 2), Some(1));
        // One key on the page: no cut, and split_leaf turns that into a Bug.
        assert_eq!(
            cut_between_keys(&entries_from(&["a", "a", "a", "a"]), 2),
            None
        );

        // The promoted key of an internal page differs from both of its neighbours.
        assert_eq!(
            key_to_promote(&entries_from(&["a", "b", "c", "d"]), 2),
            Some(2)
        );
        assert_eq!(
            key_to_promote(&entries_from(&["a", "a", "b", "c", "c"]), 2),
            Some(2)
        );
        assert_eq!(
            key_to_promote(&entries_from(&["a", "b", "b", "c", "d"]), 2),
            Some(3)
        );
        // Runs on both sides of the cut: no key of the page can go up alone.
        assert_eq!(key_to_promote(&two_runs, 2), None);
    }

    #[test]
    fn split_leaf_keeps_a_run_of_equal_keys_on_one_side() {
        let (_dir, storage) = instance("split-run");
        let (left, count) = leaf_of_runs(&storage, 8, 100);
        let before = entries_of(&page_of(&storage, left));
        assert_eq!(before.len(), count);
        let balanced = balance_point(&entry_weights(&before));
        assert_eq!(
            before[balanced - 1].0,
            before[balanced].0,
            "the cut the byte sizes pick falls inside a run of equal keys"
        );

        let split =
            split_leaf(&storage, left, &mut PageWrites::unlogged()).expect("split the leaf");
        let left_entries = entries_of(&page_of(&storage, left));
        let right_entries = entries_of(&page_of(&storage, split.right));
        assert_eq!(
            [left_entries.clone(), right_entries.clone()].concat(),
            before,
            "nothing lost, nothing duplicated"
        );
        assert_eq!(split.separator, right_entries[0].0);
        assert!(
            left_entries.last().expect("the left leaf holds an entry").0 < split.separator,
            "the highest key of the left leaf is below the separator, not equal to it"
        );
        assert_eq!(
            left_entries
                .iter()
                .filter(|(key, _)| *key == split.separator)
                .count(),
            0,
            "the run of the separator went to the right leaf as a block"
        );
        assert_eq!(
            right_entries
                .iter()
                .filter(|(key, _)| *key == split.separator)
                .count(),
            8,
            "the eight entries of that key are still together"
        );
        // 72 entries, runs of 8: the cut moved from 36 to the run boundary at 32.
        assert_eq!(
            (count, left_entries.len(), right_entries.len()),
            (72, 32, 40)
        );
    }

    #[test]
    fn split_leaf_refuses_a_run_it_cannot_cut() {
        let (_dir, storage) = instance("split-one-key");
        let (left, count) = leaf_of_runs(&storage, usize::MAX, 100);
        let before = entries_of(&page_of(&storage, left));
        assert_eq!(before.len(), count);
        assert!(before.windows(2).all(|pair| pair[0].0 == pair[1].0));

        let err = split_leaf(&storage, left, &mut PageWrites::unlogged())
            .expect_err("a page of one key has no cut");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("carry one key"), "{err}");
        assert_eq!(
            entries_of(&page_of(&storage, left)),
            before,
            "the refused split left the leaf as it was"
        );
        assert_eq!(
            storage.control().expect("the control block").next_page_id,
            PageId(1),
            "the refusal came before the allocation, so no page was orphaned"
        );
    }

    #[test]
    fn split_internal_promotes_a_key_out_of_a_run() {
        let (_dir, storage) = instance("internal-run");
        let id = allocate(&storage).expect("allocate an internal page");
        let pin = storage.pool.pin(id).expect("pin the internal page");
        pin.with_page_mut(|page| {
            init_internal(page, PageId(100));
            // A run of four `m` around the middle, one key on the left, two on the right.
            for (rank, key) in ["a", "m", "m", "m", "m", "s", "t"].iter().enumerate() {
                assert_eq!(
                    internal_put(page, key.as_bytes(), PageId(101 + rank as u64))
                        .expect("put a separator"),
                    Put::Inserted
                );
            }
        })
        .expect("the test holds the only pin");
        PageWrites::unlogged()
            .mark(&storage, id)
            .expect("mark the page dirty");
        drop(pin);

        let before = page_of(&storage, id);
        let keys_before = internal_keys(&before);
        let children_before = internal_children(&before);
        let balanced = balance_point(&entry_weights(
            &owned_entries(&before, INTERNAL_DIRECTORY).expect("the entries read back"),
        ));
        assert_eq!(
            keys_before[balanced],
            b"m".to_vec(),
            "the middle is in the run"
        );

        let split = split_internal(&storage, id, &mut PageWrites::unlogged())
            .expect("split the internal page");
        let left_page = page_of(&storage, id);
        let right_page = page_of(&storage, split.right);
        let left_keys = internal_keys(&left_page);
        let right_keys = internal_keys(&right_page);
        assert_eq!(split.separator, b"s".to_vec());
        assert_eq!(
            left_keys,
            [
                b"a".to_vec(),
                b"m".to_vec(),
                b"m".to_vec(),
                b"m".to_vec(),
                b"m".to_vec()
            ]
        );
        assert_eq!(right_keys, [b"t".to_vec()]);
        assert_eq!(
            [left_keys, vec![split.separator.clone()], right_keys].concat(),
            keys_before
        );
        assert_eq!(
            [
                internal_children(&left_page),
                internal_children(&right_page)
            ]
            .concat(),
            children_before
        );
        assert_eq!(
            internal_len(&left_page) + 1,
            internal_children(&left_page).len()
        );
        assert_eq!(
            internal_len(&right_page) + 1,
            internal_children(&right_page).len()
        );
    }

    #[test]
    fn split_internal_refuses_a_page_it_cannot_promote_out_of() {
        let (_dir, storage) = instance("internal-runs");
        let id = allocate(&storage).expect("allocate an internal page");
        let pin = storage.pool.pin(id).expect("pin the internal page");
        pin.with_page_mut(|page| {
            init_internal(page, PageId(200));
            for (rank, key) in ["a", "a", "b", "b"].iter().enumerate() {
                internal_put(page, key.as_bytes(), PageId(201 + rank as u64))
                    .expect("put a separator");
            }
        })
        .expect("the test holds the only pin");
        PageWrites::unlogged()
            .mark(&storage, id)
            .expect("mark the page dirty");
        drop(pin);

        let err = split_internal(&storage, id, &mut PageWrites::unlogged())
            .expect_err("each key equals a neighbour");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("4 keys"), "{err}");
        assert_eq!(internal_keys(&page_of(&storage, id)).len(), 4);
    }

    #[test]
    fn equal_keys_stay_on_one_side_of_a_split() {
        let (_dir, storage) = instance("split-drawn");
        let mut rng = Lcg(0x5eed);
        let (mut straddled, mut refused) = (0, 0);
        for round in 0..200 {
            let id = allocate(&storage).expect("allocate a leaf");
            let pin = storage.pool.pin(id).expect("pin the leaf");
            let put = pin
                .with_page_mut(|page| {
                    init_leaf(page);
                    let mut put: Vec<Entry> = Vec::new();
                    loop {
                        // Keys of at most three letters out of four: runs are frequent.
                        let key: Vec<u8> = (0..rng.below(4))
                            .map(|_| b'a' + rng.below(4) as u8)
                            .collect();
                        let payload = vec![(put.len() % 251) as u8; rng.below(300)];
                        if leaf_put(page, &key, &payload).expect("put an entry") != Put::Inserted {
                            break;
                        }
                        put.push((key, payload));
                    }
                    put
                })
                .expect("the test holds the only pin");
            PageWrites::unlogged()
                .mark(&storage, id)
                .expect("mark the leaf dirty");
            drop(pin);

            let before = entries_of(&page_of(&storage, id));
            let mut sorted = put.clone();
            sorted.sort_by(|left, right| left.0.cmp(&right.0));
            assert_eq!(
                before, sorted,
                "round {round}: the directory is in key order"
            );
            if before.len() < 2 {
                continue;
            }
            let balanced = balance_point(&entry_weights(&before));
            if before[balanced - 1].0 == before[balanced].0 {
                straddled += 1;
            }
            match split_leaf(&storage, id, &mut PageWrites::unlogged()) {
                Ok(split) => {
                    let left = entries_of(&page_of(&storage, id));
                    let right = entries_of(&page_of(&storage, split.right));
                    assert_eq!(
                        [left.clone(), right.clone()].concat(),
                        before,
                        "round {round}"
                    );
                    assert_eq!(split.separator, right[0].0, "round {round}");
                    assert!(
                        left.last().expect("an entry on the left").0 < split.separator,
                        "round {round}: a key of the left leaf reaches the separator"
                    );
                }
                Err(err) => {
                    refused += 1;
                    assert!(
                        matches!(err, InternalError::Bug(_)),
                        "round {round}: {err:?}"
                    );
                    assert!(
                        before.windows(2).all(|pair| pair[0].0 == pair[1].0),
                        "round {round}: a split was refused on a page of several keys"
                    );
                }
            }
        }
        // The axis is exercised: the byte-balanced cut fell inside a run in these rounds.
        assert!(
            straddled >= 10,
            "{straddled} rounds out of 200 straddle a run"
        );
        assert_eq!(refused, 0, "{refused} pages of a single key were drawn");
    }

    #[test]
    fn a_slot_count_past_the_page_is_corruption() {
        let mut page = leaf(15);
        leaf_put(&mut page, b"k", b"v").expect("put an entry");
        // 48 + 4 × 4 000 = 16 048 bytes of directory asked for in a page of 8 192.
        page.set_slot_count(4_000);
        for err in [
            leaf_entry(&page, 3_000).expect_err("the directory runs past the page"),
            leaf_iter(&page)
                .err()
                .expect("the iterator reads the directory"),
            leaf_put(&mut page.clone(), b"z", b"v").expect_err("a put reads the free space"),
        ] {
            assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
            assert!(err.to_string().contains("4000"), "{err}");
        }

        // The largest directory a leaf holds: (8 192 − 48) / 4 = 2 036 slots, ending on the
        // last byte of the page.
        page.set_slot_count(2_036);
        assert_eq!(
            directory_end(&page, LEAF_DIRECTORY).expect("2 036 slots fit"),
            PAGE_SIZE
        );
        page.set_slot_count(2_037);
        let err = directory_end(&page, LEAF_DIRECTORY).expect_err("one slot too many");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    }

    #[test]
    fn balance_point_keeps_both_halves() {
        assert_eq!(balance_point(&[1, 1]), 1);
        assert_eq!(balance_point(&[1, 1, 1]), 2);
        assert_eq!(balance_point(&[1, 1, 1, 1]), 2);
        // A first entry heavier than the others still stays alone on the left.
        assert_eq!(balance_point(&[100, 1, 1]), 1);
        // A last entry that carries the weight leaves the ones before it on the left.
        assert_eq!(balance_point(&[1, 1, 100]), 2);
    }

    // ---------------------------------------------------------------------------------------
    // The tree over those pages
    // ---------------------------------------------------------------------------------------

    /// The columns of a table of one nullable `int` column.
    fn int_types() -> Vec<TypeInfo> {
        vec![TypeInfo::new(SqlType::Int, true)]
    }

    /// An ascending key on the column 0.
    fn ascending() -> Vec<KeyColumn> {
        vec![KeyColumn {
            column: 0,
            descending: false,
        }]
    }

    /// An empty tree on one ascending `int` column of the instance.
    fn int_tree(storage: &DiskStorage) -> BTree<'_> {
        BTree::create(storage, &ascending(), &int_types()).expect("create a tree")
    }

    /// The payload of these tests: the eight bytes of a [`crate::RowId`], big-endian so that
    /// their byte order is the order of the numbers.
    fn rid(number: u64) -> Vec<u8> {
        number.to_be_bytes().to_vec()
    }

    /// The `int` of each entry, in the order the seek answered them.
    fn ints(entries: &[TreeEntry]) -> Vec<i32> {
        entries
            .iter()
            .map(|(values, _)| match values.as_slice() {
                [Value::I32(number)] => *number,
                other => panic!("an entry of the tree carries {other:?}"),
            })
            .collect()
    }

    /// The payloads of the entries, in the order the seek answered them.
    fn payloads(entries: &[TreeEntry]) -> Vec<Vec<u8>> {
        entries.iter().map(|(_, payload)| payload.clone()).collect()
    }

    /// The whole tree, in key order.
    fn all(tree: &BTree<'_>) -> Vec<TreeEntry> {
        tree.seek(&KeyRange::Full, Direction::Forward)
            .expect("seek the whole tree")
    }

    /// The kind of the page `id` of the instance.
    fn kind_of(storage: &DiskStorage, id: PageId) -> PageKind {
        page_of(storage, id)
            .kind()
            .expect("read the kind of the page")
    }

    #[test]
    fn insert_seek_point_int() {
        let (_dir, storage) = instance("btree-point");
        let mut tree = int_tree(&storage);
        for (number, row) in [(1, 10u64), (3, 30), (2, 20)] {
            tree.insert(&[Value::I32(number)], &rid(row))
                .expect("insert an entry");
        }

        let found = tree
            .seek(&KeyRange::Point(vec![Value::I32(2)]), Direction::Forward)
            .expect("seek the point 2");
        assert_eq!(ints(&found), vec![2]);
        assert_eq!(payloads(&found), vec![rid(20)]);
        assert_eq!(ints(&all(&tree)), vec![1, 2, 3]);
    }

    #[test]
    fn seek_between_and_full_both_directions() {
        let (_dir, storage) = instance("btree-between");
        let mut tree = int_tree(&storage);
        for number in [7, 2, 9, 4, 1, 10, 6, 3, 8, 5] {
            tree.insert(&[Value::I32(number)], &rid(number as u64))
                .expect("insert an entry");
        }
        let seek = |range: KeyRange, dir| tree.seek(&range, dir).expect("seek the range");

        assert_eq!(
            ints(&seek(KeyRange::Full, Direction::Forward)),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
        assert_eq!(
            ints(&seek(KeyRange::Full, Direction::Backward)),
            vec![10, 9, 8, 7, 6, 5, 4, 3, 2, 1]
        );

        let closed = KeyRange::Between(
            Bound::Included(vec![Value::I32(3)]),
            Bound::Included(vec![Value::I32(7)]),
        );
        assert_eq!(
            ints(&seek(closed.clone(), Direction::Forward)),
            vec![3, 4, 5, 6, 7]
        );
        assert_eq!(
            ints(&seek(closed, Direction::Backward)),
            vec![7, 6, 5, 4, 3]
        );

        let open = KeyRange::Between(
            Bound::Excluded(vec![Value::I32(3)]),
            Bound::Excluded(vec![Value::I32(7)]),
        );
        assert_eq!(ints(&seek(open.clone(), Direction::Forward)), vec![4, 5, 6]);
        assert_eq!(ints(&seek(open, Direction::Backward)), vec![6, 5, 4]);

        let from = KeyRange::Between(Bound::Included(vec![Value::I32(8)]), Bound::Unbounded);
        assert_eq!(
            ints(&seek(from.clone(), Direction::Forward)),
            vec![8, 9, 10]
        );
        assert_eq!(ints(&seek(from, Direction::Backward)), vec![10, 9, 8]);

        let until = KeyRange::Between(Bound::Unbounded, Bound::Excluded(vec![Value::I32(4)]));
        assert_eq!(
            ints(&seek(until.clone(), Direction::Forward)),
            vec![1, 2, 3]
        );
        assert_eq!(ints(&seek(until, Direction::Backward)), vec![3, 2, 1]);

        let above = KeyRange::Between(Bound::Included(vec![Value::I32(11)]), Bound::Unbounded);
        assert!(seek(above.clone(), Direction::Forward).is_empty());
        assert!(seek(above, Direction::Backward).is_empty());
    }

    #[test]
    fn split_grows_root() {
        let (_dir, storage) = instance("btree-grow");
        let mut tree = int_tree(&storage);
        let leaf_root = tree.root();
        assert_eq!(kind_of(&storage, leaf_root), PageKind::BTreeLeaf);

        for number in 0..300 {
            tree.insert(&[Value::I32(number)], &rid(number as u64))
                .expect("insert an entry");
        }

        assert_ne!(tree.root(), leaf_root, "the root moved");
        assert_eq!(kind_of(&storage, tree.root()), PageKind::BTreeInternal);
        assert_eq!(ints(&all(&tree)), (0..300).collect::<Vec<_>>());
        assert_eq!(
            ints(
                &tree
                    .seek(&KeyRange::Full, Direction::Backward)
                    .expect("seek backwards")
            ),
            (0..300).rev().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_third_level_appears_when_the_root_splits() {
        let (_dir, storage) = instance("btree-third-level");
        let mut tree = int_tree(&storage);
        let payload = vec![b'.'; 200];
        for number in 0..2_000 {
            tree.insert(&[Value::I32(number)], &payload)
                .expect("insert an entry");
        }

        let root = page_of(&storage, tree.root());
        assert_eq!(
            root.kind().expect("the kind of the root"),
            PageKind::BTreeInternal
        );
        let child = internal_child(&root, 0).expect("the leftmost child of the root");
        assert_eq!(
            kind_of(&storage, child),
            PageKind::BTreeInternal,
            "the children of the root are internal pages, so the tree has three levels"
        );
        assert_eq!(ints(&all(&tree)), (0..2_000).collect::<Vec<_>>());
    }

    #[test]
    fn delete_then_seek_absent() {
        let (_dir, storage) = instance("btree-delete");
        let mut tree = int_tree(&storage);
        for number in 1..=3 {
            tree.insert(&[Value::I32(number)], &rid(number as u64))
                .expect("insert an entry");
        }

        assert!(
            tree.delete(&[Value::I32(2)], &rid(2))
                .expect("delete the entry 2")
        );
        assert!(
            tree.seek(&KeyRange::Point(vec![Value::I32(2)]), Direction::Forward)
                .expect("seek the point 2")
                .is_empty()
        );
        assert_eq!(ints(&all(&tree)), vec![1, 3]);

        assert!(
            !tree
                .delete(&[Value::I32(2)], &rid(2))
                .expect("delete the entry 2 again"),
            "the second delete finds nothing"
        );
        assert!(
            !tree
                .delete(&[Value::I32(9)], &rid(9))
                .expect("delete an entry that was never there")
        );
        assert!(
            !tree
                .delete(&[Value::I32(1)], &rid(7))
                .expect("delete the key 1 under another payload"),
            "the payload is part of what is deleted"
        );
        assert_eq!(ints(&all(&tree)), vec![1, 3]);
    }

    #[test]
    fn nulls_first_on_int_column() {
        let (_dir, storage) = instance("btree-nulls");
        let mut tree = int_tree(&storage);
        tree.insert(&[Value::I32(1)], &rid(1)).expect("insert 1");
        tree.insert(&[Value::Null], &rid(0)).expect("insert NULL");

        let found = all(&tree);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, vec![Value::Null], "NULL comes first");
        assert_eq!(found[1].0, vec![Value::I32(1)]);
        assert_eq!(
            payloads(
                &tree
                    .seek(&KeyRange::Point(vec![Value::Null]), Direction::Forward)
                    .expect("seek the NULL")
            ),
            vec![rid(0)],
            "NULL is equal to NULL"
        );
    }

    #[test]
    fn descending_column_reverses_the_order_nulls_included() {
        let (_dir, storage) = instance("btree-descending");
        let columns = vec![KeyColumn {
            column: 0,
            descending: true,
        }];
        let mut tree =
            BTree::create(&storage, &columns, &int_types()).expect("create a descending tree");
        for number in [2, 1, 3] {
            tree.insert(&[Value::I32(number)], &rid(number as u64))
                .expect("insert an entry");
        }
        tree.insert(&[Value::Null], &rid(0)).expect("insert NULL");

        let found = all(&tree);
        assert_eq!(
            found
                .iter()
                .map(|(values, _)| values[0].clone())
                .collect::<Vec<_>>(),
            vec![Value::I32(3), Value::I32(2), Value::I32(1), Value::Null],
            "the descending column reverses the values and puts NULL last"
        );
    }

    #[test]
    fn negative_ints_sort_below_zero_as_compare_orders_them() {
        let (_dir, storage) = instance("btree-negative");
        let mut tree = int_tree(&storage);
        for number in [1, -1, 0] {
            tree.insert(&[Value::I32(number)], &rid(0))
                .expect("insert an entry");
        }
        assert_eq!(ints(&all(&tree)), vec![-1, 0, 1]);

        // The distinguishing vector: read as bytes, the encoded key of -1 sits above that of 1
        // (the encoder is little-endian and untouched by the sign), so a tree that
        // ordered its entries by their bytes would answer 0, 1, -1 above.
        assert!(
            encode_entry_key(&[Value::I32(-1)], &rid(0))
                > encode_entry_key(&[Value::I32(1)], &rid(0))
        );
    }

    #[test]
    fn a_page_full_of_one_key_splits_because_the_payloads_differ() {
        let (_dir, storage) = instance("btree-one-key");
        let mut tree = int_tree(&storage);
        for row in 0..400u64 {
            tree.insert(&[Value::I32(7)], &rid(row))
                .expect("insert an entry of the key 7");
        }

        assert_eq!(
            kind_of(&storage, tree.root()),
            PageKind::BTreeInternal,
            "400 entries of one key filled the leaf root and split it"
        );
        let found = tree
            .seek(&KeyRange::Point(vec![Value::I32(7)]), Direction::Forward)
            .expect("seek the point 7");
        assert_eq!(found.len(), 400);
        assert_eq!(ints(&found), vec![7; 400]);
        assert_eq!(
            payloads(&found),
            (0..400u64).map(rid).collect::<Vec<_>>(),
            "the entries of one key come back in payload order"
        );
    }

    #[test]
    fn entries_of_uneven_size_read_back_in_order() {
        let (_dir, storage) = instance("btree-uneven");
        let mut tree = int_tree(&storage);
        // Payloads of 8 to 1 600 bytes, 200 sizes taken in turn: the cut of a split follows the
        // byte sizes (`balance_point`), so it lands away from the middle of the entries.
        let payload = |number: i32| vec![b'x'; 8 + (number % 200) as usize * 8];
        for step in 0..1_200i32 {
            let number = (step * 7) % 1_200;
            tree.insert(&[Value::I32(number)], &payload(number))
                .expect("insert an entry");
        }

        let found = all(&tree);
        assert_eq!(found.len(), 1_200);
        assert_eq!(ints(&found), (0..1_200).collect::<Vec<_>>());
        assert_eq!(
            found
                .iter()
                .map(|(_, bytes)| bytes.len())
                .collect::<Vec<_>>(),
            (0..1_200)
                .map(|number| payload(number).len())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            ints(
                &tree
                    .seek(&KeyRange::Full, Direction::Backward)
                    .expect("seek backwards")
            ),
            (0..1_200).rev().collect::<Vec<_>>()
        );
    }

    #[test]
    fn delete_takes_one_occurrence_of_a_duplicated_key() {
        let (_dir, storage) = instance("btree-duplicates");
        let mut tree = int_tree(&storage);
        for row in [1u64, 2, 3] {
            tree.insert(&[Value::I32(5)], &rid(row))
                .expect("insert an entry of the key 5");
        }

        assert!(
            tree.delete(&[Value::I32(5)], &rid(2))
                .expect("delete one of the three")
        );
        let found = tree
            .seek(&KeyRange::Point(vec![Value::I32(5)]), Direction::Forward)
            .expect("seek the point 5");
        assert_eq!(payloads(&found), vec![rid(1), rid(3)]);
        assert!(
            !tree
                .delete(&[Value::I32(5)], &rid(2))
                .expect("delete the same one again")
        );
    }

    #[test]
    fn an_emptied_leaf_stays_in_the_chain_and_a_seek_steps_over_it() {
        let (_dir, storage) = instance("btree-emptied");
        let mut tree = int_tree(&storage);
        for number in 0..300 {
            tree.insert(&[Value::I32(number)], &rid(number as u64))
                .expect("insert an entry");
        }

        let first = tree.edge_leaf(true).expect("the leftmost leaf");
        let held = leaf_len(&page_of(&storage, first));
        assert!(
            held > 0 && held < 300,
            "the leftmost leaf holds {held} of the 300 entries"
        );
        for number in 0..held as i32 {
            assert!(
                tree.delete(&[Value::I32(number)], &rid(number as u64))
                    .expect("delete an entry of the leftmost leaf")
            );
        }

        assert_eq!(leaf_len(&page_of(&storage, first)), 0, "the leaf is empty");
        assert_eq!(
            tree.edge_leaf(true).expect("the leftmost leaf again"),
            first,
            "the empty leaf is still the leftmost one: no merge, no page given back"
        );
        assert_eq!(
            ints(&all(&tree)),
            (held as i32..300).collect::<Vec<_>>(),
            "a forward seek steps over the empty leaf"
        );
        assert_eq!(
            ints(
                &tree
                    .seek(&KeyRange::Full, Direction::Backward)
                    .expect("seek backwards")
            ),
            (held as i32..300).rev().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_two_column_key_seeks_by_prefix() {
        let (_dir, storage) = instance("btree-prefix");
        let types = vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::Int, true),
        ];
        let columns = vec![
            KeyColumn {
                column: 0,
                descending: false,
            },
            KeyColumn {
                column: 1,
                descending: false,
            },
        ];
        let mut tree = BTree::create(&storage, &columns, &types).expect("create a tree");
        for (left, right) in [(1, 1), (1, 2), (2, 1)] {
            tree.insert(&[Value::I32(left), Value::I32(right)], &rid(0))
                .expect("insert an entry");
        }

        let prefix = tree
            .seek(&KeyRange::Point(vec![Value::I32(1)]), Direction::Forward)
            .expect("seek the prefix 1");
        assert_eq!(
            prefix
                .iter()
                .map(|(values, _)| values.clone())
                .collect::<Vec<_>>(),
            vec![
                vec![Value::I32(1), Value::I32(1)],
                vec![Value::I32(1), Value::I32(2)]
            ]
        );
        let whole = tree
            .seek(
                &KeyRange::Point(vec![Value::I32(1), Value::I32(2)]),
                Direction::Forward,
            )
            .expect("seek the whole key");
        assert_eq!(whole.len(), 1);
        let err = tree
            .seek(
                &KeyRange::Point(vec![Value::I32(1), Value::I32(2), Value::I32(3)]),
                Direction::Forward,
            )
            .expect_err("a bound of three values on a key of two columns");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            "internal bug: a key bound carries 3 values, the key has 2 columns"
        );
    }

    #[test]
    fn a_seek_answers_a_vec_a_later_insert_does_not_change() {
        let (_dir, storage) = instance("btree-isolation");
        let mut tree = int_tree(&storage);
        for number in 0..10 {
            tree.insert(&[Value::I32(number)], &rid(0))
                .expect("insert an entry");
        }

        let taken = all(&tree);
        tree.insert(&[Value::I32(10)], &rid(0))
            .expect("insert one more");
        assert_eq!(ints(&taken), (0..10).collect::<Vec<_>>());
        assert_eq!(all(&tree).len(), 11);
    }

    #[test]
    fn seek_of_an_empty_tree_answers_nothing() {
        let (_dir, storage) = instance("btree-empty");
        let mut tree = int_tree(&storage);
        for dir in [Direction::Forward, Direction::Backward] {
            assert!(
                tree.seek(&KeyRange::Full, dir)
                    .expect("seek an empty tree")
                    .is_empty()
            );
            assert!(
                tree.seek(&KeyRange::Point(vec![Value::I32(1)]), dir)
                    .expect("seek a point of an empty tree")
                    .is_empty()
            );
        }
        assert!(
            !tree
                .delete(&[Value::I32(1)], &rid(0))
                .expect("delete from an empty tree")
        );
    }

    #[test]
    fn a_tree_reopened_by_its_root_reads_its_entries_back() {
        let dir = TempDir::created("btree-reopen");
        let root = {
            let storage =
                DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
            let mut tree = int_tree(&storage);
            for number in 0..300 {
                tree.insert(&[Value::I32(number)], &rid(number as u64))
                    .expect("insert an entry");
            }
            storage.pool.flush_all().expect("flush the pool");
            tree.root()
        };

        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen the instance");
        let tree =
            BTree::open(&storage, root, &ascending(), &int_types()).expect("reopen the tree");
        assert_eq!(tree.root(), root);
        assert_eq!(ints(&all(&tree)), (0..300).collect::<Vec<_>>());
    }

    #[test]
    fn a_leaf_that_lost_slots_takes_entries_again_after_a_compaction() {
        let order = KeyOrder::new(&ascending(), &int_types()).expect("build the order");
        let probe = |number: i32| Probe {
            values: vec![Value::I32(number)],
            payload: PayloadBound::At(rid(number as u64)),
        };
        let entry = |number: i32| encode_entry_key(&[Value::I32(number)], &rid(number as u64));

        let mut page = leaf(1);
        let mut count = 0i32;
        while place_in_leaf(&mut page, &order, &probe(count), &entry(count))
            .expect("put an entry in the leaf")
            == Put::Inserted
        {
            count += 1;
        }
        assert_eq!(count, 262, "a leaf takes 262 entries of this shape");

        // The slots of the 100 lowest keys go. They were written first, at the highest offsets,
        // so the free span between the directory and the entries grows by the four bytes of each
        // freed slot, which is what `taken_raw` reads below; the bytes of the entries
        // themselves come back at the compaction.
        let removed = 100;
        for _ in 0..removed {
            remove_slot(&mut page, LEAF_DIRECTORY, 0).expect("take a slot out");
        }

        let mut raw = page.clone();
        let mut taken_raw = 0i32;
        loop {
            let index = leaf_len(&raw);
            let bytes = entry(count + taken_raw);
            if put_entry(&mut raw, LEAF_DIRECTORY, index, &bytes, &[])
                .expect("a put without compaction")
                == Put::Full
            {
                break;
            }
            taken_raw += 1;
        }

        let mut taken = 0i32;
        while place_in_leaf(
            &mut page,
            &order,
            &probe(count + taken),
            &entry(count + taken),
        )
        .expect("a put that compacts when it finds no room")
            == Put::Inserted
        {
            taken += 1;
        }

        assert_eq!(taken, removed, "the compaction hands the 100 entries back");
        assert_eq!(
            taken_raw, 13,
            "without the compaction, only the four bytes of each freed slot are reused"
        );
        assert_eq!(leaf_len(&page), (count - removed + taken) as usize);
    }

    /// A pseudo-random source with a seed, so that the sizes a test draws are the same at each
    /// run.
    struct Draw(u64);

    impl Draw {
        /// The next draw.
        fn step(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }

        /// A draw in `0..n`.
        fn below(&mut self, n: usize) -> usize {
            (self.step() % n as u64) as usize
        }
    }

    #[test]
    fn the_ceiling_of_an_entry_is_a_quarter_of_a_leaf() {
        assert_eq!(MAX_INSERT_BYTES, 2_028);
        assert_eq!(PAGE_SIZE - LEAF_DIRECTORY, 8_144);
        let entry = SLOT_SIZE + ENTRY_OVERHEAD + MAX_INSERT_BYTES;
        assert_eq!(
            entry, 2_036,
            "a leaf entry at the ceiling weighs 2 036 bytes"
        );
        assert_eq!(
            4 * entry,
            PAGE_SIZE - LEAF_DIRECTORY,
            "four of them are exactly what a leaf holds"
        );

        // A separator carries the same key plus the eight bytes of a child; an internal page
        // holds three of those, the number `split_internal` asks for.
        let separator = entry + CHILD_BYTES;
        assert_eq!(separator, 2_044);
        assert!(
            3 * separator <= PAGE_SIZE - INTERNAL_DIRECTORY,
            "three separators of {separator} bytes hold in the {} an internal page leaves",
            PAGE_SIZE - INTERNAL_DIRECTORY
        );
        assert!(
            4 * separator > PAGE_SIZE - INTERNAL_DIRECTORY,
            "and four do not"
        );
    }

    #[test]
    fn two_hundred_entries_at_the_ceiling_read_back_in_order() {
        let (_dir, storage) = instance("btree-ceiling");
        let mut tree = int_tree(&storage);
        let frame = encode_entry_key(&[Value::I32(0)], &[]).len();
        assert_eq!(frame, 15, "an int key and an empty payload take 15 bytes");
        let payload = vec![b'x'; MAX_INSERT_BYTES - frame];
        assert_eq!(
            encode_entry_key(&[Value::I32(0)], &payload).len(),
            MAX_INSERT_BYTES,
            "the entry sits exactly at the ceiling"
        );

        for number in 0..200 {
            tree.insert(&[Value::I32(number)], &payload)
                .expect("insert an entry of the size the ceiling allows");
        }

        let found = all(&tree);
        assert_eq!(found.len(), 200);
        assert_eq!(ints(&found), (0..200).collect::<Vec<_>>());
        assert_eq!(found[199].1.len(), MAX_INSERT_BYTES - frame);
        let root = page_of(&storage, tree.root());
        assert_eq!(
            root.kind().expect("the kind of the root"),
            PageKind::BTreeInternal,
            "the root split"
        );
        let child = internal_child(&root, 0).expect("the leftmost child of the root");
        assert_eq!(
            kind_of(&storage, child),
            PageKind::BTreeInternal,
            "and its children are internal pages: three levels at 200 entries of this size"
        );
    }

    #[test]
    fn an_entry_above_the_ceiling_is_refused_before_any_write() {
        let (_dir, storage) = instance("btree-above-ceiling");
        let mut tree = int_tree(&storage);
        let frame = encode_entry_key(&[Value::I32(0)], &[]).len();
        let pages_before = storage
            .control()
            .expect("read the control block")
            .next_page_id;

        let err = tree
            .insert(&[Value::I32(1)], &vec![b'x'; MAX_INSERT_BYTES - frame + 1])
            .expect_err("one byte above the ceiling");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(
            err.to_string().starts_with(&format!(
                "internal bug: an entry of {} bytes",
                MAX_INSERT_BYTES + 1
            )),
            "{err}"
        );
        assert_eq!(
            storage
                .control()
                .expect("read the control block again")
                .next_page_id,
            pages_before,
            "the refusal allocated no page"
        );
        assert_eq!(
            leaf_len(&page_of(&storage, tree.root())),
            0,
            "and wrote nothing in the root leaf"
        );

        // One byte less goes in, so the refusal is the ceiling and not the shape of the entry.
        tree.insert(&[Value::I32(1)], &vec![b'x'; MAX_INSERT_BYTES - frame])
            .expect("an entry at the ceiling");
        assert_eq!(all(&tree).len(), 1);
    }

    #[test]
    fn sizes_drawn_below_the_ceiling_read_back_against_a_model() {
        let (_dir, storage) = instance("btree-drawn-sizes");
        let mut tree = int_tree(&storage);
        let frame = encode_entry_key(&[Value::I32(0)], &[]).len();
        let mut draw = Draw(0x5eed_0018);
        let mut model: Vec<(i32, Vec<u8>)> = Vec::new();
        let mut widest = 0;

        for step in 0..600u64 {
            let key = draw.below(120) as i32 - 40;
            let width = 8 + draw.below(MAX_INSERT_BYTES - frame - 7);
            let mut payload = rid(step);
            payload.resize(width, b'z');
            assert_eq!(
                encode_entry_key(&[Value::I32(key)], &payload).len(),
                width + frame
            );
            widest = widest.max(width);
            tree.insert(&[Value::I32(key)], &payload)
                .expect("insert an entry of a drawn size");
            model.push((key, payload));
        }

        assert_eq!(widest, 2_006, "the widest draw of this seed");
        assert!(widest + frame <= MAX_INSERT_BYTES);
        model.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let got = all(&tree)
            .into_iter()
            .map(|(values, payload)| match values.as_slice() {
                [Value::I32(number)] => (*number, payload),
                other => panic!("an entry of the tree carries {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            got, model,
            "600 entries of drawn sizes, in key then payload order"
        );
        assert_eq!(
            kind_of(&storage, tree.root()),
            PageKind::BTreeInternal,
            "the draws filled more than one page"
        );
    }

    #[test]
    fn five_payloads_around_the_ceiling_are_refused_with_the_tree_intact() {
        let (_dir, storage) = instance("btree-ceiling-payloads");
        let mut tree = int_tree(&storage);
        for (key, width) in [(1, 1_341usize), (2, 1_340)] {
            tree.insert(&[Value::I32(key)], &vec![b'x'; width])
                .expect("a payload below the ceiling");
        }
        let pages_before = storage
            .control()
            .expect("read the control block")
            .next_page_id;
        let before = all(&tree);

        for (key, width) in [(3, 2_686usize), (4, 2_685), (9, 2_686)] {
            let err = tree
                .insert(&[Value::I32(key)], &vec![b'x'; width])
                .expect_err("a payload above the ceiling");
            assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        }

        assert_eq!(
            storage
                .control()
                .expect("read the control block again")
                .next_page_id,
            pages_before,
            "the three refusals allocated no page"
        );
        assert_eq!(all(&tree), before, "and left the entries as they were");

        // An entry put after the refusals reads back with the two that were there, and the
        // refused key 9 is absent: the refusals left no entry behind.
        tree.insert(&[Value::I32(5)], &[b'x'; 10])
            .expect("a small entry after the refusals");
        assert_eq!(ints(&all(&tree)), vec![1, 2, 5]);
        assert!(
            tree.seek(&KeyRange::Point(vec![Value::I32(9)]), Direction::Forward)
                .expect("seek the refused key")
                .is_empty()
        );
    }

    #[test]
    fn a_half_of_a_split_without_room_is_refused_before_the_allocation() {
        let order = KeyOrder::new(&ascending(), &int_types()).expect("build the order");
        let mut page = leaf(7);
        // Two entries heavier than the tree itself takes, put through the byte API of the page
        // layer: the cut then leaves one half nearly full.
        let big = encode_entry_key(&[Value::I32(1)], &vec![b'x'; 6_200]);
        let small = encode_entry_key(&[Value::I32(9)], &vec![b'x'; 1_000]);
        assert_eq!(
            leaf_put(&mut page, &big, &[]).expect("put the heavy entry"),
            Put::Inserted
        );
        assert_eq!(
            leaf_put(&mut page, &small, &[]).expect("put the light entry"),
            Put::Inserted
        );
        let before = page.clone();

        let payload = vec![b'x'; MAX_INSERT_BYTES - 15];
        let probe = Probe {
            values: vec![Value::I32(5)],
            payload: PayloadBound::At(payload.clone()),
        };
        let entry = encode_entry_key(&[Value::I32(5)], &payload);
        let err = leaf_split_fits(&page, &order, &probe, &entry)
            .expect_err("the left half of the cut has no room for the entry");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            "internal bug: the left half of the split of leaf 7 would hold 6223 bytes and has \
             no room for the 2036 of the entry that caused the split; this build does not split \
             it again"
        );
        assert_eq!(page.0, before.0, "the check wrote nothing");

        // Counter-check: the same page and the same cut take a lighter entry.
        let light = vec![b'x'; 100];
        let probe = Probe {
            values: vec![Value::I32(5)],
            payload: PayloadBound::At(light.clone()),
        };
        leaf_split_fits(
            &page,
            &order,
            &probe,
            &encode_entry_key(&[Value::I32(5)], &light),
        )
        .expect("a lighter entry fits the half it goes to");
    }

    #[test]
    fn a_key_that_does_not_match_the_columns_is_a_bug() {
        let (_dir, storage) = instance("btree-key-shape");
        let mut tree = int_tree(&storage);
        let err = tree.insert(&[], &rid(0)).expect_err("a key of no value");
        assert_eq!(
            err.to_string(),
            "internal bug: a B+tree entry carries 0 values, the key has 1 columns"
        );
        assert!(
            tree.insert(&[Value::I32(1), Value::I32(2)], &rid(0))
                .is_err(),
            "a key of two values on a key of one column"
        );
        assert!(tree.delete(&[], &rid(0)).is_err());

        assert!(
            KeyOrder::new(&[], &int_types()).is_err(),
            "a key of no column"
        );
        let err = KeyOrder::new(
            &[KeyColumn {
                column: 3,
                descending: false,
            }],
            &int_types(),
        )
        .expect_err("a column outside the table");
        assert_eq!(
            err.to_string(),
            "internal bug: key column 3 is outside the 1 columns of the table"
        );
    }
}
