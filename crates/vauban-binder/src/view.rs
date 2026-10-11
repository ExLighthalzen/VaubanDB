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
//! The filter on the database is not a name: the per-database views read `DB_ID()` and
//! `DB_NAME()`, which answer the session's database. A view named with a database part has
//! both calls pinned to that database before its text is parsed ([`in_named_database`]), so
//! `d2.sys.tables` read from `master` answers the tables of `d2` (unit test
//! `a_view_named_with_a_database_filters_on_that_database`).
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

use std::borrow::Cow;
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
    let definition = in_named_database(definition, written.database.as_ref());
    let definition = definition.as_ref();
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
            depth: 0,
        })
        .collect()
}

/// The part of a name that was written, or the value the context completes it with — the rule
/// `CatalogView::resolve_table` applies to find the object (`names.rs`, module
/// documentation).
fn part<'a>(written: Option<&'a Ident>, default: &'a str) -> &'a str {
    written.map_or(default, |ident| ident.value.as_str())
}

/// The definition as it reads in the database the view was named in.
///
/// The per-database views filter their internal table on `database_id = DB_ID()` and spell
/// their catalog column as `DB_NAME()`. Both calls answer the database of the session when
/// the statement runs, which is the database of the view when the name carries no database
/// part. When it carries one, `d2.sys.tables` read from `master`, the two calls are pinned
/// to that database: `DB_ID()` becomes `DB_ID(N'd2')`, and `DB_NAME()` becomes
/// `DB_NAME(DB_ID(N'd2'))`, which answers the spelling the catalogue holds rather than the
/// one written. The text is the catalogue's own, so the two spellings are matched as the
/// definitions write them; a definition without either call is returned as it is (unit
/// tests `a_view_named_with_a_database_filters_on_that_database` and
/// `a_view_named_without_a_database_keeps_its_definition`).
fn in_named_database<'a>(definition: &'a str, database: Option<&Ident>) -> Cow<'a, str> {
    let Some(database) = database else {
        return Cow::Borrowed(definition);
    };
    if !definition.contains("DB_ID()") && !definition.contains("DB_NAME()") {
        return Cow::Borrowed(definition);
    }
    let id = format!("DB_ID(N'{}')", database.value.replace('\'', "''"));
    // `DB_NAME()` is first spelled through `DB_ID()`, so that the written name is inserted
    // by the last replacement and stays inside its literal
    // (`a_database_name_spelling_a_call_is_not_replaced_again`).
    Cow::Owned(
        definition
            .replace("DB_NAME()", "DB_NAME(DB_ID())")
            .replace("DB_ID()", &id),
    )
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
mod tests;
