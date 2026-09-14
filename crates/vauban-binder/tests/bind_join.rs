//! The `FROM` of more than one source: the tree of `Join` nodes it binds to, the schema of
//! that tree, and the scope its columns resolve in.
//!
//! The double of the catalogue knows three tables: `dbo.a (k int NOT NULL, c int NULL)`,
//! `dbo.b (k int NOT NULL, c int NULL)`, whose columns share their names, and
//! `dbo.d (k int NOT NULL, e int NULL)`, whose `e` is in no other table. The bound nodes
//! derive no `PartialEq`: a shape is checked by pattern matching.

use vauban_binder::{
    BindContext, BoundExpr, BoundExprKind, BoundStatement, CatalogView, ColumnBinding, JoinKind,
    LogicalPlan, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{Ident, ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{SqlType, TypeInfo};

/// A catalogue of three tables in `master.dbo`: `a (k, c)`, `b (k, c)` and `d (k, e)`.
///
/// The comparison of the name is ASCII case-insensitive, and the parts that were not
/// written are filled from the arguments, so `master.dbo.a`, `dbo.a` and `A` reach the
/// same table.
struct ThreeTables;

impl CatalogView for ThreeTables {
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
        let (object, second) = match name.name.value.to_ascii_lowercase().as_str() {
            "a" => (1, "c"),
            "b" => (2, "c"),
            "d" => (3, "e"),
            _ => return None,
        };
        Some(ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("a small identifier"))),
            columns: vec![
                column(ColumnId(1), 0, "k", false),
                column(ColumnId(2), 1, second, true),
            ],
            kind: ResolvedTableKind::Table,
        })
    }
}

fn column(id: ColumnId, index: usize, name: &str, nullable: bool) -> ColumnBinding {
    ColumnBinding {
        column: id,
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, nullable),
    }
}

/// Binds the statements of `text` in order against the three tables, in `master` and
/// `dbo`: the plan of the last one, or the first error.
fn bind_query(text: &str) -> Result<LogicalPlan, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let catalog = ThreeTables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
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

/// The node under the `Project` (and under the `Filter` of a `WHERE`, when one is
/// written): the `FROM` as bound.
fn from_of(plan: &LogicalPlan) -> &LogicalPlan {
    let LogicalPlan::Project { input, .. } = plan else {
        panic!("the root is not a Project: {plan:?}");
    };
    let mut node: &LogicalPlan = input;
    while let LogicalPlan::Filter { input, .. } = node {
        node = input;
    }
    node
}

/// The alias of a `Scan`, which is what a leaf of the join tree is.
fn scan_alias(plan: &LogicalPlan) -> &str {
    match plan {
        LogicalPlan::Scan { alias, .. } => alias,
        other => panic!("not a Scan: {other:?}"),
    }
}

/// The projected columns of `text`: name, index of the `ColumnRef` (`None` for another
/// expression), type.
#[track_caller]
fn projected(text: &str) -> Vec<(String, Option<usize>, TypeInfo)> {
    let plan = plan_of(text);
    let LogicalPlan::Project { exprs, schema, .. } = &plan else {
        panic!("the root is not a Project: {plan:?}");
    };
    assert_eq!(schema.columns.len(), exprs.len(), "{text}");
    exprs
        .iter()
        .enumerate()
        .map(|(i, projection)| {
            assert_eq!(schema.columns[i].name, projection.name, "{text}");
            assert_eq!(schema.columns[i].ty, projection.expr.ty, "{text}");
            let index = match &projection.expr.kind {
                BoundExprKind::ColumnRef(binding) => Some(binding.index),
                _ => None,
            };
            (projection.name.clone(), index, projection.expr.ty.clone())
        })
        .collect()
}

#[track_caller]
fn header(text: &str) -> Vec<String> {
    projected(text).into_iter().map(|(name, ..)| name).collect()
}

// ---------------------------------------------------------------------------------------
// The shape of the tree
// ---------------------------------------------------------------------------------------

/// `FROM a JOIN b ON … JOIN d ON …` is `Join(Join(a, b), d)`: left-deep, in written order,
/// each `ON` a predicate.
#[test]
fn two_tables_bind_to_a_left_deep_join() {
    let plan =
        plan_of("SELECT a.c, b.c, d.e FROM dbo.a JOIN dbo.b ON a.k = b.k JOIN dbo.d ON a.k = d.k");
    let LogicalPlan::Join {
        left,
        right,
        kind: JoinKind::Inner,
        on: Some(outer_on),
        schema,
    } = from_of(&plan)
    else {
        panic!("not an inner join with an ON: {plan:?}");
    };
    assert_eq!(scan_alias(right), "d");
    assert!(outer_on.is_predicate());
    let LogicalPlan::Join {
        left: a,
        right: b,
        kind: JoinKind::Inner,
        on: Some(inner_on),
        ..
    } = &**left
    else {
        panic!("the left input is not an inner join with an ON: {left:?}");
    };
    assert_eq!(scan_alias(a), "a");
    assert_eq!(scan_alias(b), "b");
    assert!(inner_on.is_predicate());
    // The schema of the outer join is the columns of the inner join then those of `d`.
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["k", "c", "k", "c", "k", "e"]);
    // Each kind is carried over as written, a `RIGHT` included.
    for (text, expected) in [
        (
            "SELECT 1 FROM dbo.a INNER JOIN dbo.b ON a.k = b.k",
            JoinKind::Inner,
        ),
        (
            "SELECT 1 FROM dbo.a LEFT JOIN dbo.b ON a.k = b.k",
            JoinKind::Left,
        ),
        (
            "SELECT 1 FROM dbo.a LEFT OUTER JOIN dbo.b ON a.k = b.k",
            JoinKind::Left,
        ),
        (
            "SELECT 1 FROM dbo.a RIGHT JOIN dbo.b ON a.k = b.k",
            JoinKind::Right,
        ),
        (
            "SELECT 1 FROM dbo.a FULL JOIN dbo.b ON a.k = b.k",
            JoinKind::Full,
        ),
        ("SELECT 1 FROM dbo.a CROSS JOIN dbo.b", JoinKind::Cross),
    ] {
        let plan = plan_of(text);
        let LogicalPlan::Join { kind, on, .. } = from_of(&plan) else {
            panic!("{text}: not a join: {plan:?}");
        };
        assert_eq!(*kind, expected, "{text}");
        assert_eq!(on.is_some(), expected != JoinKind::Cross, "{text}");
    }
}

/// `FROM a, b` is a `Join` of kind `Cross` without an `on`; `FROM a, b JOIN d ON …` pairs
/// `a` with what the `JOIN` built, so the comma is the outer node.
#[test]
fn a_comma_is_a_cross_join() {
    let plan = plan_of("SELECT a.c, b.c FROM dbo.a, dbo.b");
    let LogicalPlan::Join {
        left,
        right,
        kind: JoinKind::Cross,
        on: None,
        ..
    } = from_of(&plan)
    else {
        panic!("not a cross join without an ON: {plan:?}");
    };
    assert_eq!(scan_alias(left), "a");
    assert_eq!(scan_alias(right), "b");

    let plan = plan_of("SELECT a.c, b.c, d.e FROM dbo.a, dbo.b JOIN dbo.d ON b.k = d.k");
    let LogicalPlan::Join {
        left,
        right,
        kind: JoinKind::Cross,
        on: None,
        ..
    } = from_of(&plan)
    else {
        panic!("not a cross join without an ON: {plan:?}");
    };
    assert_eq!(scan_alias(left), "a");
    let LogicalPlan::Join {
        kind: JoinKind::Inner,
        on: Some(_),
        ..
    } = &**right
    else {
        panic!("the right input is not the inner join: {right:?}");
    };
    // Three sources through commas alone: `Join(Join(a, b), d)`.
    let plan = plan_of("SELECT 1 FROM dbo.a, dbo.b, dbo.d");
    let LogicalPlan::Join { left, right, .. } = from_of(&plan) else {
        panic!("not a join: {plan:?}");
    };
    assert_eq!(scan_alias(right), "d");
    assert!(matches!(&**left, LogicalPlan::Join { .. }));
}

/// The `ON` of a join sees the sources of that join, not the ones written before a comma
/// and not the ones joined after it.
#[test]
fn a_join_sees_its_own_sides() {
    for (text, name) in [
        ("SELECT 1 FROM dbo.a, dbo.b JOIN dbo.d ON a.k = d.k", "a.k"),
        (
            "SELECT 1 FROM dbo.a JOIN dbo.b ON a.k = d.k JOIN dbo.d ON a.k = d.k",
            "d.k",
        ),
        (
            "SELECT 1 FROM dbo.a JOIN dbo.b ON a.k = nosuch.k",
            "nosuch.k",
        ),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 4104, "{text}: {}", error.message);
        assert!(error.message.contains(name), "{text}: {}", error.message);
    }
    // The same `ON` binds once `d` is on one of its sides, and the one of the first join
    // reads both of its sides.
    assert!(bind_query("SELECT 1 FROM dbo.a, dbo.b JOIN dbo.d ON b.k = d.k").is_ok());
    assert!(
        bind_query("SELECT 1 FROM dbo.a JOIN dbo.b ON a.k = b.k JOIN dbo.d ON b.k = d.k").is_ok()
    );
}

// ---------------------------------------------------------------------------------------
// The schema of a join
// ---------------------------------------------------------------------------------------

/// A `LEFT JOIN` marks the columns of its right side nullable in the schema of the `Join`
/// and in the type of a projection over them; an `INNER JOIN` over the same tables marks
/// nothing, which is the counter-proof. `RIGHT` pads the left side, `FULL` both.
#[test]
fn left_join_makes_the_right_side_nullable() {
    let nullability = |text: &str| -> Vec<bool> {
        let plan = plan_of(text);
        let LogicalPlan::Join { schema, .. } = from_of(&plan) else {
            panic!("{text}: not a join: {plan:?}");
        };
        schema.columns.iter().map(|c| c.ty.nullable).collect()
    };
    // `k` is NOT NULL and `c` is NULL in the catalogue, on both tables.
    let from_catalogue = [false, true, false, true];
    assert_eq!(
        nullability("SELECT 1 FROM dbo.a INNER JOIN dbo.b ON a.k = b.k"),
        from_catalogue
    );
    assert_eq!(
        nullability("SELECT 1 FROM dbo.a CROSS JOIN dbo.b"),
        from_catalogue
    );
    assert_eq!(
        nullability("SELECT 1 FROM dbo.a LEFT JOIN dbo.b ON a.k = b.k"),
        [false, true, true, true]
    );
    assert_eq!(
        nullability("SELECT 1 FROM dbo.a RIGHT JOIN dbo.b ON a.k = b.k"),
        [true, true, false, true]
    );
    assert_eq!(
        nullability("SELECT 1 FROM dbo.a FULL JOIN dbo.b ON a.k = b.k"),
        [true, true, true, true]
    );
    // The projection reads the same nullability: `b.k` is nullable under a `LEFT JOIN`
    // and not under an `INNER JOIN`; `a.k` the other way round under a `RIGHT JOIN`.
    let projected_nullable = |text: &str| projected(text)[0].2.nullable;
    assert!(projected_nullable(
        "SELECT b.k FROM dbo.a LEFT JOIN dbo.b ON a.k = b.k"
    ));
    assert!(!projected_nullable(
        "SELECT b.k FROM dbo.a INNER JOIN dbo.b ON a.k = b.k"
    ));
    assert!(!projected_nullable(
        "SELECT a.k FROM dbo.a LEFT JOIN dbo.b ON a.k = b.k"
    ));
    assert!(projected_nullable(
        "SELECT a.k FROM dbo.a RIGHT JOIN dbo.b ON a.k = b.k"
    ));
    assert!(!projected_nullable(
        "SELECT b.k FROM dbo.a RIGHT JOIN dbo.b ON a.k = b.k"
    ));
    assert!(projected_nullable(
        "SELECT a.k FROM dbo.a FULL JOIN dbo.b ON a.k = b.k"
    ));
    // Left-deep: the right side of the outer `LEFT JOIN` is `d` alone, and the inner
    // `INNER JOIN` keeps `a` and `b` as the catalogue has them.
    assert_eq!(
        nullability("SELECT 1 FROM dbo.a JOIN dbo.b ON a.k = b.k LEFT JOIN dbo.d ON a.k = d.k"),
        [false, true, false, true, true, true]
    );
}

/// A column of the right side indexes the row of the join past the width of the left
/// side: `b.k` is at 2 and `b.c` at 3 over `a (k, c)`, in the select list, in the `WHERE`
/// and in the `ON`; the columns of the left side keep their index.
#[test]
fn a_column_of_the_right_side_indexes_past_the_left_one() {
    let indexes: Vec<Option<usize>> =
        projected("SELECT a.k, a.c, b.k, b.c FROM dbo.a JOIN dbo.b ON a.k = b.k")
            .into_iter()
            .map(|(_, index, _)| index)
            .collect();
    assert_eq!(indexes, [Some(0), Some(1), Some(2), Some(3)]);
    // Three sources: `d` starts at 4.
    let indexes: Vec<Option<usize>> = projected(
        "SELECT d.k, d.e, b.c FROM dbo.a JOIN dbo.b ON a.k = b.k JOIN dbo.d ON a.k = d.k",
    )
    .into_iter()
    .map(|(_, index, _)| index)
    .collect();
    assert_eq!(indexes, [Some(4), Some(5), Some(3)]);
    // The `ON` reads the same indexes.
    let plan = plan_of("SELECT 1 FROM dbo.a JOIN dbo.b ON a.k = b.c");
    let LogicalPlan::Join {
        on:
            Some(BoundExpr {
                kind: BoundExprKind::Compare { left, right, .. },
                ..
            }),
        ..
    } = from_of(&plan)
    else {
        panic!("not a join on a comparison: {plan:?}");
    };
    let index_of = |expr: &BoundExpr| match &expr.kind {
        BoundExprKind::ColumnRef(binding) => binding.index,
        other => panic!("not a column: {other:?}"),
    };
    assert_eq!((index_of(left), index_of(right)), (0, 3));
}

// ---------------------------------------------------------------------------------------
// The scope of several sources
// ---------------------------------------------------------------------------------------

/// A bare name two sources carry is 209, in the select list, in the `WHERE` and in the
/// `ON`, on the line of the column; a bare name one source carries binds.
#[test]
fn an_ambiguous_unqualified_column_is_209() {
    for text in [
        "SELECT c FROM dbo.a JOIN dbo.b ON a.k = b.k",
        "SELECT 1 FROM dbo.a JOIN dbo.b ON a.k = b.k WHERE c = 1",
        "SELECT c FROM dbo.a, dbo.b",
        "SELECT c FROM dbo.a LEFT JOIN dbo.b ON a.k = b.k",
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 209, "{text}: {}", error.message);
        assert_eq!(error.severity, 16, "{text}");
        assert_eq!(error.state, 1, "{text}");
        assert_eq!(
            error.message,
            SqlError::ambiguous_column_name("c").message,
            "{text}"
        );
    }
    let in_on = error_of("SELECT 1 FROM dbo.a JOIN dbo.b ON k = 1");
    assert_eq!(in_on.number, 209, "{}", in_on.message);
    assert_eq!(in_on.message, SqlError::ambiguous_column_name("k").message);
    // The line is the column's, not the statement's.
    let error = error_of("SELECT 1 AS n;\nSELECT\nc\nFROM dbo.a JOIN dbo.b ON a.k = b.k;");
    assert_eq!(error.number, 209);
    assert_eq!(error.line, 3);
    // Counter-proof: `e` is in `d` alone and `c` in `a` alone once `b` is out of the
    // `FROM`; a qualified `c` binds whichever side it names.
    assert_eq!(
        header("SELECT e, c FROM dbo.a JOIN dbo.d ON a.k = d.k"),
        ["e", "c"]
    );
    assert_eq!(
        projected("SELECT a.c, b.c FROM dbo.a JOIN dbo.b ON a.k = b.k")
            .into_iter()
            .map(|(name, index, _)| (name, index))
            .collect::<Vec<_>>(),
        [("c".to_owned(), Some(1)), ("c".to_owned(), Some(3))]
    );
    assert!(
        bind_query("SELECT 1 FROM dbo.a JOIN dbo.b ON a.k = b.k WHERE a.c = 1 AND b.c = 2").is_ok()
    );
}

/// A prefix that names no source of the `FROM` is 4104 on the whole dotted name, on the
/// line of the column: an unknown name, the name an alias hides, and a two-part prefix
/// that does not name the source.
#[test]
fn an_unknown_prefix_is_4104() {
    for (text, name) in [
        ("SELECT x.c FROM dbo.a JOIN dbo.b ON a.k = b.k", "x.c"),
        ("SELECT a.c FROM dbo.a AS x JOIN dbo.b ON x.k = b.k", "a.c"),
        (
            "SELECT dbo.x.c FROM dbo.a AS x JOIN dbo.b ON x.k = b.k",
            "dbo.x.c",
        ),
        (
            "SELECT nosch.a.c FROM dbo.a JOIN dbo.b ON a.k = b.k",
            "nosch.a.c",
        ),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 4104, "{text}: {}", error.message);
        assert_eq!(error.severity, 16, "{text}");
        assert_eq!(error.state, 1, "{text}");
        assert_eq!(
            error.message,
            SqlError::multi_part_identifier(name).message,
            "{text}"
        );
    }
    let error = error_of("SELECT 1 AS n;\nSELECT\nx.c\nFROM dbo.a JOIN dbo.b ON a.k = b.k;");
    assert_eq!(error.number, 4104);
    assert_eq!(error.line, 3);
    // Counter-proof: the alias, the name in another case, and the schema-qualified name
    // each reach their source.
    assert_eq!(header("SELECT A.c FROM dbo.a JOIN dbo.b ON 1 = 1"), ["c"]);
    assert_eq!(
        header("SELECT x.c FROM dbo.a AS x JOIN dbo.b ON x.k = b.k"),
        ["c"]
    );
    assert_eq!(
        header("SELECT dbo.b.c FROM dbo.a JOIN dbo.b ON a.k = b.k"),
        ["c"]
    );
    // A qualified name the source does not carry is 207, the prefix having been bound.
    let error = error_of("SELECT b.e FROM dbo.a JOIN dbo.b ON a.k = b.k");
    assert_eq!(error.number, 207, "{}", error.message);
}

/// `SELECT *` over a join projects the columns of each source in written order, each in
/// catalogue order; `b.*` and `dbo.b.*` those of `b` alone, in place; `x.*` is 107.
#[test]
fn star_expands_every_table_in_written_order() {
    assert_eq!(
        header("SELECT * FROM dbo.a JOIN dbo.b ON 1 = 1"),
        ["k", "c", "k", "c"]
    );
    assert_eq!(
        header("SELECT * FROM dbo.b JOIN dbo.a ON 1 = 1 JOIN dbo.d ON 1 = 1"),
        ["k", "c", "k", "c", "k", "e"]
    );
    assert_eq!(header("SELECT * FROM dbo.d, dbo.a"), ["k", "e", "k", "c"]);
    // The expanded references index the row of the join.
    let indexes: Vec<Option<usize>> = projected("SELECT * FROM dbo.a JOIN dbo.b ON 1 = 1")
        .into_iter()
        .map(|(_, index, _)| index)
        .collect();
    assert_eq!(indexes, [Some(0), Some(1), Some(2), Some(3)]);
    // `b.*` is the right side alone, at its index, where it was written.
    assert_eq!(
        projected("SELECT b.*, a.k FROM dbo.a JOIN dbo.b ON a.k = b.k")
            .into_iter()
            .map(|(name, index, _)| (name, index))
            .collect::<Vec<_>>(),
        [
            ("k".to_owned(), Some(2)),
            ("c".to_owned(), Some(3)),
            ("k".to_owned(), Some(0))
        ]
    );
    assert_eq!(
        header("SELECT dbo.b.* FROM dbo.a JOIN dbo.b ON a.k = b.k"),
        ["k", "c"]
    );
    assert_eq!(
        header("SELECT y.* FROM dbo.a AS x JOIN dbo.a AS y ON x.k = y.k"),
        ["k", "c"]
    );
    // A `*` under a `LEFT JOIN` carries the nullability of the join.
    let nullable: Vec<bool> = projected("SELECT * FROM dbo.a LEFT JOIN dbo.b ON a.k = b.k")
        .into_iter()
        .map(|(_, _, ty)| ty.nullable)
        .collect();
    assert_eq!(nullable, [false, true, true, true]);
    // An unknown prefix on a wildcard is 107, on the line of the wildcard.
    for text in [
        "SELECT x.* FROM dbo.a JOIN dbo.b ON a.k = b.k",
        "SELECT a.* FROM dbo.a AS x JOIN dbo.b ON x.k = b.k",
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 107, "{text}: {}", error.message);
        assert_eq!(error.severity, 15, "{text}");
    }
    let error = error_of("SELECT 1 AS n;\nSELECT\nx.*\nFROM dbo.a JOIN dbo.b ON a.k = b.k;");
    assert_eq!(error.number, 107);
    assert_eq!(error.line, 3);
}

/// Two sources of one exposed name: 1013 when both are table names, 1011 when both are
/// aliases, 1012 when one of them is; each on the line the statement starts on, each
/// after the 208 of a name that reaches nothing, and after the errors of an `ON` bound
/// before the second source was added.
#[test]
fn a_duplicate_exposed_name_is_1011_1012_or_1013() {
    let check = |text: &str, number: u32, message: &SqlError| {
        let error = error_of(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
        assert_eq!(error.severity, 16, "{text}");
        assert_eq!(error.state, 1, "{text}");
        assert_eq!(error.message, message.message, "{text}");
    };
    // 1013: the source that came second is printed first, as written.
    check(
        "SELECT k FROM dbo.a JOIN dbo.a ON 1 = 1",
        1013,
        &SqlError::same_exposed_names("dbo.a", "dbo.a"),
    );
    check(
        "SELECT 1 FROM dbo.a, dbo.a",
        1013,
        &SqlError::same_exposed_names("dbo.a", "dbo.a"),
    );
    check(
        "SELECT 1 FROM a JOIN dbo.a ON 1 = 1",
        1013,
        &SqlError::same_exposed_names("dbo.a", "a"),
    );
    check(
        "SELECT 1 FROM dbo.a JOIN dbo.d ON 1 = 1 JOIN [a] ON 1 = 1",
        1013,
        &SqlError::same_exposed_names("a", "dbo.a"),
    );
    check(
        "SELECT 1 FROM dbo.a, dbo.d JOIN dbo.a ON 1 = 1",
        1013,
        &SqlError::same_exposed_names("dbo.a", "dbo.a"),
    );
    check(
        "SELECT 1 FROM master.dbo.a JOIN dbo.A ON 1 = 1",
        1013,
        &SqlError::same_exposed_names("dbo.A", "master.dbo.a"),
    );
    // 1011: the alias that came second, as written.
    check(
        "SELECT x.k FROM dbo.a AS x JOIN dbo.b AS X ON 1 = 1",
        1011,
        &SqlError::duplicate_correlation_name("X"),
    );
    check(
        "SELECT 1 FROM dbo.a AS x, dbo.b AS x",
        1011,
        &SqlError::duplicate_correlation_name("x"),
    );
    // 1012: the alias then the table, whichever came first.
    check(
        "SELECT 1 FROM dbo.a AS B JOIN dbo.b ON 1 = 1",
        1012,
        &SqlError::correlation_name_is_a_table_name("B", "dbo.b"),
    );
    check(
        "SELECT 1 FROM b JOIN dbo.a AS b ON 1 = 1",
        1012,
        &SqlError::correlation_name_is_a_table_name("b", "b"),
    );
    // The line is the statement's: `SELECT` on line 2, the second `dbo.a` on line 4.
    let error = error_of("SELECT 1 AS n;\nSELECT\n1\nFROM dbo.a JOIN\ndbo.a ON 1 = 1;");
    assert_eq!(error.number, 1013);
    assert_eq!(error.line, 2);
    // Each name is resolved first: 208 wins over the comparison.
    for text in [
        "SELECT 1 FROM nosuch JOIN nosuch ON 1 = 1",
        "SELECT 1 FROM dbo.a JOIN nosuch ON 1 = 1 JOIN dbo.a ON 1 = 1",
    ] {
        assert_eq!(error_of(text).number, 208, "{text}");
    }
    // The comparison comes before the `ON` of its own join, and after the `ON` of the
    // joins nested under it.
    assert_eq!(
        error_of("SELECT 1 FROM dbo.a JOIN dbo.a ON x.k = 1").number,
        1013
    );
    assert_eq!(
        error_of("SELECT 1 FROM dbo.a JOIN dbo.b ON k = 1 JOIN dbo.a ON 1 = 1").number,
        209
    );
    // Counter-proof: two aliases that differ, and a table whose name differs from an
    // alias in case alone under an alias of its own, bind.
    assert!(bind_query("SELECT x.k, y.k FROM dbo.a AS x JOIN dbo.a AS y ON x.k = y.k").is_ok());
    assert!(bind_query("SELECT 1 FROM dbo.a AS b JOIN dbo.b AS a ON 1 = 1").is_ok());
}

/// An `ON` that is a value and not a predicate is 4145, quoting the token that follows
/// it, on the line of that token.
#[test]
fn an_on_that_is_not_a_predicate_is_4145() {
    let error = error_of("SELECT a.c FROM dbo.a JOIN dbo.b ON 1;");
    assert_eq!(error.number, 4145, "{}", error.message);
    assert_eq!(error.severity, 15);
    assert_eq!(error.state, 1);
    assert_eq!(error.message, SqlError::non_boolean_expression(";").message);
    let error = error_of("SELECT a.c FROM dbo.a JOIN dbo.b ON a.k JOIN dbo.d ON 1 = 1;");
    assert_eq!(error.number, 4145, "{}", error.message);
    assert_eq!(
        error.message,
        SqlError::non_boolean_expression("JOIN").message
    );
    let error = error_of("SELECT 1 AS n;\nSELECT\n1\nFROM dbo.a JOIN dbo.b ON\n1;");
    assert_eq!(error.number, 4145);
    assert_eq!(error.line, 5);
    // Counter-proof: a predicate that names one side alone, and one over a `CROSS JOIN`
    // written as a `WHERE`, bind.
    assert!(bind_query("SELECT a.c FROM dbo.a JOIN dbo.b ON a.k = 1").is_ok());
    assert!(bind_query("SELECT 1 FROM dbo.a CROSS JOIN dbo.b WHERE a.k = b.k").is_ok());
}

/// A `CROSS JOIN` with an `ON` and a `JOIN` without one do not reach the binder: the
/// parser refuses them, 156 near `ON` and 102 near the token that follows.
#[test]
fn a_cross_join_with_an_on_and_a_join_without_one_are_syntax_errors() {
    for (text, number) in [
        (
            "SELECT a.c, b.c FROM dbo.a CROSS JOIN dbo.b ON a.k = b.k;",
            156,
        ),
        ("SELECT a.c, b.c FROM dbo.a JOIN dbo.b;", 102),
        (
            "SELECT 1 AS n; SELECT a.c FROM dbo.a JOIN dbo.b; SELECT 2 AS n;",
            102,
        ),
        ("SELECT a.c FROM dbo.a LEFT JOIN dbo.b;", 102),
    ] {
        let error = parse_batch(text, &ParseOptions::default()).expect_err(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
    }
}

/// A hint word glued to a name, and a `WITH (…)` list, are accepted on either side of a
/// join as they are on a single-table `FROM`; each `Scan` carries the hint of its side.
#[test]
fn a_hint_word_on_either_side_binds() {
    for text in [
        "SELECT a.c FROM dbo.a (NOLOCK) JOIN dbo.b WITH (NOLOCK) ON a.k = b.k",
        "SELECT a.c FROM dbo.a WITH (NOLOCK) JOIN dbo.b (NOLOCK) ON a.k = b.k",
        "SELECT a.c FROM dbo.a (NOLOCK), dbo.b (NOLOCK)",
        "SELECT x.c FROM dbo.a (NOLOCK) AS x JOIN dbo.b AS y (NOLOCK) ON x.k = y.k",
    ] {
        let plan = plan_of(text);
        let LogicalPlan::Join { left, right, .. } = from_of(&plan) else {
            panic!("{text}: not a join: {plan:?}");
        };
        for side in [&**left, &**right] {
            let LogicalPlan::Scan { hints, .. } = side else {
                panic!("{text}: not a Scan: {side:?}");
            };
            assert_eq!(
                *hints,
                vauban_binder::LockHints {
                    nolock: true,
                    ..vauban_binder::LockHints::default()
                },
                "{text}"
            );
        }
    }
    // The errors of an argument list keep their numbers on either side: 215 for a
    // value, 207 for a column.
    assert_eq!(
        error_of("SELECT 1 FROM dbo.a JOIN dbo.b (1) ON a.k = b.k").number,
        215
    );
    assert_eq!(
        error_of("SELECT 1 FROM dbo.a (x) JOIN dbo.b ON a.k = b.k").number,
        207
    );
}

/// A `WHERE` over a join reads both sides, and a `TOP` reads neither: the row count of a
/// `TOP` binds against no source.
#[test]
fn the_where_reads_both_sides_and_the_top_neither() {
    let plan = plan_of("SELECT a.c FROM dbo.a LEFT JOIN dbo.d ON a.k = d.k WHERE e IS NULL");
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("not a Project: {plan:?}");
    };
    assert!(matches!(&**input, LogicalPlan::Filter { .. }));
    assert!(bind_query("SELECT TOP 1 a.k FROM dbo.a JOIN dbo.b ON a.k = b.k").is_ok());
    assert_eq!(
        error_of("SELECT TOP (k) 1 FROM dbo.a JOIN dbo.b ON a.k = b.k").number,
        207
    );
}
