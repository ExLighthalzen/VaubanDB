//! [`binary_op_type`]: the type of a binary operation, precision and scale included.
//!
//! `precedence` answers "what is the common type of these two operands?"; this module
//! answers "what is the type of the *result* of this operator?", which is a different
//! question: the common type of `numeric(2, 1)` and `numeric(2, 1)` is `numeric(2, 1)`,
//! while their quotient is `numeric(8, 6)` and their product is `numeric(5, 2)`. The
//! answers come from the precision-and-scale table of T-SQL, which this module implements
//! verbatim, reduction rules included.
//!
//! No value is computed here: `arith::eval` consumes the [`TypeInfo`] this module returns.
//!
//! # Errors 402 and 8117: which of the two a refused pair raises
//!
//! There are two ways of refusing a pair of operands: 402, which names both operands
//! and the operator, or 8117, which names the left operand alone and the operator. The
//! same pair changes answer with the operator: `bit + bit` is 402 and `bit * bit` is
//! 8117.
//!
//! [`refusal`] holds the rule. In short:
//!
//! * `*` and `/` raise **8117** on the left operand;
//! * `+`, `-`, `%` and the bitwise operators raise **402**, except when the operator
//!   serves neither of the two types ([`serves`]), where the answer is 8117 on the left
//!   one: `date + date`, `float % float`, `decimal & decimal`, a `uniqueidentifier` with
//!   another type of its kind.
//!
//! `tests/op_type.rs` exercises the pairs above.

use vauban_errors::{SqlError, SqlResult};

use crate::arith::BinaryOp;
use crate::errors;
use crate::precedence::{
    implicit_result_type, operand_precision_scale, ordered_operand_type_clash, rank,
};
use crate::sql_type::{Len, SqlType, TypeFamily, TypeInfo};

/// The largest precision a `decimal` or a `numeric` can hold.
const MAX_PRECISION: i32 = 38;

/// The scale a division falls back on, and the floor of its computed scale.
///
/// `e1 / e2` has `s = max(6, s1 + p2 + 1)`, and an over-long product or quotient whose
/// integral part exceeds 32 digits is brought back to this scale.
const MIN_DIVISION_SCALE: i32 = 6;

/// The number of integral digits beyond which the reduction rules of a multiplication or
/// a division stop trying to keep the scale.
const WIDE_INTEGRAL_PART: i32 = 32;

/// The longest `char`, `varchar`, `binary` or `varbinary` a concatenation produces before
/// it saturates, in bytes.
const MAX_BYTES: u32 = 8000;

/// The longest `nchar` or `nvarchar` a concatenation produces, in characters (SQL Server
/// reports twice that many bytes as its `MaxLength`).
const MAX_CHARACTERS: u32 = 4000;

/// The type of the result of `op` applied to operands of types `a` and `b`.
///
/// The three families of operators answer to three different rules:
///
/// - **concatenation** ([`BinaryOp::Concat`], and [`BinaryOp::Add`] when both operands are
///   character or binary — the parser writes one `+` for both, and this function accepts
///   either spelling): the result is the strongest of the two character or binary types,
///   variable as soon as one operand is variable and Unicode as soon as one operand is,
///   and its declared length is the sum of the two, capped at 8000 characters (4000 for
///   the `N` types); a `max` operand gives a `max` result;
/// - **arithmetic** (`+`, `-`, `*`, `/`, `%`): the common type of the two operands
///   ([`implicit_result_type`]) decides the family, and an exact-numeric result then goes
///   through the precision-and-scale table. Two integers give the stronger integer type
///   (`int / int` is
///   an `int`: the division is integral), `money` against an integer stays `money`,
///   `money` against a `decimal` joins the table with `numeric(19, 4)` as its precision,
///   and an approximate operand carries everything to `float` (or to `real` when both
///   operands are `real`). Only `datetime` and `smalldatetime` accept `+` and `-` with a
///   number;
/// - **bitwise** (`&`, `|`, `^`): one operand is an integer or a `bit` and the other an
///   integer, a `bit`, a character or a binary type, and the result is the stronger of the
///   two (`bit & bit` stays `bit`, `binary & int` is an `int`).
///
/// The result is nullable as soon as one operand is, and carries the collation of the
/// character operand — the left one when both are character types.
///
/// # Errors
///
/// - **206**, when `date`, `time`, `datetime2` or `datetimeoffset` meets a number: those
///   four types convert to no number, and when two operands have no common type
///   (`uniqueidentifier` with a number);
/// - **257**, when a `datetime` or a `smalldatetime` meets a number under `*` or `/`: the
///   date is converted to the other operand rather than the other way round. Not under
///   `%`, which raises 402 on the pair, and not for the four temporal types that convert
///   to no number, which raise 206;
/// - **8117** and **402**, when the pair has no such operator: `bit * bit`,
///   `bit + bit`, `date + date`, `float % float`, a bitwise operator on a `float` or a
///   `decimal`, a concatenation of a character operand with a binary one. Which of the two
///   numbers is raised is the subject of the note at the top of this module.
///
/// # Examples
///
/// ```
/// use vauban_types::{BinaryOp, SqlType, TypeInfo, binary_op_type};
///
/// let int = TypeInfo::new(SqlType::Int, false);
/// let one_five = TypeInfo::new(SqlType::Numeric { precision: 2, scale: 1 }, false);
/// let sum = binary_op_type(BinaryOp::Add, &int, &one_five)?;
/// assert_eq!(sum.ty, SqlType::Numeric { precision: 12, scale: 1 });
/// # Ok::<(), vauban_errors::SqlError>(())
/// ```
pub fn binary_op_type(op: BinaryOp, a: &TypeInfo, b: &TypeInfo) -> SqlResult<TypeInfo> {
    let ty = match op {
        BinaryOp::Concat => concat_type(op, &a.ty, &b.ty)?,
        // The binder tells `Concat` from `Add`, but a `+` between two character or binary
        // operands is a concatenation whichever variant the caller chose.
        BinaryOp::Add if is_concatenable(&a.ty) && is_concatenable(&b.ty) => {
            concat_type(op, &a.ty, &b.ty)?
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            arithmetic_type(op, a, b)?
        }
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => bitwise_type(op, a, b)?,
    };

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

/// The name messages 402 and 8117 print for `op`.
///
/// The named operators are printed bare (`add operator`, `subtract operator`) and the
/// symbolic ones between apostrophes (`'&' operator`). `Concat` is the `+` token, so it
/// prints `add` like [`BinaryOp::Add`].
fn operator_name(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add | BinaryOp::Concat => "add",
        BinaryOp::Sub => "subtract",
        BinaryOp::Mul => "multiply",
        BinaryOp::Div => "divide",
        BinaryOp::Mod => "modulo",
        BinaryOp::BitAnd => "'&'",
        BinaryOp::BitOr => "'|'",
        BinaryOp::BitXor => "'^'",
    }
}

/// The error for a pair of operands `op` refuses: 402 naming both, or 8117 naming the
/// left one.
///
/// `alone` says that the pair is refused for the *type*'s sake and not for the pair's: both
/// operands belong to a family the operator does not serve at all. The callers compute it,
/// because what "the same family" means changes with the operator.
///
/// The rule, pair by pair (`tests/op_type.rs`):
///
/// | pair | `+` | `-` | `*` | `/` | `%` |
/// |---|---|---|---|---|---|
/// | `bit`, `bit` | 402 | 402 | 8117 | 8117 | 402 |
/// | `varchar`, `varchar` | concatenates | 402 | 8117 | 8117 | 402 |
/// | `varbinary`, `varbinary` | concatenates | | 8117 | | 402 |
/// | `uniqueidentifier`, `uniqueidentifier` | 8117 | 8117 | 8117 | | |
///
/// `SELECT CAST(1 AS bit) - CAST(1 AS bit);`, `… % …;`, `SELECT CAST('3' AS varchar(2)) /
/// CAST('1' AS varchar(2));`, `SELECT CAST(0x01 AS varbinary(2)) % CAST(0x02 AS
/// varbinary(2));` and `DECLARE @g uniqueidentifier = NEWID(); SELECT @g - @g;` are the
/// queries of the remaining cells.
fn refusal(op: BinaryOp, left: &SqlType, right: &SqlType, alone: bool) -> SqlError {
    if alone || matches!(op, BinaryOp::Mul | BinaryOp::Div) {
        errors::invalid_operand_type(left, operator_name(op))
    } else {
        errors::incompatible_types_for_operator(left, right, operator_name(op))
    }
}

/// Whether `ty` is an operand `+` concatenates rather than adds: a character or a binary
/// type.
fn is_concatenable(ty: &SqlType) -> bool {
    matches!(ty.family(), TypeFamily::Character | TypeFamily::Binary)
}

/// Whether `ty` is a variable-length type, whose concatenation stays variable-length.
///
/// `char(3) + char(4)` is a `char(7)` and `char(3) + varchar(4)` is a `varchar`: the result
/// is fixed when both operands are, and variable otherwise.
fn is_variable_length(ty: &SqlType) -> bool {
    matches!(
        ty,
        SqlType::VarChar(_) | SqlType::NVarChar(_) | SqlType::VarBinary(_)
    )
}

/// Whether `ty` is one of the two Unicode character types, which carry the whole
/// concatenation to `nchar` or `nvarchar`.
fn is_unicode(ty: &SqlType) -> bool {
    matches!(ty, SqlType::NChar(_) | SqlType::NVarChar(_))
}

/// The declared length of a character or binary type.
///
/// Every caller has already checked the family, so a type without a length — which cannot
/// reach here — reads as the empty length rather than as a panic.
fn length_of(ty: &SqlType) -> Len {
    match ty {
        SqlType::Char(len)
        | SqlType::VarChar(len)
        | SqlType::NChar(len)
        | SqlType::NVarChar(len)
        | SqlType::Binary(len)
        | SqlType::VarBinary(len) => *len,
        _ => Len::Fixed(0),
    }
}

/// The declared length of a concatenation: the sum of the two, saturated at `cap`.
///
/// A `max` operand gives a `max` result, whatever the other one is.
fn concat_length(a: Len, b: Len, cap: u32) -> Len {
    match (a, b) {
        (Len::Max, _) | (_, Len::Max) => Len::Max,
        (Len::Fixed(x), Len::Fixed(y)) => {
            let sum = u32::from(x).saturating_add(u32::from(y)).min(cap);
            // `sum` is at most `cap`, which is 8000 or 4000: the cast keeps its value.
            Len::Fixed(sum as u16)
        }
    }
}

/// The type of `a + b` when `+` concatenates.
///
/// Two character operands give a character result and two binary operands a binary one;
/// the two families do not mix.
fn concat_type(op: BinaryOp, a: &SqlType, b: &SqlType) -> SqlResult<SqlType> {
    let len = |cap| concat_length(length_of(a), length_of(b), cap);
    match (a.family(), b.family()) {
        (TypeFamily::Character, TypeFamily::Character) => {
            let variable = is_variable_length(a) || is_variable_length(b);
            Ok(match (is_unicode(a) || is_unicode(b), variable) {
                (true, true) => SqlType::NVarChar(len(MAX_CHARACTERS)),
                (true, false) => SqlType::NChar(len(MAX_CHARACTERS)),
                (false, true) => SqlType::VarChar(len(MAX_BYTES)),
                (false, false) => SqlType::Char(len(MAX_BYTES)),
            })
        }
        (TypeFamily::Binary, TypeFamily::Binary) => {
            if is_variable_length(a) || is_variable_length(b) {
                Ok(SqlType::VarBinary(len(MAX_BYTES)))
            } else {
                Ok(SqlType::Binary(len(MAX_BYTES)))
            }
        }
        // A character operand with a binary one, or a caller that asked for a
        // concatenation of two numbers: 402, naming both operands in the order the query
        // wrote them.
        _ => Err(refusal(op, a, b, false)),
    }
}

/// Whether `ty` is a date or time type that takes part in arithmetic at all.
///
/// Only `datetime` and `smalldatetime`, the two types stored as a day count, do:
/// `GETDATE() + 1` is a `datetime`, while `CAST('2000-01-01' AS date) + 1` is error 206 and
/// `date + date` error 8117.
fn accepts_arithmetic(ty: &SqlType) -> bool {
    matches!(ty, SqlType::DateTime | SqlType::SmallDateTime)
}

/// Whether `op` has a meaning for an operand of type `ty` read alone, before the other
/// operand is looked at.
///
/// This is the test [`refusal`] reads for its `alone` argument: a pair whose two types are
/// both outside the operator's own families answers 8117 on the left one, where a pair with
/// one type inside answers 402 on both. The families, over the ordered pairs of types
/// under `+ - * / % &` (`tests/op_type.rs`):
///
/// - `+` and `-` serve the families this crate knows but `uniqueidentifier` and the four
///   temporal types that take no arithmetic (`date + date` is 8117, `date + bit` is 402);
/// - `*` and `/` serve the same list minus the temporal types, `datetime` included
///   (`datetime * bit` is 8117, `datetime * int` 257);
/// - `%` serves the exact numbers, `bit`, the character and the binary types, and neither
///   `float`/`real` nor a temporal type nor a GUID (`float % float` is 8117, `float % int`
///   and `datetime % int` 402);
/// - the bitwise operators serve the integers, `bit`, the character and the binary types
///   (`decimal & decimal` is 8117, `float & int` 402).
fn serves(op: BinaryOp, ty: &SqlType) -> bool {
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Concat => match ty.family() {
            TypeFamily::DateTime => accepts_arithmetic(ty),
            TypeFamily::Guid => false,
            _ => true,
        },
        BinaryOp::Mul | BinaryOp::Div => {
            !matches!(ty.family(), TypeFamily::DateTime | TypeFamily::Guid)
        }
        BinaryOp::Mod => matches!(
            ty.family(),
            TypeFamily::Integer
                | TypeFamily::ExactNumeric
                | TypeFamily::Money
                | TypeFamily::Bit
                | TypeFamily::Character
                | TypeFamily::Binary
        ),
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => matches!(
            ty.family(),
            TypeFamily::Integer | TypeFamily::Bit | TypeFamily::Character | TypeFamily::Binary
        ),
    }
}

/// Whether an operand of type `ty` **carries** the arithmetic of `op`, as opposed to
/// converting to an operand that does.
///
/// A `bit`, a character and a binary operand have no arithmetic of their own: they borrow
/// the number facing them (`CAST('12' AS varchar(10)) * CAST(3 AS int)` is an `int`), and a
/// pair in which neither operand carries one is refused before the common type is
/// consulted (`bit * bit`, `varchar - varchar`). `float` and `real` carry `+ - * /` and
/// not `%`: `float % int` is 402.
fn carries_arithmetic(op: BinaryOp, ty: &SqlType) -> bool {
    match ty.family() {
        TypeFamily::Integer | TypeFamily::ExactNumeric | TypeFamily::Money => true,
        TypeFamily::ApproxNumeric => op != BinaryOp::Mod,
        TypeFamily::DateTime => {
            accepts_arithmetic(ty) && matches!(op, BinaryOp::Add | BinaryOp::Sub)
        }
        _ => false,
    }
}

/// The error a refused pair of non-temporal operands raises, 8117 or 402 by [`serves`].
fn pair_refusal(op: BinaryOp, a: &SqlType, b: &SqlType) -> SqlError {
    refusal(op, a, b, !serves(op, a) && !serves(op, b))
}

/// Whether `%` accepts an operand of type `ty` facing an operand of type `other`.
///
/// `%` is the operator whose domain depends on **both** types, which is why it has a test
/// of its own. A character or a binary operand is accepted opposite an integer and refused
/// opposite anything else (`CAST(5 AS int) % CAST('12' AS varchar(10))` and its reverse
/// answer a row, while `CAST(1.5 AS decimal(2,1)) % CAST('12' AS varchar(10))`, `money %
/// varchar`, `bit % varchar` and `varchar % varchar` raise 402), and a `bit` is accepted
/// opposite a number and refused elsewhere (`bit % int`, `bit % money` and `bit %
/// decimal(9,3)` answer a row, `bit % bit` and `bit % varchar` raise 402).
fn mod_accepts(ty: &SqlType, other: &SqlType) -> bool {
    match ty.family() {
        TypeFamily::Integer | TypeFamily::ExactNumeric | TypeFamily::Money => true,
        TypeFamily::Bit => matches!(
            other.family(),
            TypeFamily::Integer | TypeFamily::ExactNumeric | TypeFamily::Money
        ),
        TypeFamily::Character | TypeFamily::Binary => other.family() == TypeFamily::Integer,
        _ => false,
    }
}

/// The type of `a op b` when at least one operand is a date or time type.
fn temporal_arithmetic(op: BinaryOp, a: &TypeInfo, b: &TypeInfo) -> SqlResult<SqlType> {
    let temporal = |t: &TypeInfo| t.ty.family() == TypeFamily::DateTime;
    // `datetime` and `smalldatetime` take part in `+` and `-`, and in nothing else.
    let takes_part = |t: &TypeInfo| {
        !temporal(t) || (matches!(op, BinaryOp::Add | BinaryOp::Sub) && accepts_arithmetic(&t.ty))
    };

    if takes_part(a) && takes_part(b) {
        // `datetime` and `smalldatetime` keep their own type and outrank the numbers, so
        // the common type *is* the result: `smalldatetime + 1` is a `smalldatetime`, and
        // `smalldatetime + datetime` a `datetime`. The other operand still has to be one
        // the operator serves: `datetime + NEWID()` raises 402 naming both.
        if serves(op, &a.ty) && serves(op, &b.ty) {
            return Ok(implicit_result_type(a, b)?.ty);
        }
        return Err(pair_refusal(op, &a.ty, &b.ty));
    }

    // One operand is a temporal type this operator does not serve. When the *other* is a
    // number, the error reports the conversion that would have had to be made rather than
    // the operator: 257 for `datetime` and `smalldatetime`, which do convert to a number
    // (`datetime * int`), 206 for `date`, `time`, `datetime2` and `datetimeoffset`, which
    // have no conversion towards a number (`date * int`). `%` reports neither: it raises
    // 402 on the pair (`datetime % int`).
    let (bad, other) = if temporal(a) && !takes_part(a) {
        (a, b)
    } else {
        (b, a)
    };
    if !temporal(other) && carries_arithmetic(op, &other.ty) && op != BinaryOp::Mod {
        // `accepts_arithmetic` names the same two types as the conversion chart: `datetime`
        // and `smalldatetime` are exactly the temporal types that convert to a number.
        return Err(if accepts_arithmetic(&bad.ty) {
            errors::implicit_conversion_not_allowed(&bad.ty, &other.ty)
        } else {
            // Keep the temporal name first (`ordered_operand_type_clash` lists the numeric
            // types it covers).
            ordered_operand_type_clash(&a.ty, &b.ty)
        });
    }
    // Everything else is the operator refusing the pair: 8117 on the left operand when the
    // operator serves neither type (`date + date`, `datetime * datetime`, `date % float`),
    // 402 on both when it serves one (`date + bit`, `date + varbinary`, `datetime % int`).
    Err(pair_refusal(op, &a.ty, &b.ty))
}

/// The type of `a op b` for `+`, `-`, `*`, `/` and `%`.
fn arithmetic_type(op: BinaryOp, a: &TypeInfo, b: &TypeInfo) -> SqlResult<SqlType> {
    if a.ty.family() == TypeFamily::DateTime || b.ty.family() == TypeFamily::DateTime {
        return temporal_arithmetic(op, a, b);
    }

    // Neither operand carries the arithmetic: the pair is refused before the common type
    // is consulted. This is where `bit + bit`, `varchar * varchar` and `bit + NEWID()`
    // stop; the GUID pairs would otherwise raise 206, where the answer is 402 on `+`, `-`
    // and `%` and 8117 on `*` and `/`.
    if !carries_arithmetic(op, &a.ty) && !carries_arithmetic(op, &b.ty) {
        return Err(pair_refusal(op, &a.ty, &b.ty));
    }
    // `%` alone refuses pairs whose common type exists: a character or a binary operand
    // opposite anything but an integer, a `bit` opposite anything but a number
    // ([`mod_accepts`]).
    if op == BinaryOp::Mod && !(mod_accepts(&a.ty, &b.ty) && mod_accepts(&b.ty, &a.ty)) {
        return Err(pair_refusal(op, &a.ty, &b.ty));
    }

    // The common type decides the family of the result. It also raises 206 for a pair with
    // no common type (`uniqueidentifier` with a number), naming the GUID first, so
    // this path reorders nothing itself.
    let common = implicit_result_type(a, b)?.ty;
    match common.family() {
        // Two integers give the stronger integer type, and the division is integral:
        // `7 / 2` is the `int` 3, and the small integers are *not* promoted to `int`.
        TypeFamily::Integer => Ok(common),
        // `money` against an integer, a `bit` or a `money` stays in the money family and
        // keeps the stronger of the two types (`smallmoney + int` is a `smallmoney`). A
        // `decimal` operand outranks `money`, so that pair never lands here: it goes
        // through the table below with `numeric(19, 4)` for the money operand.
        TypeFamily::Money => Ok(common),
        // One approximate operand carries the result: `real + real` is a `real`,
        // `real + float` and `float * numeric` are `float`. `%` stops above, at
        // `carries_arithmetic`.
        TypeFamily::ApproxNumeric => Ok(common),
        TypeFamily::ExactNumeric => Ok(exact_numeric_type(op, &a.ty, &b.ty, &common)),
        // A pair whose common type has no arithmetic and whose operands got past
        // `carries_arithmetic` above: no known pair reaches this arm, which keeps the
        // refusal of the operator.
        TypeFamily::Bit
        | TypeFamily::Character
        | TypeFamily::Binary
        | TypeFamily::Guid
        | TypeFamily::DateTime => Err(pair_refusal(op, &a.ty, &b.ty)),
    }
}

/// `precision` and `scale`, spelled like `common`.
///
/// `decimal * decimal` reports `decimal` and `decimal * numeric` reports `numeric`, which
/// is exactly the rule `precedence` already applied to `common`.
fn spelled_like(common: &SqlType, precision: u8, scale: u8) -> SqlType {
    if matches!(common, SqlType::Numeric { .. }) {
        SqlType::Numeric { precision, scale }
    } else {
        SqlType::Decimal { precision, scale }
    }
}

/// The exact-numeric result of `op`, from the precision-and-scale table.
///
/// With `e1` of precision `p1` and scale `s1` and `e2` of precision `p2` and scale `s2`.
/// An operand that is not `decimal` but declares a precision enters with the precision and
/// the scale of its own type, so `int` is `numeric(10, 0)` and `money` is `numeric(19, 4)`;
/// against a `decimal` or a `numeric`, an operand whose declaration carries neither, a
/// character type or a `bit`, enters with those of the *other* operand
/// ([`operand_precision_scale`]: `CAST('2' AS varchar(4)) * CAST(1.5 AS decimal(5,2))` is
/// a `decimal(11,4)`, the product of two `decimal(5,2)`). That borrowing is the table's
/// reading and not the common type of the pair, which
/// `precedence::numeric_precision_scale` answers on its own terms for a `bit`.
///
/// | operation | precision | scale |
/// |---|---|---|
/// | `e1 + e2`, `e1 - e2` | `max(s1, s2) + max(p1 - s1, p2 - s2) + 1` | `max(s1, s2)` |
/// | `e1 * e2` | `p1 + p2 + 1` | `s1 + s2` |
/// | `e1 / e2` | `p1 - s1 + s2 + max(6, s1 + p2 + 1)` | `max(6, s1 + p2 + 1)` |
/// | `e1 % e2` | `min(p1 - s1, p2 - s2) + max(s1, s2)` | `max(s1, s2)` |
///
/// A precision over 38 is then brought back by [`reduce`].
///
/// `common` gives the spelling, `decimal` or `numeric`. It also stays the answer for a pair
/// in which neither operand declares a precision, a branch the callers here do not reach: an
/// exact-numeric common type comes from a `decimal` or `numeric` operand, which lends its
/// precision to the other side, and a pair of two operands without a precision is refused
/// before the table (`CAST('2' AS varchar(4)) * CAST(1 AS bit)` is 8117 naming `varchar`,
/// and `arithmetic_type` refuses that pair on its `bit` common type).
fn exact_numeric_type(op: BinaryOp, a: &SqlType, b: &SqlType, common: &SqlType) -> SqlType {
    let (Some((p1, s1)), Some((p2, s2))) =
        (operand_precision_scale(a, b), operand_precision_scale(b, a))
    else {
        return *common;
    };
    let (p1, s1) = (i32::from(p1), i32::from(s1));
    let (p2, s2) = (i32::from(p2), i32::from(s2));
    let integral = (p1 - s1).max(p2 - s2);

    let (precision, scale) = match op {
        BinaryOp::Add | BinaryOp::Sub => {
            let scale = s1.max(s2);
            (scale + integral + 1, scale)
        }
        BinaryOp::Mul => (p1 + p2 + 1, s1 + s2),
        BinaryOp::Div => {
            let scale = MIN_DIVISION_SCALE.max(s1 + p2 + 1);
            (p1 - s1 + s2 + scale, scale)
        }
        BinaryOp::Mod => {
            let scale = s1.max(s2);
            ((p1 - s1).min(p2 - s2) + scale, scale)
        }
        // The caller never routes a bitwise operator or a concatenation here.
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Concat => {
            return *common;
        }
    };

    let (precision, scale) = reduce(op, precision, scale, integral);
    spelled_like(common, precision, scale)
}

/// The reduction rules, applied when the computed `precision` exceeds 38.
///
/// The result precision and scale have an absolute maximum of 38. When a result precision
/// is greater than 38, it is reduced to 38, and the corresponding scale is reduced to try
/// to prevent truncating the integral part of the result.
///
/// - addition and subtraction: the integral part needs `max(p1 - s1, p2 - s2)` digits, so
///   the scale becomes `min(precision, 38) - max(p1 - s1, p2 - s2)`;
/// - multiplication and division: the integral part needs `precision - scale` digits, and
///   the scale becomes `min(scale, 38 - (precision - scale))` when that integral part is
///   below 32 digits; it is left alone when it is already below 6, and brought back to 6
///   when it is above 6 and the integral part exceeds 32 digits.
///
/// Two worked examples: `decimal(30, 20) * decimal(30, 20)` would need `numeric(61, 40)`
/// and gives `decimal(38, 17)`; `decimal(30, 10) * decimal(30, 10)` gives `decimal(38, 6)`.
///
/// `integral` is `max(p1 - s1, p2 - s2)`, the integral part the two *operands* need, which
/// is what the addition rule reduces against.
fn reduce(op: BinaryOp, precision: i32, scale: i32, integral: i32) -> (u8, u8) {
    let scale = if precision <= MAX_PRECISION {
        scale
    } else {
        match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mod => MAX_PRECISION - integral,
            _ => {
                let result_integral = precision - scale;
                if result_integral < WIDE_INTEGRAL_PART {
                    scale.min(MAX_PRECISION - result_integral)
                } else if scale > MIN_DIVISION_SCALE {
                    MIN_DIVISION_SCALE
                } else {
                    scale
                }
            }
        }
    };
    let precision = precision.clamp(1, MAX_PRECISION);
    let scale = scale.clamp(0, precision);
    // Both are within `1..=38` now, so the casts keep their values.
    (precision as u8, scale as u8)
}

/// The type of `a & b`, `a | b` or `a ^ b`.
///
/// One operand is an integer or a `bit`, the other an integer, a `bit`, a character or a
/// binary type, and the result is the stronger of the two: `bit & bit` stays a `bit`,
/// `bit & int` and `int & bigint` give `int` and `bigint`.
///
/// The character and binary operands convert to the integer facing them, and that integer
/// is the result: `CAST(0x0F AS binary(1)) & CAST(255 AS int)` is the `int` 15 and
/// `CAST(1 AS bit) & CAST('12' AS varchar(4))` the `bit` 1. Two of them together are
/// refused, in the four combinations: `binary & binary`, `varchar & varchar`, `binary &
/// varchar` and its reverse raise 402, so the pair needs an integer or a `bit` on one
/// side.
///
/// # Errors
///
/// 402 naming both operands when the operator serves at least one of the two types, 8117
/// naming the left one when it serves neither: `float & int` is 402 naming both while
/// `decimal & decimal` and `decimal & money` are 8117 naming `decimal`.
fn bitwise_type(op: BinaryOp, a: &TypeInfo, b: &TypeInfo) -> SqlResult<SqlType> {
    let integral = |ty: &SqlType| matches!(ty.family(), TypeFamily::Bit | TypeFamily::Integer);
    let accepted = (integral(&a.ty) || integral(&b.ty)) && serves(op, &a.ty) && serves(op, &b.ty);
    if !accepted {
        return Err(pair_refusal(op, &a.ty, &b.ty));
    }
    // The integer or `bit` operand outranks the character and binary types, so the stronger
    // of the two is the one the other converts to.
    let stronger = if rank(&a.ty) >= rank(&b.ty) {
        a.ty
    } else {
        b.ty
    };
    Ok(stronger)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(ty: SqlType) -> TypeInfo {
        TypeInfo::new(ty, false)
    }

    fn num(precision: u8, scale: u8) -> SqlType {
        SqlType::Numeric { precision, scale }
    }

    fn dec(precision: u8, scale: u8) -> SqlType {
        SqlType::Decimal { precision, scale }
    }

    fn ty_of(op: BinaryOp, a: SqlType, b: SqlType) -> SqlType {
        binary_op_type(op, &info(a), &info(b))
            .expect("the pair has a result type")
            .ty
    }

    /// The four lines of the precision-and-scale table, on two sample operands.
    #[test]
    fn precision_and_scale_table_lines() {
        assert_eq!(ty_of(BinaryOp::Mul, num(2, 1), num(3, 2)), num(6, 3));
        assert_eq!(ty_of(BinaryOp::Add, num(2, 1), num(3, 2)), num(4, 2));
        assert_eq!(ty_of(BinaryOp::Sub, num(2, 1), num(3, 2)), num(4, 2));
        assert_eq!(ty_of(BinaryOp::Div, num(2, 1), num(2, 1)), num(8, 6));
        assert_eq!(ty_of(BinaryOp::Mod, num(5, 2), num(3, 1)), num(4, 2));
    }

    /// The spelling follows the operands: `decimal` unless one of them is a `numeric`.
    #[test]
    fn spelling_follows_the_operands() {
        assert_eq!(ty_of(BinaryOp::Mul, dec(5, 2), dec(5, 2)), dec(11, 4));
        assert_eq!(ty_of(BinaryOp::Mul, dec(5, 2), num(5, 2)), num(11, 4));
    }

    /// A `varchar` operand converts implicitly and enters the table with the precision and
    /// the scale of the other operand.
    ///
    /// Vectors: `CAST('2' AS varchar(4)) + CAST(1.5 AS decimal(9,3))` is a `decimal(10,3)`,
    /// the sum of two `decimal(9,3)`, where the common type of the pair would give
    /// `decimal(9,3)`; the product of that pair is a `decimal(19,6)`, against
    /// `decimal(9,3)` for the common type.
    #[test]
    fn an_operand_without_precision_borrows_the_other_one() {
        let varchar = SqlType::VarChar(Len::Fixed(4));
        assert_eq!(ty_of(BinaryOp::Add, varchar, dec(9, 3)), dec(10, 3));
        assert_eq!(ty_of(BinaryOp::Mul, varchar, dec(9, 3)), dec(19, 6));
        // The written order leaves the sum alone.
        assert_eq!(ty_of(BinaryOp::Add, dec(9, 3), varchar), dec(10, 3));
    }

    /// `reduce` never returns a precision outside `1..=38` nor a scale above it.
    #[test]
    fn reduction_stays_inside_the_declared_range() {
        for op in [
            BinaryOp::Add,
            BinaryOp::Sub,
            BinaryOp::Mul,
            BinaryOp::Div,
            BinaryOp::Mod,
        ] {
            for p1 in [1u8, 2, 10, 20, 30, 38] {
                for s1 in [0u8, 1, 6, 10, 20, 38] {
                    if s1 > p1 {
                        continue;
                    }
                    for p2 in [1u8, 3, 19, 38] {
                        for s2 in [0u8, 4, 17, 38] {
                            if s2 > p2 {
                                continue;
                            }
                            let ty = ty_of(op, num(p1, s1), num(p2, s2));
                            let SqlType::Numeric { precision, scale } = ty else {
                                unreachable!("two numerics give a numeric, got {ty:?}")
                            };
                            assert!((1..=38).contains(&precision), "{op:?} {ty:?}");
                            assert!(scale <= precision, "{op:?} {ty:?}");
                        }
                    }
                }
            }
        }
    }
}
