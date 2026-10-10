use vauban_types::Value;

use super::super::encode::{decode_row, encode_row};
use super::super::heap::Rid;
use super::super::page::PageId;
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
    assert_eq!(PREFIX_LEN, 38);
    assert_eq!(bytes.len(), PREFIX_LEN + change.columns.len());
    assert_eq!(&bytes[0..4], &9u32.to_le_bytes());
    assert_eq!(&bytes[4..12], &3u64.to_le_bytes());
    assert_eq!(&bytes[12..20], &4u64.to_le_bytes());
    assert_eq!(&bytes[20..28], &NO_XMAX.to_le_bytes());
    assert_eq!(&bytes[28..36], &7u64.to_le_bytes());
    // No page operation: the payload of a clustered table.
    assert_eq!(&bytes[36..38], &0u16.to_le_bytes());
    assert_eq!(&bytes[38..], change.columns.as_slice());
    assert_eq!(RowChange::decode(&bytes).expect("decode"), change);
    // The columns read back as the row that was encoded, which is what a redo inserts.
    assert_eq!(
        decode_row(&bytes[38..]).expect("decode the columns"),
        Row(vec![Value::I32(11)])
    );
}

/// The change of an update of row 3 creating version 9, for the tests of the page operations.
fn update_change() -> RowChange {
    RowChange {
        table: TableId(9),
        header: VersionHeader {
            row: RowId(3),
            xmin: TxnId(4),
            xmax: None,
            seq: 9,
        },
        columns: columns(12),
    }
}

#[test]
fn the_page_operations_sit_between_the_prefix_and_the_columns() {
    let change = update_change();
    let ops = [
        PageOp::SetXmax {
            rid: Rid {
                page: PageId(5),
                slot: 2,
            },
            seq: 7,
        },
        PageOp::Put {
            rid: Rid {
                page: PageId(6),
                slot: 0,
            },
        },
    ];
    let bytes = change.encode_with(&ops);

    assert_eq!(OP_LEN, 19);
    assert_eq!(bytes.len(), PREFIX_LEN + 2 * OP_LEN + change.columns.len());
    assert_eq!(&bytes[36..38], &2u16.to_le_bytes());
    assert_eq!(bytes[38], TAG_SET_XMAX);
    assert_eq!(&bytes[39..47], &5u64.to_le_bytes());
    assert_eq!(&bytes[47..49], &2u16.to_le_bytes());
    assert_eq!(&bytes[49..57], &7u64.to_le_bytes());
    assert_eq!(bytes[57], TAG_PUT);
    assert_eq!(&bytes[58..66], &6u64.to_le_bytes());
    assert_eq!(&bytes[66..68], &0u16.to_le_bytes());
    assert_eq!(&bytes[68..76], &0u64.to_le_bytes());
    assert_eq!(&bytes[76..], change.columns.as_slice());
    assert_eq!(
        RowChange::decode_with_ops(&bytes).expect("decode"),
        (change.clone(), ops.to_vec())
    );
    // Without its operations, the payload reads back as the same change.
    assert_eq!(RowChange::decode(&bytes).expect("decode"), change);
    assert_eq!(ops[0].page(), PageId(5));
    assert_eq!(ops[1].page(), PageId(6));
}

#[test]
fn a_payload_shorter_than_its_page_operations_is_corruption() {
    let ops = [PageOp::Put {
        rid: Rid {
            page: PageId(6),
            slot: 1,
        },
    }];
    let mut bytes = RowChange {
        columns: Vec::new(),
        ..update_change()
    }
    .encode_with(&ops);
    bytes.pop();
    let err = RowChange::decode_with_ops(&bytes).expect_err("one byte short");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("1 page operations"), "{err}");
}

#[test]
fn an_unknown_page_operation_is_corruption() {
    let ops = [PageOp::Put {
        rid: Rid {
            page: PageId(6),
            slot: 1,
        },
    }];
    let mut bytes = update_change().encode_with(&ops);
    bytes[PREFIX_LEN] = 3;
    let err = RowChange::decode_with_ops(&bytes).expect_err("tag 3");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("page operation 3"), "{err}");
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
