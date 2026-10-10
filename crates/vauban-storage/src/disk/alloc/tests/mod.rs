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

/// A `data` holding two pages past the counter of the ctl reopens to an allocator that grows
/// the file beyond them.
///
/// The state is the one a stopped process leaves when `data` grew and `vauban.ctl` did not
/// follow: here the ctl is put back to the copy taken at two pages, after two more were
/// allocated. Without the count at the open, the next `allocate` asks `data` to shrink from
/// four pages to three and is refused, and so is each one after it.
#[test]
fn a_reopen_counts_the_pages_data_holds_past_the_counter() {
    let dir = TempDir::created("pages-past-counter");
    let behind = {
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        allocate(&storage).expect("allocate page 0");
        allocate(&storage).expect("allocate page 1");
        let behind = control_of(&storage);
        allocate(&storage).expect("allocate page 2");
        allocate(&storage).expect("allocate page 3");
        behind
    };
    behind
        .write(&dir.child(CONTROL_FILE_NAME))
        .expect("put back the ctl of two pages");
    assert_eq!(data_len(&dir), 4 * 8192, "data kept its four pages");

    let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    assert_eq!(control_of(&reopened).next_page_id, PageId(4));
    assert_eq!(
        Control::read(&dir.child(CONTROL_FILE_NAME))
            .expect("read the ctl")
            .next_page_id,
        PageId(4),
        "the counted pages reached the ctl"
    );
    assert_eq!(
        allocate(&reopened).expect("allocate after the reopen"),
        PageId(4),
        "the allocator grows the file past the pages it holds"
    );
    assert_eq!(data_len(&dir), 5 * 8192);
    assert_eq!(
        free_list(&reopened).expect("walk the free list"),
        Vec::new()
    );
}

/// A counter ahead of `data` is left where the ctl puts it: the count only raises it.
#[test]
fn a_reopen_leaves_a_counter_ahead_of_data_alone() {
    let dir = TempDir::created("counter-ahead");
    drop(DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance"));
    Control {
        next_page_id: PageId(3),
        ..Control::default()
    }
    .write(&dir.child(CONTROL_FILE_NAME))
    .expect("forge a ctl three pages ahead of an empty data");

    let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    assert_eq!(control_of(&reopened).next_page_id, PageId(3));
    assert_eq!(data_len(&dir), 0);
}
