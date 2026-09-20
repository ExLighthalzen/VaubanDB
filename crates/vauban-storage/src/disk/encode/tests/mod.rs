use super::*;

/// The bytes of one value.
fn encoded(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value(value, &mut out);
    out
}

/// Encodes a value, decodes it back and checks that the decoder consumed exactly the
/// bytes the encoder wrote.
fn roundtrip(value: &Value) -> Value {
    let buffer = encoded(value);
    let (back, used) = decode_value(&buffer).expect("bytes this file just wrote");
    assert_eq!(
        used,
        buffer.len(),
        "decode_value should consume the whole value {value:?}"
    );
    back
}

/// Fails the test unless `err` is a `Corruption` whose message contains each needle.
fn assert_corruption(err: &InternalError, needles: &[&str]) {
    assert!(
        matches!(err, InternalError::Corruption(_)),
        "expected Corruption, got {err:?}"
    );
    let message = err.to_string();
    for needle in needles {
        assert!(
            message.contains(needle),
            "the message should contain {needle:?}: {message}"
        );
    }
}

#[test]
fn roundtrip_every_value_variant() {
    // One vector per variant `Value` declares, the two floats apart because `NaN` and
    // `-0.0` are compared on their bits below. Sixteen variants here, plus `F64` and
    // `F32`, is the eighteen of the tag table.
    let vectors = [
        Value::Null,
        Value::Bit(true),
        Value::I8(255),
        Value::I16(-2),
        Value::I32(0),
        Value::I64(i64::MIN),
        Value::Decimal(Decimal {
            mantissa: -1,
            precision: 5,
            scale: 2,
        }),
        Value::Money(-1),
        Value::String(SqlString {
            text: String::new(),
        }),
        Value::String(SqlString {
            text: "é".to_owned(),
        }),
        Value::Bytes(vec![0, 255]),
        Value::Date(Date { days: 730_119 }),
        Value::Time(Time { ticks_100ns: 1 }),
        Value::DateTime(DateTime {
            days: 0,
            ticks_300th: 0,
        }),
        Value::DateTime2(DateTime2 {
            date: Date { days: 730_119 },
            time: Time { ticks_100ns: 1 },
        }),
        Value::DateTimeOffset(DateTimeOffset {
            utc: DateTime2 {
                date: Date { days: 730_119 },
                time: Time { ticks_100ns: 1 },
            },
            offset_minutes: -840,
        }),
        Value::Guid([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
    ];
    for value in &vectors {
        assert_eq!(&roundtrip(value), value, "round trip of {value:?}");
    }

    // Floats: `to_bits`, not `PartialEq`. `f64::NAN != f64::NAN`, and `-0.0 == 0.0`
    // would let a lost sign bit through.
    match roundtrip(&Value::F64(f64::NAN)) {
        Value::F64(back) => assert_eq!(back.to_bits(), f64::NAN.to_bits()),
        other => panic!("expected F64, got {other:?}"),
    }
    match roundtrip(&Value::F32(-0.0)) {
        Value::F32(back) => assert_eq!(back.to_bits(), (-0.0f32).to_bits()),
        other => panic!("expected F32, got {other:?}"),
    }
    assert_ne!((-0.0f32).to_bits(), 0.0f32.to_bits());
}

#[test]
fn tag_bytes_are_part_of_the_format() {
    let table = [
        (Value::Null, TAG_NULL, 0u8),
        (Value::Bit(false), TAG_BIT, 1),
        (Value::I8(0), TAG_I8, 2),
        (Value::I16(0), TAG_I16, 3),
        (Value::I32(0), TAG_I32, 4),
        (Value::I64(0), TAG_I64, 5),
        (
            Value::Decimal(Decimal {
                mantissa: 0,
                precision: 1,
                scale: 0,
            }),
            TAG_DECIMAL,
            6,
        ),
        (Value::F64(0.0), TAG_F64, 7),
        (Value::F32(0.0), TAG_F32, 8),
        (Value::Money(0), TAG_MONEY, 9),
        (
            Value::String(SqlString {
                text: String::new(),
            }),
            TAG_STRING,
            10,
        ),
        (Value::Bytes(Vec::new()), TAG_BYTES, 11),
        (Value::Date(Date { days: 0 }), TAG_DATE, 12),
        (Value::Time(Time { ticks_100ns: 0 }), TAG_TIME, 13),
        (
            Value::DateTime(DateTime {
                days: 0,
                ticks_300th: 0,
            }),
            TAG_DATETIME,
            14,
        ),
        (
            Value::DateTime2(DateTime2 {
                date: Date { days: 0 },
                time: Time { ticks_100ns: 0 },
            }),
            TAG_DATETIME2,
            15,
        ),
        (
            Value::DateTimeOffset(DateTimeOffset {
                utc: DateTime2 {
                    date: Date { days: 0 },
                    time: Time { ticks_100ns: 0 },
                },
                offset_minutes: 0,
            }),
            TAG_DATETIMEOFFSET,
            16,
        ),
        (Value::Guid([0; 16]), TAG_GUID, 17),
    ];
    assert_eq!(table.len(), 18, "one row per variant of the tag table");
    for (value, tag, documented) in &table {
        assert_eq!(tag, documented, "tag constant of {value:?}");
        assert_eq!(encoded(value)[0], *documented, "first byte of {value:?}");
    }

    // 18 is the first free tag, so it decodes as unknown rather than as something.
    let err = decode_value(&[18]).expect_err("tag 18 is not in the table");
    assert_corruption(&err, &["unknown value tag 18"]);
}

#[test]
fn payload_layouts_are_little_endian() {
    assert_eq!(encoded(&Value::Null), vec![TAG_NULL]);
    assert_eq!(encoded(&Value::Bit(true)), vec![TAG_BIT, 1]);
    assert_eq!(encoded(&Value::Bit(false)), vec![TAG_BIT, 0]);
    assert_eq!(encoded(&Value::I8(255)), vec![TAG_I8, 0xFF]);
    assert_eq!(encoded(&Value::I16(0x0102)), vec![TAG_I16, 0x02, 0x01]);
    assert_eq!(
        encoded(&Value::I32(0x0102_0304)),
        vec![TAG_I32, 0x04, 0x03, 0x02, 0x01]
    );
    // `decimal`: 16 bytes of mantissa, then precision, then scale.
    let decimal = encoded(&Value::Decimal(Decimal {
        mantissa: -1,
        precision: 5,
        scale: 2,
    }));
    assert_eq!(decimal.len(), 1 + 16 + 1 + 1);
    assert_eq!(decimal[0], TAG_DECIMAL);
    assert_eq!(&decimal[1..17], &[0xFF; 16]);
    assert_eq!(&decimal[17..], &[5, 2]);
    // `datetime2` is a date payload then a time payload, no inner tag.
    let stamp = DateTime2 {
        date: Date { days: 730_119 },
        time: Time { ticks_100ns: 1 },
    };
    let composed = encoded(&Value::DateTime2(stamp));
    assert_eq!(composed.len(), 1 + 4 + 8);
    assert_eq!(composed[0], TAG_DATETIME2);
    assert_eq!(&composed[1..5], &encoded(&Value::Date(stamp.date))[1..]);
    assert_eq!(&composed[5..], &encoded(&Value::Time(stamp.time))[1..]);
    // `datetimeoffset` is that payload plus a signed `i16` of minutes.
    let offset = encoded(&Value::DateTimeOffset(DateTimeOffset {
        utc: stamp,
        offset_minutes: -840,
    }));
    assert_eq!(offset.len(), 1 + 4 + 8 + 2);
    assert_eq!(offset[0], TAG_DATETIMEOFFSET);
    assert_eq!(&offset[1..13], &composed[1..]);
    assert_eq!(&offset[13..], &(-840i16).to_le_bytes());
    // `uniqueidentifier`: the 16 bytes in the order they sit in the variant.
    let guid = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let encoded_guid = encoded(&Value::Guid(guid));
    assert_eq!(encoded_guid[0], TAG_GUID);
    assert_eq!(&encoded_guid[1..], &guid);
}

#[test]
fn string_utf8_roundtrip() {
    let value = Value::String(SqlString {
        text: "é".to_owned(),
    });
    let buffer = encoded(&value);
    // Tag, then the byte count, then the UTF-8: `é` is one character and two bytes, and
    // the count is the two.
    assert_eq!(buffer[0], TAG_STRING);
    assert_eq!(
        u32::from_le_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]),
        2
    );
    assert_eq!(&buffer[5..], &[0xC3, 0xA9]);
    assert_eq!(buffer.len(), 1 + 4 + 2);
    assert_eq!("é".chars().count(), 1, "one character, two bytes");
    assert_eq!(roundtrip(&value), value);

    // Empty text: the count is 0 and no payload follows.
    let empty = Value::String(SqlString {
        text: String::new(),
    });
    assert_eq!(encoded(&empty), vec![TAG_STRING, 0, 0, 0, 0]);
    assert_eq!(roundtrip(&empty), empty);

    // A count in bytes is what lets a multi-byte payload come back whole.
    let mixed = Value::String(SqlString {
        text: "aéb".to_owned(),
    });
    assert_eq!(encoded(&mixed)[1], 4);
    assert_eq!(roundtrip(&mixed), mixed);

    // Bytes that are not UTF-8 under tag 10 are corruption, not a lossy string.
    let err =
        decode_value(&[TAG_STRING, 2, 0, 0, 0, 0xFF, 0xFE]).expect_err("0xFF 0xFE is not UTF-8");
    assert_corruption(&err, &["not valid UTF-8"]);
}

#[test]
fn row_roundtrip_mixed_and_empty() {
    // A row of no column is the codec byte and a count of zero.
    let empty = Row(Vec::new());
    let mut buffer = Vec::new();
    encode_row(&empty, &mut buffer);
    assert_eq!(buffer, vec![ROW_CODEC_VERSION, 0, 0, 0, 0]);
    assert_eq!(
        decode_row(&buffer).expect("a row of no column"),
        empty,
        "round trip of Row(vec![])"
    );

    // Three columns, one of each of the shapes the heap will meet first: a fixed-width
    // number, a `NULL` and an empty string.
    let mixed = Row(vec![
        Value::I32(1),
        Value::Null,
        Value::String(SqlString {
            text: String::new(),
        }),
    ]);
    buffer.clear();
    encode_row(&mixed, &mut buffer);
    assert_eq!(buffer[0], ROW_CODEC_VERSION);
    assert_eq!(&buffer[1..5], &[3, 0, 0, 0]);
    assert_eq!(buffer.len(), 1 + 4 + 5 + 1 + 5);
    assert_eq!(decode_row(&buffer).expect("a row of three columns"), mixed);

    // `encode_row` appends: two rows in one buffer keep their own frame.
    let mut pair = Vec::new();
    encode_row(&empty, &mut pair);
    let first = pair.len();
    encode_row(&mixed, &mut pair);
    assert_eq!(&pair[..first], &[ROW_CODEC_VERSION, 0, 0, 0, 0]);
    assert_eq!(decode_row(&pair[first..]).expect("the second row"), mixed);
}

#[test]
fn unknown_tag_is_corruption() {
    let err = decode_value(&[0xFF]).expect_err("0xFF is not a tag");
    assert_corruption(&err, &["unknown value tag 255", "offset 0"]);

    // Same byte in the middle of a row: the message names the column and the offset.
    let row = Row(vec![Value::I32(1), Value::Null]);
    let mut buffer = Vec::new();
    encode_row(&row, &mut buffer);
    let second_value = 1 + 4 + 5;
    assert_eq!(buffer[second_value], TAG_NULL);
    buffer[second_value] = 0xFF;
    let err = decode_row(&buffer).expect_err("0xFF as the tag of the second column");
    assert_corruption(&err, &["column 1", "unknown value tag 255", "offset 10"]);
}

#[test]
fn truncated_payload_is_corruption() {
    // A `string` that announces ten bytes in a buffer that holds three.
    let buffer = [TAG_STRING, 10, 0, 0, 0, b'a', b'b', b'c'];
    let err = decode_value(&buffer).expect_err("ten announced, three present");
    assert_corruption(&err, &["needs 10 bytes", "has 3 left"]);
    assert!(decode_row(&buffer).is_err());

    // An empty buffer has not even a tag.
    let err = decode_value(&[]).expect_err("no tag at all");
    assert_corruption(&err, &["value tag", "needs 1 bytes"]);

    // One vector per variant that carries a payload (`Null` is a lone tag), each cut at
    // every length below its own: a value that stops early is corruption, never a value
    // built out of the bytes that happen to be there.
    let values = [
        Value::Bit(true),
        Value::I8(1),
        Value::I16(-2),
        Value::I32(0),
        Value::I64(i64::MIN),
        Value::Decimal(Decimal {
            mantissa: -1,
            precision: 5,
            scale: 2,
        }),
        Value::F64(f64::NAN),
        Value::F32(-0.0),
        Value::Money(-1),
        Value::String(SqlString {
            text: "é".to_owned(),
        }),
        Value::Bytes(vec![0, 255]),
        Value::Date(Date { days: 730_119 }),
        Value::Time(Time { ticks_100ns: 1 }),
        Value::DateTime(DateTime {
            days: 0,
            ticks_300th: 0,
        }),
        Value::DateTime2(DateTime2 {
            date: Date { days: 730_119 },
            time: Time { ticks_100ns: 1 },
        }),
        Value::DateTimeOffset(DateTimeOffset {
            utc: DateTime2 {
                date: Date { days: 730_119 },
                time: Time { ticks_100ns: 1 },
            },
            offset_minutes: -840,
        }),
        Value::Guid([0; 16]),
    ];
    for value in &values {
        let whole = encoded(value);
        for cut in 1..whole.len() {
            match decode_value(&whole[..cut]) {
                Ok((decoded, used)) => {
                    panic!("{value:?} cut at {cut} decoded as {decoded:?} over {used} bytes")
                }
                Err(err) => assert!(
                    matches!(err, InternalError::Corruption(_)),
                    "{value:?} cut at {cut} should be Corruption, got {err:?}"
                ),
            }
        }
    }
}

#[test]
fn row_codec_v2_is_corruption() {
    let err = decode_row(&[2, 0, 0, 0, 0]).expect_err("codec 2 is not read here");
    assert_corruption(&err, &["codec version 2", "this build reads 1"]);

    // The same row with its first byte back to 1 decodes, so the codec byte is what the
    // error was about.
    assert_eq!(
        decode_row(&[ROW_CODEC_VERSION, 0, 0, 0, 0]).expect("codec 1"),
        Row(Vec::new())
    );

    // Codec 0 is refused the same way: version 1 is the only one written.
    let err = decode_row(&[0, 0, 0, 0, 0]).expect_err("codec 0 is not read here");
    assert_corruption(&err, &["codec version 0"]);
}

#[test]
fn row_with_trailing_bytes_is_corruption() {
    let row = Row(vec![Value::I32(7)]);
    let mut buffer = Vec::new();
    encode_row(&row, &mut buffer);
    assert_eq!(decode_row(&buffer).expect("the row itself"), row);
    buffer.push(0);
    let err = decode_row(&buffer).expect_err("one byte too many");
    assert_corruption(&err, &["row of 1 columns", "leaves 1 unread bytes"]);

    // A row that announces more columns than the buffer carries is a truncation, the
    // mirror case of the leftover.
    let mut short = Vec::new();
    encode_row(&Row(vec![Value::I32(7)]), &mut short);
    short[1] = 2;
    let err = decode_row(&short).expect_err("two columns announced, one present");
    assert_corruption(&err, &["column 1", "value tag"]);
}

#[test]
fn bit_payload_other_than_0_or_1_is_corruption() {
    let err = decode_value(&[TAG_BIT, 2]).expect_err("2 is neither 0 nor 1");
    assert_corruption(&err, &["bit payload 2", "expected 0 or 1"]);
    assert_eq!(
        decode_value(&[TAG_BIT, 0]).expect("0 is false").0,
        Value::Bit(false)
    );
    assert_eq!(
        decode_value(&[TAG_BIT, 1]).expect("1 is true").0,
        Value::Bit(true)
    );
}

#[test]
fn decode_value_reports_what_it_consumed_and_leaves_the_rest() {
    // `decode_value` stops after one value; the leftover is the caller's business, and
    // only `decode_row` refuses it.
    let mut buffer = Vec::new();
    encode_value(&Value::I16(-2), &mut buffer);
    buffer.extend_from_slice(b"tail");
    let (value, used) = decode_value(&buffer).expect("one value then four spare bytes");
    assert_eq!(value, Value::I16(-2));
    assert_eq!(used, 3);
    assert_eq!(&buffer[used..], b"tail");
}

#[test]
fn the_file_imports_only_types_and_errors() {
    // The codec must not depend on the wire format crate, nor on the page layout: the
    // three `use` lines outside the tests are the whole dependency list.
    let source = include_str!("../../encode.rs");
    let (code, _tests) = source
        .split_once("#[cfg(test)]")
        .expect("the test module of this file");
    let imports: Vec<&str> = code
        .lines()
        .filter(|line| line.starts_with("use "))
        .collect();
    assert_eq!(imports.len(), 3, "unexpected import list: {imports:?}");
    assert!(
        imports.contains(&"use vauban_errors::InternalError;"),
        "{imports:?}"
    );
    assert!(imports.contains(&"use crate::Row;"), "{imports:?}");
    assert!(
        imports
            .iter()
            .any(|line| line.starts_with("use vauban_types::")),
        "{imports:?}"
    );
}
