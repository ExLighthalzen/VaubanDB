//! `ORDER BY`, `SELECT DISTINCT` and what `TOP` gains once a query is ordered — `WITH TIES`
//! and `PERCENT`: the two entry points below, the [`LogicalPlan::Sort`] and
//! [`LogicalPlan::Distinct`] variants and the [`SortKey`](crate::bound::SortKey) they hold.
//!
//! `TOP` alone does not come here: `query.rs::bind_top` binds it into a
//! [`LogicalPlan::Limit`], and `TOP … WITH TIES` needs an `ORDER BY` (1062), so it reaches
//! this file through [`bind_order_by`].
//!
//! # Where the `Sort` sits
//!
//! `query.rs::finish` hands this file a plan whose `Limit` already sits over the `Distinct`.
//! Two stacks come out of it, and the rule that picks one is: **the `Sort` goes directly
//! over the operator that still produces the values of its keys, and under the `Limit`**,
//! because `TOP` counts the rows of the ordered result.
//!
//! ```text
//! SELECT TOP 2 a FROM dbo.t ORDER BY b           Limit → Project → Sort → Filter → Scan
//! SELECT DISTINCT TOP 2 c FROM dbo.t ORDER BY c  Limit → Sort → Distinct → Project → Scan
//! ```
//!
//! - **Without `DISTINCT`**, an `ORDER BY` may name a column the select list drops
//!   (`SELECT a FROM dbo.t ORDER BY b` answers its rows). That value is gone from the row a
//!   `Project`
//!   publishes, and no node of `bound/mod.rs` carries a hidden column, so the `Sort` goes
//!   **under** the `Project`, with keys bound in the scope of the `FROM`. A `Project` keeps
//!   one row per input row, so ordering under it orders the result.
//! - **With `DISTINCT`**, error 145 refuses such a key: each one is then a projected column,
//!   and the `Sort` goes **over** the `Distinct`, so that the ordered result does not hang on
//!   the order the deduplication happens to produce. A key is a
//!   [`BoundExprKind::ColumnRef`] on its position in the row the `Distinct` publishes — the
//!   way `bound/mod.rs` already has a `Project` designate a column of an `Aggregate` below
//!   it.
//!
//! The form rejected: one stack with the `Sort` under the `Distinct` in both cases. It
//! answers the same rows as this one when the deduplication keeps the order of its input,
//! and that is a contract on a node of the planner and the executor. The two stacks above
//! hold on their own. `tests/bind_sort.rs`, `the_plan_shape_is_the_documented_one`, pins
//! both.
//!
//! ## What the shape changes in the answer
//!
//! Under the `Project`, a key written as an alias is the expression of the select item
//! **bound a second time**: the plan holds two nodes for it, the one it sorts on and the
//! one it publishes. A key that does not give the same value at each evaluation therefore
//! comes out sorted on one draw and published on another.
//! `SELECT CAST(NEWID() AS varchar(36)) AS x FROM dbo.t ORDER BY x` answers its rows in
//! ascending order of the **published** value on SQL Server, an order this stack does not
//! give that form. The two nodes are pinned by `tests/bind_sort.rs`,
//! `a_key_written_as_an_alias_is_bound_a_second_time`; executing them is the executor's,
//! and the note is here because the shape is what causes it.
//!
//! # What a key is written as
//!
//! What SQL Server answers:
//!
//! | written | what it means |
//! |---|---|
//! | `ORDER BY 2` | the 2nd output column, 1-based, a `SELECT *` counting its expanded columns |
//! | `ORDER BY +2`, `(2)`, `((2))`, `+(2)`, `(+2)` | the 2nd output column too: a sign and parentheses around an integer literal leave a position |
//! | `ORDER BY 5` over two columns | **108** (the position is out of range), and so do `0`, `-1`, `+5`, `(5)`, `-(1)` and `(-1)`, the message naming the value read (`5`, `-1`) |
//! | `ORDER BY 2147483647` | **108**, where `2147483648`, `-2147483649` and `99999999999999999999` answer **408**: a position stops at what `int` holds |
//! | `ORDER BY (1 + 1)`, `(2.0)` | **408**: what the parentheses hold is an integer literal and its sign, not an expression |
//! | `ORDER BY 1 COLLATE …` | **447** (`int` is invalid for `COLLATE`), over a `varchar` item, over a position out of range and with a collation name the server does not know (where 447 comes before 448): the position is not read under a `COLLATE` |
//! | `ORDER BY c COLLATE X, c` | the rows, where `ORDER BY c COLLATE X, c COLLATE X` answers **169**: the collation is part of what makes two keys the same |
//! | `ORDER BY x` | the output column named `x`, an alias winning over a column of the table of that name (`SELECT a AS b FROM … ORDER BY b` orders by `a`) |
//! | `ORDER BY x` matching two output columns | **209** (ambiguous column name) |
//! | `ORDER BY x + 1`, `ORDER BY x COLLATE …` | `x` is looked up in the `FROM`, not in the select list: **207** when the table has no such column |
//! | `ORDER BY 'abc'`, `NULL`, `2.0`, `CAST(1 AS int)`, `LEN('abc')`, a `CASE` of constants | **408** (a constant expression at position n) |
//! | `ORDER BY @@SPID`, `GETDATE()`, `LEN(@@VERSION)`, `1 + a` | the four order their rows (`tests/bind_sort.rs`, `a_constant_key_is_408`) |
//! | `ORDER BY a, a`, `1, a`, `a, dbo.t.a`, `a + 1, a + 1` | **169** (a column specified more than once), where `a, a + 0` passes |
//! | `SELECT DISTINCT a … ORDER BY b` | **145**, where `ORDER BY dbo.t.a` passes and `ORDER BY 1 + a` against a select list holding `a + 1` does not |
//! | `SELECT DISTINCT c … ORDER BY c COLLATE X` | **145**, where the same key over `SELECT DISTINCT c COLLATE X` answers the rows and over `SELECT DISTINCT c COLLATE Y` answers 145 |
//! | `SELECT DISTINCT 1 ORDER BY @@SPID` | **145** without a `FROM`, where `ORDER BY 1` and `ORDER BY x` over `SELECT DISTINCT 1 AS x` answer their row |
//!
//! Two orders between those numbers hold too, and this file follows them: a key that does
//! not resolve raises its own error before 145 (`SELECT DISTINCT a … ORDER BY nosuch`
//! answers 207, `… ORDER BY 5` answers 108), and 169 comes before 145
//! (`SELECT DISTINCT a … ORDER BY b, b` answers 169).
//!
//! 145 hangs on the word written in the statement, not on the [`LogicalPlan::Distinct`]
//! node: `query.rs::finish` puts none on a `SELECT DISTINCT` without a `FROM`, over a
//! single row it would remove nothing, and `SELECT DISTINCT 1 ORDER BY @@SPID` answers 145
//! nonetheless. This file reads `QuerySpec::distinct`.
//!
//! Error 1008, which `ORDER BY @v` answers over a declared variable, is not raised here: a
//! variable needs a real `VariableScope`, which `variables.rs` does not provide yet.

use std::collections::HashMap;

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    Expr, Literal, ObjectName, OrderItem, QueryBody, QuerySpec, SelectStatement, Span, TableRef,
    UnaryOp,
};
use vauban_types::{Collation, TypeInfo};

use crate::bound::{
    BoundExpr, BoundExprKind, BoundProjection, BoundTop, ColumnBinding, LogicalPlan, OutputSchema,
    SortKey,
};
use crate::context::BindContext;
use crate::errors::line_of;
use crate::expr::{Scope, bind_expr};
use crate::names::alias_of;
use crate::query::{bug, not_implemented};
use crate::star::Source;
use crate::view;

/// Binds the `ORDER BY` of a statement into a [`LogicalPlan::Sort`] over `input`.
///
/// `input` is the plan `query.rs::bind_select` built for the body of the statement, `TOP`
/// and `DISTINCT` included: this function takes that stack apart and puts it back with the
/// `Sort` at the place the module documentation fixes.
///
/// # Errors
///
/// - **108** for a position outside the select list, **209** for a name that matches two
///   output columns, **408** for a constant key, **169** for two keys that designate the
///   same column, **145** for a key outside the select list of a `SELECT DISTINCT`, and the
///   447 or 448 of a `COLLATE` written on a key;
/// - the errors binding a key raises, 207 and 4104 among them;
/// - an internal error 50000 naming the form, for an `ORDER BY` over a set operator, over
///   a grouped query (where SQL Server answers 8127), over a join or over a derived table.
pub(crate) fn bind_order_by(
    stmt: &SelectStatement,
    input: LogicalPlan,
    ctx: &BindContext<'_>,
    from_scope: Option<Scope>,
) -> SqlResult<LogicalPlan> {
    match &stmt.body {
        QueryBody::Select(spec) => bind_order_by_select(stmt, input, ctx, spec, from_scope),
        QueryBody::SetOp { .. } | QueryBody::Nested(..) => bind_order_by_set_op(stmt, input, ctx),
    }
}

/// Binds the ORDER BY of a plain `SELECT`.
fn bind_order_by_select(
    stmt: &SelectStatement,
    input: LogicalPlan,
    ctx: &BindContext<'_>,
    spec: &QuerySpec,
    from_scope: Option<Scope>,
) -> SqlResult<LogicalPlan> {
    let (body, top) = strip_limit(input);
    let (deduplicated, projected) = match body {
        LogicalPlan::Distinct(inner) => (true, *inner),
        other => (false, other),
    };
    let LogicalPlan::Project {
        input: source,
        exprs,
        schema,
    } = projected
    else {
        return Err(bug(format!(
            "sort::bind_order_by: the body of an ORDER BY is not a projection: {projected:?}"
        )));
    };

    let scope = match from_scope {
        Some(scope) => scope,
        None => scope_of(&source, spec, ctx)?,
    };
    if grouped_source(&source) {
        return Err(not_implemented("the ORDER BY of a grouped query"));
    }
    let mut keys = Vec::with_capacity(stmt.order_by.len());
    for (position, item) in stmt.order_by.iter().enumerate() {
        keys.push(resolve_key(item, position, &exprs, &schema, &scope, ctx)?);
    }
    refuse_a_repeated_key(&keys)?;
    // 145 is raised on the word of the statement and not on the `Distinct` of the plan:
    // `SELECT DISTINCT 1 ORDER BY @@SPID` has no node, and answers 145.
    let positions = if spec.distinct {
        select_list_positions(&keys, &exprs)?
    } else {
        Vec::new()
    };

    let plan = if deduplicated {
        let keys = project_the_keys(keys, &positions, &schema)?;
        LogicalPlan::Sort {
            input: Box::new(LogicalPlan::Distinct(Box::new(LogicalPlan::Project {
                input: source,
                exprs,
                schema,
            }))),
            keys,
        }
    } else {
        let keys = keys.into_iter().map(ResolvedKey::into_sort_key).collect();
        LogicalPlan::Project {
            input: Box::new(LogicalPlan::Sort {
                input: source,
                keys,
            }),
            exprs,
            schema,
        }
    };
    Ok(put_the_limit_back(plan, top))
}

/// Binds the ORDER BY of a set operator: `UNION … ORDER BY a`.
///
/// The Sort goes directly above the `SetOp`: the keys resolve against the output
/// schema of the set operation, by ordinal or by name, with no `FROM` scope.
/// The ORDER BY items are resolved the same way as for a plain SELECT: an integer
/// literal is a position (108 out of range), a bare name matches an output column,
/// and anything else is an expression that is refused by 408 if it is constant.
fn bind_order_by_set_op(
    stmt: &SelectStatement,
    input: LogicalPlan,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let LogicalPlan::SetOp { schema, .. } = &input else {
        return Err(bug(format!(
            "sort::bind_order_by_set_op: the input is not a SetOp: {input:?}"
        )));
    };
    // The projections match the output columns one-to-one.
    let exprs: Vec<BoundProjection> = schema
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| BoundProjection {
            expr: BoundExpr {
                kind: BoundExprKind::ColumnRef(ColumnBinding {
                    column: vauban_catalog::ColumnId(i as i32),
                    index: i,
                    name: col.name.clone(),
                    ty: col.ty.clone(),
                }),
                ty: col.ty.clone(),
                line: 1,
            },
            name: col.name.clone(),
        })
        .collect();

    let mut keys = Vec::with_capacity(stmt.order_by.len());
    for (position, item) in stmt.order_by.iter().enumerate() {
        keys.push(resolve_key(
            item,
            position,
            &exprs,
            schema,
            &Scope::empty(),
            ctx,
        )?);
    }
    refuse_a_repeated_key(&keys)?;

    let keys = keys.into_iter().map(ResolvedKey::into_sort_key).collect();
    Ok(LogicalPlan::Sort {
        input: Box::new(input),
        keys,
    })
}

/// Binds a `SELECT DISTINCT` over a `FROM` into a [`LogicalPlan::Distinct`] over `input`.
///
/// Without a `FROM` the word removes nothing over the single row of
/// [`LogicalPlan::OneRow`], and `query.rs` binds the statement without coming here.
pub(crate) fn bind_distinct(input: LogicalPlan, _ctx: &BindContext<'_>) -> SqlResult<LogicalPlan> {
    Ok(LogicalPlan::Distinct(Box::new(input)))
}

/// Error 1033: an `ORDER BY` written in a derived table, a subquery or a view with no
/// `TOP`, `OFFSET` or `FOR XML` to make it meaningful.
///
/// SQL Server answers that number, severity 15 and state 1 on three shapes: a derived
/// table (`SELECT d.a FROM (SELECT a FROM dbo.t ORDER BY a) AS d`), an
/// `IN (SELECT … ORDER BY …)` and a scalar subquery. The derived table with a `TOP` answers
/// its row instead. The constructor lives next to the clause it is about; the site that
/// knows a query is nested is `subquery.rs` or `setop.rs`.
// Nothing in the crate binds a nested query yet, so the constructor has no caller.
#[allow(dead_code, reason = "no nested query is bound yet")]
pub(crate) fn order_by_invalid_in_a_subquery() -> SqlError {
    SqlError::new(
        1033,
        15,
        1,
        "ORDER BY is not allowed in a view, a derived table or a subquery without TOP, \
         OFFSET or FOR XML.",
    )
}

/// A key, once its position or its name has been resolved and its expression bound.
struct ResolvedKey {
    /// The expression to order by, bound in the scope of the `FROM`.
    expr: BoundExpr,
    /// True for `DESC`.
    desc: bool,
    /// The collation of a `COLLATE` written on the key.
    collation: Option<Collation>,
    /// The 0-based output column the key was written as a position or as a name of, `None`
    /// for a key written as an expression.
    item: Option<usize>,
    /// What two keys of the same column share ([`fingerprint`]).
    print: String,
}

impl ResolvedKey {
    /// The key of a `Sort` placed under the `Project`, which keeps the expression as bound.
    fn into_sort_key(self) -> SortKey {
        SortKey {
            expr: self.expr,
            desc: self.desc,
            collation: self.collation,
        }
    }
}

/// Resolves one `ORDER BY` item into a key bound in the scope of the `FROM`.
///
/// `position` is 0-based and serves the message of 408, which counts from 1.
fn resolve_key(
    item: &OrderItem,
    position: usize,
    exprs: &[BoundProjection],
    schema: &OutputSchema,
    scope: &Scope,
    ctx: &BindContext<'_>,
) -> SqlResult<ResolvedKey> {
    let written = &item.expr;
    let written_collation = item.collate.as_deref();
    // Neither lookup happens under a `COLLATE`: the clause sends the item to the `FROM` as
    // an expression of its own, which is how `SELECT c FROM … ORDER BY 1 COLLATE
    // Latin1_General_CI_AS` answers 447 over an `int` where the 1st item is a `varchar`,
    // and how `ORDER BY x COLLATE …` answers 207.
    //
    // Without one, a position is read before anything is bound: `SELECT 1 AS x, a FROM …
    // ORDER BY 1` orders by a constant item without the 408 below.
    let resolved = if written_collation.is_some() {
        None
    } else if let Some((ordinal, span)) = ordinal_of(written) {
        let index = usize::try_from(ordinal)
            .ok()
            .filter(|index| (1..=schema.columns.len()).contains(index))
            .ok_or_else(|| position_out_of_range(ordinal, line_of(&span)))?;
        Some(index - 1)
    } else {
        named_output_column(written, schema)?
    };

    let (expr, index) = match resolved {
        Some(index) => {
            let projection = exprs.get(index).ok_or_else(|| {
                bug(format!(
                    "sort::resolve_key: no projection {index} to order by"
                ))
            })?;
            (projection.expr.clone(), Some(index))
        }
        None => (bind_expr(written, ctx, scope)?, None),
    };
    // The `COLLATE` is applied before 408 is raised: `ORDER BY 2.0 COLLATE
    // Latin1_General_CI_AS` answers 447 where `ORDER BY 'abc' COLLATE Latin1_General_CI_AS`
    // answers 408.
    let expr = collate(expr, written_collation)?;
    if index.is_none() && is_constant(written) {
        return Err(constant_in_the_order_by_list(position + 1).with_line(expr.line));
    }
    let print = fingerprint(&expr);
    Ok(ResolvedKey {
        expr,
        desc: item.desc,
        collation: collation_of(item.collate.as_deref())?,
        item: index,
        print,
    })
}

/// The position an `ORDER BY` item was written as, with the span 108 is reported on;
/// `None` for an item written as anything else than an integer literal under its signs and
/// its parentheses.
///
/// A sign is part of the position: `ORDER BY -1` answers 108 naming `-1` rather than a
/// syntax error, and `ORDER BY +2` orders by the 2nd item. Parentheses leave it a position
/// too: `(2)`, `((2))`, `+(2)` and `(+2)` order by the 2nd item and `(5)`, `-(1)` and
/// `(-1)` answer 108.
///
/// The position is an `int`: `ORDER BY 2147483647` answers 108 naming it, where
/// `2147483648` and `-2147483649` answer **408**. What is not a position falls through to
/// [`is_constant`], a decimal (`2.0`) and a sum of literals (`(1 + 1)`) among them.
fn ordinal_of(written: &Expr) -> Option<(i32, Span)> {
    let span = match written {
        Expr::Literal(_, span) | Expr::Nested(_, span) | Expr::Unary { span, .. } => *span,
        _ => return None,
    };
    let value = signed_integer_literal(written)?;
    i32::try_from(value).ok().map(|position| (position, span))
}

/// The value of the integer literal an expression holds under its parentheses and its
/// signs; `None` for another shape and for a literal past `bigint` (`ORDER BY
/// 99999999999999999999` answers 408).
///
/// The walk is a loop rather than a recursion, for the reason [`is_constant`] gives: the
/// depth of an expression is the client's to choose.
fn signed_integer_literal(written: &Expr) -> Option<i64> {
    let mut sign = 1i64;
    let mut node = written;
    loop {
        match node {
            Expr::Literal(Literal::Integer(text), _) => {
                return text
                    .parse::<i64>()
                    .ok()
                    .and_then(|value| value.checked_mul(sign));
            }
            Expr::Nested(inner, _) => node = inner,
            Expr::Unary {
                op: UnaryOp::Plus,
                expr,
                ..
            } => node = expr,
            Expr::Unary {
                op: UnaryOp::Minus,
                expr,
                ..
            } => {
                sign = -sign;
                node = expr;
            }
            _ => return None,
        }
    }
}

/// The 0-based output column a bare name designates, `None` when the select list publishes
/// no column of that name — the key is then an expression of the `FROM`.
///
/// The comparison is `eq_ignore_ascii_case`, as [`Source::matches`] compares a qualifier:
/// `SELECT a AS x FROM … ORDER BY X` orders by the item. [`resolve_key`] skips the lookup
/// under a `COLLATE`, so that `SELECT c AS x FROM … ORDER BY x COLLATE Latin1_General_CI_AS`
/// answers **207**, as `ORDER BY x + 1` does — an output name is read where the item is
/// that name alone.
///
/// # Errors
///
/// 209 when two output columns carry the name: `SELECT a AS x, b AS x FROM … ORDER BY x`
/// and `SELECT a AS b, b FROM … ORDER BY b` both answer it.
fn named_output_column(written: &Expr, schema: &OutputSchema) -> SqlResult<Option<usize>> {
    let Expr::Column(reference) = written else {
        return Ok(None);
    };
    if reference.qualifier.is_some() {
        return Ok(None);
    }
    let name = &reference.name.value;
    let mut found = None;
    for (index, column) in schema.columns.iter().enumerate() {
        if column.name.eq_ignore_ascii_case(name) {
            if found.is_some() {
                return Err(SqlError::ambiguous_column_name(name).with_line(reference.span.line));
            }
            found = Some(index);
        }
    }
    Ok(found)
}

/// Applies the `COLLATE` of an `ORDER BY` item to the type of its key.
///
/// The clause is written on the item and not inside the expression (`parser::OrderItem`
/// carries it), so the check `expr.rs::bind_collate` makes on a `COLLATE` node is made here
/// instead, with the same two errors: 448 for a name the server does not know, and **447**
/// for a key that is not a character string — `SELECT a FROM dbo.t ORDER BY a COLLATE
/// Latin1_General_CI_AS` answers it over an `int` column, where the same clause on a
/// `varchar` column answers its rows.
///
/// The type is read **before** the name, unlike `expr.rs::bind_collate`: `SELECT c FROM …
/// ORDER BY 1 COLLATE Not_A_Collation` answers 447 and not the 448 of an unknown
/// collation.
///
/// The key becomes a [`BoundExprKind::Collate`] node, the shape `expr.rs` gives the same
/// text written in a select list, so that the two sides of a `SELECT DISTINCT` compare: the
/// clause on one side alone answers 145.
fn collate(expr: BoundExpr, collation: Option<&str>) -> SqlResult<BoundExpr> {
    let Some(name) = collation else {
        return Ok(expr);
    };
    if !expr.ty.ty.is_string() {
        return Err(SqlError::collate_on_non_string(expr.ty.ty.error_name()).with_line(expr.line));
    }
    let collation = Collation::parse(name).map_err(|e| e.with_line(expr.line))?;
    let ty = TypeInfo {
        ty: expr.ty.ty,
        nullable: expr.ty.nullable,
        collation: Some(collation),
    };
    let line = expr.line;
    Ok(BoundExpr {
        kind: BoundExprKind::Collate {
            expr: Box::new(expr),
        },
        ty,
        line,
    })
}

/// The collation of a key, for [`SortKey::collation`](crate::bound::SortKey::collation);
/// [`collate`] has already refused the names this rejects.
fn collation_of(collation: Option<&str>) -> SqlResult<Option<Collation>> {
    collation.map(Collation::parse).transpose()
}

/// The 0-based select list position of each key of a `SELECT DISTINCT`, or **145** for a
/// key the select list does not publish.
///
/// A key written as a position or as an output name carries its own; one written as an
/// expression is looked up by [`fingerprint`] among the projections, which is what makes
/// `SELECT DISTINCT a FROM … ORDER BY dbo.t.a` pass and
/// `SELECT DISTINCT a + 1 FROM … ORDER BY 1 + a` answer 145. The fingerprint carries the
/// collation of the key, so that `SELECT DISTINCT c COLLATE X … ORDER BY c COLLATE X`
/// passes where the same key over `SELECT DISTINCT c` answers 145.
fn select_list_positions(keys: &[ResolvedKey], exprs: &[BoundProjection]) -> SqlResult<Vec<usize>> {
    let mut projected: HashMap<String, usize> = HashMap::new();
    for (index, projection) in exprs.iter().enumerate() {
        projected
            .entry(fingerprint(&projection.expr))
            .or_insert(index);
    }
    keys.iter()
        .map(|key| match key.item {
            Some(index) => Ok(index),
            None => projected.get(&key.print).copied().ok_or_else(|| {
                SqlError::order_by_item_not_in_distinct_select_list().with_line(key.expr.line)
            }),
        })
        .collect()
}

/// Turns the keys of a `SELECT DISTINCT` into references to the row the `Distinct`
/// publishes, at the positions [`select_list_positions`] gave them.
fn project_the_keys(
    keys: Vec<ResolvedKey>,
    positions: &[usize],
    schema: &OutputSchema,
) -> SqlResult<Vec<SortKey>> {
    let columns = view::source_columns(schema);
    let mut bound = Vec::with_capacity(keys.len());
    for (rank, key) in keys.into_iter().enumerate() {
        let index = *positions.get(rank).ok_or_else(|| {
            bug(format!(
                "sort::project_the_keys: no select list position for key {rank}"
            ))
        })?;
        let column = columns
            .get(index)
            .ok_or_else(|| bug(format!("sort::project_the_keys: no output column {index}")))?;
        let ty = match key.collation {
            Some(collation) => TypeInfo {
                ty: column.ty.ty,
                nullable: column.ty.nullable,
                collation: Some(collation),
            },
            None => column.ty.clone(),
        };
        bound.push(SortKey {
            expr: BoundExpr {
                kind: BoundExprKind::ColumnRef(column.clone()),
                ty,
                line: key.expr.line,
            },
            desc: key.desc,
            collation: key.collation,
        });
    }
    Ok(bound)
}

/// Raises 169 when two keys designate the same column.
///
/// `ORDER BY a, a`, `ORDER BY a, dbo.t.a`, `ORDER BY 1, a`, `ORDER BY 1, 1`,
/// `ORDER BY a + 1, a + 1`, `ORDER BY a ASC, a DESC` and, over `SELECT a AS x, b`,
/// `ORDER BY x, a` each answer the number, where `ORDER BY a, a + 0` answers the rows. The
/// direction written on a key is therefore not part of what makes two of them the same.
fn refuse_a_repeated_key(keys: &[ResolvedKey]) -> SqlResult<()> {
    let mut seen: Vec<&str> = Vec::with_capacity(keys.len());
    for key in keys {
        if seen.contains(&key.print.as_str()) {
            return Err(column_named_twice_in_the_order_by_list().with_line(key.expr.line));
        }
        seen.push(&key.print);
    }
    Ok(())
}

/// What two expressions share when SQL Server calls them the same column.
///
/// The bound shape is compared, not the text: `a` and `dbo.t.a` bind to the same
/// [`BoundExprKind::ColumnRef`] and count as one key (169), while `a + 1` and `1 + a` are
/// two (145 over a `SELECT DISTINCT a + 1`). The rendering is the `Debug` of the bound
/// expression with the `line: n` of each node blanked ([`blank_the_lines`]), so that an
/// `ORDER BY` split over two lines fingerprints as one written on a single line
/// (`tests/bind_sort.rs`, `the_same_key_twice_is_169`).
///
/// The collation of the node itself closes the rendering: it lives in the `ty` of a
/// [`BoundExprKind::Collate`] and not in its `kind`, and it separates two keys —
/// `ORDER BY c COLLATE X, c` answers its rows where `ORDER BY c COLLATE X, c COLLATE X`
/// answers 169.
fn fingerprint(expr: &BoundExpr) -> String {
    let mut print = blank_the_lines(&format!("{:?}", expr.kind));
    print.push_str(&format!(" collation: {:?}", expr.ty.collation));
    print
}

/// The `Debug` of a node with the `line: n` of each field blanked, the text of the strings
/// it holds left untouched.
///
/// A literal is rendered between double quotes by `Debug`, and the scan copies what a quote
/// opens until the quote that closes it: `ORDER BY c + 'line: 1', c + 'line: 2'` is two
/// keys and answers its rows, where the same literal on both keys answers 169.
fn blank_the_lines(rendered: &str) -> String {
    const FIELD: &str = "line: ";
    let mut out = String::with_capacity(rendered.len());
    let mut rest = rendered;
    loop {
        let quote = rest.find('"');
        let field = rest.find(FIELD);
        match (quote, field) {
            (_, Some(at)) if quote.is_none_or(|q| at < q) => {
                out.push_str(&rest[..at]);
                out.push_str("line: _");
                rest = rest[at + FIELD.len()..].trim_start_matches(|c: char| c.is_ascii_digit());
            }
            (Some(at), _) => {
                out.push_str(&rest[..=at]);
                rest = &rest[at + 1..];
                let end = end_of_string_literal(rest);
                out.push_str(&rest[..end]);
                rest = &rest[end..];
            }
            (None, _) => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

/// The byte index just past the `"` that closes a string literal `Debug` has opened, or the
/// length of `rest` when it holds no such quote. A `\"` inside the literal is part of it.
fn end_of_string_literal(rest: &str) -> usize {
    let mut escaped = false;
    for (at, c) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => return at + 1,
            _ => {}
        }
    }
    rest.len()
}

/// Whether a key is the constant expression error 408 refuses.
///
/// `'abc'`, `NULL`, `2.0`, `CAST(1 AS int)`, `LEN('abc')` and
/// `CASE WHEN 1 = 1 THEN 1 ELSE 2 END` answer 408; `@@SPID`, `GETDATE()`, `LEN(@@VERSION)`,
/// `ABS(@v)`, `@v + 0` and `1 + a` answer the rows. A column, a variable and a call written
/// without an argument are the three leaves that make a key run-time, and a form this walk
/// does not enumerate is left alone rather than refused. The walk is a worklist rather than
/// a recursion, for the reason `query::holds_an_aggregate` gives: the depth of an
/// expression is the client's to choose.
fn is_constant(written: &Expr) -> bool {
    let mut pending = vec![written];
    while let Some(expr) = pending.pop() {
        match expr {
            Expr::Literal(..) => {}
            Expr::Nested(inner, _)
            | Expr::Unary { expr: inner, .. }
            | Expr::Cast { expr: inner, .. }
            | Expr::IsNull { expr: inner, .. }
            | Expr::Collate { expr: inner, .. } => pending.push(inner),
            Expr::Convert {
                expr: inner, style, ..
            } => {
                pending.push(inner);
                pending.extend(style.iter().map(AsRef::as_ref));
            }
            Expr::Binary { left, right, .. } => {
                pending.push(left);
                pending.push(right);
            }
            Expr::Function { args, .. } if !args.is_empty() => pending.extend(args),
            Expr::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                pending.extend(operand.iter().map(AsRef::as_ref));
                pending.extend(else_.iter().map(AsRef::as_ref));
                for arm in arms {
                    pending.push(&arm.when);
                    pending.push(&arm.then);
                }
            }
            _ => return false,
        }
    }
    true
}

/// Whether the plan under a `Project` is a grouped query.
fn grouped_source(source: &LogicalPlan) -> bool {
    let mut node = source;
    while let LogicalPlan::Filter { input, .. } = node {
        node = input;
    }
    matches!(node, LogicalPlan::Aggregate { .. })
}

/// The scope the keys of an `ORDER BY` bind in when none was handed over from
/// `query.rs::bind_query_spec`.
///
/// A `Scan` publishes its columns under its own `alias`, and an expanded view publishes
/// the columns of its output under the alias read off the reference (`view::source_columns`).
fn scope_of(source: &LogicalPlan, spec: &QuerySpec, ctx: &BindContext<'_>) -> SqlResult<Scope> {
    let mut node = source;
    while let LogicalPlan::Filter { input, .. } = node {
        node = input;
    }
    let reference = spec.from.first();
    match (node, reference) {
        (LogicalPlan::OneRow, _) | (_, None) => Ok(Scope::empty()),
        (LogicalPlan::Scan { columns, alias, .. }, Some(reference)) => {
            Ok(Scope::over(Source::new(
                alias,
                written_name(reference),
                columns,
                ctx.default_schema,
                ctx.database,
            )))
        }
        (LogicalPlan::Aggregate { .. }, _) => {
            Err(not_implemented("the ORDER BY of a grouped query"))
        }
        (expanded, Some(reference)) => match reference_alias(reference) {
            Some(alias) => Ok(Scope::over(Source::new(
                &alias,
                written_name(reference),
                &view::source_columns(expanded.schema()),
                ctx.default_schema,
                ctx.database,
            ))),
            None => Ok(Scope::empty()),
        },
    }
}

/// The name of the table a reference was written with, when no alias hides it.
///
/// Same rule as `query.rs`, whose copy binds the select list.
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

/// The name the rest of the query refers to an expanded view by, `None` for a reference
/// that puts no source in scope. Same rule as `query.rs`.
fn reference_alias(reference: &TableRef) -> Option<String> {
    match reference {
        TableRef::Table { name, alias, .. } | TableRef::Function { name, alias, .. } => {
            Some(alias_of(name, alias.as_ref()))
        }
        _ => None,
    }
}

/// Takes the `Limit` of a `TOP` off the plan, so that the `Sort` can go under it.
fn strip_limit(plan: LogicalPlan) -> (LogicalPlan, Option<BoundTop>) {
    match plan {
        LogicalPlan::Limit { input, top } => (*input, Some(top)),
        other => (other, None),
    }
}

/// Puts back the `Limit` [`strip_limit`] took off.
fn put_the_limit_back(plan: LogicalPlan, top: Option<BoundTop>) -> LogicalPlan {
    match top {
        Some(top) => LogicalPlan::Limit {
            input: Box::new(plan),
            top,
        },
        None => plan,
    }
}

/// Error 108, severity 16, state 1: an `ORDER BY` position outside the select list.
///
/// `errors` has no entry for the number, so the error is built here, as `parser::expr.rs`
/// builds its 125: `SELECT a, b FROM dbo.t ORDER BY 5` answers 108, and `ORDER BY 0` and
/// `ORDER BY -1` answer the same number with the position as written. The value read is
/// what the message names, signs and parentheses folded: `ORDER BY -(1)` and
/// `ORDER BY (-1)` both name `-1`.
fn position_out_of_range(position: i32, line: u32) -> SqlError {
    SqlError::new(
        108,
        16,
        1,
        format!("ORDER BY position {position} is outside the select list."),
    )
    .with_line(line)
}

/// Error 408, severity 16, state 1: a constant key. `position` counts from 1.
///
/// Built here for the reason [`position_out_of_range`] gives. `SELECT a FROM dbo.t
/// ORDER BY a, 'abc'` answers 408 naming position 2, which is where the number in the
/// message comes from.
fn constant_in_the_order_by_list(position: usize) -> SqlError {
    SqlError::new(
        408,
        16,
        1,
        format!("ORDER BY item at position {position} is a constant expression."),
    )
}

/// Error 169, severity 15, state 1: the same column written twice in the `ORDER BY`.
///
/// Built here for the reason [`position_out_of_range`] gives: `SELECT a, b FROM dbo.t
/// ORDER BY a, a` answers it.
fn column_named_twice_in_the_order_by_list() -> SqlError {
    SqlError::new(
        169,
        15,
        1,
        "A column appears more than once in the ORDER BY list.",
    )
}

#[cfg(test)]
mod tests {
    use super::{fingerprint, is_constant, order_by_invalid_in_a_subquery};
    use crate::bound::{BoundExpr, BoundExprKind};
    use vauban_parser::{Expr, ParseOptions, Statement, parse_batch};
    use vauban_types::{SqlType, TypeInfo, Value};

    /// The first `ORDER BY` key of a statement, as the parser produces it.
    fn key(text: &str) -> Expr {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let Statement::Select(select) = &batch.statements[0] else {
            unreachable!("{text} is a SELECT")
        };
        select.order_by[0].expr.clone()
    }

    /// The constant walk answers on the seven forms SQL Server refuses with 408 and on the
    /// eight it accepts (module documentation).
    #[test]
    fn a_constant_key_is_the_one_408_names() {
        for text in [
            "SELECT a FROM dbo.t ORDER BY 'abc'",
            "SELECT a FROM dbo.t ORDER BY NULL",
            "SELECT a FROM dbo.t ORDER BY 2.0",
            "SELECT a FROM dbo.t ORDER BY CAST(1 AS int)",
            "SELECT a FROM dbo.t ORDER BY LEN('abc')",
            "SELECT a FROM dbo.t ORDER BY CASE WHEN 1 = 1 THEN 1 ELSE 2 END",
            "SELECT a FROM dbo.t ORDER BY (1 + 1)",
        ] {
            assert!(is_constant(&key(text)), "{text}");
        }
        for text in [
            "SELECT a FROM dbo.t ORDER BY @@SPID",
            "SELECT a FROM dbo.t ORDER BY GETDATE()",
            "SELECT a FROM dbo.t ORDER BY LEN(@@VERSION)",
            "SELECT a FROM dbo.t ORDER BY ABS(@v)",
            "SELECT a FROM dbo.t ORDER BY @v + 0",
            "SELECT a FROM dbo.t ORDER BY 1 + a",
            "SELECT a FROM dbo.t ORDER BY a",
            "SELECT a FROM dbo.t ORDER BY CASE WHEN a = 1 THEN 1 ELSE 2 END",
        ] {
            assert!(!is_constant(&key(text)), "{text}");
        }
    }

    /// Two expressions of the same shape written on two lines share their fingerprint, and
    /// two of different shapes do not.
    #[test]
    fn a_fingerprint_ignores_the_line_a_node_was_written_on() {
        let literal = |line: u32| BoundExpr {
            kind: BoundExprKind::Literal(Value::I32(1)),
            ty: TypeInfo::new(SqlType::Int, false),
            line,
        };
        let negated = |line: u32| BoundExpr {
            kind: BoundExprKind::Negate(Box::new(literal(line))),
            ty: TypeInfo::new(SqlType::Int, false),
            line,
        };
        assert_eq!(fingerprint(&negated(1)), fingerprint(&negated(9)));
        assert_ne!(fingerprint(&negated(1)), fingerprint(&literal(1)));
        assert!(
            !fingerprint(&negated(1)).contains("line: 1"),
            "the line is blanked"
        );
    }

    /// The 1033 a nested query raises carries the number, the severity and the state SQL
    /// Server gives it.
    #[test]
    fn an_order_by_in_a_subquery_is_1033() {
        let error = order_by_invalid_in_a_subquery();
        assert_eq!((error.number, error.severity, error.state), (1033, 15, 1));
        assert_eq!(
            error.message,
            "ORDER BY is not allowed in a view, a derived table or a subquery without TOP, \
             OFFSET or FOR XML."
        );
    }
}
