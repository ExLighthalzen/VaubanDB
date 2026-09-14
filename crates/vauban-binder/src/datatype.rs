//! Data types: `parser::DataType` to `types::SqlType`.
//!
//! The parser does not know types: it hands over a name as written and a list of
//! arguments (`parser::DataType`). This module is where a name becomes a
//! [`SqlType`]: it lower-cases the name, resolves the ISO synonyms, applies the
//! **declaration** defaults of each parameter and checks their bounds. `CAST`,
//! `CONVERT`, `DECLARE` and the DDL go through [`resolve_data_type`].

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{DataType, TypeArg};
use vauban_types::{Len, SqlType};

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
///   [`out_of_range`] for the numbers SQL Server really uses and why they are not
///   produced yet.
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
/// refused.
///
/// `int(10)` does not reach the binder — SQL Server answers a syntax error and so
/// does the parser — but the check costs nothing and keeps the function total.
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
        _ => return Err(out_of_range(ty, position)),
    };
    if !(1..=max_len).contains(&len) {
        return Err(out_of_range(ty, position));
    }
    let len = u16::try_from(len).map_err(|_| out_of_range(ty, position))?;
    Ok(make(Len::Fixed(len)))
}

/// Resolves `decimal`/`numeric`: no argument means `(18, 0)`, one argument means
/// `(p, 0)`, precision is `1..=38` and scale is `0..=p`.
fn exact_numeric(ty: &DataType, position: u32, make: fn(u8, u8) -> SqlType) -> SqlResult<SqlType> {
    let (precision, scale) = match ty.args.as_slice() {
        [] => (DEFAULT_PRECISION, 0),
        [TypeArg::Number(p)] => (*p, 0),
        [TypeArg::Number(p), TypeArg::Number(s)] => (*p, *s),
        _ => return Err(out_of_range(ty, position)),
    };
    if !(1..=MAX_PRECISION).contains(&precision) || !(0..=precision).contains(&scale) {
        return Err(out_of_range(ty, position));
    }
    let precision = u8::try_from(precision).map_err(|_| out_of_range(ty, position))?;
    let scale = u8::try_from(scale).map_err(|_| out_of_range(ty, position))?;
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
        _ => return Err(out_of_range(ty, position)),
    };
    match bits {
        1..=MAX_REAL_BITS => Ok(SqlType::Real),
        b if (MAX_REAL_BITS + 1..=MAX_FLOAT_BITS).contains(&b) => Ok(SqlType::Float),
        _ => Err(out_of_range(ty, position)),
    }
}

/// Resolves `time`, `datetime2` and `datetimeoffset`: no argument means scale 7,
/// `(s)` means `0..=7`.
fn time_scale(ty: &DataType, position: u32, make: fn(u8) -> SqlType) -> SqlResult<SqlType> {
    let scale = match ty.args.as_slice() {
        [] => DEFAULT_TIME_SCALE,
        [TypeArg::Number(s)] => *s,
        _ => return Err(out_of_range(ty, position)),
    };
    if !(0..=MAX_TIME_SCALE).contains(&scale) {
        return Err(out_of_range(ty, position));
    }
    let scale = u8::try_from(scale).map_err(|_| out_of_range(ty, position))?;
    Ok(make(scale))
}

/// Error 2715 for a name no type answers to, with the name as the user wrote it
/// (`DataType::name` keeps the case and the qualification).
fn unknown(ty: &DataType, position: u32) -> SqlError {
    SqlError::cannot_find_data_type(position, &ty.name)
}

/// **Placeholder** error for a parameter out of bounds.
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
fn out_of_range(ty: &DataType, position: u32) -> SqlError {
    unknown(ty, position)
}

#[cfg(test)]
mod tests {
    use super::resolve_data_type;
    use vauban_parser::{DataType, Span, TypeArg};
    use vauban_types::{Len, SqlType};

    /// A span the tests do not look at: `resolve_data_type` reports a position,
    /// not a line.
    fn span() -> Span {
        Span {
            line: 1,
            column: 1,
            offset: 0,
            len: 1,
        }
    }

    /// The `DataType` the parser produces for `name` and `args`, built by hand
    /// from the same two pieces the parser stores (the name as written, the
    /// arguments in order).
    fn ty(name: &str, args: &[TypeArg]) -> vauban_errors::SqlResult<SqlType> {
        resolve_data_type(
            &DataType {
                name: name.to_owned(),
                args: args.to_vec(),
                span: span(),
            },
            1,
        )
    }

    /// Same, for the types written without arguments.
    fn simple(name: &str) -> vauban_errors::SqlResult<SqlType> {
        ty(name, &[])
    }

    #[test]
    fn resolves_simple_types() {
        assert_eq!(simple("int"), Ok(SqlType::Int));
        assert_eq!(simple("INT"), Ok(SqlType::Int));
        assert_eq!(simple("Int"), Ok(SqlType::Int));
        assert_eq!(simple("bigint"), Ok(SqlType::BigInt));
        assert_eq!(simple("smallint"), Ok(SqlType::SmallInt));
        assert_eq!(simple("tinyint"), Ok(SqlType::TinyInt));
        assert_eq!(simple("bit"), Ok(SqlType::Bit));
        assert_eq!(simple("money"), Ok(SqlType::Money));
        assert_eq!(simple("smallmoney"), Ok(SqlType::SmallMoney));
        assert_eq!(simple("date"), Ok(SqlType::Date));
        assert_eq!(simple("datetime"), Ok(SqlType::DateTime));
        assert_eq!(simple("smalldatetime"), Ok(SqlType::SmallDateTime));
        assert_eq!(simple("uniqueidentifier"), Ok(SqlType::UniqueIdentifier));
        assert_eq!(simple("real"), Ok(SqlType::Real));
    }

    #[test]
    fn resolves_parameterised_types() {
        assert_eq!(
            ty("varchar", &[TypeArg::Number(10)]),
            Ok(SqlType::VarChar(Len::Fixed(10)))
        );
        assert_eq!(
            ty("varchar", &[TypeArg::Max]),
            Ok(SqlType::VarChar(Len::Max))
        );
        // `nvarchar(MAX)`: the parser normalises the keyword into `TypeArg::Max`,
        // so the binder does not see the spelling.
        assert_eq!(
            ty("nvarchar", &[TypeArg::Max]),
            Ok(SqlType::NVarChar(Len::Max))
        );
        assert_eq!(
            ty("varbinary", &[TypeArg::Max]),
            Ok(SqlType::VarBinary(Len::Max))
        );
        assert_eq!(
            ty("decimal", &[TypeArg::Number(18), TypeArg::Number(2)]),
            Ok(SqlType::Decimal {
                precision: 18,
                scale: 2
            })
        );
        assert_eq!(
            ty("numeric", &[TypeArg::Number(38)]),
            Ok(SqlType::Numeric {
                precision: 38,
                scale: 0
            })
        );
        assert_eq!(
            ty("datetime2", &[TypeArg::Number(3)]),
            Ok(SqlType::DateTime2(3))
        );
        assert_eq!(ty("time", &[TypeArg::Number(7)]), Ok(SqlType::Time(7)));
        assert_eq!(ty("time", &[TypeArg::Number(0)]), Ok(SqlType::Time(0)));
        assert_eq!(
            ty("datetimeoffset", &[TypeArg::Number(2)]),
            Ok(SqlType::DateTimeOffset(2))
        );
        assert_eq!(
            ty("char", &[TypeArg::Number(8000)]),
            Ok(SqlType::Char(Len::Fixed(8000)))
        );
        assert_eq!(
            ty("nchar", &[TypeArg::Number(4000)]),
            Ok(SqlType::NChar(Len::Fixed(4000)))
        );
        assert_eq!(
            ty("binary", &[TypeArg::Number(4)]),
            Ok(SqlType::Binary(Len::Fixed(4)))
        );
    }

    #[test]
    fn applies_declaration_defaults() {
        assert_eq!(simple("varchar"), Ok(SqlType::VarChar(Len::Fixed(1))));
        assert_eq!(simple("char"), Ok(SqlType::Char(Len::Fixed(1))));
        assert_eq!(simple("nchar"), Ok(SqlType::NChar(Len::Fixed(1))));
        assert_eq!(simple("nvarchar"), Ok(SqlType::NVarChar(Len::Fixed(1))));
        assert_eq!(simple("binary"), Ok(SqlType::Binary(Len::Fixed(1))));
        assert_eq!(simple("varbinary"), Ok(SqlType::VarBinary(Len::Fixed(1))));
        assert_eq!(
            simple("decimal"),
            Ok(SqlType::Decimal {
                precision: 18,
                scale: 0
            })
        );
        assert_eq!(
            simple("numeric"),
            Ok(SqlType::Numeric {
                precision: 18,
                scale: 0
            })
        );
        assert_eq!(
            ty("decimal", &[TypeArg::Number(5)]),
            Ok(SqlType::Decimal {
                precision: 5,
                scale: 0
            })
        );
        assert_eq!(simple("datetime2"), Ok(SqlType::DateTime2(7)));
        assert_eq!(simple("time"), Ok(SqlType::Time(7)));
        assert_eq!(simple("datetimeoffset"), Ok(SqlType::DateTimeOffset(7)));
        assert_eq!(simple("float"), Ok(SqlType::Float));
        assert_eq!(ty("float", &[TypeArg::Number(1)]), Ok(SqlType::Real));
        assert_eq!(ty("float", &[TypeArg::Number(24)]), Ok(SqlType::Real));
        assert_eq!(ty("float", &[TypeArg::Number(25)]), Ok(SqlType::Float));
        assert_eq!(ty("float", &[TypeArg::Number(53)]), Ok(SqlType::Float));
    }

    #[test]
    fn resolves_iso_synonyms() {
        assert_eq!(
            ty("dec", &[TypeArg::Number(5), TypeArg::Number(2)]),
            Ok(SqlType::Decimal {
                precision: 5,
                scale: 2
            })
        );
        assert_eq!(simple("integer"), Ok(SqlType::Int));
        assert_eq!(simple("double precision"), Ok(SqlType::Float));
        assert_eq!(simple("DOUBLE PRECISION"), Ok(SqlType::Float));
        assert_eq!(
            ty("character", &[TypeArg::Number(10)]),
            Ok(SqlType::Char(Len::Fixed(10)))
        );
        assert_eq!(
            ty("char varying", &[TypeArg::Number(10)]),
            Ok(SqlType::VarChar(Len::Fixed(10)))
        );
        assert_eq!(
            ty("character varying", &[TypeArg::Number(10)]),
            Ok(SqlType::VarChar(Len::Fixed(10)))
        );
        assert_eq!(
            ty("national char", &[TypeArg::Number(2)]),
            Ok(SqlType::NChar(Len::Fixed(2)))
        );
        assert_eq!(
            ty("national character", &[TypeArg::Number(2)]),
            Ok(SqlType::NChar(Len::Fixed(2)))
        );
        assert_eq!(
            ty("national char varying", &[TypeArg::Number(10)]),
            Ok(SqlType::NVarChar(Len::Fixed(10)))
        );
        assert_eq!(
            ty("national character varying", &[TypeArg::Number(10)]),
            Ok(SqlType::NVarChar(Len::Fixed(10)))
        );
        assert_eq!(
            ty("binary varying", &[TypeArg::Number(4)]),
            Ok(SqlType::VarBinary(Len::Fixed(4)))
        );
    }

    #[test]
    fn unknown_type_is_2715() {
        let err = simple("foo").expect_err("foo is not a type");
        assert_eq!(err.number, 2715);
        assert_eq!(err.severity, 16);
        assert_eq!(
            err.message,
            "Column, parameter or variable #1: unknown data type foo."
        );

        // A qualified name arrives in one piece and is echoed as written.
        let err = simple("dbo.MonType").expect_err("user types are not resolved");
        assert_eq!(err.number, 2715);
        assert_eq!(
            err.message,
            "Column, parameter or variable #1: unknown data type dbo.MonType."
        );

        // A type SQL Server has and VaubanDB does not: same number, a deliberate
        // difference.
        for name in [
            "sql_variant",
            "xml",
            "text",
            "ntext",
            "image",
            "hierarchyid",
            "geometry",
            "geography",
            "timestamp",
            "rowversion",
            "table",
            "cursor",
        ] {
            let err = simple(name).expect_err("not a type VaubanDB serves");
            assert_eq!(err.number, 2715, "{name}");
            assert_eq!(
                err.message,
                format!("Column, parameter or variable #1: unknown data type {name}.")
            );
        }
    }

    #[test]
    fn position_is_the_rank_in_the_statement() {
        let err = resolve_data_type(
            &DataType {
                name: "foo".to_owned(),
                args: Vec::new(),
                span: span(),
            },
            3,
        )
        .expect_err("foo is not a type");
        assert_eq!(
            err.message,
            "Column, parameter or variable #3: unknown data type foo."
        );
    }

    #[test]
    fn out_of_bounds_parameters_are_refused() {
        // The number is the 2715 placeholder of `out_of_range`, whose rustdoc
        // lists what SQL Server really answers:
        // 131 for the two lengths, 2717 for `decimal(39,2)`, 192 for
        // `decimal(5,6)`, 1002 for `time(8)` and a syntax error 156 for
        // `char(max)`. Those numbers have no catalogue entry yet.
        let vectors: [(&str, Vec<TypeArg>); 6] = [
            ("varchar", vec![TypeArg::Number(9000)]),
            ("nvarchar", vec![TypeArg::Number(5000)]),
            ("decimal", vec![TypeArg::Number(39), TypeArg::Number(2)]),
            ("decimal", vec![TypeArg::Number(5), TypeArg::Number(6)]),
            ("time", vec![TypeArg::Number(8)]),
            ("char", vec![TypeArg::Max]),
        ];
        for (name, args) in vectors {
            let err = ty(name, &args).expect_err("out of bounds");
            assert_eq!(err.number, 2715, "{name}");
        }

        // The other bounds of the same family, refused the same way.
        assert!(ty("char", &[TypeArg::Number(8001)]).is_err());
        assert!(ty("binary", &[TypeArg::Number(0)]).is_err());
        assert!(ty("nchar", &[TypeArg::Number(4001)]).is_err());
        assert!(ty("varbinary", &[TypeArg::Number(-1)]).is_err());
        assert!(ty("nchar", &[TypeArg::Max]).is_err());
        assert!(ty("binary", &[TypeArg::Max]).is_err());
        assert!(ty("decimal", &[TypeArg::Number(0)]).is_err());
        assert!(ty("float", &[TypeArg::Number(0)]).is_err());
        assert!(ty("float", &[TypeArg::Number(54)]).is_err());
        assert!(ty("datetime2", &[TypeArg::Number(8)]).is_err());
        assert!(ty("datetimeoffset", &[TypeArg::Number(-1)]).is_err());
        assert!(ty("int", &[TypeArg::Number(10)]).is_err());
        assert!(ty("varchar", &[TypeArg::Ident("max".to_owned())]).is_err());
        assert!(ty("decimal", &[TypeArg::Max]).is_err());
    }

    /// The numbers SQL Server answers for the six shapes above. Ignored: 131, 192,
    /// 1002 and 2717 are neither in the errors catalogue nor given a constructor.
    /// The assertions are written down so that whoever adds them has the test
    /// ready.
    #[test]
    #[ignore = "numbers 131, 192, 1002 and 2717 are missing from the errors catalogue"]
    fn out_of_bounds_parameters_use_the_numbers_of_sql_server() {
        assert_eq!(
            ty("varchar", &[TypeArg::Number(9000)])
                .expect_err("out of bounds")
                .number,
            131
        );
        assert_eq!(
            ty("nvarchar", &[TypeArg::Number(5000)])
                .expect_err("out of bounds")
                .number,
            131
        );
        assert_eq!(
            ty("decimal", &[TypeArg::Number(39), TypeArg::Number(2)])
                .expect_err("out of bounds")
                .number,
            2717
        );
        assert_eq!(
            ty("decimal", &[TypeArg::Number(5), TypeArg::Number(6)])
                .expect_err("out of bounds")
                .number,
            192
        );
        assert_eq!(
            ty("time", &[TypeArg::Number(8)])
                .expect_err("out of bounds")
                .number,
            1002
        );
    }
}
