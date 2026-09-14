//! Integration tests of `vauban_types::compare`: three-valued logic, one rule per
//! value family, strings under the default collation, and the precondition errors.

use std::cmp::Ordering::{Equal, Greater, Less};

use vauban_types::{
    Collation, Date, DateTime, DateTime2, DateTimeOffset, Decimal, SqlString, Time, Value, compare,
};

fn text(t: &str) -> Value {
    Value::String(SqlString { text: t.to_owned() })
}

fn decimal(mantissa: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal(Decimal {
        mantissa,
        precision,
        scale,
    })
}

fn cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    compare(a, b, &Collation::DEFAULT).expect("same family under the default collation")
}

fn assert_internal_bug(a: &Value, b: &Value, collation: &Collation) {
    let err = compare(a, b, collation).expect_err("expected an internal error");
    assert_eq!(err.number, 50000);
    assert_eq!(err.severity, 16);
    assert!(
        err.message.starts_with("Internal error: "),
        "unexpected message: {}",
        err.message
    );
}

#[test]
fn compare_null_with_anything_is_none() {
    assert_eq!(cmp(&Value::Null, &Value::I32(1)), None);
    assert_eq!(cmp(&Value::I32(1), &Value::Null), None);
    assert_eq!(cmp(&Value::Null, &Value::Null), None);
    // NULL wins over an incompatible pair: no error, just UNKNOWN.
    assert_eq!(cmp(&Value::Null, &text("x")), None);
    assert_eq!(cmp(&Value::Bytes(vec![1]), &Value::Null), None);
}

#[test]
fn compare_bits() {
    assert_eq!(cmp(&Value::Bit(false), &Value::Bit(true)), Some(Less));
    assert_eq!(cmp(&Value::Bit(true), &Value::Bit(false)), Some(Greater));
    assert_eq!(cmp(&Value::Bit(true), &Value::Bit(true)), Some(Equal));
}

#[test]
fn compare_strings_are_case_insensitive_for_ascii() {
    assert_eq!(cmp(&text("abc"), &text("ABC")), Some(Equal));
    assert_eq!(cmp(&text("Hello World"), &text("hELLO wORLD")), Some(Equal));
}

#[test]
fn compare_strings_ignore_trailing_spaces() {
    assert_eq!(cmp(&text("abc"), &text("abc  ")), Some(Equal));
    assert_eq!(cmp(&text("abc  "), &text("abc")), Some(Equal));
    assert_eq!(cmp(&text(""), &text("   ")), Some(Equal));
    // Only U+0020 is a padding space: a trailing tab is significant.
    assert_eq!(cmp(&text("abc"), &text("abc\t")), Some(Less));
}

#[test]
fn compare_strings_keep_leading_spaces() {
    assert_eq!(cmp(&text(" a"), &text("a")), Some(Less));
    assert_eq!(cmp(&text("a"), &text(" a")), Some(Greater));
}

#[test]
fn compare_strings_order_follows_the_collation() {
    assert_eq!(cmp(&text("a"), &text("b")), Some(Less));
    // Folding happens before the comparison: 'a' (folded to 'A') sorts before 'B'.
    assert_ne!(cmp(&text("B"), &text("a")), Some(Less));
    assert_eq!(cmp(&text("a"), &text("B")), Some(Less));
    // Accent-sensitive: an accented letter comes after its base letter, whatever the
    // case of that base letter.
    assert_eq!(cmp(&text("é"), &text("E")), Some(Greater));
    assert_eq!(cmp(&text("é"), &text("e")), Some(Greater));
    // A prefix is smaller than the longer string.
    assert_eq!(cmp(&text("ab"), &text("abc")), Some(Less));
    assert_eq!(cmp(&text("abc"), &text("ab")), Some(Greater));
    assert_eq!(cmp(&text(""), &text("a")), Some(Less));
}

#[test]
fn compare_integers_of_different_widths() {
    assert_eq!(cmp(&Value::I8(255), &Value::I16(3)), Some(Greater));
    assert_eq!(cmp(&Value::I64(-1), &Value::I32(0)), Some(Less));
    assert_eq!(cmp(&Value::I16(7), &Value::I64(7)), Some(Equal));
    assert_eq!(
        cmp(&Value::I32(i32::MIN), &Value::I64(i64::MIN)),
        Some(Greater)
    );
    assert_eq!(cmp(&Value::I8(0), &Value::I16(-1)), Some(Greater));
}

#[test]
fn compare_decimals_with_different_scales() {
    // 1.50 == 1.5
    assert_eq!(cmp(&decimal(150, 3, 2), &decimal(15, 2, 1)), Some(Equal));
    // -0.1 < 0.01
    assert_eq!(cmp(&decimal(-1, 1, 1), &decimal(1, 2, 2)), Some(Less));
    // 38 nines (precision 38, scale 0) > 1, without overflow.
    let thirty_eight_nines: i128 = 99_999_999_999_999_999_999_999_999_999_999_999_999;
    assert_eq!(
        cmp(&decimal(thirty_eight_nines, 38, 0), &decimal(1, 1, 0)),
        Some(Greater)
    );
    // Large mantissa against a large scale: aligning by multiplication would overflow.
    assert_eq!(
        cmp(
            &decimal(thirty_eight_nines, 38, 0),
            &decimal(thirty_eight_nines, 38, 38)
        ),
        Some(Greater)
    );
    // Negatives: magnitudes are compared then the sign is applied.
    assert_eq!(cmp(&decimal(-150, 3, 2), &decimal(-125, 3, 2)), Some(Less));
    assert_eq!(cmp(&decimal(-1, 1, 0), &decimal(-15, 2, 1)), Some(Greater));
    // Zero equals zero whatever the scale.
    assert_eq!(cmp(&decimal(0, 5, 3), &decimal(0, 1, 0)), Some(Equal));
    // Integer part decides before the fractional part.
    assert_eq!(
        cmp(&decimal(2_001, 4, 3), &decimal(19, 2, 1)),
        Some(Greater)
    );
    assert_eq!(
        cmp(&decimal(10_001, 5, 4), &decimal(1_001, 4, 3)),
        Some(Less)
    );
}

#[test]
fn compare_money() {
    assert_eq!(
        cmp(&Value::Money(10_000), &Value::Money(10_000)),
        Some(Equal)
    );
    assert_eq!(cmp(&Value::Money(-1), &Value::Money(0)), Some(Less));
}

#[test]
fn compare_floats_across_f32_and_f64() {
    assert_eq!(cmp(&Value::F32(1.5), &Value::F64(1.5)), Some(Equal));
    assert_eq!(cmp(&Value::F64(-0.0), &Value::F64(0.0)), Some(Equal));
    assert_eq!(cmp(&Value::F64(1.0), &Value::F32(2.0)), Some(Less));
    assert_eq!(cmp(&Value::F32(-1.0), &Value::F32(-2.0)), Some(Greater));
}

#[test]
fn compare_nan_is_internal_bug_not_null() {
    assert_internal_bug(&Value::F64(f64::NAN), &Value::F64(1.0), &Collation::DEFAULT);
}

#[test]
fn compare_bytes_pad_shorter_with_zero() {
    assert_eq!(
        cmp(&Value::Bytes(vec![0x01]), &Value::Bytes(vec![0x01, 0x00])),
        Some(Equal)
    );
    assert_eq!(
        cmp(&Value::Bytes(vec![0x01]), &Value::Bytes(vec![0x02])),
        Some(Less)
    );
    assert_eq!(
        cmp(
            &Value::Bytes(vec![0x01, 0x00]),
            &Value::Bytes(vec![0x01, 0x01])
        ),
        Some(Less)
    );
    assert_eq!(
        cmp(&Value::Bytes(vec![0x01, 0x01]), &Value::Bytes(vec![0x01])),
        Some(Greater)
    );
    assert_eq!(
        cmp(&Value::Bytes(vec![]), &Value::Bytes(vec![0x00, 0x00])),
        Some(Equal)
    );
}

/// `uniqueidentifier` values compare by groups of the textual form, from the last group
/// to the first, and inside a group by the stored bytes read from left to right. The two
/// vectors below are both the **opposite** of the ordinal order of the 16 stored bytes.
#[test]
fn compare_guids_the_way_sql_server_orders_them() {
    // `00000000-0000-0000-0000-000000000001` against `01000000-0000-0000-0000-000000000000`:
    // the last group decides, so the first value is the greater one, although its stored
    // bytes are the smaller ones.
    let mut last_group = [0u8; 16];
    let mut first_group = [0u8; 16];
    last_group[15] = 1;
    first_group[3] = 1;
    assert_eq!(
        cmp(&Value::Guid(last_group), &Value::Guid(last_group)),
        Some(Equal)
    );
    assert_eq!(
        cmp(&Value::Guid(last_group), &Value::Guid(first_group)),
        Some(Greater)
    );

    // Inside the first group, the little-endian storage is read in storage order:
    // `01000000-…` (stored `00 00 00 01`) is **less** than `00000100-…` (stored `00 01 00 00`).
    let mut low = [0u8; 16];
    let mut high = [0u8; 16];
    low[3] = 1;
    high[1] = 1;
    assert_eq!(cmp(&Value::Guid(low), &Value::Guid(high)), Some(Less));
}

#[test]
fn compare_dates_and_times() {
    // date
    assert_eq!(
        cmp(
            &Value::Date(Date { days: 730_119 }),
            &Value::Date(Date { days: 730_120 })
        ),
        Some(Less)
    );
    // time
    assert_eq!(
        cmp(
            &Value::Time(Time { ticks_100ns: 10 }),
            &Value::Time(Time { ticks_100ns: 9 })
        ),
        Some(Greater)
    );
    // datetime: days first, then ticks
    assert_eq!(
        cmp(
            &Value::DateTime(DateTime {
                days: 1,
                ticks_300th: 0
            }),
            &Value::DateTime(DateTime {
                days: 0,
                ticks_300th: 25_919_999
            })
        ),
        Some(Greater)
    );
    assert_eq!(
        cmp(
            &Value::DateTime(DateTime {
                days: 0,
                ticks_300th: 1
            }),
            &Value::DateTime(DateTime {
                days: 0,
                ticks_300th: 2
            })
        ),
        Some(Less)
    );
    // datetime2: date first, then time
    let dt2 = |days: i32, ticks_100ns: u64| DateTime2 {
        date: Date { days },
        time: Time { ticks_100ns },
    };
    assert_eq!(
        cmp(&Value::DateTime2(dt2(5, 0)), &Value::DateTime2(dt2(4, 999))),
        Some(Greater)
    );
    assert_eq!(
        cmp(&Value::DateTime2(dt2(5, 1)), &Value::DateTime2(dt2(5, 1))),
        Some(Equal)
    );
    // datetimeoffset: same instant, different offsets -> Equal
    assert_eq!(
        cmp(
            &Value::DateTimeOffset(DateTimeOffset {
                utc: dt2(730_119, 36_000_000_000),
                offset_minutes: 120
            }),
            &Value::DateTimeOffset(DateTimeOffset {
                utc: dt2(730_119, 36_000_000_000),
                offset_minutes: -300
            })
        ),
        Some(Equal)
    );
    assert_eq!(
        cmp(
            &Value::DateTimeOffset(DateTimeOffset {
                utc: dt2(730_119, 1),
                offset_minutes: 0
            }),
            &Value::DateTimeOffset(DateTimeOffset {
                utc: dt2(730_119, 2),
                offset_minutes: 0
            })
        ),
        Some(Less)
    );
}

#[test]
fn compare_incompatible_families_is_internal_bug() {
    let default = &Collation::DEFAULT;
    assert_internal_bug(&Value::I32(1), &decimal(1, 1, 0), default);
    assert_internal_bug(&text("a"), &Value::Bytes(vec![0x61]), default);
    assert_internal_bug(
        &Value::DateTime(DateTime {
            days: 0,
            ticks_300th: 0,
        }),
        &Value::DateTime2(DateTime2 {
            date: Date { days: 0 },
            time: Time { ticks_100ns: 0 },
        }),
        default,
    );
    assert_internal_bug(&Value::I32(1), &Value::F64(1.0), default);
    assert_internal_bug(&Value::Bit(true), &Value::I8(1), default);
    assert_internal_bug(&Value::Money(1), &decimal(1, 1, 0), default);

    // The message names both variants.
    let err = compare(&Value::I32(1), &decimal(1, 1, 0), default).expect_err("incompatible");
    assert!(err.message.contains("I32"), "message: {}", err.message);
    assert!(err.message.contains("Decimal"), "message: {}", err.message);
}

#[test]
fn compare_via_value() {
    // Strings go through `Collation::compare`, whose rules the integration test
    // `collation.rs` covers: what matters here is that `compare` applies them. `'e' = 'é'`
    // is false and `'e' < 'é'` is true, and the collation ignores case, so 'é' > 'E'.
    assert_eq!(
        compare(&text("é"), &text("E"), &Collation::DEFAULT),
        Ok(Some(Greater))
    );
    assert_eq!(
        compare(&text("a-b"), &text("ab"), &Collation::DEFAULT),
        Ok(Some(Less))
    );

    // Any other collation is *compared* like the default one: `Collation::parse` names
    // more collations than `compare` implements, an assumed gap.
    let case_sensitive = Collation {
        lcid: 0x0409,
        flags: 0x0C,
        version: 0,
        sort_id: 51,
    };
    assert_ne!(case_sensitive, Collation::DEFAULT);
    assert_eq!(
        compare(&text("a"), &text("A"), &case_sensitive),
        Ok(Some(Equal))
    );
    // The collation is ignored for non-string values.
    assert_eq!(
        compare(&Value::I32(1), &Value::I32(2), &case_sensitive),
        Ok(Some(Less))
    );
    assert_eq!(compare(&Value::Null, &text("a"), &case_sensitive), Ok(None));
}
