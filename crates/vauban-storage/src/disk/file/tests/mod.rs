use super::super::page::{Lsn, PageKind};
use super::super::temp::TempDir;
use super::*;

/// A data file of `pages` pages in a temporary directory, with its guard.
fn data_file(label: &str, pages: u64) -> (TempDir, DataFile) {
    let dir = TempDir::created(label);
    let file = DataFile::create(&dir.child("data")).expect("create a data file");
    file.extend_to(pages).expect("extend the data file");
    (dir, file)
}

#[test]
fn write_then_read_page_roundtrip() {
    let (_dir, file) = data_file("page-roundtrip", 1);
    assert_eq!(file.page_count().expect("page count"), 1);

    let mut page = Page::empty(PageKind::Heap, PageId(0));
    page.set_lsn(Lsn(42));
    page.0[32..37].copy_from_slice(b"hello");
    page.seal();
    let sealed_checksum = page.checksum_field();
    file.write_page(PageId(0), &page).expect("write page 0");

    let read = file.read_page(PageId(0)).expect("read page 0 back");
    assert_eq!(
        read.kind().expect("the kind of the page read back"),
        PageKind::Heap
    );
    assert_eq!(read.page_id(), PageId(0));
    assert_eq!(read.lsn(), Lsn(42));
    assert_eq!(read.checksum_field(), sealed_checksum);
    assert!(
        read.verify().is_ok(),
        "the checksum agrees after the round-trip"
    );
    assert_eq!(&read.0[32..37], b"hello");
    assert_eq!(read, page);
}

#[test]
fn write_page_does_not_reseal_the_page() {
    let (_dir, file) = data_file("no-reseal", 1);
    let mut page = Page::empty(PageKind::Heap, PageId(0));
    page.seal();
    // Changed after the seal: the checksum no longer covers the bytes, and the file layer
    // writes what it is given.
    page.0[32] = 1;
    file.write_page(PageId(0), &page).expect("write page 0");
    let err = file
        .read_page(PageId(0))
        .expect_err("the page was changed after its seal");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("checksum"), "{err}");
}

#[test]
fn read_page_past_the_end_of_the_file_is_corruption() {
    let (_dir, file) = data_file("read-past-end", 1);
    let err = file
        .read_page(PageId(1))
        .expect_err("the file holds page 0 only");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("page 1"), "{err}");
    assert!(!err.to_string().contains("unexpected end"), "{err}");
}

#[test]
fn write_page_past_the_end_of_the_file_is_a_bug() {
    let (_dir, file) = data_file("write-past-end", 0);
    let mut page = Page::empty(PageKind::Heap, PageId(0));
    page.seal();
    let err = file
        .write_page(PageId(0), &page)
        .expect_err("the file holds no page");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("page 0"), "{err}");
}

#[test]
fn extend_to_grows_by_whole_pages_and_refuses_to_shrink() {
    let (dir, file) = data_file("extend", 0);
    assert_eq!(file.page_count().expect("page count"), 0);
    assert_eq!(
        std::fs::metadata(dir.child("data"))
            .expect("the data file exists")
            .len(),
        0
    );
    file.extend_to(3).expect("extend to 3 pages");
    assert_eq!(file.page_count().expect("page count"), 3);
    assert_eq!(
        std::fs::metadata(dir.child("data"))
            .expect("the data file exists")
            .len(),
        3 * PAGE_SIZE as u64
    );
    file.extend_to(3)
        .expect("extend to the size it already has");
    let err = file.extend_to(2).expect_err("shrinking is refused");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert_eq!(file.page_count().expect("page count"), 3);
}

#[test]
fn a_page_written_at_another_position_keeps_its_own_identifier() {
    // The file layer writes where it is told; it does not renumber the page. The header
    // still carries `PageId(0)`, which is how the buffer pool catches a misplaced page.
    let (_dir, file) = data_file("misplaced", 2);
    let mut page = Page::empty(PageKind::Heap, PageId(0));
    page.seal();
    file.write_page(PageId(1), &page).expect("write at page 1");
    let read = file.read_page(PageId(1)).expect("read page 1 back");
    assert_eq!(read.page_id(), PageId(0));
}

#[test]
fn open_rejects_a_size_that_is_not_whole_pages() {
    let dir = TempDir::created("ragged");
    let path = dir.child("data");
    std::fs::write(&path, [0u8; 100]).expect("write a ragged data file");
    let err = DataFile::open(&path).expect_err("100 bytes is not a whole number of pages");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("100 bytes"), "{err}");

    std::fs::write(&path, [0u8; PAGE_SIZE]).expect("write one whole page");
    let file = DataFile::open(&path).expect("one whole page opens");
    assert_eq!(file.page_count().expect("page count"), 1);
    assert_eq!(file.path(), path);
}

#[test]
fn create_refuses_an_existing_file() {
    let dir = TempDir::created("create-twice");
    let path = dir.child("data");
    DataFile::create(&path).expect("create a data file");
    let err = DataFile::create(&path).expect_err("the file already exists");
    assert!(matches!(err, InternalError::Io(_)), "{err:?}");
}
