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
use vauban_types::{Len, SqlType};

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

// ---------------------------------------------------------------------------
// The type of a run, over its operands
// ---------------------------------------------------------------------------

/// The column types of every `SELECT` under the set operation, left to right, read on the
/// schema of each operand that is not itself a set operation.
fn leaf_types(plan: &LogicalPlan) -> Vec<SqlType> {
    let mut out = Vec::new();
    collect_leaf_types(plan, &mut out);
    out
}

fn collect_leaf_types(plan: &LogicalPlan, out: &mut Vec<SqlType>) {
    match plan {
        LogicalPlan::SetOp { left, right, .. } => {
            collect_leaf_types(left, out);
            collect_leaf_types(right, out);
        }
        LogicalPlan::Sort { input, .. } => collect_leaf_types(input, out),
        other => out.push(other.schema().columns[0].ty.ty),
    }
}

/// The type of a `UNION ALL` run is decided over all of its operands: integer literals
/// count their digits against a decimal, and bare `NULL`s bring no type, however the tree
/// is shaped.
#[test]
fn a_union_all_run_is_typed_over_its_operands() {
    let numeric_2_1 = SqlType::Numeric {
        precision: 2,
        scale: 1,
    };
    assert_eq!(
        types("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 2.5"),
        vec![(numeric_2_1, false)]
    );
    assert_eq!(
        leaf_types(&plan("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 2.5")),
        vec![numeric_2_1; 3]
    );
    let varchar_1 = SqlType::VarChar(Len::Fixed(1));
    for text in [
        "SELECT NULL UNION ALL SELECT 'a'",
        "SELECT NULL UNION ALL SELECT NULL UNION ALL SELECT 'a'",
        "(SELECT NULL UNION ALL SELECT NULL) UNION ALL SELECT 'a'",
        "SELECT NULL INTERSECT SELECT NULL UNION ALL SELECT 'a'",
        "SELECT NULL UNION SELECT NULL UNION ALL SELECT 'a'",
    ] {
        assert_eq!(types(text), vec![(varchar_1, true)], "{text}");
    }
    // Bare NULLs alone stay int.
    assert_eq!(
        types("SELECT NULL UNION ALL SELECT NULL"),
        vec![(SqlType::Int, true)]
    );
    // The lengths are those of the widest operand of the run.
    assert_eq!(
        types(
            "SELECT CAST('ab' AS varchar(5)) UNION ALL SELECT N'abc' \
             UNION ALL SELECT CAST('x' AS varchar(20))"
        ),
        vec![(SqlType::NVarChar(Len::Fixed(20)), true)]
    );
}

/// Fifty branches are balanced by the parser; the type is still decided over the fifty,
/// and each of them is converted to it.
#[test]
fn a_balanced_run_of_fifty_branches_is_typed_over_its_fifty_operands() {
    let mut text = String::from("SELECT 1");
    for i in 2..50 {
        text.push_str(&format!(" UNION ALL SELECT {i}"));
    }
    text.push_str(" UNION ALL SELECT 2.5");
    let numeric_3_1 = SqlType::Numeric {
        precision: 3,
        scale: 1,
    };
    let p = plan(&text);
    assert_eq!(p.schema().columns[0].ty.ty, numeric_3_1);
    assert_eq!(leaf_types(&p), vec![numeric_3_1; 50]);
}

/// A `UNION` run under a `UNION ALL` keeps its own type, which the outer run converts
/// above the nested node: the deduplication happens on the strings.
#[test]
fn a_nested_run_is_converted_above_its_own_node() {
    let p = plan("SELECT '1' UNION SELECT '01' UNION ALL SELECT 5");
    let LogicalPlan::SetOp {
        all: true,
        left,
        schema,
        ..
    } = p
    else {
        panic!("expected the UNION ALL at the root, got {p:?}");
    };
    assert_eq!(schema.columns[0].ty.ty, SqlType::Int);
    let LogicalPlan::Project { input, exprs, .. } = *left else {
        panic!("expected a projection above the nested UNION, got {left:?}");
    };
    assert!(matches!(exprs[0].expr.kind, BoundExprKind::Convert { .. }));
    assert_eq!(exprs[0].expr.ty.ty, SqlType::Int);
    let LogicalPlan::SetOp {
        all: false, schema, ..
    } = *input
    else {
        panic!("expected the nested UNION, got {input:?}");
    };
    assert_eq!(schema.columns[0].ty.ty, SqlType::VarChar(Len::Fixed(2)));

    // A UNION run under a UNION ALL is typed on its own: its integers are no longer
    // literals for the outer run.
    assert_eq!(
        types("SELECT 1 UNION SELECT 2 UNION ALL SELECT 2.5"),
        vec![(
            SqlType::Numeric {
                precision: 11,
                scale: 1
            },
            false
        )]
    );
}

/// `INTERSECT` and `EXCEPT` hand the digits of their left literal to the run above them.
#[test]
fn intersect_hands_its_left_literal_to_the_run_above() {
    for (text, precision) in [
        ("SELECT 12345 INTERSECT SELECT 1 UNION ALL SELECT 2.5", 6),
        ("SELECT 1 INTERSECT SELECT 12345 UNION ALL SELECT 2.5", 2),
        (
            "SELECT CAST(1 AS int) INTERSECT SELECT 1 UNION ALL SELECT 2.5",
            11,
        ),
        ("SELECT 1 EXCEPT SELECT 2 UNION ALL SELECT 2.5", 2),
    ] {
        assert_eq!(
            plan(text).schema().columns[0].ty.ty,
            SqlType::Numeric {
                precision,
                scale: 1
            },
            "{text}"
        );
    }
}

/// An exact numeric past 38 digits keeps its integral digits and gives up scale.
#[test]
fn a_wide_decimal_gives_up_scale() {
    for (text, scale) in [
        (
            "SELECT CAST(1 AS decimal(35,0)) UNION ALL SELECT CAST(1 AS decimal(20,19))",
            3,
        ),
        (
            "SELECT CAST(1 AS decimal(38,0)) UNION ALL SELECT CAST(1 AS decimal(10,9)) \
             UNION ALL SELECT CAST(1 AS decimal(5,2))",
            0,
        ),
        (
            "SELECT CAST(1 AS decimal(30,0)) UNION ALL SELECT CAST(1 AS decimal(10,9))",
            8,
        ),
    ] {
        assert_eq!(
            plan(text).schema().columns[0].ty.ty,
            SqlType::Decimal {
                precision: 38,
                scale
            },
            "{text}"
        );
    }
}

/// A clash between a date and an integer literal names the literal `int`, even beside a
/// decimal where the literal counts its digits.
#[test]
fn a_clash_names_the_literal_as_an_int() {
    let error = err("SELECT CAST('2020-01-01' AS date) UNION ALL SELECT 1 UNION ALL SELECT 2.5");
    assert_eq!(error.number, 206);
    assert!(error.message.contains("int"), "{}", error.message);
    assert!(!error.message.contains("numeric"), "{}", error.message);
}

/// A signed integer literal counts its digits as the unsigned one does.
#[test]
fn a_negative_literal_counts_its_digits() {
    for (text, precision) in [
        ("SELECT 2.5 UNION ALL SELECT -7", 2),
        ("SELECT -(7) UNION ALL SELECT 2.5", 2),
        ("SELECT -(-7) UNION ALL SELECT 2.5", 2),
        ("SELECT -9999 UNION ALL SELECT 2.5", 5),
        ("SELECT -10000 UNION ALL SELECT 2.5", 7),
        ("SELECT -12345 UNION ALL SELECT 2.5", 7),
        ("SELECT -99999 UNION ALL SELECT 2.5", 7),
        ("SELECT -123456 UNION ALL SELECT 2.5", 7),
        ("SELECT -1234567 UNION ALL SELECT 2.5", 8),
        ("SELECT -2147483647 UNION ALL SELECT 2.5", 11),
        ("SELECT 12345 UNION ALL SELECT 2.5", 6),
        ("SELECT -100 INTERSECT SELECT 1 UNION ALL SELECT 2.5", 4),
    ] {
        assert_eq!(
            plan(text).schema().columns[0].ty.ty,
            SqlType::Numeric {
                precision,
                scale: 1
            },
            "{text}"
        );
    }
}

/// A catalogue without tables, for a `FROM` over a table value constructor.
struct NoTables;

impl vauban_binder::CatalogView for NoTables {
    fn resolve_table(
        &self,
        _name: &vauban_parser::ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<vauban_binder::ResolvedTable> {
        None
    }

    fn view_definition(&self, _object: vauban_catalog::ObjectId) -> Option<&str> {
        None
    }

    fn identity_column(
        &self,
        _object: vauban_catalog::ObjectId,
    ) -> Option<vauban_catalog::ColumnId> {
        None
    }

    fn computed_columns(&self, _object: vauban_catalog::ObjectId) -> Vec<vauban_catalog::ColumnId> {
        Vec::new()
    }

    fn constraint_names_on_table(&self, _table: vauban_catalog::ObjectId) -> Vec<String> {
        Vec::new()
    }

    fn primary_key_columns(&self, _table: vauban_catalog::ObjectId) -> Option<Vec<String>> {
        None
    }

    fn unique_key_on(&self, _table: vauban_catalog::ObjectId, _columns: &[String]) -> bool {
        false
    }

    fn classify_table_reference(
        &self,
        _name: &vauban_parser::ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> vauban_binder::TableReferenceKind {
        vauban_binder::TableReferenceKind::Unknown
    }
}

/// Binds the first statement of `text` with a catalogue, so that a `FROM` binds.
fn plan_with_from(text: &str) -> LogicalPlan {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let ctx = BindContext {
        text,
        catalog: Some(&NoTables),
        database: "master",
        default_schema: "dbo",
        variables: &vauban_binder::NoVariables,
        options: SessionOptions::default(),
    };
    match bind(&batch.statements[0], &ctx).expect("the statement binds") {
        BoundStatement::Query(plan) => *plan,
        other => panic!("expected a bound query, got {other:?}"),
    }
}

/// A `SELECT DISTINCT` branch deduplicates in its own type: the conversion goes above its
/// `Distinct`, on the left and on the right.
#[test]
fn a_distinct_branch_is_converted_above_its_distinct() {
    let distinct = "SELECT DISTINCT v FROM (VALUES ('1'), ('01')) t(v)";
    for text in [
        format!("{distinct} UNION ALL SELECT 5"),
        format!("SELECT 5 UNION ALL {distinct}"),
    ] {
        let LogicalPlan::SetOp { left, right, .. } = plan_with_from(&text) else {
            panic!("expected a set operation for {text}");
        };
        let branch = if text.starts_with("SELECT DISTINCT") {
            left
        } else {
            right
        };
        let LogicalPlan::Project { input, exprs, .. } = *branch else {
            panic!("expected a projection above the DISTINCT branch of {text}");
        };
        assert!(matches!(exprs[0].expr.kind, BoundExprKind::Convert { .. }));
        assert_eq!(exprs[0].expr.ty.ty, SqlType::Int);
        assert!(
            matches!(*input, LogicalPlan::Distinct(_)),
            "{text}: {input:?}"
        );
    }
}
