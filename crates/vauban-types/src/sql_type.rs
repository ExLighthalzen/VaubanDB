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
mod tests;
