//! The `FROM` of more than one source: `FROM a, b`, `FROM a JOIN b ON …`, and the
//! multi-table scope the columns of such a query resolve against (209, 4104, 1011 to 1013).
//!
//! [`bind_from`] folds the list the parser built into a **left-deep** tree of
//! [`LogicalPlan::Join`] nodes, in the order the sources were written, and hands back the
//! scope the rest of the query binds in. It decides what is stated below over
//! `dbo.a (k int NOT NULL, c int NULL)`, `dbo.b (k, c)` and `dbo.d (k int NOT NULL, e int NULL)`;
//! `tests/bind_join.rs` binds each line.
//!
//! # The shape of the tree
//!
//! | written | bound |
//! |---|---|
//! | `FROM a JOIN b ON … JOIN d ON …` | `Join(Join(a, b), d)` |
//! | `FROM a, b` | `Join(a, b)` of kind `Cross`, without an `on` |
//! | `FROM a, b JOIN d ON …` | `Join(a, Join(b, d))`: the comma pairs what the `JOIN` built |
//! | `FROM a CROSS JOIN b` | `Join(a, b)` of kind `Cross`, without an `on` |
//!
//! The five kinds are carried over one for one, a written `RIGHT` staying `Right`:
//! rewriting it is the planner's decision, not this file's. The `ON` of a join sees the
//! sources of **that** join and of the ones nested under it, not the ones written before a
//! comma: `FROM a, b JOIN d ON a.k = d.k` answers 4104 on `a.k`, and
//! `FROM a JOIN b ON a.k = d.k JOIN d ON …` answers 4104 on `d.k` (`a_join_sees_its_own_sides`).
//!
//! # The schema of a join
//!
//! The columns of the left input then those of the right, as [`LogicalPlan::Join`] states,
//! a column of the right input indexing that concatenation at its own index plus the width
//! of the left input. The side an outer join may pad with `NULL` becomes nullable: the
//! right one of a `LEFT`, the left one of a `RIGHT`, both of a `FULL`; an `INNER` and a
//! `CROSS` change nothing. That nullability is what the `SELECT b.k FROM a LEFT JOIN b …`
//! projection carries (`left_join_makes_the_right_side_nullable`).
//!
//! # Two sources of one exposed name
//!
//! A source is referred to by its alias when one was written, by the object part of its
//! name otherwise (`names::alias_of`); two sources of one `FROM` may not share that name,
//! compared ASCII case-insensitively, whichever database or schema they come from. The
//! number depends on which of the two names is an alias, and the message prints the names
//! as they were written, delimiters removed:
//!
//! | written | answer |
//! |---|---|
//! | `FROM dbo.a JOIN dbo.a ON 1 = 1`, `FROM dbo.a, dbo.a` | 1013, printing `"dbo.a"` and `"dbo.a"` |
//! | `FROM a JOIN dbo.a ON 1 = 1` | 1013 printing `"dbo.a"` first: the source that came second |
//! | `FROM master.sys.objects JOIN sys.objects ON 1 = 0` | 1013: the exposed name is `objects` on both sides |
//! | `FROM dbo.a AS x JOIN dbo.b AS X ON 1 = 1`, `FROM dbo.a AS x, dbo.b AS x` | 1011 printing `'X'`: the alias that came second |
//! | `FROM dbo.a AS b JOIN dbo.b ON 1 = 1`, `FROM dbo.b JOIN dbo.a AS b ON 1 = 1` | 1012 printing `'b'` and `'dbo.b'` |
//!
//! Each source is resolved first, so a name that reaches nothing answers 208 before the
//! comparison, and the `ON` of the join is bound after it:
//!
//! | written | answer |
//! |---|---|
//! | `FROM nosuch JOIN nosuch ON 1 = 1` | 208 |
//! | `FROM dbo.a JOIN dbo.a ON x.k = 1` | 1013, before the 4104 of the `ON` |
//! | `FROM dbo.a JOIN dbo.b ON k = 1 JOIN dbo.a ON 1 = 1` | 209: the first `ON` is bound before the third source is compared |
//!
//! The three numbers carry the line their statement starts on, like 208.
//! `FROM dbo.a JOIN dbo.a ON 1` answers 1013 here and 4145 on SQL Server, where a
//! non-boolean condition is refused while the text is read, before the names are
//! resolved; `query.rs` already answers 208 to `FROM nosuch WHERE 1` for the same reason.
//!
//! # The hint words of a source
//!
//! A source written with a hint list, `FROM a (NOLOCK) JOIN b WITH (NOLOCK) ON …`, is
//! bound by the same `names::bind_from` a single-table `FROM` goes through: `hints.rs`
//! reads the words there, and each `Scan` carries its own
//! [`LockHints`](crate::LockHints) (`a_hint_word_on_either_side_binds`).

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Expr, JoinKind as WrittenJoinKind, TableRef};

use crate::bound::{JoinKind, LogicalPlan, OutputColumn, OutputSchema};
use crate::context::BindContext;
use crate::expr::{Scope, bind_condition};
use crate::names::{self, alias_of};
use crate::query::{bug, dotted};
use crate::star::Source;
use crate::view;

/// A bound reference of the `FROM`: its plan, and the sources it puts in scope, in
/// written order, with what each was written as for the messages of 1011 to 1013.
struct Bound {
    /// The plan that reads the reference: a `Scan`, the plan of an expanded view, or a
    /// `Join` of those.
    plan: LogicalPlan,
    /// The sources the reference puts in scope, one per leaf, in written order.
    sources: Vec<Source>,
    /// How each leaf was written, in the order of `sources`.
    written: Vec<Written>,
}

/// How a leaf of the `FROM` was written, for the message of a duplicate exposed name.
struct Written {
    /// The name of the table as written, delimiters removed, dots kept.
    name: String,
    /// Whether an alias was written, in which case the exposed name of the source is that
    /// alias.
    aliased: bool,
}

/// Binds a `FROM` of more than one source, or one written with a `JOIN`, into the tree of
/// [`LogicalPlan::Join`] nodes that reads it, and the scope its select list and its
/// `WHERE` bind in.
///
/// `statement_line` is the line the statement starts on, which is the one a 208 and a
/// duplicate exposed name (1011 to 1013) carry. The single-source `FROM` does not come
/// here: `query.rs` hands it to `names::bind_from`.
///
/// # Errors
///
/// - 208 for a name that reaches nothing, and the errors `names::bind_from` raises for a
///   reference it does not bind yet;
/// - 1011, 1012 or 1013 for two sources of one exposed name (module documentation);
/// - the errors of the `ON`: 4145 for a value, 4104 for a prefix naming no source of the
///   join, 209 for a bare name two of its sources carry, 207 for an unknown column.
pub(crate) fn bind_from(
    from: &[TableRef],
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<(LogicalPlan, Scope)> {
    let mut bound: Option<Bound> = None;
    for reference in from {
        let next = bind_reference(reference, statement_line, ctx)?;
        bound = Some(match bound {
            None => next,
            // The comma pairs each row of what came before with each row of the next
            // reference: the same node as a `CROSS JOIN`, without an `ON`.
            Some(left) => join(left, next, JoinKind::Cross, None, statement_line, ctx)?,
        });
    }
    let Bound { plan, sources, .. } =
        bound.ok_or_else(|| bug("bind_from: a FROM without a reference"))?;
    Ok((plan, Scope::over_all(sources)))
}

/// Binds one reference of the `FROM`: a leaf through `names::bind_from`, a `JOIN` by
/// binding its two sides and pairing them.
fn bind_reference(
    reference: &TableRef,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<Bound> {
    match reference {
        TableRef::Join {
            left,
            right,
            kind,
            on,
            ..
        } => {
            let left = bind_reference(left, statement_line, ctx)?;
            let right = bind_reference(right, statement_line, ctx)?;
            join(
                left,
                right,
                kind_of(*kind),
                on.as_ref(),
                statement_line,
                ctx,
            )
        }
        leaf => {
            let plan = names::bind_from(leaf, statement_line, ctx)?;
            let source = leaf_source(&plan, leaf, ctx)?;
            let written = match leaf {
                TableRef::Table { name, alias, .. } | TableRef::Function { name, alias, .. } => {
                    Written {
                        name: dotted(name),
                        aliased: alias.is_some(),
                    }
                }
                _ => Written {
                    name: String::new(),
                    aliased: false,
                },
            };
            Ok(Bound {
                plan,
                sources: vec![source],
                written: vec![written],
            })
        }
    }
}

/// The kind of the bound node for the kind that was written, one for one.
fn kind_of(kind: WrittenJoinKind) -> JoinKind {
    match kind {
        WrittenJoinKind::Inner => JoinKind::Inner,
        WrittenJoinKind::Left => JoinKind::Left,
        WrittenJoinKind::Right => JoinKind::Right,
        WrittenJoinKind::Full => JoinKind::Full,
        WrittenJoinKind::Cross => JoinKind::Cross,
    }
}

/// Pairs two bound references into a [`LogicalPlan::Join`] of kind `kind`, binding `on`
/// in the scope of both.
///
/// The exposed names are compared before the `ON` is bound, and the sources of the right
/// side are moved past the width of the left input before the scope is built, so that a
/// column reference bound in the `ON` indexes the row of the join
/// (`a_column_of_the_right_side_indexes_past_the_left_one`).
fn join(
    left: Bound,
    right: Bound,
    kind: JoinKind,
    on: Option<&Expr>,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<Bound> {
    check_exposed_names(&left, &right, statement_line)?;
    let (left_padded, right_padded) = match kind {
        JoinKind::Inner | JoinKind::Cross => (false, false),
        JoinKind::Left => (false, true),
        JoinKind::Right => (true, false),
        JoinKind::Full => (true, true),
    };
    let offset = left.plan.schema().columns.len();
    let mut sources: Vec<Source> = left
        .sources
        .into_iter()
        .map(|source| nullable_if(source, left_padded))
        .collect();
    sources.extend(
        right
            .sources
            .into_iter()
            .map(|source| nullable_if(source.shifted_by(offset), right_padded)),
    );
    let schema = OutputSchema {
        columns: padded_columns(left.plan.schema(), left_padded)
            .chain(padded_columns(right.plan.schema(), right_padded))
            .collect(),
    };
    let scope = Scope::over_all(sources.clone());
    let on = match (kind, on) {
        (JoinKind::Cross, None) => None,
        (JoinKind::Cross, Some(_)) => {
            return Err(bug(
                "bind_from: a CROSS JOIN with an ON, which the parser refuses",
            ));
        }
        (_, None) => {
            return Err(bug(
                "bind_from: a JOIN without an ON, which the parser refuses",
            ));
        }
        (_, Some(condition)) => Some(bind_condition(condition, ctx, &scope)?),
    };
    let mut written = left.written;
    written.extend(right.written);
    Ok(Bound {
        plan: LogicalPlan::Join {
            left: Box::new(left.plan),
            right: Box::new(right.plan),
            kind,
            on,
            schema,
        },
        sources,
        written,
    })
}

/// Raises 1011, 1012 or 1013 when a source of `right` exposes the name of a source of
/// `left`: 1011 when both names are aliases, 1012 when one of them is, 1013 when neither.
///
/// The sources of `right` are walked in written order, each compared with the sources of
/// `left` in written order; 1011 prints the alias of the source of `right`, 1012 the
/// alias then the table, 1013 the source of `right` then the one of `left` (module
/// documentation). The line is the statement's.
fn check_exposed_names(left: &Bound, right: &Bound, statement_line: u32) -> SqlResult<()> {
    for (later, later_written) in right.sources.iter().zip(&right.written) {
        let clash = left.sources.iter().zip(&left.written).find(|(earlier, _)| {
            earlier
                .exposed_name()
                .eq_ignore_ascii_case(later.exposed_name())
        });
        let Some((earlier, earlier_written)) = clash else {
            continue;
        };
        let error = match (earlier_written.aliased, later_written.aliased) {
            (true, true) => SqlError::duplicate_correlation_name(later.exposed_name()),
            (true, false) => SqlError::correlation_name_is_a_table_name(
                earlier.exposed_name(),
                &later_written.name,
            ),
            (false, true) => SqlError::correlation_name_is_a_table_name(
                later.exposed_name(),
                &earlier_written.name,
            ),
            (false, false) => {
                SqlError::same_exposed_names(&later_written.name, &earlier_written.name)
            }
        };
        return Err(error.with_line(statement_line));
    }
    Ok(())
}

/// `source` made nullable when `padded`, unchanged otherwise.
fn nullable_if(source: Source, padded: bool) -> Source {
    if padded {
        source.made_nullable()
    } else {
        source
    }
}

/// The columns of `schema`, made nullable when `padded`.
fn padded_columns(schema: &OutputSchema, padded: bool) -> impl Iterator<Item = OutputColumn> {
    schema.columns.iter().map(move |column| {
        let mut column = column.clone();
        column.ty.nullable |= padded;
        column
    })
}

/// The source a leaf reference puts in scope, read off the plan `names::bind_from` built
/// for it, by the rules `query.rs` applies to a single-table `FROM`: a `Scan` publishes its
/// columns under its own `alias`, and an expanded view publishes the columns of its output
/// under the alias read off the reference (`view::source_columns`).
///
/// # Errors
///
/// An internal error 50000 for a reference that is neither a table nor a `nom(…)`:
/// `names::bind_from` refuses those before this point.
fn leaf_source(
    plan: &LogicalPlan,
    reference: &TableRef,
    ctx: &BindContext<'_>,
) -> SqlResult<Source> {
    let (name, alias) = match reference {
        TableRef::Table { name, alias, .. } | TableRef::Function { name, alias, .. } => {
            (name, alias.as_ref())
        }
        _ => return Err(bug("bind_from: a reference bound without a name to expose")),
    };
    // An alias hides the name of the table: a two- or three-part qualifier then matches
    // nothing (`star.rs`, `an_alias_hides_the_name_of_the_source`).
    let written = alias.is_none().then_some(name);
    Ok(match plan {
        LogicalPlan::Scan { columns, alias, .. } => {
            Source::new(alias, written, columns, ctx.default_schema, ctx.database)
        }
        expanded => Source::new(
            &alias_of(name, alias),
            written,
            &view::source_columns(expanded.schema()),
            ctx.default_schema,
            ctx.database,
        ),
    })
}
