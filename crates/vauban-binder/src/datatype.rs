//! Data types: `parser::DataType` to `types::SqlType`.
//!
//! The parser does not know types: it hands over a name as written and a list of
//! arguments (`parser::DataType`). This module is where a name becomes a
//! [`SqlType`]: it lower-cases the name, resolves the ISO synonyms, applies the
//! **declaration** defaults of each parameter and checks their bounds. `CAST`,
//! `CONVERT`, `DECLARE` and the DDL go through [`resolve_data_type`].

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{DataType, TypeArg};
use vauban_types::{Len, SqlType, TypeInfo};

/// Longest non-`max` `char`, `varchar`, `binary` or `varbinary`, in characters or
/// bytes.
const MAX_BYTE_LEN: i64 = 8_000;

/// Longest non-`max` `nchar` or `nvarchar`, in characters.
const MAX_CHAR_LEN: i64 = 4_000;

/// Length a character or binary type gets when it is **declared** without one.
///
/// A `CAST`/`CONVERT` target uses 30 instead, and that rule belongs to `call.rs`,
/// not here: see the note on [`resolve_data_type`].
const DEFAULT_LEN: i64 = 1;

/// Largest precision of a `decimal`/`numeric`.
const MAX_PRECISION: i64 = 38;

/// Precision a `decimal`/`numeric` gets when it is written without arguments.
const DEFAULT_PRECISION: i64 = 18;

/// Largest mantissa width of a `float(n)` that still means `real`, and smallest
/// that means `float` once passed.
const MAX_REAL_BITS: i64 = 24;

/// Largest mantissa width a `float(n)` may declare.
const MAX_FLOAT_BITS: i64 = 53;

/// Largest fractional-seconds scale of `time`, `datetime2` and `datetimeoffset`.
const MAX_TIME_SCALE: i64 = 7;

/// Scale those three types get when they are written without arguments.
const DEFAULT_TIME_SCALE: i64 = 7;

/// Length of `sysname`, the alias of `nvarchar(128) NOT NULL` in a declaration.
///
/// `128` is also the length of a `sysname` column in `vauban-catalog`.
const SYSNAME_LEN: u16 = 128;

/// Resolves a data type as written into the engine type it denotes.
///
/// The name is matched case-insensitively; the parser has already collapsed the
/// blanks inside a multi-word name (`double precision`, `national character
/// varying`), so the comparison is a plain lower-cased equality. The ISO synonyms
/// resolve to their SQL Server type, and each parameter gets the default SQL Server
/// gives it in a **declaration**.
///
/// `position` is the 1-based rank of the column, parameter or variable inside its
/// statement, the `#<n>` of message 2715. The counter restarts at 1 for each
/// statement. An expression has at most one type and passes 1; `DECLARE` and
/// `CREATE TABLE` are the ones that really number their columns.
///
/// # The default length here is the one of a declaration
///
/// `DECLARE @v varchar` is a `varchar(1)`, `CAST(x AS varchar)` is a
/// `varchar(30)`:
/// `SELECT CAST(SQL_VARIANT_PROPERTY(CAST('abcdefghijklmnopqrstuvwxyz01234567890'
/// AS varchar), 'MaxLength') AS int)` answers `30`. This function applies **1**
/// without exception: `call.rs` overrides it on the result for a conversion, and
/// owns the test that proves it.
///
/// # Errors
///
/// - An unknown name, a qualified name (`dbo.MonType`, a user type) and a type
///   that exists in SQL Server but not in VaubanDB (`sql_variant`, `xml`,
///   `text`…) yield error **2715**, built by [`SqlError::cannot_find_data_type`].
///   The last group is not an unknown type but an unimplemented one, hence a
///   deliberate difference from SQL Server.
/// - A parameter out of bounds (`varchar(9000)`, `decimal(39,2)`, `time(8)`,
///   `char(max)`…) yields **2715 as a placeholder**: see
///   [`value_out_of_bounds`] for the numbers SQL Server really uses and why they are not
///   produced yet.
/// - A type that takes no parameter, given one anyway (`int(10)`,
///   `sysname(10)`), yields **2716**, built by [`out_of_range`]: the type is
///   recognised, it simply does not accept a length.
///
/// In a `CAST`, an unknown type is error **243**, not 2715: `SELECT CAST(1 AS foo)`
/// answers 243, severity 16, state 1, while `DECLARE @v foo` answers 2715. Which of
/// the two a caller reports depends on the context, so it belongs to the callers
/// (`call.rs` for conversions, `variables.rs` and `ddl.rs` for declarations); this
/// function speaks the language of a declaration.
pub(crate) fn resolve_data_type(ty: &DataType, position: u32) -> SqlResult<SqlType> {
    let name = ty.name.to_ascii_lowercase();
    match name.as_str() {
        "bit" => plain(ty, position, SqlType::Bit),
        "tinyint" => plain(ty, position, SqlType::TinyInt),
        "smallint" => plain(ty, position, SqlType::SmallInt),
        // `integer` is the ISO synonym of `int`.
        "int" | "integer" => plain(ty, position, SqlType::Int),
        "bigint" => plain(ty, position, SqlType::BigInt),
        "money" => plain(ty, position, SqlType::Money),
        "smallmoney" => plain(ty, position, SqlType::SmallMoney),
        "date" => plain(ty, position, SqlType::Date),
        "datetime" => plain(ty, position, SqlType::DateTime),
        "smalldatetime" => plain(ty, position, SqlType::SmallDateTime),
        "uniqueidentifier" => plain(ty, position, SqlType::UniqueIdentifier),
        "real" => plain(ty, position, SqlType::Real),
        // `double precision` is the ISO synonym of `float`; `float(n)` decides
        // between `real` and `float` on its own.
        "float" | "double precision" => float(ty, position),
        // `dec` is the ISO synonym of `decimal`. `numeric` keeps its own name:
        // the two are functionally equal but a client reads the name back.
        "dec" | "decimal" => exact_numeric(ty, position, decimal),
        "numeric" => exact_numeric(ty, position, numeric),
        "char" | "character" => sized(ty, position, SqlType::Char, MAX_BYTE_LEN, MaxLen::Refused),
        "varchar" | "char varying" | "character varying" => sized(
            ty,
            position,
            SqlType::VarChar,
            MAX_BYTE_LEN,
            MaxLen::Allowed,
        ),
        "nchar" | "national char" | "national character" => {
            sized(ty, position, SqlType::NChar, MAX_CHAR_LEN, MaxLen::Refused)
        }
        "nvarchar" | "national char varying" | "national character varying" => sized(
            ty,
            position,
            SqlType::NVarChar,
            MAX_CHAR_LEN,
            MaxLen::Allowed,
        ),
        "binary" => sized(ty, position, SqlType::Binary, MAX_BYTE_LEN, MaxLen::Refused),
        "varbinary" | "binary varying" => sized(
            ty,
            position,
            SqlType::VarBinary,
            MAX_BYTE_LEN,
            MaxLen::Allowed,
        ),
        "time" => time_scale(ty, position, SqlType::Time),
        "datetime2" => time_scale(ty, position, SqlType::DateTime2),
        "datetimeoffset" => time_scale(ty, position, SqlType::DateTimeOffset),
        // `sysname` is `nvarchar(128)`; a declaration does not take a length
        // (`DECLARE @v sysname(10)` answers 2716 on SQL Server).
        "sysname" => sysname(ty, position),
        // Types SQL Server has and VaubanDB does not. They are refused as
        // *unknown* for want of anything better to say; the difference is
        // deliberate (`SELECT CAST(1 AS sql_variant)` and `SELECT CAST('a' AS
        // text)` both succeed on SQL Server). `rowversion` and `timestamp` are
        // the same type under two names.
        "sql_variant" | "xml" | "text" | "ntext" | "image" | "hierarchyid" | "geometry"
        | "geography" | "timestamp" | "rowversion" | "table" | "cursor" => {
            Err(unknown(ty, position))
        }
        // Anything else, a qualified name included: `DataType::name` holds
        // `dbo.MonType` in one piece, and user types are not resolved.
        _ => Err(unknown(ty, position)),
    }
}

/// Resolves the declared type of a parameter or variable into the type it carries, nullable.
///
/// This is [`resolve_data_type`] for callers outside the binder that read a declaration
/// list themselves, such as the `@params` of `sp_executesql` and `sp_prepare`. It applies
/// the declaration defaults (`varchar` is `varchar(1)`, `decimal` is `decimal(18, 0)`,
/// `datetime2` keeps its written scale) and answers 2715 for an unknown name, 2716 for
/// `int(10)` (`declared_type_is_the_resolved_type_nullable`). `position` is the 1-based
/// rank of the declaration in its list, the `#<n>` of those two messages.
///
/// A declared parameter or variable accepts `NULL`, hence the nullable [`TypeInfo`].
pub fn resolve_declared_type(ty: &DataType, position: u32) -> SqlResult<TypeInfo> {
    resolve_data_type(ty, position).map(|sql_type| TypeInfo::new(sql_type, true))
}

/// Whether a sized type accepts the `max` argument.
///
/// `varchar`, `nvarchar` and `varbinary` do; the others do not. SQL Server rejects
/// `char(max)` as a **syntax** error (`SELECT CAST(1 AS char(max))` answers 156),
/// which is the parser's business; this module simply refuses the argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaxLen {
    /// `varchar(max)`, `nvarchar(max)`, `varbinary(max)`.
    Allowed,
    /// `char(max)`, `nchar(max)`, `binary(max)`.
    Refused,
}

/// Builds `decimal(p, s)`; a named function so that [`exact_numeric`] can take
/// the constructor as a value.
fn decimal(precision: u8, scale: u8) -> SqlType {
    SqlType::Decimal { precision, scale }
}

/// Builds `numeric(p, s)`.
fn numeric(precision: u8, scale: u8) -> SqlType {
    SqlType::Numeric { precision, scale }
}

/// A type that takes no parameter: it resolves to `sql_type`, and any argument is
/// refused as 2716 — the type is recognised, unlike the 2715 [`unknown`] answers, and
/// simply does not accept a length (`DECLARE @v int(10);`, `DECLARE @v sysname(10);`).
fn plain(ty: &DataType, position: u32, sql_type: SqlType) -> SqlResult<SqlType> {
    if ty.args.is_empty() {
        Ok(sql_type)
    } else {
        Err(out_of_range(ty, position))
    }
}

/// Resolves a character or binary type: no argument means [`DEFAULT_LEN`], `(n)`
/// means `1..=max_len`, and `(max)` is accepted only when `max_allowed` says so.
fn sized(
    ty: &DataType,
    position: u32,
    make: fn(Len) -> SqlType,
    max_len: i64,
    max_allowed: MaxLen,
) -> SqlResult<SqlType> {
    let len = match ty.args.as_slice() {
        [] => DEFAULT_LEN,
        [TypeArg::Number(n)] => *n,
        [TypeArg::Max] if max_allowed == MaxLen::Allowed => return Ok(make(Len::Max)),
        _ => return Err(value_out_of_bounds(ty, position)),
    };
    if !(1..=max_len).contains(&len) {
        return Err(value_out_of_bounds(ty, position));
    }
    let len = u16::try_from(len).map_err(|_| value_out_of_bounds(ty, position))?;
    Ok(make(Len::Fixed(len)))
}

/// Resolves `decimal`/`numeric`: no argument means `(18, 0)`, one argument means
/// `(p, 0)`, precision is `1..=38` and scale is `0..=p`.
fn exact_numeric(ty: &DataType, position: u32, make: fn(u8, u8) -> SqlType) -> SqlResult<SqlType> {
    let (precision, scale) = match ty.args.as_slice() {
        [] => (DEFAULT_PRECISION, 0),
        [TypeArg::Number(p)] => (*p, 0),
        [TypeArg::Number(p), TypeArg::Number(s)] => (*p, *s),
        _ => return Err(value_out_of_bounds(ty, position)),
    };
    if !(1..=MAX_PRECISION).contains(&precision) || !(0..=precision).contains(&scale) {
        return Err(value_out_of_bounds(ty, position));
    }
    let precision = u8::try_from(precision).map_err(|_| value_out_of_bounds(ty, position))?;
    let scale = u8::try_from(scale).map_err(|_| value_out_of_bounds(ty, position))?;
    Ok(make(precision, scale))
}

/// Resolves `float` and its ISO synonym: no argument means `float`, `float(n)` is
/// `real` up to 24 bits of mantissa and `float` from 25 to 53.
///
/// `DECLARE @v float(54)` answers 2750 on SQL Server, while
/// `SELECT CAST(1 AS float(54))` succeeds and yields a `float`. A declaration is
/// what this function resolves, so 54 is refused.
fn float(ty: &DataType, position: u32) -> SqlResult<SqlType> {
    let bits = match ty.args.as_slice() {
        [] => return Ok(SqlType::Float),
        [TypeArg::Number(n)] => *n,
        _ => return Err(value_out_of_bounds(ty, position)),
    };
    match bits {
        1..=MAX_REAL_BITS => Ok(SqlType::Real),
        b if (MAX_REAL_BITS + 1..=MAX_FLOAT_BITS).contains(&b) => Ok(SqlType::Float),
        _ => Err(value_out_of_bounds(ty, position)),
    }
}

/// Resolves `sysname`: an unparameterised name means `nvarchar(128)`; a length
/// is refused in a declaration.
fn sysname(ty: &DataType, position: u32) -> SqlResult<SqlType> {
    plain(ty, position, SqlType::NVarChar(Len::Fixed(SYSNAME_LEN)))
}

/// Whether `ty` names `sysname`, case-insensitively.
pub(crate) fn is_sysname(ty: &DataType) -> bool {
    ty.name.eq_ignore_ascii_case("sysname")
}

/// Resolves `sysname` as the target of a conversion: an unparameterised name
/// means `nvarchar(128)`, `(n)` means `nvarchar(n)` for `1..=4000`.
///
/// A declaration refuses a length; a `CAST`/`CONVERT` accepts one, as
/// `SELECT SQL_VARIANT_PROPERTY(CAST(N'x' AS sysname(10)), 'MaxLength')` answers
/// `20` on SQL Server.
pub(crate) fn resolve_sysname_cast(ty: &DataType, position: u32) -> SqlResult<SqlType> {
    let len = match ty.args.as_slice() {
        [] => SYSNAME_LEN,
        [TypeArg::Number(n)] if (1..=MAX_CHAR_LEN).contains(n) => {
            u16::try_from(*n).map_err(|_| value_out_of_bounds(ty, position))?
        }
        _ => return Err(value_out_of_bounds(ty, position)),
    };
    Ok(SqlType::NVarChar(Len::Fixed(len)))
}

/// Resolves `time`, `datetime2` and `datetimeoffset`: no argument means scale 7,
/// `(s)` means `0..=7`.
fn time_scale(ty: &DataType, position: u32, make: fn(u8) -> SqlType) -> SqlResult<SqlType> {
    let scale = match ty.args.as_slice() {
        [] => DEFAULT_TIME_SCALE,
        [TypeArg::Number(s)] => *s,
        _ => return Err(value_out_of_bounds(ty, position)),
    };
    if !(0..=MAX_TIME_SCALE).contains(&scale) {
        return Err(value_out_of_bounds(ty, position));
    }
    let scale = u8::try_from(scale).map_err(|_| value_out_of_bounds(ty, position))?;
    Ok(make(scale))
}

/// Error 2715 for a name no type answers to, with the name as the user wrote it
/// (`DataType::name` keeps the case and the qualification).
fn unknown(ty: &DataType, position: u32) -> SqlError {
    SqlError::cannot_find_data_type(position, &ty.name)
}

/// **Placeholder** error for a parameter whose value is out of bounds, on a type that
/// does accept one.
///
/// SQL Server has one number per case, and the errors catalogue has them not
/// yet. Until they are, an out-of-bounds parameter answers the same 2715
/// as an unknown type, which is the wrong number with the wrong message but the
/// right verdict — the type is refused.
///
/// What SQL Server answers, as `SELECT CAST(1 AS <type>)` and as
/// `DECLARE @v <type>` (number/severity/state):
///
/// | Type | `CAST` | `DECLARE` |
/// |---|---|---|
/// | `varchar(9000)` | 131/15/3, the size exceeds the maximum of 8000 | same |
/// | `nvarchar(5000)` | 131/16/1, the size exceeds the maximum of 4000 | 2717/16/2, the size given to the parameter exceeds the maximum |
/// | `decimal(39, 2)` | 2717/16/1, the size exceeds the maximum of 38 | same |
/// | `decimal(5, 6)` | 192/15/1, the scale must not exceed the precision | same |
/// | `time(8)` | 1002/15/1, the scale is invalid | same |
/// | `char(max)` | 156/15/1, a syntax error (the **parser** refuses it) | same |
///
/// `float(0)`, `varchar(0)` and `decimal(0, 0)` answer 1001/15/1 (a length or
/// precision of 0 is invalid), and `float(54)` answers 2750/16/1 in a
/// declaration. The numbers 131, 192, 1001, 1002, 2717 and 2750 need a catalogue
/// entry and a constructor in `errors` first.
fn value_out_of_bounds(ty: &DataType, position: u32) -> SqlError {
    unknown(ty, position)
}

/// Error 2716 for a type that [`plain`] resolves, given a length it does not accept
/// (`DECLARE @v int(10);`, `DECLARE @v sysname(10);`): the type is recognised, unlike
/// the 2715 [`unknown`] answers, and no value would make the length acceptable — the
/// difference from [`value_out_of_bounds`], whose type takes a parameter and refuses
/// this one's value instead.
///
/// The state follows the type name: `SqlError::type_does_not_allow_length`'s state
/// table.
fn out_of_range(ty: &DataType, position: u32) -> SqlError {
    SqlError::type_does_not_allow_length(position, &ty.name)
}

#[cfg(test)]
mod tests;
