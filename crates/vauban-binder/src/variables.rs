//! The variables of a batch: `DECLARE @x <type> [= e]`, `SET @x = e`, `SELECT @x = e`, and
//! the scope that holds what the batch declared, [`BatchVariables`].
//!
//! # Who declares
//!
//! The binder binds **one statement at a time** against a [`VariableScope`] it reads and
//! does not write. [`bind_declare`] answers a [`BoundStatement::Declare`] and enters
//! nothing into the scope: it is the caller (the session, or a test) that, on that answer,
//! calls [`BatchVariables::declare`] for each declaration before it binds the next
//! statement. There is no interior mutability in [`BatchVariables`], so a
//! `&dyn VariableScope` handed to a [`BindContext`] does not change under the binder.
//!
//! A consequence the caller relies on: within one `DECLARE`, a later initial value does not
//! see an earlier item of the same statement. `DECLARE @a int = 1, @b int = @a;` answers
//! 137 on `@a` (`tests/bind_variables.rs`,
//! `a_declaration_does_not_see_the_items_of_its_own_statement`).
//!
//! # What each form binds to
//!
//! | Written | Bound to |
//! |---|---|
//! | `DECLARE @x int, @s varchar(10) = 'a'` | `Declare([..])`, one entry per item, in order |
//! | `SET @x = e`, `SET @x = (SELECT …)` | `SetVariable { name, value }` |
//! | `SET @x += e` (and `-=`, `*=`, `/=`, `%=`, `&=`, `|=`, `^=`) | `SetVariable` of `@x <op> e` |
//! | `SELECT @x = e` | `SetVariable { name, value }` |
//! | `SELECT @x = e, @y = f` | `Block([SetVariable, SetVariable])`, in written order |
//! | `SELECT @x = e, @y = f FROM t …` | `SelectAssign { input, assignments }`, in written order |
//!
//! A value is bound, then wrapped in a [`BoundExprKind::Convert`] towards the declared type
//! when its own type differs, so that the executor stores the declared type and nothing
//! else. The pair is checked first: a value whose type has no implicit conversion to the
//! declared one is error **206**, which names the value's type before the declared one
//! (`DECLARE @x int = NEWID();` names `uniqueidentifier` then `int`, and
//! `DECLARE @g uniqueidentifier = 1;` names `int` then `uniqueidentifier`). An untyped
//! `NULL` is exempt from that check, `uniqueidentifier` included
//! (`tests/bind_variables.rs`, `an_initial_value_of_an_incompatible_type_is_206`).
//!
//! # `SELECT @x = e` is not a query
//!
//! The statement stores a value and answers **no result set**: the bound form has no
//! output column (`tests/bind_variables.rs`, `select_assignment_produces_no_result_column`).
//! Several targets assign left to right, each seeing the targets before it:
//! `SELECT @x = 1, @y = @x;` leaves `@y` at 1, and `SELECT @y = @x, @x = 1;` with `@x` at 5
//! leaves `@y` at 5. A select list that both assigns and returns a column
//! (`SELECT @x = 1, 2;`, `SELECT 2, @x = 1;`) is error **141**, raised after the 137 of an
//! undeclared target: `SELECT @z = 1, 2;` without a `DECLARE` answers 137.
//!
//! # `SELECT @x = e FROM t` assigns from a plan
//!
//! With a `FROM`, the statement binds to a [`BoundStatement::SelectAssign`]: the values
//! become the select list of the same statement read as a query, which `query.rs` binds
//! with the scope of the `FROM`, its `WHERE`, `TOP` and `ORDER BY`; the `Project` of that
//! plan is then taken out, its input is the `input` of the variant and its expressions,
//! each wrapped in a `Convert` towards the declared type, are the values of the
//! `assignments` (`tests/bind_variables.rs`,
//! `select_assign_with_from_binds_input_and_targets`,
//! `select_assign_with_from_keeps_where_and_order_by`). The targets are checked before the
//! query is bound: `SELECT @z = nosuch FROM t` without a `DECLARE` is 137, and 207 once
//! `@z` is declared (`select_assign_with_from_checks_137_before_141`). The semantics the
//! variant carries are written on it in `bound/mod.rs`.
//!
//! The forms that need a plan and have no `FROM` — a `WHERE`, a `TOP`, an `ORDER BY` over
//! the one row of a `SELECT` without `FROM` — and `SELECT DISTINCT @x = a FROM t`, whose
//! deduplication sits between the values and the assignment, are **not bound here**: they
//! answer an internal error naming the clause (`tests/bind_variables.rs`,
//! `select_assign_without_from_defers_the_clauses_that_need_a_plan`). For the record,
//! `SELECT @x = 1 WHERE 1 = 0;` and `SELECT TOP (0) @x = 1;` assign nothing. `DISTINCT`
//! without a `FROM` is accepted: over the single row it removes nothing.
//!
//! # Out of scope
//!
//! `DECLARE @t TABLE (…)` is not bound: it answers an internal error naming the form
//! (`tests/bind_variables.rs`, `a_table_variable_is_out_of_scope`). The parser reads
//! `DECLARE @c CURSOR` as a scalar variable of a type spelled `CURSOR`, which is 2715 here;
//! the cursor item of the AST, were it built, answers an internal error too.

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    AliasStyle, AssignOp, AssignTarget, BinaryOp as AstBinaryOp, DeclareItem, DeclareStatement,
    Expr, QueryBody, QuerySpec, SelectItem, SelectStatement, SetStatement, SetValue, Span,
};
use vauban_types::{SqlType, TypeInfo, Value, implicit_result_type};

use crate::bound::{
    BoundDeclaration, BoundExpr, BoundExprKind, BoundProjection, BoundStatement, LogicalPlan,
};
use crate::context::{BindContext, VariableScope};
use crate::datatype::resolve_data_type;
use crate::errors::{line_of, on_the_statement};
use crate::expr::{Scope, bind_expr};
use crate::query::{bind_select, bug, not_implemented};
use crate::subquery;

/// The variables a batch has declared so far, with their declared types.
///
/// The scope the session builds for a batch: empty at the start, one entry per
/// [`BoundDeclaration`] the session enters with [`BatchVariables::declare`] after the
/// binder answered a [`BoundStatement::Declare`]. Names are looked up without regard to
/// case, ASCII-wise: `@x` and `@X` are one variable (`tests/bind_variables.rs`,
/// `the_scope_is_case_insensitive`).
///
/// The type is read through [`VariableScope`], the trait [`BindContext`] borrows; nothing
/// here is mutable behind a shared reference, so the scope does not change while a
/// statement is being bound.
#[derive(Debug, Clone, Default)]
pub struct BatchVariables {
    /// `(name as declared, declared type)`, in declaration order.
    declared: Vec<(String, TypeInfo)>,
}

impl BatchVariables {
    /// A scope with no variable.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enters `name` (`@` included) with its declared type.
    ///
    /// # Errors
    ///
    /// Error 134 when a variable of that name, in any case, is already in the scope. The
    /// binder raises the same 134 on a `DECLARE` of a name already in scope
    /// (`tests/bind_variables.rs`, `a_second_declare_of_the_same_name_is_134`), so a
    /// caller that declares what the binder accepted does not meet this error; it stands
    /// for a caller that skips the binder.
    pub fn declare(&mut self, name: &str, ty: TypeInfo) -> SqlResult<()> {
        if self.type_of(name).is_some() {
            return Err(SqlError::variable_already_declared(name));
        }
        self.declared.push((name.to_owned(), ty));
        Ok(())
    }
}

impl VariableScope for BatchVariables {
    fn type_of(&self, name: &str) -> Option<TypeInfo> {
        self.declared
            .iter()
            .find(|(declared, _)| declared.eq_ignore_ascii_case(name))
            .map(|(_, ty)| ty.clone())
    }
}

/// Binds a `DECLARE` into [`BoundStatement::Declare`], one entry per item in written order.
///
/// # Errors
///
/// - 134 when an item names a variable already in scope, or already declared by an
///   earlier item of the same statement; the name quoted is the one of the item, as
///   written.
/// - 2715 when the type is not one the engine knows; the position in the message counts
///   the items of the statement from 1.
/// - The errors of the initial value: 137 on a variable it reads, which the items of the
///   same statement are not, 206 when its type does not convert to the declared one.
/// - An internal error for a table variable or a cursor variable, which are not bound.
///
/// 134, 2715 and 206 carry the line of the statement.
pub(crate) fn bind_declare(
    stmt: &DeclareStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let line = line_of(&stmt.span);
    let mut declarations: Vec<BoundDeclaration> = Vec::with_capacity(stmt.items.len());
    for (index, item) in stmt.items.iter().enumerate() {
        let (name, ty, default) = match item {
            DeclareItem::Variable { name, ty, default } => (name, ty, default.as_deref()),
            DeclareItem::TableVariable { .. } => {
                return Err(not_implemented("DECLARE @t TABLE, a table variable (V2)"));
            }
            DeclareItem::Cursor { .. } => {
                return Err(not_implemented("DECLARE @c CURSOR, a cursor variable (V2)"));
            }
        };
        let already = ctx.variables.type_of(name).is_some()
            || declarations
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(name));
        if already {
            return Err(SqlError::variable_already_declared(name).with_line(line));
        }
        let position = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
        let ty = resolve_data_type(ty, position).map_err(|e| e.with_line(line))?;
        // A variable is nullable: it starts at NULL and may be set back to it.
        let ty = TypeInfo::new(ty, true);
        let value = match default {
            Some(expr) => Some(assigned_value(expr, &ty, ctx, line)?),
            None => None,
        };
        declarations.push(BoundDeclaration {
            name: name.clone(),
            ty,
            value,
        });
    }
    Ok(BoundStatement::Declare(declarations))
}

/// Binds a `SET @x = e` into [`BoundStatement::SetVariable`].
///
/// A compound operator is desugared before binding: `SET @x += e` binds as
/// `SET @x = @x + e`, and the operation is typed as the same expression written in a
/// `SELECT` would be (`tests/bind_variables.rs`,
/// `a_compound_assignment_is_the_arithmetic_on_the_variable`). `SET @x = (SELECT …)` hands
/// the query to `subquery.rs`.
///
/// # Errors
///
/// 137, state 1, when `@x` is not in scope; 206 when the value does not convert to the
/// declared type; the errors of the value itself. 137 and 206 carry the line of the
/// statement.
pub(crate) fn bind_set_variable(
    stmt: &SetStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let line = line_of(&stmt.span);
    let name = match &stmt.target {
        AssignTarget::Variable(name) => name,
        AssignTarget::Column(_) | AssignTarget::VariableAndColumn { .. } => {
            return Err(not_implemented("SET on a column, which is not a variable"));
        }
    };
    let Some(ty) = ctx.variables.type_of(name) else {
        return Err(SqlError::must_declare_scalar_variable_assigned(name).with_line(line));
    };
    let value = match &stmt.value {
        SetValue::Expr(expr) => match compound_operator(stmt.op) {
            None => assigned_value(expr, &ty, ctx, line)?,
            Some(op) => {
                let operation = Expr::Binary {
                    op,
                    op_span: stmt.span,
                    left: Box::new(Expr::Variable {
                        name: name.clone(),
                        span: stmt.span,
                    }),
                    right: Box::new(expr.clone()),
                    span: stmt.span,
                };
                assigned_value(&operation, &ty, ctx, line)?
            }
        },
        SetValue::Query(query) => {
            let bound = subquery::bind_scalar(query, ctx, &Scope::empty())
                .map_err(|e| on_the_statement(e, line))?;
            converted(bound, &ty, line)?
        }
    };
    Ok(BoundStatement::SetVariable {
        name: name.clone(),
        value,
    })
}

/// Whether the select list of `stmt` carries an assignment, `@x = e`, at its top level.
///
/// The dispatch of `statement.rs` routes such a statement to [`bind_select_assignment`]
/// instead of the query binder. An assignment inside a set operator is left to the query
/// binder, which refuses it.
pub(crate) fn is_assignment_select(stmt: &SelectStatement) -> bool {
    match &stmt.body {
        QueryBody::Select(spec) => spec.items.iter().any(|item| {
            matches!(
                item,
                SelectItem::Expr {
                    expr: Expr::Assign { .. },
                    ..
                }
            )
        }),
        QueryBody::SetOp { .. } | QueryBody::Nested(..) => false,
    }
}

/// Binds a `SELECT @x = e [, @y = f]` without a `FROM` into one
/// [`BoundStatement::SetVariable`], or a [`BoundStatement::Block`] of them in written
/// order, and the same list with a `FROM` into a [`BoundStatement::SelectAssign`].
///
/// See the module documentation for what is bound here and what is deferred.
///
/// # Errors
///
/// In this order: 137, state 1, on the first target not in scope; 141 when an item of the
/// list is not an assignment; an internal error naming the clause when the statement
/// needs a plan and has no `FROM` (`WHERE`, `TOP`, …), or is a `DISTINCT` over a `FROM`;
/// then, with a `FROM`, the errors of the query (207, 208, …); then the errors of each
/// value, 206 included. 137 carries the line of the assignment it names, 141 the line of
/// the statement.
pub(crate) fn bind_select_assignment(
    stmt: &SelectStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let QueryBody::Select(spec) = &stmt.body else {
        return Err(not_implemented(
            "SELECT @x = e under a set operator, which assigns a variable",
        ));
    };
    let mut assignments: Vec<(&str, &Expr, &Span, TypeInfo)> = Vec::new();
    let mut returns_a_column = false;
    for item in &spec.items {
        match item {
            SelectItem::Expr {
                expr:
                    Expr::Assign {
                        target,
                        value,
                        span,
                    },
                ..
            } => {
                let Some(ty) = ctx.variables.type_of(target) else {
                    return Err(SqlError::must_declare_scalar_variable_assigned(target)
                        .with_line(line_of(span)));
                };
                assignments.push((target, value, span, ty));
            }
            SelectItem::Wildcard(_)
            | SelectItem::QualifiedWildcard(_)
            | SelectItem::Expr { .. } => {
                returns_a_column = true;
            }
        }
    }
    if returns_a_column {
        return Err(SqlError::assignment_mixed_with_data_retrieval().with_line(line_of(&spec.span)));
    }
    if !spec.from.is_empty() {
        return bind_select_assign_from(stmt, spec, &assignments, ctx);
    }
    if let Some(clause) = clause_needing_a_plan(stmt) {
        return Err(not_implemented(&format!(
            "SELECT @x = e with {clause}, which assigns from a plan"
        )));
    }
    let mut bound: Vec<BoundStatement> = Vec::with_capacity(assignments.len());
    for (name, value, span, ty) in assignments {
        let value = assigned_value(value, &ty, ctx, line_of(span))?;
        bound.push(BoundStatement::SetVariable {
            name: name.to_owned(),
            value,
        });
    }
    if bound.len() == 1 {
        return Ok(bound.swap_remove(0));
    }
    Ok(BoundStatement::Block(bound))
}

/// Binds `SELECT @x = e, @y = f FROM t …` into a [`BoundStatement::SelectAssign`], the
/// targets having been checked by [`bind_select_assignment`].
///
/// The statement is read as the query `SELECT e, f FROM t …` and bound by
/// [`bind_select`], so that its `FROM`, `WHERE`, `TOP` and `ORDER BY` are bound once, in
/// `query.rs`; the `Project` of that plan is then detached ([`detach_projection`]).
///
/// # Errors
///
/// An internal error for `DISTINCT`; the errors of the query; 206, on the line of the
/// assignment, for a value whose type has no implicit conversion to the declared one.
fn bind_select_assign_from(
    stmt: &SelectStatement,
    spec: &QuerySpec,
    assignments: &[(&str, &Expr, &Span, TypeInfo)],
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    if spec.distinct {
        return Err(not_implemented(
            "SELECT DISTINCT @x = e FROM, which deduplicates the values before assigning them",
        ));
    }
    let items = assignments
        .iter()
        .map(|(_, value, _, _)| SelectItem::Expr {
            expr: (*value).clone(),
            alias: None,
            alias_style: AliasStyle::As,
        })
        .collect();
    let query = SelectStatement {
        with: stmt.with.clone(),
        body: QueryBody::Select(Box::new(QuerySpec {
            items,
            ..spec.clone()
        })),
        order_by: stmt.order_by.clone(),
        offset_fetch: stmt.offset_fetch.clone(),
        for_clause: stmt.for_clause.clone(),
        span: stmt.span,
    };
    let (input, values) = detach_projection(bind_select(&query, ctx)?)?;
    if values.len() != assignments.len() {
        return Err(bug(format!(
            "variables::bind_select_assign_from: {} assignments, {} projected values",
            assignments.len(),
            values.len()
        )));
    }
    let mut bound: Vec<(String, BoundExpr)> = Vec::with_capacity(assignments.len());
    for ((name, _, span, ty), projection) in assignments.iter().zip(values) {
        let value = check_convertible(projection.expr, ty, line_of(span))?;
        bound.push(((*name).to_owned(), convert_always(value, ty.ty)));
    }
    Ok(BoundStatement::SelectAssign {
        input: Box::new(input),
        assignments: bound,
    })
}

/// Splits the plan of a query into the plan under its `Project` and the expressions of
/// that `Project`, looking through the `Limit` a `TOP` puts above it.
///
/// `query.rs` builds the `Project` at the top of a plan, or directly under the `Limit`;
/// `sort.rs` keeps a `Sort` written without `DISTINCT` under it, so the `Sort` stays in
/// the plan handed back (`tests/bind_variables.rs`,
/// `select_assign_with_from_keeps_where_and_order_by`).
fn detach_projection(plan: LogicalPlan) -> SqlResult<(LogicalPlan, Vec<BoundProjection>)> {
    match plan {
        LogicalPlan::Limit { input, top } => {
            let (source, exprs) = detach_projection(*input)?;
            Ok((
                LogicalPlan::Limit {
                    input: Box::new(source),
                    top,
                },
                exprs,
            ))
        }
        LogicalPlan::Project { input, exprs, .. } => Ok((*input, exprs)),
        other => Err(bug(format!(
            "variables::detach_projection: no projection at the top of the plan of an \
             assignment: {other:?}"
        ))),
    }
}

/// The first clause of a `stmt` without `FROM` that makes the assignment need a plan,
/// named for the internal error, or `None` for a bare `SELECT [DISTINCT] @x = e`.
fn clause_needing_a_plan(stmt: &SelectStatement) -> Option<&'static str> {
    let QueryBody::Select(spec) = &stmt.body else {
        return Some("a set operator");
    };
    if stmt.with.is_some() {
        return Some("WITH");
    }
    if spec.where_.is_some() {
        return Some("WHERE");
    }
    if spec.top.is_some() {
        return Some("TOP");
    }
    if !spec.group_by.is_empty() {
        return Some("GROUP BY");
    }
    if spec.having.is_some() {
        return Some("HAVING");
    }
    if spec.into.is_some() {
        return Some("INTO");
    }
    if !stmt.order_by.is_empty() {
        return Some("ORDER BY");
    }
    if stmt.offset_fetch.is_some() {
        return Some("OFFSET");
    }
    if stmt.for_clause.is_some() {
        return Some("FOR");
    }
    None
}

/// The arithmetic a compound assignment operator stands for, `None` for `=`.
fn compound_operator(op: AssignOp) -> Option<AstBinaryOp> {
    match op {
        AssignOp::Set => None,
        AssignOp::AddAssign => Some(AstBinaryOp::Add),
        AssignOp::SubAssign => Some(AstBinaryOp::Sub),
        AssignOp::MulAssign => Some(AstBinaryOp::Mul),
        AssignOp::DivAssign => Some(AstBinaryOp::Div),
        AssignOp::ModAssign => Some(AstBinaryOp::Mod),
        AssignOp::BitAndAssign => Some(AstBinaryOp::BitAnd),
        AssignOp::BitOrAssign => Some(AstBinaryOp::BitOr),
        AssignOp::BitXorAssign => Some(AstBinaryOp::BitXor),
    }
}

/// Binds `expr` against the batch variables alone, then converts it to `target`.
///
/// `line` is the line of the statement, which the errors of `NAMES_THE_STATEMENT` and
/// the 206 of the pair take.
fn assigned_value(
    expr: &Expr,
    target: &TypeInfo,
    ctx: &BindContext<'_>,
    line: u32,
) -> SqlResult<BoundExpr> {
    let bound = bind_expr(expr, ctx, &Scope::empty()).map_err(|e| on_the_statement(e, line))?;
    converted(bound, target, line)
}

/// `value` converted to `target`: unchanged when its type is already `target`, wrapped in
/// a [`BoundExprKind::Convert`] otherwise.
///
/// # Errors
///
/// 206 when the two types have no implicit conversion, the value's type named first.
fn converted(value: BoundExpr, target: &TypeInfo, line: u32) -> SqlResult<BoundExpr> {
    let value = check_convertible(value, target, line)?;
    Ok(convert_to(value, target.ty))
}

/// `value` itself when its type has an implicit conversion to `target`, or when it is an
/// untyped `NULL`.
///
/// # Errors
///
/// 206 otherwise, the value's type named first, on `line`.
fn check_convertible(value: BoundExpr, target: &TypeInfo, line: u32) -> SqlResult<BoundExpr> {
    let untyped_null = matches!(value.kind, BoundExprKind::Literal(Value::Null));
    if !untyped_null && implicit_result_type(&value.ty, target).is_err() {
        return Err(
            SqlError::operand_type_clash(value.ty.ty.name(), target.ty.name()).with_line(line),
        );
    }
    Ok(value)
}

/// `expr` wrapped in the `Convert` node that takes it to `target`, or `expr` itself when it
/// already has that type. The result is nullable: it is stored into a variable, which is.
fn convert_to(expr: BoundExpr, target: SqlType) -> BoundExpr {
    if expr.ty.ty == target {
        return expr;
    }
    convert_always(expr, target)
}

/// `expr` wrapped in the `Convert` node that takes it to `target`, its own type being
/// `target` or not. The result is nullable, as [`convert_to`]'s is.
fn convert_always(expr: BoundExpr, target: SqlType) -> BoundExpr {
    let ty = TypeInfo::new(target, true);
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
