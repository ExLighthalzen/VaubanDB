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
    let source = include_str!("../../page.rs");
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
