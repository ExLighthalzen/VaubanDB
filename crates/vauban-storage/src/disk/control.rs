//! The control file of an on-disk instance, `vauban.ctl`: a versioned header and the
//! counters that survive a close and a reopen.
//!
//! The file is VaubanDB's own format, versioned from its first byte; its checksum is computed with the register the pages use
//! ([`super::crc`]), over the whole file with the checksum field read as zero.
//! [`super::DiskStorage::open`] writes it when it creates an instance and reads it when it
//! reopens one; the layout of the directory around it is documented there.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

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

    /// Writes the whole file at `path`, creating or truncating it, and `sync_all`s it.
    ///
    /// The file is 128 bytes and is rewritten in full rather than patched field by field. The
    /// call truncates before it writes, so a write that stops in the middle leaves a file
    /// shorter than 128 bytes, which [`Control::read`] reports by its size and not by a
    /// checksum (test `control_rejects_a_file_of_another_size`). The checksum answers the
    /// other case: a file of 128 bytes whose bytes no longer hash to the value they carry
    /// (test `control_rejects_a_checksum_that_disagrees`, checked on the bytes through
    /// [`Control::from_bytes`]).
    pub(crate) fn write(&self, path: &Path) -> Result<(), InternalError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&self.to_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// Reads the file at `path`, checking its size, its magic, its version and its checksum.
    ///
    /// A file whose size is not [`CONTROL_SIZE`] is [`InternalError::Corruption`] naming the
    /// size that was read, not a bare end-of-file.
    pub(crate) fn read(path: &Path) -> Result<Self, InternalError> {
        let mut file = OpenOptions::new().read(true).open(path)?;
        let mut bytes = Vec::with_capacity(CONTROL_SIZE);
        file.read_to_end(&mut bytes)?;
        let bytes: [u8; CONTROL_SIZE] = bytes.as_slice().try_into().map_err(|_| {
            InternalError::Corruption(format!(
                "control file {} is {} bytes, expected {CONTROL_SIZE}",
                path.display(),
                bytes.len()
            ))
        })?;
        Self::from_bytes(&bytes)
    }
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
mod tests {
    use super::super::temp::TempDir;
    use super::*;

    #[test]
    fn note_checkpoint_moves_the_two_lsns_and_survives_the_round_trip() {
        let mut control = Control::default();
        assert_eq!(control.latest_checkpoint_lsn, Lsn(0));
        assert_eq!(control.durable_lsn, Lsn(0));

        control.note_checkpoint(Lsn(42));

        assert_eq!(control.latest_checkpoint_lsn, Lsn(42));
        assert_eq!(control.durable_lsn, Lsn(42));
        let bytes = control.to_bytes();
        assert_eq!(read_u64(&bytes, OFF_LATEST_CHECKPOINT_LSN), 42);
        assert_eq!(read_u64(&bytes, OFF_DURABLE_LSN), 42);
        assert_eq!(Control::from_bytes(&bytes).expect("read back"), control);
        // The other counters are the ones of the block it was called on.
        assert_eq!(control.next_page_id, Control::default().next_page_id);
        assert_eq!(control.next_row_id, Control::default().next_row_id);
    }

    #[test]
    fn fresh_control_writes_the_documented_layout() {
        let bytes = Control::default().to_bytes();
        assert_eq!(bytes.len(), 128);
        assert_eq!(&bytes[0..8], b"VAUBANDB");
        assert_eq!(read_u32(&bytes, 8), 2, "format_version");
        assert_ne!(read_u32(&bytes, 12), 0);
        assert_eq!(read_u64(&bytes, 16), 0, "next_page_id");
        assert_eq!(read_u64(&bytes, 24), u64::MAX, "free_head");
        assert_eq!(read_u64(&bytes, 32), 1, "next_db_id");
        assert_eq!(read_u64(&bytes, 40), 1, "next_table_id");
        assert_eq!(read_u64(&bytes, 48), 1, "next_index_id");
        assert_eq!(read_u64(&bytes, 56), 1, "next_row_id");
        assert_eq!(read_u64(&bytes, 64), 0, "latest_checkpoint_lsn");
        assert_eq!(read_u64(&bytes, 72), 0, "durable_lsn");
        assert_eq!(read_u64(&bytes, 80), u64::MAX, "meta_root");
        assert!(
            bytes[OFF_RESERVED..].iter().all(|&byte| byte == 0),
            "the reserved area is zero"
        );
        assert_eq!(CONTROL_SIZE - OFF_RESERVED, 40);
    }

    /// The vector that distinguishes the table of the layout from its permutations.
    ///
    /// The two tests around this one do not: a fresh block writes `1` in four fields and `0`
    /// in three others, so a swap inside either group answers the same, and a round-trip
    /// through `to_bytes` then `from_bytes` reads with the constants it wrote with, so it is
    /// symmetric. Here each field holds a value of its own, the bytes are laid out at the
    /// offsets of the table spelled as literals, and both directions are checked against
    /// them: swapping two offsets changes the answer. Swapping the constants `OFF_NEXT_DB_ID`
    /// and `OFF_NEXT_TABLE_ID`, swapping `OFF_NEXT_PAGE_ID` and `OFF_DURABLE_LSN`, or swapping
    /// the two `write_u64` of [`Control::to_bytes`] alone, each turns this test red.
    #[test]
    fn each_field_of_the_ctl_reads_and_writes_at_its_own_offset() {
        let mut bytes = [0u8; CONTROL_SIZE];
        bytes[0..8].copy_from_slice(b"VAUBANDB");
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        bytes[16..24].copy_from_slice(&11u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&22u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&33u64.to_le_bytes());
        bytes[40..48].copy_from_slice(&44u64.to_le_bytes());
        bytes[48..56].copy_from_slice(&55u64.to_le_bytes());
        bytes[56..64].copy_from_slice(&66u64.to_le_bytes());
        bytes[64..72].copy_from_slice(&77u64.to_le_bytes());
        bytes[72..80].copy_from_slice(&88u64.to_le_bytes());
        bytes[80..88].copy_from_slice(&99u64.to_le_bytes());
        seal(&mut bytes);

        let control = Control {
            next_page_id: PageId(11),
            free_head: Some(PageId(22)),
            next_db_id: 33,
            next_table_id: 44,
            next_index_id: 55,
            next_row_id: 66,
            latest_checkpoint_lsn: Lsn(77),
            durable_lsn: Lsn(88),
            meta_root: Some(PageId(99)),
        };
        // Reading: the value at each offset lands in the field the table names.
        assert_eq!(
            Control::from_bytes(&bytes).expect("a sealed ctl reads back"),
            control
        );
        // Writing: each field goes back to the offset the table names, checksum included.
        assert_eq!(control.to_bytes(), bytes);
    }

    #[test]
    fn control_round_trips_through_bytes() {
        let control = Control {
            next_page_id: PageId(12),
            free_head: Some(PageId(3)),
            next_db_id: 4,
            next_table_id: 5,
            next_index_id: 6,
            next_row_id: 7,
            latest_checkpoint_lsn: Lsn(8),
            durable_lsn: Lsn(9),
            meta_root: Some(PageId(10)),
        };
        let read = Control::from_bytes(&control.to_bytes()).expect("a sealed ctl reads back");
        assert_eq!(read, control);

        let fresh = Control::default();
        assert_eq!(
            Control::from_bytes(&fresh.to_bytes()).expect("a fresh ctl reads back"),
            fresh
        );
        assert_eq!(fresh.free_head, None);
        assert_eq!(fresh.meta_root, None);
    }

    #[test]
    fn control_write_then_read_from_disk() {
        let dir = TempDir::created("ctl-roundtrip");
        let path = dir.child("vauban.ctl");
        let control = Control {
            next_page_id: PageId(2),
            ..Control::default()
        };
        control.write(&path).expect("write a ctl");
        assert_eq!(
            std::fs::metadata(&path).expect("the ctl exists").len(),
            CONTROL_SIZE as u64
        );
        assert_eq!(Control::read(&path).expect("read the ctl back"), control);

        // Rewriting truncates: the file keeps its size and the new counters.
        let moved = Control {
            next_page_id: PageId(5),
            ..control
        };
        moved.write(&path).expect("rewrite the ctl");
        assert_eq!(
            std::fs::metadata(&path).expect("the ctl exists").len(),
            CONTROL_SIZE as u64
        );
        assert_eq!(Control::read(&path).expect("read the ctl back"), moved);
    }

    #[test]
    fn control_rejects_a_bad_magic() {
        let mut bytes = Control::default().to_bytes();
        bytes[0] = b'X';
        seal(&mut bytes);
        let err = Control::from_bytes(&bytes).expect_err("`XAUBANDB` is not the magic");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        // `58` is the ASCII code of `X`, printed as the value that was read.
        assert!(err.to_string().contains("58"), "{err}");
    }

    #[test]
    fn control_rejects_another_format_version() {
        let mut bytes = Control::default().to_bytes();
        write_u32(&mut bytes, OFF_FORMAT_VERSION, 3);
        seal(&mut bytes);
        let err = Control::from_bytes(&bytes).expect_err("version 3 is not read by this build");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("format version 3"), "{err}");
    }

    #[test]
    fn a_ctl_of_version_one_is_refused_by_its_version() {
        // The block of version 1: the layout of this file up to `durable_lsn`, then 48
        // reserved bytes of zero where `meta_root` now sits. Those zeros read back as
        // `Some(PageId(0))`, so what refuses such a file has to be its version.
        let mut bytes = [0u8; CONTROL_SIZE];
        bytes[OFF_MAGIC..OFF_MAGIC + MAGIC.len()].copy_from_slice(&MAGIC);
        write_u32(&mut bytes, OFF_FORMAT_VERSION, 1);
        write_u64(&mut bytes, OFF_FREE_HEAD, NO_FREE_HEAD);
        for offset in [
            OFF_NEXT_DB_ID,
            OFF_NEXT_TABLE_ID,
            OFF_NEXT_INDEX_ID,
            OFF_NEXT_ROW_ID,
        ] {
            write_u64(&mut bytes, offset, 1);
        }
        seal(&mut bytes);
        assert_eq!(
            read_u64(&bytes, OFF_META_ROOT),
            0,
            "a block of version 1 carries zeros where `meta_root` sits"
        );

        let err = Control::from_bytes(&bytes).expect_err("a block of version 1 is refused");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("format version 1"), "{err}");

        // Through `open`, with the two other files of an instance beside it.
        let dir = TempDir::created("ctl-version-one");
        std::fs::write(dir.child(super::super::CONTROL_FILE_NAME), bytes)
            .expect("write a ctl of version 1");
        std::fs::write(dir.child(super::super::DATA_FILE_NAME), []).expect("write `data`");
        std::fs::write(dir.child(super::super::WAL_FILE_NAME), []).expect("write `wal`");
        let err = super::super::DiskStorage::open(dir.path(), super::super::DiskOptions::default())
            .expect_err("an instance of version 1 is refused");
        assert!(err.to_string().contains("format version 1"), "{err}");
    }

    #[test]
    fn control_rejects_a_checksum_that_disagrees() {
        let mut bytes = Control::default().to_bytes();
        bytes[OFF_NEXT_ROW_ID] = 9;
        let err = Control::from_bytes(&bytes).expect_err("the counter changed after the seal");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("checksum"), "{err}");
        seal(&mut bytes);
        assert_eq!(
            Control::from_bytes(&bytes)
                .expect("sealed again, the checksum agrees")
                .next_row_id,
            9
        );
    }

    #[test]
    fn control_rejects_a_file_of_another_size() {
        let dir = TempDir::created("ctl-size");
        let path = dir.child("vauban.ctl");
        std::fs::write(&path, [0u8; 64]).expect("write a short ctl");
        let err = Control::read(&path).expect_err("64 bytes is not a ctl");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("64 bytes"), "{err}");
        assert!(err.to_string().contains("128"), "{err}");
    }
}
