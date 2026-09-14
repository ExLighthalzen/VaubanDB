//! `default_display`: a value rendered as `CONVERT(varchar, x)` renders it, without style.
//!
//! One rendering serves three callers: the conversion of a value to a character type, the
//! text the error messages quote (`The varchar value '…' could not be converted`), and the
//! tests. Writing it once is what keeps the three consistent.
//!
//! The default style of each date type is 0 for `datetime` and `smalldatetime`, 121 (the
//! ODBC canonical form) for `date`, `time`, `datetime2` and `datetimeoffset`. The numeric
//! defaults are style 0 too: at most six significant digits for `float` and `real`, two
//! decimals for `money`.

use crate::calendar::{self, DAYS_1900, hms_from_ticks, ticks_300th_to_100ns};
use crate::{Date, DateTime, DateTime2, DateTimeOffset, Len, SqlType, Time, TypeInfo, Value};

/// Fractional-seconds digits of a `time(7)`, the finest SQL Server offers, and the scale
/// assumed when the declared type does not say (a broken precondition).
const MAX_FRACTION_DIGITS: u8 = 7;

/// Upper-case hexadecimal digits, the alphabet of a binary literal.
const HEX_DIGITS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F',
];

/// Renders `v`, whose type is `ty`, the way `CONVERT(varchar(40), v)` renders it.
///
/// This is the default (style 0) rendering, and the only one this crate offers: the styles of
/// `CONVERT` belong to the conversion itself (`convert::to_character` for `float` and
/// `money`, `convert::datetime` for dates, `convert::binary` for binaries).
///
/// # What `ty` decides
///
/// The variant of `v` fixes the shape of the result; `ty` supplies the declared parameters
/// the value does not carry: the scale of a `decimal`, the fractional digits of a `time`, the
/// length a `char` is padded to, the length a `binary` is padded to.
///
/// When the scale of a [`crate::Decimal`] differs from the scale `ty` declares, the mantissa
/// is **rescaled to the scale of `ty`**, rounding half away from zero — the rounding of SQL
/// Server, never truncation and never Rust's round-half-to-even.
///
/// # Two deliberate departures from `CONVERT`
///
/// * `Value::Null` renders as the four letters `NULL`. SQL Server never renders `NULL` as
///   text — `CONVERT(varchar, NULL)` is `NULL` — but this function also composes error
///   messages, where a `String` is the only possible answer. A caller that produces a SQL
///   value (`convert`) handles `Value::Null` before calling in.
/// * A binary value renders as `0x` followed by upper-case hexadecimal, which is `CONVERT`
///   **style 1**, not style 0: `CONVERT(varchar(40), 0x4E616D65)` without style yields the
///   bytes read as characters (`Name`). The hexadecimal form is the one error messages, tests
///   and clients need; the real binary-to-character conversion is `convert::to_character`'s.
///
/// # Broken preconditions
///
/// The function returns a `String`, never an error, and never aborts in a release build. When
/// the variant of `v` does not match `ty.ty` — a [`Value::I32`] with a `varchar` type, say —
/// it renders the natural form of `v` (what its own family imposes) and trips a
/// `debug_assert!`: that mismatch is a bug of the caller, and raising a second error while
/// building the text of a first one would be worse than a slightly odd string.
pub fn default_display(v: &Value, ty: &TypeInfo) -> String {
    let t = &ty.ty;
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Bit(b) => {
            check_type(matches!(t, SqlType::Bit), v, t);
            (if *b { "1" } else { "0" }).to_owned()
        }
        Value::I8(n) => {
            check_type(matches!(t, SqlType::TinyInt), v, t);
            n.to_string()
        }
        Value::I16(n) => {
            check_type(matches!(t, SqlType::SmallInt), v, t);
            n.to_string()
        }
        Value::I32(n) => {
            check_type(matches!(t, SqlType::Int), v, t);
            n.to_string()
        }
        Value::I64(n) => {
            check_type(matches!(t, SqlType::BigInt), v, t);
            n.to_string()
        }
        Value::Decimal(d) => {
            let scale = match t {
                SqlType::Decimal { scale, .. } | SqlType::Numeric { scale, .. } => *scale,
                _ => {
                    check_type(false, v, t);
                    d.scale
                }
            };
            match rescale(d.mantissa, d.scale, scale) {
                Some(mantissa) => plain_decimal(mantissa, scale),
                // Unreachable with a well-formed `decimal`: 38 digits fit in an `i128`.
                None => {
                    check_type(false, v, t);
                    plain_decimal(d.mantissa, d.scale)
                }
            }
        }
        Value::F64(x) => {
            check_type(matches!(t, SqlType::Float), v, t);
            float_style_zero(*x)
        }
        Value::F32(x) => {
            check_type(matches!(t, SqlType::Real), v, t);
            float_style_zero(*x as f64)
        }
        Value::Money(m) => {
            check_type(matches!(t, SqlType::Money | SqlType::SmallMoney), v, t);
            money_style_zero(*m)
        }
        Value::String(s) => match t {
            SqlType::Char(Len::Fixed(n)) | SqlType::NChar(Len::Fixed(n)) => pad_right(&s.text, *n),
            SqlType::Char(Len::Max)
            | SqlType::NChar(Len::Max)
            | SqlType::VarChar(_)
            | SqlType::NVarChar(_) => s.text.clone(),
            _ => {
                check_type(false, v, t);
                s.text.clone()
            }
        },
        Value::Bytes(b) => match t {
            SqlType::Binary(Len::Fixed(n)) => {
                let mut padded = b.clone();
                padded.resize(usize::from(*n).max(b.len()), 0);
                hex_literal(&padded)
            }
            SqlType::Binary(Len::Max) | SqlType::VarBinary(_) => hex_literal(b),
            _ => {
                check_type(false, v, t);
                hex_literal(b)
            }
        },
        Value::Date(d) => {
            check_type(matches!(t, SqlType::Date), v, t);
            iso_date(*d)
        }
        Value::Time(time) => {
            let scale = match t {
                SqlType::Time(s) => *s,
                _ => {
                    check_type(false, v, t);
                    MAX_FRACTION_DIGITS
                }
            };
            iso_time(*time, scale)
        }
        Value::DateTime(dt) => {
            check_type(
                matches!(t, SqlType::DateTime | SqlType::SmallDateTime),
                v,
                t,
            );
            datetime_style_zero(*dt)
        }
        Value::DateTime2(dt) => {
            let scale = match t {
                SqlType::DateTime2(s) => *s,
                _ => {
                    check_type(false, v, t);
                    MAX_FRACTION_DIGITS
                }
            };
            format!("{} {}", iso_date(dt.date), iso_time(dt.time, scale))
        }
        Value::DateTimeOffset(dto) => {
            let scale = match t {
                SqlType::DateTimeOffset(s) => *s,
                _ => {
                    check_type(false, v, t);
                    MAX_FRACTION_DIGITS
                }
            };
            iso_datetimeoffset(*dto, scale)
        }
        Value::Guid(bytes) => {
            check_type(matches!(t, SqlType::UniqueIdentifier), v, t);
            guid(bytes)
        }
    }
}

/// Trips a `debug_assert!` when the variant of `v` does not match `ty`, and does nothing
/// in a release build. See the "Broken preconditions" section of [`default_display`].
fn check_type(ok: bool, v: &Value, ty: &SqlType) {
    debug_assert!(
        ok,
        "default_display: value {v:?} does not match type {}",
        ty.declaration()
    );
}

/// `n / d` rounded to the nearest, halves **away from zero** — the rounding of SQL Server,
/// never Rust's round-half-to-even and never a truncation. `d` must be strictly positive.
fn divide_rounding(n: i128, d: i128) -> i128 {
    let quotient = n / d;
    let remainder = n % d;
    // `2 * |r| >= d` bumps the quotient one step in the direction of the sign of `n`. The
    // comparison goes through `u128` because `2 * |r|` may not fit in an `i128`.
    if remainder.unsigned_abs() * 2 >= d.unsigned_abs() {
        quotient + if n < 0 { -1 } else { 1 }
    } else {
        quotient
    }
}

/// Moves `mantissa` from scale `from` to scale `to`, rounding half away from zero.
///
/// `None` when scaling up overflows an `i128`, which a `decimal(38, s)` never does.
fn rescale(mantissa: i128, from: u8, to: u8) -> Option<i128> {
    if to == from {
        return Some(mantissa);
    }
    if to > from {
        return 10_i128
            .checked_pow(u32::from(to - from))
            .and_then(|f| mantissa.checked_mul(f));
    }
    let divisor = 10_i128.checked_pow(u32::from(from - to))?;
    Some(divide_rounding(mantissa, divisor))
}

/// Renders `mantissa / 10^scale` in plain notation with exactly `scale` decimals.
fn plain_decimal(mantissa: i128, scale: u8) -> String {
    let digits = mantissa.unsigned_abs().to_string();
    let sign = if mantissa < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    // At least one digit before the point: `5` at scale 3 is `0.005`, not `.005`.
    let width = usize::from(scale) + 1;
    let padded = format!("{digits:0>width$}");
    let split = padded.len() - usize::from(scale);
    format!("{sign}{}.{}", &padded[..split], &padded[split..])
}

/// Renders a `money` amount, held in ten-thousandths, with two decimals (style 0).
///
/// No thousands separator: that is style 1. Rounding is half away from zero, done on
/// integers — a `money` never goes through an `f64`.
fn money_style_zero(amount: i64) -> String {
    // From ten-thousandths to hundredths: two decimals fewer.
    plain_decimal(divide_rounding(i128::from(amount), 100), 2)
}

/// Renders a `float` or a `real` with style 0.
///
/// Style 0 is at most 6 digits, in scientific notation when appropriate. Concretely, the
/// value is rounded to six significant digits, then written in plain notation when its
/// decimal exponent
/// lies in `-4..6`, and in scientific notation otherwise — the rule of `%g` in C. Trailing
/// zeros of the mantissa are dropped (`1000`, not `1000.00`; `1.23457e+006`, not
/// `1.234570e+006`), and the exponent is always signed and padded to **three** digits.
///
/// A `real` goes through an `f64` first: six significant digits never see the difference.
fn float_style_zero(x: f64) -> String {
    if !x.is_finite() {
        // SQL Server has no infinity or NaN in a `float`; nothing can produce one here.
        return x.to_string();
    }
    if x == 0.0 {
        return "0".to_owned();
    }
    // `{:.5e}` gives six significant digits: one before the point and five after.
    let scientific = format!("{x:.5e}");
    // `{:e}` always writes a parsable exponent; the fallbacks below are unreachable.
    let (mantissa, exponent) = match scientific.split_once('e') {
        Some((m, e)) => match e.parse::<i32>() {
            Ok(exponent) => (m, exponent),
            Err(_) => (m, 0),
        },
        None => (scientific.as_str(), 0),
    };
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    // The six significant digits, point removed: `1.23457` becomes `123457`.
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();

    if !(-4..6).contains(&exponent) {
        let mantissa = trim_zeros(mantissa);
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        return format!("{sign}{mantissa}e{exponent_sign}{:03}", exponent.abs());
    }
    // Plain notation: the point goes after `exponent + 1` significant digits.
    let point = exponent + 1;
    let plain = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if (point as usize) >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    format!("{sign}{}", trim_zeros(&plain))
}

/// Drops the trailing zeros of a decimal fraction, and the point when nothing is left of it.
/// A string without a point is returned unchanged.
fn trim_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_owned();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Pads `text` on the right with spaces up to `n` **characters**, the way a `char(n)` or an
/// `nchar(n)` stores it. A longer text is returned as is: truncation is `convert`'s job.
fn pad_right(text: &str, n: u16) -> String {
    let length = text.chars().count();
    let missing = usize::from(n).saturating_sub(length);
    let mut out = String::with_capacity(text.len() + missing);
    out.push_str(text);
    for _ in 0..missing {
        out.push(' ');
    }
    out
}

/// `0x` followed by the bytes in upper-case hexadecimal, the literal form of T-SQL.
fn hex_literal(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + 2 * bytes.len());
    out.push_str("0x");
    for b in bytes {
        out.push(HEX_DIGITS[usize::from(b >> 4)]);
        out.push(HEX_DIGITS[usize::from(b & 0x0f)]);
    }
    out
}

/// `yyyy-mm-dd`, the default (style 121) rendering of a `date`.
fn iso_date(d: Date) -> String {
    let (y, m, day) = calendar::civil_from_days(d.days);
    format!("{y:04}-{m:02}-{day:02}")
}

/// `hh:mi:ss` plus a point and `scale` fractional digits when `scale > 0`, the default
/// (style 121) rendering of a `time(s)`.
///
/// The fraction is **truncated** to `scale` digits: a `time(3)` holds three digits, and the
/// extra ones a [`Time`] can carry are not part of the value.
fn iso_time(t: Time, scale: u8) -> String {
    let (h, mi, s, fraction) = hms_from_ticks(t.ticks_100ns);
    let head = format!("{h:02}:{mi:02}:{s:02}");
    if scale == 0 {
        return head;
    }
    let seven = format!("{fraction:07}");
    let kept = usize::from(scale.min(MAX_FRACTION_DIGITS));
    format!("{head}.{}", &seven[..kept])
}

/// `yyyy-mm-dd hh:mi:ss[.fff…] ±hh:mm`, the default (style 121) rendering of a
/// `datetimeoffset(s)`.
///
/// The date and time are shown **in the local time of the offset**, that is `utc + offset`,
/// which is what SQL Server stored before converting the value to UTC.
fn iso_datetimeoffset(dto: DateTimeOffset, scale: u8) -> String {
    let ticks_per_day = calendar::TICKS_PER_DAY as i64;
    let shifted = dto.utc.time.ticks_100ns as i64 + i64::from(dto.offset_minutes) * 600_000_000;
    let local = DateTime2 {
        date: Date {
            days: dto.utc.date.days + shifted.div_euclid(ticks_per_day) as i32,
        },
        time: Time {
            ticks_100ns: shifted.rem_euclid(ticks_per_day) as u64,
        },
    };
    let sign = if dto.offset_minutes < 0 { '-' } else { '+' };
    let minutes = dto.offset_minutes.unsigned_abs();
    format!(
        "{} {} {sign}{:02}:{:02}",
        iso_date(local.date),
        iso_time(local.time, scale),
        minutes / 60,
        minutes % 60
    )
}

/// `mon dd yyyy hh:miAM`, style 0 of `datetime` and `smalldatetime`.
///
/// `mon` is the three-letter English month; the day and the hour are right-aligned on two
/// characters, so a day or an hour below 10 is preceded by a space; the clock is a 12-hour
/// one, midnight reading `12:00AM` and noon `12:00PM`; there is no space before `AM`/`PM` and
/// no seconds at all.
fn datetime_style_zero(dt: DateTime) -> String {
    let (y, m, day) = calendar::civil_from_days(DAYS_1900.saturating_add(dt.days));
    let (h, mi, _, _) = hms_from_ticks(ticks_300th_to_100ns(dt.ticks_300th));
    let month = month_abbreviation(m);
    let meridiem = if h < 12 { "AM" } else { "PM" };
    let hour12 = if h % 12 == 0 { 12 } else { h % 12 };
    format!("{month} {day:>2} {y:04} {hour12:>2}:{mi:02}{meridiem}")
}

/// The three-letter English month abbreviation `datetime` style 0 prints.
///
/// The last arm is unreachable: [`calendar::civil_from_days`] always yields `1..=12`.
fn month_abbreviation(m: u8) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

/// The 8-4-4-4-12 upper-case hexadecimal form of a `uniqueidentifier`.
///
/// [`Value::Guid`] holds the 16 bytes in the order SQL Server stores and transmits them: the
/// first three groups are little-endian, the last two are read in order.
fn guid(b: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[3],
        b[2],
        b[1],
        b[0],
        b[5],
        b[4],
        b[7],
        b[6],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pieces that have no direct vector in `tests/display.rs` because they are private.
    #[test]
    fn rescale_rounds_half_away_from_zero() {
        assert_eq!(rescale(150, 2, 2), Some(150));
        assert_eq!(rescale(15, 1, 3), Some(1500));
        assert_eq!(rescale(105, 3, 2), Some(11)); // 0.105 -> 0.11, not 0.10
        assert_eq!(rescale(-105, 3, 2), Some(-11));
        assert_eq!(rescale(104, 3, 2), Some(10));
        assert_eq!(rescale(-104, 3, 2), Some(-10));
        assert_eq!(rescale(5, 1, 0), Some(1)); // 0.5 -> 1
        assert_eq!(rescale(-5, 1, 0), Some(-1));
        assert_eq!(rescale(i128::MAX, 0, 38), None);
    }

    #[test]
    fn trim_zeros_keeps_integers_intact() {
        assert_eq!(trim_zeros("1000"), "1000");
        assert_eq!(trim_zeros("1.00000"), "1");
        assert_eq!(trim_zeros("0.100000"), "0.1");
        assert_eq!(trim_zeros("1.23457"), "1.23457");
    }

    #[test]
    fn pad_right_counts_characters_not_bytes() {
        assert_eq!(pad_right("ab", 5), "ab   ");
        assert_eq!(pad_right("éé", 4), "éé  ");
        assert_eq!(pad_right("abcdef", 3), "abcdef");
    }
}
