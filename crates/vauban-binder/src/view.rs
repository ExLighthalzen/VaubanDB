//! Expansion of a view into the plan of its definition.
//!
//! A system view of VaubanDB is a real query: the catalogue keeps
//! the T-SQL text of a `SELECT` over one denormalised internal table, and
//! [`CatalogView::view_definition`](crate::CatalogView::view_definition) hands that text
//! over. [`expand`] parses it, binds it, and gives `names::scan` the plan to put where the
//! `Scan` of the view would have been. Nothing here is specific to `sys.databases` or to
//! `sys.tables`: a view whose definition the catalogue registers is expanded by this code
//! without a line being added, which is how the `sys.*` and `INFORMATION_SCHEMA` views
//! arrive. `CREATE VIEW` is out of scope: no user view reaches this file.
//!
//! The tests below exercise the expansion against doubles of
//! [`CatalogView`](crate::CatalogView) that answer a `View` and its text, on definitions
//! written in the shape `views/sys_tables.rs` produces (the module `views` of
//! `vauban-catalog` is `pub(crate)`, so its text is copied into the tests rather than read).
//!
//! # Where the definition is bound
//!
//! In the **database and the schema of the view**, not in those of the statement that names
//! it: `master.sys.databases` read from `mydb` binds its definition in `master` and in
//! `sys`. Those two values come from the name as written, completed from the
//! [`BindContext`] exactly as `CatalogView::resolve_table` completes them to find the view
//! itself — so a definition that names an object without qualifying it reaches the objects
//! of the view's own database (unit test
//! `the_definition_binds_in_the_database_and_schema_of_the_view`, whose counter-proof is the
//! same unqualified definition read from another database). The definitions of the `sys.*`
//! views spell `master.dbo.vauban_sys_*` in full, so they do not depend on it; views
//! installed in a user database will.
//!
//! The inner [`BindContext`] carries the definition as its `text` (so that a `near '…'`
//! slices the text the error was raised in), the same catalogue, the `SET` options of the
//! session, and **no variable**: a `@x` declared by the batch is not in scope inside a view
//! (`a_definition_sees_no_variable_of_the_batch`). The text is parsed with
//! [`ParseOptions::default`] — `QUOTED_IDENTIFIER ON` — because the definition is the
//! catalogue's own text and a session that turned the option off must not change the meaning
//! of the `[precision]` of `sys.types` in it.
//!
//! # What the outer query sees
//!
//! The expanded plan is a `Project` (the select list of the definition), possibly over a
//! `Filter` (its `WHERE`) over the `Scan` of the internal table. The columns the outer query
//! refers to therefore index the **output of that plan**, by position, and not the row of the
//! internal table: `vauban_sys_databases` holds `database_id`, `name`, `collation_name` in
//! that order while `sys.databases` publishes `name` first, so the outer `name` is position 0
//! where the internal `name` has ordinal 1 ([`source_columns`], unit test
//! `the_outer_columns_index_the_output_of_the_view`).
//!
//! # Errors that are bugs, and the guard against a cycle
//!
//! The client did not write the definition, so what goes wrong inside it is no message for
//! the client: a text that does not parse, a text that is not a single `SELECT`, a missing
//! definition for an object the catalogue called a view, and a binding error of the
//! definition each come out as `InternalError::Bug` — 50000 — naming the view and the error
//! it carried (`a_definition_that_is_not_one_select_is_a_bug`,
//! `expanding_unknown_view_definition_is_a_bug`, `a_definition_that_does_not_bind_is_a_bug`).
//! A 102 or a 208 about `master.dbo.vauban_sys_objects` would send a reader of the log
//! looking for a user's typo.
//!
//! A definition that names its own view would recurse forever. [`Expansion`] keeps the
//! identifiers being expanded on the thread that binds — the same shape as the depth guard of
//! `depth.rs`, one stack per binding because the engine is synchronous — and answers a
//! `Bug` on the second entry for one identifier (unit tests
//! `a_view_that_refers_to_itself_is_a_bug` and `two_views_that_refer_to_each_other_are_a_bug`).
//! The system views are not recursive, so the guard is a safety net and not a behaviour a
//! client can reach.

use std::cell::RefCell;

use vauban_catalog::{ColumnId, ObjectId};
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Ident, ObjectName, ParseOptions, Statement, parse_batch};

use crate::bound::{ColumnBinding, LogicalPlan, OutputSchema};
use crate::context::{BindContext, NoVariables, ResolvedTable};
use crate::query::{bind_select, bug, dotted};

/// Expands the view `resolved`, written `written`, into the plan of its definition.
///
/// The answer replaces the `Scan` `names::scan` builds for a table: a view holds no row of
/// its own. The leaf of that plan is the `Scan` of the internal table the definition reads,
/// so nothing downstream — `planner`, `executor` — needs to know a view was named.
///
/// # Errors
///
/// The internal error 50000, naming the view, when the catalogue carries no definition for
/// it, when the definition is not a single `SELECT`, when it does not parse, when it refers
/// to itself, and when binding it fails (module documentation). No error this function builds
/// reaches the client with a number of its own.
pub(crate) fn expand(
    resolved: &ResolvedTable,
    written: &ObjectName,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let name = dotted(written);
    let catalog = ctx.catalog.ok_or_else(|| {
        bug(format!(
            "view::expand: '{name}' was resolved as a view without a catalogue to read its definition from"
        ))
    })?;
    let definition = catalog.view_definition(resolved.object).ok_or_else(|| {
        bug(format!(
            "view::expand: the catalogue resolved '{name}' as a view and carries no definition for it"
        ))
    })?;
    let _guard = Expansion::enter(resolved.object).ok_or_else(|| {
        bug(format!(
            "view::expand: the definition of '{name}' refers to itself, directly or through another view"
        ))
    })?;
    let batch = parse_batch(definition, &ParseOptions::default())
        .map_err(|err| failed(&name, "does not parse", &err))?;
    let [Statement::Select(select)] = batch.statements.as_slice() else {
        return Err(bug(format!(
            "view::expand: the definition of '{name}' is not a single SELECT: {definition}"
        )));
    };
    let no_variables = NoVariables;
    let inner = BindContext {
        text: definition,
        catalog: ctx.catalog,
        database: part(written.database.as_ref(), ctx.database),
        default_schema: part(written.schema.as_ref(), ctx.default_schema),
        variables: &no_variables,
        options: ctx.options,
    };
    bind_select(select, &inner).map_err(|err| failed(&name, "does not bind", &err))
}

/// The columns the outer query sees over an expanded view: one per column of the output of
/// the plan, in that output's order.
///
/// `index` is the **position** in that output, which is what the executor reads to pick a value
/// out of the row the expanded plan produces — the row of the `Project` of the definition,
/// not the row of the internal table `storage` holds. `column` is that position, 1-based, as
/// `sys.columns.column_id` numbers the columns of a view: a [`ColumnBinding`] needs an
/// identifier and a view has no `ColumnId` of its own in the catalogue. The projection reads
/// `index`, and the tests of this file assert both fields
/// (`source_columns_are_numbered_by_position`).
pub(crate) fn source_columns(schema: &OutputSchema) -> Vec<ColumnBinding> {
    schema
        .columns
        .iter()
        .enumerate()
        .map(|(position, column)| ColumnBinding {
            column: ColumnId(i32::try_from(position.saturating_add(1)).unwrap_or(i32::MAX)),
            index: position,
            name: column.name.clone(),
            ty: column.ty.clone(),
        })
        .collect()
}

/// The part of a name that was written, or the value the context completes it with — the rule
/// `CatalogView::resolve_table` applies to find the object (`names.rs`, module
/// documentation).
fn part<'a>(written: Option<&'a Ident>, default: &'a str) -> &'a str {
    written.map_or(default, |ident| ident.value.as_str())
}

/// The bug a definition that will not go through raises: which view, what failed, and the
/// error it failed with, so that a log names the number without that number reaching the
/// client.
fn failed(name: &str, what: &str, err: &SqlError) -> SqlError {
    bug(format!(
        "view::expand: the definition of '{name}' {what}: error {} {}",
        err.number, err.message
    ))
}

thread_local! {
    /// The views being expanded on this thread, innermost last.
    ///
    /// One binding at a time runs on a thread (`depth.rs` says the same of its counter), so
    /// this is the stack of the expansion in progress. It is borrowed inside
    /// [`Expansion::enter`] and in the `Drop` of the guard, not across a call that could
    /// re-enter.
    static EXPANDING: RefCell<Vec<ObjectId>> = const { RefCell::new(Vec::new()) };
}

/// One view held open while its definition is bound; leaves the stack when dropped.
///
/// `#[must_use]`: a guard dropped at once would guard nothing. The `Drop` runs on the way
/// back **and** on the `?` of an error, so a refused definition leaves no identifier behind
/// for the next statement of the batch (unit test
/// `a_refused_expansion_leaves_no_identifier_behind`).
#[derive(Debug)]
#[must_use = "the view is being expanded only as long as the guard lives"]
pub(crate) struct Expansion(ObjectId);

impl Expansion {
    /// Enters the expansion of `object`, or `None` when it is already being expanded.
    fn enter(object: ObjectId) -> Option<Self> {
        EXPANDING.with_borrow_mut(|expanding| {
            if expanding.contains(&object) {
                return None;
            }
            expanding.push(object);
            Some(Expansion(object))
        })
    }
}

impl Drop for Expansion {
    fn drop(&mut self) {
        EXPANDING.with_borrow_mut(|expanding| {
            // The guards are dropped innermost first, so the identifier to remove is the
            // last one; the `retain` is the safety net, and says less about the order.
            if expanding.last() == Some(&self.0) {
                expanding.pop();
            } else {
                expanding.retain(|entry| *entry != self.0);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{EXPANDING, source_columns};
    use crate::bound::{
        BoundExprKind, BoundProjection, BoundStatement, ColumnBinding, LogicalPlan,
    };
    use crate::context::{
        BindContext, CatalogView, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
        VariableScope,
    };
    use vauban_catalog::{ColumnId, ObjectId, TableId};
    use vauban_errors::SqlError;
    use vauban_parser::{
        Ident, ObjectName, ParseOptions, QueryBody, Statement, TableRef, parse_batch,
    };
    use vauban_types::{Len, SqlType, TypeInfo};

    /// `sysname`, the type of a name column of an internal table (`bootstrap.rs`).
    const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

    /// The identifier of the view `sys.databases` in the doubles below.
    const DATABASES_VIEW: ObjectId = ObjectId(-11);

    /// The identifier of the view `sys.tables` in the doubles below.
    const TABLES_VIEW: ObjectId = ObjectId(-12);

    /// The storage identifier of `master.dbo.vauban_sys_databases` in the doubles below.
    const DATABASES_TABLE: TableId = TableId(11);

    /// The storage identifier of `master.dbo.vauban_sys_objects` in the doubles below.
    const OBJECTS_TABLE: TableId = TableId(12);

    /// The text of `sys.databases`, the first four items of the 89 of `views/sys_core.rs`.
    ///
    /// The layout is the one the `definition` helper of that file produces — `SELECT `, one
    /// item per line, `,\n       ` between two of them, an item whose expression is its own
    /// name written bare, `\n  FROM master.dbo.<table>` at the end — and the items are its
    /// first four, in its order. The `views` module of `vauban-catalog` is `pub(crate)`, so
    /// the text is copied here rather than read from it (module documentation).
    const DATABASES_DEFINITION: &str = "SELECT name,\n       database_id,\n       \
         CAST(NULL AS int) AS source_database_id,\n       \
         CAST(NULL AS varbinary(85)) AS owner_sid\n  FROM master.dbo.vauban_sys_databases";

    /// The text of `sys.tables`, the first seven items of the 48 of `views/sys_tables.rs`
    /// (`name`, `object_id`, `principal_id`, `schema_id`, `parent_object_id`, `type`,
    /// `type_desc`), with the filter that file gives it — the two-line `WHERE` included.
    const TABLES_DEFINITION: &str = "SELECT name,\n       object_id,\n       \
         CAST(NULL AS int) AS principal_id,\n       schema_id,\n       parent_object_id,\n       \
         type,\n       type_desc\n  FROM master.dbo.vauban_sys_objects\n \
         WHERE database_id = DB_ID()\n   AND type = 'U '";

    /// A double of the catalogue: the views it knows with their text, and the tables it knows
    /// with their rows in `storage`.
    ///
    /// An entry is `(database, schema, name, …)`, compared without regard to case as a `CI`
    /// collation compares identifiers, and the parts the written name leaves out are filled
    /// from the two arguments `resolve_table` receives — which is what makes the database and
    /// the schema of the inner context observable.
    struct Fake {
        /// `(database, schema, name, object, definition)`; a `None` definition is a view the
        /// catalogue has no text for.
        views: Vec<(
            &'static str,
            &'static str,
            &'static str,
            ObjectId,
            Option<&'static str>,
        )>,
        /// `(database, schema, name, storage identifier, columns)`.
        tables: Vec<(
            &'static str,
            &'static str,
            &'static str,
            TableId,
            Vec<ColumnBinding>,
        )>,
    }

    impl CatalogView for Fake {
        fn resolve_table(
            &self,
            name: &ObjectName,
            database: &str,
            default_schema: &str,
        ) -> Option<ResolvedTable> {
            if name.server.is_some() {
                return None;
            }
            let written = |part: Option<&Ident>, default: &str| {
                part.map_or_else(|| default.to_owned(), |ident| ident.value.clone())
            };
            let db = written(name.database.as_ref(), database);
            let schema = written(name.schema.as_ref(), default_schema);
            let object = &name.name.value;
            let same = |d: &str, s: &str, n: &str| {
                d.eq_ignore_ascii_case(&db)
                    && s.eq_ignore_ascii_case(&schema)
                    && n.eq_ignore_ascii_case(object)
            };
            if let Some((.., id, _)) = self.views.iter().find(|(d, s, n, ..)| same(d, s, n)) {
                return Some(ResolvedTable {
                    object: *id,
                    table: None,
                    columns: Vec::new(),
                    kind: ResolvedTableKind::View,
                });
            }
            let (.., id, columns) = self.tables.iter().find(|(d, s, n, ..)| same(d, s, n))?;
            Some(ResolvedTable {
                object: ObjectId(0),
                table: Some(*id),
                columns: columns.clone(),
                kind: ResolvedTableKind::Table,
            })
        }

        fn view_definition(&self, object: ObjectId) -> Option<&str> {
            self.views
                .iter()
                .find(|(.., id, _)| *id == object)
                .and_then(|(.., definition)| *definition)
        }
    }

    impl Fake {
        /// The catalogue of the tests: `sys.databases` and `sys.tables` over the two internal
        /// tables of `bootstrap.rs` and of `views/sys_tables.rs`, with the columns and the
        /// order those files declare.
        fn system_views() -> Self {
            Fake {
                views: vec![
                    (
                        "master",
                        "sys",
                        "databases",
                        DATABASES_VIEW,
                        Some(DATABASES_DEFINITION),
                    ),
                    (
                        "master",
                        "sys",
                        "tables",
                        TABLES_VIEW,
                        Some(TABLES_DEFINITION),
                    ),
                ],
                tables: Fake::internal_tables(),
            }
        }

        /// The two internal tables, as the bootstrap declares their columns.
        fn internal_tables() -> Vec<(
            &'static str,
            &'static str,
            &'static str,
            TableId,
            Vec<ColumnBinding>,
        )> {
            vec![
                (
                    "master",
                    "dbo",
                    "vauban_sys_databases",
                    DATABASES_TABLE,
                    columns(&[
                        (1, 0, "database_id", SqlType::Int, false),
                        (2, 1, "name", SYSNAME, false),
                        (3, 2, "collation_name", SYSNAME, true),
                    ]),
                ),
                (
                    "master",
                    "dbo",
                    "vauban_sys_objects",
                    OBJECTS_TABLE,
                    columns(&[
                        (1, 0, "database_id", SqlType::Int, false),
                        (2, 1, "object_id", SqlType::Int, false),
                        (3, 2, "name", SYSNAME, false),
                        (4, 3, "schema_id", SqlType::Int, false),
                        (5, 4, "parent_object_id", SqlType::Int, false),
                        (6, 5, "type", SqlType::Char(Len::Fixed(2)), false),
                        (7, 6, "type_desc", SqlType::NVarChar(Len::Fixed(60)), false),
                    ]),
                ),
            ]
        }
    }

    /// A scope that knows one variable, `@x`, to tell the scope of the statement from the
    /// scope of a definition (`a_definition_sees_no_variable_of_the_batch`).
    struct KnowsX;

    impl VariableScope for KnowsX {
        fn type_of(&self, name: &str) -> Option<TypeInfo> {
            (name == "@x").then(|| TypeInfo::new(SqlType::Int, true))
        }
    }

    /// `(column identifier, ordinal, name, type, nullable)` into bindings.
    fn columns(items: &[(i32, usize, &str, SqlType, bool)]) -> Vec<ColumnBinding> {
        items
            .iter()
            .map(|(id, index, name, ty, nullable)| ColumnBinding {
                column: ColumnId(*id),
                index: *index,
                name: (*name).to_owned(),
                ty: TypeInfo::new(*ty, *nullable),
            })
            .collect()
    }

    /// Binds the first statement of `text` against `catalog`, in `database` and `schema`, with
    /// `variables` in scope.
    ///
    /// The `sysfn` registry is filled first, as `star.rs` does in `bind_with`: against an empty
    /// registry the `DB_ID()` of the filter of `sys.tables` answers 195, and the three tests
    /// that expand that view would fail under `cargo test -p vauban-binder --lib view::`
    /// while passing in the whole binary, where another test had filled the registry.
    /// `crate::call::tests::registry` fills it once for the test binary, so no test here
    /// depends on the order the others ran in.
    fn bind_scoped(
        text: &str,
        catalog: &dyn CatalogView,
        database: &str,
        schema: &str,
        variables: &dyn VariableScope,
    ) -> Result<LogicalPlan, SqlError> {
        crate::call::tests::registry();
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let statement = batch.statements.first().expect("one statement");
        let ctx = BindContext {
            text,
            catalog: Some(catalog),
            database,
            default_schema: schema,
            variables,
            options: SessionOptions::default(),
        };
        match crate::bind(statement, &ctx)? {
            BoundStatement::Query(plan) => Ok(*plan),
            other => panic!("not a query: {other:?}"),
        }
    }

    /// Binds in `database` and `schema`, with no variable in scope.
    fn bind_in(
        text: &str,
        catalog: &dyn CatalogView,
        database: &str,
        schema: &str,
    ) -> Result<LogicalPlan, SqlError> {
        bind_scoped(text, catalog, database, schema, &NoVariables)
    }

    /// Binds in `master` and `dbo`, the pair a client gets without a database name.
    fn bind(text: &str, catalog: &dyn CatalogView) -> Result<LogicalPlan, SqlError> {
        bind_in(text, catalog, "master", "dbo")
    }

    #[track_caller]
    fn err(text: &str, catalog: &dyn CatalogView) -> SqlError {
        bind(text, catalog).expect_err("the binding fails")
    }

    /// The `Scan` at the bottom of a plan, through the `Project`, `Filter` and `Limit` nodes
    /// above it, with the columns it reads.
    #[track_caller]
    fn leaf_scan(plan: &LogicalPlan) -> (TableId, &[ColumnBinding]) {
        let mut node = plan;
        loop {
            match node {
                LogicalPlan::Scan { table, columns, .. } => {
                    return (*table, columns.as_slice());
                }
                LogicalPlan::Project { input, .. }
                | LogicalPlan::Filter { input, .. }
                | LogicalPlan::Limit { input, .. } => node = input.as_ref(),
                other => panic!("no Scan at the bottom of the plan: {other:?}"),
            }
        }
    }

    /// The projections of the outermost `Project` of a plan.
    #[track_caller]
    fn projections(plan: &LogicalPlan) -> &[BoundProjection] {
        let LogicalPlan::Project { exprs, .. } = plan else {
            panic!("the root is not a Project: {plan:?}");
        };
        exprs.as_slice()
    }

    /// The plan of the definition, under the `Project` of the statement.
    #[track_caller]
    fn expanded(plan: &LogicalPlan) -> &LogicalPlan {
        let LogicalPlan::Project { input, .. } = plan else {
            panic!("the root is not the Project of the statement: {plan:?}");
        };
        input.as_ref()
    }

    /// A name of one to three parts, as the parser builds it.
    fn object_name(parts: &[&str]) -> ObjectName {
        let text = format!("SELECT 1 FROM {}", parts.join("."));
        let batch = parse_batch(&text, &ParseOptions::default()).expect("the text parses");
        let Some(Statement::Select(select)) = batch.statements.first() else {
            panic!("not a SELECT");
        };
        let QueryBody::Select(spec) = &select.body else {
            panic!("not a query specification");
        };
        match spec.from.first().expect("one table reference") {
            TableRef::Table { name, .. } => name.clone(),
            other => panic!("not a table reference: {other:?}"),
        }
    }

    /// The plan of `SELECT name FROM sys.databases` keeps no `Scan` of the view, and reads
    /// the internal table instead.
    ///
    /// The identifier compared is the one the catalogue publishes for
    /// `master.dbo.vauban_sys_databases`, read from the same double through `resolve_table`,
    /// and that double gives the view no storage identifier at all (`table: None`, as
    /// `catalog_view.rs` does for a view), so no `Scan` of the plan could carry the view. The
    /// counter-proof that `TableId(11)` is not a constant of the fixture: the other internal
    /// table of the double, reached through `sys.tables`, answers `TableId(12)`.
    #[test]
    fn from_sys_databases_expands_to_scan_of_internal_table() {
        let fake = Fake::system_views();
        let plan = bind("SELECT name FROM sys.databases", &fake).expect("sys.databases expands");
        let (table, columns) = leaf_scan(&plan);
        assert_eq!(table, DATABASES_TABLE);
        assert_eq!(columns.len(), 3, "the columns of the internal table");
        let internal = fake
            .resolve_table(
                &object_name(&["master", "dbo", "vauban_sys_databases"]),
                "master",
                "dbo",
            )
            .expect("the internal table resolves");
        assert_eq!(
            internal.table,
            Some(table),
            "the leaf is the internal table"
        );
        let view = fake
            .resolve_table(&object_name(&["sys", "databases"]), "master", "dbo")
            .expect("the view resolves");
        assert_eq!(view.kind, ResolvedTableKind::View);
        assert_eq!(view.table, None, "a view has no row of its own");
        let tables = bind("SELECT name FROM sys.tables", &fake).expect("sys.tables expands");
        assert_eq!(leaf_scan(&tables).0, OBJECTS_TABLE);
    }

    /// The statement publishes one column, named `name`, of the type the internal column
    /// carries — `nvarchar(128)`, as `sysname` is.
    #[test]
    fn select_name_from_sys_databases_has_column_name() {
        let plan = bind("SELECT name FROM sys.databases", &Fake::system_views())
            .expect("sys.databases expands");
        let schema = plan.schema();
        assert_eq!(schema.columns.len(), 1);
        assert_eq!(schema.columns[0].name, "name");
        assert_eq!(schema.columns[0].ty, TypeInfo::new(SYSNAME, false));
    }

    /// `SELECT * FROM sys.databases` publishes the columns of the definition, in its order
    /// and under its names — the four of [`DATABASES_DEFINITION`], two of which are `CAST`
    /// expressions named by an `AS` and carry the type of the `CAST`.
    #[test]
    fn select_star_from_sys_databases_has_the_columns_of_the_definition() {
        let plan = bind("SELECT * FROM sys.databases", &Fake::system_views())
            .expect("sys.databases expands");
        let columns = &plan.schema().columns;
        let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
        assert_eq!(
            names,
            ["name", "database_id", "source_database_id", "owner_sid"]
        );
        assert_eq!(columns[0].ty, TypeInfo::new(SYSNAME, false));
        assert_eq!(columns[1].ty, TypeInfo::new(SqlType::Int, false));
        assert_eq!(columns[2].ty.ty, SqlType::Int, "CAST(NULL AS int)");
        assert!(columns[2].ty.nullable, "CAST(NULL AS int) is nullable");
    }

    /// The columns of the outer query index the **output of the view**, not the row of the
    /// internal table.
    ///
    /// What separates the two is their order: `vauban_sys_databases` holds `database_id`,
    /// `name`, `collation_name` and the view publishes `name` first, so the outer `name` is
    /// position **0** where the internal `name` has ordinal **1** — and the `Scan` under the
    /// `Project` of the definition keeps that ordinal 1, which is what the executor reads to
    /// pick the value out of the row of `storage`.
    #[test]
    fn the_outer_columns_index_the_output_of_the_view() {
        let plan = bind("SELECT name FROM sys.databases", &Fake::system_views())
            .expect("sys.databases expands");
        let outer = projections(&plan);
        assert_eq!(outer.len(), 1);
        let BoundExprKind::ColumnRef(binding) = &outer[0].expr.kind else {
            panic!(
                "the outer column is not a ColumnRef: {:?}",
                outer[0].expr.kind
            );
        };
        assert_eq!(binding.index, 0, "position in the output of the view");
        assert_eq!(binding.name, "name");
        let (_, internal) = leaf_scan(&plan);
        let name = internal
            .iter()
            .find(|column| column.name == "name")
            .expect("the internal table has a column name");
        assert_eq!(name.index, 1, "ordinal in the row of storage");
    }

    /// The `WHERE` of a definition becomes a `Filter` **under** the `Project` of its select
    /// list, so it may name a column the view does not publish: `sys.tables` filters on
    /// `database_id`, which is not one of its columns.
    ///
    /// The counter-proof that the `Filter` comes from the definition and not from the
    /// statement: `sys.databases`, whose definition has no `WHERE`, expands to a `Project`
    /// straight over its `Scan`.
    #[test]
    fn a_filter_of_the_definition_sits_under_its_project() {
        let fake = Fake::system_views();
        let plan = bind("SELECT name FROM sys.tables", &fake).expect("sys.tables expands");
        let LogicalPlan::Project { input: view, .. } = expanded(&plan) else {
            panic!("the expanded view is not a Project: {:?}", expanded(&plan));
        };
        assert!(
            matches!(view.as_ref(), LogicalPlan::Filter { .. }),
            "the WHERE of the definition is missing: {view:?}"
        );
        assert!(
            !plan
                .schema()
                .columns
                .iter()
                .any(|column| column.name == "database_id"),
            "the filtered column is not published"
        );
        let databases = bind("SELECT name FROM sys.databases", &fake).expect("expands");
        let LogicalPlan::Project { input: view, .. } = expanded(&databases) else {
            panic!("the expanded view is not a Project");
        };
        assert!(
            matches!(view.as_ref(), LogicalPlan::Scan { .. }),
            "a definition without a WHERE has no Filter: {view:?}"
        );
    }

    /// A qualifier names an expanded view as it names a table: the alias when one was
    /// written, the schema and the object part otherwise (`star.rs`, `Source::matches`).
    #[test]
    fn a_qualifier_names_an_expanded_view() {
        let fake = Fake::system_views();
        for text in [
            "SELECT sys.databases.name FROM sys.databases",
            "SELECT databases.name FROM sys.databases",
            "SELECT d.name FROM sys.databases AS d",
            "SELECT d.* FROM sys.databases AS d",
        ] {
            let plan = bind(text, &fake).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            assert_eq!(leaf_scan(&plan).0, DATABASES_TABLE, "{text}");
        }
        // An alias hides the name of the view, as it hides the name of a table.
        let hidden = err("SELECT databases.name FROM sys.databases AS d", &fake);
        assert_eq!(hidden.number, 4104, "{}", hidden.message);
        // A column the definition does not publish is 207 — the scope is the output of the
        // view, while the internal table does hold a column `collation_name`.
        let absent = err("SELECT collation_name FROM sys.databases", &fake);
        assert_eq!(absent.number, 207, "{}", absent.message);
        assert_eq!(absent.message, "Unknown column name 'collation_name'.");
    }

    /// `sys` is not `dbo`, and no fallback looks for a system view under the default
    /// schema. `FROM tables` and `FROM databases` in `dbo` answer 208 where
    /// their `sys.`-qualified spelling expands.
    #[test]
    fn unqualified_tables_in_dbo_is_208() {
        let fake = Fake::system_views();
        for (text, printed) in [
            ("SELECT name FROM tables", "tables"),
            ("SELECT name FROM databases", "databases"),
            ("SELECT name FROM dbo.tables", "dbo.tables"),
        ] {
            let error = err(text, &fake);
            assert_eq!(error.number, 208, "{text}: {}", error.message);
            assert_eq!(error.message, format!("Unknown object name '{printed}'."));
        }
        assert!(bind("SELECT name FROM sys.tables", &fake).is_ok());
        assert!(bind("SELECT name FROM sys.databases", &fake).is_ok());
    }

    /// A `View` whose definition the catalogue does not carry is a bug, not a message for
    /// the client: a registered view carries its text.
    #[test]
    fn expanding_unknown_view_definition_is_a_bug() {
        let fake = Fake {
            views: vec![("master", "sys", "databases", DATABASES_VIEW, None)],
            tables: Vec::new(),
        };
        let error = err("SELECT name FROM sys.databases", &fake);
        assert_eq!(error.number, 50000, "{}", error.message);
        assert!(
            error.message.contains("sys.databases"),
            "the bug names the view: {}",
            error.message
        );
        assert!(error.message.contains("no definition"), "{}", error.message);
    }

    /// A definition that names its own view is a bug, and the stack is left clean for the
    /// next statement.
    #[test]
    fn a_view_that_refers_to_itself_is_a_bug() {
        let fake = Fake {
            views: vec![(
                "master",
                "sys",
                "databases",
                DATABASES_VIEW,
                Some("SELECT name FROM sys.databases"),
            )],
            tables: Vec::new(),
        };
        let error = err("SELECT name FROM sys.databases", &fake);
        assert_eq!(error.number, 50000, "{}", error.message);
        assert!(
            error.message.contains("refers to itself"),
            "{}",
            error.message
        );
        assert_eq!(EXPANDING.with_borrow(Vec::len), 0);
    }

    /// Two views that name each other stop at the second entry of the first identifier: the
    /// guard holds identifiers, not a depth.
    #[test]
    fn two_views_that_refer_to_each_other_are_a_bug() {
        let fake = Fake {
            views: vec![
                (
                    "master",
                    "sys",
                    "databases",
                    DATABASES_VIEW,
                    Some("SELECT name FROM sys.tables"),
                ),
                (
                    "master",
                    "sys",
                    "tables",
                    TABLES_VIEW,
                    Some("SELECT name FROM sys.databases"),
                ),
            ],
            tables: Vec::new(),
        };
        let error = err("SELECT name FROM sys.databases", &fake);
        assert_eq!(error.number, 50000, "{}", error.message);
        assert!(
            error.message.contains("refers to itself"),
            "{}",
            error.message
        );
        assert_eq!(EXPANDING.with_borrow(Vec::len), 0);
    }

    /// A refused expansion leaves no identifier on the stack: the next statement binds as the
    /// first did, and the counter-proof of the guard's `Drop` is the same view expanding
    /// twice in a row.
    #[test]
    fn a_refused_expansion_leaves_no_identifier_behind() {
        let fake = Fake::system_views();
        assert_eq!(EXPANDING.with_borrow(Vec::len), 0);
        assert_eq!(err("SELECT nosuch FROM sys.databases", &fake).number, 207);
        assert_eq!(EXPANDING.with_borrow(Vec::len), 0);
        for _ in 0..2 {
            assert!(bind("SELECT name FROM sys.databases", &fake).is_ok());
            assert_eq!(EXPANDING.with_borrow(Vec::len), 0);
        }
    }

    /// A definition that does not parse, and one that is not a single `SELECT`, are bugs
    /// naming the view: the client did not write that text, so it gets no 102 and no 111.
    #[test]
    fn a_definition_that_is_not_one_select_is_a_bug() {
        for (definition, expected) in [
            ("SELECT FROM", "does not parse"),
            ("SELECT 1; SELECT 2", "not a single SELECT"),
            ("", "not a single SELECT"),
            ("CREATE TABLE dbo.t (a int)", "not a single SELECT"),
        ] {
            let fake = Fake {
                views: vec![(
                    "master",
                    "sys",
                    "databases",
                    DATABASES_VIEW,
                    Some(definition),
                )],
                tables: Vec::new(),
            };
            let error = err("SELECT name FROM sys.databases", &fake);
            assert_eq!(error.number, 50000, "{definition}: {}", error.message);
            assert!(
                error.message.contains(expected) && error.message.contains("sys.databases"),
                "{definition}: {}",
                error.message
            );
        }
    }

    /// A binding error of the definition is a bug too, naming the view and the number it
    /// carried: a 208 about `master.dbo.vauban_sys_databases` is not a client's typo.
    #[test]
    fn a_definition_that_does_not_bind_is_a_bug() {
        let fake = Fake {
            views: vec![(
                "master",
                "sys",
                "databases",
                DATABASES_VIEW,
                Some(DATABASES_DEFINITION),
            )],
            tables: Vec::new(),
        };
        let error = err("SELECT name FROM sys.databases", &fake);
        assert_eq!(error.number, 50000, "{}", error.message);
        assert!(error.message.contains("does not bind"), "{}", error.message);
        assert!(error.message.contains("error 208"), "{}", error.message);
        assert!(
            error.message.contains("sys.databases"),
            "the bug names the view: {}",
            error.message
        );
        // The counter-proof: the same definition over a catalogue that holds the internal
        // table binds.
        assert!(bind("SELECT name FROM sys.databases", &Fake::system_views()).is_ok());
    }

    /// The definition is bound in the database and the schema of the **view**, not in those
    /// of the statement.
    ///
    /// The definition below names its internal table without a database and without a schema,
    /// and the double holds that table in `mydb.sys` alone. `mydb.sys.v` read from `master`
    /// and `dbo` therefore expands, and the counter-proof says the two values are the view's:
    /// a copy of the same view in `master.dbo` binds the same text in `master.dbo`, where the
    /// double holds no such table.
    #[test]
    fn the_definition_binds_in_the_database_and_schema_of_the_view() {
        let fake = Fake {
            views: vec![
                (
                    "mydb",
                    "sys",
                    "v",
                    DATABASES_VIEW,
                    Some("SELECT name FROM vauban_sys_v"),
                ),
                (
                    "master",
                    "dbo",
                    "v",
                    TABLES_VIEW,
                    Some("SELECT name FROM vauban_sys_v"),
                ),
            ],
            tables: vec![(
                "mydb",
                "sys",
                "vauban_sys_v",
                DATABASES_TABLE,
                columns(&[(1, 0, "name", SYSNAME, false)]),
            )],
        };
        let plan = bind_in("SELECT name FROM mydb.sys.v", &fake, "master", "dbo")
            .expect("the view of mydb.sys expands");
        assert_eq!(leaf_scan(&plan).0, DATABASES_TABLE);
        // Read from `mydb`, the unqualified spelling of the same view expands too.
        let from_mydb = bind_in("SELECT name FROM sys.v", &fake, "mydb", "dbo")
            .expect("the same view, named from its own database");
        assert_eq!(leaf_scan(&from_mydb).0, DATABASES_TABLE);
        // Counter-proof: the copy in `master.dbo` binds its definition in `master.dbo`,
        // where the double holds no `vauban_sys_v`.
        let other_database = err("SELECT name FROM dbo.v", &fake);
        assert_eq!(other_database.number, 50000, "{}", other_database.message);
        assert!(
            other_database.message.contains("error 208"),
            "{}",
            other_database.message
        );
    }

    /// The variables of the batch are not in scope inside a definition: a `@x` in a definition
    /// is a bug of the catalogue, not a 137 for the client.
    ///
    /// The scope of the test **does** know `@x`: the outer statement binds `SELECT @x`
    /// under it, and the definition that names the same `@x` still answers the bug carrying
    /// the 137 — which is what distinguishes the `NoVariables` this function hands the inner
    /// context from `ctx.variables`, whose forwarding would have bound that definition.
    #[test]
    fn a_definition_sees_no_variable_of_the_batch() {
        let fake = Fake {
            views: vec![(
                "master",
                "sys",
                "databases",
                DATABASES_VIEW,
                Some("SELECT @x AS name FROM master.dbo.vauban_sys_databases"),
            )],
            tables: Fake::internal_tables(),
        };
        // The counter-proof: the scope of the statement knows `@x`, and the statement binds.
        assert!(
            bind_scoped("SELECT @x", &fake, "master", "dbo", &KnowsX).is_ok(),
            "the outer scope knows @x"
        );
        let error = bind_scoped(
            "SELECT name FROM sys.databases",
            &fake,
            "master",
            "dbo",
            &KnowsX,
        )
        .expect_err("the definition does not see @x");
        assert_eq!(error.number, 50000, "{}", error.message);
        assert!(error.message.contains("error 137"), "{}", error.message);
        // With `NoVariables` as the scope of the statement, the bug is the same one: the two
        // scopes of this test bracket what the statement declared.
        let without = err("SELECT name FROM sys.databases", &fake);
        assert_eq!(without.number, 50000, "{}", without.message);
        assert!(without.message.contains("error 137"), "{}", without.message);
    }

    /// [`source_columns`] numbers the output of the plan from 0, and the identifiers from 1.
    #[test]
    fn source_columns_are_numbered_by_position() {
        let plan = bind("SELECT * FROM sys.databases", &Fake::system_views()).expect("expands");
        let bindings = source_columns(expanded(&plan).schema());
        assert_eq!(bindings.len(), 4);
        for (position, binding) in bindings.iter().enumerate() {
            assert_eq!(binding.index, position);
            assert_eq!(
                binding.column,
                ColumnId(i32::try_from(position + 1).expect("four columns"))
            );
        }
        assert_eq!(bindings[0].name, "name");
        assert_eq!(bindings[3].name, "owner_sid");
    }
}
