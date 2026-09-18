//! `INSERT`: `VALUES`, `INSERT … SELECT` and `DEFAULT VALUES`, bound into an
//! [`InsertPlan`]; and `SELECT … INTO`, bound into a [`SelectIntoPlan`].
//!
//! # What the plan holds
//!
//! [`InsertPlan::columns`] is the list of target columns, in the order the rows of
//! [`InsertPlan::source`] fill them. It is the written column list resolved against the
//! table, or, when none was written, the **insertable** columns of the table in the order
//! of the catalogue: the `IDENTITY` column and the computed columns are left out
//! (`tests/bind_insert.rs`, `insert_without_column_list_uses_catalog_order` and
//! `insert_without_column_list_skips_the_identity_column`).
//!
//! A column that takes its default is **absent** from `columns`: what fills it (the
//! `IDENTITY` counter, the `DEFAULT` of the catalogue, `NULL`, or error 515) is decided
//! where the statement runs, not here. That is the one rule for the three spellings:
//!
//! | Written | `columns` | `source` |
//! |---|---|---|
//! | `INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')` | `a`, `b` | `Values` of two rows |
//! | `INSERT INTO t (a, d) VALUES (1, DEFAULT)` | `a` | `Values` of one row of one value |
//! | `INSERT INTO t DEFAULT VALUES` | empty (`insert_default_values_marks_all_columns_default`) | `Values` of one empty row |
//! | `INSERT INTO t (a, b) SELECT x, y FROM u` | `a`, `b` | the bound plan of the `SELECT` |
//!
//! `DEFAULT` written in some rows of a multi-row `VALUES` and not in the others has no
//! place in that shape: the form is refused by an internal error naming it
//! (`default_in_some_rows_only_is_not_bound`).
//!
//! # The conversion towards the column
//!
//! Each expression of a `VALUES` row is wrapped in a [`BoundExprKind::Convert`] towards
//! the type of its column, whether or not its own type already matches
//! (`insert_values_two_rows_binds`): after the binding, the executor decides no
//! conversion. A pair that has no implicit conversion is error 206, the value's type named
//! before the column's (`incompatible_type_is_206`); an untyped `NULL` is exempt from that
//! check. Over several rows, the types of one column are reconciled row after row before
//! the column is looked at, which is what names `int` before `date` for
//! `VALUES (1), (CAST('20200101' AS date))` into an `int` column
//! (`incompatible_type_between_two_rows_names_the_rows`). A conversion that is legal and
//! fails on a value (`'abc'` into an `int`) is not an error here.
//!
//! The plan of an `INSERT … SELECT` is kept as `bind_select` built it: its columns are
//! checked against the target columns the same way, and the executor converts each value
//! it reads from it (`insert_select_binds_a_plan`).
//!
//! # The errors, in the order they are raised
//!
//! Two faults in one statement come out in the order below; `tests/bind_insert.rs` binds
//! one pair per step (`an_unknown_column_precedes_a_repeated_one`,
//! `rows_that_disagree_precede_a_short_row`,
//! `a_short_row_precedes_a_default_on_the_identity_column`,
//! `a_short_row_precedes_a_type_clash`, `a_short_row_precedes_a_repeated_column`,
//! `a_default_on_the_identity_column_precedes_a_repeated_column`,
//! `a_type_clash_precedes_a_repeated_column`,
//! `a_repeated_column_precedes_the_identity_refusal`,
//! `a_type_clash_precedes_the_identity_refusal`).
//!
//! 1. 208, the target resolves to nothing; the internal error for a view, a table variable,
//!    `TOP`, `OUTPUT` or an `EXECUTE` source;
//! 2. 207, a column of the list that the table does not have;
//! 3. 10709, two `VALUES` rows of different width;
//! 4. the arity: 109 and 110 for a `VALUES` row shorter or longer than the list, 120 and
//!    121 for a `SELECT` source shorter or longer than the list, and, without a list, 213
//!    or, when the table has an `IDENTITY` column and the source is wider than the
//!    insertable columns, 8101 (`insert_without_column_list_and_too_many_values_is_8101`);
//! 5. 339, `DEFAULT` or `NULL` written for the `IDENTITY` column;
//! 6. 206, a value whose type has no implicit conversion to its column;
//! 7. 264, a column named twice in the list, which comes out after the types of its
//!    values have been checked;
//! 8. 544, the `IDENTITY` column named in the list with a value
//!    (`insert_into_identity_column_is_544_or_8101`).
//!
//! Each of them carries the line the statement starts on
//! (`errors_carry_the_line_of_the_statement`).
//!
//! # 544 and `IDENTITY_INSERT`
//!
//! When [`crate::SessionOptions::with_identity_insert`] names this table, an explicit
//! value for the identity column binds (`tests/bind_insert.rs`,
//! `identity_insert_on_for_the_target_accepts_an_explicit_value`). For another table, or
//! when no table is named, the same `INSERT` with a column list answers 544 and without
//! one answers 8101 (`insert_into_identity_column_is_544_or_8101`).
//!
//! 544 is raised here, while the statement is bound. A batch that writes `SELECT 1/0;`
//! before such an `INSERT` therefore answers 544, where SQL Server, which raises 544
//! while the statement runs, answers 8134.
//!
//! # The names in 544 and 8101
//!
//! 8101 prints the table as it was written, delimiters removed (`INSERT INTO DBO.TI
//! VALUES (1, 1)` prints `'DBO.TI'`). 544 prints the object part alone, as written
//! (`'ti'`); the spelling of the catalogue, which SQL Server prints there, is not available
//! to the binder.

use vauban_catalog::{ColumnDef, ColumnId, TableDef, TableId};
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    Expr, Ident, InsertSource, InsertStatement, Literal, ObjectName, QueryBody, SelectStatement,
    TableRef,
};
use vauban_types::{TypeInfo, Value, implicit_result_type};

use crate::bound::{
    BoundExpr, BoundExprKind, BoundStatement, ColumnBinding, InsertPlan, LogicalPlan, OutputColumn,
    OutputSchema, SelectIntoPlan,
};
use crate::context::{BindContext, CatalogView, ResolvedTable, ResolvedTableKind};
use crate::ddl::table_name;
use crate::errors::{line_of, on_the_statement};
use crate::expr::{Scope, bind_expr};
use crate::names::{check_object_exists, from_needs_the_catalogue};
use crate::query::{bind_select, bug, dotted, not_implemented, not_yet, reread_table_arguments};

/// Binds an `INSERT` into [`BoundStatement::Insert`].
///
/// # Errors
///
/// The errors of the module documentation, in the order it lists them.
pub(crate) fn bind_insert(
    stmt: &InsertStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let line = line_of(&stmt.span);
    if stmt.top.is_some() {
        return Err(not_implemented("TOP in an INSERT"));
    }
    if stmt.output.is_some() {
        return Err(not_implemented("OUTPUT"));
    }
    let target = Target::resolve(&stmt.target, line, ctx)?;
    let listed = !stmt.columns.is_empty();
    let columns = target.columns(&stmt.columns, line)?;
    let source = match &stmt.source {
        InsertSource::DefaultValues => Source {
            columns: Vec::new(),
            plan: values_of(vec![Vec::new()], &[]),
        },
        InsertSource::Values(rows) => {
            bind_values(rows, columns.clone(), listed, &target, line, ctx)?
        }
        InsertSource::Query(select) => {
            bind_query(select, columns.clone(), listed, &target, line, ctx)?
        }
        InsertSource::Execute(_) => return Err(not_implemented("INSERT … EXECUTE")),
    };
    check_repeated(&columns, line)?;
    target.check_identity_written(&source.columns, line, ctx)?;
    Ok(BoundStatement::Insert(InsertPlan {
        table: target.table,
        columns: source.columns,
        source: Box::new(source.plan),
    }))
}

// -------------------------------------------------------------------------------------------
// SELECT … INTO
// -------------------------------------------------------------------------------------------

/// Binds `SELECT … INTO` into [`BoundStatement::SelectInto`].
///
/// The table [`SelectIntoPlan::def`] carries is deduced from [`LogicalPlan::schema`] of
/// the source query: one column per output column, name and type included. A column without
/// a name answers 1038 state 5 (`SELECT a, a + 1 INTO dbo.t FROM dbo.s;`). Two columns
/// sharing a name reuse [`SqlError::duplicate_column_name`]
/// (2705 state 3). A name the catalogue resolves already answers 2714 before the source is
/// bound, as for [`crate::ddl::bind_create_table`].
///
/// # Errors
///
/// - 2714 when the `INTO` target already exists;
/// - 1038 state 5 when an output column lacks a name;
/// - 2705 when two output columns share a name;
/// - the errors of [`bind_select`] on the source query.
pub(crate) fn bind_select_into(
    stmt: &SelectStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let line = line_of(&stmt.span);
    let into = into_target(stmt)?;
    let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
    let name = table_name(into, ctx).map_err(|err| on_the_statement(err, line))?;
    if catalog
        .resolve_table(into, ctx.database, ctx.default_schema)
        .is_some()
    {
        return Err(SqlError::object_already_exists(&into.name.value).with_line(line));
    }
    let mut source_stmt = stmt.clone();
    clear_into(&mut source_stmt);
    let plan = bind_select(&source_stmt, ctx).map_err(|err| on_the_statement(err, line))?;
    let schema = plan.schema();
    check_output_columns(schema, &name.name, line)?;
    let columns = schema
        .columns
        .iter()
        .map(|column| ColumnDef {
            name: column.name.clone(),
            ty: column.ty.clone(),
            default: None,
            identity: None,
            computed: None,
        })
        .collect();
    Ok(BoundStatement::SelectInto(SelectIntoPlan {
        def: TableDef {
            name,
            columns,
            constraints: Vec::new(),
        },
        source: Box::new(plan),
    }))
}

/// The `INTO` table of `stmt`, which the dispatch of `statement.rs` already looked for.
fn into_target(stmt: &SelectStatement) -> SqlResult<&ObjectName> {
    match &stmt.body {
        QueryBody::Select(spec) => spec
            .into
            .as_ref()
            .ok_or_else(|| bug("bind_select_into: the statement carries no INTO target")),
        QueryBody::SetOp { .. } => Err(not_yet(
            "bind_select_into: SELECT … INTO over a set operator is not implemented yet",
        )),
        QueryBody::Nested(..) => Err(not_yet(
            "bind_select_into: SELECT … INTO over a parenthesised query body is not implemented yet",
        )),
    }
}

/// Clears the `INTO` clause so [`bind_select`] can bind the source query alone.
fn clear_into(stmt: &mut SelectStatement) {
    if let QueryBody::Select(spec) = &mut stmt.body {
        spec.into = None;
    }
}

/// 1038 state 5 for a column without a name, 2705 for a duplicate output name.
fn check_output_columns(schema: &OutputSchema, table: &str, line: u32) -> SqlResult<()> {
    for (index, column) in schema.columns.iter().enumerate() {
        if column.name.is_empty() {
            let mut err = SqlError::object_or_column_name_missing();
            err.state = 5;
            return Err(err.with_line(line));
        }
        if schema.columns[..index]
            .iter()
            .any(|before| before.name.eq_ignore_ascii_case(&column.name))
        {
            return Err(SqlError::duplicate_column_name(&column.name, table).with_line(line));
        }
    }
    Ok(())
}

/// The resolved target of the statement: the table, its columns, and the two kinds of
/// column an `INSERT` does not fill by itself.
struct Target {
    /// The rows in `storage`.
    table: TableId,
    /// The name as written, for the messages of 544 and 8101.
    name: ObjectName,
    /// The columns of the table, in the order of the catalogue.
    all: Vec<ColumnBinding>,
    /// The `IDENTITY` column, when the table has one.
    identity: Option<ColumnId>,
    /// The computed columns.
    computed: Vec<ColumnId>,
}

/// The target columns and the plan that fills them, before they are put in the
/// [`InsertPlan`].
struct Source {
    columns: Vec<ColumnBinding>,
    plan: LogicalPlan,
}

impl Target {
    /// Resolves the written target against the catalogue.
    ///
    /// # Errors
    ///
    /// - 208 when the name resolves to nothing;
    /// - 215, or the error of an argument, for a name glued to arguments that are not a
    ///   lone hint word (`reread_table_arguments`), after the 208;
    /// - the internal error 50000 for a view, a table variable, and the references a
    ///   target cannot be;
    /// - the internal error of a context without a catalogue.
    fn resolve(target: &TableRef, line: u32, ctx: &BindContext<'_>) -> SqlResult<Self> {
        let name = match target {
            TableRef::Table { name, .. } => name,
            TableRef::Function {
                name,
                args,
                alias,
                span,
            } => {
                check_object_exists(name, line, ctx)?;
                reread_table_arguments(name, args, alias.as_ref(), span, ctx)?;
                name
            }
            TableRef::Variable { .. } => {
                return Err(not_yet(
                    "bind_insert: INSERT INTO @t writes a table variable, which is not implemented yet",
                ));
            }
            TableRef::Derived { .. }
            | TableRef::Join { .. }
            | TableRef::Apply { .. }
            | TableRef::Pivot(_)
            | TableRef::Unpivot(_) => {
                return Err(bug(
                    "bind_insert: the parser produced a target that is not a named table",
                ));
            }
        };
        let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
        let resolved = resolve_name(catalog, name, ctx)
            .ok_or_else(|| SqlError::invalid_object_name(&dotted(name)).with_line(line))?;
        let table = match resolved.kind {
            ResolvedTableKind::View => {
                return Err(not_yet(
                    "bind_insert: INSERT into a view is not implemented yet (V2)",
                ));
            }
            ResolvedTableKind::Table => resolved.table.ok_or_else(|| {
                bug("bind_insert: the catalogue resolved a table with no rows in storage")
            })?,
        };
        Ok(Self {
            table,
            name: name.clone(),
            all: resolved.columns,
            identity: catalog.identity_column(resolved.object),
            computed: catalog.computed_columns(resolved.object),
        })
    }

    /// The target columns: the written list resolved, or the insertable columns of the
    /// table when the list is empty. A name written twice is resolved twice here and
    /// refused by [`check_repeated`], once the source has been checked.
    ///
    /// # Errors
    ///
    /// 207 for a written name the table does not have, on `line`.
    fn columns(&self, written: &[Ident], line: u32) -> SqlResult<Vec<ColumnBinding>> {
        if written.is_empty() {
            return Ok(self
                .all
                .iter()
                .filter(|column| !self.is_generated(column))
                .cloned()
                .collect());
        }
        written
            .iter()
            .map(|ident| {
                self.all
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(&ident.value))
                    .cloned()
                    .ok_or_else(|| SqlError::invalid_column_name(&ident.value).with_line(line))
            })
            .collect()
    }

    /// Whether `column` is one the statement does not fill when no list is written: the
    /// `IDENTITY` column or a computed one.
    fn is_generated(&self, column: &ColumnBinding) -> bool {
        self.is_identity(column) || self.computed.contains(&column.column)
    }

    fn is_identity(&self, column: &ColumnBinding) -> bool {
        self.identity == Some(column.column)
    }

    /// The arity error of a source of `width` columns against `columns`, `Ok` when the two
    /// match. `listed` says whether a column list was written, `select` whether the source
    /// is a query.
    fn check_arity(
        &self,
        width: usize,
        columns: &[ColumnBinding],
        listed: bool,
        select: bool,
        line: u32,
    ) -> SqlResult<()> {
        if width == columns.len() {
            return Ok(());
        }
        let err = match (listed, select) {
            (false, _) if self.identity.is_some() && width > columns.len() => {
                SqlError::identity_insert_requires_column_list(&dotted(&self.name))
            }
            (false, _) => SqlError::column_count_does_not_match_table(),
            (true, false) if width < columns.len() => SqlError::more_columns_than_values(),
            (true, false) => SqlError::more_values_than_columns(),
            (true, true) if width < columns.len() => {
                SqlError::select_list_shorter_than_insert_list()
            }
            (true, true) => SqlError::select_list_longer_than_insert_list(),
        };
        Err(err.with_line(line))
    }

    /// 544 when the `IDENTITY` column is among the columns a value is written for and
    /// `SET IDENTITY_INSERT` is not open for this table.
    fn check_identity_written(
        &self,
        columns: &[ColumnBinding],
        line: u32,
        ctx: &BindContext<'_>,
    ) -> SqlResult<()> {
        if columns.iter().any(|column| self.is_identity(column))
            && !identity_insert_covers(ctx, &self.name)
        {
            return Err(SqlError::identity_insert_is_off(&self.name.name.value).with_line(line));
        }
        Ok(())
    }
}

/// Whether `SET IDENTITY_INSERT` is open for `name`, filling missing database and schema
/// from the bind context (`tests/bind_insert.rs`,
/// `identity_insert_on_for_the_target_accepts_an_explicit_value`).
fn identity_insert_covers(ctx: &BindContext<'_>, name: &ObjectName) -> bool {
    let database = name
        .database
        .as_ref()
        .map_or(ctx.database, |ident| ident.value.as_str());
    let schema = name
        .schema
        .as_ref()
        .map_or(ctx.default_schema, |ident| ident.value.as_str());
    ctx.options
        .identity_insert_covers(database, schema, &name.name.value)
}

/// 264 when a column appears twice in the resolved list, named as the catalogue spells it.
fn check_repeated(columns: &[ColumnBinding], line: u32) -> SqlResult<()> {
    for (position, column) in columns.iter().enumerate() {
        if columns[..position]
            .iter()
            .any(|before| before.column == column.column)
        {
            return Err(SqlError::column_specified_more_than_once(&column.name).with_line(line));
        }
    }
    Ok(())
}

/// Asks the catalogue what `name` reaches; a four-part name reaches nothing.
fn resolve_name(
    catalog: &dyn CatalogView,
    name: &ObjectName,
    ctx: &BindContext<'_>,
) -> Option<ResolvedTable> {
    if name.server.is_some() {
        return None;
    }
    catalog.resolve_table(name, ctx.database, ctx.default_schema)
}

/// Binds the rows of a `VALUES` source.
///
/// The columns written `DEFAULT` in each row are dropped from the target list and from
/// the rows (module documentation).
///
/// # Errors
///
/// 10709, then the arity errors of [`Target::check_arity`], then 339, then the errors of
/// the expressions and 206, each on `line`.
fn bind_values(
    rows: &[Vec<Expr>],
    columns: Vec<ColumnBinding>,
    listed: bool,
    target: &Target,
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<Source> {
    let width = rows.first().map_or(0, Vec::len);
    if rows.iter().any(|row| row.len() != width) {
        return Err(SqlError::table_value_constructor_rows_differ().with_line(line));
    }
    target.check_arity(width, &columns, listed, false, line)?;

    for (position, column) in columns.iter().enumerate() {
        if target.is_identity(column)
            && rows
                .iter()
                .any(|row| is_default(&row[position]) || is_null(&row[position]))
        {
            return Err(SqlError::default_or_null_as_identity_value().with_line(line));
        }
    }
    // A position takes its default when the word is written in each row; the word in
    // some rows and not in the others is the shape the plan cannot hold.
    let defaults: Vec<bool> = (0..width)
        .map(|position| rows.iter().all(|row| is_default(&row[position])))
        .collect();
    let mixed = rows.iter().any(|row| {
        row.iter()
            .zip(&defaults)
            .any(|(expr, default)| is_default(expr) && !default)
    });
    if mixed {
        return Err(not_yet(
            "bind_insert: DEFAULT in some rows of a multi-row VALUES and not in the others is not implemented yet",
        ));
    }

    let kept: Vec<ColumnBinding> = columns
        .into_iter()
        .zip(&defaults)
        .filter(|(_, default)| !**default)
        .map(|(column, _)| column)
        .collect();
    let mut bound_rows: Vec<Vec<BoundExpr>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut bound_row = Vec::with_capacity(kept.len());
        for (expr, default) in row.iter().zip(&defaults) {
            if !*default {
                let value =
                    bind_expr(expr, ctx, &Scope::empty()).map_err(|e| on_the_statement(e, line))?;
                bound_row.push(value);
            }
        }
        bound_rows.push(bound_row);
    }
    let converted = convert_rows(bound_rows, &kept, line)?;
    let plan = values_of(converted, &kept);
    Ok(Source {
        columns: kept,
        plan,
    })
}

/// Binds the `SELECT` of an `INSERT … SELECT` and checks its columns against the target.
///
/// # Errors
///
/// The errors of `bind_select`, then the arity errors of [`Target::check_arity`], then
/// 206, each on `line`.
fn bind_query(
    select: &SelectStatement,
    columns: Vec<ColumnBinding>,
    listed: bool,
    target: &Target,
    line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<Source> {
    let plan = bind_select(select, ctx).map_err(|e| on_the_statement(e, line))?;
    let width = plan.schema().columns.len();
    target.check_arity(width, &columns, listed, true, line)?;
    let untyped = projected_untyped_nulls(&plan);
    for (position, (produced, column)) in plan.schema().columns.iter().zip(&columns).enumerate() {
        if untyped.get(position).copied().unwrap_or(false) {
            continue;
        }
        check_pair(&produced.ty, &column.ty, line)?;
    }
    Ok(Source { columns, plan })
}

/// Which columns of `plan` are the bare `NULL` literal, which has no type of its own and
/// is exempt from the type check (`SELECT NULL` into a `date` column binds).
///
/// Read from the `Project` the select list bound to, under the `Limit` and `Sort` a
/// `TOP` and an `ORDER BY` put above it; a plan whose top is another node answers no
/// exemption.
fn projected_untyped_nulls(plan: &LogicalPlan) -> Vec<bool> {
    match plan {
        LogicalPlan::Project { exprs, .. } => exprs
            .iter()
            .map(|projection| is_untyped_null(&projection.expr))
            .collect(),
        LogicalPlan::Limit { input, .. } | LogicalPlan::Sort { input, .. } => {
            projected_untyped_nulls(input)
        }
        LogicalPlan::Distinct(input) => projected_untyped_nulls(input),
        _ => Vec::new(),
    }
}

/// 206 when `value` has no implicit conversion to `target`, the value's type named first.
fn check_pair(value: &TypeInfo, target: &TypeInfo, line: u32) -> SqlResult<()> {
    if implicit_result_type(value, target).is_err() {
        return Err(
            SqlError::operand_type_clash(value.ty.name(), target.ty.name()).with_line(line),
        );
    }
    Ok(())
}

/// Wraps each value of each row in the `Convert` towards its column, after the check of
/// the types (module documentation, "The conversion towards the column").
///
/// # Errors
///
/// 206 on `line`.
fn convert_rows(
    rows: Vec<Vec<BoundExpr>>,
    columns: &[ColumnBinding],
    line: u32,
) -> SqlResult<Vec<Vec<BoundExpr>>> {
    for (position, column) in columns.iter().enumerate() {
        let mut common: Option<TypeInfo> = None;
        for row in &rows {
            let value = &row[position];
            if is_untyped_null(value) {
                continue;
            }
            common = Some(match common {
                None => value.ty.clone(),
                Some(so_far) => {
                    check_pair(&so_far, &value.ty, line)?;
                    implicit_result_type(&so_far, &value.ty).unwrap_or(so_far)
                }
            });
        }
        if let Some(common) = common {
            check_pair(&common, &column.ty, line)?;
        }
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .zip(columns)
                .map(|(value, column)| convert_to(value, &column.ty))
                .collect()
        })
        .collect())
}

/// `value` under the `Convert` node towards `target`. The node is nullable when the value
/// is: the `NOT NULL` of the column is checked where the row is written, not here.
fn convert_to(value: BoundExpr, target: &TypeInfo) -> BoundExpr {
    let ty = TypeInfo {
        ty: target.ty,
        nullable: value.ty.nullable,
        collation: target.collation,
    };
    let line = value.line;
    BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(value),
            style: None,
            try_: false,
        },
        ty,
        line,
    }
}

/// The `Values` node of `rows`, its schema the names and types of `columns`.
fn values_of(rows: Vec<Vec<BoundExpr>>, columns: &[ColumnBinding]) -> LogicalPlan {
    LogicalPlan::Values {
        rows,
        schema: OutputSchema {
            columns: columns
                .iter()
                .map(|column| OutputColumn {
                    name: column.name.clone(),
                    ty: column.ty.clone(),
                })
                .collect(),
        },
    }
}

/// Whether the written expression is the word `DEFAULT`.
fn is_default(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(Literal::Default, _))
}

/// Whether the written expression is the word `NULL`.
fn is_null(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(Literal::Null, _))
}

/// Whether the bound expression is the `NULL` literal, which has no type of its own.
fn is_untyped_null(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::Literal(Value::Null))
}
