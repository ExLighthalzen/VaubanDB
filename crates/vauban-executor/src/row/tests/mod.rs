use super::*;

/// The identifier a value of the tail holds, read back as a write statement reads it.
fn row_id_of(value: &Value) -> Option<RowId> {
    match value {
        Value::I64(bits) => Some(RowId(u64::from_ne_bytes(bits.to_ne_bytes()))),
        _ => None,
    }
}

/// A row identifier comes back from its value unchanged, the highest one included, and a
/// `NULL` names no row.
#[test]
fn a_row_id_round_trips_through_its_value() {
    for id in [
        RowId(0),
        RowId(1),
        RowId(i64::MAX as u64 + 1),
        RowId(u64::MAX),
    ] {
        assert_eq!(row_id_of(&row_id_value(id)), Some(id));
    }
    assert_eq!(row_id_of(&Value::Null), None);
}

/// The visible values of both sides come first, then the tails, left before right.
#[test]
fn join_rows_moves_the_tails_after_the_visible_values() {
    let left = vec![Value::I32(1), Value::I32(2), Value::I64(10)];
    let right = vec![Value::I32(3), Value::I64(20), Value::I64(21)];
    let joined = join_rows(
        &JoinSide {
            row: Some(&left),
            width: 2,
            tail: 1,
        },
        &JoinSide {
            row: Some(&right),
            width: 1,
            tail: 2,
        },
    );
    assert_eq!(
        joined,
        vec![
            Value::I32(1),
            Value::I32(2),
            Value::I32(3),
            Value::I64(10),
            Value::I64(20),
            Value::I64(21),
        ]
    );
}

/// A padded side gives a `NULL` per visible value and per row identifier; sides without a
/// tail give the plain concatenation.
#[test]
fn join_rows_pads_a_missing_side() {
    let right = vec![Value::I32(3), Value::I64(20)];
    let joined = join_rows(
        &JoinSide {
            row: None,
            width: 2,
            tail: 1,
        },
        &JoinSide {
            row: Some(&right),
            width: 1,
            tail: 1,
        },
    );
    assert_eq!(
        joined,
        vec![
            Value::Null,
            Value::Null,
            Value::I32(3),
            Value::Null,
            Value::I64(20),
        ]
    );
    let plain = join_rows(
        &JoinSide {
            row: Some(&[Value::I32(1)]),
            width: 1,
            tail: 0,
        },
        &JoinSide {
            row: None,
            width: 2,
            tail: 0,
        },
    );
    assert_eq!(plain, vec![Value::I32(1), Value::Null, Value::Null]);
}
