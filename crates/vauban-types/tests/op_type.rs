//! `binary_op_type`: the type of a binary operation, from the public API of the crate.
//!
//! The vectors below are lines of the precision-and-scale table, and the pairs of types
//! each operator accepts or refuses.

use vauban_types::{BinaryOp, Collation, Len, SqlType, TypeInfo, binary_op_type};

/// A non-nullable operand of type `ty`.
fn info(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, false)
}

fn num(precision: u8, scale: u8) -> SqlType {
    SqlType::Numeric { precision, scale }
}

fn dec(precision: u8, scale: u8) -> SqlType {
    SqlType::Decimal { precision, scale }
}

/// The type of `a op b`, which the vector expects to succeed.
fn ty_of(op: BinaryOp, a: SqlType, b: SqlType) -> SqlType {
    binary_op_type(op, &info(a), &info(b))
        .unwrap_or_else(|e| panic!("{a:?} {op:?} {b:?} must have a type, got {}", e.message))
        .ty
}

/// The error number of `a op b`, which the vector expects to fail.
fn err_of(op: BinaryOp, a: SqlType, b: SqlType) -> u32 {
    refusal_of(op, a, b).0
}

/// The number and the message of `a op b`, which the vector expects to fail.
fn refusal_of(op: BinaryOp, a: SqlType, b: SqlType) -> (u32, String) {
    match binary_op_type(op, &info(a), &info(b)) {
        Ok(t) => panic!("{a:?} {op:?} {b:?} must be refused, got {:?}", t.ty),
        Err(e) => (e.number, e.message),
    }
}

/// The four lines of the precision-and-scale table, and the `int` operand that enters it
/// as `numeric(10, 0)`.
#[test]
fn decimal_arithmetic_types() {
    assert_eq!(ty_of(BinaryOp::Mul, num(2, 1), num(3, 2)), num(6, 3));
    assert_eq!(ty_of(BinaryOp::Add, num(2, 1), num(3, 2)), num(4, 2));
    assert_eq!(ty_of(BinaryOp::Sub, num(2, 1), num(3, 2)), num(4, 2));
    assert_eq!(ty_of(BinaryOp::Div, num(2, 1), num(2, 1)), num(8, 6));
    assert_eq!(ty_of(BinaryOp::Mod, num(5, 2), num(3, 1)), num(4, 2));

    // `CAST(1 AS int) + CAST(1.5 AS numeric(2,1))` is a
    // numeric(12, 1). `int` counts as numeric(10, 0), so s = max(0, 1) = 1 and
    // p = 1 + max(10, 1) + 1 = 12.
    assert_eq!(ty_of(BinaryOp::Add, SqlType::Int, num(2, 1)), num(12, 1));
    // `tinyint` counts as numeric(3, 0), which gives
    // numeric(5, 1). The `bit` answers numeric(3, 1) here under both readings of its
    // precision — its own numeric(1, 0) and the numeric(2, 1) of the other operand — so
    // this line is a regression guard, not a vector; `precision_of_an_operand_without_one`
    // below holds the shapes that separate the two.
    assert_eq!(ty_of(BinaryOp::Add, SqlType::Bit, num(2, 1)), num(3, 1));
    assert_eq!(ty_of(BinaryOp::Add, SqlType::TinyInt, num(2, 1)), num(5, 1));
    // `money` counts as numeric(19, 4), so the pair leaves the
    // money family for numeric(20, 4) and numeric(22, 5).
    assert_eq!(ty_of(BinaryOp::Add, SqlType::Money, num(2, 1)), num(20, 4));
    assert_eq!(ty_of(BinaryOp::Mul, SqlType::Money, num(2, 1)), num(22, 5));

    // Two `decimal` operands report `decimal`, a
    // mixed pair reports `numeric`.
    assert_eq!(ty_of(BinaryOp::Mul, dec(5, 2), dec(5, 2)), dec(11, 4));
    assert_eq!(ty_of(BinaryOp::Mul, dec(5, 2), num(5, 2)), num(11, 4));
}

/// The precision an operand whose type declares neither brings to the table.
///
/// A character operand and a `bit` operand enter the table with the precision **and** the
/// scale of the other operand, so each line below is the line of the table for two
/// operands of that other type.
#[test]
fn precision_of_an_operand_without_one() {
    let varchar = SqlType::VarChar(Len::Fixed(4));

    // `varchar(4)` against a decimal, one operator per line: the products of `decimal(5,2)
    // * decimal(5,2)` and `decimal(9,3) * decimal(9,3)`, their sum and difference, their
    // quotient. The competing reading, the common type of the pair, answers `decimal(5,2)`
    // and `decimal(9,3)` to the whole list.
    assert_eq!(ty_of(BinaryOp::Mul, varchar, dec(5, 2)), dec(11, 4));
    assert_eq!(ty_of(BinaryOp::Mul, varchar, dec(9, 3)), dec(19, 6));
    assert_eq!(ty_of(BinaryOp::Add, varchar, dec(9, 3)), dec(10, 3));
    assert_eq!(ty_of(BinaryOp::Sub, varchar, dec(9, 3)), dec(10, 3));
    assert_eq!(ty_of(BinaryOp::Div, varchar, dec(9, 3)), dec(22, 13));

    // `decimal(1,1)`, which keeps zero integral digit, separates the borrowed precision from
    // the widened common type `decimal(2,1)`: the product is a `decimal(3,2)` and not a
    // `decimal(5,2)`, the quotient a `decimal(7,6)`.
    assert_eq!(ty_of(BinaryOp::Mul, varchar, dec(1, 1)), dec(3, 2));
    assert_eq!(ty_of(BinaryOp::Div, varchar, dec(1, 1)), dec(7, 6));

    // The reduction rules still apply above 38: `decimal(38,10) * decimal(38,10)` would
    // need 77 digits, and the integral part of the product exceeds 32, so the scale falls
    // back to 6.
    assert_eq!(ty_of(BinaryOp::Mul, varchar, dec(38, 10)), dec(38, 6));

    // The shape of the character operand leaves that product alone: declared length 30,
    // fixed length, Unicode and `max` answer `decimal(19,6)` like `varchar(4)`.
    for character in [
        SqlType::VarChar(Len::Fixed(30)),
        SqlType::Char(Len::Fixed(4)),
        SqlType::NVarChar(Len::Fixed(4)),
        SqlType::VarChar(Len::Max),
    ] {
        assert_eq!(
            ty_of(BinaryOp::Mul, character, dec(9, 3)),
            dec(19, 6),
            "{character:?}"
        );
    }
    // The spelling follows the decimal operand, as it does for two numeric operands.
    assert_eq!(ty_of(BinaryOp::Mul, varchar, num(9, 3)), num(19, 6));

    // A `bit` operand borrows likewise, on five operators: `decimal(2,1)` and
    // `decimal(9,3)` under `*`, then `+`, `/` and `%` against `decimal(9,3)`, and
    // `decimal(1,1)` under `*`. Under its own `numeric(1, 0)` the five answers would be
    // `decimal(4,1)`, `decimal(11,3)`, `decimal(10,3)`, `decimal(14,10)` and
    // `decimal(4,3)`.
    assert_eq!(ty_of(BinaryOp::Mul, SqlType::Bit, dec(2, 1)), dec(5, 2));
    assert_eq!(ty_of(BinaryOp::Mul, SqlType::Bit, dec(9, 3)), dec(19, 6));
    assert_eq!(ty_of(BinaryOp::Add, SqlType::Bit, dec(9, 3)), dec(10, 3));
    assert_eq!(ty_of(BinaryOp::Div, SqlType::Bit, dec(9, 3)), dec(22, 13));
    assert_eq!(ty_of(BinaryOp::Mod, SqlType::Bit, dec(9, 3)), dec(9, 3));
    assert_eq!(ty_of(BinaryOp::Mul, SqlType::Bit, dec(1, 1)), dec(3, 2));

    // Controls: an operand whose type does declare a precision keeps it. `int` enters as
    // numeric(10, 0), `tinyint` as numeric(3, 0) and `money` as numeric(19, 4). Borrowing
    // would give `decimal(3,2)`, `decimal(19,6)` and `decimal(19,6)` instead.
    assert_eq!(ty_of(BinaryOp::Mul, SqlType::Int, dec(1, 1)), dec(12, 1));
    assert_eq!(
        ty_of(BinaryOp::Mul, SqlType::TinyInt, dec(9, 3)),
        dec(13, 3)
    );
    assert_eq!(ty_of(BinaryOp::Mul, SqlType::Money, dec(9, 3)), dec(29, 7));

    // The common type of the pair, which `COALESCE` reads and which the products above are
    // not: `decimal(9,3)` is returned unchanged against a character operand, while `int`
    // widens `decimal(1,1)` to `decimal(11,1)`. The `bit` line of this common type is a
    // deliberate difference from SQL Server, which gives `decimal(1,1)` for
    // `COALESCE(CAST(0 AS bit), CAST(0.1 AS decimal(1,1)))`: the `decimal(2,1)` asserted
    // here is what lets six comparisons of a `bit` worth 1 with a `decimal(1,1)` or a
    // `decimal(5,5)` answer a row instead of the 8115 that conversion to `decimal(1,1)`
    // raises. Separating the two readings needs a second function chosen by the caller,
    // in `binder`.
    let common = |a: SqlType, b: SqlType| {
        vauban_types::implicit_result_type(&info(a), &info(b))
            .unwrap_or_else(|e| panic!("{a:?} and {b:?} have a common type: {}", e.message))
            .ty
    };
    assert_eq!(common(varchar, dec(9, 3)), dec(9, 3));
    assert_eq!(common(SqlType::Bit, dec(1, 1)), dec(2, 1));
    assert_eq!(common(SqlType::Int, dec(1, 1)), dec(11, 1));

    // Out of the exact-numeric table: a `bit` against an `int` is an `int` and against
    // `money` a `money`, and `datetime` and `smalldatetime` keep their own type under `+`.
    assert_eq!(
        ty_of(BinaryOp::Mul, SqlType::Bit, SqlType::Int),
        SqlType::Int
    );
    assert_eq!(
        ty_of(BinaryOp::Mul, SqlType::Bit, SqlType::Money),
        SqlType::Money
    );
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::DateTime, dec(9, 3)),
        SqlType::DateTime
    );
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::SmallDateTime, dec(9, 3)),
        SqlType::SmallDateTime
    );
}

/// Two worked examples of the reduction: a precision over 38 is brought back to 38 and
/// the scale is reduced with it.
#[test]
fn decimal_overflow_reduction() {
    // p = 61 and s = 40; the integral part needs 21 digits, below 32, so
    // s = min(40, 38 - 21) = 17.
    assert_eq!(ty_of(BinaryOp::Mul, dec(30, 20), dec(30, 20)), dec(38, 17));
    // p = 61 and s = 20; the integral part needs 41 digits, above 32, and the scale is
    // above 6, so it is brought back to 6.
    assert_eq!(ty_of(BinaryOp::Mul, dec(30, 10), dec(30, 10)), dec(38, 6));

    // Addition: the scale is reduced to leave room for max(p1 - s1, p2 - s2) digits.
    assert_eq!(ty_of(BinaryOp::Add, num(38, 10), num(38, 30)), num(38, 10));
    // Division: s = max(6, 20 + 38 + 1) = 59 and p = 38 - 20 + 30 + 59, well over 38.
    let quotient = ty_of(BinaryOp::Div, num(38, 20), num(38, 30));
    let SqlType::Numeric { precision, scale } = quotient else {
        panic!("a quotient of two numerics is a numeric, got {quotient:?}")
    };
    assert_eq!(precision, 38);
    assert!(scale <= 38, "{quotient:?}");
}

/// Integers, `money` and the bitwise operators, whose results are types rather than
/// precisions.
#[test]
fn integer_and_money_types() {
    // `int / int` stays `int` (the division is integral) and the
    // stronger integer type wins.
    assert_eq!(
        ty_of(BinaryOp::Div, SqlType::Int, SqlType::Int),
        SqlType::Int
    );
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::Int, SqlType::BigInt),
        SqlType::BigInt
    );
    // The small integers are **not** promoted to `int`.
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::TinyInt, SqlType::TinyInt),
        SqlType::TinyInt
    );
    assert_eq!(
        ty_of(BinaryOp::Mul, SqlType::SmallInt, SqlType::SmallInt),
        SqlType::SmallInt
    );

    // The money family keeps the stronger of the two types.
    assert_eq!(
        ty_of(BinaryOp::Div, SqlType::Money, SqlType::Int),
        SqlType::Money
    );
    assert_eq!(
        ty_of(BinaryOp::Div, SqlType::Money, SqlType::Money),
        SqlType::Money
    );
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::SmallMoney, SqlType::Int),
        SqlType::SmallMoney
    );
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::SmallMoney, SqlType::Money),
        SqlType::Money
    );

    // An approximate operand wins, and
    // two `real` operands stay `real`.
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::Real, SqlType::Real),
        SqlType::Real
    );
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::Real, SqlType::Float),
        SqlType::Float
    );
    assert_eq!(
        ty_of(BinaryOp::Mul, SqlType::Float, num(2, 1)),
        SqlType::Float
    );

    // The bitwise operators on integers and bits.
    assert_eq!(
        ty_of(BinaryOp::BitAnd, SqlType::Int, SqlType::Int),
        SqlType::Int
    );
    assert_eq!(
        ty_of(BinaryOp::BitOr, SqlType::Int, SqlType::BigInt),
        SqlType::BigInt
    );
    assert_eq!(
        ty_of(BinaryOp::BitAnd, SqlType::Bit, SqlType::Bit),
        SqlType::Bit
    );
    assert_eq!(
        ty_of(BinaryOp::BitXor, SqlType::Bit, SqlType::Int),
        SqlType::Int
    );

    // SQL Server refuses a bitwise operator on an approximate or an
    // exact-numeric operand, with 402, which names both operands in the order the query
    // wrote them.
    assert_eq!(
        refusal_of(BinaryOp::BitAnd, SqlType::Float, SqlType::Int),
        (
            402,
            "The types float and int cannot be combined by the '&' operator.".to_owned()
        )
    );
    // `SELECT CAST(1.5 AS numeric(2,1)) & 1;` and its `decimal` twin: 402 is the one
    // message that prints the *declared* spelling.
    assert_eq!(
        refusal_of(BinaryOp::BitAnd, num(5, 2), SqlType::Int),
        (
            402,
            "The types numeric and int cannot be combined by the '&' operator.".to_owned()
        )
    );
    assert_eq!(
        refusal_of(BinaryOp::BitAnd, dec(5, 2), SqlType::Int),
        (
            402,
            "The types decimal and int cannot be combined by the '&' operator.".to_owned()
        )
    );
    // A binary or a character operand converts to the integer
    // facing it, and that integer is the result: `CAST(0x0F AS binary(1)) & CAST(255 AS
    // int)` is the `int` 15. Two of them together are refused: `binary & binary` and
    // `varchar & varchar` answer 402.
    assert_eq!(
        ty_of(
            BinaryOp::BitAnd,
            SqlType::VarBinary(Len::Fixed(4)),
            SqlType::Int
        ),
        SqlType::Int
    );
    assert_eq!(
        ty_of(
            BinaryOp::BitAnd,
            SqlType::VarChar(Len::Fixed(4)),
            SqlType::Bit
        ),
        SqlType::Bit
    );
    assert_eq!(
        refusal_of(
            BinaryOp::BitAnd,
            SqlType::Binary(Len::Fixed(1)),
            SqlType::Binary(Len::Fixed(1))
        ),
        (
            402,
            "The types binary and binary cannot be combined by the '&' operator.".to_owned()
        )
    );
    // When the operator serves neither type, the answer is 8117 on the left operand
    // instead of 402 on both. SQL Server names `decimal` there, with the declared
    // spelling, where `errors::invalid_operand_type` writes `numeric` for both spellings,
    // a deliberate difference: this vector fixes the number and the operand, not the
    // spelling.
    assert_eq!(err_of(BinaryOp::BitAnd, dec(5, 2), dec(5, 2)), 8117);
    assert_eq!(
        refusal_of(BinaryOp::BitAnd, num(5, 2), SqlType::Money),
        (
            8117,
            "Data type numeric is not accepted by the '&' operator.".to_owned()
        )
    );

    // A binary or character operand
    // converts implicitly and the number's type wins.
    assert_eq!(
        ty_of(
            BinaryOp::Add,
            SqlType::VarBinary(Len::Fixed(2)),
            SqlType::Int
        ),
        SqlType::Int
    );
    assert_eq!(
        ty_of(BinaryOp::Sub, SqlType::VarChar(Len::Fixed(4)), SqlType::Int),
        SqlType::Int
    );
}

/// `bit + bit` is refused, and the number differs per operator.
///
/// `SELECT CAST(1 AS bit) + CAST(1 AS bit);` **refuses** the pair with error 402, naming
/// both, while `*` is error 8117, naming the left one. So the promotion of `bit` to `int`
/// only exists in a **mixed** expression.
///
/// `-` and `%` follow `+`, and `/` follows `*`: `SELECT CAST(1 AS bit) - CAST(1 AS bit);`,
/// `… % …;` and `… / …;`.
#[test]
fn bit_arithmetic_is_refused_per_operator() {
    assert_eq!(
        refusal_of(BinaryOp::Add, SqlType::Bit, SqlType::Bit),
        (
            402,
            "The types bit and bit cannot be combined by the add operator.".to_owned()
        )
    );
    assert_eq!(err_of(BinaryOp::Sub, SqlType::Bit, SqlType::Bit), 402);
    assert_eq!(err_of(BinaryOp::Mod, SqlType::Bit, SqlType::Bit), 402);
    assert_eq!(
        refusal_of(BinaryOp::Mul, SqlType::Bit, SqlType::Bit),
        (
            8117,
            "Data type bit is not accepted by the multiply operator.".to_owned()
        )
    );
    assert_eq!(err_of(BinaryOp::Div, SqlType::Bit, SqlType::Bit), 8117);
    // Mixed, so the `bit` is promoted.
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::Bit, SqlType::Int),
        SqlType::Int
    );
}

/// `%` is the one arithmetic operator `float` and `real` do not take.
///
/// `SELECT CAST(1 AS float) % CAST(2 AS float);` is error 8117 on the **left** operand:
/// `SELECT CAST(1 AS real) % CAST(2 AS float);` names `real`, not the `float` the pair
/// would promote to. A mixed pair answers 402 instead (`SELECT CAST(1 AS float) % 2;` and
/// `SELECT 5 % CAST(2 AS float);`), and `money % money` is accepted (`SELECT CAST(10 AS
/// money) % CAST(3 AS money);` is a `money`).
#[test]
fn float_has_no_modulo() {
    assert_eq!(
        refusal_of(BinaryOp::Mod, SqlType::Float, SqlType::Float),
        (
            8117,
            "Data type float is not accepted by the modulo operator.".to_owned()
        )
    );
    assert_eq!(
        refusal_of(BinaryOp::Mod, SqlType::Real, SqlType::Real),
        (
            8117,
            "Data type real is not accepted by the modulo operator.".to_owned()
        )
    );
    assert_eq!(
        refusal_of(BinaryOp::Mod, SqlType::Real, SqlType::Float),
        (
            8117,
            "Data type real is not accepted by the modulo operator.".to_owned()
        )
    );
    assert_eq!(
        refusal_of(BinaryOp::Mod, SqlType::Float, SqlType::Int),
        (
            402,
            "The types float and int cannot be combined by the modulo operator.".to_owned()
        )
    );
    assert_eq!(err_of(BinaryOp::Mod, SqlType::Int, SqlType::Float), 402);
    assert_eq!(err_of(BinaryOp::Mod, num(2, 1), SqlType::Float), 402);
    assert_eq!(err_of(BinaryOp::Mod, SqlType::Money, SqlType::Float), 402);
    // The other four operators keep the approximate result they always had.
    for op in [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul, BinaryOp::Div] {
        assert_eq!(ty_of(op, SqlType::Float, SqlType::Float), SqlType::Float);
        assert_eq!(ty_of(op, SqlType::Real, SqlType::Real), SqlType::Real);
    }
    assert_eq!(
        ty_of(BinaryOp::Mod, SqlType::Money, SqlType::Money),
        SqlType::Money
    );
}

/// Dates: only `datetime` and `smalldatetime` take part in arithmetic.
#[test]
fn date_arithmetic_types() {
    // The date keeps its own type.
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::DateTime, SqlType::Int),
        SqlType::DateTime
    );
    assert_eq!(
        ty_of(BinaryOp::Sub, SqlType::SmallDateTime, SqlType::Int),
        SqlType::SmallDateTime
    );
    // Two day
    // counts add up, and the stronger type wins.
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::DateTime, SqlType::DateTime),
        SqlType::DateTime
    );
    assert_eq!(
        ty_of(
            BinaryOp::Add,
            SqlType::SmallDateTime,
            SqlType::SmallDateTime
        ),
        SqlType::SmallDateTime
    );
    assert_eq!(
        ty_of(BinaryOp::Add, SqlType::SmallDateTime, SqlType::DateTime),
        SqlType::DateTime
    );

    // The four types of 2008 against an `int`: error 206, naming the date first, in both
    // operand orders.
    for ty in [
        SqlType::Date,
        SqlType::Time(3),
        SqlType::DateTime2(3),
        SqlType::DateTimeOffset(3),
    ] {
        assert_eq!(err_of(BinaryOp::Add, ty, SqlType::Int), 206, "{ty:?}");
        assert_eq!(err_of(BinaryOp::Sub, ty, SqlType::Int), 206, "{ty:?}");
        assert_eq!(err_of(BinaryOp::Add, SqlType::Int, ty), 206, "{ty:?}");
    }
    let clash = binary_op_type(BinaryOp::Add, &info(SqlType::Date), &info(SqlType::Int))
        .expect_err("a date and a number never add up");
    assert_eq!(clash.number, 206);
    assert_eq!(
        clash.message,
        "Type mismatch: date cannot be combined with int."
    );

    // Error 8117, `Operand data type date is invalid for add
    // operator.` — the type has no arithmetic at all, even against another date.
    let alone = binary_op_type(BinaryOp::Add, &info(SqlType::Date), &info(SqlType::Date))
        .expect_err("`date` has no addition");
    assert_eq!(alone.number, 8117);
    assert_eq!(
        alone.message,
        "Data type date is not accepted by the add operator."
    );
    // `SELECT CAST('2000-01-01' AS date) + CAST('01:00' AS time(3));` and
    // `… - CAST('2000-01-02' AS date);`: 8117 on the left operand too, since neither
    // operand takes arithmetic.
    assert_eq!(err_of(BinaryOp::Add, SqlType::Date, SqlType::Time(3)), 8117);
    assert_eq!(err_of(BinaryOp::Sub, SqlType::Date, SqlType::Date), 8117);
    // As soon as one operand *does* take arithmetic, the pair is
    // refused with 402, naming both operands in the order the query wrote them
    // (`SELECT CAST('2000-01-01' AS datetime) + CAST('2000-01-01' AS date);` is the twin).
    assert_eq!(
        refusal_of(BinaryOp::Add, SqlType::Date, SqlType::DateTime),
        (
            402,
            "The types date and datetime cannot be combined by the add operator.".to_owned()
        )
    );
    assert_eq!(
        refusal_of(BinaryOp::Add, SqlType::DateTime, SqlType::Date),
        (
            402,
            "The types datetime and date cannot be combined by the add operator.".to_owned()
        )
    );

    // `*`, `/` and `%` convert the date to the other operand
    // and refuse that conversion, error 257.
    let times = binary_op_type(BinaryOp::Mul, &info(SqlType::DateTime), &info(SqlType::Int))
        .expect_err("`datetime` has no multiplication");
    assert_eq!(times.number, 257);
    assert_eq!(
        times.message,
        "No implicit conversion from datetime to int; use CONVERT explicitly."
    );
    assert_eq!(err_of(BinaryOp::Div, SqlType::DateTime, SqlType::Int), 257);

    // `%` is **not** one of them. It answers 402 on the pair, in the order the query wrote
    // it, where `*` and `/` answer 257.
    assert_eq!(
        refusal_of(BinaryOp::Mod, SqlType::DateTime, SqlType::Int),
        (
            402,
            "The types datetime and int cannot be combined by the modulo operator.".to_owned()
        )
    );
    assert_eq!(err_of(BinaryOp::Mod, SqlType::Int, SqlType::DateTime), 402);

    // A temporal type that converts to no number answers 206, the temporal name first,
    // under `*` and `/` as under `+`.
    assert_eq!(
        refusal_of(BinaryOp::Mul, SqlType::TinyInt, SqlType::Date),
        (
            206,
            "Type mismatch: date cannot be combined with tinyint.".to_owned()
        )
    );
    assert_eq!(err_of(BinaryOp::Div, SqlType::Date, SqlType::TinyInt), 206);

    // Against an operand that carries no arithmetic of its own, the same `date` answers
    // 402 and not 206.
    assert_eq!(
        refusal_of(BinaryOp::Add, SqlType::Date, SqlType::Bit),
        (
            402,
            "The types date and bit cannot be combined by the add operator.".to_owned()
        )
    );
    // And `*` answers 8117 on the left operand there, where `datetime * int` gives 257.
    assert_eq!(
        refusal_of(BinaryOp::Mul, SqlType::DateTime, SqlType::Bit),
        (
            8117,
            "Data type datetime is not accepted by the multiply operator.".to_owned()
        )
    );
}

/// Concatenation: the strongest of the two types, with the lengths added and capped.
#[test]
fn concat_types_and_lengths() {
    let varchar = |n| SqlType::VarChar(Len::Fixed(n));
    let nvarchar = |n| SqlType::NVarChar(Len::Fixed(n));

    // 10 + 20 = 30.
    assert_eq!(
        ty_of(BinaryOp::Concat, varchar(10), varchar(20)),
        varchar(30)
    );
    // 5000 + 5000 saturates at 8000 bytes.
    assert_eq!(
        ty_of(BinaryOp::Concat, varchar(5000), varchar(5000)),
        varchar(8000)
    );
    // 3000 + 3000 saturates at 4000 characters, which SQL
    // Server reports as a MaxLength of 8000 bytes.
    assert_eq!(
        ty_of(BinaryOp::Concat, nvarchar(3000), nvarchar(3000)),
        nvarchar(4000)
    );
    // A Unicode operand carries the pair.
    assert_eq!(
        ty_of(BinaryOp::Concat, varchar(10), nvarchar(5)),
        nvarchar(15)
    );
    // A `max` operand gives a `max` result.
    assert_eq!(
        ty_of(BinaryOp::Concat, SqlType::VarChar(Len::Max), varchar(10)),
        SqlType::VarChar(Len::Max)
    );
    assert_eq!(
        ty_of(BinaryOp::Concat, varchar(10), SqlType::NVarChar(Len::Max)),
        SqlType::NVarChar(Len::Max)
    );
    // 4 + 4 = 8.
    assert_eq!(
        ty_of(
            BinaryOp::Concat,
            SqlType::VarBinary(Len::Fixed(4)),
            SqlType::VarBinary(Len::Fixed(4))
        ),
        SqlType::VarBinary(Len::Fixed(8))
    );

    // The result is fixed-length only when both operands are.
    assert_eq!(
        ty_of(
            BinaryOp::Concat,
            SqlType::Char(Len::Fixed(3)),
            SqlType::Char(Len::Fixed(4))
        ),
        SqlType::Char(Len::Fixed(7))
    );
    assert_eq!(
        ty_of(
            BinaryOp::Concat,
            SqlType::NChar(Len::Fixed(3)),
            SqlType::NChar(Len::Fixed(4))
        ),
        SqlType::NChar(Len::Fixed(7))
    );
    assert_eq!(
        ty_of(BinaryOp::Concat, SqlType::Char(Len::Fixed(3)), varchar(4)),
        varchar(7)
    );
    assert_eq!(
        ty_of(BinaryOp::Concat, SqlType::NChar(Len::Fixed(3)), varchar(4)),
        nvarchar(7)
    );
    assert_eq!(
        ty_of(
            BinaryOp::Concat,
            SqlType::Binary(Len::Fixed(4)),
            SqlType::Binary(Len::Fixed(4))
        ),
        SqlType::Binary(Len::Fixed(8))
    );

    // The parser writes one `+` for both operations: `Add` on two character or binary
    // operands is the same concatenation as `Concat`.
    assert_eq!(
        ty_of(BinaryOp::Add, varchar(10), varchar(20)),
        ty_of(BinaryOp::Concat, varchar(10), varchar(20))
    );
    assert_eq!(
        ty_of(
            BinaryOp::Add,
            SqlType::VarBinary(Len::Fixed(4)),
            SqlType::VarBinary(Len::Fixed(4))
        ),
        SqlType::VarBinary(Len::Fixed(8))
    );

    // The two families do not mix, error 402, naming both.
    // Neither does a concatenation asked for on two numbers.
    assert_eq!(
        refusal_of(BinaryOp::Add, varchar(4), SqlType::VarBinary(Len::Fixed(4))),
        (
            402,
            "The types varchar and varbinary cannot be combined by the add operator.".to_owned()
        )
    );
    assert_eq!(err_of(BinaryOp::Concat, SqlType::Int, SqlType::Int), 402);

    // Only `+` concatenates; `-` on two character operands is
    // refused with 402, and `%` with it (`SELECT CAST('3' AS varchar(2)) % CAST('1' AS
    // varchar(2));`), while `*` and `/` answer 8117 on the left operand
    // (`… * …;`, `… / …;`).
    assert_eq!(
        refusal_of(BinaryOp::Sub, varchar(2), varchar(2)),
        (
            402,
            "The types varchar and varchar cannot be combined by the subtract operator.".to_owned()
        )
    );
    assert_eq!(err_of(BinaryOp::Mod, varchar(2), varchar(2)), 402);
    assert_eq!(
        refusal_of(BinaryOp::Mul, varchar(2), varchar(2)),
        (
            8117,
            "Data type varchar is not accepted by the multiply operator.".to_owned()
        )
    );
    assert_eq!(err_of(BinaryOp::Div, varchar(2), varchar(2)), 8117);
    // `DECLARE @g uniqueidentifier = NEWID(); SELECT @g + @g;` and `@g - @g`, `@g * @g`:
    // a `uniqueidentifier` is refused for its own sake, 8117, whatever the operator.
    for op in [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul] {
        assert_eq!(
            refusal_of(op, SqlType::UniqueIdentifier, SqlType::UniqueIdentifier).0,
            8117,
            "{op:?}"
        );
    }
    // `SELECT CAST(0x01 AS varbinary(2)) % CAST(0x02 AS varbinary(2));` is 402, and
    // `… * …;` is 8117: the binary family follows the character one.
    let varbinary = SqlType::VarBinary(Len::Fixed(2));
    assert_eq!(err_of(BinaryOp::Mod, varbinary, varbinary), 402);
    assert_eq!(err_of(BinaryOp::Mul, varbinary, varbinary), 8117);
}

/// Nullability and collation ride along the result.
#[test]
fn nullability_propagates() {
    let not_null = TypeInfo::new(SqlType::Int, false);
    let nullable = TypeInfo::new(SqlType::Int, true);

    let both_known = binary_op_type(BinaryOp::Add, &not_null, &not_null).expect("int + int");
    assert!(!both_known.nullable);
    assert_eq!(both_known.collation, None);

    for (a, b) in [
        (&not_null, &nullable),
        (&nullable, &not_null),
        (&nullable, &nullable),
    ] {
        assert!(
            binary_op_type(BinaryOp::Add, a, b)
                .expect("int + int")
                .nullable
        );
    }

    // A character result keeps the collation of the left operand.
    let left = TypeInfo {
        ty: SqlType::VarChar(Len::Fixed(10)),
        nullable: false,
        collation: Collation::parse("Latin1_General_CS_AS").ok(),
    };
    let right = TypeInfo::new(SqlType::VarChar(Len::Fixed(5)), true);
    let concat = binary_op_type(BinaryOp::Concat, &left, &right).expect("varchar + varchar");
    assert_eq!(concat.ty, SqlType::VarChar(Len::Fixed(15)));
    assert!(concat.nullable);
    assert_eq!(concat.collation, left.collation);

    // A number carries no collation, whatever its operands were.
    let from_strings = binary_op_type(BinaryOp::Sub, &left, &right).expect_err("no subtraction");
    assert_eq!(from_strings.number, 402);
    let mixed = binary_op_type(BinaryOp::Add, &left, &TypeInfo::new(SqlType::Int, false))
        .expect("varchar + int");
    assert_eq!(mixed.ty, SqlType::Int);
    assert_eq!(mixed.collation, None);
}

/// Forty pairs of a number and a non-numeric type, in both orders, through the type API.
#[test]
fn operand_clash_names_the_nonnumeric_type_first() {
    let arithmetic = [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul, BinaryOp::Div];
    for (number, other, operators) in [
        (SqlType::Int, SqlType::UniqueIdentifier, &arithmetic[..]),
        (SqlType::BigInt, SqlType::UniqueIdentifier, &arithmetic[..]),
        (dec(6, 2), SqlType::UniqueIdentifier, &arithmetic[..]),
        (SqlType::Int, SqlType::Date, &arithmetic[..2]),
        (SqlType::Int, SqlType::Time(3), &arithmetic[..2]),
        (num(6, 2), SqlType::DateTime2(3), &arithmetic[..2]),
        (SqlType::Float, SqlType::DateTimeOffset(3), &arithmetic[..2]),
    ] {
        for &op in operators {
            for (a, b) in [(number, other), (other, number)] {
                let error = binary_op_type(op, &info(a), &info(b)).expect_err("206");
                assert_eq!((error.number, error.severity, error.state), (206, 16, 2));
                assert_eq!(
                    error.message,
                    format!(
                        "Type mismatch: {} cannot be combined with {}.",
                        other.name(),
                        number.name()
                    ),
                    "{a:?} {op:?} {b:?}"
                );
            }
        }
    }
}

/// Common-type inference names a `uniqueidentifier` before a number and leaves the other
/// pairs in the order its caller passed them; the temporal reordering `binary_op_type`
/// applies must not reach it.
///
/// Ten `COALESCE` shapes: `SELECT COALESCE(1, CAST('20010102' AS date));` and `SELECT
/// COALESCE(CAST('20010102' AS date), 1);` both answer 206/16/2 naming `int` before
/// `date`, and so do the same two batches with `CAST('12:34:56' AS time(7))`,
/// `CAST('20010102' AS datetime2(0))` or `CAST('2001-01-02T00:00:00+03:00' AS
/// datetimeoffset(7))` in place of the date, each naming its own type second. The GUID
/// pair reads the other way: `SELECT COALESCE(1, CAST('01234567-89ab-cdef-0123-456789abcdef'
/// AS uniqueidentifier));` and its reverse both name `uniqueidentifier` before `int`.
///
/// The GUID order is the same in four constructs, arithmetic, two-argument `COALESCE`, the
/// branches of a `CASE` and the `=` comparison, so it is settled here for those four and
/// for both operand orders, and for them only: a fifth construct reads the same pair the
/// other way round in SQL Server. `SELECT ISNULL(NEWID(), 1);` names `int` before
/// `uniqueidentifier` there, the GUID second; `ISNULL` names its second argument first on
/// the five pairs and the two orders. Those ten shapes answer with a row and no error in
/// this engine, so nothing asserted below is reached through `ISNULL`.
///
/// The temporal order is not settled by the pair: the four `COALESCE(<temporal>, 1)`
/// shapes, and the four `CASE WHEN 1=1 THEN <temporal> ELSE 1 END` ones that read alike,
/// stay a known difference from SQL Server, asserted below as it stands, because the same
/// function serves comparison, where `SELECT CASE WHEN 1 = CAST('20010102' AS date) THEN
/// 1 ELSE 0 END;` and its reverse both name the temporal first. The pair alone therefore
/// does not decide that order, and this signature carries no context.
#[test]
fn common_type_clash_keeps_caller_order() {
    for other in [
        SqlType::Date,
        SqlType::Time(7),
        SqlType::DateTime2(0),
        SqlType::DateTimeOffset(7),
    ] {
        for (a, b) in [(SqlType::Int, other), (other, SqlType::Int)] {
            let error = vauban_types::implicit_result_type(&info(a), &info(b))
                .expect_err("the pair has no common type");
            assert_eq!(
                (error.number, error.severity, error.state),
                (206, 16, 2),
                "{a:?} {b:?}"
            );
            assert_eq!(
                error.message,
                format!(
                    "Type mismatch: {} cannot be combined with {}.",
                    a.name(),
                    b.name()
                ),
                "{a:?} {b:?}"
            );
        }
    }

    // `COALESCE(1, NEWID())` and `COALESCE(NEWID(), 1)`: the GUID is named first on both
    // sides.
    for (a, b) in [
        (SqlType::Int, SqlType::UniqueIdentifier),
        (SqlType::UniqueIdentifier, SqlType::Int),
    ] {
        let error = vauban_types::implicit_result_type(&info(a), &info(b))
            .expect_err("uniqueidentifier and int have no common type");
        assert_eq!(
            (error.number, error.severity, error.state),
            (206, 16, 2),
            "{a:?} {b:?}"
        );
        assert_eq!(
            error.message, "Type mismatch: uniqueidentifier cannot be combined with int.",
            "{a:?} {b:?}"
        );
    }
}
