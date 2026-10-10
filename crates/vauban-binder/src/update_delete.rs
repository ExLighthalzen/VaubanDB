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
//! (`update_inserts_convert_to_column_type`) — except when `c` is a bounded `varchar`,
//! `nvarchar` or `varbinary` and `e` is already of the matching family, where `e` keeps its
//! own type instead, so the assignment still knows how long the value really is. The
//! columns of the target are in scope on the
//! right side of a `SET` and in the `WHERE`: `SET a = a + 1` reads the old value of `a`.
//! A compound operator is desugared before binding, as
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
//! A hint list on the target (`UPDATE dbo.t WITH (UPDLOCK) SET …`) is read as `names.rs`
//! reads one on a source of a `FROM`, and travels on the `Scan` of the target
//! (`a_hint_list_on_the_target_is_accepted`), whose reads take the locks it asks for.

use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    AssignOp, AssignTarget, Assignment, BinaryOp as AstBinaryOp, ColumnRef, DeleteStatement, Expr,
    Ident, Literal, ObjectName, TableRef, UpdateStatement,
};
use vauban_types::{Len, SqlType, TypeInfo, Value, implicit_result_type};

use crate::bound::{
    BoundExpr, BoundExprKind, BoundStatement, ColumnBinding, DeletePlan, LogicalPlan, UpdatePlan,
};
use crate::context::{BindContext, CatalogView, ResolvedTableKind};
use crate::depth::at_statement;
use crate::errors::{line_of, on_the_statement};
use crate::expr::{Scope, bind_condition, bind_expr};
use crate::insert::bind_target_expressions;
use crate::names::{alias_of, bind_from, from_needs_the_catalogue, resolve_table};
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
    let target = bind_target(&stmt.target, line, ctx)?;
    let input = filtered(target.scan, stmt.where_.as_ref(), &target.scope, ctx)?;
    let assignments = bind_assignments(
        &stmt.assignments,
        &target.scope,
        &target.scope,
        target.object,
        line,
        ctx,
    )?;
    let expressions = bind_target_expressions(target.object, None, ctx);
    Ok(BoundStatement::Update(UpdatePlan {
        table: target.table,
        input: Box::new(input),
        assignments,
        target: expressions,
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
    let target = bind_target(&stmt.target, line, ctx)?;
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

/// Resolves the target written on a statement without a `FROM` and builds its `Scan`. A
/// statement with a `FROM` matches its target against the sources of that clause instead
/// ([`joined_target`]).
///
/// The parser reads the target as a [`TableRef::Table`] (a hint list allowed, no alias) or
/// a [`TableRef::Variable`]; the other variants cannot come out of it for these two
/// statements and answer an internal error, so that a grammar that grows is noticed.
///
/// # Errors
///
/// - 208 on `line` when the name resolves to nothing, four-part names included;
/// - the refusal of `names::from_needs_the_catalogue` when the context has no catalogue;
/// - the internal error 50000 for a view or a table variable.
fn bind_target(reference: &TableRef, line: u32, ctx: &BindContext<'_>) -> SqlResult<Target> {
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
        | TableRef::Values { .. }
        | TableRef::Pivot(_)
        | TableRef::Unpivot(_) => {
            return Err(bug(
                "bind_target: the parser does not build this reference as the target of an \
                 UPDATE or a DELETE",
            ));
        }
    };
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
/// pair — except when the column is a bounded `varchar`, `nvarchar` or `varbinary` and
/// `value` is already of the matching family, where `value` is answered unchanged: the
/// assignment that follows checks its length once it knows it, rather than narrowing it
/// here (`a_string_keeps_its_own_length_into_a_bounded_character_column`).
///
/// The `NULL` literal converts to any column: it has no type of its own, so it is not
/// the exception above. The node keeps the nullability of the value: the `NOT NULL` of the
/// column is checked where the row is written, not here.
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
    if !untyped_null && keeps_its_own_length(&value.ty, target) {
        return Ok(value);
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

/// Whether a value of type `source` keeps its own length instead of being narrowed to
/// `target` while the assignment is bound: true for a string reaching a bounded `varchar`
/// or `nvarchar`, or a binary reaching a bounded `varbinary`. A narrower fixed
/// `char`/`nchar`, an unbounded target, or a value of a different family keeps narrowing
/// here, unaffected.
fn keeps_its_own_length(source: &TypeInfo, target: &TypeInfo) -> bool {
    match &target.ty {
        SqlType::VarChar(Len::Fixed(_)) | SqlType::NVarChar(Len::Fixed(_)) => source.ty.is_string(),
        SqlType::VarBinary(Len::Fixed(_)) => {
            matches!(source.ty, SqlType::Binary(_) | SqlType::VarBinary(_))
        }
        _ => false,
    }
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
// | `UPDATE master.dbo.t … FROM dbo.t JOIN dbo.u AS y ON …` | the unaliased `dbo.t` |
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
// read.
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
// # Which rows of the plan are written
//
// `UpdatePlan` and `DeletePlan` name the target by its `TableId` alone, which does not say
// which source of a `FROM` that reads the table twice is the one written through. The
// `Scan` of the source the target names therefore carries one more binding after its
// columns: the row-identifier binding `vauban_planner::row_id_binding` builds for its table
// (`target_row_id`), which makes that scan, and no other, hand the identifier of each row
// it reads to the rows of the plan. The other reads of the table carry none
// (`tests/bind_update_from.rs`, `the_source_the_target_names_carries_the_row_identifier`).
// The binding is not a column: the `schema` of the scan and the scope the statement is
// bound in do not hold it, so the positions every expression reads are unchanged.
//
// A target that no source of the `FROM` names joins the `FROM` as one more source, after
// the others and without an `ON`: `UPDATE dbo.b SET v = 7 FROM dbo.s WHERE s.id = 2` writes
// the three rows of a `b` of three rows when `s` holds a row of `id` 2, and zero when it does
// not (`vauban-executor/tests/dml_input.rs`, `a_target_outside_the_from_is_joined_to_it`).
// Its columns are in scope for the `WHERE` and the right side of a `SET`, under the name the
// target was written with (`update_target_outside_the_from_joins_the_from`).

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
    let (input, scope, target) =
        joined_input(&stmt.target, &stmt.from, stmt.where_.as_ref(), line, ctx)?;
    let assignments = bind_assignments(
        &stmt.assignments,
        &target.scope,
        &scope,
        target.object,
        line,
        ctx,
    )?;
    let expressions = bind_target_expressions(target.object, None, ctx);
    Ok(BoundStatement::Update(UpdatePlan {
        table: target.table,
        input: Box::new(input),
        assignments,
        target: expressions,
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
    let (input, _, target) =
        joined_input(&stmt.target, &stmt.from, stmt.where_.as_ref(), line, ctx)?;
    Ok(BoundStatement::Delete(DeletePlan {
        table: target.table,
        input: Box::new(input),
    }))
}

/// The rows a joined statement writes through, the scope its `WHERE` and the right side of
/// its `SET` are bound in, and its target, in the order of the checks above.
///
/// The `FROM` is bound first, then the target is matched against its sources. A target no
/// source names is appended to the `FROM` and the clause bound again with it. The `Scan` of
/// the source the target names then carries the row-identifier binding of its table
/// ([`mark_target`]), and the `WHERE` is bound over the scope of the clause.
///
/// # Errors
///
/// The errors of the section above, in its order.
fn joined_input(
    reference: &TableRef,
    from: &[TableRef],
    condition: Option<&Expr>,
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<(LogicalPlan, Scope, JoinedTarget)> {
    let (sources, scope) = crate::join::bind_from(from, line, ctx)?;
    let name = target_name(reference)?;
    check_update_target_exposed_names(name, from, line, ctx)?;
    let target = joined_target(reference, from, line, ctx)?;
    let (mut sources, scope, exposed) = match &target.exposed {
        Some(exposed) => (sources, scope, exposed.clone()),
        None => {
            let mut widened = from.to_vec();
            widened.push(reference.clone());
            let (sources, scope) = crate::join::bind_from(&widened, line, ctx)?;
            (sources, scope, alias_of(name, None))
        }
    };
    mark_target(&mut sources, target.table, &exposed)?;
    let input = filtered(sources, condition, &scope, ctx)?;
    Ok((input, scope, target))
}

/// Appends the row-identifier binding of `table` ([`target_row_id`]) to the columns of the
/// `Scan` of `table` exposed as `exposed` in the tree of joins `plan` holds, and answers
/// whether one was found. The exposed names of a `FROM` are distinct, which the binding of
/// the clause has checked (1011 to 1013), so one scan at most answers both.
///
/// # Errors
///
/// The internal error 50000 for a table whose identifier does not fit in a column
/// identifier.
fn mark_target(plan: &mut LogicalPlan, table: TableId, exposed: &str) -> SqlResult<bool> {
    match plan {
        LogicalPlan::Join { left, right, .. } => {
            Ok(mark_target(left, table, exposed)? || mark_target(right, table, exposed)?)
        }
        LogicalPlan::Scan {
            table: read,
            columns,
            alias,
            ..
        } if *read == table && alias.eq_ignore_ascii_case(exposed) => {
            let binding = target_row_id(table).ok_or_else(|| {
                bug("mark_target: the table identifier does not fit in a column identifier")
            })?;
            columns.push(binding);
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// The binding `vauban_planner::row_id_binding` builds for `table`, which asks the scan that
/// carries it for the identifier of each row it reads: the table negated and moved one
/// down in `column`, `index` 0, the name `%rowid`, a `bigint`. The binder does not depend on
/// the planner, so the value is built here. That the planner reads it as the mark of the
/// target is what a self-join written through the chain needs
/// (`vauban-executor/tests/dml_input.rs`, `a_self_join_writes_the_source_the_target_names`).
///
/// `None` for a table whose identifier does not fit in a column identifier.
fn target_row_id(table: TableId) -> Option<ColumnBinding> {
    let id = i32::try_from(table.0).ok()?;
    Some(ColumnBinding {
        column: ColumnId(-id - 1),
        index: 0,
        name: "%rowid".to_owned(),
        ty: TypeInfo::new(SqlType::BigInt, false),
        depth: 0,
    })
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
    /// The exposed name of the source of the `FROM` the target names; `None` for a target
    /// the catalogue resolved, which no source names.
    exposed: Option<String>,
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
/// - the internal error 50000 for a table variable, a view or a derived table as the target.
fn joined_target(
    reference: &TableRef,
    from: &[TableRef],
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<JoinedTarget> {
    let target = target_name(reference)?;
    let one_part = target.server.is_none() && target.database.is_none() && target.schema.is_none();
    if one_part
        && derived_aliases(from)
            .iter()
            .any(|alias| alias.value.eq_ignore_ascii_case(&target.name.value))
    {
        return Err(not_implemented(
            "a derived table as the target of an UPDATE or a DELETE",
        ));
    }
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

/// Refuses an `UPDATE`/`DELETE` whose target and an unaliased source of the `FROM` share an
/// object name but were written with different qualifiers that reach **different** tables
/// (`master.sys.objects` against `sys.objects`).
///
/// 1013 is raised when both names resolve in the catalogue and reach different tables
/// (`update_from_three_part_target_is_1013_when_both_exist_and_not_same_table`). When the
/// target reaches nothing, [`joined_target`] answers 208 instead
/// (`update_from_missing_three_part_target_is_208_not_1013`).
///
/// # Errors
///
/// 1013 on `line` when such a pair is found and both names resolve.
fn check_update_target_exposed_names(
    target: &ObjectName,
    from: &[TableRef],
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<()> {
    if target.server.is_some() {
        return Ok(());
    }
    let Some(catalog) = ctx.catalog else {
        return Ok(());
    };
    if catalog
        .resolve_table(target, ctx.database, ctx.default_schema)
        .is_none()
    {
        return Ok(());
    }
    let mut leaves = Vec::new();
    collect_leaves(from, &mut leaves)?;
    let target_written = dotted(target);
    for leaf in leaves {
        if leaf.alias.is_some() || leaf.name.server.is_some() {
            continue;
        }
        if !leaf
            .name
            .name
            .value
            .eq_ignore_ascii_case(&target.name.value)
        {
            continue;
        }
        if dotted(leaf.name).eq_ignore_ascii_case(&target_written) {
            continue;
        }
        if same_table(target, leaf.name, ctx) {
            continue;
        }
        if catalog
            .resolve_table(leaf.name, ctx.database, ctx.default_schema)
            .is_none()
        {
            continue;
        }
        return Err(
            SqlError::same_exposed_names(&target_written, &dotted(leaf.name)).with_line(line),
        );
    }
    Ok(())
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
        exposed: Some(alias_of(leaf.name, leaf.alias)),
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
    if target.server.is_some() {
        return Err(SqlError::invalid_object_name(&dotted(target)).with_line(line));
    }
    let resolved = resolve_table(target, ctx)?
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
        exposed: None,
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
        | TableRef::Values { .. }
        | TableRef::Pivot(_)
        | TableRef::Unpivot(_) => Err(bug(
            "joined_target: the parser does not build this reference as the target of an \
             UPDATE or a DELETE",
        )),
    }
}

/// Collects the tables of a `FROM` in written order, walking each `JOIN` left side first. A
/// derived table and a `VALUES` list are not tables, and are not collected: the target is
/// not matched against them ([`derived_aliases`]).
///
/// # Errors
///
/// The internal error 50000 for a source that is neither a table, a `name(…)`, a derived
/// table nor a `VALUES` list: `join::bind_from` refuses those before this point.
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
            TableRef::Derived { .. } | TableRef::Values { .. } => {}
            _ => {
                return Err(bug(
                    "collect_leaves: a source the FROM binder does not read as a table",
                ));
            }
        }
    }
    Ok(())
}

/// The aliases of the derived tables and `VALUES` lists of a `FROM`, at any depth of its
/// joins.
fn derived_aliases(from: &[TableRef]) -> Vec<&Ident> {
    let mut aliases = Vec::new();
    for reference in from {
        match reference {
            TableRef::Join { left, right, .. } => {
                aliases.extend(derived_aliases(std::slice::from_ref(left.as_ref())));
                aliases.extend(derived_aliases(std::slice::from_ref(right.as_ref())));
            }
            TableRef::Derived {
                alias: Some(alias), ..
            }
            | TableRef::Values {
                alias: Some(alias), ..
            } => aliases.push(alias),
            _ => {}
        }
    }
    aliases
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
mod tests;
