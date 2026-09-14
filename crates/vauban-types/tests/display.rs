//! The civil calendar and `default_display`, the style-0 rendering of a value.

use vauban_types::calendar::{DAYS_1900, civil_from_days, days_from_civil, is_valid_civil};
use vauban_types::{
    Date, DateTime, DateTime2, DateTimeOffset, Decimal, Len, SqlString, SqlType, Time, TypeInfo,
    Value, default_display,
};

/// `TypeInfo` of `ty`, nullable, the shape every vector below uses.
fn t(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, true)
}

/// `decimal(p, s)` holding `mantissa`.
fn decimal(mantissa: i128, precision: u8, scale: u8) -> (Value, TypeInfo) {
    (
        Value::Decimal(Decimal {
            mantissa,
            precision,
            scale,
        }),
        t(SqlType::Decimal { precision, scale }),
    )
}

/// Renders a value with the type it is meant to have.
fn show(v: Value, ty: SqlType) -> String {
    default_display(&v, &t(ty))
}

#[test]
fn calendar_round_trip() {
    for days in [0, 1, 59, 60, 365, 366, 730_119, 3_652_058] {
        let (y, m, d) = civil_from_days(days);
        assert_eq!(days_from_civil(y, m, d), days, "day {days} -> {y}-{m}-{d}");
    }
    assert_eq!(civil_from_days(0), (1, 1, 1));
    assert_eq!(civil_from_days(730_119), (2000, 1, 1));
    assert_eq!(civil_from_days(3_652_058), (9999, 12, 31));
    assert_eq!(days_from_civil(1900, 1, 1), 693_595);
    assert_eq!(DAYS_1900, 693_595);
}

#[test]
fn calendar_leap_years() {
    // 1900 is divisible by 100 but not by 400: not a leap year.
    assert!(!is_valid_civil(1900, 2, 29));
    assert_eq!(
        days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 1),
        28
    );
    // 2000 is divisible by 400, 2024 by 4 only: both are leap years.
    assert!(is_valid_civil(2000, 2, 29));
    assert!(is_valid_civil(2024, 2, 29));
    assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
    assert_eq!(
        days_from_civil(2001, 1, 1) - days_from_civil(2000, 1, 1),
        366
    );
    assert_eq!(
        days_from_civil(1901, 1, 1) - days_from_civil(1900, 1, 1),
        365
    );

    // The bounds of `date`.
    assert!(is_valid_civil(1, 1, 1));
    assert!(is_valid_civil(9999, 12, 31));
    assert!(!is_valid_civil(0, 12, 31));
    assert!(!is_valid_civil(10_000, 1, 1));
    assert!(!is_valid_civil(2024, 13, 1));
    assert!(!is_valid_civil(2024, 4, 31));
    assert!(!is_valid_civil(2024, 1, 0));
}

#[test]
fn display_null_is_the_four_letters() {
    assert_eq!(default_display(&Value::Null, &t(SqlType::Int)), "NULL");
    assert_eq!(
        default_display(&Value::Null, &t(SqlType::VarChar(Len::Fixed(10)))),
        "NULL"
    );
}

#[test]
fn display_integers_and_bit() {
    assert_eq!(show(Value::I8(255), SqlType::TinyInt), "255");
    assert_eq!(show(Value::I16(-32768), SqlType::SmallInt), "-32768");
    assert_eq!(
        show(Value::I32(-2_147_483_648), SqlType::Int),
        "-2147483648"
    );
    assert_eq!(
        show(Value::I64(9_223_372_036_854_775_807), SqlType::BigInt),
        "9223372036854775807"
    );
    assert_eq!(show(Value::Bit(true), SqlType::Bit), "1");
    assert_eq!(show(Value::Bit(false), SqlType::Bit), "0");
}

#[test]
fn display_exact_numeric() {
    let (v, ty) = decimal(150, 5, 2);
    assert_eq!(default_display(&v, &ty), "1.50");

    let (v, ty) = decimal(0, 1, 0);
    assert_eq!(default_display(&v, &ty), "0");

    let (v, ty) = decimal(0, 5, 2);
    assert_eq!(default_display(&v, &ty), "0.00");

    let (v, ty) = decimal(-5, 5, 3);
    assert_eq!(default_display(&v, &ty), "-0.005");

    let (v, ty) = decimal(99_999_999_999_999_999_999_999_999_999_999_999_999, 38, 0);
    assert_eq!(
        default_display(&v, &ty),
        "99999999999999999999999999999999999999"
    );

    // `numeric(p, s)` renders like `decimal(p, s)`: one representation, two names.
    assert_eq!(
        default_display(
            &Value::Decimal(Decimal {
                mantissa: -150,
                precision: 5,
                scale: 2
            }),
            &t(SqlType::Numeric {
                precision: 5,
                scale: 2
            })
        ),
        "-1.50"
    );

    // The declared scale wins over the scale the value carries, and the rescaling rounds
    // half away from zero.
    assert_eq!(
        default_display(
            &Value::Decimal(Decimal {
                mantissa: 1_005,
                precision: 5,
                scale: 3
            }),
            &t(SqlType::Decimal {
                precision: 5,
                scale: 2
            })
        ),
        "1.01"
    );
    assert_eq!(
        default_display(
            &Value::Decimal(Decimal {
                mantissa: -1_005,
                precision: 5,
                scale: 3
            }),
            &t(SqlType::Decimal {
                precision: 5,
                scale: 2
            })
        ),
        "-1.01"
    );
}

#[test]
fn display_money() {
    assert_eq!(show(Value::Money(15_000), SqlType::Money), "1.50");
    assert_eq!(show(Value::Money(103_497), SqlType::Money), "10.35");
    assert_eq!(show(Value::Money(-103_497), SqlType::Money), "-10.35");
    assert_eq!(show(Value::Money(0), SqlType::Money), "0.00");
    assert_eq!(show(Value::Money(15_000), SqlType::SmallMoney), "1.50");
    // Half away from zero, not to even: 0.005 goes up on both signs.
    assert_eq!(show(Value::Money(50), SqlType::Money), "0.01");
    assert_eq!(show(Value::Money(-50), SqlType::Money), "-0.01");
}

#[test]
fn display_float() {
    assert_eq!(show(Value::F64(0.1), SqlType::Float), "0.1");
    assert_eq!(
        show(Value::F64(1_234_567.0), SqlType::Float),
        "1.23457e+006"
    );
    assert_eq!(
        show(Value::F64(-0.000001234), SqlType::Float),
        "-1.234e-006"
    );
    assert_eq!(show(Value::F64(1000.0), SqlType::Float), "1000");
    assert_eq!(show(Value::F64(0.000001), SqlType::Float), "1e-006");
    assert_eq!(show(Value::F64(1e15), SqlType::Float), "1e+015");
    assert_eq!(show(Value::F32(1.5), SqlType::Real), "1.5");
    assert_eq!(show(Value::F64(0.0), SqlType::Float), "0");
}

#[test]
fn display_char_pads_and_varchar_does_not() {
    let ab = || {
        Value::String(SqlString {
            text: "ab".to_owned(),
        })
    };
    assert_eq!(show(ab(), SqlType::Char(Len::Fixed(5))), "ab   ");
    assert_eq!(show(ab(), SqlType::NChar(Len::Fixed(5))), "ab   ");
    assert_eq!(show(ab(), SqlType::VarChar(Len::Fixed(5))), "ab");
    assert_eq!(show(ab(), SqlType::NVarChar(Len::Fixed(5))), "ab");
    assert_eq!(show(ab(), SqlType::VarChar(Len::Max)), "ab");
    assert_eq!(show(ab(), SqlType::Char(Len::Fixed(2))), "ab");
}

#[test]
fn display_binary_is_hexadecimal() {
    let one = || Value::Bytes(vec![0x1f]);
    assert_eq!(show(one(), SqlType::VarBinary(Len::Fixed(4))), "0x1F");
    assert_eq!(show(one(), SqlType::Binary(Len::Fixed(4))), "0x1F000000");
    assert_eq!(show(one(), SqlType::VarBinary(Len::Max)), "0x1F");
    assert_eq!(
        show(
            Value::Bytes(vec![0x4e, 0x61, 0x6d, 0x65]),
            SqlType::VarBinary(Len::Fixed(4))
        ),
        "0x4E616D65"
    );
    assert_eq!(
        show(Value::Bytes(Vec::new()), SqlType::VarBinary(Len::Max)),
        "0x"
    );
}

#[test]
fn display_guid_is_upper_case() {
    // 0E984725-C51C-4BF4-9960-E1C80E27ABA0: the first three groups little-endian.
    let bytes = [
        0x25, 0x47, 0x98, 0x0e, 0x1c, 0xc5, 0xf4, 0x4b, 0x99, 0x60, 0xe1, 0xc8, 0x0e, 0x27, 0xab,
        0xa0,
    ];
    assert_eq!(
        show(Value::Guid(bytes), SqlType::UniqueIdentifier),
        "0E984725-C51C-4BF4-9960-E1C80E27ABA0"
    );
    assert_eq!(
        show(Value::Guid([0; 16]), SqlType::UniqueIdentifier),
        "00000000-0000-0000-0000-000000000000"
    );
}

#[test]
fn display_dates() {
    assert_eq!(
        show(Value::Date(Date { days: 730_119 }), SqlType::Date),
        "2000-01-01"
    );
    assert_eq!(
        show(Value::Date(Date { days: 0 }), SqlType::Date),
        "0001-01-01"
    );
    assert_eq!(
        show(Value::Date(Date { days: 3_652_058 }), SqlType::Date),
        "9999-12-31"
    );

    // 13:05:06.1234567 in 100 ns ticks since midnight.
    let ticks = 471_061_234_567;
    let time = Time { ticks_100ns: ticks };
    assert_eq!(
        show(Value::Time(time), SqlType::Time(7)),
        "13:05:06.1234567"
    );
    assert_eq!(show(Value::Time(time), SqlType::Time(0)), "13:05:06");
    assert_eq!(show(Value::Time(time), SqlType::Time(3)), "13:05:06.123");
    assert_eq!(
        show(Value::Time(Time { ticks_100ns: 0 }), SqlType::Time(7)),
        "00:00:00.0000000"
    );

    let stamp = DateTime2 {
        date: Date { days: 730_119 },
        time,
    };
    assert_eq!(
        show(Value::DateTime2(stamp), SqlType::DateTime2(7)),
        "2000-01-01 13:05:06.1234567"
    );
    assert_eq!(
        show(Value::DateTime2(stamp), SqlType::DateTime2(0)),
        "2000-01-01 13:05:06"
    );

    // Rendered in local time: the UTC instant plus the offset.
    assert_eq!(
        show(
            Value::DateTimeOffset(DateTimeOffset {
                utc: stamp,
                offset_minutes: 120
            }),
            SqlType::DateTimeOffset(7)
        ),
        "2000-01-01 15:05:06.1234567 +02:00"
    );
    assert_eq!(
        show(
            Value::DateTimeOffset(DateTimeOffset {
                utc: stamp,
                offset_minutes: -330
            }),
            SqlType::DateTimeOffset(7)
        ),
        "2000-01-01 07:35:06.1234567 -05:30"
    );
    // An offset that pushes the local time into the previous day.
    assert_eq!(
        show(
            Value::DateTimeOffset(DateTimeOffset {
                utc: DateTime2 {
                    date: Date { days: 730_119 },
                    time: Time { ticks_100ns: 0 }
                },
                offset_minutes: -60
            }),
            SqlType::DateTimeOffset(0)
        ),
        "1999-12-31 23:00:00 -01:00"
    );
}

#[test]
fn display_datetime_style_zero() {
    let noon_minus = DateTime {
        days: days_from_civil(2000, 1, 1) - DAYS_1900,
        // 13:05:00 in 1/300 s ticks.
        ticks_300th: (13 * 3600 + 5 * 60) * 300,
    };
    assert_eq!(
        show(Value::DateTime(noon_minus), SqlType::DateTime),
        "Jan  1 2000  1:05PM"
    );

    let midnight = DateTime {
        days: days_from_civil(2000, 1, 1) - DAYS_1900,
        ticks_300th: 0,
    };
    assert_eq!(
        show(Value::DateTime(midnight), SqlType::DateTime),
        "Jan  1 2000 12:00AM"
    );
    // `smalldatetime` uses the same style 0.
    assert_eq!(
        show(Value::DateTime(midnight), SqlType::SmallDateTime),
        "Jan  1 2000 12:00AM"
    );

    let christmas = DateTime {
        days: days_from_civil(2000, 12, 25) - DAYS_1900,
        ticks_300th: (23 * 3600 + 59 * 60) * 300,
    };
    assert_eq!(
        show(Value::DateTime(christmas), SqlType::DateTime),
        "Dec 25 2000 11:59PM"
    );

    let noon = DateTime {
        days: days_from_civil(2000, 6, 30) - DAYS_1900,
        ticks_300th: 12 * 3600 * 300,
    };
    assert_eq!(
        show(Value::DateTime(noon), SqlType::DateTime),
        "Jun 30 2000 12:00PM"
    );
}
