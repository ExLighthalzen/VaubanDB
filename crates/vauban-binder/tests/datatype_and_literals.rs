//! The contract `datatype.rs` and `literal.rs` lean on, checked from outside the crate.
//!
//! # Why the binding tests themselves are not here
//!
//! `resolve_data_type` and `bind_literal` are `pub(crate)`: `pub` is for what the crate
//! exposes. An integration test sees the public surface and nothing else, so the tests
//! of the two functions live next to them, in `src/datatype.rs` and `src/literal.rs`.
//!
//! # What this file does check
//!
//! The payload convention of `types`, which the mapping table of `bind_literal`
//! reads as a given: the `$` of a `money` and the `0x` of a binary are **not** in the
//! text the parser stores, and `types::parse_literal` expects them absent. If that
//! ever changes, the binder is the crate that has to change with it, and this test
//! says so from the outside.

use vauban_types::{Len, LiteralKind, SqlString, SqlType, Value, parse_literal};

/// The text of a `money` literal reaches `types` without its `$`, and the text of a
/// binary literal without its `0x` — the form `parser::Literal` stores.
#[test]
fn types_expects_the_payload_the_parser_stores() {
    let (value, ty) = parse_literal(LiteralKind::Money, "1.50").expect("$1.50");
    assert_eq!(ty.ty, SqlType::Money);
    assert_eq!(value, Value::Money(15_000));
    assert!(parse_literal(LiteralKind::Money, "$1.50").is_err());

    let (value, ty) = parse_literal(LiteralKind::Hex, "00FF").expect("0x00FF");
    assert_eq!(ty.ty, SqlType::VarBinary(Len::Fixed(2)));
    assert_eq!(value, Value::Bytes(vec![0x00, 0xFF]));
    assert!(parse_literal(LiteralKind::Hex, "0x00FF").is_err());
}

/// A string literal reaches `types` already unescaped and without its delimiters:
/// `'a''b'` is three characters, hence a `varchar(3)`.
#[test]
fn types_counts_the_characters_of_an_unescaped_string() {
    let (value, ty) = parse_literal(LiteralKind::Str, "a'b").expect("'a''b'");
    assert_eq!(ty.ty, SqlType::VarChar(Len::Fixed(3)));
    assert_eq!(
        value,
        Value::String(SqlString {
            text: "a'b".to_owned()
        })
    );

    let (_, ty) = parse_literal(LiteralKind::NStr, "é").expect("N'é'");
    assert_eq!(ty.ty, SqlType::NVarChar(Len::Fixed(1)));
}

/// The seven kinds of `LiteralKind` are the seven variants of `parser::Literal` that
/// carry text: `Null` and `Default` have none and are typed by the binder itself.
#[test]
fn every_literal_kind_has_a_vector() {
    let vectors = [
        (LiteralKind::Integer, "1"),
        (LiteralKind::Decimal, "1.50"),
        (LiteralKind::Float, "1e3"),
        (LiteralKind::Money, "1.50"),
        (LiteralKind::Hex, "00FF"),
        (LiteralKind::Str, "a"),
        (LiteralKind::NStr, "é"),
    ];
    for (kind, text) in vectors {
        assert!(parse_literal(kind, text).is_ok(), "{kind:?} {text}");
    }
}
