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
