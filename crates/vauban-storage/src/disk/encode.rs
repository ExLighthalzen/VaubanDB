//! Binary encoding of the values and the rows the storage layer exchanges with the rest of
//! the engine ([`Value`], [`Row`]), without any I/O and without any notion of page.
//!
//! This is **not** the TDS wire encoding: it is VaubanDB's own on-disk format, versioned
//! independently of SQL Server's. The heap ([`super::heap`]) puts the bytes produced here into a
//! slotted page or into an overflow chain, and hands them back untouched to [`decode_row`].
//!
//! # Tagged codec
//!
//! Each value carries its own tag, so decoding needs no `TypeInfo` of the column: a row read
//! from a page is decodable on its own. There is no global version byte for a value — the tag
//! *is* the version, a new layout for a type takes a new tag. A row, on the other hand, opens
//! with a codec version byte, because the frame around the values (the column count) may
//! change without any tag changing.
//!
//! Everything is little-endian.
//!
//! # Errors
//!
//! A buffer that ends in the middle of a payload, an unknown tag, a `bit` payload other than
//! 0 or 1, a `string` payload that is not valid UTF-8, a row codec version other than
//! [`ROW_CODEC_VERSION`] and bytes left over after the last column of a row are all reported
//! as [`InternalError::Corruption`], with a message that names the value read. No
//! [`InternalError::Bug`] is built in this file: the bytes come from the disk, not from the
//! caller. The decoders reach the buffer through [`Reader::take`], which checks the bound
//! before it slices, so a short buffer is an error rather than a panic.

use vauban_errors::InternalError;
use vauban_types::{Date, DateTime, DateTime2, DateTimeOffset, Decimal, SqlString, Time, Value};

use crate::Row;

/// Codec version written as the first byte of a row by [`encode_row`], the only one
/// [`decode_row`] reads.
pub(crate) const ROW_CODEC_VERSION: u8 = 1;

/// Tag of [`Value::Null`].
const TAG_NULL: u8 = 0;
/// Tag of [`Value::Bit`].
const TAG_BIT: u8 = 1;
/// Tag of [`Value::I8`].
const TAG_I8: u8 = 2;
/// Tag of [`Value::I16`].
const TAG_I16: u8 = 3;
/// Tag of [`Value::I32`].
const TAG_I32: u8 = 4;
/// Tag of [`Value::I64`].
const TAG_I64: u8 = 5;
/// Tag of [`Value::Decimal`].
const TAG_DECIMAL: u8 = 6;
/// Tag of [`Value::F64`].
const TAG_F64: u8 = 7;
/// Tag of [`Value::F32`].
const TAG_F32: u8 = 8;
/// Tag of [`Value::Money`].
const TAG_MONEY: u8 = 9;
/// Tag of [`Value::String`].
const TAG_STRING: u8 = 10;
/// Tag of [`Value::Bytes`].
const TAG_BYTES: u8 = 11;
/// Tag of [`Value::Date`].
const TAG_DATE: u8 = 12;
/// Tag of [`Value::Time`].
const TAG_TIME: u8 = 13;
/// Tag of [`Value::DateTime`].
const TAG_DATETIME: u8 = 14;
/// Tag of [`Value::DateTime2`].
const TAG_DATETIME2: u8 = 15;
/// Tag of [`Value::DateTimeOffset`].
const TAG_DATETIMEOFFSET: u8 = 16;
/// Tag of [`Value::Guid`].
const TAG_GUID: u8 = 17;

/// Appends the encoding of one value to `out`.
///
/// # Tags and payloads, little-endian
///
/// | Tag | Variant | Payload |
/// |---|---|---|
/// | 0 | [`Value::Null`] | (empty) |
/// | 1 | [`Value::Bit`] | `u8`, 0 or 1; another byte is corruption when read back |
/// | 2 | [`Value::I8`] | `u8` (SQL Server `tinyint` is unsigned) |
/// | 3 | [`Value::I16`] | `i16` |
/// | 4 | [`Value::I32`] | `i32` |
/// | 5 | [`Value::I64`] | `i64` |
/// | 6 | [`Value::Decimal`] | `i128` mantissa, `u8` precision, `u8` scale (18 bytes) |
/// | 7 | [`Value::F64`] | 8 bytes of `f64::to_le_bytes` |
/// | 8 | [`Value::F32`] | 4 bytes of `f32::to_le_bytes` |
/// | 9 | [`Value::Money`] | `i64` (the amount times 10 000) |
/// | 10 | [`Value::String`] | `u32` byte count, then that many bytes of UTF-8 |
/// | 11 | [`Value::Bytes`] | `u32` byte count, then that many bytes |
/// | 12 | [`Value::Date`] | `i32` `days` |
/// | 13 | [`Value::Time`] | `u64` `ticks_100ns` |
/// | 14 | [`Value::DateTime`] | `i32` `days`, `u32` `ticks_300th` |
/// | 15 | [`Value::DateTime2`] | the payload of tag 12 then the payload of tag 13, untagged |
/// | 16 | [`Value::DateTimeOffset`] | the payload of tag 15 then `i16` `offset_minutes` |
/// | 17 | [`Value::Guid`] | the 16 bytes in the order they sit in [`Value::Guid`] |
///
/// The table covers the eighteen variants `Value` declares today. The `match` below has no
/// wildcard arm, so a variant added to `Value` later fails to compile here instead of being
/// written with someone else's tag.
///
/// # Floating-point values
///
/// `f32` and `f64` go through their IEEE bit pattern, so what comes back carries the bits
/// that went in. Comparing the two ends of a round trip is therefore a comparison of
/// `to_bits`, not of the floats: the round trip is checked on `f64::NAN`, which `PartialEq`
/// reports unequal to itself, and on `-0.0f32`, which `PartialEq` reports equal to `0.0`.
///
/// # Character and binary payloads
///
/// The byte count of a `string` is the length of its UTF-8, in **bytes**, not in characters,
/// and there is no terminating NUL. `Bytes` uses the same frame.
pub(crate) fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Bit(flag) => {
            out.push(TAG_BIT);
            out.push(u8::from(*flag));
        }
        Value::I8(number) => {
            out.push(TAG_I8);
            out.push(*number);
        }
        Value::I16(number) => {
            out.push(TAG_I16);
            out.extend_from_slice(&number.to_le_bytes());
        }
        Value::I32(number) => {
            out.push(TAG_I32);
            out.extend_from_slice(&number.to_le_bytes());
        }
        Value::I64(number) => {
            out.push(TAG_I64);
            out.extend_from_slice(&number.to_le_bytes());
        }
        Value::Decimal(decimal) => {
            out.push(TAG_DECIMAL);
            out.extend_from_slice(&decimal.mantissa.to_le_bytes());
            out.push(decimal.precision);
            out.push(decimal.scale);
        }
        Value::F64(number) => {
            out.push(TAG_F64);
            out.extend_from_slice(&number.to_le_bytes());
        }
        Value::F32(number) => {
            out.push(TAG_F32);
            out.extend_from_slice(&number.to_le_bytes());
        }
        Value::Money(amount) => {
            out.push(TAG_MONEY);
            out.extend_from_slice(&amount.to_le_bytes());
        }
        Value::String(text) => {
            out.push(TAG_STRING);
            encode_len_prefixed(text.text.as_bytes(), out);
        }
        Value::Bytes(bytes) => {
            out.push(TAG_BYTES);
            encode_len_prefixed(bytes, out);
        }
        Value::Date(date) => {
            out.push(TAG_DATE);
            encode_date_payload(date, out);
        }
        Value::Time(time) => {
            out.push(TAG_TIME);
            encode_time_payload(time, out);
        }
        Value::DateTime(stamp) => {
            out.push(TAG_DATETIME);
            out.extend_from_slice(&stamp.days.to_le_bytes());
            out.extend_from_slice(&stamp.ticks_300th.to_le_bytes());
        }
        Value::DateTime2(stamp) => {
            out.push(TAG_DATETIME2);
            encode_datetime2_payload(stamp, out);
        }
        Value::DateTimeOffset(stamp) => {
            out.push(TAG_DATETIMEOFFSET);
            encode_datetime2_payload(&stamp.utc, out);
            out.extend_from_slice(&stamp.offset_minutes.to_le_bytes());
        }
        Value::Guid(bytes) => {
            out.push(TAG_GUID);
            out.extend_from_slice(bytes);
        }
    }
}

/// Reads one value from the start of `bytes` and returns it with the number of bytes it took.
///
/// Bytes after the value are left to the caller: this is what lets [`decode_row`] walk a row
/// column by column. [`decode_row`] is the one that refuses leftovers.
pub(crate) fn decode_value(bytes: &[u8]) -> Result<(Value, usize), InternalError> {
    let mut reader = Reader::new(bytes);
    let value = read_value(&mut reader)?;
    Ok((value, reader.position()))
}

/// Appends the encoding of one row to `out`: the byte [`ROW_CODEC_VERSION`], a `u32` column
/// count, then the columns in order, each tagged by [`encode_value`].
///
/// The count saturates at [`u32::MAX`], like the byte counts of [`encode_len_prefixed`]: a
/// row of more columns than that does not fit the field, and the decoder then stops on a
/// truncation rather than the caller seeing a panic.
pub(crate) fn encode_row(row: &Row, out: &mut Vec<u8>) {
    out.push(ROW_CODEC_VERSION);
    let columns = u32::try_from(row.0.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&columns.to_le_bytes());
    for value in &row.0 {
        encode_value(value, out);
    }
}

/// Reads a row from the whole of `bytes`.
///
/// The buffer is consumed to its last byte: a row followed by unread bytes is
/// [`InternalError::Corruption`], because the caller cut the slice at the wrong place or the
/// page is damaged.
pub(crate) fn decode_row(bytes: &[u8]) -> Result<Row, InternalError> {
    let mut reader = Reader::new(bytes);
    let codec = reader.byte("row codec version")?;
    if codec != ROW_CODEC_VERSION {
        return Err(InternalError::Corruption(format!(
            "row declares codec version {codec}, this build reads {ROW_CODEC_VERSION}"
        )));
    }
    let columns = u32::from_le_bytes(reader.array("row column count")?);
    // No `with_capacity`: the count comes from the disk, and a damaged one would ask for
    // gigabytes before the first truncation error.
    let mut values = Vec::new();
    for column in 0..columns {
        // The column is named in front of the reason, and the `data corruption:` prefix of
        // the inner error is not repeated.
        values.push(read_value(&mut reader).map_err(|err| match err {
            InternalError::Corruption(reason) => {
                InternalError::Corruption(format!("column {column} of a row: {reason}"))
            }
            other => other,
        })?);
    }
    let left = reader.remaining();
    if left != 0 {
        return Err(InternalError::Corruption(format!(
            "row of {columns} columns leaves {left} unread bytes"
        )));
    }
    Ok(Row(values))
}

/// Writes a `u32` byte count followed by the bytes themselves.
///
/// The count is a length in bytes. A payload longer than [`u32::MAX`] bytes does not fit the
/// field: the count saturates there, and the decoder then reads a payload that disagrees with
/// the rest of the buffer rather than the caller seeing a panic.
fn encode_len_prefixed(bytes: &[u8], out: &mut Vec<u8>) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
}

/// The payload of tag 12, without the tag.
fn encode_date_payload(date: &Date, out: &mut Vec<u8>) {
    out.extend_from_slice(&date.days.to_le_bytes());
}

/// The payload of tag 13, without the tag.
fn encode_time_payload(time: &Time, out: &mut Vec<u8>) {
    out.extend_from_slice(&time.ticks_100ns.to_le_bytes());
}

/// The payload of tag 15: a date payload then a time payload, neither of them tagged.
fn encode_datetime2_payload(stamp: &DateTime2, out: &mut Vec<u8>) {
    encode_date_payload(&stamp.date, out);
    encode_time_payload(&stamp.time, out);
}

/// Reads the payload of tag 12.
fn read_date_payload(reader: &mut Reader<'_>) -> Result<Date, InternalError> {
    Ok(Date {
        days: i32::from_le_bytes(reader.array("date days")?),
    })
}

/// Reads the payload of tag 13.
fn read_time_payload(reader: &mut Reader<'_>) -> Result<Time, InternalError> {
    Ok(Time {
        ticks_100ns: u64::from_le_bytes(reader.array("time ticks")?),
    })
}

/// Reads the payload of tag 15.
fn read_datetime2_payload(reader: &mut Reader<'_>) -> Result<DateTime2, InternalError> {
    Ok(DateTime2 {
        date: read_date_payload(reader)?,
        time: read_time_payload(reader)?,
    })
}

/// Reads one tagged value from `reader`, leaving it just after the payload.
fn read_value(reader: &mut Reader<'_>) -> Result<Value, InternalError> {
    let tag_at = reader.position();
    let tag = reader.byte("value tag")?;
    let value = match tag {
        TAG_NULL => Value::Null,
        TAG_BIT => {
            let at = reader.position();
            match reader.byte("bit payload")? {
                0 => Value::Bit(false),
                1 => Value::Bit(true),
                other => {
                    return Err(InternalError::Corruption(format!(
                        "bit payload {other} at offset {at}, expected 0 or 1"
                    )));
                }
            }
        }
        TAG_I8 => Value::I8(reader.byte("tinyint payload")?),
        TAG_I16 => Value::I16(i16::from_le_bytes(reader.array("smallint payload")?)),
        TAG_I32 => Value::I32(i32::from_le_bytes(reader.array("int payload")?)),
        TAG_I64 => Value::I64(i64::from_le_bytes(reader.array("bigint payload")?)),
        TAG_DECIMAL => {
            let mantissa = i128::from_le_bytes(reader.array("decimal mantissa")?);
            let precision = reader.byte("decimal precision")?;
            let scale = reader.byte("decimal scale")?;
            Value::Decimal(Decimal {
                mantissa,
                precision,
                scale,
            })
        }
        TAG_F64 => Value::F64(f64::from_le_bytes(reader.array("float payload")?)),
        TAG_F32 => Value::F32(f32::from_le_bytes(reader.array("real payload")?)),
        TAG_MONEY => Value::Money(i64::from_le_bytes(reader.array("money payload")?)),
        TAG_STRING => {
            let at = reader.position();
            let bytes = reader.len_prefixed("string payload")?;
            let text = std::str::from_utf8(bytes).map_err(|err| {
                InternalError::Corruption(format!(
                    "string payload at offset {at} is not valid UTF-8: {err}"
                ))
            })?;
            Value::String(SqlString {
                text: text.to_owned(),
            })
        }
        TAG_BYTES => Value::Bytes(reader.len_prefixed("binary payload")?.to_vec()),
        TAG_DATE => Value::Date(read_date_payload(reader)?),
        TAG_TIME => Value::Time(read_time_payload(reader)?),
        TAG_DATETIME => Value::DateTime(DateTime {
            days: i32::from_le_bytes(reader.array("datetime days")?),
            ticks_300th: u32::from_le_bytes(reader.array("datetime ticks")?),
        }),
        TAG_DATETIME2 => Value::DateTime2(read_datetime2_payload(reader)?),
        TAG_DATETIMEOFFSET => Value::DateTimeOffset(DateTimeOffset {
            utc: read_datetime2_payload(reader)?,
            offset_minutes: i16::from_le_bytes(reader.array("datetimeoffset offset")?),
        }),
        TAG_GUID => Value::Guid(reader.array("uniqueidentifier payload")?),
        other => {
            return Err(InternalError::Corruption(format!(
                "unknown value tag {other} at offset {tag_at}"
            )));
        }
    };
    Ok(value)
}

/// A cursor over a buffer read back from the disk.
///
/// Its only job is that no read of this file indexes the buffer without checking the bound
/// first: a buffer that stops in the middle of a payload is an [`InternalError::Corruption`]
/// naming what was being read and how many bytes were left.
struct Reader<'a> {
    /// The buffer being read.
    bytes: &'a [u8],
    /// Offset of the next byte to read.
    at: usize,
}

impl<'a> Reader<'a> {
    /// A cursor on the first byte of `bytes`.
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Offset of the next byte to read, which is also the number of bytes consumed so far.
    fn position(&self) -> usize {
        self.at
    }

    /// Number of bytes not read yet.
    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    /// Consumes `len` bytes, or reports the truncation. `what` names the field being read.
    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], InternalError> {
        let end = self.at.saturating_add(len);
        if end > self.bytes.len() {
            return Err(InternalError::Corruption(format!(
                "{what} needs {len} bytes at offset {}, the buffer has {} left",
                self.at,
                self.remaining()
            )));
        }
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    /// Consumes one byte.
    fn byte(&mut self, what: &str) -> Result<u8, InternalError> {
        Ok(self.take(1, what)?[0])
    }

    /// Consumes `N` bytes as an array, ready for a `from_le_bytes`.
    fn array<const N: usize>(&mut self, what: &str) -> Result<[u8; N], InternalError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N, what)?);
        Ok(out)
    }

    /// Consumes a `u32` byte count and the bytes it announces.
    fn len_prefixed(&mut self, what: &str) -> Result<&'a [u8], InternalError> {
        let announced = u32::from_le_bytes(self.array(what)?);
        let len = usize::try_from(announced).map_err(|_| {
            InternalError::Corruption(format!(
                "{what} announces {announced} bytes, more than this platform addresses"
            ))
        })?;
        self.take(len, what)
    }
}

#[cfg(test)]
mod tests {
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
        let err = decode_value(&[TAG_STRING, 2, 0, 0, 0, 0xFF, 0xFE])
            .expect_err("0xFF 0xFE is not UTF-8");
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
        let source = include_str!("encode.rs");
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
}
