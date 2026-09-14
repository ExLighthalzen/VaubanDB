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
mod tests {
    use super::bind_literal;
    use vauban_errors::SqlResult;
    use vauban_parser::{Literal, Span};
    use vauban_types::{Len, LiteralKind, SqlString, SqlType, TypeInfo, Value, parse_literal};

    use crate::bound::{BoundExpr, BoundExprKind};

    /// A span on `line`, the only field a bound literal keeps.
    fn span(line: u32) -> Span {
        Span {
            line,
            column: 8,
            offset: 7,
            len: 1,
        }
    }

    /// The bound form of a literal of the AST, on line 1.
    ///
    /// The tests name the variant the lexer chooses. That choice is the lexer's and
    /// not the binder's: `bind_literal` does not look at the text to decide a kind.
    fn lit(literal: Literal) -> SqlResult<BoundExpr> {
        bind_literal(&literal, &span(1))
    }

    /// The value of a bound literal, or a panic when the node is not one.
    fn value(expr: &BoundExpr) -> &Value {
        match &expr.kind {
            BoundExprKind::Literal(value) => value,
            other => panic!("expected a literal, got {other:?}"),
        }
    }

    #[test]
    fn binds_numeric_literals() {
        let one = lit(Literal::Integer("1".to_owned())).expect("1 binds");
        assert_eq!(one.ty.ty, SqlType::Int);
        assert_eq!(value(&one), &Value::I32(1));
        assert!(!one.ty.nullable);

        // `1.50` keeps the text it was written with: the scale counts the digits
        // the user typed, so this is a numeric(3,2) and not a numeric(2,1).
        let fixed = lit(Literal::Decimal("1.50".to_owned())).expect("1.50 binds");
        assert_eq!(
            fixed.ty.ty,
            SqlType::Numeric {
                precision: 3,
                scale: 2
            }
        );

        let float = lit(Literal::Float("1e3".to_owned())).expect("1e3 binds");
        assert_eq!(float.ty.ty, SqlType::Float);

        let money = lit(Literal::Money("1.50".to_owned())).expect("$1.50 binds");
        assert_eq!(money.ty.ty, SqlType::Money);

        let binary = lit(Literal::Binary("00FF".to_owned())).expect("0x00FF binds");
        assert_eq!(binary.ty.ty, SqlType::VarBinary(Len::Fixed(2)));
    }

    #[test]
    fn literal_kind_mapping_is_exhaustive() {
        // One vector per variant of `parser::Literal` that carries text. The
        // expected answer is `types::parse_literal` called directly: what is under
        // test is the wiring — the kind chosen and the text handed over — and not
        // the rules of `types`.
        let vectors = [
            (Literal::Integer("1".to_owned()), LiteralKind::Integer, "1"),
            (
                Literal::Decimal("1.50".to_owned()),
                LiteralKind::Decimal,
                "1.50",
            ),
            (Literal::Float("1e3".to_owned()), LiteralKind::Float, "1e3"),
            (
                // `$1.50` reaches the AST without its `$`.
                Literal::Money("1.50".to_owned()),
                LiteralKind::Money,
                "1.50",
            ),
            (
                // `0x00FF` reaches the AST without its `0x`.
                Literal::Binary("00FF".to_owned()),
                LiteralKind::Hex,
                "00FF",
            ),
            (
                Literal::Str {
                    value: "a".to_owned(),
                    unicode: false,
                },
                LiteralKind::Str,
                "a",
            ),
            (
                Literal::Str {
                    value: "é".to_owned(),
                    unicode: true,
                },
                LiteralKind::NStr,
                "é",
            ),
        ];
        for (literal, kind, text) in vectors {
            let bound = lit(literal.clone()).expect("binds");
            let (expected_value, expected_ty) = parse_literal(kind, text).expect("parses");
            assert_eq!(value(&bound), &expected_value, "{literal:?}");
            assert_eq!(bound.ty, expected_ty, "{literal:?}");
        }
    }

    #[test]
    fn binds_string_literals() {
        let ascii = lit(Literal::Str {
            value: "a".to_owned(),
            unicode: false,
        })
        .expect("'a' binds");
        assert_eq!(ascii.ty.ty, SqlType::VarChar(Len::Fixed(1)));
        assert!(!ascii.ty.nullable);

        let unicode = lit(Literal::Str {
            value: "é".to_owned(),
            unicode: true,
        })
        .expect("N'é' binds");
        assert_eq!(unicode.ty.ty, SqlType::NVarChar(Len::Fixed(1)));

        // `'a''b'`: the parser has already unescaped the doubled quote, so the
        // binder sees three characters and hands them over untouched.
        let escaped = lit(Literal::Str {
            value: "a'b".to_owned(),
            unicode: false,
        })
        .expect("'a''b' binds");
        assert_eq!(escaped.ty.ty, SqlType::VarChar(Len::Fixed(3)));
        assert_eq!(
            value(&escaped),
            &Value::String(SqlString {
                text: "a'b".to_owned()
            })
        );
    }

    #[test]
    fn binds_null_as_nullable_int() {
        let null = lit(Literal::Null).expect("NULL binds");
        assert_eq!(value(&null), &Value::Null);
        assert_eq!(null.ty, TypeInfo::new(SqlType::Int, true));
        assert_eq!(null.ty.ty, SqlType::Int);
        assert!(null.ty.nullable);
    }

    #[test]
    fn default_is_an_internal_error() {
        let err = bind_literal(&Literal::Default, &span(1)).expect_err("DEFAULT is not a value");
        assert_eq!(err.number, 50000);
        assert_eq!(err.severity, 16);
    }

    #[test]
    fn literal_carries_its_line() {
        // `SELECT 1;\n\nSELECT 2`: the second literal starts on line 3.
        let first = bind_literal(&Literal::Integer("1".to_owned()), &span(1)).expect("binds");
        let second = bind_literal(&Literal::Integer("2".to_owned()), &span(3)).expect("binds");
        assert_eq!(first.line, 1);
        assert_eq!(second.line, 3);
    }

    #[test]
    fn a_literal_out_of_range_reports_the_error_of_types() {
        // 39 digits: `types::parse_literal` refuses it, and the binder passes the
        // error on without touching it.
        let text = "1".repeat(39);
        let err = lit(Literal::Integer(text.clone())).expect_err("39 digits do not fit");
        let expected = parse_literal(LiteralKind::Integer, &text).expect_err("39 digits");
        assert_eq!(err, expected);
    }
}
