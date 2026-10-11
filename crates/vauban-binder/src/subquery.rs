//! Subqueries in the positions T-SQL allows one: `EXISTS (…)`, `IN (SELECT …)`, the
//! quantified comparison `v > ALL (SELECT …)`, the scalar `(SELECT …)` of a value position,
//! and the derived table of a `FROM`.
//!
//! Each entry point receives the query and the scope of the enclosing query, builds a
//! child scope whose parent link leads to that enclosing scope for correlated references,
//! binds the inner query recursively through
//! [`bind_select_with_parent`](crate::query::bind_select_with_parent),
//! and wraps the result in the bound variant the position calls for.
//!
//! # Correlation level
//!
//! The bound plan does **not** carry a correlation depth yet: the `Exists`, `ScalarSubquery`
//! and `InSubquery` variants of [`BoundExprKind`] hold a [`LogicalPlan`] whose `Filter`
//! nodes may reference columns of the enclosing plan (`WHERE EXISTS (SELECT 1 FROM b WHERE
//! b.k = a.k)`, where `a.k` is a column of the outer `Scan`). The planner reads the
//! correlation level off the plan when it decides between decorrelation and row-by-row
//! evaluation.
//!
//! # Errors
//!
//! - 116 when a subquery not introduced by `EXISTS` carries more than one expression in its
//!   select list;
//! - 8158 when a derived table has more columns than its column list names;
//! - 8159 when a derived table has fewer columns than its column list names;
//! - the errors of the inner query: 207, 209, 4104, 208 and everything
//!   `bind_select` raises.

use vauban_catalog::ColumnId;
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    BinaryOp as AstBinaryOp, Expr, Ident, Quantifier, SelectStatement, Span, TableRef,
};
use vauban_types::{SqlType, TypeInfo, Value};

use crate::bound::{
    BoundCaseArm, BoundExpr, BoundExprKind, BoundProjection, BoundSubquery, ColumnBinding,
    CompareOp, LogicalPlan, OutputColumn, OutputSchema,
};
use crate::context::BindContext;
use crate::errors::line_of;
use crate::expr::{Scope, bind_expr, compare_node, convert_to};
use crate::insert::{common_value_type, is_untyped_null};
use crate::query::bind_select_with_parent;

/// Binds a derived table of a `FROM` into a [`LogicalPlan::Subquery`]: the subquery
/// `(SELECT …) AS d [(c1, c2)]`, or the table value constructor `(VALUES (…), (…)) AS d
/// (c1, c2)` that [`bind_values_derived`] reads.
///
/// The inner query is bound through [`bind_select`] with no parent scope, since a derived
/// table cannot reference columns of the enclosing query (SQL Server refuses `SELECT 1 FROM
/// (SELECT c FROM #t AS d WHERE d.k = a.k) AS d2` by error 4104). The alias is required:
/// the parser does not accept a derived table without one (error 102), so the binder is
/// asked what a missing alias raises.
///
/// # Column list
///
/// When a column list is written, `(c1, c2)`, its length must match the width of the inner
/// plan: 8158 when there are more columns in the plan than names, 8159 when there are fewer.
/// Columns are then renamed: the schema of the `Subquery` node carries the names from the
/// list.
///
/// # Errors
///
/// - 8158 or 8159 when the column list length differs from the plan width;
/// - the errors of the inner query.
pub(crate) fn bind_derived(
    reference: &TableRef,
    _statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let (query, alias, columns, span) = match reference {
        TableRef::Derived {
            query,
            alias,
            columns,
            span,
        } => (query, alias, columns, span),
        TableRef::Values {
            rows,
            alias,
            columns,
            span,
        } => return bind_values_derived(rows, alias.as_ref(), columns, span, ctx),
        _ => {
            return Err(crate::query::bug(
                "bind_derived: expected TableRef::Derived or TableRef::Values",
            ));
        }
    };
    let inner = bind_select_with_parent(query, ctx, &Scope::empty())?;
    let inner_schema = inner.schema().clone();

    let alias = alias
        .as_ref()
        .map(|a| a.value.clone())
        .unwrap_or_else(String::new);

    let schema = if columns.is_empty() {
        inner_schema
    } else {
        let n_cols = columns.len();
        let n_inner = inner_schema.columns.len();
        if n_cols > n_inner {
            return Err(
                SqlError::derived_table_fewer_columns_than_column_list(&alias)
                    .with_line(line_of(span)),
            );
        }
        if n_cols < n_inner {
            return Err(
                SqlError::derived_table_more_columns_than_column_list(&alias)
                    .with_line(line_of(span)),
            );
        }
        OutputSchema {
            columns: columns
                .iter()
                .zip(&inner_schema.columns)
                .map(|(ident, col)| OutputColumn {
                    name: ident.value.clone(),
                    ty: col.ty.clone(),
                })
                .collect(),
        }
    };

    Ok(LogicalPlan::Subquery {
        input: Box::new(inner),
        alias,
        schema,
    })
}

/// Binds a table value constructor written where a derived table stands,
/// `(VALUES (…), (…)) AS v (c1, c2)`, into a [`LogicalPlan::Values`] under the
/// [`LogicalPlan::Subquery`] that carries the alias.
///
/// # The type of a column
///
/// A constructor has this in common with a set operation and not with an `INSERT`: there
/// is no target column to take a type from. So each column takes the common type of the
/// expressions written at its position ([`common_value_type`]), and each value is put
/// under a `Convert` towards it. A position holding the untyped `NULL` literal and
/// nothing else takes `int`. The column is nullable when the position holds one such
/// literal or one nullable value, which
/// `tests/bind_subquery.rs` asserts against the type the pair takes under a set
/// operation (`a_values_column_takes_the_type_its_pair_takes_under_a_set_operation`).
///
/// # The order of the checks
///
/// Rows of differing widths are refused before the column list is looked at, so that
/// `(VALUES (1), (2, 3)) v (c, d)` answers 10709 rather than an arity error, and the
/// column list is looked at before the expressions are bound.
///
/// # Errors
///
/// - **10709** when the rows are not of one width;
/// - **8155** when the alias carries a column list of length zero, a constructor leaving
///   its columns unnamed;
/// - **8158** when the rows are wider than the column list, **8159** when they are
///   narrower;
/// - **206** when two values of one position have no type in common;
/// - the errors of the expressions.
fn bind_values_derived(
    rows: &[Vec<Expr>],
    alias: Option<&Ident>,
    columns: &[Ident],
    span: &Span,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    let line = line_of(span);
    let alias = alias.map(|a| a.value.clone()).unwrap_or_default();
    let width = rows.first().map_or(0, Vec::len);
    if rows.iter().any(|row| row.len() != width) {
        return Err(SqlError::table_value_constructor_rows_differ().with_line(line));
    }
    if columns.is_empty() {
        return Err(SqlError::no_column_name_in_derived_table(1, &alias).with_line(line));
    }
    if columns.len() < width {
        return Err(SqlError::derived_table_more_columns_than_column_list(&alias).with_line(line));
    }
    if columns.len() > width {
        return Err(SqlError::derived_table_fewer_columns_than_column_list(&alias).with_line(line));
    }

    let mut bound_rows: Vec<Vec<BoundExpr>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut bound_row = Vec::with_capacity(width);
        for expr in row {
            bound_row.push(bind_expr(expr, ctx, &Scope::empty())?);
        }
        bound_rows.push(bound_row);
    }

    let mut output_columns = Vec::with_capacity(width);
    for (position, ident) in columns.iter().enumerate() {
        let nullable = bound_rows
            .iter()
            .any(|row| is_untyped_null(&row[position]) || row[position].ty.nullable);
        let ty = match common_value_type(&bound_rows, position, line)? {
            Some(common) => TypeInfo {
                ty: common.ty,
                nullable,
                collation: common.collation,
            },
            None => TypeInfo::new(SqlType::Int, nullable),
        };
        output_columns.push(OutputColumn {
            name: ident.value.clone(),
            ty,
        });
    }
    let schema = OutputSchema {
        columns: output_columns,
    };

    let converted: Vec<Vec<BoundExpr>> = bound_rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .zip(&schema.columns)
                .map(|(value, column)| convert_to(value, &column.ty))
                .collect()
        })
        .collect();

    Ok(LogicalPlan::Subquery {
        input: Box::new(LogicalPlan::Values {
            rows: converted,
            schema: schema.clone(),
        }),
        alias,
        schema,
    })
}

/// Binds an `EXISTS (SELECT …)` into [`BoundExprKind::Exists`].
///
/// The inner query may have any number of columns in its select list: `EXISTS` is exempt
/// from error 116. The result is a predicate (`is_predicate() == true`).
pub(crate) fn bind_exists(
    query: &SelectStatement,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let child_scope = Scope::empty().inside(scope.clone());
    let plan = bind_select_with_parent(query, ctx, &child_scope)?;
    Ok(BoundExpr {
        kind: BoundExprKind::Exists(BoundSubquery::new(plan)),
        ty: vauban_types::TypeInfo::new(vauban_types::SqlType::Bit, false),
        line: line_of(&query.span),
    })
}

/// Binds a `(SELECT …)` written where a value is expected into
/// [`BoundExprKind::ScalarSubquery`].
///
/// The inner query must carry **exactly one** column in its select list: a second column
/// answers 116. The type of the result is the type of that single column, made **nullable**
/// regardless of the nullability of the column: zero rows answer `NULL`.
///
/// # Errors
///
/// - 116 when the select list has more than one column.
pub(crate) fn bind_scalar(
    query: &SelectStatement,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let child_scope = Scope::empty().inside(scope.clone());
    let plan = bind_select_with_parent(query, ctx, &child_scope)?;
    let schema = plan.schema();
    if schema.columns.len() != 1 {
        return Err(SqlError::only_one_expression_in_subquery().with_line(line_of(&query.span)));
    }
    let mut ty = schema.columns[0].ty.clone();
    ty.nullable = true;
    Ok(BoundExpr {
        kind: BoundExprKind::ScalarSubquery(BoundSubquery::new(plan)),
        ty,
        line: line_of(&query.span),
    })
}

/// Binds an `e [NOT] IN (SELECT …)` into [`BoundExprKind::InSubquery`].
///
/// The inner query must carry **exactly one** column in its select list (116 otherwise).
/// The tested value `e` and the column of the subquery are brought to their common type
/// ([`vauban_types::implicit_result_type`]): `e` is wrapped in a [`BoundExprKind::Convert`]
/// when it is not of that type, and the column under a projection that converts it
/// ([`convert_produced_column`]).
///
/// # Errors
///
/// - 116 when the select list has more than one column;
/// - 206 when the tested value and the subquery column have no common type.
pub(crate) fn bind_in_subquery(
    expr: &Expr,
    query: &SelectStatement,
    negated: bool,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(&query.span);
    let child_scope = Scope::empty().inside(scope.clone());
    let plan = bind_select_with_parent(query, ctx, &child_scope)?;
    let schema = plan.schema();
    if schema.columns.len() != 1 {
        return Err(SqlError::only_one_expression_in_subquery().with_line(line));
    }
    let subquery_ty = schema.columns[0].ty.clone();
    let expr = bind_expr(expr, ctx, scope)?;
    let common = vauban_types::implicit_result_type(&expr.ty, &subquery_ty)
        .map_err(|e| if e.line == 0 { e.with_line(line) } else { e })?;
    let nullable = expr.ty.nullable || subquery_ty.nullable;
    let plan = convert_produced_column(plan, &common, line);
    Ok(BoundExpr {
        kind: BoundExprKind::InSubquery {
            expr: Box::new(crate::expr::convert_to(expr, &common)),
            plan: BoundSubquery::new(plan),
            negated,
        },
        ty: vauban_types::TypeInfo::new(vauban_types::SqlType::Bit, nullable),
        line,
    })
}

/// Puts the single column `plan` produces under a [`BoundExprKind::Convert`] towards
/// `common`, through a [`LogicalPlan::Project`] over `plan`; `plan` is returned as is when
/// its column already has the type of `common`.
///
/// The membership test then compares two values of one type, whether the planner turns
/// it into a semi-join or evaluates the subquery row by row: `d IN (SELECT i …)`, `d`
/// `decimal(5,2)` and `i` `int`, compares two decimals (`tests/bind_subquery.rs`,
/// `in_subquery_converts_the_produced_column_to_the_common_type`).
fn convert_produced_column(plan: LogicalPlan, common: &TypeInfo, line: u32) -> LogicalPlan {
    let [produced] = plan.schema().columns.as_slice() else {
        return plan;
    };
    if produced.ty.ty == common.ty {
        return plan;
    }
    let produced = produced.clone();
    let column = BoundExpr {
        kind: BoundExprKind::ColumnRef(ColumnBinding {
            column: ColumnId(1),
            index: 0,
            name: produced.name.clone(),
            ty: produced.ty.clone(),
            depth: 0,
        }),
        ty: produced.ty,
        line,
    };
    let converted = convert_to(column, common);
    let schema = OutputSchema {
        columns: vec![OutputColumn {
            name: produced.name.clone(),
            ty: converted.ty.clone(),
        }],
    };
    LogicalPlan::Project {
        input: Box::new(plan),
        exprs: vec![BoundProjection {
            expr: converted,
            name: produced.name,
        }],
        schema,
    }
}

/// Binds `v <op> ANY | SOME | ALL (SELECT …)`.
///
/// `= ANY` (and `= SOME`) is `v IN (SELECT …)` and `<> ALL` is `v NOT IN (SELECT …)`,
/// three-valued logic included: both are bound by [`bind_in_subquery`], so that the
/// planner decorrelates them where it decorrelates an `IN`.
///
/// The other forms keep the shape of an `IN` over a rewritten subquery. Each row
/// `s` of the subquery is mapped to the outcome of `v <op> s`, `CASE WHEN v <op> s THEN 1
/// WHEN NOT (v <op> s) THEN 0 END`, `NULL` for unknown; then
///
/// - `v <op> ANY (S)` is `1 IN (outcomes)`: true when one comparison holds; otherwise
///   unknown when one comparison is unknown; otherwise false, an empty `S` included;
/// - `v <op> ALL (S)` is `0 NOT IN (outcomes)`: false when one comparison fails; otherwise
///   unknown when one comparison is unknown; otherwise true, an empty `S` included, with
///   `v` `NULL` or not.
///
/// `v` is bound one query deeper than where it is written, since it is read inside the
/// rewritten subquery: its columns carry one more level of
/// [`ColumnBinding::depth`], which makes the subquery correlated
/// (`tests/bind_subquery.rs`, `a_quantified_comparison_reads_the_tested_value_one_query_deeper`).
///
/// # Errors
///
/// - 116 when the select list has more than one column;
/// - 206 when the tested value and the subquery column have no common type;
/// - the errors of the inner query and of `v`.
pub(crate) fn bind_quantified(
    expr: &Expr,
    op: AstBinaryOp,
    quantifier: Quantifier,
    query: &SelectStatement,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let op = match op {
        AstBinaryOp::Eq => CompareOp::Eq,
        AstBinaryOp::Ne => CompareOp::Ne,
        AstBinaryOp::Lt => CompareOp::Lt,
        AstBinaryOp::Le | AstBinaryOp::NotGt => CompareOp::Le,
        AstBinaryOp::Gt => CompareOp::Gt,
        AstBinaryOp::Ge | AstBinaryOp::NotLt => CompareOp::Ge,
        _ => {
            return Err(crate::query::bug(
                "bind_quantified: the operator of a quantified predicate is a comparison",
            ));
        }
    };
    let all = quantifier == Quantifier::All;
    match (op, all) {
        (CompareOp::Eq, false) => return bind_in_subquery(expr, query, false, ctx, scope),
        (CompareOp::Ne, true) => return bind_in_subquery(expr, query, true, ctx, scope),
        _ => {}
    }
    let line = line_of(&query.span);
    let child_scope = Scope::empty().inside(scope.clone());
    let plan = bind_select_with_parent(query, ctx, &child_scope)?;
    let [produced] = plan.schema().columns.as_slice() else {
        return Err(SqlError::only_one_expression_in_subquery().with_line(line));
    };
    let produced = produced.clone();
    let tested = bind_expr(expr, ctx, &child_scope)?;
    let candidate = BoundExpr {
        kind: BoundExprKind::ColumnRef(ColumnBinding {
            column: ColumnId(1),
            index: 0,
            name: produced.name.clone(),
            ty: produced.ty.clone(),
            depth: 0,
        }),
        ty: produced.ty.clone(),
        line,
    };
    let comparison = compare_node(op, tested, candidate, line)?;
    let nullable = comparison.ty.nullable;
    let int = |value: i32| BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(value)),
        ty: TypeInfo::new(SqlType::Int, false),
        line,
    };
    let fails = BoundExpr {
        kind: BoundExprKind::Not(Box::new(comparison.clone())),
        ty: comparison.ty.clone(),
        line,
    };
    let outcome_ty = TypeInfo::new(SqlType::Int, true);
    let outcome = BoundExpr {
        kind: BoundExprKind::Case {
            operand: None,
            arms: vec![
                BoundCaseArm {
                    when: comparison,
                    then: int(1),
                },
                BoundCaseArm {
                    when: fails,
                    then: int(0),
                },
            ],
            else_: None,
        },
        ty: outcome_ty.clone(),
        line,
    };
    let outcomes = LogicalPlan::Project {
        input: Box::new(plan),
        exprs: vec![BoundProjection {
            expr: outcome,
            name: String::new(),
        }],
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: String::new(),
                ty: outcome_ty,
            }],
        },
    };
    Ok(BoundExpr {
        kind: BoundExprKind::InSubquery {
            expr: Box::new(int(if all { 0 } else { 1 })),
            plan: BoundSubquery::new(outcomes),
            negated: all,
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line,
    })
}
