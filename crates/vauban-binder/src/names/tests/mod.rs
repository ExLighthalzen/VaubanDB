use super::alias_of;
use crate::bound::{BoundStatement, ColumnBinding, LogicalPlan};
use crate::context::{
    BindContext, CatalogView, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{Ident, ObjectName, ParseOptions, Span, parse_batch};
use vauban_types::{SqlType, TypeInfo};

/// A hand-built double of the catalogue that knows one table, `master.dbo.t`, with two
/// columns; `resolve_table` is the method `bind_from` calls.
///
/// The comparison is ASCII case-insensitive, as a `CI` collation is, and the double
/// fills the parts that were not written from the two arguments it is handed — which is
/// what gives `from_unqualified_uses_dbo` and its counter-proof their meaning: those
/// values come from the [`BindContext`], not from a constant here.
struct OneTable;

impl CatalogView for OneTable {
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
        let db = part(name.database.as_ref(), database);
        let schema = part(name.schema.as_ref(), default_schema);
        (db.eq_ignore_ascii_case("master")
            && schema.eq_ignore_ascii_case("dbo")
            && name.name.value.eq_ignore_ascii_case("t"))
        .then(|| ResolvedTable {
            object: ObjectId(42),
            table: Some(TableId(7)),
            columns: vec![
                column(ColumnId(1), 0, "id", SqlType::Int, false),
                column(ColumnId(2), 1, "a", SqlType::Int, true),
            ],
            kind: ResolvedTableKind::Table,
        })
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
        name: &vauban_parser::ObjectName,
        database: &str,
        default_schema: &str,
    ) -> crate::TableReferenceKind {
        match crate::CatalogView::resolve_table(self, name, database, default_schema) {
            Some(_) => crate::TableReferenceKind::Table,
            None => crate::TableReferenceKind::Unknown,
        }
    }
}

/// A double that resolves any name it is handed to the same table, four-part names
/// included: the counter-proof of the 208 of a four-part name. With it, what refuses
/// `nosrv.master.dbo.t` is this module and not the resolution.
struct AnyName;

impl CatalogView for AnyName {
    fn resolve_table(
        &self,
        _name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        Some(ResolvedTable {
            object: ObjectId(42),
            table: Some(TableId(7)),
            columns: vec![column(ColumnId(1), 0, "id", SqlType::Int, false)],
            kind: ResolvedTableKind::Table,
        })
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
        name: &vauban_parser::ObjectName,
        database: &str,
        default_schema: &str,
    ) -> crate::TableReferenceKind {
        match crate::CatalogView::resolve_table(self, name, database, default_schema) {
            Some(_) => crate::TableReferenceKind::Table,
            None => crate::TableReferenceKind::Unknown,
        }
    }
}

/// A double that resolves the name it is handed to a view, to reach the branch of
/// `view.rs`.
struct OneView;

impl CatalogView for OneView {
    fn resolve_table(
        &self,
        _name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        Some(ResolvedTable {
            object: ObjectId(42),
            table: None,
            columns: Vec::new(),
            kind: ResolvedTableKind::View,
        })
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
        name: &vauban_parser::ObjectName,
        database: &str,
        default_schema: &str,
    ) -> crate::TableReferenceKind {
        match crate::CatalogView::resolve_table(self, name, database, default_schema) {
            Some(_) => crate::TableReferenceKind::Table,
            None => crate::TableReferenceKind::Unknown,
        }
    }
}

fn column(id: ColumnId, index: usize, name: &str, ty: SqlType, nullable: bool) -> ColumnBinding {
    ColumnBinding {
        column: id,
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
        depth: 0,
    }
}

/// Binds the first statement of `text` against `catalog`, in `master` and `dbo`.
fn bind_with(text: &str, catalog: &dyn CatalogView) -> Result<LogicalPlan, SqlError> {
    bind_in(text, catalog, "master", "dbo", 0)
}

/// Binds the statement of index `index`, under the database and default schema given.
fn bind_in(
    text: &str,
    catalog: &dyn CatalogView,
    database: &str,
    schema: &str,
    index: usize,
) -> Result<LogicalPlan, SqlError> {
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let statement = batch.statements.get(index).expect("the statement is there");
    let ctx = BindContext {
        text,
        catalog: Some(catalog),
        database,
        default_schema: schema,
        variables: &NoVariables,
        options: SessionOptions::default(),
    };
    match crate::bind(statement, &ctx)? {
        BoundStatement::Query(plan) => Ok(*plan),
        other => panic!("not a query: {other:?}"),
    }
}

/// The table, the alias and the columns of the `Scan` under the root `Project`, as
/// `SELECT 1 FROM t` builds it.
#[track_caller]
fn scan_of(plan: &LogicalPlan) -> (TableId, &str, &[ColumnBinding]) {
    let LogicalPlan::Project { input, .. } = plan else {
        panic!("the root is not a Project: {plan:?}");
    };
    let LogicalPlan::Scan {
        table,
        columns,
        alias,
        ..
    } = input.as_ref()
    else {
        panic!("the input of the Project is not a Scan: {input:?}");
    };
    (*table, alias.as_str(), columns.as_slice())
}

#[track_caller]
fn err(text: &str, catalog: &dyn CatalogView) -> SqlError {
    bind_with(text, catalog).expect_err("the binding fails")
}

/// `SELECT 1 FROM dbo.t` is a `Project` of the literal over a `Scan` of `t`: the table
/// identifier and the columns come from the catalogue, and the output schema of the
/// statement has the one column the select list asked for.
#[test]
fn from_dbo_t_is_a_scan() {
    let plan = bind_with("SELECT 1 FROM dbo.t", &OneTable).expect("dbo.t resolves");
    let (table, alias, columns) = scan_of(&plan);
    assert_eq!(table, TableId(7));
    assert_eq!(alias, "t");
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[1].name, "a");
    let LogicalPlan::Project { input, schema, .. } = &plan else {
        panic!("scan_of checked the shape")
    };
    // The `Scan` publishes the columns of the table, the statement the select list.
    assert_eq!(input.schema().columns.len(), 2);
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].ty, TypeInfo::new(SqlType::Int, false));
}

/// A one-part name takes the schema of the context, a two-part name its database. The
/// counter-proof is the same text under another default schema and another database:
/// the double resolves `master.dbo.t` alone, so both answer 208 — the two values travel
/// from the [`BindContext`] to the catalogue.
#[test]
fn from_unqualified_uses_dbo() {
    for text in ["SELECT 1 FROM t", "SELECT 1 FROM dbo.t", "SELECT 1 FROM T"] {
        let plan = bind_with(text, &OneTable).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let (_, alias, _) = scan_of(&plan);
        assert_eq!(alias.to_ascii_lowercase(), "t");
    }
    let other_schema =
        bind_in("SELECT 1 FROM t", &OneTable, "master", "guest", 0).expect_err("t is not in guest");
    assert_eq!(other_schema.number, 208);
    assert_eq!(other_schema.message, "Unknown object name 't'.");
    let other_database = bind_in("SELECT 1 FROM dbo.t", &OneTable, "mydb", "dbo", 0)
        .expect_err("dbo.t is not in mydb");
    assert_eq!(other_database.number, 208);
    assert_eq!(other_database.message, "Unknown object name 'dbo.t'.");
}

/// 208 with the name as written, the message of the constructor of `vauban-errors`, and
/// the line of the statement. The spellings are those of the module documentation.
#[test]
fn from_unknown_table_is_208() {
    for (text, printed) in [
        ("SELECT 1 FROM nosuch", "nosuch"),
        ("SELECT 1 FROM dbo.nosuch", "dbo.nosuch"),
        ("SELECT 1 FROM [dbo].[nosuch]", "dbo.nosuch"),
        ("SELECT 1 FROM nosch.nosuch", "nosch.nosuch"),
        ("SELECT 1 FROM master.dbo.nosuch", "master.dbo.nosuch"),
        ("SELECT 1 FROM nodb.dbo.t", "nodb.dbo.t"),
        ("SELECT 1 FROM nodb..t", "nodb..t"),
        ("SELECT 1 FROM nosuch AS z", "nosuch"),
        ("SELECT 1 FROM nosuch z", "nosuch"),
        ("SELECT 1 FROM nosuch WITH (NOLOCK)", "nosuch"),
        ("SELECT 1 FROM nosuch (NOLOCK)", "nosuch"),
        ("SELECT * FROM nosuch", "nosuch"),
    ] {
        let error = err(text, &OneTable);
        assert_eq!(error.number, 208, "{text}: {}", error.message);
        assert_eq!(error.severity, 16, "{text}");
        assert_eq!(error.state, 1, "{text}");
        assert_eq!(
            error.message,
            SqlError::invalid_object_name(printed).message,
            "{text}"
        );
        assert_eq!(error.message, format!("Unknown object name '{printed}'."));
        assert_eq!(error.line, 1, "{text}");
    }
}

/// The line of a 208 is the first line of its statement, not the line of the name and
/// not the first line of the batch — the two batches of the module documentation.
#[test]
fn the_line_of_a_208_is_the_statement() {
    let error = err("SELECT 1 AS n\nFROM\nnosuch", &OneTable);
    assert_eq!(error.number, 208);
    assert_eq!(error.line, 1);
    let second = bind_in(
        "SELECT 1 AS n;\nSELECT 2 AS n\nFROM\nnosuch",
        &OneTable,
        "master",
        "dbo",
        1,
    )
    .expect_err("nosuch resolves to nothing");
    assert_eq!(second.number, 208);
    assert_eq!(second.line, 2);
}

/// A four-part name is 208 here because the binder has no server name to compare its
/// server part with: the two spellings, `nosrv` and `vauban`, both answer it
/// (this test and `four_part_local_server_is_208_too`). `AnyName` resolves the names it
/// is handed, so the refusal is this module's: with it, the same name without its
/// server part binds.
#[test]
fn four_part_wrong_server_is_208() {
    let error = err("SELECT 1 FROM nosrv.master.dbo.t", &AnyName);
    assert_eq!(error.number, 208);
    assert_eq!(error.message, "Unknown object name 'nosrv.master.dbo.t'.");
    assert_eq!(error.line, 1);
    assert!(bind_with("SELECT 1 FROM master.dbo.t", &AnyName).is_ok());
    assert_eq!(
        err("SELECT 1 FROM nosrv.master.dbo.t", &OneTable).number,
        208
    );
}

/// The difference named in the module documentation: on SQL Server a four-part name
/// whose server part is the local instance resolves as its last three parts do, and
/// VaubanDB answers 208 instead. The test that fails the day a server name reaches the
/// binder.
#[test]
fn four_part_local_server_is_208_too() {
    let error = err("SELECT 1 FROM vauban.master.dbo.t", &OneTable);
    assert_eq!(error.number, 208);
    assert_eq!(error.message, "Unknown object name 'vauban.master.dbo.t'.");
}

/// A view holds no row of its own, so this module builds no `Scan` for it: it hands the
/// name to `view.rs`, which expands the definition.
///
/// `OneView` answers a view and **no** definition (its `view_definition` is `None`), so what
/// comes back here is the bug `view::expand` raises for a view the catalogue has no text
/// for, naming the view. The expansion of a real definition is tested in `view.rs`,
/// `from_sys_databases_expands_to_scan_of_internal_table`.
#[test]
fn from_a_view_is_expanded_by_view_rs() {
    let error = err("SELECT 1 FROM sys.tables", &OneView);
    assert_eq!(error.number, 50000, "{}", error.message);
    assert!(error.message.contains("view::expand"), "{}", error.message);
    assert!(
        error.message.contains("sys.tables"),
        "the bug names the view: {}",
        error.message
    );
}

/// Without a catalogue the answer is the internal error, not 208: nothing was
/// consulted. `OneTable` answers the same text with a `Scan`, which is what tells the
/// two apart.
#[test]
fn without_a_catalogue_from_is_internal() {
    let text = "SELECT 1 FROM dbo.t";
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let statement = batch.statements.first().expect("one statement");
    let ctx = BindContext::scalar(text, SessionOptions::default());
    let error = crate::bind(statement, &ctx).expect_err("no catalogue, no FROM");
    assert_eq!(error.number, 50000, "{}", error.message);
    assert!(error.message.contains("FROM"), "{}", error.message);
    assert!(error.message.contains("catalog"), "{}", error.message);
    assert!(bind_with(text, &OneTable).is_ok());
}

/// A hint is kept by the parser and dropped here: once the table is resolved, the two
/// spellings of `NOLOCK` bind to the same `Scan` as the bare name.
#[test]
fn a_resolved_table_ignores_its_hints() {
    for text in [
        "SELECT 1 FROM dbo.t WITH (NOLOCK)",
        "SELECT 1 FROM dbo.t (NOLOCK)",
        "SELECT 1 FROM dbo.t AS z (NOLOCK)",
    ] {
        let plan = bind_with(text, &OneTable).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let (table, _, columns) = scan_of(&plan);
        assert_eq!(table, TableId(7), "{text}");
        assert_eq!(columns.len(), 2, "{text}");
    }
    // The re-reading of the arguments still runs first: arguments that are not a hint
    // word are 215, and an argument that is a column is 207.
    assert_eq!(err("SELECT 1 FROM dbo.t (1)", &OneTable).number, 215);
    assert_eq!(err("SELECT 1 FROM dbo.t (x)", &OneTable).number, 207);
}

/// A name that reaches nothing answers 208 **before** its arguments are re-read, which
/// is the order SQL Server resolves in; a name that resolves keeps the 215 and the 207
/// of the re-reading (`a_resolved_table_ignores_its_hints`).
#[test]
fn an_unknown_object_answers_208_before_its_arguments() {
    for text in [
        "SELECT 1 FROM nosuch (1)",
        "SELECT 1 FROM nosuch (x)",
        "SELECT 1 FROM nosuch ()",
        "SELECT 1 FROM nosuch (NOLOCK)",
        "SELECT 1 FROM nosuch (1, 2)",
        "SELECT 1 FROM dbo.nosuch (1) AS z",
    ] {
        let error = err(text, &OneTable);
        assert_eq!(error.number, 208, "{text}: {}", error.message);
        assert_eq!(error.state, 1, "{text}");
        assert_eq!(error.line, 1, "{text}");
    }
    // `nosuch (1), t (1)` answers the 208 of the first reference, not the 215 of the
    // second. The references are still walked in FROM order, so the mirror text
    // answers the 215 of `dbo.t (1)`.
    let error = err("SELECT 1 FROM nosuch (1), dbo.t (1)", &OneTable);
    assert_eq!(error.number, 208, "{}", error.message);
    assert_eq!(error.message, "Unknown object name 'nosuch'.");
    let error = err("SELECT 1 FROM dbo.t (1), nosuch (1)", &OneTable);
    assert_eq!(error.number, 215, "{}", error.message);
}

/// Without a catalogue the re-reading of the arguments answers as before: nothing was
/// consulted, so no 208 comes out of a name nobody could look up. The counter-proof of
/// the test above.
#[test]
fn without_a_catalogue_arguments_are_still_re_read() {
    for (text, number) in [
        ("SELECT 1 FROM nosuch (1)", 215),
        ("SELECT 1 FROM nosuch (x)", 207),
    ] {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let statement = batch.statements.first().expect("one statement");
        let ctx = BindContext::scalar(text, SessionOptions::default());
        let error = crate::bind(statement, &ctx).expect_err("no catalogue, no FROM");
        assert_eq!(error.number, number, "{text}: {}", error.message);
    }
}

/// `SELECT DISTINCT` over a `FROM` puts a `Distinct` over the projection: dropping the
/// word silently would answer two rows where SQL Server answers one — a table holding
/// the row `1` twice answers one row with `DISTINCT` and two without. Without a `FROM`
/// the word changes nothing over `OneRow` and no node is put on; an unresolved name
/// answers its 208 first, as SQL Server does.
#[test]
fn distinct_over_a_from_is_a_distinct_node() {
    let plan = bind_with("SELECT DISTINCT 1 FROM dbo.t", &OneTable).expect("dbo.t resolves");
    assert!(matches!(plan, LogicalPlan::Distinct(_)), "{plan:?}");
    assert_eq!(err("SELECT DISTINCT 1 FROM nosuch", &OneTable).number, 208);
    // The counter-proof: without the word, the projection is the root of the plan.
    let plain = bind_with("SELECT 1 FROM dbo.t", &OneTable).expect("dbo.t resolves");
    assert!(matches!(plain, LogicalPlan::Project { .. }), "{plain:?}");
    let one_row = crate::bind(
        parse_batch("SELECT DISTINCT 1", &ParseOptions::default())
            .expect("the text parses")
            .statements
            .first()
            .expect("one statement"),
        &BindContext::scalar("SELECT DISTINCT 1", SessionOptions::default()),
    );
    let Ok(BoundStatement::Query(plan)) = one_row else {
        panic!("SELECT DISTINCT 1 binds to a query, got {one_row:?}")
    };
    assert!(matches!(*plan, LogicalPlan::Project { .. }), "{plan:?}");
}

/// The alias the rest of the query names the source by, with and without `AS`.
#[test]
fn an_alias_replaces_the_object_part() {
    let ident = |value: &str| Ident {
        value: value.to_owned(),
        quoted: false,
    };
    let name = ObjectName {
        server: None,
        database: None,
        schema: Some(ident("dbo")),
        name: ident("t"),
        span: Span::EMPTY,
    };
    assert_eq!(alias_of(&name, None), "t");
    assert_eq!(alias_of(&name, Some(&ident("z"))), "z");
    for text in ["SELECT 1 FROM dbo.t AS z", "SELECT 1 FROM dbo.t z"] {
        let plan = bind_with(text, &OneTable).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let (_, alias, _) = scan_of(&plan);
        assert_eq!(alias, "z", "{text}");
    }
}

/// The references `bind_from` does not bind yet, each message naming the form.
#[test]
fn the_other_references_name_their_form() {
    // Derived tables are bound by `subquery.rs`; `APPLY` is still unimplemented.
    assert!(bind_with("SELECT 1 FROM (SELECT 1 AS n) AS d", &OneTable).is_ok());
    let (text, expected) = ("SELECT 1 FROM dbo.t AS a CROSS APPLY dbo.t AS b", "APPLY");
    let error = err(text, &OneTable);
    assert_eq!(error.number, 50000, "{text}: {}", error.message);
    assert!(
        error.message.contains(expected),
        "{text}: {}",
        error.message
    );
}
