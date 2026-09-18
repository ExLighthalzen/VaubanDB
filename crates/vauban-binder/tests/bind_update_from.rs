//! `UPDATE … FROM` and `DELETE … FROM` whose `FROM` reads more than one source: which
//! source the target names, what each part of the statement sees, and the order and the line
//! of the errors they raise.
//!
//! The double of the catalogue knows four tables and one view in `master.dbo`:
//! `t (k int NOT NULL, a int NULL)`, `u (k int NOT NULL, b int NULL, c int NULL)`,
//! `ti (id int IDENTITY NOT NULL, k int NOT NULL, a int NULL)`, `tc (k int NOT NULL,
//! a int NULL)` whose `a` is computed, and the view `v`. The bound nodes derive no
//! `PartialEq`: a shape is checked by pattern matching.

use vauban_binder::{
    BindContext, BoundExpr, BoundExprKind, BoundStatement, CatalogView, ColumnBinding, DeletePlan,
    JoinKind, LogicalPlan, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
    UpdatePlan, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{Ident, ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{SqlType, TypeInfo};

/// The tables of the module documentation, in `master.dbo`.
struct Tables;

fn column(id: i32, index: usize, name: &str, ty: SqlType, nullable: bool) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(id),
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
    }
}

impl CatalogView for Tables {
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
        let (object, columns, kind) = match name.name.value.to_ascii_lowercase().as_str() {
            "t" => (
                1,
                vec![
                    column(1, 0, "k", SqlType::Int, false),
                    column(2, 1, "a", SqlType::Int, true),
                ],
                ResolvedTableKind::Table,
            ),
            "u" => (
                2,
                vec![
                    column(1, 0, "k", SqlType::Int, false),
                    column(2, 1, "b", SqlType::Int, true),
                    column(3, 2, "c", SqlType::Int, true),
                ],
                ResolvedTableKind::Table,
            ),
            "ti" => (
                3,
                vec![
                    column(1, 0, "id", SqlType::Int, false),
                    column(2, 1, "k", SqlType::Int, false),
                    column(3, 2, "a", SqlType::Int, true),
                ],
                ResolvedTableKind::Table,
            ),
            "tc" => (
                4,
                vec![
                    column(1, 0, "k", SqlType::Int, false),
                    column(2, 1, "a", SqlType::Int, true),
                ],
                ResolvedTableKind::Table,
            ),
            "v" => (5, Vec::new(), ResolvedTableKind::View),
            _ => return None,
        };
        let table = (kind == ResolvedTableKind::Table)
            .then(|| TableId(u32::try_from(object).expect("a small identifier")));
        Some(ResolvedTable {
            object: ObjectId(object),
            table,
            columns,
            kind,
        })
    }

    fn view_definition(&self, object: ObjectId) -> Option<&str> {
        (object == ObjectId(5)).then_some("SELECT k, a FROM dbo.t")
    }

    fn identity_column(&self, object: ObjectId) -> Option<ColumnId> {
        (object == ObjectId(3)).then_some(ColumnId(1))
    }

    fn computed_columns(&self, object: ObjectId) -> Vec<ColumnId> {
        if object == ObjectId(4) {
            vec![ColumnId(2)]
        } else {
            Vec::new()
        }
    }
}

/// Binds the statements of `text` in order, in `master` and `dbo`: the last bound
/// statement, or the first error.
fn bind_text(text: &str) -> Result<BoundStatement, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let catalog = Tables;
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
        last = Some(bind(statement, &ctx)?);
    }
    Ok(last.expect("one statement"))
}

#[track_caller]
fn update_of(text: &str) -> UpdatePlan {
    match bind_text(text) {
        Ok(BoundStatement::Update(plan)) => plan,
        Ok(other) => panic!("{text}: not an UPDATE: {other:?}"),
        Err(e) => panic!("{text}: {e:?}"),
    }
}

#[track_caller]
fn delete_of(text: &str) -> DeletePlan {
    match bind_text(text) {
        Ok(BoundStatement::Delete(plan)) => plan,
        Ok(other) => panic!("{text}: not a DELETE: {other:?}"),
        Err(e) => panic!("{text}: {e:?}"),
    }
}

#[track_caller]
fn error_of(text: &str) -> SqlError {
    match bind_text(text) {
        Ok(bound) => panic!("{text} binds: {bound:?}"),
        Err(e) => e,
    }
}

/// The `Join` under `input`, below the `Filter` of a `WHERE` when one was written, and
/// whether that `Filter` is there.
#[track_caller]
fn join_of(input: &LogicalPlan) -> (&LogicalPlan, bool) {
    match input {
        LogicalPlan::Filter { input, .. } => (join_of(input).0, true),
        LogicalPlan::Join { .. } => (input, false),
        other => panic!("neither a Join nor a Filter: {other:?}"),
    }
}

/// The name of every column a `ColumnRef` of `expr` reads, at any depth.
fn columns_read(expr: &BoundExpr, into: &mut Vec<String>) {
    match &expr.kind {
        BoundExprKind::ColumnRef(binding) => into.push(binding.name.clone()),
        BoundExprKind::Convert { expr, .. } => columns_read(expr, into),
        BoundExprKind::Arith { left, right, .. } => {
            columns_read(left, into);
            columns_read(right, into);
        }
        BoundExprKind::Compare { left, right, .. } => {
            columns_read(left, into);
            columns_read(right, into);
        }
        BoundExprKind::Logical { left, right, .. } => {
            columns_read(left, into);
            columns_read(right, into);
        }
        BoundExprKind::Negate(inner) | BoundExprKind::BitNot(inner) | BoundExprKind::Not(inner) => {
            columns_read(inner, into);
        }
        BoundExprKind::Function { args, .. } => {
            for arg in args {
                columns_read(arg, into);
            }
        }
        _ => {}
    }
}

/// The index each `ColumnRef` of `expr` carries, in the order they are read.
fn indexes_read(expr: &BoundExpr) -> Vec<usize> {
    fn walk(expr: &BoundExpr, into: &mut Vec<usize>) {
        match &expr.kind {
            BoundExprKind::ColumnRef(binding) => into.push(binding.index),
            BoundExprKind::Convert { expr, .. } => walk(expr, into),
            BoundExprKind::Arith { left, right, .. } => {
                walk(left, into);
                walk(right, into);
            }
            _ => {}
        }
    }
    let mut indexes = Vec::new();
    walk(expr, &mut indexes);
    indexes
}

// ---------------------------------------------------------------------------------------
// Which source the target names
// ---------------------------------------------------------------------------------------

/// The target written as the alias of a source is that source's table, and the plan the
/// statement writes through is the `Join` of the `FROM`, under the `Filter` of the `WHERE`.
///
/// The counter-proof that the target is read off the `FROM` and not off its own name: the
/// alias `x` names no table of the catalogue, and `UPDATE x SET a = 1;` without a `FROM`
/// answers 208.
#[test]
fn update_from_join_binds_target_by_alias() {
    let plan = update_of(
        "UPDATE x SET x.a = y.b FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE y.c > 0;",
    );
    assert_eq!(plan.table, TableId(1), "the table of the alias x");
    let (join, filtered) = join_of(&plan.input);
    assert!(filtered, "the WHERE filters the join");
    let LogicalPlan::Join {
        left, right, kind, ..
    } = join
    else {
        unreachable!("join_of answered a Join");
    };
    assert_eq!(*kind, JoinKind::Inner);
    assert!(matches!(left.as_ref(), LogicalPlan::Scan { .. }));
    assert!(matches!(right.as_ref(), LogicalPlan::Scan { .. }));
    assert_eq!(error_of("UPDATE x SET a = 1;").number, 208);
    // Without a `WHERE` the `Join` is the input itself.
    let plan = update_of("UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;");
    assert!(matches!(plan.input.as_ref(), LogicalPlan::Join { .. }));
    // One source under a `FROM`, the target its alias: the input is that source alone.
    let plan = update_of("UPDATE x SET x.a = 1 FROM dbo.t AS x;");
    assert_eq!(plan.table, TableId(1));
    assert!(matches!(plan.input.as_ref(), LogicalPlan::Scan { .. }));
}

/// A target written as the **name** of a source names that source, the parts it does not
/// carry completed from the context, and an alias on that source hides nothing from it.
///
/// The shape that separates this rule from a match on the exposed name is the third: the
/// source carries the alias `x`, and the target `dbo.t` reaches it all the same, where a
/// match on the exposed name would find nothing.
#[test]
fn update_from_binds_a_target_written_as_the_name_of_a_source() {
    for text in [
        "UPDATE dbo.t SET a = y.b FROM dbo.t JOIN dbo.u AS y ON t.k = y.k;",
        "UPDATE t SET a = y.b FROM dbo.t JOIN dbo.u AS y ON t.k = y.k;",
        "UPDATE dbo.t SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "UPDATE t SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "UPDATE master.dbo.t SET a = 1 FROM dbo.t JOIN dbo.u AS y ON t.k = y.k;",
        "UPDATE dbo.t SET a = 1 FROM master.dbo.t JOIN dbo.u AS y ON t.k = y.k;",
        "UPDATE DBO.T SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
    ] {
        assert_eq!(update_of(text).table, TableId(1), "{text}");
    }
    // The target on the right side of the join, and a source the target is not.
    let plan = update_of("UPDATE y SET y.b = x.a FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;");
    assert_eq!(plan.table, TableId(2), "the table of the alias y");
}

/// The value written into a column of the target is a `ColumnRef` of the **other** source,
/// carried under a `Convert` to the type of the column assigned.
#[test]
fn update_from_reads_the_other_table() {
    let plan = update_of("UPDATE x SET x.a = y.b FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;");
    let [(column, value)] = plan.assignments.as_slice() else {
        panic!("one assignment: {:?}", plan.assignments);
    };
    assert_eq!(column.name, "a");
    assert_eq!(column.index, 1, "the ordinal of `a` in the target");
    let mut read = Vec::new();
    columns_read(value, &mut read);
    assert_eq!(read, ["b"], "the value reads a column of y");
    // The column of `y` indexes the row of the `Join`: the left input is two columns wide,
    // so `b`, second column of `u`, is index 3 there. Its ordinal in `u` alone is 1, which
    // is what tells the two readings apart.
    assert_eq!(indexes_read(value), [3]);
    // Both sources are readable on the right side, and an expression over the two binds.
    let plan =
        update_of("UPDATE x SET x.a = y.b + y.c FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;");
    let mut read = Vec::new();
    columns_read(&plan.assignments[0].1, &mut read);
    assert_eq!(read, ["b", "c"]);
}

/// A target the `FROM` does not read answers **208** when the catalogue holds no such table.
#[test]
fn update_target_absent_from_from_error_number() {
    for text in [
        "UPDATE z SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "UPDATE dbo.nosuch SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "DELETE z FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "UPDATE nodb.dbo.t SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 208, "{text}: {}", error.message);
        assert_eq!(error.severity, 16, "{text}");
        assert_eq!(error.state, 1, "{text}");
        assert_eq!(error.line, 1, "{text}");
    }
    assert_eq!(
        error_of("UPDATE z SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;").message,
        "Unknown object name 'z'."
    );
    assert_eq!(
        update_of("UPDATE dbo.u SET b = 1 FROM dbo.u AS y JOIN dbo.t AS x ON x.k = y.k;").table,
        TableId(2)
    );
}

/// A target the `FROM` does not read but the catalogue holds is the table the name resolves to.
#[test]
fn update_target_outside_the_from_binds_the_catalogue_table() {
    for text in [
        "UPDATE dbo.u SET b = 1 FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k;",
        "UPDATE dbo.u SET b = 1 FROM dbo.t JOIN dbo.t AS y ON t.k = y.k;",
    ] {
        let plan = update_of(text);
        assert_eq!(plan.table, TableId(2), "{text}");
        assert!(
            matches!(plan.input.as_ref(), LogicalPlan::Join { .. }),
            "{text}"
        );
    }
    for text in [
        "DELETE dbo.u FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k;",
        "DELETE dbo.u FROM dbo.t JOIN dbo.t AS y ON t.k = y.k;",
    ] {
        let plan = delete_of(text);
        assert_eq!(plan.table, TableId(2), "{text}");
        assert!(
            matches!(plan.input.as_ref(), LogicalPlan::Join { .. }),
            "{text}"
        );
    }
}

/// One-part target `t` when `dbo.u AS t` exposes the same name as unaliased `dbo.t`: the `FROM`
/// answers **1012** before the target is read (`a_duplicate_exposed_name_is_1011_1012_or_1013`).
#[test]
fn update_from_target_t_when_u_is_aliased_as_t_from_refuses_1012() {
    for text in [
        "UPDATE t SET a = 1 FROM dbo.t JOIN dbo.u AS t ON t.k = 1;",
        "UPDATE t SET b = 1 FROM dbo.t JOIN dbo.u AS t ON t.k = 1;",
    ] {
        assert_eq!(error_of(text).number, 1012, "{text}");
    }
}

/// One-part target `t` when only `dbo.u AS t` matches by alias, not `dbo.t AS x` by table name:
/// the target is the table of the alias (`u`), and a column the target does not carry is **207**.
#[test]
fn update_from_target_t_when_x_and_u_as_t_set_a_is_207() {
    let error = error_of("UPDATE t SET a = 1 FROM dbo.t AS x JOIN dbo.u AS t ON x.k = t.k;");
    assert_eq!(error.number, 207, "{}", error.message);
    assert!(error.message.contains("a"), "{}", error.message);
}

/// One-part target `t` over `FROM dbo.t AS x JOIN dbo.u AS t`: the alias `t` names `u`, and
/// `SET b` binds a column of that target.
#[test]
fn update_from_target_t_when_x_and_u_as_t_set_b_binds_u() {
    let plan = update_of("UPDATE t SET b = 1 FROM dbo.t AS x JOIN dbo.u AS t ON x.k = t.k;");
    assert_eq!(plan.table, TableId(2), "the table of the alias t on u");
    assert_eq!(plan.assignments[0].0.name, "b");
}

/// When the `FROM` reads the target's table twice, the target is named by the alias of one of
/// them, or by the name of the source that carries no alias. A bare name that reaches both
/// answers **8154**, printing the target as it was written.
///
/// The two sources without any alias answer 1013 before the target is looked at: the `FROM`
/// itself refuses two sources of one exposed name. What separates 8154 from 1013 is which of
/// the two sources carries an alias, not the target.
#[test]
fn update_self_join_requires_the_alias() {
    let plan = update_of("UPDATE x SET x.a = y.a FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k;");
    assert_eq!(plan.table, TableId(1));
    // The source without an alias is the target, where the other carries one.
    assert_eq!(
        update_of("UPDATE dbo.t SET a = 1 FROM dbo.t JOIN dbo.t AS y ON t.k = y.k;").table,
        TableId(1)
    );
    for (text, printed) in [
        (
            "UPDATE dbo.t SET a = 1 FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k;",
            "dbo.t",
        ),
        (
            "UPDATE t SET a = 1 FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k;",
            "t",
        ),
        (
            "UPDATE dbo.t SET a = 1 FROM dbo.t AS x, dbo.t AS y;",
            "dbo.t",
        ),
        (
            "DELETE dbo.t FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k;",
            "dbo.t",
        ),
        (
            "DELETE FROM dbo.t FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k;",
            "dbo.t",
        ),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 8154, "{text}: {}", error.message);
        assert_eq!(error.severity, 16, "{text}");
        assert_eq!(error.state, 1, "{text}");
        assert_eq!(error.line, 1, "{text}");
        assert_eq!(
            error.message,
            format!("The reference to table '{printed}' is ambiguous."),
            "{text}"
        );
    }
    // Neither source carries an alias: the FROM answers 1013 first, whatever the target.
    for text in [
        "UPDATE dbo.t SET a = 1 FROM dbo.t JOIN dbo.t ON 1 = 1;",
        "UPDATE z SET a = 1 FROM dbo.t JOIN dbo.t ON 1 = 1;",
    ] {
        assert_eq!(error_of(text).number, 1013, "{text}");
    }
}

// ---------------------------------------------------------------------------------------
// What each part of the statement sees
// ---------------------------------------------------------------------------------------

/// The left side of a `SET` is a column of the target, bare or qualified by the name the
/// target is exposed by: a qualifier naming another source is **4104**, a column only that
/// other source carries is **207**, and a column both sources carry is neither, where the
/// same bare name in the `WHERE` is 209.
///
/// The last pair is what separates the two scopes: `SET k = 1` binds while `WHERE k = 1`
/// answers 209 over the same `FROM`.
#[test]
fn set_left_side_must_be_a_target_column() {
    for (text, number, printed) in [
        (
            "UPDATE x SET y.b = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            4104,
            "y.b",
        ),
        (
            "UPDATE x SET z.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            4104,
            "z.a",
        ),
        (
            "UPDATE x SET t.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            4104,
            "t.a",
        ),
        (
            "UPDATE x SET dbo.t.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            4104,
            "dbo.t.a",
        ),
        (
            "UPDATE x SET x.nocol = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            207,
            "nocol",
        ),
        (
            "UPDATE x SET b = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            207,
            "b",
        ),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
        assert!(error.message.contains(printed), "{text}: {}", error.message);
    }
    // Bare, and qualified by the exposure of the target: the same column of the target.
    for text in [
        "UPDATE x SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "UPDATE dbo.t SET t.a = 1 FROM dbo.t JOIN dbo.u AS y ON t.k = y.k;",
        "UPDATE dbo.t SET dbo.t.a = 1 FROM dbo.t JOIN dbo.u AS y ON t.k = y.k;",
    ] {
        let plan = update_of(text);
        assert_eq!(plan.assignments.len(), 1, "{text}");
        assert_eq!(plan.assignments[0].0.name, "a", "{text}");
        assert_eq!(plan.assignments[0].0.index, 1, "{text}");
    }
    // A name both sources carry is a column of the target on the left, and 209 in the WHERE.
    let plan = update_of("UPDATE x SET k = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;");
    assert_eq!(plan.assignments[0].0.name, "k");
    assert_eq!(plan.assignments[0].0.index, 0, "the ordinal in the target");
    assert_eq!(
        error_of("UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE k = 1;")
            .number,
        209
    );
    // The checks of the one-table form still run on the target of a joined statement.
    assert_eq!(
        error_of("UPDATE x SET x.id = 1 FROM dbo.ti AS x JOIN dbo.u AS y ON x.k = y.k;").number,
        8102
    );
    assert_eq!(
        error_of("UPDATE x SET x.a = 1 FROM dbo.tc AS x JOIN dbo.u AS y ON x.k = y.k;").number,
        271
    );
    for text in [
        "UPDATE x SET x.a = 1, x.a = 2 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "UPDATE x SET a = 1, x.a = 2 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
    ] {
        assert_eq!(error_of(text).number, 264, "{text}");
    }
}

/// A bare column two sources of the `FROM` carry is **209**, in the `WHERE`, in the value of
/// a `SET` and in the `ON`: the rule `join.rs` states, which this file does not redefine.
/// A qualifier naming no source is 4104 and an unknown column is 207.
#[test]
fn ambiguous_column_in_where_is_209() {
    for text in [
        "UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE k = 1;",
        "UPDATE x SET x.a = k FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
        "UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON k = 1;",
        "DELETE x FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE k = 1;",
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 209, "{text}: {}", error.message);
        assert_eq!(error.message, "Column name 'k' is ambiguous.", "{text}");
    }
    for (text, number) in [
        (
            "UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE z.k = 1;",
            4104,
        ),
        (
            "UPDATE x SET x.a = z.k FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            4104,
        ),
        (
            "UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE nocol = 1;",
            207,
        ),
        (
            "UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE 1;",
            4145,
        ),
    ] {
        assert_eq!(error_of(text).number, number, "{text}");
    }
    // A column of the other source binds in the WHERE, which is the counter-proof of the
    // four refusals above.
    assert!(
        bind_text(
            "UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE y.c > 0;"
        )
        .is_ok()
    );
}

// ---------------------------------------------------------------------------------------
// DELETE
// ---------------------------------------------------------------------------------------

/// `DELETE <target> FROM …` and `DELETE FROM <target> FROM …` are one statement: the target
/// is resolved as an `UPDATE`'s, and the plan is the `Join` of the second `FROM` under the
/// `Filter` of the `WHERE`.
#[test]
fn delete_from_join_binds() {
    for text in [
        "DELETE x FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE y.c > 0;",
        "DELETE FROM x FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE y.c > 0;",
    ] {
        let plan = delete_of(text);
        assert_eq!(plan.table, TableId(1), "{text}");
        let (join, filtered) = join_of(&plan.input);
        assert!(filtered, "{text}: the WHERE filters the join");
        assert!(matches!(join, LogicalPlan::Join { .. }), "{text}");
    }
    for text in [
        "DELETE dbo.t FROM dbo.t JOIN dbo.u AS y ON t.k = y.k;",
        "DELETE FROM dbo.t FROM dbo.t JOIN dbo.u AS y ON t.k = y.k;",
        "DELETE t FROM dbo.t JOIN dbo.u AS y ON t.k = y.k;",
    ] {
        let plan = delete_of(text);
        assert_eq!(plan.table, TableId(1), "{text}");
        assert!(
            matches!(plan.input.as_ref(), LogicalPlan::Join { .. }),
            "{text}"
        );
    }
    // The target on the right of the join, and a comma in place of the JOIN.
    assert_eq!(
        delete_of("DELETE y FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;").table,
        TableId(2)
    );
    assert_eq!(
        delete_of("DELETE x FROM dbo.t AS x, dbo.u AS y WHERE x.k = y.k;").table,
        TableId(1)
    );
}

// ---------------------------------------------------------------------------------------
// The shape of the FROM, the order of the checks, the line of each error
// ---------------------------------------------------------------------------------------

/// The `FROM` of a joined statement is the one `join.rs` binds: a comma, the five join
/// kinds, three sources, and a hint list on a source. The kind written is the kind carried,
/// and a `LEFT JOIN` makes the right side nullable, which the value of a `SET` shows.
#[test]
fn the_from_of_a_joined_statement_is_the_from_of_a_query() {
    for (text, kind) in [
        (
            "UPDATE x SET x.a = y.b FROM dbo.t AS x, dbo.u AS y WHERE x.k = y.k;",
            JoinKind::Cross,
        ),
        (
            "UPDATE x SET x.a = y.b FROM dbo.t AS x CROSS JOIN dbo.u AS y;",
            JoinKind::Cross,
        ),
        (
            "UPDATE x SET x.a = y.b FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            JoinKind::Inner,
        ),
        (
            "UPDATE x SET x.a = y.b FROM dbo.t AS x LEFT JOIN dbo.u AS y ON x.k = y.k;",
            JoinKind::Left,
        ),
        (
            "UPDATE x SET x.a = y.b FROM dbo.t AS x RIGHT JOIN dbo.u AS y ON x.k = y.k;",
            JoinKind::Right,
        ),
        (
            "UPDATE x SET x.a = y.b FROM dbo.t AS x FULL JOIN dbo.u AS y ON x.k = y.k;",
            JoinKind::Full,
        ),
    ] {
        let plan = update_of(text);
        let (join, _) = join_of(&plan.input);
        let LogicalPlan::Join { kind: bound, .. } = join else {
            unreachable!("join_of answered a Join");
        };
        assert_eq!(*bound, kind, "{text}");
    }
    // Three sources make a left-deep tree; the target is the one its name reaches.
    let plan = update_of(
        "UPDATE y SET y.b = x.a FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k \
         JOIN dbo.tc AS w ON w.k = y.k;",
    );
    assert_eq!(plan.table, TableId(2));
    let LogicalPlan::Join { left, .. } = plan.input.as_ref() else {
        panic!("the input is not a Join: {:?}", plan.input);
    };
    assert!(matches!(left.as_ref(), LogicalPlan::Join { .. }));
    // A hint list on a source of the FROM is read there, and the statement binds.
    assert_eq!(
        update_of(
            "UPDATE x SET x.a = y.b FROM dbo.t AS x JOIN dbo.u AS y WITH (NOLOCK) ON x.k = y.k;"
        )
        .table,
        TableId(1)
    );
    assert_eq!(
        update_of("UPDATE x SET x.a = 1 FROM dbo.t AS x (NOLOCK);").table,
        TableId(1)
    );
    // A view as a source of the FROM is expanded, as it is under a SELECT.
    assert_eq!(
        update_of("UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.v AS y ON x.k = y.k;").table,
        TableId(1)
    );
    // A view as the target is not bound here, whether the FROM names it or not.
    assert_eq!(
        error_of("UPDATE dbo.v SET a = 1 FROM dbo.v JOIN dbo.u AS y ON v.k = y.k;").number,
        50000
    );
}

/// A statement that carries several faults reports the one the order of the section gives:
/// the `FROM` first, then the target, then the `WHERE`, then the `SET` list.
#[test]
fn the_faults_of_a_joined_statement_are_reported_in_order() {
    for (text, number) in [
        // The FROM before the target: the 208 is the source's, not the target's.
        (
            "UPDATE z SET a = 1 FROM dbo.nosuch AS x JOIN dbo.u AS y ON x.k = y.k;",
            208,
        ),
        // The ON before everything the target and the SET list raise.
        (
            "UPDATE x SET nocol = 1 FROM dbo.t AS x JOIN dbo.u AS y ON k = 1;",
            209,
        ),
        // The target before the WHERE and before the left side of a SET.
        (
            "UPDATE dbo.t SET a = 1 FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k WHERE nocol = 1;",
            8154,
        ),
        (
            "UPDATE dbo.t SET y.a = 1 FROM dbo.t AS x JOIN dbo.t AS y ON x.k = y.k;",
            8154,
        ),
        // The WHERE before the left side of a SET.
        (
            "UPDATE x SET nocol = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE nocol2 = 1;",
            207,
        ),
        // The left side before the value.
        (
            "UPDATE x SET nocol = nocol2 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            207,
        ),
        // The IDENTITY before the repeated column.
        (
            "UPDATE x SET x.id = 1, x.a = 1, x.a = 2 FROM dbo.ti AS x JOIN dbo.u AS y ON x.k = y.k;",
            8102,
        ),
    ] {
        assert_eq!(error_of(text).number, number, "{text}");
    }
    // The two 207 above are told apart by the column each names.
    assert_eq!(
        error_of(
            "UPDATE x SET nocol = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k WHERE nocol2 = 1;"
        )
        .message,
        "Unknown column name 'nocol2'."
    );
    assert_eq!(
        error_of("UPDATE x SET nocol = nocol2 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;")
            .message,
        "Unknown column name 'nocol'."
    );
}

/// 8154, the 208 of the target and a 4104 on the left side of a `SET` carry the line the
/// statement starts on, where a 209 raised in the `WHERE` keeps the line of its column.
#[test]
fn the_errors_of_the_joined_form_carry_their_line() {
    let statement_line = [
        (
            "SELECT 1;\nUPDATE\n  dbo.t\nSET\n  a = 1\nFROM\n  dbo.t AS x\n  \
             JOIN dbo.t AS y ON x.k = y.k;",
            8154,
        ),
        (
            "SELECT 1;\nDELETE\n  dbo.t\nFROM\n  dbo.t AS x\n  JOIN dbo.t AS y ON x.k = y.k;",
            8154,
        ),
        (
            "SELECT 1;\nUPDATE\n  z\nSET\n  a = 1\nFROM\n  dbo.t AS x\n  \
             JOIN dbo.u AS y ON x.k = y.k;",
            208,
        ),
        (
            "SELECT 1;\nUPDATE\n  x\nSET\n  y.b = 1\nFROM\n  dbo.t AS x\n  \
             JOIN dbo.u AS y ON x.k = y.k;",
            4104,
        ),
    ];
    for (text, number) in statement_line {
        let error = error_of(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
        assert_eq!(error.line, 2, "{text}: the line the statement starts on");
    }
    // A 209 of the WHERE keeps the line of the column, which is not the statement's.
    let error = error_of(
        "SELECT 1;\nUPDATE\n  x\nSET\n  x.a = 1\nFROM\n  dbo.t AS x\n  \
         JOIN dbo.u AS y ON x.k = y.k\nWHERE\n  k = 1;",
    );
    assert_eq!(error.number, 209, "{}", error.message);
    assert_eq!(error.line, 10);
}

/// The forms the joined statement does not bind answer the internal error 50000 naming
/// themselves, not a plan that would write other rows.
#[test]
fn the_forms_a_joined_statement_does_not_bind_name_themselves() {
    for (text, form) in [
        (
            "UPDATE TOP (1) x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            "TOP",
        ),
        (
            "DELETE TOP (1) x FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            "TOP",
        ),
        (
            "UPDATE x SET x.a = 1 OUTPUT DELETED.a FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            "OUTPUT",
        ),
        (
            "UPDATE @v SET a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            "table variable",
        ),
        (
            "UPDATE x SET @v = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            "@variable",
        ),
        (
            "UPDATE x SET x.a = DEFAULT FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;",
            "DEFAULT",
        ),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 50000, "{text}: {}", error.message);
        assert!(
            error.message.contains(form) && error.message.contains("is not implemented yet"),
            "{text}: {}",
            error.message
        );
    }
}

/// Without a catalogue the sources of the `FROM` cannot be looked up: the refusal of the
/// clause, not a 208 that would tell a client its table does not exist.
#[test]
fn a_joined_statement_needs_the_catalogue() {
    register_builtins();
    let text = "UPDATE x SET x.a = 1 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k;";
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let ctx = BindContext::scalar(text, SessionOptions::default());
    let error = bind(&batch.statements[0], &ctx).expect_err("no catalogue, no source");
    assert_eq!(error.number, 50000, "{}", error.message);
}
