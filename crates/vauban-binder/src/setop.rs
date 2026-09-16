//! `UNION [ALL]`, `EXCEPT` and `INTERSECT`: the operands' widths (205) and the common type
//! of each column.
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

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{QueryBody, SelectStatement, SetOp as AstSetOp, Span};
use vauban_types::{TypeInfo, implicit_result_type};

use crate::bound::{
    BoundExpr, BoundExprKind, BoundProjection, LogicalPlan, OutputColumn, OutputSchema, SetOpKind,
};
use crate::context::BindContext;
use crate::errors::line_of;
use crate::expr::Scope;
use crate::query::bind_select_with_parent;

/// Binds a `QueryBody::SetOp` into a [`LogicalPlan::SetOp`].
///
/// The operands are bound recursively, then the output schema is built column by column
/// from the common type of each pair. A conversion is inserted in the operand whose
/// column type differs from the common one.
///
/// The ORDER BY of the statement is left to `query.rs`, which calls
/// `sort::bind_order_by` on the result of this function.
///
/// # Errors
///
/// - **205** when the two operands do not have the same number of columns;
/// - errors of `types::implicit_result_type` for a pair that shares no common type.
pub(crate) fn bind_set_op(
    body: &QueryBody,
    stmt: &SelectStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let (op, all, left_body, right_body, span) = match body {
        QueryBody::SetOp {
            op,
            all,
            left,
            right,
            span,
        } => (op, all, left, right, span),
        _ => unreachable!("bind_set_op called on a non-SetOp body"),
    };

    let left = bind_operand(left_body, ctx)?;
    let right = bind_operand(right_body, ctx)?;

    let left_schema = left.schema().clone();
    let right_schema = right.schema().clone();
    if left_schema.columns.len() != right_schema.columns.len() {
        return Err(SqlError::set_operator_column_count_mismatch().with_line(line_of(span)));
    }

    let mut output_columns = Vec::with_capacity(left_schema.columns.len());
    let mut need_left_convert = Vec::with_capacity(left_schema.columns.len());
    let mut need_right_convert = Vec::with_capacity(left_schema.columns.len());

    for (left_col, right_col) in left_schema.columns.iter().zip(right_schema.columns.iter()) {
        let common = implicit_result_type(&left_col.ty, &right_col.ty)?;
        output_columns.push(OutputColumn {
            name: left_col.name.clone(),
            ty: TypeInfo {
                ty: common.ty,
                nullable: left_col.ty.nullable || right_col.ty.nullable,
                collation: common.collation,
            },
        });
        need_left_convert.push(left_col.ty.ty != common.ty);
        need_right_convert.push(right_col.ty.ty != common.ty);
    }

    let plan = LogicalPlan::SetOp {
        op: match op {
            AstSetOp::Union => SetOpKind::Union,
            AstSetOp::Except => SetOpKind::Except,
            AstSetOp::Intersect => SetOpKind::Intersect,
        },
        all: *all,
        left: Box::new(apply_conversions(left, &need_left_convert, &output_columns)),
        right: Box::new(apply_conversions(
            right,
            &need_right_convert,
            &output_columns,
        )),
        schema: OutputSchema {
            columns: output_columns,
        },
    };

    // ORDER BY is left to query.rs which calls sort::bind_order_by on the result.
    let _ = stmt;
    Ok(plan)
}

/// Applies conversions to the columns of the operand plan that need them.
fn apply_conversions(
    plan: LogicalPlan,
    need_convert: &[bool],
    output_columns: &[OutputColumn],
) -> LogicalPlan {
    if need_convert.iter().all(|b| !b) {
        return plan;
    }
    wrap_project(plan, need_convert, output_columns)
}

/// Walks the plan to find the Project and wraps its expressions in Convert nodes.
fn wrap_project(
    plan: LogicalPlan,
    need_convert: &[bool],
    output_columns: &[OutputColumn],
) -> LogicalPlan {
    match plan {
        LogicalPlan::Project {
            input,
            mut exprs,
            schema,
        } => {
            let new_exprs = exprs
                .drain(..)
                .enumerate()
                .map(|(i, projection)| {
                    if let Some(true) = need_convert.get(i)
                        && projection.expr.ty.ty != output_columns[i].ty.ty
                    {
                        BoundProjection {
                            expr: convert_expr(projection.expr, &output_columns[i].ty),
                            name: projection.name,
                        }
                    } else {
                        projection
                    }
                })
                .collect();
            let new_schema = OutputSchema {
                columns: schema
                    .columns
                    .into_iter()
                    .enumerate()
                    .map(|(i, col)| {
                        if let Some(target) = output_columns.get(i) {
                            OutputColumn {
                                name: col.name,
                                ty: target.ty.clone(),
                            }
                        } else {
                            col
                        }
                    })
                    .collect(),
            };
            LogicalPlan::Project {
                input,
                exprs: new_exprs,
                schema: new_schema,
            }
        }
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(wrap_project(*input, need_convert, output_columns)),
            predicate,
        },
        LogicalPlan::Limit { input, top } => LogicalPlan::Limit {
            input: Box::new(wrap_project(*input, need_convert, output_columns)),
            top,
        },
        LogicalPlan::Distinct(input) => {
            LogicalPlan::Distinct(Box::new(wrap_project(*input, need_convert, output_columns)))
        }
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(wrap_project(*input, need_convert, output_columns)),
            keys,
        },
        other @ LogicalPlan::SetOp { .. } => other,
        other => other,
    }
}

/// Wraps an expression in a `Convert` node towards `target`.
fn convert_expr(expr: BoundExpr, target: &TypeInfo) -> BoundExpr {
    let line = expr.line;
    BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(expr),
            style: None,
            try_: false,
        },
        ty: target.clone(),
        line,
    }
}

/// Binds one operand of a set operation into a [`LogicalPlan`].
///
/// A `QueryBody::Select` is bound through `bind_select_with_parent`; a `SetOp` recurses
/// into [`bind_set_op`].
fn bind_operand(body: &QueryBody, ctx: &BindContext<'_>) -> SqlResult<LogicalPlan> {
    let span = span_of(body);
    let synthetic = SelectStatement {
        with: None,
        body: body.clone(),
        order_by: Vec::new(),
        offset_fetch: None,
        for_clause: None,
        span,
    };
    match body {
        QueryBody::Select(_) => bind_select_with_parent(&synthetic, ctx, &Scope::empty()),
        QueryBody::SetOp { .. } => bind_set_op(&synthetic.body, &synthetic, ctx),
        QueryBody::Nested(inner, _) => bind_operand(inner, ctx),
    }
}

/// The span of a `QueryBody`.
fn span_of(body: &QueryBody) -> Span {
    match body {
        QueryBody::Select(spec) => spec.span,
        QueryBody::SetOp { span, .. } => *span,
        QueryBody::Nested(_, span) => *span,
    }
}
