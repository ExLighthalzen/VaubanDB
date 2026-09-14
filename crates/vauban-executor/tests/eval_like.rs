//! `LIKE`, `NOT LIKE`, the `ESCAPE` clause and the collation of the operands.
//!
//! # Why the trees below are built by hand
//!
//! The trees are built the way `tests/eval_expr.rs` and `tests/eval_calls.rs` build
//! theirs, with the **real rules** of the crates the binder would have used:
//! `parse_literal` for a literal and the shape `bind_like` gives a `LIKE` node, the
//! conversion of a non-character operand included. Each test states the SQL it stands for.
//!
//! # What is *not* tested here
//!
//! The matching engine — `%`, `_`, `[abc]`, `[a-c]`, `[^abc]`, the trailing blanks — belongs
//! to `Collation::like` and to the tests of `vauban-types`. What follows tests the branch
//! around it: the three-valued logic, the negation, the `ESCAPE` clause and the choice of
//! collation.

use vauban_binder::{BoundCaseArm, BoundExpr, BoundExprKind, SessionOptions};
use vauban_errors::{SqlError, SqlResult};
use vauban_executor::{ExecContext, eval_expr};
use vauban_sysfn::StaticContext;
use vauban_types::{
    Collation, Len, LiteralKind, SqlString, SqlType, TypeInfo, Value, parse_literal,
};

// ---------------------------------------------------------------------------------------
// Evaluating
// ---------------------------------------------------------------------------------------

/// Evaluates `expr` under the default session options.
fn run(expr: &BoundExpr) -> SqlResult<Value> {
    let context = StaticContext::default();
    let mut ctx = ExecContext::scalar(&context, SessionOptions::default());
    eval_expr(expr, None, &mut ctx)
}

/// The value of `expr`.
fn v(expr: &BoundExpr) -> Value {
    run(expr).expect("the expression evaluates")
}

/// The error `expr` raises.
fn err(expr: &BoundExpr) -> SqlError {
    run(expr).expect_err("the expression raises")
}

// ---------------------------------------------------------------------------------------
// Building the bound tree a statement would have produced
// ---------------------------------------------------------------------------------------

/// A literal, with the value and the type `types` gives it (`parse_literal`).
fn lit(kind: LiteralKind, text: &str) -> BoundExpr {
    let (value, ty) = parse_literal(kind, text).expect("the literal is well formed");
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 1,
    }
}

/// A character literal written `'…'`, typed `varchar(n)`.
fn s(text: &str) -> BoundExpr {
    lit(LiteralKind::Str, text)
}

/// A character literal written `N'…'`, typed `nvarchar(n)`.
fn ns(text: &str) -> BoundExpr {
    lit(LiteralKind::NStr, text)
}

/// An integer literal.
fn int(text: &str) -> BoundExpr {
    lit(LiteralKind::Integer, text)
}

/// The bare `NULL` of the T-SQL text, which the binder types `int`, nullable.
fn null() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::Null),
        ty: TypeInfo::new(SqlType::Int, true),
        line: 1,
    }
}

/// A character value of an arbitrary type, for the operands a literal cannot spell —
/// `CAST('!' AS char(3))` and its two padding blanks.
fn text_of(text: &str, ty: SqlType) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::String(SqlString {
            text: text.to_owned(),
        })),
        ty: TypeInfo::new(ty, false),
        line: 1,
    }
}

/// `CAST(expr AS to)`, the node `bind_like` wraps a non-character operand in
/// (`to_string_operand`): the target is `varchar(max)`, or `nvarchar(max)` as soon as one
/// operand is Unicode, and `max` is chosen because it cannot truncate.
fn cast(expr: BoundExpr, to: SqlType) -> BoundExpr {
    let ty = TypeInfo::new(to, expr.ty.nullable);
    BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(expr),
            style: None,
            try_: false,
        },
        ty,
        line: 1,
    }
}

/// A non-character operand as `bind_like` leaves it: converted to `varchar(max)`.
fn as_varchar(expr: BoundExpr) -> BoundExpr {
    cast(expr, SqlType::VarChar(Len::Max))
}

/// `expr COLLATE name`: the variant only marks that the user asked, the collation itself
/// lives in the `ty` of the node.
fn collate(expr: BoundExpr, name: &str) -> BoundExpr {
    let ty = TypeInfo {
        collation: Some(Collation::parse(name).expect("a collation the server knows")),
        ..expr.ty.clone()
    };
    BoundExpr {
        kind: BoundExprKind::Collate {
            expr: Box::new(expr),
        },
        ty,
        line: 1,
    }
}

/// `expr [NOT] LIKE pattern [ESCAPE escape]`, typed `bit` as `bind_like` types it: nullable
/// as soon as one of the three operands is.
fn like_node(
    expr: BoundExpr,
    pattern: BoundExpr,
    escape: Option<BoundExpr>,
    negated: bool,
) -> BoundExpr {
    let nullable =
        expr.ty.nullable || pattern.ty.nullable || escape.as_ref().is_some_and(|e| e.ty.nullable);
    BoundExpr {
        kind: BoundExprKind::Like {
            expr: Box::new(expr),
            pattern: Box::new(pattern),
            escape: escape.map(Box::new),
            negated,
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line: 1,
    }
}

/// `expr LIKE pattern`.
fn like(expr: BoundExpr, pattern: BoundExpr) -> BoundExpr {
    like_node(expr, pattern, None, false)
}

/// `expr LIKE pattern ESCAPE escape`.
fn like_escape(expr: BoundExpr, pattern: BoundExpr, escape: BoundExpr) -> BoundExpr {
    like_node(expr, pattern, Some(escape), false)
}

/// `expr NOT LIKE pattern`. The parser puts the negation on the node rather than wrapping
/// it in a `NOT`, which is what this helper reproduces.
fn not_like(expr: BoundExpr, pattern: BoundExpr) -> BoundExpr {
    like_node(expr, pattern, None, true)
}

/// `CASE WHEN predicate THEN 1 ELSE 0 END`: a predicate has no type that can be projected
/// in T-SQL, so it is read through a `CASE`.
fn when(predicate: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Case {
            operand: None,
            arms: vec![BoundCaseArm {
                when: predicate,
                then: int("1"),
            }],
            else_: Some(Box::new(int("0"))),
        },
        ty: TypeInfo::new(SqlType::Int, false),
        line: 1,
    }
}

// ---------------------------------------------------------------------------------------
// The predicate
// ---------------------------------------------------------------------------------------

/// `SELECT CASE WHEN 'abc' LIKE 'a%' THEN 1 ELSE 0 END,
///         CASE WHEN 'abc' LIKE 'b%' THEN 1 ELSE 0 END;` answers `1, 0`. The false half
/// is asserted too: a rule whose only vector answers true is a coincidence.
#[test]
fn like_basic() {
    assert_eq!(v(&when(like(s("abc"), s("a%")))), Value::I32(1));
    assert_eq!(v(&when(like(s("abc"), s("b%")))), Value::I32(0));
}

/// `SELECT CASE WHEN 'ABC' LIKE 'a%' THEN 1 ELSE 0 END,
///         CASE WHEN N'é' LIKE N'e' THEN 1 ELSE 0 END;` answers `1, 0`: the default
/// collation of the server,
/// `SQL_Latin1_General_CP1_CI_AS`, ignores the case and **keeps** the accents. Both halves
/// sit here because `_CI` alone would not tell `_CI_AS` from `_CI_AI`.
#[test]
fn like_is_case_insensitive_by_default() {
    assert_eq!(v(&when(like(s("ABC"), s("a%")))), Value::I32(1));
    assert_eq!(v(&when(like(ns("é"), ns("e")))), Value::I32(0));
}

/// The collation handed to `Collation::like` is the one of the tested value, and a
/// `COLLATE` on it changes **nothing** — a known gap, pinned here so that it fails the day
/// the gap closes.
///
/// `SELECT CASE WHEN ('ABC' COLLATE Latin1_General_CS_AS) LIKE 'a%' THEN 1 ELSE 0 END,
///  CASE WHEN (N'é' COLLATE Latin1_General_CI_AI) LIKE N'e' THEN 1 ELSE 0 END;` answers
/// `0, 1` on SQL Server against the `1, 0` below: `Collation::compare_char` does not
/// consult `self` and compares everything as `_CI_AS`. Nothing of that lives in the
/// executor — the branch does pass the collation of the operand down — so the gap is
/// `types`'s rather than worked around here.
#[test]
fn like_collation_is_passed_down_but_ignored() {
    let cs_as = collate(s("ABC"), "Latin1_General_CS_AS");
    assert_eq!(v(&when(like(cs_as, s("a%")))), Value::I32(1));
    let ci_ai = collate(ns("é"), "Latin1_General_CI_AI");
    assert_eq!(v(&when(like(ci_ai, ns("e")))), Value::I32(0));
}

/// The raw value of `'abc' LIKE NULL`, `NULL LIKE 'a%'` and `NULL LIKE NULL` is
/// [`Value::Null`] — *unknown*, neither true nor false.
///
/// Read directly on `eval_expr`, not through a `CASE`, because a `CASE` cannot tell unknown
/// from false. Note the shape: `('abc' LIKE NULL) IS NULL` is **rejected** by SQL Server
/// with 156 — a predicate is not a value in T-SQL.
#[test]
fn like_with_null() {
    // The pattern is the bare `NULL` of the text, which `bind_like` converts towards
    // `varchar(max)` like any non-character operand.
    assert_eq!(v(&like(s("abc"), as_varchar(null()))), Value::Null);
    assert_eq!(v(&like(as_varchar(null()), s("a%"))), Value::Null);
    assert_eq!(
        v(&like(as_varchar(null()), as_varchar(null()))),
        Value::Null
    );
}

/// `SELECT CASE WHEN 'abc' NOT LIKE 'b%' THEN 1 ELSE 0 END,
///         CASE WHEN 'abc' NOT LIKE 'a%' THEN 1 ELSE 0 END;` answers `1, 0`, and
/// `NULL NOT LIKE 'a%'` is [`Value::Null`]: the negation swaps the two defined truth
/// values and leaves the third alone.
#[test]
fn not_like_negates_but_keeps_the_unknown() {
    assert_eq!(v(&when(not_like(s("abc"), s("b%")))), Value::I32(1));
    assert_eq!(v(&when(not_like(s("abc"), s("a%")))), Value::I32(0));
    assert_eq!(v(&not_like(as_varchar(null()), s("a%"))), Value::Null);
}

/// `SELECT CASE WHEN '100%' LIKE '100!%' ESCAPE '!' THEN 1 ELSE 0 END,
///         CASE WHEN '100x' LIKE '100!%' ESCAPE '!' THEN 1 ELSE 0 END;` answers `1, 0`:
/// the escaped `%` is a literal per cent and no longer a wildcard,
/// which the second column is there to prove — a wildcard would have matched `100x` too.
#[test]
fn like_with_escape() {
    let matched = when(like_escape(s("100%"), s("100!%"), s("!")));
    assert_eq!(v(&matched), Value::I32(1));
    let unmatched = when(like_escape(s("100x"), s("100!%"), s("!")));
    assert_eq!(v(&unmatched), Value::I32(0));
}

/// A `NULL` in an `ESCAPE` clause is **no escape at all**, not unknown — the one place a
/// careless vector hides the rule.
///
/// `'abc' LIKE 'a%' ESCAPE NULL` answers true on SQL Server, but so does
/// `'abc' LIKE 'a%'`: that vector distinguishes nothing. The vector below puts a `!`
/// **and** a `%` in the pattern, where the three candidate rules disagree — unknown would
/// answer -1, "escape as usual" 0, "no escape at all" 1 — and SQL Server answers `1, 0`
/// for the pair.
#[test]
fn escape_null_is_no_escape() {
    let dropped = like_escape(s("a!bc"), s("a!%"), as_varchar(null()));
    assert_eq!(v(&when(dropped)), Value::I32(1));
    let kept = like_escape(s("a!bc"), s("a!%"), s("!"));
    assert_eq!(v(&when(kept)), Value::I32(0));
}

/// `'a%c' LIKE 'a1%c' ESCAPE 1` answers true and `'a%c' LIKE 'a1%c'` answers false: the
/// `ESCAPE` operand is not required to be a character string, `bind_like` converts it,
/// and `1` becomes the escape character `'1'`.
#[test]
fn escape_is_converted_like_any_operand() {
    let escaped = like_escape(s("a%c"), s("a1%c"), as_varchar(int("1")));
    assert_eq!(v(&when(escaped)), Value::I32(1));
    assert_eq!(v(&when(like(s("a%c"), s("a1%c")))), Value::I32(0));
}

/// An `ESCAPE` clause of anything but one character raises **506**, severity 16, and, *on
/// the two non-Unicode predicates below*, state 1.
///
/// The state is asserted here for these two shapes only, and **not** as a property of 506:
/// it is 1 while the predicate is non-Unicode and **2** as soon as it is not, an `N` on the
/// value, on the pattern or on the escape operand alone being enough
/// (`pattern::tests::escape_state_follows_the_three_operand_types`).
///
/// The empty string is not one character either, and the text is echoed as it comes: a
/// `char(3)` pads `'!'` with two blanks and the message quotes `"!  "`, so nothing is
/// trimmed before the length is counted.
#[test]
fn invalid_escape_is_an_error() {
    let two = err(&like_escape(s("a"), s("a"), s("ab")));
    assert_eq!(two.number, 506);
    assert_eq!(two.severity, 16);
    // The predicate is non-Unicode, which is the only shape whose state is 1.
    assert_eq!(two.state, 1);
    assert!(two.message.contains("\"ab\""), "{}", two.message);

    let empty = err(&like_escape(s("a"), s("a"), s("")));
    assert_eq!(empty.number, 506);
    assert_eq!(empty.state, 1);
    assert!(empty.message.contains("\"\""), "{}", empty.message);

    let padded = text_of("!  ", SqlType::Char(Len::Fixed(3)));
    let blanks = err(&like_escape(s("a"), s("a"), padded));
    assert_eq!(blanks.number, 506);
    assert_eq!(blanks.state, 1);
    assert!(blanks.message.contains("\"!  \""), "{}", blanks.message);
}

/// The message of the 506 names the predicate `LIKE` for the negative form too:
/// `SELECT 1 WHERE 'a' NOT LIKE 'a' ESCAPE 'ab';` names a `LIKE` predicate — no `NOT`
/// anywhere — with state 1 like its positive twin. That shape, and not the positive one,
/// is what justifies the constant `PREDICATE` of `pattern.rs`.
#[test]
fn invalid_escape_on_not_like_still_names_like() {
    let negated = err(&like_node(s("a"), s("a"), Some(s("ab")), true));
    assert_eq!(negated.number, 506);
    assert_eq!(negated.state, 1);
    assert!(negated.message.contains("LIKE"), "{}", negated.message);
    assert!(!negated.message.contains("NOT"), "{}", negated.message);
}

/// The length of the `ESCAPE` operand is counted in **UTF-16 code units**, the unit SQL
/// Server counts, and not in Rust `char`s.
///
/// Both halves are asserted. `N'é'` is one unit and is a perfectly good escape character:
/// `N'a%c' LIKE N'aé%c' ESCAPE N'é'` answers 1 and `N'axc' LIKE N'aé%c' ESCAPE N'é'`
/// answers 0, so nothing rejects non-ASCII escapes as such. `N'𝄞'` (U+1D11E) is **one**
/// scalar written on **two** units and is refused with 506, quoting `"𝄞"`. Counting
/// `char`s would accept it.
///
/// It is not what `LEN` counts either: `LEN(N'𝄞')` is 2 under the default collation and 1
/// under `Latin1_General_100_CI_AS_SC`, yet the same escape written under that collation
/// still raises 506. The state of that 506 is 2, because the predicate is Unicode — see
/// [`invalid_escape_is_an_error`].
#[test]
fn escape_is_counted_in_utf16_code_units() {
    let escaped = like_escape(ns("a%c"), ns("aé%c"), ns("é"));
    assert_eq!(v(&when(escaped)), Value::I32(1));
    let unmatched = like_escape(ns("axc"), ns("aé%c"), ns("é"));
    assert_eq!(v(&when(unmatched)), Value::I32(0));

    let astral = err(&like_escape(ns("a%c"), ns("a𝄞%c"), ns("𝄞")));
    assert_eq!(astral.number, 506);
    assert_eq!(astral.state, 2);
    assert!(astral.message.contains("\"𝄞\""), "{}", astral.message);
}

/// A `NULL` on either operand wins over a malformed `ESCAPE`: `NULL LIKE 'a' ESCAPE 'ab'`
/// and `'a' LIKE NULL ESCAPE 'ab'` answer unknown and raise **nothing**, although
/// [`invalid_escape_is_an_error`] shows the very same clause raising 506 once both
/// operands are known.
#[test]
fn a_null_operand_beats_an_invalid_escape() {
    let null_value = like_escape(as_varchar(null()), s("a"), s("ab"));
    assert_eq!(v(&null_value), Value::Null);
    let null_pattern = like_escape(s("a"), as_varchar(null()), s("ab"));
    assert_eq!(v(&null_pattern), Value::Null);
}
