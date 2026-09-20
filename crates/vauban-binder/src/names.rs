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
use crate::hints;
use crate::query::{bug, dotted, not_yet};
use crate::view;

/// Binds the single table reference of a `FROM` into the plan that reads it.
///
/// `statement_line` is the line the statement starts on, which is the one a 208 carries
/// (module documentation). The reference has already been walked by
/// `query::check_table_arguments`, so a `nom(…)` that reaches this function carries a lone
/// hint word: `hints.rs` reads it, as it reads a `WITH (NOLOCK)` written in full, into the
/// `hints` of the `Scan`, before the name is resolved.
///
/// # Errors
///
/// - the errors of `hints::bind_hints` (1047 among them), before the name is looked up;
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
        TableRef::Table {
            name, alias, hints, ..
        } => {
            let hints = hints::bind_hints(hints, ctx)?;
            scan(name, alias.as_ref(), hints, statement_line, ctx)
        }
        // The arguments are the hint `check_table_arguments` accepted; anything else has
        // already answered 215, or the error one of them raised.
        TableRef::Function {
            name, alias, args, ..
        } => {
            let hints = hints::bind_argument_hints(args, ctx)?;
            scan(name, alias.as_ref(), hints, statement_line, ctx)
        }
        // `query.rs` routes a join to `join.rs` before reaching this function; the arm is
        // kept so that the `match` on `TableRef` stays exhaustive.
        TableRef::Join { .. } => Err(not_yet("bind_from: a join in FROM is not implemented yet")),
        TableRef::Apply { .. } => Err(not_yet(
            "bind_select: CROSS APPLY and OUTER APPLY in FROM are not implemented yet",
        )),
        // Routed to `subquery.rs` by `query.rs` before reaching this function; the arm is
        // kept so that the `match` on `TableRef` stays exhaustive.
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
    hints: LockHints,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let resolved = resolve(name, ctx)?.ok_or_else(|| invalid_object_name(name, statement_line))?;
    match resolved.kind {
        // A view holds no row of its own: the plan of its definition takes the place of the
        // `Scan` this function would have built (`view.rs`). The hints written on the
        // reference were checked above and are not carried into that plan.
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
                hints,
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
pub(crate) fn resolve_table(
    name: &ObjectName,
    ctx: &BindContext<'_>,
) -> SqlResult<Option<ResolvedTable>> {
    resolve(name, ctx)
}

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
mod tests;
