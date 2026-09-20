use super::super::page::{HEADER_SIZE, PageKind};
use super::super::temp::TempDir;
use super::*;

/// Where the tests write their marker, the first byte after the common header.
const MARK: usize = HEADER_SIZE;

/// A sealed empty page carrying `id` in its header.
fn sealed(id: PageId) -> Page {
    let mut page = Page::empty(PageKind::Heap, id);
    page.seal();
    page
}

/// A sealed page carrying `id` and `mark` right after its header.
fn marked(id: PageId, mark: &[u8]) -> Page {
    let mut page = Page::empty(PageKind::Heap, id);
    page.0[MARK..MARK + mark.len()].copy_from_slice(mark);
    page.seal();
    page
}

/// Writes `mark` right after the header of the pinned page.
fn write_mark(pin: &PinGuard<'_>, mark: &[u8]) {
    pin.with_page_mut(|page| page.0[MARK..MARK + mark.len()].copy_from_slice(mark))
        .expect("change the pinned page");
}

/// The bytes right after the header of the page at `id` in the file.
fn mark_on_disk(file: &DataFile, id: PageId, len: usize) -> Vec<u8> {
    let page = file
        .read_page(id)
        .unwrap_or_else(|err| panic!("page {id} should read back: {err}"));
    page.0[MARK..MARK + len].to_vec()
}

/// A data file of `pages` sealed empty pages and a pool of `frames` frames over it.
///
/// The durable LSN is a [`Fixed`], so a test chooses on which side of the write-ahead
/// rule its pages sit: `Lsn(u64::MAX)` puts none of them ahead of the log.
fn fixture(
    label: &str,
    pages: u64,
    frames: usize,
    durable: Lsn,
) -> (TempDir, Arc<DataFile>, BufferPool) {
    let (dir, file) = data_file(label, pages);
    let pool = BufferPool::with_durable_lsn(Arc::clone(&file), frames, Arc::new(Fixed(durable)))
        .expect("build the pool");
    (dir, file, pool)
}

/// A data file of `pages` sealed empty pages, each carrying its own identifier.
fn data_file(label: &str, pages: u64) -> (TempDir, Arc<DataFile>) {
    let dir = TempDir::created(label);
    let file = Arc::new(
        DataFile::create(&dir.child(super::super::DATA_FILE_NAME)).expect("create the data file"),
    );
    file.extend_to(pages).expect("extend the data file");
    for index in 0..pages {
        let id = PageId(index);
        file.write_page(id, &sealed(id))
            .expect("write an empty page");
    }
    (dir, file)
}

#[test]
fn new_rejects_a_pool_of_zero_frames() {
    let (_dir, file) = data_file("zero-frames", 1);
    let err = BufferPool::with_durable_lsn(Arc::clone(&file), 0, Arc::new(Fixed(Lsn(0))))
        .expect_err("0 frames is a bug");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("0 frames"), "{err}");
    assert!(validate_buffer_pages(0).is_err());
    assert!(validate_buffer_pages(1).is_ok());
    assert_eq!(
        BufferPool::with_durable_lsn(file, 3, Arc::new(Fixed(Lsn(0))))
            .expect("3 frames is a pool")
            .capacity(),
        3
    );
}

#[test]
fn pin_miss_loads_from_file() {
    let (_dir, file, pool) = fixture("pin-miss", 1, 4, Lsn(0));
    let written = marked(PageId(0), b"first");
    file.write_page(PageId(0), &written).expect("write page 0");

    let pin = pool.pin(PageId(0)).expect("pin page 0");
    assert_eq!(pin.page_id(), PageId(0));
    let cached = pin
        .with_page(|page| page.clone())
        .expect("read the cached page");
    assert_eq!(cached, written);
    assert_eq!(&cached.0[MARK..MARK + 5], b"first");
}

#[test]
fn pin_hit_does_not_reread() {
    let (_dir, file, pool) = fixture("pin-hit", 1, 4, Lsn(0));
    file.write_page(PageId(0), &marked(PageId(0), b"old"))
        .expect("write the first version");

    let first = pool.pin(PageId(0)).expect("pin page 0");
    assert_eq!(
        first
            .with_page(|page| page.0[MARK..MARK + 3].to_vec())
            .expect("read the cached page"),
        b"old"
    );
    drop(first);

    // Changed under the cache, without going through the pool.
    file.write_page(PageId(0), &marked(PageId(0), b"new"))
        .expect("write the second version");
    assert_eq!(mark_on_disk(&file, PageId(0), 3), b"new");

    let second = pool.pin(PageId(0)).expect("pin page 0 again");
    assert_eq!(
        second
            .with_page(|page| page.0[MARK..MARK + 3].to_vec())
            .expect("read the cached page"),
        b"old",
        "the second pin is served by the cache, not by the file"
    );
}

#[test]
fn evict_writes_dirty_committed_page() {
    // One frame, two pages: pinning B has to evict A.
    let (_dir, file, pool) = fixture("evict-dirty", 2, 1, Lsn(u64::MAX));
    {
        let a = pool.pin(PageId(0)).expect("pin page 0");
        write_mark(&a, b"changed");
        pool.mark_dirty(PageId(0), Lsn(4)).expect("mark page 0");
        pool.set_in_progress(PageId(0), false)
            .expect("page 0 belongs to a finished transaction");
    }
    // Dropping the guard writes nothing.
    assert_eq!(mark_on_disk(&file, PageId(0), 7), b"\0\0\0\0\0\0\0");

    let b = pool.pin(PageId(1)).expect("pin page 1, evicting page 0");
    assert_eq!(b.page_id(), PageId(1));
    let evicted = file.read_page(PageId(0)).expect("page 0 reads back");
    assert_eq!(&evicted.0[MARK..MARK + 7], b"changed");
    assert_eq!(evicted.lsn(), Lsn(4));
    // The page was sealed by the pool: `read_page` checks the checksum.
    assert!(evicted.verify().is_ok());
}

#[test]
fn no_steal_skips_in_progress() {
    let (_dir, file, pool) = fixture("no-steal", 2, 1, Lsn(u64::MAX));
    {
        let a = pool.pin(PageId(0)).expect("pin page 0");
        write_mark(&a, b"running");
        pool.mark_dirty(PageId(0), Lsn(7)).expect("mark page 0");
        pool.set_in_progress(PageId(0), true)
            .expect("page 0 belongs to a running transaction");
    }

    let err = pool
        .pin(PageId(1))
        .expect_err("the only frame holds a dirty page of a running transaction");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("exhausted"), "{err}");
    assert!(
        err.to_string().contains("dirty and in progress: 1"),
        "{err}"
    );

    let on_disk = file.read_page(PageId(0)).expect("page 0 reads back");
    assert_eq!(&on_disk.0[MARK..MARK + 7], b"\0\0\0\0\0\0\0");
    assert_eq!(on_disk.lsn(), Lsn(0));

    // The flag is what held the page: cleared, the same pin succeeds and writes it.
    pool.set_in_progress(PageId(0), false)
        .expect("the transaction of page 0 finished");
    pool.pin(PageId(1)).expect("page 0 may now be evicted");
    assert_eq!(mark_on_disk(&file, PageId(0), 7), b"running");
}

#[test]
fn flush_all_skips_in_progress() {
    let (_dir, file, pool) = fixture("flush-all", 2, 4, Lsn(u64::MAX));
    {
        let a = pool.pin(PageId(0)).expect("pin page 0");
        write_mark(&a, b"running");
        pool.mark_dirty(PageId(0), Lsn(3)).expect("mark page 0");
        pool.set_in_progress(PageId(0), true)
            .expect("page 0 belongs to a running transaction");

        let b = pool.pin(PageId(1)).expect("pin page 1");
        write_mark(&b, b"settled");
        pool.mark_dirty(PageId(1), Lsn(4)).expect("mark page 1");
    }

    pool.flush_all().expect("flush every writable page");
    assert_eq!(mark_on_disk(&file, PageId(1), 7), b"settled");
    assert_eq!(mark_on_disk(&file, PageId(0), 7), b"\0\0\0\0\0\0\0");

    // The flag is what held page 0 back, not its contents.
    pool.set_in_progress(PageId(0), false)
        .expect("the transaction of page 0 finished");
    pool.flush_all().expect("flush page 0 as well");
    assert_eq!(mark_on_disk(&file, PageId(0), 7), b"running");
}

#[test]
fn flush_respects_durable_lsn() {
    let (_dir, file, pool) = fixture("durable-lsn", 2, 4, Lsn(10));
    {
        let ahead = pool.pin(PageId(0)).expect("pin page 0");
        write_mark(&ahead, b"ahead");
        pool.mark_dirty(PageId(0), Lsn(11))
            .expect("mark page 0 at LSN 11");
    }
    let err = pool
        .flush(PageId(0))
        .expect_err("LSN 11 is past the durable 10");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("LSN 11"), "{err}");
    assert!(err.to_string().contains("durable up to 10"), "{err}");
    assert_eq!(mark_on_disk(&file, PageId(0), 5), b"\0\0\0\0\0");

    {
        let at_horizon = pool.pin(PageId(1)).expect("pin page 1");
        write_mark(&at_horizon, b"kept!");
        pool.mark_dirty(PageId(1), Lsn(10))
            .expect("mark page 1 at LSN 10");
    }
    pool.flush(PageId(1)).expect("LSN 10 is durable");
    let written = file.read_page(PageId(1)).expect("page 1 reads back");
    assert_eq!(&written.0[MARK..MARK + 5], b"kept!");
    assert_eq!(written.lsn(), Lsn(10));
}

#[test]
fn a_journal_with_nothing_flushed_lets_out_the_pages_no_record_describes() {
    let (_dir, file, pool) = fixture("durable-zero", 2, 4, Lsn(0));
    {
        let unlogged = pool.pin(PageId(0)).expect("pin page 0");
        write_mark(&unlogged, b"unlogged");
        pool.mark_dirty(PageId(0), Lsn(0))
            .expect("mark page 0, still at LSN 0");

        let logged = pool.pin(PageId(1)).expect("pin page 1");
        write_mark(&logged, b"logged");
        pool.mark_dirty(PageId(1), Lsn(1))
            .expect("mark page 1 at LSN 1");
    }

    pool.flush(PageId(0))
        .expect("a page at LSN 0 may be written");
    assert_eq!(mark_on_disk(&file, PageId(0), 8), b"unlogged");
    let err = pool
        .flush(PageId(1))
        .expect_err("a durable LSN of 0 covers no record");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("durable up to 0"), "{err}");
    assert_eq!(Fixed(Lsn(0)).durable_lsn(), Lsn(0));
    assert_eq!(Fixed(Lsn(12)).durable_lsn(), Lsn(12));
}

#[test]
fn a_page_the_journal_has_not_covered_is_not_a_victim_either() {
    // One frame, one dirty page of a finished transaction at LSN 11, durable 10: the
    // eviction path applies the same rule as `flush`, and says so.
    let (_dir, file, pool) = fixture("victim-ahead", 2, 1, Lsn(10));
    {
        let ahead = pool.pin(PageId(0)).expect("pin page 0");
        write_mark(&ahead, b"ahead");
        pool.mark_dirty(PageId(0), Lsn(11))
            .expect("mark page 0 at LSN 11");
    }
    let err = pool
        .pin(PageId(1))
        .expect_err("the only frame cannot be freed");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("exhausted"), "{err}");
    assert!(
        err.to_string()
            .contains("dirty and ahead of the durable LSN: 1"),
        "{err}"
    );
    assert_eq!(mark_on_disk(&file, PageId(0), 5), b"\0\0\0\0\0");
}

#[test]
fn a_pinned_frame_is_not_a_victim() {
    let (_dir, _file, pool) = fixture("victim-pinned", 2, 1, Lsn(u64::MAX));
    let held = pool.pin(PageId(0)).expect("pin page 0");
    let err = pool.pin(PageId(1)).expect_err("the only frame is pinned");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("pinned: 1"), "{err}");
    drop(held);
    pool.pin(PageId(1)).expect("the frame is free again");
}

#[test]
fn two_pages_are_pinned_at_once() {
    let (_dir, _file, pool) = fixture("two-pins", 2, 2, Lsn(u64::MAX));
    let a = pool.pin(PageId(0)).expect("pin page 0");
    let b = pool.pin(PageId(1)).expect("pin page 1");
    assert_eq!(
        a.with_page(|page| page.page_id()).expect("read page 0"),
        PageId(0)
    );
    assert_eq!(
        b.with_page(|page| page.page_id()).expect("read page 1"),
        PageId(1)
    );
    write_mark(&a, b"a");
    write_mark(&b, b"b");
}

#[test]
fn two_pins_of_one_page_are_counted_and_refuse_a_change() {
    let (_dir, _file, pool) = fixture("shared-pin", 2, 1, Lsn(u64::MAX));
    let first = pool.pin(PageId(0)).expect("pin page 0");
    let second = pool.pin(PageId(0)).expect("pin page 0 twice");
    let err = first
        .with_page_mut(|page| page.set_lsn(Lsn(1)))
        .expect_err("two pins on the page");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("2 pins"), "{err}");
    // Reading is allowed while the page is shared.
    first.with_page(|page| page.lsn()).expect("read page 0");
    second.with_page(|page| page.lsn()).expect("read page 0");

    drop(second);
    first
        .with_page_mut(|page| page.set_lsn(Lsn(1)))
        .expect("the last pin may change the page");
    drop(first);
    // Both pins are gone, so the frame may be reused.
    pool.pin(PageId(1)).expect("pin page 1");
}

#[test]
fn mark_dirty_needs_a_pinned_page() {
    let (_dir, file, pool) = fixture("mark-dirty", 1, 4, Lsn(u64::MAX));
    let err = pool
        .mark_dirty(PageId(0), Lsn(1))
        .expect_err("page 0 is not in the pool");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("not in the buffer pool"), "{err}");

    let pin = pool.pin(PageId(0)).expect("pin page 0");
    write_mark(&pin, b"mark");
    drop(pin);
    let err = pool
        .mark_dirty(PageId(0), Lsn(1))
        .expect_err("page 0 is cached but no longer pinned");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("not pinned"), "{err}");

    // Nothing was marked, so `flush_all` writes nothing.
    pool.flush_all()
        .expect("flush a pool that holds no dirty page");
    assert_eq!(mark_on_disk(&file, PageId(0), 4), b"\0\0\0\0");
}

#[test]
fn mark_dirty_does_not_move_the_page_lsn_backwards() {
    let (_dir, file, pool) = fixture("lsn-backwards", 1, 4, Lsn(u64::MAX));
    {
        let pin = pool.pin(PageId(0)).expect("pin page 0");
        write_mark(&pin, b"kept");
        pool.mark_dirty(PageId(0), Lsn(5)).expect("mark at LSN 5");
        pool.mark_dirty(PageId(0), Lsn(2))
            .expect("mark again at LSN 2");
        assert_eq!(
            pin.with_page(|page| page.lsn()).expect("read the LSN"),
            Lsn(5)
        );
    }
    pool.flush(PageId(0)).expect("flush page 0");
    assert_eq!(
        file.read_page(PageId(0)).expect("page 0 reads back").lsn(),
        Lsn(5)
    );
}

#[test]
fn set_in_progress_needs_the_page_only_when_it_is_set() {
    let (_dir, _file, pool) = fixture("in-progress", 1, 4, Lsn(u64::MAX));
    let err = pool
        .set_in_progress(PageId(0), true)
        .expect_err("page 0 is not in the pool");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("needs it pinned"), "{err}");
    pool.set_in_progress(PageId(0), false)
        .expect("clearing the flag on a page the pool does not hold");
}

#[test]
fn flush_of_a_page_the_pool_does_not_hold_writes_nothing() {
    let (_dir, file, pool) = fixture("flush-absent", 1, 4, Lsn(u64::MAX));
    pool.flush(PageId(0)).expect("nothing is cached");
    pool.flush(PageId(9))
        .expect("page 9 is not even in the file");
    assert_eq!(mark_on_disk(&file, PageId(0), 4), b"\0\0\0\0");
}

#[test]
fn pin_refuses_a_page_whose_header_names_another_position() {
    let (_dir, file, pool) = fixture("misplaced", 2, 4, Lsn(0));
    // The file layer writes where it is told and does not renumber the page.
    file.write_page(PageId(1), &sealed(PageId(0)))
        .expect("write page 0 at position 1");
    let err = pool
        .pin(PageId(1))
        .expect_err("the page at position 1 says it is page 0");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("position 1"), "{err}");
    assert!(err.to_string().contains("identifier 0"), "{err}");
}

#[test]
fn pin_past_the_end_of_the_file_is_the_error_of_the_file_layer() {
    let (_dir, _file, pool) = fixture("pin-past-end", 1, 4, Lsn(0));
    let err = pool.pin(PageId(1)).expect_err("the file holds page 0 only");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("page 1"), "{err}");
}

#[test]
fn the_clock_walks_the_frames_it_may_evict() {
    // Two frames, three pages. Page 0 and page 1 are read, then page 2 evicts one of
    // them and page 3 the other: both come back from the file, so both were evicted.
    let (_dir, file, pool) = fixture("clock", 4, 2, Lsn(u64::MAX));
    for index in 0..2 {
        let pin = pool.pin(PageId(index)).expect("pin a page");
        write_mark(&pin, b"seen");
        pool.mark_dirty(PageId(index), Lsn(1))
            .expect("mark the page");
    }
    assert_eq!(mark_on_disk(&file, PageId(0), 4), b"\0\0\0\0");
    assert_eq!(mark_on_disk(&file, PageId(1), 4), b"\0\0\0\0");

    drop(pool.pin(PageId(2)).expect("pin page 2"));
    drop(pool.pin(PageId(3)).expect("pin page 3"));
    assert_eq!(mark_on_disk(&file, PageId(0), 4), b"seen");
    assert_eq!(mark_on_disk(&file, PageId(1), 4), b"seen");
}

#[test]
fn poisoned_lock_is_reported_as_corruption() {
    let (_dir, _file, pool) = fixture("poisoned", 1, 4, Lsn(u64::MAX));
    // Taken before the poison, so that the two entry points of a guard are reached too.
    let pin = pool.pin(PageId(0)).expect("pin page 0");
    let outcome = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _guard = pool.inner.lock().unwrap();
                panic!("poisoning the buffer pool lock on purpose");
            })
            .join()
    });
    assert!(outcome.is_err());

    let err = pool.pin(PageId(0)).expect_err("the lock is poisoned");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "data corruption: buffer pool lock poisoned"
    );
    assert!(pool.flush(PageId(0)).is_err());
    assert!(pool.flush_all().is_err());
    assert!(pool.mark_dirty(PageId(0), Lsn(1)).is_err());
    assert!(pool.set_in_progress(PageId(0), true).is_err());
    assert!(pin.with_page(|page| page.lsn()).is_err());
    assert!(pin.with_page_mut(|page| page.set_lsn(Lsn(1))).is_err());
    // Seven entry points, the number the rustdoc of `BufferPool` names.
    assert_eq!(pin.page_id(), PageId(0), "page_id takes no lock");
}

#[test]
fn buffer_pool_is_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<BufferPool>();
    assert::<Fixed>();
}
