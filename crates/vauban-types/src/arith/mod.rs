//! Arithmetic and the other binary operators: the type of a result, then its value.

mod eval;
mod op_type;

pub use eval::*;
pub use op_type::*;

/// A binary operator of T-SQL, as the binder hands it to this crate.
///
/// `Concat` is the `+` of two character or binary operands: the same token as `Add`, but a
/// different operation, so the binder tells the two apart before calling in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// `+` on numbers and dates.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Mod,
    /// `&`.
    BitAnd,
    /// `|`.
    BitOr,
    /// `^`.
    BitXor,
    /// `+` on strings and binaries.
    Concat,
}
