//! Expansion of `*` and `t.*` into the columns of the sources in scope.
//!
//! A wildcard is not an expression: it produces **several** projected columns, and
//! `query.rs` splices them into the select list where the `*` was written. This file
//! decides three things, stated below over a table `dbo.t (a int NOT NULL, b int NULL)`
//! holding two rows:
//!
//! # What a `*` expands to, and in which order
//!
//! The columns of the source, in the order the catalogue gives them, under the names the
//! catalogue holds: `SELECT * FROM dbo.t` answers the header `a`, `b`. The expansion
//! happens **in place**, at the position the `*` was written:
//!
//! | written | header |
//! |---|---|
//! | `SELECT * FROM dbo.t` | `a`, `b` |
//! | `SELECT 1, * FROM dbo.t` | `""`, `a`, `b` — an unaliased literal has no name |
//! | `SELECT *, 1 FROM dbo.t` | `a`, `b`, `""` |
//! | `SELECT *, * FROM dbo.t` | `a`, `b`, `a`, `b` |
//! | `SELECT b, *, a FROM dbo.t` | `b`, `a`, `b`, `a` |
//!
//! # Which qualifiers a `t.*` accepts
//!
//! One part, matched against the name the source answers to — the alias when one was
//! written, the object part of the table name otherwise (`names::alias_of`). Two parts,
//! matched against the schema **the name resolved in** and that object part. Three parts,
//! matched against the **current** database as well, which is why `master.dbo.t.*` answers
//! 107 from another database and the columns from `master`. A fourth part is refused:
//!
//! | written over `FROM dbo.t` | answer |
//! |---|---|
//! | `t.*`, `T.*`, `[t].*`, `"t".*` | the columns |
//! | `dbo.t.*`, `DBO.T.*` | the columns |
//! | `dbo.t.*` over `FROM t` | the columns — the schema compared is the resolved one |
//! | `<current db>.dbo.t.*`, `<CURRENT DB>.DBO.T.*` | the columns |
//! | `dbo.u.*` over `FROM s.u` | 107 `'dbo.u'` |
//! | `nosch.t.*`, `<current db>.nosch.t.*` | 107, the prefix printed whole |
//! | `x.*` | 107 `'x'` |
//! | `master.dbo.t.*` from a database that is not `master` | 107 `'master.dbo.t'` |
//! | `srv.master.dbo.t.*` | 117, the three-prefix maximum |
//!
//! The two three-part lines are what tells "three parts name nothing" from "the database
//! part must be the current one": the same spelling answers the columns or a 107 depending
//! on the database the batch runs in.
//!
//! An alias hides the name it replaces, for a wildcard as for a column: over
//! `FROM dbo.t AS z`, `z.*` answers the columns while `t.*`, `dbo.t.*` and `dbo.z.*` each
//! answer 107 printing the prefix as written. That is the same rule `names::alias_of`
//! states for `SELECT t.id FROM dbo.t AS z`, which answers 4104.
//!
//! # What the expanded reference points at
//!
//! Each projected column is a [`BoundExprKind::ColumnRef`] holding the [`ColumnBinding`]
//! the catalogue handed the `Scan`, unchanged: its `index` is the 0-based `ordinal` of the
//! column in the row `storage` produces, which is not the position the column takes in the
//! output of the `Project` as soon as a column of the table has been dropped
//! (`the_expanded_columns_index_the_row_of_storage`, and `catalog_view.rs`,
//! `columns_are_indexed_by_ordinal_not_by_identifier`).

use vauban_errors::SqlResult;
use vauban_parser::{Ident, ObjectName};

use crate::bound::{BoundExpr, BoundExprKind, BoundProjection, ColumnBinding};
use crate::errors::line_of;
use crate::query::qualified_wildcard;

/// The one source a `FROM` of a single table puts in scope, as a wildcard reads it.
///
/// The `Scan` node carries the columns and the name the query refers to them by; the
/// database and the schema are not in the node, so `query.rs` computes them from the name as
/// written and from the context — the values the catalogue itself resolved with (`names.rs`,
/// `scan`).
#[derive(Debug, Clone)]
pub(crate) struct Source {
    /// The name a one-part qualifier is matched against: the alias, or the object part.
    alias: String,
    /// The three-part name a two- or three-part qualifier is matched against, `None` when an
    /// alias was written and hides the name of the table.
    qualified: Option<QualifiedName>,
    /// The columns of the source, in the order the catalogue gave them to the `Scan`.
    columns: Vec<ColumnBinding>,
}

/// The name of a source, completed with the database and the schema it resolved in.
#[derive(Debug, Clone)]
struct QualifiedName {
    /// The database the name resolved in: the one written, or the current one.
    database: String,
    /// The schema the name resolved in: the one written, or the default one.
    schema: String,
    /// The object part, as written.
    name: String,
}

impl Source {
    /// The source of a `FROM` whose `Scan` publishes `columns` under `alias`.
    ///
    /// `written` is the name of the table as the user spelled it, and is `None` when an
    /// alias was written: a two- or three-part qualifier then matches nothing (`dbo.t.*` and
    /// `<current db>.dbo.t.*` over `FROM dbo.t AS z` answer 107).
    /// `default_schema` and `database` fill the parts the name does not carry, as
    /// [`CatalogView::resolve_table`](crate::CatalogView::resolve_table) fills them to
    /// resolve the table.
    pub(crate) fn new(
        alias: &str,
        written: Option<&ObjectName>,
        columns: &[ColumnBinding],
        default_schema: &str,
        database: &str,
    ) -> Self {
        let part = |ident: Option<&Ident>, default: &str| {
            ident.map_or_else(|| default.to_owned(), |ident| ident.value.clone())
        };
        let qualified = written.map(|name| QualifiedName {
            database: part(name.database.as_ref(), database),
            schema: part(name.schema.as_ref(), default_schema),
            name: name.name.value.clone(),
        });
        Source {
            alias: alias.to_owned(),
            qualified,
            columns: columns.to_vec(),
        }
    }

    /// Whether `qualifier` names this source, compared with `eq_ignore_ascii_case`.
    ///
    /// The comparison is ASCII case-insensitive: an accent-insensitive and case-insensitive
    /// collation matches `[É]` with a column `é`, which this does not — a deliberate
    /// difference from SQL Server on that one pair.
    ///
    /// The same rule serves a `t.*` and a `t.a`: one part is the alias, two parts are the
    /// resolved schema and the object name, three parts add the database the source resolved
    /// in, a fourth part names nothing. The error alone differs — 107 for a wildcard, 4104
    /// for a column — and the shapes are in the module documentation and in `expr.rs`,
    /// `bind_column`.
    pub(crate) fn matches(&self, qualifier: &ObjectName) -> bool {
        if qualifier.server.is_some() {
            return false;
        }
        match (&qualifier.database, &qualifier.schema, &self.qualified) {
            (None, None, _) => self.alias.eq_ignore_ascii_case(&qualifier.name.value),
            (None, Some(schema), Some(source)) => {
                schema.value.eq_ignore_ascii_case(&source.schema)
                    && qualifier.name.value.eq_ignore_ascii_case(&source.name)
            }
            (Some(database), Some(schema), Some(source)) => {
                database.value.eq_ignore_ascii_case(&source.database)
                    && schema.value.eq_ignore_ascii_case(&source.schema)
                    && qualifier.name.value.eq_ignore_ascii_case(&source.name)
            }
            _ => false,
        }
    }
}

/// What a bare column name reaches in a source.
///
/// `Ambiguous` cannot come out of one table of the catalogue, which refuses two columns of
/// the same name at creation (2705, `ddl.rs`). It is here because 209 is raised on the
/// **scope** and not on the table, and because a join puts two sources in scope and reads
/// this same function: the unit test `two_columns_of_the_same_name_in_scope_are_209`
/// builds the shape by hand.
pub(crate) enum Lookup<'a> {
    /// Exactly one column of that name.
    One(&'a ColumnBinding),
    /// No column of that name: error 207 for a bare name.
    Absent,
    /// Several columns of that name: error 209.
    Ambiguous,
}

impl Source {
    /// The column of this source named `name`, compared ASCII case-insensitively.
    pub(crate) fn column(&self, name: &str) -> Lookup<'_> {
        let mut found = self
            .columns
            .iter()
            .filter(|column| column.name.eq_ignore_ascii_case(name));
        match (found.next(), found.next()) {
            (Some(column), None) => Lookup::One(column),
            (Some(_), Some(_)) => Lookup::Ambiguous,
            _ => Lookup::Absent,
        }
    }
}

/// Expands a `*` into one projected column per column of the source, in catalogue order.
///
/// `line` is the line of the `*` token, which each expanded [`BoundExpr`] carries: the
/// columns come from the catalogue and have no line of their own.
pub(crate) fn expand(source: &Source, line: u32) -> Vec<BoundProjection> {
    source
        .columns
        .iter()
        .map(|column| BoundProjection {
            expr: BoundExpr {
                ty: column.ty.clone(),
                kind: BoundExprKind::ColumnRef(column.clone()),
                line,
            },
            name: column.name.clone(),
        })
        .collect()
}

/// Expands a `t.*` when `qualifier` names the source, and refuses it otherwise.
///
/// # Errors
///
/// The 107 of a prefix that names no source, and the 117 of a four-part one — both built
/// by [`qualified_wildcard`], which is what a `t.*` without a `FROM` answers.
pub(crate) fn expand_qualified(
    source: &Source,
    qualifier: &ObjectName,
) -> SqlResult<Vec<BoundProjection>> {
    if source.matches(qualifier) {
        return Ok(expand(source, line_of(&qualifier.span)));
    }
    Err(qualified_wildcard(qualifier))
}

#[cfg(test)]
mod tests {
    use super::Source;
    use crate::bound::{BoundExprKind, BoundStatement, ColumnBinding, LogicalPlan};
    use crate::context::{
        BindContext, CatalogView, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
    };
    use vauban_catalog::{ColumnId, ObjectId, TableId};
    use vauban_errors::SqlError;
    use vauban_parser::{Ident, ObjectName, ParseOptions, Span, parse_batch};
    use vauban_types::{SqlType, TypeFamily, TypeInfo};

    /// A hand-built double of the catalogue that knows one table, `master.<schema>.<name>`,
    /// with the columns it was built with: the double of `names.rs`, with the schema and
    /// the name made fields so that a source outside `dbo` can be bound.
    ///
    /// The comparison is ASCII case-insensitive, as a `CI` collation is, and the parts that
    /// were not written are filled from the two arguments the method is handed — so
    /// `a_two_part_qualifier_matches_the_resolved_schema` reads the default schema of the
    /// [`BindContext`], not a constant here.
    struct OneTable {
        schema: &'static str,
        name: &'static str,
        columns: Vec<ColumnBinding>,
    }

    impl OneTable {
        /// `dbo.t (a int NOT NULL, b int NULL)`, the two columns holding ordinals 0 and 1.
        fn t() -> Self {
            OneTable {
                schema: "dbo",
                name: "t",
                columns: vec![
                    column(ColumnId(1), 0, "a", SqlType::Int, false),
                    column(ColumnId(2), 1, "b", SqlType::Int, true),
                ],
            }
        }

        /// The same two columns after a third, written between them, was dropped: the
        /// ordinals are 0 and 2 where the output positions are 0 and 1.
        fn t_with_a_dropped_column() -> Self {
            OneTable {
                schema: "dbo",
                name: "t",
                columns: vec![
                    column(ColumnId(1), 0, "a", SqlType::Int, false),
                    column(ColumnId(3), 2, "b", SqlType::Int, true),
                ],
            }
        }

        /// `s.u (c int NOT NULL)`: a source whose schema is not the default one.
        fn s_u() -> Self {
            OneTable {
                schema: "s",
                name: "u",
                columns: vec![column(ColumnId(1), 0, "c", SqlType::Int, false)],
            }
        }

        /// `dbo.t` with a third column named after a niladic function, which a delimited
        /// `[USER]` declares.
        fn t_with_a_user_column() -> Self {
            let mut table = OneTable::t();
            table
                .columns
                .push(column(ColumnId(3), 2, "USER", SqlType::Int, true));
            table
        }

        /// Two columns of the same name in one source: the catalogue cannot produce it
        /// (2705 refuses them at creation), a join can, and 209 is raised on the scope.
        /// Built by hand for `two_columns_of_the_same_name_in_scope_are_209`.
        fn t_with_two_columns_named_a() -> Self {
            OneTable {
                schema: "dbo",
                name: "t",
                columns: vec![
                    column(ColumnId(1), 0, "a", SqlType::Int, false),
                    column(ColumnId(2), 1, "a", SqlType::Int, true),
                ],
            }
        }
    }

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
                && schema.eq_ignore_ascii_case(self.schema)
                && name.name.value.eq_ignore_ascii_case(self.name))
            .then(|| ResolvedTable {
                object: ObjectId(42),
                table: Some(TableId(7)),
                columns: self.columns.clone(),
                kind: ResolvedTableKind::Table,
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
    ///
    /// The `sysfn` registry is filled first: against an empty registry `ABS(b)` answers 195
    /// and `USER` binds as a column instead of the function.
    fn bind_with(text: &str, catalog: &dyn CatalogView) -> Result<LogicalPlan, SqlError> {
        crate::call::tests::registry();
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let statement = batch.statements.first().expect("one statement");
        let ctx = BindContext {
            text,
            catalog: Some(catalog),
            database: "master",
            default_schema: "dbo",
            variables: &NoVariables,
            options: SessionOptions::default(),
        };
        match crate::bind(statement, &ctx)? {
            BoundStatement::Query(plan) => Ok(*plan),
            other => panic!("not a query: {other:?}"),
        }
    }

    /// The name, the storage index and the type of each projected column of the root
    /// `Project` of `text`.
    #[track_caller]
    fn projected(text: &str, catalog: &dyn CatalogView) -> Vec<(String, Option<usize>, TypeInfo)> {
        let plan = bind_with(text, catalog).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let LogicalPlan::Project { exprs, schema, .. } = &plan else {
            panic!("the root is not a Project: {plan:?}");
        };
        assert_eq!(schema.columns.len(), exprs.len(), "{text}");
        exprs
            .iter()
            .enumerate()
            .map(|(i, projection)| {
                assert_eq!(schema.columns[i].name, projection.name, "{text}");
                let index = match &projection.expr.kind {
                    BoundExprKind::ColumnRef(binding) => Some(binding.index),
                    _ => None,
                };
                (projection.name.clone(), index, projection.expr.ty.clone())
            })
            .collect()
    }

    /// The names of the projected columns of `text`, in order.
    #[track_caller]
    fn header(text: &str, catalog: &dyn CatalogView) -> Vec<String> {
        projected(text, catalog)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    }

    #[track_caller]
    fn err(text: &str, catalog: &dyn CatalogView) -> SqlError {
        bind_with(text, catalog).expect_err("the binding fails")
    }

    /// `SELECT * FROM dbo.t` projects the columns of the table in the order the catalogue
    /// gives them, under their catalogue names, with their types and their nullability —
    /// the header `a`, `b`. `TOP` still wraps the `Project`.
    #[test]
    fn select_star_from_t_projects_all_columns_in_ordinal_order() {
        let columns = projected("SELECT * FROM dbo.t", &OneTable::t());
        assert_eq!(
            columns,
            vec![
                ("a".to_owned(), Some(0), TypeInfo::new(SqlType::Int, false)),
                ("b".to_owned(), Some(1), TypeInfo::new(SqlType::Int, true)),
            ]
        );
        let plan = bind_with("SELECT TOP 1 * FROM dbo.t", &OneTable::t()).expect("t resolves");
        let LogicalPlan::Limit { input, .. } = &plan else {
            panic!("the root is not a Limit: {plan:?}");
        };
        assert_eq!(input.schema().columns.len(), 2);
    }

    /// A wildcard qualified by the name of the single source expands to what the bare `*`
    /// expands to: the spellings SQL Server answers the two rows to, the two-part one
    /// included.
    #[test]
    fn select_star_t_star_same_when_one_table() {
        let catalog = OneTable::t();
        let bare = projected("SELECT * FROM dbo.t", &catalog);
        for text in [
            "SELECT t.* FROM dbo.t",
            "SELECT T.* FROM dbo.t",
            "SELECT [t].* FROM dbo.t",
            "SELECT \"t\".* FROM dbo.t",
            "SELECT dbo.t.* FROM dbo.t",
            "SELECT DBO.T.* FROM dbo.t",
            "SELECT t.* FROM dbo.t WITH (NOLOCK)",
        ] {
            assert_eq!(projected(text, &catalog), bare, "{text}");
        }
    }

    /// The schema a two-part qualifier is matched against is the one the name resolved in,
    /// not the one the `FROM` spelled: `dbo.t.*` over `FROM t` answers the columns, and
    /// `dbo.u.*` over `FROM s.u` answers 107. The counter-proof of the second is `s.u.*`
    /// over the same `FROM`.
    #[test]
    fn a_two_part_qualifier_matches_the_resolved_schema() {
        let catalog = OneTable::t();
        assert_eq!(header("SELECT dbo.t.* FROM t", &catalog), ["a", "b"]);
        let outside = OneTable::s_u();
        assert_eq!(header("SELECT s.u.* FROM s.u", &outside), ["c"]);
        let error = err("SELECT dbo.u.* FROM s.u", &outside);
        assert_eq!(error.number, 107, "{}", error.message);
        assert_eq!(
            error.message,
            "The column prefix 'dbo.u' matches no table or alias of the query."
        );
    }

    /// A prefix that names no source is 107, printed as written; a fourth part is 117. The
    /// spellings are in the module documentation: a three-part prefix is refused when its
    /// database is not the current one, `master` being the context's here.
    #[test]
    fn a_qualifier_that_names_no_source_is_107() {
        let catalog = OneTable::t();
        for (text, printed) in [
            ("SELECT x.* FROM dbo.t", "x"),
            ("SELECT nosch.t.* FROM dbo.t", "nosch.t"),
            ("SELECT otherdb.dbo.t.* FROM dbo.t", "otherdb.dbo.t"),
            ("SELECT master.nosch.t.* FROM dbo.t", "master.nosch.t"),
        ] {
            let error = err(text, &catalog);
            assert_eq!(error.number, 107, "{text}: {}", error.message);
            assert_eq!(error.severity, 15, "{text}");
            assert_eq!(error.state, 1, "{text}");
            assert_eq!(
                error.message,
                format!("The column prefix '{printed}' matches no table or alias of the query."),
                "{text}"
            );
        }
        let four = err("SELECT srv.master.dbo.t.* FROM dbo.t", &catalog);
        assert_eq!(four.number, 117, "{}", four.message);
        assert_eq!(
            four.message,
            "The column name 'srv.master.dbo.t' has too many prefixes; at most 3 are allowed."
        );
    }

    /// An alias hides the name of the table for a wildcard, as it does for a column: `z.*`
    /// answers the columns and the three spellings of the hidden name answer 107.
    #[test]
    fn an_alias_hides_the_name_of_the_source() {
        let catalog = OneTable::t();
        for text in ["SELECT z.* FROM dbo.t AS z", "SELECT z.* FROM dbo.t z"] {
            assert_eq!(header(text, &catalog), ["a", "b"], "{text}");
        }
        for (text, printed) in [
            ("SELECT t.* FROM dbo.t AS z", "t"),
            ("SELECT dbo.t.* FROM dbo.t AS z", "dbo.t"),
            ("SELECT dbo.z.* FROM dbo.t AS z", "dbo.z"),
        ] {
            let error = err(text, &catalog);
            assert_eq!(error.number, 107, "{text}: {}", error.message);
            assert!(
                error.message.contains(&format!("'{printed}'")),
                "{text}: {}",
                error.message
            );
        }
    }

    /// The expansion happens at the position the `*` was written, and an unaliased literal
    /// keeps its empty name. The five shapes of the module documentation, the mixed
    /// `SELECT b, *, a` included.
    #[test]
    fn a_wildcard_is_expanded_where_it_was_written() {
        let catalog = OneTable::t();
        assert_eq!(header("SELECT 1, * FROM dbo.t", &catalog), ["", "a", "b"]);
        assert_eq!(header("SELECT *, 1 FROM dbo.t", &catalog), ["a", "b", ""]);
        assert_eq!(
            header("SELECT *, * FROM dbo.t", &catalog),
            ["a", "b", "a", "b"]
        );
        assert_eq!(
            header("SELECT 1 AS n, t.* FROM dbo.t", &catalog),
            ["n", "a", "b"]
        );
        // The fifth: a named column on either side of the star (`b`, `a`, `b` and
        // `b`, `a`, `b`, `a`).
        assert_eq!(header("SELECT b, * FROM dbo.t", &catalog), ["b", "a", "b"]);
        assert_eq!(
            header("SELECT b, *, a FROM dbo.t", &catalog),
            ["b", "a", "b", "a"]
        );
    }

    /// The `index` of an expanded column is the ordinal of the column in the row `storage`
    /// produces, not the position the column takes in the output of the `Project`. What
    /// separates the two: a table whose middle column was dropped publishes the ordinals 0
    /// and 2 for the output positions 0 and 1. With `OneTable::t`, whose ordinals are 0 and
    /// 1, the two readings agree and prove nothing.
    #[test]
    fn the_expanded_columns_index_the_row_of_storage() {
        let dropped = projected("SELECT * FROM dbo.t", &OneTable::t_with_a_dropped_column());
        assert_eq!(
            dropped.iter().map(|c| c.1).collect::<Vec<_>>(),
            vec![Some(0), Some(2)]
        );
        assert_eq!(
            dropped.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let intact = projected("SELECT * FROM dbo.t", &OneTable::t());
        assert_eq!(
            intact.iter().map(|c| c.1).collect::<Vec<_>>(),
            vec![Some(0), Some(1)]
        );
    }

    /// Without a source in scope a `*` is still 263 and a `t.*` still 107: the expansion
    /// is reached through the `FROM`, and the counter-proof is the same two texts with
    /// `FROM dbo.t`, which bind.
    #[test]
    fn a_wildcard_without_a_from_keeps_263_and_107() {
        for (text, number) in [("SELECT *", 263), ("SELECT t.*", 107)] {
            let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
            let statement = batch.statements.first().expect("one statement");
            let ctx = BindContext::scalar(text, SessionOptions::default());
            let error = crate::bind(statement, &ctx).expect_err("no FROM, no expansion");
            assert_eq!(error.number, number, "{text}: {}", error.message);
        }
        let catalog = OneTable::t();
        assert_eq!(header("SELECT * FROM dbo.t", &catalog), ["a", "b"]);
        assert_eq!(header("SELECT t.* FROM dbo.t", &catalog), ["a", "b"]);
    }

    /// `SELECT a FROM dbo.t` is a `ColumnRef` over the binding the catalogue gave the
    /// `Scan`: index 0, `int` not null, and `b` is index 1, `int` null. The output name is
    /// the one the user wrote.
    #[test]
    fn select_a_from_t_is_column_ref() {
        let catalog = OneTable::t();
        assert_eq!(
            projected("SELECT a FROM dbo.t", &catalog),
            vec![("a".to_owned(), Some(0), TypeInfo::new(SqlType::Int, false))]
        );
        assert_eq!(
            projected("SELECT b FROM dbo.t", &catalog),
            vec![("b".to_owned(), Some(1), TypeInfo::new(SqlType::Int, true))]
        );
        // The qualified spellings SQL Server answers the rows to, and the alias.
        for text in [
            "SELECT t.a FROM dbo.t",
            "SELECT dbo.t.a FROM dbo.t",
            "SELECT dbo.t.a FROM t",
            "SELECT [a] FROM dbo.t",
            "SELECT (a) FROM dbo.t",
            "SELECT A FROM dbo.t",
        ] {
            let columns = projected(text, &catalog);
            assert_eq!(columns.len(), 1, "{text}");
            assert_eq!(columns[0].1, Some(0), "{text}");
            assert_eq!(columns[0].2, TypeInfo::new(SqlType::Int, false), "{text}");
        }
        assert_eq!(header("SELECT z.a FROM dbo.t AS z", &catalog), ["a"]);
        assert_eq!(header("SELECT c FROM s.u", &OneTable::s_u()), ["c"]);
    }

    /// A name no column of the source answers to is 207, on the line of the reference, and
    /// the counter-proof is the same text with a name the source does answer to.
    #[test]
    fn unknown_column_is_207() {
        let catalog = OneTable::t();
        for text in [
            "SELECT nosuch FROM dbo.t",
            "SELECT a FROM dbo.t WHERE nosuch = 1",
            "SELECT nosuch + 1 FROM dbo.t",
            "SELECT ABS(nosuch) FROM dbo.t",
        ] {
            let error = err(text, &catalog);
            assert_eq!(error.number, 207, "{text}: {}", error.message);
            assert_eq!(error.severity, 16, "{text}");
            assert_eq!(error.state, 1, "{text}");
            assert_eq!(error.message, "Unknown column name 'nosuch'.", "{text}");
            assert_eq!(error.line, 1, "{text}");
        }
        // The `WHERE` is bound before the select list, so its column is the one reported.
        let error = err("SELECT nosuch FROM dbo.t WHERE alsonosuch = 1", &catalog);
        assert_eq!(error.message, "Unknown column name 'alsonosuch'.");
        // The line is the reference's, not the statement's.
        let error = err("SELECT\n  1,\n  nosuch\nFROM dbo.t", &catalog);
        assert_eq!(error.number, 207);
        assert_eq!(error.line, 3);
        assert!(bind_with("SELECT a FROM dbo.t", &catalog).is_ok());
    }

    /// A qualifier that names no source is 4104 on the whole dotted name, and a qualifier
    /// that names the source is not.
    ///
    /// The three-part prefix that names the **current** database binds instead, which is
    /// what `a_three_part_qualifier_matches_the_current_database` asserts: `master` is the
    /// database of the context here, so `otherdb.dbo.t.a` is the 4104 and `master.dbo.t.a`
    /// is the column.
    ///
    /// The fifth part of `SELECT srv.master.dbo.t.a`, which SQL Server answers 4104 to, is
    /// a syntax error 102 out of the **parser** of VaubanDB, which reads four parts at most:
    /// the difference is the parser's, not a line here.
    #[test]
    fn unknown_prefix_is_4104() {
        let catalog = OneTable::t();
        for (text, printed) in [
            ("SELECT nosuch.a FROM dbo.t", "nosuch.a"),
            ("SELECT otherdb.dbo.t.a FROM dbo.t", "otherdb.dbo.t.a"),
            ("SELECT master.nosch.t.a FROM dbo.t", "master.nosch.t.a"),
            ("SELECT t.a FROM dbo.t AS z", "t.a"),
            ("SELECT dbo.t.a FROM dbo.t AS z", "dbo.t.a"),
            ("SELECT master.dbo.t.a FROM dbo.t AS z", "master.dbo.t.a"),
            ("SELECT a FROM dbo.t WHERE nosuch.b = 1", "nosuch.b"),
        ] {
            let error = err(text, &catalog);
            assert_eq!(error.number, 4104, "{text}: {}", error.message);
            assert_eq!(error.severity, 16, "{text}");
            assert_eq!(error.state, 1, "{text}");
            assert_eq!(
                error.message,
                format!("The qualified name \"{printed}\" matches nothing in scope."),
                "{text}"
            );
            assert_eq!(error.line, 1, "{text}");
        }
        // The line is the reference's.
        let error = err("SELECT\n  1,\n  nopre.a\nFROM dbo.t", &catalog);
        assert_eq!(error.number, 4104);
        assert_eq!(error.line, 3);
        assert!(bind_with("SELECT t.a FROM dbo.t", &catalog).is_ok());
    }

    /// A prefix that names the source and a column name nothing answers to is **207**, not
    /// 4104: the prefix bound, the name did not. The counter-proof is the line above it in
    /// `unknown_prefix_is_4104`, where the prefix is what fails.
    #[test]
    fn a_qualified_unknown_column_is_207() {
        let error = err("SELECT t.nosuch FROM dbo.t", &OneTable::t());
        assert_eq!(error.number, 207, "{}", error.message);
        assert_eq!(error.message, "Unknown column name 'nosuch'.");
    }

    /// Two columns of one name in scope answer 209. No single-source text reaches it — one
    /// table of the catalogue cannot hold them, `CREATE TABLE` refusing the second by 2705
    /// — so the scope is built by hand. SQL Server answers that same 209 over two sources:
    /// `SELECT a FROM dbo.t AS t1, dbo.t AS t2`. The counter-proof is the same text against
    /// a source with one `a`, which binds.
    #[test]
    fn two_columns_of_the_same_name_in_scope_are_209() {
        let error = err(
            "SELECT a FROM dbo.t",
            &OneTable::t_with_two_columns_named_a(),
        );
        assert_eq!(error.number, 209, "{}", error.message);
        assert_eq!(error.severity, 16);
        assert_eq!(error.state, 1);
        assert_eq!(error.message, "Column name 'a' is ambiguous.");
        assert_eq!(error.line, 1);
        // A qualifier does not disambiguate two columns of the same source.
        assert_eq!(
            err(
                "SELECT t.a FROM dbo.t",
                &OneTable::t_with_two_columns_named_a()
            )
            .number,
            209
        );
        assert!(bind_with("SELECT a FROM dbo.t", &OneTable::t()).is_ok());
    }

    /// The scope reaches the operands of an operator, the arguments of a call and the source
    /// of a `CAST`: `call.rs` threads it through `bind_operand`.
    #[test]
    fn a_column_is_in_scope_in_an_operand_a_call_and_a_cast() {
        let catalog = OneTable::t();
        // The types SQL Server reports: `int`, `int`, `bigint`. Nullability is not
        // asserted here; the rule is `expr.rs`'s.
        for (text, ty) in [
            ("SELECT a + 0 FROM dbo.t", SqlType::Int),
            ("SELECT ABS(b) FROM dbo.t", SqlType::Int),
            ("SELECT CAST(a AS bigint) FROM dbo.t", SqlType::BigInt),
        ] {
            let columns = projected(text, &catalog);
            assert_eq!(columns.len(), 1, "{text}");
            // An expression is nameless, whichever column it is built on.
            assert_eq!(columns[0].0, "", "{text}");
            assert_eq!(columns[0].2.ty, ty, "{text}");
            assert_eq!(columns[0].1, None, "{text}: not a bare ColumnRef");
        }
    }

    /// The `WHERE` is bound against the source, below the `Project`, and may name a column
    /// the projection does not return. The `ColumnRef` of the `Filter` indexes
    /// the row of the `Scan`: `b` is index 1 there, where it is not projected at all.
    #[test]
    fn the_where_is_bound_against_the_source() {
        let plan = bind_with("SELECT a FROM dbo.t WHERE b = 20", &OneTable::t()).expect("binds");
        let LogicalPlan::Project { input, .. } = &plan else {
            panic!("the root is not a Project: {plan:?}");
        };
        let LogicalPlan::Filter { input, predicate } = input.as_ref() else {
            panic!("the input of the Project is not a Filter: {input:?}");
        };
        assert!(matches!(input.as_ref(), LogicalPlan::Scan { .. }));
        let BoundExprKind::Compare { left, .. } = &predicate.kind else {
            panic!("the predicate is not a comparison: {predicate:?}");
        };
        let BoundExprKind::ColumnRef(binding) = &left.kind else {
            panic!("the left operand is not a column: {left:?}");
        };
        assert_eq!(binding.name, "b");
        assert_eq!(binding.index, 1);
    }

    /// Two places under a `FROM` bind against the **empty** scope: the row count of a
    /// `TOP`, where SQL Server answers 4115, and the arguments glued to the name of the
    /// `FROM`, where it answers 207 on a column of the table itself. VaubanDB answers 207
    /// to both, a deliberate difference on the first; `query.rs` documents why.
    #[test]
    fn a_column_is_out_of_scope_in_a_top_and_in_a_from_argument() {
        let catalog = OneTable::t();
        for text in ["SELECT TOP (a) b FROM dbo.t", "SELECT 1 FROM dbo.t (a)"] {
            let error = err(text, &catalog);
            assert_eq!(error.number, 207, "{text}: {}", error.message);
            assert_eq!(error.message, "Unknown column name 'a'.", "{text}");
        }
        // The counter-proof: the same column in the select list binds.
        assert!(bind_with("SELECT a FROM dbo.t", &catalog).is_ok());
    }

    /// A niladic function is read before the scope, and a column of that very name does not
    /// hide it: over a table holding a column `[USER]`, `SELECT USER FROM dbo.t` answers
    /// `dbo` typed `nvarchar` and `SELECT [USER] FROM dbo.t` answers the column. The two
    /// texts differ by the delimiters alone.
    #[test]
    fn a_niladic_function_wins_over_a_column_of_the_same_name() {
        let catalog = OneTable::t_with_a_user_column();
        let function = projected("SELECT USER FROM dbo.t", &catalog);
        assert_eq!(function.len(), 1);
        assert_eq!(function[0].0, "");
        assert_eq!(function[0].1, None, "a function is not a ColumnRef");
        assert_eq!(function[0].2.ty.family(), TypeFamily::Character);
        let delimited = projected("SELECT [USER] FROM dbo.t", &catalog);
        assert_eq!(
            delimited,
            vec![(
                "USER".to_owned(),
                Some(2),
                TypeInfo::new(SqlType::Int, true)
            )]
        );
    }

    /// A column reference is returned under the name it was **written** with, delimiters,
    /// parentheses and a unary `+` dropped and the case kept; an expression has no name. The
    /// spellings are listed in `query.rs`, `column_name`.
    #[test]
    fn a_column_is_returned_under_the_name_it_was_written_with() {
        let catalog = OneTable::t();
        for (text, expected) in [
            ("SELECT a FROM dbo.t", "a"),
            ("SELECT A FROM dbo.t", "A"),
            ("SELECT [a] FROM dbo.t", "a"),
            ("SELECT (a) FROM dbo.t", "a"),
            ("SELECT t.a FROM dbo.t", "a"),
            ("SELECT a AS x FROM dbo.t", "x"),
            ("SELECT a + 0 FROM dbo.t", ""),
            ("SELECT ABS(b) FROM dbo.t", ""),
            // The unary plus keeps the name, the unary minus and `~` lose it: the pair is
            // what `query::written_column` separates.
            ("SELECT +a FROM dbo.t", "a"),
            ("SELECT + +a FROM dbo.t", "a"),
            ("SELECT +(a) FROM dbo.t", "a"),
            ("SELECT -a FROM dbo.t", ""),
            ("SELECT ~a FROM dbo.t", ""),
        ] {
            assert_eq!(header(text, &catalog), [expected], "{text}");
        }
        assert_eq!(header("SELECT a, a FROM dbo.t", &catalog), ["a", "a"]);
        assert_eq!(
            header("SELECT b, * FROM dbo.t", &catalog),
            ["b", "a", "b"],
            "a column and a star in one select list"
        );
    }

    /// A three-part qualifier names the source when its database part is the **current**
    /// database, the schema and the object part matching as in the two-part form. What
    /// tells this from "three parts name nothing": the same spelling binds from the
    /// database it names and answers 4104 or 107 from another. The context here runs in
    /// `master`.
    #[test]
    fn a_three_part_qualifier_matches_the_current_database() {
        let catalog = OneTable::t();
        for text in [
            "SELECT master.dbo.t.a FROM dbo.t",
            "SELECT MASTER.DBO.T.a FROM dbo.t",
            "SELECT master.dbo.t.a FROM master.dbo.t",
            "SELECT [master].[dbo].[t].a FROM dbo.t",
        ] {
            assert_eq!(header(text, &catalog), ["a"], "{text}");
        }
        assert_eq!(
            header("SELECT master.dbo.t.* FROM dbo.t", &catalog),
            ["a", "b"]
        );
        // Another database, the wrong schema, and an alias that hides the name: the three
        // refusals SQL Server answers to the same shapes.
        for (text, number) in [
            ("SELECT otherdb.dbo.t.a FROM dbo.t", 4104),
            ("SELECT master.nosch.t.a FROM dbo.t", 4104),
            ("SELECT master.dbo.t.a FROM dbo.t AS z", 4104),
            ("SELECT otherdb.dbo.t.* FROM dbo.t", 107),
            ("SELECT master.dbo.t.* FROM dbo.t AS z", 107),
        ] {
            assert_eq!(err(text, &catalog).number, number, "{text}");
        }
    }

    /// [`Source::new`] takes the database and the schema of the context when the name was
    /// written without them, and drops the qualified form when an alias was written.
    #[test]
    fn a_source_keeps_the_schema_it_resolved_in() {
        let ident = |value: &str| Ident {
            value: value.to_owned(),
            quoted: false,
        };
        let name = ObjectName {
            server: None,
            database: None,
            schema: None,
            name: ident("t"),
            span: Span::EMPTY,
        };
        let columns = [column(ColumnId(1), 0, "a", SqlType::Int, false)];
        let unqualified = Source::new("t", Some(&name), &columns, "dbo", "master");
        let qualified = unqualified.qualified.expect("a name without an alias");
        assert_eq!(qualified.database, "master");
        assert_eq!(qualified.schema, "dbo");
        assert_eq!(qualified.name, "t");
        let aliased = Source::new("z", None, &columns, "dbo", "master");
        assert!(aliased.qualified.is_none());
        assert_eq!(aliased.alias, "z");
    }
}
