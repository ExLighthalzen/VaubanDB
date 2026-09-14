//! Queries: `SELECT`, `FROM`, `WHERE`, `TOP`, column names.
//!
//! [`bind_select`] turns a `SELECT` into the plan `statement.rs` wraps in a
//! [`BoundStatement::Query`](crate::BoundStatement::Query). Two things are decided here
//! and nowhere else: the **shape** of the plan, which the executor relies on, and the
//! **name** of each output column, which the client reads in the `COLMETADATA`.
//!
//! # The order of the operators
//!
//! ```text
//! OneRow → Filter (WHERE) → Project (select list) → Limit (TOP)
//! ```
//!
//! `WHERE` filters **before** the projection: the clause may name what the projection does
//! not return. `TOP` truncates **after** it: it counts rows of the result. That is SQL
//! Server's logical processing order of the `SELECT` statement, and the executor builds
//! its iterators on it.
//!
//! # The name of a column
//!
//! An expression without an alias has **no name**: SQL Server sends an empty string in the
//! `COLMETADATA`, and so does the binder — not the text of the expression, not `Column1`:
//!
//! | Query | Column name |
//! |---|---|
//! | `SELECT 1;`, `SELECT 1 + 1;` | `""` |
//! | `SELECT LEN('abc');` | `""` — a function call is not named after the function |
//! | `SELECT @@SPID;`, `SELECT @@VERSION;` | `""` — same |
//! | `SELECT 1 AS n;`, `SELECT 1 n;`, `SELECT n = 1;` | `n` |
//! | `SELECT 1 [my col];` | `my col` |
//! | `SELECT 1 'lit';` | `lit` |
//!
//! The three ways of writing an alias mean the same thing, so `AliasStyle` plays no part
//! here: the text of the `Ident` alone does, delimiters already stripped by the parser.
//!
//! # The arguments of a `nom(…)` in a `FROM`
//!
//! The parser reads `t (NOLOCK)` and `f(1)` alike, as a [`TableRef::Function`]: it is the
//! binder that, the name turning out not to be a function, re-reads a lone hint word as
//! the hint list of a [`TableRef::Table`], or raises **215**. That re-reading is
//! [`reread_table_arguments`]. The name is resolved **first** when the context carries a
//! catalogue, as SQL Server resolves it: `FROM nosuch (1)` and `FROM nosuch (x)` answer 208
//! and not 215 or 207 (`names::check_object_exists`). With no catalogue nothing is
//! consulted, and the re-reading runs before the clause is refused, so that its errors —
//! 215, 207 on an argument — are the ones the client sees; see [`check_table_arguments`].
//!
//! # What is refused
//!
//! A clause the binder does not handle yet (`INTO`, `WITH`, `OFFSET … FETCH`,
//! `FOR XML`/`FOR JSON`) is an **internal** error 50000 naming the clause. A silently
//! dropped clause is worse than a loud one.

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{
    ColumnRef, Expr, Ident, InList, ObjectName, QueryBody, QuerySpec, SelectItem, SelectStatement,
    Span, TableHint, TableRef, Top, UnaryOp,
};
use vauban_sysfn::{FunctionKind, lookup};
use vauban_types::{SqlType, TypeFamily, TypeInfo, Value};

use crate::bound::{
    BoundExpr, BoundExprKind, BoundProjection, BoundTop, LogicalPlan, OutputColumn, OutputSchema,
};
use crate::context::{BindContext, TableReferenceKind};
use crate::errors::{line_of, statement_last_line};
use crate::expr::{Scope, bind_condition, bind_expr};
use crate::names::{alias_of, bind_from, check_object_exists, from_needs_the_catalogue};
use crate::star::{self, Source};
use crate::view;
use crate::{aggregate, join, setop, sort, subquery};

/// Binds a `SELECT` into its bound plan, or hands the clause to the file that binds it.
///
/// The plan is `OneRow`/`Scan` → `Filter` → `Project` → `Limit`, the innermost operators
/// first, `Project` being the level a select list binds to. See the module documentation
/// for why that order is the one SQL Server evaluates in. The clauses below are dispatched
/// and bound elsewhere:
///
/// | Written | Bound by |
/// |---|---|
/// | a `FROM` of more than one source, or one holding a `JOIN` | `join.rs` |
/// | `GROUP BY`, `HAVING`, or a select list holding an aggregate call | `aggregate.rs` |
/// | `ORDER BY`, and `SELECT DISTINCT` over a `FROM` | `sort.rs` |
/// | a derived table in the `FROM` | `subquery.rs` |
/// | `UNION`, `EXCEPT`, `INTERSECT` | `setop.rs` |
///
/// `TOP` is not in that table: it binds into a [`LogicalPlan::Limit`] ([`bind_top`]), and
/// `WITH TIES` and `PERCENT` reach `sort.rs` through the `ORDER BY` row, `WITH TIES`
/// without an `ORDER BY` being error 1062.
///
/// # Errors
///
/// - the user errors the select list and the `WHERE` raise, unchanged: 4145 for
///   `SELECT 1 WHERE 1`, 137 for `SELECT @x = 1`, 263 for `SELECT *`, 1060 for a `TOP`
///   whose row count is not an integer;
/// - the errors of the arguments glued to a name of the `FROM` (`FROM t (1)`, `FROM t (x)`):
///   215, and the 207, 137, 4104 or 195 that binding an argument raises
///   ([`reread_table_arguments`]), once the name has been resolved if a catalogue is
///   there, and before the clause is refused if it is not ([`check_table_arguments`]);
/// - an internal error 50000 naming the form, for a clause of the table above that is not
///   bound yet, for `INTO`, `OFFSET … FETCH`, `WITH` and `FOR XML`.
pub(crate) fn bind_select(stmt: &SelectStatement, ctx: &BindContext<'_>) -> SqlResult<LogicalPlan> {
    if stmt.with.is_some() {
        return Err(not_yet(
            "bind_select: WITH (common table expressions) is not implemented yet",
        ));
    }
    if stmt.offset_fetch.is_some() {
        return Err(not_yet(
            "bind_select: OFFSET … FETCH is not implemented yet",
        ));
    }
    if stmt.for_clause.is_some() {
        return Err(not_yet(
            "bind_select: FOR XML and FOR JSON are not implemented yet",
        ));
    }
    let plan = match &stmt.body {
        QueryBody::Select(spec) => bind_query_spec(spec, stmt, ctx)?,
        QueryBody::SetOp { .. } => setop::bind_set_op(&stmt.body, stmt, ctx)?,
        QueryBody::Nested(..) => {
            return Err(not_yet(
                "bind_select: a parenthesised query body is not implemented yet",
            ));
        }
    };
    if stmt.order_by.is_empty() {
        return Ok(plan);
    }
    sort::bind_order_by(stmt, plan, ctx)
}

/// Binds one `SELECT … WHERE …` specification, the body of [`bind_select`].
///
/// `stmt` is the statement the specification belongs to: `TOP … WITH TIES` needs to know
/// whether an `ORDER BY` was written, and that clause hangs on the statement, not on the
/// specification.
fn bind_query_spec(
    spec: &QuerySpec,
    stmt: &SelectStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    for table_ref in &spec.from {
        check_table_arguments(table_ref, line_of(&stmt.span), ctx)?;
    }
    if !spec.from.is_empty() && ctx.catalog.is_none() {
        return Err(from_needs_the_catalogue());
    }
    if spec.into.is_some() {
        return Err(not_implemented("SELECT … INTO, which creates a table"));
    }

    let (mut plan, joined) = match spec.from.as_slice() {
        [] => (LogicalPlan::OneRow, None),
        [TableRef::Derived { .. }] => (
            subquery::bind_derived(&spec.from[0], line_of(&stmt.span), ctx)?,
            None,
        ),
        [only] if !matches!(only, TableRef::Join { .. }) => {
            (bind_from(only, line_of(&stmt.span), ctx)?, None)
        }
        // `join.rs` hands back the scope with the plan: the sources of a join are not
        // read off its root.
        from => {
            let (plan, scope) = join::bind_from(from, line_of(&stmt.span), ctx)?;
            (plan, Some(scope))
        }
    };
    // The source is read off the `Scan` while it is still the root of the plan, so that a
    // `*` written after a `WHERE` still expands (`star.rs`).
    let scope = match (joined, &plan, spec.from.first()) {
        (Some(scope), _, _) => scope,
        (None, LogicalPlan::Scan { columns, alias, .. }, Some(reference)) => {
            Scope::over(Source::new(
                alias,
                written_name(reference),
                columns,
                ctx.default_schema,
                ctx.database,
            ))
        }
        // An expanded view: `bind_from` put the plan of the definition where a `Scan` would
        // have been, and the columns of the outer query index the **output** of that plan
        // (`view.rs`, `source_columns`). Its alias is read off the reference, the plan having
        // no node to carry it.
        (None, expanded, Some(reference)) => match reference_alias(reference) {
            Some(alias) => Scope::over(Source::new(
                &alias,
                written_name(reference),
                &view::source_columns(expanded.schema()),
                ctx.default_schema,
                ctx.database,
            )),
            None => Scope::empty(),
        },
        (None, _, None) => Scope::empty(),
    };
    if let Some(condition) = &spec.where_ {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: bind_condition(condition, ctx, &scope)?,
        };
    }

    if !spec.group_by.is_empty() || spec.having.is_some() || aggregated_select_list(spec) {
        // `aggregate.rs` builds the `Aggregate` **and** the `Project` above it: the projection
        // of a grouped query names the results of the aggregates by their index in the
        // schema of the `Aggregate` (`bound/mod.rs`), which the loop below knows nothing
        // of. What comes back is therefore the plan up to and including its projection.
        // A select list holding an aggregate comes here with no `GROUP BY` written
        // ([`aggregated_select_list`]): the group is then the whole input, and the
        // `group_by` of the variant is empty.
        let grouped = aggregate::bind_aggregate(spec, plan, ctx)?;
        return finish(grouped, spec, stmt, ctx);
    }

    let mut projections = Vec::with_capacity(spec.items.len());
    for item in &spec.items {
        projections.extend(bind_select_item(item, &scope, ctx)?);
    }
    let schema = OutputSchema {
        columns: projections
            .iter()
            .map(|projection| OutputColumn {
                name: projection.name.clone(),
                ty: projection.expr.ty.clone(),
            })
            .collect(),
    };
    plan = LogicalPlan::Project {
        input: Box::new(plan),
        exprs: projections,
        schema,
    };

    finish(plan, spec, stmt, ctx)
}

/// Puts the operators that sit **above** the projection on a projected plan: `DISTINCT`,
/// then `TOP`.
///
/// Both paths of [`bind_query_spec`] end here — the grouped one, whose projection
/// `aggregate.rs` built, and the plain one — so that the order of those two operators is
/// written once. The `Limit` of a `TOP` is put **above** the `Distinct`: it counts the rows
/// the deduplication left. It is not the last operator of the statement for all that: an
/// `ORDER BY` is bound on the plan this function returns, so the `Sort` `sort.rs` builds
/// sits above that `Limit` (`sort::bind_order_by`, `SELECT TOP 1 c FROM dbo.t ORDER BY c`).
///
/// `SELECT DISTINCT` **without a `FROM`** is accepted and ignored, which is why the word
/// alone does not send the statement to `sort.rs`: over the single row of
/// [`LogicalPlan::OneRow`] it removes nothing (`SELECT DISTINCT 1` answers one row), while
/// over a `FROM` it removes rows (`names.rs`, `distinct_over_a_from_is_a_distinct_node`).
fn finish(
    plan: LogicalPlan,
    spec: &QuerySpec,
    stmt: &SelectStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let mut plan = if spec.distinct && !spec.from.is_empty() {
        sort::bind_distinct(plan, ctx)?
    } else {
        plan
    };
    if let Some(top) = &spec.top {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            top: bind_top(top, stmt, ctx)?,
        };
    }
    Ok(plan)
}

/// Whether the select list of `spec` holds an aggregate call, which sends the query to
/// `aggregate.rs` even when no `GROUP BY` is written.
///
/// `SELECT COUNT(*) FROM dbo.t` aggregates the whole input into one row: the plan is an
/// [`LogicalPlan::Aggregate`] whose `group_by` is empty, so the routing cannot hang on the
/// `GROUP BY` clause alone.
///
/// The `HAVING` and the `GROUP BY` keys are not walked: the caller already sends a query
/// that writes either of them to `aggregate.rs`. Nor is the `WHERE`, where an aggregate is
/// refused rather than grouped, which is `aggregate.rs`'s business.
fn aggregated_select_list(spec: &QuerySpec) -> bool {
    spec.items.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => holds_an_aggregate(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_) => false,
    })
}

/// Whether `expr` holds an aggregate call, the sub-expressions below included.
///
/// Two spellings count: a call the `sysfn` registry answers for with
/// [`FunctionKind::Aggregate`], and a call written with a star — `COUNT(*)` and
/// `COUNT_BIG(*)`, which take no argument and are therefore not entries of that registry
/// (`vauban-sysfn`, `builtins/aggregates.rs`). A name written with a qualifier is left
/// alone, as [`call.rs`](crate::call) leaves it: `dbo.SUM(1)` is error 4121 there.
///
/// A subquery is not walked into: the aggregate of `SELECT (SELECT SUM(c) FROM dbo.u)`
/// groups the inner query, not the outer one. The walk is a worklist rather than a
/// recursion, for the reason `expr.rs`'s `contains_invalid_niladic` gives: the depth of an
/// expression is the client's to choose.
fn holds_an_aggregate(expr: &Expr) -> bool {
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        match expr {
            Expr::Function {
                name, args, star, ..
            } => {
                if *star || is_aggregate_name(name) {
                    return true;
                }
                pending.extend(args);
            }
            Expr::Nested(inner, _)
            | Expr::Unary { expr: inner, .. }
            | Expr::Cast { expr: inner, .. }
            | Expr::IsNull { expr: inner, .. }
            | Expr::Collate { expr: inner, .. }
            | Expr::Assign { value: inner, .. } => pending.push(inner),
            Expr::Binary { left, right, .. } => {
                pending.push(left);
                pending.push(right);
            }
            Expr::Convert { expr, style, .. } => {
                pending.push(expr);
                if let Some(style) = style {
                    pending.push(style);
                }
            }
            Expr::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                if let Some(operand) = operand {
                    pending.push(operand);
                }
                for arm in arms {
                    pending.push(&arm.when);
                    pending.push(&arm.then);
                }
                if let Some(otherwise) = else_ {
                    pending.push(otherwise);
                }
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                pending.push(expr);
                pending.push(pattern);
                if let Some(escape) = escape {
                    pending.push(escape);
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                pending.push(expr);
                pending.push(low);
                pending.push(high);
            }
            Expr::In { expr, list, .. } => {
                pending.push(expr);
                if let InList::Exprs(items) = list {
                    pending.extend(items);
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether an unqualified function name is that of an aggregate of the registry.
fn is_aggregate_name(name: &ObjectName) -> bool {
    if name.server.is_some() || name.database.is_some() || name.schema.is_some() {
        return false;
    }
    lookup(&name.name.value).is_some_and(|def| def.kind == FunctionKind::Aggregate)
}

/// Binds one item of the select list into the projected columns it produces.
///
/// An item is one column, except a wildcard, which is as many as the source has: the
/// answer is a `Vec` the caller splices in place, which is where SQL Server puts the
/// columns of a `*` written between two others (`star.rs`). Five shapes, in this order:
///
/// 1. `*` and `t.*` **over a `FROM`** expand the columns of the source, which `star.rs`
///    does, error 107 or 117 in hand when the prefix names no source;
/// 2. `*` without a `FROM` is error **263** (`SELECT *;`, severity 16, state 1);
/// 3. `t.*` without a `FROM` is error **107** and `a.b.c.d.*` error **117** — see
///    [`qualified_wildcard`];
/// 4. `@x = e` is not a column, see [`assignment`];
/// 5. anything else is the expression, bound by `expr::bind_expr`, under the name of its
///    alias or under the empty name.
///
/// A **named** column is bound by `expr.rs` against the same scope, which is why the
/// wildcards and the expressions are handed the one value: `SELECT b, *, a FROM dbo.t`
/// answers `b`, `a`, `b`, `a` on SQL Server and here (`star.rs`,
/// `a_wildcard_is_expanded_where_it_was_written`).
fn bind_select_item(
    item: &SelectItem,
    scope: &Scope,
    ctx: &BindContext<'_>,
) -> SqlResult<Vec<BoundProjection>> {
    match item {
        SelectItem::Wildcard(span) => match scope.sources() {
            [] => Err(SqlError::select_star_without_from().with_line(line_of(span))),
            sources => Ok(star::expand_all(sources, line_of(span))),
        },
        SelectItem::QualifiedWildcard(name) => star::expand_qualified(scope.sources(), name),
        SelectItem::Expr {
            expr: Expr::Assign { target, span, .. },
            ..
        } => Err(assignment(target, span, ctx)),
        SelectItem::Expr { expr, alias, .. } => {
            let bound = bind_expr(expr, ctx, scope)?;
            let name = column_name(expr, &bound, alias.as_ref());
            Ok(vec![BoundProjection { expr: bound, name }])
        }
    }
}

/// The name of the table a reference was written with, when no alias hides it.
///
/// A `t.*` matches a two-part qualifier against the schema and the object part of that
/// name; an alias replaces both (`star.rs`, `an_alias_hides_the_name_of_the_source`). A
/// reference that is neither a table nor a `nom(…)` has already been refused by
/// `names.rs`, in `bind_from`, before this point.
fn written_name(reference: &TableRef) -> Option<&ObjectName> {
    match reference {
        TableRef::Table {
            name, alias: None, ..
        }
        | TableRef::Function {
            name, alias: None, ..
        } => Some(name),
        _ => None,
    }
}

/// The name the rest of the query refers to an **expanded view** by, `None` for a reference
/// that puts no source in scope.
///
/// A `Scan` carries that string in its `alias` field; the plan of a view is a `Project`, which
/// has no such field, so it is read off the reference by the rule a `Scan` was built with
/// (`names::alias_of`): the alias when one was written, the object part of the name otherwise
/// (`view.rs`, `a_qualifier_names_an_expanded_view`).
/// A reference that is neither a table nor a `nom(…)` has already been refused by `bind_from`,
/// and its `None` leaves the scope empty rather than inventing a name.
fn reference_alias(reference: &TableRef) -> Option<String> {
    match reference {
        TableRef::Table { name, alias, .. } | TableRef::Function { name, alias, .. } => {
            Some(alias_of(name, alias.as_ref()))
        }
        _ => None,
    }
}

/// The name a projected column is returned under: the alias, the written name of a column
/// reference, or the empty string.
///
/// The alias is taken as the parser stored it, delimiters removed and `]]` unescaped, so
/// `[my col]` and `'lit'` name a column `my col` and `lit`. The way it was written
/// (`AS n`, `n`, `n = e`) changes nothing — see the module documentation.
///
/// Without an alias, a **column reference** keeps the name the user wrote for it, and an
/// expression stays nameless. Over `dbo.t (a int NOT NULL, …)`:
///
/// | written | header |
/// |---|---|
/// | `SELECT a FROM dbo.t`, `SELECT [a] FROM dbo.t`, `SELECT (a) FROM dbo.t` | `a` |
/// | `SELECT A FROM dbo.t` | `A` — the case as written, not the catalogue's |
/// | `SELECT t.a FROM dbo.t` | `a` — the qualifier is dropped |
/// | `SELECT +a FROM dbo.t`, `SELECT + +a`, `SELECT +(a)` | `a` — see [`written_column`] |
/// | `SELECT a + 0 FROM dbo.t`, `SELECT ABS(b) FROM dbo.t` | `""` |
/// | `SELECT -a FROM dbo.t`, `SELECT ~a FROM dbo.t` | `""` |
/// | `SELECT USER FROM dbo.t`, the table holding a column `[USER]` | `""` — the function |
///
/// The last two lines are why the **bound** node decides and not the written one: `USER`
/// is written as a column reference and binds to a niladic call, which has no name.
fn column_name(expr: &Expr, bound: &BoundExpr, alias: Option<&Ident>) -> String {
    if let Some(alias) = alias {
        return alias.value.clone();
    }
    if !matches!(bound.kind, BoundExprKind::ColumnRef(_)) {
        return String::new();
    }
    written_column(expr).map_or_else(String::new, |ident| ident.value.clone())
}

/// The identifier a column reference was written with, parentheses and unary `+` looked
/// through.
///
/// `SELECT (a) FROM dbo.t`, `SELECT +a FROM dbo.t`, `SELECT + +a FROM dbo.t` and
/// `SELECT +(a) FROM dbo.t` answer the header `a`, where `SELECT -a` and `SELECT ~a` answer
/// the empty name. The pair `+a` / `-a` is what separates the two rules: the unary plus is
/// the one operator the binder drops, `bind_unary` handing its
/// operand back unchanged, so the node that comes out *is* the column reference, while `-a`
/// and `~a` build a `Negate` and a `BitNot` that [`column_name`] leaves nameless by its
/// `ColumnRef` test.
fn written_column(expr: &Expr) -> Option<&Ident> {
    match expr {
        Expr::Nested(inner, _) => written_column(inner),
        Expr::Unary {
            op: UnaryOp::Plus,
            expr: inner,
            ..
        } => written_column(inner),
        Expr::Column(column) => Some(&column.name),
        _ => None,
    }
}

/// The error a `t.*` raises when no `FROM` names `t`.
///
/// | Query | Answer |
/// |---|---|
/// | `SELECT t.*;` | 107/15/1, the prefix `'t'` matching no table name or alias |
/// | `SELECT db.dbo.t.*;` | 107/15/1, the prefix printed whole: `'db.dbo.t'` |
/// | `SELECT a.b.c.d.*;` | 117/15/1, more than the maximum of 3 prefixes |
///
pub(crate) fn qualified_wildcard(name: &ObjectName) -> SqlError {
    let printed = dotted(name);
    let error = if name.server.is_some() {
        SqlError::too_many_column_prefixes(&printed)
    } else {
        SqlError::column_prefix_does_not_match(&printed)
    };
    error.with_line(line_of(&name.span))
}

/// The parts of an object name joined by dots, as SQL Server prints them in messages 107,
/// 117, 208 and 215: the identifiers themselves, as written, without their delimiters.
/// 208 is `names.rs`'s, and prints the name the same way (`names.rs`, module
/// documentation).
pub(crate) fn dotted(name: &ObjectName) -> String {
    [
        name.server.as_ref(),
        name.database.as_ref(),
        name.schema.as_ref(),
        Some(&name.name),
    ]
    .into_iter()
    .skip_while(Option::is_none)
    .map(|ident| ident.map_or("", |ident| ident.value.as_str()))
    .collect::<Vec<_>>()
    .join(".")
}

/// The error an `@x = e` written where a column is expected raises.
///
/// An assignment produces **no column**: it stores a value into a variable, and the
/// statement answers no result set. A `SELECT` whose own list assigns does not come here:
/// `statement.rs` routes it to `variables.rs`. What can come here is an assignment under
/// a set operator, `SELECT @x = 1 UNION SELECT 2;`, which answers error **141** (severity
/// 15, state 1) once the variable is in scope, and error **137** (severity 15, **state
/// 1**, the state of an assignment, where reading the same undeclared variable answers
/// state 2) when it is not. That state has its own constructor,
/// `SqlError::must_declare_scalar_variable_assigned`, so nothing is retouched here. The
/// set operators are not bound yet, so that shape is refused before reaching this
/// function.
fn assignment(target: &str, span: &Span, ctx: &BindContext<'_>) -> SqlError {
    if ctx.variables.type_of(target).is_none() {
        return SqlError::must_declare_scalar_variable_assigned(target).with_line(line_of(span));
    }
    SqlError::assignment_mixed_with_data_retrieval().with_line(line_of(span))
}

/// The words a lone bare identifier glued to a table name is re-read as: the table hints
/// SQL Server accepts without `WITH`.
///
/// One `SELECT id FROM t (<word>) WHERE id = 1;` per word over a table `t`: eighteen of
/// them answer the row on SQL Server: `FORCESCAN`, `HOLDLOCK`, `KEEPDEFAULTS`,
/// `KEEPIDENTITY`, `NOLOCK`, `NOWAIT`, `PAGLOCK`, `READCOMMITTED`, `READCOMMITTEDLOCK`,
/// `READPAST`, `READUNCOMMITTED`, `REPEATABLEREAD`, `ROWLOCK`, `SERIALIZABLE`, `TABLOCK`,
/// `TABLOCKX`, `UPDLOCK`, `XLOCK`. Five more are re-read as a hint and **then** refused by
/// the check of the hint itself, which this module does not do: `NOEXPAND` and
/// `IGNORE_CONSTRAINTS`/`IGNORE_TRIGGERS` answer 8171 (the hint is invalid on the object),
/// `SNAPSHOT` 367, `FORCESEEK` 8622. They are listed so that the word is read as SQL Server
/// reads it, a hint; VaubanDB then keeps it without applying it, as it keeps
/// `WITH (NOEXPAND)`.
///
/// Two words that look like hints are **not** in the list: `FASTFIRSTROW`, a hint of older
/// versions, answers 207 (invalid column name) — it is a column now; and `INDEX`, which
/// answers 1018 out of the **parser** (`t (INDEX(0))`, `t (NOLOCK, INDEX(0))`), a number
/// `vauban_errors` does not have and the parser of VaubanDB does not raise.
///
/// The match ignores ASCII case: `t (nolock)` and `t ([nolock])` answer the row too.
const TABLE_HINT_WORDS: [&str; 23] = [
    "FORCESCAN",
    "FORCESEEK",
    "HOLDLOCK",
    "IGNORE_CONSTRAINTS",
    "IGNORE_TRIGGERS",
    "KEEPDEFAULTS",
    "KEEPIDENTITY",
    "NOEXPAND",
    "NOLOCK",
    "NOWAIT",
    "PAGLOCK",
    "READCOMMITTED",
    "READCOMMITTEDLOCK",
    "READPAST",
    "READUNCOMMITTED",
    "REPEATABLEREAD",
    "ROWLOCK",
    "SERIALIZABLE",
    "SNAPSHOT",
    "TABLOCK",
    "TABLOCKX",
    "UPDLOCK",
    "XLOCK",
];

/// Walks references in FROM order, classifying calls before considering hints.
///
/// # What the re-reading stands in for
///
/// SQL Server resolves the name **first**: `FROM nosuch (1)`, `nosuch (NOLOCK)`, `nosuch ()`
/// and `nosuch (x)` answer 208 (invalid object name) — the last one 208 and not 207, which
/// is what puts the resolution before the arguments. A name that resolves to a
/// table-valued function is a call: `dbo.f (NOLOCK)` answers 207 (invalid column name),
/// `dbo.f ()` 313, and no hint is looked for. A name that resolves to a scalar function is
/// no object (`dbo.s (1)` answers 208, state 3), and a built-in scalar function no more
/// (`LEN(1)` answers 208 `'LEN'`). A name that resolves to a table or a view reaches
/// [`reread_table_arguments`]; the other names do not.
///
/// A supplied `CatalogView` can identify a table-valued function: its arguments are bound
/// as expressions without hint re-reading. Any other classification has its name resolved
/// before the re-reading, which is what puts 208 in front of 215 and 207. With no
/// catalogue, nothing is consulted and the provisional table path remains. Function
/// signatures, the 313 of `dbo.f ()` and the 208 of a scalar function are still to come,
/// with the table-valued functions.
///
/// The re-read reference is dropped: `bind_from` scans the reference the parser built, its
/// arguments being the hint this walk accepted. What survives is the **error**, so that
/// `FROM t (1)` answers 215 and `FROM t (x)` 207 rather than the refusal of a clause or the
/// error of another reference. In the order of the `FROM`: `t (x), t (1)` answers 207 and
/// `t (1), t (x)` 215, and a join is walked left to right.
///
/// A derived table is a query of its own: `subquery.rs` binds it through `bind_select`,
/// which walks its `FROM` in turn, so it is not descended here. A CTE is refused before
/// this point.
///
/// # Errors
///
/// The 208 of [`check_object_exists`], then those of [`reread_table_arguments`].
fn check_table_arguments(
    table_ref: &TableRef,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<()> {
    match table_ref {
        TableRef::Function {
            name,
            args,
            alias,
            span,
        } => {
            let kind = ctx.catalog.map_or(TableReferenceKind::Unknown, |catalog| {
                catalog.classify_table_reference(name, ctx.database, ctx.default_schema)
            });
            if kind == TableReferenceKind::TableValuedFunction {
                // Function arguments are expressions: NOLOCK must not become a hint.
                for arg in args {
                    bind_expr(arg, ctx, &Scope::empty())?;
                }
                Ok(())
            } else {
                check_object_exists(name, statement_line, ctx)?;
                reread_table_arguments(name, args, alias.as_ref(), span, ctx).map(drop)
            }
        }
        TableRef::Join { left, right, .. } | TableRef::Apply { left, right, .. } => {
            check_table_arguments(left, statement_line, ctx)?;
            check_table_arguments(right, statement_line, ctx)
        }
        TableRef::Pivot(pivot) => check_table_arguments(&pivot.source, statement_line, ctx),
        TableRef::Unpivot(unpivot) => check_table_arguments(&unpivot.source, statement_line, ctx),
        TableRef::Table { .. } | TableRef::Derived { .. } | TableRef::Variable { .. } => Ok(()),
    }
}

/// Re-reads the arguments of a `nom(…)` whose name is **not** a function: a lone hint word
/// becomes the hint list of a [`TableRef::Table`], anything else is error 215.
///
/// What SQL Server answers over a table `t`:
///
/// | written | answer |
/// |---|---|
/// | `t (NOLOCK)`, `t (nolock)`, `t ([NOLOCK])`, `t ("NOLOCK")` | the rows, as `t WITH (NOLOCK)` |
/// | `t ((NOLOCK))`, `t (((NOLOCK)))` | the rows: the parentheses change nothing |
/// | `t (NOLOCK) AS z`, `t (NOLOCK) z` | the rows: the hint sits before the alias |
/// | `t (1)`, `t ()`, `t ('a')`, `t (NULL)`, `t (1, 2)`, `t ('NOLOCK')` | **215** |
/// | `t (@p)` declared, `t ((SELECT 1))`, `t (1 + 'a')`, `t (CURRENT_TIMESTAMP)` | 215: the argument binds, then the name is no function |
/// | `t (x)`, `t (id)` — `id` a column of `t` | 207 naming `x` and `id`, respectively |
/// | `t (NOLOCK, 1)`, `t (1, NOLOCK)`, `t (NOLOCK, NOLOCK)`, `t (NOLOCK, READPAST)`, `t (NOLOCK + 0)` | 207 on `'NOLOCK'` |
/// | `t (dbo.NOLOCK)`, `t (t.NOLOCK)` | 4104 |
/// | `t (@p)` undeclared | 137, state 2 |
/// | `t (NO_SUCH_FN(1))` | 195 |
/// | `t ([NO LOCK])` | 207 `'NO LOCK'` |
///
/// The implemented dispatch has two branches:
///
/// 1. **exactly one** argument that is a bare, unqualified identifier naming a table hint
///    ([`TABLE_HINT_WORDS`]), parentheses around it stripped, whatever its case and its
///    delimiters — the reference is `t WITH (<word>)`, the word kept as written, the alias
///    kept;
/// 2. other arguments produce 215 after successful binding by the expression binder. The
///    single-argument shapes `x`, `@p`, `dbo.NOLOCK` and `NO_SUCH_FN(1)` produce 207, 137,
///    4104 and 195. The provisional left-to-right traversal differs from SQL Server for
///    `t(NOLOCK, @x)`: it produces 207 rather than 137. `t(DEFAULT)` also remains
///    unsupported (50000 rather than 215). Both are deliberate differences.
///
/// The hint word wins over a column of that name that *is* in scope: with an outer column
/// named `NOLOCK`, `SELECT (SELECT COUNT(*) FROM t (NOLOCK)) FROM (SELECT 1 AS NOLOCK) d`
/// answers the row, where the same shape with `x` answers 215 because `x` binds to the
/// outer column. That is what puts branch 1 before branch 2: had the column been tried
/// first, `NOLOCK` would have bound and 215 followed. Without a catalogue the scope is
/// empty: the unqualified `x` and `id` answer 207, while `dbo.NOLOCK` answers 4104.
///
/// The arguments are bound and **dropped**: they type-check (`t (1 + 'a')` answers 215, not
/// 245), they do not run.
///
/// # Error 215
///
/// Severity 16, state 1. The name is printed **as written, delimiters removed**, not
/// resolved: `[dbo].[t]` and `dbo.[t]` print `dbo.t`, `[t]` and `"t"` print `t`, `dbo.T`
/// prints `dbo.T`, `DBO.t` prints `DBO.t`, `master.dbo.spt_values` prints the three parts.
/// Its line is the one the **name starts on**: with `FROM`, `dbo`, `.`, `t` and `(1)` each
/// on its own line, the answer is the line of `dbo`; with `t`, `(`, `1`, `)` and `;` each
/// on its own, the line of `t`. 207 keeps the line of the column, as everywhere.
///
/// The catalogue owns the entry and the constructor. The binder supplies the written name
/// and the line.
///
/// # Errors
///
/// - 215 when the arguments are not a lone hint word;
/// - before it, whatever binding an argument raises: 207, 137, 4104, 195, 8631…
pub(crate) fn reread_table_arguments(
    name: &ObjectName,
    args: &[Expr],
    alias: Option<&Ident>,
    span: &Span,
    ctx: &BindContext<'_>,
) -> SqlResult<TableRef> {
    if let [arg] = args
        && let Some(word) = lone_hint_word(arg)
    {
        return Ok(TableRef::Table {
            name: name.clone(),
            alias: alias.cloned(),
            hints: vec![TableHint {
                name: word.name.value.clone(),
                args: Vec::new(),
                span: word.span,
            }],
            span: *span,
        });
    }
    for arg in args {
        bind_expr(arg, ctx, &Scope::empty())?;
    }
    Err(parameters_supplied(name))
}

/// The hint word an argument is, if it is one: a bare, unqualified identifier of
/// [`TABLE_HINT_WORDS`], under any number of parentheses.
///
/// `Ident::quoted` plays no part: `[NOLOCK]` and `"NOLOCK"` are hints like `NOLOCK`.
/// Nothing else is: not a literal, not a variable, not `NOLOCK + 0`, not `dbo.NOLOCK`.
fn lone_hint_word(arg: &Expr) -> Option<&ColumnRef> {
    match arg {
        Expr::Nested(inner, _) => lone_hint_word(inner),
        Expr::Column(column) if column.qualifier.is_none() && is_table_hint(&column.name.value) => {
            Some(column)
        }
        _ => None,
    }
}

/// Whether `word` is a hint, ignoring ASCII case and trailing ASCII spaces; this does not
/// trim tabs or other Unicode whitespace.
fn is_table_hint(word: &str) -> bool {
    TABLE_HINT_WORDS
        .iter()
        .any(|hint| hint.eq_ignore_ascii_case(word.trim_end_matches(' ')))
}

/// The error 215 of a `nom(…)` whose arguments are not a hint — see
/// [`reread_table_arguments`] for the text, the name and the line.
fn parameters_supplied(name: &ObjectName) -> SqlError {
    SqlError::parameters_supplied_to_non_function(&dotted(name)).with_line(line_of(&name.span))
}

/// Binds the `TOP` clause into the [`BoundTop`] of a `Limit`.
///
/// `TOP` is honoured even over a single row: `SELECT TOP 0 1` returns nothing.
///
/// # The type of the row count
///
/// The expression is converted to `bigint`, or to `float` when `PERCENT` is written. SQL
/// Server is stricter than that first conversion: anything that is not already an
/// **integer** type is refused rather than truncated:
///
/// | Query | Answer |
/// |---|---|
/// | `SELECT TOP 5.5 1;`, `SELECT TOP 1.0 1;`, `SELECT TOP (1e0) 1;` | 1060/15/1 |
/// | `SELECT TOP (CAST(1 AS bit)) 1;`, `SELECT TOP (CAST('1' AS varchar(2))) 1;` | 1060/15/1 |
/// | `SELECT TOP (CAST(1 AS bigint)) 1;`, `SELECT TOP (0) 1;` | accepted |
/// | `SELECT TOP (NULL) 1;` | 1060/15/1, although `NULL` alone is typed `int` |
/// | `SELECT TOP 5.5 PERCENT 1;`, `SELECT TOP (CAST('50' AS varchar(2))) PERCENT 1;` | accepted |
///
/// `PERCENT` therefore checks nothing at bind time: the conversion to `float` is a
/// [`BoundExprKind::Convert`] node the executor evaluates, and a value it refuses
/// (`SELECT TOP (NULL) PERCENT 1;` answers 1014) is its business, not the binder's.
///
/// # What the binder does **not** check
///
/// A negative count (`SELECT TOP (-1) 1;` answers 127/15/1) and a `NULL` hidden behind an
/// expression (`SELECT TOP (CAST(NULL AS int)) 1;` answers 1060) are **values**: seeing
/// them at bind time would mean folding constants, which the binder does not do. The executor
/// raises them, `SqlError::top_negative` and `SqlError::top_null` in hand. The bare `NULL`
/// literal above is the one exception, and it is not a fold: the node *is* the constant.
///
/// # Errors
///
/// - **1060** (`SqlError::top_null`) when a non-`PERCENT` row count is not an integer;
/// - **1062** for `WITH TIES` without `ORDER BY`
///   (`SELECT TOP (1) WITH TIES 1;`, severity 15, state 1);
/// - whatever binding the row-count expression raises.
///
/// # 1062 is reported on the **last** line of the statement
///
/// Not on the `TOP` clause, and not on the select list either. With the `SELECT` on line 2
/// and `TOP (1) WITH TIES` on line 3: `1` / `WHERE` / `1 = 1;` answers **6**, the
/// statement's end, where the select list ends on 4. The terminating `;` counts and a
/// comment after it does not ([`statement_last_line`] carries the eight shapes). Nothing
/// about the `TOP` clause
/// explains it: 127 and 1060 name the row count, 1031 the first line of the select list and
/// 1062 the last line of the statement, three lines for three errors of one clause.
///
fn bind_top(top: &Top, stmt: &SelectStatement, ctx: &BindContext<'_>) -> SqlResult<BoundTop> {
    if top.with_ties && stmt.order_by.is_empty() {
        return Err(SqlError::top_with_ties_without_order_by()
            .with_line(statement_last_line(ctx.text, &stmt.span)));
    }
    let count = bind_expr(&top.expr, ctx, &Scope::empty())?;
    let target = if top.percent {
        SqlType::Float
    } else {
        if count.ty.ty.family() != TypeFamily::Integer || is_null_literal(&count) {
            return Err(SqlError::top_null().with_line(count.line));
        }
        SqlType::BigInt
    };
    Ok(BoundTop {
        expr: convert_to(count, target),
        percent: top.percent,
        with_ties: top.with_ties,
    })
}

/// Whether a bound expression is the `NULL` **literal** itself, the only value the binder
/// knows without evaluating anything (see [`bind_top`]).
fn is_null_literal(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::Literal(Value::Null))
}

/// Wraps an expression in the [`BoundExprKind::Convert`] node that takes it to `target`,
/// or hands it back untouched when it already has that type.
///
/// The binder inserts the implicit conversions explicitly: after binding, the executor
/// decides no conversion of its own. Nullability and line are those of the operand.
fn convert_to(expr: BoundExpr, target: SqlType) -> BoundExpr {
    if expr.ty.ty == target {
        return expr;
    }
    let ty = TypeInfo::new(target, expr.ty.nullable);
    let line = expr.line;
    BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(expr),
            style: None,
            try_: false,
        },
        ty,
        line,
    }
}

/// A clause the binder does not bind, reported to the client as the generic error 50000.
/// `names.rs` raises its own through this function, so that they read alike.
pub(crate) fn not_yet(what: &str) -> SqlError {
    bug(what.to_owned())
}

/// An engine bug, reported to the client as the generic error 50000.
pub(crate) fn bug(message: impl Into<String>) -> SqlError {
    SqlError::from(InternalError::Bug(message.into()))
}

/// The internal error a form the binder does not handle yet answers: what was written,
/// and that it is not implemented (`tests/bound_shape_relational.rs`,
/// `an_unimplemented_form_names_itself`).
pub(crate) fn not_implemented(form: &str) -> SqlError {
    bug(format!("{form} is not implemented yet"))
}

#[cfg(test)]
mod tests {
    use super::{
        TABLE_HINT_WORDS, column_name, dotted, is_null_literal, is_table_hint, lone_hint_word,
        reread_table_arguments,
    };
    use crate::bound::{BoundExpr, BoundExprKind, ColumnBinding};
    use crate::context::{BindContext, SessionOptions};
    use vauban_catalog::ColumnId;
    use vauban_parser::{
        Expr, Ident, ObjectName, ParseOptions, QueryBody, Span, Statement, TableRef, parse_batch,
    };
    use vauban_types::{SqlType, TypeInfo, Value};

    fn ident(value: &str) -> Ident {
        Ident {
            value: value.to_owned(),
            quoted: false,
        }
    }

    /// An expression without an alias has no name; a column reference keeps the name it
    /// was written with, parentheses and delimiters looked through, and the alias wins over
    /// both. The four spellings of the module documentation of [`column_name`].
    #[test]
    fn a_column_keeps_its_written_name_and_an_expression_has_none() {
        let literal = BoundExpr {
            kind: BoundExprKind::Literal(Value::I32(1)),
            ty: TypeInfo::new(SqlType::Int, false),
            line: 1,
        };
        let one = expr_of("SELECT 1");
        assert_eq!(column_name(&one, &literal, None), "");
        assert_eq!(column_name(&one, &literal, Some(&ident("n"))), "n");

        let reference = BoundExpr {
            kind: BoundExprKind::ColumnRef(ColumnBinding {
                column: ColumnId(1),
                index: 0,
                name: "a".to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
            }),
            ty: TypeInfo::new(SqlType::Int, false),
            line: 1,
        };
        for (text, expected) in [
            ("SELECT a FROM t", "a"),
            ("SELECT A FROM t", "A"),
            ("SELECT [a] FROM t", "a"),
            ("SELECT (a) FROM t", "a"),
            ("SELECT t.a FROM t", "a"),
            ("SELECT +a FROM t", "a"),
            ("SELECT + +a FROM t", "a"),
            ("SELECT +(a) FROM t", "a"),
        ] {
            let written = expr_of(text);
            assert_eq!(column_name(&written, &reference, None), expected, "{text}");
        }
        // The alias still wins, and a node that did not bind to a column has no name even
        // when it was written as one (`SELECT USER FROM dbo.t`).
        let written = expr_of("SELECT a FROM t");
        assert_eq!(column_name(&written, &reference, Some(&ident("x"))), "x");
        assert_eq!(column_name(&written, &literal, None), "");
        // `-a` and `~a` are not column references, written or bound: the unary plus is the
        // one `written_column` looks through.
        for text in ["SELECT -a FROM t", "SELECT ~a FROM t"] {
            let written = expr_of(text);
            assert_eq!(column_name(&written, &reference, None), "", "{text}");
        }
    }

    /// The expression of the first select item of `text`.
    #[track_caller]
    fn expr_of(text: &str) -> Expr {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let Statement::Select(select) = &batch.statements[0] else {
            panic!("not a SELECT: {text}");
        };
        let QueryBody::Select(spec) = &select.body else {
            panic!("not a plain SELECT: {text}");
        };
        let vauban_parser::SelectItem::Expr { expr, .. } = &spec.items[0] else {
            panic!("not an expression: {text}");
        };
        expr.clone()
    }

    #[test]
    fn dotted_joins_the_parts_that_were_written() {
        let mut name = ObjectName {
            server: None,
            database: None,
            schema: None,
            name: ident("t"),
            span: Span::EMPTY,
        };
        assert_eq!(dotted(&name), "t");
        name.schema = Some(ident("dbo"));
        assert_eq!(dotted(&name), "dbo.t");
        name.database = Some(ident("db"));
        name.server = Some(ident("srv"));
        assert_eq!(dotted(&name), "srv.db.dbo.t");
    }

    #[test]
    fn only_the_null_literal_is_a_null_literal() {
        let null = BoundExpr {
            kind: BoundExprKind::Literal(Value::Null),
            ty: TypeInfo::new(SqlType::Int, true),
            line: 1,
        };
        assert!(is_null_literal(&null));
        let one = BoundExpr {
            kind: BoundExprKind::Literal(Value::I32(1)),
            ty: TypeInfo::new(SqlType::Int, false),
            line: 1,
        };
        assert!(!is_null_literal(&one));
    }

    /// The first table reference of the `FROM` of `text`, a `SELECT`.
    fn first_table_ref(text: &str) -> TableRef {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let Statement::Select(select) = &batch.statements[0] else {
            panic!("not a SELECT: {text}");
        };
        let QueryBody::Select(spec) = &select.body else {
            panic!("not a plain SELECT: {text}");
        };
        spec.from[0].clone()
    }

    /// Re-reads the `nom(…)` written in `text`, and returns its re-serialisation.
    fn reread(text: &str) -> String {
        // `TableRef` implements `Drop`, which forbids moving a field out of one by pattern
        // matching (E0509): it is read through a borrow instead.
        let table = first_table_ref(text);
        let TableRef::Function {
            name,
            args,
            alias,
            span,
        } = &table
        else {
            panic!("not a nom(…): {text}");
        };
        let ctx = BindContext::scalar(text, SessionOptions::default());
        reread_table_arguments(name, args, alias.as_ref(), span, &ctx)
            .expect("the arguments are a hint")
            .to_string()
    }

    /// `FROM t (NOLOCK)` is `FROM t WITH (NOLOCK)`: the re-read reference re-serialises
    /// exactly like the one the parser builds from the `WITH` spelling, alias included.
    #[test]
    fn a_lone_hint_word_is_the_hint_list_of_the_table() {
        let with = first_table_ref("SELECT 1 FROM t WITH (NOLOCK)").to_string();
        assert_eq!(with, "t WITH (NOLOCK)");
        assert_eq!(reread("SELECT 1 FROM t (NOLOCK)"), with);
        assert_eq!(reread("SELECT 1 FROM t(NOLOCK)"), with);
        assert_eq!(reread("SELECT 1 FROM t ((NOLOCK))"), with);
        assert_eq!(reread("SELECT 1 FROM t (((NOLOCK)))"), with);
        assert_eq!(reread("SELECT 1 FROM t ([NOLOCK])"), with);
        assert_eq!(reread("SELECT 1 FROM t (\"NOLOCK\")"), with);

        let with_alias = first_table_ref("SELECT 1 FROM t AS z WITH (NOLOCK)").to_string();
        assert_eq!(with_alias, "t AS z WITH (NOLOCK)");
        assert_eq!(reread("SELECT 1 FROM t (NOLOCK) AS z"), with_alias);
        assert_eq!(reread("SELECT 1 FROM t (NOLOCK) z"), with_alias);

        // The name is kept as written, and so is the word.
        assert_eq!(
            reread("SELECT 1 FROM [dbo].[t] (nolock)"),
            first_table_ref("SELECT 1 FROM [dbo].[t] WITH (nolock)").to_string()
        );
        assert_eq!(
            reread("SELECT 1 FROM dbo.t (readpast)"),
            "dbo.t WITH (readpast)"
        );
    }

    #[test]
    fn every_hint_word_is_matched_ignoring_case() {
        for word in TABLE_HINT_WORDS {
            assert!(is_table_hint(word), "{word}");
            assert!(is_table_hint(&word.to_ascii_lowercase()), "{word}");
        }
        assert!(!is_table_hint("x"));
        assert!(!is_table_hint("FASTFIRSTROW"));
        assert!(!is_table_hint("INDEX"));
        assert!(!is_table_hint("NO LOCK"));
    }

    /// A bare, unqualified hint word — alone — is a hint; the rest are arguments.
    #[test]
    fn what_is_not_a_lone_hint_word() {
        let args_of = |text: &str| {
            // Borrowed as in `reread` above, then cloned, because the reference itself
            // does not outlive the closure.
            let table = first_table_ref(text);
            let TableRef::Function { args, .. } = &table else {
                panic!("not a nom(…): {text}");
            };
            args.clone()
        };
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t (NOLOCK)")[0]).is_some());
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t ((NOLOCK))")[0]).is_some());
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t (x)")[0]).is_none());
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t (1)")[0]).is_none());
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t ('NOLOCK')")[0]).is_none());
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t (@p)")[0]).is_none());
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t (dbo.NOLOCK)")[0]).is_none());
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t (NOLOCK + 0)")[0]).is_none());
        assert!(lone_hint_word(&args_of("SELECT 1 FROM t ([NO LOCK])")[0]).is_none());
        // Two words are two arguments, whatever they spell.
        let two = args_of("SELECT 1 FROM t (NOLOCK, READPAST)");
        assert_eq!(two.len(), 2);
    }
}
