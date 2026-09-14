//! Scalar data types and the type of a column or expression.

use crate::collation::Collation;

/// Declared length of a character or binary type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Len {
    /// SQL Server `(n)`: a fixed maximum, in characters or bytes.
    Fixed(u16),
    /// SQL Server `(max)`: up to 2^31 - 1 bytes.
    Max,
}

/// A SQL Server scalar data type, with its declared parameters.
///
/// `sql_variant` and `xml` are not represented yet (reserved for later versions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlType {
    /// SQL Server `bit`.
    Bit,
    /// SQL Server `tinyint` (unsigned, `0..=255`).
    TinyInt,
    /// SQL Server `smallint`.
    SmallInt,
    /// SQL Server `int`.
    Int,
    /// SQL Server `bigint`.
    BigInt,
    /// SQL Server `decimal(p, s)`.
    Decimal {
        /// Total number of digits, `1..=38`.
        precision: u8,
        /// Number of digits to the right of the decimal point, `0..=precision`.
        scale: u8,
    },
    /// SQL Server `numeric(p, s)`.
    ///
    /// `numeric` and `decimal` are functionally equivalent, but they keep distinct names in
    /// the metadata a client reads,
    /// so they are two variants that share one representation ([`crate::Value::Decimal`]).
    Numeric {
        /// Total number of digits, `1..=38`.
        precision: u8,
        /// Number of digits to the right of the decimal point, `0..=precision`.
        scale: u8,
    },
    /// SQL Server `float` (`float(53)`, 8 bytes).
    Float,
    /// SQL Server `real` (`float(24)`, 4 bytes).
    Real,
    /// SQL Server `money`.
    Money,
    /// SQL Server `smallmoney`.
    SmallMoney,
    /// SQL Server `char(n)`.
    Char(Len),
    /// SQL Server `varchar(n)` / `varchar(max)`.
    VarChar(Len),
    /// SQL Server `nchar(n)`.
    NChar(Len),
    /// SQL Server `nvarchar(n)` / `nvarchar(max)`.
    NVarChar(Len),
    /// SQL Server `binary(n)`.
    Binary(Len),
    /// SQL Server `varbinary(n)` / `varbinary(max)`.
    VarBinary(Len),
    /// SQL Server `date`.
    Date,
    /// SQL Server `time(s)`; the payload is the fractional-seconds scale, `0..=7`.
    Time(u8),
    /// SQL Server `datetime`.
    DateTime,
    /// SQL Server `smalldatetime`.
    SmallDateTime,
    /// SQL Server `datetime2(s)`; the payload is the fractional-seconds scale, `0..=7`.
    DateTime2(u8),
    /// SQL Server `datetimeoffset(s)`; the payload is the fractional-seconds scale, `0..=7`.
    DateTimeOffset(u8),
    /// SQL Server `uniqueidentifier`.
    UniqueIdentifier,
}

/// The broad family a [`SqlType`] belongs to.
///
/// The families group the types that share one set of rules: conversion target, implicit
/// precedence, arithmetic. They follow the documented categories of the data types, with
/// two deliberate departures: `bit` is alone (it is neither an
/// integer for `CAST` nor a numeric for arithmetic), and every date or time type is in a
/// single `DateTime` family (they convert to one another under the same rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeFamily {
    /// `bit`.
    Bit,
    /// `tinyint`, `smallint`, `int`, `bigint`.
    Integer,
    /// `decimal(p, s)` and `numeric(p, s)`.
    ExactNumeric,
    /// `float` and `real`.
    ApproxNumeric,
    /// `money` and `smallmoney`.
    Money,
    /// `char`, `varchar`, `nchar`, `nvarchar`.
    Character,
    /// `binary` and `varbinary`.
    Binary,
    /// `date`, `time`, `datetime`, `smalldatetime`, `datetime2`, `datetimeoffset`.
    DateTime,
    /// `uniqueidentifier`.
    Guid,
}

impl SqlType {
    /// Returns `true` for the character types that carry a collation: `char`, `varchar`,
    /// `nchar` and `nvarchar`. Binary types are not strings.
    pub fn is_string(&self) -> bool {
        matches!(
            self,
            SqlType::Char(_) | SqlType::VarChar(_) | SqlType::NChar(_) | SqlType::NVarChar(_)
        )
    }

    /// Returns `true` for the two types that carry a declared precision and a declared
    /// scale: `decimal(p, s)` and `numeric(p, s)`.
    ///
    /// They are functionally equivalent and share [`crate::Value::Decimal`] as their
    /// representation; their name, and the TDS token that carries them, are what differ.
    pub fn is_exact_numeric(&self) -> bool {
        matches!(self, SqlType::Decimal { .. } | SqlType::Numeric { .. })
    }

    /// The SQL name of the type, lower-cased and without its parameters, as SQL Server
    /// writes it in `sys.types` and in the metadata a client reads.
    ///
    /// Two distinct variants never share a name, so the name identifies the variant; the
    /// parameters are added by [`SqlType::declaration`]. For the name SQL Server prints
    /// *in an error message*, use [`SqlType::error_name`], which differs for `decimal`.
    pub fn name(&self) -> &'static str {
        match self {
            SqlType::Bit => "bit",
            SqlType::TinyInt => "tinyint",
            SqlType::SmallInt => "smallint",
            SqlType::Int => "int",
            SqlType::BigInt => "bigint",
            SqlType::Decimal { .. } => "decimal",
            SqlType::Numeric { .. } => "numeric",
            SqlType::Float => "float",
            SqlType::Real => "real",
            SqlType::Money => "money",
            SqlType::SmallMoney => "smallmoney",
            SqlType::Char(_) => "char",
            SqlType::VarChar(_) => "varchar",
            SqlType::NChar(_) => "nchar",
            SqlType::NVarChar(_) => "nvarchar",
            SqlType::Binary(_) => "binary",
            SqlType::VarBinary(_) => "varbinary",
            SqlType::Date => "date",
            SqlType::Time(_) => "time",
            SqlType::DateTime => "datetime",
            SqlType::SmallDateTime => "smalldatetime",
            SqlType::DateTime2(_) => "datetime2",
            SqlType::DateTimeOffset(_) => "datetimeoffset",
            SqlType::UniqueIdentifier => "uniqueidentifier",
        }
    }

    /// The name SQL Server uses **inside its error messages**.
    ///
    /// Identical to [`SqlType::name`] except for `decimal(p, s)`, which SQL Server names
    /// `numeric` in messages 220, 232, 242, 245, 8114 and 8115: `CAST(1234 AS decimal(3,0))`
    /// reports `Converting int to data type numeric overflowed.`
    ///
    /// This is the only naming function `crate::errors` uses; everywhere else (catalogue,
    /// `sys.types`, binder) [`SqlType::name`] is the source of truth.
    pub fn error_name(&self) -> &'static str {
        match self {
            SqlType::Decimal { .. } => "numeric",
            other => other.name(),
        }
    }

    /// The type with its parameters, spelled as in a `CREATE TABLE` or a `CAST`:
    /// `int`, `decimal(5,2)`, `varchar(10)`, `varchar(max)`, `time(7)`, `datetime2(3)`.
    ///
    /// No space follows the comma, which is the form SQL Server's own tools print. This
    /// is a declaration, not a message: an error names a type by [`SqlType::error_name`].
    pub fn declaration(&self) -> String {
        match self {
            SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
                format!("{}({precision},{scale})", self.name())
            }
            SqlType::Char(len)
            | SqlType::VarChar(len)
            | SqlType::NChar(len)
            | SqlType::NVarChar(len)
            | SqlType::Binary(len)
            | SqlType::VarBinary(len) => match len {
                Len::Fixed(n) => format!("{}({n})", self.name()),
                Len::Max => format!("{}(max)", self.name()),
            },
            SqlType::Time(scale) | SqlType::DateTime2(scale) | SqlType::DateTimeOffset(scale) => {
                format!("{}({scale})", self.name())
            }
            other => other.name().to_owned(),
        }
    }

    /// The [`TypeFamily`] this type belongs to.
    ///
    /// `convert` dispatches on the family of its target, and the arithmetic and precedence
    /// rules are stated per family rather than per type.
    pub fn family(&self) -> TypeFamily {
        match self {
            SqlType::Bit => TypeFamily::Bit,
            SqlType::TinyInt | SqlType::SmallInt | SqlType::Int | SqlType::BigInt => {
                TypeFamily::Integer
            }
            SqlType::Decimal { .. } | SqlType::Numeric { .. } => TypeFamily::ExactNumeric,
            SqlType::Float | SqlType::Real => TypeFamily::ApproxNumeric,
            SqlType::Money | SqlType::SmallMoney => TypeFamily::Money,
            SqlType::Char(_) | SqlType::VarChar(_) | SqlType::NChar(_) | SqlType::NVarChar(_) => {
                TypeFamily::Character
            }
            SqlType::Binary(_) | SqlType::VarBinary(_) => TypeFamily::Binary,
            SqlType::Date
            | SqlType::Time(_)
            | SqlType::DateTime
            | SqlType::SmallDateTime
            | SqlType::DateTime2(_)
            | SqlType::DateTimeOffset(_) => TypeFamily::DateTime,
            SqlType::UniqueIdentifier => TypeFamily::Guid,
        }
    }
}

/// What a column or an expression "is": a type, its nullability and, for character
/// types, its collation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeInfo {
    /// The scalar data type.
    pub ty: SqlType,
    /// Whether the column or expression may hold `NULL`.
    pub nullable: bool,
    /// The collation, `Some` for character types only.
    pub collation: Option<Collation>,
}

impl TypeInfo {
    /// Builds a `TypeInfo` with the default collation when `ty` is a character type
    /// ([`SqlType::is_string`]) and no collation otherwise.
    pub fn new(ty: SqlType, nullable: bool) -> Self {
        let collation = if ty.is_string() {
            Some(Collation::DEFAULT)
        } else {
            None
        };
        TypeInfo {
            ty,
            nullable,
            collation,
        }
    }
}

#[cfg(test)]
mod tests {
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
}
