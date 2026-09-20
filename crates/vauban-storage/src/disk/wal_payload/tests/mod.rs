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
