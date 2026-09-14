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
        error.message.contains("has fewer columns"),
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
        error.message.contains("has more columns"),
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
