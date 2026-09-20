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
