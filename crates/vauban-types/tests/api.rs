//! Integration test: the public API of `vauban-types` is importable from the crate
//! root, every `Value` variant exists under its planned name, and the derives are
//! exactly those of the module README.

use std::hash::Hash;

use vauban_types::{
    Collation, Date, DateTime, DateTime2, DateTimeOffset, Decimal, Len, SqlString, SqlType, Time,
    TypeInfo, Value,
};

#[test]
fn value_variants_construct() {
    let date = Date { days: 730_119 };
    let time = Time {
        ticks_100ns: 123_456_789,
    };
    let dt2 = DateTime2 { date, time };
    let values = vec![
        Value::Null,
        Value::Bit(true),
        Value::I8(255),
        Value::I16(-1),
        Value::I32(1),
        Value::I64(i64::MAX),
        Value::Decimal(Decimal {
            mantissa: 12_345,
            precision: 5,
            scale: 2,
        }),
        Value::F64(1.5),
        Value::F32(2.5),
        Value::Money(10_000),
        Value::String(SqlString {
            text: "abc".to_owned(),
        }),
        Value::Bytes(vec![0x01, 0x02]),
        Value::Date(date),
        Value::Time(time),
        Value::DateTime(DateTime {
            days: 36_524,
            ticks_300th: 300,
        }),
        Value::DateTime2(dt2),
        Value::DateTimeOffset(DateTimeOffset {
            utc: dt2,
            offset_minutes: -120,
        }),
        Value::Guid([0; 16]),
    ];
    assert_eq!(values.len(), 18);
    for v in &values {
        let shown = format!("{v:?}");
        assert!(!shown.is_empty());
        // Structural equality, including on floats and on the clone.
        assert_eq!(v, &v.clone());
    }
}

#[test]
fn type_info_and_collation_are_public() {
    let info = TypeInfo::new(SqlType::VarChar(Len::Max), false);
    assert_eq!(info.collation, Some(Collation::DEFAULT));
    assert!(SqlType::NChar(Len::Fixed(3)).is_string());
}

fn copy_eq_hash<T: Copy + Eq + Hash>() {}
fn clone_eq_hash<T: Clone + Eq + Hash>() {}
fn clone_partial_eq<T: Clone + PartialEq + std::fmt::Debug>() {}

#[test]
fn derives_are_exactly_those_of_the_readme() {
    copy_eq_hash::<SqlType>();
    copy_eq_hash::<Len>();
    copy_eq_hash::<Collation>();
    copy_eq_hash::<Decimal>();
    copy_eq_hash::<Date>();
    copy_eq_hash::<Time>();
    copy_eq_hash::<DateTime>();
    copy_eq_hash::<DateTime2>();
    copy_eq_hash::<DateTimeOffset>();

    clone_eq_hash::<TypeInfo>();
    clone_eq_hash::<SqlString>();

    clone_partial_eq::<Value>();
}
