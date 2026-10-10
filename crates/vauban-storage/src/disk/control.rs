//! The control file of an on-disk instance, `vauban.ctl`: a versioned header and the
//! counters that survive a close and a reopen.
//!
//! The file is VaubanDB's own format, versioned from its first byte; its checksum is computed with the register the pages use
//! ([`super::crc`]), over the whole file with the checksum field read as zero.
//! [`super::DiskStorage::open`] writes it when it creates an instance and reads it when it
//! reopens one; the layout of the directory around it is documented there.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use vauban_errors::InternalError;

use super::crc::Crc32;
use super::page::{Lsn, PageId};

/// Size of `vauban.ctl`, in bytes. The bytes after the last field are zero.
pub(crate) const CONTROL_SIZE: usize = 128;

/// Value written in the `format_version` field by this build.
///
/// Version 1 was the block without a catalogue, whose 48 reserved bytes started at offset 80.
/// Version 2 took the first eight of them for `meta_root`, where a block of version 1 carries
/// zeros — which read back as `Some(PageId(0))`, that is page 0 taken for the head of the
/// chain of the catalogue. That is not a value a sentinel can rule out, page 0 being the root
/// a fresh instance normally allocates, so the version carries the change and a block of
/// version 1 is refused by its version rather than by what it makes of page 0
/// (`a_ctl_of_version_one_is_refused_by_its_version`).
pub(crate) const CONTROL_FORMAT_VERSION: u32 = 2;

/// Magic at the start of the file: the ASCII bytes `V` `A` `U` `B` `A` `N` `D` `B`.
const MAGIC: [u8; 8] = *b"VAUBANDB";

/// Value stored in `free_head` when the free list has no head.
const NO_FREE_HEAD: u64 = u64::MAX;

/// Value stored in `meta_root` when the instance holds no catalogue page yet.
const NO_META_ROOT: u64 = u64::MAX;

/// Offset of the magic.
const OFF_MAGIC: usize = 0;
/// Offset of the format version.
const OFF_FORMAT_VERSION: usize = 8;
/// Offset of the checksum.
const OFF_CHECKSUM: usize = 12;
/// Offset of `next_page_id`.
const OFF_NEXT_PAGE_ID: usize = 16;
/// Offset of `free_head`.
const OFF_FREE_HEAD: usize = 24;
/// Offset of `next_db_id`.
const OFF_NEXT_DB_ID: usize = 32;
/// Offset of `next_table_id`.
const OFF_NEXT_TABLE_ID: usize = 40;
/// Offset of `next_index_id`.
const OFF_NEXT_INDEX_ID: usize = 48;
/// Offset of `next_row_id`.
const OFF_NEXT_ROW_ID: usize = 56;
/// Offset of `latest_checkpoint_lsn`.
const OFF_LATEST_CHECKPOINT_LSN: usize = 64;
/// Offset of `durable_lsn`.
const OFF_DURABLE_LSN: usize = 72;
/// Offset of `meta_root`.
const OFF_META_ROOT: usize = 80;
/// Offset of the reserved area: 40 zero bytes, up to [`CONTROL_SIZE`].
const OFF_RESERVED: usize = 88;

/// Contents of `vauban.ctl`: 128 bytes, little-endian.
///
/// # Layout
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0 | 8 | magic, bytes `V` `A` `U` `B` `A` `N` `D` `B` |
/// | 8 | 4 | `format_version` = 2 |
/// | 12 | 4 | checksum CRC-32 of the 128 bytes, this field read as four zero bytes |
/// | 16 | 8 | `next_page_id`: next [`PageId`] to hand out, 0 on a fresh instance |
/// | 24 | 8 | `free_head`: head of the free list, `u64::MAX` when there is none |
/// | 32 | 8 | `next_db_id` = 1 |
/// | 40 | 8 | `next_table_id` = 1 |
/// | 48 | 8 | `next_index_id` = 1 |
/// | 56 | 8 | `next_row_id` = 1, counted for the whole instance |
/// | 64 | 8 | `latest_checkpoint_lsn` = 0 |
/// | 72 | 8 | `durable_lsn` = 0 |
/// | 80 | 8 | `meta_root`: head of the chain of catalogue pages, `u64::MAX` before the first DDL |
/// | 88 | 40 | reserved, zero |
///
/// `next_row_id` is a single counter for the instance: the trait promises a [`crate::RowId`]
/// unique within its table and not reused, which a counter shared by the tables satisfies at
/// the price of gaps in a given table (the catalogue keeps a per-table counter on top, without
/// changing this file).
///
/// # Checksum
///
/// [`Control::to_bytes`] writes the CRC-32 of the 128 bytes with the checksum field read as
/// four zero bytes, the rule the pages follow ([`super::page::Page::seal`]).
/// [`Control::from_bytes`] recomputes it the same way, after the magic and the version, so
/// that a file written by another format is reported as such rather than as a checksum
/// mismatch. Nothing is repaired: a magic that differs, a `format_version` other than
/// [`CONTROL_FORMAT_VERSION`] and a
/// checksum that disagrees are all [`InternalError::Corruption`], and the message names the
/// value that was read.
///
/// The counters are moved forward by the allocator ([`super::alloc`]) and by the catalogue
/// ([`super::meta`]); this module writes them once at creation and reads them back.
/// `meta_root` sits at offset 80, taken from the reserved area of version 1, and the
/// `format_version` went to 2 with it; a
/// fresh block writes `u64::MAX` there, which is the value that says "no catalogue page yet"
/// (`fresh_control_writes_the_documented_layout` reads those eight bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Control {
    /// Next [`PageId`] the allocator may hand out.
    pub(crate) next_page_id: PageId,
    /// Head of the list of free pages, `None` when the list is empty.
    pub(crate) free_head: Option<PageId>,
    /// Next [`crate::DbId`] to hand out.
    pub(crate) next_db_id: u64,
    /// Next [`crate::TableId`] to hand out.
    pub(crate) next_table_id: u64,
    /// Next [`crate::IndexId`] to hand out.
    pub(crate) next_index_id: u64,
    /// Next [`crate::RowId`] to hand out, for the whole instance.
    pub(crate) next_row_id: u64,
    /// LSN of the last checkpoint record, 0 when there is none.
    pub(crate) latest_checkpoint_lsn: Lsn,
    /// LSN up to which the journal is known to be durable, 0 at creation.
    pub(crate) durable_lsn: Lsn,
    /// Head of the chain of [`super::page::PageKind::Meta`] pages holding the catalogue,
    /// `None` until the first DDL statement allocates it ([`super::meta::ensure_root`]).
    ///
    /// The walk of the catalogue starts at this field: the head of the chain is a page that
    /// another page does not point at, so it is named in this file rather than in the journal,
    /// which may be truncated some day.
    pub(crate) meta_root: Option<PageId>,
}

impl Default for Control {
    /// The control block of a fresh instance: no page allocated, no free list, the object
    /// counters at 1, the LSNs at 0.
    fn default() -> Self {
        Self {
            next_page_id: PageId(0),
            free_head: None,
            next_db_id: 1,
            next_table_id: 1,
            next_index_id: 1,
            next_row_id: 1,
            latest_checkpoint_lsn: Lsn(0),
            durable_lsn: Lsn(0),
            meta_root: None,
        }
    }
}

impl Control {
    /// The 128 bytes of the file, checksum included.
    pub(crate) fn to_bytes(self) -> [u8; CONTROL_SIZE] {
        let mut bytes = [0u8; CONTROL_SIZE];
        bytes[OFF_MAGIC..OFF_MAGIC + MAGIC.len()].copy_from_slice(&MAGIC);
        write_u32(&mut bytes, OFF_FORMAT_VERSION, CONTROL_FORMAT_VERSION);
        write_u64(&mut bytes, OFF_NEXT_PAGE_ID, self.next_page_id.0);
        write_u64(
            &mut bytes,
            OFF_FREE_HEAD,
            match self.free_head {
                Some(page) => page.0,
                None => NO_FREE_HEAD,
            },
        );
        write_u64(&mut bytes, OFF_NEXT_DB_ID, self.next_db_id);
        write_u64(&mut bytes, OFF_NEXT_TABLE_ID, self.next_table_id);
        write_u64(&mut bytes, OFF_NEXT_INDEX_ID, self.next_index_id);
        write_u64(&mut bytes, OFF_NEXT_ROW_ID, self.next_row_id);
        write_u64(
            &mut bytes,
            OFF_LATEST_CHECKPOINT_LSN,
            self.latest_checkpoint_lsn.0,
        );
        write_u64(&mut bytes, OFF_DURABLE_LSN, self.durable_lsn.0);
        write_u64(
            &mut bytes,
            OFF_META_ROOT,
            match self.meta_root {
                Some(page) => page.0,
                None => NO_META_ROOT,
            },
        );
        seal(&mut bytes);
        bytes
    }

    /// Reads a control block from the 128 bytes of the file.
    pub(crate) fn from_bytes(bytes: &[u8; CONTROL_SIZE]) -> Result<Self, InternalError> {
        let magic: [u8; MAGIC.len()] = bytes[OFF_MAGIC..OFF_MAGIC + MAGIC.len()]
            .try_into()
            .map_err(|_| InternalError::Bug("the magic of a ctl is 8 bytes".into()))?;
        if magic != MAGIC {
            return Err(InternalError::Corruption(format!(
                "control file starts with magic {magic:02x?}, expected {MAGIC:02x?}"
            )));
        }
        let version = read_u32(bytes, OFF_FORMAT_VERSION);
        if version != CONTROL_FORMAT_VERSION {
            return Err(InternalError::Corruption(format!(
                "control file declares format version {version}, this build reads \
                 {CONTROL_FORMAT_VERSION}"
            )));
        }
        let stored = read_u32(bytes, OFF_CHECKSUM);
        let computed = checksum(bytes);
        if stored != computed {
            return Err(InternalError::Corruption(format!(
                "control file carries checksum {stored:#010x}, its bytes hash to {computed:#010x}"
            )));
        }
        let free_head = match read_u64(bytes, OFF_FREE_HEAD) {
            NO_FREE_HEAD => None,
            head => Some(PageId(head)),
        };
        let meta_root = match read_u64(bytes, OFF_META_ROOT) {
            NO_META_ROOT => None,
            root => Some(PageId(root)),
        };
        Ok(Self {
            next_page_id: PageId(read_u64(bytes, OFF_NEXT_PAGE_ID)),
            free_head,
            next_db_id: read_u64(bytes, OFF_NEXT_DB_ID),
            next_table_id: read_u64(bytes, OFF_NEXT_TABLE_ID),
            next_index_id: read_u64(bytes, OFF_NEXT_INDEX_ID),
            next_row_id: read_u64(bytes, OFF_NEXT_ROW_ID),
            latest_checkpoint_lsn: Lsn(read_u64(bytes, OFF_LATEST_CHECKPOINT_LSN)),
            durable_lsn: Lsn(read_u64(bytes, OFF_DURABLE_LSN)),
            meta_root,
        })
    }

    /// Records the checkpoint whose record is numbered `lsn`: `latest_checkpoint_lsn` takes
    /// it, and `durable_lsn` with it.
    ///
    /// The two fields take the same value because the checkpoint appends its record and
    /// `sync_all`s the journal before it writes this block ([`super::DiskStorage::checkpoint`]):
    /// the record `lsn` names is durable, so the journal is durable up to it. A commit that
    /// follows the checkpoint syncs the journal further without rewriting this file, this
    /// build writing the control block at the checkpoint; a reopen may therefore read a
    /// `durable_lsn` behind the last record of the journal. The caller persists the block with
    /// [`Control::write`], which `sync_all`s it.
    ///
    /// `latest_checkpoint_lsn` says where the last checkpoint happened. It does not say by
    /// itself where a redo may start: a transaction that began before the checkpoint record
    /// and committed after it has its row records behind that LSN and its page out of `data`:
    /// one insert astride one checkpoint gives `Insert` at `Lsn(2)`, `Checkpoint` at `Lsn(3)`,
    /// `Commit` at `Lsn(4)`
    /// (`a_transaction_astride_the_checkpoint_leaves_its_insert_before_redo_lsn`, in
    /// `disk/mod.rs`). The journal is kept whole — a checkpoint does not truncate it
    /// (`two_checkpoints_leave_two_records`, in `disk/mod.rs` as well) — so those earlier
    /// records are still readable; where the scan of a recovery begins is decided by
    /// [`super::recover`].
    pub(crate) fn note_checkpoint(&mut self, lsn: Lsn) {
        self.latest_checkpoint_lsn = lsn;
        self.durable_lsn = lsn;
    }

    /// Writes the whole file at `path` and `sync_all`s it, so that a stopped process does not
    /// leave a file cut short.
    ///
    /// A file already at `path` is overwritten in place, its 128 bytes written from offset 0
    /// without truncating it first: the file keeps its size through the call, and the checksum
    /// answers for bytes that the write would leave half old and half new. A truncation
    /// followed by a write would leave an empty file to a process stopped between the two.
    ///
    /// A file that is not there yet is created in one step: the 128 bytes go to a staging file
    /// beside `path` ([`staging_path`]), which is `sync_all`ed and then renamed to `path`; on
    /// Unix the directory is `sync_all`ed after the rename. A process stopped during the
    /// creation leaves no file at `path`, or the whole one (test
    /// `a_creation_stopped_before_its_rename_leaves_no_ctl`); a staging file left behind by
    /// such a stop is truncated and written again by the next creation.
    pub(crate) fn write(&self, path: &Path) -> Result<(), InternalError> {
        let bytes = self.to_bytes();
        match OpenOptions::new().write(true).open(path) {
            Ok(mut file) => {
                file.write_all(&bytes)?;
                if file.metadata()?.len() != CONTROL_SIZE as u64 {
                    file.set_len(CONTROL_SIZE as u64)?;
                }
                file.sync_all()?;
                Ok(())
            }
            Err(err) if err.kind() == ErrorKind::NotFound => create(path, &bytes),
            Err(err) => Err(err.into()),
        }
    }

    /// Reads the file at `path`, checking its size, its magic, its version and its checksum.
    ///
    /// A file whose size is not [`CONTROL_SIZE`] is [`InternalError::Corruption`] naming the
    /// size that was read, not a bare end-of-file.
    ///
    /// An empty file is what a build that truncated the file in place before writing it
    /// leaves when it stops between the two. When the `data` and `wal` files beside it are
    /// both there and both empty, the instance had not written anything yet: the file is
    /// written again with the block of a fresh instance, [`Control::default`], and that
    /// block is returned (test `an_empty_ctl_beside_an_empty_instance_is_written_again`).
    /// Otherwise the error says that the file is empty and that a directory whose creation
    /// was interrupted can be removed and created again (test
    /// `an_empty_ctl_beside_a_used_instance_is_refused_with_a_way_out`).
    pub(crate) fn read(path: &Path) -> Result<Self, InternalError> {
        let mut file = OpenOptions::new().read(true).open(path)?;
        let mut bytes = Vec::with_capacity(CONTROL_SIZE);
        file.read_to_end(&mut bytes)?;
        drop(file);
        if bytes.is_empty() {
            return Self::read_empty(path);
        }
        let bytes: [u8; CONTROL_SIZE] = bytes.as_slice().try_into().map_err(|_| {
            InternalError::Corruption(format!(
                "control file {} is {} bytes, expected {CONTROL_SIZE}",
                path.display(),
                bytes.len()
            ))
        })?;
        Self::from_bytes(&bytes)
    }

    /// The answer of [`Control::read`] for an empty file at `path`.
    fn read_empty(path: &Path) -> Result<Self, InternalError> {
        let empty_sibling = |name: &str| {
            path.parent()
                .map(|dir| dir.join(name))
                .and_then(|sibling| fs::metadata(sibling).ok())
                .is_some_and(|meta| meta.is_file() && meta.len() == 0)
        };
        if empty_sibling(super::DATA_FILE_NAME) && empty_sibling(super::WAL_FILE_NAME) {
            let fresh = Self::default();
            fresh.write(path)?;
            return Ok(fresh);
        }
        Err(InternalError::Corruption(format!(
            "control file {} is 0 bytes, expected {CONTROL_SIZE}: a write of it was \
             interrupted and the instance beside it holds pages or journal records it no \
             longer describes; if this directory was being created, remove it and start again",
            path.display()
        )))
    }
}

/// Creates the file at `path` holding `bytes`, through a staging file renamed to `path`
/// ([`Control::write`]).
fn create(path: &Path, bytes: &[u8; CONTROL_SIZE]) -> Result<(), InternalError> {
    let staging = staging_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&staging)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&staging, path)?;
    sync_directory_of(path)?;
    Ok(())
}

/// The staging file [`Control::write`] writes before renaming it to `path`: the same name
/// with `.tmp` appended, in the same directory, so that the rename stays within one file
/// system.
fn staging_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// `sync_all`s the directory that holds `path`, which makes a rename in it durable.
#[cfg(unix)]
fn sync_directory_of(path: &Path) -> Result<(), InternalError> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Elsewhere a directory is not opened as a file; the rename is left to the file system.
#[cfg(not(unix))]
fn sync_directory_of(_path: &Path) -> Result<(), InternalError> {
    Ok(())
}

/// Writes the checksum of `bytes` into its checksum field.
///
/// Exposed to the tests of the module, which forge a control file and need its checksum to
/// agree with the bytes they wrote.
pub(crate) fn seal(bytes: &mut [u8; CONTROL_SIZE]) {
    let computed = checksum(bytes);
    write_u32(bytes, OFF_CHECKSUM, computed);
}

/// CRC-32 of the 128 bytes, the checksum field read as four zero bytes.
fn checksum(bytes: &[u8; CONTROL_SIZE]) -> u32 {
    let mut register = Crc32::new();
    register.update(&bytes[..OFF_CHECKSUM]);
    register.update(&[0u8; 4]);
    register.update(&bytes[OFF_CHECKSUM + 4..]);
    register.finish()
}

/// Reads a little-endian `u32` at `at`.
fn read_u32(bytes: &[u8; CONTROL_SIZE], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Reads a little-endian `u64` at `at`.
fn read_u64(bytes: &[u8; CONTROL_SIZE], at: usize) -> u64 {
    u64::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
        bytes[at + 4],
        bytes[at + 5],
        bytes[at + 6],
        bytes[at + 7],
    ])
}

/// Writes a little-endian `u32` at `at`.
fn write_u32(bytes: &mut [u8; CONTROL_SIZE], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Writes a little-endian `u64` at `at`.
fn write_u64(bytes: &mut [u8; CONTROL_SIZE], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests;
