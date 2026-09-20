//! Literals: `parser::Literal` to `types::LiteralKind`, then to a `Value`.
//!
//! The parser reads the shape of a literal, not its type: `Literal::Integer("1")`
//! is text plus a verdict of the lexer. `types::parse_literal` turns that pair into
//! a [`Value`] and the [`TypeInfo`] SQL Server gives it, and this module
//! is the hinge between the two: it maps `parser::Literal` onto
//! [`LiteralKind`](vauban_types::LiteralKind), one variant per variant, and wraps
//! the answer in a [`BoundExpr`].
//!
//! The correspondence lives **here** and not in `types` because `types` does not
//! depend on `parser`; it does not live in `parser` because the parser knows nothing
//! of types. The `match` of [`bind_literal`] has no `_ =>` arm on purpose: a new
//! variant of `parser::Literal` must break this file rather than be typed by
//! accident.
//!
//! No typing rule is reimplemented here. That `1.50` is a `numeric(3,2)`, that `1e3`
//! is a `float` and that `2147483648` is a `numeric(10,0)` is the business of
//! `types::parse_literal`.

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{Literal, Span};
use vauban_types::{LiteralKind, SqlType, TypeInfo, Value, parse_literal};

use crate::bound::{BoundExpr, BoundExprKind};
use crate::errors::line_of;

/// Binds a literal of the AST into a typed expression.
///
/// The text handed to `types::parse_literal` is the payload the parser stored, not
/// a reconstruction: `1.50` stays `1.50` and does not become `1.5`. The parser stores
/// it in the shape `parse_literal` expects — a `Money` without its `$`, a `Binary`
/// without its `0x`, a string already unescaped and without delimiters — so the
/// correspondence is a plain one-to-one match:
///
/// | `parser::Literal` | [`LiteralKind`] | `text` |
/// |---|---|---|
/// | `Integer(s)` | `Integer` | `s` |
/// | `Decimal(s)` | `Decimal` | `s` |
/// | `Float(s)` | `Float` | `s` |
/// | `Money(s)` | `Money` | `s` |
/// | `Binary(s)` | `Hex` | `s` |
/// | `Str { value, unicode: false }` | `Str` | `value` |
/// | `Str { value, unicode: true }` | `NStr` | `value` |
///
/// `span` gives the node its line ([`BoundExpr::line`]), which a runtime error later
/// reports; the column and the length are not kept.
///
/// # `NULL` has a type, and it is not an inference
///
/// An untyped `NULL` binds to `Value::Null` typed `int`, nullable — what SQL Server
/// announces for `SELECT NULL`. It is the default type of a `NULL` that stands alone,
/// and `ISNULL`/`COALESCE`/`CASE` replace it with the type of the other branch.
///
/// # Errors
///
/// - A `text` that does not match its kind, or a value out of the range of its type,
///   is the error `types::parse_literal` returns, passed on unchanged.
/// - `Literal::Default` — the word `DEFAULT` in the place of an expression — is an
///   internal error 50000. The parser produces it inside an `INSERT … VALUES` row or
///   an `UPDATE … SET c = DEFAULT`: reaching it here means the
///   caller bound a node it should have handled itself, so it is a bug of the engine
///   and not a mistake of the user.
pub(crate) fn bind_literal(lit: &Literal, span: &Span) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let (kind, text) = match lit {
        // `NULL` and `DEFAULT` are not kinds of `parse_literal`: they carry no text.
        Literal::Null => {
            return Ok(BoundExpr {
                kind: BoundExprKind::Literal(Value::Null),
                ty: TypeInfo::new(SqlType::Int, true),
                line,
            });
        }
        Literal::Default => {
            return Err(SqlError::from(InternalError::Bug(
                "bind_literal: DEFAULT is not an expression; it is bound by INSERT and UPDATE"
                    .into(),
            )));
        }
        Literal::Integer(text) => (LiteralKind::Integer, text.as_str()),
        Literal::Decimal(text) => (LiteralKind::Decimal, text.as_str()),
        Literal::Float(text) => (LiteralKind::Float, text.as_str()),
        Literal::Money(text) => (LiteralKind::Money, text.as_str()),
        Literal::Binary(text) => (LiteralKind::Hex, text.as_str()),
        Literal::Str {
            value,
            unicode: false,
        } => (LiteralKind::Str, value.as_str()),
        Literal::Str {
            value,
            unicode: true,
        } => (LiteralKind::NStr, value.as_str()),
    };
    let (value, ty) = parse_literal(kind, text)?;
    Ok(BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line,
    })
}

#[cfg(test)]
mod tests;
