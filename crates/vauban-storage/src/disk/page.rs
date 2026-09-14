//! Page format of the on-disk layout: pages of 8 192 bytes, a common header of 32 bytes, and
//! a CRC-32 checksum covering the page.
//!
//! The layout is VaubanDB's own and is versioned from its first byte. The other modules of
//! the on-disk layout (heap, B+tree, WAL) read and write the common header through the accessors of [`Page`] rather than by touching its first 32
//! bytes, so that a change of layout stays in this file.

use std::fmt;

use vauban_errors::InternalError;

use super::crc::Crc32;

/// Size of a page, in bytes.
pub(crate) const PAGE_SIZE: usize = 8192;

/// Size of the common header at the start of a page, in bytes.
pub(crate) const HEADER_SIZE: usize = 32;

/// Value written in the `format_version` field by this build.
pub(crate) const FORMAT_VERSION: u8 = 1;

/// Magic at the start of a page: the ASCII bytes `V`, `D`, `B`, `P`.
const MAGIC: [u8; 4] = *b"VDBP";

/// Offset of the magic.
const OFF_MAGIC: usize = 0;
/// Offset of the format version.
const OFF_FORMAT_VERSION: usize = 4;
/// Offset of the page kind.
const OFF_KIND: usize = 5;
/// Offset of the flag word.
const OFF_FLAGS: usize = 6;
/// Offset of the page identifier.
const OFF_PAGE_ID: usize = 8;
/// Offset of the page LSN.
const OFF_PAGE_LSN: usize = 16;
/// Offset of the checksum.
const OFF_CHECKSUM: usize = 24;
/// Offset of the slot count.
const OFF_SLOT_COUNT: usize = 28;
/// Offset of the `lower` boundary.
const OFF_LOWER: usize = 30;

/// Identifier of a page within a data file: its index in the file, counted from 0.
///
/// `Display` writes the bare decimal integer, like the identifier newtypes of the crate root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PageId(pub u64);

impl fmt::Display for PageId {
    /// Writes the bare decimal integer, without the type name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Log sequence number of the WAL record that last changed a page.
///
/// A page built by [`Page::empty`] carries `Lsn(0)`: 0 means never logged. The log sequence
/// numbers handed out by the journal start at 1.
///
/// `Display` writes the bare decimal integer, like the identifier newtypes of the crate root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Lsn(pub u64);

impl fmt::Display for Lsn {
    /// Writes the bare decimal integer, without the type name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Kind of a page, stored as one byte at offset 5 of the header.
///
/// The byte values are part of the on-disk format: a new kind takes a free value, an existing
/// one keeps its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub(crate) enum PageKind {
    /// Page that belongs to the file and holds nothing; the allocator hands it out.
    Free = 0,
    /// Slotted page of table rows.
    Heap = 1,
    /// Continuation of a row too large for a single page.
    Overflow = 2,
    /// Internal node of a B+tree.
    BTreeInternal = 3,
    /// Leaf node of a B+tree.
    BTreeLeaf = 4,
    /// Page of the catalogue chain.
    Meta = 5,
    /// Directory mapping a [`crate::RowId`] to its physical position.
    RowIdDir = 6,
}

impl PageKind {
    /// Reads a kind from its stored byte.
    ///
    /// A byte outside the enum is data corruption, not a panic: the error names the value
    /// that was read.
    pub(crate) fn from_byte(raw: u8) -> Result<Self, InternalError> {
        match raw {
            0 => Ok(Self::Free),
            1 => Ok(Self::Heap),
            2 => Ok(Self::Overflow),
            3 => Ok(Self::BTreeInternal),
            4 => Ok(Self::BTreeLeaf),
            5 => Ok(Self::Meta),
            6 => Ok(Self::RowIdDir),
            other => Err(InternalError::Corruption(format!(
                "unknown page kind {other}"
            ))),
        }
    }

    /// The byte written at offset 5 of the header.
    pub(crate) fn as_byte(self) -> u8 {
        self as u8
    }
}

/// A page of the on-disk layout: 8 192 bytes, header included.
///
/// # Common header (offsets 0..32), little-endian
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0 | 4 | magic, bytes `V` `D` `B` `P` (`0x56 0x44 0x42 0x50`) |
/// | 4 | 1 | `format_version` = 1 |
/// | 5 | 1 | [`PageKind`] |
/// | 6 | 2 | `flags`, 0 (no flag is defined) |
/// | 8 | 8 | [`PageId`] |
/// | 16 | 8 | `page_lsn` ([`Lsn`]) |
/// | 24 | 4 | checksum CRC-32 |
/// | 28 | 2 | `slot_count` (0 on a fresh page; the heap uses it) |
/// | 30 | 2 | `lower`: first free byte after the header and the kind-specific directory; 32 on a fresh page |
///
/// `upper` is deliberately absent from the common header: the heap computes it from the end
/// of the page.
///
/// # Checksum
///
/// [`Page::seal`] writes the CRC-32 of the 8 192 bytes with the checksum field read as four
/// zero bytes; [`Page::verify`] recomputes it the same way and compares. A page is sealed
/// before it is handed to the file layer and verified after it is read back.
///
/// # Reading a page back
///
/// [`Page::from_bytes`] checks the magic, the format version, the kind and the checksum. A
/// magic that differs, a `format_version` other than 1, a kind outside [`PageKind`] and a
/// checksum that disagrees are all [`InternalError::Corruption`], and the error names the
/// value that was read. Nothing is repaired.
///
/// The tuple field is public so that tests and the other modules can address raw bytes; a
/// page is obtained from [`Page::empty`] or [`Page::from_bytes`] rather than assembled byte by
/// byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page(pub [u8; PAGE_SIZE]);

impl Page {
    /// A fresh page: header filled for `kind` and `id`, `flags` 0, `page_lsn` 0, checksum 0,
    /// `slot_count` 0, `lower` 32, and the bytes 32..8192 left at zero.
    pub(crate) fn empty(kind: PageKind, id: PageId) -> Self {
        let mut page = Self([0u8; PAGE_SIZE]);
        page.0[OFF_MAGIC..OFF_MAGIC + MAGIC.len()].copy_from_slice(&MAGIC);
        page.0[OFF_FORMAT_VERSION] = FORMAT_VERSION;
        page.0[OFF_KIND] = kind.as_byte();
        page.write_u64(OFF_PAGE_ID, id.0);
        page.write_u16(OFF_LOWER, HEADER_SIZE as u16);
        page
    }

    /// Reads a page from a buffer of 8 192 bytes, checking the header and the checksum.
    pub(crate) fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Result<Self, InternalError> {
        let page = Self(bytes);
        page.verify()?;
        Ok(page)
    }

    /// The format version stored in the header, whether or not this build reads it.
    pub(crate) fn format_version(&self) -> u8 {
        self.0[OFF_FORMAT_VERSION]
    }

    /// The kind stored in the header, or [`InternalError::Corruption`] for a byte outside
    /// [`PageKind`].
    pub(crate) fn kind(&self) -> Result<PageKind, InternalError> {
        PageKind::from_byte(self.0[OFF_KIND])
    }

    /// Overwrites the kind byte.
    pub(crate) fn set_kind(&mut self, kind: PageKind) {
        self.0[OFF_KIND] = kind.as_byte();
    }

    /// The flag word. Pages built by [`Page::empty`] carry 0; no flag is defined.
    pub(crate) fn flags(&self) -> u16 {
        self.read_u16(OFF_FLAGS)
    }

    /// The identifier stored in the header.
    pub(crate) fn page_id(&self) -> PageId {
        PageId(self.read_u64(OFF_PAGE_ID))
    }

    /// The LSN stored in the header. `Lsn(0)` means never logged.
    pub(crate) fn lsn(&self) -> Lsn {
        Lsn(self.read_u64(OFF_PAGE_LSN))
    }

    /// Overwrites the LSN.
    pub(crate) fn set_lsn(&mut self, lsn: Lsn) {
        self.write_u64(OFF_PAGE_LSN, lsn.0);
    }

    /// The checksum field as it stands in the header, without recomputing it.
    pub(crate) fn checksum_field(&self) -> u32 {
        self.read_u32(OFF_CHECKSUM)
    }

    /// The number of slots declared by the header.
    pub(crate) fn slot_count(&self) -> u16 {
        self.read_u16(OFF_SLOT_COUNT)
    }

    /// Overwrites the number of slots.
    pub(crate) fn set_slot_count(&mut self, slots: u16) {
        self.write_u16(OFF_SLOT_COUNT, slots);
    }

    /// The first free byte after the header and the kind-specific directory.
    pub(crate) fn lower(&self) -> u16 {
        self.read_u16(OFF_LOWER)
    }

    /// Overwrites the `lower` boundary.
    pub(crate) fn set_lower(&mut self, lower: u16) {
        self.write_u16(OFF_LOWER, lower);
    }

    /// Writes the checksum of the page into its header.
    pub(crate) fn seal(&mut self) {
        let checksum = self.checksum();
        self.write_u32(OFF_CHECKSUM, checksum);
    }

    /// Checks the header and the checksum of a page read back from a file.
    ///
    /// The header is checked first, so a page whose magic or version is wrong is reported as
    /// such rather than as a checksum mismatch.
    pub(crate) fn verify(&self) -> Result<(), InternalError> {
        self.verify_header()?;
        let stored = self.checksum_field();
        let computed = self.checksum();
        if stored != computed {
            return Err(InternalError::Corruption(format!(
                "page {} carries checksum {stored:#010x}, its bytes hash to {computed:#010x}",
                self.page_id()
            )));
        }
        Ok(())
    }

    /// Checks the magic, the format version and the kind byte.
    fn verify_header(&self) -> Result<(), InternalError> {
        let magic = [
            self.0[OFF_MAGIC],
            self.0[OFF_MAGIC + 1],
            self.0[OFF_MAGIC + 2],
            self.0[OFF_MAGIC + 3],
        ];
        if magic != MAGIC {
            return Err(InternalError::Corruption(format!(
                "page {} starts with magic {magic:02x?}, expected {MAGIC:02x?}",
                self.page_id()
            )));
        }
        let version = self.format_version();
        if version != FORMAT_VERSION {
            return Err(InternalError::Corruption(format!(
                "page {} declares format version {version}, this build reads {FORMAT_VERSION}",
                self.page_id()
            )));
        }
        self.kind()?;
        Ok(())
    }

    /// CRC-32 of the 8 192 bytes of the page, its checksum field read as four zero bytes.
    fn checksum(&self) -> u32 {
        let mut register = Crc32::new();
        register.update(&self.0[..OFF_CHECKSUM]);
        register.update(&[0u8; 4]);
        register.update(&self.0[OFF_CHECKSUM + 4..]);
        register.finish()
    }

    /// Reads a little-endian `u16` at `at`.
    fn read_u16(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.0[at], self.0[at + 1]])
    }

    /// Reads a little-endian `u32` at `at`.
    fn read_u32(&self, at: usize) -> u32 {
        u32::from_le_bytes([self.0[at], self.0[at + 1], self.0[at + 2], self.0[at + 3]])
    }

    /// Reads a little-endian `u64` at `at`.
    fn read_u64(&self, at: usize) -> u64 {
        u64::from_le_bytes([
            self.0[at],
            self.0[at + 1],
            self.0[at + 2],
            self.0[at + 3],
            self.0[at + 4],
            self.0[at + 5],
            self.0[at + 6],
            self.0[at + 7],
        ])
    }

    /// Writes a little-endian `u16` at `at`.
    fn write_u16(&mut self, at: usize, value: u16) {
        self.0[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    /// Writes a little-endian `u32` at `at`.
    fn write_u32(&mut self, at: usize, value: u32) {
        self.0[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Writes a little-endian `u64` at `at`.
    fn write_u64(&mut self, at: usize, value: u64) {
        self.0[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page sealed with a payload, the shape used by several tests below.
    fn sealed_heap_page() -> Page {
        let mut page = Page::empty(PageKind::Heap, PageId(7));
        page.set_lsn(Lsn(3));
        page.0[HEADER_SIZE..HEADER_SIZE + 5].copy_from_slice(b"hello");
        page.seal();
        page
    }

    #[test]
    fn page_is_exactly_8kib() {
        assert_eq!(PAGE_SIZE, 8192);
        assert_eq!(size_of::<Page>(), 8192);
        assert_eq!(Page::empty(PageKind::Free, PageId(0)).0.len(), 8192);
    }

    #[test]
    fn empty_page_writes_the_documented_header() {
        let page = Page::empty(PageKind::Meta, PageId(0x0102_0304_0506_0708));
        assert_eq!(&page.0[0..4], b"VDBP");
        assert_eq!(&page.0[0..4], &[0x56, 0x44, 0x42, 0x50]);
        assert_eq!(page.format_version(), 1);
        assert_eq!(page.0[5], PageKind::Meta.as_byte());
        assert_eq!(page.flags(), 0);
        // Little-endian: the low byte of the identifier comes first.
        assert_eq!(page.0[8], 0x08);
        assert_eq!(page.page_id(), PageId(0x0102_0304_0506_0708));
        assert_eq!(page.lsn(), Lsn(0));
        assert_eq!(page.checksum_field(), 0);
        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.lower(), 32);
        assert_eq!(HEADER_SIZE, 32);
        assert!(page.0[HEADER_SIZE..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn header_accessors_write_where_the_table_says() {
        let mut page = Page::empty(PageKind::Free, PageId(1));
        page.set_kind(PageKind::BTreeLeaf);
        page.set_lsn(Lsn(0x0A0B_0C0D_0E0F_1011));
        page.set_slot_count(0x0203);
        page.set_lower(0x0405);
        assert_eq!(page.0[5], 4);
        assert_eq!(
            &page.0[16..24],
            &[0x11, 0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A]
        );
        assert_eq!(&page.0[28..30], &[0x03, 0x02]);
        assert_eq!(&page.0[30..32], &[0x05, 0x04]);
        assert_eq!(
            page.kind().expect("kind byte 4 is BTreeLeaf"),
            PageKind::BTreeLeaf
        );
        assert_eq!(page.slot_count(), 0x0203);
        assert_eq!(page.lower(), 0x0405);
    }

    #[test]
    fn page_seal_then_verify() {
        let page = sealed_heap_page();
        assert!(page.verify().is_ok());
        assert_eq!(
            page.kind().expect("kind of a sealed heap page"),
            PageKind::Heap
        );
        assert_eq!(page.page_id(), PageId(7));
        assert_eq!(page.lsn(), Lsn(3));
        assert_ne!(page.checksum_field(), 0);
        // `from_bytes` accepts the same buffer.
        Page::from_bytes(page.0).expect("a sealed page reads back");

        let mut flipped = page.clone();
        flipped.0[HEADER_SIZE] ^= 0xFF;
        let err = flipped
            .verify()
            .expect_err("a flipped payload byte is corruption");
        assert!(
            matches!(err, InternalError::Corruption(_)),
            "expected Corruption, got {err:?}"
        );
        assert!(
            err.to_string().contains("page 7"),
            "the message should name the PageId: {err}"
        );
        assert!(Page::from_bytes(flipped.0).is_err());
    }

    #[test]
    fn unknown_page_kind_is_corruption() {
        let mut page = Page::empty(PageKind::Heap, PageId(7));
        page.0[5] = 99;
        // Sealed after the change, so the checksum agrees and the kind is what fails.
        page.seal();
        let err = page.kind().expect_err("99 is outside PageKind");
        assert!(err.to_string().contains("99"), "{err}");
        let err = page
            .verify()
            .expect_err("the header check rejects the kind byte");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("99"), "{err}");
        let err = Page::from_bytes(page.0).expect_err("from_bytes rejects the kind byte");
        assert!(err.to_string().contains("99"), "{err}");
    }

    #[test]
    fn page_kind_bytes_are_part_of_the_format() {
        let kinds = [
            (PageKind::Free, 0u8),
            (PageKind::Heap, 1),
            (PageKind::Overflow, 2),
            (PageKind::BTreeInternal, 3),
            (PageKind::BTreeLeaf, 4),
            (PageKind::Meta, 5),
            (PageKind::RowIdDir, 6),
        ];
        for (kind, byte) in kinds {
            assert_eq!(kind.as_byte(), byte);
            assert_eq!(PageKind::from_byte(byte).expect("byte in the enum"), kind);
        }
        assert!(PageKind::from_byte(7).is_err());
        assert!(PageKind::from_byte(u8::MAX).is_err());
    }

    #[test]
    fn bad_magic_is_corruption_naming_the_bytes() {
        let mut page = sealed_heap_page();
        page.0[0] = b'X';
        page.seal();
        let err = page.verify().expect_err("magic `XDBP` is not the magic");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        // `58` is the ASCII code of `X`, printed as the value that was read.
        assert!(err.to_string().contains("58"), "{err}");
    }

    #[test]
    fn other_format_version_is_corruption_naming_the_version() {
        let mut page = sealed_heap_page();
        page.0[4] = 2;
        page.seal();
        let err = page
            .verify()
            .expect_err("version 2 is not read by this build");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("format version 2"), "{err}");
        assert_eq!(page.format_version(), 2);
    }

    #[test]
    fn lsn_zero_means_unlogged() {
        assert_eq!(Lsn(0).to_string(), "0");
        assert_eq!(Lsn(u64::MAX).to_string(), u64::MAX.to_string());
        assert_eq!(format!("{:?}", Lsn(3)), "Lsn(3)");
        assert_eq!(Page::empty(PageKind::Heap, PageId(1)).lsn(), Lsn(0));

        // The sentence is required in the rustdoc of `Lsn`, not merely somewhere in the file:
        // look at the contiguous block of `///` lines that precedes the declaration.
        let source = include_str!("page.rs");
        let (before_declaration, _) = source
            .split_once("pub(crate) struct Lsn")
            .expect("the declaration of Lsn");
        let doc_block: Vec<&str> = before_declaration
            .lines()
            .rev()
            .take_while(|line| {
                let line = line.trim_start();
                line.starts_with("///") || line.starts_with("#[")
            })
            .filter(|line| line.trim_start().starts_with("///"))
            .collect();
        assert!(!doc_block.is_empty(), "Lsn should carry a rustdoc block");
        assert!(
            doc_block
                .iter()
                .any(|line| line.contains("0 means never logged")),
            "the rustdoc of Lsn should contain the sentence"
        );
    }

    #[test]
    fn page_ids_display_as_bare_integers() {
        assert_eq!(PageId(0).to_string(), "0");
        assert_eq!(PageId(7).to_string(), "7");
        assert_eq!(format!("{:?}", PageId(7)), "PageId(7)");
        assert!(PageId(1) < PageId(2));
        assert!(Lsn(1) < Lsn(2));
    }

    #[test]
    fn a_flipped_byte_of_the_header_is_caught_too() {
        // `slot_count` is inside the checksummed range, unlike the checksum field itself.
        let mut page = sealed_heap_page();
        page.set_slot_count(1);
        assert!(page.verify().is_err());
        page.seal();
        assert!(page.verify().is_ok());
    }
}
