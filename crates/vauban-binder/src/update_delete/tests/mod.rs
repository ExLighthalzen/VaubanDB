use super::compound_operator;
use vauban_parser::{AssignOp, BinaryOp};

/// Each of the eight compound operators stands for one arithmetic operator; `=` alone
/// stands for no operator.
#[test]
fn each_compound_operator_has_its_arithmetic() {
    assert!(compound_operator(AssignOp::Set).is_none());
    let pairs = [
        (AssignOp::AddAssign, BinaryOp::Add),
        (AssignOp::SubAssign, BinaryOp::Sub),
        (AssignOp::MulAssign, BinaryOp::Mul),
        (AssignOp::DivAssign, BinaryOp::Div),
        (AssignOp::ModAssign, BinaryOp::Mod),
        (AssignOp::BitAndAssign, BinaryOp::BitAnd),
        (AssignOp::BitOrAssign, BinaryOp::BitOr),
        (AssignOp::BitXorAssign, BinaryOp::BitXor),
    ];
    for (op, expected) in pairs {
        assert_eq!(compound_operator(op), Some(expected), "{op:?}");
    }
}
