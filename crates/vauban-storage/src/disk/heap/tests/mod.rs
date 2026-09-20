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
