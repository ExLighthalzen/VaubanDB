use super::super::buffer::DurableLsn;
use super::super::control::Control;
use super::super::temp::TempDir;
use super::super::wal::{PageWrite, WalRecordKind};
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
fn a_freed_slot_ties_the_page_to_the_lsn_it_is_given() {
    let (_dir, storage) = instance("heap-delete-lsn");
    let heap = heap_of(&storage);
    let first = heap.insert(&payload(20, 1)).expect("insert the first row");
    let second = heap.insert(&payload(20, 2)).expect("insert the second row");
    assert_eq!(
        storage.wal.durable_lsn(),
        Lsn(0),
        "nothing is journalled yet"
    );

    // The flag a caller set before the delete stays: the page is held back.
    storage
        .pool
        .set_in_progress(first.page, true)
        .expect("flag the page");
    assert!(heap.free_slot(first, Lsn(5)).expect("delete under lsn 5"));
    let pin = storage.pool.pin(first.page).expect("pin the page");
    assert_eq!(pin.with_page(Page::lsn).expect("read the lsn"), Lsn(5));
    drop(pin);
    storage
        .pool
        .flush(first.page)
        .expect("a flagged page is skipped, not written");

    // Once the flag is down, the page carries a record the journal has not made durable, and
    // the pool refuses to write it.
    storage
        .pool
        .set_in_progress(first.page, false)
        .expect("lower the flag");
    let err = storage
        .pool
        .flush(first.page)
        .expect_err("the page is ahead of the journal");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");

    // `Lsn(0)` leaves the LSN of the page where it stands.
    assert!(heap.free_slot(second, Lsn(0)).expect("delete under lsn 0"));
    let pin = storage.pool.pin(second.page).expect("pin the page");
    assert_eq!(pin.with_page(Page::lsn).expect("read the lsn"), Lsn(5));
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

/// The `page_lsn` of the page `id`, read through the pool.
fn lsn_of(storage: &DiskStorage, id: PageId) -> Lsn {
    let pin = storage.pool.pin(id).expect("pin the page");
    pin.with_page(Page::lsn).expect("read the page")
}

#[test]
fn put_past_the_directory_adds_free_slots_up_to_its_slot() {
    let (_dir, storage) = instance("heap-put-past");
    let heap = heap_of(&storage);
    let rid = Rid {
        page: heap.first_page(),
        slot: 3,
    };
    assert!(heap.put(rid, &payload(20, 1), Lsn(0)).expect("put"));
    assert_eq!(heap.get(rid).expect("get"), Some(payload(20, 1)));
    assert_eq!(heap.scan().expect("scan"), vec![rid]);
    for slot in 0..3 {
        assert!(
            slot_of(&storage, Rid { slot, ..rid }).is_free(),
            "slot {slot}"
        );
    }
    // The next insert takes the first of the free slots.
    let next = heap.insert(&payload(8, 2)).expect("insert");
    assert_eq!(next.slot, 0);
}

#[test]
fn put_over_a_record_replaces_it() {
    let (_dir, storage) = instance("heap-put-over");
    let heap = heap_of(&storage);
    let rid = heap.insert(&payload(40, 1)).expect("insert");
    let other = heap.insert(&payload(10, 2)).expect("insert");
    assert!(heap.put(rid, &payload(12, 9), Lsn(0)).expect("put"));
    assert_eq!(heap.get(rid).expect("get"), Some(payload(12, 9)));
    assert_eq!(heap.get(other).expect("get"), Some(payload(10, 2)));
    assert_eq!(heap.scan().expect("scan"), vec![rid, other]);
}

#[test]
fn put_without_room_answers_false_and_leaves_the_page() {
    let (_dir, storage) = instance("heap-put-full");
    let heap = heap_of(&storage);
    let big = heap.insert(&payload(8000, 1)).expect("insert");
    let before = storage
        .pool
        .pin(big.page)
        .expect("pin")
        .with_page(Clone::clone)
        .expect("read");
    let rid = Rid {
        page: big.page,
        slot: 1,
    };
    assert!(!heap.put(rid, &payload(500, 2), Lsn(9)).expect("put"));
    let after = storage
        .pool
        .pin(big.page)
        .expect("pin")
        .with_page(Clone::clone)
        .expect("read");
    assert_eq!(after, before);
}

#[test]
fn reserve_answers_the_slot_insert_then_takes() {
    let (_dir, storage) = instance("heap-reserve");
    let heap = heap_of(&storage);
    let first = heap.insert(&payload(16, 1)).expect("insert");
    heap.insert(&payload(16, 2)).expect("insert");
    assert!(heap.delete(first).expect("delete"));
    let reserved = heap.reserve(16).expect("reserve");
    assert_eq!(reserved, first, "the free slot is reused");
    // Reserving writes nothing: the slot is still free.
    assert_eq!(heap.get(reserved).expect("get"), None);
    assert_eq!(heap.insert(&payload(16, 3)).expect("insert"), reserved);
    // A record past the inline limit is reserved the room of its stub.
    let long = heap.reserve(MAX_INLINE_LEN + 1).expect("reserve");
    assert_eq!(long.page, heap.first_page());
}

#[test]
fn put_and_overwrite_carry_the_lsn_to_the_page() {
    let (_dir, storage) = instance("heap-put-lsn");
    let heap = heap_of(&storage);
    let head = heap.first_page();
    let rid = heap.reserve(16).expect("reserve");
    assert!(heap.put(rid, &payload(16, 1), Lsn(7)).expect("put"));
    assert_eq!(lsn_of(&storage, head), Lsn(7));
    // `Lsn(0)` and a smaller number leave the page where it is.
    let other = heap.insert(&payload(16, 2)).expect("insert");
    assert!(
        heap.overwrite(other, 0, &[9, 9], Lsn(5))
            .expect("overwrite")
    );
    assert_eq!(lsn_of(&storage, head), Lsn(7));
    assert!(
        heap.overwrite(rid, 2, &[4, 4, 4], Lsn(8))
            .expect("overwrite")
    );
    assert_eq!(lsn_of(&storage, head), Lsn(8));
    let mut expected = payload(16, 1);
    expected[2..5].copy_from_slice(&[4, 4, 4]);
    assert_eq!(heap.get(rid).expect("get"), Some(expected));
    assert_eq!(heap.page_lsn(head).expect("page lsn"), Some(Lsn(8)));
}

#[test]
fn overwrite_of_an_overflow_record_changes_the_first_page_of_its_chain() {
    let (_dir, storage) = instance("heap-overwrite-overflow");
    let heap = heap_of(&storage);
    let row = payload(MAX_INLINE_LEN + 100, 4);
    let rid = heap.insert(&row).expect("insert a long row");
    assert!(heap.overwrite(rid, 16, &[1; 8], Lsn(3)).expect("overwrite"));
    let mut expected = row.clone();
    expected[16..24].fill(1);
    assert_eq!(heap.get(rid).expect("get"), Some(expected));
    // Past the first page of the chain is refused.
    let err = heap
        .overwrite(rid, OVERFLOW_CAPACITY, &[1], Lsn(3))
        .expect_err("past the first page");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    // A free slot answers false.
    let short = heap.insert(&payload(16, 5)).expect("insert");
    assert!(heap.delete(short).expect("delete"));
    assert!(!heap.overwrite(short, 0, &[1], Lsn(4)).expect("overwrite"));
}

/// The page records of the journal of `storage`, decoded, with their numbers.
fn page_writes(storage: &DiskStorage) -> Vec<(Lsn, PageWrite)> {
    storage
        .wal
        .records()
        .expect("read the journal")
        .into_iter()
        .filter(|record| record.kind == WalRecordKind::PageWrite)
        .map(|record| {
            let write = PageWrite::decode(&record.payload).expect("a page record reads back");
            (record.lsn, write)
        })
        .collect()
}

#[test]
fn a_page_the_heap_grows_by_is_formatted_and_linked_by_one_record() {
    let (_dir, storage) = instance("heap-grow-record");
    let heap = heap_of(&storage);
    let head = heap.first_page();
    while pages_of(&heap).len() < 2 {
        heap.insert(&payload(1000, 1)).expect("insert");
    }
    let fresh = pages_of(&heap)[1];
    let (lsn, write) = page_writes(&storage)
        .into_iter()
        .find(|(_, write)| write.pages.iter().any(|delta| delta.page == fresh))
        .expect("a record names the fresh page");
    let pages: Vec<PageId> = write.pages.iter().map(|delta| delta.page).collect();
    assert_eq!(
        pages,
        vec![fresh, head],
        "the fresh page first, then its link"
    );
    assert!(write.pages[0].whole, "the fresh page is logged whole");
    assert_eq!(write.pages[0].kind, PageKind::Heap.as_byte());
    // The link of the head page, bytes 34..42, is what the record changes there.
    let link: Vec<(u16, usize)> = write.pages[1]
        .ranges
        .iter()
        .map(|(at, bytes)| (*at, bytes.len()))
        .collect();
    assert_eq!(link, vec![(OFF_HEAP_NEXT as u16, 8)]);
    assert!(storage.wal.durable_lsn() >= lsn, "the growth is durable");
    assert!(lsn_of(&storage, fresh) >= lsn);
    assert!(lsn_of(&storage, head) >= lsn);
}

#[test]
fn a_long_record_is_journalled_as_its_chain_and_its_stub() {
    let (_dir, storage) = instance("heap-long-record");
    let heap = heap_of(&storage);
    let before = page_writes(&storage).len();
    let rid = heap.reserve(3 * OVERFLOW_CAPACITY).expect("reserve");
    let row = payload(3 * OVERFLOW_CAPACITY, 6);
    assert!(heap.put(rid, &row, Lsn(0)).expect("put a long record"));
    let written: Vec<(Lsn, PageWrite)> = page_writes(&storage).split_off(before);
    assert_eq!(written.len(), 2, "the chain, then the stub");
    let chain = &written[0].1;
    assert_eq!(chain.pages.len(), 3);
    assert!(
        chain
            .pages
            .iter()
            .all(|delta| delta.whole && delta.kind == PageKind::Overflow.as_byte())
    );
    let stub = &written[1].1;
    assert_eq!(stub.pages.len(), 1);
    assert_eq!(stub.pages[0].page, rid.page);
    assert!(!stub.pages[0].whole);
    assert!(
        storage.wal.durable_lsn() >= written[1].0,
        "synced before the answer"
    );
    assert_eq!(lsn_of(&storage, rid.page), written[1].0);
    assert_eq!(heap.get(rid).expect("get"), Some(row));
}

#[test]
fn an_overwrite_of_a_long_record_is_journalled_on_its_first_overflow_page() {
    let (_dir, storage) = instance("heap-overwrite-long-record");
    let heap = heap_of(&storage);
    let rid = heap
        .insert(&payload(MAX_INLINE_LEN + 100, 4))
        .expect("insert a long row");
    let stub_lsn = lsn_of(&storage, rid.page);
    let before = page_writes(&storage).len();
    assert!(heap.overwrite(rid, 16, &[1; 8], Lsn(3)).expect("overwrite"));
    let written: Vec<(Lsn, PageWrite)> = page_writes(&storage).split_off(before);
    assert_eq!(written.len(), 1);
    let (lsn, write) = &written[0];
    assert_eq!(write.pages.len(), 1);
    let head = write.pages[0].page;
    assert_ne!(head, rid.page, "the first page of the chain, not the stub");
    assert_eq!(
        write.pages[0].ranges,
        vec![(OVERFLOW_PAYLOAD_START + 16, vec![1; 8])]
    );
    assert_eq!(lsn_of(&storage, head), *lsn);
    assert_eq!(
        lsn_of(&storage, rid.page),
        stub_lsn,
        "the stub page is left alone"
    );
    assert!(storage.wal.durable_lsn() >= *lsn);
}
