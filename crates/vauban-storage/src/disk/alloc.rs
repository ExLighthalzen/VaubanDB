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
//! chained before it
//! writes the control file, so `data` holds the link the ctl is about to name
//! (`free_clears_in_progress_so_the_link_reaches_data`).
//!
//! A crash between the growth of `data` and the write of the control file leaves a page in the
//! file that `next_page_id` does not count: the next [`allocate`] hands out that identifier
//! once more and writes the page over it. Losing a page in flight this way is accepted: the
//! journal does not cover allocations, and a page allocated but not yet linked to a table is
//! an orphan of the DDL protocol.
//!
//! # Locking
//!
//! An allocation takes the lock of the control block ([`DiskStorage::lock_control`]) and holds
//! it across the pin it needs, so an allocation under way is not raced by another: the second
//! waits for that lock before it reads `next_page_id`. The order is control block, then buffer
//! pool.

use vauban_errors::InternalError;

use super::DiskStorage;
use super::page::{HEADER_SIZE, Lsn, Page, PageId, PageKind};

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

/// Puts the page `id` back in the free list, at its head.
///
/// The page is rewritten as a [`PageKind::Free`] one carrying the old head as its link, its
/// `in_progress` flag is cleared, and it is written to `data` before `vauban.ctl` names it as
/// the head. The LSN passed to [`super::buffer::BufferPool::mark_dirty`] is `Lsn(0)`: the
/// journal has no record for a `free`, nor for the DDL that calls this.
///
/// # Errors
///
/// [`InternalError::Bug`] when `id` was not handed out by [`allocate`] — that is when it is
/// `next_page_id` or past it — and when the page carries a pin: rewriting it asks for the
/// exclusive pin [`super::buffer::PinGuard::with_page_mut`] answers for, so a page a caller
/// still holds is refused (`free_pinned_is_bug` keeps one pin and reads "2 pins"). The
/// control block is left as it was in both cases.
///
/// [`InternalError::Bug`] as well when the page carries an LSN the journal has not made
/// durable: writing it would put the page ahead of the log, the rule of
/// [`super::buffer::BufferPool::flush`]. In this build the pool reads its durable LSN from
/// [`super::buffer::AlwaysZero`], so that case is a page a caller marked at an LSN other than
/// 0.
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
    let pin = storage.pool.pin(id)?;
    pin.with_page_mut(|page| {
        page.set_kind(PageKind::Free);
        page.set_slot_count(0);
        page.set_lower(FREE_PAGE_LOWER);
        set_next_free(page, head);
    })?;
    storage.pool.mark_dirty(id, Lsn(0))?;
    storage.pool.set_in_progress(id, false)?;
    drop(pin);
    storage.pool.flush(id)?;

    updated.free_head = Some(id);
    updated.write(&storage.control_path())?;
    *control = updated;
    Ok(())
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
/// page this function writes is therefore one the pool has yet to cache. The page a recycled
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
mod tests {
    use super::super::control::Control;
    use super::super::temp::TempDir;
    use super::super::{CONTROL_FILE_NAME, DATA_FILE_NAME, DiskOptions};
    use super::*;

    /// An empty instance in a temporary directory, with the guard that removes it.
    fn instance(label: &str) -> (TempDir, DiskStorage) {
        let dir = TempDir::created(label);
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        (dir, storage)
    }

    /// Size of the data file of the instance, in bytes.
    fn data_len(dir: &TempDir) -> u64 {
        std::fs::metadata(dir.child(DATA_FILE_NAME))
            .expect("the data file exists")
            .len()
    }

    /// The control block of the instance, read through its lock.
    fn control_of(storage: &DiskStorage) -> Control {
        storage.control().expect("read the control block")
    }

    #[test]
    fn allocate_extends_file() {
        let (dir, storage) = instance("alloc-extends");
        assert_eq!(data_len(&dir), 0, "a fresh instance holds no page");

        assert_eq!(
            allocate(&storage).expect("allocate the first page"),
            PageId(0)
        );
        assert_eq!(data_len(&dir), 8192);
        assert_eq!(storage.data.page_count().expect("page count"), 1);
        assert_eq!(control_of(&storage).next_page_id, PageId(1));

        assert_eq!(
            allocate(&storage).expect("allocate a second page"),
            PageId(1)
        );
        assert_eq!(data_len(&dir), 16384);
        assert_eq!(storage.data.page_count().expect("page count"), 2);
        assert_eq!(control_of(&storage).next_page_id, PageId(2));
        assert_eq!(control_of(&storage).free_head, None);
    }

    /// The page handed out is a sealed free page, which is what lets a caller pin it.
    ///
    /// A page of zeros would fail the magic of [`Page::from_bytes`], so this reads the two
    /// pages back through the file layer, which checks the header and the checksum, and then
    /// through the pool, which is the path a caller takes.
    #[test]
    fn allocate_writes_a_sealed_free_page() {
        let (_dir, storage) = instance("alloc-sealed");
        for expected in [PageId(0), PageId(1)] {
            let id = allocate(&storage).expect("allocate a page");
            assert_eq!(id, expected);
            let page = storage.data.read_page(id).expect("the page reads back");
            assert!(page.verify().is_ok(), "the allocator sealed the page");
            assert_eq!(
                page.kind().expect("the kind of a fresh page"),
                PageKind::Free
            );
            assert_eq!(page.page_id(), id);
            assert_eq!(page.slot_count(), 0);
            assert_eq!(
                page.lower(),
                40,
                "the header and the eight bytes of the link"
            );
            assert_eq!(
                next_free(&page),
                None,
                "a fresh page ends the list it heads"
            );
            assert_eq!(page.lsn(), Lsn(0));
            storage
                .pool
                .pin(id)
                .expect("a caller pins what it was given");
        }
    }

    /// The allocator hands out the page without a pin.
    ///
    /// [`super::super::buffer::PinGuard::with_page_mut`] asks for an exclusive pin, so it
    /// succeeds here because the pin the caller takes is the first one: an `allocate` that
    /// pinned would leave two and the call would answer [`InternalError::Bug`]. Making
    /// [`allocate`] end on a pin it leaks turns this test red, and with it the tests of this
    /// file that free a page they allocated, since a leaked pin also refuses the rewrite of
    /// [`free`].
    #[test]
    fn allocate_leaves_the_page_unpinned() {
        let (_dir, storage) = instance("alloc-unpinned");
        let id = allocate(&storage).expect("allocate a page");
        let pin = storage.pool.pin(id).expect("pin the page just handed out");
        pin.with_page_mut(|page| page.set_kind(PageKind::Heap))
            .expect("the caller holds an exclusive pin on the page");
    }

    #[test]
    fn free_then_allocate_reuses() {
        let (dir, storage) = instance("free-reuse");
        assert_eq!(allocate(&storage).expect("allocate page 0"), PageId(0));
        assert_eq!(allocate(&storage).expect("allocate page 1"), PageId(1));
        assert_eq!(data_len(&dir), 16384);

        free(&storage, PageId(0)).expect("free page 0");
        assert_eq!(control_of(&storage).free_head, Some(PageId(0)));

        assert_eq!(
            allocate(&storage).expect("allocate after the free"),
            PageId(0),
            "the head of the free list is handed out, not PageId(2)"
        );
        assert_eq!(control_of(&storage).free_head, None);
        assert_eq!(control_of(&storage).next_page_id, PageId(2));
        assert_eq!(data_len(&dir), 16384, "recycling does not grow the file");

        // The list is empty again, so the page after it comes from the end of the file.
        assert_eq!(
            allocate(&storage).expect("allocate a fresh page"),
            PageId(2)
        );
        assert_eq!(data_len(&dir), 24576);
    }

    /// Two frees then two allocations: the list is walked from its head, last freed first.
    ///
    /// The order is what this test asserts: taking the tail instead would answer `PageId(0)`
    /// first. The link of page 1 is read from `data` rather than from the cache, so the chain
    /// it asserts is the one a reopen would find.
    #[test]
    fn free_then_allocate_walks_the_list_from_its_head() {
        let (_dir, storage) = instance("free-lifo");
        for expected in [PageId(0), PageId(1)] {
            assert_eq!(allocate(&storage).expect("allocate a page"), expected);
        }
        free(&storage, PageId(0)).expect("free page 0");
        free(&storage, PageId(1)).expect("free page 1");
        assert_eq!(control_of(&storage).free_head, Some(PageId(1)));

        let chained = storage
            .data
            .read_page(PageId(1))
            .expect("page 1 reads back");
        assert_eq!(
            next_free(&chained),
            Some(PageId(0)),
            "page 1 links to the head it replaced"
        );
        let tail = storage
            .data
            .read_page(PageId(0))
            .expect("page 0 reads back");
        assert_eq!(next_free(&tail), None, "page 0 ends the list");

        assert_eq!(allocate(&storage).expect("allocate"), PageId(1));
        assert_eq!(control_of(&storage).free_head, Some(PageId(0)));
        assert_eq!(allocate(&storage).expect("allocate"), PageId(0));
        assert_eq!(control_of(&storage).free_head, None);
        assert_eq!(allocate(&storage).expect("allocate"), PageId(2));
    }

    #[test]
    fn counters_survive_reopen() {
        let dir = TempDir::created("counters-reopen");
        {
            let storage =
                DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
            assert_eq!(allocate(&storage).expect("allocate page 0"), PageId(0));
            assert_eq!(allocate(&storage).expect("allocate page 1"), PageId(1));
        }

        let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        assert_eq!(control_of(&reopened).next_page_id, PageId(2));
        assert_eq!(control_of(&reopened).free_head, None);
        assert_eq!(
            allocate(&reopened).expect("allocate after the reopen"),
            PageId(2),
            "the counter of the ctl is where the allocator resumes"
        );
        assert_eq!(data_len(&dir), 24576);
    }

    /// The head of the free list survives a reopen too, in the ctl and in the page.
    ///
    /// `counters_survive_reopen` cannot show it: a list that was thrown away reads as the
    /// empty one a fresh instance has, and `next_page_id` would answer 2 either way. Here the
    /// reopened instance hands out the freed page instead of the end of the file.
    #[test]
    fn the_free_list_survives_a_reopen() {
        let dir = TempDir::created("free-list-reopen");
        {
            let storage =
                DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
            allocate(&storage).expect("allocate page 0");
            allocate(&storage).expect("allocate page 1");
            free(&storage, PageId(0)).expect("free page 0");
        }

        let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        assert_eq!(control_of(&reopened).free_head, Some(PageId(0)));
        assert_eq!(
            allocate(&reopened).expect("allocate after the reopen"),
            PageId(0)
        );
        assert_eq!(control_of(&reopened).free_head, None);
        assert_eq!(data_len(&dir), 16384, "the file did not grow");
    }

    #[test]
    fn free_pinned_is_bug() {
        let (_dir, storage) = instance("free-pinned");
        let id = allocate(&storage).expect("allocate a page");
        let pin = storage.pool.pin(id).expect("pin the page and keep it");

        let err = free(&storage, id).expect_err("a pinned page is not put back in the list");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("2 pins"), "{err}");
        assert_eq!(
            control_of(&storage).free_head,
            None,
            "the refused free left the control block as it was"
        );

        // The pin is what refused: dropped, the call that just failed succeeds.
        drop(pin);
        free(&storage, id).expect("free the page once no caller holds it");
        assert_eq!(control_of(&storage).free_head, Some(id));
    }

    #[test]
    fn free_of_a_page_never_handed_out_is_bug() {
        let (dir, storage) = instance("free-unallocated");
        let err = free(&storage, PageId(0)).expect_err("the instance has handed out no page");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("page 0"), "{err}");
        assert_eq!(data_len(&dir), 0, "the refused free grew nothing");

        assert_eq!(allocate(&storage).expect("allocate page 0"), PageId(0));
        free(&storage, PageId(0)).expect("page 0 was handed out, so it is freed");

        // `next_page_id` is 1: the identifier itself is past what was handed out.
        let err = free(&storage, PageId(1)).expect_err("page 1 was not handed out");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        let err = free(&storage, PageId(END_OF_FREE_LIST))
            .expect_err("the sentinel was not handed out either");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert_eq!(control_of(&storage).free_head, Some(PageId(0)));
    }

    /// `free` clears `in_progress` and flushes, so the link reaches `data` instead of stopping
    /// in the cache.
    ///
    /// Page 1 is dirtied by a transaction the pool believes to be running, which is what
    /// [`super::super::buffer::BufferPool::flush`] skips. Dropping the `set_in_progress` call
    /// of [`free`] leaves page 1 in `data` with the link of a fresh page and turns this test
    /// red.
    #[test]
    fn free_clears_in_progress_so_the_link_reaches_data() {
        let (_dir, storage) = instance("free-in-progress");
        assert_eq!(allocate(&storage).expect("allocate page 0"), PageId(0));
        assert_eq!(allocate(&storage).expect("allocate page 1"), PageId(1));
        free(&storage, PageId(0)).expect("free page 0");
        {
            let pin = storage.pool.pin(PageId(1)).expect("pin page 1");
            pin.with_page_mut(|page| page.set_kind(PageKind::Heap))
                .expect("a transaction writes its own kind");
            storage
                .pool
                .mark_dirty(PageId(1), Lsn(0))
                .expect("mark page 1");
            storage
                .pool
                .set_in_progress(PageId(1), true)
                .expect("its transaction has not finished");
        }

        free(&storage, PageId(1)).expect("free page 1");
        let on_disk = storage
            .data
            .read_page(PageId(1))
            .expect("page 1 reads back");
        assert_eq!(
            on_disk.kind().expect("the kind of a freed page"),
            PageKind::Free
        );
        assert_eq!(
            next_free(&on_disk),
            Some(PageId(0)),
            "the link sits in `data`, which is where a reopen reads it"
        );
        assert_eq!(on_disk.slot_count(), 0);
        assert_eq!(on_disk.lower(), 40);
    }

    /// A `free_head` naming a page of another kind is corruption, not a page handed out.
    ///
    /// The ctl is forged so that its head names a sealed heap page: the chain of a live
    /// structure would otherwise be handed out as free space.
    #[test]
    fn allocate_refuses_a_head_that_is_not_a_free_page() {
        let dir = TempDir::created("head-not-free");
        {
            let storage =
                DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
            allocate(&storage).expect("allocate page 0");
            let mut page = Page::empty(PageKind::Heap, PageId(0));
            page.seal();
            storage
                .data
                .write_page(PageId(0), &page)
                .expect("write a heap page over page 0");
        }
        Control {
            next_page_id: PageId(1),
            free_head: Some(PageId(0)),
            ..Control::default()
        }
        .write(&dir.child(CONTROL_FILE_NAME))
        .expect("forge a ctl whose free list heads at page 0");

        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let err = allocate(&storage).expect_err("the head of the list is a heap page");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("head 0"), "{err}");
        assert!(err.to_string().contains("Heap"), "{err}");
        assert_eq!(
            control_of(&storage).free_head,
            Some(PageId(0)),
            "the refused allocation left the control block as it was"
        );
    }

    /// `u64::MAX` is the sentinel of the free list, so the allocator stops before handing it
    /// out; the file is not grown by the refused call.
    #[test]
    fn allocate_refuses_to_hand_out_the_sentinel() {
        let dir = TempDir::created("sentinel");
        drop(DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance"));
        Control {
            next_page_id: PageId(END_OF_FREE_LIST),
            ..Control::default()
        }
        .write(&dir.child(CONTROL_FILE_NAME))
        .expect("forge a ctl at the last identifier a u64 holds");

        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        let err = allocate(&storage).expect_err("the sentinel is not a page");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
        assert!(err.to_string().contains("sentinel"), "{err}");
        assert_eq!(data_len(&dir), 0, "the refused allocation grew nothing");
        assert_eq!(
            control_of(&storage).next_page_id,
            PageId(END_OF_FREE_LIST),
            "the counter did not move"
        );
    }

    /// The link is written and read at the eight bytes right after the header.
    ///
    /// The offset is part of the on-disk format, so it is asserted as a literal rather than
    /// through [`OFF_NEXT_FREE`] alone: a link moved elsewhere in the payload would still
    /// round-trip through [`next_free`] and [`set_next_free`].
    #[test]
    fn the_link_of_a_free_page_sits_at_bytes_32_to_40() {
        assert_eq!(OFF_NEXT_FREE, 32);
        assert_eq!(FREE_PAGE_LOWER, 40);
        assert_eq!(END_OF_FREE_LIST, u64::MAX);

        let mut page = Page::empty(PageKind::Free, PageId(4));
        assert_eq!(
            next_free(&page),
            Some(PageId(0)),
            "the zero bytes of a page read as a link to page 0"
        );
        set_next_free(&mut page, Some(PageId(0x0102_0304_0506_0708)));
        assert_eq!(
            &page.0[32..40],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
            "little-endian, low byte first"
        );
        assert_eq!(next_free(&page), Some(PageId(0x0102_0304_0506_0708)));

        set_next_free(&mut page, None);
        assert_eq!(&page.0[32..40], &[0xFF; 8]);
        assert_eq!(next_free(&page), None);
        // The bytes after the link are left alone by the two helpers.
        assert!(page.0[40..].iter().all(|&byte| byte == 0));
    }
}
