use super::{
    CollationLabel, collation_of_comparison, collation_of_result, implicit_result_type,
    numeric_precision_scale, operand_precision_scale, rank,
};
use crate::collation::Collation;
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

fn named(name: &str) -> Collation {
    Collation::parse(name).expect("a known collation")
}

/// `cs = ci COLLATE Latin1_General_CI_AS` compares under the explicit collation, whichever
/// side carries it, and `cs = 'A'` under the collation of the column.
#[test]
fn an_explicit_collation_wins_over_a_column() {
    let cs = CollationLabel::Implicit(named("Latin1_General_CS_AS"));
    let ci = CollationLabel::Explicit(named("Latin1_General_CI_AS"));
    let literal = CollationLabel::CoercibleDefault(Collation::DEFAULT);
    assert_eq!(collation_of_comparison(cs, ci, "equal to").unwrap(), ci);
    assert_eq!(collation_of_comparison(ci, cs, "equal to").unwrap(), ci);
    assert_eq!(
        collation_of_comparison(cs, literal, "equal to").unwrap(),
        cs
    );
    assert_eq!(
        collation_of_comparison(literal, cs, "equal to").unwrap(),
        cs
    );
    assert_eq!(collation_of_comparison(literal, ci, "like").unwrap(), ci);
}

/// Two labels of one strength and one collation agree; two defaults keep the left one.
#[test]
fn equal_labels_agree() {
    let cs = named("Latin1_General_CS_AS");
    let ai = named("Latin1_General_CI_AI");
    for label in [CollationLabel::Explicit(cs), CollationLabel::Implicit(cs)] {
        assert_eq!(
            collation_of_comparison(label, label, "equal to").unwrap(),
            label
        );
        assert_eq!(
            collation_of_result(label, label, "add", &SqlType::VarChar(Len::Fixed(2))).unwrap(),
            label
        );
    }
    let left = CollationLabel::CoercibleDefault(cs);
    let right = CollationLabel::CoercibleDefault(ai);
    assert_eq!(
        collation_of_comparison(left, right, "equal to").unwrap(),
        left
    );
}

/// Two columns of different collations: 468 in a comparison, 457 in a concatenation; two
/// different `COLLATE` clauses: 468 in both.
#[test]
fn two_columns_leave_a_concatenation_without_collation() {
    let ty = SqlType::VarChar(Len::Fixed(20));
    let (cs, ci) = (named("Latin1_General_CS_AS"), named("Latin1_General_CI_AS"));
    let (left, right) = (CollationLabel::Implicit(cs), CollationLabel::Implicit(ci));
    let err = collation_of_comparison(left, right, "equal to").unwrap_err();
    assert_eq!((err.number, err.severity, err.state), (468, 16, 9));
    assert!(err.message.contains("equal to"), "{}", err.message);
    // The right operand's collation is named first.
    let first = err
        .message
        .find("Latin1_General_CI_AS")
        .expect("names the right one");
    let second = err
        .message
        .find("Latin1_General_CS_AS")
        .expect("names the left one");
    assert!(first < second, "{}", err.message);
    let err = collation_of_result(left, right, "add", &ty).unwrap_err();
    assert_eq!((err.number, err.severity, err.state), (457, 16, 1));
    assert!(err.message.contains("add") && err.message.contains("varchar"));

    let (left, right) = (CollationLabel::Explicit(cs), CollationLabel::Explicit(ci));
    let err = collation_of_comparison(left, right, "less than").unwrap_err();
    assert_eq!((err.number, err.state), (468, 9));
    let err = collation_of_result(left, right, "CASE", &ty).unwrap_err();
    assert_eq!((err.number, err.state), (468, 9));
    assert!(err.message.contains("CASE"), "{}", err.message);
}

/// Each family of names, parsed then named again; a non-UTF8 `_SC` name loses its `_SC`,
/// which has no bit.
#[test]
fn a_collation_spells_back_its_name() {
    for name in [
        "SQL_Latin1_General_CP1_CI_AS",
        "SQL_Latin1_General_CP1_CS_AS",
        "SQL_Latin1_General_CP1_CI_AI",
        "Latin1_General_CS_AS",
        "Latin1_General_CI_AI_KS_WS",
        "Latin1_General_BIN",
        "Latin1_General_BIN2",
        "Latin1_General_100_CI_AS",
        "Latin1_General_100_CS_AI_WS",
        "Latin1_General_100_CI_AS_SC_UTF8",
        "Latin1_General_100_BIN2_UTF8",
    ] {
        assert_eq!(named(name).name(), name);
    }
    assert_eq!(
        named("Latin1_General_100_CI_AS_SC").name(),
        "Latin1_General_100_CI_AS"
    );
    assert_eq!(Collation::DEFAULT.name(), "SQL_Latin1_General_CP1_CI_AS");
}
