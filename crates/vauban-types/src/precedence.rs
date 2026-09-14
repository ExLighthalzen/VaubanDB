//! Data type precedence and [`implicit_result_type`].
//!
//! Data type precedence ranks the data types: when an operator combines two operands of
//! different types, the one of lower precedence is converted to the one of higher
//! precedence. This module answers that one question, "what is the common type of these
//! two?", and nothing else: it knows no operator and
//! computes no value. The type of the *result* of an operator (`1 / 3`, `'a' + 'b'`)
//! belongs to `arith::op_type`, which reuses [`rank`] and [`operand_precision_scale`].

use vauban_errors::{SqlError, SqlResult};

use crate::errors;
use crate::sql_type::{Len, SqlType, TypeFamily, TypeInfo};

/// The precedence of `ty`, from 23 (strongest) down to 1 (weakest).
///
/// The order is the data type precedence of T-SQL, restricted to the types [`SqlType`]
/// represents: the types the precedence also ranks (`sql_variant`, `xml`, `ntext`, `text`,
/// `image`, `timestamp` and the user-defined types) have no variant here and therefore
/// no rank.
///
/// The sequence decreases in the **weak** sense: `decimal` and `numeric` are functionally
/// one type and share a single rank. Each
/// other pair of distinct variants has distinct ranks, so the rank identifies the variant
/// up to that one tie, and up to the parameters (`varchar(10)` and `varchar(max)` are one
/// rank).
///
/// The absolute values mean nothing outside this module: only their order is a rule.
pub(crate) fn rank(ty: &SqlType) -> u8 {
    match ty {
        SqlType::DateTimeOffset(_) => 23,
        SqlType::DateTime2(_) => 22,
        SqlType::DateTime => 21,
        SqlType::SmallDateTime => 20,
        SqlType::Date => 19,
        SqlType::Time(_) => 18,
        SqlType::Float => 17,
        SqlType::Real => 16,
        // `decimal` and `numeric`: same precedence, on purpose.
        SqlType::Decimal { .. } | SqlType::Numeric { .. } => 15,
        SqlType::Money => 14,
        SqlType::SmallMoney => 13,
        SqlType::BigInt => 12,
        SqlType::Int => 11,
        SqlType::SmallInt => 10,
        SqlType::TinyInt => 9,
        SqlType::Bit => 8,
        SqlType::UniqueIdentifier => 7,
        SqlType::NVarChar(_) => 6,
        SqlType::NChar(_) => 5,
        SqlType::VarChar(_) => 4,
        SqlType::Char(_) => 3,
        SqlType::VarBinary(_) => 2,
        SqlType::Binary(_) => 1,
    }
}

/// The precision and the scale `ty` brings to the **common type** of a pair, out of its own
/// declaration, or `None` when its declaration carries neither.
///
/// The precision and scale of an expression that is not `decimal` are those defined for
/// its data type: `int` counts as `numeric(10, 0)`, `bigint` as `numeric(19, 0)`,
/// `smallint` as `numeric(5, 0)`, `tinyint` as `numeric(3, 0)`, `bit` as `numeric(1, 0)`,
/// `money` as `numeric(19, 4)` and `smallmoney` as `numeric(10, 4)`. Three of those lines
/// show in a product: `int * decimal(1,1)` is a `decimal(12,1)`, `tinyint * decimal(9,3)`
/// a `decimal(13,3)` and `money * decimal(9,3)` a `decimal(29,7)`
/// (`tests::non_decimal_types_have_the_precision_of_their_type`).
///
/// The `bit` line holds here and is overridden in [`table_precision_scale`], for the
/// arithmetic table alone: the two readings of a `bit` pull in opposite directions, and
/// this crate cannot satisfy both through one common type.
///
/// - Against the `numeric(1, 0)` kept here: `COALESCE(CAST(0 AS bit), CAST(0.1 AS
///   decimal(1,1)))` is a `decimal(1,1)` and `COALESCE(CAST(1 AS bit), CAST(0.1 AS
///   decimal(1,1)))` raises 8115 naming `tinyint` and `numeric` in SQL Server, which
///   leaves that common type unwidened where [`merge_exact_numeric`] widens it to
///   `decimal(2,1)`: a deliberate difference.
/// - For it: `CAST(1 AS bit) > CAST(0.1 AS decimal(1,1))` answers a row, and so do `=`, the
///   reversed `<`, `BETWEEN`, `IN` and `> CAST(0.11111 AS decimal(5,5))`.
///   `binder::expr::compare_node` converts a comparison operand to the type this function
///   computes, so a `bit` reduced to `None` would send those six to the 8115 quoted above,
///   the way a character operand of overflowing value goes.
///
/// What the `numeric(1, 0)` kept here buys stops where the widening saturates:
/// `CAST(1 AS bit) >= CAST(0.1 AS decimal(38,38))` answers a row in SQL Server and raises
/// that 8115 here, a deliberate difference.
///
/// `float` and `real` return `None`: they are approximate, their declaration carries a
/// mantissa width rather than a decimal precision and a scale, and `arith::op_type` sends a
/// pair that holds one of them to `float` without consulting the precision-and-scale table.
/// The character, binary, date and `uniqueidentifier` types return `None` too: their
/// declarations carry a length or a fractional-seconds scale, not a decimal precision.
pub(crate) fn numeric_precision_scale(ty: &SqlType) -> Option<(u8, u8)> {
    match ty {
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            Some((*precision, *scale))
        }
        SqlType::BigInt => Some((19, 0)),
        SqlType::Int => Some((10, 0)),
        SqlType::SmallInt => Some((5, 0)),
        SqlType::TinyInt => Some((3, 0)),
        SqlType::Bit => Some((1, 0)),
        SqlType::Money => Some((19, 4)),
        SqlType::SmallMoney => Some((10, 4)),
        _ => None,
    }
}

/// The precision and the scale `ty` brings to the precision-and-scale **table** of an
/// arithmetic result out of its own declaration, or `None` when it brings neither there.
///
/// It is [`numeric_precision_scale`], except for `bit`, which brings neither:
/// `CAST(0 AS bit) * CAST(0.1 AS decimal(1,1))` is a `decimal(3,2)`, the product of two
/// `decimal(1,1)`, where `numeric(1, 0)` gives a `decimal(3,1)`, and `CAST(1 AS bit) *
/// CAST(1.5 AS decimal(9,3))` is a `decimal(19,6)` against the `decimal(11,3)` of the
/// `numeric(1, 0)` reading. The common type of the pair keeps `numeric(1, 0)`: see
/// [`numeric_precision_scale`].
fn table_precision_scale(ty: &SqlType) -> Option<(u8, u8)> {
    match ty {
        SqlType::Bit => None,
        _ => numeric_precision_scale(ty),
    }
}

/// The precision and the scale an operand of type `ty` brings to the precision-and-scale
/// table when the other operand of the operation is of type `other`.
///
/// It is [`table_precision_scale`] of `ty` when its declaration carries one, and that of
/// `other` when it does not: a character operand and a `bit` operand enter the table with
/// the precision **and** the scale of the other side. On `* decimal(9,3)`, the operands
/// `varchar(4)`, `varchar(30)`, `char(4)`, `nvarchar(4)`, `varchar(max)` and `CAST(1 AS
/// bit)` each give `decimal(19,6)`, which is the `decimal(9,3) * decimal(9,3)` line of the
/// table (`tests::an_operand_without_a_precision_borrows_the_other_one`).
///
/// The reading that competes with it, the common type of the pair, is separated by
/// `decimal(1,1)`, which keeps zero integral digit: `CAST('0.1' AS varchar(4)) * CAST(0.1 AS
/// decimal(1,1))` and `CAST(0 AS bit) * CAST(0.1 AS decimal(1,1))` are both a `decimal(3,2)`,
/// the product of two `decimal(1,1)`, where the common type of either pair, `decimal(1,1)`
/// for the character operand and `decimal(2,1)` for the `bit`, gives `decimal(3,2)` for the
/// first and `decimal(5,2)` for the second. The forms covered are `+`, `-`, `*`, `/`
/// against `decimal(9,3)` for both kinds of operand, `*` and `/` against `decimal(1,1)`,
/// and `%` against `decimal(9,3)` for the `bit` (`%` between a character operand and a
/// `decimal` is refused). Nothing is claimed here for a pair whose two operands lack a
/// precision.
pub(crate) fn operand_precision_scale(ty: &SqlType, other: &SqlType) -> Option<(u8, u8)> {
    table_precision_scale(ty).or_else(|| table_precision_scale(other))
}

/// The largest precision a `decimal` or a `numeric` can hold.
const MAX_PRECISION: u8 = 38;

/// How a type behaves towards the *other* families when a conversion is implied.
///
/// Coarser than [`TypeFamily`]: every numeric family reconciles with every other numeric
/// family, so they share one group here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// `bit`, the integers, `decimal`, `numeric`, `money`, `smallmoney`, `float`, `real`.
    Numeric,
    /// `char`, `varchar`, `nchar`, `nvarchar`.
    Character,
    /// `binary`, `varbinary`.
    Binary,
    /// `date`, `time`, `datetime`, `smalldatetime`, `datetime2`, `datetimeoffset`.
    Temporal,
    /// `uniqueidentifier`.
    Guid,
}

/// The [`Group`] of `ty`.
fn group(ty: &SqlType) -> Group {
    match ty.family() {
        TypeFamily::Bit
        | TypeFamily::Integer
        | TypeFamily::ExactNumeric
        | TypeFamily::ApproxNumeric
        | TypeFamily::Money => Group::Numeric,
        TypeFamily::Character => Group::Character,
        TypeFamily::Binary => Group::Binary,
        TypeFamily::DateTime => Group::Temporal,
        TypeFamily::Guid => Group::Guid,
    }
}

/// Whether a date or time type converts to and from numbers and binaries.
///
/// `datetime` and `smalldatetime`, the two types stored as a day count, do (`GETDATE() + 1`
/// is legal); `date`, `time`, `datetime2` and `datetimeoffset` have no conversion to
/// a number.
fn temporal_converts_to_numbers(ty: &SqlType) -> bool {
    matches!(ty, SqlType::DateTime | SqlType::SmallDateTime)
}

/// Whether the two types have a common type at all, that is, whether the weaker one
/// converts **implicitly** to the stronger one.
///
/// The pairs this crate refuses are the ones with no implicit conversion in either
/// direction: `uniqueidentifier` with a number, a date or a binary (a `CAST` is required
/// there), `date`, `time`, `datetime2` or `datetimeoffset` with a number or a binary, and
/// a binary with `float` or `real`.
///
/// On that last line, `SELECT CAST(1.5 AS float) + CAST(0x0F AS binary(1));` raises 206
/// naming binary and float, and so does its reverse, while the same binary against a
/// `decimal` is accepted and fails at the value instead (8114, `SELECT CAST(1.5 AS
/// decimal(2,1)) + CAST(0x0F AS binary(1));`).
fn pair_converts_implicitly(a: &SqlType, b: &SqlType) -> bool {
    let (ga, gb) = (group(a), group(b));
    if ga == gb {
        return true;
    }
    // A character type converts implicitly to and from every other type this crate knows.
    if ga == Group::Character || gb == Group::Character {
        return true;
    }
    if binary_against_approximate(a, b) {
        return false;
    }
    match (ga, gb) {
        (Group::Guid, _) | (_, Group::Guid) => false,
        (Group::Temporal, _) => temporal_converts_to_numbers(a),
        (_, Group::Temporal) => temporal_converts_to_numbers(b),
        // Numbers and binaries: `DECLARE @b binary(4) = 1;` is legal both ways.
        _ => true,
    }
}

/// Whether the pair is a binary type with a `float` or a `real`, in either order: the one
/// pair of a binary and a number that has no implicit conversion (see
/// [`pair_converts_implicitly`]).
fn binary_against_approximate(a: &SqlType, b: &SqlType) -> bool {
    let pair = |x: &SqlType, y: &SqlType| {
        x.family() == TypeFamily::Binary && y.family() == TypeFamily::ApproxNumeric
    };
    pair(a, b) || pair(b, a)
}

/// Order the names of an already selected 206 diagnostic of an **arithmetic** operator.
///
/// `SELECT 1 + CAST('20000101' AS date);` and its reverse both name `date` before `int`,
/// so the written order is not the one reported. This is not precedence either: `date`
/// ranks above `int`, whereas the GUID that [`implicit_result_type`] reorders ranks below
/// it.
///
/// Both operand orders and `+ -` for date/int, time(3)/int, datetime2(3)/numeric(6,2) and
/// datetimeoffset(3)/float, sixteen shapes, name the temporal type first, and so do
/// `tinyint`, `smallint`, `real`, `money`, `smallmoney`, `numeric(9,4)` and
/// `decimal(19,0)` on `+` against the four temporal types in both orders
/// (`operand_clash_names_the_nonnumeric_type_first` in `tests/op_type.rs`).
///
/// The test below reorders on the [`Group`] of the left type, so it also fires on `bit`,
/// which [`group`] counts as a number and which SQL Server does **not** put at 206: on
/// the four temporal types, both written orders, `+`, it raises 402, as in `SELECT CAST(1
/// AS bit) + CAST('20010102' AS date);`. This crate raises 206 there, a deliberate
/// difference, and the reordering turns it into `date` before `bit`; the order claimed
/// here is claimed for the numeric types listed above, not for `bit`.
///
/// The same caller also reaches this function with a binary or a character operand facing
/// one of those four types, and there the written order stands: `SELECT CAST(0x01 AS
/// varbinary(1)) + CAST('20010102' AS date);` names `varbinary` before `date` and its
/// reverse `date` before `varbinary`, `SELECT CAST('20010102' AS date) + 'a';` names
/// `date` before `varchar` and its reverse `varchar` before `date`. On the binary pair
/// what diverges from SQL Server is the number rather than the order: `SELECT CAST(NULL
/// AS date) + CAST(NULL AS varbinary(4));` raises 402 there, naming `date` and
/// `varbinary`, and its reverse 402 naming `varbinary` and `date`, `*` and `/` raising
/// 8117 on `date`. Nothing is claimed about a character type facing a temporal one.
///
/// `COALESCE` reads a number against a temporal type the other way: `SELECT COALESCE(1,
/// CAST('20010102' AS date));` and its reverse both name `int` first, and the three other
/// modern temporal types answer likewise against `1`. That is why this helper stays out of
/// common-type inference, whose callers include both constructs. It does not select the
/// error number; the operator's existing 402/8117 decisions stay with its caller.
pub(crate) fn ordered_operand_type_clash(a: &SqlType, b: &SqlType) -> SqlError {
    if group(a) == Group::Numeric
        && matches!(
            b,
            SqlType::Date | SqlType::Time(_) | SqlType::DateTime2(_) | SqlType::DateTimeOffset(_)
        )
    {
        errors::operand_type_clash(b, a)
    } else {
        errors::operand_type_clash(a, b)
    }
}

/// Error 206 for a pair that has no common type, `uniqueidentifier` named before a number.
///
/// The GUID comes first in four constructs, in both operand orders: the arithmetic
/// operators `+ - * /` against int, bigint and decimal(6,2), two-argument `COALESCE`
/// (`SELECT COALESCE(1, NEWID());` and its reverse), the branches of a `CASE` (`SELECT
/// CASE WHEN 1=1 THEN 1 ELSE NEWID() END;` and its reverse) and the `=` comparison
/// (`SELECT CASE WHEN 1 = CAST('01234567-89ab-cdef-0123-456789abcdef' AS
/// uniqueidentifier) THEN 1 ELSE 0 END;` and its reverse, which name `uniqueidentifier`
/// before `tinyint`). The pair settles that order in those four constructs; it does not
/// settle it for each construct, `ISNULL` naming its **second** argument first in SQL
/// Server (`SELECT ISNULL(NEWID(), 1);` names `int` before `uniqueidentifier`). The
/// `ISNULL` shapes, this pair and the four modern temporal types against `int` in both
/// orders, answer with a row and no error in this engine, so no caller reaches
/// that order from here today.
///
/// The other pairs keep the order their caller gave, which is what [`implicit_result_type`]
/// documents.
fn common_type_clash(a: &SqlType, b: &SqlType) -> SqlError {
    let guid_second = group(a) == Group::Numeric && *b == SqlType::UniqueIdentifier;
    // A binary type facing a `float` or a `real` is named first whichever side wrote it:
    // `SELECT CAST(1.5 AS float) + CAST(0x0F AS binary(1));` and its reverse both name
    // `binary` before `float`.
    let binary_second = binary_against_approximate(a, b) && b.family() == TypeFamily::Binary;
    if guid_second || binary_second {
        errors::operand_type_clash(b, a)
    } else {
        errors::operand_type_clash(a, b)
    }
}

/// The wider of two declared lengths, `max` beating every fixed length.
fn widest(a: Len, b: Len) -> Len {
    match (a, b) {
        (Len::Max, _) | (_, Len::Max) => Len::Max,
        (Len::Fixed(x), Len::Fixed(y)) => Len::Fixed(x.max(y)),
    }
}

/// The declared length of a character or binary type, `None` for the other types.
fn length_of(ty: &SqlType) -> Option<Len> {
    match ty {
        SqlType::Char(len)
        | SqlType::VarChar(len)
        | SqlType::NChar(len)
        | SqlType::NVarChar(len)
        | SqlType::Binary(len)
        | SqlType::VarBinary(len) => Some(*len),
        _ => None,
    }
}

/// The fractional-seconds scale of `time`, `datetime2` and `datetimeoffset`, `None` for the
/// other types.
fn seconds_scale(ty: &SqlType) -> Option<u8> {
    match ty {
        SqlType::Time(scale) | SqlType::DateTime2(scale) | SqlType::DateTimeOffset(scale) => {
            Some(*scale)
        }
        _ => None,
    }
}

/// The exact-numeric type that holds both operands: `s = max(s1, s2)` and
/// `p = min(38, max(p1 - s1, p2 - s2) + s)`, the integral parts and the fractional parts
/// each being kept whole.
///
/// `winner` is the `decimal` or `numeric` operand. When the declaration of `other` carries
/// neither precision nor scale — a character or binary operand, converted implicitly to the
/// number — the type is `winner`, unchanged:
/// `COALESCE(CAST('2' AS varchar(4)), CAST(1.5 AS decimal(9,3)))` is a `decimal(9,3)`. A
/// `bit` is not in that group here, though SQL Server treats it as one on the pair
/// `bit`/`decimal(1,1)`: this function answers `decimal(2,1)` where SQL Server gives
/// `decimal(1,1)`, a deliberate difference the rustdoc of [`numeric_precision_scale`]
/// states with the comparisons that hold it in place.
///
/// The name of the result is `numeric` as soon as one of the two operands is a `numeric`:
/// the two spellings share one representation and one precedence, and `numeric` is the type
/// of the literals (`1.5`), so a mixed pair reads `numeric`.
fn merge_exact_numeric(winner: &SqlType, other: &SqlType) -> SqlType {
    let (Some((p1, s1)), Some((p2, s2))) = (
        numeric_precision_scale(winner),
        numeric_precision_scale(other),
    ) else {
        return *winner;
    };
    let scale = s1.max(s2);
    let integral = u16::from(p1.saturating_sub(s1)).max(u16::from(p2.saturating_sub(s2)));
    let precision = (integral + u16::from(scale)).min(u16::from(MAX_PRECISION));
    // `precision` is at most `MAX_PRECISION`, so the cast keeps its value.
    let precision = precision as u8;
    if matches!(winner, SqlType::Numeric { .. }) || matches!(other, SqlType::Numeric { .. }) {
        SqlType::Numeric { precision, scale }
    } else {
        SqlType::Decimal { precision, scale }
    }
}

/// The common type of two types: the one of higher precedence, widened so that it holds
/// both operands.
fn common_type(a: &SqlType, b: &SqlType) -> SqlType {
    let (winner, other) = if rank(a) >= rank(b) { (a, b) } else { (b, a) };
    match winner {
        SqlType::Decimal { .. } | SqlType::Numeric { .. } => merge_exact_numeric(winner, other),
        SqlType::Char(len)
        | SqlType::VarChar(len)
        | SqlType::NChar(len)
        | SqlType::NVarChar(len)
        | SqlType::Binary(len)
        | SqlType::VarBinary(len) => {
            let len = match length_of(other) {
                Some(other_len) => widest(*len, other_len),
                None => *len,
            };
            with_length(winner, len)
        }
        SqlType::Time(scale) | SqlType::DateTime2(scale) | SqlType::DateTimeOffset(scale) => {
            // Two `time(s)`, two `datetime2(s)` or two `datetimeoffset(s)` share a rank, so
            // the scale of the result is the wider of the two; against any other type the
            // winner keeps its own scale.
            match seconds_scale(other) {
                Some(other_scale) if rank(winner) == rank(other) => {
                    with_seconds_scale(winner, (*scale).max(other_scale))
                }
                _ => *winner,
            }
        }
        other => *other,
    }
}

/// `ty`, a character or binary type, with `len` as its declared length.
fn with_length(ty: &SqlType, len: Len) -> SqlType {
    match ty {
        SqlType::Char(_) => SqlType::Char(len),
        SqlType::VarChar(_) => SqlType::VarChar(len),
        SqlType::NChar(_) => SqlType::NChar(len),
        SqlType::NVarChar(_) => SqlType::NVarChar(len),
        SqlType::Binary(_) => SqlType::Binary(len),
        SqlType::VarBinary(_) => SqlType::VarBinary(len),
        other => *other,
    }
}

/// `ty`, a `time`, `datetime2` or `datetimeoffset`, with `scale` as its fractional-seconds
/// scale.
fn with_seconds_scale(ty: &SqlType, scale: u8) -> SqlType {
    match ty {
        SqlType::Time(_) => SqlType::Time(scale),
        SqlType::DateTime2(_) => SqlType::DateTime2(scale),
        SqlType::DateTimeOffset(_) => SqlType::DateTimeOffset(scale),
        other => *other,
    }
}

/// The common type of two operands: the type both convert to implicitly before an operator
/// or a comparison combines them.
///
/// The type is the one of higher precedence, widened so that it holds both operands:
///
/// - two `decimal` or `numeric` operands, or one of them and any other exact number, give
///   `s = max(s1, s2)` and `p = min(38, max(p1 - s1, p2 - s2) + s)`, an operand that is not
///   `decimal` counting with the precision and the scale of its own type (`int` is
///   `numeric(10, 0)`); a pair whose spellings differ reads `numeric`;
/// - two character or two binary operands give the wider of the two declared lengths, `max`
///   beating every fixed length;
/// - two `time`, `datetime2` or `datetimeoffset` operands give the wider scale;
/// - two identical types give that type.
///
/// The result is nullable as soon as one operand is, and carries the collation of the
/// character operand — the left one when both are character types.
///
/// This is **not** the type of the result of an operator: the common type of
/// `numeric(2, 1)` and `numeric(2, 1)` is `numeric(2, 1)`, while their quotient is
/// `numeric(8, 6)`. That table is `binary_op_type`'s.
///
/// # Errors
///
/// Error 206, `Type mismatch: <a> cannot be combined with <b>.`, when the two types have
/// no implicit conversion at all: `uniqueidentifier` with a number, a date or a binary, and
/// `date`, `time`, `datetime2` or `datetimeoffset` with a number or a binary.
///
/// A `uniqueidentifier` paired with a number is named first, on both written sides, in
/// four constructs: arithmetic, two-argument `COALESCE`, the branches of a `CASE` and the
/// `=` comparison. [`common_type_clash`] lists them, and lists too the `ISNULL` shape that
/// reads the same pair the other way round, which no caller reaches here because this
/// engine answers those shapes with a row instead of a 206.
///
/// The other pairs are named in the order the caller passed them, because there the
/// constructs disagree and this signature says nothing about which one is asking: `SELECT
/// COALESCE(1, CAST('20010102' AS date));` and `SELECT COALESCE(CAST('20010102' AS date),
/// 1);` both name `int` before `date`, and the two branches of a `CASE` answer likewise,
/// whereas `SELECT CASE WHEN 1 = CAST('20010102' AS date) THEN 1 ELSE 0 END;` and its
/// reverse both name `date` first, as `SELECT 1 + CAST('20010102' AS date);` and its
/// reverse do. Caller order therefore serves one written order of each of those
/// constructs; the arithmetic order, which needs no such arbitration, belongs to
/// `arith::op_type` and its [`ordered_operand_type_clash`]. The `COALESCE` shapes caller
/// order reproduces, and the reversed ones it does not, are listed in the
/// `common_type_clash_keeps_caller_order` test of `tests/op_type.rs`.
pub fn implicit_result_type(a: &TypeInfo, b: &TypeInfo) -> SqlResult<TypeInfo> {
    if !pair_converts_implicitly(&a.ty, &b.ty) {
        return Err(common_type_clash(&a.ty, &b.ty));
    }

    let ty = common_type(&a.ty, &b.ty);
    let collation = if ty.is_string() {
        a.collation.or(b.collation)
    } else {
        None
    };
    Ok(TypeInfo {
        ty,
        nullable: a.nullable || b.nullable,
        collation,
    })
}

#[cfg(test)]
mod tests {
    use super::{implicit_result_type, numeric_precision_scale, operand_precision_scale, rank};
    use crate::sql_type::{Len, SqlType, TypeInfo};

    /// The 24 types of [`SqlType`] in the order of the data type precedence, strongest
    /// first.
    fn precedence_list() -> Vec<SqlType> {
        vec![
            SqlType::DateTimeOffset(7),
            SqlType::DateTime2(7),
            SqlType::DateTime,
            SqlType::SmallDateTime,
            SqlType::Date,
            SqlType::Time(7),
            SqlType::Float,
            SqlType::Real,
            SqlType::Decimal {
                precision: 5,
                scale: 2,
            },
            SqlType::Numeric {
                precision: 5,
                scale: 2,
            },
            SqlType::Money,
            SqlType::SmallMoney,
            SqlType::BigInt,
            SqlType::Int,
            SqlType::SmallInt,
            SqlType::TinyInt,
            SqlType::Bit,
            SqlType::UniqueIdentifier,
            SqlType::NVarChar(Len::Fixed(10)),
            SqlType::NChar(Len::Fixed(10)),
            SqlType::VarChar(Len::Fixed(10)),
            SqlType::Char(Len::Fixed(10)),
            SqlType::VarBinary(Len::Fixed(10)),
            SqlType::Binary(Len::Fixed(10)),
        ]
    }

    fn info(ty: SqlType) -> TypeInfo {
        TypeInfo::new(ty, false)
    }

    /// `rank` follows the published order, and the type of higher rank wins.
    ///
    /// The order is checked on the whole list rather than pair by pair: a variant added to
    /// `SqlType` without a rank breaks the count below.
    #[test]
    fn precedence_order() {
        let list = precedence_list();
        assert_eq!(list.len(), 24);

        for pair in list.windows(2) {
            let (strong, weak) = (&pair[0], &pair[1]);
            let ex_aequo = strong.is_exact_numeric() && weak.is_exact_numeric();
            if ex_aequo {
                assert_eq!(
                    rank(strong),
                    rank(weak),
                    "{} and {} must share a rank",
                    strong.name(),
                    weak.name()
                );
            } else {
                assert!(
                    rank(strong) > rank(weak),
                    "{} must outrank {}",
                    strong.name(),
                    weak.name()
                );
            }
        }

        // Every pair of the list, not only the adjacent ones.
        for (i, strong) in list.iter().enumerate() {
            for weak in &list[i + 1..] {
                assert!(rank(strong) >= rank(weak));
            }
        }

        let numeric = SqlType::Numeric {
            precision: 2,
            scale: 1,
        };
        let common = |a: SqlType, b: SqlType| {
            implicit_result_type(&info(a), &info(b))
                .expect("the pair has a common type")
                .ty
        };
        assert_eq!(common(SqlType::Int, numeric).name(), "numeric");
        assert_eq!(common(SqlType::Int, SqlType::Float), SqlType::Float);
        assert_eq!(common(SqlType::Int, SqlType::Money), SqlType::Money);
        assert_eq!(
            common(SqlType::VarChar(Len::Fixed(10)), SqlType::Int),
            SqlType::Int
        );
        assert_eq!(common(SqlType::Date, SqlType::DateTime), SqlType::DateTime);
    }

    /// An operand that is not `decimal` enters the table with the precision and the scale
    /// of its own type.
    #[test]
    fn non_decimal_types_have_the_precision_of_their_type() {
        assert_eq!(numeric_precision_scale(&SqlType::BigInt), Some((19, 0)));
        assert_eq!(numeric_precision_scale(&SqlType::Int), Some((10, 0)));
        assert_eq!(numeric_precision_scale(&SqlType::SmallInt), Some((5, 0)));
        assert_eq!(numeric_precision_scale(&SqlType::TinyInt), Some((3, 0)));
        assert_eq!(numeric_precision_scale(&SqlType::Money), Some((19, 4)));
        assert_eq!(numeric_precision_scale(&SqlType::SmallMoney), Some((10, 4)));
        assert_eq!(
            numeric_precision_scale(&SqlType::Numeric {
                precision: 5,
                scale: 2
            }),
            Some((5, 2))
        );
        assert_eq!(
            numeric_precision_scale(&SqlType::Decimal {
                precision: 38,
                scale: 38
            }),
            Some((38, 38))
        );

        assert_eq!(numeric_precision_scale(&SqlType::Float), None);
        assert_eq!(numeric_precision_scale(&SqlType::Real), None);
        assert_eq!(numeric_precision_scale(&SqlType::Date), None);
        assert_eq!(
            numeric_precision_scale(&SqlType::VarChar(Len::Fixed(10))),
            None
        );
        assert_eq!(numeric_precision_scale(&SqlType::UniqueIdentifier), None);
        // `bit` stays with the integers here, on `numeric(1, 0)`: the comparisons of a
        // `bit` with a `decimal(1,1)` answer a row, which the `decimal(1,1)` of the
        // unwidened common type would turn into 8115.
        assert_eq!(numeric_precision_scale(&SqlType::Bit), Some((1, 0)));
    }

    /// An operand whose declaration carries neither precision nor scale borrows the other
    /// operand's in the arithmetic table, and one that carries a precision keeps it.
    ///
    /// Vectors: `CAST('2' AS varchar(4)) * CAST(1.5 AS decimal(9,3))` is a `decimal(19,6)`
    /// and `CAST(0 AS bit) * CAST(0.1 AS decimal(1,1))` a `decimal(3,2)`, so both borrow;
    /// `CAST(3 AS int) * CAST(0.1 AS decimal(1,1))` is a `decimal(12,1)`, so `int` keeps its
    /// `numeric(10, 0)`. The `bit` line is where this function parts from
    /// [`numeric_precision_scale`], which keeps `Some((1, 0))` for the common type.
    #[test]
    fn an_operand_without_a_precision_borrows_the_other_one() {
        let nine_three = SqlType::Decimal {
            precision: 9,
            scale: 3,
        };
        let varchar = SqlType::VarChar(Len::Fixed(4));
        assert_eq!(operand_precision_scale(&varchar, &nine_three), Some((9, 3)));
        assert_eq!(
            operand_precision_scale(&SqlType::Bit, &nine_three),
            Some((9, 3))
        );
        assert_eq!(operand_precision_scale(&nine_three, &varchar), Some((9, 3)));
        assert_eq!(
            operand_precision_scale(&SqlType::Int, &nine_three),
            Some((10, 0))
        );
        // Two operands whose declarations carry neither: nothing to borrow.
        // `arith::op_type` does not reach that branch, since it refuses such a pair before
        // the table.
        assert_eq!(operand_precision_scale(&varchar, &SqlType::Bit), None);
        assert_eq!(operand_precision_scale(&SqlType::Bit, &varchar), None);
    }

    /// The precision saturates at 38 rather than overflowing.
    #[test]
    fn precision_saturates_at_thirty_eight() {
        let a = SqlType::Numeric {
            precision: 38,
            scale: 0,
        };
        let b = SqlType::Numeric {
            precision: 38,
            scale: 10,
        };
        assert_eq!(
            implicit_result_type(&info(a), &info(b))
                .expect("two numerics always have a common type")
                .ty,
            SqlType::Numeric {
                precision: 38,
                scale: 10
            }
        );
    }
}
