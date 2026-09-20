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
mod tests;
