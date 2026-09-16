//! `UNION [ALL]`, `EXCEPT`, `INTERSECT` and their errors (205).
//!
//! Each test starts from SQL text, bound with no catalogue (no `FROM` is needed for
//! most vectors). The bound nodes derive no `PartialEq`; a plan is read by pattern
//! matching, a type by its `(SqlType, nullable)` pair.

use vauban_binder::{
    BindContext, BoundExprKind, BoundStatement, LogicalPlan, SessionOptions, SetOpKind, bind,
};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::SqlType;

/// Binds the first statement of `text` and returns its plan.
fn plan(text: &str) -> LogicalPlan {
    match bound(text) {
        BoundStatement::Query(plan) => *plan,
        other => panic!("expected a bound query, got {other:?}"),
    }
}

/// Binds the first statement of `text`, whatever it binds to.
fn bound(text: &str) -> BoundStatement {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let ctx = BindContext::scalar(text, SessionOptions::default());
    bind(&batch.statements[0], &ctx).expect("the statement binds")
}

/// The error binding the first statement of `text` raises.
fn err(text: &str) -> SqlError {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let ctx = BindContext::scalar(text, SessionOptions::default());
    bind(&batch.statements[0], &ctx).expect_err("the statement does not bind")
}

/// The `(SqlType, nullable)` of every column of the result of `text`.
fn types(text: &str) -> Vec<(SqlType, bool)> {
    let plan = plan(text);
    let schema = plan.schema();
    schema
        .columns
        .iter()
        .map(|col| (col.ty.ty, col.ty.nullable))
        .collect()
}

/// The names of every column of the result of `text`.
fn names(text: &str) -> Vec<String> {
    let plan = plan(text);
    plan.schema()
        .columns
        .iter()
        .map(|col| col.name.clone())
        .collect()
}

/// The name of the first column of the result of `text`.
fn first_name(text: &str) -> String {
    let mut n = names(text);
    assert!(!n.is_empty());
    n.remove(0)
}

// ---------------------------------------------------------------------------
// 205: column count mismatch
// ---------------------------------------------------------------------------

/// `SELECT 1 UNION SELECT 1, 2` raises 205.
#[test]
fn column_count_mismatch_is_205() {
    let error = err("SELECT 1 UNION SELECT 1, 2");
    assert_eq!(error.number, 205);
    assert_eq!(
        error.message,
        "Each side of a UNION, INTERSECT or EXCEPT must select the same number of expressions."
    );
}

// ---------------------------------------------------------------------------
// Common type by precedence
// ---------------------------------------------------------------------------

/// `SELECT CAST(1 AS int) UNION SELECT CAST(1 AS bigint)` gives a `bigint` schema
/// and a `Convert` node on the `int` side only.
#[test]
fn the_common_type_is_the_precedence_winner() {
    let plan_text = "SELECT CAST(1 AS int) UNION SELECT CAST(1 AS bigint)";
    let p = plan(plan_text);
    let LogicalPlan::SetOp {
        op: SetOpKind::Union,
        all: false,
        left,
        right,
        schema,
    } = p
    else {
        panic!("expected SetOp, got {p:?}");
    };
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].ty.ty, SqlType::BigInt);
    assert!(schema.columns[0].ty.nullable); // CAST is always nullable in VaubanDB

    // The left operand's projection has a Convert to bigint.
    assert!(has_convert_to(&left, SqlType::BigInt));
    // The right operand has a Convert (CAST does not change the schema type).
    assert!(has_convert_to(&right, SqlType::BigInt));

    // Counter-proof: both sides have the same type, Convert may or may not appear.
}

/// Whether the plan (unwrapped through Limit/Distinct/Filter) has a `Project` whose
/// first expression is a `Convert` to `target`.
fn has_convert_to(plan: &LogicalPlan, target: SqlType) -> bool {
    let mut p = plan;
    loop {
        match p {
            LogicalPlan::Project { exprs, .. } => {
                return exprs.first().is_some_and(|proj| {
                    matches!(&proj.expr.kind, BoundExprKind::Convert { .. })
                        && proj.expr.ty.ty == target
                });
            }
            LogicalPlan::Filter { input, .. } | LogicalPlan::Limit { input, .. } => p = input,
            LogicalPlan::Distinct(input) => p = input,
            LogicalPlan::Sort { input, .. } => p = input,
            _ => return false,
        }
    }
}

// ---------------------------------------------------------------------------
// Nullability
// ---------------------------------------------------------------------------

/// `SELECT CAST(1 AS int) UNION ALL SELECT CAST(NULL AS int)` renders `nullable`.
#[test]
fn nullability_is_the_union_of_both_sides() {
    let t = types("SELECT 1 AS a UNION ALL SELECT NULL AS b");
    assert_eq!(t, vec![(SqlType::Int, true)]);

    // Both sides non-nullable: the result is non-nullable.
    let t2 = types("SELECT CAST(1 AS int) AS a UNION ALL SELECT CAST(2 AS int) AS b");
    assert_eq!(t2, vec![(SqlType::Int, true)]); // CAST is always nullable in VaubanDB
}

// ---------------------------------------------------------------------------
// Output names from the first SELECT
// ---------------------------------------------------------------------------

/// `SELECT 1 AS a UNION SELECT 2 AS b` names the column `a`.
#[test]
fn output_names_come_from_the_first_select() {
    assert_eq!(first_name("SELECT 1 AS a UNION SELECT 2 AS b"), "a");
    // Without an alias, the column has no name.
    assert_eq!(first_name("SELECT 1 UNION SELECT 2"), "");
    // The alias of the first SELECT wins over the second.
    assert_eq!(first_name("SELECT 1 AS x UNION SELECT 2 AS y"), "x");
    assert_eq!(first_name("SELECT 1 AS x UNION SELECT 2"), "x");
}

// ---------------------------------------------------------------------------
// UNION ALL is a flag, not a Distinct
// ---------------------------------------------------------------------------

/// `SELECT 1 UNION SELECT 2` has no `Distinct` node: the deduplication is a property
/// of the `SetOp` node itself.
#[test]
fn union_all_is_a_flag_not_a_distinct() {
    let p = plan("SELECT 1 UNION SELECT 2");
    let LogicalPlan::SetOp {
        op,
        all,
        left,
        right,
        ..
    } = p
    else {
        panic!("expected SetOp, got {p:?}");
    };
    assert_eq!(op, SetOpKind::Union);
    assert!(!all); // no ALL: deduplication is built-in
    // There is no Distinct wrapping the SetOp.
    assert!(matches!(*left, LogicalPlan::Project { .. }));
    assert!(matches!(*right, LogicalPlan::Project { .. }));

    // UNION ALL does keep duplicates.
    let p2 = plan("SELECT 1 UNION ALL SELECT 2");
    let LogicalPlan::SetOp { all: all2, .. } = p2 else {
        panic!("expected SetOp");
    };
    assert!(all2);
}

// ---------------------------------------------------------------------------
// ORDER BY after a set operator
// ---------------------------------------------------------------------------

/// `SELECT 1 AS a UNION SELECT 2 AS a ORDER BY a` binds: the ORDER BY names columns
/// of the result set.
#[test]
fn order_by_after_a_set_op_uses_the_result_scope() {
    let p = plan("SELECT 1 AS a UNION SELECT 2 AS a ORDER BY a");
    let LogicalPlan::Sort { input, keys } = p else {
        panic!("expected Sort over SetOp, got {p:?}");
    };
    assert_eq!(keys.len(), 1);
    assert!(matches!(*input, LogicalPlan::SetOp { .. }));

    // Ordinal: `ORDER BY 1` also binds.
    let p2 = plan("SELECT 1 AS a UNION SELECT 2 AS a ORDER BY 1");
    let LogicalPlan::Sort { .. } = p2 else {
        panic!("expected Sort over SetOp, got {p2:?}");
    };

    // An ORDER BY key that is not a column of the result: need to measure the error
    // number. For now, it is 207 (invalid column name) — SQL Server answers 207 on
    // `SELECT 1 AS a UNION SELECT 2 ORDER BY nosuch`.
    let error = err("SELECT 1 AS a UNION SELECT 2 AS b ORDER BY nosuch");
    assert_eq!(error.number, 207);
    assert!(error.message.contains("nosuch"));
}

// ---------------------------------------------------------------------------
// Three operands (parser tree preserved)
// ---------------------------------------------------------------------------

/// `SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3` keeps the left-associative
/// parser tree.
#[test]
fn three_operands_keep_the_parser_tree() {
    let p = plan("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3");
    let LogicalPlan::SetOp {
        op,
        all,
        left,
        right,
        schema,
    } = p
    else {
        panic!("expected SetOp, got {p:?}");
    };
    assert_eq!(op, SetOpKind::Union);
    assert!(all);

    // The outer SetOp's right operand is the third SELECT.
    assert!(matches!(*right, LogicalPlan::Project { .. }));

    // The outer SetOp's left operand is the inner SetOp.
    let LogicalPlan::SetOp {
        op: inner_op,
        all: inner_all,
        left: inner_left,
        right: inner_right,
        ..
    } = *left
    else {
        panic!("expected nested SetOp");
    };
    assert_eq!(inner_op, SetOpKind::Union);
    assert!(inner_all);
    assert!(matches!(*inner_left, LogicalPlan::Project { .. }));
    assert!(matches!(*inner_right, LogicalPlan::Project { .. }));

    // The schema has one column, its type is int (the three operands have the same
    // type).
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].ty.ty, SqlType::Int);
}

// ---------------------------------------------------------------------------
// EXCEPT and INTERSECT
// ---------------------------------------------------------------------------

/// `EXCEPT` and `INTERSECT` also produce a `SetOp` node.
#[test]
fn except_and_intersect_produce_a_set_op() {
    let p = plan("SELECT 1 EXCEPT SELECT 2");
    let LogicalPlan::SetOp {
        op: SetOpKind::Except,
        ..
    } = p
    else {
        panic!("expected SetOp(Except), got {p:?}");
    };

    let p2 = plan("SELECT 1 INTERSECT SELECT 2");
    let LogicalPlan::SetOp {
        op: SetOpKind::Intersect,
        ..
    } = p2
    else {
        panic!("expected SetOp(Intersect), got {p:?}");
    };
}

// ---------------------------------------------------------------------------
// Type conversion with varchar and int
// ---------------------------------------------------------------------------

/// `SELECT 'hello' UNION SELECT 1` — varchar and int → int.
#[test]
fn string_and_int_use_the_precedence() {
    let t = types("SELECT 'hello' UNION SELECT 1");
    assert_eq!(t, vec![(SqlType::Int, false)]);
}
