//! `UNION [ALL]`, `EXCEPT` and `INTERSECT`: the operands' widths (205) and the type of
//! each column of the result.
//!
//! An `ORDER BY` written after a set operation orders the whole of it, and the Sort
//! goes directly above the `SetOp`. The key resolution is `sort.rs`'s business, not this
//! file's.
//!
//! # What `UNION` without `ALL` means
//!
//! `UNION` without `ALL` is `all: false`: the deduplication is a property of the SetOp
//! node, not a `Distinct` above it. The executor and the plan render `UNION` as a flag,
//! not as a stacking of operators.
//!
//! # The type of a column
//!
//! A run of one operator, `UNION ALL` or `UNION`, is typed as one n-ary operation over
//! the operands of the run, whatever the shape of its tree: the parser balances a long
//! run, and parentheses around a part of the run change nothing
//! (`tests/bind_setop.rs`, `a_union_all_run_is_typed_over_its_operands`). Two effects
//! show it:
//!
//! - a bare `NULL` operand does not take part in the type: `SELECT NULL UNION ALL SELECT
//!   NULL UNION ALL SELECT 'a'` is a `varchar(1)`;
//! - an integer literal facing an exact numeric counts its own digits, as under `CASE`:
//!   `SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 2.5` is a `numeric(2,1)`.
//!
//! A run nested under another operator is an operand of that operator, typed on its own
//! first: `SELECT '1' UNION SELECT '01' UNION ALL SELECT 5` deduplicates the two strings
//! before they become integers, and answers 1, 1 and 5. The nested run brings its type to
//! the outer one, a bare `NULL` type if its operands are bare `NULL`s, and the outer run
//! converts it above the nested node (`tests/bind_setop.rs`,
//! `a_nested_run_is_converted_above_its_own_node`).
//!
//! `EXCEPT` and `INTERSECT` are binary: their type is that of their two operands, and they
//! hand their **left** operand's reading of an integer literal to the operator above them:
//! `SELECT 12345 INTERSECT SELECT 1 UNION ALL SELECT 2.5` is a `numeric(6,1)`
//! (`tests/bind_setop.rs`, `intersect_hands_its_left_literal_to_the_run_above`).
//!
//! An exact numeric result wider than 38 digits keeps its integral digits and gives up
//! scale: `decimal(35,0)` against `decimal(20,19)` is a `decimal(38,3)`
//! (`tests/bind_setop.rs`, `a_wide_decimal_gives_up_scale`).

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{QueryBody, SelectStatement, SetOp as AstSetOp, Span};
use vauban_types::{SqlType, TypeFamily, TypeInfo, Value, implicit_result_type};

use crate::bound::{
    BoundExpr, BoundExprKind, BoundProjection, LogicalPlan, OutputColumn, OutputSchema, SetOpKind,
};
use crate::context::BindContext;
use crate::errors::line_of;
use crate::expr::{Scope, convert_to};
use crate::query::bind_select_with_parent;
use crate::view::source_columns;

/// The largest precision of an exact numeric.
const MAX_PRECISION: u8 = 38;

/// Binds a `QueryBody::SetOp` into a [`LogicalPlan::SetOp`].
///
/// The operands are bound recursively, the width of each operator is checked, then each
/// run of one operator is typed over its operands (module documentation) and each operand
/// is converted to the type of its run.
///
/// Each operand is bound under `parent`, the scope of the query that encloses the set
/// operation, so a branch of a subquery reads the columns of that query
/// (`tests/bind_subquery.rs`, `each_branch_of_a_set_operation_reads_the_enclosing_query`).
///
/// The ORDER BY of the statement is left to `query.rs`, which calls
/// `sort::bind_order_by` on the result of this function.
///
/// # Errors
///
/// - **205** when the two operands of an operator do not have the same number of columns;
/// - errors of `types::implicit_result_type` for operands that share no common type.
pub(crate) fn bind_set_op(
    body: &QueryBody,
    stmt: &SelectStatement,
    ctx: &BindContext<'_>,
    parent: &Scope,
) -> SqlResult<LogicalPlan> {
    // ORDER BY is left to query.rs which calls sort::bind_order_by on the result.
    let _ = stmt;
    let tree = bind_tree(body, ctx, parent)?;
    Ok(type_tree(tree)?.plan)
}

/// A set operation whose operands are bound and not yet converted.
enum Tree {
    /// One `SELECT`, as `query.rs` bound it.
    Leaf(LogicalPlan),
    /// One operator over two operands of the same width.
    Node {
        /// The operator.
        kind: SetOpKind,
        /// Whether `ALL` was written.
        all: bool,
        /// The line of the operator's text, for the conversions put above a nested run.
        line: u32,
        /// The left operand.
        left: Box<Tree>,
        /// The right operand.
        right: Box<Tree>,
    },
}

impl Tree {
    /// The number of columns the operand produces.
    fn width(&self) -> usize {
        match self {
            Tree::Leaf(plan) => plan.schema().columns.len(),
            Tree::Node { left, .. } => left.width(),
        }
    }

    /// Whether this operand continues a `UNION` run of `run_all`.
    fn continues(&self, run_kind: SetOpKind, run_all: bool) -> bool {
        matches!(self, Tree::Node { kind, all, .. }
            if *kind == SetOpKind::Union && *kind == run_kind && *all == run_all)
    }
}

/// Binds the operands of a set operation and checks the width of each operator.
fn bind_tree(body: &QueryBody, ctx: &BindContext<'_>, parent: &Scope) -> SqlResult<Tree> {
    match body {
        QueryBody::SetOp {
            op,
            all,
            left,
            right,
            span,
        } => {
            let left = bind_tree(left, ctx, parent)?;
            let right = bind_tree(right, ctx, parent)?;
            if left.width() != right.width() {
                return Err(SqlError::set_operator_column_count_mismatch().with_line(line_of(span)));
            }
            Ok(Tree::Node {
                kind: match op {
                    AstSetOp::Union => SetOpKind::Union,
                    AstSetOp::Except => SetOpKind::Except,
                    AstSetOp::Intersect => SetOpKind::Intersect,
                },
                all: *all,
                line: line_of(span),
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        QueryBody::Nested(inner, _) => bind_tree(inner, ctx, parent),
        QueryBody::Select(_) => {
            let synthetic = SelectStatement {
                with: None,
                body: body.clone(),
                order_by: Vec::new(),
                offset_fetch: None,
                for_clause: None,
                span: span_of(body),
            };
            Ok(Tree::Leaf(bind_select_with_parent(
                &synthetic, ctx, parent,
            )?))
        }
    }
}

/// How one column of an operand takes part in the type of its run.
struct ColumnView {
    /// The type of the column.
    ty: TypeInfo,
    /// True for a bare `NULL`, which brings its nullability and not its type.
    bare_null: bool,
    /// The digits of an integer literal, which it enters an exact numeric with.
    literal_digits: Option<u8>,
}

/// An operand in its own types, with the view its run reads it through.
struct Typed {
    /// The plan of the operand.
    plan: LogicalPlan,
    /// One view per column.
    views: Vec<ColumnView>,
    /// The line a conversion put above the operand carries.
    line: u32,
}

/// Types a set operation: the run at its root, then, through it, the runs nested in it.
fn type_tree(tree: Tree) -> SqlResult<Typed> {
    let (kind, all, line) = match &tree {
        Tree::Leaf(_) => {
            let Tree::Leaf(plan) = tree else {
                unreachable!("matched as a leaf just above")
            };
            let views = leaf_views(&plan);
            return Ok(Typed {
                plan,
                views,
                line: 0,
            });
        }
        Tree::Node {
            kind, all, line, ..
        } => (*kind, *all, *line),
    };
    let mut operands = Vec::new();
    let skeleton = split_run(tree, kind, all, &mut operands);
    let typed = operands
        .into_iter()
        .map(type_tree)
        .collect::<SqlResult<Vec<_>>>()?;
    let width = typed.first().map_or(0, |operand| operand.views.len());
    let mut columns = Vec::with_capacity(width);
    for column in 0..width {
        let views: Vec<&ColumnView> = typed.iter().map(|operand| &operand.views[column]).collect();
        columns.push(run_type(&views)?);
    }
    let views = run_views(kind, &typed, &columns);
    let mut plans: Vec<Option<LogicalPlan>> = typed
        .into_iter()
        .map(|operand| Some(convert_operand(operand, &columns)))
        .collect();
    let plan = assemble(skeleton, &mut plans, &columns);
    Ok(Typed { plan, views, line })
}

/// The shape of a run, its operands replaced by their position in the run.
enum Skeleton {
    /// The operand at this position.
    Operand(usize),
    /// One operator of the run.
    Node {
        /// The operator.
        kind: SetOpKind,
        /// Whether `ALL` was written.
        all: bool,
        /// The left operand.
        left: Box<Skeleton>,
        /// The right operand.
        right: Box<Skeleton>,
    },
}

/// Takes the operands of the run headed by `tree` out of it, left to right.
///
/// The root of `tree` belongs to the run; below it, a node of the same `UNION` or the same
/// `UNION ALL` does too, and anything else is an operand.
fn split_run(tree: Tree, kind: SetOpKind, all: bool, operands: &mut Vec<Tree>) -> Skeleton {
    match tree {
        Tree::Node {
            kind: node_kind,
            all: node_all,
            left,
            right,
            ..
        } => {
            let left = split_child(*left, kind, all, operands);
            let right = split_child(*right, kind, all, operands);
            Skeleton::Node {
                kind: node_kind,
                all: node_all,
                left: Box::new(left),
                right: Box::new(right),
            }
        }
        leaf @ Tree::Leaf(_) => {
            operands.push(leaf);
            Skeleton::Operand(operands.len() - 1)
        }
    }
}

/// One child of a node of the run: part of the run, or an operand of it.
fn split_child(child: Tree, kind: SetOpKind, all: bool, operands: &mut Vec<Tree>) -> Skeleton {
    if child.continues(kind, all) {
        split_run(child, kind, all, operands)
    } else {
        operands.push(child);
        Skeleton::Operand(operands.len() - 1)
    }
}

/// Rebuilds the nodes of a run over its converted operands.
fn assemble(
    skeleton: Skeleton,
    plans: &mut [Option<LogicalPlan>],
    columns: &[TypeInfo],
) -> LogicalPlan {
    match skeleton {
        Skeleton::Operand(position) => plans[position]
            .take()
            .unwrap_or_else(|| unreachable!("an operand appears once in its skeleton")),
        Skeleton::Node {
            kind,
            all,
            left,
            right,
        } => {
            let left = assemble(*left, plans, columns);
            let right = assemble(*right, plans, columns);
            let schema = OutputSchema {
                columns: left
                    .schema()
                    .columns
                    .iter()
                    .zip(columns)
                    .map(|(column, ty)| OutputColumn {
                        name: column.name.clone(),
                        ty: ty.clone(),
                    })
                    .collect(),
            };
            LogicalPlan::SetOp {
                op: kind,
                all,
                left: Box::new(left),
                right: Box::new(right),
                schema,
            }
        }
    }
}

/// The views a run hands to the operator above it.
///
/// A column whose operands are bare `NULL`s stays a bare `NULL`. Under `EXCEPT` and
/// `INTERSECT`, the left operand's integer literal keeps its digits.
fn run_views(kind: SetOpKind, typed: &[Typed], columns: &[TypeInfo]) -> Vec<ColumnView> {
    columns
        .iter()
        .enumerate()
        .map(|(column, ty)| {
            let bare_null = typed.iter().all(|operand| operand.views[column].bare_null);
            let literal_digits = match kind {
                SetOpKind::Union => None,
                SetOpKind::Except | SetOpKind::Intersect => typed
                    .first()
                    .and_then(|left| left.views[column].literal_digits),
            };
            ColumnView {
                ty: ty.clone(),
                bare_null,
                literal_digits,
            }
        })
        .collect()
}

/// The views of the columns of one `SELECT`, read on its projection.
fn leaf_views(plan: &LogicalPlan) -> Vec<ColumnView> {
    let projections = projection_of(plan);
    plan.schema()
        .columns
        .iter()
        .enumerate()
        .map(|(column, output)| {
            let expr = projections
                .and_then(|exprs| exprs.get(column))
                .map(|projection| &projection.expr);
            let bare_null =
                expr.is_some_and(|e| matches!(e.kind, BoundExprKind::Literal(Value::Null)));
            let literal_digits = expr.and_then(literal_digits);
            ColumnView {
                ty: output.ty.clone(),
                bare_null,
                literal_digits,
            }
        })
        .collect()
}

/// The projections of the `Project` a `SELECT` ends with, under its `DISTINCT`, `TOP` and
/// `ORDER BY`.
fn projection_of(plan: &LogicalPlan) -> Option<&[BoundProjection]> {
    match plan {
        LogicalPlan::Project { exprs, .. } => Some(exprs),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Distinct(input) => projection_of(input),
        _ => None,
    }
}

/// The type of one column of a run, over the views of its operands.
///
/// Bare `NULL`s are left out while a typed operand remains; integer literals enter as a
/// `numeric` of their own digits when an exact numeric operand is there; the fold goes
/// left to right; the result is nullable when one operand is.
fn run_type(views: &[&ColumnView]) -> SqlResult<TypeInfo> {
    let typed: Vec<&ColumnView> = views.iter().filter(|v| !v.bare_null).copied().collect();
    let pool: &[&ColumnView] = if typed.is_empty() { views } else { &typed };
    let exact = pool.iter().any(|view| view.ty.ty.is_exact_numeric());
    let entries: Vec<TypeInfo> = pool
        .iter()
        .map(|view| match view.literal_digits {
            Some(precision) if exact && view.ty.ty.family() == TypeFamily::Integer => {
                TypeInfo::new(
                    SqlType::Numeric {
                        precision,
                        scale: 0,
                    },
                    view.ty.nullable,
                )
            }
            _ => view.ty.clone(),
        })
        .collect();
    let mut entries_iter = entries.iter();
    let Some(first) = entries_iter.next() else {
        unreachable!("a set operator has two operands")
    };
    let mut common = first.clone();
    for (entry, view) in entries_iter.zip(pool.iter().skip(1)) {
        // A pair that shares no type is named by the operand's own type, `int` for an
        // integer literal (`tests/bind_setop.rs`, `a_clash_names_the_literal_as_an_int`).
        common = implicit_result_type(&common, entry)
            .map_err(|e| implicit_result_type(&common, &view.ty).err().unwrap_or(e))?;
    }
    common.ty = keep_integral_digits(common.ty, &entries);
    common.nullable = views.iter().any(|view| view.ty.nullable);
    Ok(common)
}

/// An exact numeric result whose integral digits and scale add up past 38 digits gives up
/// scale: `decimal(38,0)` against `decimal(10,9)` is a `decimal(38,0)`.
fn keep_integral_digits(ty: SqlType, entries: &[TypeInfo]) -> SqlType {
    if !ty.is_exact_numeric() {
        return ty;
    }
    let parts: Vec<(u8, u8)> = entries
        .iter()
        .filter_map(|entry| precision_scale(&entry.ty))
        .collect();
    let integral = parts
        .iter()
        .map(|(precision, scale)| precision.saturating_sub(*scale))
        .max()
        .unwrap_or(0);
    let scale = parts.iter().map(|(_, scale)| *scale).max().unwrap_or(0);
    if u16::from(integral) + u16::from(scale) <= u16::from(MAX_PRECISION) {
        return ty;
    }
    let scale = MAX_PRECISION.saturating_sub(integral);
    match ty {
        SqlType::Numeric { .. } => SqlType::Numeric {
            precision: MAX_PRECISION,
            scale,
        },
        _ => SqlType::Decimal {
            precision: MAX_PRECISION,
            scale,
        },
    }
}

/// The precision and scale a type brings to an exact numeric, `None` for the other types.
fn precision_scale(ty: &SqlType) -> Option<(u8, u8)> {
    match ty {
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            Some((*precision, *scale))
        }
        SqlType::BigInt => Some((19, 0)),
        SqlType::Int => Some((10, 0)),
        SqlType::SmallInt => Some((5, 0)),
        SqlType::TinyInt => Some((3, 0)),
        SqlType::Bit => Some((1, 0)),
        SqlType::Money => Some((19, 4)),
        SqlType::SmallMoney => Some((10, 4)),
        _ => None,
    }
}

/// The type `expr` enters a common type with, beside other values of which one is an exact
/// numeric when `exact` is true: an integer literal then counts its own digits, as an
/// operand of a set operation does (`tests/bind_subquery.rs`,
/// `a_values_column_takes_the_type_its_pair_takes_under_a_set_operation`).
pub(crate) fn literal_entry_type(expr: &BoundExpr, exact: bool) -> TypeInfo {
    let digits = if exact { literal_digits(expr) } else { None };
    match digits {
        Some(precision) => TypeInfo::new(
            SqlType::Numeric {
                precision,
                scale: 0,
            },
            expr.ty.nullable,
        ),
        None => expr.ty.clone(),
    }
}

/// The digits of an integer literal, signed or not, `None` for another expression.
///
/// A negated literal counts the digits of its value, except at five digits, where it
/// counts six: `-7` is a `numeric(1,0)`, `-9999` a `numeric(4,0)`, `-12345` and `-10000` a
/// `numeric(6,0)`, `-123456` a `numeric(6,0)` again (`tests/bind_setop.rs`,
/// `a_negative_literal_counts_its_digits`).
fn literal_digits(expr: &BoundExpr) -> Option<u8> {
    if expr.ty.ty.family() != TypeFamily::Integer {
        return None;
    }
    match &expr.kind {
        BoundExprKind::Literal(value) => integer_digits(value),
        BoundExprKind::Negate(inner) => match &inner.kind {
            BoundExprKind::Literal(_) => {
                literal_digits(inner).map(|digits| if digits == 5 { 6 } else { digits })
            }
            _ => literal_digits(inner),
        },
        _ => None,
    }
}

/// The number of decimal digits of an integer value, at least one, or `None` when the value
/// is not an integer.
fn integer_digits(value: &Value) -> Option<u8> {
    let value: i128 = match value {
        Value::I8(v) => i128::from(*v),
        Value::I16(v) => i128::from(*v),
        Value::I32(v) => i128::from(*v),
        Value::I64(v) => i128::from(*v),
        _ => return None,
    };
    let mut rest = value.unsigned_abs();
    let mut digits: u8 = 1;
    while rest >= 10 {
        rest /= 10;
        digits = digits.saturating_add(1);
    }
    Some(digits)
}

/// Converts an operand of a run to the types of the run's columns.
///
/// A `SELECT` converts the expressions of its projection; a nested run, whose rows are
/// deduplicated in its own types, is converted by a projection above it.
fn convert_operand(operand: Typed, columns: &[TypeInfo]) -> LogicalPlan {
    let unchanged = operand
        .plan
        .schema()
        .columns
        .iter()
        .zip(columns)
        .all(|(column, ty)| column.ty == *ty);
    if unchanged {
        return operand.plan;
    }
    match operand.plan {
        plan @ LogicalPlan::SetOp { .. } => project_above(plan, columns, operand.line),
        plan if deduplicates(&plan) => project_above(plan, columns, operand.line),
        plan => convert_projection(plan, columns, operand.line),
    }
}

/// Whether a `SELECT` removes duplicates above its projection: its rows are then
/// deduplicated in its own types, and converted above the `Distinct`
/// (`tests/bind_setop.rs`, `a_distinct_branch_is_converted_above_its_distinct`).
fn deduplicates(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Distinct(_) => true,
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Sort { input, .. } => deduplicates(input),
        _ => false,
    }
}

/// A `Project` above an operand that converts its columns.
fn project_above(plan: LogicalPlan, columns: &[TypeInfo], line: u32) -> LogicalPlan {
    let exprs: Vec<BoundProjection> = source_columns(plan.schema())
        .into_iter()
        .zip(columns)
        .map(|(binding, ty)| {
            let name = binding.name.clone();
            let column = BoundExpr {
                ty: binding.ty.clone(),
                kind: BoundExprKind::ColumnRef(binding),
                line,
            };
            BoundProjection {
                expr: convert_to(column, ty),
                name,
            }
        })
        .collect();
    let schema = projected_schema(&exprs, columns);
    LogicalPlan::Project {
        input: Box::new(plan),
        exprs,
        schema,
    }
}

/// The schema of a converted projection: its names, the run's types.
fn projected_schema(exprs: &[BoundProjection], columns: &[TypeInfo]) -> OutputSchema {
    OutputSchema {
        columns: exprs
            .iter()
            .zip(columns)
            .map(|(projection, ty)| OutputColumn {
                name: projection.name.clone(),
                ty: ty.clone(),
            })
            .collect(),
    }
}

/// Converts the expressions of the `Project` a `SELECT` ends with.
fn convert_projection(plan: LogicalPlan, columns: &[TypeInfo], line: u32) -> LogicalPlan {
    match plan {
        LogicalPlan::Project { input, exprs, .. } => {
            let exprs: Vec<BoundProjection> = exprs
                .into_iter()
                .zip(columns)
                .map(|(projection, ty)| BoundProjection {
                    expr: convert_expr(projection.expr, ty),
                    name: projection.name,
                })
                .collect();
            let schema = projected_schema(&exprs, columns);
            LogicalPlan::Project {
                input,
                exprs,
                schema,
            }
        }
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(convert_projection(*input, columns, line)),
            predicate,
        },
        LogicalPlan::Limit { input, top } => LogicalPlan::Limit {
            input: Box::new(convert_projection(*input, columns, line)),
            top,
        },
        LogicalPlan::Distinct(input) => {
            LogicalPlan::Distinct(Box::new(convert_projection(*input, columns, line)))
        }
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(convert_projection(*input, columns, line)),
            keys,
        },
        other => project_above(other, columns, line),
    }
}

/// One expression of a projection, converted to `target`: a bare `NULL` takes the type,
/// another expression is wrapped in a `Convert` when its type differs.
fn convert_expr(mut expr: BoundExpr, target: &TypeInfo) -> BoundExpr {
    if matches!(expr.kind, BoundExprKind::Literal(Value::Null)) {
        expr.ty = target.clone();
        return expr;
    }
    convert_to(expr, target)
}

/// The span of a `QueryBody`.
fn span_of(body: &QueryBody) -> Span {
    match body {
        QueryBody::Select(spec) => spec.span,
        QueryBody::SetOp { span, .. } => *span,
        QueryBody::Nested(_, span) => *span,
    }
}
