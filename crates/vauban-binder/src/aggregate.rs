//! `GROUP BY`, `HAVING` and the aggregate calls of a select list: the
//! [`LogicalPlan::Aggregate`] node, the [`AggregateCall`]s it computes, and the errors of
//! a column or an aggregate written where the grouping does not allow it (8120, 8121, 130,
//! 144, 147).
//!
//! # The shape of the plan
//!
//! ```text
//! SELECT k, COUNT(*) FROM dbo.t GROUP BY k HAVING COUNT(*) > 1
//!
//! Project (k, COUNT(*)) → Filter (HAVING) → Aggregate (keys: k; aggregates: COUNT(*)) → Scan
//! ```
//!
//! The `Aggregate` publishes the `GROUP BY` keys first, then the aggregates, in the order
//! each was met (`bound/mod.rs`). What sits above it — the `HAVING` and the projection —
//! names no column of the `FROM`: each aggregate call and each expression that is a key
//! is replaced by a [`BoundExprKind::ColumnRef`] on its position in that schema, which is
//! the **extraction** below. `COUNT(*)` written in the select list and again in the
//! `HAVING` is one entry of `aggregates`, and both places point at it
//! (`tests/bind_aggregate.rs`, `an_aggregate_written_twice_is_extracted_once`).
//!
//! An aggregate written without a `GROUP BY` (`SELECT COUNT(*) FROM dbo.t`, `SELECT 1 FROM
//! dbo.t HAVING COUNT(*) > 1`) groups the whole input: `group_by` is empty and the node
//! produces **one row**, on an empty input as well — `SELECT COUNT(*) FROM dbo.e` over an
//! empty `dbo.e` answers one row holding `0`, and `SELECT MIN(k) FROM dbo.e` one row holding
//! `NULL`. The executor computes that node on that promise (`tests/bind_aggregate.rs`,
//! `aggregate_without_group_by_is_one_group`).
//!
//! # What an expression above the `Aggregate` may hold
//!
//! Over `dbo.t (k int NOT NULL, c int NULL, s varchar(10) NULL)`:
//!
//! | written | answer |
//! |---|---|
//! | `SELECT k, COUNT(*) FROM dbo.t GROUP BY k` | the plan above |
//! | `SELECT k, c FROM dbo.t GROUP BY k` | **8120** on `c`, printed `dbo.t.c` |
//! | `SELECT k FROM dbo.t GROUP BY k HAVING c > 1` | **8121** on `c` |
//! | `SELECT * FROM dbo.t GROUP BY k` | 8120 on `c`, the first expanded column that is no key |
//! | `SELECT * FROM dbo.t GROUP BY k, c, s` | the three columns |
//! | `SELECT k, CASE WHEN c > 1 THEN 1 ELSE 0 END FROM dbo.t GROUP BY k` | 8120 on `c`: a column under an operator or a `CASE` is looked at as a column |
//! | `SELECT k, SUM(c) FROM dbo.t GROUP BY k`, `SELECT k, COUNT(k + c) …` | the plan: under an aggregate, a column need not be a key |
//! | `SELECT COUNT(*) + k FROM dbo.t`, `SELECT k FROM dbo.t HAVING COUNT(*) > 1` | 8120 on `k`: no `GROUP BY` means no key |
//! | `SELECT 1 FROM dbo.t HAVING k > 1` | 8121 on `k` |
//! | `SELECT 1 FROM dbo.t GROUP BY k`, `SELECT @v, COUNT(*) FROM dbo.t GROUP BY k` | the plan: a constant and a variable are no column |
//! | `SELECT k, CASE WHEN CURRENT_TIMESTAMP > '2000-01-01' THEN 1 ELSE 0 END … GROUP BY k` | the plan: a niladic function is no column |
//! | `SELECT k FROM dbo.t WHERE COUNT(*) > 1`, `SELECT 1 WHERE COUNT(*) > 0` | **147**, on the line of the statement |
//! | `SELECT k FROM dbo.t GROUP BY SUM(c)` | **144**, on the line of the call |
//! | `SELECT SUM(COUNT(*)) FROM dbo.t`, `SELECT SUM(1 + MIN(c)) FROM dbo.t` | **130**, on the line of the inner call |
//! | `SELECT k FROM dbo.t GROUP BY k HAVING 1` | 4145, as a `WHERE 1` |
//! | `SELECT k FROM dbo.t GROUP BY nosuch`, `SELECT k AS kk FROM dbo.t GROUP BY kk` | 207: a key is looked up in the `FROM`, not in the select list |
//!
//! Where several of those faults are written in one statement, the clauses are checked
//! in the order `WHERE`, `GROUP BY`, `HAVING`, select list
//! (`tests/bind_aggregate.rs`, `the_having_is_checked_before_the_select_list`):
//!
//! | written | answer |
//! |---|---|
//! | `SELECT k, c FROM dbo.t GROUP BY k HAVING s > 'a'` | 8121 on `s` |
//! | `SELECT SUM(s) FROM dbo.t GROUP BY k HAVING c > 1` | 8121, not the 8117 of the `SUM` |
//! | `SELECT k FROM dbo.t WHERE COUNT(*) > 1 GROUP BY nosuch` | 147 |
//! | `SELECT k FROM dbo.t GROUP BY SUM(c) HAVING c > 1` | 144 |
//!
//! # When an expression is a key
//!
//! A key may be an expression (`GROUP BY c + 1`, `GROUP BY LEN(s)`), and an expression of
//! the select list or of the `HAVING` is that key when its **bound** form is the same:
//! the same column by position, the same literal, the same operator, the same function,
//! the same target type of a conversion, node for node ([`same_expression`]). Parentheses,
//! the case of a name and a qualifier are gone once bound, so over `GROUP BY c + 1` the
//! select list may write `c + 1`, `c+1`, `(c + 1)`, `C + 1`, `t.c + 1` and `(c + 1) * 2`,
//! and over `GROUP BY LEN(s)` it may write `len(s)`; while `1 + c`, `c + 2`, `c + 1.0` and
//! `c` alone answer 8120 on `c`, `s + 'A'` over `GROUP BY s + 'a'` answers 8120 on `s`,
//! and `CAST(c AS int)` over `GROUP BY CAST(c AS bigint)` answers 8120 on `c`. The same
//! rule serves the `HAVING`: `HAVING c + 1 > 5` over `GROUP BY c + 1` binds and
//! `HAVING 1 + c > 5` answers 8121. Stated on those shapes (`tests/bind_aggregate.rs`,
//! `a_key_expression_is_matched_on_its_bound_form`).
//!
//! # The name 8120 and 8121 print
//!
//! The table part is the name of the table **as written in the `FROM`**, the alias set
//! aside, and the column part is the name the catalogue holds: `FROM dbo.t` prints
//! `dbo.t.c`, `FROM t` prints `t.c`, `FROM [dbo].[T]` prints `dbo.T.c` for a column
//! written `[C]`, `FROM dbo.t AS x` with `x.c` or `c` prints `dbo.t.c`, and over a join
//! the table the column belongs to (`FROM dbo.t AS a JOIN dbo.e AS b ON … GROUP BY a.k`
//! with `b.c` prints `dbo.e.c`). Stated on those shapes
//! (`tests/bind_aggregate.rs`, `a_column_outside_group_by_is_8120`).
//!
//! # What is not bound here
//!
//! `GROUP BY 1`, `GROUP BY 'a'`, `GROUP BY NULL` and `GROUP BY @v` answer 164 on SQL Server
//! (a key must hold a column); here the key binds as a constant, which no expression of
//! the select list is matched against, a deliberate difference. `ORDER BY` over a grouped
//! query is `sort.rs`'s and is not bound yet. `GROUPING SETS`, `ROLLUP`, `CUBE`, `OVER`
//! and the aggregates the registry does not hold (`STRING_AGG`, `STDEV`, `VAR`,
//! `CHECKSUM_AGG`) are refused by name.

use vauban_catalog::ColumnId;
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    CaseArm, ColumnRef, Expr, Ident, InList, QuerySpec, SelectItem, Span, TableRef,
};
use vauban_types::TypeInfo;

use crate::bound::{
    AggregateCall, BoundCaseArm, BoundExpr, BoundExprKind, BoundProjection, ColumnBinding,
    LogicalPlan, OutputColumn, OutputSchema,
};
use crate::call::{bind_aggregate_call, star_under_another_name};
use crate::context::BindContext;
use crate::errors::line_of;
use crate::expr::{Scope, bind_condition, bind_expr};
use crate::names::alias_of;
use crate::query::{bug, column_name, dotted, is_aggregate_name, not_implemented};
use crate::star::{self, Lookup, Source};

/// Binds the grouping of a `SELECT` into a [`LogicalPlan::Aggregate`] over `input`, the
/// `HAVING` into a [`LogicalPlan::Filter`] above it, and the select list into the
/// [`LogicalPlan::Project`] on top: the plan up to and including its projection.
///
/// `scope` is the scope of the `FROM`, against which the keys, the arguments of the
/// aggregates and the columns of the select list resolve. The `WHERE` has been bound
/// before this point ([`refuse_in_where`] having refused an aggregate in it).
///
/// # Errors
///
/// The numbers of the module documentation: 144 for a key holding an aggregate, 8121 and
/// 8120 for a column of the `HAVING` or of the select list that is neither a key nor
/// under an aggregate, 130 for an aggregate under an aggregate, 4145 for a `HAVING` that
/// is not a predicate, and what binding a key, an argument or an expression raises (207,
/// 209, 4104, 8117, 174, 195…).
pub(crate) fn bind_aggregate(
    spec: &QuerySpec,
    input: LogicalPlan,
    scope: &Scope,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let mut grouping = Grouping {
        keys: Vec::new(),
        aggregates: Vec::new(),
        aggregate_types: Vec::new(),
    };
    for key in &spec.group_by {
        if let Some(call) = find_aggregate(key) {
            refuse_star(call, ctx)?;
            return Err(SqlError::aggregate_in_group_by().with_line(line_of(&span_of(call))));
        }
        grouping.keys.push(bind_expr(key, ctx, scope)?);
    }

    // The select list is extracted first, so that its aggregates come first in the node,
    // and its error is held back: the `HAVING` is checked before the select list, and its
    // error wins (module documentation, the order of the clauses).
    let projections = grouping.bind_select_list(spec, scope, ctx);
    let having = match &spec.having {
        Some(condition) => Some(grouping.bind_having(condition, spec, scope, ctx)?),
        None => None,
    };
    let projections = projections?;

    let schema = grouping.schema();
    let mut plan = LogicalPlan::Aggregate {
        input: Box::new(input),
        group_by: grouping.keys,
        aggregates: grouping.aggregates,
        schema: schema.clone(),
    };
    if let Some(mut predicate) = having {
        name_slots(&mut predicate, &schema);
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate,
        };
    }
    let projections: Vec<BoundProjection> = projections
        .into_iter()
        .map(|mut projection| {
            name_slots(&mut projection.expr, &schema);
            projection
        })
        .collect();
    let projected = OutputSchema {
        columns: projections
            .iter()
            .map(|projection| OutputColumn {
                name: projection.name.clone(),
                ty: projection.expr.ty.clone(),
            })
            .collect(),
    };
    Ok(LogicalPlan::Project {
        input: Box::new(plan),
        exprs: projections,
        schema: projected,
    })
}

/// Refuses an aggregate written in a `WHERE`: error **147**, severity 15, state 1, on the
/// line of the statement — not on the line of the call: `SELECT k`, `FROM dbo.t`,
/// `WHERE` and `SUM(c) > 1;` on four lines answer the line of the `SELECT`.
///
/// The clause is refused before it is bound, so that `SELECT k FROM dbo.t WHERE COUNT(*)
/// > 1 GROUP BY nosuch` answers 147 and not 207, and the check does not depend on a
/// `GROUP BY` being written (`SELECT 1 WHERE COUNT(*) > 0` answers 147 too). A star
/// under a name that is not a counting aggregate is the 102 of
/// [`star_under_another_name`] in a `WHERE` as in a select list
/// (`tests/bind_aggregate.rs`, `the_errors_of_the_call_itself`).
pub(crate) fn refuse_in_where(
    condition: &Expr,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<()> {
    match find_aggregate(condition) {
        Some(call) => {
            refuse_star(call, ctx)?;
            Err(SqlError::aggregate_in_where().with_line(statement_line))
        }
        None => Ok(()),
    }
}

/// Which clause a column that is neither a key nor aggregated was written in: the number
/// differs (8120 for the select list, 8121 for the `HAVING`), the message does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    /// The select list, error 8120.
    SelectList,
    /// The `HAVING`, error 8121.
    Having,
}

/// The keys and the aggregates extracted so far: the columns the `Aggregate` publishes,
/// in the order the node lists them.
struct Grouping {
    /// The `GROUP BY` keys, bound against the `FROM`, in written order.
    keys: Vec<BoundExpr>,
    /// The aggregates, in the order they were extracted: those of the select list, then
    /// those the `HAVING` adds.
    aggregates: Vec<AggregateCall>,
    /// The type of the result of each entry of `aggregates`.
    aggregate_types: Vec<TypeInfo>,
}

impl Grouping {
    /// Binds each item of the select list into its projected columns, a `*` expanding
    /// into one column per column of the `FROM` (`star.rs`), each one a key or error 8120.
    fn bind_select_list(
        &mut self,
        spec: &QuerySpec,
        scope: &Scope,
        ctx: &BindContext<'_>,
    ) -> SqlResult<Vec<BoundProjection>> {
        let mut projections = Vec::with_capacity(spec.items.len());
        for item in &spec.items {
            match item {
                SelectItem::Wildcard(span) => {
                    let sources = scope.sources();
                    if sources.is_empty() {
                        return Err(SqlError::select_star_without_from().with_line(line_of(span)));
                    }
                    for projection in star::expand_all(sources, line_of(span)) {
                        projections.push(self.key_column(projection, spec, scope)?);
                    }
                }
                SelectItem::QualifiedWildcard(name) => {
                    for projection in star::expand_qualified(scope.sources(), name)? {
                        projections.push(self.key_column(projection, spec, scope)?);
                    }
                }
                SelectItem::Expr {
                    expr: Expr::Assign { .. },
                    ..
                } => {
                    return Err(not_implemented(
                        "an assignment in the select list of a grouped query",
                    ));
                }
                SelectItem::Expr { expr, alias, .. } => {
                    let rewritten = self.rewrite(expr, Clause::SelectList, spec, scope, ctx)?;
                    let bound = bind_expr(&rewritten, ctx, &self.slot_scope(ctx))?;
                    let name = column_name(expr, &bound, alias.as_ref());
                    projections.push(BoundProjection { expr: bound, name });
                }
            }
        }
        Ok(projections)
    }

    /// Binds the `HAVING` above the `Aggregate`: its aggregates extracted, its keys
    /// replaced, and the rest a predicate (4145 otherwise, quoting the token that follows
    /// the condition as `WHERE` does).
    fn bind_having(
        &mut self,
        condition: &Expr,
        spec: &QuerySpec,
        scope: &Scope,
        ctx: &BindContext<'_>,
    ) -> SqlResult<BoundExpr> {
        let rewritten = self.rewrite(condition, Clause::Having, spec, scope, ctx)?;
        bind_condition(&rewritten, ctx, &self.slot_scope(ctx))
    }

    /// One column of an expanded `*`: a reference to the key it is, or error 8120.
    fn key_column(
        &self,
        projection: BoundProjection,
        spec: &QuerySpec,
        scope: &Scope,
    ) -> SqlResult<BoundProjection> {
        let line = projection.expr.line;
        if let Some(index) = self.key_index(&projection.expr) {
            return Ok(BoundProjection {
                expr: self.slot_reference(index, line),
                name: projection.name,
            });
        }
        let BoundExprKind::ColumnRef(binding) = &projection.expr.kind else {
            return Err(bug(
                "bind_aggregate: an expanded wildcard column is not a column reference",
            ));
        };
        Err(column_outside_group_by(
            binding,
            line,
            Clause::SelectList,
            spec,
            scope,
        ))
    }

    /// The extraction: the same expression, each aggregate call and each key replaced by a
    /// reference to its column of the `Aggregate`, and a column that is neither refused.
    ///
    /// Top down. A node that holds no aggregate is bound against the `FROM` and compared
    /// with the keys; when it matches no key and is a column reference, the error is
    /// raised there, and otherwise its children are looked at in turn, so that `k + 1`
    /// over `GROUP BY k` finds its key one level down. A node that holds an aggregate is
    /// not bound against the `FROM`: its children are looked at. A literal and a variable
    /// are left alone, a key they could match being no column.
    fn rewrite(
        &mut self,
        e: &Expr,
        clause: Clause,
        spec: &QuerySpec,
        scope: &Scope,
        ctx: &BindContext<'_>,
    ) -> SqlResult<Expr> {
        match e {
            Expr::Nested(inner, span) => Ok(Expr::Nested(
                Box::new(self.rewrite(inner, clause, spec, scope, ctx)?),
                *span,
            )),
            Expr::Function { args, span, .. } if is_aggregate_call(e) => {
                if let Some(inner) = args.iter().find_map(find_aggregate) {
                    refuse_star(inner, ctx)?;
                    return Err(SqlError::nested_aggregate().with_line(line_of(&span_of(inner))));
                }
                let (call, ty) = bind_aggregate_call(e, ctx, scope)?;
                let index = self.keys.len() + self.aggregate_index(call, ty);
                Ok(slot_column(index, *span))
            }
            Expr::Literal(..) | Expr::Variable { .. } => Ok(e.clone()),
            _ if find_aggregate(e).is_none() => {
                let bound = bind_expr(e, ctx, scope)?;
                if let Some(index) = self.key_index(&bound) {
                    return Ok(slot_column(index, span_of(e)));
                }
                if let BoundExprKind::ColumnRef(binding) = &bound.kind {
                    return Err(column_outside_group_by(
                        binding, bound.line, clause, spec, scope,
                    ));
                }
                self.rewrite_children(e, clause, spec, scope, ctx)
            }
            _ => self.rewrite_children(e, clause, spec, scope, ctx),
        }
    }

    /// [`Grouping::rewrite`] applied to each child of `e`, the node itself rebuilt around
    /// them. A subquery is not walked into; a node without children is cloned.
    fn rewrite_children(
        &mut self,
        e: &Expr,
        clause: Clause,
        spec: &QuerySpec,
        scope: &Scope,
        ctx: &BindContext<'_>,
    ) -> SqlResult<Expr> {
        let mut child = |inner: &Expr| self.rewrite(inner, clause, spec, scope, ctx);
        Ok(match e {
            Expr::Binary {
                op,
                op_span,
                left,
                right,
                span,
            } => Expr::Binary {
                op: *op,
                op_span: *op_span,
                left: Box::new(child(left)?),
                right: Box::new(child(right)?),
                span: *span,
            },
            Expr::Unary { op, expr, span } => Expr::Unary {
                op: *op,
                expr: Box::new(child(expr)?),
                span: *span,
            },
            Expr::Function {
                name,
                args,
                star,
                distinct,
                over,
                span,
            } => Expr::Function {
                name: name.clone(),
                args: args.iter().map(&mut child).collect::<SqlResult<_>>()?,
                star: *star,
                distinct: *distinct,
                over: over.clone(),
                span: *span,
            },
            Expr::Case {
                operand,
                arms,
                else_,
                span,
            } => Expr::Case {
                operand: operand
                    .as_deref()
                    .map(&mut child)
                    .transpose()?
                    .map(Box::new),
                arms: arms
                    .iter()
                    .map(|arm| {
                        Ok(CaseArm {
                            when: child(&arm.when)?,
                            then: child(&arm.then)?,
                        })
                    })
                    .collect::<SqlResult<_>>()?,
                else_: else_.as_deref().map(&mut child).transpose()?.map(Box::new),
                span: *span,
            },
            Expr::Cast {
                expr,
                ty,
                try_,
                span,
            } => Expr::Cast {
                expr: Box::new(child(expr)?),
                ty: ty.clone(),
                try_: *try_,
                span: *span,
            },
            Expr::Convert {
                ty,
                expr,
                style,
                try_,
                span,
            } => Expr::Convert {
                ty: ty.clone(),
                expr: Box::new(child(expr)?),
                style: style.as_deref().map(&mut child).transpose()?.map(Box::new),
                try_: *try_,
                span: *span,
            },
            Expr::IsNull {
                expr,
                negated,
                span,
            } => Expr::IsNull {
                expr: Box::new(child(expr)?),
                negated: *negated,
                span: *span,
            },
            Expr::In {
                expr,
                list,
                negated,
                span,
            } => Expr::In {
                expr: Box::new(child(expr)?),
                list: match list {
                    InList::Exprs(items) => {
                        InList::Exprs(items.iter().map(&mut child).collect::<SqlResult<_>>()?)
                    }
                    InList::Subquery(query) => InList::Subquery(query.clone()),
                },
                negated: *negated,
                span: *span,
            },
            Expr::Like {
                expr,
                pattern,
                escape,
                negated,
                span,
            } => Expr::Like {
                expr: Box::new(child(expr)?),
                pattern: Box::new(child(pattern)?),
                escape: escape.as_deref().map(&mut child).transpose()?.map(Box::new),
                negated: *negated,
                span: *span,
            },
            Expr::Between {
                expr,
                low,
                high,
                negated,
                span,
            } => Expr::Between {
                expr: Box::new(child(expr)?),
                low: Box::new(child(low)?),
                high: Box::new(child(high)?),
                negated: *negated,
                span: *span,
            },
            Expr::Collate {
                expr,
                collation,
                collation_span,
                span,
            } => Expr::Collate {
                expr: Box::new(child(expr)?),
                collation: collation.clone(),
                collation_span: *collation_span,
                span: *span,
            },
            Expr::Nested(inner, span) => Expr::Nested(Box::new(child(inner)?), *span),
            Expr::Literal(..)
            | Expr::Column(_)
            | Expr::Variable { .. }
            | Expr::Exists(..)
            | Expr::Subquery(..)
            | Expr::Quantified { .. }
            | Expr::Assign { .. }
            | Expr::NextValueFor { .. }
            | Expr::InvalidNiladic { .. }
            | Expr::Placeholder(_) => e.clone(),
        })
    }

    /// The position in `aggregates` of a call already extracted that is the same as
    /// `call` — same definition, same `DISTINCT`, same argument by [`same_expression`], or
    /// no argument on both sides — and otherwise the position `call` is appended at.
    fn aggregate_index(&mut self, call: AggregateCall, ty: TypeInfo) -> usize {
        let same = self.aggregates.iter().position(|existing| {
            std::ptr::eq(existing.def, call.def)
                && existing.distinct == call.distinct
                && match (&existing.arg, &call.arg) {
                    (None, None) => true,
                    (Some(a), Some(b)) => same_expression(a, b),
                    _ => false,
                }
        });
        match same {
            Some(index) => index,
            None => {
                self.aggregates.push(call);
                self.aggregate_types.push(ty);
                self.aggregates.len() - 1
            }
        }
    }

    /// The position of the key `e` is, by [`same_expression`], or `None`.
    fn key_index(&self, e: &BoundExpr) -> Option<usize> {
        self.keys.iter().position(|key| same_expression(key, e))
    }

    /// The columns the `Aggregate` publishes: the keys, each named after its column when
    /// it is one and unnamed otherwise, then the aggregates, unnamed.
    fn schema(&self) -> OutputSchema {
        let keys = self.keys.iter().map(|key| OutputColumn {
            name: match &key.kind {
                BoundExprKind::ColumnRef(binding) => binding.name.clone(),
                _ => String::new(),
            },
            ty: key.ty.clone(),
        });
        let aggregates = self.aggregate_types.iter().map(|ty| OutputColumn {
            name: String::new(),
            ty: ty.clone(),
        });
        OutputSchema {
            columns: keys.chain(aggregates).collect(),
        }
    }

    /// The scope an extracted expression binds against: one source holding the columns
    /// of the `Aggregate` under the names [`slot_column`] writes, and nothing of the
    /// `FROM`.
    fn slot_scope(&self, ctx: &BindContext<'_>) -> Scope {
        let columns: Vec<ColumnBinding> = self
            .schema()
            .columns
            .into_iter()
            .enumerate()
            .map(|(index, column)| ColumnBinding {
                column: SLOT_COLUMN_ID,
                index,
                name: slot_name(index),
                ty: column.ty,
            })
            .collect();
        Scope::over(Source::new(
            "",
            None,
            &columns,
            ctx.default_schema,
            ctx.database,
        ))
    }

    /// A bound reference to column `index` of the `Aggregate`, as [`bind_expr`] builds it
    /// from [`slot_column`].
    fn slot_reference(&self, index: usize, line: u32) -> BoundExpr {
        let ty = self.schema().columns[index].ty.clone();
        BoundExpr {
            kind: BoundExprKind::ColumnRef(ColumnBinding {
                column: SLOT_COLUMN_ID,
                index,
                name: slot_name(index),
                ty: ty.clone(),
            }),
            ty,
            line,
        }
    }
}

/// The `column` of a [`ColumnBinding`] that designates a column of an `Aggregate` rather
/// than a column of a table: `0`, which the catalogue gives no column (its identifiers
/// start at 1, `ColumnId(ordinal + 1)`).
const SLOT_COLUMN_ID: ColumnId = ColumnId(0);

/// The name column `index` of the `Aggregate` is looked up under while an extracted
/// expression is bound: a NUL and the index, which no written identifier is.
fn slot_name(index: usize) -> String {
    format!("\u{0}{index}")
}

/// A column reference on column `index` of the `Aggregate`, written in place of an
/// aggregate call or of a key and bound against [`Grouping::slot_scope`]. `span` is the
/// position of what it replaces, so that the line of the bound node, and the token 4145
/// quotes after a `HAVING COUNT(*)`, are those of the written text.
fn slot_column(index: usize, span: Span) -> Expr {
    Expr::Column(ColumnRef {
        qualifier: None,
        name: Ident {
            value: slot_name(index),
            quoted: true,
        },
        span,
    })
}

/// Gives each column reference of an extracted expression the name of its column in
/// `schema`, in place of the name it was looked up under.
fn name_slots(e: &mut BoundExpr, schema: &OutputSchema) {
    if let BoundExprKind::ColumnRef(binding) = &mut e.kind
        && binding.column == SLOT_COLUMN_ID
        && let Some(column) = schema.columns.get(binding.index)
    {
        binding.name = column.name.clone();
    }
    for child in children_mut(e) {
        name_slots(child, schema);
    }
}

/// The direct children of a bound expression, a plan under it not included.
fn children_mut(e: &mut BoundExpr) -> Vec<&mut BoundExpr> {
    match &mut e.kind {
        BoundExprKind::Literal(_)
        | BoundExprKind::ColumnRef(_)
        | BoundExprKind::Variable { .. }
        | BoundExprKind::Exists(_)
        | BoundExprKind::ScalarSubquery(_) => Vec::new(),
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => vec![left.as_mut(), right.as_mut()],
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner }
        | BoundExprKind::InSubquery { expr: inner, .. } => vec![inner.as_mut()],
        BoundExprKind::In { expr, list, .. } => {
            let mut children = vec![expr.as_mut()];
            children.extend(list.iter_mut());
            children
        }
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            let mut children = vec![expr.as_mut(), pattern.as_mut()];
            children.extend(escape.as_deref_mut());
            children
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            let mut children: Vec<&mut BoundExpr> = Vec::new();
            children.extend(operand.as_deref_mut());
            for BoundCaseArm { when, then } in arms.iter_mut() {
                children.push(when);
                children.push(then);
            }
            children.extend(else_.as_deref_mut());
            children
        }
        BoundExprKind::Function { args, .. } => args.iter_mut().collect(),
    }
}

/// Whether two bound expressions are the same, node for node: same type, and same
/// variant with the same operator, literal, column (by position), function definition,
/// or conversion, the operands compared in turn. A subquery is the same as nothing. The
/// rule that decides when an expression of the select list is a `GROUP BY` key (module
/// documentation), and when two aggregate calls are one.
fn same_expression(a: &BoundExpr, b: &BoundExpr) -> bool {
    if a.ty != b.ty {
        return false;
    }
    let same_list = |x: &[BoundExpr], y: &[BoundExpr]| {
        x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_expression(x, y))
    };
    let same_option = |x: Option<&BoundExpr>, y: Option<&BoundExpr>| match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => same_expression(x, y),
        _ => false,
    };
    match (&a.kind, &b.kind) {
        (BoundExprKind::Literal(x), BoundExprKind::Literal(y)) => x == y,
        (BoundExprKind::ColumnRef(x), BoundExprKind::ColumnRef(y)) => {
            x.index == y.index && x.column == y.column
        }
        (BoundExprKind::Variable { name: x }, BoundExprKind::Variable { name: y }) => {
            x.eq_ignore_ascii_case(y)
        }
        (
            BoundExprKind::Arith {
                op: x,
                left: xl,
                right: xr,
            },
            BoundExprKind::Arith {
                op: y,
                left: yl,
                right: yr,
            },
        ) => x == y && same_expression(xl, yl) && same_expression(xr, yr),
        (
            BoundExprKind::Compare {
                op: x,
                left: xl,
                right: xr,
            },
            BoundExprKind::Compare {
                op: y,
                left: yl,
                right: yr,
            },
        ) => x == y && same_expression(xl, yl) && same_expression(xr, yr),
        (
            BoundExprKind::Logical {
                op: x,
                left: xl,
                right: xr,
            },
            BoundExprKind::Logical {
                op: y,
                left: yl,
                right: yr,
            },
        ) => x == y && same_expression(xl, yl) && same_expression(xr, yr),
        (BoundExprKind::Negate(x), BoundExprKind::Negate(y))
        | (BoundExprKind::BitNot(x), BoundExprKind::BitNot(y))
        | (BoundExprKind::Not(x), BoundExprKind::Not(y))
        | (BoundExprKind::Collate { expr: x }, BoundExprKind::Collate { expr: y }) => {
            same_expression(x, y)
        }
        (
            BoundExprKind::IsNull {
                expr: x,
                negated: xn,
            },
            BoundExprKind::IsNull {
                expr: y,
                negated: yn,
            },
        ) => xn == yn && same_expression(x, y),
        (
            BoundExprKind::In {
                expr: x,
                list: xs,
                negated: xn,
            },
            BoundExprKind::In {
                expr: y,
                list: ys,
                negated: yn,
            },
        ) => xn == yn && same_expression(x, y) && same_list(xs, ys),
        (
            BoundExprKind::Like {
                expr: x,
                pattern: xp,
                escape: xe,
                negated: xn,
            },
            BoundExprKind::Like {
                expr: y,
                pattern: yp,
                escape: ye,
                negated: yn,
            },
        ) => {
            xn == yn
                && same_expression(x, y)
                && same_expression(xp, yp)
                && same_option(xe.as_deref(), ye.as_deref())
        }
        (
            BoundExprKind::Case {
                operand: xo,
                arms: xa,
                else_: xe,
            },
            BoundExprKind::Case {
                operand: yo,
                arms: ya,
                else_: ye,
            },
        ) => {
            same_option(xo.as_deref(), yo.as_deref())
                && xa.len() == ya.len()
                && xa.iter().zip(ya).all(|(x, y)| {
                    same_expression(&x.when, &y.when) && same_expression(&x.then, &y.then)
                })
                && same_option(xe.as_deref(), ye.as_deref())
        }
        (
            BoundExprKind::Convert {
                expr: x,
                style: xs,
                try_: xt,
            },
            BoundExprKind::Convert {
                expr: y,
                style: ys,
                try_: yt,
            },
        ) => xs == ys && xt == yt && same_expression(x, y),
        (
            BoundExprKind::Function { def: xd, args: xa },
            BoundExprKind::Function { def: yd, args: ya },
        ) => std::ptr::eq(*xd, *yd) && same_list(xa, ya),
        _ => false,
    }
}

/// The error of a column that is neither a key nor under an aggregate: 8120 in the select
/// list, 8121 in the `HAVING`, on the line of the column, naming the table as the module
/// documentation says.
fn column_outside_group_by(
    binding: &ColumnBinding,
    line: u32,
    clause: Clause,
    spec: &QuerySpec,
    scope: &Scope,
) -> SqlError {
    let table = written_table_of(binding, spec, scope);
    let error = match clause {
        Clause::SelectList => SqlError::column_invalid_in_select_list(&table, &binding.name),
        Clause::Having => SqlError::column_invalid_in_having(&table, &binding.name),
    };
    error.with_line(line)
}

/// The name, as written in the `FROM`, of the table `binding` is a column of.
///
/// The source in scope that holds the binding is found by the name and the position of
/// the column, and the reference of the `FROM` that source came from by the name the
/// query refers to it by (its alias, or its object name — `names::alias_of`), the join
/// having refused two references of one exposed name. A reference this file does not
/// know the written name of, or a column found in no source, prints the exposed name.
fn written_table_of(binding: &ColumnBinding, spec: &QuerySpec, scope: &Scope) -> String {
    let source = scope.sources().iter().find(|source| {
        matches!(
            source.column(&binding.name),
            Lookup::One(found) if found.index == binding.index && found.column == binding.column
        )
    });
    let Some(exposed) = source.map(Source::exposed_name) else {
        return String::new();
    };
    let mut references = Vec::new();
    flatten_references(&spec.from, &mut references);
    references
        .into_iter()
        .find_map(|reference| match reference {
            TableRef::Table { name, alias, .. } | TableRef::Function { name, alias, .. }
                if alias_of(name, alias.as_ref()).eq_ignore_ascii_case(exposed) =>
            {
                Some(dotted(name))
            }
            _ => None,
        })
        .unwrap_or_else(|| exposed.to_owned())
}

/// The references of a `FROM`, in written order, a join or an `APPLY` opened.
fn flatten_references<'a>(references: &'a [TableRef], out: &mut Vec<&'a TableRef>) {
    for reference in references {
        match reference {
            TableRef::Join { left, right, .. } | TableRef::Apply { left, right, .. } => {
                flatten_references(std::slice::from_ref(left), out);
                flatten_references(std::slice::from_ref(right), out);
            }
            other => out.push(other),
        }
    }
}

/// Whether `e` is an aggregate call: a call written with a star or a `DISTINCT`, or a
/// call whose unqualified name the registry answers for with an aggregate
/// (`query::is_aggregate_name`). The first two are refused by `call.rs` when the name is
/// no aggregate (102 for `SUM(*)`, 195 for `LEN(DISTINCT s)`).
fn is_aggregate_call(e: &Expr) -> bool {
    match e {
        Expr::Function {
            name,
            star,
            distinct,
            ..
        } => *star || *distinct || is_aggregate_name(name),
        _ => false,
    }
}

/// The first aggregate call `e` holds, in written order, itself included, or `None`. A
/// subquery is not walked into. A worklist rather than a recursion, for the reason
/// `query::holds_an_aggregate` gives.
fn find_aggregate(e: &Expr) -> Option<&Expr> {
    let mut pending = vec![e];
    while let Some(e) = pending.pop() {
        if is_aggregate_call(e) {
            return Some(e);
        }
        let mut children: Vec<&Expr> = Vec::new();
        match e {
            Expr::Function { args, .. } => children.extend(args),
            Expr::Nested(inner, _)
            | Expr::Unary { expr: inner, .. }
            | Expr::Cast { expr: inner, .. }
            | Expr::IsNull { expr: inner, .. }
            | Expr::Collate { expr: inner, .. }
            | Expr::Assign { value: inner, .. } => children.push(inner),
            Expr::Binary { left, right, .. } => {
                children.push(left);
                children.push(right);
            }
            Expr::Convert { expr, style, .. } => {
                children.push(expr);
                children.extend(style.as_deref());
            }
            Expr::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                children.extend(operand.as_deref());
                for arm in arms {
                    children.push(&arm.when);
                    children.push(&arm.then);
                }
                children.extend(else_.as_deref());
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                children.push(expr);
                children.push(pattern);
                children.extend(escape.as_deref());
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                children.push(expr);
                children.push(low);
                children.push(high);
            }
            Expr::In { expr, list, .. } => {
                children.push(expr);
                if let InList::Exprs(items) = list {
                    children.extend(items);
                }
            }
            Expr::Literal(..)
            | Expr::Column(_)
            | Expr::Variable { .. }
            | Expr::Exists(..)
            | Expr::Subquery(..)
            | Expr::Quantified { .. }
            | Expr::NextValueFor { .. }
            | Expr::InvalidNiladic { .. }
            | Expr::Placeholder(_) => {}
        }
        // Pushed last to first, so that the leftmost child is popped first.
        pending.extend(children.into_iter().rev());
    }
    None
}

/// The error of a call that is spelled as an aggregate under a name that is none, raised
/// before the number of the clause the call was found in: `SELECT k FROM dbo.t WHERE
/// SUM(*) > 1` answers 102 near `*` and `SELECT k FROM dbo.t WHERE LEN(DISTINCT s) = 1`
/// answers 195 naming an aggregate function, as the two do in a select list
/// (`tests/bind_aggregate.rs`, `the_errors_of_the_call_itself`).
fn refuse_star(call: &Expr, ctx: &BindContext<'_>) -> SqlResult<()> {
    match call {
        Expr::Function {
            name,
            star: true,
            span,
            ..
        } => star_under_another_name(name, span, ctx),
        Expr::Function {
            name,
            distinct: true,
            span,
            ..
        } if !is_aggregate_name(name) => Err(SqlError::not_a_recognized_name(
            &name.name.value,
            "aggregate function",
        )
        .with_line(line_of(span))),
        _ => Ok(()),
    }
}

/// The position of an expression, whichever variant it is.
fn span_of(e: &Expr) -> Span {
    match e {
        Expr::Literal(_, span)
        | Expr::Nested(_, span)
        | Expr::Variable { span, .. }
        | Expr::Binary { span, .. }
        | Expr::Unary { span, .. }
        | Expr::Function { span, .. }
        | Expr::Case { span, .. }
        | Expr::Cast { span, .. }
        | Expr::Convert { span, .. }
        | Expr::IsNull { span, .. }
        | Expr::In { span, .. }
        | Expr::Like { span, .. }
        | Expr::Between { span, .. }
        | Expr::Exists(_, span)
        | Expr::Subquery(_, span)
        | Expr::Quantified { span, .. }
        | Expr::Collate { span, .. }
        | Expr::Assign { span, .. }
        | Expr::NextValueFor { span, .. }
        | Expr::InvalidNiladic { span, .. }
        | Expr::Placeholder(span) => *span,
        Expr::Column(column) => column.span,
    }
}
