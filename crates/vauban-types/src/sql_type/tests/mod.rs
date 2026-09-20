use super::{Len, SqlType, TypeFamily, TypeInfo};
use crate::collation::Collation;

/// One instance of each of the 24 variants of [`SqlType`], in declaration order.
///
/// Adding a variant to `SqlType` must break the length assertions below: the tests of
/// this module are exhaustive by construction, not by `match`.
fn all_types() -> Vec<SqlType> {
    vec![
        SqlType::Bit,
        SqlType::TinyInt,
        SqlType::SmallInt,
        SqlType::Int,
        SqlType::BigInt,
        SqlType::Decimal {
            precision: 5,
            scale: 2,
        },
        SqlType::Numeric {
            precision: 5,
            scale: 2,
        },
        SqlType::Float,
        SqlType::Real,
        SqlType::Money,
        SqlType::SmallMoney,
        SqlType::Char(Len::Fixed(1)),
        SqlType::VarChar(Len::Max),
        SqlType::NChar(Len::Fixed(1)),
        SqlType::NVarChar(Len::Max),
        SqlType::Binary(Len::Fixed(1)),
        SqlType::VarBinary(Len::Max),
        SqlType::Date,
        SqlType::Time(7),
        SqlType::DateTime,
        SqlType::SmallDateTime,
        SqlType::DateTime2(7),
        SqlType::DateTimeOffset(7),
        SqlType::UniqueIdentifier,
    ]
}

#[test]
fn type_info_new_sets_collation_for_strings() {
    let s = TypeInfo::new(SqlType::NVarChar(Len::Fixed(10)), true);
    assert_eq!(s.ty, SqlType::NVarChar(Len::Fixed(10)));
    assert!(s.nullable);
    assert_eq!(s.collation, Some(Collation::DEFAULT));

    let i = TypeInfo::new(SqlType::Int, false);
    assert_eq!(i.ty, SqlType::Int);
    assert!(!i.nullable);
    assert_eq!(i.collation, None);
}

#[test]
fn is_string_only_for_character_types() {
    assert!(SqlType::Char(Len::Fixed(1)).is_string());
    assert!(SqlType::VarChar(Len::Max).is_string());
    assert!(SqlType::NChar(Len::Fixed(1)).is_string());
    assert!(SqlType::NVarChar(Len::Max).is_string());

    assert!(!SqlType::Binary(Len::Fixed(1)).is_string());
    assert!(!SqlType::VarBinary(Len::Max).is_string());
    assert!(!SqlType::Int.is_string());
    assert!(!SqlType::UniqueIdentifier.is_string());
    assert!(
        !SqlType::Decimal {
            precision: 18,
            scale: 2
        }
        .is_string()
    );
    assert!(
        !SqlType::Numeric {
            precision: 18,
            scale: 2
        }
        .is_string()
    );
}

#[test]
fn is_exact_numeric_only_for_decimal_and_numeric() {
    assert!(
        SqlType::Decimal {
            precision: 18,
            scale: 2
        }
        .is_exact_numeric()
    );
    assert!(
        SqlType::Numeric {
            precision: 2,
            scale: 1
        }
        .is_exact_numeric()
    );

    assert!(!SqlType::Int.is_exact_numeric());
    assert!(!SqlType::Money.is_exact_numeric());
    assert!(!SqlType::Float.is_exact_numeric());
    assert!(!SqlType::VarChar(Len::Max).is_exact_numeric());
}

/// `decimal(p, s)` and `numeric(p, s)` are two distinct types, whatever their
/// parameters: a client reads two different names in the metadata.
#[test]
fn decimal_and_numeric_are_distinct_types() {
    let d = SqlType::Decimal {
        precision: 2,
        scale: 1,
    };
    let n = SqlType::Numeric {
        precision: 2,
        scale: 1,
    };
    assert_ne!(d, n);
    assert_eq!(TypeInfo::new(n, false).collation, None);
}

#[test]
fn type_names_match_sql_server() {
    assert_eq!(
        SqlType::Decimal {
            precision: 5,
            scale: 2
        }
        .name(),
        "decimal"
    );
    assert_eq!(
        SqlType::Numeric {
            precision: 5,
            scale: 2
        }
        .name(),
        "numeric"
    );
    assert_eq!(SqlType::VarChar(Len::Max).name(), "varchar");
    assert_eq!(SqlType::SmallDateTime.name(), "smalldatetime");
    assert_eq!(SqlType::UniqueIdentifier.name(), "uniqueidentifier");

    let types = all_types();
    assert_eq!(types.len(), 24);
    let mut names: Vec<&str> = types.iter().map(SqlType::name).collect();
    assert!(names.iter().all(|n| !n.is_empty()));
    names.sort_unstable();
    let unique = names.len();
    names.dedup();
    assert_eq!(names.len(), unique, "two variants share one name");
}

#[test]
fn declarations_include_parameters() {
    assert_eq!(
        SqlType::Decimal {
            precision: 5,
            scale: 2
        }
        .declaration(),
        "decimal(5,2)"
    );
    assert_eq!(
        SqlType::VarChar(Len::Fixed(10)).declaration(),
        "varchar(10)"
    );
    assert_eq!(SqlType::VarChar(Len::Max).declaration(), "varchar(max)");
    assert_eq!(SqlType::Time(7).declaration(), "time(7)");
    assert_eq!(SqlType::Int.declaration(), "int");
    assert_eq!(SqlType::Money.declaration(), "money");
    // The remaining parameterised variants, for the record.
    assert_eq!(
        SqlType::Numeric {
            precision: 38,
            scale: 0
        }
        .declaration(),
        "numeric(38,0)"
    );
    assert_eq!(SqlType::NChar(Len::Fixed(3)).declaration(), "nchar(3)");
    assert_eq!(SqlType::VarBinary(Len::Max).declaration(), "varbinary(max)");
    assert_eq!(SqlType::DateTime2(3).declaration(), "datetime2(3)");
    assert_eq!(
        SqlType::DateTimeOffset(0).declaration(),
        "datetimeoffset(0)"
    );
}

/// SQL Server never writes `decimal` in an error message: `CAST(1234 AS decimal(3,0))`
/// reports `... converting int to data type numeric.` Every other type keeps its name.
#[test]
fn error_names_say_numeric_for_decimal() {
    let d = SqlType::Decimal {
        precision: 5,
        scale: 2,
    };
    assert_eq!(d.name(), "decimal");
    assert_eq!(d.error_name(), "numeric");

    let others: Vec<SqlType> = all_types().into_iter().filter(|t| *t != d).collect();
    assert_eq!(others.len(), 23);
    for t in others {
        assert_eq!(t.error_name(), t.name(), "{t:?}");
    }
}

#[test]
fn families_partition_all_types() {
    assert_eq!(SqlType::Bit.family(), TypeFamily::Bit);
    for t in [
        SqlType::TinyInt,
        SqlType::SmallInt,
        SqlType::Int,
        SqlType::BigInt,
    ] {
        assert_eq!(t.family(), TypeFamily::Integer, "{t:?}");
    }
    for t in [
        SqlType::Decimal {
            precision: 5,
            scale: 2,
        },
        SqlType::Numeric {
            precision: 5,
            scale: 2,
        },
    ] {
        assert_eq!(t.family(), TypeFamily::ExactNumeric, "{t:?}");
    }
    for t in [SqlType::Float, SqlType::Real] {
        assert_eq!(t.family(), TypeFamily::ApproxNumeric, "{t:?}");
    }
    for t in [SqlType::Money, SqlType::SmallMoney] {
        assert_eq!(t.family(), TypeFamily::Money, "{t:?}");
    }
    for t in [
        SqlType::Char(Len::Fixed(1)),
        SqlType::VarChar(Len::Max),
        SqlType::NChar(Len::Fixed(1)),
        SqlType::NVarChar(Len::Max),
    ] {
        assert_eq!(t.family(), TypeFamily::Character, "{t:?}");
    }
    for t in [SqlType::Binary(Len::Fixed(1)), SqlType::VarBinary(Len::Max)] {
        assert_eq!(t.family(), TypeFamily::Binary, "{t:?}");
    }
    for t in [
        SqlType::Date,
        SqlType::Time(7),
        SqlType::DateTime,
        SqlType::SmallDateTime,
        SqlType::DateTime2(7),
        SqlType::DateTimeOffset(7),
    ] {
        assert_eq!(t.family(), TypeFamily::DateTime, "{t:?}");
    }
    assert_eq!(SqlType::UniqueIdentifier.family(), TypeFamily::Guid);

    // The two predicates `is_string` and `is_exact_numeric` are exactly two families.
    for t in all_types() {
        assert_eq!(t.is_string(), t.family() == TypeFamily::Character, "{t:?}");
        assert_eq!(
            t.is_exact_numeric(),
            t.family() == TypeFamily::ExactNumeric,
            "{t:?}"
        );
    }
}
