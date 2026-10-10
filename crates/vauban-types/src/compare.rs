//! Three-valued comparison of [`Value`]s under a collation, and the sort keys that
//! order and hash values the same way ([`sort_key`], [`hash_key`]).
//!
//! This is the only place in the engine that compares two SQL values: `storage` uses it
//! to order index keys and detect duplicates. Conversions between type families are not
//! done here (the caller converts both sides to a common type first).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::hash::Hasher;
use std::iter::repeat_n;

use vauban_errors::{InternalError, SqlResult};

use crate::collation::{AsciiClass, Level, Rules, SPACE_UNIT, Table, ascii_last, ascii_weight};
use crate::{Collation, DateTime2, Decimal, Value};

/// Compares two values the way SQL Server does, with three-valued logic.
///
/// Returns:
/// - `Ok(None)` if `a` or `b` is [`Value::Null`], whatever the other operand is (`NULL`
///   compared to anything is `UNKNOWN`);
/// - `Ok(Some(ordering))` if both values belong to the same family (table below);
/// - `Err` (an [`InternalError::Bug`] converted to a `SqlError` number `50000`) if the two
///   values belong to different families. This is a broken precondition, not a SQL error:
///   the caller must have converted both sides to a common type beforehand
///   (`implicit_result_type` and `convert`). `None` is reserved to `NULL` and
///   never signals an incompatibility.
///
/// # Families
///
/// | Family | Variants | Rule |
/// |---|---|---|
/// | boolean | `Bit` | `false < true` |
/// | integers | `I8`, `I16`, `I32`, `I64` | widened to `i64`, so `I8(255) > I16(3)` |
/// | decimals | `Decimal` | numeric value, whatever the precision and scale |
/// | money | `Money` | the `i64` amount in ten-thousandths |
/// | floats | `F32`, `F64` | widened to `f64` |
/// | strings | `String` | [`Collation::compare`] |
/// | bytes | `Bytes` | byte by byte, the shorter padded with `0x00` (`0x01 = 0x0100`) |
/// | guid | `Guid` | the bytes in the order SQL Server compares them ([`GUID_ORDER`]) |
/// | date | `Date` | days |
/// | time | `Time` | ticks since midnight |
/// | datetime | `DateTime` | `(days, ticks_300th)` |
/// | datetime2 | `DateTime2` | `(date, time)` |
/// | datetimeoffset | `DateTimeOffset` | the `utc` instant only; the offset is ignored |
///
/// Every other pair (`I32` against `Decimal`, `String` against `Bytes`, `DateTime`
/// against `DateTime2`, ...) is an error. `collation` is ignored for non-string values.
///
/// # Strings
///
/// Two strings are handed to [`Collation::compare`], which holds the whole rule: trailing
/// spaces ignored, primary weights of code page 1252 first, secondary weights (the
/// accents) next, case never (`_CI`). Its documentation also states the two limits of
/// that rule: each collation is compared like the default one, and the order is the
/// `varchar` one even for `nvarchar` data.
///
/// # Guids
///
/// `uniqueidentifier` values are ordered by groups of the textual form, from the last
/// group to the first, and inside each group by the **stored** bytes read from the first
/// to the last, never by the numeric value of the group. [`GUID_ORDER`] is that reading
/// order (`guid_comparison_order` in `tests/convert_binary.rs`).
///
/// # A provisional rule
///
/// - `Bytes`: the `0x00` padding reproduces the documented behaviour of `binary`.
///
/// # Examples
///
/// ```
/// use std::cmp::Ordering;
/// use vauban_types::{compare, Collation, SqlString, Value};
///
/// let text = |t: &str| Value::String(SqlString { text: t.to_owned() });
///
/// assert_eq!(compare(&Value::Null, &Value::I32(1), &Collation::DEFAULT), Ok(None));
/// assert_eq!(
///     compare(&Value::I8(255), &Value::I64(3), &Collation::DEFAULT),
///     Ok(Some(Ordering::Greater))
/// );
/// assert_eq!(
///     compare(&text("abc  "), &text("ABC"), &Collation::DEFAULT),
///     Ok(Some(Ordering::Equal))
/// );
/// assert!(compare(&Value::I32(1), &Value::F64(1.0), &Collation::DEFAULT).is_err());
/// ```
pub fn compare(a: &Value, b: &Value, collation: &Collation) -> SqlResult<Option<Ordering>> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(None);
    }
    if let (Some(x), Some(y)) = (integer_as_i64(a), integer_as_i64(b)) {
        return Ok(Some(x.cmp(&y)));
    }
    if let (Some(x), Some(y)) = (float_as_f64(a), float_as_f64(b)) {
        return compare_floats(x, y).map(Some);
    }
    let ordering = match (a, b) {
        (Value::Bit(x), Value::Bit(y)) => x.cmp(y),
        (Value::Decimal(x), Value::Decimal(y)) => compare_decimals(x, y)?,
        (Value::Money(x), Value::Money(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => collation.compare(&x.text, &y.text),
        (Value::Bytes(x), Value::Bytes(y)) => compare_bytes(x, y),
        (Value::Guid(x), Value::Guid(y)) => compare_guids(x, y),
        (Value::Date(x), Value::Date(y)) => x.days.cmp(&y.days),
        (Value::Time(x), Value::Time(y)) => x.ticks_100ns.cmp(&y.ticks_100ns),
        (Value::DateTime(x), Value::DateTime(y)) => {
            (x.days, x.ticks_300th).cmp(&(y.days, y.ticks_300th))
        }
        (Value::DateTime2(x), Value::DateTime2(y)) => compare_datetime2(x, y),
        (Value::DateTimeOffset(x), Value::DateTimeOffset(y)) => compare_datetime2(&x.utc, &y.utc),
        _ => {
            return Err(bug(format!(
                "compare: incompatible value families {} and {}",
                variant_name(a),
                variant_name(b)
            )));
        }
    };
    Ok(Some(ordering))
}

/// Builds the internal error every precondition failure of this module reports.
fn bug(message: String) -> vauban_errors::SqlError {
    InternalError::Bug(message).into()
}

/// The integer families widened to `i64`; `None` for any other variant.
fn integer_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::I8(x) => Some(i64::from(*x)),
        Value::I16(x) => Some(i64::from(*x)),
        Value::I32(x) => Some(i64::from(*x)),
        Value::I64(x) => Some(*x),
        _ => None,
    }
}

/// The float families widened to `f64`; `None` for any other variant.
fn float_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::F32(x) => Some(f64::from(*x)),
        Value::F64(x) => Some(*x),
        _ => None,
    }
}

/// `partial_cmp` is `None` only for NaN, which SQL Server never produces: that is a bug,
/// not a `NULL`.
fn compare_floats(x: f64, y: f64) -> SqlResult<Ordering> {
    x.partial_cmp(&y)
        .ok_or_else(|| bug("compare: NaN in float comparison".to_owned()))
}

/// Compares two decimals by numeric value without overflowing `i128`.
///
/// Signs first, then integer parts, then fractional parts aligned on the larger scale:
/// a fractional part is `< 10^scale`, so once multiplied by `10^(max_scale - scale)` it
/// stays `< 10^max_scale <= 10^38`, which fits in a `u128`.
fn compare_decimals(x: &Decimal, y: &Decimal) -> SqlResult<Ordering> {
    let sign_x = x.mantissa.signum();
    let sign_y = y.mantissa.signum();
    if sign_x != sign_y {
        return Ok(sign_x.cmp(&sign_y));
    }
    if sign_x == 0 {
        return Ok(Ordering::Equal);
    }
    let magnitude = compare_decimal_magnitudes(x, y)?;
    Ok(if sign_x < 0 {
        magnitude.reverse()
    } else {
        magnitude
    })
}

/// Compares the absolute values of two decimals.
fn compare_decimal_magnitudes(x: &Decimal, y: &Decimal) -> SqlResult<Ordering> {
    let max_scale = x.scale.max(y.scale);
    let (int_x, frac_x) = split_decimal(x, max_scale)?;
    let (int_y, frac_y) = split_decimal(y, max_scale)?;
    Ok(int_x.cmp(&int_y).then(frac_x.cmp(&frac_y)))
}

/// Splits `|d|` into its integer part and its fractional part rescaled to `max_scale`.
fn split_decimal(d: &Decimal, max_scale: u8) -> SqlResult<(u128, u128)> {
    let abs = d.mantissa.unsigned_abs();
    let scale_factor = pow10(d.scale)?;
    let align_factor = pow10(max_scale - d.scale)?;
    let integer = abs / scale_factor;
    let fraction = (abs % scale_factor)
        .checked_mul(align_factor)
        .ok_or_else(|| bug(format!("compare: decimal scale {} out of range", d.scale)))?;
    Ok((integer, fraction))
}

/// `10^exp` as a `u128`; an error past 38, the largest scale a `decimal` can have.
fn pow10(exp: u8) -> SqlResult<u128> {
    10u128
        .checked_pow(u32::from(exp))
        .ok_or_else(|| bug(format!("compare: decimal scale {exp} out of range")))
}

/// The 16 stored bytes of a `uniqueidentifier` in the order SQL Server compares them: the
/// last group of the textual form first, then the fourth, the third, the second and the
/// first, each read from its first stored byte to its last.
///
/// The first three groups are stored little-endian ([`Value::Guid`]), so reading them in
/// storage order is **not** reading their numeric value:
/// `CAST('01000000-0000-0000-0000-000000000000' AS uniqueidentifier) <
/// CAST('00000100-0000-0000-0000-000000000000' AS uniqueidentifier)` being true although
/// `0x01000000` is the larger of the two first groups.
const GUID_ORDER: [usize; 16] = [10, 11, 12, 13, 14, 15, 8, 9, 6, 7, 4, 5, 0, 1, 2, 3];

/// `uniqueidentifier` comparison: the stored bytes taken in [`GUID_ORDER`].
fn compare_guids(x: &[u8; 16], y: &[u8; 16]) -> Ordering {
    for index in GUID_ORDER {
        let ordering = x[index].cmp(&y[index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// `binary` comparison: byte by byte, the shorter operand padded with `0x00`.
fn compare_bytes(a: &[u8], b: &[u8]) -> Ordering {
    let len = a.len().max(b.len());
    let padded_a = a.iter().copied().chain(repeat_n(0u8, len - a.len()));
    let padded_b = b.iter().copied().chain(repeat_n(0u8, len - b.len()));
    padded_a.cmp(padded_b)
}

/// `datetime2` as `(date.days, time.ticks_100ns)`.
fn compare_datetime2(x: &DateTime2, y: &DateTime2) -> Ordering {
    (x.date.days, x.time.ticks_100ns).cmp(&(y.date.days, y.time.ticks_100ns))
}

/// The variant name, for error messages.
fn variant_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bit(_) => "Bit",
        Value::I8(_) => "I8",
        Value::I16(_) => "I16",
        Value::I32(_) => "I32",
        Value::I64(_) => "I64",
        Value::Decimal(_) => "Decimal",
        Value::F64(_) => "F64",
        Value::F32(_) => "F32",
        Value::Money(_) => "Money",
        Value::String(_) => "String",
        Value::Bytes(_) => "Bytes",
        Value::Date(_) => "Date",
        Value::Time(_) => "Time",
        Value::DateTime(_) => "DateTime",
        Value::DateTime2(_) => "DateTime2",
        Value::DateTimeOffset(_) => "DateTimeOffset",
        Value::Guid(_) => "Guid",
    }
}

/// Appends to `out` the sort key of `v` under `collation`: a byte string whose order is
/// the order of [`compare`], so that two values of the same family are equal under
/// [`compare`] exactly when their keys are equal (`tests/sort_key.rs`).
///
/// The key starts with a byte that names the family of the value, so `NULL` (which has
/// the smallest key) and values of different families have distinct keys; within a
/// family, the key follows the rule of the table of [`compare`]:
///
/// - `I8`, `I16`, `I32` and `I64` holding one number share its key (`I8(5)` and `I64(5)`,
///   `value_keys_fold_the_differences_compare_ignores`);
/// - a decimal is keyed on its numeric value, its precision and scale left out (`1.5`
///   and `1.500` share a key);
/// - `F32` is widened to `f64`, and `-0.0` has the key of `0.0`;
/// - a string is keyed by [`Collation::sort_key`], trailing spaces and the differences
///   the collation ignores included;
/// - bytes are keyed without their trailing `0x00`, as the padding of [`compare`] has it;
/// - a `datetimeoffset` is keyed on its UTC instant.
///
/// Each key carries its own end, so the concatenation of the keys of the columns of two
/// rows orders and groups the rows column after column.
///
/// The collation is read for strings and ignored otherwise. A decimal whose scale is
/// beyond 38 has no key: the error is the one [`compare`] reports for it
/// (`a_decimal_scale_beyond_38_has_no_key`).
///
/// ```
/// use vauban_types::{sort_key, Collation, Decimal, SqlString, Value};
///
/// let key = |v: &Value| {
///     let mut out = Vec::new();
///     sort_key(v, &Collation::DEFAULT, &mut out).unwrap();
///     out
/// };
/// let text = |t: &str| Value::String(SqlString { text: t.to_owned() });
/// let decimal = |mantissa, scale| Value::Decimal(Decimal { mantissa, precision: 10, scale });
///
/// assert_eq!(key(&text("abc  ")), key(&text("ABC")));
/// assert!(key(&text("abc")) < key(&text("abd")));
/// assert_eq!(key(&decimal(15, 1)), key(&decimal(1500, 3)));
/// assert_eq!(key(&Value::F64(-0.0)), key(&Value::F64(0.0)));
/// assert_eq!(key(&Value::Bytes(vec![1])), key(&Value::Bytes(vec![1, 0, 0])));
/// ```
pub fn sort_key(v: &Value, collation: &Collation, out: &mut Vec<u8>) -> SqlResult<()> {
    write_key(v, collation, out)
}

/// Feeds the sort key of `v` under `collation` ([`sort_key`]) to `state`, in one
/// [`Hasher::write`]: two values equal under [`compare`] feed the same bytes, so they hash
/// alike under any [`Hasher`].
///
/// ```
/// use std::collections::hash_map::DefaultHasher;
/// use std::hash::Hasher;
/// use vauban_types::{hash_key, Collation, SqlString, Value};
///
/// let hash = |t: &str| {
///     let mut state = DefaultHasher::new();
///     let v = Value::String(SqlString { text: t.to_owned() });
///     hash_key(&v, &Collation::parse("Latin1_General_CI_AS").unwrap(), &mut state).unwrap();
///     state.finish()
/// };
/// assert_eq!(hash("Straße "), hash("STRASSE"));
/// assert_ne!(hash("Straße"), hash("Strasse-"));
/// ```
pub fn hash_key<H: Hasher>(v: &Value, collation: &Collation, state: &mut H) -> SqlResult<()> {
    KEY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.clear();
        write_key(v, collation, &mut buffer)?;
        state.write(&buffer);
        Ok(())
    })
}

thread_local! {
    /// The key [`hash_key`] builds, kept from one call to the next to spare an allocation.
    static KEY_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

impl Collation {
    /// Appends to `out` the sort key of `s` under this collation: a byte string whose order
    /// is the order of [`Collation::compare`], so that two strings are equal under it
    /// exactly when their keys are equal (`tests/sort_key.rs`). The key carries its own
    /// end, so that keys written end to end still order and group column after column
    /// (`string_keys_order_as_compare_under_each_collation`).
    ///
    /// Under a weight table the key holds, level after level, the weights
    /// [`Collation::compare`] compares at that level: a level the collation ignores (the
    /// case under `_CI`, the accents under `_AI`) leaves nothing, and the weights of the
    /// trailing spaces a level pads with are left out, so `'abc'` and `'ABC  '` share a
    /// key under `_CI`. Under `_BIN2` the key is the UTF-16 code units, trailing spaces
    /// left out; under `_BIN`, the first code unit then the bytes of the others.
    ///
    /// ```
    /// use vauban_types::Collation;
    ///
    /// let key = |collation: &Collation, s: &str| {
    ///     let mut out = Vec::new();
    ///     collation.sort_key(s, &mut out);
    ///     out
    /// };
    /// let latin1 = Collation::DEFAULT;
    /// assert_eq!(key(&latin1, "abc"), key(&latin1, "ABC  "));
    /// assert!(key(&latin1, "e") < key(&latin1, "é"));
    /// let cs = Collation::parse("Latin1_General_CS_AS").unwrap();
    /// assert_ne!(key(&cs, "abc"), key(&cs, "ABC"));
    /// let windows = Collation::parse("Latin1_General_CI_AS").unwrap();
    /// assert_eq!(key(&windows, "ae"), key(&windows, "Æ"));
    /// ```
    pub fn sort_key(&self, s: &str, out: &mut Vec<u8>) {
        self.write_string_key(s, out);
    }

    /// [`Collation::sort_key`], for [`sort_key`] and [`hash_key`].
    fn write_string_key(&self, s: &str, out: &mut Vec<u8>) {
        match self.rules() {
            Rules::Binary => write_binary_key(s.trim_end_matches(' '), out),
            Rules::Binary2 => {
                let mut level = PaddedLevel::new(u64::from(SPACE_UNIT), Width::Two);
                for unit in s.encode_utf16() {
                    level.push(u64::from(unit), out);
                }
                level.finish(out);
            }
            Rules::Table(table) => {
                let (levels, count) = self.levels(table);
                for &level in &levels[..count] {
                    if s.is_ascii() {
                        self.write_ascii_level(table, s.as_bytes(), level, out);
                    } else {
                        self.write_level(table, s, level, out);
                    }
                }
            }
        }
    }

    /// The key of one level of a table comparison, from [`Collation::level_weights`].
    fn write_level(&self, table: &Table, s: &str, level: Level, out: &mut Vec<u8>) {
        if level == Level::Last {
            for weight in self.level_weights(table, s, level) {
                write_unpadded(weight, Width::Eight, out);
            }
            out.extend_from_slice(&[END]);
            return;
        }
        let pad = self
            .level_weights(table, " ", level)
            .next()
            .unwrap_or_default();
        let mut key = PaddedLevel::new(pad, level_width(level));
        for weight in self.level_weights(table, s, level) {
            key.push(weight, out);
        }
        key.finish(out);
    }

    /// [`Collation::write_level`] for an ASCII string, through the rows of the table: the
    /// same weights, without the lookup of each character.
    fn write_ascii_level(&self, table: &Table, s: &[u8], level: Level, out: &mut Vec<u8>) {
        let rows = &table.ascii;
        if level == Level::Last {
            for weight in ascii_last(rows, s) {
                write_unpadded(weight, Width::Eight, out);
            }
            out.extend_from_slice(&[END]);
            return;
        }
        let flags = self.flags;
        let pad = ascii_weight(rows[usize::from(b' ')], level, flags);
        let mut key = PaddedLevel::new(pad, level_width(level));
        for byte in s {
            let row = rows[usize::from(*byte & 0x7F)];
            if row.class == AsciiClass::Plain {
                key.push(ascii_weight(row, level, flags), out);
            }
        }
        key.finish(out);
    }
}

/// The byte that ends a level, or a sequence, compared without padding: below the
/// [`MORE`] that precedes each of its elements.
const END: u8 = 0;
/// The byte before each element of a sequence compared without padding.
const MORE: u8 = 1;
/// How the weights of a sequence are written.
#[derive(Clone, Copy)]
enum Width {
    /// Two bytes: an accent weight, a code unit.
    Two,
    /// Two bytes below `0xFF00`, else `0xFF` and three bytes: a primary weight reaches past
    /// `0x10_0000` for a character outside the table, a tertiary weight holds a 16-bit
    /// case weight shifted by two, and most weights fit in two bytes.
    Wide,
    /// Eight bytes: a last-level weight, position and weight.
    Eight,
}

/// The width of the weights of a level before the last.
fn level_width(level: Level) -> Width {
    match level {
        Level::Accent => Width::Two,
        Level::Primary | Level::Own | Level::Tertiary => Width::Wide,
        Level::Last => Width::Eight,
    }
}

/// The first weight [`Width::Wide`] writes on four bytes.
const WIDE_FROM: u64 = 0xFF00;

/// Appends `weight` on `width`, most significant byte first, so that the order of the
/// bytes is the order of the weights.
fn put_weight(weight: u64, width: Width, out: &mut Vec<u8>) {
    let bytes = weight.to_be_bytes();
    match width {
        Width::Two => out.extend_from_slice(&bytes[6..]),
        Width::Wide if weight < WIDE_FROM => out.extend_from_slice(&bytes[6..]),
        Width::Wide => out.extend_from_slice(&[0xFF, bytes[5], bytes[6], bytes[7]]),
        Width::Eight => out.extend_from_slice(&bytes),
    }
}

/// Appends one element of a sequence compared without padding, where a sequence that is
/// a prefix of another is the smaller one.
fn write_unpadded(weight: u64, width: Width, out: &mut Vec<u8>) {
    out.extend_from_slice(&[MORE]);
    put_weight(weight, width, out);
}

/// The key of a sequence of weights compared with padding: the shorter sequence is
/// extended with the weight `pad` ([`Collation::compare`]).
///
/// A weight other than `pad` is written as it is. A `pad` weight is written with a flag
/// that says whether the next weight other than `pad` is below or above it, and the
/// trailing `pad` weights are dropped for an end marker that sits between the two
/// flags: the end of a sequence then compares with a run of `pad` weights as the
/// padding does, against the first weight after the run.
struct PaddedLevel {
    /// The padding weight.
    pad: u64,
    /// The width of the weights of this level.
    width: Width,
    /// The `pad` weights read and not yet written.
    pending: usize,
}

impl PaddedLevel {
    /// The flag of a `pad` weight followed by a smaller weight.
    const BELOW: u8 = 0;
    /// The flag of the end of the sequence.
    const END: u8 = 1;
    /// The flag of a `pad` weight followed by a larger weight.
    const ABOVE: u8 = 2;

    fn new(pad: u64, width: Width) -> Self {
        Self {
            pad,
            width,
            pending: 0,
        }
    }

    /// Takes the next weight of the sequence.
    fn push(&mut self, weight: u64, out: &mut Vec<u8>) {
        if weight == self.pad {
            self.pending += 1;
            return;
        }
        let flag = if weight < self.pad {
            Self::BELOW
        } else {
            Self::ABOVE
        };
        for _ in 0..self.pending {
            put_weight(self.pad, self.width, out);
            out.extend_from_slice(&[flag]);
        }
        self.pending = 0;
        put_weight(weight, self.width, out);
    }

    /// Ends the sequence, dropping its trailing `pad` weights.
    fn finish(self, out: &mut Vec<u8>) {
        put_weight(self.pad, self.width, out);
        out.extend_from_slice(&[Self::END]);
    }
}

/// The `_BIN` key of a string whose trailing spaces are already dropped: the first code
/// unit, then the little-endian bytes of the others, a shorter string sorting first.
/// The empty string is the bare end marker.
fn write_binary_key(s: &str, out: &mut Vec<u8>) {
    let mut units = s.encode_utf16();
    let Some(first) = units.next() else {
        out.extend_from_slice(&[END]);
        return;
    };
    write_unpadded(u64::from(first), Width::Two, out);
    for unit in units {
        write_escaped(&unit.to_le_bytes(), out);
    }
    out.extend_from_slice(&ESCAPED_END);
}

/// The first byte of a key: the family of the value.
#[derive(Clone, Copy)]
#[repr(u8)]
enum KeyFamily {
    Null,
    Bit,
    Integer,
    Decimal,
    Float,
    Money,
    String,
    Bytes,
    Guid,
    Date,
    Time,
    DateTime,
    DateTime2,
    DateTimeOffset,
}

/// [`sort_key`], for [`hash_key`] too.
fn write_key(v: &Value, collation: &Collation, out: &mut Vec<u8>) -> SqlResult<()> {
    let family = |family: KeyFamily| [family as u8];
    if let Some(x) = integer_as_i64(v) {
        out.extend_from_slice(&family(KeyFamily::Integer));
        out.extend_from_slice(&flip_sign(x).to_be_bytes());
        return Ok(());
    }
    if let Some(x) = float_as_f64(v) {
        out.extend_from_slice(&family(KeyFamily::Float));
        out.extend_from_slice(&float_key(x).to_be_bytes());
        return Ok(());
    }
    match v {
        Value::Null => out.extend_from_slice(&family(KeyFamily::Null)),
        Value::Bit(x) => out.extend_from_slice(&[KeyFamily::Bit as u8, u8::from(*x)]),
        Value::Decimal(x) => {
            out.extend_from_slice(&family(KeyFamily::Decimal));
            write_decimal_key(x, out)?;
        }
        Value::Money(x) => {
            out.extend_from_slice(&family(KeyFamily::Money));
            out.extend_from_slice(&flip_sign(*x).to_be_bytes());
        }
        Value::String(x) => {
            out.extend_from_slice(&family(KeyFamily::String));
            collation.write_string_key(&x.text, out);
        }
        Value::Bytes(x) => {
            out.extend_from_slice(&family(KeyFamily::Bytes));
            write_bytes_key(x, out);
        }
        Value::Guid(x) => {
            out.extend_from_slice(&family(KeyFamily::Guid));
            out.extend_from_slice(&GUID_ORDER.map(|index| x[index]));
        }
        Value::Date(x) => {
            out.extend_from_slice(&family(KeyFamily::Date));
            out.extend_from_slice(&flip_sign_32(x.days).to_be_bytes());
        }
        Value::Time(x) => {
            out.extend_from_slice(&family(KeyFamily::Time));
            out.extend_from_slice(&x.ticks_100ns.to_be_bytes());
        }
        Value::DateTime(x) => {
            out.extend_from_slice(&family(KeyFamily::DateTime));
            out.extend_from_slice(&flip_sign_32(x.days).to_be_bytes());
            out.extend_from_slice(&x.ticks_300th.to_be_bytes());
        }
        Value::DateTime2(x) => {
            out.extend_from_slice(&family(KeyFamily::DateTime2));
            write_datetime2_key(x, out);
        }
        Value::DateTimeOffset(x) => {
            out.extend_from_slice(&family(KeyFamily::DateTimeOffset));
            write_datetime2_key(&x.utc, out);
        }
        // Read before the match.
        Value::I8(_) | Value::I16(_) | Value::I32(_) | Value::I64(_) => {}
        Value::F32(_) | Value::F64(_) => {}
    }
    Ok(())
}

/// A signed integer as an unsigned one of the same order.
fn flip_sign(x: i64) -> u64 {
    (x as u64) ^ (1 << 63)
}

/// [`flip_sign`] on 32 bits.
fn flip_sign_32(x: i32) -> u32 {
    (x as u32) ^ (1 << 31)
}

/// A float as an unsigned integer of the same order, `-0.0` folded onto `0.0`.
fn float_key(x: f64) -> u64 {
    let x = if x == 0.0 { 0.0 } else { x };
    let bits = x.to_bits();
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    }
}

/// The key of a decimal: its sign, then for a non-zero value its integer part and its
/// fractional part on 38 digits, complemented when negative so that a larger magnitude
/// sorts first.
fn write_decimal_key(x: &Decimal, out: &mut Vec<u8>) -> SqlResult<()> {
    const NEGATIVE: u8 = 0;
    const ZERO: u8 = 1;
    const POSITIVE: u8 = 2;
    if x.mantissa == 0 {
        out.extend_from_slice(&[ZERO]);
        return Ok(());
    }
    if x.scale > MAX_DECIMAL_SCALE {
        return Err(bug(format!(
            "sort_key: decimal scale {} out of range",
            x.scale
        )));
    }
    let (integer, fraction) = split_decimal(x, MAX_DECIMAL_SCALE)?;
    if x.mantissa < 0 {
        out.extend_from_slice(&[NEGATIVE]);
        out.extend_from_slice(&(!integer).to_be_bytes());
        out.extend_from_slice(&(!fraction).to_be_bytes());
    } else {
        out.extend_from_slice(&[POSITIVE]);
        out.extend_from_slice(&integer.to_be_bytes());
        out.extend_from_slice(&fraction.to_be_bytes());
    }
    Ok(())
}

/// The largest scale of a `decimal`.
const MAX_DECIMAL_SCALE: u8 = 38;

/// The key of `binary` data: its bytes without the trailing `0x00`, each `0x00` escaped
/// so that the end marker sorts below any byte that could follow it.
fn write_bytes_key(x: &[u8], out: &mut Vec<u8>) {
    let end = x.iter().rposition(|byte| *byte != 0).map_or(0, |at| at + 1);
    write_escaped(&x[..end], out);
    out.extend_from_slice(&ESCAPED_END);
}

/// The end of a byte sequence written by [`write_escaped`].
const ESCAPED_END: [u8; 2] = [0, 0];

/// Appends `bytes` with each `0x00` written `0x00 0xFF`, so that [`ESCAPED_END`] sorts
/// below any byte that could follow in a longer sequence and cannot be read as an
/// escaped byte.
fn write_escaped(bytes: &[u8], out: &mut Vec<u8>) {
    for &byte in bytes {
        if byte == 0 {
            out.extend_from_slice(&[0, 0xFF]);
        } else {
            out.extend_from_slice(&[byte]);
        }
    }
}

/// The key of a `datetime2`: days, then ticks.
fn write_datetime2_key(x: &DateTime2, out: &mut Vec<u8>) {
    out.extend_from_slice(&flip_sign_32(x.date.days).to_be_bytes());
    out.extend_from_slice(&x.time.ticks_100ns.to_be_bytes());
}
