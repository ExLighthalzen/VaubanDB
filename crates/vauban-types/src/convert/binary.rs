//! Conversions towards `binary`, `varbinary` and `uniqueidentifier`.
//!
//! The other direction — a binary value rendered as text — is not here: the dispatch of
//! [`crate::convert`] sends every character target to [`super::to_character`], where the three
//! binary styles are written next to the other styles.
//!
//! # Binary styles of a character source
//!
//! Style `0` (and no style) translates each character to its own byte, styles `1`
//! and `2` read the text as hexadecimal digits, `1` requiring the `0x` prefix and `2`
//! refusing it. Any other style number is error 9809: `SELECT CONVERT(binary(4), '41', 3);`
//! and `…, 126);` both raise it, naming varchar and varbinary. A binary or a
//! `uniqueidentifier` source ignores the style instead: `SELECT CONVERT(binary(4), 0x41, 1);`
//! yields `0x41000000`, not an error.
//!
//! # Length of the target: two independent ends, not one alignment
//!
//! `binary(n)` pads the value to `n` bytes and truncates it to `n`, `varbinary(n)` only
//! truncates, `varbinary(max)` keeps everything. Which end each of the two operations acts
//! on depends on the source, and — this is the part that costs a rewrite — **the two ends
//! are two independent properties**, not one alignment read twice:
//!
//! * *Which end receives the padding.* A character, a binary and a `uniqueidentifier`
//!   source are padded at the **tail**: `CAST(0x41 AS binary(4))` = `0x41000000`,
//!   `CAST('ABCDE' AS binary(12))` keeps `ABCDE` in the five leading bytes, and
//!   `CAST(CAST('0E984725-C51C-4BF4-9960-E1C80E27ABA0' AS uniqueidentifier) AS binary(20))`
//!   = `0x2547980E1CC5F44B9960E1C80E27ABA000000000`. The other sources are padded at the
//!   **head**: `datetime`, `smalldatetime`, `int`, `bigint`, `float`, `money`, and
//!   `decimal`.
//! * *Which end survives the truncation.* A character, a binary, a `uniqueidentifier` —
//!   **and a `decimal`** — keep their **head**: `CAST(0x0102030405 AS binary(3))` =
//!   `0x010203`, `CAST(… AS uniqueidentifier) AS binary(8))` = `0x2547980E1CC5F44B`, the
//!   eight *leading* bytes of the sixteen stored. `datetime`, `smalldatetime`, `int`,
//!   `bigint`, `float` and `money` keep their **tail**:
//!   `CAST(CAST(258 AS int) AS binary(2))` = `0x0102`, the *low* two bytes of `0x00000102`.
//!
//! `decimal` and `numeric` are the vector that tells the two properties apart, and the
//! reason [`Align`] carries two fields instead of one. Over five precisions and
//! magnitudes, both signs, typed and untyped literals, `binary(n)` and `varbinary(n)`
//! (`tests/convert_binary.rs`): the eight bytes of `CAST(1.5 AS decimal(5,2))` are
//! `0x0502000196000000`, precision, scale, a zero byte, the sign byte, then the mantissa
//! little-endian, and
//!
//! * `… AS binary(3))` = `0x050200`, the three **leading** bytes, so the cut is at the
//!   tail, like a byte string and unlike a `datetime`;
//! * `… AS binary(12))` = `0x000000000502000196000000`, four zero bytes **in front**, so
//!   the padding is at the head, like a `datetime` and unlike a byte string.
//!
//! A `decimal` therefore pads like a number and truncates like a byte string. Note what
//! `binary(3)` keeps: the header, and not one digit of the value. Do not restate this as
//! "numbers are right-aligned": that generalisation is false, and `decimal` is where it
//! breaks.
//!
//! # The four types of 2008: a scale byte, then a refusal instead of a cut
//!
//! `date`, `time(s)`, `datetime2(s)` and `datetimeoffset(s)` write their storage bytes, and
//! the three scaled ones put the **scale** in a leading byte, a shape the rest of this
//! module does not produce. The three scaled types behave alike on the eight scales of
//! their type (`tests::the_2008_helpers_write_the_scale_byte_then_the_storage`):
//!
//! * A `date` is the three little-endian bytes of its day count since 0001-01-01:
//!   `CAST(CAST('2000-01-02' AS date) AS varbinary(max))` = `0x08240B`, the two ends of the
//!   calendar giving `0x000000` and `0xDAB937`.
//! * A `time(s)` is the scale byte, then the ticks of that scale little-endian on three
//!   bytes at the scales 0 to 2, four at 3 and 4, five at 5 to 7 — a stored width of 4, 5
//!   or 6: `CAST(CAST('13:05:06.1234567' AS time(3)) AS varbinary(max))` = `0x034BC8CE02`.
//! * A `datetime2(s)` appends the three date bytes to that, for a width of 7, 8 or 9:
//!   `0x034BC8CE0208240B` at the scale 3.
//! * A `datetimeoffset(s)` writes the **UTC** instant, then the offset in minutes on two
//!   signed little-endian bytes, for a width of 9, 10 or 11: that same instant at `+02:00`
//!   and the scale 0 is `0x00E29B0008240B7800`, where `-05:30` writes `0xB6FE`.
//!
//! A wider target pads at the tail, like a byte string: `… AS binary(12)` of that `time(3)`
//! is `0x034BC8CE0200000000000000`. A **narrower** target is where the two families part,
//! and it is the vector that separates them: a `date` cuts in silence — `binary(2)` =
//! `0x0824`, `binary(1)` = `0x08` — where the three scaled types raise **8152**, severity
//! 16 state 17, on `varbinary(n)` as on `binary(n)`. [`Narrow`] carries that difference.
//!
//! # The numeric sources: the storage big-endian, and a `decimal` that is not its storage
//!
//! Family by family, each on the widths 1, one below, equal to, one above the storage,
//! and 12 (`tests/convert_binary.rs`):
//!
//! * An integer is its two's complement **big-endian** on its storage width — one byte for
//!   a `tinyint`, two, four, eight: `CAST(CAST(258 AS int) AS varbinary(max))` =
//!   `0x00000102`, `CAST(-2 AS int)` = `0xFFFFFFFE`. A `bit` is **one** byte, `0x01` or
//!   `0x00`.
//! * A `float` is the eight bytes of its IEEE 754 double, big-endian, a `real` the four of
//!   its single: `1.5` is `0x3FF8000000000000` and `0x3FC00000`. The sign bit is written as
//!   it is: `CAST(0 AS float) * -1` gives `0x8000000000000000`, the negative zero, and
//!   `CAST(-0.0 AS float)` gives `0x00…` because the literal `-0.0` is an exact zero before
//!   it becomes a double.
//! * A `money` is its amount in ten-thousandths as a big-endian `bigint`, a `smallmoney` as
//!   a big-endian `int`: `1.5` is `0x0000000000003A98` and `0x00003A98`.
//! * A `decimal` or a `numeric` is **not** its storage: a four-byte header `precision,
//!   scale, 0x00, sign` — `0x01` for a positive or zero mantissa, `0x00` for a negative
//!   one — then the absolute mantissa **little-endian**, on the smallest multiple of four
//!   bytes that holds it, four at least. `CAST(1.5 AS decimal(5,2))` is
//!   `0x0502000196000000`, eight bytes, and so is `CAST(1.5 AS decimal(38,4))`
//!   (`0x26040001983A0000`): the width follows the value, not the precision. The
//!   mantissa takes eight bytes from 2³², twelve from 2⁶⁴, sixteen from 2⁹⁶, for a total
//!   of 8, 12, 16 or 20.
//!
//! The two ends, family by family. The integers, `bit`, `float`, `real`,
//! `money` and `smallmoney` pad at the **head** and keep their **tail**, like a `datetime`
//! ([`Align::NUMBER`]): `CAST(CAST(258 AS int) AS binary(2))` = `0x0102`, `binary(1)` =
//! `0x02`, `binary(8)` = `0x0000000000000102`; `CAST(CAST(1.5 AS float) AS binary(4))` =
//! `0x00000000`, the four trailing bytes, which drop the exponent and the integer part
//! without a word. A `decimal` pads at the **head** too — `binary(12)` =
//! `0x000000000502000196000000` — and keeps its **head**: `binary(3)` = `0x050200`, the
//! header and not one digit of the value ([`Align::DECIMAL`]). No numeric source refuses a
//! narrow target: the widths 1 and storage − 1 on `binary(n)` and `varbinary(n)` raise no
//! 8152 (`tests/convert_binary.rs`); the refusal stays with the three scaled types of
//! 2008.
//!
//! The style is ignored on these sources, as on a binary one: `CONVERT(binary(4),
//! CAST(258 AS int), 3)` and `…, 126)` both give `0x00000102`
//! (`number_to_binary_ignores_the_style` in `tests/convert_binary.rs`).
//!
//! The other direction is in [`super::numeric`]: an integer, a `bit`, a `money` and a
//! `smallmoney` read back from the bytes written here (`number_to_binary_round_trip` in
//! `tests/convert_binary.rs`); `float` and `real` have no way back (529); a `decimal`
//! reads back there when the bytes carry a well-formed header and a non-zero mantissa,
//! and raises 8114 otherwise, a deliberate difference from SQL Server.

use vauban_errors::SqlResult;

use super::datetime::TICKS_300TH_PER_MINUTE;
use crate::collation::cp1252_byte;
use crate::errors;
use crate::{Date, DateTime, Decimal, Len, SqlType, Time, TypeInfo, Value};

/// Number of bytes of a `uniqueidentifier`.
const GUID_BYTES: usize = 16;

/// Number of stored bytes of a `datetime` ([`datetime_bytes`]).
const DATETIME_BYTES: usize = 8;

/// Number of stored bytes of a `smalldatetime` ([`smalldatetime_bytes`]).
const SMALLDATETIME_BYTES: usize = 4;

/// Number of stored bytes of a `date` ([`date_bytes`]).
const DATE_BYTES: usize = 3;

/// Number of stored bytes of the offset of a `datetimeoffset`: the minutes, signed.
const OFFSET_BYTES: usize = 2;

/// Fractional-second digits the finest scale holds, that of a `time(7)`.
const MAX_SCALE: u8 = 7;

/// Number of stored bytes of a `smallmoney`: the low half of the `bigint` a `money` writes.
const SMALLMONEY_BYTES: usize = 4;

/// Bytes of the header a `decimal` writes in front of its mantissa: precision, scale, a
/// zero byte, the sign ([`decimal_bytes`]).
const DECIMAL_HEADER_BYTES: usize = 4;

/// The mantissa of a `decimal` is written on a multiple of this many bytes, four at least
/// (a zero mantissa takes four: `0x0502000100000000`).
const DECIMAL_MANTISSA_STEP: usize = 4;

/// The sign byte of a `decimal` whose mantissa is positive or zero.
const DECIMAL_SIGN_POSITIVE: u8 = 0x01;

/// The sign byte of a `decimal` whose mantissa is negative.
const DECIMAL_SIGN_NEGATIVE: u8 = 0x00;

/// Sizes in bytes of the five groups of the `8-4-4-4-12` textual form, in reading order.
const GUID_GROUPS: [usize; 5] = [4, 2, 2, 2, 6];

/// Number of groups whose bytes are stored little-endian: the first three.
const GUID_LITTLE_ENDIAN_GROUPS: usize = 3;

/// The byte a character with no code page 1252 equivalent becomes: `?`, the substitution
/// character SQL Server itself writes when Unicode text reaches a `varchar`
/// (`SELECT CONVERT(binary(4), CAST(N'中' AS varchar(4)), 0);` = `0x3F000000`).
const SUBSTITUTE: u8 = b'?';

/// The prefix style `1` requires in front of the hexadecimal digits, and writes in front
/// of them in the other direction ([`super::to_character`]).
pub(crate) const HEX_PREFIX: &str = "0x";

/// The type the errors of a binary conversion name: `varbinary`, even when the target is
/// a `binary(n)` (`string_to_binary_styles` in `tests/convert_binary.rs`).
///
/// `SELECT CONVERT(binary(4), '4E616D6', 2);` raises 8114 naming varchar and varbinary,
/// and `SELECT CONVERT(binary(4), '41', 3);` raises 9809 naming the same pair.
/// [`SqlType::error_name`] ignores the length, so [`Len::Max`] here carries no meaning.
pub(crate) const BINARY_ERROR_TYPE: SqlType = SqlType::VarBinary(Len::Max);

/// Converts `v` to the binary target `to`, under the `CONVERT` style when given.
///
/// `Value::Null` never reaches this function: [`crate::convert`] answers it first.
pub(crate) fn to_binary(
    v: &Value,
    from: &TypeInfo,
    to: &TypeInfo,
    style: Option<i32>,
) -> SqlResult<Value> {
    let (bytes, align) = match (&from.ty, v) {
        (ty, Value::String(s)) if ty.is_string() => {
            (string_to_bytes(&s.text, ty, style)?, Align::BYTES)
        }
        (SqlType::Binary(_) | SqlType::VarBinary(_), Value::Bytes(b)) => (b.clone(), Align::BYTES),
        (SqlType::UniqueIdentifier, Value::Guid(g)) => (g.to_vec(), Align::BYTES),
        (SqlType::DateTime, Value::DateTime(d)) => (datetime_bytes(*d).to_vec(), Align::NUMBER),
        (SqlType::SmallDateTime, Value::DateTime(d)) => {
            (smalldatetime_bytes(*d).to_vec(), Align::NUMBER)
        }
        (SqlType::Bit, Value::Bit(b)) => (vec![u8::from(*b)], Align::NUMBER),
        (SqlType::TinyInt, Value::I8(n)) => (vec![*n], Align::NUMBER),
        (SqlType::SmallInt, Value::I16(n)) => (n.to_be_bytes().to_vec(), Align::NUMBER),
        (SqlType::Int, Value::I32(n)) => (n.to_be_bytes().to_vec(), Align::NUMBER),
        (SqlType::BigInt, Value::I64(n)) => (n.to_be_bytes().to_vec(), Align::NUMBER),
        (SqlType::Float, Value::F64(f)) => (f.to_be_bytes().to_vec(), Align::NUMBER),
        (SqlType::Real, Value::F32(f)) => (f.to_be_bytes().to_vec(), Align::NUMBER),
        (SqlType::Money, Value::Money(m)) => (m.to_be_bytes().to_vec(), Align::NUMBER),
        (SqlType::SmallMoney, Value::Money(m)) => (smallmoney_bytes(*m).to_vec(), Align::NUMBER),
        (SqlType::Decimal { .. } | SqlType::Numeric { .. }, Value::Decimal(d)) => {
            (decimal_bytes(d), Align::DECIMAL)
        }
        (SqlType::Date, Value::Date(d)) => (date_bytes(*d).to_vec(), Align::BYTES),
        (SqlType::Time(scale), Value::Time(t)) => (scaled_time_bytes(*t, *scale), Align::SCALED),
        (SqlType::DateTime2(scale), Value::DateTime2(d)) => {
            let mut bytes = scaled_time_bytes(d.time, *scale);
            bytes.extend_from_slice(&date_bytes(d.date));
            (bytes, Align::SCALED)
        }
        (SqlType::DateTimeOffset(scale), Value::DateTimeOffset(d)) => {
            let mut bytes = scaled_time_bytes(d.utc.time, *scale);
            bytes.extend_from_slice(&date_bytes(d.utc.date));
            let offset: [u8; OFFSET_BYTES] = d.offset_minutes.to_le_bytes();
            bytes.extend_from_slice(&offset);
            (bytes, Align::SCALED)
        }
        _ => return Err(errors::error_converting(&from.ty, &BINARY_ERROR_TYPE)),
    };
    Ok(Value::Bytes(fit(bytes, &to.ty, align)?))
}

/// Converts `v` to `uniqueidentifier`.
///
/// There is no `to` parameter: `uniqueidentifier` carries no parameter, so the target type
/// would add nothing. `style` is accepted and ignored, as SQL Server does
/// (`SELECT CONVERT(uniqueidentifier, '0E984725-C51C-4BF4-9960-E1C80E27ABA0', 1);` converts
/// instead of raising 9809).
///
/// A character source must be exactly `8-4-4-4-12`, optionally wrapped in braces; anything
/// else — surrounding spaces included — is error 8169. A binary source is read as the 16
/// stored bytes, padded with `0x00` or truncated like a `binary(16)` would be:
/// `SELECT CAST(0x41 AS uniqueidentifier);` yields `00000041-0000-0000-0000-000000000000`,
/// and `CAST(0x2547980E1CC5F44B9960E1C80E27ABA0AABBCCDD AS uniqueidentifier)` drops the
/// four extra bytes. Every other source is error 529 (`SELECT CAST(1 AS
/// uniqueidentifier);`, `guid_to_string_and_binary` in `tests/convert_binary.rs`).
pub(crate) fn to_guid(v: &Value, from: &TypeInfo, _style: Option<i32>) -> SqlResult<Value> {
    let guid = match (&from.ty, v) {
        (ty, Value::String(s)) if ty.is_string() => {
            parse_guid(&s.text).ok_or_else(errors::conversion_failed_guid)?
        }
        (SqlType::Binary(_) | SqlType::VarBinary(_), Value::Bytes(b)) => guid_from_bytes(b),
        (SqlType::UniqueIdentifier, Value::Guid(g)) => *g,
        _ => {
            return Err(errors::explicit_conversion_not_allowed(
                &from.ty,
                &SqlType::UniqueIdentifier,
            ));
        }
    };
    Ok(Value::Guid(guid))
}

/// One end of a binary value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The leading bytes.
    Head,
    /// The trailing bytes.
    Tail,
}

/// What a binary target narrower than the value does with it.
///
/// A cut is silent: the byte-string sources, the two legacy date types and a `date` take
/// one. The three scaled types of 2008 take [`Narrow::Refuse`] instead: the same
/// `time(3)` that fills a `binary(5)` raises 8152 towards a `binary(4)`, and that is the
/// vector which separates the two families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Narrow {
    /// Keep this end, drop the rest, say nothing.
    Keep(End),
    /// Refuse the conversion: error 8152.
    Refuse,
}

/// What the declared length of a binary target does — **two properties, not one**.
///
/// Padding and narrowing are two independent properties of the source type, and the four
/// consts below name the combinations the sources of this module produce. They are not a
/// classification of SQL Server: `decimal` and `numeric` take [`Align::DECIMAL`], `padded:
/// End::Head` with `narrow: Narrow::Keep(End::Head)`, which no other source builds. Reusing
/// [`Align::NUMBER`] because a `decimal` is a number would silently keep the wrong bytes.
///
/// The vectors that separate the properties are in the module documentation. A source
/// given the wrong ends answers a plausible and wrong value rather
/// than an error, which is why this is a type and not a `bool` buried in a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Align {
    /// End that `binary(n)` fills with `0x00` when the value is shorter than `n`.
    padded: End,
    /// What happens when the value is longer than `n`, for both targets.
    narrow: Narrow,
}

impl Align {
    /// Padded at the tail, cut at the tail: character, binary and `uniqueidentifier`
    /// sources, and a `date`.
    const BYTES: Self = Self {
        padded: End::Tail,
        narrow: Narrow::Keep(End::Head),
    };
    /// Padded at the head, cut at the head: `datetime`, `smalldatetime`, the integers,
    /// `bit`, `float`, `real`, `money` and `smallmoney` — but **not** `decimal`.
    const NUMBER: Self = Self {
        padded: End::Head,
        narrow: Narrow::Keep(End::Tail),
    };
    /// Padded at the head like a number, cut at the **tail** like a byte string: `decimal`
    /// and `numeric`, whose leading header survives a narrow target where a number keeps
    /// its low bytes.
    const DECIMAL: Self = Self {
        padded: End::Head,
        narrow: Narrow::Keep(End::Head),
    };
    /// Padded at the tail, and refused rather than cut: `time(s)`, `datetime2(s)` and
    /// `datetimeoffset(s)`, the three sources that carry a scale byte.
    const SCALED: Self = Self {
        padded: End::Tail,
        narrow: Narrow::Refuse,
    };
}

/// Applies the declared length of the binary target: padding for `binary(n)`, narrowing for
/// both, nothing for `varbinary(max)`, each as `align` names it — `align.padded` for the end
/// the padding lands on, `align.narrow` for the cut or the refusal, two ends that diverge on
/// the sources [`Align`] lists.
///
/// `binary(max)` does not exist in SQL Server; the parser never builds one, and it is treated
/// like `varbinary(max)` here rather than given a rule of its own.
///
/// A length of zero does not reach this function either: `binary(0)` and `varbinary(0)`
/// are refused while the batch is read, error **1001** regardless of the source type, and
/// `vauban-binder` refuses the same declaration one length earlier, with the placeholder
/// number its own `out_of_range` documents. Should one arrive, the answer here is the
/// empty binary on either end, and 8152 under [`Narrow::Refuse`] since no value of those
/// three types is empty.
fn fit(mut bytes: Vec<u8>, to: &SqlType, align: Align) -> SqlResult<Vec<u8>> {
    let (width, pads) = match to {
        SqlType::Binary(Len::Fixed(n)) => (usize::from(*n), true),
        SqlType::VarBinary(Len::Fixed(n)) => (usize::from(*n), false),
        _ => return Ok(bytes),
    };
    if bytes.len() > width {
        match align.narrow {
            Narrow::Keep(End::Head) => bytes.truncate(width),
            Narrow::Keep(End::Tail) => {
                bytes.drain(..bytes.len() - width);
            }
            Narrow::Refuse => return Err(errors::truncated()),
        }
        return Ok(bytes);
    }
    if !pads {
        // `varbinary(n)` never pads, whichever end it would have padded.
        return Ok(bytes);
    }
    match align.padded {
        End::Tail => bytes.resize(width, 0),
        End::Head => {
            let mut padded = vec![0_u8; width - bytes.len()];
            padded.append(&mut bytes);
            bytes = padded;
        }
    }
    Ok(bytes)
}

/// The eight stored bytes of a `datetime`: the signed day count since 1900-01-01, then the
/// unsigned count of 1/300 s ticks of the day, both **big-endian**.
///
/// This is the layout [`super::datetime`] reads in the other direction, and the round trip
/// closes on it: `CAST(CAST(CAST('1899-12-30 12:00:00' AS datetime) AS binary(8)) AS
/// datetime)` is the value it started from.
/// `CAST(CAST('1900-01-02 12:00:00' AS datetime) AS binary(8))` = `0x0000000100C5C100`,
/// `CAST(CAST('1899-12-30 12:00:00' AS datetime) AS binary(8))` = `0xFFFFFFFE00C5C100` and
/// `CAST(CAST('9999-12-31 23:59:59.997' AS datetime) AS binary(8))` = `0x002D247F018B81FF`.
///
/// A narrower target keeps the **tail** ([`Align::NUMBER`]): the same value is
/// `0x00C5C100` in `binary(4)`, `0xC100` in `binary(2)` and `0x00` in `binary(1)`, and a
/// wider one pads on the left — `0x000000000000000100C5C100` in `binary(12)`.
fn datetime_bytes(v: DateTime) -> [u8; DATETIME_BYTES] {
    let days = v.days.to_be_bytes();
    let ticks = v.ticks_300th.to_be_bytes();
    [
        days[0], days[1], days[2], days[3], ticks[0], ticks[1], ticks[2], ticks[3],
    ]
}

/// The four stored bytes of a `smalldatetime`: the unsigned day count since 1900-01-01,
/// then the minute of the day, both big-endian.
///
/// `CAST(CAST('2079-06-06 23:59' AS smalldatetime) AS binary(4))` = `0xFFFF059F`
/// (day 65 535, minute 1 439) and `CAST(CAST('1900-01-01 00:00' AS smalldatetime) AS
/// binary(4))` = `0x00000000`. The same right-hand alignment applies: the first value is
/// `0x059F` in `binary(2)` and `0x0000000000000000FFFF059F` in `binary(12)`.
///
/// Both fields are two bytes wide, so each is taken from the low half of the value
/// [`DateTime`] holds. A `smalldatetime` never overflows either half — its calendar stops
/// at day 65 535 and its clock at minute 1 439 — and the truncation is what the storage
/// layout does anyway, so no case is lost by taking the low half without a check.
fn smalldatetime_bytes(v: DateTime) -> [u8; SMALLDATETIME_BYTES] {
    let days = v.days.to_be_bytes();
    let minutes = (v.ticks_300th / TICKS_300TH_PER_MINUTE).to_be_bytes();
    [days[2], days[3], minutes[2], minutes[3]]
}

/// The four stored bytes of a `smallmoney`: the amount in ten-thousandths as a big-endian
/// `int`, the low half of the `bigint` [`Value::Money`] holds.
///
/// `1.5` is `0x00003A98`, `-1.5` is
/// `0xFFFFC568`, `214748.3647` is `0x7FFFFFFF`. A `smallmoney` stays within the range of an
/// `int` — that is what its conversions enforce — so the high half is the sign extension
/// and dropping it loses nothing.
fn smallmoney_bytes(amount: i64) -> [u8; SMALLMONEY_BYTES] {
    let bytes = amount.to_be_bytes();
    [bytes[4], bytes[5], bytes[6], bytes[7]]
}

/// The bytes a `decimal` or a `numeric` writes: the header `precision, scale, 0x00, sign`,
/// then the absolute mantissa little-endian on the smallest multiple of
/// [`DECIMAL_MANTISSA_STEP`] bytes that holds it, one step at least.
///
/// `CAST(1.5 AS decimal(5,2))` is `0x0502000196000000`, `-1.5` flips the sign byte to
/// `0x00`, and zero keeps `0x01`. The mantissa `2³² − 1` still takes four bytes
/// (`0x14000001FFFFFFFF`), `2³²` takes eight (`0x140000010000000001000000`), `2⁶⁴`
/// twelve and `2⁹⁶` sixteen, the largest `decimal(38,0)` filling the sixteen.
///
/// The header comes from the value, not from the declared type: the two agree by
/// construction (`from_exact` of [`super::numeric`] stamps the target's precision and
/// scale on the value it builds), and the value is what the bytes describe.
fn decimal_bytes(d: &Decimal) -> Vec<u8> {
    let sign = if d.mantissa < 0 {
        DECIMAL_SIGN_NEGATIVE
    } else {
        DECIMAL_SIGN_POSITIVE
    };
    let magnitude = d.mantissa.unsigned_abs();
    let significant = (u128::BITS - magnitude.leading_zeros()).div_ceil(u8::BITS) as usize;
    let width = significant
        .max(DECIMAL_MANTISSA_STEP)
        .next_multiple_of(DECIMAL_MANTISSA_STEP);
    let mut bytes = Vec::with_capacity(DECIMAL_HEADER_BYTES + width);
    bytes.extend_from_slice(&[d.precision, d.scale, 0x00, sign]);
    bytes.extend_from_slice(&magnitude.to_le_bytes()[..width]);
    bytes
}

/// The three stored bytes of a `date`: the day count since 0001-01-01, little-endian.
///
/// `CAST(CAST('2000-01-02' AS date) AS varbinary(max))` = `0x08240B`, day 730 120; the
/// first day of the calendar is `0x000000`
/// and 9999-12-31 is `0xDAB937`, day 3 652 058.
///
/// [`Date::days`] is that day count, so its fourth byte is zero over the range the type
/// holds and dropping it loses nothing there; a value outside the calendar would keep its
/// low three bytes, which is what the storage holds anyway.
fn date_bytes(v: Date) -> [u8; DATE_BYTES] {
    let days = v.days.to_le_bytes();
    [days[0], days[1], days[2]]
}

/// The stored bytes of a `time(s)`, a `datetime2(s)` or a `datetimeoffset(s)` clock: the
/// scale in a leading byte, then the count of ticks of that scale, little-endian on
/// [`time_width`] bytes.
///
/// `13:05:06.1234567` gives `0x0002B800` at the scale 0 (47 106 seconds), `0x034BC8CE02`
/// at the scale 3, and `0x07870370AD6D` at the scale 7.
///
/// The division truncates, which costs nothing: [`Time::ticks_100ns`] is already a multiple
/// of the step of its declared scale, since that is what the date conversions store
/// (`round_to_scale` of [`super::datetime`]). A scale above [`MAX_SCALE`] is not a type the
/// binder builds; it would write the scale byte it was given and the ticks of a `time(7)`.
fn scaled_time_bytes(v: Time, scale: u8) -> Vec<u8> {
    let step = 10_u64.pow(u32::from(MAX_SCALE.saturating_sub(scale)));
    let ticks = (v.ticks_100ns / step).to_le_bytes();
    let mut bytes = Vec::with_capacity(1 + time_width(scale));
    bytes.push(scale);
    bytes.extend_from_slice(&ticks[..time_width(scale)]);
    bytes
}

/// Bytes the tick count of a scale takes: three up to the scale 2, four at 3 and 4, five
/// from 5 on.
///
/// The stored width of the whole value is one more for a `time(s)` — the scale byte — plus
/// [`DATE_BYTES`] for a `datetime2(s)` and [`OFFSET_BYTES`] more for a `datetimeoffset(s)`,
/// which gives the widths 4/5/6, 7/8/9 and 9/10/11 at the eight scales of each of the
/// three types.
fn time_width(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

/// Reads a character value as bytes under its binary style.
fn string_to_bytes(text: &str, from: &SqlType, style: Option<i32>) -> SqlResult<Vec<u8>> {
    let failed = || errors::error_converting(from, &BINARY_ERROR_TYPE);
    match style {
        None | Some(0) => Ok(encode_text(text, from)),
        Some(1) => match text.strip_prefix(HEX_PREFIX) {
            Some(digits) => hex_to_bytes(digits).ok_or_else(failed),
            None => Err(failed()),
        },
        Some(2) => hex_to_bytes(text).ok_or_else(failed),
        Some(other) => Err(errors::unsupported_style(other, from, &BINARY_ERROR_TYPE)),
    }
}

/// Style `0`: the characters become bytes, one per character for `char` and `varchar` (code
/// page 1252), two per UTF-16 code unit for `nchar` and `nvarchar`.
///
/// `SELECT CONVERT(binary(8), 'Name', 0);` = `0x4E616D6500000000`
/// while `SELECT CONVERT(binary(8), N'Name', 0);` = `0x4E0061006D006500`, and
/// `SELECT CONVERT(binary(4), N'中', 0);` = `0x2D4E0000` (UTF-16LE, not the code page).
fn encode_text(text: &str, from: &SqlType) -> Vec<u8> {
    if matches!(from, SqlType::NChar(_) | SqlType::NVarChar(_)) {
        let mut bytes = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    } else {
        text.chars()
            .map(|c| cp1252_byte(c).unwrap_or(SUBSTITUTE))
            .collect()
    }
}

/// Styles `1` and `2`: an even number of hexadecimal digits, upper or lower case.
///
/// `None` for an odd count or a digit that is not hexadecimal, which the caller turns into
/// error 8114 (`SELECT CONVERT(binary(4), '4E616D6', 2);` and
/// `SELECT CONVERT(binary(4), 'ZZ', 2);` both answer *Error converting data type varchar to
/// varbinary.*). An empty text gives an empty result, not an error
/// (`SELECT DATALENGTH(CONVERT(varbinary(4), '', 2));` = `0`).
fn hex_to_bytes(text: &str) -> Option<Vec<u8>> {
    let digits = text.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for [high, low] in digits.as_chunks::<2>().0 {
        bytes.push(hex_byte(*high, *low)?);
    }
    Some(bytes)
}

/// The byte two hexadecimal digits spell, or `None` when one of them is not one.
fn hex_byte(high: u8, low: u8) -> Option<u8> {
    Some(hex_digit(high)? << 4 | hex_digit(low)?)
}

/// The value of one hexadecimal digit, `0` to `9`, `a` to `f` and `A` to `F`.
fn hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Reads the `8-4-4-4-12` textual form of a `uniqueidentifier` into its 16 stored bytes.
///
/// Braces around the whole value are accepted (both or neither), the digits are read in
/// either case, and nothing else is tolerated: no surrounding space, no missing dash
/// (`SELECT CAST('0E984725C51C4BF49960E1C80E27ABA0' AS uniqueidentifier);` and
/// `SELECT CAST('  0E984725-C51C-4BF4-9960-E1C80E27ABA0  ' AS uniqueidentifier);` both raise
/// 8169).
///
/// The three first groups are stored little-endian, the two last in reading order
/// ([`Value::Guid`]): `0E984725-C51C-4BF4-9960-E1C80E27ABA0` is stored
/// `25 47 98 0E 1C C5 F4 4B 99 60 E1 C8 0E 27 AB A0`, which
/// `SELECT CAST(CAST('0E984725-C51C-4BF4-9960-E1C80E27ABA0' AS uniqueidentifier) AS
/// binary(16));` shows.
fn parse_guid(text: &str) -> Option<[u8; GUID_BYTES]> {
    let inner = match text.strip_prefix('{') {
        Some(rest) => rest.strip_suffix('}')?,
        None if text.ends_with('}') => return None,
        None => text,
    };
    let mut groups = inner.split('-');
    let mut bytes = [0u8; GUID_BYTES];
    let mut written = 0;
    for (index, size) in GUID_GROUPS.into_iter().enumerate() {
        let digits = groups.next()?.as_bytes();
        if digits.len() != size * 2 {
            return None;
        }
        let group = &mut bytes[written..written + size];
        for (position, [high, low]) in digits.as_chunks::<2>().0.iter().enumerate() {
            group[position] = hex_byte(*high, *low)?;
        }
        if index < GUID_LITTLE_ENDIAN_GROUPS {
            group.reverse();
        }
        written += size;
    }
    if groups.next().is_some() {
        return None;
    }
    Some(bytes)
}

/// Reads a binary value as the 16 stored bytes of a `uniqueidentifier`, padding with `0x00`
/// and truncating exactly like a `binary(16)` target would.
fn guid_from_bytes(bytes: &[u8]) -> [u8; GUID_BYTES] {
    let mut guid = [0u8; GUID_BYTES];
    let taken = bytes.len().min(GUID_BYTES);
    guid[..taken].copy_from_slice(&bytes[..taken]);
    guid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hexadecimal reading at the level of the private helper.
    #[test]
    fn hex_to_bytes_needs_an_even_count_of_hex_digits() {
        assert_eq!(hex_to_bytes("4E616D65"), Some(vec![0x4E, 0x61, 0x6D, 0x65]));
        assert_eq!(hex_to_bytes("4e616d65"), Some(vec![0x4E, 0x61, 0x6D, 0x65]));
        assert_eq!(hex_to_bytes(""), Some(vec![]));
        assert_eq!(hex_to_bytes("4E616D6"), None);
        assert_eq!(hex_to_bytes("ZZ"), None);
        assert_eq!(hex_to_bytes("0x4E"), None);
        // A multi-byte character is read as its bytes, none of which is a digit; the
        // function never indexes inside a character.
        assert_eq!(hex_to_bytes("é"), None);
    }

    #[test]
    fn parse_guid_reads_the_five_groups_in_storage_order() {
        let expected = [
            0x25, 0x47, 0x98, 0x0E, 0x1C, 0xC5, 0xF4, 0x4B, 0x99, 0x60, 0xE1, 0xC8, 0x0E, 0x27,
            0xAB, 0xA0,
        ];
        assert_eq!(
            parse_guid("0E984725-C51C-4BF4-9960-E1C80E27ABA0"),
            Some(expected)
        );
        assert_eq!(
            parse_guid("{0e984725-c51c-4bf4-9960-e1c80e27aba0}"),
            Some(expected)
        );
        assert_eq!(parse_guid("0E984725C51C4BF49960E1C80E27ABA0"), None);
        assert_eq!(parse_guid("{0E984725-C51C-4BF4-9960-E1C80E27ABA0"), None);
        assert_eq!(parse_guid("0E984725-C51C-4BF4-9960-E1C80E27ABA0}"), None);
        assert_eq!(parse_guid(" 0E984725-C51C-4BF4-9960-E1C80E27ABA0"), None);
        assert_eq!(parse_guid("0E984725-C51C-4BF4-9960-E1C80E27ABA0-0"), None);
        assert_eq!(parse_guid("not-a-guid"), None);
        assert_eq!(parse_guid(""), None);
    }

    #[test]
    fn encode_text_follows_the_kind_of_the_source() {
        let ascii = SqlType::VarChar(Len::Fixed(4));
        let national = SqlType::NVarChar(Len::Fixed(4));
        assert_eq!(encode_text("Name", &ascii), vec![0x4E, 0x61, 0x6D, 0x65]);
        assert_eq!(
            encode_text("Name", &national),
            vec![0x4E, 0x00, 0x61, 0x00, 0x6D, 0x00, 0x65, 0x00]
        );
        assert_eq!(encode_text("é", &ascii), vec![0xE9]);
        assert_eq!(encode_text("é", &national), vec![0xE9, 0x00]);
        assert_eq!(encode_text("中", &national), vec![0x2D, 0x4E]);
        // No code page 1252 byte: the `?` substitute.
        assert_eq!(encode_text("中", &ascii), vec![SUBSTITUTE]);
    }

    /// The two ends at the level of the helper, on vectors that tell them apart: under
    /// [`Align::BYTES`] `binary(4)` of `0x0102` is `0x01020000`, under [`Align::NUMBER`]
    /// it is `0x00000102`, and `binary(1)` keeps `0x01` on one side and `0x02` on the
    /// other. A vector of equal length would answer the same thing on both and would prove
    /// nothing.
    ///
    /// The fourth combination, padded at the head and cut at the tail, is
    /// [`Align::DECIMAL`], pinned on the bytes of `CAST(1.5 AS decimal(5,2))`,
    /// `0x0502000196000000`: `0x050200` at `binary(3)` and four leading zero bytes at
    /// `binary(12)`.
    #[test]
    fn fit_pads_and_truncates_at_the_end_the_alignment_names() {
        let value = || vec![0x01_u8, 0x02];
        let binary = |n: u16| SqlType::Binary(Len::Fixed(n));
        let varbinary = |n: u16| SqlType::VarBinary(Len::Fixed(n));

        assert_eq!(fit(value(), &binary(4), Align::BYTES), Ok(vec![1, 2, 0, 0]));
        assert_eq!(
            fit(value(), &binary(4), Align::NUMBER),
            Ok(vec![0, 0, 1, 2])
        );
        assert_eq!(fit(value(), &binary(1), Align::BYTES), Ok(vec![1]));
        assert_eq!(fit(value(), &binary(1), Align::NUMBER), Ok(vec![2]));
        assert_eq!(fit(value(), &binary(2), Align::BYTES), Ok(value()));
        assert_eq!(fit(value(), &binary(2), Align::NUMBER), Ok(value()));

        // `varbinary(n)` truncates at the same end and never pads, whatever the alignment.
        assert_eq!(fit(value(), &varbinary(4), Align::BYTES), Ok(value()));
        assert_eq!(fit(value(), &varbinary(4), Align::NUMBER), Ok(value()));
        assert_eq!(fit(value(), &varbinary(1), Align::BYTES), Ok(vec![1]));
        assert_eq!(fit(value(), &varbinary(1), Align::NUMBER), Ok(vec![2]));

        // `varbinary(max)` keeps everything, on the three alignments.
        for align in [Align::BYTES, Align::NUMBER, Align::SCALED] {
            assert_eq!(
                fit(value(), &SqlType::VarBinary(Len::Max), align),
                Ok(value())
            );
        }

        // `Align::SCALED` pads at the tail like `Align::BYTES` and parts from it on the
        // narrow targets, which it refuses: the two widths are what tell them apart.
        assert_eq!(
            fit(value(), &binary(4), Align::SCALED),
            Ok(vec![1, 2, 0, 0])
        );
        assert_eq!(fit(value(), &varbinary(4), Align::SCALED), Ok(value()));
        assert_eq!(fit(value(), &binary(2), Align::SCALED), Ok(value()));
        for to in [binary(1), varbinary(1)] {
            let e = fit(value(), &to, Align::SCALED).expect_err("narrower than the value");
            assert_eq!(e.number, 8152);
            assert_eq!(e.severity, 16);
            assert_eq!(e.state, 17);
            assert_eq!(
                e.message,
                "Data too long: the string or binary value would be cut."
            );
            assert!(fit(value(), &to, Align::BYTES).is_ok());
        }

        // A length of zero is refused before the conversion, by SQL Server as by the
        // binder; the function still has to answer something, and it is the empty binary
        // whichever end it would have cut — and 8152 where it would have refused.
        for align in [Align::BYTES, Align::NUMBER] {
            assert_eq!(fit(value(), &binary(0), align), Ok(Vec::<u8>::new()));
            assert_eq!(fit(value(), &varbinary(0), align), Ok(Vec::<u8>::new()));
        }
        assert!(fit(value(), &binary(0), Align::SCALED).is_err());

        // The `decimal` combination: padded at the head like a number, cut at the tail
        // like a byte string. Neither of the two other alignments answers both of these,
        // which is what makes it a vector and not a coincidence.
        let decimal = Align::DECIMAL;
        let d = || vec![0x05_u8, 0x02, 0x00, 0x01, 0x96, 0x00, 0x00, 0x00];
        assert_eq!(fit(d(), &binary(3), decimal), Ok(vec![0x05, 0x02, 0x00]));
        assert_eq!(fit(d(), &varbinary(3), decimal), Ok(vec![0x05, 0x02, 0x00]));
        assert_eq!(
            fit(d(), &binary(12), decimal),
            Ok(vec![
                0, 0, 0, 0, 0x05, 0x02, 0x00, 0x01, 0x96, 0x00, 0x00, 0x00
            ])
        );
        assert_eq!(fit(d(), &varbinary(12), decimal), Ok(d()));
        // `Align::BYTES` agrees on the truncation and differs on the padding,
        // `Align::NUMBER` the other way round: it takes both widths to separate the three,
        // and one width alone would prove nothing.
        assert_eq!(
            fit(d(), &binary(3), Align::BYTES),
            fit(d(), &binary(3), decimal)
        );
        assert_ne!(
            fit(d(), &binary(12), Align::BYTES),
            fit(d(), &binary(12), decimal)
        );
        assert_ne!(
            fit(d(), &binary(3), Align::NUMBER),
            fit(d(), &binary(3), decimal)
        );
        assert_eq!(
            fit(d(), &binary(12), Align::NUMBER),
            fit(d(), &binary(12), decimal)
        );
    }

    /// The three stored layouts of 2008, at the level of the helpers, on the bytes of
    /// `'2000-01-02 13:05:06.1234567'`.
    #[test]
    fn the_2008_helpers_write_the_scale_byte_then_the_storage() {
        assert_eq!(date_bytes(Date { days: 730_120 }), [0x08, 0x24, 0x0B]);
        assert_eq!(date_bytes(Date { days: 0 }), [0x00, 0x00, 0x00]);
        assert_eq!(date_bytes(Date { days: 3_652_058 }), [0xDA, 0xB9, 0x37]);

        let hex = |bytes: Vec<u8>| {
            bytes
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join("")
        };
        // The eight scales, and with them the three widths 4, 5 and 6. The ticks are those
        // of `13:05:06.1234567` snapped to each scale, which is the value the conversions
        // store — the engine rounds the fraction while reading the string, so the scale 4
        // holds `.1235` and not `.1234`.
        for (scale, ticks_100ns, expected) in [
            (0, 471_060_000_000_u64, "0002B800"),
            (1, 471_061_000_000, "01153007"),
            (2, 471_061_200_000, "02D4E047"),
            (3, 471_061_230_000, "034BC8CE02"),
            (4, 471_061_235_000, "04F3D2131C"),
            (5, 471_061_234_600, "057A3DC61801"),
            (6, 471_061_234_570, "06C166BEF70A"),
            (7, 471_061_234_567, "07870370AD6D"),
        ] {
            let bytes = scaled_time_bytes(Time { ticks_100ns }, scale);
            assert_eq!(hex(bytes.clone()), expected, "time({scale})");
            assert_eq!(bytes.len(), 1 + time_width(scale));
        }
        assert_eq!(
            scaled_time_bytes(Time { ticks_100ns: 0 }, 7),
            vec![0x07, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            hex(scaled_time_bytes(
                Time {
                    ticks_100ns: 863_999_999_999
                },
                7
            )),
            "07FFBF692AC9"
        );
    }

    /// The two stored layouts, at the level of the helpers.
    #[test]
    fn the_date_helpers_write_the_stored_layout() {
        assert_eq!(
            datetime_bytes(DateTime {
                days: 1,
                ticks_300th: 12_960_000
            }),
            [0x00, 0x00, 0x00, 0x01, 0x00, 0xC5, 0xC1, 0x00]
        );
        assert_eq!(
            datetime_bytes(DateTime {
                days: -2,
                ticks_300th: 12_960_000
            }),
            [0xFF, 0xFF, 0xFF, 0xFE, 0x00, 0xC5, 0xC1, 0x00]
        );
        assert_eq!(
            smalldatetime_bytes(DateTime {
                days: 65_535,
                ticks_300th: 1_439 * TICKS_300TH_PER_MINUTE
            }),
            [0xFF, 0xFF, 0x05, 0x9F]
        );
        assert_eq!(
            smalldatetime_bytes(DateTime {
                days: 0,
                ticks_300th: 0
            }),
            [0x00, 0x00, 0x00, 0x00]
        );
    }

    /// The header, the sign byte and the four mantissa widths.
    #[test]
    fn decimal_bytes_write_the_header_then_the_mantissa_on_four_byte_steps() {
        let hex = |bytes: Vec<u8>| {
            bytes
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join("")
        };
        let decimal = |mantissa: i128, precision: u8, scale: u8| Decimal {
            mantissa,
            precision,
            scale,
        };
        assert_eq!(hex(decimal_bytes(&decimal(150, 5, 2))), "0502000196000000");
        assert_eq!(hex(decimal_bytes(&decimal(-150, 5, 2))), "0502000096000000");
        assert_eq!(hex(decimal_bytes(&decimal(0, 5, 2))), "0502000100000000");
        assert_eq!(
            hex(decimal_bytes(&decimal(15_000, 38, 4))),
            "26040001983A0000"
        );
        assert_eq!(hex(decimal_bytes(&decimal(15, 2, 1))), "020100010F000000");
        // The four widths of the mantissa: a step of four bytes at each power of 2^32.
        assert_eq!(
            hex(decimal_bytes(&decimal((1 << 32) - 1, 20, 0))),
            "14000001FFFFFFFF"
        );
        assert_eq!(
            hex(decimal_bytes(&decimal(1 << 32, 20, 0))),
            "140000010000000001000000"
        );
        assert_eq!(
            hex(decimal_bytes(&decimal(-(1 << 32), 20, 0))),
            "140000000000000001000000"
        );
        assert_eq!(
            hex(decimal_bytes(&decimal((1 << 64) - 1, 20, 0))),
            "14000001FFFFFFFFFFFFFFFF"
        );
        assert_eq!(
            hex(decimal_bytes(&decimal(1 << 64, 20, 0))),
            "14000001000000000000000001000000"
        );
        assert_eq!(
            hex(decimal_bytes(&decimal((1 << 96) - 1, 38, 0))),
            "26000001FFFFFFFFFFFFFFFFFFFFFFFF"
        );
        assert_eq!(
            hex(decimal_bytes(&decimal(1 << 96, 38, 0))),
            "2600000100000000000000000000000001000000"
        );
        let largest = 99_999_999_999_999_999_999_999_999_999_999_999_999_i128;
        assert_eq!(
            hex(decimal_bytes(&decimal(largest, 38, 0))),
            "26000001FFFFFFFF3F228A097AC4865AA84C3B4B"
        );
        assert_eq!(
            hex(decimal_bytes(&decimal(-largest, 38, 0))),
            "26000000FFFFFFFF3F228A097AC4865AA84C3B4B"
        );
    }

    /// The low half of the amount, on three vectors.
    #[test]
    fn smallmoney_bytes_keep_the_low_half_of_the_amount() {
        assert_eq!(smallmoney_bytes(15_000), [0x00, 0x00, 0x3A, 0x98]);
        assert_eq!(smallmoney_bytes(-15_000), [0xFF, 0xFF, 0xC5, 0x68]);
        assert_eq!(smallmoney_bytes(2_147_483_647), [0x7F, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn guid_from_bytes_pads_and_truncates() {
        assert_eq!(guid_from_bytes(&[0x41])[0], 0x41);
        assert_eq!(guid_from_bytes(&[0x41])[1..], [0u8; 15]);
        assert_eq!(guid_from_bytes(&[0xFF; 20]), [0xFF; GUID_BYTES]);
        assert_eq!(guid_from_bytes(&[]), [0u8; GUID_BYTES]);
    }
}
