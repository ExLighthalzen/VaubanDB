//! Resolution of a one- to four-part table name against the catalogue, and error 208 when
//! it reaches nothing.
//!
//! [`bind_from`] turns the single table reference of a `FROM` into a
//! [`LogicalPlan::Scan`]. It decides three things: which parts of the name fill in the ones
//! that were not written, what the 208 message prints, and which line it carries.
//!
//! # Filling in the parts that were not written
//!
//! A written name has one to four parts. The object part is there in each of them; the
//! schema defaults to [`BindContext::default_schema`] (`dbo`) and the database to
//! [`BindContext::database`]. The catalogue does the filling in and the case-insensitive
//! comparison ([`CatalogView::resolve_table`](crate::CatalogView::resolve_table) takes
//! `database` and `default_schema` as arguments); this module hands it the two values and
//! reads its answer.
//!
//! | written | what SQL Server answers |
//! |---|---|
//! | `FROM t`, `FROM dbo.t`, `FROM DBO.T` over a table `dbo.t` | the rows |
//! | `FROM nosuch` | 208 naming `'nosuch'` |
//! | `FROM dbo.nosuch`, `FROM [dbo].[nosuch]` | 208 `'dbo.nosuch'` |
//! | `FROM nosch.nosuch` | 208 `'nosch.nosuch'` |
//! | `FROM master.dbo.nosuch` | 208 `'master.dbo.nosuch'` |
//! | `FROM nodb.dbo.t`, `nodb` being no database | 208 `'nodb.dbo.t'`, **not 911** |
//! | `FROM nodb..t` | 208 `'nodb..t'`, the empty part printed |
//! | `FROM nosuch AS z`, `FROM nosuch z`, `FROM nosuch WITH (NOLOCK)` | 208 `'nosuch'` |
//! | `SELECT * FROM nosuch` | 208, and not the 263 of a `*` without `FROM` |
//! | `FROM dbo.t, nosuch` and `FROM nosuch, dbo.t` | 208 `'nosuch'` |
//!
//! A name glued to arguments is resolved here too, and before those arguments are re-read:
//! `FROM nosuch (1)`, `FROM nosuch (x)` and `FROM nosuch ()` answer 208 where the
//! re-reading of the arguments alone would answer 215 and 207. That is
//! [`check_object_exists`], which `query::check_table_arguments` calls.
//!
//! A database that does not exist therefore answers 208 like any other unresolved name:
//! 911 (the database does not exist) is `USE`'s, not a `FROM`'s. Nothing here builds a 911.
//!
//! # The name in the message is the one that was written
//!
//! 208 prints the parts that were written, joined by dots, delimiters removed — the rule
//! `dotted` already applies to 215, 107 and 117. `dbo.nosuch` stays `dbo.nosuch` and does
//! not lose its schema; `nosuch` stays `nosuch` and is not completed into `dbo.nosuch`; the
//! case is kept. The lines of the table above spell one missing object three ways.
//!
//! # The four-part name, and what VaubanDB answers instead
//!
//! SQL Server reads a four-part name against `sys.servers`. With the server part naming the
//! local instance, the name resolves as its last three parts do:
//! `<@@SERVERNAME>.master.sys.objects` answers the rows, and
//! `<@@SERVERNAME>.master.dbo.nosuch` answers 208 `'master.dbo.nosuch'` — the server part
//! dropped from the message. With another name, the answer is **7202**, state 2 (the server
//! is not in `sys.servers`), before the object is looked at: `nosrv.master.dbo.t` and
//! `nosrv.master.dbo.nosuch` answer that same error.
//!
//! VaubanDB reproduces neither: it has no `sys.servers`, no linked server, and
//! [`BindContext`] carries no server name to compare against. A four-part name is refused
//! here, before the catalogue is asked, as a 208 over the name as written, server part
//! included (`four_part_wrong_server_is_208`, `four_part_local_server_is_208_too`). Two
//! deliberate differences follow, until the binder is given a server name: 7202 is not
//! raised, and the local-server spelling of a table that exists is refused instead of
//! resolved.
//!
//! # The line of a 208
//!
//! The first line of the **statement**, not of the name. Two batches separate the three
//! candidates:
//!
//! | batch | answer |
//! |---|:-:|
//! | `SELECT 1 AS n` (1) / `FROM` (2) / `nosuch;` (3) | **1** — the `SELECT`, not the name |
//! | `SELECT 1 AS n;` (1) / `SELECT 2 AS n` (2) / `FROM` (3) / `nosuch;` (4) | **2** — that statement, not the batch |
//!
//! 208 belongs with 206, 263 and the rest of `errors::NAMES_THE_STATEMENT`. The line is
//! set at the raising site as well, which gives the client the same message:
//! `errors::on_the_statement` overrides the line of the numbers it lists and leaves the
//! others as their site set them.

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Ident, ObjectName, TableRef};

use crate::bound::{LockHints, LogicalPlan, OutputColumn, OutputSchema};
use crate::context::{BindContext, ResolvedTable, ResolvedTableKind};
use crate::query::{bug, dotted, not_yet};
use crate::view;

/// Binds the single table reference of a `FROM` into the plan that reads it.
///
/// `statement_line` is the line the statement starts on, which is the one a 208 carries
/// (module documentation). The reference has already been walked by
/// `query::check_table_arguments`, so a `nom(…)` that reaches this function carries a lone
/// hint word: its arguments were re-read as a hint and are dropped here, as a
/// `WITH (NOLOCK)` written in full is (`hints.rs` gives hints their meaning).
///
/// # Errors
///
/// - 208 when the name resolves to nothing, or when it has four parts;
/// - the internal error 50000 for a reference the binder does not bind yet: a join, an
///   `APPLY`, a derived table, a table variable, `PIVOT`/`UNPIVOT`;
/// - for a view, the errors of [`view::expand`], which are the internal error 50000 as well:
///   the definition is the catalogue's text and nothing that goes wrong in it is a message
///   for the client (`view.rs`).
pub(crate) fn bind_from(
    table_ref: &TableRef,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    match table_ref {
        TableRef::Table { name, alias, .. } => scan(name, alias.as_ref(), statement_line, ctx),
        // The arguments are the hint `check_table_arguments` accepted; anything else has
        // already answered 215, or the error one of them raised.
        TableRef::Function { name, alias, .. } => scan(name, alias.as_ref(), statement_line, ctx),
        // `query.rs` routes a join to `join.rs` before reaching this function; the arm is
        // kept so that the `match` on `TableRef` stays exhaustive.
        TableRef::Join { .. } => Err(not_yet("bind_from: a join in FROM is not implemented yet")),
        TableRef::Apply { .. } => Err(not_yet(
            "bind_select: CROSS APPLY and OUTER APPLY in FROM are not implemented yet",
        )),
        // Routed to `subquery.rs` by `query.rs`, like the join above.
        TableRef::Derived { .. } => Err(not_yet(
            "bind_from: a derived table in FROM is not implemented yet",
        )),
        TableRef::Variable { .. } => Err(not_yet(
            "bind_select: FROM @t reads a table variable, which is not implemented yet",
        )),
        TableRef::Pivot(_) => Err(not_yet("bind_select: PIVOT in FROM is not implemented yet")),
        TableRef::Unpivot(_) => Err(not_yet(
            "bind_select: UNPIVOT in FROM is not implemented yet",
        )),
    }
}

/// Resolves `name` and builds the [`LogicalPlan::Scan`] that reads it, or the plan of the
/// definition when the name reaches a view.
///
/// A view stores no row, so there is no `Scan` to build for it: [`view::expand`] parses its
/// definition and binds it, and the plan it answers takes the place of the node this function
/// would have built. The leaf of that plan is the `Scan` of the internal table the
/// definition reads (`view.rs`, `from_sys_databases_expands_to_scan_of_internal_table`).
///
/// The node carries the columns of the table in the order the catalogue gives them, and a
/// schema built from that same list: a `Scan` publishes its names and types from the node
/// alone (`scan_schema_is_the_declared_schema`). Reading fewer columns than the
/// table has is the `planner`'s decision, not this one's.
///
/// The bindings are the catalogue's, unchanged: `ColumnBinding.index` stays the 0-based
/// `ordinal` of the column in the row `storage` hands out (`catalog_view.rs`,
/// `columns_are_indexed_by_ordinal_not_by_identifier`), which is what the executor reads to
/// project and reorder a row — not the position of the column in the output of the node.
fn scan(
    name: &ObjectName,
    alias: Option<&Ident>,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let resolved = resolve(name, ctx)?.ok_or_else(|| invalid_object_name(name, statement_line))?;
    match resolved.kind {
        // A view holds no row of its own: the plan of its definition takes the place of the
        // `Scan` this function would have built (`view.rs`).
        ResolvedTableKind::View => view::expand(&resolved, name, ctx),
        ResolvedTableKind::Table => {
            let table = resolved.table.ok_or_else(|| {
                bug("bind_from: the catalogue resolved a table with no rows in storage")
            })?;
            let schema = OutputSchema {
                columns: resolved
                    .columns
                    .iter()
                    .map(|column| OutputColumn {
                        name: column.name.clone(),
                        ty: column.ty.clone(),
                    })
                    .collect(),
            };
            Ok(LogicalPlan::Scan {
                table,
                columns: resolved.columns,
                alias: alias_of(name, alias),
                schema,
                // The words of a hint list are accepted and dropped until `hints.rs` reads
                // them (`bound/mod.rs`, `LockHints`).
                hints: LockHints::default(),
            })
        }
    }
}

/// The refusal of a `FROM` when the context carries no catalogue: the internal error
/// 50000 for the whole clause.
///
/// A name that cannot be looked up is not a name that is absent: answering 208 would tell
/// a client its table does not exist when nothing was consulted. `BindContext::scalar` is
/// in that case, so the tests that bind without a catalogue read the clause refusal
/// (`without_a_catalogue_from_is_internal`, and the `FROM` tests of
/// `tests/bind_select.rs`). The one that supplies a catalogue,
/// `classified_table_functions_do_not_reread_hints`, reads the 208 of a name that resolves
/// to nothing instead.
pub(crate) fn from_needs_the_catalogue() -> SqlError {
    not_yet("bind_select: FROM requires the catalog, which the session hands to the binder")
}

/// Refuses `name` by a 208 when the catalogue reaches nothing, before its arguments are
/// re-read (module documentation, `an_unknown_object_answers_208_before_its_arguments`).
///
/// `query::check_table_arguments` calls this on a `nom(…)` whose name it did not classify
/// as a table-valued function, so that `FROM nosuch (1)` answers what SQL Server answers,
/// 208, and not the 215 of the re-reading. With no catalogue the answer is `Ok`: nothing
/// was consulted, and the re-reading keeps its behaviour
/// (`without_a_catalogue_arguments_are_still_re_read`).
///
/// # Errors
///
/// 208 when the name resolves to nothing, four-part names included.
pub(crate) fn check_object_exists(
    name: &ObjectName,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<()> {
    if ctx.catalog.is_none() {
        return Ok(());
    }
    match resolve(name, ctx)? {
        Some(_) => Ok(()),
        None => Err(invalid_object_name(name, statement_line)),
    }
}

/// Asks the catalogue what `name` reaches, `None` when it reaches nothing.
///
/// A four-part name is refused before the catalogue is asked (module documentation,
/// `four_part_wrong_server_is_208`).
///
/// # Errors
///
/// [`from_needs_the_catalogue`] when the context carries no catalogue.
fn resolve(name: &ObjectName, ctx: &BindContext<'_>) -> SqlResult<Option<ResolvedTable>> {
    if name.server.is_some() {
        return Ok(None);
    }
    let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
    Ok(catalog.resolve_table(name, ctx.database, ctx.default_schema))
}

/// The 208 of a name that reached nothing: the name as written, on the line the statement
/// starts on (module documentation).
fn invalid_object_name(name: &ObjectName, statement_line: u32) -> SqlError {
    SqlError::invalid_object_name(&dotted(name)).with_line(statement_line)
}

/// The name the rest of the query refers to the source by: the alias when one was written,
/// the object part of the name otherwise.
///
/// `pub(crate)` for `query.rs`, which applies the same rule to an **expanded view**: its plan
/// is a `Project` and carries no `alias` field for the scope to read.
///
/// An alias hides the name it replaces — `SELECT t.id FROM dbo.t AS z` answers 4104 (the
/// multi-part identifier cannot be bound) while `SELECT z.id FROM dbo.t z` answers the row.
/// Without an alias, the qualifier may be written with more parts than this string keeps:
/// `SELECT t.id FROM dbo.t` and `SELECT dbo.t.id FROM dbo.t` both answer the row. Matching
/// a qualifier against a source is `star.rs`'s, which reads this field.
pub(crate) fn alias_of(name: &ObjectName, alias: Option<&Ident>) -> String {
    alias.map_or_else(|| name.name.value.clone(), |ident| ident.value.clone())
}

#[cfg(test)]
mod tests {
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
    }

    fn column(
        id: ColumnId,
        index: usize,
        name: &str,
        ty: SqlType,
        nullable: bool,
    ) -> ColumnBinding {
        ColumnBinding {
            column: id,
            index,
            name: name.to_owned(),
            ty: TypeInfo::new(ty, nullable),
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
        let other_schema = bind_in("SELECT 1 FROM t", &OneTable, "master", "guest", 0)
            .expect_err("t is not in guest");
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
    /// `OneView` answers a view and **no** definition — the default of the trait — so what
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
        for (text, expected) in [
            ("SELECT 1 FROM (SELECT 1 AS n) AS d", "derived table"),
            ("SELECT 1 FROM dbo.t AS a CROSS APPLY dbo.t AS b", "APPLY"),
        ] {
            let error = err(text, &OneTable);
            assert_eq!(error.number, 50000, "{text}: {}", error.message);
            assert!(
                error.message.contains(expected),
                "{text}: {}",
                error.message
            );
        }
    }
}
