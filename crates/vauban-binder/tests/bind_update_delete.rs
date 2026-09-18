//! `UPDATE` and `DELETE` over one table: the shape of the plan they bind to, the errors
//! of the target, of the `SET` list and of the `WHERE`, the order in which two faults of
//! one statement are reported, and the line each error carries.
//!
//! The double of the catalogue knows five tables and one view in `master.dbo`:
//! `t (a int NOT NULL, b nvarchar(10) NULL)`, `ti (id int IDENTITY NOT NULL, v int NOT
//! NULL)`, `tc (a int NOT NULL, c int NOT NULL)` whose `c` is computed, `tx (u
//! uniqueidentifier NULL)`, `u (k int NOT NULL)`, and the view `v`. The bound nodes derive
//! no `PartialEq`: a shape is checked by pattern matching, a conversion by its `.ty`.

use vauban_binder::{
    BindContext, BoundExpr, BoundExprKind, BoundStatement, CatalogView, ColumnBinding, DeletePlan,
    LockHints, LogicalPlan, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
    UpdatePlan, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{Ident, ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo};

/// The tables of the module documentation, in `master.dbo`.
///
/// The comparison of the name is ASCII case-insensitive, and the parts that were not
/// written are filled from the arguments, so `master.dbo.t`, `dbo.t` and `T` reach the
/// same table.
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
                    column(1, 0, "a", SqlType::Int, false),
                    column(2, 1, "b", SqlType::NVarChar(Len::Fixed(10)), true),
                ],
                ResolvedTableKind::Table,
            ),
            "ti" => (
                2,
                vec![
                    column(1, 0, "id", SqlType::Int, false),
                    column(2, 1, "v", SqlType::Int, false),
                ],
                ResolvedTableKind::Table,
            ),
            "tc" => (
                3,
                vec![
                    column(1, 0, "a", SqlType::Int, false),
                    column(2, 1, "c", SqlType::Int, false),
                ],
                ResolvedTableKind::Table,
            ),
            "v" => (4, Vec::new(), ResolvedTableKind::View),
            "u" => (
                5,
                vec![column(1, 0, "k", SqlType::Int, false)],
                ResolvedTableKind::Table,
            ),
            "tx" => (
                6,
                vec![column(1, 0, "u", SqlType::UniqueIdentifier, true)],
                ResolvedTableKind::Table,
            ),
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
        (object == ObjectId(4)).then_some("SELECT a, b FROM dbo.t")
    }

    fn identity_column(&self, object: ObjectId) -> Option<ColumnId> {
        (object == ObjectId(2)).then_some(ColumnId(1))
    }

    fn computed_columns(&self, object: ObjectId) -> Vec<ColumnId> {
        if object == ObjectId(3) {
            vec![ColumnId(2)]
        } else {
            Vec::new()
        }
    }
}

/// Binds the statements of `text` in order against the tables, in `master` and `dbo`:
/// the bound statement of the last one, or the first error.
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

/// The number, the state and the line of the first error of `text`.
#[track_caller]
fn number_line(text: &str) -> (u32, u32) {
    let error = error_of(text);
    (error.number, error.line)
}

/// The `Scan` at the bottom of `input`, and whether a `Filter` sits above it.
fn scan_of(input: &LogicalPlan) -> (&LogicalPlan, bool) {
    match input {
        LogicalPlan::Filter { input, .. } => (scan_of(input).0, true),
        LogicalPlan::Scan { .. } => (input, false),
        other => panic!("neither a Scan nor a Filter: {other:?}"),
    }
}

/// Whether `expr` holds, at any depth, a `ColumnRef` of the column named `name`.
fn reads_column(expr: &BoundExpr, name: &str) -> bool {
    match &expr.kind {
        BoundExprKind::ColumnRef(binding) => binding.name == name,
        BoundExprKind::Convert { expr, .. } => reads_column(expr, name),
        BoundExprKind::Arith { left, right, .. } => {
            reads_column(left, name) || reads_column(right, name)
        }
        BoundExprKind::Negate(inner) | BoundExprKind::BitNot(inner) => reads_column(inner, name),
        BoundExprKind::Function { args, .. } => args.iter().any(|arg| reads_column(arg, name)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------
// The shape of an UPDATE
// ---------------------------------------------------------------------------------------

/// `UPDATE dbo.t SET a = 1`: the `Scan` of `t`, no `Filter`, one assignment on the column
/// `a` of the catalogue.
#[test]
fn update_binds_to_the_scan_of_the_target() {
    let plan = update_of("UPDATE dbo.t SET a = 1;");
    assert_eq!(plan.table, TableId(1));
    let (scan, filtered) = scan_of(&plan.input);
    assert!(!filtered, "no WHERE, no Filter");
    let LogicalPlan::Scan {
        table,
        columns,
        alias,
        hints,
        ..
    } = scan
    else {
        unreachable!()
    };
    assert_eq!(*table, TableId(1));
    assert_eq!(alias, "t");
    assert_eq!(columns.len(), 2);
    assert!(matches!(hints, LockHints { .. }));
    assert_eq!(plan.assignments.len(), 1);
    let (column, value) = &plan.assignments[0];
    assert_eq!(column.column, ColumnId(1));
    assert_eq!(column.name, "a");
    assert_eq!(column.index, 0);
    assert!(matches!(value.kind, BoundExprKind::Convert { .. }));
}

/// `SET a = a + 1` reads the old value: the right side holds a `ColumnRef` of `a`, under
/// the `Convert` towards the column.
#[test]
fn update_set_reads_the_old_value() {
    let plan = update_of("UPDATE dbo.t SET a = a + 1;");
    let (column, value) = &plan.assignments[0];
    assert_eq!(column.name, "a");
    assert!(reads_column(value, "a"), "{value:?}");
    let BoundExprKind::Convert { expr, .. } = &value.kind else {
        panic!("{value:?}");
    };
    let BoundExprKind::Arith { left, .. } = &expr.kind else {
        panic!("{expr:?}");
    };
    let BoundExprKind::ColumnRef(binding) = &left.kind else {
        panic!("{left:?}");
    };
    assert_eq!(binding.column, ColumnId(1));
    assert_eq!(binding.index, 0);

    // Two assignments read the row as it was: `b = CAST(a …)` reads `a`, not the new `a`.
    let plan = update_of("UPDATE dbo.t SET a = a + 1, b = CAST(a AS nvarchar(10));");
    assert_eq!(plan.assignments.len(), 2);
    assert!(reads_column(&plan.assignments[1].1, "a"));
    // The column of the target is read through the table name too.
    let plan = update_of("UPDATE dbo.t SET a = t.a + 1;");
    assert!(reads_column(&plan.assignments[0].1, "a"));
}

/// Each value sits under a `Convert` whose `.ty` is the type of the column: `int` for
/// `a`, `nvarchar(10)` for `b`; a `NULL` converts to either.
#[test]
fn update_inserts_convert_to_column_type() {
    let plan = update_of("UPDATE dbo.t SET b = a, a = 1;");
    let (b, to_b) = &plan.assignments[0];
    assert_eq!(b.name, "b");
    assert!(
        matches!(to_b.kind, BoundExprKind::Convert { .. }),
        "{to_b:?}"
    );
    assert_eq!(to_b.ty.ty, SqlType::NVarChar(Len::Fixed(10)));
    assert!(
        !to_b.ty.nullable,
        "a NOT NULL value stays NOT NULL under the Convert"
    );
    let (a, to_a) = &plan.assignments[1];
    assert_eq!(a.name, "a");
    assert!(
        matches!(to_a.kind, BoundExprKind::Convert { .. }),
        "{to_a:?}"
    );
    assert_eq!(to_a.ty.ty, SqlType::Int);
    // The literal keeps its own line under the node.
    assert_eq!(to_a.line, 1);

    let plan = update_of("UPDATE dbo.t SET a = NULL, b = NULL;");
    assert_eq!(plan.assignments[0].1.ty.ty, SqlType::Int);
    assert!(plan.assignments[0].1.ty.nullable);
    assert_eq!(
        plan.assignments[1].1.ty.ty,
        SqlType::NVarChar(Len::Fixed(10))
    );
    let plan = update_of("UPDATE dbo.tx SET u = NULL;");
    assert_eq!(plan.assignments[0].1.ty.ty, SqlType::UniqueIdentifier);
    // A string literal converts to `int` at bind time; the value is the executor's.
    let plan = update_of("UPDATE dbo.t SET a = N'12';");
    assert_eq!(plan.assignments[0].1.ty.ty, SqlType::Int);
}

/// `WHERE` filters the `Scan`, and its columns are the target's, with or without the
/// table name as qualifier.
#[test]
fn update_with_where_filters_the_scan() {
    for text in [
        "UPDATE dbo.t SET a = a + 10 WHERE b = N'x';",
        "UPDATE dbo.t SET a = a + 10 WHERE t.b = N'x';",
        "UPDATE dbo.t SET a = 1 WHERE dbo.t.b = N'x' AND a > 0;",
    ] {
        let plan = update_of(text);
        let LogicalPlan::Filter { input, predicate } = &*plan.input else {
            panic!("{text}: {:?}", plan.input);
        };
        assert!(predicate.is_predicate(), "{text}");
        assert!(matches!(**input, LogicalPlan::Scan { .. }), "{text}");
    }
}

/// `SET t.a = 1` and `SET dbo.t.a = 1` name the column of the target; `SET T.A = 1` too.
#[test]
fn a_qualifier_naming_the_target_binds_the_column() {
    for text in [
        "UPDATE dbo.t SET t.a = 2;",
        "UPDATE dbo.t SET dbo.t.a = 2;",
        "UPDATE dbo.t SET master.dbo.t.a = 2;",
        "UPDATE dbo.t SET T.A = 2;",
        "UPDATE t SET dbo.t.a = 2;",
    ] {
        let plan = update_of(text);
        assert_eq!(plan.assignments[0].0.column, ColumnId(1), "{text}");
    }
}

/// `SET a += 1` binds as `SET a = a + 1`, and `SET b += N'y'` as a concatenation.
#[test]
fn a_compound_assignment_reads_the_column() {
    let plan = update_of("UPDATE dbo.t SET a += 1;");
    let (column, value) = &plan.assignments[0];
    assert_eq!(column.name, "a");
    let BoundExprKind::Convert { expr, .. } = &value.kind else {
        panic!("{value:?}");
    };
    assert!(matches!(expr.kind, BoundExprKind::Arith { .. }), "{expr:?}");
    assert!(reads_column(expr, "a"));
    assert_eq!(value.ty.ty, SqlType::Int);

    let plan = update_of("UPDATE dbo.t SET b += N'y';");
    assert!(reads_column(&plan.assignments[0].1, "b"));
    assert_eq!(
        plan.assignments[0].1.ty.ty,
        SqlType::NVarChar(Len::Fixed(10))
    );

    // Counter-proof: a plain `=` reads nothing of the column.
    let plan = update_of("UPDATE dbo.t SET a = 1;");
    assert!(!reads_column(&plan.assignments[0].1, "a"));
}

/// A hint list on the target is accepted; the `Scan` carries the default hints.
#[test]
fn a_hint_list_on_the_target_is_accepted() {
    let plan = update_of("UPDATE dbo.t WITH (UPDLOCK) SET a = 2;");
    let (scan, _) = scan_of(&plan.input);
    let LogicalPlan::Scan { hints, .. } = scan else {
        unreachable!()
    };
    assert!(matches!(hints, LockHints { .. }));
    let plan = delete_of("DELETE FROM dbo.t WITH (ROWLOCK);");
    assert!(matches!(*plan.input, LogicalPlan::Scan { .. }));
}

// ---------------------------------------------------------------------------------------
// The shape of a DELETE
// ---------------------------------------------------------------------------------------

/// `DELETE dbo.t` binds to the `Scan` of `t`, with no `Filter`.
#[test]
fn delete_without_from_binds() {
    let plan = delete_of("DELETE dbo.t;");
    assert_eq!(plan.table, TableId(1));
    let (scan, filtered) = scan_of(&plan.input);
    assert!(!filtered);
    let LogicalPlan::Scan { table, alias, .. } = scan else {
        unreachable!()
    };
    assert_eq!(*table, TableId(1));
    assert_eq!(alias, "t");

    let plan = delete_of("DELETE dbo.t WHERE t.a = 1;");
    let (_, filtered) = scan_of(&plan.input);
    assert!(filtered);
}

/// `DELETE FROM dbo.t` binds to the same plan as `DELETE dbo.t`, `WHERE` included.
#[test]
fn delete_from_binds() {
    let plan = delete_of("DELETE FROM dbo.t;");
    assert_eq!(plan.table, TableId(1));
    let (scan, filtered) = scan_of(&plan.input);
    assert!(!filtered);
    let LogicalPlan::Scan { table, alias, .. } = scan else {
        unreachable!()
    };
    assert_eq!(*table, TableId(1));
    assert_eq!(alias, "t");

    let plan = delete_of("DELETE FROM dbo.t WHERE a = 1;");
    let LogicalPlan::Filter { input, predicate } = &*plan.input else {
        panic!("{:?}", plan.input);
    };
    assert!(predicate.is_predicate());
    assert!(matches!(**input, LogicalPlan::Scan { .. }));
    // A one-part name reaches the table as a two-part one does.
    assert_eq!(delete_of("DELETE FROM t;").table, TableId(1));
    assert_eq!(delete_of("DELETE T;").table, TableId(1));
}

// ---------------------------------------------------------------------------------------
// The errors
// ---------------------------------------------------------------------------------------

/// A column the target does not have, on the left of a `SET`, on its right, or in the
/// `WHERE`, is 207 on the line of the column.
#[test]
fn update_unknown_column_is_207() {
    for (text, line) in [
        ("UPDATE dbo.t SET nocol = 1;", 1),
        ("UPDATE dbo.t SET t.nocol = 1;", 1),
        ("UPDATE\ndbo.t\nSET\nnocol =\nb;", 4),
        ("UPDATE\ndbo.t\nSET\na = 1,\n\nnocol = 1;", 6),
        ("UPDATE\ndbo.t\nSET\na =\nnocol;", 5),
        ("UPDATE\ndbo.t\nSET\na = a\nWHERE\na = nocol;", 6),
        ("UPDATE dbo.t SET nocol = DEFAULT;", 1),
        ("DELETE\ndbo.t\nWHERE\na = nocol;", 4),
        ("DELETE FROM dbo.t WHERE nocol = 1;", 1),
    ] {
        let error = error_of(text);
        assert_eq!((error.number, error.line), (207, line), "{text}");
        assert!(
            error.message.contains("'nocol'"),
            "{text}: {}",
            error.message
        );
    }
}

/// A qualifier that is not the target, on the left of a `SET`, is 4104 on the line of
/// the statement; in the `WHERE` it keeps the line `expr.rs` gives it.
#[test]
fn a_qualifier_that_is_not_the_target_is_4104_on_the_statement_line() {
    for (text, printed) in [
        ("UPDATE dbo.t SET x.a = 1;", "x.a"),
        ("UPDATE dbo.t SET nosch.t.a = 1;", "nosch.t.a"),
        ("UPDATE dbo.t SET nodb.dbo.t.a = 1;", "nodb.dbo.t.a"),
        ("UPDATE\ndbo.t\nSET\nx.a =\nb;", "x.a"),
    ] {
        let error = error_of(text);
        assert_eq!((error.number, error.line), (4104, 1), "{text}");
        assert!(
            error.message.contains(&format!("\"{printed}\"")),
            "{text}: {}",
            error.message
        );
    }
    assert_eq!(
        number_line("UPDATE dbo.t SET a = 1 WHERE x.a = 1;"),
        (4104, 1)
    );
    assert_eq!(number_line("DELETE FROM dbo.t WHERE x.a = 1;"), (4104, 1));
}

/// The `IDENTITY` column on the left of a `SET` is 8102, naming the column as the
/// catalogue spells it, on the line of the statement: alone, after another column,
/// qualified, in upper case, with a `DEFAULT`, a `NULL` or a compound operator.
#[test]
fn update_identity_column_is_8102() {
    for text in [
        "UPDATE dbo.ti SET id = 1;",
        "UPDATE dbo.ti SET v = 1, id = 1;",
        "UPDATE dbo.ti SET ti.id = 1;",
        "UPDATE DBO.TI SET ID = 1;",
        "UPDATE dbo.ti SET id = NULL;",
        "UPDATE dbo.ti SET id += 1;",
        "UPDATE dbo.ti SET id = v;",
    ] {
        let error = error_of(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (8102, 16, 1),
            "{text}"
        );
        assert_eq!(error.line, 1, "{text}");
        assert!(error.message.contains("'id'"), "{text}: {}", error.message);
    }
    // Reading the column is allowed, on the right and in the WHERE.
    let plan = update_of("UPDATE dbo.ti SET v = id WHERE id = 1;");
    assert_eq!(plan.assignments[0].0.name, "v");
}

/// `SET id = DEFAULT` over the `IDENTITY` answers 8102, not the refusal of `DEFAULT`.
#[test]
fn identity_set_to_default_is_8102() {
    assert_eq!(number_line("UPDATE dbo.ti SET id = DEFAULT;"), (8102, 1));
}

/// A computed column on the left of a `SET` is 271, naming the column as the catalogue
/// spells it, on the line of the statement.
#[test]
fn update_computed_column_is_271() {
    for text in [
        "UPDATE dbo.tc SET c = 1;",
        "UPDATE dbo.tc SET C = 1;",
        "UPDATE dbo.tc SET a = 1, c = 1;",
        "UPDATE dbo.tc SET c = 1, c = 2;",
        "UPDATE\ndbo.tc\nSET\na = a,\nc =\na;",
    ] {
        let error = error_of(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (271, 16, 1),
            "{text}"
        );
        assert_eq!(error.line, 1, "{text}");
        assert!(error.message.contains("\"c\""), "{text}: {}", error.message);
    }
    // Reading the column is allowed.
    let plan = update_of("UPDATE dbo.tc SET a = c;");
    assert!(reads_column(&plan.assignments[0].1, "c"));
}

/// A column assigned twice in one `SET` is 264, naming the column as the catalogue spells
/// it, on the line of the statement: adjacent or not, whatever the case or the qualifier.
#[test]
fn a_column_assigned_twice_is_264() {
    for text in [
        "UPDATE dbo.t SET a = 1, a = 2;",
        "UPDATE dbo.t SET a = 1, A = 2;",
        "UPDATE dbo.t SET A = 1, a = 2;",
        "UPDATE dbo.t SET a = 1, t.a = 2;",
        "UPDATE dbo.t SET b = N'x', a = 1, a = 2;",
        "UPDATE\ndbo.t\nSET\na = a,\na =\na;",
    ] {
        let error = error_of(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (264, 16, 1),
            "{text}"
        );
        assert_eq!(error.line, 1, "{text}");
        assert!(error.message.contains("'a'"), "{text}: {}", error.message);
    }
}

/// A `WHERE` that is a value and not a condition is 4145, on the line of the token that
/// follows it, in an `UPDATE` as in a `DELETE`.
#[test]
fn where_non_predicate_is_4145() {
    for (text, line) in [
        ("UPDATE dbo.t SET a = 1 WHERE 1;", 1),
        ("UPDATE dbo.t SET a = 1 WHERE a;", 1),
        ("DELETE FROM dbo.t WHERE 1;", 1),
        ("DELETE dbo.t WHERE a;", 1),
        ("UPDATE\ndbo.t\nSET\na = a\nWHERE\na\n;", 7),
        ("DELETE\nFROM\ndbo.t\nWHERE\n1\n;", 6),
    ] {
        let error = error_of(text);
        assert_eq!((error.number, error.severity), (4145, 15), "{text}");
        assert_eq!(error.line, line, "{text}");
    }
}

/// A target that resolves to nothing is 208 on the line of the statement, printing the
/// name as written.
#[test]
fn update_unknown_table_is_208() {
    for (text, printed) in [
        ("UPDATE dbo.nosuch SET a = 1;", "dbo.nosuch"),
        ("UPDATE nosuch SET a = 1;", "nosuch"),
        ("UPDATE nodb.dbo.t SET a = 1;", "nodb.dbo.t"),
        ("UPDATE srv.master.dbo.t SET a = 1;", "srv.master.dbo.t"),
        (
            "UPDATE dbo.nosuch SET nocol = 1 WHERE nocol2 = 1;",
            "dbo.nosuch",
        ),
        ("DELETE FROM dbo.nosuch;", "dbo.nosuch"),
        ("DELETE dbo.nosuch WHERE a = 1;", "dbo.nosuch"),
        (
            "SELECT 1 AS n;\nUPDATE\ndbo.nosuch\nSET\na = a;",
            "dbo.nosuch",
        ),
        ("SELECT 1 AS n;\nDELETE\nFROM\ndbo.nosuch;", "dbo.nosuch"),
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 208, "{text}");
        let line = if text.starts_with("SELECT") { 2 } else { 1 };
        assert_eq!(error.line, line, "{text}");
        assert!(
            error.message.contains(&format!("'{printed}'")),
            "{text}: {}",
            error.message
        );
    }
}

/// A value with no implicit conversion to its column is 206, state 2, on the line of the
/// statement, the value's type named before the column's.
#[test]
fn a_value_that_does_not_convert_is_206() {
    for (text, value, column) in [
        (
            "UPDATE dbo.t SET a = CAST('20200101' AS date);",
            "date",
            "int",
        ),
        ("UPDATE dbo.t SET a = NEWID();", "uniqueidentifier", "int"),
        (
            "UPDATE dbo.t SET a += CAST('20200101' AS date);",
            "date",
            "int",
        ),
        ("UPDATE dbo.tx SET u = 1;", "int", "uniqueidentifier"),
        (
            "UPDATE\ndbo.t\nSET\na =\nCAST('20200101' AS date);",
            "date",
            "int",
        ),
    ] {
        let error = error_of(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (206, 16, 2),
            "{text}"
        );
        assert_eq!(error.line, 1, "{text}");
        let first = error.message.find(value).unwrap_or(usize::MAX);
        let second = error.message.rfind(column).unwrap_or(0);
        assert!(first < second, "{text}: {}", error.message);
    }
}

/// The errors that name the statement carry the line it starts on, wherever their node
/// is: 208, 206, 264, 8102, 271. A second statement carries its own line.
#[test]
fn the_errors_of_the_statement_carry_its_line() {
    let head = "SELECT 1 AS n;\n";
    for (text, number) in [
        ("UPDATE\n\ndbo.ti\n\n\nSET\n\nid = v;", 8102),
        ("UPDATE\ndbo.ti\nSET\nv = v,\nid =\nv;", 8102),
        ("UPDATE\n\ndbo.t\n\nSET\n\nb = N'x',\na = 1,\n\na = 2;", 264),
        ("UPDATE\ndbo.tc\nSET\na = a,\nc =\na;", 271),
        ("UPDATE\ndbo.t\nSET\na =\nNEWID();", 206),
        ("UPDATE\ndbo.nosuch\nSET\na = a;", 208),
        ("DELETE\nFROM\ndbo.nosuch;", 208),
    ] {
        let batch = format!("{head}{text}");
        assert_eq!(number_line(&batch), (number, 2), "{batch:?}");
    }
}

/// The right side of a `SET` binds as an expression: 137 for an undeclared variable, 195
/// for an unknown function, each on the line of its node.
#[test]
fn the_right_side_binds_as_an_expression() {
    assert_eq!(number_line("UPDATE\ndbo.t\nSET\na =\n@x;"), (137, 5));
    assert_eq!(
        number_line("UPDATE\ndbo.t\nSET\na =\nNO_SUCH_FN(1);"),
        (195, 5)
    );
    assert_eq!(number_line("DELETE dbo.t WHERE a = @x;"), (137, 1));
}

// ---------------------------------------------------------------------------------------
// Two faults in one statement
// ---------------------------------------------------------------------------------------

/// The `WHERE` is bound before the `SET` list: its 207, 4104, 137 and 4145 come out
/// before a 207 on the left of a `SET`, before a 8102, a 264 and a 206.
#[test]
fn the_where_is_bound_before_the_set_list() {
    let nocol2 = |text: &str| {
        let error = error_of(text);
        assert_eq!(error.number, 207, "{text}");
        assert!(
            error.message.contains("'nocol2'"),
            "{text}: {}",
            error.message
        );
    };
    nocol2("UPDATE dbo.t SET nocol = 1 WHERE nocol2 = 1;");
    nocol2("UPDATE dbo.t SET a = nocol WHERE nocol2 = 1;");
    nocol2("UPDATE dbo.t SET a = CAST('20200101' AS date) WHERE nocol2 = 1;");
    nocol2("UPDATE dbo.ti SET id = 1 WHERE nocol2 = 1;");
    nocol2("UPDATE dbo.t SET a = 1, a = 2 WHERE nocol2 = 1;");
    assert_eq!(
        error_of("UPDATE dbo.t SET nocol = 1 WHERE x.a = 1;").number,
        4104
    );
    assert_eq!(
        error_of("UPDATE dbo.t SET nocol = 1 WHERE a = @x;").number,
        137
    );
    assert_eq!(error_of("UPDATE dbo.ti SET id = 1 WHERE 1;").number, 4145);
    assert_eq!(error_of("UPDATE dbo.t SET nocol = a WHERE 1;").number, 4145);
    assert_eq!(
        error_of("UPDATE dbo.t SET a = CAST('20200101' AS date) WHERE 1;").number,
        4145
    );
}

/// The left sides of the `SET` list are resolved, in written order, before any right
/// side is bound.
#[test]
fn the_left_sides_are_resolved_before_the_values() {
    let named = |text: &str, number: u32, printed: &str| {
        let error = error_of(text);
        assert_eq!(error.number, number, "{text}");
        assert!(error.message.contains(printed), "{text}: {}", error.message);
    };
    named("UPDATE dbo.t SET nocol = 1, a = nocol2;", 207, "'nocol'");
    named("UPDATE dbo.t SET a = nocol2, nocol = 1;", 207, "'nocol'");
    named("UPDATE dbo.t SET a = x.a, nocol = 1;", 207, "'nocol'");
    named("UPDATE dbo.t SET x.a = 1, nocol = 1;", 4104, "\"x.a\"");
    named("UPDATE dbo.t SET nocol = 1, x.a = 1;", 207, "'nocol'");
}

/// The right sides are bound before the checks on the columns: a 207 on a value comes out
/// before the 8102, the 271 and the 264 of another assignment, wherever it is written.
#[test]
fn the_values_are_bound_before_the_identity_check() {
    for text in [
        "UPDATE dbo.ti SET id = 1, v = nocol;",
        "UPDATE dbo.ti SET v = nocol, id = 1;",
        "UPDATE dbo.tc SET c = 1, a = nocol;",
        "UPDATE dbo.t SET a = nocol, a = 2;",
    ] {
        assert_eq!(error_of(text).number, 207, "{text}");
    }
    // The 264 of the second assignment comes before a 207 on its own value: the value of
    // the first assignment binds, and the checks run on the first assignment before the
    // second value is looked at.
    assert_eq!(error_of("UPDATE dbo.t SET a = 1, a = nocol;").number, 207);
}

/// 8102, 271 and 264 are checked assignment by assignment, in written order: the first
/// assignment at fault decides.
#[test]
fn identity_and_repeated_columns_are_checked_in_written_order() {
    for (text, number) in [
        ("UPDATE dbo.ti SET id = 1, id = 2;", 8102),
        ("UPDATE dbo.ti SET v = 1, v = 2, id = 1;", 264),
        ("UPDATE dbo.ti SET id = 1, v = 1, v = 2;", 8102),
        ("UPDATE dbo.tc SET a = 1, a = 2, c = 1;", 264),
        ("UPDATE dbo.tc SET c = 1, a = 1, a = 2;", 271),
    ] {
        assert_eq!(error_of(text).number, number, "{text}");
    }
}

/// 206 belongs to the same pass as 8102 and 271: the first assignment at fault decides.
#[test]
fn the_conversion_is_checked_in_the_same_pass_as_the_identity() {
    for (text, number) in [
        (
            "UPDATE dbo.ti SET id = 1, v = CAST('20200101' AS date);",
            8102,
        ),
        (
            "UPDATE dbo.ti SET v = CAST('20200101' AS date), id = 1;",
            206,
        ),
        (
            "UPDATE dbo.tc SET c = 1, a = CAST('20200101' AS date);",
            271,
        ),
        (
            "UPDATE dbo.tc SET a = CAST('20200101' AS date), c = 1;",
            206,
        ),
        ("UPDATE dbo.t SET a = CAST('20200101' AS date), a = 2;", 206),
    ] {
        assert_eq!(error_of(text).number, number, "{text}");
    }
}

// ---------------------------------------------------------------------------------------
// What is not bound
// ---------------------------------------------------------------------------------------

/// `TOP`, `OUTPUT`, a view, a table variable and `SET @x = …` answer the internal error
/// 50000 naming the form.
#[test]
fn the_other_forms_name_themselves() {
    for (text, form) in [
        ("UPDATE TOP (1) dbo.t SET a = 2;", "TOP"),
        ("DELETE TOP (1) FROM dbo.t;", "TOP"),
        ("UPDATE dbo.t SET a = 2 OUTPUT DELETED.a;", "OUTPUT"),
        ("DELETE FROM dbo.t OUTPUT DELETED.a;", "OUTPUT"),
        ("UPDATE dbo.v SET a = 2;", "view"),
        ("DELETE FROM dbo.v;", "view"),
        ("UPDATE @t SET a = 2;", "table variable"),
        ("DELETE FROM @t;", "table variable"),
        ("UPDATE dbo.t SET @x = a;", "@variable"),
        ("UPDATE dbo.t SET @x = a = 2;", "@variable"),
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

/// `SET c = DEFAULT` is refused by the internal error 50000, after the checks on the
/// columns before it.
#[test]
fn set_default_is_not_bound_yet() {
    for text in [
        "UPDATE dbo.t SET b = DEFAULT;",
        "UPDATE dbo.t SET a = 1, b = DEFAULT;",
    ] {
        let error = error_of(text);
        assert_eq!(error.number, 50000, "{text}: {}", error.message);
        assert!(
            error.message.contains("DEFAULT"),
            "{text}: {}",
            error.message
        );
    }
    assert_eq!(error_of("UPDATE dbo.t SET nocol = DEFAULT;").number, 207);
    assert_eq!(error_of("UPDATE dbo.t SET a = 1, a = DEFAULT;").number, 264);
}

/// Without a catalogue the target cannot be looked up: the refusal of the clause, not a
/// 208 that would tell a client its table does not exist.
#[test]
fn without_a_catalogue_the_target_is_refused() {
    register_builtins();
    let text = "UPDATE dbo.t SET a = 1; DELETE FROM dbo.t;";
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let ctx = BindContext::scalar(text, SessionOptions::default());
    for statement in &batch.statements {
        let error = bind(statement, &ctx).expect_err("no catalogue");
        assert_eq!(error.number, 50000, "{}", error.message);
        assert!(error.message.contains("catalog"), "{}", error.message);
    }
}
