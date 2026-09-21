//! Binder tests: subqueries — `EXISTS`, `IN (SELECT …)`, scalar subquery, correlated
//! subquery, derived table.
//!
//! Each test binds against a [`CatalogView`] double that knows two tables,
//! `a (k int not null, c int)` and `b (k int, c int)`, under the schema `dbo` of the
//! current database. No row is necessary: the binder reads nothing from storage.

use vauban_binder::{
    BindContext, BoundExprKind, BoundStatement, CatalogView, LogicalPlan, NoVariables,
    ResolvedTable, ResolvedTableKind, SessionOptions, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{SqlType, TypeInfo};

/// Two tables, `dbo.a (k int not null, c int)` and `dbo.b (k int, c int)`.
struct TwoTables;

impl CatalogView for TwoTables {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        let object = match name.name.value.as_str() {
            "a" | "A" => 1,
            "b" | "B" => 2,
            _ => return None,
        };
        let columns = match name.name.value.as_str() {
            "a" | "A" => vec![
                binding(ColumnId(1), 0, "k", SqlType::Int, false),
                binding(ColumnId(2), 1, "c", SqlType::Int, true),
            ],
            _ => vec![
                binding(ColumnId(1), 0, "k", SqlType::Int, true),
                binding(ColumnId(2), 1, "c", SqlType::Int, true),
            ],
        };
        Some(ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("small identifier"))),
            columns,
            kind: ResolvedTableKind::Table,
        })
    }
}

fn binding(
    id: ColumnId,
    index: usize,
    name: &str,
    ty: SqlType,
    nullable: bool,
) -> vauban_binder::ColumnBinding {
    vauban_binder::ColumnBinding {
        column: id,
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
        depth: 0,
    }
}

/// Binds the first statement of `text` against `TwoTables`, in `master.dbo`.
fn bind_with(text: &str) -> Result<LogicalPlan, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let catalog = TwoTables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    };
    match bind(&batch.statements[0], &ctx)? {
        BoundStatement::Query(plan) => Ok(*plan),
        other => panic!("not a query: {other:?}"),
    }
}

fn error_of(text: &str) -> SqlError {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let catalog = TwoTables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    };
    for stmt in &batch.statements {
        if let Err(e) = bind(stmt, &ctx) {
            return e;
        }
    }
    unreachable!("{text} should fail")
}

// ---------------------------------------------------------------------------------------
// EXISTS
// ---------------------------------------------------------------------------------------

/// `EXISTS` is a predicate: `is_predicate()` is true, and it sits in a `WHERE`.
#[test]
fn exists_is_a_predicate() {
    let plan = bind_with("SELECT c FROM a WHERE EXISTS (SELECT 1)").expect("binds");
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    let LogicalPlan::Filter { predicate, .. } = input.as_ref() else {
        panic!("input is not a Filter: {input:?}");
    };
    assert!(predicate.is_predicate());
    assert!(matches!(predicate.kind, BoundExprKind::Exists(_)));
}

/// `EXISTS` accepts a select list with more than one column (116 does not apply).
#[test]
fn two_columns_under_exists_binds() {
    let plan = bind_with("SELECT c FROM a WHERE EXISTS (SELECT 1, 2)").expect("binds");
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    assert!(matches!(input.as_ref(), LogicalPlan::Filter { .. }));
}

// ---------------------------------------------------------------------------------------
// Correlated subquery (EXISTS ... WHERE inner.col = outer.col)
// ---------------------------------------------------------------------------------------

/// A column of the outer table resolves in the scope of the inner query.
#[test]
fn a_correlated_column_resolves_in_the_outer_scope() {
    let plan = bind_with("SELECT c FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)")
        .expect("correlated subquery binds");
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    let LogicalPlan::Filter { predicate, .. } = input.as_ref() else {
        panic!("input is not a Filter: {input:?}");
    };
    let BoundExprKind::Exists(inner) = &predicate.kind else {
        panic!("predicate is not Exists: {predicate:?}");
    };
    // The inner plan has a Filter whose predicate references `a.k`.
    let LogicalPlan::Project {
        input: inner_input, ..
    } = inner.as_ref()
    else {
        panic!("inner root is not a Project: {inner:?}");
    };
    let LogicalPlan::Filter {
        predicate: inner_pred,
        ..
    } = inner_input.as_ref()
    else {
        panic!("inner input is not a Filter: {inner_input:?}");
    };
    // The inner predicate should reference the outer column `a.k`.
    assert!(inner_pred.is_predicate());
}

/// Without the outer table, a column referenced in the inner query that came from the
/// outer scope is not found: a qualified reference like `a.k` with no source `a` is 4104,
/// and an unqualified reference is 207.
#[test]
fn a_correlated_column_207_without_the_outer_table() {
    // Qualified when `a` is not in scope → 4104.
    let error = error_of("SELECT 1 WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)");
    assert_eq!(error.number, 4104, "{}", error.message);
    // Unqualified when neither scope has the column → 207.
    let error = error_of("SELECT 1 WHERE EXISTS (SELECT nosuch FROM b)");
    assert_eq!(error.number, 207, "{}", error.message);
}

// ---------------------------------------------------------------------------------------
// Scope depth of a column reference
// ---------------------------------------------------------------------------------------

/// Each `ColumnRef` of `plan`, as `(name, depth)`, in a depth-first walk that descends
/// into the nested subqueries: what the tests on `depth` read off a bound plan.
fn column_depths(plan: &LogicalPlan) -> Vec<(String, u8)> {
    fn from_expr(expr: &vauban_binder::BoundExpr, out: &mut Vec<(String, u8)>) {
        match &expr.kind {
            BoundExprKind::ColumnRef(binding) => out.push((binding.name.clone(), binding.depth)),
            BoundExprKind::Exists(inner) | BoundExprKind::ScalarSubquery(inner) => {
                from_plan(inner, out);
            }
            BoundExprKind::InSubquery { expr, plan, .. } => {
                from_expr(expr, out);
                from_plan(plan, out);
            }
            BoundExprKind::Compare { left, right, .. }
            | BoundExprKind::Logical { left, right, .. }
            | BoundExprKind::Arith { left, right, .. } => {
                from_expr(left, out);
                from_expr(right, out);
            }
            BoundExprKind::Convert { expr, .. } | BoundExprKind::Not(expr) => from_expr(expr, out),
            BoundExprKind::Literal(_) => {}
            other => panic!("the walk does not descend into {other:?}"),
        }
    }
    fn from_plan(plan: &LogicalPlan, out: &mut Vec<(String, u8)>) {
        match plan {
            LogicalPlan::Project { input, exprs, .. } => {
                for projection in exprs {
                    from_expr(&projection.expr, out);
                }
                from_plan(input, out);
            }
            LogicalPlan::Filter { input, predicate } => {
                from_expr(predicate, out);
                from_plan(input, out);
            }
            LogicalPlan::Aggregate { input, .. } => from_plan(input, out),
            LogicalPlan::Scan { .. } | LogicalPlan::OneRow => {}
            other => panic!("the walk does not descend into {other:?}"),
        }
    }
    let mut out = Vec::new();
    from_plan(plan, &mut out);
    out
}

/// `z.k` and `x.k` are the same column of the same table, read at the same index: the
/// reference written in the subquery to the column of the enclosing query has `depth` 1,
/// the one to the column of its own `FROM` has `depth` 0. The `x.k` of the outer select
/// list and the unnamed slot of the `COUNT(*)` have `depth` 0 as well.
#[test]
fn an_outer_column_carries_its_scope_depth() {
    let plan = bind_with("SELECT x.k, (SELECT COUNT(*) FROM a z WHERE z.k > x.k) FROM a x")
        .expect("the correlated subquery binds");
    let depths = column_depths(&plan);
    assert_eq!(
        depths,
        vec![
            ("k".to_owned(), 0),
            (String::new(), 0),
            ("k".to_owned(), 0),
            ("k".to_owned(), 1),
        ],
        "{plan:#?}"
    );
    // The counter-proof on `column` and `index`: the two references of the inner
    // predicate are identical on both.
    let LogicalPlan::Project { exprs, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    let BoundExprKind::ScalarSubquery(inner) = &exprs[1].expr.kind else {
        panic!("second item is not a scalar subquery: {:?}", exprs[1]);
    };
    let LogicalPlan::Project { input, .. } = inner.as_ref() else {
        panic!("inner root is not a Project: {inner:?}");
    };
    let LogicalPlan::Aggregate { input, .. } = input.as_ref() else {
        panic!("inner input is not an Aggregate: {input:?}");
    };
    let LogicalPlan::Filter { predicate, .. } = input.as_ref() else {
        panic!("the aggregate reads no Filter: {input:?}");
    };
    let BoundExprKind::Compare { left, right, .. } = &predicate.kind else {
        panic!("the predicate is not a comparison: {predicate:?}");
    };
    let (BoundExprKind::ColumnRef(z), BoundExprKind::ColumnRef(x)) = (&left.kind, &right.kind)
    else {
        panic!("the comparison is not between two columns: {predicate:?}");
    };
    assert_eq!((z.column, z.index), (x.column, x.index));
    assert_eq!((z.depth, x.depth), (0, 1));
}

/// Two subqueries down, a column of the outermost query has `depth` 2, one of the middle
/// query `depth` 1, and one of the innermost `FROM` `depth` 0.
#[test]
fn a_column_two_queries_up_has_depth_two() {
    let plan = bind_with(
        "SELECT x.k FROM a x WHERE EXISTS (SELECT 1 FROM a y WHERE y.k = x.k \
         AND EXISTS (SELECT 1 FROM a z WHERE z.k > y.k AND z.c = x.c))",
    )
    .expect("the nested correlated subqueries bind");
    let depths = column_depths(&plan);
    assert_eq!(
        depths,
        vec![
            ("k".to_owned(), 0),
            ("k".to_owned(), 0),
            ("k".to_owned(), 1),
            ("k".to_owned(), 0),
            ("k".to_owned(), 1),
            ("c".to_owned(), 0),
            ("c".to_owned(), 2),
        ],
        "{plan:#?}"
    );
}

/// A subquery without a `FROM` is one query too: `x.k` read from `(SELECT x.k)` has
/// `depth` 1, and from `(SELECT (SELECT x.k))` `depth` 2.
#[test]
fn a_subquery_without_a_from_counts_as_one_level() {
    let plan = bind_with("SELECT (SELECT x.k) FROM a x").expect("binds");
    assert_eq!(column_depths(&plan), vec![("k".to_owned(), 1)], "{plan:#?}");
    let plan = bind_with("SELECT (SELECT (SELECT x.k)) FROM a x").expect("binds");
    assert_eq!(column_depths(&plan), vec![("k".to_owned(), 2)], "{plan:#?}");
}

// ---------------------------------------------------------------------------------------
// scalar subquery
// ---------------------------------------------------------------------------------------

/// A scalar subquery with two columns is 116.
#[test]
fn two_columns_in_a_scalar_subquery_is_116() {
    let error = error_of("SELECT (SELECT k, c FROM a)");
    assert_eq!(error.number, 116, "{}", error.message);
    assert_eq!(
        error.message,
        "A subquery not introduced by EXISTS must select a single expression."
    );
    // The counter-proof: one column binds.
    assert!(bind_with("SELECT (SELECT k FROM a)").is_ok());
}

/// A scalar subquery is nullable even when its column is NOT NULL.
#[test]
fn a_scalar_subquery_is_nullable_even_on_a_not_null_column() {
    let plan = bind_with("SELECT (SELECT k FROM a)").expect("binds");
    let LogicalPlan::Project { exprs, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    assert_eq!(exprs.len(), 1);
    assert!(
        exprs[0].expr.ty.nullable,
        "scalar subquery must be nullable"
    );
    // Counter-proof: the column `a.k` alone is NOT NULL.
    let plan = bind_with("SELECT k FROM a").expect("binds");
    let LogicalPlan::Project { exprs, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    assert!(!exprs[0].expr.ty.nullable, "a.k must not be nullable");
}

// ---------------------------------------------------------------------------------------
// IN (SELECT ...)
// ---------------------------------------------------------------------------------------

/// `IN (SELECT ...)` with two columns is 116.
#[test]
fn two_columns_in_in_subquery_is_116() {
    let error = error_of("SELECT c FROM a WHERE c IN (SELECT k, c FROM b)");
    assert_eq!(error.number, 116, "{}", error.message);
}

/// `IN (SELECT ...)` inserts a `Convert` when the tested value and the column of the
/// subquery have different types (here `int` and `int`, same, so no conversion; tested
/// with `nvarchar` → `int` conversion).
#[test]
fn in_a_subquery_converts_to_the_common_type() {
    let plan = bind_with("SELECT 1 WHERE 1 IN (SELECT k FROM a)").expect("binds");
    // Root is Project above Filter.
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    let LogicalPlan::Filter { predicate, .. } = input.as_ref() else {
        panic!("input is not a Filter: {input:?}");
    };
    assert!(predicate.is_predicate());
    assert!(matches!(predicate.kind, BoundExprKind::InSubquery { .. }));
}

// ---------------------------------------------------------------------------------------
// Derived table
// ---------------------------------------------------------------------------------------

/// `FROM (SELECT ...) AS d` exposes the alias, and `SELECT d.c` resolves.
#[test]
fn a_derived_table_exposes_its_alias() {
    let plan = bind_with("SELECT d.c FROM (SELECT c FROM a) AS d").expect("binds");
    let LogicalPlan::Project { exprs, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    assert_eq!(exprs.len(), 1);
    assert!(matches!(exprs[0].expr.kind, BoundExprKind::ColumnRef(_)));

    // `SELECT a.c` answers 4104: `a` is not in scope here.
    let error = error_of("SELECT a.c FROM (SELECT c FROM a) AS d");
    assert_eq!(error.number, 4104, "{}", error.message);
}

/// The column list of a derived table renames the output columns.
#[test]
fn a_derived_column_list_renames_the_output() {
    let plan = bind_with("SELECT d.x FROM (SELECT c FROM a) AS d(x)").expect("binds");
    let LogicalPlan::Project { exprs, input, .. } = &plan else {
        panic!("root is not a Project: {plan:?}");
    };
    assert_eq!(exprs.len(), 1);
    let LogicalPlan::Subquery { schema, .. } = input.as_ref() else {
        panic!("input is not a Subquery: {input:?}");
    };
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].name, "x");
}

/// A column list shorter than the inner columns is 8158.
#[test]
fn derived_column_list_longer_than_columns_is_8159() {
    let error = error_of("SELECT 1 FROM (SELECT c, k FROM a) AS d(x, y, z)");
    assert_eq!(error.number, 8159, "{}", error.message);
    assert!(
        error.message.contains("names more columns"),
        "{}",
        error.message
    );
}

/// A column list longer than the inner columns is 8158.
#[test]
fn derived_column_list_shorter_than_columns_is_8158() {
    let error = error_of("SELECT 1 FROM (SELECT c, k FROM a) AS d(x)");
    assert_eq!(error.number, 8158, "{}", error.message);
    assert!(
        error.message.contains("names fewer columns"),
        "{}",
        error.message
    );
}

// ---------------------------------------------------------------------------------------
// 207 — unknown column in subquery
// ---------------------------------------------------------------------------------------

/// An unknown column name in a subquery is 207.
#[test]
fn unknown_column_in_subquery_is_207() {
    let error = error_of("SELECT (SELECT nosuch FROM b) FROM a");
    assert_eq!(error.number, 207, "{}", error.message);
    assert_eq!(error.message, "Unknown column name 'nosuch'.");
}

// ---------------------------------------------------------------------------------------
// 116 — too many expressions in non-EXISTS subquery
// ---------------------------------------------------------------------------------------

#[test]
fn two_columns_in_scalar_is_116() {
    let error = error_of("SELECT (SELECT k, c FROM a) FROM b");
    assert_eq!(error.number, 116, "{}", error.message);
}

#[test]
fn two_columns_in_in_is_116() {
    let error = error_of("SELECT 1 WHERE c IN (SELECT k, c FROM a)");
    assert_eq!(error.number, 116, "{}", error.message);
}

// ---------------------------------------------------------------------------------------
// Table value constructor as a derived table
// ---------------------------------------------------------------------------------------

/// The `Values` node a constructor binds to, read through the `Subquery` that carries the
/// alias, and the schema of that `Subquery`.
fn values_of(
    text: &str,
) -> (
    Vec<Vec<vauban_binder::BoundExpr>>,
    vauban_binder::OutputSchema,
) {
    let plan = bind_with(text).expect("binds");
    let mut node = &plan;
    loop {
        match node {
            LogicalPlan::Subquery { input, schema, .. } => {
                let LogicalPlan::Values { rows, .. } = input.as_ref() else {
                    panic!("the Subquery does not hold a Values: {input:?}");
                };
                return (rows.clone(), schema.clone());
            }
            LogicalPlan::Project { input, .. }
            | LogicalPlan::Filter { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Aggregate { input, .. } => node = input.as_ref(),
            other => panic!("no Subquery over a Values in {other:?}"),
        }
    }
}

/// `FROM (VALUES (1), (2)) v(c)` binds to two rows of one column named by the list, under
/// the alias the rest of the query reads `v.c` through.
#[test]
fn a_values_constructor_binds_to_rows_under_its_alias() {
    let (rows, schema) = values_of("SELECT v.c FROM (VALUES (1), (2)) v(c)");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 1);
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].name, "c");
    assert_eq!(schema.columns[0].ty.ty, SqlType::Int);
    assert!(!schema.columns[0].ty.nullable);

    // The alias is what the outer query resolves through: `a.c` names no source here.
    let error = error_of("SELECT a.c FROM (VALUES (1)) v(c)");
    assert_eq!(error.number, 4104, "{}", error.message);
}

/// A constructor stands where a derived table stands: under an aggregate, in a join, in a
/// scalar subquery.
#[test]
fn a_values_constructor_stands_where_a_derived_table_stands() {
    bind_with("SELECT SUM(c) FROM (VALUES (1), (2), (3)) v(c)").expect("binds under an aggregate");
    bind_with("SELECT t.c FROM (VALUES (1)) t(c) JOIN a ON a.k = t.c").expect("binds in a join");
    bind_with("SELECT (SELECT MAX(c) FROM (VALUES (1), (2)) v(c)) AS m")
        .expect("binds in a scalar subquery");
}

/// Each column takes the common type of the expressions written at its position, and is
/// nullable when the position holds an untyped `NULL` or a nullable value.
#[test]
fn a_values_column_takes_the_common_type_of_its_position() {
    let (_, plain) = values_of("SELECT c FROM (VALUES (1), (2)) v(c)");
    assert_eq!(plain.columns[0].ty.ty, SqlType::Int);
    assert!(!plain.columns[0].ty.nullable);

    let (_, mixed) = values_of("SELECT c FROM (VALUES (1), (2.5)) v(c)");
    assert!(
        matches!(mixed.columns[0].ty.ty, SqlType::Numeric { .. }),
        "{:?}",
        mixed.columns[0].ty.ty
    );

    let (_, text) = values_of("SELECT c FROM (VALUES ('a'), (N'b')) v(c)");
    assert!(
        matches!(text.columns[0].ty.ty, SqlType::NVarChar(_)),
        "{:?}",
        text.columns[0].ty.ty
    );

    let (_, with_null) = values_of("SELECT c FROM (VALUES (NULL), (1)) v(c)");
    assert_eq!(with_null.columns[0].ty.ty, SqlType::Int);
    assert!(with_null.columns[0].ty.nullable);

    let (_, only_nulls) = values_of("SELECT c FROM (VALUES (NULL), (NULL)) v(c)");
    assert_eq!(only_nulls.columns[0].ty.ty, SqlType::Int);
    assert!(only_nulls.columns[0].ty.nullable);
}

/// A position holding two types takes the type the pair takes under a set operation:
/// each of the three mixtures below binds to the type of the `UNION ALL` written beside
/// it, precision and scale included.
#[test]
fn a_values_column_takes_the_type_its_pair_takes_under_a_set_operation() {
    for (constructor, set_operation) in [
        (
            "SELECT c FROM (VALUES (1), (2.5)) v(c)",
            "SELECT 1 AS c UNION ALL SELECT 2.5",
        ),
        (
            "SELECT c FROM (VALUES ('a'), (N'b')) v(c)",
            "SELECT 'a' AS c UNION ALL SELECT N'b'",
        ),
        (
            "SELECT c FROM (VALUES (NULL), (1)) v(c)",
            "SELECT NULL AS c UNION ALL SELECT 1",
        ),
    ] {
        let (_, schema) = values_of(constructor);
        let other = bind_with(set_operation).expect("the set operation binds");
        let expected = &other.schema().columns[0].ty;
        assert_eq!(
            schema.columns[0].ty.ty, expected.ty,
            "{constructor} against {set_operation}"
        );
        assert_eq!(
            schema.columns[0].ty.nullable, expected.nullable,
            "{constructor} against {set_operation}"
        );
    }
}

/// Two values of one position with no common type answer 206 at binding.
#[test]
fn a_values_position_without_a_common_type_is_206() {
    let error = error_of("SELECT c FROM (VALUES (1), (CAST('2020-01-01' AS date))) v(c)");
    assert_eq!(error.number, 206, "{}", error.message);
}

/// An alias with no column list is 8155: a constructor leaves its columns unnamed.
#[test]
fn a_values_alias_without_a_column_list_is_8155() {
    let error = error_of("SELECT * FROM (VALUES (1), (2)) v");
    assert_eq!(error.number, 8155, "{}", error.message);
    assert!(error.message.contains("'v'"), "{}", error.message);
}

/// A column list shorter than the rows is 8158, longer is 8159, as after a subquery.
#[test]
fn a_values_column_list_of_the_wrong_length_is_8158_or_8159() {
    let shorter = error_of("SELECT * FROM (VALUES (1, 2)) v(c)");
    assert_eq!(shorter.number, 8158, "{}", shorter.message);
    let longer = error_of("SELECT * FROM (VALUES (1)) v(c, d)");
    assert_eq!(longer.number, 8159, "{}", longer.message);
}

/// Rows of differing widths are 10709, and that check comes before the column list is
/// looked at: the list matches the first row in the second text below.
#[test]
fn values_rows_of_differing_widths_are_10709_before_the_column_list() {
    let plain = error_of("SELECT * FROM (VALUES (1), (2, 3)) v(c)");
    assert_eq!(plain.number, 10709, "{}", plain.message);
    let matching_list = error_of("SELECT * FROM (VALUES (1), (2, 3)) v(c, d)");
    assert_eq!(matching_list.number, 10709, "{}", matching_list.message);
}
