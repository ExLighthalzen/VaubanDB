//! TYPE_INFO and value encoding/decoding ([MS-TDS] 2.2.5.4, 2.2.5.5).
//!
//! [MS-TDS] 2.2.5.4 Data Type Definitions sorts the types by the way their length travels:
//!
//! - fixed-length ([MS-TDS] 2.2.5.4.1): the TYPE_INFO is the type token alone, the value has
//!   no length prefix and can never be NULL;
//! - BYTELEN_TYPE ([MS-TDS] 2.2.5.4.2): the TYPE_INFO carries one byte of maximum length (or a
//!   precision and a scale, or a scale alone), every value starts with its actual length on one
//!   byte, and a length of `0` is NULL;
//! - USHORTLEN_TYPE ([MS-TDS] 2.2.5.4.2): `char(n)`, `varchar(n)`, `nchar(n)`, `nvarchar(n)`,
//!   `binary(n)`, `varbinary(n)`; the TYPE_INFO carries the maximum length on two bytes and,
//!   for the character types, a 5-byte collation; values start with their length on two
//!   bytes, `0xFFFF` is NULL;
//! - PARTLENTYPE ([MS-TDS] 2.2.5.4.3, Partially Length-Prefixed): `varchar(max)`,
//!   `nvarchar(max)`, `varbinary(max)`; announced by a maximum length of `0xFFFF`, values
//!   travel as a total length then chunks.
//!
//! [`encode_type_info`] and [`encode_value`] dispatch on the `SqlType`; [`bytelen`] handles the
//! first two families (every type that is neither a string nor a binary), [`ushortlen`] and
//! [`plp`] the last two, with [`collation`] (the `Collation` rule) and [`cp1252`] (the code
//! page of `char` / `varchar`) as shared bricks. [`decode`] is the inverse direction, for the
//! TYPE_INFO and values a client sends in RPC parameters.

mod bytelen;
mod collation;
mod cp1252;
mod decode;
mod plp;
mod ushortlen;

use bytes::BytesMut;
use vauban_types::{Len, SqlType, TypeInfo, Value};

pub(crate) use decode::{decode_type_info, decode_value};

use crate::error::TdsError;

// ---- Fixed-length type tokens ([MS-TDS] 2.2.5.4.1 Fixed-Length Data Types) ----

/// [MS-TDS] 2.2.5.4.1 NULLTYPE: untyped NULL, only in parameters sent by a client.
pub(crate) const NULLTYPE: u8 = 0x1F;
/// [MS-TDS] 2.2.5.4.1 INT1TYPE (`tinyint`, 1 byte).
pub(crate) const INT1TYPE: u8 = 0x30;
/// [MS-TDS] 2.2.5.4.1 BITTYPE (`bit`, 1 byte).
pub(crate) const BITTYPE: u8 = 0x32;
/// [MS-TDS] 2.2.5.4.1 INT2TYPE (`smallint`, 2 bytes).
pub(crate) const INT2TYPE: u8 = 0x34;
/// [MS-TDS] 2.2.5.4.1 INT4TYPE (`int`, 4 bytes).
pub(crate) const INT4TYPE: u8 = 0x38;
/// [MS-TDS] 2.2.5.4.1 DATETIM4TYPE (`smalldatetime`, 4 bytes).
pub(crate) const DATETIM4TYPE: u8 = 0x3A;
/// [MS-TDS] 2.2.5.4.1 FLT4TYPE (`real`, 4 bytes).
pub(crate) const FLT4TYPE: u8 = 0x3B;
/// [MS-TDS] 2.2.5.4.1 MONEYTYPE (`money`, 8 bytes).
pub(crate) const MONEYTYPE: u8 = 0x3C;
/// [MS-TDS] 2.2.5.4.1 DATETIMETYPE (`datetime`, 8 bytes).
pub(crate) const DATETIMETYPE: u8 = 0x3D;
/// [MS-TDS] 2.2.5.4.1 FLT8TYPE (`float`, 8 bytes).
pub(crate) const FLT8TYPE: u8 = 0x3E;
/// [MS-TDS] 2.2.5.4.1 MONEY4TYPE (`smallmoney`, 4 bytes).
pub(crate) const MONEY4TYPE: u8 = 0x7A;
/// [MS-TDS] 2.2.5.4.1 INT8TYPE (`bigint`, 8 bytes).
pub(crate) const INT8TYPE: u8 = 0x7F;

// ---- BYTELEN type tokens ([MS-TDS] 2.2.5.4.2 Variable-Length Data Types) ----

/// [MS-TDS] 2.2.5.4.2 GUIDTYPE (`uniqueidentifier`, length 16).
pub(crate) const GUIDTYPE: u8 = 0x24;
/// [MS-TDS] 2.2.5.4.2 INTNTYPE (nullable integers, length 1, 2, 4 or 8).
pub(crate) const INTNTYPE: u8 = 0x26;
/// [MS-TDS] 2.2.5.4.2 DECIMALNTYPE (`decimal`; TYPE_INFO carries length, precision, scale).
pub(crate) const DECIMALNTYPE: u8 = 0x6A;
/// [MS-TDS] 2.2.5.4.2 NUMERICNTYPE (`numeric`): same layout as DECIMALNTYPE. The two tokens
/// carry the two distinct type names a client reads in the metadata, so `SqlType::Decimal`
/// travels as DECIMALNTYPE and `SqlType::Numeric` as NUMERICNTYPE, both ways.
pub(crate) const NUMERICNTYPE: u8 = 0x6C;
/// [MS-TDS] 2.2.5.4.2 BITNTYPE (nullable `bit`, length 1).
pub(crate) const BITNTYPE: u8 = 0x68;
/// [MS-TDS] 2.2.5.4.2 FLTNTYPE (nullable `real` / `float`, length 4 or 8).
pub(crate) const FLTNTYPE: u8 = 0x6D;
/// [MS-TDS] 2.2.5.4.2 MONEYNTYPE (nullable `smallmoney` / `money`, length 4 or 8).
pub(crate) const MONEYNTYPE: u8 = 0x6E;
/// [MS-TDS] 2.2.5.4.2 DATETIMNTYPE (nullable `smalldatetime` / `datetime`, length 4 or 8).
pub(crate) const DATETIMNTYPE: u8 = 0x6F;
/// [MS-TDS] 2.2.5.4.2 DATENTYPE (`date`; no length byte in TYPE_INFO, values are BYTELEN).
pub(crate) const DATENTYPE: u8 = 0x28;
/// [MS-TDS] 2.2.5.4.2 TIMENTYPE (`time(s)`; TYPE_INFO carries the scale).
pub(crate) const TIMENTYPE: u8 = 0x29;
/// [MS-TDS] 2.2.5.4.2 DATETIME2NTYPE (`datetime2(s)`; TYPE_INFO carries the scale).
pub(crate) const DATETIME2NTYPE: u8 = 0x2A;
/// [MS-TDS] 2.2.5.4.2 DATETIMEOFFSETNTYPE (`datetimeoffset(s)`; TYPE_INFO carries the scale).
pub(crate) const DATETIMEOFFSETNTYPE: u8 = 0x2B;

// ---- USHORTLEN type tokens ([MS-TDS] 2.2.5.4.2), also used with a length of 0xFFFF for
// the PLP forms ([MS-TDS] 2.2.5.4.3) ----

/// [MS-TDS] 2.2.5.4.2 BIGVARBINTYPE (`varbinary(n)`, and `varbinary(max)` as PLP).
pub(crate) const BIGVARBINTYPE: u8 = 0xA5;
/// [MS-TDS] 2.2.5.4.2 BIGVARCHRTYPE (`varchar(n)`, and `varchar(max)` as PLP).
pub(crate) const BIGVARCHRTYPE: u8 = 0xA7;
/// [MS-TDS] 2.2.5.4.2 BIGBINARYTYPE (`binary(n)`).
pub(crate) const BIGBINARYTYPE: u8 = 0xAD;
/// [MS-TDS] 2.2.5.4.2 BIGCHARTYPE (`char(n)`).
pub(crate) const BIGCHARTYPE: u8 = 0xAF;
/// [MS-TDS] 2.2.5.4.2 NVARCHARTYPE (`nvarchar(n)`, and `nvarchar(max)` as PLP).
pub(crate) const NVARCHARTYPE: u8 = 0xE7;
/// [MS-TDS] 2.2.5.4.2 NCHARTYPE (`nchar(n)`).
pub(crate) const NCHARTYPE: u8 = 0xEF;

/// Appends the TYPE_INFO ([MS-TDS] 2.2.5.6 Type Info Rule Definition) of `ti` to `out`.
///
/// Every `SqlType` is covered. The fixed-or-N choice for the types that have two forms is
/// documented in [`bytelen`]; the collation of the character types comes from
/// `ti.collation`, `Collation::DEFAULT` when it is `None` ([`ushortlen`], [`plp`]). A
/// declared length outside the SQL Server bounds, or `char(max)` / `nchar(max)` /
/// `binary(max)` (types that do not exist), fail with `Malformed`.
pub(crate) fn encode_type_info(ti: &TypeInfo, out: &mut BytesMut) -> Result<(), TdsError> {
    match family(ti.ty) {
        Family::ByteLen => bytelen::encode_type_info(ti, out),
        Family::UShortLen => ushortlen::encode_type_info(ti, out),
        Family::Plp => plp::encode_type_info(ti, out),
    }
}

/// Appends `value` to `out` in the layout that the TYPE_INFO of `ti` announces
/// ([MS-TDS] 2.2.5.5 Data Type Dependent Data Streams).
///
/// `Value::Null` with `ti.nullable == false` fails with `NullInNotNullable`; a value whose
/// variant or magnitude does not match `ti` fails with `ValueTypeMismatch` (no conversion is
/// attempted). Nothing is written when an error is returned.
pub(crate) fn encode_value(
    ti: &TypeInfo,
    value: &Value,
    out: &mut BytesMut,
) -> Result<(), TdsError> {
    match family(ti.ty) {
        Family::ByteLen => bytelen::encode_value(ti, value, out),
        Family::UShortLen => ushortlen::encode_value(ti, value, out),
        Family::Plp => plp::encode_value(ti, value, out),
    }
}

/// The length family of a type, which decides the file that encodes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Fixed-length or BYTELEN_TYPE: `bytelen.rs`.
    ByteLen,
    /// USHORTLEN_TYPE: `char(n)`, `varchar(n)`, `nchar(n)`, `nvarchar(n)`, `binary(n)`,
    /// `varbinary(n)`. Also where `Char(Len::Max)`, `NChar(Len::Max)` and
    /// `Binary(Len::Max)` land: SQL Server has no such types, `ushortlen` refuses them with
    /// `Malformed` rather than inventing a PLP form for them.
    UShortLen,
    /// PARTLENTYPE (Partially Length-Prefixed): `varchar(max)`, `nvarchar(max)`,
    /// `varbinary(max)`.
    Plp,
}

/// Routes `ty` to its family. Only the three `(max)` types that exist in SQL Server are PLP.
fn family(ty: SqlType) -> Family {
    match ty {
        SqlType::VarChar(Len::Max) | SqlType::NVarChar(Len::Max) | SqlType::VarBinary(Len::Max) => {
            Family::Plp
        }
        SqlType::Char(_)
        | SqlType::VarChar(_)
        | SqlType::NChar(_)
        | SqlType::NVarChar(_)
        | SqlType::Binary(_)
        | SqlType::VarBinary(_) => Family::UShortLen,
        SqlType::Bit
        | SqlType::TinyInt
        | SqlType::SmallInt
        | SqlType::Int
        | SqlType::BigInt
        | SqlType::Decimal { .. }
        | SqlType::Numeric { .. }
        | SqlType::Float
        | SqlType::Real
        | SqlType::Money
        | SqlType::SmallMoney
        | SqlType::Date
        | SqlType::Time(_)
        | SqlType::DateTime
        | SqlType::SmallDateTime
        | SqlType::DateTime2(_)
        | SqlType::DateTimeOffset(_)
        | SqlType::UniqueIdentifier => Family::ByteLen,
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use vauban_types::{Len, SqlType, TypeInfo};

    use super::{Family, encode_type_info, family};

    /// One instance of **every** variant of `SqlType`.
    fn one_of_each() -> Vec<SqlType> {
        vec![
            SqlType::Bit,
            SqlType::TinyInt,
            SqlType::SmallInt,
            SqlType::Int,
            SqlType::BigInt,
            SqlType::Decimal {
                precision: 18,
                scale: 2,
            },
            SqlType::Numeric {
                precision: 18,
                scale: 2,
            },
            SqlType::Float,
            SqlType::Real,
            SqlType::Money,
            SqlType::SmallMoney,
            SqlType::Char(Len::Fixed(3)),
            SqlType::VarChar(Len::Fixed(10)),
            SqlType::NChar(Len::Fixed(3)),
            SqlType::NVarChar(Len::Fixed(10)),
            SqlType::Binary(Len::Fixed(4)),
            SqlType::VarBinary(Len::Fixed(8)),
            SqlType::VarChar(Len::Max),
            SqlType::NVarChar(Len::Max),
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
    fn all_sql_types_have_type_info() {
        for ty in one_of_each() {
            for nullable in [false, true] {
                let ti = TypeInfo::new(ty, nullable);
                let mut out = BytesMut::new();
                let result = encode_type_info(&ti, &mut out);
                assert!(result.is_ok(), "{ty:?} nullable={nullable}: {result:?}");
                assert!(!out.is_empty(), "{ty:?}: empty TYPE_INFO");
            }
        }
    }

    #[test]
    fn family_routing() {
        for ty in [
            SqlType::VarChar(Len::Max),
            SqlType::NVarChar(Len::Max),
            SqlType::VarBinary(Len::Max),
        ] {
            assert_eq!(family(ty), Family::Plp, "{ty:?}");
        }
        for ty in [
            SqlType::Char(Len::Fixed(1)),
            SqlType::VarChar(Len::Fixed(1)),
            SqlType::NChar(Len::Fixed(1)),
            SqlType::NVarChar(Len::Fixed(1)),
            SqlType::Binary(Len::Fixed(1)),
            SqlType::VarBinary(Len::Fixed(1)),
            // No `(max)` form in SQL Server: refused by `ushortlen`, never PLP.
            SqlType::Char(Len::Max),
            SqlType::NChar(Len::Max),
            SqlType::Binary(Len::Max),
        ] {
            assert_eq!(family(ty), Family::UShortLen, "{ty:?}");
        }
        for ty in one_of_each() {
            if !ty.is_string() && !matches!(ty, SqlType::Binary(_) | SqlType::VarBinary(_)) {
                assert_eq!(family(ty), Family::ByteLen, "{ty:?}");
            }
        }
    }
}
