//! The data file of an on-disk instance: an array of pages of [`PAGE_SIZE`] bytes, addressed
//! by [`PageId`].
//!
//! Reads and writes go through [`std::fs::File`], `seek` and `read_exact` / `write_all`; the
//! file is not mapped into memory, so that the decision of when a page sits in memory and
//! when it is written back belongs to the buffer pool rather than to the kernel.
//!
//! This layer moves 8 192-byte pages and checks their checksum; it does not grow the file by
//! itself and does not cache anything. The allocator ([`super::alloc`]) owns the growth, the
//! buffer pool ([`super::buffer`]) owns the caching.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use vauban_errors::InternalError;

use super::page::{PAGE_SIZE, Page, PageId};

/// The file that holds the pages of an instance, `<dir>/data`.
///
/// Its size is a multiple of [`PAGE_SIZE`]; the number of pages it holds is its size divided
/// by [`PAGE_SIZE`], which is 0 on a fresh instance. A size that is not a multiple is
/// [`InternalError::Corruption`] at open time.
///
/// The handle is shared behind `&self`, and a read is a `seek` followed by a `read_exact` on
/// that handle: the caller is responsible for not issuing two such pairs at once on the same
/// `DataFile`. That caller is the buffer pool ([`super::buffer`]), which serialises page I/O.
#[derive(Debug)]
pub(crate) struct DataFile {
    /// Path of the file, kept for the error messages.
    path: PathBuf,
    /// Open handle, readable and writable.
    file: std::fs::File,
}

impl DataFile {
    /// Creates the file at `path`, empty, and fails if it already exists.
    ///
    /// Called on the creation path of [`super::DiskStorage::open`], which has already checked
    /// that the directory holds no instance.
    pub(crate) fn create(path: &Path) -> Result<Self, InternalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    /// Opens the existing file at `path` for reading and writing.
    ///
    /// A size that is not a multiple of [`PAGE_SIZE`] is [`InternalError::Corruption`] naming
    /// the size that was read: a page has been truncated, and this layer does not repair.
    pub(crate) fn open(path: &Path) -> Result<Self, InternalError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        if size % PAGE_SIZE as u64 != 0 {
            return Err(InternalError::Corruption(format!(
                "data file {} is {size} bytes, not a multiple of {PAGE_SIZE}",
                path.display()
            )));
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    /// Path of the file.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Number of pages the file holds, that is its size divided by [`PAGE_SIZE`].
    pub(crate) fn page_count(&self) -> Result<u64, InternalError> {
        Ok(self.file.metadata()?.len() / PAGE_SIZE as u64)
    }

    /// Grows the file to `pages` pages of zero bytes.
    ///
    /// The zero bytes are not valid pages: the caller writes a page over them before reading
    /// them back. Growing the file for a real allocation is the allocator's business; here this
    /// is the raw `set_len` it calls, and what the tests use to obtain a page to write on.
    /// Shrinking is refused as [`InternalError::Bug`].
    pub(crate) fn extend_to(&self, pages: u64) -> Result<(), InternalError> {
        let current = self.page_count()?;
        if pages < current {
            return Err(InternalError::Bug(format!(
                "data file {} holds {current} pages, cannot extend it to {pages}",
                self.path.display()
            )));
        }
        let size = pages.checked_mul(PAGE_SIZE as u64).ok_or_else(|| {
            InternalError::Bug(format!("{pages} pages of {PAGE_SIZE} bytes overflow a u64"))
        })?;
        self.file.set_len(size)?;
        Ok(())
    }

    /// Reads the page `id` and checks its header and its checksum ([`Page::from_bytes`]).
    ///
    /// An `id` that the file does not hold is [`InternalError::Corruption`] naming the
    /// [`PageId`], rather than a bare end-of-file from the operating system: the caller asked
    /// for a page that the instance does not contain.
    pub(crate) fn read_page(&self, id: PageId) -> Result<Page, InternalError> {
        let pages = self.page_count()?;
        if id.0 >= pages {
            return Err(InternalError::Corruption(format!(
                "page {id} is past the end of data file {}, which holds {pages} pages",
                self.path.display()
            )));
        }
        let mut buffer = [0u8; PAGE_SIZE];
        let mut handle = &self.file;
        // `id.0 < pages`, and `pages` is the size of the file divided by `PAGE_SIZE`, so the
        // product is at most that size and fits in a `u64`.
        handle.seek(SeekFrom::Start(id.0 * PAGE_SIZE as u64))?;
        handle.read_exact(&mut buffer)?;
        Page::from_bytes(buffer)
    }

    /// Writes `page` at the position `id`, as it stands.
    ///
    /// The page is written byte for byte: the caller calls [`Page::seal`] first, and this
    /// layer does not recompute the checksum. An `id` the file does not hold is
    /// [`InternalError::Bug`], because the allocator grows the file before handing out a
    /// [`PageId`].
    ///
    /// The write is not followed by a `sync_all`: what makes a change durable is the journal,
    /// and the pages of the data file are flushed by the checkpoint.
    pub(crate) fn write_page(&self, id: PageId, page: &Page) -> Result<(), InternalError> {
        let pages = self.page_count()?;
        if id.0 >= pages {
            return Err(InternalError::Bug(format!(
                "page {id} is past the end of data file {}, which holds {pages} pages; the \
                 allocator grows the file first",
                self.path.display()
            )));
        }
        let mut handle = &self.file;
        // Bounded as in `read_page`.
        handle.seek(SeekFrom::Start(id.0 * PAGE_SIZE as u64))?;
        handle.write_all(&page.0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
}
