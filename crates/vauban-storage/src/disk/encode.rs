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
mod tests;
