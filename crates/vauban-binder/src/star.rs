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
//! | `sys.objects.*` over `FROM master.sys.objects`, from a database that is not `master` | 107 `'sys.objects'` — two parts are completed with the current database |
//! | `master.sys.objects.*` over that same `FROM` | the columns |
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
    /// Whether `database` is the current one, which a two-part qualifier is completed with
    /// before it is compared (`a_two_part_qualifier_is_completed_with_the_current_database`).
    in_current_database: bool,
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
            in_current_database: name
                .database
                .as_ref()
                .is_none_or(|written| written.value.eq_ignore_ascii_case(database)),
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
    /// resolved schema and the object name **in the current database**, three parts add the
    /// database the source resolved in, a fourth part names nothing. The error alone differs
    /// — 107 for a wildcard, 4104 for a column — and the shapes are in the module
    /// documentation and in `expr.rs`, `bind_column`.
    ///
    /// A two-part qualifier is completed with the current database before it is compared,
    /// so it does not reach a source of another database: from a database that is not
    /// `master`, `sys.objects.name` over `FROM master.sys.objects` answers 4104 and
    /// `sys.objects.*` answers 107, where `master.sys.objects.name` and `objects.name` bind
    /// (`a_two_part_qualifier_is_completed_with_the_current_database`).
    pub(crate) fn matches(&self, qualifier: &ObjectName) -> bool {
        if qualifier.server.is_some() {
            return false;
        }
        match (&qualifier.database, &qualifier.schema, &self.qualified) {
            (None, None, _) => self.alias.eq_ignore_ascii_case(&qualifier.name.value),
            (None, Some(schema), Some(source)) => {
                source.in_current_database
                    && schema.value.eq_ignore_ascii_case(&source.schema)
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

/// Expands a `t.*` over the sources in scope: the columns of the one `qualifier` names.
///
/// # Errors
///
/// The 107 of a prefix that names no source, and the 117 of a four-part one — both built
/// by [`qualified_wildcard`], which is what a `t.*` without a `FROM` answers, `sources`
/// being empty then.
pub(crate) fn expand_qualified(
    sources: &[Source],
    qualifier: &ObjectName,
) -> SqlResult<Vec<BoundProjection>> {
    match sources.iter().find(|source| source.matches(qualifier)) {
        Some(source) => Ok(expand(source, line_of(&qualifier.span))),
        None => Err(qualified_wildcard(qualifier)),
    }
}

// ---------------------------------------------------------------------------------------
// A scope of several sources: the `FROM` of a join
// ---------------------------------------------------------------------------------------
//
// Over `dbo.a (k int NOT NULL, c int NULL)` and `dbo.b (k int NOT NULL, c int NULL)`:
//
// | written | answer |
// |---|---|
// | `SELECT * FROM dbo.a JOIN dbo.b ON 1 = 1` | `k`, `c`, `k`, `c` — the sources in written order |
// | `SELECT b.* FROM dbo.a JOIN dbo.b ON a.k = b.k` | `k`, `c` — the columns of `b` alone |
// | `SELECT dbo.b.* FROM …` | the same two columns |
// | `SELECT x.* FROM dbo.a JOIN dbo.b ON …` | 107 `'x'` |
// | `SELECT c FROM dbo.a JOIN dbo.b ON …` | 209 `'c'` |
// | `SELECT k FROM dbo.a JOIN dbo.b ON k = 1` | 209 `'k'`, raised in the `ON` |
// | `SELECT a.c, b.c FROM dbo.a JOIN dbo.b ON …` | the two columns, the qualifier deciding |
// | `SELECT x.c FROM dbo.a JOIN dbo.b ON …` | 4104 `"x.c"` |
// | `SELECT e, c FROM dbo.a JOIN dbo.d ON …`, `d (k, e)` | the two columns: `c` is in one source |
//
// `tests/bind_join.rs` binds each of these shapes.

impl Source {
    /// The name a one-part qualifier is matched against: the alias, or the object part.
    /// A join compares it across its sources to raise 1011 to 1013 (`join.rs`).
    pub(crate) fn exposed_name(&self) -> &str {
        &self.alias
    }

    /// The same source, its columns indexing the row of a `Join` whose left input is
    /// `offset` columns wide: the index of each column moved by `offset`
    /// (`bound/mod.rs`, [`LogicalPlan::Join`](crate::bound::LogicalPlan::Join)).
    pub(crate) fn shifted_by(mut self, offset: usize) -> Self {
        for column in &mut self.columns {
            column.index = column.index.saturating_add(offset);
        }
        self
    }

    /// The same source on the side of an outer join that may be padded with `NULL`: each
    /// column made nullable.
    pub(crate) fn made_nullable(mut self) -> Self {
        for column in &mut self.columns {
            column.ty.nullable = true;
        }
        self
    }
}

/// What a column reference reaches across the sources in scope: `None` when no source
/// answers to its qualifier (the scope being empty, or the prefix naming nothing), and
/// otherwise the [`Lookup`] of the name in the sources that do.
///
/// Without a qualifier, each source is asked, and a name two of them carry is
/// [`Lookup::Ambiguous`]. With one, the sources it names are asked, and a name absent from
/// them is [`Lookup::Absent`]: the prefix was bound, the name was not. `expr.rs`,
/// `bind_column`, turns the three answers into 207, 209 and 4104.
pub(crate) fn lookup<'a>(
    sources: &'a [Source],
    qualifier: Option<&ObjectName>,
    name: &str,
) -> Option<Lookup<'a>> {
    let mut found = None;
    for source in sources
        .iter()
        .filter(|source| qualifier.is_none_or(|qualifier| source.matches(qualifier)))
    {
        found = Some(match (found, source.column(name)) {
            (None, lookup) | (Some(Lookup::Absent), lookup) => lookup,
            (Some(kept), Lookup::Absent) => kept,
            (Some(_), _) => Lookup::Ambiguous,
        });
    }
    found
}

/// Expands a `*` over the sources in scope: the columns of each source, in the order the
/// sources were written, each source in catalogue order ([`expand`]).
pub(crate) fn expand_all(sources: &[Source], line: u32) -> Vec<BoundProjection> {
    sources
        .iter()
        .flat_map(|source| expand(source, line))
        .collect()
}

#[cfg(test)]
mod tests;
