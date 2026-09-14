//! The binding of an `INSERT`: the pairing of the column list with the table, the arity
//! of the source, the `IDENTITY` and computed columns, `DEFAULT`, and the conversion of
//! each value towards its column.
//!
//! The catalogue is a double that knows five tables and one view, all in `dbo`:
//!
//! | name | columns |
//! |---|---|
//! | `t` | `a int NOT NULL`, `b nvarchar(10) NULL` |
//! | `ti` | `id int IDENTITY NOT NULL`, `v int NOT NULL` |
//! | `td` | `a int NOT NULL`, `d int NOT NULL`, `n int NULL` |
//! | `tc` | `a int NOT NULL`, `c int` computed |
//! | `tx` | `u uniqueidentifier NULL`, `dt date NULL` |
//! | `v` | a view |
//!
//! The bound nodes derive no `PartialEq`: a shape is checked by pattern matching, a type by
//! its `ty`.

use vauban_binder::{
    BindContext, BoundExpr, BoundExprKind, BoundStatement, CatalogView, ColumnBinding, InsertPlan,
    LogicalPlan, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo};

/// The double of the catalogue described in the module documentation.
struct Tables;

fn column(id: i32, index: usize, name: &str, ty: SqlType, nullable: bool) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(id),
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
    }
}

fn int() -> SqlType {
    SqlType::Int
}

fn nvarchar10() -> SqlType {
    SqlType::NVarChar(Len::Fixed(10))
}

impl CatalogView for Tables {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        if name.server.is_some() {
            return None;
        }
        if let Some(schema) = &name.schema
            && !schema.value.eq_ignore_ascii_case("dbo")
        {
            return None;
        }
        let table = |object: i32, columns: Vec<ColumnBinding>| ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("a small identifier"))),
            columns,
            kind: ResolvedTableKind::Table,
        };
        match name.name.value.to_ascii_lowercase().as_str() {
            "t" => Some(table(
                1,
                vec![
                    column(1, 0, "a", int(), false),
                    column(2, 1, "b", nvarchar10(), true),
                ],
            )),
            "ti" => Some(table(
                2,
                vec![
                    column(1, 0, "id", int(), false),
                    column(2, 1, "v", int(), false),
                ],
            )),
            "td" => Some(table(
                3,
                vec![
                    column(1, 0, "a", int(), false),
                    column(2, 1, "d", int(), false),
                    column(3, 2, "n", int(), true),
                ],
            )),
            "tc" => Some(table(
                4,
                vec![
                    column(1, 0, "a", int(), false),
                    column(2, 1, "c", int(), true),
                ],
            )),
            "tx" => Some(table(
                6,
                vec![
                    column(1, 0, "u", SqlType::UniqueIdentifier, true),
                    column(2, 1, "dt", SqlType::Date, true),
                ],
            )),
            "v" => Some(ResolvedTable {
                object: ObjectId(5),
                table: None,
                columns: Vec::new(),
                kind: ResolvedTableKind::View,
            }),
            _ => None,
        }
    }

    fn identity_column(&self, object: ObjectId) -> Option<ColumnId> {
        (object == ObjectId(2)).then_some(ColumnId(1))
    }

    fn computed_columns(&self, object: ObjectId) -> Vec<ColumnId> {
        if object == ObjectId(4) {
            vec![ColumnId(2)]
        } else {
            Vec::new()
        }
    }
}

/// Binds the single statement of `text` against the double.
fn try_bind(text: &str) -> Result<BoundStatement, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    assert_eq!(batch.statements.len(), 1, "{text}: one statement");
    let catalog = Tables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    };
    bind(&batch.statements[0], &ctx)
}

fn insert(text: &str) -> InsertPlan {
    match try_bind(text) {
        Ok(BoundStatement::Insert(plan)) => plan,
        Ok(other) => panic!("{text}: bound to {other:?}, not an INSERT"),
        Err(e) => panic!("{text}: {e:?}"),
    }
}

fn err(text: &str) -> SqlError {
    match try_bind(text) {
        Ok(bound) => panic!("{text}: bound to {bound:?}, no error"),
        Err(e) => e,
    }
}

/// The rows of a `Values` source, or a panic naming the node.
fn rows(plan: &LogicalPlan) -> &[Vec<BoundExpr>] {
    match plan {
        LogicalPlan::Values { rows, .. } => rows,
        other => panic!("not a Values: {other:?}"),
    }
}

/// The type of the `Convert` a value is wrapped in, and the type of what it wraps.
fn convert_of(expr: &BoundExpr) -> (&TypeInfo, &TypeInfo) {
    match &expr.kind {
        BoundExprKind::Convert {
            expr: inner,
            style: None,
            try_: false,
        } => (&expr.ty, &inner.ty),
        other => panic!("not a Convert: {other:?}"),
    }
}

fn names(columns: &[ColumnBinding]) -> Vec<&str> {
    columns.iter().map(|column| column.name.as_str()).collect()
}

/// Asserts that `error` is the error `expected` builds, on `line`: number, severity,
/// state and message.
fn assert_error(error: &SqlError, expected: &SqlError, line: u32) {
    assert_eq!(
        (error.number, error.severity, error.state, error.line),
        (expected.number, expected.severity, expected.state, line),
        "{}",
        error.message
    );
    assert_eq!(error.message, expected.message);
}

// ---------------------------------------------------------------------------------------
// The shape of the plan
// ---------------------------------------------------------------------------------------

#[test]
fn insert_values_two_rows_binds() {
    let plan = insert("INSERT INTO dbo.t (a, b) VALUES (1, N'x'), (2, N'y');");
    assert_eq!(plan.table, TableId(1));
    assert_eq!(names(&plan.columns), ["a", "b"]);
    let rows = rows(&plan.source);
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row.len(), 2);
        let (to, from) = convert_of(&row[0]);
        assert_eq!(to.ty, int());
        assert_eq!(from.ty, int(), "an int literal is converted all the same");
        let (to, from) = convert_of(&row[1]);
        assert_eq!(to.ty, nvarchar10());
        assert_eq!(from.ty, SqlType::NVarChar(Len::Fixed(1)));
    }
    assert_eq!(plan.source.schema().columns.len(), 2);
    assert_eq!(plan.source.schema().columns[1].ty.ty, nvarchar10());
}

#[test]
fn insert_without_column_list_uses_catalog_order() {
    let plan = insert("INSERT INTO dbo.t VALUES (1, N'x');");
    assert_eq!(names(&plan.columns), ["a", "b"]);
    assert_eq!(plan.columns[0].column, ColumnId(1));
    assert_eq!(plan.columns[1].column, ColumnId(2));
    let listed = insert("INSERT INTO dbo.t (b, a) VALUES (N'x', 1);");
    assert_eq!(
        names(&listed.columns),
        ["b", "a"],
        "a written list keeps its order"
    );
    let (to, _) = convert_of(&rows(&listed.source)[0][0]);
    assert_eq!(to.ty, nvarchar10());
}

#[test]
fn insert_without_column_list_skips_the_identity_column() {
    let plan = insert("INSERT INTO dbo.ti VALUES (5);");
    assert_eq!(names(&plan.columns), ["v"]);
    let computed = insert("INSERT INTO dbo.tc VALUES (1);");
    assert_eq!(names(&computed.columns), ["a"]);
}

#[test]
fn insert_select_binds_a_plan() {
    let plan = insert("INSERT INTO dbo.t (a, b) SELECT 1, N'x';");
    assert_eq!(names(&plan.columns), ["a", "b"]);
    assert!(
        matches!(*plan.source, LogicalPlan::Project { .. }),
        "the source is the bound SELECT, got {:?}",
        plan.source
    );
    assert_eq!(plan.source.schema().columns.len(), 2);
    let unlisted = insert("INSERT INTO dbo.t SELECT 1, N'x';");
    assert_eq!(names(&unlisted.columns), ["a", "b"]);
}

#[test]
fn insert_default_values_marks_all_columns_default() {
    let plan = insert("INSERT INTO dbo.td DEFAULT VALUES;");
    assert_eq!(plan.table, TableId(3));
    assert!(plan.columns.is_empty(), "no column receives a value");
    let rows = rows(&plan.source);
    assert_eq!(rows.len(), 1, "one row is inserted");
    assert!(rows[0].is_empty());
    assert!(plan.source.schema().columns.is_empty());
    let listed = insert("INSERT INTO dbo.td (d) DEFAULT VALUES;");
    assert!(
        listed.columns.is_empty(),
        "the list is checked, then ignored"
    );
}

#[test]
fn default_in_a_row_drops_the_column() {
    let plan = insert("INSERT INTO dbo.td (a, d) VALUES (1, DEFAULT);");
    assert_eq!(names(&plan.columns), ["a"]);
    assert_eq!(rows(&plan.source)[0].len(), 1);
    let unlisted = insert("INSERT INTO dbo.td VALUES (1, DEFAULT, DEFAULT);");
    assert_eq!(names(&unlisted.columns), ["a"]);
    let two_rows = insert("INSERT INTO dbo.td (a, d) VALUES (1, DEFAULT), (2, DEFAULT);");
    assert_eq!(names(&two_rows.columns), ["a"]);
    assert_eq!(rows(&two_rows.source).len(), 2);
    let only = insert("INSERT INTO dbo.td (d) VALUES (DEFAULT);");
    assert!(only.columns.is_empty());
    assert_eq!(rows(&only.source).len(), 1);
}

#[test]
fn default_in_some_rows_only_is_not_bound() {
    let error = err("INSERT INTO dbo.td (a, d) VALUES (1, DEFAULT), (2, 8);");
    assert_eq!(error.number, 50000);
    assert!(
        error.message.contains("DEFAULT in some rows"),
        "{}",
        error.message
    );
}

#[test]
fn values_expression_is_not_folded() {
    let plan = insert("INSERT INTO dbo.t (a) VALUES (1 + 1);");
    let (_, from) = convert_of(&rows(&plan.source)[0][0]);
    assert_eq!(from.ty, int());
    assert!(matches!(
        &rows(&plan.source)[0][0].kind,
        BoundExprKind::Convert { expr, .. } if matches!(expr.kind, BoundExprKind::Arith { .. })
    ));
}

#[test]
fn a_null_literal_is_converted_without_a_type_check() {
    let plan = insert("INSERT INTO dbo.tx (u, dt) VALUES (NULL, NULL);");
    let row = &rows(&plan.source)[0];
    assert_eq!(convert_of(&row[0]).0.ty, SqlType::UniqueIdentifier);
    assert_eq!(convert_of(&row[1]).0.ty, SqlType::Date);
    assert!(convert_of(&row[0]).0.nullable);
}

#[test]
fn hints_on_the_target_are_accepted() {
    let plan = insert("INSERT INTO dbo.t WITH (TABLOCK) (a, b) VALUES (1, N'x');");
    assert_eq!(names(&plan.columns), ["a", "b"]);
}

#[test]
fn column_names_are_matched_without_case() {
    let plan = insert("INSERT INTO DBO.T (A, B) VALUES (1, N'x');");
    assert_eq!(names(&plan.columns), ["a", "b"], "the catalogue's spelling");
}

// ---------------------------------------------------------------------------------------
// The errors
// ---------------------------------------------------------------------------------------

#[test]
fn insert_arity_mismatch_is_213() {
    let expected = SqlError::column_count_does_not_match_table();
    for text in [
        "INSERT INTO dbo.t VALUES (1);",
        "INSERT INTO dbo.t VALUES (1, N'x', 3);",
        "INSERT INTO dbo.t VALUES (1), (2);",
        "INSERT INTO dbo.t SELECT 1;",
        "INSERT INTO dbo.tc VALUES (1, 2);",
        "INSERT INTO dbo.t VALUES (CAST('20200101' AS date));",
    ] {
        assert_error(&err(text), &expected, 1);
    }
}

#[test]
fn insert_arity_mismatch_with_a_list_is_109_or_110() {
    assert_error(
        &err("INSERT INTO dbo.t (a, b) VALUES (1);"),
        &SqlError::more_columns_than_values(),
        1,
    );
    assert_error(
        &err("INSERT INTO dbo.t (a, b) VALUES (1), (2);"),
        &SqlError::more_columns_than_values(),
        1,
    );
    assert_error(
        &err("INSERT INTO dbo.t (a) VALUES (1, N'x');"),
        &SqlError::more_values_than_columns(),
        1,
    );
}

#[test]
fn insert_select_arity_mismatch_with_a_list_is_120_or_121() {
    assert_error(
        &err("INSERT INTO dbo.t (a, b) SELECT 1;"),
        &SqlError::select_list_shorter_than_insert_list(),
        1,
    );
    assert_error(
        &err("INSERT INTO dbo.t (a, b) SELECT 1, N'x', 3;"),
        &SqlError::select_list_longer_than_insert_list(),
        1,
    );
}

#[test]
fn values_rows_of_different_width_are_10709() {
    let expected = SqlError::table_value_constructor_rows_differ();
    for text in [
        "INSERT INTO dbo.t (a, b) VALUES (1, N'x'), (2);",
        "INSERT INTO dbo.t (a, b) VALUES (1, N'x'), (2, N'y', 3);",
        "INSERT INTO dbo.t VALUES (1, N'x'), (2);",
        "INSERT INTO dbo.ti VALUES (1, 1), (1);",
    ] {
        assert_error(&err(text), &expected, 1);
    }
}

#[test]
fn insert_into_identity_column_is_544_or_8101() {
    // Named in the list, with a value: 544, the object part of the name as written.
    let off = SqlError::identity_insert_is_off("ti");
    for text in [
        "INSERT INTO dbo.ti (id, v) VALUES (1, 1);",
        "INSERT INTO dbo.ti (id) VALUES (1);",
        "INSERT INTO dbo.ti (id, v) SELECT 1, 1;",
    ] {
        assert_error(&err(text), &off, 1);
    }
    // Without a list, one value too many lands on it: 8101, the name as written.
    assert_error(
        &err("INSERT INTO dbo.ti VALUES (1, 1);"),
        &SqlError::identity_insert_requires_column_list("dbo.ti"),
        1,
    );
    assert_error(
        &err("INSERT INTO ti VALUES (1, 1);"),
        &SqlError::identity_insert_requires_column_list("ti"),
        1,
    );
    assert_error(
        &err("INSERT INTO DBO.TI VALUES (1, 1);"),
        &SqlError::identity_insert_requires_column_list("DBO.TI"),
        1,
    );
}

#[test]
fn insert_without_column_list_and_too_many_values_is_8101() {
    // Two values over the one insertable column, on a table with an IDENTITY: 8101 and
    // not 213; the same width on a table without one is 213 (`insert_arity_mismatch_is_213`).
    assert_error(
        &err("INSERT INTO dbo.ti VALUES (1, 1, 1);"),
        &SqlError::identity_insert_requires_column_list("dbo.ti"),
        1,
    );
    assert_error(
        &err("INSERT INTO dbo.ti SELECT 1, 1;"),
        &SqlError::identity_insert_requires_column_list("dbo.ti"),
        1,
    );
}

#[test]
fn default_or_null_on_the_identity_column_is_339() {
    let expected = SqlError::default_or_null_as_identity_value();
    assert_error(
        &err("INSERT INTO dbo.ti (id, v) VALUES (DEFAULT, 1);"),
        &expected,
        1,
    );
    assert_error(
        &err("INSERT INTO dbo.ti (id, v) VALUES (NULL, 1);"),
        &expected,
        1,
    );
}

#[test]
fn insert_unknown_column_is_207() {
    assert_error(
        &err("INSERT INTO dbo.t (a, nocol) VALUES (1, 2);"),
        &SqlError::invalid_column_name("nocol"),
        1,
    );
    assert_error(
        &err("INSERT INTO dbo.t (a, nocol) SELECT 1, 2;"),
        &SqlError::invalid_column_name("nocol"),
        1,
    );
    assert_error(
        &err("INSERT INTO dbo.td (nocol) DEFAULT VALUES;"),
        &SqlError::invalid_column_name("nocol"),
        1,
    );
    // A hint word between the parentheses is a column list, not a hint.
    assert_error(
        &err("INSERT INTO dbo.t (NOLOCK) VALUES (1);"),
        &SqlError::invalid_column_name("NOLOCK"),
        1,
    );
}

#[test]
fn repeated_column_in_the_list_is_264() {
    let expected = SqlError::column_specified_more_than_once("a");
    for text in [
        "INSERT INTO dbo.t (a, a) VALUES (1, 2);",
        "INSERT INTO dbo.t (a, A) VALUES (1, 2);",
        "INSERT INTO dbo.t (A, a) VALUES (1, 2);",
        "INSERT INTO dbo.t (a, a) SELECT 1, 2;",
    ] {
        assert_error(&err(text), &expected, 1);
    }
}

#[test]
fn unknown_table_is_208() {
    assert_error(
        &err("INSERT INTO dbo.nosuch (a) VALUES (1);"),
        &SqlError::invalid_object_name("dbo.nosuch"),
        1,
    );
    assert_error(
        &err("INSERT INTO nosuch VALUES (1);"),
        &SqlError::invalid_object_name("nosuch"),
        1,
    );
    assert_error(
        &err("INSERT INTO srv.master.dbo.t (a) VALUES (1);"),
        &SqlError::invalid_object_name("srv.master.dbo.t"),
        1,
    );
}

#[test]
fn incompatible_type_is_206() {
    let error = err("INSERT INTO dbo.t (a) VALUES (CAST('20200101' AS date));");
    assert_eq!(
        (error.number, error.severity, error.state, error.line),
        (206, 16, 2, 1)
    );
    assert_eq!(
        error.message,
        SqlError::operand_type_clash("date", "int").message
    );
    let through_select = err("INSERT INTO dbo.t (a) SELECT CAST('20200101' AS date);");
    assert_eq!(through_select.number, 206);
    assert_eq!(through_select.message, error.message);
    // The value is named first whichever side the uniqueidentifier is on.
    let guid = err("INSERT INTO dbo.tx (u) VALUES (1);");
    assert_eq!(guid.number, 206);
    assert_eq!(
        guid.message,
        SqlError::operand_type_clash("int", "uniqueidentifier").message
    );
    let guid_value = err("INSERT INTO dbo.t (a) VALUES (NEWID());");
    assert_eq!(
        guid_value.message,
        SqlError::operand_type_clash("uniqueidentifier", "int").message
    );
}

#[test]
fn a_null_through_a_select_source_is_not_typed() {
    let plan = insert("INSERT INTO dbo.tx (dt) SELECT NULL;");
    assert_eq!(names(&plan.columns), ["dt"]);
    let sorted = insert("INSERT INTO dbo.tx (dt, u) SELECT TOP (1) NULL, NULL ORDER BY 1;");
    assert_eq!(names(&sorted.columns), ["dt", "u"]);
}

#[test]
fn incompatible_type_between_two_rows_names_the_rows() {
    let error = err("INSERT INTO dbo.t (a) VALUES (1), (CAST('20200101' AS date));");
    assert_eq!((error.number, error.line), (206, 1));
    assert_eq!(
        error.message,
        SqlError::operand_type_clash("int", "date").message
    );
    let guid = err("INSERT INTO dbo.tx (u) VALUES (NEWID()), (1);");
    assert_eq!(guid.number, 206);
    assert_eq!(
        guid.message,
        SqlError::operand_type_clash("uniqueidentifier", "int").message
    );
}

#[test]
fn a_legal_conversion_that_may_fail_on_a_value_binds() {
    let plan = insert("INSERT INTO dbo.t (a) VALUES ('abc');");
    let (to, from) = convert_of(&rows(&plan.source)[0][0]);
    assert_eq!(to.ty, int());
    assert_eq!(from.ty, SqlType::VarChar(Len::Fixed(3)));
}

#[test]
fn insert_into_view_names_v2() {
    let error = err("INSERT INTO dbo.v (a, b) VALUES (1, N'x');");
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("view"), "{}", error.message);
    assert!(error.message.contains("V2"), "{}", error.message);
}

#[test]
fn forms_out_of_scope_name_themselves() {
    for (text, what) in [
        ("INSERT TOP (1) INTO dbo.t (a) VALUES (1);", "TOP"),
        (
            "INSERT INTO dbo.t (a) OUTPUT INSERTED.a VALUES (1);",
            "OUTPUT",
        ),
        ("INSERT INTO @t (a) VALUES (1);", "table variable"),
        ("INSERT INTO dbo.t (a) EXEC dbo.p;", "EXECUTE"),
    ] {
        let error = err(text);
        assert_eq!(error.number, 50000, "{text}");
        assert!(error.message.contains(what), "{text}: {}", error.message);
    }
}

// ---------------------------------------------------------------------------------------
// The order of the errors
// ---------------------------------------------------------------------------------------

#[test]
fn an_unknown_column_precedes_a_repeated_one() {
    assert_eq!(
        err("INSERT INTO dbo.t (a, a, nocol) VALUES (1, 2, 3);").number,
        207
    );
    assert_eq!(err("INSERT INTO dbo.t (a, nocol) VALUES (1);").number, 207);
    assert_eq!(
        err("INSERT INTO dbo.t (nocol) VALUES (CAST('20200101' AS date));").number,
        207
    );
}

#[test]
fn a_short_row_precedes_a_repeated_column() {
    assert_eq!(err("INSERT INTO dbo.t (a, a) VALUES (1);").number, 109);
    assert_eq!(
        err("INSERT INTO dbo.t (a, a) VALUES (1, 2), (3);").number,
        10709
    );
    assert_eq!(err("INSERT INTO dbo.t (a, a) SELECT 1, 2, 3;").number, 121);
}

#[test]
fn a_default_on_the_identity_column_precedes_a_repeated_column() {
    assert_eq!(
        err("INSERT INTO dbo.ti (id, id) VALUES (NULL, 1);").number,
        339
    );
}

#[test]
fn a_type_clash_precedes_a_repeated_column() {
    assert_eq!(
        err("INSERT INTO dbo.t (a, a) VALUES (CAST('20200101' AS date), 1);").number,
        206
    );
}

#[test]
fn a_repeated_column_precedes_the_identity_refusal() {
    assert_eq!(
        err("INSERT INTO dbo.ti (id, id) VALUES (1, 1);").number,
        264
    );
    assert_eq!(err("INSERT INTO dbo.td (d, d) DEFAULT VALUES;").number, 264);
}

#[test]
fn rows_that_disagree_precede_a_short_row() {
    assert_eq!(
        err("INSERT INTO dbo.t (a, b) VALUES (1, N'x'), (2);").number,
        10709
    );
    assert_eq!(err("INSERT INTO dbo.ti VALUES (1, 1), (1);").number, 10709);
}

#[test]
fn a_short_row_precedes_a_default_on_the_identity_column() {
    assert_eq!(
        err("INSERT INTO dbo.ti (id, v) VALUES (DEFAULT);").number,
        109
    );
    assert_eq!(err("INSERT INTO dbo.ti (id, v) VALUES (1);").number, 109);
}

#[test]
fn a_short_row_precedes_a_type_clash() {
    assert_eq!(
        err("INSERT INTO dbo.t (a, b) VALUES (CAST('20200101' AS date));").number,
        109
    );
    assert_eq!(
        err("INSERT INTO dbo.t (a) SELECT CAST('20200101' AS date), 2;").number,
        121
    );
}

#[test]
fn a_type_clash_precedes_the_identity_refusal() {
    assert_eq!(
        err("INSERT INTO dbo.ti (id, v) VALUES (1, CAST('20200101' AS date));").number,
        206
    );
}

#[test]
fn errors_carry_the_line_of_the_statement() {
    for (text, number) in [
        ("SELECT 1 AS n;\nINSERT INTO\n  dbo.t\nVALUES\n  (1);", 213),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.ti\n  (id, v)\nVALUES\n  (1, 1);",
            544,
        ),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.t\n  (a, nocol)\nVALUES\n  (1, 2);",
            207,
        ),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.t\n  (a, a)\nVALUES\n  (1, 2);",
            264,
        ),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.t\n  (a, b)\nVALUES\n  (1);",
            109,
        ),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.t\n  (a, b)\nSELECT\n  1, 2, 3;",
            121,
        ),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.t\n  (a)\nVALUES\n  (CAST('20200101' AS date));",
            206,
        ),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.ti\nVALUES\n  (1, 1);",
            8101,
        ),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.ti\n  (id, v)\nVALUES\n  (NULL, 1);",
            339,
        ),
        (
            "SELECT 1 AS n;\nINSERT INTO\n  dbo.t\n  (a, b)\nVALUES\n  (1, N'x'),\n  (2);",
            10709,
        ),
    ] {
        register_builtins();
        let batch = parse_batch(text, &ParseOptions::default()).expect("parses");
        let catalog = Tables;
        let ctx = BindContext {
            text,
            catalog: Some(&catalog),
            database: "master",
            default_schema: "dbo",
            variables: &NoVariables,
            options: SessionOptions::default(),
        };
        let error = bind(&batch.statements[1], &ctx).expect_err(text);
        assert_eq!((error.number, error.line), (number, 2), "{text}");
    }
}
