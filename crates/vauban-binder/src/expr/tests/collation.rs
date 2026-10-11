//! Collation precedence: which collation an operator compares or returns under, and the
//! conflicts it refuses with 468 or 457.

use vauban_catalog::ColumnId;
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Expr, ParseOptions, QueryBody, SelectItem, Statement, parse_batch};
use vauban_types::{Collation, Len, SqlType, TypeInfo};

use super::super::{Scope, bind_expr};
use crate::bound::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::call::tests::registry;
use crate::context::{BindContext, SessionOptions};
use crate::star::Source;

fn named(name: &str) -> Collation {
    Collation::parse(name).expect("a known collation")
}

/// A `varchar(10)` column under `collation`.
fn column(name: &str, index: usize, collation: &str) -> ColumnBinding {
    let mut ty = TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true);
    ty.collation = Some(named(collation));
    ColumnBinding {
        column: ColumnId(i32::try_from(index + 1).expect("a small index")),
        index,
        name: name.to_owned(),
        ty,
        depth: 0,
    }
}

/// The first select item of `SELECT <expr>`, as the parser builds it.
fn parsed(expr: &str) -> Expr {
    let text = format!("SELECT {expr}");
    let batch = parse_batch(&text, &ParseOptions::default()).expect("parses");
    let Some(Statement::Select(select)) = batch.statements.into_iter().next() else {
        panic!("not a SELECT");
    };
    let QueryBody::Select(spec) = &select.body else {
        panic!("not a query specification");
    };
    match spec.items.first() {
        Some(SelectItem::Expr { expr, .. }) => expr.clone(),
        _ => panic!("not an expression"),
    }
}

/// `expr` bound over a table `t (cs, ci, d)`: `cs` is `Latin1_General_CS_AS`, `ci`
/// `Latin1_General_CI_AS` and `d` the default collation.
fn bind(expr: &str) -> SqlResult<BoundExpr> {
    registry();
    let columns = [
        column("cs", 0, "Latin1_General_CS_AS"),
        column("ci", 1, "Latin1_General_CI_AS"),
        column("d", 2, "SQL_Latin1_General_CP1_CI_AS"),
    ];
    let scope = Scope::over(Source::new("t", None, &columns, "dbo", "master"));
    let text = format!("SELECT {expr}");
    bind_expr(
        &parsed(expr),
        &BindContext::scalar(&text, SessionOptions::default()),
        &scope,
    )
}

fn err(expr: &str) -> SqlError {
    bind(expr).expect_err("fails to bind")
}

/// The collation each operand of a comparison or of a `LIKE` is evaluated under.
fn operand_collations(bound: &BoundExpr) -> (Option<Collation>, Option<Collation>) {
    match &bound.kind {
        BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Like {
            expr: left,
            pattern: right,
            ..
        } => (left.ty.collation, right.ty.collation),
        other => panic!("not a comparison: {other:?}"),
    }
}

#[test]
fn two_columns_of_different_collations_conflict() {
    for (expr, number, operation) in [
        ("CASE WHEN cs = ci THEN 1 END", 468, "equal to"),
        ("CASE WHEN cs < ci THEN 1 END", 468, "less than"),
        ("CASE WHEN cs LIKE ci THEN 1 END", 468, "like"),
        ("CASE WHEN cs IN ('a', ci) THEN 1 END", 468, "equal to"),
        (
            "CASE WHEN cs BETWEEN ci AND 'z' THEN 1 END",
            468,
            "greater than or equal to",
        ),
        ("CASE cs WHEN ci THEN 1 END", 468, "equal to"),
        ("CASE WHEN cs = ci + ci THEN 1 END", 468, "equal to"),
        ("CASE WHEN cs = UPPER(ci) THEN 1 END", 468, "equal to"),
        (
            "CASE WHEN cs = CAST(ci AS nvarchar(5)) THEN 1 END",
            468,
            "equal to",
        ),
        ("NULLIF(cs, ci)", 468, "equal to"),
        ("CHARINDEX(cs, ci)", 468, "charindex"),
        ("REPLACE(cs, ci, 'x')", 468, "replace"),
        ("cs + ci", 457, "add"),
        ("CASE WHEN 1 = 1 THEN cs ELSE ci END", 457, "CASE"),
        ("COALESCE(cs, ci)", 457, "CASE"),
    ] {
        let error = err(expr);
        assert_eq!(error.number, number, "{expr}: {}", error.message);
        assert!(
            error.message.contains(operation),
            "{expr}: {}",
            error.message
        );
    }
}

#[test]
fn two_explicit_collations_that_differ_are_468() {
    for (expr, operation) in [
        (
            "CASE WHEN 'a' COLLATE Latin1_General_CI_AS = 'A' COLLATE Latin1_General_CS_AS THEN 1 END",
            "equal to",
        ),
        (
            "'a' COLLATE Latin1_General_CI_AS + 'b' COLLATE Latin1_General_CS_AS",
            "add",
        ),
        (
            "COALESCE('a' COLLATE Latin1_General_CI_AS, 'b' COLLATE Latin1_General_CS_AS)",
            "CASE",
        ),
    ] {
        let error = err(expr);
        assert_eq!((error.number, error.state), (468, 9), "{expr}");
        assert!(
            error.message.contains(operation),
            "{expr}: {}",
            error.message
        );
    }
}

#[test]
fn the_stronger_label_decides_the_comparison() {
    let (cs, ci) = (named("Latin1_General_CS_AS"), named("Latin1_General_CI_AS"));
    let compare = |expr: &str| {
        let bound = bind(&format!("CASE WHEN {expr} THEN 1 END")).expect("binds");
        let BoundExprKind::Case { arms, .. } = bound.kind else {
            panic!("not a CASE");
        };
        operand_collations(&arms[0].when)
    };
    // An explicit collation wins over a column, on either side.
    assert_eq!(
        compare("cs = ci COLLATE Latin1_General_CI_AS"),
        (Some(ci), Some(ci))
    );
    assert_eq!(
        compare("ci COLLATE Latin1_General_CI_AS = cs"),
        (Some(ci), Some(ci))
    );
    // A column wins over a literal and a variable-free conversion of a number.
    assert_eq!(compare("'A' = cs"), (Some(cs), Some(cs)));
    assert_eq!(compare("CAST(1 AS varchar(5)) = cs"), (Some(cs), Some(cs)));
    // Two literals: the explicit one on the right decides.
    assert_eq!(
        compare("'a' = 'A' COLLATE Latin1_General_CS_AS"),
        (Some(cs), Some(cs))
    );
    assert_eq!(
        compare("'ABC' LIKE 'a%' COLLATE Latin1_General_CS_AS"),
        (Some(cs), Some(cs))
    );
    // A concatenation and a `CASE` carry the column they hold.
    assert_eq!(compare("'x' + cs = 'XA'"), (Some(cs), Some(cs)));
    assert_eq!(
        compare("CASE WHEN 1 = 1 THEN cs ELSE 'x' END = 'A'"),
        (Some(cs), Some(cs))
    );
}

/// A column compared under another collation is wrapped in a conversion to its own type,
/// so that no seek reads it as a key of its index; a literal is retyped in place.
#[test]
fn a_recollated_column_is_wrapped_and_a_literal_is_not() {
    let bound = bind("CASE WHEN cs = 'a' COLLATE Latin1_General_CI_AS THEN 1 END").expect("binds");
    let BoundExprKind::Case { arms, .. } = bound.kind else {
        panic!("not a CASE");
    };
    let BoundExprKind::Compare { left, .. } = &arms[0].when.kind else {
        panic!("not a comparison");
    };
    let BoundExprKind::Convert { expr: inner, .. } = &left.kind else {
        panic!("the column is not wrapped: {left:?}");
    };
    assert!(matches!(inner.kind, BoundExprKind::ColumnRef(_)));
    assert_eq!(left.ty.ty, inner.ty.ty);

    // Ten characters: the literal is a `varchar(10)` already, so no length conversion
    // hides it.
    let bound = bind("CASE WHEN 'abcdefghij' = cs THEN 1 END").expect("binds");
    let BoundExprKind::Case { arms, .. } = bound.kind else {
        panic!("not a CASE");
    };
    let BoundExprKind::Compare { left, right, .. } = &arms[0].when.kind else {
        panic!("not a comparison");
    };
    assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
    assert_eq!(left.ty.collation, Some(named("Latin1_General_CS_AS")));
    assert!(
        matches!(right.kind, BoundExprKind::ColumnRef(_)),
        "{right:?}"
    );
}

#[test]
fn the_result_carries_the_resolved_collation() {
    let (cs, ai) = (named("Latin1_General_CS_AS"), named("Latin1_General_CI_AI"));
    let collation = |expr: &str| bind(expr).expect("binds").ty.collation;
    assert_eq!(collation("'x' + cs"), Some(cs));
    assert_eq!(
        collation("CASE WHEN 1 = 1 THEN cs ELSE ci COLLATE Latin1_General_CI_AI END"),
        Some(ai)
    );
    assert_eq!(collation("COALESCE('x', cs)"), Some(cs));
    // `ISNULL` resolves nothing and raises nothing.
    assert!(bind("ISNULL(cs, ci)").is_ok());
}

/// `IN` resolves each element against the tested value: two explicit collations that
/// differ among the elements raise nothing.
#[test]
fn in_resolves_each_element_on_its_own() {
    let bound = bind(
        "CASE WHEN cs IN ('a' COLLATE Latin1_General_CI_AS, 'b' COLLATE Latin1_General_CI_AI) THEN 1 END",
    )
    .expect("binds");
    let BoundExprKind::Case { arms, .. } = bound.kind else {
        panic!("not a CASE");
    };
    let BoundExprKind::In { expr, .. } = &arms[0].when.kind else {
        panic!("not an IN");
    };
    assert_eq!(expr.ty.collation, Some(named("Latin1_General_CI_AS")));
}

/// `ISNULL` and `NULLIF` carry the label of their first argument, and
/// a scalar subquery that of the expression it projects: none of these shapes conflicts.
#[test]
fn a_result_carries_the_label_of_what_it_returns() {
    for expr in [
        "CASE WHEN cs = ISNULL('A', ci) THEN 1 END",
        "CASE WHEN d = NULLIF('b', ci) THEN 1 END",
        "cs + ISNULL('x', ci)",
        "CASE WHEN cs = LEFT(ISNULL('ab', ci), 1) THEN 1 END",
        "CASE WHEN cs = (SELECT 'a') THEN 1 END",
        "(SELECT 'a') + cs",
    ] {
        assert!(bind(expr).is_ok(), "{expr}: {:?}", bind(expr).err());
    }
    for expr in [
        "CASE WHEN cs = ISNULL(ci, 'A') THEN 1 END",
        "CASE WHEN cs = ISNULL(NULL, ci) THEN 1 END",
        "CASE WHEN cs = NULLIF(ci, 'A') THEN 1 END",
        "CASE WHEN cs = REPLACE('A', 'x', ci) THEN 1 END",
    ] {
        assert_eq!(err(expr).number, 468, "{expr}");
    }
    // `ISNULL('A', cs)` compares under the default collation, which ignores the case.
    let bound = bind("CASE WHEN ISNULL('A', cs) = 'a' THEN 1 END").expect("binds");
    let BoundExprKind::Case { arms, .. } = bound.kind else {
        panic!("not a CASE");
    };
    assert_eq!(
        operand_collations(&arms[0].when),
        (Some(Collation::DEFAULT), Some(Collation::DEFAULT))
    );
}

/// A column of the default collation reads as coercible by default, as a column of a
/// derived table or of `VALUES` that projects a literal does: it takes the collation of
/// the other operand instead of raising 468 or 457.
#[test]
fn a_derived_column_of_a_literal_does_not_conflict() {
    for expr in [
        "CASE WHEN cs = d THEN 1 END",
        "CASE WHEN d = ci THEN 1 END",
        "CASE WHEN cs LIKE d THEN 1 END",
    ] {
        let bound = bind(expr).unwrap_or_else(|e| panic!("{expr}: {e:?}"));
        let BoundExprKind::Case { arms, .. } = &bound.kind else {
            panic!("{expr}: not a CASE");
        };
        let (left, right) = operand_collations(&arms[0].when);
        assert_eq!(left, right, "{expr}: both operands under one collation");
        assert_ne!(
            left,
            Some(Collation::DEFAULT),
            "{expr}: the column of its own collation wins"
        );
    }
    let concatenated = bind("cs + d").expect("binds");
    assert_eq!(
        concatenated.ty.collation,
        Some(named("Latin1_General_CS_AS"))
    );
}
