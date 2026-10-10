use vauban_types::Value;

use super::super::encode::{decode_row, encode_row};
use super::super::version::NO_XMAX;
use super::*;
use crate::{Row, RowId, TxnId};

/// The columns of a row of one `int`, encoded as a payload carries them.
fn columns(value: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode_row(&Row(vec![Value::I32(value)]), &mut bytes);
    bytes
}

#[test]
fn an_insert_payload_is_the_table_then_the_version_record() {
    let change = RowChange {
        table: TableId(9),
        header: VersionHeader {
            row: RowId(3),
            xmin: TxnId(4),
            xmax: None,
            seq: 7,
        },
        columns: columns(11),
    };
    let bytes = change.encode();

    assert_eq!(OFF_HEADER, 4);
    assert_eq!(PREFIX_LEN, 36);
    assert_eq!(bytes.len(), PREFIX_LEN + change.columns.len());
    assert_eq!(&bytes[0..4], &9u32.to_le_bytes());
    assert_eq!(&bytes[4..12], &3u64.to_le_bytes());
    assert_eq!(&bytes[12..20], &4u64.to_le_bytes());
    assert_eq!(&bytes[20..28], &NO_XMAX.to_le_bytes());
    assert_eq!(&bytes[28..36], &7u64.to_le_bytes());
    assert_eq!(&bytes[36..], change.columns.as_slice());
    assert_eq!(RowChange::decode(&bytes).expect("decode"), change);
    // The columns read back as the row that was encoded, which is what a redo inserts.
    assert_eq!(
        decode_row(&bytes[36..]).expect("decode the columns"),
        Row(vec![Value::I32(11)])
    );
}

#[test]
fn a_delete_payload_carries_its_xmax_and_no_column() {
    let change = RowChange {
        table: TableId(1),
        header: VersionHeader {
            row: RowId(2),
            xmin: TxnId(4),
            xmax: Some(TxnId(5)),
            seq: 8,
        },
        columns: Vec::new(),
    };
    let bytes = change.encode();

    assert_eq!(bytes.len(), PREFIX_LEN);
    assert_eq!(&bytes[20..28], &5u64.to_le_bytes());
    let read = RowChange::decode(&bytes).expect("decode");
    assert_eq!(read, change);
    assert_eq!(read.header.xmax, Some(TxnId(5)));
    assert!(read.columns.is_empty());
}

#[test]
fn decode_refuses_a_payload_shorter_than_the_prefix() {
    for len in [0, 4, PREFIX_LEN - 1] {
        let err = RowChange::decode(&vec![0u8; len])
            .expect_err("a payload shorter than its prefix is refused");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(
            err.to_string().contains(&format!("{len} payload bytes")),
            "{err}"
        );
    }
    // The shortest payload this module reads is the one of a `Delete`.
    assert!(RowChange::decode(&[0u8; PREFIX_LEN]).is_ok());
}

/// A release of slot 5 of page 12 of table 9, freeing version 7 of row 3.
fn slot_release(release: Release) -> SlotRelease {
    SlotRelease {
        table: TableId(9),
        header: VersionHeader {
            row: RowId(3),
            xmin: TxnId(4),
            xmax: Some(TxnId(6)),
            seq: 7,
        },
        at: Rid {
            page: PageId(12),
            slot: 5,
        },
        release,
    }
}

#[test]
fn a_slot_release_is_47_bytes_and_reads_back() {
    for (release, byte) in [(Release::Removed, 1u8), (Release::XmaxCleared, 2)] {
        let payload = slot_release(release);
        let bytes = payload.encode();
        assert_eq!(SLOT_RELEASE_LEN, 47);
        assert_eq!(bytes.len(), SLOT_RELEASE_LEN);
        assert_eq!(&bytes[0..4], &9u32.to_le_bytes());
        assert_eq!(&bytes[4..12], &3u64.to_le_bytes());
        assert_eq!(&bytes[12..20], &4u64.to_le_bytes());
        assert_eq!(&bytes[20..28], &6u64.to_le_bytes());
        assert_eq!(&bytes[28..36], &7u64.to_le_bytes());
        assert_eq!(&bytes[36..44], &12u64.to_le_bytes());
        assert_eq!(&bytes[44..46], &5u16.to_le_bytes());
        assert_eq!(bytes[46], byte);
        assert_eq!(SlotRelease::decode(&bytes).expect("decode"), payload);
    }
}

#[test]
fn a_slot_release_of_another_length_or_release_is_corruption() {
    let bytes = slot_release(Release::Removed).encode();
    for cut in [&bytes[..46], &[bytes.as_slice(), &[0]].concat()[..]] {
        let err = SlotRelease::decode(cut).expect_err("a payload of another length");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("expected 47"), "{err}");
    }
    let mut unknown = bytes.clone();
    unknown[46] = 3;
    let err = SlotRelease::decode(&unknown).expect_err("release 3 is unknown");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("release 3"), "{err}");
}
