//! `UPDATE` and `DELETE`: the target, the `SET` list, the `WHERE`, and the errors they
//! raise at bind time (208, 4145, 207, 4104, 8102, 271, 264, 206, 8154). Over one table,
//! and over the `FROM` of a join, whose rules are the last section of this file.
//!
//! [`bind_update`] fills an [`UpdatePlan`] and [`bind_delete`] a [`DeletePlan`]. The rows
//! to touch are the same node for both: the `Scan` of the target, under the `Filter` of the
//! `WHERE` when one was written (`tests/bind_update_delete.rs`,
//! `delete_without_from_binds`, `delete_from_binds`, `update_with_where_filters_the_scan`).
//! Each `SET c = e` becomes one entry of `assignments`: the [`ColumnBinding`] of `c` as the
//! catalogue holds it, and `e` under a [`BoundExprKind::Convert`] towards the type of `c`
//! (`update_inserts_convert_to_column_type`). The columns of the target are in scope on the
//! right side of a `SET` and in the `WHERE`: `SET a = a + 1` reads the old value of `a`
//! (`update_set_reads_the_old_value`). A compound operator is desugared before binding, as
//! `variables.rs` does for `SET @x += e`: `SET a += 1` binds as `SET a = a + 1`
//! (`a_compound_assignment_reads_the_column`).
//!
//! # The order of the checks
//!
//! One statement can carry several faults; the one reported is decided by the order below,
//! each step exhausted before the next one starts. The tests of
//! `tests/bind_update_delete.rs` named after each pair pin it
//! (`the_where_is_bound_before_the_set_list`, `the_left_sides_are_resolved_before_the_values`,
//! `the_values_are_bound_before_the_identity_check`,
//! `identity_and_repeated_columns_are_checked_in_written_order`,
//! `the_conversion_is_checked_in_the_same_pass_as_the_identity`):
//!
//! 1. the target: 208 when the name resolves to nothing;
//! 2. the `WHERE`: 4145 for a value where a condition is expected, then what binding it
//!    raises (207, 4104, 137, 195);
//! 3. the **left** side of each `SET`, in written order: 207 for a column the target does
//!    not have, 4104 for a qualifier that does not name the target;
//! 4. the **right** side of each `SET`, in written order: what binding the expression
//!    raises;
//! 5. each assignment in written order: 8102 for the `IDENTITY` column, 271 for a computed
//!    column, 264 for a column already assigned in the same `SET`, 206 for a value with no
//!    implicit conversion to the column.
//!
//! # The line of each error
//!
//! 208, 206, 264, 8102 and 271 carry the line the statement starts on
//! (`the_errors_of_the_statement_carry_its_line`). So does a 4104 on the left side of a
//! `SET` (`a_qualifier_that_is_not_the_target_is_4104_on_the_statement_line`), where a 207
//! there keeps the line of the column (`update_unknown_column_is_207`). The `WHERE` and the
//! right side of a `SET` are bound by `expr.rs`, and their errors keep the lines that file
//! gives them: 4145 the line of the token after the expression, 207 the line of the column.
//!
//! # What is not bound here
//!
//! Each form below answers the internal error 50000 naming it, and not a plan of the target
//! alone (`the_other_forms_name_themselves`):
//!
//! - `TOP` in either statement (`the_other_forms_name_themselves`);
//! - `OUTPUT`, a view or a table variable as the target;
//! - `SET @x = e` and `SET @x = c = e`, which assign a variable;
//! - `SET c = DEFAULT`: no node of the bound plan says "the default of the column", so the
//!   word is refused rather than bound as a `NULL` (`set_default_is_not_bound_yet`). It is
//!   refused after the checks of step 5 on the columns before it, so that
//!   `SET id = DEFAULT` over an `IDENTITY` answers 8102 (`identity_set_to_default_is_8102`).
//!
//! A hint list on the target (`UPDATE dbo.t WITH (UPDLOCK) SET …`) is accepted and its
//! words are dropped, as `names.rs` drops them on a `FROM`: the `Scan` carries
//! [`LockHints::default`](crate::bound::LockHints::default) until the words are read
//! (`a_hint_list_on_the_target_is_accepted`).

use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    AssignOp, AssignTarget, Assignment, BinaryOp as AstBinaryOp, ColumnRef, DeleteStatement, Expr,
    Ident, Literal, ObjectName, TableRef, UpdateStatement,
};
use vauban_types::{TypeInfo, Value, implicit_result_type};

use crate::bound::{
    BoundExpr, BoundExprKind, BoundStatement, ColumnBinding, DeletePlan, LogicalPlan, UpdatePlan,
};
use crate::context::{BindContext, CatalogView, ResolvedTableKind};
use crate::depth::at_statement;
use crate::errors::{line_of, on_the_statement};
use crate::expr::{Scope, bind_condition, bind_expr};
use crate::names::{alias_of, bind_from, from_needs_the_catalogue};
use crate::query::{bug, dotted, not_implemented};
use crate::star::{self, Lookup, Source};

/// Binds an `UPDATE` into [`BoundStatement::Update`].
///
/// # Errors
///
/// The numbers of the module documentation, in the order it gives; 8631 when the tree of
/// an expression is deeper than the binder walks; the internal error 50000 for a form the
/// module does not bind. The errors that name the statement (`crate::errors`,
/// `NAMES_THE_STATEMENT`) carry the line it starts on.
pub(crate) fn bind_update(
    stmt: &UpdateStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let line = line_of(&stmt.span);
    update(stmt, ctx, line)
        .map_err(|err| at_statement(err, line))
        .map_err(|err| on_the_statement(err, line))
}

/// Binds a `DELETE` into [`BoundStatement::Delete`].
///
/// `DELETE dbo.t` and `DELETE FROM dbo.t` are one statement to the parser, and one plan
/// here.
///
/// # Errors
///
/// As [`bind_update`], without the errors of a `SET` list.
pub(crate) fn bind_delete(
    stmt: &DeleteStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let line = line_of(&stmt.span);
    delete(stmt, ctx, line)
        .map_err(|err| at_statement(err, line))
        .map_err(|err| on_the_statement(err, line))
}

/// The body of [`bind_update`]: the clauses that are refused, then the target, the
/// `WHERE` and the `SET` list, in the order of the module documentation.
fn update(stmt: &UpdateStatement, ctx: &BindContext<'_>, line: u32) -> SqlResult<BoundStatement> {
    if stmt.top.is_some() {
        return Err(not_implemented("TOP in an UPDATE"));
    }
    if stmt.output.is_some() {
        return Err(not_implemented("OUTPUT in an UPDATE (V2)"));
    }
    if !stmt.from.is_empty() {
        return update_from(stmt, ctx, line);
    }
    let target = bind_target(&stmt.target, &[], line, ctx)?;
    let input = filtered(target.scan, stmt.where_.as_ref(), &target.scope, ctx)?;
    let assignments = bind_assignments(
        &stmt.assignments,
        &target.scope,
        &target.scope,
        target.object,
        line,
        ctx,
    )?;
    Ok(BoundStatement::Update(UpdatePlan {
        table: target.table,
        input: Box::new(input),
        assignments,
    }))
}

/// The body of [`bind_delete`].
fn delete(stmt: &DeleteStatement, ctx: &BindContext<'_>, line: u32) -> SqlResult<BoundStatement> {
    if stmt.top.is_some() {
        return Err(not_implemented("TOP in a DELETE"));
    }
    if stmt.output.is_some() {
        return Err(not_implemented("OUTPUT in a DELETE (V2)"));
    }
    if !stmt.from.is_empty() {
        return delete_from(stmt, ctx, line);
    }
    let target = bind_target(&stmt.target, &[], line, ctx)?;
    let input = filtered(target.scan, stmt.where_.as_ref(), &target.scope, ctx)?;
    Ok(BoundStatement::Delete(DeletePlan {
        table: target.table,
        input: Box::new(input),
    }))
}

/// The target of an `UPDATE` or a `DELETE`, resolved.
struct Target {
    /// The table in `storage`, which the plan names.
    table: TableId,
    /// The object in the catalogue, which the `IDENTITY` and computed columns are read for.
    object: ObjectId,
    /// The `Scan` of the target, which the `WHERE` filters.
    scan: LogicalPlan,
    /// The columns of the target, in scope for the `SET` list and the `WHERE`.
    scope: Scope,
}

/// Resolves the target written on the statement and builds its `Scan`.
///
/// `from` is the list of sources a `FROM` clause put in scope, empty for the forms bound
/// here: a one-part target is first matched against their exposed names
/// ([`target_source`]), and a target that names one of them is not resolved by the
/// catalogue. That match is what the joined form will read the target through; `update`
/// and `delete` hand an empty slice.
///
/// The parser reads the target as a [`TableRef::Table`] (a hint list allowed, no alias) or
/// a [`TableRef::Variable`]; the other variants cannot come out of it for these two
/// statements and answer an internal error, so that a grammar that grows is noticed.
///
/// # Errors
///
/// - 208 on `line` when the name resolves to nothing, four-part names included;
/// - the refusal of `names::from_needs_the_catalogue` when the context has no catalogue;
/// - the internal error 50000 for a view, a table variable, or a target that names a
///   source of the `FROM`.
fn bind_target(
    reference: &TableRef,
    from: &[Source],
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<Target> {
    let name = match reference {
        TableRef::Table { name, .. } | TableRef::Function { name, .. } => name,
        TableRef::Variable { .. } => {
            return Err(not_implemented(
                "a table variable as the target of an UPDATE or a DELETE",
            ));
        }
        TableRef::Join { .. }
        | TableRef::Apply { .. }
        | TableRef::Derived { .. }
        | TableRef::Pivot(_)
        | TableRef::Unpivot(_) => {
            return Err(bug(
                "bind_target: the parser does not build this reference as the target of an \
                 UPDATE or a DELETE",
            ));
        }
    };
    if target_source(name, from).is_some() {
        return Err(not_implemented(
            "a target named by the alias of a source of the FROM clause",
        ));
    }
    let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
    let resolved = if name.server.is_some() {
        None
    } else {
        catalog.resolve_table(name, ctx.database, ctx.default_schema)
    };
    let Some(resolved) = resolved else {
        return Err(SqlError::invalid_object_name(&dotted(name)).with_line(line));
    };
    if resolved.kind == ResolvedTableKind::View {
        return Err(not_implemented(
            "a view as the target of an UPDATE or a DELETE (V2)",
        ));
    }
    let scan = bind_from(reference, line, ctx)?;
    let LogicalPlan::Scan {
        table,
        columns,
        alias,
        ..
    } = &scan
    else {
        return Err(bug(
            "bind_target: the catalogue resolved a table and the FROM binder built no Scan",
        ));
    };
    let scope = Scope::over(Source::new(
        alias,
        Some(name),
        columns,
        ctx.default_schema,
        ctx.database,
    ));
    Ok(Target {
        table: *table,
        object: resolved.object,
        scan,
        scope,
    })
}

/// The source of `from` a **one-part** target names by its exposed name, compared ASCII
/// case-insensitively; `None` for a qualified target, and for a name no source answers to.
///
/// A statement without a `FROM` hands an empty slice, and the target is then the table
/// the catalogue resolves (`bind_target`). The unit test
/// `a_one_part_target_matches_the_exposed_name_of_a_source` builds a scope by hand.
fn target_source<'a>(name: &ObjectName, from: &'a [Source]) -> Option<&'a Source> {
    if name.server.is_some() || name.database.is_some() || name.schema.is_some() {
        return None;
    }
    from.iter()
        .find(|source| source.exposed_name().eq_ignore_ascii_case(&name.name.value))
}

/// The `Scan` of the target under the `Filter` of the `WHERE`, or the `Scan` alone.
///
/// # Errors
///
/// Those of [`bind_condition`]: 4145 for a value where a condition is expected, and what
/// binding the expression raises.
fn filtered(
    scan: LogicalPlan,
    condition: Option<&Expr>,
    scope: &Scope,
    ctx: &BindContext<'_>,
) -> SqlResult<LogicalPlan> {
    match condition {
        None => Ok(scan),
        Some(condition) => Ok(LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: bind_condition(condition, ctx, scope)?,
        }),
    }
}

/// Binds the `SET` list: steps 3, 4 and 5 of the module documentation.
///
/// The two sides of a `SET` are resolved against two scopes, which the joined form tells
/// apart: `target` holds the target alone, under the name the statement exposes it by, and
/// `values` holds the sources a `FROM` put in scope. Over one table the caller hands the
/// same scope twice.
///
/// # Errors
///
/// 207 and 4104 on a left side; what binding a right side raises; 8102, 271, 264 and 206,
/// on `line`; the internal error 50000 for a `DEFAULT` or a variable on the left.
fn bind_assignments(
    assignments: &[Assignment],
    target: &Scope,
    values_scope: &Scope,
    object: ObjectId,
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<Vec<(ColumnBinding, BoundExpr)>> {
    let mut columns = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        columns.push(assigned_column(assignment, target, line)?);
    }
    let mut values = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        values.push(assigned_value(assignment, values_scope, ctx)?);
    }
    let catalog: Option<&dyn CatalogView> = ctx.catalog;
    let identity = catalog.and_then(|catalog| catalog.identity_column(object));
    let computed = catalog.map_or_else(Vec::new, |catalog| catalog.computed_columns(object));
    let mut assigned: Vec<ColumnId> = Vec::with_capacity(columns.len());
    let mut bound = Vec::with_capacity(columns.len());
    for (column, value) in columns.into_iter().zip(values) {
        if identity == Some(column.column) {
            return Err(SqlError::cannot_update_identity_column(&column.name).with_line(line));
        }
        if computed.contains(&column.column) {
            return Err(SqlError::cannot_update_computed_column(&column.name).with_line(line));
        }
        if assigned.contains(&column.column) {
            return Err(SqlError::column_specified_more_than_once(&column.name).with_line(line));
        }
        assigned.push(column.column);
        let Some(value) = value else {
            return Err(not_implemented(
                "SET column = DEFAULT in an UPDATE, which no node of the plan carries",
            ));
        };
        let value = converted(value, &column.ty, line)?;
        bound.push((column, value));
    }
    Ok(bound)
}

/// The column a `SET` writes: the left side, resolved against the target alone.
///
/// # Errors
///
/// - 207 on the line of the column when the target has no column of that name, whether a
///   qualifier naming the target was written or not;
/// - 4104 on `line`, the statement's, when the qualifier names something else than the
///   target: `SET x.a = 1`, `SET nosch.t.a = 1`, `SET nodb.dbo.t.a = 1`;
/// - 209 when the scope carries the name twice, which one table cannot;
/// - the internal error 50000 for a variable on the left side.
fn assigned_column(assignment: &Assignment, scope: &Scope, line: u32) -> SqlResult<ColumnBinding> {
    let column = match &assignment.target {
        AssignTarget::Column(column) => column,
        AssignTarget::Variable(_) => {
            return Err(not_implemented("SET @variable = e in an UPDATE"));
        }
        AssignTarget::VariableAndColumn { .. } => {
            return Err(not_implemented("SET @variable = column = e in an UPDATE"));
        }
    };
    match star::lookup(
        scope.sources(),
        column.qualifier.as_ref(),
        &column.name.value,
    ) {
        None => Err(SqlError::multi_part_identifier(&dotted_column(column)).with_line(line)),
        Some(Lookup::One(binding)) => Ok(binding.clone()),
        Some(Lookup::Absent) => {
            Err(SqlError::invalid_column_name(&column.name.value).with_line(line_of(&column.span)))
        }
        Some(Lookup::Ambiguous) => {
            Err(SqlError::ambiguous_column_name(&column.name.value)
                .with_line(line_of(&column.span)))
        }
    }
}

/// The value a `SET` writes, bound against the target: `None` for the word `DEFAULT`.
///
/// A compound operator is desugared first: `SET a += e` is bound as the expression
/// `a + e`, the column on the left read through the same reference the assignment wrote.
///
/// # Errors
///
/// What [`bind_expr`] raises.
fn assigned_value(
    assignment: &Assignment,
    scope: &Scope,
    ctx: &BindContext<'_>,
) -> SqlResult<Option<BoundExpr>> {
    if matches!(assignment.value, Expr::Literal(Literal::Default, _)) {
        return Ok(None);
    }
    let bound = match (compound_operator(assignment.op), &assignment.target) {
        (Some(op), AssignTarget::Column(column)) => {
            let operation = Expr::Binary {
                op,
                op_span: column.span,
                left: Box::new(Expr::Column(column.clone())),
                right: Box::new(assignment.value.clone()),
                span: column.span,
            };
            bind_expr(&operation, ctx, scope)?
        }
        _ => bind_expr(&assignment.value, ctx, scope)?,
    };
    Ok(Some(bound))
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

/// `value` under the `Convert` towards the type of its column, after the check of the
/// pair.
///
/// The `NULL` literal converts to any column: it has no type of its own. The node keeps
/// the nullability of the value: the `NOT NULL` of the column is checked where the row is
/// written, not here.
///
/// # Errors
///
/// 206 on `line` when the two types have no implicit conversion, the value's named first.
fn converted(value: BoundExpr, target: &TypeInfo, line: u32) -> SqlResult<BoundExpr> {
    let untyped_null = matches!(value.kind, BoundExprKind::Literal(Value::Null));
    if !untyped_null && implicit_result_type(&value.ty, target).is_err() {
        return Err(
            SqlError::operand_type_clash(value.ty.ty.name(), target.ty.name()).with_line(line),
        );
    }
    let ty = TypeInfo {
        ty: target.ty,
        nullable: value.ty.nullable,
        collation: target.collation,
    };
    let line = value.line;
    Ok(BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(value),
            style: None,
            try_: false,
        },
        ty,
        line,
    })
}

/// The parts of a column reference joined by dots, as message 4104 prints them: the
/// identifiers as written, delimiters already stripped by the parser.
fn dotted_column(column: &ColumnRef) -> String {
    match &column.qualifier {
        Some(qualifier) => format!("{}.{}", dotted(qualifier), column.name.value),
        None => column.name.value.clone(),
    }
}

// ---------------------------------------------------------------------------------------
// The joined form: `UPDATE … FROM` and `DELETE … FROM`
// ---------------------------------------------------------------------------------------
//
// `UPDATE <target> SET … FROM <sources> [WHERE …]` and `DELETE [FROM] <target>
// FROM <sources> [WHERE …]` write into one of the tables the `FROM` reads. The plan is the
// tree `join.rs` builds for that `FROM`, under the `Filter` of the `WHERE`, and the target
// is the table one source of it names. The statements below are stated over
// `dbo.t (k int NOT NULL, a int NULL)` and `dbo.u (k int NOT NULL, b int NULL, c int NULL)`;
// `tests/bind_update_from.rs` binds each of them.
//
// # Which source the target names
//
// The target is matched against the sources of the `FROM` **before** any lookup, on the
// names as they were written, the parts neither name carries completed from the context
// (`same_table`). An alias does not hide the table name from the target, where it does hide
// it from a column qualifier:
//
// | written | the target is |
// |---|---|
// | `UPDATE x … FROM dbo.t AS x JOIN dbo.u AS y ON …` | `x`, matched by its alias |
// | `UPDATE dbo.t … FROM dbo.t JOIN dbo.u AS y ON …` | the source `dbo.t` |
// | `UPDATE t … FROM dbo.t JOIN dbo.u AS y ON …` | the same, one part completed with `dbo` |
// | `UPDATE dbo.t … FROM dbo.t AS x JOIN dbo.u AS y ON …` | `x`: its alias hides nothing here |
// | `UPDATE dbo.t … FROM master.dbo.t JOIN dbo.u AS y ON …` | that source, from `master` |
//
// The fourth line is what tells this rule from a match on the exposed name: `UPDATE dbo.t
// SET a = 99 FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k` writes the rows the join keeps,
// not the whole table, which is what a target read as one more source would do.
//
// When several sources are the target's table, exactly one of them must carry no
// alias; that one is the target. Otherwise the reference is ambiguous, error 8154 printing
// the target as written:
//
// | written | answer |
// |---|---|
// | `UPDATE dbo.t … FROM dbo.t JOIN dbo.t AS y ON …` | the source without an alias |
// | `UPDATE dbo.t … FROM dbo.t AS x JOIN dbo.t AS y ON …` | 8154 `'dbo.t'` |
// | `UPDATE t … FROM dbo.t AS x JOIN dbo.u AS t ON …` | `u`, matched by the alias `t` |
// | `UPDATE t … FROM dbo.t AS x JOIN dbo.t AS y ON …` | 8154 `'t'` |
// | `UPDATE dbo.t … FROM dbo.t AS x, dbo.t AS y` | 8154 `'dbo.t'` |
// | `DELETE dbo.t FROM dbo.t AS x JOIN dbo.t AS y ON …` | 8154 `'dbo.t'` |
// | `UPDATE dbo.t … FROM dbo.t JOIN dbo.t ON 1 = 1` | 1013, raised by the `FROM` itself |
//
// A one-part target that names an alias of a source wins over a source of the same table
// name (`update_from_target_t_when_x_and_u_as_t_set_b_binds_u`). When unaliased `dbo.t` and
// `dbo.u AS t` would expose the same name, the `FROM` answers 1012 before the target is
// read (`update_from_target_t_when_u_is_aliased_as_t_from_refuses_1012`).
//
// A target no source names is looked up in the catalogue: 208 when it reaches nothing
// (`UPDATE z … FROM dbo.t AS x JOIN dbo.u AS y ON …`), and otherwise the table the name
// resolves to (`update_target_outside_the_from_binds_the_catalogue_table`).
//
// # What each part of the statement sees
//
// The **left** side of a `SET` is a column of the target alone, named bare or qualified by
// the name the target is exposed by — its alias when it carries one, in which case the
// table name reaches nothing there. The **right** side of a `SET` and the `WHERE` see the
// sources of the join, under the rules of `join.rs`:
//
// | written over `FROM dbo.t AS x JOIN dbo.u AS y ON x.k = y.k` | answer |
// |---|---|
// | `UPDATE x SET x.a = y.b …` | binds: the value is a column of `y` |
// | `UPDATE x SET a = 1 …` | binds |
// | `UPDATE x SET t.a = 1 …`, `UPDATE x SET dbo.t.a = 1 …` | 4104: the alias hides the name |
// | `UPDATE x SET y.b = 1 …` | 4104 `"y.b"`: `y` is not the target |
// | `UPDATE x SET z.a = 1 …` | 4104 `"z.a"` |
// | `UPDATE x SET x.nocol = 1 …` | 207 |
// | `UPDATE x SET b = 1 …` | 207: `b` is a column of `y`, not of the target |
// | `UPDATE x SET k = 1 …` | binds: the left side sees the target alone, so `k` is not 209 |
// | `UPDATE x SET x.a = k …`, `… WHERE k = 1` | 209: both sources carry `k` |
// | `UPDATE x SET x.a = z.k …`, `… WHERE z.k = 1` | 4104 |
// | `UPDATE x SET x.a = 1 … WHERE nocol = 1` | 207 |
// | `UPDATE x SET x.a = 1 … WHERE 1` | 4145 |
//
// The line `SET k = 1` is what separates the two scopes: a left side resolved against the
// join scope would answer 209 there, and a right side resolved against the target alone would
// not answer 209.
//
// # The order of the checks
//
// One statement can carry several faults; each step below is exhausted before the next one
// starts, and the tests named after each pair pin the order:
//
// 1. the `FROM`: 208 for a source that reaches nothing, 1011 to 1013 for two sources of one
//    exposed name, then what binding each `ON` raises;
// 2. the target: 8154, or the 208 of a name the catalogue does not hold;
// 3. the `WHERE`;
// 4. the `SET` list, in the three steps the module documentation gives for one table.
//
// 8154, 208 and a 4104 on the left side of a `SET` carry the line the statement starts on;
// the errors of the `ON`, of the `WHERE` and of a right side keep the lines `expr.rs` gives
// them (`the_errors_of_the_joined_form_carry_their_line`).
//
// # What the plan does not carry
//
// `UpdatePlan` and `DeletePlan` name the target by its `TableId` alone. When the same table
// is read twice by the `FROM`, that identifier does not say which of the two sources the
// rows are written through, and the columns of `assignments` index the row of the target and
// not the row of the `Join`. A field naming the source is what a planner would need, and
// adding one is not this file's to do.

/// The body of the joined `UPDATE`, in the order of the section above.
///
/// # Errors
///
/// The numbers of that section; the internal error 50000 for a target absent from the
/// `FROM` and for the forms `update` refuses before calling this.
fn update_from(
    stmt: &UpdateStatement,
    ctx: &BindContext<'_>,
    line: u32,
) -> SqlResult<BoundStatement> {
    let (sources, scope) = crate::join::bind_from(&stmt.from, line, ctx)?;
    let target = joined_target(&stmt.target, &stmt.from, line, ctx)?;
    let input = filtered(sources, stmt.where_.as_ref(), &scope, ctx)?;
    let assignments = bind_assignments(
        &stmt.assignments,
        &target.scope,
        &scope,
        target.object,
        line,
        ctx,
    )?;
    Ok(BoundStatement::Update(UpdatePlan {
        table: target.table,
        input: Box::new(input),
        assignments,
    }))
}

/// The body of the joined `DELETE`: as [`update_from`], without a `SET` list.
///
/// # Errors
///
/// As [`update_from`].
fn delete_from(
    stmt: &DeleteStatement,
    ctx: &BindContext<'_>,
    line: u32,
) -> SqlResult<BoundStatement> {
    let (sources, scope) = crate::join::bind_from(&stmt.from, line, ctx)?;
    let target = joined_target(&stmt.target, &stmt.from, line, ctx)?;
    let input = filtered(sources, stmt.where_.as_ref(), &scope, ctx)?;
    Ok(BoundStatement::Delete(DeletePlan {
        table: target.table,
        input: Box::new(input),
    }))
}

/// The target of a joined statement: the table written into, and the scope the left side of
/// a `SET` resolves against.
struct JoinedTarget {
    /// The table in `storage`, which the plan names.
    table: TableId,
    /// The object in the catalogue, read for the `IDENTITY` and the computed columns.
    object: ObjectId,
    /// The target alone, exposed under the name the statement refers to it by: the columns
    /// index the row of the **target**, not the row of the `Join`.
    scope: Scope,
}

/// A source of the `FROM` as it was written, which the target is matched against.
struct Leaf<'a> {
    /// The name of the table, delimiters already stripped by the parser.
    name: &'a ObjectName,
    /// The alias, when one was written.
    alias: Option<&'a Ident>,
}

/// Resolves the target of a joined statement against the sources of its `FROM`.
///
/// The rules are the ones stated above the section: the alias of a source, or its name with
/// the parts it does not carry completed from the context; when several sources are that
/// table, the one without an alias.
///
/// # Errors
///
/// - 8154 on `line` when several sources are the target's table and zero or more than one of
///   them carries no alias (`update_self_join_requires_the_alias`);
/// - 208 on `line` when no source names the target and the catalogue holds no such table;
/// - the refusal of `names::from_needs_the_catalogue` without a catalogue;
/// - the internal error 50000 for a table variable or a view as the target.
fn joined_target(
    reference: &TableRef,
    from: &[TableRef],
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<JoinedTarget> {
    let target = target_name(reference)?;
    let mut leaves = Vec::new();
    collect_leaves(from, &mut leaves)?;
    match pick_target_leaf(&leaves, target, line, ctx)? {
        Some(leaf) => joined_target_from_leaf(leaf, ctx),
        None => resolve_target_from_catalog(target, line, ctx),
    }
}

/// Whether a source of the `FROM` is the target: by alias for a one-part target, else by table.
enum TargetMatch {
    /// The target names the alias of the source.
    Alias,
    /// The target names the table of the source.
    TableName,
}

/// Classifies how `leaf` is named by `target`, or returns `None`.
fn classify_target_match(
    leaf: &Leaf<'_>,
    target: &ObjectName,
    ctx: &BindContext<'_>,
) -> Option<TargetMatch> {
    let one_part = target.server.is_none() && target.database.is_none() && target.schema.is_none();
    if one_part
        && let Some(alias) = leaf.alias
        && alias.value.eq_ignore_ascii_case(&target.name.value)
    {
        return Some(TargetMatch::Alias);
    }
    same_table(target, leaf.name, ctx).then_some(TargetMatch::TableName)
}

/// The source of the `FROM` the target names, when one of them does.
///
/// A one-part target that matches an alias wins over a match on the table name
/// (`update_from_target_t_when_x_and_u_as_t_set_b_binds_u`). When several sources remain,
/// the one without an alias is the target; otherwise 8154.
///
/// # Errors
///
/// 8154 on `line` when several sources match and the rule above does not pick one.
fn pick_target_leaf<'a>(
    leaves: &'a [Leaf<'a>],
    target: &ObjectName,
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<Option<&'a Leaf<'a>>> {
    let mut alias_matches = Vec::new();
    let mut table_matches = Vec::new();
    for leaf in leaves {
        match classify_target_match(leaf, target, ctx) {
            Some(TargetMatch::Alias) => alias_matches.push(leaf),
            Some(TargetMatch::TableName) => table_matches.push(leaf),
            None => {}
        }
    }
    let candidates = if alias_matches.is_empty() {
        table_matches
    } else {
        alias_matches
    };
    match candidates.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(*only)),
        several => {
            let mut without_alias = several.iter().filter(|leaf| leaf.alias.is_none());
            match (without_alias.next(), without_alias.next()) {
                (Some(only), None) => Ok(Some(*only)),
                _ => Err(SqlError::table_is_ambiguous(&dotted(target)).with_line(line)),
            }
        }
    }
}

/// Builds the target from a source the `FROM` reads.
fn joined_target_from_leaf(leaf: &Leaf<'_>, ctx: &BindContext<'_>) -> SqlResult<JoinedTarget> {
    let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
    let resolved = catalog
        .resolve_table(leaf.name, ctx.database, ctx.default_schema)
        .ok_or_else(|| {
            bug("joined_target: the FROM binder resolved a source the catalogue does not hold")
        })?;
    if resolved.kind == ResolvedTableKind::View {
        return Err(not_implemented(
            "a view as the target of an UPDATE or a DELETE (V2)",
        ));
    }
    let table = resolved.table.ok_or_else(|| {
        bug("joined_target: the catalogue resolved a table without a storage identifier")
    })?;
    // An alias hides the name of the table on the left side of a `SET`, as it does for a
    // column of a `SELECT`: `SET dbo.t.a = 1` over `FROM dbo.t AS x` answers 4104.
    let written = leaf.alias.is_none().then_some(leaf.name);
    Ok(JoinedTarget {
        table,
        object: resolved.object,
        scope: Scope::over(Source::new(
            &alias_of(leaf.name, leaf.alias),
            written,
            &resolved.columns,
            ctx.default_schema,
            ctx.database,
        )),
    })
}

/// Resolves a target the `FROM` does not read against the catalogue.
///
/// # Errors
///
/// 208 on `line` when the name reaches nothing; the refusal of `names::from_needs_the_catalogue`
/// without a catalogue; the internal error 50000 for a view as the target.
fn resolve_target_from_catalog(
    target: &ObjectName,
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<JoinedTarget> {
    let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
    if target.server.is_some() {
        return Err(SqlError::invalid_object_name(&dotted(target)).with_line(line));
    }
    let resolved = catalog
        .resolve_table(target, ctx.database, ctx.default_schema)
        .ok_or_else(|| SqlError::invalid_object_name(&dotted(target)).with_line(line))?;
    if resolved.kind == ResolvedTableKind::View {
        return Err(not_implemented(
            "a view as the target of an UPDATE or a DELETE (V2)",
        ));
    }
    let table = resolved.table.ok_or_else(|| {
        bug("resolve_target_from_catalog: the catalogue resolved a table without a storage identifier")
    })?;
    Ok(JoinedTarget {
        table,
        object: resolved.object,
        scope: Scope::over(Source::new(
            &alias_of(target, None),
            Some(target),
            &resolved.columns,
            ctx.default_schema,
            ctx.database,
        )),
    })
}

/// The name of the target of a joined statement.
///
/// # Errors
///
/// The internal error 50000 for a table variable, and for a reference the parser does not
/// build in that position, so that a grammar that grows is noticed.
fn target_name(reference: &TableRef) -> SqlResult<&ObjectName> {
    match reference {
        TableRef::Table { name, .. } | TableRef::Function { name, .. } => Ok(name),
        TableRef::Variable { .. } => Err(not_implemented(
            "a table variable as the target of an UPDATE or a DELETE",
        )),
        TableRef::Join { .. }
        | TableRef::Apply { .. }
        | TableRef::Derived { .. }
        | TableRef::Pivot(_)
        | TableRef::Unpivot(_) => Err(bug(
            "joined_target: the parser does not build this reference as the target of an \
             UPDATE or a DELETE",
        )),
    }
}

/// Collects the sources of a `FROM` in written order, walking each `JOIN` left side first.
///
/// # Errors
///
/// The internal error 50000 for a source that is neither a table nor a `name(…)`:
/// `join::bind_from` refuses those before this point.
fn collect_leaves<'a>(from: &'a [TableRef], leaves: &mut Vec<Leaf<'a>>) -> SqlResult<()> {
    for reference in from {
        match reference {
            TableRef::Join { left, right, .. } => {
                collect_leaves(std::slice::from_ref(left.as_ref()), leaves)?;
                collect_leaves(std::slice::from_ref(right.as_ref()), leaves)?;
            }
            TableRef::Table { name, alias, .. } | TableRef::Function { name, alias, .. } => {
                leaves.push(Leaf {
                    name,
                    alias: alias.as_ref(),
                });
            }
            _ => {
                return Err(bug(
                    "collect_leaves: a source the FROM binder does not read as a table",
                ));
            }
        }
    }
    Ok(())
}

/// Whether two written names reach one table, the parts neither carries completed from the
/// context, compared ASCII case-insensitively as a `CI` collation is.
///
/// A four-part name reaches no source: linked servers are not read here, as `names.rs` does
/// not read them for a `FROM`.
fn same_table(left: &ObjectName, right: &ObjectName, ctx: &BindContext<'_>) -> bool {
    if left.server.is_some() || right.server.is_some() {
        return false;
    }
    let part = |written: Option<&Ident>, default: &str| -> String {
        written.map_or_else(|| default.to_owned(), |ident| ident.value.clone())
    };
    let schema = |name: &ObjectName| part(name.schema.as_ref(), ctx.default_schema);
    let database = |name: &ObjectName| part(name.database.as_ref(), ctx.database);
    left.name.value.eq_ignore_ascii_case(&right.name.value)
        && schema(left).eq_ignore_ascii_case(&schema(right))
        && database(left).eq_ignore_ascii_case(&database(right))
}

#[cfg(test)]
mod tests {
    use super::{compound_operator, target_source};
    use crate::bound::ColumnBinding;
    use crate::star::Source;
    use vauban_catalog::ColumnId;
    use vauban_parser::{AssignOp, BinaryOp, Ident, ObjectName, Span};
    use vauban_types::{SqlType, TypeInfo};

    fn ident(value: &str) -> Ident {
        Ident {
            value: value.to_owned(),
            quoted: false,
        }
    }

    fn name(parts: &[&str]) -> ObjectName {
        let mut name = ObjectName {
            server: None,
            database: None,
            schema: None,
            name: ident(parts[parts.len() - 1]),
            span: Span::EMPTY,
        };
        let mut rest = parts[..parts.len() - 1].iter().rev();
        name.schema = rest.next().map(|part| ident(part));
        name.database = rest.next().map(|part| ident(part));
        name.server = rest.next().map(|part| ident(part));
        name
    }

    fn source(alias: &str) -> Source {
        let columns = [ColumnBinding {
            column: ColumnId(1),
            index: 0,
            name: "a".to_owned(),
            ty: TypeInfo::new(SqlType::Int, false),
        }];
        Source::new(alias, Some(&name(&["dbo", "t"])), &columns, "dbo", "master")
    }

    /// A one-part target matches a source by its exposed name, in upper case as in lower
    /// case; a qualified one matches nothing, and neither does a name no source answers to.
    #[test]
    fn a_one_part_target_matches_the_exposed_name_of_a_source() {
        let sources = [source("x"), source("t")];
        assert!(target_source(&name(&["x"]), &sources).is_some());
        assert!(target_source(&name(&["X"]), &sources).is_some());
        assert_eq!(
            target_source(&name(&["t"]), &sources).map(Source::exposed_name),
            Some("t")
        );
        assert!(target_source(&name(&["dbo", "t"]), &sources).is_none());
        assert!(target_source(&name(&["master", "dbo", "t"]), &sources).is_none());
        assert!(target_source(&name(&["y"]), &sources).is_none());
        assert!(target_source(&name(&["x"]), &[]).is_none());
    }

    /// Each of the eight compound operators stands for one arithmetic operator; `=` alone
    /// stands for no operator.
    #[test]
    fn each_compound_operator_has_its_arithmetic() {
        assert!(compound_operator(AssignOp::Set).is_none());
        let pairs = [
            (AssignOp::AddAssign, BinaryOp::Add),
            (AssignOp::SubAssign, BinaryOp::Sub),
            (AssignOp::MulAssign, BinaryOp::Mul),
            (AssignOp::DivAssign, BinaryOp::Div),
            (AssignOp::ModAssign, BinaryOp::Mod),
            (AssignOp::BitAndAssign, BinaryOp::BitAnd),
            (AssignOp::BitOrAssign, BinaryOp::BitOr),
            (AssignOp::BitXorAssign, BinaryOp::BitXor),
        ];
        for (op, expected) in pairs {
            assert_eq!(compound_operator(op), Some(expected), "{op:?}");
        }
    }
}
