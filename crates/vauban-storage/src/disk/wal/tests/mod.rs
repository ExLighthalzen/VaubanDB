//! Tests of the write-ahead journal: appending records and reading them back, the layout
//! and checksum of a record on disk, durability across a reopen, and what [`Wal::open`]
//! decides when a file was left mid-append or carries a damaged record — truncation when
//! nothing readable follows it, [`InternalError::Corruption`] otherwise. Also covered: the
//! checkpoint payload and [`WalHandle`], the shared handle around one journal.

use super::super::temp::TempDir;
use super::*;

/// A journal in a temporary directory, with the guard that removes it.
fn journal(label: &str) -> (TempDir, Wal) {
    let dir = TempDir::created(label);
    let wal = Wal::open(&dir.child("wal")).expect("open a journal");
    (dir, wal)
}

/// The records of a journal, or the error that stopped the reading.
fn records(wal: &Wal) -> Vec<WalRecord> {
    wal.iter()
        .expect("open the journal for reading")
        .collect::<Result<Vec<_>, _>>()
        .expect("read the records back")
}

/// Size of the journal on disk.
fn size(path: &Path) -> u64 {
    std::fs::metadata(path).expect("the journal exists").len()
}

/// Appends raw bytes past the end of the journal, as a torn append would leave them.
fn append_bytes(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open the journal to append raw bytes");
    file.write_all(bytes).expect("append raw bytes");
}

/// Flips the byte at `at` in the journal.
fn flip(path: &Path, at: usize) {
    let mut bytes = std::fs::read(path).expect("read the journal");
    bytes[at] ^= 0xFF;
    std::fs::write(path, bytes).expect("write the journal back");
}

/// A record built field by field, checksum sealed over what the fields say.
///
/// Lets the tests write a record that [`WalRecord::encode`] would not: another version,
/// a kind outside the enum, a number that does not continue the chain.
fn raw_record(version: u8, kind: u8, lsn: u64, prev_lsn: u64, txn: u64, payload: &[u8]) -> Vec<u8> {
    let length = HEADER_LEN + payload.len() + CHECKSUM_LEN;
    let mut bytes = Vec::with_capacity(LENGTH_LEN + length);
    bytes.extend_from_slice(&(length as u32).to_le_bytes());
    bytes.push(version);
    bytes.push(kind);
    bytes.extend_from_slice(&lsn.to_le_bytes());
    bytes.extend_from_slice(&prev_lsn.to_le_bytes());
    bytes.extend_from_slice(&txn.to_le_bytes());
    bytes.extend_from_slice(payload);
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes
}

// --- Checkpoint payload -------------------------------------------------------------

#[test]
fn a_checkpoint_payload_is_three_little_endian_u64() {
    let payload = CheckpointPayload {
        redo_lsn: Lsn(12),
        next_page_id: PageId(34),
        next_row_id: 56,
    };
    let bytes = payload.encode();

    assert_eq!(CHECKPOINT_PAYLOAD_LEN, 24);
    assert_eq!(bytes.len(), CHECKPOINT_PAYLOAD_LEN);
    assert_eq!(&bytes[0..8], &12u64.to_le_bytes());
    assert_eq!(&bytes[8..16], &34u64.to_le_bytes());
    assert_eq!(&bytes[16..24], &56u64.to_le_bytes());
    assert_eq!(CheckpointPayload::decode(&bytes).expect("decode"), payload);
}

#[test]
fn a_checkpoint_payload_of_another_length_is_corruption() {
    for len in [0, CHECKPOINT_PAYLOAD_LEN - 1, CHECKPOINT_PAYLOAD_LEN + 1] {
        let err = CheckpointPayload::decode(&vec![0u8; len])
            .expect_err("a payload of another length is refused");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(
            err.to_string().contains(&format!("{len} payload bytes")),
            "{err}"
        );
    }
}

#[test]
fn the_payload_of_a_checkpoint_names_the_record_itself() {
    let (dir, mut wal) = journal("wal-checkpoint-names-itself");
    wal.append(WalRecordKind::Begin, TxnId(3), &[])
        .expect("append a begin");

    let lsn = wal
        .append_checkpoint(PageId(7), 9)
        .expect("append the checkpoint record");

    assert_eq!(lsn, Lsn(2), "the record follows the begin");
    let written = records(&wal);
    assert_eq!(written.len(), 2);
    let record = &written[1];
    assert_eq!(record.kind, WalRecordKind::Checkpoint);
    assert_eq!(record.lsn, lsn);
    assert_eq!(
        record.txn,
        TxnId(0),
        "a checkpoint belongs to no transaction"
    );
    let payload = CheckpointPayload::decode(&record.payload).expect("decode the payload");
    assert_eq!(
        payload,
        CheckpointPayload {
            redo_lsn: lsn,
            next_page_id: PageId(7),
            next_row_id: 9,
        }
    );
    // The record is in the file; `Wal::append_checkpoint` leaves the `sync_all` to the
    // handle, so the durable LSN is still the one of before.
    assert_eq!(wal.durable_lsn(), Lsn(0));
    assert_eq!(size(&dir.child("wal")), wal.append_offset());
}

#[test]
fn the_handle_syncs_the_checkpoint_record_it_appends() {
    let dir = TempDir::created("wal-checkpoint-handle");
    let handle = WalHandle::open(&dir.child("wal")).expect("open the journal");

    let lsn = handle
        .append_checkpoint(PageId(1), 2)
        .expect("append the checkpoint record");

    assert_eq!(lsn, Lsn(1));
    assert_eq!(handle.durable_lsn(), lsn, "the record was synced");
    let written = handle.records().expect("read the journal");
    assert_eq!(kinds_of(&written), vec![WalRecordKind::Checkpoint]);
    assert_eq!(
        CheckpointPayload::decode(&written[0].payload)
            .expect("decode the payload")
            .redo_lsn,
        lsn
    );
}

/// The kind of each record, in order.
fn kinds_of(records: &[WalRecord]) -> Vec<WalRecordKind> {
    records.iter().map(|record| record.kind).collect()
}

#[test]
fn open_creates_an_empty_journal() {
    let dir = TempDir::created("wal-create");
    let path = dir.child("wal");
    assert!(!path.exists());
    let wal = Wal::open(&path).expect("create a journal");
    assert!(path.is_file());
    assert_eq!(size(&path), 0);
    assert_eq!(wal.path(), path);
    assert_eq!(wal.append_offset(), 0);
    assert_eq!(wal.last_lsn(), Lsn(0));
    assert_eq!(wal.durable_lsn(), Lsn(0));
    assert_eq!(wal.records(), 0);
    assert!(records(&wal).is_empty());
}

#[test]
fn append_then_iter() {
    let (dir, mut wal) = journal("append-then-iter");
    assert_eq!(
        wal.append(WalRecordKind::Begin, TxnId(9), &[])
            .expect("append Begin"),
        Lsn(1)
    );
    assert_eq!(
        wal.append(WalRecordKind::Insert, TxnId(9), &[1, 2, 3])
            .expect("append Insert"),
        Lsn(2)
    );
    assert_eq!(
        wal.append(WalRecordKind::Commit, TxnId(9), &[])
            .expect("append Commit"),
        Lsn(3)
    );
    assert_eq!(wal.last_lsn(), Lsn(3));
    assert_eq!(wal.records(), 3);

    let read = records(&wal);
    assert_eq!(read.len(), 3);
    assert_eq!(
        read.iter().map(|record| record.kind).collect::<Vec<_>>(),
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Commit
        ]
    );
    assert_eq!(
        read.iter().map(|record| record.lsn).collect::<Vec<_>>(),
        vec![Lsn(1), Lsn(2), Lsn(3)]
    );
    // Chained: each record names the one before it, the first names `Lsn(0)`.
    assert_eq!(
        read.iter()
            .map(|record| record.prev_lsn)
            .collect::<Vec<_>>(),
        vec![Lsn(0), Lsn(1), Lsn(2)]
    );
    assert_eq!(
        read.iter().map(|record| record.txn).collect::<Vec<_>>(),
        vec![TxnId(9), TxnId(9), TxnId(9)]
    );
    assert_eq!(read[0].payload, Vec::<u8>::new());
    assert_eq!(read[1].payload, vec![1, 2, 3]);
    assert_eq!(read[2].payload, Vec::<u8>::new());

    // Two records with an empty payload and one with three bytes: 34 + 37 + 34.
    assert_eq!(wal.append_offset(), 105);
    assert_eq!(size(&dir.child("wal")), 105);
}

#[test]
fn record_bytes_follow_the_documented_layout() {
    let (dir, mut wal) = journal("wal-layout");
    wal.append(WalRecordKind::Insert, TxnId(0x0102_0304_0506_0708), &[7, 8])
        .expect("append Insert");
    wal.flush().expect("flush");
    let bytes = std::fs::read(dir.child("wal")).expect("read the journal");

    assert_eq!(bytes.len(), 36, "34 bytes of frame and two of payload");
    // `length`: the bytes after the field itself, checksum included.
    assert_eq!(read_u32(&bytes, 0), 32);
    assert_eq!(bytes[4], 1, "version");
    assert_eq!(bytes[5], WalRecordKind::Insert.as_byte());
    assert_eq!(bytes[5], 2);
    assert_eq!(read_u64(&bytes, 6), 1, "lsn");
    assert_eq!(read_u64(&bytes, 14), 0, "prev_lsn of the first record");
    assert_eq!(read_u64(&bytes, 22), 0x0102_0304_0506_0708, "txn");
    // Little-endian: the low byte of the transaction comes first.
    assert_eq!(bytes[22], 0x08);
    assert_eq!(&bytes[30..32], &[7, 8], "payload");
    assert_eq!(
        read_u32(&bytes, 32),
        crc32(&bytes[..32]),
        "the checksum covers the record but its own four bytes"
    );
}

#[test]
fn flush_is_durable_across_reopen() {
    let (dir, mut wal) = journal("wal-flush");
    wal.append(WalRecordKind::Begin, TxnId(1), &[])
        .expect("append Begin");
    wal.append(WalRecordKind::Insert, TxnId(1), &[4, 5])
        .expect("append Insert");
    assert_eq!(wal.durable_lsn(), Lsn(0), "nothing was flushed yet");
    wal.flush().expect("flush");
    assert_eq!(wal.durable_lsn(), Lsn(2));
    let written = records(&wal);
    drop(wal);

    let mut reopened = Wal::open(&dir.child("wal")).expect("reopen the journal");
    assert_eq!(records(&reopened), written);
    assert_eq!(reopened.last_lsn(), Lsn(2));
    assert_eq!(reopened.records(), 2);
    // `open` ends with a `sync_all`, so it reports the record it read as durable.
    assert_eq!(reopened.durable_lsn(), Lsn(2));
    assert_eq!(reopened.append_offset(), size(&dir.child("wal")));

    // The numbering carries on from the file rather than starting over.
    assert_eq!(
        reopened
            .append(WalRecordKind::Commit, TxnId(1), &[])
            .expect("append after the reopen"),
        Lsn(3)
    );
    let after = records(&reopened);
    assert_eq!(after.len(), 3);
    assert_eq!(after[2].prev_lsn, Lsn(2));
}

#[test]
fn unflushed_tail_may_vanish() {
    // `Drop` does not flush, so that a crash test can kill a process holding records that
    // were never synced. The source is asserted here rather than only in the rustdoc: a
    // `Drop` that synced would make this file lie.
    let source = include_str!("../../wal.rs");
    let drop_impl = format!("impl {} for Wal", "Drop");
    assert!(
        !source.contains(&drop_impl),
        "wal.rs should carry no {drop_impl}"
    );
    assert!(
        !source.contains(&format!("impl {} for WalIter", "Drop")),
        "the iterator should not sync either"
    );

    let (dir, mut wal) = journal("wal-unflushed");
    let first = wal
        .append(WalRecordKind::Begin, TxnId(2), &[])
        .expect("append Begin");
    let second = wal
        .append(WalRecordKind::Insert, TxnId(2), &[6])
        .expect("append Insert");
    assert_eq!((first, second), (Lsn(1), Lsn(2)));
    assert_eq!(wal.durable_lsn(), Lsn(0), "no flush, nothing durable");
    let written = records(&wal);
    drop(wal);

    // Without a flush the two records are not promised to be there. What is asserted is
    // that the journal reopens on a prefix of what was appended: this process wrote the
    // bytes through `write_all`, so the operating system usually hands them back whole,
    // and the test does not require the loss it allows.
    let reopened = Wal::open(&dir.child("wal")).expect("reopen the journal");
    let read = records(&reopened);
    assert!(read.len() <= written.len(), "{} records", read.len());
    assert_eq!(read, written[..read.len()]);
    assert_eq!(reopened.last_lsn(), Lsn(read.len() as u64));
    assert_eq!(reopened.records(), read.len() as u64);
}

#[test]
fn drop_does_not_flush_says_the_rustdoc_of_wal() {
    // The sentence is required in the rustdoc of `Wal`, not merely somewhere in the file:
    // look at the contiguous block of `///` lines that precedes the declaration.
    let source = include_str!("../../wal.rs");
    let (before_declaration, _) = source
        .split_once("pub(crate) struct Wal {")
        .expect("the declaration of Wal");
    let doc_block: Vec<&str> = before_declaration
        .lines()
        .rev()
        .take_while(|line| {
            let line = line.trim_start();
            line.starts_with("///") || line.starts_with("#[")
        })
        .filter(|line| line.trim_start().starts_with("///"))
        .collect();
    assert!(!doc_block.is_empty(), "Wal should carry a rustdoc block");
    assert!(
        doc_block
            .iter()
            .any(|line| line.contains("does not flush it")),
        "the rustdoc of Wal should say that dropping it does not flush"
    );
}

#[test]
fn truncated_last_record_is_discarded() {
    let (dir, mut wal) = journal("wal-torn");
    wal.append(WalRecordKind::Begin, TxnId(3), &[])
        .expect("append Begin");
    wal.flush().expect("flush");
    let written = records(&wal);
    let whole = wal.append_offset();
    drop(wal);

    // Four bytes announcing a record of 100, and not one byte of it.
    append_bytes(&dir.child("wal"), &100u32.to_le_bytes());
    assert_eq!(size(&dir.child("wal")), whole + 4);

    let mut reopened = Wal::open(&dir.child("wal")).expect("the torn tail is not an error");
    assert_eq!(records(&reopened), written);
    assert_eq!(reopened.last_lsn(), Lsn(1));
    assert_eq!(reopened.records(), 1);
    assert_eq!(
        size(&dir.child("wal")),
        whole,
        "the file is truncated to the end of the last whole record"
    );
    assert_eq!(reopened.append_offset(), whole);
    // The next record goes where the torn bytes were.
    assert_eq!(
        reopened
            .append(WalRecordKind::Commit, TxnId(3), &[])
            .expect("append over the truncated tail"),
        Lsn(2)
    );
    assert_eq!(records(&reopened).len(), 2);
}

#[test]
fn a_zero_filled_tail_is_discarded() {
    let (dir, mut wal) = journal("wal-zeros");
    wal.append(WalRecordKind::Begin, TxnId(3), &[])
        .expect("append Begin");
    wal.flush().expect("flush");
    let whole = wal.append_offset();
    drop(wal);

    // Eight zero bytes: the length field frames no record, and 0 is not a kind either.
    append_bytes(&dir.child("wal"), &[0u8; 8]);
    let reopened = Wal::open(&dir.child("wal")).expect("a zero-filled tail is not an error");
    assert_eq!(reopened.records(), 1);
    assert_eq!(size(&dir.child("wal")), whole);
}

#[test]
fn crc_mismatch_interior_is_corruption() {
    let (dir, mut wal) = journal("wal-interior-crc");
    wal.append(WalRecordKind::Insert, TxnId(4), &[1, 2, 3])
        .expect("append Insert");
    wal.append(WalRecordKind::Commit, TxnId(4), &[])
        .expect("append Commit");
    wal.flush().expect("flush");
    let full = wal.append_offset();
    drop(wal);

    // Offset 30 is the first payload byte of the first record.
    flip(&dir.child("wal"), 30);
    let err = Wal::open(&dir.child("wal")).expect_err("a damaged interior record");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("checksum"), "{err}");
    assert!(err.to_string().contains("offset 0"), "{err}");
    assert_eq!(
        size(&dir.child("wal")),
        full,
        "a refused journal is left as it stands"
    );
}

#[test]
fn crc_mismatch_of_the_last_record_is_discarded() {
    // The counterpart of the test above, and what makes the two cases part on the position
    // rather than on the checksum alone: a byte flipped inside the last record leaves a
    // tail this build drops, where a byte flipped inside a record that has a whole record
    // after it is an error.
    let (dir, mut wal) = journal("wal-tail-crc");
    wal.append(WalRecordKind::Insert, TxnId(4), &[1, 2, 3])
        .expect("append Insert");
    let first = wal.append_offset();
    wal.append(WalRecordKind::Commit, TxnId(4), &[])
        .expect("append Commit");
    wal.flush().expect("flush");
    let written = records(&wal);
    drop(wal);

    // Offset `first + 6` is the first byte of the `lsn` of the second record.
    flip(&dir.child("wal"), first as usize + 6);
    let reopened = Wal::open(&dir.child("wal")).expect("a damaged tail record is dropped");
    assert_eq!(records(&reopened), written[..1]);
    assert_eq!(reopened.last_lsn(), Lsn(1));
    assert_eq!(size(&dir.child("wal")), first);
}

/// A journal of three records of 34 bytes each, flushed, in a temporary directory.
///
/// The three records carry an empty payload, so each of them is a frame of 34 bytes and the
/// file is 102 bytes long: the offsets of the records are 0, 34 and 68, which is what the
/// tests below flip bytes at.
fn journal_of_three_records(label: &str) -> (TempDir, Vec<WalRecord>, Vec<u8>) {
    let dir = TempDir::created(label);
    let mut wal = Wal::open(&dir.child("wal")).expect("open a journal");
    for kind in [
        WalRecordKind::Begin,
        WalRecordKind::Insert,
        WalRecordKind::Commit,
    ] {
        wal.append(kind, TxnId(8), &[]).expect("append a record");
    }
    wal.flush().expect("flush");
    assert_eq!(wal.append_offset(), 102);
    let written = records(&wal);
    drop(wal);
    let bytes = std::fs::read(dir.child("wal")).expect("read the journal");
    assert_eq!(bytes.len(), 102);
    (dir, written, bytes)
}

#[test]
fn an_interior_length_that_overshoots_the_file_is_corruption() {
    // The `length` of the second record of three goes from 30
    // to 65 310 by the flip of its second byte. The frame then announces more bytes than
    // the file holds, which is not something the scan can step over — and the third record
    // is whole and its checksum agrees, so the second is an interior record, not the tail
    // of an append that did not finish.
    let (dir, _written, bytes) = journal_of_three_records("wal-interior-length");
    let path = dir.child("wal");
    assert_eq!(read_u32(&bytes, 34), 30, "the length of the second record");
    flip(&path, 35);
    assert_eq!(
        read_u32(&std::fs::read(&path).expect("read the journal"), 34),
        65_310
    );

    let err = Wal::open(&path).expect_err("an interior record that cannot be framed");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("offset 34"), "{err}");
    assert!(err.to_string().contains("offset 68"), "{err}");
    assert!(err.to_string().contains("interior record"), "{err}");
    assert_eq!(
        size(&path),
        102,
        "a refused journal keeps the records that follow the damage"
    );
}

#[test]
fn an_interior_length_that_frames_no_record_is_corruption() {
    // The other unusable frame: four zero bytes in place of the `length` of the second
    // record of three. Nothing to step over either, and the third record checks out.
    let (dir, _written, _bytes) = journal_of_three_records("wal-interior-zeros");
    let path = dir.child("wal");
    let mut bytes = std::fs::read(&path).expect("read the journal");
    bytes[34..38].fill(0);
    std::fs::write(&path, &bytes).expect("write the journal back");

    let err = Wal::open(&path).expect_err("an interior length that frames no record");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("offset 34"), "{err}");
    assert!(err.to_string().contains("offset 68"), "{err}");
    assert_eq!(size(&path), 102);
}

#[test]
fn one_flipped_byte_of_a_record_that_has_a_whole_record_after_it_is_corruption() {
    // Swept on one shape: a journal of three records of 34 bytes, the eight bits of one
    // byte flipped at a time over the 68 bytes of the first two records, 68 journals in all.
    // Each flip leaves the third record whole with an agreeing checksum, so each of these 68
    // names an interior record, in whichever field of those two records the byte sat — the
    // `length` as much as the body or the checksum. Flipping eight bits is a coarse vector:
    // a `length` mangled that way lands outside the bounds of a record, where one bit of it
    // can leave a frame that still fits in the file. That finer sweep is
    // `one_flipped_bit_does_not_truncate_over_a_sealed_record`, below.
    for at in 0..68usize {
        let (dir, _written, _bytes) = journal_of_three_records(&format!("wal-sweep-interior-{at}"));
        let path = dir.child("wal");
        flip(&path, at);
        // The record the flip landed in, named by the error.
        let hit = if at < 34 { "offset 0" } else { "offset 34" };
        match Wal::open(&path) {
            Err(InternalError::Corruption(message)) => {
                assert!(message.contains(hit), "offset {at}: {message}");
                assert!(
                    message.contains("interior record"),
                    "offset {at}: {message}"
                );
            }
            Err(other) => panic!("offset {at}: expected Corruption, got {other:?}"),
            Ok(accepted) => panic!(
                "offset {at}: open accepted the journal and kept {} of the 3 records",
                accepted.records()
            ),
        }
        assert_eq!(
            size(&path),
            102,
            "offset {at}: the file is left as it stands"
        );
    }
}

#[test]
fn one_flipped_byte_of_the_last_record_leaves_the_records_before_it() {
    // The counterpart, on the same shape: the 34 bytes of the third record, one flip at a
    // time. Nothing whole follows any of them, so each is the tail of an append this build
    // drops, and the two records before it come back.
    for at in 68..102usize {
        let (dir, written, _bytes) = journal_of_three_records(&format!("wal-sweep-tail-{at}"));
        let path = dir.child("wal");
        flip(&path, at);
        let reopened = Wal::open(&path)
            .unwrap_or_else(|err| panic!("offset {at}: open should truncate, got {err}"));
        assert_eq!(records(&reopened), written[..2], "offset {at}");
        assert_eq!(reopened.last_lsn(), Lsn(2), "offset {at}");
        assert_eq!(size(&path), 68, "offset {at}");
    }
}

#[test]
fn an_unknown_kind_is_corruption_even_at_the_end() {
    // Checksum sealed over the fields, so the record was written whole: nothing was lost
    // in a torn append, this build simply does not know kind 99. This is the vector that
    // separates the rule from "anything the last record carries is dropped".
    let dir = TempDir::created("wal-unknown-kind");
    let path = dir.child("wal");
    std::fs::write(&path, raw_record(RECORD_VERSION, 99, 1, 0, 5, &[])).expect("write");
    let err = Wal::open(&path).expect_err("kind 99 is outside WalRecordKind");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("99"), "{err}");
    assert_eq!(size(&path), 34, "the file is left as it stands");
}

#[test]
fn another_format_version_is_corruption() {
    let dir = TempDir::created("wal-version");
    let path = dir.child("wal");
    std::fs::write(
        &path,
        raw_record(2, WalRecordKind::Begin.as_byte(), 1, 0, 5, &[]),
    )
    .expect("write");
    let err = Wal::open(&path).expect_err("version 2 is not read by this build");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("format version 2"), "{err}");
}

#[test]
fn a_number_that_does_not_continue_the_chain_is_corruption() {
    let dir = TempDir::created("wal-chain");
    let path = dir.child("wal");
    let kind = WalRecordKind::Begin.as_byte();

    // A journal whose first record is numbered 4 instead of 1.
    std::fs::write(&path, raw_record(RECORD_VERSION, kind, 4, 3, 1, &[])).expect("write");
    let err = Wal::open(&path).expect_err("a fresh journal starts at 1");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("lsn 4"), "{err}");

    // A second record that names another record than the one before it.
    let mut bytes = raw_record(RECORD_VERSION, kind, 1, 0, 1, &[]);
    bytes.extend_from_slice(&raw_record(RECORD_VERSION, kind, 2, 7, 1, &[]));
    std::fs::write(&path, bytes).expect("write");
    let err = Wal::open(&path).expect_err("prev_lsn 7 does not name the record before it");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("offset 34"), "{err}");
}

#[test]
fn a_payload_over_the_maximum_is_a_bug() {
    let (dir, mut wal) = journal("wal-too-long");
    let err = wal
        .append(
            WalRecordKind::Insert,
            TxnId(1),
            &vec![0u8; MAX_PAYLOAD_LEN + 1],
        )
        .expect_err("one byte more than a record holds");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(
        err.to_string().contains(&MAX_PAYLOAD_LEN.to_string()),
        "{err}"
    );
    assert_eq!(wal.append_offset(), 0, "nothing was written");
    assert_eq!(wal.last_lsn(), Lsn(0));

    // The payload of that size does fit, and fills a record of exactly 1 MiB.
    wal.append(WalRecordKind::Insert, TxnId(1), &vec![7u8; MAX_PAYLOAD_LEN])
        .expect("the largest payload a record holds");
    wal.flush().expect("flush");
    assert_eq!(wal.append_offset(), MAX_RECORD_LEN as u64);
    assert_eq!(size(&dir.child("wal")), 1_048_576);
    let read = records(&wal);
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].payload.len(), MAX_PAYLOAD_LEN);
    assert_eq!(MAX_PAYLOAD_LEN, 1_048_576 - 34);
}

#[test]
fn a_length_field_outside_the_bounds_frames_no_record() {
    // Read through `frame` rather than through `open`: at the end of a file both this and
    // a torn record stop the scan, so the two are told apart here.
    let announced = MAX_RECORD_LEN as u32;
    let mut bytes: &[u8] = &announced.to_le_bytes();
    match frame(&mut bytes, 8, LENGTH_LEN as u64).expect("read the length field") {
        Framed::Unframed { reason } => {
            assert!(reason.contains("offset 8"), "{reason}");
            assert!(reason.contains(&announced.to_string()), "{reason}");
        }
        other => panic!("a length of {announced} frames no record: {other:?}"),
    }

    // One byte less than the frame of a record with an empty payload.
    let mut bytes: &[u8] = &(MIN_LENGTH as u32 - 1).to_le_bytes();
    assert!(matches!(
        frame(&mut bytes, 0, LENGTH_LEN as u64).expect("read the length field"),
        Framed::Unframed { .. }
    ));

    // A length the format allows, with fewer bytes behind it than it announces: torn.
    let record = raw_record(RECORD_VERSION, WalRecordKind::Begin.as_byte(), 1, 0, 1, &[]);
    let mut bytes: &[u8] = &record[..20];
    match frame(&mut bytes, 0, 20).expect("read the length field") {
        Framed::Torn { reason } => {
            assert!(reason.contains("34 bytes"), "{reason}");
            assert!(reason.contains("holds 20"), "{reason}");
        }
        other => panic!("20 bytes of a record of 34: {other:?}"),
    }
}

#[test]
fn record_kind_bytes_are_part_of_the_format() {
    let kinds = [
        (WalRecordKind::Begin, 1u8),
        (WalRecordKind::Insert, 2),
        (WalRecordKind::Update, 3),
        (WalRecordKind::Delete, 4),
        (WalRecordKind::Commit, 5),
        (WalRecordKind::Abort, 6),
        (WalRecordKind::Checkpoint, 7),
        (WalRecordKind::CreateDatabase, 8),
        (WalRecordKind::DropDatabase, 9),
        (WalRecordKind::CreateTable, 10),
        (WalRecordKind::DropTable, 11),
        (WalRecordKind::CreateIndex, 12),
        (WalRecordKind::DropIndex, 13),
        (WalRecordKind::Savepoint, 14),
        (WalRecordKind::RollbackTo, 15),
        (WalRecordKind::Compensation, 16),
        (WalRecordKind::Purge, 17),
    ];
    assert_eq!(kinds.len(), 17);
    for (kind, byte) in kinds {
        assert_eq!(kind.as_byte(), byte);
        assert_eq!(
            WalRecordKind::from_byte(byte).expect("byte in the enum"),
            kind
        );
    }
    // 0 is not a kind, so a run of zero bytes does not read as one.
    assert!(WalRecordKind::from_byte(0).is_err());
    assert!(WalRecordKind::from_byte(18).is_err());
    let err = WalRecordKind::from_byte(u8::MAX).expect_err("255 is outside the enum");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("255"), "{err}");
}

#[test]
fn records_of_every_kind_make_the_round_trip() {
    let (_dir, mut wal) = journal("wal-kinds-roundtrip");
    let kinds = [
        WalRecordKind::Begin,
        WalRecordKind::Insert,
        WalRecordKind::Update,
        WalRecordKind::Delete,
        WalRecordKind::Commit,
        WalRecordKind::Abort,
        WalRecordKind::Checkpoint,
        WalRecordKind::CreateDatabase,
        WalRecordKind::DropDatabase,
        WalRecordKind::CreateTable,
        WalRecordKind::DropTable,
        WalRecordKind::CreateIndex,
        WalRecordKind::DropIndex,
        WalRecordKind::Savepoint,
        WalRecordKind::RollbackTo,
        WalRecordKind::Compensation,
        WalRecordKind::Purge,
    ];
    for (index, kind) in kinds.iter().enumerate() {
        let payload = vec![index as u8; index];
        let lsn = wal
            .append(*kind, TxnId(index as u64), &payload)
            .expect("append a record");
        assert_eq!(lsn, Lsn(index as u64 + 1));
    }
    wal.flush().expect("flush");
    let read = records(&wal);
    assert_eq!(read.len(), kinds.len());
    for (index, (record, kind)) in read.iter().zip(kinds).enumerate() {
        assert_eq!(record.kind, kind);
        assert_eq!(record.txn, TxnId(index as u64));
        assert_eq!(record.payload, vec![index as u8; index]);
    }
}

#[test]
fn iter_stops_at_the_records_the_wal_knows_about() {
    let (dir, mut wal) = journal("wal-iter-bound");
    wal.append(WalRecordKind::Begin, TxnId(1), &[])
        .expect("append Begin");
    wal.flush().expect("flush");
    // Bytes appended behind the back of the structure are past its `append_offset`, so
    // the iterator does not read them.
    append_bytes(
        &dir.child("wal"),
        &raw_record(
            RECORD_VERSION,
            WalRecordKind::Commit.as_byte(),
            2,
            1,
            1,
            &[],
        ),
    );
    assert_eq!(size(&dir.child("wal")), 68);
    assert_eq!(wal.append_offset(), 34);
    assert_eq!(records(&wal).len(), 1);
}

/// A flushed journal of one record per entry of `payloads`, each payload a run of `9`.
///
/// Returns the guard of the directory, the bytes of the file, the offsets the records start
/// at followed by the size of the file, and the records as [`Wal::iter`] read them.
fn journal_of(payloads: &[usize], label: &str) -> (TempDir, Vec<u8>, Vec<u64>, Vec<WalRecord>) {
    let dir = TempDir::created(label);
    let path = dir.child("wal");
    let mut wal = Wal::open(&path).expect("open a journal");
    let mut bounds = vec![0u64];
    for (index, payload) in payloads.iter().enumerate() {
        let kind = if index + 1 == payloads.len() {
            WalRecordKind::Commit
        } else {
            WalRecordKind::Insert
        };
        wal.append(kind, TxnId(1), &vec![9u8; *payload])
            .expect("append a record");
        bounds.push(wal.append_offset());
    }
    wal.flush().expect("flush");
    let written = records(&wal);
    drop(wal);
    let bytes = std::fs::read(&path).expect("read the journal");
    (dir, bytes, bounds, written)
}

/// Opens the journal of `payloads` once per bit of its file, that bit flipped, and asserts
/// what `open` answered each time.
///
/// A flip that lands in a record other than the last leaves whole records behind it, so the
/// journal is refused and left untouched; a flip in the last record leaves the tail of an
/// append that did not finish, so the records before it come back and the file is cut at the
/// end of the last of them. Returns how many journals fell in each of the two cases, and the
/// offsets of the shape, so that the caller can assert the shape it swept.
fn sweep_one_bit(payloads: &[usize], label: &str) -> (usize, usize, Vec<u64>) {
    let (_source, bytes, bounds, written) = journal_of(payloads, &format!("{label}-source"));
    let last = payloads.len() - 1;
    let work = TempDir::created(label);
    let path = work.child("wal");
    let mut refused = 0usize;
    let mut cut = 0usize;
    for at in 0..bytes.len() {
        for bit in 0..8u32 {
            let mut copy = bytes.clone();
            copy[at] ^= 1u8 << bit;
            std::fs::write(&path, &copy).expect("write the flipped journal");
            // Record the flipped byte belongs to.
            let hit = bounds
                .iter()
                .rposition(|start| *start <= at as u64)
                .expect("the byte sits in a record");
            if hit < last {
                let message = match Wal::open(&path) {
                    Err(InternalError::Corruption(message)) => message,
                    Err(other) => panic!("byte {at} bit {bit}: {other:?}"),
                    Ok(accepted) => panic!(
                        "byte {at} bit {bit}: open kept {} of the {} records and left a \
                         file of {} bytes out of the {} the journal held, over a record \
                         the flip did not touch",
                        accepted.records(),
                        payloads.len(),
                        size(&path),
                        bytes.len()
                    ),
                };
                assert!(
                    message.contains(&format!("offset {}", bounds[hit])),
                    "byte {at} bit {bit}: {message}"
                );
                assert!(
                    message.contains("interior record"),
                    "byte {at} bit {bit}: {message}"
                );
                assert_eq!(
                    size(&path),
                    bytes.len() as u64,
                    "byte {at} bit {bit}: a refused journal is left as it stands"
                );
                refused += 1;
            } else {
                let reopened =
                    Wal::open(&path).unwrap_or_else(|err| panic!("byte {at} bit {bit}: {err}"));
                assert_eq!(records(&reopened), written[..last], "byte {at} bit {bit}");
                assert_eq!(reopened.records(), last as u64, "byte {at} bit {bit}");
                assert_eq!(size(&path), bounds[last], "byte {at} bit {bit}");
                cut += 1;
            }
        }
    }
    (refused, cut, bounds)
}

#[test]
fn one_flipped_bit_does_not_truncate_over_a_sealed_record() {
    // Two shapes, one bit at a time over each byte of the file. What this sweep reaches and
    // the eight-bit one above does not: a `length` field altered by a single bit can stay
    // inside the bounds of a record and frame bytes the file holds, so a scan that stepped
    // over it on the word of that `length` walked past whole records.
    //
    // 34/64, records at 0 and 34, a file of 98 bytes: bit 6 of byte 0 turns the `length` of
    // the first record from 30 into 94, a frame of 98 bytes — the file itself.
    let (refused, cut, bounds) = sweep_one_bit(&[0, 30], "wal-bit-sweep-98");
    assert_eq!(bounds, vec![0, 34, 98]);
    assert_eq!(
        (refused, cut),
        (34 * 8, 64 * 8),
        "784 journals: the 34 bytes of the first record refused, the 64 of the second cut"
    );

    // 34/64/34, records at 0, 34 and 98, a file of 132 bytes: the same flip of byte 0 ends
    // the first frame at 98, where the third record starts, so a scan that stepped over it
    // read that third record and lost the second one.
    let (refused, cut, bounds) = sweep_one_bit(&[0, 30, 0], "wal-bit-sweep-132");
    assert_eq!(bounds, vec![0, 34, 98, 132]);
    assert_eq!(
        (refused, cut),
        (98 * 8, 34 * 8),
        "1 056 journals: the 98 bytes of the first two records refused, the 34 of the third cut"
    );
}

#[test]
fn the_two_bit_sweeps_stop_on_frames_of_the_three_failing_kinds() {
    // The numbers the module documentation cites for the sweep above: the frame the scan
    // stops on is read here for each of the same 1 840 flipped journals, without opening
    // them, and sorted by the failure it carries. Neither `Whole` nor `Rejected` turns up in
    // these 1 840, which the arm that panics below states: one bit flipped inside a record
    // leaves the checksum of that record disagreeing.
    let mut counts = (0usize, 0usize, 0usize);
    for payloads in [vec![0usize, 30], vec![0usize, 30, 0]] {
        let (_dir, bytes, bounds, _written) = journal_of(&payloads, "wal-bit-sweep-kinds");
        let mut shape = (0usize, 0usize, 0usize);
        for at in 0..bytes.len() {
            for bit in 0..8u32 {
                let mut copy = bytes.clone();
                copy[at] ^= 1u8 << bit;
                let hit = bounds
                    .iter()
                    .rposition(|start| *start <= at as u64)
                    .expect("the byte sits in a record");
                let start = bounds[hit] as usize;
                let mut region: &[u8] = &copy[start..];
                let remaining = (copy.len() - start) as u64;
                match frame(&mut region, start as u64, remaining).expect("read the frame") {
                    Framed::Damaged { .. } => shape.0 += 1,
                    Framed::Torn { .. } => shape.1 += 1,
                    Framed::Unframed { .. } => shape.2 += 1,
                    other => panic!("byte {at} bit {bit}: {other:?}"),
                }
            }
        }
        let expected = if payloads.len() == 2 {
            (726, 29, 29)
        } else {
            (968, 43, 45)
        };
        assert_eq!(shape, expected, "shape {payloads:?}");
        counts = (counts.0 + shape.0, counts.1 + shape.1, counts.2 + shape.2);
    }
    assert_eq!(counts, (1694, 72, 74));
    assert_eq!(counts.0 + counts.1 + counts.2, 98 * 8 + 132 * 8);
}

#[test]
fn a_damaged_length_that_spans_the_file_keeps_the_record_behind_it() {
    // One bit of the `length` of the first
    // record of two, 30 into 94, so the frame it announces ends at the last byte of the
    // file. A scan that stepped over that frame landed past the second record and emptied
    // the file without a word; the second record is whole, so this is an interior record.
    let (dir, bytes, bounds, _written) = journal_of(&[0, 30], "wal-spanning-length");
    let path = dir.child("wal");
    assert_eq!(bounds, vec![0, 34, 98]);
    assert_eq!(read_u32(&bytes, 0), 30, "the length of the first record");
    let mut copy = bytes.clone();
    copy[0] ^= 0x40;
    assert_eq!(
        read_u32(&copy, 0),
        94,
        "a frame of 98 bytes, the file itself"
    );
    std::fs::write(&path, &copy).expect("write the journal back");

    let err = Wal::open(&path).expect_err("a damaged record with a whole record after it");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("offset 0"), "{err}");
    assert!(err.to_string().contains("offset 34"), "{err}");
    assert!(err.to_string().contains("interior record"), "{err}");
    assert_eq!(size(&path), 98, "the journal is left as it stands");
}

#[test]
fn a_file_holding_no_agreeing_checksum_is_emptied_without_an_error() {
    // The other end of the truncation rule, and the price it has: 500 bytes in which the
    // walk finds no checksum that agrees are read as one unfinished append and removed, with
    // nothing said. Nothing in this build writes such a file, and `open` has nothing to tell
    // it apart from a first append that stopped halfway with: the case is written down here
    // rather than repaired.
    let dir = TempDir::created("wal-no-record-at-all");
    let path = dir.child("wal");
    let junk: Vec<u8> = (0..500u32).map(|index| (index * 7 + 3) as u8).collect();
    std::fs::write(&path, &junk).expect("write bytes that are no record");
    let opened = Wal::open(&path).expect("bytes framing no record read as a tail");
    assert_eq!(opened.records(), 0);
    assert_eq!(opened.last_lsn(), Lsn(0));
    assert_eq!(size(&path), 0, "the file is emptied");
}

#[test]
fn a_torn_tail_whose_payload_holds_a_record_reads_as_an_interior_record() {
    // The false find the walk can make: the tail is one record whose payload
    // carries the bytes of another, cut two bytes short. Those inner bytes carry a checksum
    // that agrees, so the walk stops on them and reports an interior record where a crash
    // had left a tail. The journal is refused rather than truncated, which is the safe way
    // round of the two. Outside a payload built to hold a record, what the walk leans on is
    // the CRC-32; the case asserted here is the built one.
    let dir = TempDir::created("wal-nested-record");
    let path = dir.child("wal");
    let mut wal = Wal::open(&path).expect("open a journal");
    wal.append(WalRecordKind::Begin, TxnId(1), &[])
        .expect("append Begin");
    wal.flush().expect("flush");
    drop(wal);
    let nested = raw_record(
        RECORD_VERSION,
        WalRecordKind::Commit.as_byte(),
        2,
        1,
        1,
        &[],
    );
    let mut outer = raw_record(
        RECORD_VERSION,
        WalRecordKind::Insert.as_byte(),
        2,
        1,
        1,
        &nested,
    );
    outer.truncate(outer.len() - 2);
    append_bytes(&path, &outer);
    assert_eq!(size(&path), 100, "34 bytes of record and 66 of torn tail");

    let err = Wal::open(&path).expect_err("the nested record is taken for a record behind");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("offset 34"), "{err}");
    // Offset 64: where the payload of the tail starts carrying the nested record.
    assert!(err.to_string().contains("offset 64"), "{err}");
    assert!(err.to_string().contains("interior record"), "{err}");
    assert_eq!(size(&path), 100, "the journal is left as it stands");
}

#[test]
fn a_sealed_record_this_build_will_not_read_stops_the_truncation() {
    // A record sealed over kind 99 has a checksum that agrees, so it reached the disk whole;
    // cutting the file before it would throw away a record that is entire. The walk after an
    // unreadable frame therefore counts it, and the two files below differ by that alone.
    let dir = TempDir::created("wal-sealed-unknown-kind");
    let path = dir.child("wal");
    let begin = WalRecordKind::Begin.as_byte();

    // A `length` of four zero bytes in the second record of three, the third sealed over
    // kind 99: nothing follows with a kind this build reads, and the frame at 34 cannot be
    // walked over, yet offset 68 holds 34 bytes written whole.
    let mut bytes = raw_record(RECORD_VERSION, begin, 1, 0, 1, &[]);
    bytes.extend_from_slice(&raw_record(RECORD_VERSION, begin, 2, 1, 1, &[]));
    bytes.extend_from_slice(&raw_record(RECORD_VERSION, 99, 3, 2, 1, &[]));
    bytes[34..38].fill(0);
    std::fs::write(&path, &bytes).expect("write");
    let err = Wal::open(&path).expect_err("sealed bytes sit behind the unreadable frame");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("offset 34"), "{err}");
    assert!(err.to_string().contains("offset 68"), "{err}");
    assert!(err.to_string().contains("interior record"), "{err}");
    assert_eq!(size(&path), 102, "the journal is left as it stands");

    // The same file without those sealed bytes: the zeroed `length` is then the tail of an
    // append that did not finish, and the first record comes back.
    std::fs::write(&path, &bytes[..68]).expect("write");
    let reopened = Wal::open(&path).expect("a zeroed length at the end is a tail");
    assert_eq!(reopened.records(), 1);
    assert_eq!(size(&path), 34);
}

// --- The handle an instance holds ---------------------------------------------------

#[test]
fn the_handle_shares_one_journal_and_its_durable_lsn() {
    let dir = TempDir::created("wal-handle");
    let path = dir.child("wal");
    let handle = WalHandle::open(&path).expect("open a journal");
    let shared = handle.clone();

    assert_eq!(handle.durable_lsn(), Lsn(0));
    assert_eq!(
        handle
            .append(WalRecordKind::Begin, TxnId(3), &[])
            .expect("append Begin"),
        Lsn(1)
    );
    // The clone appends to the same journal, and the numbering continues.
    assert_eq!(
        shared
            .append(WalRecordKind::Insert, TxnId(3), &[7])
            .expect("append Insert"),
        Lsn(2)
    );
    // Appending does not make anything durable.
    assert_eq!(handle.durable_lsn(), Lsn(0));
    assert_eq!(handle.last_lsn().expect("last lsn"), Lsn(2));

    assert_eq!(
        shared
            .append_durable(WalRecordKind::Commit, TxnId(3), &[])
            .expect("append Commit and flush"),
        Lsn(3)
    );
    // The flush covers the records appended before the one it was asked for.
    assert_eq!(handle.durable_lsn(), Lsn(3));
    assert_eq!(shared.durable_lsn(), Lsn(3));

    let kinds: Vec<_> = handle
        .records()
        .expect("read the records back")
        .iter()
        .map(|record| record.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Commit
        ]
    );
}

#[test]
fn a_poisoned_journal_lock_answers_a_durable_lsn_of_zero() {
    let dir = TempDir::created("wal-handle-poisoned");
    let handle = WalHandle::open(&dir.child("wal")).expect("open a journal");
    handle
        .append_durable(WalRecordKind::Begin, TxnId(1), &[])
        .expect("append Begin and flush");
    assert_eq!(handle.durable_lsn(), Lsn(1));

    let outcome = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _guard = handle.wal.lock().unwrap();
                panic!("poisoning the journal lock on purpose");
            })
            .join()
    });
    assert!(outcome.is_err());

    // The pool reads this through a trait that carries no error, so the answer is the one
    // that holds pages back rather than the one that lets them out.
    assert_eq!(handle.durable_lsn(), Lsn(0));
    let err = handle
        .append(WalRecordKind::Commit, TxnId(1), &[])
        .expect_err("the lock is poisoned");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert_eq!(err.to_string(), "data corruption: journal lock poisoned");
    assert!(handle.records().is_err());
    assert!(handle.last_lsn().is_err());
    assert!(
        handle
            .append_durable(WalRecordKind::Commit, TxnId(1), &[])
            .is_err()
    );
}

#[test]
fn wal_handle_is_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<WalHandle>();
}
