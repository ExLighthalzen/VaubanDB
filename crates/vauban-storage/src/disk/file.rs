//! The data file of an on-disk instance: an array of pages of [`PAGE_SIZE`] bytes, addressed
//! by [`PageId`].
//!
//! Reads and writes go through [`std::fs::File`] by positioned accesses (`read_exact_at` /
//! `write_all_at` on Unix, `seek_read` / `seek_write` on Windows), which name their offset
//! themselves rather than moving a cursor the handle shares; the file is not mapped into memory, so that the decision of when a page sits in memory and
//! when it is written back belongs to the buffer pool rather than to the kernel.
//!
//! This layer moves 8 192-byte pages and checks their checksum; it does not grow the file by
//! itself and does not cache anything. The allocator ([`super::alloc`]) owns the growth, the
//! buffer pool ([`super::buffer`]) owns the caching.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use vauban_errors::InternalError;

use super::page::{PAGE_SIZE, Page, PageId};

/// The file that holds the pages of an instance, `<dir>/data`.
///
/// Its size is a multiple of [`PAGE_SIZE`]; the number of pages it holds is its size divided
/// by [`PAGE_SIZE`], which is 0 on a fresh instance. A size that is not a multiple is
/// [`InternalError::Corruption`] at open time.
///
/// The handle is shared behind `&self`, by the buffer pool ([`super::buffer`]) and by the
/// allocator ([`super::alloc`]), which grows the file without holding the lock of the pool.
/// Each read and each write carries its own offset, so two threads that move pages at once
/// each reach the page they name (`many_threads_grow_the_file_beside_a_small_pool`, in
/// `tests/disk_concurrency.rs`).
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
        // `id.0 < pages`, and `pages` is the size of the file divided by `PAGE_SIZE`, so the
        // product is at most that size and fits in a `u64`.
        read_exact_at(&self.file, &mut buffer, id.0 * PAGE_SIZE as u64)?;
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
        // Bounded as in `read_page`.
        write_all_at(&self.file, &page.0, id.0 * PAGE_SIZE as u64)?;
        Ok(())
    }
}

/// Fills `buffer` from the bytes of `file` that start at `offset`, without moving a cursor
/// shared with another thread.
#[cfg(unix)]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
}

/// Writes `bytes` into `file` from `offset` on, without moving a cursor shared with another
/// thread.
#[cfg(unix)]
fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, offset)
}

/// Fills `buffer` from the bytes of `file` that start at `offset`.
///
/// `seek_read` names its offset in the call itself; it moves the cursor of the handle as a side
/// effect, which nothing in this layer reads. A short read is continued from where it stopped,
/// and an end of file before `buffer` is full is [`std::io::ErrorKind::UnexpectedEof`].
#[cfg(windows)]
fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        match file.seek_read(buffer, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the data file ends inside a page",
                ));
            }
            Ok(read) => {
                buffer = &mut buffer[read..];
                offset += read as u64;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Writes `bytes` into `file` from `offset` on, continuing a short write from where it stopped.
#[cfg(windows)]
fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        match file.seek_write(bytes, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "the data file took no byte of a page",
                ));
            }
            Ok(written) => {
                bytes = &bytes[written..];
                offset += written as u64;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
