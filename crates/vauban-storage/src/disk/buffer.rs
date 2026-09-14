//! Buffer pool of the on-disk layout: a fixed number of frames holding pages of the data
//! file, pinned by their callers, evicted by a clock, and written back at eviction,
//! [`BufferPool::flush`] and [`BufferPool::flush_all`].
//!
//! The engine reads and writes the pages of `data` through the pool: the callers
//! `pin` a page instead of reading it. The tests of the [`super`] module reach
//! [`super::file::DataFile`] directly — [`super::DiskStorage::data`] is `pub(crate)` and holds
//! the same handle as the pool — so what the pool serialises is the engine's own `seek` +
//! `read_exact` pairs, by holding its lock across the read and across the write. A
//! [`PinGuard`] does *not* hold that lock, so a caller may hold two pins at once (the heap
//! pins two pages).
//!
//! # No-steal
//!
//! A dirty page whose transaction is still running (`in_progress`) is neither written nor
//! evicted: rolling a transaction back is then a matter of throwing its pages away, and while
//! its pages carry the flag, what it changed does not reach `data`. The price is written where it
//! is paid, on [`BufferPool::pin`]: a transaction that dirties more pages than the pool holds
//! frames saturates it, and the next `pin` of an absent page answers
//! [`InternalError::Bug`] naming the pool as exhausted rather than blocking or panicking.
//!
//! # Write-ahead logging
//!
//! The pool writes a page when the journal already holds, durably, the record that describes
//! its state: `page_lsn <= durable_lsn()`, or `page_lsn == Lsn(0)` for a page no record
//! describes yet. It reads the durable LSN through [`DurableLsn`], which the journal answers:
//! [`super::DiskStorage::open`] builds the pool over the [`super::wal::WalHandle`] of the
//! instance, so a page whose record is not flushed waits in the cache. The tests of this
//! module inject [`Fixed`] instead, which answers a constant and puts their pages on the side
//! of the write-ahead rule they mean to exercise.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use vauban_errors::InternalError;

use super::file::DataFile;
use super::page::{Lsn, Page, PageId};

/// The LSN up to which the journal is durable, as the buffer pool reads it.
///
/// `Debug` is required so that the pool, and through it [`super::DiskStorage`], stays
/// printable.
pub(crate) trait DurableLsn: Send + Sync + fmt::Debug {
    /// The greatest LSN whose record is on disk. `Lsn(0)` means the journal holds nothing
    /// durable.
    fn durable_lsn(&self) -> Lsn;
}

/// [`DurableLsn`] answering the LSN it was built with, without looking at the journal.
///
/// Injected through [`BufferPool::with_durable_lsn`] by the tests: `Fixed(Lsn(0))` is
/// a journal nothing was flushed on, `Fixed(Lsn(u64::MAX))` is one that covers the pages of a
/// test at the LSNs they carry, and an LSN in between is how the write-ahead rule is tested on
/// both of its sides. The pool of an instance reads [`super::wal::WalHandle`] instead.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fixed(pub(crate) Lsn);

impl DurableLsn for Fixed {
    fn durable_lsn(&self) -> Lsn {
        self.0
    }
}

/// One frame of the pool: a cached page and its bookkeeping.
struct Frame {
    /// Position of the page in the data file, and where its bytes go back. The header of
    /// `page` named that position when it was read: [`BufferPool::pin`] refuses a page whose
    /// header names another one.
    id: PageId,
    /// The cached bytes. Sealed by the pool just before a write, not at each change.
    page: Page,
    /// Number of live [`PinGuard`]s on this frame. A frame with a pin is not a victim.
    pins: u32,
    /// The cached bytes differ from those of the file.
    dirty: bool,
    /// A transaction that has not finished changed this page. Under no-steal such a page is
    /// not written and not evicted while it is also dirty.
    in_progress: bool,
    /// Second chance of the clock: set by a `pin`, cleared by the hand passing over.
    referenced: bool,
}

impl fmt::Debug for Frame {
    /// Prints the bookkeeping and the LSN of the page, not its 8 192 bytes: with the derived
    /// implementation, the failure message of a test that printed a pool carried the bytes of
    /// each of its frames.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("id", &self.id)
            .field("page_lsn", &self.page.lsn())
            .field("pins", &self.pins)
            .field("dirty", &self.dirty)
            .field("in_progress", &self.in_progress)
            .field("referenced", &self.referenced)
            .finish()
    }
}

impl Frame {
    /// A frame just filled by a read: pinned once, clean, not in progress, referenced.
    fn loaded(id: PageId, page: Page) -> Self {
        Self {
            id,
            page,
            pins: 1,
            dirty: false,
            in_progress: false,
            referenced: true,
        }
    }
}

/// The mutable state of the pool, behind one lock.
#[derive(Debug)]
struct Inner {
    /// The frames, filled in order until the pool holds `capacity` of them, then reused in
    /// place by the clock. A frame holds a page from its creation on: an eviction replaces its
    /// contents instead of emptying it.
    frames: Vec<Frame>,
    /// Position in `frames` of the page each [`PageId`] sits in.
    by_page: HashMap<PageId, usize>,
    /// Hand of the clock: the next frame it looks at.
    hand: usize,
}

impl Inner {
    /// The frame at `slot`, or [`InternalError::Bug`] for a slot the pool does not hold.
    ///
    /// A slot comes from `by_page` or from the clock, so it is in range at the call sites of
    /// this file; the error is what a later change would meet instead of an indexing panic.
    fn frame_mut(&mut self, slot: usize) -> Result<&mut Frame, InternalError> {
        let held = self.frames.len();
        self.frames.get_mut(slot).ok_or_else(|| {
            InternalError::Bug(format!(
                "buffer pool asked for frame {slot} while it holds {held}"
            ))
        })
    }
}

/// Cache of pages of the data file, of a fixed number of frames.
///
/// # Life of a page
///
/// [`BufferPool::pin`] answers a [`PinGuard`]: the page is in the cache and stays in its frame
/// until the last guard on it is dropped. [`PinGuard::with_page`] reads its bytes,
/// [`PinGuard::with_page_mut`] changes them, and asks for an exclusive pin. Changing bytes does not make the page dirty: the caller then
/// calls [`BufferPool::mark_dirty`] with the LSN of the journal record that describes the
/// change, which is what ties the page to the log.
///
/// Dropping the last guard does not write the page. The pool writes a dirty page at three
/// moments: when the clock evicts its frame, on [`BufferPool::flush`] of its identifier, and
/// on [`BufferPool::flush_all`].
///
/// # Eviction
///
/// Clock with a second chance: the hand walks the frames in order and takes the first one
/// that has no pin, is not a dirty page of a transaction in progress, is not a dirty page the
/// journal has not made durable, and has not been pinned since the hand last passed over it.
/// A dirty page evicted this way is written before its frame is reused.
///
/// # Locking
///
/// One `Mutex` around [`Inner`], taken again at each `pin`, `with_page`, `with_page_mut`,
/// `mark_dirty`, `set_in_progress`, `flush`, `flush_all` and drop of a guard. A poisoned lock
/// is [`InternalError::Corruption`] "buffer pool lock poisoned", as in `MemoryStorage`, and
/// the drop of a [`PinGuard`] that meets it leaves the pin count as it stands: the pool of
/// that instance is unusable anyway, since the seven entry points that answer a `Result`
/// report the poison (`poisoned_lock_is_reported_as_corruption` asserts them). The drop of a
/// guard takes the lock too and reports nothing, having nothing to report it through.
#[derive(Debug)]
pub(crate) struct BufferPool {
    /// The data file the pages come from and go back to.
    file: Arc<DataFile>,
    /// Number of frames the pool may hold, `DiskOptions::buffer_pages`.
    capacity: usize,
    /// Where the pool reads the durable LSN of the journal.
    durable: Arc<dyn DurableLsn>,
    /// Frames and their index, behind one lock.
    inner: Mutex<Inner>,
}

/// Checks the `buffer_pages` option before anything is created.
///
/// A pool of 0 frames could not answer a single `pin`, so it is [`InternalError::Bug`] rather
/// than a pool that fails at its first use. [`super::DiskStorage::open`] calls this before it
/// touches the file system, so that a refused option does not leave a half-created instance
/// behind.
pub(crate) fn validate_buffer_pages(buffer_pages: usize) -> Result<(), InternalError> {
    if buffer_pages == 0 {
        return Err(InternalError::Bug(
            "buffer pool asked for 0 frames; buffer_pages is at least 1, the default is 1024"
                .to_string(),
        ));
    }
    Ok(())
}

impl BufferPool {
    /// A pool of `buffer_pages` frames over `file`, reading its durable LSN from `durable`.
    ///
    /// [`super::DiskStorage::open`] passes the journal of the instance
    /// ([`super::wal::WalHandle`]); the tests pass a [`Fixed`].
    ///
    /// `buffer_pages == 0` is [`InternalError::Bug`] ([`validate_buffer_pages`]).
    pub(crate) fn with_durable_lsn(
        file: Arc<DataFile>,
        buffer_pages: usize,
        durable: Arc<dyn DurableLsn>,
    ) -> Result<Self, InternalError> {
        validate_buffer_pages(buffer_pages)?;
        Ok(Self {
            file,
            capacity: buffer_pages,
            durable,
            inner: Mutex::new(Inner {
                frames: Vec::new(),
                by_page: HashMap::new(),
                hand: 0,
            }),
        })
    }

    /// Number of frames the pool may hold.
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Pins the page `id`, reading it from the data file when the pool does not hold it.
    ///
    /// The guard keeps the page in its frame; dropping it releases the pin. Two pins of the
    /// same page are counted, and the frame becomes a candidate for eviction when the last of
    /// their guards is dropped.
    ///
    /// # Errors
    ///
    /// A read that fails is the error [`super::file::DataFile::read_page`] gives, so a page
    /// past the end of the file is [`InternalError::Corruption`], as is a page whose checksum
    /// or header does not hold. A page read at position `id` whose header names another
    /// identifier is [`InternalError::Corruption`] too: the file layer writes where it is
    /// told and does not renumber pages, so this catches a page written at the wrong place.
    ///
    /// A pool in which the clock finds no frame to free — each one pinned, or holding a dirty
    /// page that the no-steal rule or the write-ahead rule keeps from being written — is
    /// [`InternalError::Bug`] naming it as exhausted and counting the frames of each kind. It
    /// is not a panic and not a wait: nothing would come to free a frame, because the pool
    /// does not write a dirty page of a transaction in progress.
    #[must_use = "dropping the guard immediately unpins the page"]
    pub(crate) fn pin(&self, id: PageId) -> Result<PinGuard<'_>, InternalError> {
        let mut inner = self.lock()?;
        if let Some(&slot) = inner.by_page.get(&id) {
            let frame = inner.frame_mut(slot)?;
            frame.pins = frame.pins.saturating_add(1);
            frame.referenced = true;
            return Ok(PinGuard { pool: self, id });
        }

        let page = self.file.read_page(id)?;
        if page.page_id() != id {
            return Err(InternalError::Corruption(format!(
                "page at position {id} of data file {} carries identifier {}",
                self.file.path().display(),
                page.page_id()
            )));
        }

        let slot = if inner.frames.len() < self.capacity {
            inner.frames.push(Frame::loaded(id, page));
            inner.frames.len() - 1
        } else {
            let victim = self.choose_victim(&mut inner, id)?;
            self.write_if_needed(&mut inner, victim)?;
            let evicted = inner.frame_mut(victim)?;
            let evicted_id = evicted.id;
            *evicted = Frame::loaded(id, page);
            inner.by_page.remove(&evicted_id);
            victim
        };
        inner.by_page.insert(id, slot);
        Ok(PinGuard { pool: self, id })
    }

    /// Marks the page `id` dirty and ties it to the journal record of LSN `rec_lsn`.
    ///
    /// `rec_lsn` is the LSN of the record that describes the change the caller has just made
    /// through [`PinGuard::with_page_mut`]; the caller passes the LSN the journal handed it.
    /// The page LSN does not go backwards: the pool keeps the greater of the one the page
    /// carries and `rec_lsn`, because recovery compares that field with the LSN of the record
    /// it is about to replay ([`super::recover`]). `Lsn(0)` therefore leaves an unlogged page
    /// unlogged.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when the pool does not hold the page, or holds it with no pin:
    /// the caller changes a page it has pinned, and a page nobody pins may be evicted between
    /// the change and this call.
    pub(crate) fn mark_dirty(&self, id: PageId, rec_lsn: Lsn) -> Result<(), InternalError> {
        let mut inner = self.lock()?;
        let Some(&slot) = inner.by_page.get(&id) else {
            return Err(InternalError::Bug(format!(
                "page {id} is not in the buffer pool; mark_dirty needs it pinned"
            )));
        };
        let frame = inner.frame_mut(slot)?;
        if frame.pins == 0 {
            return Err(InternalError::Bug(format!(
                "page {id} is in the buffer pool but not pinned; mark_dirty needs a pin"
            )));
        }
        frame.dirty = true;
        if rec_lsn > frame.page.lsn() {
            frame.page.set_lsn(rec_lsn);
        }
        Ok(())
    }

    /// Says whether a transaction that has not finished changed the page `id`.
    ///
    /// The version layer sets the flag around a transaction. While it is set and the
    /// page is dirty, the page is neither written nor evicted (no-steal).
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when `in_progress` is `true` and the pool does not hold the
    /// page: the caller pins it first. Clearing the flag on a page the pool does not hold is
    /// not an error, because a clean page may have been evicted since it was set.
    pub(crate) fn set_in_progress(
        &self,
        id: PageId,
        in_progress: bool,
    ) -> Result<(), InternalError> {
        let mut inner = self.lock()?;
        match inner.by_page.get(&id).copied() {
            Some(slot) => {
                inner.frame_mut(slot)?.in_progress = in_progress;
                Ok(())
            }
            None if !in_progress => Ok(()),
            None => Err(InternalError::Bug(format!(
                "page {id} is not in the buffer pool; set_in_progress(true) needs it pinned"
            ))),
        }
    }

    /// Writes the page `id` to the data file when it is dirty and the rules allow it.
    ///
    /// A page the pool does not hold and a page that is not dirty are both `Ok(())`: there is
    /// nothing to write. A dirty page of a transaction in progress is skipped, as it is by
    /// [`BufferPool::flush_all`] (no-steal).
    ///
    /// # Errors
    ///
    /// A dirty page carrying an LSN other than `Lsn(0)` that is greater than the durable LSN
    /// is [`InternalError::Bug`]: writing it would put the page ahead of the log. Making the
    /// journal durable so that the page may leave is the caller's business.
    pub(crate) fn flush(&self, id: PageId) -> Result<(), InternalError> {
        let mut inner = self.lock()?;
        let Some(&slot) = inner.by_page.get(&id) else {
            return Ok(());
        };
        self.write_if_needed(&mut inner, slot)?;
        Ok(())
    }

    /// Writes every dirty page the rules allow, in frame order.
    ///
    /// A dirty page of a transaction in progress is **skipped**, not written: the checkpoint
    /// calls this and must leave nothing of an unfinished transaction in `data`.
    /// Pinned pages are written like the others: a pin says the caller holds the page, not
    /// that the page is being changed.
    ///
    /// # Errors
    ///
    /// The first page whose LSN is ahead of the durable LSN stops the walk with the
    /// [`InternalError::Bug`] of [`BufferPool::flush`]; the pages already written stay
    /// written and clean.
    pub(crate) fn flush_all(&self) -> Result<(), InternalError> {
        let mut inner = self.lock()?;
        for slot in 0..inner.frames.len() {
            self.write_if_needed(&mut inner, slot)?;
        }
        Ok(())
    }

    /// The lock around the frames, a poison reported as corruption.
    fn lock(&self) -> Result<MutexGuard<'_, Inner>, InternalError> {
        self.inner
            .lock()
            .map_err(|_| InternalError::Corruption("buffer pool lock poisoned".to_string()))
    }

    /// Seals and writes the page of `slot` when it is dirty and the rules allow it; answers
    /// whether it was written.
    fn write_if_needed(&self, inner: &mut Inner, slot: usize) -> Result<bool, InternalError> {
        let durable = self.durable.durable_lsn();
        let frame = inner.frame_mut(slot)?;
        if !frame.dirty || frame.in_progress {
            return Ok(false);
        }
        let page_lsn = frame.page.lsn();
        if !writable(page_lsn, durable) {
            return Err(InternalError::Bug(format!(
                "page {} carries LSN {page_lsn} and the journal is durable up to {durable}; \
                 writing the page would put it ahead of the log",
                frame.id
            )));
        }
        // The file layer writes the bytes as they stand, so the checksum is written here.
        frame.page.seal();
        self.file.write_page(frame.id, &frame.page)?;
        frame.dirty = false;
        Ok(true)
    }

    /// Picks the frame the clock evicts to make room for `wanted`.
    ///
    /// Two turns of the hand at most: the first clears the second chance of the frames that
    /// were pinned since it last passed, the second takes one of them.
    fn choose_victim(&self, inner: &mut Inner, wanted: PageId) -> Result<usize, InternalError> {
        let durable = self.durable.durable_lsn();
        let held = inner.frames.len();
        for _ in 0..2 * held {
            let slot = inner.hand;
            // `held` is the number of frames, which is `self.capacity >= 1` here.
            inner.hand = (slot + 1) % held;
            let frame = inner.frame_mut(slot)?;
            if frame.pins > 0 {
                continue;
            }
            if frame.dirty && (frame.in_progress || !writable(frame.page.lsn(), durable)) {
                continue;
            }
            if frame.referenced {
                frame.referenced = false;
                continue;
            }
            return Ok(slot);
        }

        let pinned = inner.frames.iter().filter(|frame| frame.pins > 0).count();
        let running = inner
            .frames
            .iter()
            .filter(|frame| frame.pins == 0 && frame.dirty && frame.in_progress)
            .count();
        let ahead = inner
            .frames
            .iter()
            .filter(|frame| {
                frame.pins == 0
                    && frame.dirty
                    && !frame.in_progress
                    && !writable(frame.page.lsn(), durable)
            })
            .count();
        Err(InternalError::Bug(format!(
            "buffer pool of {held} frames is exhausted: page {wanted} needs one and no frame \
             may be freed (pinned: {pinned}, dirty and in progress: {running}, dirty and ahead \
             of the durable LSN: {ahead})"
        )))
    }
}

/// Whether a page carrying `page_lsn` may be written when the journal is durable up to
/// `durable`.
///
/// `Lsn(0)` is a page no record describes ([`super::page::Lsn`]), so it does not wait for the
/// journal.
fn writable(page_lsn: Lsn, durable: Lsn) -> bool {
    page_lsn == Lsn(0) || page_lsn <= durable
}

/// A pin on one page of the pool: the page stays in its frame until the guard is dropped.
///
/// The guard holds no lock, so a caller may pin several pages at once. It borrows the pool,
/// which is what keeps the pool alive for as long as the pin.
pub(crate) struct PinGuard<'pool> {
    /// The pool the pin was taken from.
    pool: &'pool BufferPool,
    /// The page that is pinned.
    id: PageId,
}

impl fmt::Debug for PinGuard<'_> {
    /// Prints the page that is pinned. The pool is left out: a guard is printed by the
    /// message of a test that expected the pin to fail, and the pool would drag the frames
    /// into that message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinGuard").field("id", &self.id).finish()
    }
}

impl PinGuard<'_> {
    /// The page this guard pins.
    pub(crate) fn page_id(&self) -> PageId {
        self.id
    }

    /// Applies `read` to the cached page, holding the lock of the pool for that call only.
    ///
    /// # Errors
    ///
    /// [`InternalError::Corruption`] for a poisoned lock, [`InternalError::Bug`] if the pool
    /// no longer holds the page — which a live pin prevents.
    pub(crate) fn with_page<R>(&self, read: impl FnOnce(&Page) -> R) -> Result<R, InternalError> {
        let mut inner = self.pool.lock()?;
        let slot = self.slot(&inner)?;
        Ok(read(&inner.frame_mut(slot)?.page))
    }

    /// Applies `change` to the cached page.
    ///
    /// The change does not make the page dirty: the caller calls
    /// [`BufferPool::mark_dirty`] with the LSN of the journal record that describes it. A page
    /// that is not dirty is not written, so a change made without that call stays in the cache
    /// and is lost at eviction.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when the page carries more than one pin: two callers holding
    /// the same page may read it together, changing it asks for the only pin on it.
    /// [`InternalError::Corruption`] for a poisoned lock.
    pub(crate) fn with_page_mut<R>(
        &self,
        change: impl FnOnce(&mut Page) -> R,
    ) -> Result<R, InternalError> {
        let mut inner = self.pool.lock()?;
        let slot = self.slot(&inner)?;
        let frame = inner.frame_mut(slot)?;
        if frame.pins != 1 {
            return Err(InternalError::Bug(format!(
                "page {} carries {} pins; changing it asks for an exclusive pin",
                self.id, frame.pins
            )));
        }
        Ok(change(&mut frame.page))
    }

    /// The frame the pinned page sits in.
    fn slot(&self, inner: &Inner) -> Result<usize, InternalError> {
        inner.by_page.get(&self.id).copied().ok_or_else(|| {
            InternalError::Bug(format!(
                "page {} is pinned but the buffer pool no longer holds it",
                self.id
            ))
        })
    }
}

impl Drop for PinGuard<'_> {
    /// Releases the pin.
    ///
    /// A poisoned lock and a page the pool no longer holds leave the counts as they are: a
    /// drop reports nothing, and the other entry points of a poisoned pool answer
    /// [`InternalError::Corruption`].
    fn drop(&mut self) {
        if let Ok(mut inner) = self.pool.inner.lock()
            && let Some(&slot) = inner.by_page.get(&self.id)
            && let Ok(frame) = inner.frame_mut(slot)
        {
            frame.pins = frame.pins.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
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
        let pool =
            BufferPool::with_durable_lsn(Arc::clone(&file), frames, Arc::new(Fixed(durable)))
                .expect("build the pool");
        (dir, file, pool)
    }

    /// A data file of `pages` sealed empty pages, each carrying its own identifier.
    fn data_file(label: &str, pages: u64) -> (TempDir, Arc<DataFile>) {
        let dir = TempDir::created(label);
        let file = Arc::new(
            DataFile::create(&dir.child(super::super::DATA_FILE_NAME))
                .expect("create the data file"),
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
}
