//! `GROUP BY`, `HAVING` and the aggregate calls: the shape of the `Aggregate` and of what
//! sits above it, the extraction of the aggregates, and the errors 8120, 8121, 130, 144
//! and 147.
//!
//! The double of the catalogue knows two tables of `master.dbo`: `t (k int NOT NULL, c int
//! NULL, s varchar(10) NULL)` and `e (k int NOT NULL, c int NULL)`. No row is read: the
//! binding consults the columns and nothing else. The bound nodes derive no `PartialEq`:
//! a shape is checked by pattern matching.

use vauban_binder::{
    AggregateCall, BindContext, BoundExpr, BoundExprKind, BoundStatement, CatalogView,
    ColumnBinding, LogicalPlan, ResolvedTable, ResolvedTableKind, SessionOptions, VariableScope,
    bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{Ident, ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo};

/// A catalogue of two tables in `master.dbo`: `t (k, c, s)` and `e (k, c)`.
///
/// The comparison of the name is ASCII case-insensitive, and the parts that were not
/// written are filled from the arguments, so `master.dbo.t`, `dbo.t`, `t` and `[T]` reach
/// the same table.
struct TwoTables;

impl CatalogView for TwoTables {
    fn resolve_table(
        &self,
        name: &ObjectName,
        database: &str,
        default_schema: &str,
    ) -> Option<ResolvedTable> {
        if name.server.is_some() {
            return None;
        }
        let part = |ident: Option<&Ident>, default: &str| {
            ident.map_or_else(|| default.to_owned(), |ident| ident.value.clone())
        };
        if !part(name.database.as_ref(), database).eq_ignore_ascii_case("master")
            || !part(name.schema.as_ref(), default_schema).eq_ignore_ascii_case("dbo")
        {
            return None;
        }
        let mut columns = vec![
            column(ColumnId(1), 0, "k", SqlType::Int, false),
            column(ColumnId(2), 1, "c", SqlType::Int, true),
        ];
        let object = match name.name.value.to_ascii_lowercase().as_str() {
            "t" => {
                columns.push(column(
                    ColumnId(3),
                    2,
                    "s",
                    SqlType::VarChar(Len::Fixed(10)),
                    true,
                ));
                1
            }
            "e" => 2,
            _ => return None,
        };
        Some(ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("a small identifier"))),
            columns,
            kind: ResolvedTableKind::Table,
        })
    }
}

fn column(id: ColumnId, index: usize, name: &str, ty: SqlType, nullable: bool) -> ColumnBinding {
    ColumnBinding {
        column: id,
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
    }
}

/// One declared variable, `@v int`, for the select lists that name one.
struct OneVariable;

impl VariableScope for OneVariable {
    fn type_of(&self, name: &str) -> Option<TypeInfo> {
        name.eq_ignore_ascii_case("@v")
            .then(|| TypeInfo::new(SqlType::Int, true))
    }
}

/// Binds the statements of `text` in order against the two tables, in `master` and
/// `dbo`: the plan of the last one, or the first error.
fn bind_query(text: &str) -> Result<LogicalPlan, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let catalog = TwoTables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &OneVariable,
        options: SessionOptions::default(),
    };
    let mut last = None;
    for statement in &batch.statements {
        last = Some(match bind(statement, &ctx)? {
            BoundStatement::Query(plan) => *plan,
            other => panic!("{text}: not a query: {other:?}"),
        });
    }
    Ok(last.expect("one statement"))
}

#[track_caller]
fn plan_of(text: &str) -> LogicalPlan {
    bind_query(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

#[track_caller]
fn error_of(text: &str) -> SqlError {
    bind_query(text).expect_err(text)
}

/// The `Aggregate` of a grouped plan: under the `Project`, under the `Filter` of a
/// `HAVING` when one is written, and under the `Limit` and `Distinct` of a `TOP` and a
/// `SELECT DISTINCT`.
fn aggregate_of(plan: &LogicalPlan) -> &LogicalPlan {
    let mut node = plan;
    loop {
        node = match node {
            LogicalPlan::Project { input, .. }
            | LogicalPlan::Filter { input, .. }
            | LogicalPlan::Limit { input, .. } => input,
            LogicalPlan::Distinct(input) => input,
            LogicalPlan::Aggregate { .. } => return node,
            other => panic!("no Aggregate above {other:?}"),
        };
    }
}

/// The keys, the aggregates and the schema of the `Aggregate` of `plan`.
fn parts_of(plan: &LogicalPlan) -> (&[BoundExpr], &[AggregateCall], Vec<(String, TypeInfo)>) {
    let LogicalPlan::Aggregate {
        group_by,
        aggregates,
        schema,
        ..
    } = aggregate_of(plan)
    else {
        unreachable!("aggregate_of answers an Aggregate")
    };
    let columns = schema
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.ty.clone()))
        .collect();
    (group_by, aggregates, columns)
}

/// The projected columns of `plan`: name, index of the `ColumnRef` (`None` for another
/// expression), type.
fn projected(plan: &LogicalPlan) -> Vec<(String, Option<usize>, TypeInfo)> {
    let LogicalPlan::Project { exprs, schema, .. } = plan else {
        panic!("the root is not a Project: {plan:?}");
    };
    assert_eq!(schema.columns.len(), exprs.len());
    exprs
        .iter()
        .enumerate()
        .map(|(i, projection)| {
            assert_eq!(schema.columns[i].name, projection.name);
            assert_eq!(schema.columns[i].ty, projection.expr.ty);
            let index = match &projection.expr.kind {
                BoundExprKind::ColumnRef(binding) => Some(binding.index),
                _ => None,
            };
            (projection.name.clone(), index, projection.expr.ty.clone())
        })
        .collect()
}

fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

// ---------------------------------------------------------------------------------------
// The shape
// ---------------------------------------------------------------------------------------

/// The `Aggregate` publishes the keys in written order, then the aggregates in the order
/// they were extracted, and the projection above indexes that schema.
#[test]
fn group_by_builds_keys_then_aggregates() {
    let plan = plan_of("SELECT k, COUNT(*), c, SUM(c) FROM dbo.t GROUP BY c, k");
    let (keys, aggregates, schema) = parts_of(&plan);
    assert_eq!(keys.len(), 2);
    let key_indexes: Vec<usize> = keys
        .iter()
        .map(|key| match &key.kind {
            BoundExprKind::ColumnRef(binding) => binding.index,
            other => panic!("a key that is not a column: {other:?}"),
        })
        .collect();
    // `c` is column 1 of the table and `k` column 0: the keys keep the written order.
    assert_eq!(key_indexes, vec![1, 0]);
    assert_eq!(aggregates.len(), 2);
    assert_eq!(aggregates[0].def.name, "COUNT");
    assert_eq!(aggregates[1].def.name, "SUM");
    assert_eq!(
        schema,
        vec![
            ("c".to_owned(), int(true)),
            ("k".to_owned(), int(false)),
            (String::new(), int(true)),
            (String::new(), int(true)),
        ]
    );
    // The projection: `k` is key 1, `COUNT(*)` aggregate 0 (column 2), `c` key 0, `SUM(c)`
    // aggregate 1 (column 3). A key column keeps its written name, an aggregate has none.
    assert_eq!(
        projected(&plan),
        vec![
            ("k".to_owned(), Some(1), int(false)),
            (String::new(), Some(2), int(true)),
            ("c".to_owned(), Some(0), int(true)),
            (String::new(), Some(3), int(true)),
        ]
    );
    // The `Aggregate` sits directly over the `Scan`, the `Project` directly over it.
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("not a Project");
    };
    let LogicalPlan::Aggregate { input: scan, .. } = input.as_ref() else {
        panic!("not an Aggregate under the Project: {input:?}");
    };
    assert!(
        matches!(scan.as_ref(), LogicalPlan::Scan { .. }),
        "{scan:?}"
    );

    // An alias names the column; a key under an operator is still a key; a `WHERE` sits
    // under the `Aggregate` and a `HAVING` over it.
    let plan = plan_of(
        "SELECT k AS z, COUNT(*) AS n, k + 1 FROM dbo.t WHERE c IS NOT NULL GROUP BY k \
         HAVING COUNT(*) > 1",
    );
    assert_eq!(
        projected(&plan)
            .into_iter()
            .map(|(name, index, _)| (name, index))
            .collect::<Vec<_>>(),
        vec![
            ("z".to_owned(), Some(0)),
            ("n".to_owned(), Some(1)),
            (String::new(), None),
        ]
    );
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("not a Project");
    };
    let LogicalPlan::Filter { input, predicate } = input.as_ref() else {
        panic!("the HAVING is not a Filter under the Project: {input:?}");
    };
    assert!(predicate.is_predicate());
    let LogicalPlan::Aggregate { input, .. } = input.as_ref() else {
        panic!("not an Aggregate under the HAVING: {input:?}");
    };
    assert!(
        matches!(input.as_ref(), LogicalPlan::Filter { .. }),
        "the WHERE is not under the Aggregate: {input:?}"
    );

    // `TOP` and `DISTINCT` sit above the projection, as for an ungrouped query.
    let plan = plan_of("SELECT DISTINCT TOP 1 k, COUNT(*) FROM dbo.t GROUP BY k");
    let LogicalPlan::Limit { input, .. } = &plan else {
        panic!("not a Limit: {plan:?}");
    };
    let LogicalPlan::Distinct(input) = input.as_ref() else {
        panic!("not a Distinct under the Limit: {input:?}");
    };
    assert!(matches!(input.as_ref(), LogicalPlan::Project { .. }));
    assert_eq!(parts_of(&plan).1.len(), 1);
}

/// `COUNT(*)` has no argument, and its result is an `int`, nullable, as `COUNT_BIG(*)`
/// is a nullable `bigint`; `COUNT(c)` carries its argument. Written in lower case, the
/// name counts too.
#[test]
fn count_star_has_no_argument() {
    let plan = plan_of("SELECT COUNT(*), count_big(*), COUNT(c), COUNT(DISTINCT c) FROM dbo.t");
    let (keys, aggregates, schema) = parts_of(&plan);
    assert!(keys.is_empty());
    assert_eq!(aggregates.len(), 4);
    assert_eq!(aggregates[0].def.name, "COUNT");
    assert!(aggregates[0].arg.is_none());
    assert!(!aggregates[0].distinct);
    assert_eq!(aggregates[1].def.name, "COUNT_BIG");
    assert!(aggregates[1].arg.is_none());
    assert!(aggregates[2].arg.is_some());
    assert!(!aggregates[2].distinct);
    assert!(aggregates[3].arg.is_some());
    assert!(aggregates[3].distinct);
    assert_eq!(schema[0].1, int(true));
    assert_eq!(schema[1].1, TypeInfo::new(SqlType::BigInt, true));
    assert_eq!(schema[2].1, int(true));
    assert_eq!(schema[3].1, int(true));
    // Counter-proof on the nullability: a projected constant is not nullable, so the
    // `true` above is the aggregate's and not the projection's.
    let plan = plan_of("SELECT 1, COUNT(*) FROM dbo.t");
    let columns = projected(&plan);
    assert!(!columns[0].2.nullable);
    assert!(columns[1].2.nullable);
}

/// `MIN(k)` over a `NOT NULL` column is nullable, where `k` itself is not: the
/// counter-proof is the key `k` in the same schema. `MAX`, `SUM` and `AVG` follow.
#[test]
fn min_of_a_not_null_column_is_nullable() {
    let plan = plan_of("SELECT k, MIN(k), MAX(k), SUM(k), AVG(k) FROM dbo.t GROUP BY k");
    let columns = projected(&plan);
    assert_eq!(columns[0].0, "k");
    assert!(
        !columns[0].2.nullable,
        "the column itself: {:?}",
        columns[0]
    );
    for column in &columns[1..] {
        assert_eq!(column.2.ty, SqlType::Int, "{column:?}");
        assert!(column.2.nullable, "{column:?}");
    }
    // `MAX(s)` keeps the type, length and collation of its argument.
    let plan = plan_of("SELECT MAX(s) FROM dbo.t");
    assert_eq!(
        projected(&plan)[0].2,
        TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true)
    );
}

/// The same aggregate written twice is one entry of `aggregates`, wherever it is written:
/// the select list twice, the select list and the `HAVING`, or under an operator. A
/// different argument, or `DISTINCT` on one side, makes two entries.
#[test]
fn an_aggregate_written_twice_is_extracted_once() {
    let plan = plan_of("SELECT COUNT(*) FROM dbo.t HAVING COUNT(*) > 1");
    let (keys, aggregates, _) = parts_of(&plan);
    assert!(keys.is_empty());
    assert_eq!(aggregates.len(), 1);
    // Both places point at column 0 of the `Aggregate`.
    assert_eq!(projected(&plan)[0].1, Some(0));
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("not a Project");
    };
    let LogicalPlan::Filter { predicate, .. } = input.as_ref() else {
        panic!("no Filter for the HAVING: {input:?}");
    };
    let BoundExprKind::Compare { left, .. } = &predicate.kind else {
        panic!("the HAVING is not a comparison: {predicate:?}");
    };
    let BoundExprKind::ColumnRef(binding) = &left.kind else {
        panic!("the left side of the HAVING is not a reference: {left:?}");
    };
    assert_eq!(binding.index, 0);
    assert_eq!(binding.column, ColumnId(0));

    let plan = plan_of(
        "SELECT COUNT(*), COUNT(*) + 1, SUM(c), SUM(C), SUM(t.c) FROM dbo.t GROUP BY k HAVING COUNT(*) > 1 AND SUM(c) > 1",
    );
    assert_eq!(parts_of(&plan).1.len(), 2);
    assert_eq!(
        projected(&plan)
            .into_iter()
            .map(|(_, index, _)| index)
            .collect::<Vec<_>>(),
        vec![Some(1), None, Some(2), Some(2), Some(2)]
    );

    // Counter-proof: what differs is not merged.
    let plan = plan_of("SELECT COUNT(c), COUNT(DISTINCT c), COUNT(k), COUNT(*) FROM dbo.t");
    assert_eq!(parts_of(&plan).1.len(), 4);
}

/// Without a `GROUP BY`, the `Aggregate` has no key and its schema is the aggregates
/// alone: one group, hence one row, which the module documentation promises to the
/// executor. A `HAVING` alone sends the query there too, as does a `SELECT` without a
/// `FROM`.
#[test]
fn aggregate_without_group_by_is_one_group() {
    for text in [
        "SELECT COUNT(*) FROM dbo.t",
        "SELECT MIN(c) FROM dbo.e",
        "SELECT COUNT(*) FROM dbo.t WHERE 1 = 0",
        "SELECT 1 FROM dbo.t HAVING COUNT(*) > 1",
        "SELECT COUNT(*)",
        "SELECT SUM(1)",
    ] {
        let plan = plan_of(text);
        let (keys, aggregates, schema) = parts_of(&plan);
        assert!(keys.is_empty(), "{text}");
        assert_eq!(aggregates.len(), 1, "{text}");
        assert_eq!(schema.len(), 1, "{text}");
        assert!(schema[0].0.is_empty(), "{text}");
    }
    // `SELECT COUNT(*)` without a `FROM` groups the one row of `OneRow`.
    let plan = plan_of("SELECT COUNT(*)");
    let LogicalPlan::Aggregate { input, .. } = aggregate_of(&plan) else {
        unreachable!()
    };
    assert!(matches!(input.as_ref(), LogicalPlan::OneRow), "{input:?}");
    // Counter-proof: with a `GROUP BY`, the key is there.
    assert_eq!(
        parts_of(&plan_of("SELECT COUNT(*) FROM dbo.t GROUP BY k"))
            .0
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------------------
// 8120 and 8121
// ---------------------------------------------------------------------------------------

/// A column of the select list that is neither a key nor under an aggregate is 8120,
/// severity 16, state 1, on the line of the column, naming the table as written in the
/// `FROM` and the column as the catalogue holds it.
#[test]
fn a_column_outside_group_by_is_8120() {
    // The column and the statement are on different lines: the error carries the
    // column's.
    let error = error_of("SELECT k,\nc\nFROM dbo.t\nGROUP BY k");
    assert_eq!(error.number, 8120);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        SqlError::column_invalid_in_select_list("dbo.t", "c").message
    );

    // The table is printed as written, the alias set aside, and the column as the
    // catalogue names it.
    for (text, table, column) in [
        ("SELECT k, c FROM t GROUP BY k", "t", "c"),
        ("SELECT [K], [C] FROM [dbo].[T] GROUP BY [K]", "dbo.T", "c"),
        ("SELECT x.k, x.c FROM dbo.t AS x GROUP BY x.k", "dbo.t", "c"),
        ("SELECT c FROM dbo.t AS x GROUP BY k", "dbo.t", "c"),
        (
            "SELECT a.k, b.c FROM dbo.t AS a JOIN dbo.e AS b ON a.k = b.k GROUP BY a.k",
            "dbo.e",
            "c",
        ),
        // No `GROUP BY`: there is no key at all.
        ("SELECT COUNT(*) + k FROM dbo.t", "dbo.t", "k"),
        ("SELECT k FROM dbo.t HAVING COUNT(*) > 1", "dbo.t", "k"),
        // A column under an operator, a `CASE` or a scalar function is looked at.
        ("SELECT k, c + 1 FROM dbo.t GROUP BY k", "dbo.t", "c"),
        (
            "SELECT k, CASE WHEN c > 1 THEN 1 ELSE 0 END FROM dbo.t GROUP BY k",
            "dbo.t",
            "c",
        ),
        ("SELECT k, ABS(c) FROM dbo.t GROUP BY k", "dbo.t", "c"),
        // The second item is the faulty one.
        ("SELECT MIN(c), c FROM dbo.t GROUP BY k", "dbo.t", "c"),
        // `*` expands, and the first expanded column that is no key is named.
        ("SELECT * FROM dbo.t GROUP BY k", "dbo.t", "c"),
        ("SELECT t.* FROM dbo.t GROUP BY k, c", "dbo.t", "s"),
        ("SELECT * FROM dbo.t HAVING COUNT(*) > 1", "dbo.t", "k"),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 8120, "{text}: {}", error.message);
        assert_eq!(
            error.message,
            SqlError::column_invalid_in_select_list(table, column).message,
            "{text}"
        );
    }

    // Counter-proof: a column under an aggregate, a key under an operator, a constant, a
    // variable and a niladic function are no fault; and `*` over the keys binds.
    for text in [
        "SELECT k, SUM(c) FROM dbo.t GROUP BY k",
        "SELECT k, COUNT(k + c) FROM dbo.t GROUP BY k",
        "SELECT k + 1 FROM dbo.t GROUP BY k",
        "SELECT 1 FROM dbo.t GROUP BY k",
        "SELECT @v, COUNT(*) FROM dbo.t GROUP BY k",
        "SELECT k, CASE WHEN CURRENT_TIMESTAMP > '2000-01-01' THEN 1 ELSE 0 END FROM dbo.t GROUP BY k",
        "SELECT * FROM dbo.t GROUP BY s, c, k",
        "SELECT 1 FROM dbo.t HAVING COUNT(*) > 1",
    ] {
        plan_of(text);
    }
}

/// The same fault in the `HAVING` is 8121, on the line of the column, with the same
/// arguments.
#[test]
fn the_same_fault_in_having_is_8121() {
    let error = error_of("SELECT k\nFROM dbo.t\nGROUP BY k\nHAVING\nc > 1");
    assert_eq!(error.number, 8121);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert_eq!(error.line, 5);
    assert_eq!(
        error.message,
        SqlError::column_invalid_in_having("dbo.t", "c").message
    );
    for (text, column) in [
        ("SELECT 1 FROM dbo.t HAVING k > 1", "k"),
        (
            "SELECT k FROM dbo.t GROUP BY k HAVING COUNT(*) + c > 1",
            "c",
        ),
        (
            "SELECT c + 1 FROM dbo.t GROUP BY c + 1 HAVING 1 + c > 5",
            "c",
        ),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 8121, "{text}: {}", error.message);
        assert_eq!(
            error.message,
            SqlError::column_invalid_in_having("dbo.t", column).message,
            "{text}"
        );
    }
    // Counter-proof: a key, an aggregate and a key expression in the `HAVING` bind, and a
    // `HAVING` that is no predicate is 4145 quoting the token that follows it.
    for text in [
        "SELECT COUNT(*) FROM dbo.t GROUP BY k HAVING k > 1",
        "SELECT k FROM dbo.t GROUP BY k HAVING k + 1 > 1",
        "SELECT k FROM dbo.t GROUP BY k HAVING SUM(c) > 1",
        "SELECT c + 1 FROM dbo.t GROUP BY c + 1 HAVING c + 1 > 5",
    ] {
        plan_of(text);
    }
    let error = error_of("SELECT k FROM dbo.t GROUP BY k HAVING 1;");
    assert_eq!(error.number, 4145);
    assert_eq!(error.message, SqlError::non_boolean_expression(";").message);
    let error = error_of("SELECT COUNT(*) FROM dbo.t GROUP BY k HAVING COUNT(*);");
    assert_eq!(error.number, 4145);
    assert_eq!(error.message, SqlError::non_boolean_expression(";").message);
}

/// The clauses are checked in the order `WHERE`, `GROUP BY`, `HAVING`, select list, so
/// the error of an earlier clause wins over the error of a later one.
#[test]
fn the_having_is_checked_before_the_select_list() {
    for (text, number) in [
        ("SELECT k, c FROM dbo.t GROUP BY k HAVING s > 'a'", 8121),
        ("SELECT SUM(s) FROM dbo.t GROUP BY k HAVING c > 1", 8121),
        ("SELECT nosuch FROM dbo.t GROUP BY k HAVING c > 1", 8121),
        (
            "SELECT nosuch FROM dbo.t GROUP BY k HAVING nosuch2 > 1",
            207,
        ),
        ("SELECT c FROM dbo.t GROUP BY k HAVING SUM(s) > 1", 8117),
        (
            "SELECT k FROM dbo.t WHERE COUNT(*) > 1 GROUP BY nosuch",
            147,
        ),
        ("SELECT k FROM dbo.t WHERE nosuch = 1 GROUP BY SUM(c)", 207),
        ("SELECT k FROM dbo.t GROUP BY nosuch HAVING c > 1", 207),
        ("SELECT k FROM dbo.t GROUP BY SUM(c) HAVING c > 1", 144),
        ("SELECT k, c FROM dbo.t WHERE COUNT(*) > 1 GROUP BY k", 147),
        ("SELECT k, c, SUM(MAX(c)) FROM dbo.t GROUP BY k", 8120),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
    }
    let error = error_of("SELECT nosuch FROM dbo.t GROUP BY k HAVING nosuch2 > 1");
    assert_eq!(
        error.message,
        SqlError::invalid_column_name("nosuch2").message
    );
}

// ---------------------------------------------------------------------------------------
// 147, 144, 130 and the calls themselves
// ---------------------------------------------------------------------------------------

/// An aggregate in a `WHERE` is 147, severity 15, state 1, on the line of the
/// **statement**: the clause and the call are written on lines of their own, and the
/// error carries the `SELECT`'s.
#[test]
fn an_aggregate_in_where_is_147() {
    let error = error_of("SELECT 1 AS n;\nSELECT k\nFROM dbo.t\nWHERE\nSUM(c) > 1;");
    assert_eq!(error.number, 147);
    assert_eq!(error.severity, 15);
    assert_eq!(error.state, 1);
    assert_eq!(error.line, 2);
    assert_eq!(error.message, SqlError::aggregate_in_where().message);
    for text in [
        "SELECT k FROM dbo.t WHERE COUNT(*) > 1",
        "SELECT k FROM dbo.t WHERE SUM(c) > 1 GROUP BY k",
        "SELECT 1 WHERE COUNT(*) > 0",
        "SELECT k FROM dbo.t WHERE 1 = CASE WHEN MAX(c) > 1 THEN 1 ELSE 0 END",
    ] {
        assert_eq!(error_of(text).number, 147, "{text}");
    }
    // Counter-proof: a scalar call in the `WHERE` binds, a `WHERE` on a column that is no
    // key binds under the `Aggregate`.
    plan_of("SELECT k FROM dbo.t WHERE ABS(c) > 1 GROUP BY k");
    plan_of("SELECT COUNT(*) FROM dbo.t WHERE k > 1 HAVING COUNT(*) > 0");
}

/// An aggregate in a `GROUP BY` key is 144, on the line of the call; an aggregate under
/// an aggregate is 130, on the line of the inner call.
#[test]
fn an_aggregate_in_a_key_is_144_and_a_nested_one_is_130() {
    let error = error_of("SELECT k FROM dbo.t GROUP BY\nk,\nSUM(c)");
    assert_eq!(error.number, 144);
    assert_eq!(error.severity, 15);
    assert_eq!(error.state, 1);
    assert_eq!(error.line, 3);
    assert_eq!(error.message, SqlError::aggregate_in_group_by().message);
    assert_eq!(
        error_of("SELECT COUNT(*) FROM dbo.t GROUP BY COUNT(*)").number,
        144
    );

    let error = error_of("SELECT\nSUM(\n1 +\nMAX(c)) FROM dbo.t");
    assert_eq!(error.number, 130);
    assert_eq!(error.severity, 15);
    assert_eq!(error.state, 1);
    assert_eq!(error.line, 4);
    assert_eq!(error.message, SqlError::nested_aggregate().message);
    for text in [
        "SELECT SUM(COUNT(*)) FROM dbo.t",
        "SELECT k FROM dbo.t GROUP BY k HAVING SUM(MAX(c)) > 1",
        "SELECT COUNT(DISTINCT SUM(c)) FROM dbo.t",
    ] {
        assert_eq!(error_of(text).number, 130, "{text}");
    }
    // Counter-proof: a scalar function under an aggregate, and an aggregate under a
    // scalar function, both bind.
    plan_of("SELECT SUM(ABS(c)), ABS(SUM(c)) FROM dbo.t");
}

/// The errors of the call itself: 102 near `*` for a star under a name that is not
/// `COUNT` or `COUNT_BIG`, on the line of the `*`; 195 naming an aggregate function for
/// `DISTINCT` under a scalar name; 8117 for the bare `NULL` and for a type the aggregate
/// refuses; 174 for the wrong number of arguments.
#[test]
fn the_errors_of_the_call_itself() {
    let error = error_of("SELECT\nSUM(\n*) FROM dbo.t");
    assert_eq!(error.number, 102);
    assert_eq!(error.line, 3);
    assert_eq!(
        error.message,
        SqlError::incorrect_syntax_near("*", 3).message
    );
    for text in [
        "SELECT MAX(*) FROM dbo.t",
        "SELECT LEN(*) FROM dbo.t",
        "SELECT k FROM dbo.t WHERE SUM(*) > 1",
        "SELECT k FROM dbo.t GROUP BY SUM(*)",
    ] {
        assert_eq!(error_of(text).number, 102, "{text}");
    }

    for text in [
        "SELECT LEN(DISTINCT s) FROM dbo.t",
        "SELECT k FROM dbo.t WHERE LEN(DISTINCT s) = 1",
        "SELECT k FROM dbo.t GROUP BY LEN(DISTINCT s)",
        "SELECT SUM(LEN(DISTINCT s)) FROM dbo.t",
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 195, "{text}: {}", error.message);
        assert_eq!(error.severity, 15, "{text}");
        assert_eq!(error.state, 10, "{text}");
        assert_eq!(
            error.message,
            SqlError::not_a_recognized_name("LEN", "aggregate function").message,
            "{text}"
        );
    }
    let error = error_of("SELECT NOSUCH(DISTINCT c) FROM dbo.t");
    assert_eq!(error.number, 195);
    assert_eq!(
        error.message,
        SqlError::not_a_recognized_name("NOSUCH", "aggregate function").message
    );
    for text in [
        "SELECT NOSUCH(*) FROM dbo.t",
        "SELECT dbo.COUNT(*) FROM dbo.t",
    ] {
        assert_eq!(error_of(text).number, 102, "{text}");
    }

    for (text, ty, operator) in [
        ("SELECT SUM(NULL) FROM dbo.t", "NULL", "sum"),
        ("SELECT COUNT(NULL) FROM dbo.t", "NULL", "count"),
        ("SELECT SUM(s) FROM dbo.t", "varchar", "sum"),
        ("SELECT AVG(DISTINCT s) FROM dbo.t", "varchar", "avg"),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 8117, "{text}: {}", error.message);
        assert_eq!(
            error.message,
            SqlError::invalid_operand_type(ty, operator).message,
            "{text}"
        );
    }

    for text in ["SELECT COUNT(k, c) FROM dbo.t", "SELECT SUM() FROM dbo.t"] {
        assert_eq!(error_of(text).number, 174, "{text}");
    }

    // Counter-proof: `DISTINCT` under the six aggregates binds, with `distinct` set.
    let plan = plan_of(
        "SELECT COUNT(DISTINCT c), COUNT_BIG(DISTINCT c), SUM(DISTINCT c), AVG(DISTINCT c), \
         MIN(DISTINCT c), MAX(DISTINCT s) FROM dbo.t",
    );
    let aggregates = parts_of(&plan).1;
    assert_eq!(aggregates.len(), 6);
    assert!(aggregates.iter().all(|call| call.distinct));
}

// ---------------------------------------------------------------------------------------
// Keys that are expressions
// ---------------------------------------------------------------------------------------

/// An expression of the select list or of the `HAVING` is a key when its bound form is
/// the same as the key's: parentheses, case and qualifier do not count, a commuted
/// operator, another literal or another target type do.
#[test]
fn a_key_expression_is_matched_on_its_bound_form() {
    for (text, index) in [
        ("SELECT c + 1 FROM dbo.t GROUP BY c + 1", Some(0)),
        ("SELECT c+1 FROM dbo.t GROUP BY c + 1", Some(0)),
        ("SELECT (c + 1) FROM dbo.t GROUP BY c + 1", Some(0)),
        ("SELECT C + 1 FROM dbo.t GROUP BY c + 1", Some(0)),
        ("SELECT t.c + 1 FROM dbo.t GROUP BY c + 1", Some(0)),
        ("SELECT c + 1 FROM dbo.t AS x GROUP BY x.c + 1", Some(0)),
        ("SELECT LEN(s) FROM dbo.t GROUP BY LEN(s)", Some(0)),
        ("SELECT len(s) FROM dbo.t GROUP BY LEN(s)", Some(0)),
        ("SELECT t.k FROM dbo.t GROUP BY k", Some(0)),
        ("SELECT k FROM dbo.t GROUP BY t.k", Some(0)),
        ("SELECT ((k)) FROM dbo.t GROUP BY (k)", Some(0)),
        (
            "SELECT CAST(c AS bigint) FROM dbo.t GROUP BY CAST(c AS bigint)",
            Some(0),
        ),
        (
            "SELECT CASE WHEN c > 10 THEN 1 ELSE 0 END FROM dbo.t GROUP BY CASE WHEN c > 10 THEN 1 ELSE 0 END",
            Some(0),
        ),
        // The key is found one level down: the projected node is the operator above it.
        ("SELECT (c + 1) * 2 FROM dbo.t GROUP BY c + 1", None),
        ("SELECT k + 1 FROM dbo.t GROUP BY k", None),
        (
            "SELECT CASE k WHEN 1 THEN 'one' ELSE 'other' END FROM dbo.t GROUP BY k",
            None,
        ),
    ] {
        let plan = plan_of(text);
        let columns = projected(&plan);
        assert_eq!(columns[0].1, index, "{text}: {columns:?}");
        // A key that is an expression has no name in the schema of the `Aggregate`, a key
        // that is a column has the catalogue's.
        let key_name = &parts_of(&plan).2[0].0;
        let key_is_column = text.contains("GROUP BY k")
            || text.contains("GROUP BY t.k")
            || text.contains("GROUP BY (k)");
        assert_eq!(key_name.is_empty(), !key_is_column, "{text}: {key_name:?}");
    }
    // The header of a key column is the name as written; an expression has none.
    assert_eq!(
        projected(&plan_of("SELECT K FROM dbo.t GROUP BY k"))[0].0,
        "K"
    );
    assert_eq!(
        projected(&plan_of("SELECT c + 1 FROM dbo.t GROUP BY c + 1"))[0].0,
        ""
    );

    // Counter-proof: what is not the same bound form is 8120 on the column.
    for (text, column) in [
        ("SELECT 1 + c FROM dbo.t GROUP BY c + 1", "c"),
        ("SELECT c + 2 FROM dbo.t GROUP BY c + 1", "c"),
        ("SELECT c + 1.0 FROM dbo.t GROUP BY c + 1", "c"),
        ("SELECT c FROM dbo.t GROUP BY c + 1", "c"),
        ("SELECT s + 'A' FROM dbo.t GROUP BY s + 'a'", "s"),
        (
            "SELECT CAST(c AS int) FROM dbo.t GROUP BY CAST(c AS bigint)",
            "c",
        ),
        ("SELECT LEN(s) FROM dbo.t GROUP BY LEN(k)", "s"),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 8120, "{text}: {}", error.message);
        assert_eq!(
            error.message,
            SqlError::column_invalid_in_select_list("dbo.t", column).message,
            "{text}"
        );
    }
    // A key that is written twice is two keys of the node; a key is looked up in the
    // `FROM`, not among the aliases of the select list.
    assert_eq!(
        parts_of(&plan_of("SELECT k FROM dbo.t GROUP BY k, k"))
            .0
            .len(),
        2
    );
    let error = error_of("SELECT k AS kk FROM dbo.t GROUP BY kk");
    assert_eq!(error.number, 207);
    assert_eq!(error.message, SqlError::invalid_column_name("kk").message);
}

/// The references the projection and the `HAVING` hold point at the `Aggregate` with
/// `ColumnId(0)` and the name of the column in its schema, and not at a column of the
/// table.
#[test]
fn a_reference_above_the_aggregate_names_its_column() {
    let plan = plan_of("SELECT c, k, COUNT(*) FROM dbo.t GROUP BY k, c");
    let LogicalPlan::Project { exprs, .. } = &plan else {
        panic!("not a Project");
    };
    let bindings: Vec<(usize, ColumnId, String)> = exprs
        .iter()
        .map(|projection| match &projection.expr.kind {
            BoundExprKind::ColumnRef(binding) => {
                (binding.index, binding.column, binding.name.clone())
            }
            other => panic!("not a reference: {other:?}"),
        })
        .collect();
    assert_eq!(
        bindings,
        vec![
            (1, ColumnId(0), "c".to_owned()),
            (0, ColumnId(0), "k".to_owned()),
            (2, ColumnId(0), String::new()),
        ]
    );
    // Counter-proof: the argument of the aggregate and the keys point at the table.
    let plan = plan_of("SELECT k, SUM(c) FROM dbo.t GROUP BY k");
    let (keys, aggregates, _) = parts_of(&plan);
    let BoundExprKind::ColumnRef(key) = &keys[0].kind else {
        panic!("not a reference: {keys:?}");
    };
    assert_eq!((key.index, key.column), (0, ColumnId(1)));
    let Some(BoundExpr {
        kind: BoundExprKind::ColumnRef(arg),
        ..
    }) = &aggregates[0].arg
    else {
        panic!("the argument is not a reference: {aggregates:?}");
    };
    assert_eq!((arg.index, arg.column), (1, ColumnId(2)));
}
