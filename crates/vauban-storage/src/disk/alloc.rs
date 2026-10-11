//! Page allocator of the on-disk layout: hands out a [`PageId`], grows the data file when the
//! free list is empty, and chains a freed page at the head of that list.
//!
//! # One page per call
//!
//! One call to [`allocate`] hands out one page, and grows `data` by 8 192 bytes when the free
//! list is empty: `allocate_extends_file` reads a file of 8 192 bytes after the first call and
//! of 16 384 after the second. There are no extents, GAM or PFS bitmaps, so a caller that
//! wants sixteen pages calls [`allocate`] sixteen times.
//!
//! # The free list lives in the pages
//!
//! [`super::control::Control`] holds its head, `free_head`, and each page of [`PageKind::Free`]
//! holds the identifier of the next one in the eight bytes at [`OFF_NEXT_FREE`], the first
//! eight after the header. [`END_OF_FREE_LIST`] ends the chain and is the value `free_head`
//! carries
//! for an empty list; it is also the identifier [`allocate`] refuses to hand out, so a caller
//! may read it as the sentinel it is (`allocate_refuses_to_hand_out_the_sentinel`).
//!
//! [`free`] chains at the head and [`allocate`] detaches it: the page freed last is the one
//! handed out first (`free_then_allocate_walks_the_list_from_its_head`).
//!
//! # What a caller gets
//!
//! [`allocate`] does not pin the page it hands out: the
//! caller pins it and writes its own kind over [`PageKind::Free`]
//! (`allocate_leaves_the_page_unpinned`). The page is sealed in `data`, so that pin reads a
//! page back through [`super::buffer::BufferPool`] instead of meeting eight kibibytes of
//! zeros. A recycled page still carries the bytes its previous use left after the link; the
//! caller writes its own directory over them.
//!
//! # Ordering, and what a crash may leave
//!
//! Both entry points change `vauban.ctl` last: [`super::control::Control::write`] rewrites the
//! file and `sync_all`s it, and the block held in memory moves forward once that has
//! succeeded, so a failed write leaves the two in agreement. [`free`] flushes the page it
//! chained and syncs `data` before it writes the control file, so `data` holds the link the
//! ctl is about to name (`free_clears_in_progress_so_the_link_reaches_data`), a crash that
//! loses the writes `data` had not synced included
//! (`super::recover::tests::a_free_list_reads_back_after_a_crash_that_loses_the_unsynced_writes`).
//!
//! # What the journal says of it
//!
//! Neither call writes a journal record: the free list lives in `vauban.ctl` and in pages
//! written and synced to `data` by [`free`] before the control file names them, which the redo
//! does not replay. What keeps the redo of the journal off a freed page is its `page_lsn`:
//! [`free`] gives the page the number of the last record of the journal, so the records of
//! its previous use — the record that created its table, which formatted it, among them — are
//! at or below the number the free page carries on `data`, and the redo leaves the page alone
//! for them (`a_freed_page_keeps_the_number_of_its_last_record`,
//! `super::recover::tests::a_free_list_reads_back_after_a_crash_that_loses_the_unsynced_writes`).
//! The first write of its next owner is journalled whole ([`super::wal::PageSet`]), past that
//! number.
//!
//! [`free`] syncs the journal before it writes the page, when the journal holds records it has
//! not made durable: the caller appends the record that takes the page out of its structure —
//! the release of the slot that named an overflow chain, the drop of a table — before it frees
//! the page, and a page listed as free while that record could still be lost would have two
//! owners after a crash.
//!
//! A crash between the growth of `data` and the write of the control file leaves a page in the
//! file that `next_page_id` does not count. [`super::DiskStorage::open`] calls
//! [`count_the_pages_of_data`], which raises `next_page_id` to the size of `data`, so the next
//! [`allocate`] grows the file past those pages rather than asking it to shrink
//! (`a_reopen_counts_the_pages_data_holds_past_the_counter`). The pages it counts are left
//! out of use, neither handed out nor listed as free: one of them may be named by a structure
//! whose allocation the control file missed, and handing it out again would give it two
//! owners. Losing a page in flight this way is accepted: the journal does not cover
//! allocations, and a page allocated but not yet linked to a table is an orphan of the DDL
//! protocol.
//!
//! A page [`allocate`] grows the file by is written straight to `data`, without a record: it
//! is a free page no structure names yet.
//!
//! # Locking
//!
//! An allocation takes the lock of the control block ([`DiskStorage::lock_control`]) and holds
//! it across the pin it needs, so an allocation under way is not raced by another: the second
//! waits for that lock before it reads `next_page_id`. The order is control block, then buffer
//! pool.

use vauban_errors::InternalError;

use super::DiskStorage;
use super::buffer::DurableLsn;
use super::page::{HEADER_SIZE, Page, PageId, PageKind};

/// Offset, in a page of kind [`PageKind::Free`], of the identifier of the next free page: the
/// eight bytes right after the common header, which the free list writes on a page of that
/// kind.
pub(crate) const OFF_NEXT_FREE: usize = HEADER_SIZE;

/// Value carried by the page that ends the free list, and by `free_head` when the list is
/// empty. [`allocate`] refuses to hand out this identifier.
pub(crate) const END_OF_FREE_LIST: u64 = u64::MAX;

/// `lower` of a page the allocator writes as [`PageKind::Free`]: the first byte after the
/// header and the link, that is the kind-specific directory of a free page.
const FREE_PAGE_LOWER: u16 = (OFF_NEXT_FREE + 8) as u16;

/// Hands out a page of the instance, growing the data file when the free list is empty.
///
/// The page comes back with kind [`PageKind::Free`], `slot_count` 0 and unpinned: the caller
/// pins it and writes the kind of its own structure. The identifier is either the head of the
/// free list, detached here, or `next_page_id`, and then `data` grows by one page.
///
/// # Errors
///
/// [`InternalError::Corruption`] when `free_head` names a page whose kind is not
/// [`PageKind::Free`], or when the page it names cannot be read;
/// [`InternalError::Bug`] when `next_page_id` has reached [`END_OF_FREE_LIST`], which is the
/// sentinel and not a page; [`InternalError::Io`] for a failure reported by the operating
/// system while `data` or `vauban.ctl` is written.
pub(crate) fn allocate(storage: &DiskStorage) -> Result<PageId, InternalError> {
    let mut control = storage.lock_control()?;
    let mut updated = *control;
    let id = match updated.free_head {
        Some(head) => {
            updated.free_head = detach_head(storage, head)?;
            head
        }
        None => {
            let id = updated.next_page_id;
            if id.0 == END_OF_FREE_LIST {
                return Err(InternalError::Bug(format!(
                    "instance {} has handed out {id} pages; {END_OF_FREE_LIST} is the sentinel \
                     of the free list, not a page",
                    storage.dir.display()
                )));
            }
            grow_by_one_page(storage, id)?;
            updated.next_page_id = PageId(id.0 + 1);
            id
        }
    };
    updated.write(&storage.control_path())?;
    *control = updated;
    Ok(id)
}

/// Raises `next_page_id` to the number of pages `data` holds, when the file holds more.
///
/// A control file whose counter is behind `data` would make the next [`allocate`] ask
/// [`super::file::DataFile::extend_to`] for a smaller file, which it refuses: each later
/// allocation would fail the same way. The pages past the counter are counted as handed out
/// and left as they are; the control file is rewritten when the counter moves, and a
/// counter at or past the size of `data` is left alone.
///
/// # Errors
///
/// [`InternalError::Io`] for a failure reported by the operating system while the size of
/// `data` is read or `vauban.ctl` is written; [`InternalError::Corruption`] for a lock a
/// panicking thread left poisoned.
pub(crate) fn count_the_pages_of_data(storage: &DiskStorage) -> Result<(), InternalError> {
    let pages = storage.data.page_count()?;
    let mut control = storage.lock_control()?;
    if pages <= control.next_page_id.0 {
        return Ok(());
    }
    let mut updated = *control;
    updated.next_page_id = PageId(pages);
    updated.write(&storage.control_path())?;
    *control = updated;
    Ok(())
}

/// Puts the page `id` back in the free list, at its head.
///
/// The page is rewritten as a [`PageKind::Free`] one carrying the old head as its link, its
/// `in_progress` flag is cleared, and it is written to `data` and `data` synced before
/// `vauban.ctl` names it as the head. The page takes the number of the last record of the
/// journal, which is synced first when it holds records past the durable one, the record of
/// the caller among them (`a_freed_page_keeps_the_number_of_its_last_record`).
///
/// # Errors
///
/// [`InternalError::Bug`] when `id` was not handed out by [`allocate`] — that is when it is
/// `next_page_id` or past it — and when the page carries a pin: rewriting it asks for the
/// exclusive pin [`super::buffer::PinGuard::with_page_mut`] answers for, so a page a caller
/// still holds is refused (`free_pinned_is_bug` keeps one pin and reads "2 pins"). The
/// control block is left as it was in both cases.
///
/// The errors of the journal otherwise.
pub(crate) fn free(storage: &DiskStorage, id: PageId) -> Result<(), InternalError> {
    let mut control = storage.lock_control()?;
    let mut updated = *control;
    if id.0 >= updated.next_page_id.0 {
        return Err(InternalError::Bug(format!(
            "page {id} was not handed out by the allocator of instance {}, which has handed \
             out {} of them",
            storage.dir.display(),
            updated.next_page_id
        )));
    }

    let head = updated.free_head;
    let last = storage.wal.last_lsn()?;
    if last > storage.wal.durable_lsn() {
        storage.wal.flush()?;
    }
    let pin = storage.pool.pin(id)?;
    pin.with_page_mut(|page| {
        page.set_kind(PageKind::Free);
        page.set_slot_count(0);
        page.set_lower(FREE_PAGE_LOWER);
        set_next_free(page, head);
        if last > page.lsn() {
            page.set_lsn(last);
        }
    })?;
    storage.pool.mark_dirty(id, last)?;
    storage.pool.set_in_progress(id, false)?;
    drop(pin);
    storage.pool.flush(id)?;
    sync_data(storage)?;

    updated.free_head = Some(id);
    updated.write(&storage.control_path())?;
    *control = updated;
    Ok(())
}

/// `sync_all`s `data`, through a handle of its own on the file.
fn sync_data(storage: &DiskStorage) -> Result<(), InternalError> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(storage.data.path())?
        .sync_all()?;
    #[cfg(test)]
    tests::note_data_sync(storage);
    Ok(())
}

/// The pages of the free list, its head first, read without changing it.
///
/// Each page of the list must be of kind [`PageKind::Free`] and appear once: a page listed
/// twice would be handed out twice by [`allocate`], and a page of another kind is one a
/// structure still uses. The walk stops at [`END_OF_FREE_LIST`], or at the first page it
/// meets a second time, so a link that leads back up the list ends it with an error.
///
/// # Errors
///
/// [`InternalError::Corruption`] for a page listed twice, for a page of another kind, and for a
/// link to a page the allocator has not handed out; the errors of the buffer pool.
#[cfg(test)]
pub(crate) fn free_list(storage: &DiskStorage) -> Result<Vec<PageId>, InternalError> {
    let control = storage.lock_control()?;
    let mut pages = Vec::new();
    let mut listed = std::collections::HashSet::new();
    let mut next = control.free_head;
    while let Some(id) = next {
        if id.0 >= control.next_page_id.0 {
            return Err(InternalError::Corruption(format!(
                "free list links to page {id}, past the {} pages handed out",
                control.next_page_id
            )));
        }
        if !listed.insert(id) {
            return Err(InternalError::Corruption(format!(
                "free list lists page {id} twice"
            )));
        }
        let pin = storage.pool.pin(id)?;
        let (kind, link) = pin.with_page(|page| (page.kind(), next_free(page)))?;
        let kind = kind?;
        if kind != PageKind::Free {
            return Err(InternalError::Corruption(format!(
                "free list lists page {id}, whose kind is {kind:?}"
            )));
        }
        pages.push(id);
        next = link;
    }
    Ok(pages)
}

/// Detaches the head of the free list and answers what it pointed at.
fn detach_head(storage: &DiskStorage, head: PageId) -> Result<Option<PageId>, InternalError> {
    let pin = storage.pool.pin(head)?;
    let (kind, next) = pin.with_page(|page| (page.kind(), next_free(page)))?;
    let kind = kind?;
    if kind != PageKind::Free {
        return Err(InternalError::Corruption(format!(
            "free list of instance {} has head {head}, whose kind is {kind:?}",
            storage.dir.display()
        )));
    }
    Ok(next)
}

/// Grows `data` so that it holds the page `id`, and writes a sealed free page over it.
///
/// The bytes [`super::file::DataFile::extend_to`] adds are zeros, which are not a page: the
/// sealed page written here is what a later `pin` reads back. It goes through the file layer
/// rather than through the pool, because a frame of the pool is filled by a `pin` and a `pin`
/// of a page past the end of `data` is refused by [`super::file::DataFile::read_page`]: the
/// page this function writes is therefore one the pool has yet to cache. It is written under
/// the lock of the control block and not under the lock of the pool, beside the reads and
/// writes the pool issues at the same time; the file layer gives each of them its own offset.
/// The page a recycled
/// identifier names goes through the pool instead, which may hold the version [`free`]
/// dirtied.
fn grow_by_one_page(storage: &DiskStorage, id: PageId) -> Result<(), InternalError> {
    storage.data.extend_to(id.0 + 1)?;
    let mut page = Page::empty(PageKind::Free, id);
    page.set_lower(FREE_PAGE_LOWER);
    set_next_free(&mut page, None);
    page.seal();
    storage.data.write_page(id, &page)
}

/// The identifier the free page `page` links to, `None` for the page that ends the list.
fn next_free(page: &Page) -> Option<PageId> {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&page.0[OFF_NEXT_FREE..OFF_NEXT_FREE + 8]);
    match u64::from_le_bytes(raw) {
        END_OF_FREE_LIST => None,
        next => Some(PageId(next)),
    }
}

/// Writes the link of a free page: `next`, or [`END_OF_FREE_LIST`] to end the list.
fn set_next_free(page: &mut Page, next: Option<PageId>) {
    let raw = next.map_or(END_OF_FREE_LIST, |next| next.0);
    page.0[OFF_NEXT_FREE..OFF_NEXT_FREE + 8].copy_from_slice(&raw.to_le_bytes());
}

#[cfg(test)]
pub(crate) mod tests;
